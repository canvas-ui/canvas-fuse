//! The sync engine and the `Mirror` facade.
//!
//! `Mirror` is what the rest of the mount holds: the store, the real folder
//! (`local`), the hub client, and the local operations the FUSE layer calls
//! on the session thread (`commit_write`, `delete_local`, `rename_local`,
//! reads). Those never touch the network — they change the folder and the
//! store, queue a job, then wake the engine.
//!
//! The engine is one thread (`run`) that owns every network round-trip:
//!
//! 1. on a full pass, scan the folder: what changed while the daemon was
//!    down (or before it ever ran) becomes a push, a delete, a new entry;
//! 2. upload queued local work before background reads; service fresh work
//!    between background requests, preserving path dependencies and backoff;
//! 3. catch up on the hub's change feed (or rebuild from the listing when
//!    there is no cursor / the cursor is too old), reconciling every touched
//!    key with the three-way table in `reconcile.rs`;
//! 4. expire the local trash, write the status file, report to the hub.
//!
//! It wakes on a nudge (`backend.changed` on the socket), on a local write,
//! on `sync now`, and on the poll timer. Offline is a state, not an error:
//! the engine notes it, the mount keeps serving the folder, and the queue
//! waits for the next wake that finds the hub back.

use super::hub::{Change, ChangeOp, HubClient, HubError, PutBody, PutOptions, RemoteStat};
use super::local::Local;
use super::reconcile::{decide, Action};
use super::store::{
    Base, Conflict, Entry, EntryState, Job, JobKind, JobPriority, Store, Trashed, HEAD_KEY,
    META_INSTANCE, META_LISTED,
};
use super::{ConflictMode, DeleteMode, DeviceIdentity, IgnoreRules, MirrorOptions};
use crate::state::{Invalidation, Tree};
use anyhow::{Context as _, Result};
use fuser::ReplyData;
use parking_lot::{Mutex, ReentrantMutex, ReentrantMutexGuard, RwLock};
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
/// A full pass (folder scan + hub listing) this often even without
/// `sync now`, so a change the feed lost is bounded.
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
    /// The real folder the mirror keeps.
    #[serde(default)]
    pub home: String,
    pub state: SyncState,
    pub cursor: Option<u64>,
    pub head: Option<u64>,
    pub pending: u64,
    pub failed: u64,
    pub conflicts: u64,
    pub skipped: u64,
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
    /// `sync now`: a full pass (scan + listing), then answer when done.
    SyncNow(Option<Sender<()>>),
    /// The socket re-authenticated: the hub is back.
    Reconnect,
    Stop,
}

/// How the engine reaches the FUSE view. None in tests (no tree).
pub struct ViewLink {
    pub tree: Arc<RwLock<Tree>>,
    /// The Home-only view lock. Virtual tree refreshes use a separate lock;
    /// neither network requests nor background hashing may hold this one.
    pub refresh_lock: Arc<Mutex<()>>,
    pub invalidations: Arc<Mutex<Vec<Invalidation>>>,
    pub job_tx: Sender<crate::worker::Job>,
}

pub struct MirrorConfig {
    pub data_dir: PathBuf,
    /// The real folder: `<mountpoint>/Home` (or the mountpoint itself for a
    /// home-only mount). Opened before the kernel mount covers it.
    pub home_dir: PathBuf,
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

/// What a folder scan found and did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
}

pub struct Mirror {
    pub store: Arc<Store>,
    pub local: Arc<Local>,
    pub hub: Arc<HubClient>,
    pub device: DeviceIdentity,
    pub opts: MirrorOptions,
    pub mountpoint: PathBuf,
    status_path: Option<PathBuf>,
    ignore: Arc<RwLock<IgnoreRules>>,
    rt: Mutex<Runtime>,
    engine_tx: Arc<Mutex<Option<Sender<EngineMsg>>>>,
    local_changes: Arc<Mutex<super::watch::Changes>>,
    watcher: Mutex<Option<super::watch::Watcher>>,
    view: Mutex<Option<ViewLink>>,
    /// Keys with an open write handle: remote landings wait for the close.
    open_writes: Mutex<HashSet<String>>,
    directory_moves: ReentrantMutex<()>,
    directory_epoch: std::sync::atomic::AtomicU64,
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
        let local = Arc::new(Local::open(&cfg.home_dir, &cfg.data_dir)?);
        let hub = Arc::new(HubClient::with_tls(
            &cfg.server,
            &cfg.token,
            &cfg.workspace_id,
            &cfg.backend,
            &cfg.device,
            cfg.tls.as_ref(),
        )?);
        let cache = super::legacy_cache_dir(&cfg.data_dir);
        if cache.is_dir() {
            log::warn!(
                "{}: content cache of an older mirror, no longer read (its files were moved to {}); delete it",
                cache.display(),
                cfg.home_dir.display()
            );
        }
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
        if let Some(legacy) = store.take_legacy() {
            migrate_legacy(
                &store,
                &local,
                &super::legacy_cache_dir(&cfg.data_dir),
                legacy,
                &IgnoreRules::new(&patterns),
            );
        }

        let mirror = Arc::new(Self {
            store,
            local,
            hub,
            device: cfg.device,
            opts: cfg.opts,
            mountpoint: cfg.mountpoint,
            status_path: cfg.status_path,
            ignore: Arc::new(RwLock::new(IgnoreRules::new(patterns))),
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
            engine_tx: Arc::new(Mutex::new(None)),
            local_changes: Arc::new(Mutex::new(super::watch::Changes::default())),
            watcher: Mutex::new(None),
            view: Mutex::new(None),
            open_writes: Mutex::new(HashSet::new()),
            directory_moves: ReentrantMutex::new(()),
            directory_epoch: std::sync::atomic::AtomicU64::new(0),
        });
        // Watches are live before the initial scan, so changes made during
        // startup cannot fall between scanning and starting observation.
        let changes = mirror.local_changes.clone();
        let engine_tx = mirror.engine_tx.clone();
        *mirror.watcher.lock() = Some(super::watch::Watcher::start(
            mirror.local.clone(),
            mirror.ignore.clone(),
            move |incoming| {
                let mut pending = changes.lock();
                pending.rescan |= incoming.rescan;
                pending.paths.extend(incoming.paths);
                if pending.paths.len() > 8192 {
                    pending.rescan = true;
                }
                if pending.rescan {
                    pending.paths.clear();
                }
                drop(pending);
                if let Some(tx) = engine_tx.lock().as_ref() {
                    let _ = tx.send(EngineMsg::Wake);
                }
            },
        )?);
        // What happened to the folder while no daemon was looking is the
        // first thing to know — before the view is built, before the hub
        // is asked anything.
        let report = mirror.scan_local();
        if report != ScanReport::default() {
            log::info!(
                "mirror: folder scan found {} new, {} changed, {} removed since the last run",
                report.added,
                report.changed,
                report.removed
            );
        }
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

    fn view_ensure_dir(&self, key: &str) {
        self.with_view(|t| t.ensure_home_dir_key(key));
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
        self.wake();
    }

    pub fn open_local_write(
        &self,
        key: &str,
        create: bool,
        truncate: bool,
    ) -> Result<std::fs::File> {
        let _guard = self.directory_moves.lock();
        super::local::check_key(key)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(create)
            .truncate(truncate)
            .open(self.local.path(key))?;
        self.note_open_write(key);
        Ok(file)
    }

    /// Persist the dirty intent without hashing on the filesystem request thread.
    /// Empty digest means the engine must hash the closed file before upload.
    pub fn commit_local_file(&self, key: &str) -> Result<()> {
        let _guard = self.directory_moves.lock();
        let st = self.local.stat(key).context("local file disappeared")?;
        self.store.put_entry(
            key,
            &Entry {
                sha256: String::new(),
                size: st.size,
                mtime: st.mtime,
                state: EntryState::Dirty,
            },
        )?;
        self.record_dirs_for(key);
        if self.is_ignored(key) {
            self.rt.lock().skipped.insert(
                key.into(),
                "matches an exclusion rule; kept local only".into(),
            );
        } else {
            self.rt.lock().skipped.remove(key);
            self.store.enqueue(JobKind::Push { key: key.into() })?;
        }
        self.wake();
        Ok(())
    }

    fn is_open_for_write(&self, key: &str) -> bool {
        self.open_writes.lock().contains(key)
    }

    /// Every directory a key passes through exists on disk now; the store
    /// (and so the view) says so too.
    fn record_dirs_for(&self, key: &str) {
        let mut p = super::parent_key(key);
        while !p.is_empty() {
            if self.store.has_dir(p) {
                break;
            }
            let _ = self.store.add_dir(p);
            p = super::parent_key(p);
        }
    }

    /// A flushed write: bytes into the folder, entry Dirty, push queued.
    pub fn commit_write(&self, key: &str, bytes: &[u8]) -> Result<Entry> {
        let _guard = self.directory_moves.lock();
        let h = self.local.write_atomic(key, bytes)?;
        let entry = Entry {
            sha256: h.sha256,
            size: h.size,
            mtime: h.mtime,
            state: EntryState::Dirty,
        };
        self.store.put_entry(key, &entry)?;
        self.record_dirs_for(key);
        if self.store.trashed(key).is_some() {
            let _ = self.store.remove_trashed(key);
            self.local.trash_remove(key);
        }
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

    /// `rm`: the file goes, then tombstone + delete job (propagate), or
    /// forget (keep).
    pub fn delete_local(&self, key: &str) -> Result<bool> {
        let _guard = self.directory_moves.lock();
        if self.store.entry(key).is_none() {
            let existed = self.local.exists(key);
            self.local.remove_file(key)?;
            return Ok(existed);
        }
        self.local.remove_file(key)?;
        self.note_local_gone(key)?;
        self.wake();
        Ok(true)
    }

    /// The file is gone from the folder (an `rm`, or the scan found it
    /// missing): tell the hub, or just forget it.
    fn note_local_gone(&self, key: &str) -> Result<()> {
        let Some(entry) = self.store.entry(key) else {
            return Ok(());
        };
        self.store.remove_jobs_for(key)?;
        let base = self.store.base(key);
        let may_be_uploaded =
            base.is_some() || self.store.meta(&format!("upload-pending:{key}")).is_some();
        match (self.opts.deletes, may_be_uploaded, entry.state) {
            (DeleteMode::Propagate, true, _) if !self.is_ignored(key) => {
                self.store.put_entry(
                    key,
                    &Entry {
                        state: EntryState::Tombstone,
                        ..entry
                    },
                )?;
                self.store.enqueue(JobKind::Delete {
                    key: key.to_string(),
                    if_match: None,
                })?;
            }
            _ => {
                // Never pushed (or `--deletes keep`): nothing to tell the hub.
                self.store.remove_entry(key)?;
                self.store.remove_base(key)?;
            }
        }
        self.rt.lock().skipped.remove(key);
        Ok(())
    }

    pub fn mkdir_local(&self, key: &str) -> Result<()> {
        let _guard = self.directory_moves.lock();
        self.local.mkdir(key)?;
        self.record_dirs_for(key);
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
        let _guard = self.directory_moves.lock();
        if self.store.dir_has_children(key) {
            return Ok(false);
        }
        match self.local.rmdir(key) {
            Ok(()) => {}
            // Something on disk the store does not know (a dotfile written
            // around the mount, a stray part file): still not empty.
            Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) => return Ok(false),
            Err(e) => return Err(e.into()),
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

    /// `mv` of a file: the file moves, then the entry, the base and the
    /// pending jobs are re-keyed; a rename job goes out only when the hub
    /// knows the source (has a base). A file that was never pushed just
    /// gets pushed under its new name.
    pub fn rename_local(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let _guard = self.directory_moves.lock();
        self.directory_epoch.fetch_add(1, Ordering::Relaxed);
        if self.store.base(from).is_none() && self.store.entry(to).is_some() {
            // Editors commonly save a fresh temp file, then rename it over
            // the original. This is a conditional PUT of the destination,
            // not DELETE + RENAME (the temp has never existed upstream).
            self.local.rename(from, to)?;
            self.store.remove_jobs_for(from)?;
            if self.store.meta(&format!("upload-pending:{from}")).is_some() {
                self.store.enqueue(JobKind::Delete {
                    key: from.into(),
                    if_match: None,
                })?;
            }
            self.store.remove_jobs_for(to)?;
            self.store.rekey(from, to)?; // retains the destination's base
            {
                let mut writes = self.open_writes.lock();
                if writes.remove(from) {
                    writes.insert(to.to_string());
                }
            }
            if let Some(mut entry) = self.store.entry(to) {
                entry.state = EntryState::Dirty;
                self.store.put_entry(to, &entry)?;
            }
            if !self.is_ignored(to) {
                self.store.enqueue(JobKind::Push {
                    key: to.to_string(),
                })?;
            }
            self.record_dirs_for(to);
            self.wake();
            return Ok(());
        }
        if self.store.entry(to).is_some() {
            // Overwrite-rename: the destination goes first, in the queue
            // too, so the hub sees delete(to) then rename(from → to).
            let destination_base = self.store.base(to);
            self.delete_local(to)?;
            if let Some(base) = destination_base {
                self.store.enqueue(JobKind::Delete {
                    key: to.to_string(),
                    if_match: Some(base.sha256),
                })?;
            }
        }
        self.local.rename(from, to)?;
        self.rekey_entry(from, to)?;
        self.wake();
        Ok(())
    }

    /// The store side of a rename; the bytes already moved.
    fn rekey_entry(&self, from: &str, to: &str) -> Result<()> {
        let dropped = self.store.remove_jobs_for(from)?;
        self.store.rekey(from, to)?;
        self.record_dirs_for(to);
        if let Some(c) = self.store.conflict(from) {
            self.store.remove_conflict(from)?;
            self.store.put_conflict(&Conflict {
                key: to.to_string(),
                ..c
            })?;
        }
        if self.store.trashed(to).is_some() {
            let _ = self.store.remove_trashed(to);
            self.local.trash_remove(to);
        }
        let has_base = self.store.base(to).is_some();
        let may_be_uploaded =
            has_base || self.store.meta(&format!("upload-pending:{from}")).is_some();
        if may_be_uploaded && !self.is_ignored(from) {
            if self.is_ignored(to) {
                // Moving INTO an excluded name: the hub cannot hold it there.
                // Delete the source on the hub; the bytes stay local.
                let source_base = self.store.base(to);
                self.store.remove_base(to)?;
                self.store.enqueue(JobKind::Delete {
                    key: from.to_string(),
                    if_match: source_base.map(|b| b.sha256),
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
        Ok(())
    }

    /// A directory move is one durable operation, never a batch of file moves.
    pub fn rename_dir_local(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        use std::os::unix::fs::MetadataExt;
        let _guard = self.directory_moves.lock();
        self.directory_epoch.fetch_add(1, Ordering::Relaxed);
        anyhow::ensure!(
            !super::store::overlaps(from, to),
            "cannot move a directory into itself"
        );
        anyhow::ensure!(
            !self.store.dir_has_children(to) && self.store.entry(to).is_none(),
            "target exists"
        );
        let local_only = self.is_ignored(from) && self.is_ignored(to)
            || (self.store.bases(&format!("{from}/")).is_empty()
                && self
                    .store
                    .jobs()
                    .iter()
                    .any(|j| matches!(&j.kind, JobKind::Mkdir { key } if key == from)));
        anyhow::ensure!(
            local_only || (!self.is_ignored(from) && !self.is_ignored(to)),
            "cannot move a synced directory across an exclusion boundary"
        );
        for (key, _) in self.store.bases(&format!("{from}/")) {
            anyhow::ensure!(
                !self.is_ignored(&format!("{to}{}", &key[from.len()..])),
                "destination excludes a synced file"
            );
        }
        let st = std::fs::metadata(self.local.path(from))?;
        let seq = self.store.enqueue(JobKind::RenameDir {
            from: from.to_string(),
            to: to.to_string(),
            operation_id: super::operation_id()?,
            dev: st.dev(),
            ino: st.ino(),
            local_applied: false,
            remote: !local_only,
        })?;
        let job = self
            .store
            .jobs()
            .into_iter()
            .find(|j| j.seq == seq)
            .expect("queued directory move");
        if let Err(error) = self.finish_directory_move(&job) {
            // Cancel only if the syscall did not move the source. Otherwise
            // retain the intent so a restart can finish the ledger commit.
            if self.local.is_dir(from) {
                let _ = self.store.remove_job(seq);
            }
            return Err(error);
        }
        self.wake();
        Ok(())
    }

    /// Take this before the Home view lock. Protect only local namespace and
    /// ledger commits; release it before HTTP or background hashing. Delayed
    /// responses are validated or translated through the queued local moves.
    pub fn lock_directory_moves(&self) -> ReentrantMutexGuard<'_, ()> {
        self.directory_moves.lock()
    }

    fn pending_path_move(&self, key: &str) -> bool {
        self.store.jobs().iter().any(|j| {
            matches!(j.kind, JobKind::Rename { .. } | JobKind::RenameDir { .. })
                && j.kind.keys().iter().any(|p| super::store::overlaps(key, p))
        })
    }

    fn pending_directory_move(&self, key: &str) -> bool {
        self.store.jobs().iter().any(|job| {
            matches!(&job.kind, JobKind::RenameDir { from, to, .. }
            if super::store::under(key, from) || super::store::under(key, to))
        })
    }

    fn finish_directory_move(&self, job: &Job) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let JobKind::RenameDir {
            from,
            to,
            dev,
            ino,
            local_applied: false,
            ..
        } = &job.kind
        else {
            return Ok(());
        };
        let is_original = |key: &str| {
            std::fs::metadata(self.local.path(key))
                .is_ok_and(|st| st.is_dir() && st.dev() == *dev && st.ino() == *ino)
        };
        if is_original(from) {
            self.local.rename(from, to)?;
        }
        anyhow::ensure!(
            is_original(to),
            "directory move intent no longer matches its local inode"
        );
        self.store.apply_directory_rename(job)?;
        self.record_dirs_for(to);
        let mut writes = self.open_writes.lock();
        *writes = writes
            .iter()
            .map(|key| {
                if super::store::under(key, from) {
                    format!("{to}{}", &key[from.len()..])
                } else {
                    key.clone()
                }
            })
            .collect();
        Ok(())
    }

    fn recover_directory_moves(&self) -> Result<(), HubError> {
        let _guard = self.directory_moves.lock();
        for job in self.store.jobs() {
            self.finish_directory_move(&job)
                .map_err(|e| HubError::Other(e.to_string()))?;
        }
        Ok(())
    }

    /// The bytes an editor starts from: the file, as it is.
    pub fn bytes_for_edit(&self, key: &str) -> Result<Vec<u8>, i32> {
        if self.entry(key).is_none() {
            return Err(libc::ENOENT);
        }
        self.local.read_all(key).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                libc::ENOENT
            } else {
                log::warn!("{key}: read failed: {e}");
                libc::EIO
            }
        })
    }

    // ── reads (session thread; local pread) ─────────────────────────────────

    pub fn read(&self, key: &str, offset: i64, size: u32, reply: ReplyData) {
        if self.entry(key).is_none() {
            reply.error(libc::ENOENT);
            return;
        }
        match self.local.pread(key, offset.max(0) as u64, size as usize) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => reply.error(libc::ENOENT),
            Err(e) => {
                log::warn!("{key}: read failed: {e}");
                reply.error(libc::EIO);
            }
        }
    }

    // ── trash / conflicts (CLI) ─────────────────────────────────────────────

    pub fn trash_list(&self) -> Vec<(String, Trashed)> {
        self.store.trash()
    }

    /// Bring a hub-deleted key back from the local trash: a new push.
    pub fn trash_restore(&self, key: &str) -> Result<()> {
        self.store
            .trashed(key)
            .with_context(|| format!("{key} is not in the mirror trash"))?;
        if self.store.entry(key).is_some() {
            anyhow::bail!("{key} exists again; restore to a different name first");
        }
        let h = self.local.trash_restore(key)?;
        self.store.remove_base(key)?;
        let entry = Entry {
            sha256: h.sha256,
            size: h.size,
            mtime: h.mtime,
            state: EntryState::Dirty,
        };
        self.store.put_entry(key, &entry)?;
        self.record_dirs_for(key);
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
        if let Some(c) = self.store.conflict(key) {
            let _ = self.store.remove_conflict(key);
            self.local.conflict_remove(&c.local_sha256);
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
            home: self.local.display_path().to_string_lossy().to_string(),
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
            "path": s.home,
            "prefixes": Vec::<String>::new(),
            "cursor": s.cursor.unwrap_or(0),
            "pending": s.pending,
            "failed": s.failed,
            "conflicts": s.conflicts,
            "skipped": s.skipped,
            "state": s.state,
            "direction": "bi",
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

    // ── the folder scan ──────────────────────────────────────────────────────

    /// Reconcile the store with what is in the folder: files edited, added
    /// or removed while no daemon was running become pushes and deletes;
    /// directories that appeared or vanished are recorded. Pure local work,
    /// so it runs offline too. Keys with an open write handle are left to
    /// their close.
    pub fn scan_local(&self) -> ScanReport {
        self.scan_paths(&[String::new()], JobPriority::Discovered)
    }

    fn process_local_events(&self) {
        let changes = std::mem::take(&mut *self.local_changes.lock());
        if changes.rescan {
            log::warn!("mirror: recovering local changes with a full scan");
            self.scan_local();
        } else if !changes.paths.is_empty() {
            let mut paths: Vec<_> = changes.paths.into_iter().collect();
            paths.sort();
            let mut roots: Vec<String> = Vec::new();
            for path in paths {
                if !roots.iter().any(|root| super::store::under(&path, root)) {
                    roots.push(path);
                }
            }
            self.scan_paths(&roots, JobPriority::Interactive);
        }
    }

    fn scan_paths(&self, paths: &[String], priority: JobPriority) -> ScanReport {
        let epoch = self.directory_epoch.load(Ordering::Relaxed);
        let mut report = ScanReport::default();
        let mut seen_files: HashSet<String> = HashSet::new();
        let mut seen_dirs: HashSet<String> = HashSet::new();
        let affected = |key: &str| {
            paths
                .iter()
                .any(|p| p.is_empty() || super::store::under(key, p))
        };
        for (key, st) in paths.iter().flat_map(|p| self.local.walk_from(p)) {
            let guard = self.directory_moves.lock();
            if self.directory_epoch.load(Ordering::Relaxed) != epoch {
                return report;
            }
            if st.is_dir {
                seen_dirs.insert(key.clone());
                if !self.store.has_dir(&key) {
                    let _ = self.store.add_dir(&key);
                    self.view_ensure_dir(&key);
                    if !self.is_ignored(&key) {
                        let _ = self
                            .store
                            .enqueue_with_priority(JobKind::Mkdir { key }, priority);
                    }
                }
                continue;
            }
            seen_files.insert(key.clone());
            if self.pending_directory_move(&key) {
                continue;
            }
            if self.is_open_for_write(&key) {
                continue;
            }
            let entry = self.store.entry(&key);
            let live = entry.as_ref().filter(|e| e.state != EntryState::Tombstone);
            // A close-write/rename event is authoritative even if two saves
            // happen within the same millisecond with equal file lengths.
            let notified_file = priority == JobPriority::Interactive && paths.contains(&key);
            let stamp_key = format!("local-stamp:{key}");
            if !notified_file
                && live.is_some_and(|e| e.size == st.size && e.mtime == st.mtime)
                && self.store.meta(&stamp_key).as_deref() == Some(st.fingerprint.as_str())
            {
                continue;
            }
            drop(guard);
            let Ok(h) = self.local.hash(&key) else {
                continue;
            };
            let _guard = self.directory_moves.lock();
            if self.directory_epoch.load(Ordering::Relaxed) != epoch {
                return report;
            }
            if self.is_open_for_write(&key)
                || self.local.stat(&key) != Some(st.clone())
                || self.store.entry(&key) != entry
            {
                continue;
            }
            let _ = self.store.set_meta(&stamp_key, &st.fingerprint);
            if let Some(e) = live {
                if e.sha256 == h.sha256 {
                    // Touched, not changed: remember the new stamp so the
                    // next scan is cheap again.
                    let _ = self.store.put_entry(
                        &key,
                        &Entry {
                            size: h.size,
                            mtime: h.mtime,
                            ..e.clone()
                        },
                    );
                    continue;
                }
                report.changed += 1;
            } else {
                report.added += 1;
            }
            let new = Entry {
                sha256: h.sha256,
                size: h.size,
                mtime: h.mtime,
                state: EntryState::Dirty,
            };
            let _ = self.store.put_entry(&key, &new);
            if self.store.trashed(&key).is_some() {
                let _ = self.store.remove_trashed(&key);
                self.local.trash_remove(&key);
            }
            if self.is_ignored(&key) {
                self.rt.lock().skipped.insert(
                    key.clone(),
                    "matches an exclusion rule; kept local only".into(),
                );
            } else {
                self.rt.lock().skipped.remove(&key);
                // `enqueue`, not `ensure_job`: the user did something new,
                // so a push in backoff starts over.
                let _ = self
                    .store
                    .enqueue_with_priority(JobKind::Push { key: key.clone() }, priority);
            }
            self.view_upsert(&key, new.size, new.mtime);
        }
        let _guard = self.directory_moves.lock();
        if self.directory_epoch.load(Ordering::Relaxed) != epoch {
            return report;
        }
        for (key, e) in self.store.entries("") {
            if !affected(&key)
                || e.state == EntryState::Tombstone
                || seen_files.contains(&key)
                || self.pending_directory_move(&key)
                || self.local.is_file(&key)
                || self.is_open_for_write(&key)
                || !matches!(std::fs::symlink_metadata(self.local.path(&key)), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
            {
                continue;
            }
            report.removed += 1;
            let _ = self.note_local_gone(&key);
            self.view_remove(&key);
        }
        for d in self.store.dirs() {
            if !affected(&d)
                || seen_dirs.contains(&d)
                || self.pending_directory_move(&d)
                || !matches!(std::fs::symlink_metadata(self.local.path(&d)), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
            {
                continue;
            }
            let _ = self.store.remove_dir(&d);
            let _ = self.store.remove_jobs_for(&d);
            if !self.is_ignored(&d) {
                let _ = self
                    .store
                    .enqueue_with_priority(JobKind::Rmdir { key: d.clone() }, priority);
            }
            self.view_remove(&d);
        }
        if report != ScanReport::default() {
            self.wake();
        }
        report
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

    /// One pass: observe local changes, upload first, then catch up and report.
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
        self.recover_directory_moves()?;
        if full {
            // Local first: what the folder says is true whether or not the
            // hub answers.
            let report = self.scan_local();
            if report != ScanReport::default() {
                log::info!(
                    "mirror: folder scan found {} new, {} changed, {} removed",
                    report.added,
                    report.changed,
                    report.removed
                );
            }
        }
        self.process_local_events();
        self.refresh_hub_rules()?;
        self.recover_jobs();
        self.drain_jobs(64)?;
        if full
            || self.store.meta("directory-refresh").as_deref() == Some("1")
            || self.store.cursor().is_none()
            || self.store.meta(META_LISTED).is_none()
        {
            self.rebuild_from_listing()?;
            self.rt.lock().last_full = Some(Instant::now());
        } else {
            self.catch_up()?;
        }
        self.recheck_deferred()?;
        self.recover_jobs();
        self.drain_jobs(64)?;
        if self.store.meta("directory-refresh").as_deref() == Some("1") {
            self.rebuild_from_listing()?;
        }
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
        let epoch = self.directory_epoch.load(Ordering::Relaxed);
        let mut touched = HashSet::new();
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
            touched.extend(self.drain_jobs(16)?);
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
            touched.extend(self.drain_jobs(16)?);
            // This snapshot predates a prioritized mutation. Its feed echo
            // (after `head`) will reconcile it against current server state.
            if touched.iter().any(|path| super::store::under(&key, path)) {
                continue;
            }
            self.reconcile_key(&key, remote.get(&key), 0)?;
        }
        let _ = self.store.set_cursor(head);
        let _ = self.store.set_number(HEAD_KEY, head);
        let _ = self.store.set_meta(META_LISTED, "1");
        let moved = self.directory_epoch.load(Ordering::Relaxed) != epoch;
        let _ = self
            .store
            .set_meta("directory-refresh", if moved { "1" } else { "0" });
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
                self.drain_jobs(16)?;
                if self.store.meta("directory-refresh").as_deref() == Some("1") {
                    return self.rebuild_from_listing();
                }
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
        let epoch = self.directory_epoch.load(Ordering::Relaxed);
        let key = super::normalize_key(&change.key);
        if self.pending_directory_move(&key)
            || change
                .from
                .as_deref()
                .is_some_and(|from| self.pending_directory_move(from))
        {
            return Ok(()); // completion refreshes the subtree in a bulk listing
        }
        // Feed entries are notifications, not snapshots. An old put (including
        // our own echo) must never roll the base back or invent a conflict.
        let stat = self.hub.head_object(&key)?;
        if self.directory_epoch.load(Ordering::Relaxed) != epoch
            || self.pending_directory_move(&key)
        {
            return Ok(());
        }
        if change.op == ChangeOp::Rename {
            if let Some(from) = change.from.as_deref().map(super::normalize_key) {
                let source = self.hub.head_object(&from)?;
                if self.directory_epoch.load(Ordering::Relaxed) != epoch {
                    return Ok(());
                }
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

    /// A hub-side rename: move the file and re-key locally when our copy is
    /// clean; a dirty source is a new file under the old name (delete + add
    /// semantics), so it stays put and pushes as new.
    fn apply_remote_rename(&self, from: &str, to: &str, seq: u64) -> Result<(), HubError> {
        let _guard = self.directory_moves.lock();
        if self.pending_directory_move(from) || self.pending_directory_move(to) {
            return Ok(());
        }
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
            let _ = self.local.remove_file(from);
            let _ = self.store.remove_entry(from);
            let _ = self.store.remove_base(from);
            self.view_remove(from);
            return Ok(());
        }
        if let Err(e) = self.local.rename(from, to) {
            // The file is not where the store says; the next full pass
            // sorts the folder out, and `to` pulls on its own.
            log::warn!("{from} → {to}: local move failed ({e:#}); pulling instead");
            let _ = self.store.remove_entry(from);
            let _ = self.store.remove_base(from);
            self.view_remove(from);
            return Ok(());
        }
        let _ = self.store.rekey(from, to);
        self.record_dirs_for(to);
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
        let _guard = self.directory_moves.lock();
        if key.is_empty() || self.is_ignored(key) {
            return Ok(());
        }
        if self.pending_directory_move(key) {
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
                        if_match: None,
                    });
                } else {
                    let _ = self.store.remove_entry(key);
                    let _ = self.store.remove_base(key);
                }
            }
            Action::Pull { .. } => {
                drop(_guard);
                if let Some(r) = remote {
                    self.pull_key(key, r, seq, entry.clone())?;
                }
            }
            Action::TrashLocal => {
                if let Some(e) = entry {
                    match self.local.trash_put(key) {
                        Ok(()) => {
                            let _ = self.store.put_trashed(
                                key,
                                &Trashed {
                                    sha256: e.sha256.clone(),
                                    size: e.size,
                                    ts: super::now_ms(),
                                },
                            );
                            log::info!(
                                "{key}: deleted on the hub; local copy kept in the mirror trash"
                            );
                        }
                        Err(err) => {
                            return Err(HubError::Other(format!(
                                "{key}: could not preserve the local copy in trash: {err:#}"
                            )))
                        }
                    }
                }
                let _ = self.store.remove_jobs_for(key);
                let _ = self.store.remove_entry(key);
                let _ = self.store.remove_base(key);
                self.view_remove(key);
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
                drop(_guard);
                self.record_conflict(key, &e, base.as_ref(), &r);
            }
        }
        Ok(())
    }

    /// Take the hub's version of a key: the bytes land in the folder (an
    /// atomic replace), and entry + base follow.
    fn pull_key(
        &self,
        key: &str,
        r: &RemoteStat,
        seq: u64,
        expected: Option<Entry>,
    ) -> Result<(), HubError> {
        let guard = self.directory_moves.lock();
        if self.store.entry(key) != expected
            || self.is_open_for_write(key)
            || self.pending_path_move(key)
        {
            self.rt.lock().recheck.insert(key.into());
            return Ok(());
        }
        let stamp = self.local.stat(key);
        let epoch = self.directory_epoch.load(Ordering::Relaxed);
        drop(guard);
        let landing = || {
            let guard = self.directory_moves.lock();
            if self.directory_epoch.load(Ordering::Relaxed) != epoch
                || self.local.stat(key) != stamp
                || self.store.entry(key) != expected
                || self.is_open_for_write(key)
                || self.pending_path_move(key)
            {
                return Err(HubError::LocalChanged(key.into()));
            }
            Ok(guard)
        };
        let (h, _guard) = match self
            .local
            .fetch(&self.hub, key, &r.sha256, r.mtime, &landing)
        {
            Ok(result) => result,
            Err(HubError::NotFound) => {
                // Gone between the feed and now; the next change says so.
                return Ok(());
            }
            Err(HubError::LocalChanged(_)) => {
                self.scan_paths(&[key.to_string()], JobPriority::Interactive);
                self.rt.lock().recheck.insert(key.to_string());
                return Ok(());
            }
            // Do not acknowledge the feed/listing cursor for bytes we could
            // not verify or land. The next normal cycle retries this key.
            Err(e) => return Err(e),
        };
        let new_entry = Entry {
            sha256: h.sha256.clone(),
            size: h.size,
            mtime: h.mtime,
            state: EntryState::Clean,
        };
        let _ = self.store.put_entry(key, &new_entry);
        let _ = self.store.put_base(
            key,
            &Base {
                sha256: h.sha256,
                size: h.size,
                mtime: r.mtime,
                remote_seq: seq,
            },
        );
        self.record_dirs_for(key);
        if self.store.trashed(key).is_some() {
            let _ = self.store.remove_trashed(key);
            self.local.trash_remove(key);
        }
        let _ = self.store.remove_jobs_for(key);
        self.view_upsert(key, h.size, h.mtime);
        Ok(())
    }

    /// Both sides changed: keep a copy of OUR bytes (the hub's will take the
    /// name once the conflict upload went through) and queue the upload.
    fn record_conflict(&self, key: &str, entry: &Entry, base: Option<&Base>, remote_sha: &str) {
        let snapshot = match self.local.upload_snapshot(key) {
            Ok(snapshot) => snapshot,
            Err(e) => {
                log::warn!("{key}: cannot snapshot the local version for the conflict: {e:#}");
                return;
            }
        };
        let _guard = self.directory_moves.lock();
        if self.is_open_for_write(key)
            || self.pending_path_move(key)
            || self.store.entry(key).as_ref() != Some(entry)
            || self
                .local
                .stat(key)
                .is_none_or(|s| s.fingerprint != snapshot.fingerprint)
        {
            self.rt.lock().recheck.insert(self.local_result_key(key));
            return;
        }
        if let Err(e) = self.local.preserve_conflict(&snapshot) {
            log::warn!("{key}: cannot preserve conflict snapshot: {e:#}");
            return;
        }
        let local_sha = snapshot.hashed.sha256.clone();
        if local_sha != entry.sha256 {
            let _ = self.store.put_entry(
                key,
                &Entry {
                    sha256: local_sha.clone(),
                    size: snapshot.hashed.size,
                    mtime: snapshot.hashed.mtime,
                    state: EntryState::Dirty,
                },
            );
        }
        if let Some(existing) = self.store.conflict(key) {
            if existing.local_sha256 == local_sha && !existing.uploaded {
                // Already queued.
                self.ensure_job(JobKind::Conflict {
                    key: key.to_string(),
                    local_sha256: local_sha,
                    base_sha256: base.map(|b| b.sha256.clone()),
                });
                return;
            }
        }
        log::warn!("{key}: changed here and on the hub; uploading ours as a conflict");
        let _ = self.store.put_conflict(&Conflict {
            key: key.to_string(),
            local_sha256: local_sha.clone(),
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
            local_sha256: local_sha,
            base_sha256: base.map(|b| b.sha256.clone()),
        });
    }

    fn ensure_job(&self, kind: JobKind) {
        let id = kind.dedupe_id();
        if self.store.jobs().iter().any(|j| j.kind.dedupe_id() == id) {
            return;
        }
        let _ = self
            .store
            .enqueue_with_priority(kind, JobPriority::Background);
    }

    /// Keys whose remote change we skipped because a handle was open:
    /// ask the hub what is there now and decide again.
    fn recheck_deferred(&self) -> Result<(), HubError> {
        let keys: Vec<String> = self.rt.lock().recheck.drain().collect();
        for key in keys {
            if self.pending_directory_move(&key) {
                continue;
            }
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
                EntryState::Tombstone => JobKind::Delete {
                    key: key.clone(),
                    if_match: None,
                },
            };
            if !have.contains(&wanted.dedupe_id()) {
                let _ = self
                    .store
                    .enqueue_with_priority(wanted, JobPriority::Discovered);
            }
        }
    }

    /// Prioritize current user work, then offline discoveries, then repair.
    /// Dependencies always win: a write cannot overtake its rename or mkdir.
    /// Reload after every request so a fresh edit can jump an existing backlog.
    fn drain_jobs(&self, budget: usize) -> Result<HashSet<String>, HubError> {
        let mut touched = HashSet::new();
        for _ in 0..budget {
            self.process_local_events();
            let now = super::now_ms();
            let mut jobs = self.store.jobs();
            jobs.sort_by(|a, b| {
                b.priority.cmp(&a.priority).then_with(|| {
                    if a.priority == JobPriority::Interactive {
                        b.seq.cmp(&a.seq)
                    } else {
                        a.seq.cmp(&b.seq)
                    }
                })
            });
            let overlaps = |a: &Job, b: &Job| {
                a.kind
                    .keys()
                    .iter()
                    .any(|x| b.kind.keys().iter().any(|y| super::store::overlaps(x, y)))
            };
            let mut selected = None;
            for candidate in &jobs {
                let mut job = candidate;
                // Inherit urgency through earlier operations on either path.
                while let Some(prior) = jobs
                    .iter()
                    .filter(|j| j.seq < job.seq && overlaps(j, job))
                    .min_by_key(|j| j.seq)
                {
                    job = prior;
                }
                if job.not_before > now
                    || job
                        .kind
                        .keys()
                        .iter()
                        .any(|key| self.is_open_for_write(key))
                {
                    continue;
                }
                selected = Some(job.clone());
                break;
            }
            let Some(job) = selected else {
                break;
            };
            // Creating an empty folder does not invalidate the listing of
            // existing remote children under that name.
            if !matches!(job.kind, JobKind::Mkdir { .. } | JobKind::Rmdir { .. }) {
                touched.extend(job.kind.keys().into_iter().map(str::to_string));
            }
            self.run_one(&job)?;
        }
        Ok(touched)
    }

    fn run_one(&self, job: &Job) -> Result<(), HubError> {
        // Wait for the local syscall and ledger commit before loading a move
        // intent. Otherwise a worker could persist a stale, unapplied intent.
        let _move_guard = self.directory_moves.lock();
        // A local directory move can re-key/re-sequence jobs after the drain
        // pass took its snapshot. Never execute one of those stale jobs.
        let Some(current) = self.store.jobs().into_iter().find(|j| j.seq == job.seq) else {
            return Ok(());
        };
        let job = &current;
        if !matches!(job.kind, JobKind::RenameDir { .. }) && self.store.jobs().iter().any(|pending| {
            pending.seq < job.seq && matches!(&pending.kind, JobKind::RenameDir { from, to, .. }
                if job.kind.keys().iter().any(|key| super::store::under(key, from) || super::store::under(key, to)))
        }) {
            return Ok(());
        }
        drop(_move_guard);
        let result = self.run_job(job);
        let _move_guard = self.directory_moves.lock();
        if !self
            .store
            .jobs()
            .iter()
            .any(|j| j.seq == job.seq && j.kind == job.kind)
        {
            return result;
        }
        match result {
            Ok(()) => {
                let _ = self.store.remove_job(job.seq);
                drop(_move_guard);
                if let JobKind::RenameDir { from, to, .. } = &job.kind {
                    // Notifications under pending moves were deliberately
                    // deferred. Inspect those paths before any remote listing
                    // can overwrite an unobserved edit at the destination.
                    self.scan_paths(&[from.clone(), to.clone()], JobPriority::Interactive);
                }
                self.write_status(false);
            }
            Err(e) if e.is_offline() || matches!(e, HubError::Unauthorized) => return Err(e),
            Err(HubError::LocalChanged(_)) => {
                // A user edit won the race with background preparation.
                // Keep the current job ready; this is not a server failure
                // and must not impose network retry backoff on fresh work.
            }
            Err(e) if e.is_permanent() && !matches!(job.kind, JobKind::RenameDir { .. }) => {
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
            }
        }
        Ok(())
    }

    fn run_job(&self, job: &Job) -> Result<(), HubError> {
        match &job.kind {
            JobKind::Push { key } => self.run_push(key),
            JobKind::Delete { key, if_match } => self.run_delete(key, if_match.as_deref()),
            JobKind::Rename { from, to } => self.run_rename(from, to),
            JobKind::RenameDir {
                from,
                to,
                operation_id,
                local_applied,
                remote,
                ..
            } => {
                if !local_applied {
                    return Err(HubError::Other(
                        "directory move is not applied locally yet".into(),
                    ));
                }
                if *remote {
                    match self.hub.rename_directory(from, to, operation_id) {
                        // A folder discovered offline can contain only new,
                        // unpushed files. There is no server folder to move.
                        Err(HubError::NotFound)
                            if self.store.bases(&format!("{to}/")).is_empty() =>
                        {
                            self.hub.mkdir(to)?;
                        }
                        result => result?,
                    }
                    log::info!("{from} → {to}: directory renamed on the hub");
                }
                self.directory_epoch.fetch_add(1, Ordering::Relaxed);
                self.store
                    .set_meta("directory-refresh", "1")
                    .map_err(|e| HubError::Other(e.to_string()))?;
                Ok(())
            }
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

    /// Translate a completed request through local moves queued during it.
    /// The engine has not executed those moves upstream yet.
    fn local_result_key(&self, key: &str) -> String {
        let mut key = key.to_string();
        for job in self.store.jobs() {
            match job.kind {
                JobKind::Rename { from, to } if key == from => key = to,
                JobKind::RenameDir {
                    from,
                    to,
                    local_applied: true,
                    ..
                } if super::store::under(&key, &from) => {
                    key = format!("{to}{}", &key[from.len()..])
                }
                _ => {}
            }
        }
        key
    }

    fn run_push(&self, key: &str) -> Result<(), HubError> {
        let guard = self.directory_moves.lock();
        let Some(entry) = self.store.entry(key) else {
            return Ok(());
        };
        if entry.state != EntryState::Dirty
            || self.is_open_for_write(key)
            || self.pending_directory_move(key)
        {
            return Ok(());
        }
        let base = self.store.base(key);
        let epoch = self.directory_epoch.load(Ordering::Relaxed);
        drop(guard);
        // Hash/copy on the engine thread, without blocking local operations.
        let snapshot = self
            .local
            .upload_snapshot(key)
            .map_err(|e| HubError::Other(e.to_string()))?;
        let guard = self.directory_moves.lock();
        if self.directory_epoch.load(Ordering::Relaxed) != epoch
            || self.is_open_for_write(key)
            || self.store.entry(key) != Some(entry.clone())
            || self
                .local
                .stat(key)
                .is_none_or(|s| s.fingerprint != snapshot.fingerprint)
        {
            return Err(HubError::LocalChanged(key.into()));
        }
        let entry = Entry {
            sha256: snapshot.hashed.sha256.clone(),
            size: snapshot.hashed.size,
            mtime: snapshot.hashed.mtime,
            state: EntryState::Dirty,
        };
        self.store
            .put_entry(key, &entry)
            .map_err(|e| HubError::Other(e.to_string()))?;
        let opts = PutOptions {
            if_match: base.as_ref().map(|b| b.sha256.clone()),
            if_none_match_any: base.is_none(),
            sha256: Some(entry.sha256.clone()),
            mtime: Some(entry.mtime),
            ..Default::default()
        };
        self.store
            .set_meta(&format!("upload-pending:{key}"), &entry.sha256)
            .map_err(|e| HubError::Other(e.to_string()))?;
        drop(guard);
        let result = self
            .hub
            .put_object(key, &PutBody::File(snapshot.path.clone()), &opts);
        let guard = self.directory_moves.lock();
        let landed_key = self.local_result_key(key);
        match result {
            Ok(res) => {
                let sha = if res.sha256.is_empty() {
                    entry.sha256.clone()
                } else {
                    res.sha256
                };
                let mtime = if res.mtime > 0 {
                    res.mtime
                } else {
                    entry.mtime
                };
                self.store
                    .put_base(
                        &landed_key,
                        &Base {
                            sha256: sha.clone(),
                            size: entry.size,
                            mtime,
                            remote_seq: res.seq,
                        },
                    )
                    .map_err(|e| HubError::Other(e.to_string()))?;
                if let Some(mut now) = self.store.entry(&landed_key) {
                    if now.sha256 == sha
                        && now.state == EntryState::Dirty
                        && !self.is_open_for_write(&landed_key)
                    {
                        now.state = EntryState::Clean;
                        self.store
                            .put_entry(&landed_key, &now)
                            .map_err(|e| HubError::Other(e.to_string()))?;
                    }
                }
                let _ = self.store.remove_meta(&format!("upload-pending:{key}"));
                log::info!("{key}: pushed ({} bytes)", entry.size);
                Ok(())
            }
            Err(HubError::PreconditionFailed { current }) => {
                drop(guard);
                // A queued move will refresh its destination after it commits.
                if landed_key != key {
                    return Err(HubError::PreconditionFailed { current });
                }
                self.reconcile_key(key, current.as_ref(), 0)
            }
            Err(e) => Err(e),
        }
    }

    fn run_delete(&self, key: &str, fixed_match: Option<&str>) -> Result<(), HubError> {
        let guard = self.directory_moves.lock();
        let local_key = self.local_result_key(key);
        let base = self.store.base(&local_key);
        let pending = self.store.meta(&format!("upload-pending:{key}"));
        let expected = fixed_match
            .map(str::to_string)
            .or_else(|| base.map(|b| b.sha256))
            .or(pending);
        let Some(expected) = expected else {
            if self
                .store
                .entry(&local_key)
                .is_some_and(|e| e.state == EntryState::Tombstone)
            {
                let _ = self.store.remove_entry(&local_key);
            }
            return Ok(());
        };
        drop(guard);
        let result = self.hub.delete_object(key, Some(&expected));
        let guard = self.directory_moves.lock();
        let local_key = self.local_result_key(key);
        match result {
            Ok(()) | Err(HubError::NotFound) => {
                if fixed_match.is_none() {
                    if self
                        .store
                        .entry(&local_key)
                        .is_some_and(|e| e.state == EntryState::Tombstone)
                    {
                        let _ = self.store.remove_entry(&local_key);
                    }
                    let _ = self.store.remove_base(&local_key);
                }
                let _ = self.store.remove_meta(&format!("upload-pending:{key}"));
                log::info!("{key}: deleted on the hub");
                Ok(())
            }
            Err(HubError::PreconditionFailed { current }) => {
                if fixed_match.is_some() || local_key != key {
                    return Err(HubError::PreconditionFailed { current });
                }
                drop(guard);
                self.reconcile_key(key, current.as_ref(), 0)
            }
            Err(e) => Err(e),
        }
    }

    fn run_rename(&self, from: &str, to: &str) -> Result<(), HubError> {
        let guard = self.directory_moves.lock();
        let local_to = self.local_result_key(to);
        let base = self.store.base(&local_to);
        let pending = self.store.meta(&format!("upload-pending:{from}"));
        let expected = base
            .as_ref()
            .map(|b| b.sha256.as_str())
            .or(pending.as_deref());
        drop(guard);
        let result = self.hub.rename_object(from, to, expected);
        let guard = self.directory_moves.lock();
        let local_to = self.local_result_key(to);
        match result {
            Ok(()) => {
                log::info!("{from} → {to}: renamed on the hub");
                Ok(())
            }
            Err(HubError::NotFound) => {
                // The source is gone on the hub: our bytes at `to` are a new
                // file there.
                let _ = self.store.remove_base(&local_to);
                if let Some(mut e) = self.store.entry(&local_to) {
                    if self.local.is_file(&local_to) {
                        e.state = EntryState::Dirty;
                        let _ = self.store.put_entry(&local_to, &e);
                        self.ensure_job(JobKind::Push {
                            key: local_to.clone(),
                        });
                    } else {
                        log::warn!("{to}: source vanished on the hub and the file is not in the folder; dropping");
                        let _ = self.store.remove_entry(&local_to);
                        self.view_remove(&local_to);
                    }
                }
                Ok(())
            }
            Err(HubError::TargetExists) => {
                // Someone put something at the target meanwhile: take theirs
                // at `to`; our bytes are still the hub's `from`, which the
                // next full pass lists again.
                drop(guard);
                let stat = self.hub.head_object(to)?;
                let guard = self.directory_moves.lock();
                if self.local_result_key(to) != to {
                    return Err(HubError::TargetExists);
                }
                let _ = self.store.remove_base(&local_to);
                drop(guard);
                self.reconcile_key(to, stat.as_ref(), 0)?;
                Ok(())
            }
            Err(HubError::PreconditionFailed { current }) => {
                // The hub changed the source since we last saw it: keep our
                // moved bytes as a new `to`, and let the hub's `from` return.
                let _ = self.store.remove_base(&local_to);
                if let Some(mut e) = self.store.entry(&local_to) {
                    e.state = EntryState::Dirty;
                    let _ = self.store.put_entry(&local_to, &e);
                    self.ensure_job(JobKind::Push {
                        key: local_to.clone(),
                    });
                }
                drop(guard);
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
        if !self.local.has_conflict_bytes(local_sha) {
            // The snapshot is missing (a crash between record and copy):
            // take one now if the file still has those bytes.
            match self.local.conflict_snapshot(key) {
                Ok(sha) if sha == local_sha => {}
                _ => {
                    log::error!("{key}: conflict bytes {local_sha} are gone; nothing to upload");
                    return Ok(());
                }
            }
        }
        let path = self.local.conflict_path(local_sha);
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
        let stamp = chrono::DateTime::from_timestamp_millis(record.ts as i64)
            .unwrap_or_else(chrono::Utc::now);
        let mut target = match self.opts.conflicts {
            ConflictMode::Prompt => key.to_string(),
            ConflictMode::Rename => record
                .copy_key
                .clone()
                .unwrap_or_else(|| super::conflict_copy_key(key, &self.device.name, stamp)),
        };
        let mut existing = None;
        if self.opts.conflicts == ConflictMode::Rename {
            existing = self.hub.head_object(&target)?;
            if existing
                .as_ref()
                .is_some_and(|stat| stat.sha256 != local_sha)
            {
                // Two saves in the same minute must never share a conflict
                // destination. The digest also makes crash retries stable.
                target = super::conflict_copy_key(
                    key,
                    &format!("{} {local_sha}", self.device.name),
                    stamp,
                );
                existing = self.hub.head_object(&target)?;
            }
            record.copy_key = Some(target.clone());
            let _guard = self.directory_moves.lock();
            record.key = self.local_result_key(key);
            record.copy_key = Some(self.local_result_key(&target));
            self.store
                .put_conflict(&record)
                .map_err(|e| HubError::Other(e.to_string()))?;
        }
        let res = if existing
            .as_ref()
            .is_some_and(|stat| stat.sha256 == local_sha)
        {
            None // prior upload committed but its reply was lost
        } else {
            Some(self.hub.put_object(&target, &PutBody::File(path), &opts)?)
        };
        record.uploaded = true;
        if let Some(res) = res {
            record.inbox_doc_id = res.doc_id;
        }
        if self.opts.conflicts == ConflictMode::Rename {
            record.copy_key = Some(target.clone());
        }
        {
            let _guard = self.directory_moves.lock();
            record.key = self.local_result_key(key);
            record.copy_key = record.copy_key.as_ref().map(|p| self.local_result_key(p));
            let _ = self.store.put_conflict(&record);
            if record.key != key {
                return Ok(());
            }
        }
        log::info!("{key}: conflict copy uploaded ({target})");

        // The saved snapshot covers only `local_sha`. A newer save that
        // arrived while it uploaded needs its own reconciliation; it must
        // not be replaced by the hub's bytes below.
        if self
            .local
            .hash(key)
            .is_ok_and(|current| current.sha256 != local_sha)
            || self.is_open_for_write(key)
        {
            self.scan_paths(&[key.to_string()], JobPriority::Interactive);
            self.rt.lock().recheck.insert(key.to_string());
            return Ok(());
        }

        // Now the hub version keeps the name: take it. Ask the hub rather
        // than trusting the stat that raised the conflict — it may have
        // moved again.
        let expected = self.store.entry(key);
        match self.hub.head_object(key)? {
            Some(stat) => self.pull_key(key, &stat, 0, expected),
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
                Ok(())
            }
        }
    }

    /// Trash expiry and orphaned conflict snapshots. Runs even offline.
    fn housekeeping(&self) {
        let now = super::now_ms();
        for (key, t) in self.store.trash() {
            if now.saturating_sub(t.ts) > TRASH_TTL_MS {
                let _ = self.store.remove_trashed(&key);
                self.local.trash_remove(&key);
            }
        }
        let referenced: HashSet<String> = self
            .store
            .conflicts()
            .into_iter()
            .map(|c| c.local_sha256)
            .collect();
        if let Ok(rd) = std::fs::read_dir(self.local.conflict_path("")) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".part") || referenced.contains(&name) {
                    continue;
                }
                // A record that is gone (resolved, or renamed away) leaves
                // its snapshot; keep it the trash TTL, then let it go.
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| {
                        SystemTime::now()
                            .duration_since(t)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);
                if old > TRASH_TTL_MS {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Generation 1 → 2: the old mirror kept bytes in a content cache keyed by
/// digest and an index naming them. Move every cached file to its place in
/// the folder (unless the folder already has that name — the folder wins,
/// the scan decides), and carry the records over so a clean file stays
/// clean (no re-download, no re-upload) and a dirty one stays dirty (its
/// push is recovered). Keys without cached bytes were never fetched; the
/// listing pulls them like any other remote file.
fn migrate_legacy(
    store: &Store,
    local: &Local,
    cache_dir: &std::path::Path,
    legacy: super::store::Legacy,
    ignore: &IgnoreRules,
) {
    let cache_path = |sha: &str| {
        let fan = if sha.len() >= 2 { &sha[..2] } else { "xx" };
        cache_dir.join(fan).join(sha)
    };
    let bases: HashMap<String, Base> = legacy.bases.into_iter().collect();
    let mut moved = 0u64;
    let mut dirty = 0u64;
    let mut skipped = 0u64;
    for (key, e) in legacy.entries {
        if e.state == EntryState::Tombstone {
            // A delete that never reached the hub: say so again.
            if bases.contains_key(&key) {
                let _ = store.put_entry(&key, &e);
                if let Some(b) = bases.get(&key) {
                    let _ = store.put_base(&key, b);
                }
                let _ = store.enqueue(JobKind::Delete {
                    key: key.clone(),
                    if_match: None,
                });
            }
            continue;
        }
        let src = cache_path(&e.sha256);
        if local.exists(&key) {
            // Something is already there (a half-done migration, or files
            // the user put in place by hand): the scan compares it to the
            // hub; the cached copy is not needed.
            skipped += 1;
            continue;
        }
        if !src.is_file() {
            continue;
        }
        let placed = (|| -> Result<()> {
            let parent = super::parent_key(&key);
            if !parent.is_empty() {
                local.mkdir(parent)?;
            }
            std::fs::copy(&src, local.path(&key))?;
            let f = std::fs::File::options()
                .write(true)
                .open(local.path(&key))?;
            f.set_modified(ms_to_systime(e.mtime))?;
            Ok(())
        })();
        if let Err(err) = placed {
            log::warn!("{key}: could not move the cached bytes into the folder: {err:#}");
            continue;
        }
        let Some(st) = local.stat(&key) else {
            continue;
        };
        let entry = Entry {
            sha256: e.sha256.clone(),
            size: st.size,
            mtime: st.mtime,
            state: e.state,
        };
        let _ = store.put_entry(&key, &entry);
        if let Some(b) = bases.get(&key) {
            let _ = store.put_base(&key, b);
        }
        // The hub refuses excluded keys (dotfiles, editor lock files …);
        // they stay local, as they did before.
        if e.state == EntryState::Dirty && !ignore.is_ignored(&key) {
            dirty += 1;
            let _ = store.enqueue(JobKind::Push { key: key.clone() });
        }
        moved += 1;
    }
    for (key, t) in legacy.trash {
        let src = cache_path(&t.sha256);
        if !src.is_file() {
            continue;
        }
        let dest = local.trash_path(&key);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::copy(&src, &dest).is_ok() {
            let _ = store.put_trashed(&key, &t);
        }
    }
    for c in legacy.conflicts {
        let src = cache_path(&c.local_sha256);
        if src.is_file() && std::fs::copy(&src, local.conflict_path(&c.local_sha256)).is_ok() {
            let _ = store.put_conflict(&c);
        }
    }
    log::info!(
        "mirror: migrated the content cache into {}: {moved} files placed ({dirty} with a pending push), {skipped} already there",
        local.display_path().display()
    );
}
