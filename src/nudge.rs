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

/// What the nudge thread does to a path.
enum Poke {
    /// Create+unlink the marker inside this directory: a signal that SOMETHING
    /// here changed, for watchers that re-list a directory on any event.
    Dir(PathBuf),
    /// Unlink this file for real: the kernel then emits an `IN_DELETE` that
    /// names it. The daemon dropping an entry emits nothing, and
    /// `notify_delete` emits no fsnotify event either, so a per-file watcher
    /// keeps every departed document as a ghost — a context switch from 231
    /// documents to 16 would leave 215 of them. The file is already gone from
    /// the view; the tree holds it as a tombstone purely so this call can name
    /// it (see state::Invalidation::vanished).
    Unlink(PathBuf),
    /// Touch this file's timestamps: an event that NAMES a real, still-present
    /// file, for watchers that handle events per file instead of re-listing.
    ///
    /// This is the one that reaches Obsidian. It ignores the marker — an
    /// unknown path that no longer exists by the time it stats, which is
    /// exactly what a create-then-unlink leaves behind — so a view could gain
    /// twenty documents and nothing in the vault would ever mention them. A
    /// file that is still there when the watcher looks gets added instead.
    File(PathBuf),
}

/// Handle to the nudge thread. Dropping the last clone closes the channel and
/// the thread exits on its own; `stop` covers teardown while syscalls are
/// queued.
#[derive(Clone)]
pub struct Nudger {
    tx: Sender<Poke>,
}

impl Nudger {
    /// Spawn the nudge thread for a mount rooted at `mount_root`.
    pub fn spawn(mount_root: PathBuf, stop: Arc<AtomicBool>) -> std::io::Result<Self> {
        let (tx, rx) = channel::<Poke>();
        std::thread::Builder::new()
            .name("canvas-fuse-nudge".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // Debounce a refresh burst into one poke per path.
                    let mut dirs = BTreeSet::new();
                    let mut files = BTreeSet::new();
                    let mut gone = BTreeSet::new();
                    let mut sort = |poke| match poke {
                        Poke::Dir(p) => {
                            dirs.insert(p);
                        }
                        Poke::File(p) => {
                            files.insert(p);
                        }
                        Poke::Unlink(p) => {
                            gone.insert(p);
                        }
                    };
                    sort(first);
                    while let Ok(p) = rx.try_recv() {
                        sort(p);
                    }

                    // Files first: a watcher that acts per file should learn
                    // about the documents themselves before the directory
                    // signal arrives, so a re-list finds nothing new to do.
                    for rel in files {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let path = mount_root.join(rel);
                        touch(&path);
                        log::trace!("touched {}", path.display());
                    }

                    for rel in gone {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let path = mount_root.join(rel);
                        // Failure leaves the tombstone in place; the next
                        // refresh sees the document still absent and pokes again.
                        let _ = std::fs::remove_file(&path);
                        log::trace!("collected {}", path.display());
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
        let _ = self.tx.send(Poke::Dir(rel_dir));
    }

    /// Queue a touch for a file that just appeared or changed, RELATIVE to the
    /// mount root. Never blocks.
    pub fn touch_file(&self, rel_file: PathBuf) {
        let _ = self.tx.send(Poke::File(rel_file));
    }

    /// Queue the real unlink of a tombstoned file, RELATIVE to the mount root.
    /// Never blocks.
    pub fn unlink_file(&self, rel_file: PathBuf) {
        let _ = self.tx.send(Poke::Unlink(rel_file));
    }
}

/// Set a file's times to now, so the kernel emits `IN_ATTRIB` naming it.
///
/// `utimensat` rather than a write: setattr acknowledges times without touching
/// the document, so this signals a watcher without the write path ever running.
/// A failure is fine — the file may already be gone again.
fn touch(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // UTIME_NOW in both slots: "set atime and mtime to the current time".
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
    ];
    unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
}
