//! inotify "nudge" for directory watchers.
//!
//! FUSE reverse invalidation keeps kernel caches coherent but generates no
//! fsnotify events (the hook was lost in the ~5.3 refactor), so directory
//! watchers — Obsidian, file managers, chokidar — never learn that the daemon
//! changed a view. The workaround: after applying remote-driven changes,
//! create and unlink a reserved marker file (`.canvas-tmp` by default) in each affected
//! directory with real syscalls through the mount. The kernel then emits
//! `IN_CREATE`/`IN_DELETE` to the directory's watchers, and rescanning
//! watchers pick up the whole delta.
//!
//! The marker itself is virtual: fsimpl short-circuits every op on the name
//! before touching the tree or the write path, so it never appears in
//! listings, never takes a lock, and never becomes a server document. Anything
//! else that creates a `.canvas-tmp` (a user script, another tool) gets the
//! same harmless no-op behaviour — the name is reserved.
//!
//! Runs on its own thread: nudge syscalls are served by the single-threaded
//! FUSE session loop, and the worker may hold the refresh lock that a
//! concurrent editor flush occupying the session thread is waiting on —
//! nudging inline from the worker could deadlock the mount. (The marker ops
//! themselves are lock-free, but path resolution of the intermediate
//! directories is not.)

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, OnceLock};

/// Default marker filename. Virtual in every directory of the mount; also a
/// sensible name for clients/servers to auto-exclude from indexing.
///
/// The name is configurable because it is the part watchers judge. A leading
/// dot keeps the marker out of the way, but a watcher that filters hidden names
/// — Obsidian excludes dotfiles from a vault outright — discards the only event
/// it was going to get, and never learns the view changed. `--nudge-name` makes
/// that a setting rather than a rebuild.
pub const DEFAULT_NUDGE_FILE: &str = ".canvas-tmp";

static NUDGE_NAME: OnceLock<String> = OnceLock::new();

/// The marker name this process uses. One mount per process, so a global is
/// the whole story; unset means the default.
pub fn nudge_file() -> &'static str {
    NUDGE_NAME
        .get()
        .map(String::as_str)
        .unwrap_or(DEFAULT_NUDGE_FILE)
}

/// Set the marker name. First call wins; later ones are ignored.
pub fn set_nudge_file(name: &str) {
    let trimmed = name.trim();
    if !trimmed.is_empty() {
        let _ = NUDGE_NAME.set(trimmed.to_string());
    }
}

/// Fixed ino for the virtual marker, from the reserved 6..16 gap below
/// `FIRST_DYNAMIC_INO` (overlay inos start at 1<<48 — no collision either way).
pub const NUDGE_INO: u64 = 6;

/// Handle to the nudge thread. Dropping the last clone closes the channel and
/// the thread exits on its own; `stop` covers teardown while syscalls are
/// queued.
#[derive(Clone)]
pub struct Nudger {
    tx: Sender<PathBuf>,
}

impl Nudger {
    /// Spawn the nudge thread for a mount rooted at `mount_root`.
    pub fn spawn(mount_root: PathBuf, stop: Arc<AtomicBool>) -> std::io::Result<Self> {
        let (tx, rx) = channel::<PathBuf>();
        std::thread::Builder::new()
            .name("canvas-fuse-nudge".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // Debounce a refresh burst into one nudge per directory.
                    let mut dirs = BTreeSet::new();
                    dirs.insert(first);
                    while let Ok(d) = rx.try_recv() {
                        dirs.insert(d);
                    }
                    for rel in dirs {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let marker = mount_root.join(rel).join(nudge_file());
                        // Failures are fine: the dir may have vanished, or the
                        // mount may be going away. The events matter, not the file.
                        let created = std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true) // moot for a 0-byte virtual file
                            .open(&marker)
                            .is_ok();
                        if created {
                            let _ = std::fs::remove_file(&marker);
                        }
                        log::trace!("nudged {}", marker.display());
                    }
                }
                log::debug!("nudge channel closed, exiting");
            })?;
        Ok(Self { tx })
    }

    /// Queue a nudge for a directory, given as a path RELATIVE to the mount
    /// root (`Tree::path_of`). Never blocks.
    pub fn nudge(&self, rel_dir: PathBuf) {
        let _ = self.tx.send(rel_dir);
    }
}
