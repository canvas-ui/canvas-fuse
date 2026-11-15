use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Record of a running (or crashed) mount, one JSON file per mountpoint in
/// the state dir. Lets `status`/`unmount` find daemons across invocations.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MountState {
    pub mountpoint: PathBuf,
    pub server: String,
    pub pid: u32,
    pub started_at: String,
    pub contexts: Option<Vec<String>>,
    #[serde(default)]
    pub log_file: Option<PathBuf>,
    /// The workspace a `-w` mount roots (None for context mounts).
    #[serde(default)]
    pub workspace: Option<String>,
    /// `--mirror`: Home is a device mirror; `status` reads the daemon's
    /// status file and the control socket exists.
    #[serde(default)]
    pub mirror: bool,
}

pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("canvas-fuse")
}

fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(['.', '_']).to_string();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed
    }
}

/// Per-mount local state dir (holds the sticky-name redb). Each mount needs its
/// own redb — it is single-writer, so a shared dir would lock out concurrent
/// mounts. Scoped under ~/.canvas/<remote>/ so it's discoverable, and keyed by
/// context when a single context is mounted (sticky names then persist across
/// remounts of that context regardless of mountpoint); otherwise by mountpoint.
pub fn mount_data_dir(remote: &str, contexts: &[String], mountpoint: &Path) -> PathBuf {
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".canvas")
        .join(sanitize_segment(remote))
        .join("fuse");
    if contexts.len() == 1 {
        base.join("contexts").join(sanitize_segment(&contexts[0]))
    } else {
        // All-contexts or multi-context mount: key by mountpoint instead.
        let raw = mountpoint.to_string_lossy();
        let mut hash: u64 = 0xcbf29ce484222325;
        for b in raw.bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        base.join("mounts").join(format!(
            "{}.{hash:08x}",
            sanitize_segment(raw.trim_matches('/'))
        ))
    }
}

/// Per-mount state dir for a workspace mount, keyed by workspace name so sticky
/// names persist across remounts of the same workspace.
pub fn workspace_data_dir(remote: &str, workspace: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".canvas")
        .join(sanitize_segment(remote))
        .join("fuse")
        .join("workspaces")
        .join(sanitize_segment(workspace))
}

/// A mirror's ledger describes one physical folder. Sharing it merely because
/// two mounts name the same workspace turns absent local files into deletions.
pub fn workspace_mount_data_dir(remote: &str, workspace: &str, mountpoint: &Path) -> PathBuf {
    workspace_data_dir(remote, workspace).join("mounts").join(
        mount_data_dir(remote, &[], mountpoint)
            .file_name()
            .expect("mount state directory"),
    )
}

/// Mirror history belongs to the local replica, not the authentication alias.
/// Keep the historical host-based default and find older alias-based ledgers by
/// their exact marker binding. Explicit --data-dir bypasses this discovery.
pub fn mirror_data_dir(server: &str, workspace: &str, mountpoint: &Path) -> Result<PathBuf> {
    let host = server
        .rsplit("://")
        .next()
        .unwrap_or(server)
        .split('/')
        .next()
        .unwrap_or("server");
    let default = workspace_mount_data_dir(host, workspace, mountpoint);
    let canvas_home = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".canvas");
    crate::mirror::identity::find_data_dir(mountpoint, server, &default, &canvas_home)
}

fn state_file_for(mountpoint: &Path) -> PathBuf {
    // Readable prefix + short hash to avoid collisions after sanitizing
    let raw = mountpoint.to_string_lossy();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in raw.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let name: String = raw
        .trim_matches('/')
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    state_dir()
        .join("mounts")
        .join(format!("{name}.{hash:08x}.json"))
}

/// The daemon's throttled mirror status, next to the state file. Written by
/// the daemon (the only process that can open its redb), read by `status`.
pub fn status_file_for(mountpoint: &Path) -> PathBuf {
    state_file_for(mountpoint).with_extension("status.json")
}

/// The mirror control socket (see `mirror::control`), next to the state
/// file but named by the hash alone: a unix socket path must fit in
/// `sockaddr_un` (108 bytes), and the readable prefix would not.
pub fn control_socket_for(mountpoint: &Path) -> PathBuf {
    let raw = mountpoint.to_string_lossy();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in raw.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    state_dir().join("mounts").join(format!("{hash:016x}.sock"))
}

pub fn read_mirror_status(mountpoint: &Path) -> Option<crate::mirror::sync::MirrorStatus> {
    let raw = std::fs::read(status_file_for(mountpoint)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Find the mount a CLI argument names: the exact mountpoint, or the
/// directory the user passed to `mount` (the kernel mount is one level
/// below it, at `<mountpoint>/<workspace>`), when that is unambiguous.
pub fn resolve_mount(arg: &Path) -> Option<MountState> {
    let canon = arg.canonicalize().unwrap_or_else(|_| arg.to_path_buf());
    if let Some(s) = read_state(&canon) {
        return Some(s);
    }
    let below: Vec<MountState> = list_states()
        .into_iter()
        .filter(|s| s.mountpoint.parent() == Some(canon.as_path()))
        .collect();
    if below.len() == 1 {
        return below.into_iter().next();
    }
    None
}

pub fn write_state(state: &MountState) -> Result<PathBuf> {
    let path = state_file_for(&state.mountpoint);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_vec_pretty(state)?)?;
    Ok(path)
}

pub fn remove_state(mountpoint: &Path) {
    let _ = std::fs::remove_file(state_file_for(mountpoint));
    let _ = std::fs::remove_file(status_file_for(mountpoint));
}

pub fn read_state(mountpoint: &Path) -> Option<MountState> {
    let raw = std::fs::read(state_file_for(mountpoint)).ok()?;
    serde_json::from_slice(&raw).ok()
}

pub fn list_states() -> Vec<MountState> {
    let dir = state_dir().join("mounts");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|raw| serde_json::from_slice(&raw).ok())
        .collect()
}

pub fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0): 0 = alive, EPERM = alive but not ours
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Check /proc/mounts for a live canvasfs mount at this path.
pub fn is_mounted(mountpoint: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    // /proc/mounts octal-escapes spaces as \040
    let escaped = mountpoint.to_string_lossy().replace(' ', "\\040");
    mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let (Some(_dev), Some(mp), Some(fstype)) = (fields.next(), fields.next(), fields.next())
        else {
            return false;
        };
        mp == escaped && fstype.starts_with("fuse.canvas")
    })
}

/// Detach from the terminal (fork + setsid via libc::daemon) and send all
/// further output to a log file. Must be called before any threads exist —
/// fork() does not carry them over.
/// The background half of a detached mount. The foreground process that
/// started it waits on the other end of the pipe and exits with the outcome,
/// so `mount -d` succeeds only when the mount is really there.
pub struct Detached {
    outcome: std::fs::File,
}

impl Detached {
    /// The mount is up: let the waiting foreground process exit 0.
    pub fn ready(mut self) {
        use std::io::Write;
        let _ = self.outcome.write_all(b"ok");
    }

    /// The mount failed: the foreground process prints this and exits 1.
    pub fn failed(mut self, error: &str) {
        use std::io::Write;
        let _ = self.outcome.write_all(error.as_bytes());
    }
}

/// Continue in a background process with stdout/stderr in `log_file`. The
/// calling process never returns from here: it waits for the background
/// one to report (`Detached::ready` / `failed`) and exits accordingly.
pub fn daemonize(log_file: &Path) -> Result<Detached> {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    std::fs::create_dir_all(log_file.parent().unwrap())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .with_context(|| format!("opening log file {}", log_file.display()))?;
    let mut ends = [0; 2];
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        anyhow::bail!("pipe() failed: {}", std::io::Error::last_os_error());
    }
    let (mut outcome, report) = unsafe {
        (
            std::fs::File::from_raw_fd(ends[0]),
            std::fs::File::from_raw_fd(ends[1]),
        )
    };
    match unsafe { libc::fork() } {
        -1 => anyhow::bail!("fork() failed: {}", std::io::Error::last_os_error()),
        0 => {}
        _ => {
            drop(report);
            let mut message = String::new();
            let _ = outcome.read_to_string(&mut message);
            if message == "ok" {
                std::process::exit(0);
            }
            if message.is_empty() {
                eprintln!("Error: the mount did not start; see {}", log_file.display());
            } else {
                eprintln!("Error: {message}");
            }
            std::process::exit(1);
        }
    }
    drop(outcome);
    // Own session, so the terminal that started the mount can close. The
    // working directory stays: the caller already canonicalized its paths.
    unsafe {
        libc::setsid();
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, libc::STDIN_FILENO);
            libc::close(null);
        }
        libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO);
    }
    Ok(Detached { outcome: report })
}

pub fn default_log_file(mountpoint: &Path) -> PathBuf {
    state_file_for(mountpoint).with_extension("log")
}
