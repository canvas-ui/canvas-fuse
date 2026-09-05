use crate::api::ApiClient;
use crate::names::NameStore;
use crate::state::{Invalidation, Tree};
use fuser::Notifier;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Job {
    RefreshAll,
    RefreshContext(String),
    /// The mirror engine changed Home nodes; push its queued invalidations
    /// to the kernel (and the nudge thread) from the worker, which owns the
    /// notifier.
    MirrorInvalidations,
    /// A `backend.changed` / conflict event on the socket: wake the mirror.
    MirrorNudge,
    /// The socket re-authenticated: the hub is back, tell the mirror.
    MirrorReconnect,
}

/// Notifies the ws layer to subscribe to a newly discovered context's channel.
pub type NewContextCallback = Box<dyn Fn(&str) + Send + Sync>;

pub struct Worker {
    pub api: Arc<ApiClient>,
    pub tree: Arc<RwLock<Tree>>,
    pub names: Arc<NameStore>,
    pub notifier: Option<Notifier>,
    /// Idempotently ensure a context's ws channel is subscribed. Called after a
    /// *successful* document refresh (the workspace is then up, so the server
    /// will accept the subscribe — unlike at mount time when it may be down).
    pub ensure_subscribed: Option<NewContextCallback>,
    /// When set, only these context ids are materialized (agent containers
    /// typically mount a single context).
    pub context_filter: Option<HashSet<String>>,
    /// Workspace id a context mount is scoped to. A mount is one workspace, so
    /// contexts belonging to another are not materialized even though the
    /// contexts API is user-wide.
    pub context_workspace_id: Option<String>,
    /// Held across fetch+apply so refreshes serialize with write-path tree
    /// mutations (WriteStore::sync_handle).
    pub refresh_lock: Option<Arc<parking_lot::Mutex<()>>>,
    /// Emits inotify events for directory watchers after a remote-driven view
    /// change (see nudge.rs). None on the pre-mount bootstrap worker and under
    /// --no-nudge.
    pub nudger: Option<crate::nudge::Nudger>,
    /// Mirror mode: the engine's queued Home invalidations, drained on
    /// `Job::MirrorInvalidations`, and the engine itself for nudges.
    pub mirror_invalidations: Option<Arc<parking_lot::Mutex<Vec<Invalidation>>>>,
    pub mirror: Option<Arc<crate::mirror::sync::Mirror>>,
}

impl Worker {
    pub fn run(self, rx: Receiver<Job>) {
        while let Ok(first) = rx.recv() {
            // Debounce: drain everything queued and dedupe before acting,
            // a burst of document events should cause one refetch, not N
            let mut jobs: HashSet<Job> = HashSet::new();
            jobs.insert(first);
            while let Ok(job) = rx.try_recv() {
                jobs.insert(job);
            }
            // Mirror jobs are cheap and independent of the refresh below.
            if jobs.remove(&Job::MirrorInvalidations) {
                self.flush_mirror_invalidations();
            }
            if jobs.remove(&Job::MirrorReconnect) {
                if let Some(m) = &self.mirror {
                    m.reconnect();
                }
            }
            if jobs.remove(&Job::MirrorNudge) {
                if let Some(m) = &self.mirror {
                    m.wake();
                }
            }
            if jobs.is_empty() {
                continue;
            }
            if jobs.contains(&Job::RefreshAll) {
                self.refresh_all();
            } else {
                for job in jobs {
                    if let Job::RefreshContext(ctx) = job {
                        self.refresh_context(&ctx);
                    }
                }
            }
        }
        log::debug!("worker channel closed, exiting");
    }

    fn flush_mirror_invalidations(&self) {
        let Some(queue) = &self.mirror_invalidations else {
            return;
        };
        let pending: Vec<Invalidation> = std::mem::take(&mut *queue.lock());
        for inv in pending {
            self.notify(inv);
        }
    }

    pub fn refresh_all(&self) {
        if self.tree.read().is_workspace() {
            self.refresh_workspace();
            return;
        }
        let mut contexts = match self.api.list_contexts() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("context list fetch failed: {e:#}");
                return;
            }
        };
        if let Some(filter) = &self.context_filter {
            contexts.retain(|c| filter.contains(&c.id));
        }
        if let Some(ws_id) = &self.context_workspace_id {
            contexts.retain(|c| c.workspace_id.as_deref() == Some(ws_id.as_str()));
        }
        let (inv, added) = {
            let mut tree = self.tree.write();
            tree.apply_contexts(&contexts)
        };
        self.notify(inv);
        let _ = added; // subscription now happens after a successful per-context refresh
        for ctx in &contexts {
            self.refresh_context(&ctx.id);
        }
    }

    /// Workspace mount refresh: reconcile trees, each tree's path hierarchy,
    /// then the documents at every path. Held under the write-sync lock so a
    /// concurrent write doesn't get diffed away mid-mutation.
    fn refresh_workspace(&self) {
        let ws = match self.tree.read().ws_id() {
            Some(id) => id,
            None => return,
        };
        let _guard = self.refresh_lock.as_ref().map(|l| l.lock());

        let trees = match self.api.list_trees(&ws) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("workspace {ws}: tree list fetch failed: {e:#}");
                return;
            }
        };
        let inv = self.tree.write().apply_trees(&trees);
        self.notify(inv);

        // Reconcile each tree's directory hierarchy from its path list.
        for t in &trees {
            match self.api.list_tree_paths(&ws, &t.id) {
                Ok(paths) => {
                    let inv = self.tree.write().apply_tree_paths(&t.name, &paths);
                    self.notify(inv);
                }
                Err(e) => log::warn!("workspace {ws} tree {}: paths fetch failed: {e:#}", t.name),
            }
        }

        // The trash is a root of its own, not a tree path.
        match self.api.list_trash(&ws) {
            Ok(docs) => {
                let inv = self.tree.write().apply_trash_documents(&docs);
                self.notify(inv);
            }
            Err(e) => log::warn!("workspace {ws}: trash fetch failed: {e:#}"),
        }

        // Populate documents at every known path. Snapshot the path list into an
        // owned Vec first: holding a read guard as the for-loop iterator
        // temporary would deadlock against the per-path tree.write() below
        // (parking_lot is not reentrant).
        let paths = self.tree.read().ws_paths();
        for (tree_name, path) in paths {
            let meta = self.tree.read().ws_tree_meta(&tree_name);
            let Some((tree_id, tree_type)) = meta else {
                continue;
            };
            match self
                .api
                .list_tree_documents(&ws, &tree_id, &tree_type, &path)
            {
                Ok(docs) => {
                    let inv = self
                        .tree
                        .write()
                        .apply_tree_documents(&tree_name, &path, &docs);
                    self.notify(inv);
                }
                Err(e) => {
                    log::warn!(
                        "workspace {ws} tree {tree_name} path {path}: docs fetch failed: {e:#}"
                    )
                }
            }
        }
    }

    pub fn refresh_context(&self, ctx_id: &str) {
        // Refresh .context.json first so a URL switch is immediately readable
        if let Ok(ctx) = self.api.get_context(ctx_id) {
            let inv = {
                let mut tree = self.tree.write();
                tree.update_context_meta(&ctx)
            };
            self.notify(inv);
        }
        // Fetch inside the lock: a list fetched before a concurrent write but
        // applied after it would diff against a stale view.
        let _guard = self.refresh_lock.as_ref().map(|l| l.lock());
        let docs = match self.api.list_documents(ctx_id) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("document fetch for context {ctx_id} failed: {e:#}");
                return;
            }
        };
        log::info!("context {ctx_id}: {} documents in view", docs.len());
        let inv = {
            let mut tree = self.tree.write();
            tree.apply_documents(ctx_id, &docs, &self.names)
        };
        drop(_guard);
        self.notify(inv);
        // Docs fetched OK ⇒ the workspace is up ⇒ a subscribe will be accepted.
        // Idempotent (confirmed set), so this is a no-op once subscribed.
        if let Some(cb) = &self.ensure_subscribed {
            cb(ctx_id);
        }
    }

    // Push invalidations into the kernel. Errors are expected noise: ENOENT
    // just means the kernel had nothing cached for that entry.
    fn notify(&self, inv: Invalidation) {
        // Tombstoned documents are still in the tree so a real unlink can name
        // them. With no nudge thread to make that call, nothing ever would —
        // collect them here instead, or the view keeps every departed document
        // forever.
        if self.nudger.is_none() && !inv.vanished.is_empty() {
            let mut tree = self.tree.write();
            for (_, ino, _) in &inv.vanished {
                tree.drop_tombstoned(*ino);
            }
        }
        let Some(notifier) = &self.notifier else {
            return;
        };
        if inv.is_empty() {
            return;
        }
        for (parent, child, name) in &inv.removed {
            if let Err(e) = notifier.delete(*parent, *child, &OsString::from(name)) {
                log::trace!("notify delete {name}: {e}");
            }
        }
        // Without a nudge thread these were just dropped above, so the kernel
        // still has to be told. With one, the unlink itself does that — telling
        // the kernel first would drop the dentry and the unlink would 404.
        if self.nudger.is_none() {
            for (parent, child, name) in &inv.vanished {
                if let Err(e) = notifier.delete(*parent, *child, &OsString::from(name)) {
                    log::trace!("notify delete {name}: {e}");
                }
            }
        }
        for ino in &inv.changed {
            if let Err(e) = notifier.inval_inode(*ino, 0, -1) {
                log::trace!("notify inval inode {ino}: {e}");
            }
        }
        for ino in &inv.dirty_dirs {
            if let Err(e) = notifier.inval_inode(*ino, 0, -1) {
                log::trace!("notify inval dir {ino}: {e}");
            }
        }
        log::debug!(
            "kernel notified: {} removed, {} changed, {} dirty dirs",
            inv.removed.len(),
            inv.changed.len(),
            inv.dirty_dirs.len()
        );

        // Reverse invalidation generates no fsnotify events, so directory
        // watchers (Obsidian, file managers) still see nothing — queue an
        // inotify nudge for every affected directory. Queueing never blocks;
        // the syscalls happen on the nudge thread (this thread may hold
        // refresh_lock here, which the session loop can be waiting on).
        if let Some(nudger) = &self.nudger {
            let mut dirs: HashSet<u64> = inv.dirty_dirs.iter().copied().collect();
            dirs.extend(inv.removed.iter().map(|(parent, _, _)| *parent));
            let tree = self.tree.read();
            // A changed file's watchers get IN_MODIFY-ish signal via its
            // parent: rescanning watchers re-stat and pick up the new content.
            for ino in &inv.changed {
                if let Some(node) = tree.get(*ino) {
                    dirs.insert(node.parent);
                }
            }
            for ino in dirs {
                if let Some(rel) = tree.path_of(ino) {
                    nudger.nudge(rel);
                }
            }
            // And an event that NAMES each file that appeared or changed. The
            // marker alone reaches only watchers that re-list a directory on
            // any signal; one that handles events per file (Obsidian) discards
            // it — an unknown path, already gone by the time it stats — and so
            // never learns that twenty documents just arrived.
            for ino in inv.added.iter().chain(inv.changed.iter()) {
                if let Some(rel) = tree.path_of(*ino) {
                    nudger.touch_file(rel);
                }
            }
            // And a real unlink for each document that left, which is the only
            // way the kernel names it in an IN_DELETE.
            for (_, ino, _) in &inv.vanished {
                if let Some(rel) = tree.path_of(*ino) {
                    nudger.unlink_file(rel);
                }
            }
        }
    }
}
