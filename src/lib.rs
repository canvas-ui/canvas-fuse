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
    /// In-memory blob cache budget for file content, in bytes
    pub blob_cache_bytes: usize,
}

/// A live mount. Dropping it (or calling unmount) tears everything down:
/// ws client, refresh threads, and the kernel mount itself.
pub struct MountHandle {
    session: Option<fuser::BackgroundSession>,
    ws: Option<events::EventClient>,
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
        self.stop.store(true, Ordering::Relaxed);
        // Kernel mount first — the user-visible part must not wait on network
        // cleanup; rust_socketio teardown can block on its reconnect thread
        if let Some(session) = self.session.take() {
            drop(session); // joins the FUSE thread and unmounts
        }
        log::info!("unmounted {}", self.mountpoint.display());
        if let Some(ws) = self.ws.take() {
            ws.disconnect();
        }
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
    let tree = Arc::new(RwLock::new(state::Tree::new()));
    let context_filter: Option<HashSet<String>> =
        opts.contexts.as_ref().map(|c| c.iter().cloned().collect());

    // Populate before mounting so the first readdir is already correct.
    // Server being down is not fatal: the resync loop recovers.
    let bootstrap = worker::Worker {
        api: api.clone(),
        tree: tree.clone(),
        names: names.clone(),
        notifier: None,
        on_new_context: None,
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

    let ws = if opts.enable_ws {
        match events::connect(&opts.server, &opts.token, job_tx.clone(), tree.clone()) {
            Ok(ws) => Some(ws),
            Err(e) => {
                log::warn!("ws connect failed, falling back to polling only: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let on_new_context: Option<worker::NewContextCallback> = ws.as_ref().map(|ws| {
        let subscriber = ws.subscriber.clone();
        Box::new(move |ctx_id: &str| subscriber.subscribe(ctx_id)) as _
    });

    let worker = worker::Worker {
        api,
        tree,
        names,
        notifier: Some(session.notifier()),
        on_new_context,
        context_filter,
        refresh_lock: Some(write_store.sync_handle()),
    };
    std::thread::Builder::new()
        .name("canvas-fuse-worker".into())
        .spawn(move || worker.run(job_rx))?;

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
        ws,
        job_tx,
        stop,
        mountpoint: opts.mountpoint,
    })
}
