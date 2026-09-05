use crate::api::{ContextInfo, Document, HomeEntry, TreeInfo};
use crate::names::NameStore;
use crate::render;

/// The derived, read-only grouping inside a context folder.
pub const BY_SCHEMA_DIR: &str = ".by-schema";

/// Sticky-name key for a context whose URL is not known yet. Normally the
/// context's URL fills this slot (see `Tree::ctx_urls`); there are no schema
/// dirs to key by anymore, and the store's shape is unchanged.
pub const FLAT_NAME_KEY: &str = "";
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

/// The order filenames are handed out in.
///
/// A folder in a context tree lists everything filed at OR BELOW its path — a
/// context path is AND(layers along the path) — so three documents called
/// `CLAUDE.md`, filed at `/`, `/dc-migration` and `/dc-migration/tasks/foo`,
/// are all listed at `/`. Only one of them is filed at the folder you are
/// standing in, and that one keeps the plain name; the rest take the `_<id>`
/// suffix (see `render::with_id_suffix`).
///
/// Placement first, then id: the id is only the tie-break among documents of
/// equal standing, and on its own it made the name `CLAUDE.md` mean whichever
/// one happened to be created first — a different document at every path, and
/// a different one again after a re-file. `linked_here` comes from the server,
/// which is the only party that knows the tree (see `linkedHere` in the REST
/// listings); when it says nothing every document counts as filed here and
/// this is exactly the old id order.
///
/// The server's own views (WebDAV) name documents by the same rule, because a
/// document opened over two wires has to be one file with one name.
fn by_placement(docs: &[Document]) -> Vec<&Document> {
    let mut sorted: Vec<&Document> = docs.iter().collect();
    sorted.sort_by_key(|d| (!d.linked_here, d.id));
    sorted
}

/// What `.by-schema/` would contain, as one comparable value: every folder and
/// the names in it. A content edit does not move a document between folders, so
/// it does not belong in the signature — and re-materializing the grouping on
/// every keystroke would churn inodes nothing else needs churned.
fn group_signature(grouped: &BTreeMap<String, Vec<(String, NodeContent, SystemTime)>>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (dir, entries) in grouped {
        dir.hash(&mut hasher);
        for (name, _, _) in entries {
            name.hash(&mut hasher);
        }
    }
    hasher.finish()
}

pub const ROOT_INO: u64 = 1;
pub const CONTEXTS_INO: u64 = 2;
/// Workspace mounts group the trees under `Trees/` and expose the workspace
/// trash as its own root, so one mount has the same shape as the WebDAV view
/// (see docs/data-representation.md).
pub const TREES_INO: u64 = 3;
pub const TRASH_INO: u64 = 4;
pub const HOME_INO: u64 = 5;
/// The trash is physically a path in the default directory tree; a file node
/// under `Trash/` records that origin so the write path can address it.
pub const TRASH_TREE_NAME: &str = "directory";
pub const TRASH_PATH: &str = "/.trash";
const FIRST_DYNAMIC_INO: u64 = 16;

pub const CONTEXT_META_FILE: &str = ".context.json";

/// What a node serves on read. Remote content is fetched lazily through the
/// workspace content route and cached by checksum in the blob cache.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeContent {
    Dir,
    Inline(Arc<Vec<u8>>),
    /// A folder of the home drive. `loaded` flips once its listing has been
    /// fetched: home is a real filesystem and can be huge, so directories are
    /// materialized on first look rather than walked at mount.
    HomeDir {
        path: String,
        loaded: bool,
    },
    /// A real file on the home drive, read by byte window on demand.
    HomeFile {
        path: String,
        size: u64,
    },
    Remote {
        workspace_id: String,
        doc_id: u64,
        /// None until resolved from the blob (doc carried no metadata.size).
        size: Option<u64>,
        checksum: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Node {
    pub ino: u64,
    pub parent: u64,
    pub name: String,
    pub mtime: SystemTime,
    pub content: NodeContent,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        matches!(self.content, NodeContent::Dir | NodeContent::HomeDir { .. })
    }

    pub fn size(&self) -> u64 {
        match &self.content {
            NodeContent::Dir => 0,
            NodeContent::Inline(bytes) => bytes.len() as u64,
            // None (unresolved) reports 0 here; fsimpl resolves it lazily via
            // the blob store before answering getattr.
            NodeContent::Remote { size, .. } => size.unwrap_or(0),
            NodeContent::HomeDir { .. } => 0,
            NodeContent::HomeFile { size, .. } => *size,
        }
    }
}

/// Kernel-facing invalidations produced by a view update. Applied by the
/// worker via fuser's Notifier after the tree lock is released.
#[derive(Debug, Default)]
pub struct Invalidation {
    /// (parent ino, child ino, name) — emits inotify IN_DELETE via notify_delete
    pub removed: Vec<(u64, u64, String)>,
    /// File inodes whose rendered content changed — data cache must be dropped
    pub changed: Vec<u64>,
    /// Directory inodes whose listing changed — readdir cache must be dropped
    pub dirty_dirs: Vec<u64>,
    /// File inodes that just appeared in the view. The daemon materializing a
    /// file emits no fsnotify event of any kind, and a watcher that handles
    /// events per FILE (rather than re-listing the directory on any signal)
    /// therefore never learns it exists — see nudge.rs.
    pub added: Vec<u64>,
    /// (parent ino, child ino, name) — document files that LEFT the view and
    /// are held as tombstones: still in the tree, so the nudge thread can
    /// unlink them for real and the kernel emits an `IN_DELETE` that names
    /// them. `notify_delete` emits no fsnotify event, so without this a
    /// per-file watcher keeps every departed document as a ghost.
    pub vanished: Vec<(u64, u64, String)>,
}

impl Invalidation {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty()
            && self.changed.is_empty()
            && self.dirty_dirs.is_empty()
            && self.added.is_empty()
            && self.vanished.is_empty()
    }
}

/// A workspace tree (context- or directory-type) as mounted: its root dir ino
/// and the metadata needed to address its REST routes.
#[derive(Debug, Clone)]
struct WsTree {
    id: String,
    /// "context" | "directory"
    tree_type: String,
    root_ino: u64,
}

/// A document materialized as a file in a workspace tree path. Carries exactly
/// what the write path needs to update/unlink it (which tree, which path).
#[derive(Debug, Clone)]
pub struct WsFile {
    pub tree_name: String,
    pub path: String,
    pub doc_id: u64,
}

/// Workspace-mount state. Present only when the mount roots a workspace
/// (`-w`); the context-mode maps above stay empty in that case and vice versa.
struct WsState {
    ws_id: String,
    ws_name: String,
    /// tree name -> mounted tree
    trees: HashMap<String, WsTree>,
    /// (tree name, normalized path) -> directory ino. Path "/" maps to the
    /// tree's root_ino.
    path_inos: HashMap<(String, String), u64>,
    /// file ino -> which (tree, path, doc) it materializes
    file_docs: HashMap<u64, WsFile>,
}

pub struct Tree {
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, BTreeMap<String, u64>>,
    next_ino: u64,
    /// (context id, doc id) -> ino. Keeps doc inodes stable across context URL
    /// switches so open file handles survive a view swap.
    doc_inos: HashMap<(String, u64), u64>,
    ctx_inos: HashMap<String, u64>,
    /// Document files that have left the view but are still in the tree, so a
    /// real `unlink()` through the mount can name them (see Invalidation).
    /// Cleared when that unlink lands; a missed one is retried on the next
    /// refresh, since the document stays absent from the desired set.
    tombstones: HashSet<u64>,
    /// Whether departed documents are held for a real unlink (see tombstone).
    /// Off unless something is there to make that call — a bare tree removes
    /// entries outright, so the view is never left waiting on an actor that
    /// does not exist.
    defer_removals: bool,
    /// context id -> signature of the last `.by-schema/` grouping materialized
    /// for it (see `group_signature`).
    by_schema_sigs: HashMap<String, u64>,
    /// context id -> workspaceId, needed to address the content route
    ctx_workspaces: HashMap<String, String>,
    /// context id -> its current URL. Part of the sticky-name key: a context is
    /// a POINTER, and re-aiming it from `mbag://` to `mbag://dc-migration`
    /// changes which document is filed at the path and therefore which one owns
    /// a name (see `by_placement`). Keyed by context alone, the assignment made
    /// at the old URL followed the context to the new one and pinned the plain
    /// name to a document that no longer had any claim on it.
    ctx_urls: HashMap<String, String>,
    /// When set, the mount is rooted at a single context: that context's schema
    /// dirs hang directly off ROOT (no `Contexts/<id>` wrapper), so mounting
    /// `-c mbag <path>` yields `<path>/mbag/{Notes,Tabs,…}`. None = global mount
    /// (root holds `Contexts/`, and later `Workspaces/`).
    context_root: Option<String>,
    /// Set when the mount roots a workspace tree view (`-w`). Mutually
    /// exclusive with the context maps above.
    ws: Option<WsState>,
    /// Home is fed from the mirror store (`--mirror`): directories are never
    /// listed from the server, so every HomeDir is born `loaded`.
    home_mirrored: bool,
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    /// Global mount: root holds the `Contexts/` directory.
    pub fn new() -> Self {
        let mut t = Self::bare(None);
        let now = SystemTime::now();
        t.insert_node(Node {
            ino: CONTEXTS_INO,
            parent: ROOT_INO,
            name: "Contexts".to_string(),
            mtime: now,
            content: NodeContent::Dir,
        });
        t
    }

    /// Single-context mount: the context's schema dirs are materialized directly
    /// under ROOT (no `Contexts/` wrapper, no per-context dir).
    pub fn context_rooted(ctx_id: String) -> Self {
        Self::bare(Some(ctx_id))
    }

    /// Workspace mount. ROOT holds the same roots the WebDAV view exposes:
    /// `Trees/` (one directory per tree, mirroring its path hierarchy with
    /// documents as files) and `Trash/` (flat; what a delete parked there).
    /// `Home/` is not served here yet — see the README.
    pub fn workspace_rooted(ws_id: String, ws_name: String) -> Self {
        let mut t = Self::bare(None);
        let now = SystemTime::now();
        t.insert_node(Node {
            ino: TREES_INO,
            parent: ROOT_INO,
            name: "Trees".to_string(),
            mtime: now,
            content: NodeContent::Dir,
        });
        t.insert_node(Node {
            ino: TRASH_INO,
            parent: ROOT_INO,
            name: "Trash".to_string(),
            mtime: now,
            content: NodeContent::Dir,
        });
        t.insert_node(Node {
            ino: HOME_INO,
            parent: ROOT_INO,
            name: "Home".to_string(),
            mtime: now,
            content: NodeContent::HomeDir {
                path: "/".to_string(),
                loaded: false,
            },
        });
        t.ws = Some(WsState {
            ws_id,
            ws_name,
            trees: HashMap::new(),
            path_inos: HashMap::new(),
            file_docs: HashMap::new(),
        });
        t
    }

    pub fn is_workspace(&self) -> bool {
        self.ws.is_some()
    }

    pub fn is_home_mirrored(&self) -> bool {
        self.home_mirrored
    }

    fn bare(context_root: Option<String>) -> Self {
        let mut t = Self {
            nodes: HashMap::new(),
            children: HashMap::new(),
            next_ino: FIRST_DYNAMIC_INO,
            doc_inos: HashMap::new(),
            ctx_inos: HashMap::new(),
            tombstones: HashSet::new(),
            defer_removals: false,
            by_schema_sigs: HashMap::new(),
            ctx_workspaces: HashMap::new(),
            ctx_urls: HashMap::new(),
            context_root,
            ws: None,
            home_mirrored: false,
        };
        t.insert_node(Node {
            ino: ROOT_INO,
            parent: ROOT_INO,
            name: String::new(),
            mtime: SystemTime::now(),
            content: NodeContent::Dir,
        });
        t
    }

    /// The directory whose listing changes when contexts appear/disappear:
    /// ROOT in context-rooted mode, the `Contexts/` dir in global mode.
    fn contexts_parent(&self) -> u64 {
        if self.context_root.is_some() {
            ROOT_INO
        } else {
            CONTEXTS_INO
        }
    }

    fn alloc_ino(&mut self) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn insert_node(&mut self, node: Node) {
        let parent = node.parent;
        if node.ino != ROOT_INO {
            self.children
                .entry(parent)
                .or_default()
                .insert(node.name.clone(), node.ino);
        }
        self.children.entry(node.ino).or_default();
        self.nodes.insert(node.ino, node);
        self.touch_dir(parent);
    }

    pub fn remove_node(&mut self, ino: u64) -> Option<Node> {
        let node = self.nodes.remove(&ino)?;
        if let Some(siblings) = self.children.get_mut(&node.parent) {
            siblings.remove(&node.name);
        }
        self.children.remove(&ino);
        self.touch_dir(node.parent);
        Some(node)
    }

    /// Hold departed documents for a real unlink by the nudge thread. Set once
    /// at mount, when there is a nudge thread to do the collecting.
    pub fn set_deferred_removals(&mut self, on: bool) {
        self.defer_removals = on;
    }

    /// Hold a departed document file in the tree so the nudge thread can unlink
    /// it for real, which is the only way the kernel emits an `IN_DELETE` that
    /// names it. Listings show it for the few milliseconds until that lands.
    fn tombstone(&mut self, ino: u64) {
        self.tombstones.insert(ino);
    }

    /// Resolve an unlink against the tombstone set. `Some(ino)` means "this is
    /// the nudge thread collecting a document that already left the view" — the
    /// caller drops it locally and must NOT touch the server.
    pub fn take_tombstone(&mut self, parent: u64, name: &str) -> Option<u64> {
        let ino = self.children.get(&parent)?.get(name).copied()?;
        self.tombstones.remove(&ino).then_some(ino)
    }

    /// Drop a tombstoned node and its bookkeeping. Local only.
    pub fn drop_tombstoned(&mut self, ino: u64) {
        self.tombstones.remove(&ino);
        self.remove_node(ino);
        self.doc_inos.retain(|_, &mut i| i != ino);
        if let Some(w) = self.ws.as_mut() {
            w.file_docs.remove(&ino);
        }
    }

    /// A directory's mtime is when its ENTRIES last changed — POSIX updates it
    /// on every link and unlink, and this view had never done so: a context
    /// could swap its whole document set and still stat identical, so anything
    /// that decides "has this folder changed?" by timestamp (file managers,
    /// backup tools, `find -newer`) concluded it had not, and kept showing the
    /// previous view until the user pressed F5.
    ///
    /// Content edits deliberately do NOT come through here — a file's own mtime
    /// is the document's updatedAt, and rewriting one changes no entry.
    fn touch_dir(&mut self, ino: u64) {
        if let Some(dir) = self.nodes.get_mut(&ino) {
            if dir.is_dir() {
                dir.mtime = SystemTime::now();
            }
        }
    }

    pub fn get(&self, ino: u64) -> Option<&Node> {
        self.nodes.get(&ino)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<&Node> {
        let ino = self.children.get(&parent)?.get(name)?;
        self.nodes.get(ino)
    }

    /// Path of an ino relative to the mount root (empty path for the root
    /// itself). Walks `parent` links; None for unknown inos or on a cycle
    /// that isn't the root's self-parent.
    pub fn path_of(&self, ino: u64) -> Option<std::path::PathBuf> {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = ino;
        for _ in 0..256 {
            if cur == ROOT_INO {
                parts.reverse();
                return Some(parts.iter().collect());
            }
            let node = self.nodes.get(&cur)?;
            parts.push(&node.name);
            cur = node.parent;
        }
        None
    }

    pub fn list(&self, ino: u64) -> Option<Vec<&Node>> {
        let children = self.children.get(&ino)?;
        Some(
            children
                .values()
                .filter_map(|i| self.nodes.get(i))
                .collect(),
        )
    }

    /// The workspace a context view's documents belong to — the scope every
    /// document id is unique within, and so part of a sticky name's key.
    pub fn workspace_of(&self, ctx_id: &str) -> Option<String> {
        self.ctx_workspaces.get(ctx_id).cloned()
    }

    pub fn context_ids(&self) -> Vec<String> {
        self.ctx_inos.keys().cloned().collect()
    }

    pub fn context_ino(&self, ctx: &str) -> Option<u64> {
        self.ctx_inos.get(ctx).copied()
    }

    /// If ino is a schema dir (e.g. Notes under a context), return
    /// (context id, dir label). Used by the write path to classify targets.
    /// If `ino` IS a context folder, the context it holds. A context is flat, so
    /// this is the one place documents are written; `.by-schema/` is derived and
    /// never a write target.
    pub fn locate_context_dir(&self, ino: u64) -> Option<String> {
        let node = self.get(ino)?;
        if !node.is_dir() {
            return None;
        }
        // Context-rooted: ROOT is the context.
        if let Some(root_ctx) = &self.context_root {
            return (ino == ROOT_INO).then(|| root_ctx.clone());
        }
        // Global: context dir -> Contexts.
        (node.parent == CONTEXTS_INO).then(|| node.name.clone())
    }

    pub fn ino_for_doc(&self, ctx: &str, doc_id: u64) -> Option<u64> {
        self.doc_inos.get(&(ctx.to_string(), doc_id)).copied()
    }

    /// Reverse of doc_inos: which (context, doc) does this ino materialize?
    pub fn doc_for_ino(&self, ino: u64) -> Option<(String, u64)> {
        self.doc_inos
            .iter()
            .find(|(_, &i)| i == ino)
            .map(|((ctx, doc), _)| (ctx.clone(), *doc))
    }

    /// Materialize a document node directly (used right after the write path
    /// creates a doc, so the file exists before the next server refresh).
    /// The caller supplies the ino to keep open kernel handles stable.
    pub fn adopt_document(
        &mut self,
        dir_ino: u64,
        name: &str,
        ctx_id: &str,
        doc_id: u64,
        ino: u64,
        content: Arc<Vec<u8>>,
    ) {
        self.doc_inos.insert((ctx_id.to_string(), doc_id), ino);
        self.insert_node(Node {
            ino,
            parent: dir_ino,
            name: name.to_string(),
            mtime: SystemTime::now(),
            content: NodeContent::Inline(content),
        });
    }

    /// Replace a file node's inline content (post-flush local truth).
    pub fn set_inline_content(&mut self, ino: u64, content: Arc<Vec<u8>>) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.content = NodeContent::Inline(content);
            node.mtime = SystemTime::now();
        }
    }

    /// Rename an entry within its directory (write-path organizational rename).
    pub fn rename_entry(&mut self, ino: u64, new_name: &str) {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return;
        };
        let parent = node.parent;
        let old = std::mem::replace(&mut node.name, new_name.to_string());
        if let Some(siblings) = self.children.get_mut(&parent) {
            siblings.remove(&old);
            siblings.insert(new_name.to_string(), ino);
        }
    }

    /// Remove a document node (write-path unlink). Cleans the doc_inos map.
    pub fn remove_doc_node(&mut self, ino: u64) {
        self.remove_node(ino);
        self.doc_inos.retain(|_, &mut i| i != ino);
    }

    pub fn bind_doc(&mut self, ctx: &str, doc_id: u64, ino: u64) {
        self.doc_inos.insert((ctx.to_string(), doc_id), ino);
    }

    pub fn unbind_doc(&mut self, ctx: &str, doc_id: u64) {
        self.doc_inos.remove(&(ctx.to_string(), doc_id));
    }

    /// Sync the set of context dirs (incl. schema-dir skeleton + .context.json).
    /// Returns invalidations and the list of newly appeared context ids.
    pub fn apply_contexts(&mut self, contexts: &[ContextInfo]) -> (Invalidation, Vec<String>) {
        let mut inv = Invalidation::default();
        let mut added = Vec::new();
        let now = SystemTime::now();

        let wanted: HashMap<&str, &ContextInfo> =
            contexts.iter().map(|c| (c.id.as_str(), c)).collect();

        // Remove contexts that disappeared
        let stale: Vec<String> = self
            .ctx_inos
            .keys()
            .filter(|id| !wanted.contains_key(id.as_str()))
            .cloned()
            .collect();
        for ctx_id in stale {
            let ino = self.ctx_inos.remove(&ctx_id).unwrap();
            self.remove_subtree(ino, &mut inv);
            if ino == ROOT_INO {
                // Context-rooted mount whose context vanished: clear the schema
                // dirs under ROOT but keep ROOT itself (it is the mountpoint).
                inv.dirty_dirs.push(ROOT_INO);
            } else if let Some(node) = self.remove_node(ino) {
                inv.removed.push((node.parent, ino, node.name));
            }
            self.doc_inos.retain(|(c, _), _| c != &ctx_id);
            self.ctx_workspaces.remove(&ctx_id);
            self.ctx_urls.remove(&ctx_id);
            inv.dirty_dirs.push(self.contexts_parent());
        }

        for ctx in contexts {
            // In context-rooted mode, ignore every context but the rooted one.
            if let Some(root_ctx) = &self.context_root {
                if root_ctx != &ctx.id {
                    continue;
                }
            }
            let meta = render_context_meta(ctx);
            match self.ctx_inos.get(&ctx.id).copied() {
                Some(ctx_ino) => {
                    // Refresh .context.json if the context (url etc.) changed
                    let meta_ino = self
                        .children
                        .get(&ctx_ino)
                        .and_then(|c| c.get(CONTEXT_META_FILE))
                        .copied();
                    if let Some(meta_ino) = meta_ino {
                        let node = self.nodes.get_mut(&meta_ino).unwrap();
                        let fresh = NodeContent::Inline(Arc::new(meta));
                        if node.content != fresh {
                            node.content = fresh;
                            node.mtime = now;
                            inv.changed.push(meta_ino);
                        }
                    }
                    if let Some(ws) = &ctx.workspace_id {
                        self.ctx_workspaces.insert(ctx.id.clone(), ws.clone());
                    }
                    self.ctx_urls.insert(ctx.id.clone(), ctx.url.clone());
                }
                None => {
                    // The context's "directory": ROOT itself when context-rooted
                    // (its schema dirs become the mount's top level), else a new
                    // named dir under Contexts/.
                    let ctx_ino = if self.context_root.is_some() {
                        ROOT_INO
                    } else {
                        let i = self.alloc_ino();
                        self.insert_node(Node {
                            ino: i,
                            parent: CONTEXTS_INO,
                            name: ctx.id.clone(),
                            mtime: now,
                            content: NodeContent::Dir,
                        });
                        i
                    };
                    self.ctx_inos.insert(ctx.id.clone(), ctx_ino);
                    if let Some(ws) = &ctx.workspace_id {
                        self.ctx_workspaces.insert(ctx.id.clone(), ws.clone());
                    }
                    self.ctx_urls.insert(ctx.id.clone(), ctx.url.clone());
                    // Grouping is a DERIVED view, dotted and read-only. The
                    // documents themselves are the context's files (flat), so a
                    // gesture means the same thing here as anywhere else on the
                    // mount — see docs/data-representation.md.
                    let by_schema = self.alloc_ino();
                    self.insert_node(Node {
                        ino: by_schema,
                        parent: ctx_ino,
                        name: BY_SCHEMA_DIR.to_string(),
                        mtime: now,
                        content: NodeContent::Dir,
                    });
                    let meta_ino = self.alloc_ino();
                    self.insert_node(Node {
                        ino: meta_ino,
                        parent: ctx_ino,
                        name: CONTEXT_META_FILE.to_string(),
                        mtime: now,
                        content: NodeContent::Inline(Arc::new(meta)),
                    });
                    inv.dirty_dirs.push(self.contexts_parent());
                    added.push(ctx.id.clone());
                }
            }
        }
        (inv, added)
    }

    /// Refresh the .context.json of one context (URL switches must be visible
    /// to agents reading the meta file without waiting for a full resync).
    pub fn update_context_meta(&mut self, ctx: &ContextInfo) -> Invalidation {
        let mut inv = Invalidation::default();
        let Some(ctx_ino) = self.ctx_inos.get(&ctx.id).copied() else {
            return inv;
        };
        let meta = render_context_meta(ctx);
        let meta_ino = self
            .children
            .get(&ctx_ino)
            .and_then(|c| c.get(CONTEXT_META_FILE))
            .copied();
        if let Some(meta_ino) = meta_ino {
            let node = self.nodes.get_mut(&meta_ino).unwrap();
            let fresh = NodeContent::Inline(Arc::new(meta));
            if node.content != fresh {
                node.content = fresh;
                node.mtime = SystemTime::now();
                inv.changed.push(meta_ino);
            }
        }
        inv
    }

    /// Replace the document view of one context with a freshly fetched list.
    ///
    /// FLAT: the documents are the context's files. `.by-schema/` is rebuilt
    /// from the same render pass — it is derived, so it carries no sticky names
    /// and no document bindings; the flat entries own those.
    pub fn apply_documents(
        &mut self,
        ctx_id: &str,
        docs: &[Document],
        names: &NameStore,
    ) -> Invalidation {
        let mut inv = Invalidation::default();
        let Some(ctx_ino) = self.ctx_inos.get(ctx_id).copied() else {
            return inv;
        };

        // Render every doc once, deterministically, and assign the sticky
        // filename the flat view shows.
        let sorted = by_placement(docs);
        let workspace_id = self.ctx_workspaces.get(ctx_id).cloned();
        let ws_key = workspace_id.clone().unwrap_or_default();
        let name_key = self
            .ctx_urls
            .get(ctx_id)
            .cloned()
            .unwrap_or_else(|| FLAT_NAME_KEY.to_string());

        let mut desired: BTreeMap<String, (u64, NodeContent, SystemTime)> = BTreeMap::new();
        let mut grouped: BTreeMap<String, Vec<(String, NodeContent, SystemTime)>> = BTreeMap::new();
        let mut taken: HashSet<String> = HashSet::new();

        for doc in sorted {
            let rendered = render::render(doc);
            let content = match rendered.content {
                render::Content::Inline(bytes) => NodeContent::Inline(Arc::new(bytes)),
                render::Content::Remote { size } => match &workspace_id {
                    Some(ws) => NodeContent::Remote {
                        workspace_id: ws.clone(),
                        doc_id: doc.id,
                        size,
                        checksum: doc.checksum.clone(),
                    },
                    // No workspace to address the content route — degrade to the
                    // document JSON rather than an unreadable entry.
                    None => NodeContent::Inline(Arc::new(
                        serde_json::to_vec_pretty(&doc.data).unwrap_or_default(),
                    )),
                },
            };

            // Sticky names are keyed per context AND per context URL: there
            // are no schema dirs to key by anymore, and the URL is what decides
            // which documents are filed here (see `ctx_urls`).
            let persisted = names.get(ctx_id, &ws_key, &name_key, doc.id);
            let name = match persisted {
                Some(n) if !taken.contains(&n) => n,
                _ => {
                    let candidate = if taken.contains(&rendered.base_name) {
                        render::with_id_suffix(&rendered.base_name, doc.id)
                    } else {
                        rendered.base_name.clone()
                    };
                    if let Err(e) = names.put(ctx_id, &ws_key, &name_key, doc.id, &candidate) {
                        log::warn!("name store write failed: {e}");
                    }
                    candidate
                }
            };
            taken.insert(name.clone());
            grouped.entry(rendered.dir.clone()).or_default().push((
                name.clone(),
                content.clone(),
                doc.updated_at,
            ));
            desired.insert(name, (doc.id, content, doc.updated_at));
        }

        self.reconcile_doc_dir(ctx_ino, ctx_id, &desired, &mut inv);
        // Rebuild the grouping when the GROUPING changed, not when the flat
        // directory happened to gain or lose a node. A document created through
        // the mount is already in the flat view by the time the server confirms
        // it (the write path adopts it), so keying off that left it out of
        // `.by-schema/` until some unrelated change forced a rebuild.
        let signature = group_signature(&grouped);
        if self.by_schema_sigs.get(ctx_id) != Some(&signature) {
            self.by_schema_sigs.insert(ctx_id.to_string(), signature);
            self.rebuild_by_schema(ctx_ino, &grouped, &mut inv);
        }
        inv
    }

    /// Bring one directory's document files in line with `desired`. Returns
    /// whether anything changed.
    fn reconcile_doc_dir(
        &mut self,
        dir_ino: u64,
        ctx_id: &str,
        desired: &BTreeMap<String, (u64, NodeContent, SystemTime)>,
        inv: &mut Invalidation,
    ) -> bool {
        // Only document files are ours to reconcile: `.context.json` and
        // `.by-schema/` live here too and are managed elsewhere.
        let have: Vec<(String, u64)> = self
            .children
            .get(&dir_ino)
            .map(|c| {
                c.iter()
                    .filter(|(name, _)| {
                        name.as_str() != CONTEXT_META_FILE && name.as_str() != BY_SCHEMA_DIR
                    })
                    .map(|(n, i)| (n.clone(), *i))
                    .collect()
            })
            .unwrap_or_default();

        let mut dirty = false;
        for (name, ino) in have {
            match desired.get(&name) {
                Some((doc_id, content, mtime))
                    if self.doc_for_ino(ino).map(|(_, d)| d) == Some(*doc_id) =>
                {
                    let node = self.nodes.get_mut(&ino).unwrap();
                    if node.content != *content {
                        node.content = content.clone();
                        node.mtime = *mtime;
                        inv.changed.push(ino);
                    }
                }
                _ if self.defer_removals => {
                    // Held, not dropped: the unlink that names it has to find
                    // it. See Invalidation::vanished.
                    self.tombstone(ino);
                    inv.vanished.push((dir_ino, ino, name));
                    dirty = true;
                }
                _ => {
                    self.remove_node(ino);
                    self.doc_inos.retain(|_, &mut i| i != ino);
                    inv.removed.push((dir_ino, ino, name));
                    dirty = true;
                }
            }
        }

        for (name, (doc_id, content, mtime)) in desired {
            if self.lookup(dir_ino, name).is_some() {
                continue;
            }
            let ino = self.alloc_ino();
            self.insert_node(Node {
                ino,
                parent: dir_ino,
                name: name.clone(),
                mtime: *mtime,
                content: content.clone(),
            });
            self.doc_inos.insert((ctx_id.to_string(), *doc_id), ino);
            inv.added.push(ino);
            dirty = true;
        }

        if dirty {
            inv.dirty_dirs.push(dir_ino);
        }
        dirty
    }

    /// Rebuild `.by-schema/` from scratch. It is derived and read-only, so
    /// throwing it away and re-materializing is simpler — and safer — than
    /// diffing a second copy of every document.
    fn rebuild_by_schema(
        &mut self,
        ctx_ino: u64,
        grouped: &BTreeMap<String, Vec<(String, NodeContent, SystemTime)>>,
        inv: &mut Invalidation,
    ) {
        let Some(root) = self.lookup(ctx_ino, BY_SCHEMA_DIR).map(|n| n.ino) else {
            return;
        };
        self.remove_subtree(root, inv);

        for (dir, entries) in grouped {
            if entries.is_empty() {
                continue;
            }
            let dir_ino = self.alloc_ino();
            self.insert_node(Node {
                ino: dir_ino,
                parent: root,
                name: dir.clone(),
                mtime: SystemTime::now(),
                content: NodeContent::Dir,
            });
            for (name, content, mtime) in entries {
                let ino = self.alloc_ino();
                self.insert_node(Node {
                    ino,
                    parent: dir_ino,
                    name: name.clone(),
                    mtime: *mtime,
                    content: content.clone(),
                });
            }
        }
        inv.dirty_dirs.push(root);
    }

    fn ws(&self) -> &WsState {
        self.ws.as_ref().expect("workspace mode")
    }
    fn ws_mut(&mut self) -> &mut WsState {
        self.ws.as_mut().expect("workspace mode")
    }

    pub fn ws_id(&self) -> Option<String> {
        self.ws.as_ref().map(|w| w.ws_id.clone())
    }
    pub fn ws_name(&self) -> Option<String> {
        self.ws.as_ref().map(|w| w.ws_name.clone())
    }

    /// (tree id, tree type) for a mounted tree name.
    pub fn ws_tree_meta(&self, name: &str) -> Option<(String, String)> {
        self.ws
            .as_ref()?
            .trees
            .get(name)
            .map(|t| (t.id.clone(), t.tree_type.clone()))
    }

    /// All (tree name, normalized path) pairs known — the worker fetches the
    /// documents of each one on refresh.
    pub fn ws_paths(&self) -> Vec<(String, String)> {
        self.ws
            .as_ref()
            .map(|w| w.path_inos.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Drop path/file map entries whose inodes no longer exist (after a subtree
    /// removal). Keeps the workspace maps consistent with the node store.
    fn prune_ws(&mut self) {
        let nodes = &self.nodes;
        if let Some(w) = self.ws.as_mut() {
            w.path_inos.retain(|_, &mut i| nodes.contains_key(&i));
            w.file_docs.retain(|i, _| nodes.contains_key(i));
        }
    }

    /// Reconcile the set of tree root dirs under ROOT with the server's trees.
    pub fn apply_trees(&mut self, trees: &[TreeInfo]) -> Invalidation {
        let mut inv = Invalidation::default();
        let now = SystemTime::now();
        let wanted: HashSet<&str> = trees.iter().map(|t| t.name.as_str()).collect();

        let stale: Vec<String> = self
            .ws()
            .trees
            .keys()
            .filter(|n| !wanted.contains(n.as_str()))
            .cloned()
            .collect();
        for name in stale {
            let root_ino = self.ws().trees[&name].root_ino;
            self.remove_subtree(root_ino, &mut inv);
            if let Some(node) = self.remove_node(root_ino) {
                inv.removed.push((node.parent, root_ino, node.name));
            }
            self.ws_mut().trees.remove(&name);
            inv.dirty_dirs.push(TREES_INO);
        }

        for t in trees {
            match self.ws().trees.get(&t.name).map(|w| w.root_ino) {
                Some(root_ino) => {
                    // Refresh metadata in case id/type changed; keep the ino.
                    self.ws_mut().trees.insert(
                        t.name.clone(),
                        WsTree {
                            id: t.id.clone(),
                            tree_type: t.tree_type.clone(),
                            root_ino,
                        },
                    );
                }
                None => {
                    let ino = self.alloc_ino();
                    self.insert_node(Node {
                        ino,
                        parent: TREES_INO,
                        name: t.name.clone(),
                        mtime: now,
                        content: NodeContent::Dir,
                    });
                    let w = self.ws_mut();
                    w.trees.insert(
                        t.name.clone(),
                        WsTree {
                            id: t.id.clone(),
                            tree_type: t.tree_type.clone(),
                            root_ino: ino,
                        },
                    );
                    w.path_inos.insert((t.name.clone(), "/".to_string()), ino);
                    inv.dirty_dirs.push(ROOT_INO);
                }
            }
        }
        self.prune_ws();
        inv
    }

    /// Reconcile a tree's directory hierarchy from its flat path list.
    pub fn apply_tree_paths(&mut self, tree_name: &str, paths: &[String]) -> Invalidation {
        let mut inv = Invalidation::default();
        let now = SystemTime::now();
        if !self.ws().trees.contains_key(tree_name) {
            return inv;
        }

        let mut desired: Vec<String> = paths
            .iter()
            .map(|p| norm_path(p))
            .filter(|p| p != "/")
            .collect();
        desired.sort_by_key(|p| p.matches('/').count()); // shallow first
        desired.dedup();
        let desired_set: HashSet<&str> = desired.iter().map(String::as_str).collect();

        // Remove vanished path dirs, deepest first (children before parents).
        let mut existing: Vec<(String, u64)> = self
            .ws()
            .path_inos
            .iter()
            .filter(|((t, p), _)| t == tree_name && p != "/")
            .map(|((_, p), i)| (p.clone(), *i))
            .collect();
        existing.sort_by_key(|(p, _)| std::cmp::Reverse(p.matches('/').count()));
        for (p, ino) in existing {
            if !desired_set.contains(p.as_str()) {
                self.remove_subtree(ino, &mut inv);
                if let Some(node) = self.remove_node(ino) {
                    inv.dirty_dirs.push(node.parent);
                    inv.removed.push((node.parent, ino, node.name));
                }
                self.ws_mut().path_inos.remove(&(tree_name.to_string(), p));
            }
        }

        // Add new path dirs, shallow first so the parent always exists.
        for p in &desired {
            if self
                .ws()
                .path_inos
                .contains_key(&(tree_name.to_string(), p.clone()))
            {
                continue;
            }
            let parent = parent_path(p);
            let Some(parent_ino) = self
                .ws()
                .path_inos
                .get(&(tree_name.to_string(), parent))
                .copied()
            else {
                continue; // parent missing (out-of-order); next refresh fixes it
            };
            let ino = self.alloc_ino();
            self.insert_node(Node {
                ino,
                parent: parent_ino,
                name: leaf_name(p),
                mtime: now,
                content: NodeContent::Dir,
            });
            self.ws_mut()
                .path_inos
                .insert((tree_name.to_string(), p.clone()), ino);
            inv.dirty_dirs.push(parent_ino);
        }
        self.prune_ws();
        inv
    }

    /// Reconcile the document files in one tree path's directory.
    /// Insert a home directory node (mkdir). Marked loaded: it was just created
    /// and is empty, so there is nothing to fetch.
    pub fn insert_home_dir(&mut self, parent_ino: u64, name: &str, path: &str) -> u64 {
        let ino = self.alloc_ino();
        self.insert_node(Node {
            ino,
            parent: parent_ino,
            name: name.to_string(),
            mtime: SystemTime::now(),
            content: NodeContent::HomeDir {
                path: path.to_string(),
                loaded: true,
            },
        });
        ino
    }

    /// Publish a home file into the view after a write, creating the node if
    /// this was a fresh file.
    /// Publish a home file into the view after a write.
    ///
    /// `adopt_ino` is the overlay's ino, and taking it over matters: the kernel
    /// already handed that ino to the process through create(), so allocating a
    /// fresh one here would leave the cached dentry pointing at a node that no
    /// longer exists — the file reads back as ENOENT until the directory is
    /// re-listed. Same reason adopt_tree_file() adopts.
    pub fn upsert_home_file(
        &mut self,
        dir_ino: u64,
        name: &str,
        path: &str,
        size: u64,
        adopt_ino: Option<u64>,
    ) -> u64 {
        let content = NodeContent::HomeFile {
            path: path.to_string(),
            size,
        };
        if let Some(existing) = self.lookup(dir_ino, name).map(|n| n.ino) {
            if let Some(node) = self.nodes.get_mut(&existing) {
                node.content = content;
                node.mtime = SystemTime::now();
            }
            return existing;
        }
        let ino = adopt_ino.unwrap_or_else(|| self.alloc_ino());
        self.insert_node(Node {
            ino,
            parent: dir_ino,
            name: name.to_string(),
            mtime: SystemTime::now(),
            content,
        });
        ino
    }

    /// The home-drive path a node addresses, if it is one.
    pub fn home_path(&self, ino: u64) -> Option<(String, bool)> {
        match &self.nodes.get(&ino)?.content {
            NodeContent::HomeDir { path, loaded } => Some((path.clone(), *loaded)),
            NodeContent::HomeFile { path, .. } => Some((path.clone(), true)),
            _ => None,
        }
    }

    pub fn home_file(&self, ino: u64) -> Option<(String, u64)> {
        match &self.nodes.get(&ino)?.content {
            NodeContent::HomeFile { path, size } => Some((path.clone(), *size)),
            _ => None,
        }
    }

    /// Replace a home directory's children with a freshly fetched listing, and
    /// mark it loaded so the next readdir does not refetch.
    pub fn apply_home_entries(&mut self, dir_ino: u64, entries: &[HomeEntry]) -> Invalidation {
        let mut inv = Invalidation::default();
        let Some((dir_path, _)) = self.home_path(dir_ino) else {
            return inv;
        };

        let wanted: HashMap<&str, &HomeEntry> =
            entries.iter().map(|e| (e.name.as_str(), e)).collect();

        let have: Vec<(String, u64)> = self
            .children
            .get(&dir_ino)
            .map(|c| c.iter().map(|(n, i)| (n.clone(), *i)).collect())
            .unwrap_or_default();
        for (name, ino) in have {
            if !wanted.contains_key(name.as_str()) {
                self.remove_subtree(ino, &mut inv);
                self.remove_node(ino);
                inv.removed.push((dir_ino, ino, name));
            }
        }

        for entry in entries {
            let child_path = join_home_path(&dir_path, &entry.name);
            let content = if entry.is_dir {
                NodeContent::HomeDir {
                    path: child_path,
                    loaded: false,
                }
            } else {
                NodeContent::HomeFile {
                    path: child_path,
                    size: entry.size,
                }
            };
            match self.lookup(dir_ino, &entry.name).map(|n| n.ino) {
                Some(ino) => {
                    if let Some(node) = self.nodes.get_mut(&ino) {
                        // Keep a loaded directory loaded; only its identity is
                        // being confirmed here.
                        let keep_loaded = matches!(
                            (&node.content, &content),
                            (
                                NodeContent::HomeDir { loaded: true, .. },
                                NodeContent::HomeDir { .. }
                            )
                        );
                        if !keep_loaded && node.content != content {
                            node.content = content;
                            inv.changed.push(ino);
                        }
                    }
                }
                None => {
                    let ino = self.alloc_ino();
                    self.insert_node(Node {
                        ino,
                        parent: dir_ino,
                        name: entry.name.clone(),
                        mtime: SystemTime::now(),
                        content,
                    });
                }
            }
        }

        if let Some(NodeContent::HomeDir { loaded, .. }) =
            self.nodes.get_mut(&dir_ino).map(|n| &mut n.content)
        {
            *loaded = true;
        }
        inv.dirty_dirs.push(dir_ino);
        inv
    }

    // ── Home, addressed by key ──────────────────────────────────────────────
    // In mirror mode Home is fed from the mirror store rather than listed on
    // demand: keys are relative, `/`-separated paths, and directories exist
    // because a key passes through them (or because the store says so).

    /// Serve every Home directory as already listed: nothing is fetched on
    /// look, the store is the listing.
    pub fn set_home_mirrored(&mut self, on: bool) {
        self.home_mirrored = on;
        if let Some(NodeContent::HomeDir { loaded, .. }) =
            self.nodes.get_mut(&HOME_INO).map(|n| &mut n.content)
        {
            *loaded = on || *loaded;
        }
    }

    /// The store key a Home node addresses (`Docs/a.md`), None outside Home.
    /// The Home root itself is the empty key.
    pub fn home_key(&self, ino: u64) -> Option<String> {
        let (path, _) = self.home_path(ino)?;
        Some(path.trim_matches('/').to_string())
    }

    /// The Home node at a key, walking from the Home root.
    pub fn home_ino_for_key(&self, key: &str) -> Option<u64> {
        let mut ino = HOME_INO;
        for seg in key.split('/').filter(|s| !s.is_empty()) {
            ino = self.lookup(ino, seg)?.ino;
        }
        Some(ino)
    }

    /// Ensure every directory along `key` exists (the key's own leaf is a
    /// directory too). Returns the leaf dir's ino.
    fn ensure_home_dirs(&mut self, key: &str, inv: &mut Invalidation) -> u64 {
        let mut ino = HOME_INO;
        let mut path = String::new();
        for seg in key.split('/').filter(|s| !s.is_empty()) {
            path.push('/');
            path.push_str(seg);
            match self.lookup(ino, seg).map(|n| (n.ino, n.is_dir())) {
                Some((child, true)) => ino = child,
                Some((child, false)) => {
                    // A file where a directory must be: the file loses (a
                    // rename on the hub turned it into a folder name).
                    self.remove_node(child);
                    inv.removed.push((ino, child, seg.to_string()));
                    let new = self.alloc_ino();
                    self.insert_node(Node {
                        ino: new,
                        parent: ino,
                        name: seg.to_string(),
                        mtime: SystemTime::now(),
                        content: NodeContent::HomeDir {
                            path: path.clone(),
                            loaded: true,
                        },
                    });
                    inv.dirty_dirs.push(ino);
                    ino = new;
                }
                None => {
                    let new = self.alloc_ino();
                    self.insert_node(Node {
                        ino: new,
                        parent: ino,
                        name: seg.to_string(),
                        mtime: SystemTime::now(),
                        content: NodeContent::HomeDir {
                            path: path.clone(),
                            loaded: true,
                        },
                    });
                    inv.dirty_dirs.push(ino);
                    inv.added.push(new);
                    ino = new;
                }
            }
        }
        ino
    }

    /// An explicit directory (mkdir'd, or empty on the hub).
    pub fn ensure_home_dir_key(&mut self, key: &str) -> Invalidation {
        let mut inv = Invalidation::default();
        self.ensure_home_dirs(key, &mut inv);
        inv
    }

    /// Create or update the file at `key` (size/mtime), creating the
    /// directories it passes through.
    pub fn upsert_home_key(&mut self, key: &str, size: u64, mtime: SystemTime) -> Invalidation {
        let mut inv = Invalidation::default();
        let key = key.trim_matches('/');
        let (dir_key, name) = match key.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", key),
        };
        if name.is_empty() {
            return inv;
        }
        let dir_ino = self.ensure_home_dirs(dir_key, &mut inv);
        let path = format!("/{key}");
        let content = NodeContent::HomeFile { path, size };
        match self.lookup(dir_ino, name).map(|n| (n.ino, n.is_dir())) {
            Some((ino, false)) => {
                let node = self.nodes.get_mut(&ino).unwrap();
                if node.content != content || node.mtime != mtime {
                    node.content = content;
                    node.mtime = mtime;
                    inv.changed.push(ino);
                }
            }
            Some((ino, true)) => {
                // A directory where a file must be: the subtree is gone.
                self.remove_subtree(ino, &mut inv);
                self.remove_node(ino);
                inv.removed.push((dir_ino, ino, name.to_string()));
                let new = self.alloc_ino();
                self.insert_node(Node {
                    ino: new,
                    parent: dir_ino,
                    name: name.to_string(),
                    mtime,
                    content,
                });
                inv.added.push(new);
                inv.dirty_dirs.push(dir_ino);
            }
            None => {
                let new = self.alloc_ino();
                self.insert_node(Node {
                    ino: new,
                    parent: dir_ino,
                    name: name.to_string(),
                    mtime,
                    content,
                });
                inv.added.push(new);
                inv.dirty_dirs.push(dir_ino);
            }
        }
        inv
    }

    /// Remove the node at `key` (file or directory subtree), then prune the
    /// directories above it that are now empty — unless `keep_dirs` names
    /// them (explicit dirs the store still holds).
    pub fn remove_home_key(&mut self, key: &str, keep_dirs: &HashSet<String>) -> Invalidation {
        let mut inv = Invalidation::default();
        let key = key.trim_matches('/');
        let Some(ino) = self.home_ino_for_key(key) else {
            return inv;
        };
        if ino == HOME_INO {
            return inv;
        }
        self.remove_subtree(ino, &mut inv);
        let Some(node) = self.remove_node(ino) else {
            return inv;
        };
        inv.removed.push((node.parent, ino, node.name));
        inv.dirty_dirs.push(node.parent);
        // Prune upward.
        let mut cur = node.parent;
        let mut cur_key = crate::mirror::parent_key(key).to_string();
        while cur != HOME_INO {
            let empty = self
                .children
                .get(&cur)
                .map(|c| c.is_empty())
                .unwrap_or(true);
            if !empty || keep_dirs.contains(&cur_key) {
                break;
            }
            let Some(dir) = self.remove_node(cur) else {
                break;
            };
            inv.removed.push((dir.parent, cur, dir.name));
            inv.dirty_dirs.push(dir.parent);
            cur = dir.parent;
            cur_key = crate::mirror::parent_key(&cur_key).to_string();
        }
        inv
    }

    /// Re-key a Home node (file or directory) from one key to another,
    /// creating the destination's directories. Used for hub-side renames.
    pub fn rename_home_key(&mut self, from: &str, to: &str) -> Invalidation {
        let mut inv = Invalidation::default();
        let Some(ino) = self.home_ino_for_key(from) else {
            return inv;
        };
        if ino == HOME_INO {
            return inv;
        }
        let (dst_dir, dst_name) = match to.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", to),
        };
        let dst_parent = self.ensure_home_dirs(dst_dir, &mut inv);
        if let Some(existing) = self.lookup(dst_parent, dst_name).map(|n| n.ino) {
            if existing != ino {
                self.remove_subtree(existing, &mut inv);
                self.remove_node(existing);
                inv.removed
                    .push((dst_parent, existing, dst_name.to_string()));
            }
        }
        let old_parent = self.nodes.get(&ino).map(|n| n.parent).unwrap_or(HOME_INO);
        let old_name = self
            .nodes
            .get(&ino)
            .map(|n| n.name.clone())
            .unwrap_or_default();
        self.rename_home(ino, dst_parent, dst_name);
        inv.removed.push((old_parent, ino, old_name));
        inv.dirty_dirs.push(old_parent);
        inv.dirty_dirs.push(dst_parent);
        inv.added.push(ino);
        inv
    }

    /// Move a Home node to a new parent/name and re-key the `path` of it and
    /// everything below (the write path's `mv`; the kernel already knows).
    pub fn rename_home(&mut self, ino: u64, new_parent: u64, new_name: &str) {
        let Some((parent_path, _)) = self.home_path(new_parent) else {
            return;
        };
        self.move_entry(ino, new_parent, new_name);
        let new_path = join_home_path(&parent_path, new_name);
        self.rekey_home_subtree(ino, &new_path);
        self.touch_dir(new_parent);
    }

    fn rekey_home_subtree(&mut self, ino: u64, path: &str) {
        let children: Vec<(String, u64)> = self
            .children
            .get(&ino)
            .map(|c| c.iter().map(|(n, i)| (n.clone(), *i)).collect())
            .unwrap_or_default();
        if let Some(node) = self.nodes.get_mut(&ino) {
            match &mut node.content {
                NodeContent::HomeDir { path: p, .. } | NodeContent::HomeFile { path: p, .. } => {
                    *p = path.to_string();
                }
                _ => {}
            }
        }
        for (name, child) in children {
            let child_path = join_home_path(path, &name);
            self.rekey_home_subtree(child, &child_path);
        }
    }

    /// Replace the whole Home tree with the store's view: `files` as
    /// (key, size, mtime), `dirs` the explicit directories. Nodes that
    /// survive keep their inos.
    pub fn apply_home_snapshot(
        &mut self,
        files: &[(String, u64, SystemTime)],
        dirs: &[String],
    ) -> Invalidation {
        let mut inv = Invalidation::default();
        let wanted_files: HashSet<&str> = files.iter().map(|(k, _, _)| k.as_str()).collect();
        let mut wanted_dirs: HashSet<String> = dirs.iter().cloned().collect();
        for (k, _, _) in files {
            let mut p = crate::mirror::parent_key(k);
            while !p.is_empty() {
                wanted_dirs.insert(p.to_string());
                p = crate::mirror::parent_key(p);
            }
        }
        // Walk the existing Home tree and drop what is not wanted.
        let mut stack: Vec<(u64, String)> = vec![(HOME_INO, String::new())];
        let mut victims: Vec<u64> = Vec::new();
        while let Some((ino, key)) = stack.pop() {
            let children: Vec<(String, u64, bool)> = self
                .children
                .get(&ino)
                .map(|c| {
                    c.iter()
                        .filter_map(|(n, i)| {
                            self.nodes.get(i).map(|node| (n.clone(), *i, node.is_dir()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (name, child, is_dir) in children {
                let child_key = if key.is_empty() {
                    name
                } else {
                    format!("{key}/{name}")
                };
                let keep = if is_dir {
                    wanted_dirs.contains(&child_key)
                } else {
                    wanted_files.contains(child_key.as_str())
                };
                if keep {
                    if is_dir {
                        stack.push((child, child_key));
                    }
                } else {
                    victims.push(child);
                }
            }
        }
        for ino in victims {
            if let Some(node) = self.nodes.get(&ino) {
                let parent = node.parent;
                let name = node.name.clone();
                self.remove_subtree(ino, &mut inv);
                self.remove_node(ino);
                inv.removed.push((parent, ino, name));
                inv.dirty_dirs.push(parent);
            }
        }
        for d in &wanted_dirs {
            self.ensure_home_dirs(d, &mut inv);
        }
        for (k, size, mtime) in files {
            let sub = self.upsert_home_key(k, *size, *mtime);
            inv.removed.extend(sub.removed);
            inv.changed.extend(sub.changed);
            inv.dirty_dirs.extend(sub.dirty_dirs);
            inv.added.extend(sub.added);
        }
        inv
    }

    /// Materialize the workspace trash as a flat folder. Deliberately flat: it
    /// is a holding area, not a hierarchy — where each document CAME from is
    /// recorded server-side and surfaced by restore, not by nesting here.
    pub fn apply_trash_documents(&mut self, docs: &[Document]) -> Invalidation {
        let mut inv = Invalidation::default();
        if self.ws.is_none() {
            return inv;
        }
        let ws_id = self.ws().ws_id.clone();

        let sorted = by_placement(docs);

        let mut desired: BTreeMap<String, (u64, NodeContent, SystemTime)> = BTreeMap::new();
        let mut taken: HashSet<String> = HashSet::new();
        for doc in sorted {
            let (base, content) = (render::doc_name(doc), render::content(doc));
            let content = match content {
                render::Content::Inline(bytes) => NodeContent::Inline(Arc::new(bytes)),
                render::Content::Remote { size } => NodeContent::Remote {
                    workspace_id: ws_id.clone(),
                    doc_id: doc.id,
                    size,
                    checksum: doc.checksum.clone(),
                },
            };
            let name = if taken.contains(&base) {
                render::with_id_suffix(&base, doc.id)
            } else {
                base
            };
            taken.insert(name.clone());
            desired.insert(name, (doc.id, content, doc.updated_at));
        }

        let have: Vec<(String, u64)> = self
            .children
            .get(&TRASH_INO)
            .map(|c| c.iter().map(|(n, i)| (n.clone(), *i)).collect())
            .unwrap_or_default();

        let mut dirty = false;
        for (name, ino) in have {
            if !desired.contains_key(&name) {
                self.remove_node(ino);
                self.ws_mut().file_docs.remove(&ino);
                inv.removed.push((TRASH_INO, ino, name));
                dirty = true;
            }
        }
        for (name, (doc_id, content, mtime)) in desired {
            if self.lookup(TRASH_INO, &name).is_some() {
                continue;
            }
            let ino = self.alloc_ino();
            self.insert_node(Node {
                ino,
                parent: TRASH_INO,
                name,
                mtime,
                content,
            });
            self.ws_mut().file_docs.insert(
                ino,
                WsFile {
                    tree_name: TRASH_TREE_NAME.to_string(),
                    path: TRASH_PATH.to_string(),
                    doc_id,
                },
            );
            inv.added.push(ino);
            dirty = true;
        }
        if dirty {
            inv.dirty_dirs.push(TRASH_INO);
        }
        inv
    }

    pub fn apply_tree_documents(
        &mut self,
        tree_name: &str,
        path: &str,
        docs: &[Document],
    ) -> Invalidation {
        let mut inv = Invalidation::default();
        let norm = norm_path(path);
        let Some(&dir_ino) = self
            .ws()
            .path_inos
            .get(&(tree_name.to_string(), norm.clone()))
        else {
            return inv;
        };
        let ws_id = self.ws().ws_id.clone();

        let sorted = by_placement(docs);

        let mut desired: BTreeMap<String, (u64, NodeContent, SystemTime)> = BTreeMap::new();
        let mut taken: HashSet<String> = HashSet::new();
        for doc in sorted {
            let (base, content) = (render::doc_name(doc), render::content(doc));
            let content = match content {
                render::Content::Inline(bytes) => NodeContent::Inline(Arc::new(bytes)),
                render::Content::Remote { size } => NodeContent::Remote {
                    workspace_id: ws_id.clone(),
                    doc_id: doc.id,
                    size,
                    checksum: doc.checksum.clone(),
                },
            };
            let name = if taken.contains(&base) {
                render::with_id_suffix(&base, doc.id)
            } else {
                base
            };
            taken.insert(name.clone());
            desired.insert(name, (doc.id, content, doc.updated_at));
        }

        // Existing document files in this dir (subdirs are managed elsewhere).
        let have: Vec<(String, u64)> = self
            .children
            .get(&dir_ino)
            .map(|c| {
                c.iter()
                    .filter(|(_, i)| self.ws().file_docs.contains_key(i))
                    .map(|(n, i)| (n.clone(), *i))
                    .collect()
            })
            .unwrap_or_default();

        let mut dirty = false;
        for (name, ino) in have {
            let same = desired.get(&name).filter(|(doc_id, _, _)| {
                self.ws().file_docs.get(&ino).map(|f| f.doc_id) == Some(*doc_id)
            });
            match same {
                Some((_, content, mtime)) => {
                    let node = self.nodes.get_mut(&ino).unwrap();
                    if node.content != *content {
                        node.content = content.clone();
                        node.mtime = *mtime;
                        inv.changed.push(ino);
                    }
                }
                None => {
                    self.remove_node(ino);
                    self.ws_mut().file_docs.remove(&ino);
                    inv.removed.push((dir_ino, ino, name));
                    dirty = true;
                }
            }
        }
        for (name, (doc_id, content, mtime)) in desired {
            if self.lookup(dir_ino, &name).is_some() {
                continue;
            }
            let ino = self.alloc_ino();
            self.insert_node(Node {
                ino,
                parent: dir_ino,
                name,
                mtime,
                content,
            });
            self.ws_mut().file_docs.insert(
                ino,
                WsFile {
                    tree_name: tree_name.to_string(),
                    path: norm.clone(),
                    doc_id,
                },
            );
            inv.added.push(ino);
            dirty = true;
        }
        if dirty {
            inv.dirty_dirs.push(dir_ino);
            if let Some(node) = self.nodes.get_mut(&dir_ino) {
                node.mtime = SystemTime::now();
            }
        }
        inv
    }

    /// Classify a directory ino as a workspace tree path target (for mkdir /
    /// document create). Returns (tree name, tree id, tree type, path).
    pub fn locate_tree_dir(&self, ino: u64) -> Option<(String, String, String, String)> {
        let w = self.ws.as_ref()?;
        let (tree, path) = w
            .path_inos
            .iter()
            .find(|(_, &i)| i == ino)
            .map(|((t, p), _)| (t.clone(), p.clone()))?;
        let meta = w.trees.get(&tree)?;
        Some((tree, meta.id.clone(), meta.tree_type.clone(), path))
    }

    /// Classify a file ino as a workspace document. Returns (tree name, tree id,
    /// tree type, path, doc id).
    pub fn tree_file(&self, ino: u64) -> Option<(String, String, String, String, u64)> {
        let w = self.ws.as_ref()?;
        let f = w.file_docs.get(&ino)?;
        let meta = w.trees.get(&f.tree_name)?;
        Some((
            f.tree_name.clone(),
            meta.id.clone(),
            meta.tree_type.clone(),
            f.path.clone(),
            f.doc_id,
        ))
    }

    /// Materialize a directory created via the write path (mkdir), before the
    /// next server refresh confirms it. Returns the new dir ino.
    pub fn adopt_tree_dir(
        &mut self,
        parent_ino: u64,
        name: &str,
        tree_name: &str,
        path: &str,
    ) -> u64 {
        let ino = self.alloc_ino();
        self.insert_node(Node {
            ino,
            parent: parent_ino,
            name: name.to_string(),
            mtime: SystemTime::now(),
            content: NodeContent::Dir,
        });
        self.ws_mut()
            .path_inos
            .insert((tree_name.to_string(), norm_path(path)), ino);
        ino
    }

    /// Remove a directory subtree created/seen in workspace mode (rmdir).
    pub fn remove_tree_dir(&mut self, ino: u64) -> Invalidation {
        let mut inv = Invalidation::default();
        self.remove_subtree(ino, &mut inv);
        if let Some(node) = self.remove_node(ino) {
            inv.removed.push((node.parent, ino, node.name));
            inv.dirty_dirs.push(node.parent);
        }
        self.prune_ws();
        inv
    }

    /// Materialize a document file created via the write path (before refresh).
    #[allow(clippy::too_many_arguments)]
    pub fn adopt_tree_file(
        &mut self,
        dir_ino: u64,
        name: &str,
        tree_name: &str,
        path: &str,
        doc_id: u64,
        ino: u64,
        content: Arc<Vec<u8>>,
    ) {
        self.insert_node(Node {
            ino,
            parent: dir_ino,
            name: name.to_string(),
            mtime: SystemTime::now(),
            content: NodeContent::Inline(content),
        });
        self.ws_mut().file_docs.insert(
            ino,
            WsFile {
                tree_name: tree_name.to_string(),
                path: norm_path(path),
                doc_id,
            },
        );
    }

    /// Remove a document file node (workspace unlink).
    pub fn remove_tree_file(&mut self, ino: u64) {
        self.remove_node(ino);
        if let Some(w) = self.ws.as_mut() {
            w.file_docs.remove(&ino);
        }
    }

    /// Move an entry into another directory, renaming it on the way.
    fn move_entry(&mut self, ino: u64, new_parent: u64, new_name: &str) {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return;
        };
        let old_parent = node.parent;
        let old_name = std::mem::replace(&mut node.name, new_name.to_string());
        node.parent = new_parent;
        if let Some(siblings) = self.children.get_mut(&old_parent) {
            siblings.remove(&old_name);
        }
        self.children
            .entry(new_parent)
            .or_default()
            .insert(new_name.to_string(), ino);
    }

    /// Move a document file node into another directory (cross-directory
    /// rename). The document itself is unchanged — only where it is shown, and
    /// under which name. Both trees are reindexed: a move ACROSS trees leaves
    /// the source tree's path index stale otherwise.
    pub fn move_tree_file(
        &mut self,
        ino: u64,
        new_parent: u64,
        new_name: &str,
        src_tree_name: &str,
        dst_tree_name: &str,
    ) {
        self.move_entry(ino, new_parent, new_name);
        if let Some(w) = self.ws.as_mut() {
            if let Some(f) = w.file_docs.get_mut(&ino) {
                f.tree_name = dst_tree_name.to_string();
            }
        }
        // reindex recomputes every path from the tree walk, so the node's new
        // path falls out of it rather than being spelled out here.
        self.reindex_tree_paths(dst_tree_name);
        if src_tree_name != dst_tree_name {
            self.reindex_tree_paths(src_tree_name);
        }
    }

    /// Move a FOLDER node into another directory (cross-parent folder move).
    pub fn move_tree_path_node(
        &mut self,
        ino: u64,
        new_parent: u64,
        new_name: &str,
        tree_name: &str,
    ) {
        self.move_entry(ino, new_parent, new_name);
        self.reindex_tree_paths(tree_name);
    }

    /// Point a file node at a different document id (overwrite-rename).
    pub fn rebind_tree_file(&mut self, ino: u64, doc_id: u64) {
        if let Some(w) = self.ws.as_mut() {
            if let Some(f) = w.file_docs.get_mut(&ino) {
                f.doc_id = doc_id;
            }
        }
    }

    /// Reverse lookup: the file ino materializing (tree, path, doc).
    pub fn ws_ino_for_doc(&self, tree_name: &str, path: &str, doc_id: u64) -> Option<u64> {
        let w = self.ws.as_ref()?;
        let np = norm_path(path);
        w.file_docs
            .iter()
            .find(|(_, f)| f.tree_name == tree_name && f.path == np && f.doc_id == doc_id)
            .map(|(i, _)| *i)
    }

    /// Rename a directory node (same parent) and reindex the tree's path maps.
    pub fn rename_tree_path(&mut self, ino: u64, new_name: &str, tree_name: &str) {
        self.rename_entry(ino, new_name);
        self.reindex_tree_paths(tree_name);
    }

    /// Recompute path_inos (and the paths recorded in file_docs) for a tree by
    /// walking it from the root. Robust against subtree moves/renames.
    fn reindex_tree_paths(&mut self, tree_name: &str) {
        let Some(root) = self
            .ws
            .as_ref()
            .and_then(|w| w.trees.get(tree_name))
            .map(|t| t.root_ino)
        else {
            return;
        };
        let mut path_map: Vec<((String, String), u64)> =
            vec![((tree_name.to_string(), "/".to_string()), root)];
        let mut file_paths: Vec<(u64, String)> = Vec::new();
        let mut stack = vec![(root, "/".to_string())];
        while let Some((dir_ino, path)) = stack.pop() {
            let Some(children) = self.children.get(&dir_ino) else {
                continue;
            };
            for (name, &cino) in children {
                let is_file = self
                    .ws
                    .as_ref()
                    .map(|w| w.file_docs.contains_key(&cino))
                    .unwrap_or(false);
                if is_file {
                    file_paths.push((cino, path.clone()));
                    continue;
                }
                if !self.nodes.get(&cino).map(Node::is_dir).unwrap_or(false) {
                    continue;
                }
                let cpath = if path == "/" {
                    format!("/{name}")
                } else {
                    format!("{path}/{name}")
                };
                path_map.push(((tree_name.to_string(), cpath.clone()), cino));
                stack.push((cino, cpath));
            }
        }
        if let Some(w) = self.ws.as_mut() {
            w.path_inos.retain(|(t, _), _| t != tree_name);
            for (k, v) in path_map {
                w.path_inos.insert(k, v);
            }
            for (ino, p) in file_paths {
                if let Some(f) = w.file_docs.get_mut(&ino) {
                    f.path = p;
                }
            }
        }
    }

    fn remove_subtree(&mut self, ino: u64, inv: &mut Invalidation) {
        let child_inos: Vec<u64> = self
            .children
            .get(&ino)
            .map(|c| c.values().copied().collect())
            .unwrap_or_default();
        for child in child_inos {
            self.remove_subtree(child, inv);
            if let Some(node) = self.remove_node(child) {
                inv.removed.push((node.parent, child, node.name));
            }
        }
    }
}

fn render_context_meta(ctx: &ContextInfo) -> Vec<u8> {
    serde_json::to_vec_pretty(&ctx.raw).unwrap_or_default()
}

/// Normalize a tree path: leading slash, no trailing slash, collapsed slashes.
/// Root is "/".
pub fn norm_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 1);
    out.push('/');
    for seg in p.split('/').filter(|s| !s.is_empty()) {
        if out.len() > 1 {
            out.push('/');
        }
        out.push_str(seg);
    }
    out
}

/// Parent of a normalized path ("/foo/bar" -> "/foo", "/foo" -> "/").
fn parent_path(p: &str) -> String {
    match p.rsplit_once('/') {
        Some((head, _)) if !head.is_empty() => head.to_string(),
        _ => "/".to_string(),
    }
}

/// Final segment of a path ("/foo/bar" -> "bar").
fn leaf_name(p: &str) -> String {
    p.rsplit('/').next().unwrap_or("").to_string()
}

/// Join a home-drive path with a child name, keeping a single leading slash.
pub fn join_home_path(dir: &str, name: &str) -> String {
    if dir == "/" || dir.is_empty() {
        format!("/{name}")
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}
