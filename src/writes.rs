use crate::api::ApiClient;
use crate::names::NameStore;
use crate::state::Tree;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

/// Sticky-name key for a flat context view — mirrors state::FLAT_NAME_KEY.
const FLAT_DIR: &str = "";

/// Overlay inodes live far above anything Tree allocates.
const FIRST_OVERLAY_INO: u64 = 1 << 48;

/// What a dirty buffer flushes to.
#[derive(Debug, Clone)]
enum FlushTarget {
    /// Update an existing document (node lives in the tree)
    Existing { ctx: String, doc_id: u64 },
    /// Create a new document on first flush
    Create {
        ctx: String,
        dir: String,
        dir_ino: u64,
        name: String,
    },
    /// Update an existing document in a workspace tree path
    WsExisting {
        tree_name: String,
        tree_id: String,
        tree_type: String,
        path: String,
        doc_id: u64,
    },
    /// Write a real file on the home drive. Whole-file PUT on flush — the home
    /// API replaces files rather than patching them, which is the same shape
    /// this overlay already uses for documents.
    HomeFile {
        path: String,
        dir_ino: u64,
        name: String,
    },
    /// Create a new document in a workspace tree path on first flush
    WsCreate {
        tree_name: String,
        tree_id: String,
        tree_type: String,
        path: String,
        dir_ino: u64,
        name: String,
    },
}

struct OpenWrite {
    buffer: Vec<u8>,
    dirty: bool,
    refs: u32,
    target: FlushTarget,
}

/// A file that exists locally but has no document yet (created, not flushed).
pub struct OverlayEntry {
    pub ino: u64,
    pub dir_ino: u64,
    pub name: String,
    pub size: u64,
    pub mtime: SystemTime,
}

struct Inner {
    /// keyed by ino — at most one write state per file, refcounted per open
    states: HashMap<u64, OpenWrite>,
    /// pending creates: ino -> entry, plus (dir, name) -> ino for lookup
    overlay: HashMap<u64, (u64, String, SystemTime)>,
    overlay_names: HashMap<(u64, String), u64>,
    next_overlay_ino: u64,
}

pub struct WriteStore {
    api: Arc<ApiClient>,
    tree: Arc<RwLock<Tree>>,
    names: Arc<NameStore>,
    inner: Mutex<Inner>,
    /// Serializes tree-mutating write ops (flush/create-adopt, rename, unlink)
    /// against the refresh worker's fetch+apply. The two run on different
    /// threads; without this a refresh can diff the tree mid-mutation and
    /// transiently drop or mis-home an entry. (Id preservation removed the
    /// *rebind* race; this guards the multi-step local mutations that remain.)
    sync: Arc<Mutex<()>>,
}

pub enum WriteError {
    NotPermitted,
    NotFound,
    Exists,
    /// rmdir on a directory that still has children (POSIX: ENOTEMPTY).
    NotEmpty,
    CrossDir,
    Io(String),
}

impl WriteError {
    pub fn errno(&self) -> i32 {
        match self {
            WriteError::NotPermitted => libc::EACCES,
            WriteError::NotFound => libc::ENOENT,
            WriteError::Exists => libc::EEXIST,
            WriteError::NotEmpty => libc::ENOTEMPTY,
            WriteError::CrossDir => libc::EXDEV,
            WriteError::Io(_) => libc::EIO,
        }
    }
}

type WResult<T> = Result<T, WriteError>;

impl WriteStore {
    pub fn new(api: Arc<ApiClient>, tree: Arc<RwLock<Tree>>, names: Arc<NameStore>) -> Self {
        Self {
            api,
            tree,
            names,
            inner: Mutex::new(Inner {
                states: HashMap::new(),
                overlay: HashMap::new(),
                overlay_names: HashMap::new(),
                next_overlay_ino: FIRST_OVERLAY_INO,
            }),
            sync: Arc::new(Mutex::new(())),
        }
    }

    /// Shared handle the refresh worker holds across its fetch+apply cycle.
    pub fn sync_handle(&self) -> Arc<Mutex<()>> {
        self.sync.clone()
    }

    /// The context a write into `dir_ino` targets. A context folder is flat, so
    /// the folder itself is the only writable place — `.by-schema/` is a
    /// derived view and refuses writes by not resolving here.
    fn writable_context(&self, dir_ino: u64) -> Option<String> {
        self.tree.read().locate_context_dir(dir_ino)
    }

    // ── overlay view (pending creates), consumed by lookup/readdir/getattr ──

    pub fn lookup_overlay(&self, dir_ino: u64, name: &str) -> Option<OverlayEntry> {
        let inner = self.inner.lock();
        let ino = *inner.overlay_names.get(&(dir_ino, name.to_string()))?;
        let (_, _, mtime) = inner.overlay.get(&ino)?;
        let size = inner
            .states
            .get(&ino)
            .map(|s| s.buffer.len() as u64)
            .unwrap_or(0);
        Some(OverlayEntry {
            ino,
            dir_ino,
            name: name.to_string(),
            size,
            mtime: *mtime,
        })
    }

    pub fn overlay_entries(&self, dir_ino: u64) -> Vec<(u64, String)> {
        let inner = self.inner.lock();
        inner
            .overlay
            .iter()
            .filter(|(_, (d, _, _))| *d == dir_ino)
            .map(|(ino, (_, name, _))| (*ino, name.clone()))
            .collect()
    }

    pub fn overlay_attr(&self, ino: u64) -> Option<OverlayEntry> {
        let inner = self.inner.lock();
        let (dir_ino, name, mtime) = inner.overlay.get(&ino)?.clone();
        let size = inner
            .states
            .get(&ino)
            .map(|s| s.buffer.len() as u64)
            .unwrap_or(0);
        Some(OverlayEntry {
            ino,
            dir_ino,
            name,
            size,
            mtime,
        })
    }

    /// Size override for files with an active write buffer (editors stat
    /// between write and close; getattr must reflect the buffer).
    pub fn size_override(&self, ino: u64) -> Option<u64> {
        self.inner
            .lock()
            .states
            .get(&ino)
            .map(|s| s.buffer.len() as u64)
    }

    /// Buffered content, if this ino has an active write state.
    pub fn read_buffer(&self, ino: u64, offset: i64, size: u32) -> Option<Vec<u8>> {
        let inner = self.inner.lock();
        let state = inner.states.get(&ino)?;
        let start = (offset.max(0) as usize).min(state.buffer.len());
        let end = (start + size as usize).min(state.buffer.len());
        Some(state.buffer[start..end].to_vec())
    }

    // ── open / create / write / truncate ────────────────────────────────────

    /// Prepare a write state for an existing tree file (open with write access).
    /// The flush target for a home file that already exists in the view.
    fn home_target(&self, ino: u64) -> Option<FlushTarget> {
        let tree = self.tree.read();
        let (path, _size) = tree.home_file(ino)?;
        let node = tree.get(ino)?;
        Some(FlushTarget::HomeFile {
            path,
            dir_ino: node.parent,
            name: node.name.clone(),
        })
    }

    pub fn open_existing(&self, ino: u64, truncate: bool) -> WResult<()> {
        // Home files are edited whole: read the current bytes into the buffer so
        // a partial write (every editor does one) does not truncate the rest of
        // the file when the PUT replaces it.
        if let Some(target) = self.home_target(ino) {
            let (path, size) = self
                .tree
                .read()
                .home_file(ino)
                .ok_or(WriteError::NotFound)?;
            let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
            let content = if truncate || size == 0 {
                Vec::new()
            } else {
                self.api
                    .read_home_range(&ws, &path, 0, size.saturating_sub(1))
                    .map_err(|e| WriteError::Io(format!("{e:#}")))?
            };
            let mut inner = self.inner.lock();
            let state = inner.states.entry(ino).or_insert_with(|| OpenWrite {
                buffer: content,
                dirty: truncate,
                refs: 0,
                target,
            });
            state.refs += 1;
            if truncate {
                state.buffer.clear();
                state.dirty = true;
            }
            return Ok(());
        }

        let content = {
            let tree = self.tree.read();
            let node = tree.get(ino).ok_or(WriteError::NotFound)?;
            if node.is_dir() {
                return Err(WriteError::NotPermitted);
            }
            match &node.content {
                crate::state::NodeContent::Inline(bytes) => bytes.as_ref().clone(),
                _ => return Err(WriteError::NotPermitted), // blobs are read-only
            }
        };
        let target = if self.tree.read().is_workspace() {
            let (tree_name, tree_id, tree_type, path, doc_id) = self
                .tree
                .read()
                .tree_file(ino)
                .ok_or(WriteError::NotPermitted)?;
            FlushTarget::WsExisting {
                tree_name,
                tree_id,
                tree_type,
                path,
                doc_id,
            }
        } else {
            let parent = self.tree.read().get(ino).map(|n| n.parent).unwrap_or(0);
            let ctx = self
                .writable_context(parent)
                .ok_or(WriteError::NotPermitted)?;
            let (_, doc_id) = self
                .tree
                .read()
                .doc_for_ino(ino)
                .ok_or(WriteError::NotPermitted)?; // .context.json etc. have no doc
            FlushTarget::Existing { ctx, doc_id }
        };

        let mut inner = self.inner.lock();
        let state = inner.states.entry(ino).or_insert_with(|| OpenWrite {
            buffer: content,
            dirty: false,
            refs: 0,
            target,
        });
        state.refs += 1;
        if truncate {
            state.buffer.clear();
            state.dirty = true;
        }
        Ok(())
    }

    /// Open of an overlay (pending) file — just bump the refcount.
    pub fn open_overlay(&self, ino: u64, truncate: bool) -> WResult<()> {
        let mut inner = self.inner.lock();
        let state = inner.states.get_mut(&ino).ok_or(WriteError::NotFound)?;
        state.refs += 1;
        if truncate {
            state.buffer.clear();
            state.dirty = true;
        }
        Ok(())
    }

    pub fn create(&self, dir_ino: u64, name: &str) -> WResult<OverlayEntry> {
        // Home is a real drive: any name is a file, no schema inference.
        let home_dir = self.tree.read().home_path(dir_ino);
        let target = if let Some((dir_path, _)) = home_dir {
            FlushTarget::HomeFile {
                path: crate::state::join_home_path(&dir_path, name),
                dir_ino,
                name: name.to_string(),
            }
        } else if self.tree.read().is_workspace() {
            let (tree_name, tree_id, tree_type, path) = self
                .tree
                .read()
                .locate_tree_dir(dir_ino)
                .ok_or(WriteError::NotPermitted)?;
            FlushTarget::WsCreate {
                tree_name,
                tree_id,
                tree_type,
                path,
                dir_ino,
                name: name.to_string(),
            }
        } else {
            let ctx = self
                .writable_context(dir_ino)
                .ok_or(WriteError::NotPermitted)?;
            FlushTarget::Create {
                ctx,
                dir: FLAT_DIR.to_string(),
                dir_ino,
                name: name.to_string(),
            }
        };
        if self.tree.read().lookup(dir_ino, name).is_some() {
            return Err(WriteError::Exists);
        }
        let mut inner = self.inner.lock();
        if inner
            .overlay_names
            .contains_key(&(dir_ino, name.to_string()))
        {
            return Err(WriteError::Exists);
        }
        let ino = inner.next_overlay_ino;
        inner.next_overlay_ino += 1;
        let now = SystemTime::now();
        inner.overlay.insert(ino, (dir_ino, name.to_string(), now));
        inner.overlay_names.insert((dir_ino, name.to_string()), ino);
        inner.states.insert(
            ino,
            OpenWrite {
                buffer: Vec::new(),
                dirty: true,
                refs: 1,
                target,
            },
        );
        Ok(OverlayEntry {
            ino,
            dir_ino,
            name: name.to_string(),
            size: 0,
            mtime: now,
        })
    }

    pub fn write(&self, ino: u64, offset: i64, data: &[u8]) -> WResult<u32> {
        let mut inner = self.inner.lock();
        let state = inner.states.get_mut(&ino).ok_or(WriteError::NotPermitted)?;
        let offset = offset.max(0) as usize;
        let end = offset + data.len();
        if state.buffer.len() < end {
            state.buffer.resize(end, 0);
        }
        state.buffer[offset..end].copy_from_slice(data);
        state.dirty = true;
        Ok(data.len() as u32)
    }

    /// Truncate (setattr size). Without an open write state (truncate(2) on a
    /// closed file) the change is flushed immediately, as no release will come.
    pub fn truncate(&self, ino: u64, size: u64) -> WResult<()> {
        {
            let mut inner = self.inner.lock();
            if let Some(state) = inner.states.get_mut(&ino) {
                state.buffer.resize(size as usize, 0);
                state.dirty = true;
                return Ok(());
            }
        }
        self.open_existing(ino, false)?;
        {
            let mut inner = self.inner.lock();
            let state = inner.states.get_mut(&ino).ok_or(WriteError::NotFound)?;
            state.buffer.resize(size as usize, 0);
            state.dirty = true;
        }
        let result = self.flush(ino);
        self.release(ino);
        result
    }

    // ── flush / release ──────────────────────────────────────────────────────

    /// Push a dirty buffer to the server. Called from flush/fsync/release —
    /// blocks the FUSE loop for the duration of one REST call (close-time
    /// errors must reach the application).
    pub fn flush(&self, ino: u64) -> WResult<()> {
        self.flush_inner(ino, false)
    }

    /// Flush at close time: also materializes empty creates (touch).
    pub fn flush_final(&self, ino: u64) -> WResult<()> {
        self.flush_inner(ino, true)
    }

    fn flush_inner(&self, ino: u64, final_flush: bool) -> WResult<()> {
        let _sync = self.sync.lock();
        let (buffer, target, dirty) = {
            let mut inner = self.inner.lock();
            let state = match inner.states.get_mut(&ino) {
                Some(s) if s.dirty => s,
                // A home file whose bytes already went out on an earlier flush
                // still has to be PUBLISHED into the view when the handle
                // closes: the write landed, but until the overlay is retired
                // for a real node, the next lookup(2) misses and the file looks
                // like it was never created.
                Some(s) if final_flush && matches!(s.target, FlushTarget::HomeFile { .. }) => s,
                _ => return Ok(()),
            };
            let dirty = state.dirty;
            // Shells flush right after open(O_CREAT), before any write lands.
            // Creating an empty doc just to supersede it on close is churn —
            // defer empty creates to the final flush (where `touch` needs them).
            if !final_flush
                && state.buffer.is_empty()
                && matches!(
                    state.target,
                    FlushTarget::Create { .. }
                        | FlushTarget::WsCreate { .. }
                        | FlushTarget::HomeFile { .. }
                )
            {
                return Ok(());
            }
            state.dirty = false;
            (state.buffer.clone(), state.target.clone(), dirty)
        };

        let result = match &target {
            FlushTarget::HomeFile {
                path,
                dir_ino,
                name,
            } => {
                if dirty {
                    let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
                    self.api
                        .write_home(&ws, path, buffer.clone())
                        .map_err(|e| WriteError::Io(format!("{e:#}")))?;
                }
                // Publish into the view so the next lookup/readdir sees the
                // file at its real size. ONLY on the final flush: retiring the
                // overlay mid-edit would leave the next write(2) with nowhere
                // to land. The overlay's ino is adopted rather than a fresh one
                // allocated — the kernel already handed that ino to the process
                // from create(), and a new one leaves its cached dentry
                // pointing at nothing (the file reads back ENOENT until the
                // directory is listed again).
                if final_flush {
                    let mut inner = self.inner.lock();
                    let overlay_ino = inner.overlay_names.remove(&(*dir_ino, name.clone()));
                    if let Some(overlay_ino) = overlay_ino {
                        inner.overlay.remove(&overlay_ino);
                    }
                    drop(inner);
                    self.tree.write().upsert_home_file(
                        *dir_ino,
                        name,
                        path,
                        buffer.len() as u64,
                        overlay_ino,
                    );
                }
                Ok(())
            }
            FlushTarget::Existing { ctx, doc_id } => self.flush_update(ctx, *doc_id, &buffer),
            FlushTarget::Create {
                ctx,
                dir,
                dir_ino,
                name,
            } => {
                match self.flush_create(ctx, dir, *dir_ino, name, &buffer, ino) {
                    Ok(doc_id) => {
                        // Subsequent flushes on this handle are updates
                        let mut inner = self.inner.lock();
                        if let Some(state) = inner.states.get_mut(&ino) {
                            state.target = FlushTarget::Existing {
                                ctx: ctx.clone(),
                                doc_id,
                            };
                        }
                        inner.overlay.remove(&ino);
                        inner.overlay_names.remove(&(*dir_ino, name.clone()));
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            FlushTarget::WsExisting {
                tree_name,
                tree_id,
                tree_type,
                path,
                doc_id,
            } => self.flush_ws_update(tree_name, tree_id, tree_type, path, *doc_id, &buffer),
            FlushTarget::WsCreate {
                tree_name,
                tree_id,
                tree_type,
                path,
                dir_ino,
                name,
            } => {
                match self.flush_ws_create(
                    tree_name, tree_id, tree_type, path, *dir_ino, name, &buffer, ino,
                ) {
                    Ok(doc_id) => {
                        let mut inner = self.inner.lock();
                        if let Some(state) = inner.states.get_mut(&ino) {
                            state.target = FlushTarget::WsExisting {
                                tree_name: tree_name.clone(),
                                tree_id: tree_id.clone(),
                                tree_type: tree_type.clone(),
                                path: path.clone(),
                                doc_id,
                            };
                        }
                        inner.overlay.remove(&ino);
                        inner.overlay_names.remove(&(*dir_ino, name.clone()));
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        };

        if result.is_err() {
            // Keep the data; the user can retry the save
            if let Some(state) = self.inner.lock().states.get_mut(&ino) {
                state.dirty = true;
            }
        }
        result
    }

    pub fn release(&self, ino: u64) {
        let mut inner = self.inner.lock();
        if let Some(state) = inner.states.get_mut(&ino) {
            state.refs = state.refs.saturating_sub(1);
            if state.refs == 0 {
                inner.states.remove(&ino);
                // Any leftover overlay entry dies with the last handle:
                // abandoned creates vanish (failed/empty save), and renamed
                // pre-close overlays are owned by the tree node by now
                inner.overlay.remove(&ino);
                inner.overlay_names.retain(|_, &mut i| i != ino);
            }
        }
    }

    pub fn has_state(&self, ino: u64) -> bool {
        self.inner.lock().states.contains_key(&ino)
    }

    // ── unlink / rename ──────────────────────────────────────────────────────

    pub fn unlink(&self, dir_ino: u64, name: &str) -> WResult<()> {
        let _sync = self.sync.lock();

        // Pending overlay file: purely local, applies to either mode.
        {
            let mut inner = self.inner.lock();
            if let Some(ino) = inner.overlay_names.remove(&(dir_ino, name.to_string())) {
                inner.overlay.remove(&ino);
                inner.states.remove(&ino);
                return Ok(());
            }
        }

        // Home is a real drive: `rm` deletes the file. No trash, no detach —
        // the file manager's own warning is the safety net, and the workspace
        // trash is for documents, not for the drive.
        let home_child = self
            .tree
            .read()
            .lookup(dir_ino, name)
            .and_then(|n| self.tree.read().home_file(n.ino).map(|(p, _)| (n.ino, p)));
        if let Some((ino, path)) = home_child {
            let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
            self.api
                .remove_home(&ws, &path)
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;
            self.tree.write().remove_node(ino);
            self.inner.lock().states.remove(&ino);
            return Ok(());
        }

        if self.tree.read().is_workspace() {
            return self.unlink_ws(dir_ino, name);
        }
        let ctx = self
            .writable_context(dir_ino)
            .ok_or(WriteError::NotPermitted)?;

        let ino = {
            let tree = self.tree.read();
            tree.lookup(dir_ino, name)
                .map(|n| n.ino)
                .ok_or(WriteError::NotFound)?
        };
        let (_, doc_id) = self
            .tree
            .read()
            .doc_for_ino(ino)
            .ok_or(WriteError::NotPermitted)?;

        // `rm` DETACHES from this context (removes the path tick), never
        // destroys: FUSE can't distinguish `rm` from `shift+delete`, so the
        // safe default is view-scoped removal — the document survives in the DB
        // and in any other context it's linked into. (A directory-type mount,
        // where the folder IS the doc's home, could destroy instead — future.)
        self.api
            .remove_documents(&ctx, &[doc_id])
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;

        self.tree.write().remove_doc_node(ino);
        self.inner.lock().states.remove(&ino);
        Ok(())
    }

    pub fn rename(
        &self,
        src_dir: u64,
        src_name: &str,
        dst_dir: u64,
        dst_name: &str,
    ) -> WResult<()> {
        if self.tree.read().is_workspace() {
            return self.rename_ws(src_dir, src_name, dst_dir, dst_name);
        }
        if src_dir != dst_dir {
            // EXDEV makes `mv` fall back to copy+unlink, which composes from
            // primitives we already support
            return Err(WriteError::CrossDir);
        }
        let _sync = self.sync.lock();
        let ctx = self
            .writable_context(src_dir)
            .ok_or(WriteError::NotPermitted)?;
        let dir = FLAT_DIR.to_string();

        let src_overlay = {
            let inner = self.inner.lock();
            inner
                .overlay_names
                .get(&(src_dir, src_name.to_string()))
                .copied()
        };
        let dst_tree_ino = self.tree.read().lookup(dst_dir, dst_name).map(|n| n.ino);

        if let Some(src_ino) = src_overlay {
            // Pending create being renamed (editor wrote tmp, renames to target
            // before close). Retarget the open state; kernel keeps using src_ino.
            let mut inner = self.inner.lock();
            inner.overlay_names.remove(&(src_dir, src_name.to_string()));
            match dst_tree_ino.and_then(|i| self.tree.read().doc_for_ino(i)) {
                Some((_, doc_id)) => {
                    // Replaces an existing note: flushes become updates. The
                    // overlay entry stays (renamed) so the kernel's post-rename
                    // dentry — which points at src_ino — keeps resolving until
                    // the handle closes; release() cleans it up.
                    if let Some(state) = inner.states.get_mut(&src_ino) {
                        state.target = FlushTarget::Existing { ctx, doc_id };
                        state.dirty = true;
                    }
                    if let Some(entry) = inner.overlay.get_mut(&src_ino) {
                        entry.1 = dst_name.to_string();
                    }
                }
                None => {
                    if let Some(entry) = inner.overlay.get_mut(&src_ino) {
                        entry.1 = dst_name.to_string();
                    }
                    inner
                        .overlay_names
                        .insert((dst_dir, dst_name.to_string()), src_ino);
                    if let Some(state) = inner.states.get_mut(&src_ino) {
                        if let FlushTarget::Create { name, .. } = &mut state.target {
                            *name = dst_name.to_string();
                        }
                    }
                }
            }
            return Ok(());
        }

        let src_ino = self
            .tree
            .read()
            .lookup(src_dir, src_name)
            .map(|n| n.ino)
            .ok_or(WriteError::NotFound)?;
        let (_, src_doc) = self
            .tree
            .read()
            .doc_for_ino(src_ino)
            .ok_or(WriteError::NotPermitted)?;

        match dst_tree_ino {
            None => {
                // Plain rename: sticky-name reassignment, doc untouched
                self.names
                    .put(&ctx, &dir, src_doc, dst_name)
                    .map_err(|e| WriteError::Io(format!("{e:#}")))?;
                self.tree.write().rename_entry(src_ino, dst_name);
                Ok(())
            }
            Some(dst_ino) => {
                // Overwrite-rename (atomic save: tmp file replaces target).
                // Copy src content into dst's doc, then remove src. POSIX:
                // after rename the dst NAME must carry the SRC inode — the
                // kernel's post-rename dentry points at src_ino, so the
                // surviving node must live there.
                let (_, dst_doc) = self
                    .tree
                    .read()
                    .doc_for_ino(dst_ino)
                    .ok_or(WriteError::NotPermitted)?;
                let content = {
                    let tree = self.tree.read();
                    match tree.get(src_ino).map(|n| n.content.clone()) {
                        Some(crate::state::NodeContent::Inline(b)) => b.as_ref().clone(),
                        _ => return Err(WriteError::NotPermitted),
                    }
                };
                // dst keeps its (stable) id, gains src's content; src is
                // detached from this context (safe default; survives in DB).
                self.flush_update(&ctx, dst_doc, &content)?;
                self.api
                    .remove_documents(&ctx, &[src_doc])
                    .map_err(|e| WriteError::Io(format!("{e:#}")))?;

                {
                    let mut tree = self.tree.write();
                    tree.remove_doc_node(dst_ino);
                    tree.unbind_doc(&ctx, src_doc);
                    tree.rename_entry(src_ino, dst_name);
                    tree.bind_doc(&ctx, dst_doc, src_ino);
                    tree.set_inline_content(src_ino, Arc::new(content));
                }
                // An open handle on src (rename before close) now writes dst's doc
                let mut inner = self.inner.lock();
                if let Some(state) = inner.states.get_mut(&src_ino) {
                    state.target = FlushTarget::Existing {
                        ctx: ctx.clone(),
                        doc_id: dst_doc,
                    };
                }
                Ok(())
            }
        }
    }

    // ── workspace tree write path ─────────────────────────────────────────────

    /// Create a directory node (mkdir) — inserts a tree path on the server.
    pub fn mkdir(&self, parent_ino: u64, name: &str) -> WResult<u64> {
        let _sync = self.sync.lock();

        // A folder on the home drive is a real directory.
        // Bind the path BEFORE the body: an `if let` holds the read guard for the
        // whole block, and the tree.write() below would deadlock against it
        // (parking_lot is not reentrant).
        let home_parent = self.tree.read().home_path(parent_ino);
        if let Some((parent_path, _)) = home_parent {
            if self.tree.read().lookup(parent_ino, name).is_some() {
                return Err(WriteError::Exists);
            }
            let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
            let child = crate::state::join_home_path(&parent_path, name);
            self.api
                .mkdir_home(&ws, &child)
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;
            return Ok(self.tree.write().insert_home_dir(parent_ino, name, &child));
        }

        let (tree_name, tree_id, _tt, parent_path) = self
            .tree
            .read()
            .locate_tree_dir(parent_ino)
            .ok_or(WriteError::NotPermitted)?;
        let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
        if self.tree.read().lookup(parent_ino, name).is_some() {
            return Err(WriteError::Exists);
        }
        let child = join_path(&parent_path, name);
        self.api
            .insert_tree_path(&ws, &tree_id, &child)
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        let ino = self
            .tree
            .write()
            .adopt_tree_dir(parent_ino, name, &tree_name, &child);
        Ok(ino)
    }

    /// Remove a directory node (rmdir) — removes the tree path on the server.
    pub fn rmdir(&self, parent_ino: u64, name: &str) -> WResult<()> {
        let _sync = self.sync.lock();

        let home_child = self
            .tree
            .read()
            .lookup(parent_ino, name)
            .and_then(|n| self.tree.read().home_path(n.ino).map(|(p, _)| (n.ino, p)));
        if let Some((ino, path)) = home_child {
            // POSIX rmdir is non-recursive; refuse a folder we know has
            // children rather than deleting a subtree behind the user's back.
            if self.tree.read().list(ino).is_some_and(|c| !c.is_empty()) {
                return Err(WriteError::NotEmpty);
            }
            let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
            self.api
                .remove_home(&ws, &path)
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;
            self.tree.write().remove_node(ino);
            return Ok(());
        }

        let (_tn, tree_id, _tt, _pp) = self
            .tree
            .read()
            .locate_tree_dir(parent_ino)
            .ok_or(WriteError::NotPermitted)?;
        let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
        let child_ino = self
            .tree
            .read()
            .lookup(parent_ino, name)
            .map(|n| n.ino)
            .ok_or(WriteError::NotFound)?;
        let (_tn2, _id2, _tt2, child_path) = self
            .tree
            .read()
            .locate_tree_dir(child_ino)
            .ok_or(WriteError::NotPermitted)?;
        // Non-recursive: a bare `rmdir` on a non-empty folder must fail. `rm -r`
        // still works — the kernel unlinks children first, then rmdir the empty dir.
        self.api
            .remove_tree_path(&ws, &tree_id, &child_path, false)
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        // The kernel drops the dentry itself on a successful rmdir; we only need
        // to keep our local view consistent.
        let _ = self.tree.write().remove_tree_dir(child_ino);
        Ok(())
    }

    fn unlink_ws(&self, dir_ino: u64, name: &str) -> WResult<()> {
        let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
        let ino = self
            .tree
            .read()
            .lookup(dir_ino, name)
            .map(|n| n.ino)
            .ok_or(WriteError::NotFound)?;
        let (_tn, tree_id, tree_type, path, doc_id) = self
            .tree
            .read()
            .tree_file(ino)
            .ok_or(WriteError::NotPermitted)?;
        // Inside the trash, `rm` is the permanent one — same as a file manager's
        // "delete from trash". Detaching instead would be a silent no-op: the
        // document is already orphaned, so the server would file it right back.
        if path == crate::state::TRASH_PATH {
            self.api
                .empty_trash(&ws, &[doc_id])
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;
            self.tree.write().remove_tree_file(ino);
            self.inner.lock().states.remove(&ino);
            return Ok(());
        }

        // Everywhere else `rm` detaches from this tree path, never destroys —
        // the document survives in the DB and in any other path it's linked
        // into. If this WAS its last path the server files it into the trash,
        // so nothing a mount does can make a document unreachable.
        self.api
            .remove_tree_document(&ws, &tree_id, &tree_type, &path, &[doc_id], true)
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        self.tree.write().remove_tree_file(ino);
        self.inner.lock().states.remove(&ino);
        Ok(())
    }

    fn rename_ws(&self, src_dir: u64, src_name: &str, dst_dir: u64, dst_name: &str) -> WResult<()> {
        let _sync = self.sync.lock();
        let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;

        // Pending create renamed before close (editor tmp -> target).
        let src_overlay = self
            .inner
            .lock()
            .overlay_names
            .get(&(src_dir, src_name.to_string()))
            .copied();
        if let Some(src_ino) = src_overlay {
            let (tree_name, tree_id, tree_type, path) = self
                .tree
                .read()
                .locate_tree_dir(dst_dir)
                .ok_or(WriteError::NotPermitted)?;
            let dst_doc = self
                .tree
                .read()
                .lookup(dst_dir, dst_name)
                .and_then(|n| self.tree.read().tree_file(n.ino))
                .map(|(_, _, _, _, id)| id);
            let mut inner = self.inner.lock();
            inner.overlay_names.remove(&(src_dir, src_name.to_string()));
            if let Some(entry) = inner.overlay.get_mut(&src_ino) {
                entry.0 = dst_dir;
                entry.1 = dst_name.to_string();
            }
            match dst_doc {
                Some(doc_id) => {
                    if let Some(state) = inner.states.get_mut(&src_ino) {
                        state.target = FlushTarget::WsExisting {
                            tree_name,
                            tree_id,
                            tree_type,
                            path,
                            doc_id,
                        };
                        state.dirty = true;
                    }
                }
                None => {
                    inner
                        .overlay_names
                        .insert((dst_dir, dst_name.to_string()), src_ino);
                    if let Some(state) = inner.states.get_mut(&src_ino) {
                        if let FlushTarget::WsCreate {
                            tree_name: tn,
                            tree_id: ti,
                            tree_type: tt,
                            path: p,
                            dir_ino,
                            name,
                        } = &mut state.target
                        {
                            *tn = tree_name;
                            *ti = tree_id;
                            *tt = tree_type;
                            *p = path;
                            *dir_ino = dst_dir;
                            *name = dst_name.to_string();
                        }
                    }
                }
            }
            return Ok(());
        }

        let src_ino = self
            .tree
            .read()
            .lookup(src_dir, src_name)
            .map(|n| n.ino)
            .ok_or(WriteError::NotFound)?;
        let src_is_dir = self
            .tree
            .read()
            .get(src_ino)
            .map(|n| n.is_dir())
            .unwrap_or(false);

        if src_is_dir {
            // A folder move is a TREE operation: the node moves and every
            // document filed under it comes along untouched. Works across
            // parents as well as in place — but only within one tree, since
            // nodes of different trees have nothing in common.
            if self.tree.read().lookup(dst_dir, dst_name).is_some() {
                return Err(WriteError::Exists);
            }
            let (tree_name, tree_id, _tt, src_path) = self
                .tree
                .read()
                .locate_tree_dir(src_ino)
                .ok_or(WriteError::NotPermitted)?;
            let dst_path = if src_dir == dst_dir {
                join_path(&parent_of(&src_path), dst_name)
            } else {
                let (dst_tree_name, dst_tree_id, _dtt, dst_parent_path) = self
                    .tree
                    .read()
                    .locate_tree_dir(dst_dir)
                    .ok_or(WriteError::NotPermitted)?;
                if dst_tree_id != tree_id || dst_tree_name != tree_name {
                    return Err(WriteError::CrossDir);
                }
                join_path(&dst_parent_path, dst_name)
            };
            self.api
                .move_tree_path(&ws, &tree_id, &src_path, &dst_path)
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;
            self.tree
                .write()
                .move_tree_path_node(src_ino, dst_dir, dst_name, &tree_name);
            return Ok(());
        }

        // A document file rename.
        let (tree_name, tree_id, tree_type, path, src_doc) = self
            .tree
            .read()
            .tree_file(src_ino)
            .ok_or(WriteError::NotPermitted)?;
        if src_dir != dst_dir {
            // Cross-directory move = re-tag: file the document at the
            // destination, unfile it at the source. Two small requests, no
            // bytes through the mount — the same document, shown elsewhere.
            // Works across trees too, since both halves are path-scoped.
            let (dst_tree_name, dst_tree_id, dst_tree_type, dst_path) = self
                .tree
                .read()
                .locate_tree_dir(dst_dir)
                .ok_or(WriteError::NotPermitted)?;
            if self.tree.read().lookup(dst_dir, dst_name).is_some() {
                return Err(WriteError::Exists);
            }

            // Link first: a failure between the two halves leaves the document
            // findable in both places rather than in neither.
            self.api
                .link_tree_document(&ws, &dst_tree_id, &dst_tree_type, &dst_path, &[src_doc])
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;
            if dst_name != src_name {
                self.ws_set_filename(
                    &ws,
                    &dst_tree_id,
                    &dst_tree_type,
                    &dst_path,
                    src_doc,
                    dst_name,
                )?;
            }
            // The source unlink never trashes: the document is already filed at
            // the destination, so it is not orphaned.
            self.api
                .remove_tree_document(&ws, &tree_id, &tree_type, &path, &[src_doc], false)
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;

            self.tree.write().move_tree_file(
                src_ino,
                dst_dir,
                dst_name,
                &tree_name,
                &dst_tree_name,
            );
            return Ok(());
        }
        let dst_ino = self.tree.read().lookup(dst_dir, dst_name).map(|n| n.ino);

        match dst_ino {
            None => {
                // Plain rename: pin the new filename so the view round-trips.
                self.ws_set_filename(&ws, &tree_id, &tree_type, &path, src_doc, dst_name)?;
                self.tree.write().rename_entry(src_ino, dst_name);
                Ok(())
            }
            Some(dst_ino) => {
                // Overwrite-rename (atomic save): copy src content into dst's
                // doc, detach src. POSIX requires the dst NAME to carry the SRC
                // inode afterwards, so the surviving node lives at src_ino.
                let (_n, _i, _t, _p, dst_doc) = self
                    .tree
                    .read()
                    .tree_file(dst_ino)
                    .ok_or(WriteError::NotPermitted)?;
                let content = match self.tree.read().get(src_ino).map(|n| n.content.clone()) {
                    Some(crate::state::NodeContent::Inline(b)) => b.as_ref().clone(),
                    _ => return Err(WriteError::NotPermitted),
                };
                self.flush_ws_update(&tree_name, &tree_id, &tree_type, &path, dst_doc, &content)?;
                self.ws_set_filename(&ws, &tree_id, &tree_type, &path, dst_doc, dst_name)?;
                // Not a user-initiated delete: the source document was just
                // superseded by the atomic-save copy, so it must not land in
                // the trash as if someone had removed it.
                self.api
                    .remove_tree_document(&ws, &tree_id, &tree_type, &path, &[src_doc], false)
                    .map_err(|e| WriteError::Io(format!("{e:#}")))?;
                {
                    let mut tree = self.tree.write();
                    tree.remove_tree_file(dst_ino);
                    tree.rename_entry(src_ino, dst_name);
                    tree.rebind_tree_file(src_ino, dst_doc);
                    tree.set_inline_content(src_ino, Arc::new(content));
                }
                let mut inner = self.inner.lock();
                if let Some(state) = inner.states.get_mut(&src_ino) {
                    state.target = FlushTarget::WsExisting {
                        tree_name,
                        tree_id,
                        tree_type,
                        path,
                        doc_id: dst_doc,
                    };
                }
                Ok(())
            }
        }
    }

    /// GET-merge-PUT just the `data.filename` of a workspace document.
    fn ws_set_filename(
        &self,
        ws: &str,
        tree_id: &str,
        tree_type: &str,
        path: &str,
        doc_id: u64,
        filename: &str,
    ) -> WResult<()> {
        let existing = self
            .api
            .get_workspace_document(ws, doc_id)
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        let schema = existing
            .get("schema")
            .and_then(Value::as_str)
            .unwrap_or("data/schema/note")
            .to_string();
        let mut data = existing.get("data").cloned().unwrap_or_else(|| json!({}));
        data["filename"] = json!(filename);
        self.api
            .update_workspace_documents(
                ws,
                tree_id,
                tree_type,
                path,
                vec![json!({ "id": doc_id, "schema": schema, "data": data })],
            )
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        Ok(())
    }

    fn flush_ws_update(
        &self,
        tree_name: &str,
        tree_id: &str,
        tree_type: &str,
        path: &str,
        doc_id: u64,
        buffer: &[u8],
    ) -> WResult<()> {
        let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
        let existing = self
            .api
            .get_workspace_document(&ws, doc_id)
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        let schema = existing
            .get("schema")
            .and_then(Value::as_str)
            .unwrap_or("data/schema/note")
            .to_string();
        // Editing never changes what a document IS: a note stays a note, and a
        // FILE's bytes go to the blob store rather than into `data`.
        if schema == "data/schema/file" {
            let blob = self
                .api
                .upload_blob(&ws, buffer.to_vec())
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;
            let name = existing
                .get("locations")
                .and_then(Value::as_array)
                .and_then(|l| l.first())
                .and_then(|loc| loc.get("metadata"))
                .and_then(|m| m.get("filename"))
                .and_then(Value::as_str)
                .unwrap_or("file")
                .to_string();
            let mut doc = build_file_document(&name, &blob);
            doc["id"] = json!(doc_id);
            self.api
                .update_workspace_documents(&ws, tree_id, tree_type, path, vec![doc])
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        } else {
            let mut data = existing.get("data").cloned().unwrap_or_else(|| json!({}));
            apply_buffer_to_data(&schema, &mut data, buffer);
            self.api
                .update_workspace_documents(
                    &ws,
                    tree_id,
                    tree_type,
                    path,
                    vec![json!({ "id": doc_id, "schema": schema, "data": data })],
                )
                .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        }

        let ino = self.tree.read().ws_ino_for_doc(tree_name, path, doc_id);
        if let Some(ino) = ino {
            self.tree
                .write()
                .set_inline_content(ino, Arc::new(buffer.to_vec()));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn flush_ws_create(
        &self,
        tree_name: &str,
        tree_id: &str,
        tree_type: &str,
        path: &str,
        dir_ino: u64,
        name: &str,
        buffer: &[u8],
        ino: u64,
    ) -> WResult<u64> {
        let ws = self.tree.read().ws_id().ok_or(WriteError::NotPermitted)?;
        // A canvas-native name builds its abstraction; anything else is a file,
        // so its bytes go to the blob store first.
        let doc = match build_ws_document(name, buffer) {
            Some(doc) => doc,
            None => {
                let blob = self
                    .api
                    .upload_blob(&ws, buffer.to_vec())
                    .map_err(|e| WriteError::Io(format!("{e:#}")))?;
                build_file_document(name, &blob)
            }
        };
        let ids = self
            .api
            .put_tree_document(&ws, tree_id, tree_type, path, doc)
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        let doc_id = *ids
            .first()
            .ok_or_else(|| WriteError::Io("create returned no document id".to_string()))?;
        self.tree.write().adopt_tree_file(
            dir_ino,
            name,
            tree_name,
            path,
            doc_id,
            ino,
            Arc::new(buffer.to_vec()),
        );
        Ok(doc_id)
    }

    // ── server I/O ───────────────────────────────────────────────────────────

    /// Update a document's data from an edited buffer. synapsd mints a new
    /// doc id when the content checksum changes (the old id remains as a
    /// version in the DB), so on id change: rebind the ino, pin the filename
    /// to the new id, and detach the superseded version from the context —
    /// the view always shows exactly the latest. Returns the effective id.
    // Apply an edited buffer to an existing document. synapsd preserves the
    // doc id across content edits (the id is the stable bitmap key), so this is
    // a plain read-merge-write: GET to keep fields the buffer doesn't carry,
    // merge the buffer into data, PUT under the same id.
    fn flush_update(&self, ctx: &str, doc_id: u64, buffer: &[u8]) -> WResult<()> {
        let existing = self
            .api
            .get_document(ctx, doc_id)
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        let schema = existing
            .get("schema")
            .and_then(Value::as_str)
            .unwrap_or("data/schema/note")
            .to_string();
        let mut data = existing.get("data").cloned().unwrap_or_else(|| json!({}));
        apply_buffer_to_data(&schema, &mut data, buffer);

        self.api
            .update_documents(
                ctx,
                vec![json!({ "id": doc_id, "schema": schema, "data": data })],
            )
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;

        // Local truth immediately; the ws refresh will confirm. (Resolve the ino
        // under the read lock, then drop it before taking the write lock — the
        // single FUSE thread would deadlock holding both.)
        let ino = self.tree.read().ino_for_doc(ctx, doc_id);
        if let Some(ino) = ino {
            self.tree
                .write()
                .set_inline_content(ino, Arc::new(buffer.to_vec()));
        }
        Ok(())
    }

    fn flush_create(
        &self,
        ctx: &str,
        dir: &str,
        dir_ino: u64,
        name: &str,
        buffer: &[u8],
        ino: u64,
    ) -> WResult<u64> {
        // A canvas-native name builds its abstraction; anything else is a file,
        // so its bytes go to the blob store first. Same rule as a tree write and
        // as the server's inferDocFromFile — the gesture has to mean one thing.
        let doc = match build_ws_document(name, buffer) {
            Some(doc) => doc,
            None => {
                let blob = self
                    .api
                    .upload_context_blob(ctx, buffer.to_vec())
                    .map_err(|e| WriteError::Io(format!("{e:#}")))?;
                build_file_document(name, &blob)
            }
        };
        let ids = self
            .api
            .create_documents(ctx, vec![doc])
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        let doc_id = *ids
            .first()
            .ok_or_else(|| WriteError::Io("create returned no document id".to_string()))?;

        // Pin the exact filename so the server-driven view keeps it verbatim
        // (slug(title) may differ from what the editor named the file)
        self.names
            .put(ctx, dir, doc_id, name)
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;

        self.tree.write().adopt_document(
            dir_ino,
            name,
            ctx,
            doc_id,
            ino,
            Arc::new(buffer.to_vec()),
        );
        Ok(doc_id)
    }
}

/// Apply an edited buffer to a document's data, in the document's OWN schema.
///
/// Editing through a mount must never change what a document IS: a note that
/// already exists stays a note when you save over it, and a `.todo.json` is
/// read back as the JSON it was rendered as. Mirrors `applyBodyToDoc()` in the
/// server's transports/webdav/vfs-shared.js — the two wires must not disagree
/// about what a save means.
fn apply_buffer_to_data(schema: &str, data: &mut Value, buffer: &[u8]) {
    let text = String::from_utf8_lossy(buffer);
    match schema {
        "data/schema/task" => {
            // The rendered body IS the document's data as JSON; merge it back
            // key by key so fields the editor never saw survive the save.
            if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&text) {
                if let Some(obj) = data.as_object_mut() {
                    obj.extend(parsed);
                }
            }
        }
        "data/schema/tab" | "data/schema/link" => {
            if let Some(url) = extract_url(&text) {
                data["url"] = json!(url);
            }
        }
        _ => {
            // Notes (and default): content is the file. Optimistically derive
            // the title from a markdown H1 when present (client-side policy);
            // otherwise leave the existing title for the server to keep/default.
            data["content"] = json!(text);
            if let Some(heading) = first_markdown_h1(&text) {
                data["title"] = json!(heading);
            }
        }
    }
}

/// Join a normalized parent path with a child name ("/" + "x" -> "/x").
fn join_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// Parent of a normalized path ("/a/b" -> "/a", "/a" -> "/").
fn parent_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((head, _)) if !head.is_empty() => head.to_string(),
        _ => "/".to_string(),
    }
}

/// Schema a writable filename maps to in a workspace tree, or None if the
/// extension isn't one we materialize as an editable document.
/// Which schema a NEW file implies, or None when it is just a file.
///
/// `.todo.json` and `.url` keep a canvas meaning because they are not general
/// formats — a browser emits `.url` when you drag a link out of the address
/// bar, and `.todo.json` only ever comes from our own renderer. **`.md` does
/// not**: markdown is a general document format, so a new `.md` is a FILE.
/// Rendering markdown as a note is a UI decision, not a storage one. Same rule
/// as the server's `inferDocFromFile()`; the two wires must not disagree about
/// what a write means.
fn ws_doc_schema(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    if lower.ends_with(crate::render::NOTE_EXT) {
        Some("data/schema/note")
    } else if lower.ends_with(".todo.json") {
        Some("data/schema/task")
    } else if lower.ends_with(".url") {
        Some("data/schema/tab")
    } else {
        None
    }
}

/// Build a new workspace document from a filename + body. `data.filename` pins
/// the on-disk name so re-saves round-trip to the same document.
/// A File document for already-uploaded bytes. `data` stays empty (the server's
/// core/File.js reserves it for JSON docs); the name rides on the location.
fn build_file_document(name: &str, blob: &crate::api::BlobRef) -> Value {
    let mut doc = json!({
        "schema": "data/schema/file",
        "data": {},
        "locations": [{ "url": blob.url, "metadata": { "filename": name } }],
        "metadata": { "size": blob.size },
    });
    if let Some(checksum) = &blob.checksum {
        doc["checksumArray"] = json!([format!("sha256/{checksum}")]);
    }
    if let Some(mime) = &blob.mime_type {
        doc["metadata"]["contentType"] = json!(mime);
    }
    doc
}

fn build_ws_document(name: &str, buffer: &[u8]) -> Option<Value> {
    let schema = ws_doc_schema(name)?;
    let text = String::from_utf8_lossy(buffer);
    match schema {
        "data/schema/task" => {
            // `.todo.json` only ever comes from our own renderer, and what it
            // renders is the document's data as JSON — so that is how it reads
            // back. A body that is not an object contributes nothing but the
            // title, rather than being guessed at.
            let stem = name.strip_suffix(".todo.json").unwrap_or(name);
            let mut data = json!({ "title": stem, "completed": false, "filename": name });
            if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&text) {
                if let Some(obj) = data.as_object_mut() {
                    obj.extend(parsed);
                }
            }
            Some(json!({ "schema": schema, "data": data }))
        }
        "data/schema/tab" => {
            let stem = name.strip_suffix(".url").unwrap_or(name);
            let url = extract_url(&text)?;
            Some(json!({
                "schema": schema,
                "data": { "title": stem, "url": url, "filename": name }
            }))
        }
        _ => {
            let stem = name.strip_suffix(crate::render::NOTE_EXT).unwrap_or(name);
            let title = first_markdown_h1(&text).unwrap_or_else(|| stem.to_string());
            Some(json!({
                "schema": schema,
                "data": { "title": title, "content": text, "filename": name }
            }))
        }
    }
}

/// Extract a URL from a plain line or a Windows [InternetShortcut] body.
fn extract_url(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.split_whitespace().next().map(str::to_string);
    }
    for line in trimmed.lines() {
        if let Some(rest) = line.trim().strip_prefix("URL=") {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// First ATX H1 ("# Title") in a markdown body, scanning the whole document and
/// skipping fenced code blocks. `None` if absent. Title-from-heading is client
/// policy — the server only guarantees a date-stamped title when none is given.
fn first_markdown_h1(content: &str) -> Option<String> {
    let mut in_fence = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            let title = rest.trim().trim_end_matches('#').trim();
            if !title.is_empty() {
                return Some(title.chars().take(200).collect());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{build_ws_document, first_markdown_h1};

    #[test]
    fn h1_title_derivation() {
        assert_eq!(first_markdown_h1("plain body, no heading"), None);
        assert_eq!(
            first_markdown_h1("# Meeting Notes\n\nbody").as_deref(),
            Some("Meeting Notes")
        );
        // first H1 anywhere; trailing # stripped
        assert_eq!(
            first_markdown_h1("intro\n\n# Real Title ##\n").as_deref(),
            Some("Real Title")
        );
        // fenced code # ignored
        assert_eq!(
            first_markdown_h1("```\n# not a title\n```\n# Actual\n").as_deref(),
            Some("Actual")
        );
        // ## (H2) is not a title
        assert_eq!(first_markdown_h1("## subheading only\n"), None);
    }

    #[test]
    fn canvas_native_names_build_their_abstraction() {
        let tab = build_ws_document(
            "reddit.url",
            b"[InternetShortcut]\nURL=https://reddit.com\n",
        )
        .expect("a .url is a tab");
        assert_eq!(tab["schema"], "data/schema/tab");
        assert_eq!(tab["data"]["url"], "https://reddit.com");
        assert_eq!(tab["data"]["filename"], "reddit.url");

        let todo = build_ws_document("ship.todo.json", br#"{"completed": true}"#)
            .expect("a .todo.json is a task");
        assert_eq!(todo["schema"], "data/schema/task");
        assert_eq!(todo["data"]["completed"], true);
        assert_eq!(todo["data"]["title"], "ship");

        // Everything else — markdown included — is a file, and the caller
        // stores its bytes. Markdown is a general format, not a canvas one.
        let note = build_ws_document("Ideas.note.md", b"# Real Title\n\nbody\n")
            .expect("a .note.md is a note");
        assert_eq!(note["schema"], "data/schema/note");
        assert_eq!(note["data"]["title"], "Real Title");
        assert_eq!(note["data"]["filename"], "Ideas.note.md");

        // A bare .md is markdown, which is a general format — so it is a file,
        // and the caller stores its bytes.
        assert!(build_ws_document("thoughts.md", b"# Real Title\n\nbody\n").is_none());
        assert!(build_ws_document("photo.jpg", b"\xff\xd8\xff").is_none());
    }
}
