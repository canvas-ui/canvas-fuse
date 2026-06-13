pub mod api;
pub mod blobs;
pub mod config;
pub mod events;
pub mod fsimpl;
pub mod names;
pub mod render;
pub mod runtime;
pub mod state;
pub mod worker;
pub mod writes;

use anyhow::{Context as _, Result};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

pub struct MountOptions {
    pub server: String,
    pub token: String,
    pub mountpoint: PathBuf,
    pub data_dir: PathBuf,
    pub enable_ws: bool,
    pub resync_secs: u64,
    /// Only materialize these context ids (None = all accessible contexts)
    pub contexts: Option<Vec<String>>,
    /// When set, the mount is rooted at this single context (its schema dirs at
    /// the mount's top level, no `Contexts/` wrapper).
    pub context_root: Option<String>,
    /// In-memory blob cache budget for file content, in bytes
    pub blob_cache_bytes: usize,
}

/// A live mount. Dropping it (or calling unmount) tears everything down:
/// ws client, refresh threads, and the kernel mount itself.
pub struct MountHandle {
    session: Option<fuser::BackgroundSession>,
    job_tx: Sender<worker::Job>,
    stop: Arc<AtomicBool>,
    pub mountpoint: PathBuf,
}

impl MountHandle {
    pub fn refresh(&self) {
        let _ = self.job_tx.send(worker::Job::RefreshAll);
    }

    pub fn unmount(mut self) {
        self.teardown();
    }

    fn teardown(&mut self) {
        // Signals the ws supervisor and resync threads to stop; the supervisor
        // disconnects its ws client on seeing this.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(session) = self.session.take() {
            drop(session); // joins the FUSE thread and unmounts
        }
        log::info!("unmounted {}", self.mountpoint.display());
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        if self.session.is_some() {
            self.teardown();
        }
    }
}

pub fn mount(opts: MountOptions) -> Result<MountHandle> {
    // Clear a stale mount left behind by a previous crash, then ensure the dir
    let _ = std::process::Command::new("fusermount3")
        .arg("-uz")
        .arg(&opts.mountpoint)
        .output();
    std::fs::create_dir_all(&opts.mountpoint)
        .with_context(|| format!("creating mountpoint {}", opts.mountpoint.display()))?;

    let names = Arc::new(names::NameStore::open(&opts.data_dir.join("names.redb"))?);
    let api = Arc::new(api::ApiClient::new(&opts.server, &opts.token)?);
    let tree = Arc::new(RwLock::new(match &opts.context_root {
        Some(id) => state::Tree::context_rooted(id.clone()),
        None => state::Tree::new(),
    }));
    let context_filter: Option<HashSet<String>> =
        opts.contexts.as_ref().map(|c| c.iter().cloned().collect());

    // Populate before mounting so the first readdir is already correct.
    // Server being down is not fatal: the resync loop recovers.
    let bootstrap = worker::Worker {
        api: api.clone(),
        tree: tree.clone(),
        names: names.clone(),
        notifier: None,
        ensure_subscribed: None,
        context_filter: context_filter.clone(),
        refresh_lock: None,
    };
    bootstrap.refresh_all();

    let blobs = blobs::BlobStore::new(api.clone(), opts.blob_cache_bytes, 4);
    let write_store = Arc::new(writes::WriteStore::new(
        api.clone(),
        tree.clone(),
        names.clone(),
    ));
    let fs = fsimpl::CanvasFs::new(tree.clone(), blobs, write_store.clone());
    let session = fuser::spawn_mount2(
        fs,
        &opts.mountpoint,
        &[
            fuser::MountOption::FSName("canvas".to_string()),
            fuser::MountOption::Subtype("canvasfs".to_string()),
        ],
    )
    .with_context(|| format!("mounting {}", opts.mountpoint.display()))?;
    log::info!("mounted canvas at {}", opts.mountpoint.display());

    let (job_tx, job_rx) = std::sync::mpsc::channel::<worker::Job>();
    let stop = Arc::new(AtomicBool::new(false));

    // Subscriber is created up front and shared: the worker (re)subscribes a
    // context through it after each successful refresh, and the ws supervisor
    // fills its client slot whenever the connection comes up. So a ws that
    // connects late, or a workspace that starts after mount, still ends up
    // subscribed — the first successful refresh triggers the subscribe.
    let subscriber = events::Subscriber::default();
    let ensure_subscribed: Option<worker::NewContextCallback> = if opts.enable_ws {
        let s = subscriber.clone();
        Some(Box::new(move |ctx_id: &str| s.subscribe(ctx_id)))
    } else {
        None
    };

    let worker = worker::Worker {
        api,
        tree: tree.clone(),
        names,
        notifier: Some(session.notifier()),
        ensure_subscribed,
        context_filter,
        refresh_lock: Some(write_store.sync_handle()),
    };
    std::thread::Builder::new()
        .name("canvas-fuse-worker".into())
        .spawn(move || worker.run(job_rx))?;

    // ws supervisor: retries the initial connect until it succeeds, then holds
    // the (auto-reconnecting) client until stop. Survives a server/workspace
    // that is still starting at mount time.
    if opts.enable_ws {
        let server = opts.server.clone();
        let token = opts.token.clone();
        let ws_tx = job_tx.clone();
        let ws_tree = tree.clone();
        let ws_stop = stop.clone();
        std::thread::Builder::new()
            .name("canvas-fuse-ws".into())
            .spawn(move || events::supervise(server, token, ws_tx, ws_tree, subscriber, ws_stop))?;
    }

    // Periodic resync: belt and braces under ws, sole refresh path without it
    let resync_tx = job_tx.clone();
    let resync_stop = stop.clone();
    let interval = Duration::from_secs(opts.resync_secs.max(5));
    std::thread::Builder::new()
        .name("canvas-fuse-resync".into())
        .spawn(move || loop {
            std::thread::sleep(interval);
            if resync_stop.load(Ordering::Relaxed) {
                break;
            }
            if resync_tx.send(worker::Job::RefreshAll).is_err() {
                break;
            }
        })?;

    Ok(MountHandle {
        session: Some(session),
        job_tx,
        stop,
        mountpoint: opts.mountpoint,
    })
}
