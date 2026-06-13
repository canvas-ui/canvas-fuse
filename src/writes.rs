use crate::api::ApiClient;
use crate::names::NameStore;
use crate::state::Tree;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

/// Schema dirs that accept writes. Notes and Todos map to mutable JSON docs
/// (identity = doc id), so edits are plain updates — no versioning needed.
pub const WRITABLE_DIRS: &[&str] = &["Notes", "Todos"];

/// Overlay inodes live far above anything Tree allocates.
const FIRST_OVERLAY_INO: u64 = 1 << 48;

/// What a dirty buffer flushes to.
#[derive(Debug, Clone)]
enum FlushTarget {
    /// Update an existing document (node lives in the tree)
    Existing {
        ctx: String,
        dir: String,
        doc_id: u64,
    },
    /// Create a new document on first flush
    Create {
        ctx: String,
        dir: String,
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
    /// doc ids created by this mount — safe to destroy when an editor's
    /// atomic-rename save pattern replaces them
    own_docs: HashSet<u64>,
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
    CrossDir,
    Io(String),
}

impl WriteError {
    pub fn errno(&self) -> i32 {
        match self {
            WriteError::NotPermitted => libc::EACCES,
            WriteError::NotFound => libc::ENOENT,
            WriteError::Exists => libc::EEXIST,
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
                own_docs: HashSet::new(),
            }),
            sync: Arc::new(Mutex::new(())),
        }
    }

    /// Shared handle the refresh worker holds across its fetch+apply cycle.
    pub fn sync_handle(&self) -> Arc<Mutex<()>> {
        self.sync.clone()
    }

    fn writable_dir(&self, dir_ino: u64) -> Option<(String, String)> {
        let (ctx, dir) = self.tree.read().locate_schema_dir(dir_ino)?;
        WRITABLE_DIRS.contains(&dir.as_str()).then_some((ctx, dir))
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
    pub fn open_existing(&self, ino: u64, truncate: bool) -> WResult<()> {
        let (parent, content) = {
            let tree = self.tree.read();
            let node = tree.get(ino).ok_or(WriteError::NotFound)?;
            if node.is_dir() {
                return Err(WriteError::NotPermitted);
            }
            let content = match &node.content {
                crate::state::NodeContent::Inline(bytes) => bytes.as_ref().clone(),
                _ => return Err(WriteError::NotPermitted), // blobs are read-only
            };
            (node.parent, content)
        };
        let (ctx, dir) = self.writable_dir(parent).ok_or(WriteError::NotPermitted)?;
        let (_, doc_id) = self
            .tree
            .read()
            .doc_for_ino(ino)
            .ok_or(WriteError::NotPermitted)?; // .context.json etc. have no doc

        let mut inner = self.inner.lock();
        let state = inner.states.entry(ino).or_insert_with(|| OpenWrite {
            buffer: content,
            dirty: false,
            refs: 0,
            target: FlushTarget::Existing { ctx, dir, doc_id },
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
        let (ctx, dir) = self.writable_dir(dir_ino).ok_or(WriteError::NotPermitted)?;
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
                target: FlushTarget::Create {
                    ctx,
                    dir,
                    dir_ino,
                    name: name.to_string(),
                },
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
        let (buffer, target) = {
            let mut inner = self.inner.lock();
            let state = match inner.states.get_mut(&ino) {
                Some(s) if s.dirty => s,
                _ => return Ok(()),
            };
            // Shells flush right after open(O_CREAT), before any write lands.
            // Creating an empty doc just to supersede it on close is churn —
            // defer empty creates to the final flush (where `touch` needs them).
            if !final_flush
                && state.buffer.is_empty()
                && matches!(state.target, FlushTarget::Create { .. })
            {
                return Ok(());
            }
            state.dirty = false;
            (state.buffer.clone(), state.target.clone())
        };

        let result = match &target {
            FlushTarget::Existing { ctx, dir, doc_id } => {
                self.flush_update(ctx, dir, *doc_id, &buffer)
            }
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
                                dir: dir.clone(),
                                doc_id,
                            };
                        }
                        inner.overlay.remove(&ino);
                        inner.overlay_names.remove(&(*dir_ino, name.clone()));
                        inner.own_docs.insert(doc_id);
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
        let (ctx, _dir) = self.writable_dir(dir_ino).ok_or(WriteError::NotPermitted)?;

        // Pending overlay file: purely local
        {
            let mut inner = self.inner.lock();
            if let Some(ino) = inner.overlay_names.remove(&(dir_ino, name.to_string())) {
                inner.overlay.remove(&ino);
                inner.states.remove(&ino);
                return Ok(());
            }
        }

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

        let own = self.inner.lock().own_docs.contains(&doc_id);
        let res = if own {
            self.api.delete_documents(&ctx, &[doc_id])
        } else {
            // Organizational removal: detach from this context, never destroy
            self.api.remove_documents(&ctx, &[doc_id])
        };
        res.map_err(|e| WriteError::Io(format!("{e:#}")))?;

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
        if src_dir != dst_dir {
            // EXDEV makes `mv` fall back to copy+unlink, which composes from
            // primitives we already support
            return Err(WriteError::CrossDir);
        }
        let _sync = self.sync.lock();
        let (ctx, dir) = self.writable_dir(src_dir).ok_or(WriteError::NotPermitted)?;

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
                        state.target = FlushTarget::Existing { ctx, dir, doc_id };
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
                // dst keeps its (stable) id, gains src's content.
                self.flush_update(&ctx, &dir, dst_doc, &content)?;

                let own = self.inner.lock().own_docs.contains(&src_doc);
                let res = if own {
                    self.api.delete_documents(&ctx, &[src_doc])
                } else {
                    self.api.remove_documents(&ctx, &[src_doc])
                };
                res.map_err(|e| WriteError::Io(format!("{e:#}")))?;

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
                        dir: dir.clone(),
                        doc_id: dst_doc,
                    };
                }
                Ok(())
            }
        }
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
    fn flush_update(&self, ctx: &str, dir: &str, doc_id: u64, buffer: &[u8]) -> WResult<()> {
        let existing = self
            .api
            .get_document(ctx, doc_id)
            .map_err(|e| WriteError::Io(format!("{e:#}")))?;
        let schema = existing
            .get("schema")
            .and_then(Value::as_str)
            .unwrap_or("data/abstraction/note")
            .to_string();
        let mut data = existing.get("data").cloned().unwrap_or_else(|| json!({}));
        apply_buffer_to_data(dir, &mut data, buffer);

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
        let doc = build_new_document(dir, name, buffer);
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

/// Map an edited buffer back onto the document's data per schema dir.
fn apply_buffer_to_data(dir: &str, data: &mut Value, buffer: &[u8]) {
    let text = String::from_utf8_lossy(buffer);
    match dir {
        "Todos" => {
            let (title, done, description) = parse_todo_markdown(&text);
            data["title"] = json!(title);
            data["completed"] = json!(done);
            match description {
                Some(d) => data["description"] = json!(d),
                None => {
                    if let Some(obj) = data.as_object_mut() {
                        obj.remove("description");
                    }
                }
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

fn build_new_document(dir: &str, name: &str, buffer: &[u8]) -> Value {
    let text = String::from_utf8_lossy(buffer);
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    match dir {
        "Todos" => {
            let (mut title, done, description) = parse_todo_markdown(&text);
            if title.is_empty() {
                title = stem.to_string();
            }
            let mut data = json!({ "title": title, "completed": done });
            if let Some(d) = description {
                data["description"] = json!(d);
            }
            json!({ "schema": "data/abstraction/todo", "data": data })
        }
        _ => {
            // A markdown H1 wins; else fall back to the filename stem.
            let title = first_markdown_h1(&text).unwrap_or_else(|| stem.to_string());
            json!({
                "schema": "data/abstraction/note",
                "data": { "title": title, "content": text }
            })
        }
    }
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

/// Inverse of render_todo: `- [x] title` + optional description body.
pub fn parse_todo_markdown(text: &str) -> (String, bool, Option<String>) {
    let mut lines = text.lines();
    let mut title = String::new();
    let mut done = false;
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- [") {
            if let Some((mark, t)) = rest.split_once(']') {
                done = mark.trim().eq_ignore_ascii_case("x");
                title = t.trim().to_string();
                break;
            }
        }
        title = trimmed.to_string();
        break;
    }
    let description: String = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    (
        title,
        done,
        (!description.is_empty()).then_some(description),
    )
}

#[cfg(test)]
mod tests {
    use super::{first_markdown_h1, parse_todo_markdown};

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
    fn todo_roundtrip() {
        let (t, d, desc) = parse_todo_markdown("- [x] Ship MVP\n\nBefore end of month\n");
        assert_eq!(t, "Ship MVP");
        assert!(d);
        assert_eq!(desc.as_deref(), Some("Before end of month"));

        let (t, d, desc) = parse_todo_markdown("- [ ] Open task\n");
        assert_eq!(t, "Open task");
        assert!(!d);
        assert!(desc.is_none());

        // Plain text without checkbox: first line becomes the title
        let (t, d, _) = parse_todo_markdown("just a line\n");
        assert_eq!(t, "just a line");
        assert!(!d);
    }
}
