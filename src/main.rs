use anyhow::{Context as _, Result};
use canvas_fuse::{api::ApiClient, config, runtime, MountOptions};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;

/// Mount Canvas context views as live folders.
///
/// Contexts/<id>/{Tabs,Notes,Todos,Files,Emails,Links,Other}/ materialize the
/// documents of each context's current URL. Switching a context URL (from any
/// client) updates the folder contents in place.
#[derive(Parser, Debug)]
#[command(name = "canvas-fuse", version, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Connection options shared by commands that talk to a server. Resolution
/// order: flags > CANVAS_SERVER/CANVAS_API_TOKEN env > --remote from
/// ~/.canvas/config/remotes.json > boundRemote from cli-session.json.
#[derive(Args, Debug, Clone)]
struct ConnectArgs {
    /// Canvas server base URL, e.g. https://canvas.example
    #[arg(long)]
    server: Option<String>,

    /// API token (canvas-... or JWT)
    #[arg(long)]
    token: Option<String>,

    /// Named remote from ~/.canvas/config/remotes.json
    #[arg(long)]
    remote: Option<String>,
}

impl ConnectArgs {
    fn endpoint(&self) -> Result<config::Endpoint> {
        config::resolve(
            self.server.as_deref(),
            self.token.as_deref(),
            self.remote.as_deref(),
        )
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Mount context views at a directory
    Mount {
        /// Mountpoint directory (created if missing)
        mountpoint: PathBuf,

        #[command(flatten)]
        connect: ConnectArgs,

        /// Only mount specific context ids (repeatable)
        #[arg(short = 'c', long = "context")]
        contexts: Vec<String>,

        /// Run in the background (logs to the state dir)
        #[arg(short = 'd', long)]
        detach: bool,

        /// Disable the websocket event bridge (poll only)
        #[arg(long)]
        no_ws: bool,

        /// Full resync interval in seconds
        #[arg(long, default_value_t = 30)]
        resync: u64,

        /// Local state location (sticky filename map)
        #[arg(long, env = "CANVAS_FUSE_DATA_DIR")]
        data_dir: Option<PathBuf>,

        /// In-memory cache budget for file content, in MB
        #[arg(long, default_value_t = 256)]
        blob_cache_mb: usize,
    },

    /// Unmount a canvas mount and stop its daemon
    #[command(alias = "umount")]
    Unmount {
        /// Mountpoint directory
        mountpoint: PathBuf,
    },

    /// Show known canvas mounts and their health
    Status {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },

    /// Check server reachability, version and auth
    Ping {
        #[command(flatten)]
        connect: ConnectArgs,

        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },

    /// List accessible contexts
    Contexts {
        #[command(flatten)]
        connect: ConnectArgs,

        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mount {
            mountpoint,
            connect,
            contexts,
            detach,
            no_ws,
            resync,
            data_dir,
            blob_cache_mb,
        } => cmd_mount(
            mountpoint,
            connect,
            contexts,
            detach,
            no_ws,
            resync,
            data_dir,
            blob_cache_mb,
        ),
        Command::Unmount { mountpoint } => cmd_unmount(mountpoint),
        Command::Status { json } => cmd_status(json),
        Command::Ping { connect, json } => cmd_ping(connect, json),
        Command::Contexts { connect, json } => cmd_contexts(connect, json),
    }
}

fn init_logger() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("canvas_fuse=info"))
        .init();
}

#[allow(clippy::too_many_arguments)]
fn cmd_mount(
    mountpoint: PathBuf,
    connect: ConnectArgs,
    contexts: Vec<String>,
    detach: bool,
    no_ws: bool,
    resync: u64,
    data_dir: Option<PathBuf>,
    blob_cache_mb: usize,
) -> Result<()> {
    let endpoint = connect.endpoint()?;
    let data_dir = data_dir.unwrap_or_else(runtime::default_data_dir);

    // Canonicalize before any daemonize/fork so relative paths stay valid
    std::fs::create_dir_all(&mountpoint)
        .with_context(|| format!("creating mountpoint {}", mountpoint.display()))?;
    let mountpoint = mountpoint.canonicalize()?;

    // Refuse to steal a mountpoint from a live daemon
    if let Some(state) = runtime::read_state(&mountpoint) {
        if runtime::pid_alive(state.pid) && runtime::is_mounted(&mountpoint) {
            anyhow::bail!(
                "{} is already mounted by pid {} (canvas-fuse unmount first)",
                mountpoint.display(),
                state.pid
            );
        }
    }

    // Pre-flight while we can still report to the terminal
    let api = ApiClient::new(&endpoint.server, &endpoint.token)?;
    match api.ping() {
        Ok((payload, rtt)) => {
            let version = payload
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            eprintln!(
                "server {} (v{version}, {} ms, auth via {})",
                endpoint.server,
                rtt.as_millis(),
                endpoint.source
            );
        }
        Err(e) => eprintln!(
            "warning: server not reachable yet ({e:#}); mounting anyway, resync will recover"
        ),
    }

    let log_file = if detach {
        let log = runtime::default_log_file(&mountpoint);
        eprintln!("detaching; logs: {}", log.display());
        runtime::daemonize(&log)?;
        Some(log)
    } else {
        None
    };
    init_logger();

    let handle = canvas_fuse::mount(MountOptions {
        server: endpoint.server.clone(),
        token: endpoint.token,
        mountpoint: mountpoint.clone(),
        data_dir,
        enable_ws: !no_ws,
        resync_secs: resync,
        contexts: if contexts.is_empty() {
            None
        } else {
            Some(contexts.clone())
        },
        blob_cache_bytes: blob_cache_mb * 1024 * 1024,
    })?;

    runtime::write_state(&runtime::MountState {
        mountpoint: mountpoint.clone(),
        server: endpoint.server,
        pid: std::process::id(),
        started_at: chrono::Utc::now().to_rfc3339(),
        contexts: if contexts.is_empty() {
            None
        } else {
            Some(contexts)
        },
        log_file,
    })?;

    let (sig_tx, sig_rx) = std::sync::mpsc::channel::<()>();
    ctrlc::set_handler(move || {
        let _ = sig_tx.send(());
    })?;
    log::info!("ready");
    let _ = sig_rx.recv();
    runtime::remove_state(&mountpoint);
    handle.unmount();
    // rust_socketio's auto-reconnect thread can outlive disconnect() and would
    // keep this process pinned on a futex; the mount is gone, so exit hard
    std::process::exit(0);
}

fn cmd_unmount(mountpoint: PathBuf) -> Result<()> {
    init_logger();
    let mountpoint = mountpoint.canonicalize().unwrap_or(mountpoint);

    let state = runtime::read_state(&mountpoint);
    if let Some(state) = &state {
        if runtime::pid_alive(state.pid) {
            unsafe { libc::kill(state.pid as i32, libc::SIGTERM) };
            // Daemon unmounts and removes its own state file on SIGTERM
            for _ in 0..50 {
                if !runtime::is_mounted(&mountpoint) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // Mount gone is what matters; give the process a moment, then
            // make sure no half-dead daemon lingers
            for _ in 0..20 {
                if !runtime::pid_alive(state.pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if runtime::pid_alive(state.pid) {
                unsafe { libc::kill(state.pid as i32, libc::SIGKILL) };
            }
            if !runtime::is_mounted(&mountpoint) {
                println!("unmounted {}", mountpoint.display());
                runtime::remove_state(&mountpoint);
                return Ok(());
            }
            eprintln!("daemon did not release the mount, forcing");
        }
    }

    runtime::remove_state(&mountpoint);
    if runtime::is_mounted(&mountpoint) {
        let out = std::process::Command::new("fusermount3")
            .args(["-uz"])
            .arg(&mountpoint)
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "fusermount3 failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        println!("unmounted {} (forced)", mountpoint.display());
    } else if state.is_none() {
        println!("{} is not mounted", mountpoint.display());
    } else {
        println!("cleaned up stale mount {}", mountpoint.display());
    }
    Ok(())
}

fn cmd_status(as_json: bool) -> Result<()> {
    let mut entries = Vec::new();
    for state in runtime::list_states() {
        let alive = runtime::pid_alive(state.pid);
        let mounted = runtime::is_mounted(&state.mountpoint);
        if !alive && !mounted {
            // Crash leftover: clean the state file, report once as stale
            runtime::remove_state(&state.mountpoint);
        }
        entries.push((state, alive, mounted));
    }

    if as_json {
        let report: Vec<_> = entries
            .iter()
            .map(|(s, alive, mounted)| {
                json!({
                    "mountpoint": s.mountpoint,
                    "server": s.server,
                    "pid": s.pid,
                    "alive": alive,
                    "mounted": mounted,
                    "status": status_word(*alive, *mounted).trim(),
                    "startedAt": s.started_at,
                    "contexts": s.contexts,
                    "logFile": s.log_file,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("no canvas mounts");
        return Ok(());
    }
    for (s, alive, mounted) in entries {
        println!(
            "{}  {}  pid {}  {}  since {}{}",
            status_word(alive, mounted),
            s.mountpoint.display(),
            s.pid,
            s.server,
            s.started_at,
            s.contexts
                .as_ref()
                .map(|c| format!("  contexts: {}", c.join(",")))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn status_word(alive: bool, mounted: bool) -> &'static str {
    match (alive, mounted) {
        (true, true) => "ok      ",
        (true, false) => "broken  ", // daemon alive but kernel mount gone
        (false, true) => "orphaned", // mount present but daemon dead (ESTALE)
        (false, false) => "stale   ",
    }
}

fn cmd_ping(connect: ConnectArgs, as_json: bool) -> Result<()> {
    let endpoint = connect.endpoint()?;
    let api = ApiClient::new(&endpoint.server, &endpoint.token)?;

    let (payload, rtt) = api.ping()?;
    let auth_ok = api.list_contexts().map(|c| c.len());

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "server": endpoint.server,
                "reachable": true,
                "rttMs": rtt.as_millis() as u64,
                "version": payload.get("version"),
                "appName": payload.get("appName"),
                "auth": auth_ok.is_ok(),
                "contexts": auth_ok.as_ref().ok(),
                "source": endpoint.source,
            }))?
        );
        return Ok(());
    }

    println!(
        "{}: {} v{} ({} ms)",
        endpoint.server,
        payload
            .get("appName")
            .and_then(|v| v.as_str())
            .unwrap_or("canvas-server"),
        payload
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        rtt.as_millis()
    );
    match auth_ok {
        Ok(n) => println!("auth ok ({}, {n} contexts accessible)", endpoint.source),
        Err(e) => println!("auth FAILED ({}): {e:#}", endpoint.source),
    }
    Ok(())
}

fn cmd_contexts(connect: ConnectArgs, as_json: bool) -> Result<()> {
    let endpoint = connect.endpoint()?;
    let api = ApiClient::new(&endpoint.server, &endpoint.token)?;
    let contexts = api.list_contexts()?;

    if as_json {
        let raw: Vec<_> = contexts.iter().map(|c| &c.raw).collect();
        println!("{}", serde_json::to_string_pretty(&raw)?);
        return Ok(());
    }
    for ctx in contexts {
        println!("{}\t{}", ctx.id, ctx.url);
    }
    Ok(())
}
