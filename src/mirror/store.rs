//! The mirror's persistent state: one redb (`mirror.redb`) in the mount's
//! data dir, one table per concern. Values are JSON — small, self-describing,
//! and a field added later reads back through `#[serde(default)]` without a
//! migration.
//!
//! Every table name carries a generation (the `names.rs` idiom): when a
//! table's meaning changes, bump the suffix, list the old name under
//! `RETIRED`, and `open()` drops it. A mirror then rebuilds that part of its
//! state from the hub rather than reading yesterday's shape.
//!
//! Keys are hub keys: NFC, `/`-separated, relative. Never document ids.
//!
//! Generation 2 (0.11.0): bytes moved from a content cache into the real
//! Home folder. Every v1 table is retired on open, but not before its
//! entries, bases, trash and conflict records are handed to the mirror
//! (`take_legacy`), which moves the cached bytes into the folder and
//! carries the ledger over — nothing re-downloads, nothing unpushed is lost.

use anyhow::{Context as _, Result};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// `key → Entry`: what the local Home tree contains and in what state.
const ENTRIES: TableDefinition<&str, &[u8]> = TableDefinition::new("entries_v2");
/// `key → Base`: the last version agreed with the hub (the "B" of the
/// three-way decision). Written only after the byte op succeeded.
const BASE: TableDefinition<&str, &[u8]> = TableDefinition::new("base_v2");
/// Small named numbers: the change-feed cursor, the head we last saw.
const CURSOR: TableDefinition<&str, u64> = TableDefinition::new("cursor_v2");
/// Small named strings: hub instance id, backend, exclusions snapshot.
const META: TableDefinition<&str, &str> = TableDefinition::new("meta_v2");
/// `seq → Job`: the durable write-back queue, in submission order.
const JOBS: TableDefinition<u64, &[u8]> = TableDefinition::new("jobs_v2");
/// `key → Conflict`: conflicts recorded on this device.
const CONFLICTS: TableDefinition<&str, &[u8]> = TableDefinition::new("conflicts_v2");
/// `key → Trashed`: keys the hub deleted whose file sits in the local
/// trash folder (30 days).
const TRASH: TableDefinition<&str, &[u8]> = TableDefinition::new("trash_v2");
/// `dir key → ""`: directories known to exist on disk, including empty
/// ones — what the scan found, what `mkdir` made, what a pull passed
/// through.
const DIRS: TableDefinition<&str, &str> = TableDefinition::new("dirs_v2");

pub fn under(key: &str, dir: &str) -> bool {
    key == dir
        || key
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub fn overlaps(a: &str, b: &str) -> bool {
    under(a, b) || under(b, a)
}

const RETIRED: [&str; 10] = [
    "entries_v1",
    "base_v1",
    "cursor_v1",
    "meta_v1",
    "jobs_v1",
    "pins_v1",
    "cache_v1",
    "conflicts_v1",
    "trash_v1",
    "dirs_v1",
];

pub const CURSOR_KEY: &str = "cursor";
pub const HEAD_KEY: &str = "head";
const JOB_SEQ_KEY: &str = "job_seq";
pub const META_INSTANCE: &str = "instance_id";
pub const META_LISTED: &str = "listed";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryState {
    /// Local bytes == base (as far as the mirror knows).
    Clean,
    /// Local bytes differ from base; a push is owed.
    Dirty,
    /// Deleted locally; a remote delete is owed (or was declined by
    /// `--deletes keep`, in which case the entry is simply gone).
    Tombstone,
}

/// One key of the local Home tree. `sha256` is what the LOCAL bytes are —
/// the "L" of the decision; the base ledger carries "B".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub sha256: String,
    pub size: u64,
    /// ms since the epoch
    pub mtime: u64,
    pub state: EntryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub sha256: String,
    pub size: u64,
    pub mtime: u64,
    #[serde(default)]
    pub remote_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobKind {
    /// `PUT objects/<key>` with the current local bytes.
    Push {
        key: String,
    },
    /// `DELETE objects/<key>` with `If-Match` = base.
    Delete {
        key: String,
        /// Fixed precondition for a destination removed before an overwrite
        /// rename. Its current ledger already describes the incoming file.
        #[serde(default)]
        if_match: Option<String>,
    },
    /// `POST objects/rename`.
    Rename {
        from: String,
        to: String,
    },
    /// Durable directory move intent, completed locally before it goes online.
    RenameDir {
        from: String,
        to: String,
        operation_id: String,
        dev: u64,
        ino: u64,
        local_applied: bool,
        remote: bool,
    },
    /// Upload our version of a key the hub changed too.
    Conflict {
        key: String,
        local_sha256: String,
        base_sha256: Option<String>,
    },
    /// Real directory on the hub (the objects protocol has no directories;
    /// this rides the older `/home/mkdir` route).
    Mkdir {
        key: String,
    },
    Rmdir {
        key: String,
    },
}

impl JobKind {
    /// The key a job holds while it runs — one job in flight per key.
    pub fn key(&self) -> &str {
        match self {
            JobKind::Push { key }
            | JobKind::Delete { key, .. }
            | JobKind::Conflict { key, .. }
            | JobKind::Mkdir { key }
            | JobKind::Rmdir { key } => key,
            JobKind::Rename { from, .. } | JobKind::RenameDir { from, .. } => from,
        }
    }

    /// Every key the job touches (a rename holds two).
    pub fn keys(&self) -> Vec<&str> {
        match self {
            JobKind::Rename { from, to } | JobKind::RenameDir { from, to, .. } => vec![from, to],
            other => vec![other.key()],
        }
    }

    /// Dedupe identity: one pending job per (kind, key).
    pub fn dedupe_id(&self) -> String {
        match self {
            JobKind::Push { key } => format!("push|{key}"),
            JobKind::Delete { key, .. } => format!("delete|{key}"),
            JobKind::Rename { from, to } => format!("rename|{from}|{to}"),
            JobKind::RenameDir { operation_id, .. } => format!("rename_dir|{operation_id}"),
            JobKind::Conflict { key, .. } => format!("conflict|{key}"),
            JobKind::Mkdir { key } => format!("mkdir|{key}"),
            JobKind::Rmdir { key } => format!("rmdir|{key}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPriority {
    #[default]
    Background,
    Discovered,
    Interactive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    #[serde(default)]
    pub seq: u64,
    #[serde(flatten)]
    pub kind: JobKind,
    #[serde(default)]
    pub priority: JobPriority,
    #[serde(default)]
    pub attempts: u32,
    /// Do not run before this (ms since epoch); backoff after a failure.
    #[serde(default)]
    pub not_before: u64,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub created: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub key: String,
    pub local_sha256: String,
    pub hub_sha256: Option<String>,
    pub base_sha256: Option<String>,
    pub ts: u64,
    pub mode: super::ConflictMode,
    /// Set once the upload landed (inbox doc id / conflict-copy key).
    #[serde(default)]
    pub uploaded: bool,
    #[serde(default)]
    pub inbox_doc_id: Option<u64>,
    #[serde(default)]
    pub copy_key: Option<String>,
    #[serde(default)]
    pub resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trashed {
    pub sha256: String,
    pub size: u64,
    pub ts: u64,
}

/// What a generation-1 store held, read once on open before the tables go.
#[derive(Debug, Default)]
pub struct Legacy {
    pub entries: Vec<(String, Entry)>,
    pub bases: Vec<(String, Base)>,
    pub trash: Vec<(String, Trashed)>,
    pub conflicts: Vec<Conflict>,
}

impl Legacy {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.bases.is_empty()
            && self.trash.is_empty()
            && self.conflicts.is_empty()
    }
}

pub struct Store {
    db: Database,
    legacy: parking_lot::Mutex<Option<Legacy>>,
}

fn read_legacy_table<T: DeserializeOwned>(
    tx: &redb::WriteTransaction,
    name: &str,
) -> Vec<(String, T)> {
    let mut out = Vec::new();
    let Ok(t) = tx.open_table(TableDefinition::<&str, &[u8]>::new(name)) else {
        return out;
    };
    if let Ok(iter) = t.iter() {
        for item in iter.flatten() {
            if let Some(v) = dec::<T>(item.1.value()) {
                out.push((item.0.value().to_string(), v));
            }
        }
    }
    out
}

fn enc<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(v)?)
}

fn dec<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    serde_json::from_slice(bytes).ok()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path).with_context(|| format!("opening {}", path.display()))?;
        let tx = db.begin_write()?;
        // Generation 1 → 2: keep what the old tables said before they go.
        // `open_table` would CREATE a missing table, so only ask for the
        // ones the catalogue lists.
        let existing: HashSet<String> = tx
            .list_tables()
            .map(|t| t.map(|d| d.name().to_string()).collect())
            .unwrap_or_default();
        let legacy = if existing.contains("entries_v1") {
            let l = Legacy {
                entries: read_legacy_table::<Entry>(&tx, "entries_v1"),
                bases: read_legacy_table::<Base>(&tx, "base_v1"),
                trash: read_legacy_table::<Trashed>(&tx, "trash_v1"),
                conflicts: read_legacy_table::<Conflict>(&tx, "conflicts_v1")
                    .into_iter()
                    .map(|(_, c)| c)
                    .collect(),
            };
            (!l.is_empty()).then_some(l)
        } else {
            None
        };
        tx.open_table(ENTRIES)?;
        tx.open_table(BASE)?;
        tx.open_table(CURSOR)?;
        tx.open_table(META)?;
        tx.open_table(JOBS)?;
        tx.open_table(CONFLICTS)?;
        tx.open_table(TRASH)?;
        tx.open_table(DIRS)?;
        for name in RETIRED {
            // The value type does not matter for a drop; redb keys the
            // catalogue by name.
            let _ = tx.delete_table(TableDefinition::<&str, &[u8]>::new(name));
            let _ = tx.delete_table(TableDefinition::<u64, &[u8]>::new(name));
            let _ = tx.delete_table(TableDefinition::<&str, u64>::new(name));
            let _ = tx.delete_table(TableDefinition::<&str, &str>::new(name));
        }
        tx.commit()?;
        Ok(Self {
            db,
            legacy: parking_lot::Mutex::new(legacy),
        })
    }

    /// The generation-1 records found on this open, once.
    pub fn take_legacy(&self) -> Option<Legacy> {
        self.legacy.lock().take()
    }

    // ── generic helpers ──────────────────────────────────────────────────────

    fn get_json<T: DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &[u8]>,
        key: &str,
    ) -> Option<T> {
        let tx = self.db.begin_read().ok()?;
        let t = tx.open_table(table).ok()?;
        let v = t.get(key).ok()??;
        dec(v.value())
    }

    fn put_json<T: Serialize>(
        &self,
        table: TableDefinition<&str, &[u8]>,
        key: &str,
        value: &T,
    ) -> Result<()> {
        let bytes = enc(value)?;
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(table)?;
            t.insert(key, bytes.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    fn remove_key(&self, table: TableDefinition<&str, &[u8]>, key: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(table)?;
            t.remove(key)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn list_json<T: DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &[u8]>,
        prefix: &str,
    ) -> Vec<(String, T)> {
        let mut out = Vec::new();
        let Ok(tx) = self.db.begin_read() else {
            return out;
        };
        let Ok(t) = tx.open_table(table) else {
            return out;
        };
        let Ok(iter) = t.range(prefix..) else {
            return out;
        };
        for item in iter.flatten() {
            let k = item.0.value();
            if !k.starts_with(prefix) {
                break;
            }
            if let Some(v) = dec::<T>(item.1.value()) {
                out.push((k.to_string(), v));
            }
        }
        out
    }

    // ── entries ──────────────────────────────────────────────────────────────

    pub fn entry(&self, key: &str) -> Option<Entry> {
        self.get_json(ENTRIES, key)
    }

    pub fn put_entry(&self, key: &str, entry: &Entry) -> Result<()> {
        self.put_json(ENTRIES, key, entry)
    }

    pub fn remove_entry(&self, key: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(ENTRIES)?.remove(key)?;
        tx.open_table(META)?
            .remove(format!("local-stamp:{key}").as_str())?;
        tx.commit()?;
        Ok(())
    }

    /// Entries whose key starts with `prefix` (`""` = all), in key order.
    pub fn entries(&self, prefix: &str) -> Vec<(String, Entry)> {
        self.list_json(ENTRIES, prefix)
    }

    /// Entries strictly inside directory `dir` (any depth). `dir` = `""` for
    /// the root.
    pub fn entries_under(&self, dir: &str) -> Vec<(String, Entry)> {
        if dir.is_empty() {
            return self.entries("");
        }
        self.entries(&format!("{dir}/"))
    }

    pub fn entry_count(&self) -> u64 {
        self.db
            .begin_read()
            .ok()
            .and_then(|tx| tx.open_table(ENTRIES).ok())
            .and_then(|t| t.len().ok())
            .unwrap_or(0)
    }

    /// Move an entry (and its base) to a new key in one transaction. Used by
    /// renames on both sides: the bytes did not change, only their name.
    pub fn rekey(&self, from: &str, to: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut stamps = tx.open_table(META)?;
            stamps.remove(format!("local-stamp:{from}").as_str())?;
            stamps.remove(format!("local-stamp:{to}").as_str())?;
        }
        {
            let mut e = tx.open_table(ENTRIES)?;
            let v = e.get(from)?.map(|v| v.value().to_vec());
            if let Some(v) = v {
                e.remove(from)?;
                e.insert(to, v.as_slice())?;
            }
            let mut b = tx.open_table(BASE)?;
            let v = b.get(from)?.map(|v| v.value().to_vec());
            if let Some(v) = v {
                b.remove(from)?;
                b.insert(to, v.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── base ledger ──────────────────────────────────────────────────────────

    /// Re-key a directory's ledger and queued writes in one commit. The move
    /// intent already exists, so a crash after rename(2) can finish this step.
    pub fn apply_directory_rename(&self, job: &Job) -> Result<()> {
        let JobKind::RenameDir {
            from,
            to,
            local_applied: false,
            remote,
            ..
        } = &job.kind
        else {
            return Ok(());
        };
        let mapped = |key: &str| format!("{to}{}", &key[from.len()..]);
        let tx = self.db.begin_write()?;
        {
            let mut stamps = tx.open_table(META)?;
            let stale: Vec<String> = stamps
                .iter()?
                .flatten()
                .map(|(key, _)| key.value().to_string())
                .filter(|key| {
                    key.strip_prefix("local-stamp:")
                        .is_some_and(|key| under(key, from) || under(key, to))
                })
                .collect();
            for key in stale {
                stamps.remove(key.as_str())?;
            }
        }
        for definition in [ENTRIES, BASE, CONFLICTS] {
            let mut table = tx.open_table(definition)?;
            let rows: Vec<_> = table
                .iter()?
                .flatten()
                .filter(|(k, _)| under(k.value(), from))
                .map(|(k, v)| (k.value().to_string(), v.value().to_vec()))
                .collect();
            for (key, mut bytes) in rows {
                let dest = mapped(&key);
                if definition.name() == CONFLICTS.name() {
                    let mut c: Conflict = serde_json::from_slice(&bytes)?;
                    c.key = dest.clone();
                    c.copy_key = c
                        .copy_key
                        .map(|key| if under(&key, from) { mapped(&key) } else { key });
                    bytes = enc(&c)?;
                }
                table.remove(key.as_str())?;
                table.insert(dest.as_str(), bytes.as_slice())?;
            }
        }
        {
            let mut table = tx.open_table(DIRS)?;
            let rows: Vec<_> = table
                .iter()?
                .flatten()
                .map(|(k, _)| k.value().to_string())
                .filter(|k| under(k, from))
                .collect();
            for key in rows {
                table.remove(key.as_str())?;
                table.insert(mapped(&key).as_str(), "")?;
            }
            table.insert(to.as_str(), "")?;
        }
        {
            let mut table = tx.open_table(JOBS)?;
            let mut counter = tx.open_table(CURSOR)?;
            let mut next = counter.get(JOB_SEQ_KEY)?.map(|v| v.value()).unwrap_or(0);
            let rows: Vec<Job> = table
                .iter()?
                .flatten()
                .filter_map(|(_, v)| dec(v.value()))
                .collect();
            for mut pending in rows {
                if pending.seq == job.seq || !under(pending.kind.key(), from) {
                    continue;
                }
                // Earlier structural operations still address the server's
                // old namespace. Keep their order; only content work follows
                // the local path past this directory move.
                if *remote
                    && pending.seq < job.seq
                    && matches!(
                        pending.kind,
                        JobKind::Delete { .. } | JobKind::Mkdir { .. } | JobKind::Rmdir { .. }
                    )
                {
                    continue;
                }
                match &mut pending.kind {
                    JobKind::Push { key }
                    | JobKind::Delete { key, .. }
                    | JobKind::Conflict { key, .. }
                    | JobKind::Mkdir { key }
                    | JobKind::Rmdir { key } => *key = mapped(key),
                    _ => continue,
                }
                table.remove(pending.seq)?;
                next += 1;
                pending.seq = next;
                table.insert(next, enc(&pending)?.as_slice())?;
            }
            counter.insert(JOB_SEQ_KEY, next)?;
            let mut applied = job.clone();
            if let JobKind::RenameDir { local_applied, .. } = &mut applied.kind {
                *local_applied = true;
            }
            table.insert(job.seq, enc(&applied)?.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn base(&self, key: &str) -> Option<Base> {
        self.get_json(BASE, key)
    }

    pub fn put_base(&self, key: &str, base: &Base) -> Result<()> {
        self.put_json(BASE, key, base)
    }

    pub fn remove_base(&self, key: &str) -> Result<()> {
        self.remove_key(BASE, key)
    }

    pub fn bases(&self, prefix: &str) -> Vec<(String, Base)> {
        self.list_json(BASE, prefix)
    }

    // ── cursor / meta ────────────────────────────────────────────────────────

    pub fn cursor(&self) -> Option<u64> {
        self.number(CURSOR_KEY)
    }

    pub fn set_cursor(&self, seq: u64) -> Result<()> {
        self.set_number(CURSOR_KEY, seq)
    }

    pub fn clear_cursor(&self) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(CURSOR)?;
            t.remove(CURSOR_KEY)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn number(&self, name: &str) -> Option<u64> {
        let tx = self.db.begin_read().ok()?;
        let t = tx.open_table(CURSOR).ok()?;
        t.get(name).ok()?.map(|v| v.value())
    }

    pub fn set_number(&self, name: &str, value: u64) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(CURSOR)?;
            t.insert(name, value)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn meta(&self, name: &str) -> Option<String> {
        let tx = self.db.begin_read().ok()?;
        let t = tx.open_table(META).ok()?;
        t.get(name).ok()?.map(|v| v.value().to_string())
    }

    pub fn set_meta(&self, name: &str, value: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(META)?;
            t.insert(name, value)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_meta(&self, name: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(META)?.remove(name)?;
        tx.commit()?;
        Ok(())
    }

    // ── jobs ─────────────────────────────────────────────────────────────────

    /// Queue a job. One pending job per (kind, key): a second push of the
    /// same key replaces the first (the push reads the entry's CURRENT bytes
    /// when it runs, so nothing is lost), and its backoff is reset — the user
    /// just did something new.
    pub fn enqueue(&self, kind: JobKind) -> Result<u64> {
        self.enqueue_with_priority(kind, JobPriority::Interactive)
    }

    pub fn enqueue_with_priority(&self, kind: JobKind, priority: JobPriority) -> Result<u64> {
        let id = kind.dedupe_id();
        let tx = self.db.begin_write()?;
        let seq;
        {
            let mut t = tx.open_table(JOBS)?;
            let mut existing: Vec<u64> = Vec::new();
            for item in t.iter()?.flatten() {
                if let Some(j) = dec::<Job>(item.1.value()) {
                    if j.kind.dedupe_id() == id {
                        existing.push(item.0.value());
                    }
                }
            }
            for s in existing {
                t.remove(s)?;
            }
            // Monotonic, never reused: a job queued while another is being
            // removed must not inherit its number (and be removed with it).
            let mut counter = tx.open_table(CURSOR)?;
            let next = counter.get(JOB_SEQ_KEY)?.map(|v| v.value()).unwrap_or(0) + 1;
            counter.insert(JOB_SEQ_KEY, next)?;
            drop(counter);
            seq = next;
            let job = Job {
                seq,
                kind,
                priority,
                attempts: 0,
                not_before: 0,
                last_error: None,
                created: super::now_ms(),
            };
            let bytes = enc(&job)?;
            t.insert(seq, bytes.as_slice())?;
        }
        tx.commit()?;
        Ok(seq)
    }

    pub fn jobs(&self) -> Vec<Job> {
        let mut out = Vec::new();
        let Ok(tx) = self.db.begin_read() else {
            return out;
        };
        let Ok(t) = tx.open_table(JOBS) else {
            return out;
        };
        let Ok(iter) = t.iter() else {
            return out;
        };
        for item in iter.flatten() {
            if let Some(mut j) = dec::<Job>(item.1.value()) {
                j.seq = item.0.value();
                out.push(j);
            }
        }
        out
    }

    pub fn update_job(&self, job: &Job) -> Result<()> {
        let bytes = enc(job)?;
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(JOBS)?;
            // A fresh edit may have replaced this in-flight job. Its old
            // failure must not resurrect an earlier dependency/backoff.
            if t.get(job.seq)?.is_none() {
                return Ok(());
            }
            t.insert(job.seq, bytes.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_job(&self, seq: u64) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(JOBS)?;
            t.remove(seq)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop every pending job touching `key` (a local delete cancels the
    /// push that preceded it; a re-key retargets instead).
    pub fn remove_jobs_for(&self, key: &str) -> Result<Vec<Job>> {
        let mut dropped = Vec::new();
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(JOBS)?;
            let mut victims = Vec::new();
            for item in t.iter()?.flatten() {
                if let Some(mut j) = dec::<Job>(item.1.value()) {
                    if j.kind.key() == key {
                        j.seq = item.0.value();
                        victims.push(j);
                    }
                }
            }
            for j in victims {
                t.remove(j.seq)?;
                dropped.push(j);
            }
        }
        tx.commit()?;
        Ok(dropped)
    }

    // ── conflicts ────────────────────────────────────────────────────────────

    pub fn conflict(&self, key: &str) -> Option<Conflict> {
        self.get_json(CONFLICTS, key)
    }

    pub fn put_conflict(&self, c: &Conflict) -> Result<()> {
        self.put_json(CONFLICTS, &c.key, c)
    }

    pub fn remove_conflict(&self, key: &str) -> Result<()> {
        self.remove_key(CONFLICTS, key)
    }

    pub fn conflicts(&self) -> Vec<Conflict> {
        self.list_json::<Conflict>(CONFLICTS, "")
            .into_iter()
            .map(|(_, c)| c)
            .collect()
    }

    // ── trash ────────────────────────────────────────────────────────────────

    pub fn trashed(&self, key: &str) -> Option<Trashed> {
        self.get_json(TRASH, key)
    }

    pub fn put_trashed(&self, key: &str, t: &Trashed) -> Result<()> {
        self.put_json(TRASH, key, t)
    }

    pub fn remove_trashed(&self, key: &str) -> Result<()> {
        self.remove_key(TRASH, key)
    }

    pub fn trash(&self) -> Vec<(String, Trashed)> {
        self.list_json(TRASH, "")
    }

    // ── explicit dirs ────────────────────────────────────────────────────────

    pub fn dirs(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(tx) = self.db.begin_read() else {
            return out;
        };
        let Ok(t) = tx.open_table(DIRS) else {
            return out;
        };
        if let Ok(iter) = t.iter() {
            for item in iter.flatten() {
                out.push(item.0.value().to_string());
            }
        }
        out
    }

    pub fn has_dir(&self, key: &str) -> bool {
        let Ok(tx) = self.db.begin_read() else {
            return false;
        };
        let Ok(t) = tx.open_table(DIRS) else {
            return false;
        };
        matches!(t.get(key), Ok(Some(_)))
    }

    pub fn add_dir(&self, key: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(DIRS)?;
            t.insert(key, "")?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_dir(&self, key: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(DIRS)?;
            t.remove(key)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Re-key every explicit dir at or under `from` to `to`.
    pub fn rekey_dirs(&self, from: &str, to: &str) -> Result<()> {
        let dirs = self.dirs();
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(DIRS)?;
            for d in dirs {
                let new = if d == from {
                    to.to_string()
                } else if let Some(rest) = d.strip_prefix(&format!("{from}/")) {
                    format!("{to}/{rest}")
                } else {
                    continue;
                };
                t.remove(d.as_str())?;
                t.insert(new.as_str(), "")?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Is anything (entry or explicit dir) inside directory `dir`?
    pub fn dir_has_children(&self, dir: &str) -> bool {
        let prefix = format!("{dir}/");
        if !self.entries(&prefix).is_empty() {
            return true;
        }
        self.dirs().iter().any(|d| d.starts_with(&prefix))
    }

    /// Wipe entries, bases and explicit dirs — the "rebuild from listing"
    /// path keeps dirty entries and jobs, this is the nuclear option used only
    /// by tests and `--resync` style tooling.
    pub fn clear_bases(&self) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut t = tx.open_table(BASE)?;
            let keys: Vec<String> = t
                .iter()?
                .flatten()
                .map(|(k, _)| k.value().to_string())
                .collect();
            for k in keys {
                t.remove(k.as_str())?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("mirror.redb")).unwrap();
        (dir, store)
    }

    #[test]
    fn entries_and_bases_round_trip_and_list_by_prefix() {
        let (_d, s) = tmp();
        let e = Entry {
            sha256: "aa".into(),
            size: 3,
            mtime: 10,
            state: EntryState::Clean,
        };
        s.put_entry("Docs/a.txt", &e).unwrap();
        s.put_entry("Docs/b.txt", &e).unwrap();
        s.put_entry("Other/c.txt", &e).unwrap();
        assert_eq!(s.entry("Docs/a.txt"), Some(e.clone()));
        assert_eq!(s.entries_under("Docs").len(), 2);
        assert_eq!(s.entries("").len(), 3);
        assert!(s.dir_has_children("Docs"));
        assert!(!s.dir_has_children("Doc"));
        s.remove_entry("Docs/a.txt").unwrap();
        assert!(s.entry("Docs/a.txt").is_none());

        s.put_base(
            "Docs/b.txt",
            &Base {
                sha256: "aa".into(),
                size: 3,
                mtime: 10,
                remote_seq: 7,
            },
        )
        .unwrap();
        s.rekey("Docs/b.txt", "Docs/z.txt").unwrap();
        assert!(s.entry("Docs/b.txt").is_none());
        assert!(s.base("Docs/b.txt").is_none());
        assert_eq!(s.base("Docs/z.txt").unwrap().remote_seq, 7);
        assert!(s.entry("Docs/z.txt").is_some());
    }

    #[test]
    fn cursor_and_jobs_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mirror.redb");
        {
            let s = Store::open(&path).unwrap();
            assert_eq!(s.cursor(), None);
            s.set_cursor(42).unwrap();
            s.enqueue(JobKind::Push { key: "a".into() }).unwrap();
            s.enqueue(JobKind::Delete {
                key: "b".into(),
                if_match: None,
            })
            .unwrap();
            // Same (kind, key) replaces rather than duplicates.
            s.enqueue(JobKind::Push { key: "a".into() }).unwrap();
            s.add_dir("Empty").unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.cursor(), Some(42));
        let jobs = s.jobs();
        assert_eq!(jobs.len(), 2);
        // The replaced push moved to the back of the queue.
        assert!(matches!(jobs[0].kind, JobKind::Delete { .. }));
        assert!(matches!(jobs[1].kind, JobKind::Push { .. }));
        assert_eq!(jobs[1].priority, JobPriority::Interactive);
        assert!(s.has_dir("Empty"));
        let dropped = s.remove_jobs_for("a").unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(s.jobs().len(), 1);
    }

    #[test]
    fn dirs_rekey_with_their_subtree() {
        let (_d, s) = tmp();
        s.add_dir("A").unwrap();
        s.add_dir("A/B").unwrap();
        s.add_dir("AB").unwrap();
        s.rekey_dirs("A", "Z").unwrap();
        let mut dirs = s.dirs();
        dirs.sort();
        assert_eq!(dirs, vec!["AB", "Z", "Z/B"]);
    }
}
