use crate::api::ApiClient;
use crate::blobs::{reply_slice, BlobStore};
use crate::mirror::sync::Mirror;
use crate::nudge::{nudge_file, NUDGE_INO};
use crate::state::{NodeContent, Tree};
use crate::writes::WriteStore;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use parking_lot::RwLock;
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

// Short kernel cache TTLs; correctness comes from explicit notifier
// invalidations, the TTL only bounds staleness if a notification is lost.
const TTL: Duration = Duration::from_secs(1);

pub struct CanvasFs {
    tree: Arc<RwLock<Tree>>,
    blobs: Arc<BlobStore>,
    writes: Arc<WriteStore>,
    /// Home is a passthrough drive: its listings and bytes are fetched on
    /// demand rather than reconciled into the tree by the worker, so the
    /// filesystem itself needs the client.
    api: Arc<ApiClient>,
    /// Mirror mode: Home reads come from the content cache (or a fetch off
    /// this thread), never from the server inline.
    mirror: Option<Arc<Mirror>>,
    uid: u32,
    gid: u32,
}

impl CanvasFs {
    pub fn new(
        tree: Arc<RwLock<Tree>>,
        blobs: Arc<BlobStore>,
        writes: Arc<WriteStore>,
        api: Arc<ApiClient>,
        mirror: Option<Arc<Mirror>>,
    ) -> Self {
        Self {
            tree,
            blobs,
            api,
            mirror,
            writes,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn file_attr(&self, ino: u64, size: u64, mtime: SystemTime, is_dir: bool) -> FileAttr {
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind: if is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn attr(&self, node: &crate::state::Node) -> FileAttr {
        // An active write buffer wins over tree content: editors stat
        // between write() and close() and must see the buffered size.
        let size = if let Some(buffered) = self.writes.size_override(node.ino) {
            buffered
        } else if let NodeContent::Remote {
            size: None,
            workspace_id,
            doc_id,
            checksum,
        } = &node.content
        {
            // File with no metadata.size — resolve from the blob (cached after
            // the first stat) so it's shown as the real file, not a stub.
            let key = checksum
                .clone()
                .unwrap_or_else(|| format!("{workspace_id}/{doc_id}"));
            self.blobs.resolve_size(&key, workspace_id, *doc_id)
        } else {
            node.size()
        };
        self.file_attr(node.ino, size, node.mtime, node.is_dir())
    }

    /// Attr of the virtual `.canvas-tmp` nudge marker (see nudge.rs). Every op
    /// on the name/ino short-circuits BEFORE the tree and the write path: the
    /// marker must never take a lock (the nudge thread's syscalls may race a
    /// worker holding them) and never become a server document.
    fn nudge_attr(&self) -> FileAttr {
        self.file_attr(NUDGE_INO, 0, SystemTime::now(), false)
    }

    /// Attr for an ino that may be a tree node or a pending overlay file.
    fn attr_for_ino(&self, ino: u64) -> Option<FileAttr> {
        if let Some(node) = self.tree.read().get(ino) {
            return Some(self.attr(node));
        }
        let entry = self.writes.overlay_attr(ino)?;
        Some(self.file_attr(entry.ino, entry.size, entry.mtime, false))
    }
}

impl CanvasFs {
    /// Fetch a home directory's listing the first time something looks into it.
    ///
    /// Home is a real drive that can be enormous, so it is materialized folder
    /// by folder on demand rather than walked at mount. This blocks the FUSE
    /// loop for one REST call — the same trade the write path already makes on
    /// close-time flush.
    fn ensure_home_loaded(&self, ino: u64) {
        // Mirror mode: the store IS the listing; nothing is fetched on look.
        if self.mirror.is_some() {
            return;
        }
        let Some((path, loaded)) = self.tree.read().home_path(ino) else {
            return;
        };
        if loaded {
            return;
        }
        let Some(ws) = self.tree.read().ws_id() else {
            return;
        };
        match self.api.list_home(&ws, &path) {
            Ok(entries) => {
                self.tree.write().apply_home_entries(ino, &entries);
            }
            Err(e) => log::warn!("home listing {path}: {e:#}"),
        }
    }
}

fn wants_write(flags: i32) -> bool {
    (flags & libc::O_ACCMODE) != libc::O_RDONLY
}

impl Filesystem for CanvasFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        // The nudge marker "exists" only between its create and unlink, both
        // served from the dentry the create reply installed; a fresh lookup
        // always misses, keeping the marker invisible.
        if name == nudge_file() {
            reply.error(libc::ENOENT);
            return;
        }
        if let Some(node) = self.tree.read().lookup(parent, name) {
            reply.entry(&TTL, &self.attr(node), 0);
            return;
        }
        // A miss inside Home may just mean the folder has not been listed yet.
        self.ensure_home_loaded(parent);
        if let Some(node) = self.tree.read().lookup(parent, name) {
            reply.entry(&TTL, &self.attr(node), 0);
            return;
        }
        if let Some(entry) = self.writes.lookup_overlay(parent, name) {
            let attr = self.file_attr(entry.ino, entry.size, entry.mtime, false);
            reply.entry(&TTL, &attr, 0);
            return;
        }
        reply.error(libc::ENOENT);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        if ino == NUDGE_INO {
            reply.attr(&TTL, &self.nudge_attr());
            return;
        }
        match self.attr_for_ino(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(libc::ENOENT),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if ino == NUDGE_INO {
            reply.attr(&TTL, &self.nudge_attr());
            return;
        }
        if let Some(size) = size {
            log::debug!("fs setattr size ino={ino} size={size}");
            if let Err(e) = self.writes.truncate(ino, size) {
                reply.error(e.errno());
                return;
            }
        }
        // mode/uid/gid/times have no backend meaning; acknowledge silently
        match self.attr_for_ino(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        // fh 0 = read handle; flush/release already no-op on it
        if ino == NUDGE_INO {
            reply.opened(0, 0);
            return;
        }
        if !wants_write(flags) {
            reply.opened(0, 0);
            return;
        }
        log::debug!("fs open(write) ino={ino} flags={flags:#x}");
        let truncate = (flags & libc::O_TRUNC) != 0;
        let result = if self.writes.overlay_attr(ino).is_some() {
            self.writes.open_overlay(ino, truncate)
        } else {
            self.writes.open_existing(ino, truncate)
        };
        match result {
            // fh = ino marks a write handle; release/flush act only on those
            Ok(()) => reply.opened(ino, 0),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        log::debug!("fs create parent={parent} name={name}");
        // Virtual nudge marker: succeed without touching the write path (a
        // real create here would mint a server document). fh 0 keeps
        // flush/release on the no-op path.
        if name == nudge_file() {
            reply.created(&TTL, &self.nudge_attr(), 0, 0, 0);
            return;
        }
        match self.writes.create(parent, name) {
            Ok(entry) => {
                let attr = self.file_attr(entry.ino, 0, entry.mtime, false);
                reply.created(&TTL, &attr, 0, entry.ino, 0);
            }
            Err(e) => reply.error(e.errno()),
        }
    }

    fn mknod(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Only the virtual nudge marker; everything else keeps the historical
        // ENOSYS (the kernel then falls back to create for regular files).
        if name.to_str() == Some(nudge_file()) {
            reply.entry(&TTL, &self.nudge_attr(), 0);
            return;
        }
        reply.error(libc::ENOSYS);
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        log::debug!("fs mkdir parent={parent} name={name}");
        match self.writes.mkdir(parent, name) {
            Ok(ino) => {
                let attr = self.file_attr(ino, 0, SystemTime::now(), true);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => reply.error(e.errno()),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        log::debug!("fs rmdir parent={parent} name={name}");
        match self.writes.rmdir(parent, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        log::debug!("fs write ino={ino} offset={offset} len={}", data.len());
        if ino == NUDGE_INO {
            reply.written(data.len() as u32);
            return;
        }
        match self.writes.write(ino, offset, data) {
            Ok(written) => reply.written(written),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        log::debug!("fs flush ino={ino} fh={fh}");
        if fh == 0 {
            reply.ok();
            return;
        }
        match self.writes.flush(ino) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn fsync(&mut self, _req: &Request<'_>, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        if fh == 0 {
            reply.ok();
            return;
        }
        match self.writes.flush(ino) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        log::debug!("fs release ino={ino} fh={fh}");
        if fh != 0 {
            // Last-chance flush; close-time errors should reach the app
            let result = self.writes.flush_final(ino);
            self.writes.release(ino);
            if let Err(e) = result {
                reply.error(e.errno());
                return;
            }
        }
        reply.ok();
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        if name == nudge_file() {
            reply.ok();
            return;
        }
        // The nudge thread collecting a document that already left the view.
        // Checked BEFORE the write path and resolved entirely in the tree, so
        // this can never reach the server: a stale tombstone can only make a
        // file vanish locally, which the next refresh puts back.
        let tombstoned = self.tree.write().take_tombstone(parent, name);
        if let Some(ino) = tombstoned {
            self.tree.write().drop_tombstoned(ino);
            log::trace!("collected tombstone {name}");
            reply.ok();
            return;
        }
        match self.writes.unlink(parent, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.writes.rename(parent, name, newparent, newname) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if ino == NUDGE_INO {
            reply.data(&[]);
            return;
        }
        // Active write buffer is the freshest truth (editors read back
        // through the same or another handle mid-edit)
        if let Some(slice) = self.writes.read_buffer(ino, offset, size) {
            reply.data(&slice);
            return;
        }
        let content = {
            let tree = self.tree.read();
            match tree.get(ino) {
                Some(node) => node.content.clone(),
                None => {
                    // Pending overlay file without a write state cannot happen
                    // (state lives as long as the overlay entry), but be safe
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };
        match content {
            NodeContent::Dir | NodeContent::HomeDir { .. } => reply.error(libc::EISDIR),
            NodeContent::Inline(bytes) => reply_slice(reply, &bytes, offset, size),
            // Home is a passthrough: read the window straight from the drive.
            // No blob cache — these bytes are not content-addressed and can
            // change under us at any moment.
            NodeContent::HomeFile {
                path,
                size: file_size,
            } => {
                if offset as u64 >= file_size {
                    reply.data(&[]);
                    return;
                }
                if let Some(m) = &self.mirror {
                    // Cache pread, or a fetch on the pool; never the network
                    // on this thread.
                    m.read(path.trim_matches('/'), offset, size, reply);
                    return;
                }
                let start = offset as u64;
                let end = (start + size as u64 - 1).min(file_size.saturating_sub(1));
                let ws = match self.tree.read().ws_id() {
                    Some(id) => id,
                    None => {
                        reply.error(libc::EIO);
                        return;
                    }
                };
                match self.api.read_home_range(&ws, &path, start, end) {
                    Ok(bytes) => reply.data(bytes.as_slice()),
                    Err(e) => {
                        log::warn!("home read {path} [{start}-{end}]: {e:#}");
                        reply.error(libc::EIO);
                    }
                }
            }
            NodeContent::Remote {
                workspace_id,
                doc_id,
                checksum,
                ..
            } => {
                // Cache by checksum when known (content-addressed dedupe
                // across contexts), otherwise by document identity
                let key = checksum.unwrap_or_else(|| format!("{workspace_id}/{doc_id}"));
                self.blobs
                    .read(&key, &workspace_id, doc_id, offset, size, reply);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        self.ensure_home_loaded(ino);

        let mut entries: Vec<(u64, FileType, String)> = Vec::new();
        {
            let tree = self.tree.read();
            let Some(dir) = tree.get(ino) else {
                reply.error(libc::ENOENT);
                return;
            };
            if !dir.is_dir() {
                reply.error(libc::ENOTDIR);
                return;
            }
            entries.push((ino, FileType::Directory, ".".to_string()));
            entries.push((dir.parent, FileType::Directory, "..".to_string()));
            if let Some(children) = tree.list(ino) {
                for child in children {
                    // Defensive: a server doc named like the nudge marker must
                    // not surface (the name is reserved and unlookupable).
                    if child.name == nudge_file() {
                        continue;
                    }
                    let kind = if child.is_dir() {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    entries.push((child.ino, kind, child.name.clone()));
                }
            }
        }
        // Pending creates appear alongside server-backed entries
        for (overlay_ino, name) in self.writes.overlay_entries(ino) {
            if name != nudge_file() && !entries.iter().any(|(_, _, n)| n == &name) {
                entries.push((overlay_ino, FileType::RegularFile, name));
            }
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            // offset of the *next* entry, as the kernel resumes from there
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: fuser::ReplyStatfs) {
        // Report ample free space. Writes don't consume local disk (they go to
        // the server), but file managers and copy tools pre-check statvfs and
        // refuse with "not enough space" if free blocks are zero.
        const BLOCKS: u64 = 1 << 32; // ~16 TiB at 4 KiB blocks
                                     // (blocks, bfree, bavail, files, ffree, bsize, namelen, frsize)
        reply.statfs(BLOCKS, BLOCKS, BLOCKS, 0, 1 << 20, 4096, 255, 4096);
    }
}
