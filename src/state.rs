use crate::api::{ContextInfo, Document, TreeInfo};
use crate::names::NameStore;
use crate::render::{self, SCHEMA_DIRS};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

pub const ROOT_INO: u64 = 1;
pub const CONTEXTS_INO: u64 = 2;
const FIRST_DYNAMIC_INO: u64 = 16;

pub const CONTEXT_META_FILE: &str = ".context.json";

/// What a node serves on read. Remote content is fetched lazily through the
/// workspace content route and cached by checksum in the blob cache.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeContent {
    Dir,
    Inline(Arc<Vec<u8>>),
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
        matches!(self.content, NodeContent::Dir)
    }

    pub fn size(&self) -> u64 {
        match &self.content {
            NodeContent::Dir => 0,
            NodeContent::Inline(bytes) => bytes.len() as u64,
            // None (unresolved) reports 0 here; fsimpl resolves it lazily via
            // the blob store before answering getattr.
            NodeContent::Remote { size, .. } => size.unwrap_or(0),
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
}

impl Invalidation {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.changed.is_empty() && self.dirty_dirs.is_empty()
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
    /// context id -> workspaceId, needed to address the content route
    ctx_workspaces: HashMap<String, String>,
    /// When set, the mount is rooted at a single context: that context's schema
    /// dirs hang directly off ROOT (no `Contexts/<id>` wrapper), so mounting
    /// `-c mbag <path>` yields `<path>/mbag/{Notes,Tabs,…}`. None = global mount
    /// (root holds `Contexts/`, and later `Workspaces/`).
    context_root: Option<String>,
    /// Set when the mount roots a workspace tree view (`-w`). Mutually
    /// exclusive with the context maps above.
    ws: Option<WsState>,
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

    /// Workspace mount: ROOT holds one directory per tree, each mirroring the
    /// tree's path hierarchy with documents materialized as files.
    pub fn workspace_rooted(ws_id: String, ws_name: String) -> Self {
        let mut t = Self::bare(None);
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

    fn bare(context_root: Option<String>) -> Self {
        let mut t = Self {
            nodes: HashMap::new(),
            children: HashMap::new(),
            next_ino: FIRST_DYNAMIC_INO,
            doc_inos: HashMap::new(),
            ctx_inos: HashMap::new(),
            ctx_workspaces: HashMap::new(),
            context_root,
            ws: None,
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
        if node.ino != ROOT_INO {
            self.children
                .entry(node.parent)
                .or_default()
                .insert(node.name.clone(), node.ino);
        }
        self.children.entry(node.ino).or_default();
        self.nodes.insert(node.ino, node);
    }

    fn remove_node(&mut self, ino: u64) -> Option<Node> {
        let node = self.nodes.remove(&ino)?;
        if let Some(siblings) = self.children.get_mut(&node.parent) {
            siblings.remove(&node.name);
        }
        self.children.remove(&ino);
        Some(node)
    }

    pub fn get(&self, ino: u64) -> Option<&Node> {
        self.nodes.get(&ino)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<&Node> {
        let ino = self.children.get(&parent)?.get(name)?;
        self.nodes.get(ino)
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

    pub fn context_ids(&self) -> Vec<String> {
        self.ctx_inos.keys().cloned().collect()
    }

    pub fn context_ino(&self, ctx: &str) -> Option<u64> {
        self.ctx_inos.get(ctx).copied()
    }

    /// If ino is a schema dir (e.g. Notes under a context), return
    /// (context id, dir label). Used by the write path to classify targets.
    pub fn locate_schema_dir(&self, ino: u64) -> Option<(String, String)> {
        let node = self.get(ino)?;
        if !node.is_dir() || !SCHEMA_DIRS.contains(&node.name.as_str()) {
            return None;
        }
        // Context-rooted: schema dirs hang off ROOT, which is the context.
        if let Some(root_ctx) = &self.context_root {
            if node.parent == ROOT_INO {
                return Some((root_ctx.clone(), node.name.clone()));
            }
            return None;
        }
        // Global: schema dir -> context dir -> Contexts.
        let ctx_node = self.get(node.parent)?;
        if ctx_node.parent != CONTEXTS_INO {
            return None;
        }
        Some((ctx_node.name.clone(), node.name.clone()))
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
                    for dir in SCHEMA_DIRS {
                        let ino = self.alloc_ino();
                        self.insert_node(Node {
                            ino,
                            parent: ctx_ino,
                            name: dir.to_string(),
                            mtime: now,
                            content: NodeContent::Dir,
                        });
                    }
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

        // Resolve schema-dir inos for this context
        let mut dir_inos: HashMap<&str, u64> = HashMap::new();
        for dir in SCHEMA_DIRS {
            if let Some(node) = self.lookup(ctx_ino, dir) {
                dir_inos.insert(*dir, node.ino);
            }
        }

        // Render all docs and assign sticky filenames, deterministically (id order)
        let mut sorted: Vec<&Document> = docs.iter().collect();
        sorted.sort_by_key(|d| d.id);

        let workspace_id = self.ctx_workspaces.get(ctx_id).cloned();

        // (dir ino) -> name -> (doc id, content, mtime)
        let mut desired: HashMap<u64, BTreeMap<String, (u64, NodeContent, SystemTime)>> =
            HashMap::new();
        let mut taken: HashMap<u64, HashSet<String>> = HashMap::new();

        for doc in sorted {
            let rendered = render::render(doc);
            let Some(&dir_ino) = dir_inos.get(rendered.dir) else {
                continue;
            };
            let content = match rendered.content {
                render::Content::Inline(bytes) => NodeContent::Inline(Arc::new(bytes)),
                render::Content::Remote { size } => match &workspace_id {
                    Some(ws) => NodeContent::Remote {
                        workspace_id: ws.clone(),
                        doc_id: doc.id,
                        size,
                        checksum: doc.checksum.clone(),
                    },
                    // No workspace to address the content route — degrade to
                    // the document JSON rather than an unreadable entry
                    None => NodeContent::Inline(Arc::new(
                        serde_json::to_vec_pretty(&doc.data).unwrap_or_default(),
                    )),
                },
            };
            let taken = taken.entry(dir_ino).or_default();
            let persisted = names.get(ctx_id, rendered.dir, doc.id);
            let name = match persisted {
                Some(n) if !taken.contains(&n) => n,
                _ => {
                    let candidate = if taken.contains(&rendered.base_name) {
                        render::with_id_suffix(&rendered.base_name, doc.id)
                    } else {
                        rendered.base_name.clone()
                    };
                    if let Err(e) = names.put(ctx_id, rendered.dir, doc.id, &candidate) {
                        log::warn!("name store write failed: {e}");
                    }
                    candidate
                }
            };
            taken.insert(name.clone());
            desired
                .entry(dir_ino)
                .or_default()
                .insert(name, (doc.id, content, doc.updated_at));
        }

        // Diff each schema dir: remove gone entries, update changed, add new
        for &dir_ino in dir_inos.values() {
            let want = desired.remove(&dir_ino).unwrap_or_default();
            let have: Vec<(String, u64)> = self
                .children
                .get(&dir_ino)
                .map(|c| c.iter().map(|(n, i)| (n.clone(), *i)).collect())
                .unwrap_or_default();

            let mut dirty = false;
            for (name, ino) in have {
                match want.get(&name) {
                    Some((doc_id, content, mtime)) => {
                        let node = self.nodes.get_mut(&ino).unwrap();
                        let same_doc =
                            self.doc_inos.get(&(ctx_id.to_string(), *doc_id)) == Some(&ino);
                        if same_doc {
                            if node.content != *content {
                                node.content = content.clone();
                                node.mtime = *mtime;
                                inv.changed.push(ino);
                            }
                        } else {
                            // Same filename now belongs to a different document
                            self.remove_node(ino);
                            self.doc_inos
                                .retain(|(c, _), i| !(c == ctx_id && *i == ino));
                            inv.removed.push((dir_ino, ino, name.clone()));
                            dirty = true;
                        }
                    }
                    None => {
                        self.remove_node(ino);
                        self.doc_inos
                            .retain(|(c, _), i| !(c == ctx_id && *i == ino));
                        inv.removed.push((dir_ino, ino, name));
                        dirty = true;
                    }
                }
            }

            for (name, (doc_id, content, mtime)) in want {
                if self.lookup(dir_ino, &name).is_some() {
                    continue; // already present and up to date / refreshed above
                }
                let key = (ctx_id.to_string(), doc_id);
                let ino = match self.doc_inos.get(&key) {
                    Some(&ino) if self.nodes.contains_key(&ino) => {
                        // Doc moved (renamed file or schema dir); re-home it
                        let old = self.remove_node(ino).unwrap();
                        inv.removed.push((old.parent, ino, old.name));
                        ino
                    }
                    _ => self.alloc_ino(),
                };
                self.doc_inos.insert(key, ino);
                self.insert_node(Node {
                    ino,
                    parent: dir_ino,
                    name,
                    mtime,
                    content,
                });
                dirty = true;
            }

            if dirty {
                inv.dirty_dirs.push(dir_ino);
                if let Some(node) = self.nodes.get_mut(&dir_ino) {
                    node.mtime = SystemTime::now();
                }
            }
        }

        inv
    }

    // ── Workspace tree view ───────────────────────────────────────────────

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
            inv.dirty_dirs.push(ROOT_INO);
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
                        parent: ROOT_INO,
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

        let mut sorted: Vec<&Document> = docs.iter().collect();
        sorted.sort_by_key(|d| d.id);

        let mut desired: BTreeMap<String, (u64, NodeContent, SystemTime)> = BTreeMap::new();
        let mut taken: HashSet<String> = HashSet::new();
        for doc in sorted {
            let (base, content) = render::flat(doc);
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
