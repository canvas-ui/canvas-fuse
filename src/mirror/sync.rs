//! The sync engine and the `Mirror` facade.
//!
//! `Mirror` is what the rest of the mount holds: the store, the cache, the
//! hub client, and the local operations the FUSE layer calls on the session
//! thread (`commit_write`, `delete_local`, `rename_local`, reads). Those
//! never touch the network — they update the store, the cache and the job
//! queue, then wake the engine.
//!
//! The engine is one thread (`run`) that owns every network round-trip:
//!
//! 1. catch up on the hub's change feed (or rebuild from the listing when
//!    there is no cursor / the cursor is too old), reconciling every touched
//!    key with the three-way table in `reconcile.rs`;
//! 2. drain the durable job queue (pushes, deletes, renames, conflict
//!    uploads) — in submission order, one job in flight per key, backoff
//!    per job;
//! 3. materialize pinned keys, evict the cache to budget, expire the trash;
//! 4. write the status file and report to the hub.
//!
//! It wakes on a nudge (`backend.changed` on the socket), on a local write,
//! on `sync now`, and on the poll timer. Offline is a state, not an error:
//! the engine notes it, the mount keeps serving the store and the cache, and
//! the queue waits for the next wake that finds the hub back.

use super::cache::Cache;
use super::hub::{Change, ChangeOp, HubClient, HubError, PutBody, PutOptions, RemoteStat};
use super::reconcile::{decide, Action};
use super::store::{
    Base, Conflict, Entry, EntryState, Job, JobKind, Store, Trashed, HEAD_KEY, META_INSTANCE,
    META_LISTED,
};
use super::{ConflictMode, DeleteMode, DeviceIdentity, IgnoreRules, MirrorOptions};
use crate::state::{Invalidation, Tree};
use anyhow::{Context as _, Result};
use fuser::ReplyData;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const LIST_PAGE: usize = 1000;
const CHANGES_PAGE: usize = 1000;
const TRASH_TTL_MS: u64 = 30 * 24 * 3600 * 1000;
/// A full listing pass this often even without `sync now`, so a change the
/// feed lost (or an exclusion that changed) is bounded.
const FULL_PASS_EVERY: Duration = Duration::from_secs(3600);
const STATUS_THROTTLE: Duration = Duration::from_secs(1);
const REPORT_THROTTLE: Duration = Duration::from_secs(5);
const REPORT_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncState {
    Idle,
    Syncing,
    Offline,
    /// Credentials refused; nothing will move until the token is fixed and
    /// the mount restarted (or `sync now` finds the hub accepting us again).
    Paused,
}

/// What `canvas-fuse status --json` shows for a mirror mount, written by the
/// daemon to `<state dir>/mounts/<name>.<hash>.status.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorStatus {
    pub workspace_id: String,
    pub backend: String,
    pub state: SyncState,
    pub cursor: Option<u64>,
    pub head: Option<u64>,
    pub pending: u64,
    pub failed: u64,
    pub conflicts: u64,
    pub skipped: u64,
    pub pinned: Vec<String>,
    pub cache_used: u64,
    pub cache_budget: u64,
    pub last_sync: Option<String>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub entries: u64,
    #[serde(default)]
    pub device_id: String,
}

pub enum EngineMsg {
    /// Something changed on the hub (socket nudge) or locally (a write
    /// queued a job): run a cycle.
    Wake,
    /// `sync now`: a full listing pass, then answer when done.
    SyncNow(Option<Sender<()>>),
    /// The socket re-authenticated: the hub is back.
    Reconnect,
    Stop,
}

/// How the engine reaches the FUSE view. None in tests (no tree).
pub struct ViewLink {
    pub tree: Arc<RwLock<Tree>>,
    /// The write store's sync lock — held so a remote landing never
    /// interleaves with a local multi-step mutation.
    pub refresh_lock: Arc<Mutex<()>>,
    pub invalidations: Arc<Mutex<Vec<Invalidation>>>,
    pub job_tx: Sender<crate::worker::Job>,
}

pub struct MirrorConfig {
    pub data_dir: PathBuf,
    pub server: String,
    pub token: String,
    pub tls: Option<crate::tls::ClientIdentity>,
    pub workspace_id: String,
    pub backend: String,
    pub opts: MirrorOptions,
    pub device: DeviceIdentity,
    pub mountpoint: PathBuf,
    pub status_path: Option<PathBuf>,
}

struct Runtime {
    state: SyncState,
    last_sync: Option<String>,
    last_error: Option<String>,
    last_full: Option<Instant>,
    last_status_write: Option<Instant>,
    last_report: Option<Instant>,
    /// keys → reason; local files the hub refuses (excluded, invalid key).
    skipped: HashMap<String, String>,
    /// Keys whose remote change was deferred because a write handle was
    /// open; re-checked against the hub on the next cycle.
    recheck: HashSet<String>,
}

struct PendingRead {
    offset: i64,
    size: u32,
    reply: ReplyData,
}

struct FetchJob {
    key: String,
    sha: String,
}

pub struct Mirror {
    pub store: Arc<Store>,
    pub cache: Arc<Cache>,
    pub hub: Arc<HubClient>,
    pub device: DeviceIdentity,
    pub opts: MirrorOptions,
    pub mountpoint: PathBuf,
    status_path: Option<PathBuf>,
    ignore: RwLock<IgnoreRules>,
    pins: RwLock<Vec<String>>,
    rt: Mutex<Runtime>,
    engine_tx: Mutex<Option<Sender<EngineMsg>>>,
    view: Mutex<Option<ViewLink>>,
    /// Keys with an open write handle: remote landings wait for the close.
    open_writes: Mutex<HashSet<String>>,
    /// Paths whose offline read was already logged (once per path).
    eio_logged: Mutex<HashSet<String>>,
    in_flight: Mutex<HashMap<String, Vec<PendingRead>>>,
    fetch_tx: Sender<FetchJob>,
}

fn ms_to_systime(ms: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
}

fn backoff_ms(attempts: u32) -> u64 {
    let base = 60_000u64;
    let n = attempts.min(6);
    (base << n).min(3_600_000)
}

impl Mirror {
    pub fn open(cfg: MirrorConfig) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.data_dir)
            .with_context(|| format!("creating {}", cfg.data_dir.display()))?;
        let store = Arc::new(Store::open(&super::store_path(&cfg.data_dir))?);
        let cache = Arc::new(Cache::open(
            &super::cache_dir(&cfg.data_dir),
            store.clone(),
            cfg.opts.cache_budget_bytes,
        )?);
        let hub = Arc::new(HubClient::with_tls(
            &cfg.server,
            &cfg.token,
            &cfg.workspace_id,
            &cfg.backend,
            &cfg.device,
            cfg.tls.as_ref(),
        )?);
        // Pins given on the command line join the persisted set; `pin rm`
        // is how one leaves.
        for p in &cfg.opts.pins {
            store.add_pin(p)?;
        }
        let pins = store.pins();
        // Hub exclusions from the last online run, so an offline mount
        // applies the same rules it did yesterday.
        let mut patterns: Vec<String> = super::MIRROR_IGNORE_DEFAULTS
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(saved) = store.meta("exclusions") {
            patterns.extend(saved.split('\n').map(str::to_string));
        }
        patterns.extend(cfg.opts.ignore.iter().cloned());

        let (fetch_tx, fetch_rx) = channel::<FetchJob>();
        let mirror = Arc::new(Self {
            store,
            cache,
            hub,
            device: cfg.device,
            opts: cfg.opts,
            mountpoint: cfg.mountpoint,
            status_path: cfg.status_path,
            ignore: RwLock::new(IgnoreRules::new(patterns)),
            pins: RwLock::new(pins),
            rt: Mutex::new(Runtime {
                state: SyncState::Idle,
                last_sync: None,
                last_error: None,
                last_full: None,
                last_status_write: None,
                last_report: None,
                skipped: HashMap::new(),
                recheck: HashSet::new(),
            }),
            engine_tx: Mutex::new(None),
            view: Mutex::new(None),
            open_writes: Mutex::new(HashSet::new()),
            eio_logged: Mutex::new(HashSet::new()),
            in_flight: Mutex::new(HashMap::new()),
            fetch_tx,
        });
        mirror.spawn_fetch_pool(fetch_rx, 3);
        Ok(mirror)
    }

    // ── wiring ───────────────────────────────────────────────────────────────

    pub fn attach_view(&self, link: ViewLink) {
        *self.view.lock() = Some(link);
    }

    /// Start the engine thread. Returns the sender used to wake/stop it.
    pub fn spawn_engine(self: &Arc<Self>, stop: Arc<AtomicBool>) -> Result<Sender<EngineMsg>> {
        let (tx, rx) = channel::<EngineMsg>();
        *self.engine_tx.lock() = Some(tx.clone());
        let me = self.clone();
        std::thread::Builder::new()
            .name("canvas-fuse-mirror".into())
            .spawn(move || me.run(rx, stop))?;
        Ok(tx)
    }

    pub fn wake(&self) {
        if let Some(tx) = self.engine_tx.lock().as_ref() {
            let _ = tx.send(EngineMsg::Wake);
        }
    }

    pub fn reconnect(&self) {
        if let Some(tx) = self.engine_tx.lock().as_ref() {
            let _ = tx.send(EngineMsg::Reconnect);
        }
    }

    /// `sync now`: block until a full pass finished (or the timeout).
    pub fn sync_now(&self, timeout: Duration) -> bool {
        let (done_tx, done_rx) = channel::<()>();
        let sent = match self.engine_tx.lock().as_ref() {
            Some(tx) => tx.send(EngineMsg::SyncNow(Some(done_tx))).is_ok(),
            None => false,
        };
        if !sent {
            return false;
        }
        done_rx.recv_timeout(timeout).is_ok()
    }

    pub fn state(&self) -> SyncState {
        self.rt.lock().state
    }

    pub fn is_offline(&self) -> bool {
        matches!(self.rt.lock().state, SyncState::Offline | SyncState::Paused)
    }

    fn set_state(&self, state: SyncState) {
        let changed = {
            let mut rt = self.rt.lock();
            let changed = rt.state != state;
            rt.state = state;
            changed
        };
        if changed {
            log::info!("mirror state: {state:?}");
            self.write_status(true);
        }
    }

    fn set_error(&self, err: Option<String>) {
        self.rt.lock().last_error = err;
    }

    pub fn is_ignored(&self, key: &str) -> bool {
        self.ignore.read().is_ignored(key)
    }

    pub fn pins(&self) -> Vec<String> {
        self.pins.read().clone()
    }

    pub fn is_pinned(&self, key: &str) -> bool {
        super::is_pinned(&self.pins.read(), key)
    }

    pub fn add_pin(&self, glob: &str) -> Result<()> {
        self.store.add_pin(glob)?;
        *self.pins.write() = self.store.pins();
        self.wake();
        Ok(())
    }

    pub fn remove_pin(&self, glob: &str) -> Result<bool> {
        let had = self.store.remove_pin(glob)?;
        *self.pins.write() = self.store.pins();
        Ok(had)
    }

    // ── view updates (engine → tree → kernel) ───────────────────────────────

    fn with_view<F: FnOnce(&mut Tree) -> Invalidation>(&self, f: F) {
        let view = self.view.lock();
        let Some(link) = view.as_ref() else {
            return;
        };
        let inv = {
            let _guard = link.refresh_lock.lock();
            let mut tree = link.tree.write();
            f(&mut tree)
        };
        if inv.is_empty() {
            return;
        }
        link.invalidations.lock().push(inv);
        let _ = link.job_tx.send(crate::worker::Job::MirrorInvalidations);
    }

    fn view_upsert(&self, key: &str, size: u64, mtime: u64) {
        self.with_view(|t| t.upsert_home_key(key, size, ms_to_systime(mtime)));
    }

    fn view_remove(&self, key: &str) {
        let keep: HashSet<String> = self.store.dirs().into_iter().collect();
        self.with_view(|t| t.remove_home_key(key, &keep));
    }

    fn view_rename(&self, from: &str, to: &str) {
        self.with_view(|t| t.rename_home_key(from, to));
    }

    /// Build the whole Home tree from the store (mount time).
    pub fn snapshot_into(&self, tree: &mut Tree) {
        let files: Vec<(String, u64, SystemTime)> = self
            .store
            .entries("")
            .into_iter()
            .filter(|(_, e)| e.state != EntryState::Tombstone)
            .map(|(k, e)| (k, e.size, ms_to_systime(e.mtime)))
            .collect();
        let dirs = self.store.dirs();
        tree.apply_home_snapshot(&files, &dirs);
    }

    // ── local operations (session thread; no network) ───────────────────────

    pub fn entry(&self, key: &str) -> Option<Entry> {
        self.store
            .entry(key)
            .filter(|e| e.state != EntryState::Tombstone)
    }

    pub fn note_open_write(&self, key: &str) {
        self.open_writes.lock().insert(key.to_string());
    }

    pub fn note_close_write(&self, key: &str) {
        self.open_writes.lock().remove(key);
    }

    fn is_open_for_write(&self, key: &str) -> bool {
        self.open_writes.lock().contains(key)
    }

    /// A flushed write: bytes into the cache, entry Dirty, push queued.
    pub fn commit_write(&self, key: &str, bytes: &[u8]) -> Result<Entry> {
        let sha = self.cache.insert_bytes(bytes)?;
        let entry = Entry {
            sha256: sha,
            size: bytes.len() as u64,
            mtime: super::now_ms(),
            state: EntryState::Dirty,
        };
        self.store.put_entry(key, &entry)?;
        let _ = self.store.remove_trashed(key);
        if self.is_ignored(key) {
            self.rt.lock().skipped.insert(
                key.to_string(),
                "matches an exclusion rule; kept local only".into(),
            );
        } else {
            self.rt.lock().skipped.remove(key);
            self.store.enqueue(JobKind::Push {
                key: key.to_string(),
            })?;
        }
        self.wake();
        Ok(entry)
    }

    /// `rm`: tombstone + delete job (propagate), or forget (keep).
    pub fn delete_local(&self, key: &str) -> Result<bool> {
        let Some(entry) = self.store.entry(key) else {
            return Ok(false);
        };
        self.store.remove_jobs_for(key)?;
        let base = self.store.base(key);
        match (self.opts.deletes, base, entry.state) {
            (DeleteMode::Propagate, Some(_), _) if !self.is_ignored(key) => {
                self.store.put_entry(
                    key,
                    &Entry {
                        state: EntryState::Tombstone,
                        ..entry
                    },
                )?;
                self.store.enqueue(JobKind::Delete {
                    key: key.to_string(),
                })?;
            }
            _ => {
                // Never pushed (or `--deletes keep`): nothing to tell the hub.
                self.store.remove_entry(key)?;
                self.store.remove_base(key)?;
            }
        }
        self.rt.lock().skipped.remove(key);
        self.wake();
        Ok(true)
    }

    pub fn mkdir_local(&self, key: &str) -> Result<()> {
        self.store.add_dir(key)?;
        if !self.is_ignored(key) {
            self.store.enqueue(JobKind::Mkdir {
                key: key.to_string(),
            })?;
        }
        self.wake();
        Ok(())
    }

    /// Ok(false) = not empty.
    pub fn rmdir_local(&self, key: &str) -> Result<bool> {
        if self.store.dir_has_children(key) {
            return Ok(false);
        }
        self.store.remove_dir(key)?;
        self.store.remove_jobs_for(key)?;
        if !self.is_ignored(key) {
            self.store.enqueue(JobKind::Rmdir {
                key: key.to_string(),
            })?;
        }
        self.wake();
        Ok(true)
    }

    /// `mv` of a file: re-key the entry, the base and the pending jobs; a
    /// rename job goes out only when the hub knows the source (has a base).
    /// A file that was never pushed just gets pushed under its new name.
    pub fn rename_local(&self, from: &str, to: &str) -> Result<()> {
        if self.store.entry(to).is_some() {
            // Overwrite-rename: the destination goes first, in the queue
            // too, so the hub sees delete(to) then rename(from → to).
            self.delete_local(to)?;
        }
        let dropped = self.store.remove_jobs_for(from)?;
        self.store.rekey(from, to)?;
        if let Some(c) = self.store.conflict(from) {
            self.store.remove_conflict(from)?;
            self.store.put_conflict(&Conflict {
                key: to.to_string(),
                ..c
            })?;
        }
        let _ = self.store.remove_trashed(to);
        let has_base = self.store.base(to).is_some();
        if has_base && !self.is_ignored(from) {
            if self.is_ignored(to) {
                // Moving INTO an excluded name: the hub cannot hold it there.
                // Delete the source on the hub; the bytes stay local.
                self.store.remove_base(to)?;
                self.store.enqueue(JobKind::Delete {
                    key: from.to_string(),
                })?;
            } else {
                self.store.enqueue(JobKind::Rename {
                    from: from.to_string(),
                    to: to.to_string(),
                })?;
            }
        }
        for job in dropped {
            let kind = match job.kind {
                JobKind::Push { .. } => JobKind::Push {
                    key: to.to_string(),
                },
                JobKind::Conflict {
                    local_sha256,
                    base_sha256,
                    ..
                } => JobKind::Conflict {
                    key: to.to_string(),
                    local_sha256,
                    base_sha256,
                },
                JobKind::Rename { from: f, .. } if f == from => JobKind::Rename {
                    from: f,
                    to: to.to_string(),
                },
                other => other,
            };
            if !self.is_ignored(kind.key()) {
                self.store.enqueue(kind)?;
            }
        }
        if let Some(mut e) = self.store.entry(to) {
            if self.is_ignored(to) {
                self.rt.lock().skipped.insert(
                    to.to_string(),
                    "matches an exclusion rule; kept local only".into(),
                );
            } else if !has_base && e.state == EntryState::Clean {
                // Clean without a base cannot happen, but be safe: push.
                e.state = EntryState::Dirty;
                self.store.put_entry(to, &e)?;
                self.store.enqueue(JobKind::Push {
                    key: to.to_string(),
                })?;
            }
        }
        {
            let mut ow = self.open_writes.lock();
            if ow.remove(from) {
                ow.insert(to.to_string());
            }
        }
        self.wake();
        Ok(())
    }

    /// `mv` of a directory: every file under it individually (the hub has
    /// no directory rename), plus the explicit dir records.
    pub fn rename_dir_local(&self, from: &str, to: &str) -> Result<()> {
        if self.store.dir_has_children(to) || self.store.entry(to).is_some() {
            anyhow::bail!("target exists");
        }
        let children: Vec<String> = self
            .store
            .entries_under(from)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        for key in children {
            let rest = &key[from.len() + 1..];
            self.rename_local(&key, &format!("{to}/{rest}"))?;
        }
        self.store.rekey_dirs(from, to)?;
        if !self.store.has_dir(to) && !self.store.dir_has_children(to) {
            self.store.add_dir(to)?;
        }
        if !self.is_ignored(to) && !self.store.dir_has_children(to) {
            // An empty directory has no files to carry its name over.
            self.store.enqueue(JobKind::Mkdir {
                key: to.to_string(),
            })?;
        }
        if !self.is_ignored(from) {
            self.store.enqueue(JobKind::Rmdir {
                key: from.to_string(),
            })?;
        }
        self.wake();
        Ok(())
    }

    /// The bytes an editor starts from. Cache hit → local; miss → fetch now
    /// (blocking, this IS the open) when the hub is reachable.
    pub fn bytes_for_edit(&self, key: &str) -> Result<Vec<u8>, i32> {
        let Some(entry) = self.entry(key) else {
            return Err(libc::ENOENT);
        };
        if self.cache.has(&entry.sha256) {
            self.cache.touch(&entry.sha256);
            return self.cache.read_all(&entry.sha256).map_err(|_| libc::EIO);
        }
        if self.is_offline() {
            self.log_eio_once(key);
            return Err(libc::EIO);
        }
        match self.cache.fetch(&self.hub, key, &entry.sha256) {
            Ok(got) => {
                if got != entry.sha256 {
                    // The hub moved on while we were not looking; take what it
                    // has — the entry follows via the feed.
                    log::debug!("{key}: fetched {got} while entry says {}", entry.sha256);
                }
                self.cache.read_all(&got).map_err(|_| libc::EIO)
            }
            Err(HubError::NotFound) => Err(libc::ENOENT),
            Err(e) => {
                if e.is_offline() {
                    self.set_state(SyncState::Offline);
                }
                log::warn!("{key}: fetch for edit failed: {e}");
                Err(libc::EIO)
            }
        }
    }

    fn log_eio_once(&self, key: &str) {
        if self.eio_logged.lock().insert(key.to_string()) {
            log::warn!("{key}: not cached and the hub is unreachable (EIO); logged once");
        }
    }

    // ── reads (session thread; cache pread or non-blocking fetch) ───────────

    pub fn read(&self, key: &str, offset: i64, size: u32, reply: ReplyData) {
        let Some(entry) = self.entry(key) else {
            reply.error(libc::ENOENT);
            return;
        };
        let sha = entry.sha256.clone();
        if self.cache.has(&sha) {
            match self.cache.pread(&sha, offset.max(0) as u64, size as usize) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => {
                    log::warn!("{key}: cache read failed: {e}");
                    reply.error(libc::EIO);
                }
            }
            return;
        }
        if self.is_offline() {
            self.log_eio_once(key);
            reply.error(libc::EIO);
            return;
        }
        let pending = PendingRead {
            offset,
            size,
            reply,
        };
        let mut in_flight = self.in_flight.lock();
        if let Some(waiters) = in_flight.get_mut(&sha) {
            waiters.push(pending);
            return;
        }
        in_flight.insert(sha.clone(), vec![pending]);
        drop(in_flight);
        let _ = self.fetch_tx.send(FetchJob {
            key: key.to_string(),
            sha,
        });
    }

    fn spawn_fetch_pool(self: &Arc<Self>, rx: Receiver<FetchJob>, workers: usize) {
        let pool: Vec<Sender<FetchJob>> = (0..workers.max(1))
            .map(|i| {
                let (tx, rx) = channel::<FetchJob>();
                // Weak: the pool must not keep the mirror (and its redb lock)
                // alive after the mount dropped it.
                let me = Arc::downgrade(self);
                std::thread::Builder::new()
                    .name(format!("canvas-fuse-mirror-fetch-{i}"))
                    .spawn(move || {
                        while let Ok(job) = rx.recv() {
                            let Some(m) = me.upgrade() else {
                                break;
                            };
                            m.run_fetch(job);
                        }
                    })
                    .expect("spawning mirror fetch worker");
                tx
            })
            .collect();
        std::thread::Builder::new()
            .name("canvas-fuse-mirror-fetch-dispatch".into())
            .spawn(move || {
                let mut next = 0usize;
                while let Ok(job) = rx.recv() {
                    if pool[next % pool.len()].send(job).is_err() {
                        break;
                    }
                    next += 1;
                }
            })
            .expect("spawning mirror fetch dispatcher");
    }

    fn run_fetch(&self, job: FetchJob) {
        let entry = self.entry(&job.key);
        let oversized = entry
            .as_ref()
            .map(|e| e.size > self.cache.budget())
            .unwrap_or(false);
        // A file bigger than the whole cache is served by byte windows
        // straight from the hub; caching it would evict everything else.
        if oversized {
            let waiters = self.in_flight.lock().remove(&job.sha).unwrap_or_default();
            for w in waiters {
                let start = w.offset.max(0) as u64;
                let end = start + w.size as u64 - 1;
                match self.hub.get_range(&job.key, start, end) {
                    Ok(bytes) => w.reply.data(&bytes),
                    Err(e) => {
                        if e.is_offline() {
                            self.set_state(SyncState::Offline);
                        }
                        w.reply.error(libc::EIO);
                    }
                }
            }
            return;
        }
        let result = self.cache.fetch(&self.hub, &job.key, &job.sha);
        let waiters = self.in_flight.lock().remove(&job.sha).unwrap_or_default();
        match result {
            Ok(got) => {
                for w in waiters {
                    match self
                        .cache
                        .pread(&got, w.offset.max(0) as u64, w.size as usize)
                    {
                        Ok(bytes) => w.reply.data(&bytes),
                        Err(_) => w.reply.error(libc::EIO),
                    }
                }
                if got != job.sha {
                    // Bytes changed under the entry; let the engine sort it
                    // out rather than serving a mismatch silently forever.
                    self.wake();
                }
            }
            Err(e) => {
                if e.is_offline() {
                    self.set_state(SyncState::Offline);
                    self.log_eio_once(&job.key);
                } else {
                    log::warn!("{}: fetch failed: {e}", job.key);
                }
                for w in waiters {
                    w.reply.error(libc::EIO);
                }
            }
        }
    }

    // ── trash / conflicts (CLI) ─────────────────────────────────────────────

    pub fn trash_list(&self) -> Vec<(String, Trashed)> {
        self.store.trash()
    }

    /// Bring a hub-deleted key back from the local copy: a new push.
    pub fn trash_restore(&self, key: &str) -> Result<()> {
        let t = self
            .store
            .trashed(key)
            .with_context(|| format!("{key} is not in the mirror trash"))?;
        if !self.cache.has(&t.sha256) {
            anyhow::bail!("{key}: the bytes are no longer cached");
        }
        if self.store.entry(key).is_some() {
            anyhow::bail!("{key} exists again; restore to a different name first");
        }
        self.store.remove_base(key)?;
        let entry = Entry {
            sha256: t.sha256.clone(),
            size: t.size,
            mtime: super::now_ms(),
            state: EntryState::Dirty,
        };
        self.store.put_entry(key, &entry)?;
        self.store.remove_trashed(key)?;
        self.store.enqueue(JobKind::Push {
            key: key.to_string(),
        })?;
        self.view_upsert(key, entry.size, entry.mtime);
        self.wake();
        Ok(())
    }

    pub fn conflicts(&self) -> Vec<Conflict> {
        self.store.conflicts()
    }

    /// The hub says a conflict was resolved: drop our record; the outcome
    /// arrives as ordinary changes on the feed.
    pub fn conflict_resolved(&self, key: &str) {
        if let Some(mut c) = self.store.conflict(key) {
            c.resolved = true;
            let _ = self.store.remove_conflict(key);
            log::info!("{key}: conflict resolved on the hub");
        }
        self.wake();
    }

    // ── status ───────────────────────────────────────────────────────────────

    pub fn status(&self) -> MirrorStatus {
        let jobs = self.store.jobs();
        let rt = self.rt.lock();
        MirrorStatus {
            workspace_id: self.hub.workspace_id().to_string(),
            backend: self.hub.backend().to_string(),
            state: rt.state,
            cursor: self.store.cursor(),
            head: self.store.number(HEAD_KEY),
            pending: jobs.len() as u64,
            failed: jobs.iter().filter(|j| j.attempts > 0).count() as u64,
            conflicts: self
                .store
                .conflicts()
                .iter()
                .filter(|c| !c.resolved)
                .count() as u64,
            skipped: rt.skipped.len() as u64,
            pinned: self.pins.read().clone(),
            cache_used: self.cache.used(),
            cache_budget: self.cache.budget(),
            last_sync: rt.last_sync.clone(),
            last_error: rt.last_error.clone(),
            entries: self.store.entry_count(),
            device_id: self.device.id.clone(),
        }
    }

    /// Write the status file (throttled to once a second unless forced).
    pub fn write_status(&self, force: bool) {
        let Some(path) = &self.status_path else {
            return;
        };
        {
            let mut rt = self.rt.lock();
            if !force {
                if let Some(t) = rt.last_status_write {
                    if t.elapsed() < STATUS_THROTTLE {
                        return;
                    }
                }
            }
            rt.last_status_write = Some(Instant::now());
        }
        let status = self.status();
        let tmp = path.with_extension("json.tmp");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&status) {
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    fn report_to_hub(&self, force: bool) {
        {
            let rt = self.rt.lock();
            if matches!(rt.state, SyncState::Offline | SyncState::Paused) {
                return;
            }
            if let Some(t) = rt.last_report {
                let min = if force { REPORT_THROTTLE } else { REPORT_EVERY };
                if t.elapsed() < min {
                    return;
                }
            }
        }
        let s = self.status();
        let body = serde_json::json!({
            "backend": s.backend,
            "client": "fuse",
            "path": self.mountpoint.to_string_lossy(),
            "prefixes": s.pinned,
            "cursor": s.cursor.unwrap_or(0),
            "pending": s.pending,
            "failed": s.failed,
            "conflicts": s.conflicts,
            "skipped": s.skipped,
            "state": s.state,
            "lastSync": s.last_sync.unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            "lastError": s.last_error,
            "version": env!("CARGO_PKG_VERSION"),
        });
        match self.hub.report_status(&body) {
            Ok(head) => {
                let _ = self.store.set_number(HEAD_KEY, head);
                self.rt.lock().last_report = Some(Instant::now());
            }
            Err(e) => log::debug!("mirror status report failed: {e}"),
        }
    }

    // ── the engine ───────────────────────────────────────────────────────────

    fn run(self: Arc<Self>, rx: Receiver<EngineMsg>, stop: Arc<AtomicBool>) {
        let poll = Duration::from_secs(self.opts.poll_secs.max(5));
        // First cycle straight away: a fresh mount lists, a restarted one
        // catches up and drains what queued while it was down.
        self.cycle(self.store.meta(META_LISTED).is_none());
        while !stop.load(Ordering::Relaxed) {
            let (full, done) = match rx.recv_timeout(poll) {
                Ok(EngineMsg::Stop) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(EngineMsg::SyncNow(done)) => (true, done),
                Ok(EngineMsg::Wake) => (false, None),
                Ok(EngineMsg::Reconnect) => {
                    if self.rt.lock().state == SyncState::Offline {
                        self.set_state(SyncState::Idle);
                    }
                    (false, None)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (false, None),
            };
            // Coalesce a burst of wakes into one cycle.
            let mut full = full;
            let mut dones = Vec::new();
            if let Some(d) = done {
                dones.push(d);
            }
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    EngineMsg::Stop => return,
                    EngineMsg::SyncNow(d) => {
                        full = true;
                        if let Some(d) = d {
                            dones.push(d);
                        }
                    }
                    EngineMsg::Reconnect => {
                        if self.rt.lock().state == SyncState::Offline {
                            self.set_state(SyncState::Idle);
                        }
                    }
                    EngineMsg::Wake => {}
                }
            }
            let due_full = self
                .rt
                .lock()
                .last_full
                .map(|t| t.elapsed() > FULL_PASS_EVERY)
                .unwrap_or(true);
            self.cycle(full || due_full);
            for d in dones {
                let _ = d.send(());
            }
        }
        log::debug!("mirror engine exiting");
    }

    /// One pass: catch up, reconcile, drain, materialize, evict, report.
    /// Public so tests drive it without a thread.
    pub fn cycle(&self, full: bool) {
        let before = self.rt.lock().state;
        if before == SyncState::Paused && !full {
            return;
        }
        self.set_state(SyncState::Syncing);
        let result = self.cycle_inner(full);
        match result {
            Ok(()) => {
                self.rt.lock().last_sync = Some(chrono::Utc::now().to_rfc3339());
                self.set_error(None);
                self.set_state(SyncState::Idle);
            }
            Err(e) => {
                let msg = e.to_string();
                match &e {
                    HubError::Offline(_) => {
                        // Loud once, on the way down; every wake while
                        // offline would otherwise repeat it.
                        if before != SyncState::Offline {
                            log::info!("mirror: hub unreachable, going offline ({msg})");
                        } else {
                            log::debug!("mirror: still offline ({msg})");
                        }
                        self.set_error(Some(msg));
                        self.set_state(SyncState::Offline);
                    }
                    HubError::Unauthorized => {
                        log::error!("mirror: hub refused our credentials; pausing");
                        self.set_error(Some(msg));
                        self.set_state(SyncState::Paused);
                    }
                    _ => {
                        log::warn!("mirror cycle failed: {msg}");
                        self.set_error(Some(msg));
                        self.set_state(SyncState::Idle);
                    }
                }
            }
        }
        self.housekeeping();
        self.write_status(true);
        self.report_to_hub(full);
    }

    fn cycle_inner(&self, full: bool) -> Result<(), HubError> {
        self.refresh_hub_rules()?;
        if full || self.store.cursor().is_none() || self.store.meta(META_LISTED).is_none() {
            self.rebuild_from_listing()?;
            self.rt.lock().last_full = Some(Instant::now());
        } else {
            self.catch_up()?;
        }
        self.recheck_deferred()?;
        self.recover_jobs();
        self.drain_jobs()?;
        self.materialize_pins()?;
        Ok(())
    }

    /// Instance id + exclusions, once per online cycle (cheap; both change
    /// rarely, but the exclusions decide what we refuse to queue).
    fn refresh_hub_rules(&self) -> Result<(), HubError> {
        if self.store.meta(META_INSTANCE).is_none() {
            if let Some(id) = self.hub.instance_id()? {
                let _ = self.store.set_meta(META_INSTANCE, &id);
            }
        }
        if self.store.meta("exclusions").is_none() || self.rt.lock().last_full.is_none() {
            match self.hub.exclusions() {
                Ok(list) => {
                    let _ = self.store.set_meta("exclusions", &list.join("\n"));
                    let mut patterns: Vec<String> = super::MIRROR_IGNORE_DEFAULTS
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    patterns.extend(list);
                    patterns.extend(self.opts.ignore.iter().cloned());
                    *self.ignore.write() = IgnoreRules::new(patterns);
                }
                Err(e) if e.is_offline() || matches!(e, HubError::Unauthorized) => return Err(e),
                Err(e) => log::debug!("exclusions not available: {e}"),
            }
        }
        Ok(())
    }

    /// No cursor (first run) or 410: list everything, reconcile every key
    /// we or the hub know, and tail from the listing's head.
    fn rebuild_from_listing(&self) -> Result<(), HubError> {
        log::info!("mirror: full listing pass");
        let mut remote: BTreeMap<String, RemoteStat> = BTreeMap::new();
        let mut cursor: Option<String> = None;
        let mut head = 0u64;
        loop {
            let page = self.hub.list_objects("", cursor.as_deref(), LIST_PAGE)?;
            if cursor.is_none() {
                head = page.head;
            }
            for o in page.objects {
                remote.insert(
                    super::normalize_key(&o.key),
                    RemoteStat {
                        sha256: o.sha256,
                        size: o.size,
                        mtime: o.mtime,
                    },
                );
            }
            match page.cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        let mut keys: HashSet<String> = remote.keys().cloned().collect();
        keys.extend(self.store.entries("").into_iter().map(|(k, _)| k));
        keys.extend(self.store.bases("").into_iter().map(|(k, _)| k));
        let mut sorted: Vec<String> = keys.into_iter().collect();
        sorted.sort();
        for key in sorted {
            self.reconcile_key(&key, remote.get(&key), 0)?;
        }
        let _ = self.store.set_cursor(head);
        let _ = self.store.set_number(HEAD_KEY, head);
        let _ = self.store.set_meta(META_LISTED, "1");
        Ok(())
    }

    fn catch_up(&self) -> Result<(), HubError> {
        let mut since = self.store.cursor().unwrap_or(0);
        loop {
            let page = match self.hub.changes(since, CHANGES_PAGE) {
                Ok(p) => p,
                Err(HubError::CursorTooOld { oldest, head }) => {
                    log::info!("mirror: cursor {since} predates the log (oldest {oldest}, head {head}); rebuilding");
                    return self.rebuild_from_listing();
                }
                Err(e) => return Err(e),
            };
            let _ = self.store.set_number(HEAD_KEY, page.head);
            let n = page.changes.len();
            let mut last = since;
            for change in &page.changes {
                self.apply_change(change)?;
                last = last.max(change.seq);
            }
            // The cursor moves only once the whole batch reconciled.
            since = if n == 0 { page.head.max(since) } else { last };
            let _ = self.store.set_cursor(since);
            if n < CHANGES_PAGE {
                break;
            }
        }
        Ok(())
    }

    fn apply_change(&self, change: &Change) -> Result<(), HubError> {
        let key = super::normalize_key(&change.key);
        // Feed entries are notifications, not snapshots. An old put (including
        // our own echo) must never roll the base back or invent a conflict.
        let stat = self.hub.head_object(&key)?;
        if change.op == ChangeOp::Rename {
            if let Some(from) = change.from.as_deref().map(super::normalize_key) {
                let source = self.hub.head_object(&from)?;
                // Only preserve rename identity while the advertised move is
                // still current. A recreated source or edited target is instead
                // reconciled independently against its current bytes.
                if source.is_none()
                    && stat.as_ref().map(|s| &s.sha256) == change.sha256.as_ref()
                    && stat.is_some()
                {
                    self.apply_remote_rename(&from, &key, change.seq)?;
                } else {
                    self.reconcile_key(&from, source.as_ref(), change.seq)?;
                }
            }
        }
        self.reconcile_key(&key, stat.as_ref(), change.seq)
    }

    /// A hub-side rename: re-key locally when our copy is clean; a dirty
    /// source is a new file under the old name (delete + add semantics), so
    /// it stays put and pushes as new.
    fn apply_remote_rename(&self, from: &str, to: &str, seq: u64) -> Result<(), HubError> {
        let Some(entry) = self.store.entry(from) else {
            return Ok(());
        };
        let base = self.store.base(from);
        let clean = entry.state == EntryState::Clean
            && base
                .as_ref()
                .map(|b| b.sha256 == entry.sha256)
                .unwrap_or(false);
        if !clean || self.is_open_for_write(from) {
            let _ = self.store.remove_base(from);
            if entry.state == EntryState::Dirty && !self.is_ignored(from) {
                let _ = self.store.enqueue(JobKind::Push {
                    key: from.to_string(),
                });
            }
            return Ok(());
        }
        if self.store.entry(to).is_some() {
            // Something already sits at the target locally; treat the source
            // as deleted and let the target reconcile on its own.
            let _ = self.store.remove_entry(from);
            let _ = self.store.remove_base(from);
            self.view_remove(from);
            return Ok(());
        }
        let _ = self.store.rekey(from, to);
        if let Some(mut b) = self.store.base(to) {
            b.remote_seq = seq;
            let _ = self.store.put_base(to, &b);
        }
        self.view_rename(from, to);
        Ok(())
    }

    /// The heart: decide for one key and act. `remote` is what the hub says
    /// (None = gone / never there). Only the actions that need bytes talk
    /// to the hub; pushes go through the queue.
    fn reconcile_key(
        &self,
        key: &str,
        remote: Option<&RemoteStat>,
        seq: u64,
    ) -> Result<(), HubError> {
        if key.is_empty() || self.is_ignored(key) {
            return Ok(());
        }
        let pending_rename = self.store.jobs().iter().any(
            |job| matches!(&job.kind, JobKind::Rename { from, to } if from == key || to == key),
        );
        if self.is_open_for_write(key) || pending_rename {
            self.rt.lock().recheck.insert(key.to_string());
            return Ok(());
        }
        let entry = self.store.entry(key);
        let local: Option<String> = entry
            .as_ref()
            .filter(|e| e.state != EntryState::Tombstone)
            .map(|e| e.sha256.clone());
        let base = self.store.base(key);
        let action = decide(
            local.as_deref(),
            base.as_ref().map(|b| b.sha256.as_str()),
            remote.map(|r| r.sha256.as_str()),
        );
        log::trace!(
            "reconcile {key}: L={local:?} B={:?} R={:?} → {action:?}",
            base.as_ref().map(|b| &b.sha256),
            remote.map(|r| &r.sha256)
        );
        match action {
            Action::Nothing => {}
            Action::Push { if_match } => {
                if if_match.is_none() && base.is_some() {
                    // Hub deleted it under us: the old base is void, push
                    // as a new object.
                    let _ = self.store.remove_base(key);
                }
                if let Some(mut e) = entry.clone() {
                    if e.state != EntryState::Dirty {
                        e.state = EntryState::Dirty;
                        let _ = self.store.put_entry(key, &e);
                    }
                }
                self.ensure_job(JobKind::Push {
                    key: key.to_string(),
                });
            }
            Action::DeleteRemote { .. } => {
                if self.opts.deletes == DeleteMode::Propagate {
                    self.ensure_job(JobKind::Delete {
                        key: key.to_string(),
                    });
                } else {
                    let _ = self.store.remove_entry(key);
                    let _ = self.store.remove_base(key);
                }
            }
            Action::Pull { .. } => {
                if let Some(r) = remote {
                    self.pull_key(key, r, seq, entry.as_ref())?;
                }
            }
            Action::TrashLocal => {
                if let Some(e) = entry {
                    let _ = self.store.put_trashed(
                        key,
                        &Trashed {
                            sha256: e.sha256.clone(),
                            size: e.size,
                            ts: super::now_ms(),
                        },
                    );
                }
                let _ = self.store.remove_jobs_for(key);
                let _ = self.store.remove_entry(key);
                let _ = self.store.remove_base(key);
                self.view_remove(key);
                log::info!("{key}: deleted on the hub; local copy kept in the mirror trash");
            }
            Action::Adopt => match entry {
                Some(mut e) => {
                    let _ = self.store.put_base(
                        key,
                        &Base {
                            sha256: e.sha256.clone(),
                            size: e.size,
                            mtime: remote.map(|r| r.mtime).unwrap_or(e.mtime),
                            remote_seq: seq,
                        },
                    );
                    e.state = EntryState::Clean;
                    let _ = self.store.put_entry(key, &e);
                    let _ = self.store.remove_jobs_for(key);
                }
                None => {
                    let _ = self.store.remove_entry(key);
                    let _ = self.store.remove_base(key);
                    let _ = self.store.remove_jobs_for(key);
                }
            },
            Action::Conflict { remote: r } => {
                let Some(e) = entry else {
                    return Ok(());
                };
                self.record_conflict(key, &e, base.as_ref(), &r);
            }
        }
        Ok(())
    }

    /// Take the hub's version of a key: bytes now when the key is pinned or
    /// its previous bytes were cached (the user evidently reads it), else a
    /// placeholder fetched on first read.
    fn pull_key(
        &self,
        key: &str,
        r: &RemoteStat,
        seq: u64,
        entry: Option<&Entry>,
    ) -> Result<(), HubError> {
        let want_bytes =
            self.is_pinned(key) || entry.map(|e| self.cache.has(&e.sha256)).unwrap_or(false);
        let mut sha = r.sha256.clone();
        if want_bytes && !self.cache.has(&sha) {
            match self.cache.fetch(&self.hub, key, &sha) {
                Ok(got) => sha = got,
                Err(HubError::NotFound) => {
                    // Gone between the feed and now; the next change says so.
                    return Ok(());
                }
                Err(e) if e.is_offline() => return Err(e),
                Err(e) => log::warn!("{key}: pull failed ({e}); serving on demand"),
            }
        }
        let size = if sha == r.sha256 {
            r.size
        } else {
            self.store
                .cache_meta(&sha)
                .map(|m| m.size)
                .unwrap_or(r.size)
        };
        let new_entry = Entry {
            sha256: sha.clone(),
            size,
            mtime: r.mtime,
            state: EntryState::Clean,
        };
        let _ = self.store.put_entry(key, &new_entry);
        let _ = self.store.put_base(
            key,
            &Base {
                sha256: sha,
                size,
                mtime: r.mtime,
                remote_seq: seq,
            },
        );
        let _ = self.store.remove_trashed(key);
        let _ = self.store.remove_jobs_for(key);
        self.view_upsert(key, size, r.mtime);
        Ok(())
    }

    fn record_conflict(&self, key: &str, entry: &Entry, base: Option<&Base>, remote_sha: &str) {
        if let Some(existing) = self.store.conflict(key) {
            if existing.local_sha256 == entry.sha256 && !existing.uploaded {
                // Already queued.
                self.ensure_job(JobKind::Conflict {
                    key: key.to_string(),
                    local_sha256: entry.sha256.clone(),
                    base_sha256: base.map(|b| b.sha256.clone()),
                });
                return;
            }
        }
        log::warn!("{key}: changed here and on the hub; uploading ours as a conflict");
        let _ = self.store.put_conflict(&Conflict {
            key: key.to_string(),
            local_sha256: entry.sha256.clone(),
            hub_sha256: Some(remote_sha.to_string()),
            base_sha256: base.map(|b| b.sha256.clone()),
            ts: super::now_ms(),
            mode: self.opts.conflicts,
            uploaded: false,
            inbox_doc_id: None,
            copy_key: None,
            resolved: false,
        });
        // The plain push is void now; the conflict job replaces it.
        let _ = self.store.remove_jobs_for(key);
        self.ensure_job(JobKind::Conflict {
            key: key.to_string(),
            local_sha256: entry.sha256.clone(),
            base_sha256: base.map(|b| b.sha256.clone()),
        });
    }

    fn ensure_job(&self, kind: JobKind) {
        let id = kind.dedupe_id();
        if self.store.jobs().iter().any(|j| j.kind.dedupe_id() == id) {
            return;
        }
        let _ = self.store.enqueue(kind);
    }

    /// Keys whose remote change we skipped because a handle was open:
    /// ask the hub what is there now and decide again.
    fn recheck_deferred(&self) -> Result<(), HubError> {
        let keys: Vec<String> = self.rt.lock().recheck.drain().collect();
        for key in keys {
            if self.is_open_for_write(&key) {
                self.rt.lock().recheck.insert(key);
                continue;
            }
            let stat = self.hub.head_object(&key)?;
            self.reconcile_key(&key, stat.as_ref(), 0)?;
        }
        Ok(())
    }

    /// Dirty entries and tombstones that lost their job (a crash between
    /// the entry write and the enqueue) get one back.
    fn recover_jobs(&self) {
        let jobs = self.store.jobs();
        let have: HashSet<String> = jobs.iter().map(|j| j.kind.dedupe_id()).collect();
        let skipped: HashSet<String> = self.rt.lock().skipped.keys().cloned().collect();
        for (key, e) in self.store.entries("") {
            if self.is_ignored(&key) || skipped.contains(&key) {
                continue;
            }
            let wanted = match e.state {
                EntryState::Clean => continue,
                EntryState::Dirty => {
                    if self
                        .store
                        .conflict(&key)
                        .map(|c| !c.uploaded)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    JobKind::Push { key: key.clone() }
                }
                EntryState::Tombstone => JobKind::Delete { key: key.clone() },
            };
            if !have.contains(&wanted.dedupe_id()) {
                let _ = self.store.enqueue(wanted);
            }
        }
    }

    /// Run the queue in order. A job in backoff blocks later jobs on the
    /// same keys (a push of `b` must not overtake the rename `a → b`), and
    /// the first offline answer stops the pass.
    fn drain_jobs(&self) -> Result<(), HubError> {
        // A job can queue another (a 412 turns a push into a conflict
        // upload); keep going until a pass runs nothing.
        for _round in 0..8 {
            let now = super::now_ms();
            let mut blocked: HashSet<String> = HashSet::new();
            let mut ran = false;
            let jobs = self.store.jobs();
            if jobs.is_empty() {
                break;
            }
            for job in jobs {
                if job.kind.keys().iter().any(|k| blocked.contains(*k)) {
                    continue;
                }
                if job.not_before > now {
                    for k in job.kind.keys() {
                        blocked.insert(k.to_string());
                    }
                    continue;
                }
                ran = true;
                self.run_one(&job, &mut blocked)?;
            }
            if !ran {
                break;
            }
        }
        Ok(())
    }

    fn run_one(&self, job: &Job, blocked: &mut HashSet<String>) -> Result<(), HubError> {
        {
            match self.run_job(job) {
                Ok(()) => {
                    let _ = self.store.remove_job(job.seq);
                    self.write_status(false);
                }
                Err(e) if e.is_offline() || matches!(e, HubError::Unauthorized) => return Err(e),
                Err(e) if e.is_permanent() => {
                    log::warn!("job {:?} refused by the hub: {e}", job.kind);
                    self.rt
                        .lock()
                        .skipped
                        .insert(job.kind.key().to_string(), e.to_string());
                    let _ = self.store.remove_job(job.seq);
                }
                Err(e) => {
                    let mut j = job.clone();
                    j.attempts += 1;
                    j.not_before = super::now_ms() + backoff_ms(j.attempts);
                    j.last_error = Some(e.to_string());
                    log::warn!(
                        "job {:?} failed (attempt {}): {e}; retry in {}s",
                        j.kind,
                        j.attempts,
                        backoff_ms(j.attempts) / 1000
                    );
                    let _ = self.store.update_job(&j);
                    for k in job.kind.keys() {
                        blocked.insert(k.to_string());
                    }
                }
            }
        }
        Ok(())
    }

    fn run_job(&self, job: &Job) -> Result<(), HubError> {
        match &job.kind {
            JobKind::Push { key } => self.run_push(key),
            JobKind::Delete { key } => self.run_delete(key),
            JobKind::Rename { from, to } => self.run_rename(from, to),
            JobKind::Conflict {
                key,
                local_sha256,
                base_sha256,
            } => self.run_conflict(key, local_sha256, base_sha256.as_deref()),
            JobKind::Mkdir { key } => match self.hub.mkdir(key) {
                Ok(()) | Err(HubError::Refused { status: 409, .. }) => Ok(()),
                Err(e) => Err(e),
            },
            JobKind::Rmdir { key } => match self.hub.rmdir(key) {
                Ok(()) | Err(HubError::NotFound) => Ok(()),
                Err(e) => Err(e),
            },
        }
    }

    fn run_push(&self, key: &str) -> Result<(), HubError> {
        let Some(entry) = self.store.entry(key) else {
            return Ok(());
        };
        if entry.state != EntryState::Dirty {
            return Ok(());
        }
        let path = self.cache.path_for(&entry.sha256);
        if !path.is_file() {
            log::error!(
                "{key}: dirty but its bytes ({}) are not in the cache",
                entry.sha256
            );
            return Err(HubError::Refused {
                status: 0,
                code: Some("BYTES_MISSING".into()),
                message: "local bytes missing from the cache".into(),
            });
        }
        let base = self.store.base(key);
        let opts = PutOptions {
            if_match: base.as_ref().map(|b| b.sha256.clone()),
            if_none_match_any: base.is_none(),
            sha256: Some(entry.sha256.clone()),
            mtime: Some(entry.mtime),
            ..Default::default()
        };
        match self.hub.put_object(key, &PutBody::File(path), &opts) {
            Ok(res) => {
                let sha = if res.sha256.is_empty() {
                    entry.sha256.clone()
                } else {
                    res.sha256.clone()
                };
                let mtime = if res.mtime > 0 {
                    res.mtime
                } else {
                    entry.mtime
                };
                let _ = self.store.put_base(
                    key,
                    &Base {
                        sha256: sha.clone(),
                        size: entry.size,
                        mtime,
                        remote_seq: res.seq,
                    },
                );
                // The user may have written again while the upload ran;
                // only a still-matching entry becomes clean.
                if let Some(mut now) = self.store.entry(key) {
                    if now.sha256 == sha && now.state == EntryState::Dirty {
                        now.state = EntryState::Clean;
                        now.mtime = mtime;
                        let _ = self.store.put_entry(key, &now);
                    }
                }
                log::info!("{key}: pushed ({} bytes)", entry.size);
                Ok(())
            }
            Err(HubError::PreconditionFailed { current }) => {
                // The hub moved; decide again with what it has now. This
                // yields a conflict (both changed), an adopt (same bytes) or
                // a fresh push (hub deleted it).
                log::info!("{key}: precondition failed; hub has {current:?}");
                self.reconcile_key(key, current.as_ref(), 0)
            }
            Err(e) => Err(e),
        }
    }

    fn run_delete(&self, key: &str) -> Result<(), HubError> {
        let base = self.store.base(key);
        let Some(base) = base else {
            let _ = self.store.remove_entry(key);
            return Ok(());
        };
        match self.hub.delete_object(key, Some(&base.sha256)) {
            Ok(()) | Err(HubError::NotFound) => {
                let _ = self.store.remove_entry(key);
                let _ = self.store.remove_base(key);
                log::info!("{key}: deleted on the hub");
                Ok(())
            }
            Err(HubError::PreconditionFailed { current }) => {
                // Edit beats delete: the hub's version comes back.
                self.reconcile_key(key, current.as_ref(), 0)
            }
            Err(e) => Err(e),
        }
    }

    fn run_rename(&self, from: &str, to: &str) -> Result<(), HubError> {
        let base = self.store.base(to);
        match self
            .hub
            .rename_object(from, to, base.as_ref().map(|b| b.sha256.as_str()))
        {
            Ok(()) => {
                log::info!("{from} → {to}: renamed on the hub");
                Ok(())
            }
            Err(HubError::NotFound) => {
                // The source is gone on the hub: our bytes at `to` are a new
                // file there.
                let _ = self.store.remove_base(to);
                if let Some(mut e) = self.store.entry(to) {
                    if self.cache.has(&e.sha256) {
                        e.state = EntryState::Dirty;
                        let _ = self.store.put_entry(to, &e);
                        self.ensure_job(JobKind::Push {
                            key: to.to_string(),
                        });
                    } else {
                        log::warn!(
                            "{to}: source vanished on the hub and bytes are not cached; dropping"
                        );
                        let _ = self.store.remove_entry(to);
                        self.view_remove(to);
                    }
                }
                Ok(())
            }
            Err(HubError::TargetExists) => {
                // Someone put something at the target meanwhile: take theirs
                // at `to`; our bytes are still the hub's `from`, which the
                // next full pass lists again.
                let stat = self.hub.head_object(to)?;
                let _ = self.store.remove_base(to);
                self.reconcile_key(to, stat.as_ref(), 0)?;
                Ok(())
            }
            Err(HubError::PreconditionFailed { current }) => {
                // The hub changed the source since we last saw it: keep our
                // moved bytes as a new `to`, and let the hub's `from` return.
                let _ = self.store.remove_base(to);
                if let Some(mut e) = self.store.entry(to) {
                    e.state = EntryState::Dirty;
                    let _ = self.store.put_entry(to, &e);
                    self.ensure_job(JobKind::Push {
                        key: to.to_string(),
                    });
                }
                self.reconcile_key(from, current.as_ref(), 0)
            }
            Err(e) => Err(e),
        }
    }

    fn run_conflict(
        &self,
        key: &str,
        local_sha: &str,
        base_sha: Option<&str>,
    ) -> Result<(), HubError> {
        let path = self.cache.path_for(local_sha);
        if !path.is_file() {
            log::error!("{key}: conflict bytes {local_sha} missing from the cache");
            return Ok(());
        }
        let mut record = self.store.conflict(key).unwrap_or(Conflict {
            key: key.to_string(),
            local_sha256: local_sha.to_string(),
            hub_sha256: None,
            base_sha256: base_sha.map(str::to_string),
            ts: super::now_ms(),
            mode: self.opts.conflicts,
            uploaded: false,
            inbox_doc_id: None,
            copy_key: None,
            resolved: false,
        });
        let mtime = self.store.entry(key).map(|e| e.mtime).unwrap_or(0);
        let opts = PutOptions {
            sha256: Some(local_sha.to_string()),
            mtime: Some(mtime),
            conflict_of: Some(key.to_string()),
            conflict_mode: Some(self.opts.conflicts),
            base_sha256: base_sha.map(str::to_string),
            ..Default::default()
        };
        let target = match self.opts.conflicts {
            ConflictMode::Prompt => key.to_string(),
            ConflictMode::Rename => {
                super::conflict_copy_key(key, &self.device.name, chrono::Utc::now())
            }
        };
        let res = self.hub.put_object(&target, &PutBody::File(path), &opts)?;
        record.uploaded = true;
        record.inbox_doc_id = res.doc_id;
        if self.opts.conflicts == ConflictMode::Rename {
            record.copy_key = Some(target.clone());
        }
        let _ = self.store.put_conflict(&record);
        log::info!("{key}: conflict copy uploaded ({target})");

        // Now the hub version keeps the name: adopt it. Ask the hub rather
        // than trusting the stat that raised the conflict — it may have
        // moved again.
        match self.hub.head_object(key)? {
            Some(stat) => {
                let entry = self.store.entry(key);
                let force_bytes = entry
                    .as_ref()
                    .map(|e| self.cache.has(&e.sha256))
                    .unwrap_or(false);
                let mut sha = stat.sha256.clone();
                if (force_bytes || self.is_pinned(key)) && !self.cache.has(&sha) {
                    if let Ok(got) = self.cache.fetch(&self.hub, key, &sha) {
                        sha = got;
                    }
                }
                let new_entry = Entry {
                    sha256: sha.clone(),
                    size: stat.size,
                    mtime: stat.mtime,
                    state: EntryState::Clean,
                };
                let _ = self.store.put_entry(key, &new_entry);
                let _ = self.store.put_base(
                    key,
                    &Base {
                        sha256: sha,
                        size: stat.size,
                        mtime: stat.mtime,
                        remote_seq: 0,
                    },
                );
                self.view_upsert(key, stat.size, stat.mtime);
            }
            None => {
                // Hub deleted it after all: ours is the only version, push it
                // as new.
                let _ = self.store.remove_base(key);
                if let Some(mut e) = self.store.entry(key) {
                    e.state = EntryState::Dirty;
                    let _ = self.store.put_entry(key, &e);
                }
                self.ensure_job(JobKind::Push {
                    key: key.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Pinned keys get their bytes; everything pinned is marked so the
    /// cache never evicts it.
    fn materialize_pins(&self) -> Result<(), HubError> {
        let pins = self.pins();
        if pins.is_empty() {
            return Ok(());
        }
        for (key, e) in self.store.entries("") {
            if e.state == EntryState::Tombstone || !super::is_pinned(&pins, &key) {
                continue;
            }
            if self.cache.has(&e.sha256) {
                self.cache.set_pinned(&e.sha256, true);
                continue;
            }
            if e.size > self.cache.budget() {
                continue;
            }
            match self.cache.fetch(&self.hub, &key, &e.sha256) {
                Ok(got) => {
                    self.cache.set_pinned(&got, true);
                    if got != e.sha256 {
                        self.rt.lock().recheck.insert(key.clone());
                    }
                }
                Err(e) if e.is_offline() => return Err(e),
                Err(err) => log::warn!("{key}: pin fetch failed: {err}"),
            }
        }
        Ok(())
    }

    /// Cache eviction, trash expiry. Runs even offline.
    fn housekeeping(&self) {
        let now = super::now_ms();
        for (key, t) in self.store.trash() {
            if now.saturating_sub(t.ts) > TRASH_TTL_MS {
                let _ = self.store.remove_trashed(&key);
            }
        }
        let pins = self.pins();
        let mut protected: HashSet<String> = HashSet::new();
        for (key, e) in self.store.entries("") {
            let keep = e.state != EntryState::Clean
                || super::is_pinned(&pins, &key)
                || self.is_open_for_write(&key);
            if keep {
                protected.insert(e.sha256.clone());
            } else {
                self.cache.set_pinned(&e.sha256, false);
            }
        }
        for (_, t) in self.store.trash() {
            protected.insert(t.sha256);
        }
        for c in self.store.conflicts() {
            protected.insert(c.local_sha256);
        }
        let freed = self.cache.evict(&protected);
        if freed > 0 {
            log::debug!("cache: evicted {freed} bytes");
        }
    }
}
