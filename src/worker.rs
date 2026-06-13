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
}

/// Notifies the ws layer to subscribe to a newly discovered context's channel.
pub type NewContextCallback = Box<dyn Fn(&str) + Send + Sync>;

pub struct Worker {
    pub api: Arc<ApiClient>,
    pub tree: Arc<RwLock<Tree>>,
    pub names: Arc<NameStore>,
    pub notifier: Option<Notifier>,
    /// Called when a refresh discovers a context that wasn't mounted yet,
    /// so the ws layer can subscribe to its event channel.
    pub on_new_context: Option<NewContextCallback>,
    /// When set, only these context ids are materialized (agent containers
    /// typically mount a single context).
    pub context_filter: Option<HashSet<String>>,
    /// Held across fetch+apply so refreshes serialize with write-path tree
    /// mutations (WriteStore::sync_handle).
    pub refresh_lock: Option<Arc<parking_lot::Mutex<()>>>,
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

    pub fn refresh_all(&self) {
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
        let (inv, added) = {
            let mut tree = self.tree.write();
            tree.apply_contexts(&contexts)
        };
        self.notify(inv);
        if let Some(cb) = &self.on_new_context {
            for ctx_id in &added {
                cb(ctx_id);
            }
        }
        for ctx in &contexts {
            self.refresh_context(&ctx.id);
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
    }

    // Push invalidations into the kernel. Errors are expected noise: ENOENT
    // just means the kernel had nothing cached for that entry.
    fn notify(&self, inv: Invalidation) {
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
    }
}
