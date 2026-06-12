use crate::blobs::{reply_slice, BlobStore};
use crate::state::{NodeContent, Tree};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request,
};
use parking_lot::RwLock;
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::Duration;

// Short kernel cache TTLs; correctness comes from explicit notifier
// invalidations, the TTL only bounds staleness if a notification is lost.
const TTL: Duration = Duration::from_secs(1);

pub struct CanvasFs {
    tree: Arc<RwLock<Tree>>,
    blobs: Arc<BlobStore>,
    uid: u32,
    gid: u32,
}

impl CanvasFs {
    pub fn new(tree: Arc<RwLock<Tree>>, blobs: Arc<BlobStore>) -> Self {
        Self {
            tree,
            blobs,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn attr(&self, node: &crate::state::Node) -> FileAttr {
        let kind = if node.is_dir() {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let size = node.size();
        FileAttr {
            ino: node.ino,
            size,
            blocks: size.div_ceil(512),
            atime: node.mtime,
            mtime: node.mtime,
            ctime: node.mtime,
            crtime: node.mtime,
            kind,
            perm: if node.is_dir() { 0o755 } else { 0o644 },
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

impl Filesystem for CanvasFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        let tree = self.tree.read();
        match tree.lookup(parent, name) {
            Some(node) => reply.entry(&TTL, &self.attr(node), 0),
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let tree = self.tree.read();
        match tree.get(ino) {
            Some(node) => reply.attr(&TTL, &self.attr(node)),
            None => reply.error(libc::ENOENT),
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
        let content = {
            let tree = self.tree.read();
            match tree.get(ino) {
                Some(node) => node.content.clone(),
                None => {
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
        let tree = self.tree.read();
        let Some(dir) = tree.get(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !dir.is_dir() {
            reply.error(libc::ENOTDIR);
            return;
        }
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (dir.parent, FileType::Directory, "..".to_string()),
        ];
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
