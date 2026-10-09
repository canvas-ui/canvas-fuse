//! Recursive inotify watches on the held backing directory, never the FUSE
//! mount covering it. Only directory discovery walks; file events name the
//! exact paths to inspect. Overflow requires a full recovery scan.

use super::{local::Local, IgnoreRules};
use anyhow::{Context, Result};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Default)]
pub struct Changes {
    pub paths: HashSet<String>,
    pub rescan: bool,
}

pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Watches {
    fd: OwnedFd,
    dirs: HashMap<i32, String>,
    own_moves: HashSet<u32>,
    local: Arc<Local>,
    ignore: Arc<RwLock<IgnoreRules>>,
}

impl Watches {
    fn add_tree(&mut self, root: &str) -> Result<()> {
        let mut pending = vec![root.to_string()];
        while let Some(key) = pending.pop() {
            if !key.is_empty() && self.ignore.read().is_ignored(&key) {
                continue;
            }
            let path = self.local.path(&key);
            if !key.is_empty() && !self.local.is_dir(&key) {
                continue;
            }
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
            let mask = libc::IN_CREATE
                | libc::IN_CLOSE_WRITE
                | libc::IN_MOVED_TO
                | libc::IN_MOVED_FROM
                | libc::IN_DELETE
                | libc::IN_ATTRIB
                | libc::IN_DELETE_SELF
                | libc::IN_MOVE_SELF
                | libc::IN_ONLYDIR;
            let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), mask) };
            if wd < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    continue;
                }
                return Err(error).with_context(|| format!("watching mirror directory {key}"));
            }
            self.dirs.insert(wd, key.clone());
            // Install the parent first, closing the create-before-watch gap.
            let entries = match std::fs::read_dir(path) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    pending.push(if key.is_empty() {
                        name
                    } else {
                        format!("{key}/{name}")
                    });
                }
            }
        }
        Ok(())
    }

    fn remove_tree(&mut self, key: &str) {
        self.dirs.retain(|wd, dir| {
            if key.is_empty() || super::store::under(dir, key) {
                unsafe {
                    libc::inotify_rm_watch(self.fd.as_raw_fd(), *wd);
                }
                false
            } else {
                true
            }
        });
    }

    fn read(&mut self) -> Result<Changes> {
        let mut changes = Changes::default();
        let mut bytes = [0u8; 64 * 1024];
        // Bound a batch so sustained filesystem activity still wakes uploads.
        for _ in 0..64 {
            let n =
                unsafe { libc::read(self.fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
            if n < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if n == 0 {
                break;
            }
            let mut offset = 0;
            while offset + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                let event = unsafe {
                    std::ptr::read_unaligned(
                        bytes.as_ptr().add(offset).cast::<libc::inotify_event>(),
                    )
                };
                offset += std::mem::size_of::<libc::inotify_event>();
                let end = offset + event.len as usize;
                if end > n as usize {
                    changes.rescan = true;
                    break;
                }
                let name = &bytes[offset..end];
                offset = end;
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    changes.rescan = true;
                    continue;
                }
                if event.mask & libc::IN_IGNORED != 0 {
                    continue;
                }
                let Some(parent) = self.dirs.get(&event.wd).cloned() else {
                    continue;
                };
                let len = name.iter().position(|b| *b == 0).unwrap_or(name.len());
                if len == 0 {
                    if parent.is_empty() {
                        changes.rescan = true;
                    }
                    continue;
                }
                let name = String::from_utf8_lossy(&name[..len]);
                if name.ends_with(super::local::PART_SUFFIX) {
                    if event.mask & libc::IN_MOVED_FROM != 0 {
                        self.own_moves.insert(event.cookie);
                    }
                    continue;
                }
                if event.mask & libc::IN_MOVED_TO != 0 && self.own_moves.remove(&event.cookie) {
                    continue; // our atomic landing already updated the ledger
                }
                let key = if parent.is_empty() {
                    name.to_string()
                } else {
                    format!("{parent}/{name}")
                };
                if self.ignore.read().is_ignored(&key) {
                    continue;
                }
                if event.mask & libc::IN_ISDIR != 0 {
                    if event.mask & (libc::IN_DELETE | libc::IN_MOVED_FROM) != 0 {
                        self.remove_tree(&key);
                    }
                    if event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                        self.add_tree(&key)?;
                    }
                } else if event.mask & libc::IN_CREATE != 0 {
                    // A normal copy isn't ready until its writer closes it.
                    continue;
                }
                changes.paths.insert(key);
            }
        }
        if changes.rescan {
            self.own_moves.clear();
            self.remove_tree("");
            self.add_tree("")?;
        }
        Ok(changes)
    }
}

impl Watcher {
    pub fn start(
        local: Arc<Local>,
        ignore: Arc<RwLock<IgnoreRules>>,
        notify: impl Fn(Changes) + Send + 'static,
    ) -> Result<Self> {
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut watches = Watches {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            dirs: HashMap::new(),
            own_moves: HashSet::new(),
            local,
            ignore,
        };
        watches.add_tree("")?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("canvas-fuse-watch".into())
            .spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    let mut fd = libc::pollfd {
                        fd: watches.fd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if unsafe { libc::poll(&mut fd, 1, 100) } <= 0 {
                        continue;
                    }
                    match watches.read() {
                        Ok(changes) if changes.rescan || !changes.paths.is_empty() => {
                            notify(changes)
                        }
                        Ok(_) => {}
                        Err(error) => {
                            log::warn!(
                                "mirror watcher failed; requesting a recovery scan: {error:#}"
                            );
                            notify(Changes {
                                rescan: true,
                                ..Default::default()
                            });
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    #[test]
    fn a_file_notification_waits_for_close_and_names_only_the_changed_path() {
        let dir = tempfile::tempdir().unwrap();
        let local =
            Arc::new(Local::open(&dir.path().join("home"), &dir.path().join("state")).unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = Watcher::start(
            local.clone(),
            Arc::new(RwLock::new(IgnoreRules::new(Vec::<String>::new()))),
            move |c| {
                tx.send(c).unwrap();
            },
        )
        .unwrap();
        let mut file = std::fs::File::create(local.path("photo.jpg")).unwrap();
        file.write_all(b"first part").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        file.write_all(b" and the rest").unwrap();
        drop(file);
        let changes = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!changes.rescan);
        assert_eq!(changes.paths, HashSet::from(["photo.jpg".to_string()]));
        // Our own download staging files must never trigger an upload.
        std::fs::write(local.path(".remote.canvas-part"), b"partial download").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        local
            .write_atomic("our-save.txt", b"already queued by FUSE")
            .unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }
}
