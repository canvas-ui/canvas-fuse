use crate::blobs::{reply_slice, BlobStore};
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
    uid: u32,
    gid: u32,
}

impl CanvasFs {
    pub fn new(tree: Arc<RwLock<Tree>>, blobs: Arc<BlobStore>, writes: Arc<WriteStore>) -> Self {
        Self {
            tree,
            blobs,
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
        // between write() and close() and must see the buffered size
        let size = self
            .writes
            .size_override(node.ino)
            .unwrap_or_else(|| node.size());
        self.file_attr(node.ino, size, node.mtime, node.is_dir())
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

fn wants_write(flags: i32) -> bool {
    (flags & libc::O_ACCMODE) != libc::O_RDONLY
}

impl Filesystem for CanvasFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
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
        match self.writes.create(parent, name) {
            Ok(entry) => {
                let attr = self.file_attr(entry.ino, 0, entry.mtime, false);
                reply.created(&TTL, &attr, 0, entry.ino, 0);
            }
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
            NodeContent::Dir => reply.error(libc::EISDIR),
            NodeContent::Inline(bytes) => reply_slice(reply, &bytes, offset, size),
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
            if !entries.iter().any(|(_, _, n)| n == &name) {
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
        reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
    }
}
