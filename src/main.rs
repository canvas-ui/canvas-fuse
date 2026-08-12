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
    /// Mount a workspace (or one of its context views) at a directory
    Mount {
        /// With two arguments, what to mount: <workspace>,
        /// <workspace>/Contexts, or <workspace>/Contexts/<id>. With one, this
        /// IS the mountpoint and the target comes from -w / -c / --root.
        #[arg(value_name = "SELECTOR")]
        selector: Option<String>,

        /// Mountpoint directory (created if missing)
        #[arg(value_name = "MOUNTPOINT")]
        mountpoint: Option<PathBuf>,

        #[command(flatten)]
        connect: ConnectArgs,

        #[arg(
            long = "root",
            value_name = "SELECTOR",
            conflicts_with_all = ["contexts", "workspace"],
            help = "What the mount is rooted at (default: Contexts)",
            long_help = "What the mount is rooted at. A mount always scopes to ONE\n\
                workspace.\n\n\
                <workspace>                  Trees/ and Trash/ at the top,\n\
                \x20                            mounted at <mountpoint>/<workspace>/\n\
                <workspace>/Contexts         that workspace's context views\n\
                <workspace>/Contexts/<id>    one context, its schema dirs at the top"
        )]
        root: Option<String>,

        /// Mount a context view, as <workspace>/<context> (or a bare context id
        /// alongside -w). Repeatable; a single -c roots the mount at it.
        #[arg(short = 'c', long = "context")]
        contexts: Vec<String>,

        /// Mount a workspace — Trees/ and Trash/ at the top, at
        /// <mountpoint>/<workspace>/. Equivalent to `--root <workspace>`.
        #[arg(short = 'w', long = "workspace")]
        workspace: Option<String>,

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
            selector,
            mountpoint,
            connect,
            root,
            contexts,
            workspace,
            detach,
            no_ws,
            resync,
            data_dir,
            blob_cache_mb,
        } => {
            // `mount <selector> <mountpoint>` vs `mount <mountpoint>`: clap
            // cannot express "optional first positional", so the pair is
            // resolved here by how many were given.
            let (selector, mountpoint) = match (selector, mountpoint) {
                (Some(sel), Some(path)) => (Some(sel), path),
                (Some(only), None) => (None, PathBuf::from(only)),
                (None, Some(path)) => (None, path),
                (None, None) => anyhow::bail!("a mountpoint is required"),
            };
            let (workspace, contexts) = resolve_root(selector.or(root), workspace, contexts)?;
            cmd_mount(
                mountpoint,
                connect,
                contexts,
                workspace,
                detach,
                no_ws,
                resync,
                data_dir,
                blob_cache_mb,
            )
        }
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
/// Resolve what to mount into the (workspace, contexts) pair the mount speaks.
///
/// A mount always scopes to ONE workspace — contexts are addressed inside it,
/// never across workspaces. The selector may arrive as a positional argument,
/// as `--root`, or as the `-w` / `-c` flags:
///
///   myws                      the workspace: Trees/ and Trash/
///   myws/Contexts             that workspace's context views
///   myws/Contexts/foo         one context, rooted
///   -w myws                   same as `myws`
///   -c myws/foo               same as `myws/Contexts/foo`
///   -c foo -w myws            a bare context id alongside its workspace
fn resolve_root(
    selector: Option<String>,
    workspace: Option<String>,
    contexts: Vec<String>,
) -> Result<(Option<String>, Vec<String>)> {
    if let Some(selector) = selector {
        let trimmed = selector.trim_matches('/');
        let mut parts = trimmed.split('/').filter(|p| !p.is_empty());
        let Some(ws) = parts.next() else {
            anyhow::bail!("empty mount selector");
        };
        let rest: Vec<&str> = parts.collect();

        return match rest.as_slice() {
            // The whole workspace.
            [] => Ok((Some(ws.to_string()), Vec::new())),
            // Its context views, optionally rooted at one of them.
            [section] if section.eq_ignore_ascii_case("contexts") => {
                Ok((Some(ws.to_string()), Vec::new()))
            }
            [section, id] if section.eq_ignore_ascii_case("contexts") => {
                Ok((Some(ws.to_string()), vec![(*id).to_string()]))
            }
            _ => anyhow::bail!(
                "cannot mount `{selector}`: expected <workspace>, \
                 <workspace>/Contexts or <workspace>/Contexts/<id>"
            ),
        };
    }

    // Flag form. `-c <workspace>/<id>` carries its workspace the same way.
    let mut ws = workspace;
    let mut ids = Vec::new();
    for entry in contexts {
        match entry.trim_matches('/').split_once('/') {
            Some((entry_ws, id)) => {
                if ws.as_deref().is_some_and(|w| w != entry_ws) {
                    anyhow::bail!(
                        "one mount is one workspace: got both `{}` and `{entry_ws}`",
                        ws.unwrap()
                    );
                }
                ws = Some(entry_ws.to_string());
                ids.push(id.to_string());
            }
            None => ids.push(entry),
        }
    }
    Ok((ws, ids))
}

// Flat CLI plumbing: one parameter per mount flag, folded into MountOptions below.
#[allow(clippy::too_many_arguments)]
fn cmd_mount(
    mountpoint: PathBuf,
    connect: ConnectArgs,
    contexts: Vec<String>,
    workspace: Option<String>,
    detach: bool,
    no_ws: bool,
    resync: u64,
    data_dir: Option<PathBuf>,
    blob_cache_mb: usize,
) -> Result<()> {
    let endpoint = connect.endpoint()?;

    // A mount is one workspace. Naming a context inside it (`myws/Contexts/foo`)
    // mounts the CONTEXT view, not the workspace tree view — so the workspace
    // only selects the mount shape when no context was asked for.
    let workspace_mount = contexts.is_empty().then(|| workspace.clone()).flatten();
    let context_root = if contexts.len() == 1 {
        Some(contexts[0].clone())
    } else {
        None
    };
    let mountpoint = match (&workspace_mount, &context_root) {
        (Some(ws), _) => mountpoint.join(ws),
        (None, Some(ctx)) => mountpoint.join(ctx),
        (None, None) => mountpoint,
    };

    // Canonicalize before any daemonize/fork so relative paths stay valid
    std::fs::create_dir_all(&mountpoint)
        .with_context(|| format!("creating mountpoint {}", mountpoint.display()))?;
    let mountpoint = mountpoint.canonicalize()?;

    // Per-mount state dir (own sticky-name redb). Explicit --data-dir wins;
    // otherwise derive from remote + context so concurrent mounts don't share
    // (and lock) one redb. Remote label: --remote name, else the server host.
    let data_dir = data_dir.unwrap_or_else(|| {
        let remote_label = connect.remote.clone().unwrap_or_else(|| {
            endpoint
                .server
                .rsplit("://")
                .next()
                .unwrap_or(&endpoint.server)
                .split('/')
                .next()
                .unwrap_or("server")
                .to_string()
        });
        match &workspace_mount {
            Some(ws) => runtime::workspace_data_dir(&remote_label, ws),
            None => runtime::mount_data_dir(&remote_label, &contexts, &mountpoint),
        }
    });

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
        context_root: context_root.clone(),
        workspace: workspace_mount.clone(),
        context_workspace: workspace.clone(),
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

#[cfg(test)]
mod root_selector_tests {
    use super::resolve_root;

    fn resolved(
        selector: Option<&str>,
        workspace: Option<&str>,
        contexts: &[&str],
    ) -> (Option<String>, Vec<String>) {
        resolve_root(
            selector.map(str::to_string),
            workspace.map(str::to_string),
            contexts.iter().map(|c| c.to_string()).collect(),
        )
        .expect("selector should resolve")
    }

    #[test]
    fn a_bare_name_mounts_that_workspace() {
        assert_eq!(
            resolved(Some("myws"), None, &[]),
            (Some("myws".into()), vec![])
        );
        assert_eq!(
            resolved(Some("myws/Contexts"), None, &[]),
            (Some("myws".into()), vec![])
        );
    }

    #[test]
    fn a_context_selector_roots_at_one_context_of_that_workspace() {
        assert_eq!(
            resolved(Some("myws/Contexts/foo"), None, &[]),
            (Some("myws".into()), vec!["foo".to_string()])
        );
        // Typing noise carries no meaning.
        assert_eq!(
            resolved(Some("/myws/contexts/foo/"), None, &[]),
            (Some("myws".into()), vec!["foo".to_string()])
        );
    }

    #[test]
    fn the_flag_forms_agree_with_the_selector() {
        assert_eq!(
            resolved(None, Some("myws"), &[]),
            (Some("myws".into()), vec![])
        );
        // -c takes the workspace-qualified form...
        assert_eq!(
            resolved(None, None, &["myws/foo"]),
            (Some("myws".into()), vec!["foo".to_string()])
        );
        // ...or a bare id next to -w.
        assert_eq!(
            resolved(None, Some("myws"), &["foo"]),
            (Some("myws".into()), vec!["foo".to_string()])
        );
    }

    #[test]
    fn a_mount_is_one_workspace() {
        // Two workspaces in one mount has no meaning — say so rather than
        // silently mounting one of them.
        assert!(resolve_root(None, Some("a".into()), vec!["b/foo".into()]).is_err());
        assert!(resolve_root(Some("myws/Trees/directory".into()), None, vec![]).is_err());
        assert!(resolve_root(Some("myws/Nonsense/x".into()), None, vec![]).is_err());
    }
}
