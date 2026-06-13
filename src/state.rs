use crate::api::{ContextInfo, Document};
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
        size: u64,
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
            NodeContent::Remote { size, .. } => *size,
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
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    pub fn new() -> Self {
        let mut t = Self {
            nodes: HashMap::new(),
            children: HashMap::new(),
            next_ino: FIRST_DYNAMIC_INO,
            doc_inos: HashMap::new(),
            ctx_inos: HashMap::new(),
            ctx_workspaces: HashMap::new(),
        };
        let now = SystemTime::now();
        t.insert_node(Node {
            ino: ROOT_INO,
            parent: ROOT_INO,
            name: String::new(),
            mtime: now,
            content: NodeContent::Dir,
        });
        t.insert_node(Node {
            ino: CONTEXTS_INO,
            parent: ROOT_INO,
            name: "Contexts".to_string(),
            mtime: now,
            content: NodeContent::Dir,
        });
        t
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
            if let Some(node) = self.remove_node(ino) {
                inv.removed.push((node.parent, ino, node.name));
            }
            self.doc_inos.retain(|(c, _), _| c != &ctx_id);
            self.ctx_workspaces.remove(&ctx_id);
            inv.dirty_dirs.push(CONTEXTS_INO);
        }

        for ctx in contexts {
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
                    let ctx_ino = self.alloc_ino();
                    self.ctx_inos.insert(ctx.id.clone(), ctx_ino);
                    if let Some(ws) = &ctx.workspace_id {
                        self.ctx_workspaces.insert(ctx.id.clone(), ws.clone());
                    }
                    self.insert_node(Node {
                        ino: ctx_ino,
                        parent: CONTEXTS_INO,
                        name: ctx.id.clone(),
                        mtime: now,
                        content: NodeContent::Dir,
                    });
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
                    inv.dirty_dirs.push(CONTEXTS_INO);
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
