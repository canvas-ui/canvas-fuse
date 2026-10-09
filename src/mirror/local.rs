//! The mirror's bytes: a REAL directory on disk, one file per key.
//!
//! This is the whole point of mirror mode — `~/Workspaces/<ws>/Home` is a
//! plain folder that stays intact when the hub is unreachable, when the
//! daemon is down, or when the kernel mount is gone. A mount covers it with
//! the FUSE view; the daemon keeps reaching the real files through a
//! directory fd it opened BEFORE mounting (`/proc/self/fd/<n>/…` resolves
//! into the covered directory, not into the mount on top of it). Unmounted,
//! the same files sit at the same path, editable with anything; the next
//! mount scans them and reconciles.
//!
//! Everything here is local disk I/O: writes land atomically (a dotted
//! `.<name>.canvas-part` next to the target, renamed into place once the
//! bytes are complete and, for a download, the digest verified), hub-deleted
//! files move to `<data_dir>/trash/<key>` for 30 days, and the local side of
//! a conflict is snapshotted to `<data_dir>/conflicts/<sha256>` before the
//! hub's version takes the name.

use super::hub::{HubClient, HubError};
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Suffix of an in-progress write. Dotted so the hub's rules ignore it, and
/// skipped by the scan so a crash mid-download never shows up as a file.
pub const PART_SUFFIX: &str = ".canvas-part";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalStat {
    pub size: u64,
    /// ms since the epoch
    pub mtime: u64,
    pub is_dir: bool,
    /// High-resolution change identity: mtime alone misses edits made by
    /// tools that preserve timestamps, especially while the daemon is down.
    pub fingerprint: String,
}

fn fingerprint(m: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}:{}:{}:{}:{}",
        m.dev(),
        m.ino(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec()
    )
}

/// A hashed file: what the bytes on disk are right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashed {
    pub sha256: String,
    pub size: u64,
    pub mtime: u64,
}

/// An immutable upload body. Network retries reopen this snapshot, never a
/// user's actively edited inode. Dropping the request removes the staging file.
pub struct UploadSnapshot {
    pub path: PathBuf,
    pub hashed: Hashed,
    pub fingerprint: String,
}

impl Drop for UploadSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct Local {
    /// Where keys resolve: `/proc/self/fd/<n>` for a directory we hold open,
    /// so the path keeps working once a mount covers the directory itself.
    root: PathBuf,
    /// The directory the user sees (for messages and status).
    display: PathBuf,
    _fd: OwnedFd,
    trash_root: PathBuf,
    conflicts_root: PathBuf,
    uploads_root: PathBuf,
    /// Serializes rename-into-place against a concurrent removal of the same
    /// key.
    lock: Mutex<()>,
}

fn ms_of(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn io_err(e: impl std::fmt::Display) -> HubError {
    HubError::Other(e.to_string())
}

/// A key the filesystem can hold: relative, no empty or `..` segments.
pub(super) fn check_key(key: &str) -> Result<()> {
    anyhow::ensure!(!key.is_empty(), "empty key");
    anyhow::ensure!(
        key.split('/').next() != Some(super::identity::MARKER),
        ".workspace.json is reserved local mirror identity metadata"
    );
    for seg in key.split('/') {
        anyhow::ensure!(
            !seg.is_empty() && seg != "." && seg != "..",
            "invalid key {key:?}"
        );
    }
    Ok(())
}

impl Local {
    /// Open the directory (created if missing) and hold an fd to it. Must be
    /// called BEFORE a FUSE mount covers `dir` — afterwards a fresh open
    /// would land in the mount, not in the real folder.
    pub fn open(dir: &Path, data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())
            .context("directory path contains a NUL byte")?;
        let raw = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("opening {}", dir.display()));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let root = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        let trash_root = data_dir.join("trash");
        let conflicts_root = data_dir.join("conflicts");
        std::fs::create_dir_all(&trash_root)?;
        std::fs::create_dir_all(&conflicts_root)?;
        let uploads_root = data_dir.join("uploads");
        std::fs::create_dir_all(&uploads_root)?;
        // Store::open has acquired the exclusive mirror database lock. Upload
        // bodies are transient; recover pending work from Home after a crash.
        for entry in std::fs::read_dir(&uploads_root)?.flatten() {
            if entry.file_type()?.is_file() {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(Self {
            root,
            display: dir.to_path_buf(),
            _fd: fd,
            trash_root,
            conflicts_root,
            uploads_root,
            lock: Mutex::new(()),
        })
    }

    /// The folder as the user knows it.
    pub fn display_path(&self) -> &Path {
        &self.display
    }

    /// Where a key lives on disk (through the held fd).
    pub fn path(&self, key: &str) -> PathBuf {
        if key.is_empty() {
            return self.root.clone();
        }
        self.root.join(key)
    }

    fn part_path(&self, key: &str) -> PathBuf {
        let p = self.path(key);
        let leaf = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        p.with_file_name(format!(".{leaf}{PART_SUFFIX}"))
    }

    pub fn trash_path(&self, key: &str) -> PathBuf {
        self.trash_root.join(key)
    }

    pub fn conflict_path(&self, sha: &str) -> PathBuf {
        self.conflicts_root.join(sha)
    }

    // ── stat / read ──────────────────────────────────────────────────────────

    pub fn stat(&self, key: &str) -> Option<LocalStat> {
        let m = std::fs::symlink_metadata(self.path(key)).ok()?;
        if m.file_type().is_symlink() {
            return None;
        }
        Some(LocalStat {
            size: m.len(),
            mtime: m.modified().map(ms_of).unwrap_or(0),
            is_dir: m.is_dir(),
            fingerprint: fingerprint(&m),
        })
    }

    pub fn exists(&self, key: &str) -> bool {
        self.stat(key).is_some()
    }

    pub fn is_file(&self, key: &str) -> bool {
        self.stat(key).is_some_and(|s| !s.is_dir)
    }

    pub fn is_dir(&self, key: &str) -> bool {
        self.stat(key).is_some_and(|s| s.is_dir)
    }

    pub fn read_all(&self, key: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.path(key))
    }

    /// A byte window. Short reads at EOF are the kernel's normal expectation.
    pub fn pread(&self, key: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open(self.path(key))?;
        let mut buf = vec![0u8; len];
        let mut done = 0usize;
        while done < len {
            let n = f.read_at(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        Ok(buf)
    }

    /// Digest + stat of the file as it is on disk now.
    pub fn hash(&self, key: &str) -> std::io::Result<Hashed> {
        let path = self.path(key);
        let mut f = std::fs::File::open(&path)?;
        let m = f.metadata()?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(Hashed {
            sha256: super::hex(&hasher.finalize()),
            size: m.len(),
            mtime: m.modified().map(ms_of).unwrap_or(0),
        })
    }

    // ── write ────────────────────────────────────────────────────────────────

    fn ensure_parent(&self, key: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path(key).parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    /// Replace the file at `key` with `bytes`, atomically. Returns what
    /// landed (the digest is computed from the bytes, the mtime read back).
    pub fn write_atomic(&self, key: &str, bytes: &[u8]) -> Result<Hashed> {
        check_key(key)?;
        self.ensure_parent(key)?;
        // A local save must not truncate the resumable download's staging
        // inode while the network is still writing to it.
        let target = self.path(key);
        let name = target.file_name().unwrap().to_string_lossy();
        let part = target.with_file_name(format!(
            ".{name}.local-{}{PART_SUFFIX}",
            super::operation_id()?
        ));
        {
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&part)?;
            f.write_all(bytes)?;
            f.sync_data()?;
        }
        let _g = self.lock.lock();
        std::fs::rename(&part, self.path(key))?;
        let mtime = std::fs::metadata(self.path(key))
            .and_then(|m| m.modified())
            .map(ms_of)
            .unwrap_or_else(|_| super::now_ms());
        Ok(Hashed {
            sha256: super::sha256_hex(bytes),
            size: bytes.len() as u64,
            mtime,
        })
    }

    pub fn upload_snapshot(&self, key: &str) -> Result<UploadSnapshot> {
        check_key(key)?;
        let mut source = std::fs::File::open(self.path(key))?;
        let before = source.metadata()?;
        let mut snapshot = UploadSnapshot {
            path: self.uploads_root.join(super::operation_id()?),
            hashed: Hashed {
                sha256: String::new(),
                size: 0,
                mtime: before.modified().map(ms_of).unwrap_or(0),
            },
            fingerprint: fingerprint(&before),
        };
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&snapshot.path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            out.write_all(&buffer[..n])?;
            hasher.update(&buffer[..n]);
            snapshot.hashed.size += n as u64;
        }
        anyhow::ensure!(
            fingerprint(&source.metadata()?) == snapshot.fingerprint,
            "file changed while preparing upload"
        );
        snapshot.hashed.sha256 = super::hex(&hasher.finalize());
        Ok(snapshot)
    }

    /// Download `key` from the hub into place, verifying the digest before
    /// the file takes its name. Resumes a `.canvas-part` left by a crash.
    /// Returns what landed; the digest may differ from `expected_sha` when
    /// the key changed on the hub meanwhile — the caller decides.
    pub fn fetch<G>(
        &self,
        hub: &HubClient,
        key: &str,
        expected_sha: &str,
        remote_mtime: u64,
        before_land: &impl Fn() -> Result<G, HubError>,
    ) -> Result<(Hashed, G), HubError> {
        use std::os::unix::fs::MetadataExt;
        let stamp = || match std::fs::symlink_metadata(self.path(key)) {
            Ok(m) => Ok(Some((
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(e)),
        };
        check_key(key).map_err(io_err)?;
        let before = stamp()?;
        self.ensure_parent(key).map_err(io_err)?;
        // Hold the parent inode while downloading: a local directory rename
        // must not strand the staging file at its old pathname.
        let parent = std::fs::File::open(self.path(key).parent().unwrap()).map_err(io_err)?;
        let part = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()))
            .join(self.part_path(key).file_name().unwrap());
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let mut hasher = Sha256::new();
        if have > 0 {
            let mut f = std::fs::File::open(&part).map_err(io_err)?;
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = f.read(&mut buf).map_err(io_err)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
        }
        let response = hub.get_object(key, if have > 0 { Some(have) } else { None });
        let (mut resp, etag) = match response {
            // A crash may leave a complete part, or the remote may shrink.
            // An unsatisfiable resume must restart from byte zero.
            Err(HubError::Refused { status: 416, .. }) if have > 0 => {
                std::fs::remove_file(&part).map_err(io_err)?;
                return self.fetch(hub, key, expected_sha, remote_mtime, before_land);
            }
            other => other?,
        };
        let resumed = have > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        if !resumed {
            hasher = Sha256::new();
        }
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(resumed)
            .write(true)
            .truncate(!resumed)
            .open(&part)
            .map_err(io_err)?;
        let mut buf = vec![0u8; 1 << 16];
        let mut total = if resumed { have } else { 0 };
        loop {
            let n = resp
                .read(&mut buf)
                .map_err(|e| HubError::Offline(format!("reading {key}: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(io_err)?;
            total += n as u64;
        }
        out.sync_data().map_err(io_err)?;
        drop(out);
        let got = super::hex(&hasher.finalize());
        let verified_sha = etag.as_deref().unwrap_or(expected_sha);
        if verified_sha != got {
            let _ = std::fs::remove_file(&part);
            return Err(HubError::Other(format!(
                "{key}: digest mismatch (hub says {verified_sha}, got {got})"
            )));
        }
        if got != expected_sha {
            log::debug!("{key}: fetched {got} while expecting {expected_sha}");
        }
        // The hub's mtime becomes the file's: the scan then sees a file
        // that matches its entry, and `ls -l` shows when it really changed.
        if remote_mtime > 0 {
            if let Ok(f) = std::fs::File::options().write(true).open(&part) {
                let _ =
                    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(remote_mtime));
            }
        }
        let guard = match before_land() {
            Ok(guard) => guard,
            Err(error) => {
                let _ = std::fs::remove_file(&part);
                return Err(error);
            }
        };
        let _g = self.lock.lock();
        if stamp()? != before {
            // A user save can arrive while the request is in flight. Keep
            // that save; its queued job/inotify event takes priority next.
            let _ = std::fs::remove_file(&part);
            return Err(HubError::LocalChanged(key.to_string()));
        }
        std::fs::rename(&part, self.path(key)).map_err(io_err)?;
        let mtime = std::fs::metadata(self.path(key))
            .and_then(|m| m.modified())
            .map(ms_of)
            .unwrap_or(remote_mtime);
        Ok((
            Hashed {
                sha256: got,
                size: total,
                mtime,
            },
            guard,
        ))
    }

    // ── structure ────────────────────────────────────────────────────────────

    pub fn remove_file(&self, key: &str) -> std::io::Result<()> {
        check_key(key).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
        let _g = self.lock.lock();
        match std::fs::remove_file(self.path(key)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    pub fn mkdir(&self, key: &str) -> Result<()> {
        check_key(key)?;
        std::fs::create_dir_all(self.path(key))?;
        Ok(())
    }

    /// Non-recursive, like POSIX. Missing is fine.
    pub fn rmdir(&self, key: &str) -> std::io::Result<()> {
        check_key(key).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
        match std::fs::remove_dir(self.path(key)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    /// `mv` of a file or a directory, parents of the target created.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        check_key(from)?;
        check_key(to)?;
        self.ensure_parent(to)?;
        let _g = self.lock.lock();
        std::fs::rename(self.path(from), self.path(to))
            .with_context(|| format!("renaming {from} → {to}"))?;
        Ok(())
    }

    // ── trash (hub deletes) ──────────────────────────────────────────────────

    /// Move the local copy of `key` into the trash folder (replacing an older
    /// trashed copy of the same key).
    pub fn trash_put(&self, key: &str) -> Result<()> {
        check_key(key)?;
        let dest = self.trash_path(key);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&dest);
        let _g = self.lock.lock();
        match std::fs::rename(self.path(key), &dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                // Another filesystem for the data dir: copy, then remove.
                if e.raw_os_error() == Some(libc::EXDEV) {
                    std::fs::copy(self.path(key), &dest)?;
                    std::fs::remove_file(self.path(key))?;
                    return Ok(());
                }
                Err(e.into())
            }
        }
    }

    /// Bring a trashed key back into the folder. The caller re-keys first
    /// if the name is taken again.
    pub fn trash_restore(&self, key: &str) -> Result<Hashed> {
        check_key(key)?;
        let src = self.trash_path(key);
        anyhow::ensure!(src.is_file(), "{key}: the bytes are no longer in the trash");
        self.ensure_parent(key)?;
        let _g = self.lock.lock();
        if std::fs::rename(&src, self.path(key)).is_err() {
            std::fs::copy(&src, self.path(key))?;
            std::fs::remove_file(&src)?;
        }
        drop(_g);
        Ok(self.hash(key)?)
    }

    pub fn trash_remove(&self, key: &str) {
        let p = self.trash_path(key);
        let _ = std::fs::remove_file(&p);
        // Prune the directories the key passed through, while empty.
        let mut dir = p.parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            if d == self.trash_root || std::fs::remove_dir(&d).is_err() {
                break;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
    }

    // ── conflicts (our bytes, kept while the hub's take the name) ────────────

    /// Copy the current bytes of `key` to the conflicts folder, by digest.
    /// Returns the digest of what was snapshotted.
    pub fn conflict_snapshot(&self, key: &str) -> Result<String> {
        let snapshot = self.upload_snapshot(key)?;
        self.preserve_conflict(&snapshot)?;
        Ok(snapshot.hashed.sha256.clone())
    }

    pub fn preserve_conflict(&self, snapshot: &UploadSnapshot) -> Result<()> {
        let dest = self.conflict_path(&snapshot.hashed.sha256);
        if !dest.is_file() {
            std::fs::rename(&snapshot.path, &dest)?;
        }
        Ok(())
    }

    pub fn has_conflict_bytes(&self, sha: &str) -> bool {
        self.conflict_path(sha).is_file()
    }

    pub fn conflict_remove(&self, sha: &str) {
        let _ = std::fs::remove_file(self.conflict_path(sha));
    }

    // ── scan ─────────────────────────────────────────────────────────────────

    /// Everything in the folder, as (key, stat), files and directories,
    /// depth first. Symlinks and in-progress parts are left out: the former
    /// have no meaning on the hub, the latter are not files yet.
    pub fn walk(&self) -> Vec<(String, LocalStat)> {
        self.walk_from("")
    }

    /// Inspect only an event's path/subtree; ordinary notifications never
    /// require walking the rest of the workspace.
    pub fn walk_from(&self, key: &str) -> Vec<(String, LocalStat)> {
        let mut out = Vec::new();
        if key.split('/').next() == Some(super::identity::MARKER) {
            return out;
        }
        if !key.is_empty() {
            let Some(st) = self.stat(key) else {
                return out;
            };
            let is_dir = st.is_dir;
            out.push((key.to_string(), st));
            if !is_dir {
                return out;
            }
        }
        let mut stack: Vec<String> = vec![key.to_string()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(self.path(&dir)) else {
                continue;
            };
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(PART_SUFFIX)
                    || (dir.is_empty() && name == super::identity::MARKER)
                {
                    continue;
                }
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if !ft.is_file() && !ft.is_dir() {
                    continue;
                }
                let key = if dir.is_empty() {
                    name
                } else {
                    format!("{dir}/{name}")
                };
                let Ok(m) = entry.metadata() else {
                    continue;
                };
                let stat = LocalStat {
                    size: if ft.is_dir() { 0 } else { m.len() },
                    mtime: m.modified().map(ms_of).unwrap_or(0),
                    is_dir: ft.is_dir(),
                    fingerprint: fingerprint(&m),
                };
                if ft.is_dir() {
                    stack.push(key.clone());
                }
                out.push((key, stat));
            }
        }
        out
    }

    /// Trashed keys on disk, with when their file landed there.
    pub fn trash_walk(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let mut stack: Vec<PathBuf> = vec![self.trash_root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(rel) = p.strip_prefix(&self.trash_root) {
                    let mtime = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .map(ms_of)
                        .unwrap_or(0);
                    out.push((rel.to_string_lossy().replace('\\', "/"), mtime));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> (tempfile::TempDir, Local) {
        let dir = tempfile::tempdir().unwrap();
        let l = Local::open(&dir.path().join("Home"), &dir.path().join("data")).unwrap();
        (dir, l)
    }

    #[test]
    fn write_hash_read_and_walk() {
        let (d, l) = local();
        l.mkdir("Docs").unwrap();
        let download_part = l.part_path("Docs/a.txt");
        std::fs::write(&download_part, b"unfinished background download").unwrap();
        let h = l.write_atomic("Docs/a.txt", b"hello").unwrap();
        assert_eq!(
            std::fs::read(download_part).unwrap(),
            b"unfinished background download"
        );
        assert_eq!(h.sha256, super::super::sha256_hex(b"hello"));
        assert_eq!(h.size, 5);
        // The real file is where the user expects it, no indirection.
        assert_eq!(
            std::fs::read(d.path().join("Home/Docs/a.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(l.read_all("Docs/a.txt").unwrap(), b"hello");
        assert_eq!(l.pread("Docs/a.txt", 1, 3).unwrap(), b"ell");
        assert_eq!(l.pread("Docs/a.txt", 4, 10).unwrap(), b"o");
        assert_eq!(l.hash("Docs/a.txt").unwrap(), h);
        l.mkdir("Empty").unwrap();
        let mut keys: Vec<String> = l.walk().into_iter().map(|(k, _)| k).collect();
        keys.sort();
        assert_eq!(keys, vec!["Docs", "Docs/a.txt", "Empty"]);
        assert!(l.is_dir("Empty"));
        assert!(l.is_file("Docs/a.txt"));
        // Only the final file and the pre-existing download part remain;
        // the local save leaves no staging file behind.
        assert_eq!(std::fs::read_dir(l.path("Docs")).unwrap().count(), 2);
    }

    #[test]
    fn rename_trash_and_restore() {
        let (d, l) = local();
        l.write_atomic("x.txt", b"x").unwrap();
        l.rename("x.txt", "sub/y.txt").unwrap();
        assert!(!l.exists("x.txt"));
        assert_eq!(l.read_all("sub/y.txt").unwrap(), b"x");

        l.trash_put("sub/y.txt").unwrap();
        assert!(!l.exists("sub/y.txt"));
        assert!(d.path().join("data/trash/sub/y.txt").is_file());
        assert_eq!(l.trash_walk().len(), 1);
        let h = l.trash_restore("sub/y.txt").unwrap();
        assert_eq!(h.sha256, super::super::sha256_hex(b"x"));
        assert!(l.trash_walk().is_empty());
        assert_eq!(l.read_all("sub/y.txt").unwrap(), b"x");

        l.trash_put("sub/y.txt").unwrap();
        l.trash_remove("sub/y.txt");
        assert!(!d.path().join("data/trash/sub").exists(), "pruned");
    }

    #[test]
    fn keys_cannot_escape_the_folder() {
        let (_d, l) = local();
        assert!(l.write_atomic("../escape.txt", b"no").is_err());
        assert!(l.mkdir("a/../../b").is_err());
    }

    #[test]
    fn fd_keeps_working_when_the_path_is_moved_away() {
        // The fd pins the directory: renaming it on disk (the closest thing
        // to "covered by a mount" a unit test can do) changes nothing.
        let (d, l) = local();
        l.write_atomic("a.txt", b"a").unwrap();
        std::fs::rename(d.path().join("Home"), d.path().join("Elsewhere")).unwrap();
        assert_eq!(l.read_all("a.txt").unwrap(), b"a");
        l.write_atomic("b.txt", b"b").unwrap();
        assert!(d.path().join("Elsewhere/b.txt").is_file());
    }
}
