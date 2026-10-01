pub mod api;
pub mod blobs;
pub mod config;
pub mod events;
pub mod fsimpl;
pub mod mirror;
pub mod names;
pub mod nudge;
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

/// Explicit workspace sources. Empty selects the traditional full workspace.
/// One source is rooted directly; multiple sources retain Home/ and Trees/.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSelection {
    pub trees: Vec<String>,
    pub home: bool,
}

impl WorkspaceSelection {
    pub fn is_explicit(&self) -> bool {
        self.home || !self.trees.is_empty()
    }

    pub fn includes_home(&self) -> bool {
        !self.is_explicit() || self.home
    }

    pub fn includes_tree(&self, name: &str) -> bool {
        !self.is_explicit() || self.trees.iter().any(|t| t == name)
    }

    pub fn single_tree(&self) -> bool {
        !self.home && self.trees.len() == 1
    }
}

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
    /// Root the workspace context collection directly at the mountpoint.
    pub contexts_at_root: bool,
    /// When set, mount a workspace's trees (context + directory) read/write,
    /// mirroring each tree's path hierarchy. Mutually exclusive with contexts.
    pub workspace: Option<String>,
    /// Sources to expose; default preserves the full workspace layout.
    pub selection: WorkspaceSelection,
    /// The workspace a CONTEXT mount is scoped to. A mount is always one
    /// workspace; contexts belonging to any other are not materialized.
    pub context_workspace: Option<String>,
    /// In-memory blob cache budget for file content, in bytes
    pub blob_cache_bytes: usize,
    /// Emit inotify nudges (create+unlink of the virtual `.canvas-tmp`) so
    /// directory watchers see remote-driven view changes. See nudge.rs.
    pub enable_nudge: bool,
    /// `--mirror`: serve Home/ from a local mirror of the hub's
    /// `workspace:home` backend (workspace mounts only). See mirror/mod.rs.
    pub mirror: Option<mirror::MirrorOptions>,
}

/// A live mount. Dropping it (or calling unmount) tears everything down:
/// ws client, refresh threads, and the kernel mount itself.
pub struct MountHandle {
    session: Option<fuser::BackgroundSession>,
    job_tx: Sender<worker::Job>,
    stop: Arc<AtomicBool>,
    pub mountpoint: PathBuf,
    /// Mirror mode: the engine's inbox and the control socket (its file is
    /// removed on drop).
    mirror_tx: Option<Sender<mirror::sync::EngineMsg>>,
    control: Option<mirror::control::Control>,
    pub mirror: Option<Arc<mirror::sync::Mirror>>,
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
        if let Some(tx) = self.mirror_tx.take() {
            let _ = tx.send(mirror::sync::EngineMsg::Stop);
        }
        if let Some(m) = &self.mirror {
            m.write_status(true);
        }
        self.control.take();
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
    if opts.selection.is_explicit()
        && (opts.workspace.is_none() || opts.context_root.is_some() || opts.contexts.is_some())
    {
        anyhow::bail!("tree/backend selection requires a workspace mount without context views");
    }
    if opts.mirror.is_some() && (opts.workspace.is_none() || !opts.selection.includes_home()) {
        anyhow::bail!("mirror mode requires the workspace:home backend");
    }
    // Clear a stale mount left behind by a previous crash, then ensure the dir
    let _ = std::process::Command::new("fusermount3")
        .arg("-uz")
        .arg(&opts.mountpoint)
        .output();
    std::fs::create_dir_all(&opts.mountpoint)
        .with_context(|| format!("creating mountpoint {}", opts.mountpoint.display()))?;

    let names = Arc::new(names::NameStore::open(&opts.data_dir.join("names.redb"))?);
    let api = Arc::new(api::ApiClient::new(&opts.server, &opts.token)?);

    // Workspace mode roots the mount at a workspace's trees; resolve it up front
    // so the tree is built in the right mode. Live updates ride the
    // `workspace:<id>` ws channel (every tree/document change is forwarded there).
    let workspace_mode = opts.workspace.is_some();
    let mirror_mode = workspace_mode && opts.mirror.is_some();
    // A mirror must mount without the hub: the workspace id it learned on
    // its first online mount is kept in the data dir for that.
    let ws_id_file = opts.data_dir.join("workspace.json");
    let tree = Arc::new(RwLock::new(if let Some(ws_name) = &opts.workspace {
        let ws = match api.get_workspace(ws_name) {
            Ok(ws) => {
                if mirror_mode {
                    let _ = std::fs::create_dir_all(&opts.data_dir);
                    let _ = std::fs::write(
                        &ws_id_file,
                        serde_json::json!({ "id": ws.id, "name": ws.name }).to_string(),
                    );
                }
                ws
            }
            Err(e) if mirror_mode => {
                let cached = std::fs::read_to_string(&ws_id_file)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .and_then(|v| {
                        Some(api::WorkspaceInfo {
                            id: v.get("id")?.as_str()?.to_string(),
                            name: v.get("name")?.as_str()?.to_string(),
                        })
                    });
                match cached {
                    Some(ws) => {
                        log::warn!(
                            "workspace {ws_name}: hub unreachable ({e:#}); mounting the mirror offline"
                        );
                        ws
                    }
                    None => {
                        return Err(e).with_context(|| {
                            format!("resolving workspace {ws_name} (no offline record yet)")
                        })
                    }
                }
            }
            Err(e) => return Err(e).with_context(|| format!("resolving workspace {ws_name}")),
        };
        if !opts.selection.trees.is_empty() {
            match api.list_trees(&ws.id) {
                Ok(available) => {
                    for name in &opts.selection.trees {
                        anyhow::ensure!(
                            available.iter().any(|t| &t.name == name),
                            "tree `{name}` not found in workspace `{ws_name}`"
                        );
                    }
                }
                Err(e) if mirror_mode => {
                    log::warn!("tree selection cannot be checked while hub is unavailable: {e:#}");
                }
                Err(e) => return Err(e),
            }
        }
        state::Tree::workspace_selected(ws.id, ws.name, opts.selection.clone())
    } else {
        match &opts.context_root {
            Some(id) => state::Tree::context_rooted(id.clone()),
            None if opts.contexts_at_root => state::Tree::context_collection(),
            None => state::Tree::new(),
        }
    }));
    let enable_ws = opts.enable_ws;
    let context_filter: Option<HashSet<String>> = if workspace_mode {
        None
    } else {
        opts.contexts.as_ref().map(|c| c.iter().cloned().collect())
    };
    // Fail closed: an unavailable workspace must never expose other workspaces.
    let context_workspace_id: Option<String> = match (&opts.context_workspace, workspace_mode) {
        (Some(name), false) => Some(
            api.get_workspace(name)
                .with_context(|| format!("resolving context workspace {name}"))?
                .id,
        ),
        _ => None,
    };

    // Mirror: open the store and build Home from it before the mount exists,
    // so the first readdir is served locally whether or not the hub is up.
    // Bind the id first: a `tree.read()` inside the match scrutinee would
    // live for the whole match and deadlock the `tree.write()` below.
    let mirror_ws_id = tree.read().ws_id();
    let mirror: Option<Arc<mirror::sync::Mirror>> = match (&opts.mirror, mirror_ws_id) {
        (Some(mopts), Some(ws_id)) => {
            let device = mirror::DeviceIdentity::resolve();
            let m = mirror::sync::Mirror::open(mirror::sync::MirrorConfig {
                data_dir: opts.data_dir.clone(),
                server: opts.server.clone(),
                token: opts.token.clone(),
                workspace_id: ws_id,
                backend: mirror::DEFAULT_BACKEND.to_string(),
                opts: mopts.clone(),
                device,
                mountpoint: opts.mountpoint.clone(),
                status_path: Some(runtime::status_file_for(&opts.mountpoint)),
            })?;
            {
                let mut t = tree.write();
                t.set_home_mirrored(true);
                m.snapshot_into(&mut t);
            }
            log::info!(
                "mirror: {} entries from the store, device {}",
                m.store.entry_count(),
                m.device.id
            );
            Some(m)
        }
        _ => None,
    };

    // Populate before mounting so the first readdir is already correct.
    // Server being down is not fatal: the resync loop recovers.
    let bootstrap = worker::Worker {
        api: api.clone(),
        tree: tree.clone(),
        names: names.clone(),
        notifier: None,
        ensure_subscribed: None,
        context_filter: context_filter.clone(),
        context_workspace_id: context_workspace_id.clone(),
        refresh_lock: None,
        // No nudging before the mount exists (the syscalls would hit the
        // underlying directory).
        nudger: None,
        mirror_invalidations: None,
        mirror: None,
    };
    bootstrap.refresh_all();

    let blobs = blobs::BlobStore::new(api.clone(), opts.blob_cache_bytes, 4);
    let write_store = Arc::new(writes::WriteStore::new(
        api.clone(),
        tree.clone(),
        names.clone(),
        mirror.clone(),
    ));
    let fs = fsimpl::CanvasFs::new(
        tree.clone(),
        blobs,
        write_store.clone(),
        api.clone(),
        mirror.clone(),
    );
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

    // inotify nudge thread: the mount is up, so syscalls on it now reach the
    // session loop. Sender lives in the worker; worker exit closes the channel.
    let nudger = if opts.enable_nudge {
        Some(nudge::Nudger::spawn(opts.mountpoint.clone(), stop.clone())?)
    } else {
        None
    };
    // With a nudge thread to make the call, a departed document is held for a
    // real unlink so the kernel names it in an IN_DELETE — the only signal a
    // per-file watcher will act on. Without one, nothing would ever collect it.
    tree.write().set_deferred_removals(nudger.is_some());

    // Subscriber is created up front and shared: the worker (re)subscribes a
    // context through it after each successful refresh, and the ws supervisor
    // fills its client slot whenever the connection comes up. So a ws that
    // connects late, or a workspace that starts after mount, still ends up
    // subscribed — the first successful refresh triggers the subscribe.
    let subscriber = events::Subscriber::default();
    // Per-context resubscribe is a context-mode concern; workspace mounts
    // subscribe their single `workspace:<id>` channel on ws authenticate.
    let ensure_subscribed: Option<worker::NewContextCallback> = if enable_ws && !workspace_mode {
        let s = subscriber.clone();
        Some(Box::new(move |ctx_id: &str| s.subscribe(ctx_id)))
    } else {
        None
    };

    let mirror_invalidations = Arc::new(parking_lot::Mutex::new(Vec::new()));
    if let Some(m) = &mirror {
        m.attach_view(mirror::sync::ViewLink {
            tree: tree.clone(),
            refresh_lock: write_store.sync_handle(),
            invalidations: mirror_invalidations.clone(),
            job_tx: job_tx.clone(),
        });
    }
    let worker = worker::Worker {
        api,
        tree: tree.clone(),
        names,
        notifier: Some(session.notifier()),
        ensure_subscribed,
        context_filter,
        context_workspace_id,
        refresh_lock: Some(write_store.sync_handle()),
        nudger,
        mirror_invalidations: Some(mirror_invalidations),
        mirror: mirror.clone(),
    };
    std::thread::Builder::new()
        .name("canvas-fuse-worker".into())
        .spawn(move || worker.run(job_rx))?;

    // ws supervisor: retries the initial connect until it succeeds, then holds
    // the (auto-reconnecting) client until stop. Survives a server/workspace
    // that is still starting at mount time.
    if enable_ws {
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

    // Mirror engine + control socket, once everything they reach exists.
    let (mirror_tx, control) = match &mirror {
        Some(m) => {
            let tx = m.spawn_engine(stop.clone())?;
            let control = mirror::control::Control::spawn(
                &runtime::control_socket_for(&opts.mountpoint),
                m.clone(),
                stop.clone(),
            )?;
            (Some(tx), Some(control))
        }
        None => (None, None),
    };

    Ok(MountHandle {
        session: Some(session),
        job_tx,
        stop,
        mountpoint: opts.mountpoint,
        mirror_tx,
        control,
        mirror,
    })
}
