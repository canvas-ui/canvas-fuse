//! A local replica must prove that its folder and ledger belong together
//! before missing files can be interpreted as deletions. This marker is never
//! synced: many devices (or folders) can mirror the same remote workspace.

use super::store::Store;
use super::sync::MirrorConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const MARKER: &str = ".workspace.json";
pub(super) const META_BINDING: &str = "local-replica-binding";
pub(super) const META_DIRECTORIES: &str = "local-replica-directories";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DirectoryId {
    device: u64,
    inode: u64,
}

impl DirectoryId {
    fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.is_dir(),
            "{} is not a real directory",
            path.display()
        );
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

/// st_dev is a mount-time identifier (notably for ZFS and Btrfs). Persist
/// filesystem identity instead, together with inode and birth time when
/// available. Keep st_dev only as a fail-closed fallback for zero fsids.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistentDirectoryId {
    filesystem_type: i64,
    filesystem_id: [i32; 2],
    device_fallback: Option<u64>,
    inode: u64,
    created: Option<(u64, u32)>,
}

impl PersistentDirectoryId {
    fn read(file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // fstatfs fills the entire structure on success. Linux fsid_t consists
        // of two 32-bit words; libc keeps those fields private on some targets.
        let rc = unsafe { libc::fstatfs(file.as_raw_fd(), stat.as_mut_ptr()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .context("reading mirror filesystem identity");
        }
        let stat = unsafe { stat.assume_init() };
        let filesystem_id: [i32; 2] = unsafe { std::mem::transmute(stat.f_fsid) };
        let created = metadata
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|t| (t.as_secs(), t.subsec_nanos()));
        // libc's f_type varies with the target ABI; persist the same i64 on
        // glibc and musl, including targets where this cast is a no-op.
        #[allow(clippy::unnecessary_cast)]
        let filesystem_type = stat.f_type as i64;
        Ok(Self {
            filesystem_type,
            filesystem_id,
            device_fallback: (filesystem_id == [0, 0]).then_some(metadata.dev()),
            inode: metadata.ino(),
            created,
        })
    }
}

pub(super) fn persistent_directory_identity(path: &Path) -> Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    Ok(serde_json::to_string(&PersistentDirectoryId::read(&file)?)?)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Marker {
    format: String,
    version: u32,
    server: String,
    workspace_id: String,
    backend: String,
    /// Unique to this local replica, NOT the workspace or the device account.
    mirror_id: String,
    home: String,
    root_directory: DirectoryId,
    home_directory: DirectoryId,
}

pub(super) struct Destination {
    root: File,
    // Also lock Home, so a home-only mount cannot race a full workspace mount.
    _home: Option<File>,
    path: PathBuf,
    marker: Marker,
    fresh: bool,
    root_directory: DirectoryId,
    home_directory: DirectoryId,
    persistent_directories: String,
    accept_device_change: bool,
}

fn server_identity(raw: &str) -> Result<String> {
    let url = reqwest::Url::parse(raw).context("invalid mirror server URL")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "mirror server URL must be HTTP(S), without credentials, query or fragment"
    );
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn open_directory(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("opening mirror directory {}", path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    anyhow::ensure!(
        rc == 0,
        "{} is already in use by another mirror: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    Ok(file)
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Resolve existing ancestors as well, so a symlink cannot place the database
/// under the directory FUSE is about to cover. Missing leaf directories are OK.
fn prospective_path(path: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let absolute = std::path::absolute(path)?;
            let parent = absolute
                .parent()
                .context("invalid mirror state directory")?;
            let name = absolute
                .file_name()
                .context("invalid mirror state directory")?;
            Ok(prospective_path(parent)?.join(name))
        }
        Err(e) => Err(e.into()),
    }
}

fn read_marker(root: &Path) -> Result<Option<Marker>> {
    let path = root.join(MARKER);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {MARKER}")),
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= 16_384,
        "{MARKER} must be a regular identity file (at most 16 KiB)"
    );
    let mut bytes = Vec::new();
    file.take(16_385).read_to_end(&mut bytes)?;
    Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
        format!("invalid {MARKER}; refusing to sync")
    })?))
}

/// Read-only check before CLI daemonization. The authoritative check, locking
/// and database binding still happen in Mirror::open, including library users.
pub fn preflight(mountpoint: &Path) -> Result<()> {
    if read_marker(mountpoint)?.is_none() {
        anyhow::ensure!(std::fs::read_dir(mountpoint)?.next().transpose()?.is_none(),
            "refusing to mirror non-empty directory {} without a valid {MARKER}; use a new empty destination. Existing folders and sync state must be reviewed before migration",
            mountpoint.display());
    }
    Ok(())
}

/// Locate the original ledger when switching between --server and --remote.
/// Search only the standard per-mount paths; neither move history nor adopt a
/// workspace/name match as proof of replica identity. Destination/open_store
/// still validate the physical folder and binding under their locks afterward.
pub(crate) fn find_data_dir(
    mountpoint: &Path,
    server: &str,
    default: &Path,
    canvas_home: &Path,
) -> Result<PathBuf> {
    let Some(marker) = read_marker(mountpoint)? else {
        return Ok(default.to_path_buf());
    };
    anyhow::ensure!(
        marker.format == "canvas-fuse-mirror"
            && marker.version == 1
            && marker.server == server_identity(server)?,
        "{MARKER} does not match the requested server or supported mirror format"
    );
    let binding = serde_json::to_string(&marker)?;
    let mount_name = default
        .file_name()
        .context("invalid mirror state directory")?;
    let mut candidates = vec![default.to_path_buf()];
    for remote in child_directories(canvas_home)? {
        for workspace in child_directories(&remote.join("fuse/workspaces"))? {
            candidates.push(workspace.join("mounts").join(mount_name));
        }
    }
    let mut visited = std::collections::HashSet::new();
    let mut matches = Vec::new();
    for candidate in candidates {
        if !super::store_path(&candidate).try_exists()? {
            continue;
        }
        let candidate = std::fs::canonicalize(candidate)?;
        if !visited.insert(candidate.clone()) {
            continue;
        }
        if Store::replica_binding(&super::store_path(&candidate))?.as_deref() == Some(&binding) {
            matches.push(candidate);
        }
    }
    match matches.as_slice() {
        [found] => Ok(found.clone()),
        [] => anyhow::bail!(
            "cannot find the original mirror sync history for {}; default state directory is {}. Use --data-dir to select the original directory containing mirror.redb. --accept-device-change requires the original marker/database pair; do not delete or edit either file",
            mountpoint.display(), default.display()
        ),
        _ => anyhow::bail!(
            "multiple state directories match this mirror; use --data-dir to select its original sync history: {}",
            matches.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn child_directories(path: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("looking for mirror history in {}", path.display()))
        }
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    Ok(dirs)
}

/// Resolve an existing replica locally, before any network request. The cache
/// supplies only a display name: the marker is authoritative for server and UUID,
/// and Destination still verifies the physical directories and bound ledger.
pub fn cached_workspace(
    mountpoint: &Path,
    data_dir: &Path,
    server: &str,
    requested: &str,
) -> Result<Option<crate::api::WorkspaceInfo>> {
    let Some(marker) = read_marker(mountpoint)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        marker.format == "canvas-fuse-mirror"
            && marker.version == 1
            && marker.server == server_identity(server)?,
        "{MARKER} does not match the requested server or supported mirror format"
    );
    let cache = std::fs::read(data_dir.join("workspace.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let cached = cache
        .as_ref()
        .filter(|v| v.get("id").and_then(|v| v.as_str()) == Some(marker.workspace_id.as_str()));
    let name = cached.and_then(|v| v.get("name")?.as_str());
    let selector = cached.and_then(|v| v.get("selector")?.as_str());
    if requested != marker.workspace_id && name != Some(requested) && selector != Some(requested) {
        // Older replicas can have a valid marker and ledger but no name cache.
        // This is a cache miss, not permission to trust the directory's name.
        return Ok(None);
    }
    Ok(Some(crate::api::WorkspaceInfo {
        id: marker.workspace_id,
        name: name.unwrap_or(requested).to_string(),
    }))
}

/// Prefer local resolution, but recover a missing/stale display-name cache by
/// checking the hub. A remote name must resolve to the marker's exact UUID;
/// Destination still validates the directory/ledger before scans or cache writes.
pub(crate) fn resolve_workspace(
    mountpoint: &Path,
    data_dir: &Path,
    server: &str,
    requested: &str,
    lookup: impl FnOnce(&str) -> Result<crate::api::WorkspaceInfo>,
) -> Result<(crate::api::WorkspaceInfo, bool)> {
    if let Some(workspace) = cached_workspace(mountpoint, data_dir, server, requested)? {
        return Ok((workspace, true));
    }
    let marker = read_marker(mountpoint)?;
    let workspace = lookup(requested).with_context(|| match &marker {
        Some(marker) => format!(
            "cannot verify workspace name `{requested}` for existing mirror UUID {}: the workspace.json name cache is missing or does not match. Reconnect to the hub and retry the same mount command to repair the cache; keep {MARKER} and the original sync database",
            marker.workspace_id
        ),
        None => format!("resolving workspace {requested} (first mirror mount requires the hub)"),
    })?;
    if let Some(marker) = &marker {
        anyhow::ensure!(
            workspace.id == marker.workspace_id,
            "workspace name `{requested}` resolves to UUID {}, but {MARKER} belongs to UUID {}; refusing to sync {}",
            workspace.id, marker.workspace_id, mountpoint.display()
        );
    }
    Ok((workspace, marker.is_some()))
}

/// Persist the name/selector mapping after the destination has been validated.
/// Atomic publication and fsync make the first online mount usable after reboot.
pub(crate) fn cache_workspace(
    data_dir: &Path,
    requested: &str,
    workspace: &crate::api::WorkspaceInfo,
) -> Result<()> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "id": workspace.id, "name": workspace.name, "selector": requested,
    }))?;
    let target = data_dir.join("workspace.json");
    if std::fs::read(&target).ok().as_deref() == Some(bytes.as_slice()) {
        return Ok(());
    }
    let temporary = data_dir.join(format!("workspace.{}.tmp", super::operation_id()?));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &target)?;
        File::open(data_dir)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result.context("persisting mirror workspace name for offline startup")
}

impl Destination {
    /// Old directory-move intents only recorded st_dev. They can follow the
    /// verified Home filesystem across renumbering, never an unrelated mount.
    pub(super) fn matches_legacy_device(&self, recorded: u64, current: u64) -> bool {
        recorded == current
            || (recorded == self.marker.home_directory.device
                && current == self.home_directory.device)
    }

    pub(super) fn prepare(cfg: &MirrorConfig) -> Result<Self> {
        let home = if cfg.home_dir == cfg.mountpoint {
            "."
        } else {
            anyhow::ensure!(
                cfg.home_dir == cfg.mountpoint.join("Home"),
                "mirror Home must be the mountpoint or its Home subdirectory"
            );
            "Home"
        };
        let server = server_identity(&cfg.server)?;
        std::fs::create_dir_all(&cfg.mountpoint)?;
        let root = open_directory(&cfg.mountpoint)?;
        let path = fd_path(&root);
        anyhow::ensure!(
            !prospective_path(&cfg.data_dir)?.starts_with(std::fs::canonicalize(&path)?),
            "mirror data directory must be outside the mountpoint"
        );
        let previous = read_marker(&path)?;
        let fresh = previous.is_none();
        if fresh {
            preflight(&path)
                .with_context(|| format!("mirror destination {}", cfg.mountpoint.display()))?;
            anyhow::ensure!(!super::store_path(&cfg.data_dir).try_exists()?,
                "refusing to attach existing mirror state {} to an unmarked folder {}; the old ledger may contain deletions. Use a new empty destination and a separate data directory",
                cfg.data_dir.display(), cfg.mountpoint.display());
        }
        if let Some(marker) = &previous {
            anyhow::ensure!(marker.format == "canvas-fuse-mirror" && marker.version == 1
                && marker.server == server && marker.workspace_id == cfg.workspace_id
                && marker.backend == cfg.backend && marker.home == home
                && marker.mirror_id.len() == 32,
                "{MARKER} does not match the requested server, workspace UUID, backend or mount layout; refusing to sync {}",
                cfg.mountpoint.display());
        }
        let home_path = path.join(home);
        if fresh && home != "." {
            std::fs::create_dir(&home_path)?;
        }
        let root_directory = DirectoryId::read(&path.join("."))?;
        let home_directory = DirectoryId::read(&home_path)?;
        let home_file = if home == "." {
            None
        } else {
            Some(open_directory(&home_path)?)
        };
        let marker = previous.unwrap_or_else(|| Marker {
            format: "canvas-fuse-mirror".into(),
            version: 1,
            server,
            workspace_id: cfg.workspace_id.clone(),
            backend: cfg.backend.clone(),
            mirror_id: String::new(),
            home: home.into(),
            root_directory: root_directory.clone(),
            home_directory: home_directory.clone(),
        });
        // Inodes are checked before even opening the database. Device numbers
        // may differ across boots: open_store checks their persistent replacement
        // against the original ledger before allowing scans or queued mutations.
        anyhow::ensure!(marker.root_directory.inode == root_directory.inode
            && marker.home_directory.inode == home_directory.inode,
            "mirror directory identity changed at {} (root inode {} -> {}, Home inode {} -> {}); a copied marker or replaced directory cannot reuse sync history",
            cfg.mountpoint.display(), marker.root_directory.inode, root_directory.inode,
            marker.home_directory.inode, home_directory.inode);
        let persistent_directories = serde_json::to_string(&(
            PersistentDirectoryId::read(&root)?,
            PersistentDirectoryId::read(home_file.as_ref().unwrap_or(&root))?,
        ))?;
        let mut destination = Self {
            root,
            _home: home_file,
            path,
            marker,
            fresh,
            root_directory,
            home_directory,
            persistent_directories,
            accept_device_change: cfg.opts.accept_device_change,
        };
        if fresh {
            destination.marker.mirror_id = super::operation_id()?;
        }
        Ok(destination)
    }

    pub(super) fn open_store(&self, data_dir: &Path) -> Result<Store> {
        Store::open_bound(
            &super::store_path(data_dir),
            &serde_json::to_string(&self.marker)?,
            &self.persistent_directories,
            self.marker.root_directory == self.root_directory
                && self.marker.home_directory == self.home_directory,
            self.accept_device_change,
        )
    }

    /// Publish complete JSON without overwriting another marker. Fsync both
    /// the file and its directory before allowing the initial scan.
    pub(super) fn commit(&self) -> Result<()> {
        if self.fresh {
            let temporary = self
                .path
                .join(format!(".workspace.{}.tmp", self.marker.mirror_id));
            let result = (|| -> Result<()> {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)?;
                file.write_all(&serde_json::to_vec_pretty(&self.marker)?)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
                std::fs::hard_link(&temporary, self.path.join(MARKER))?;
                Ok(())
            })();
            let _ = std::fs::remove_file(&temporary);
            result.context("publishing mirror identity; refusing to sync")?;
            self.root.sync_all()?;
        }
        self.verify()
    }

    pub(super) fn verify(&self) -> Result<()> {
        anyhow::ensure!(self.root.metadata()?.nlink() > 0
            && read_marker(&self.path)?.as_ref() == Some(&self.marker)
            && DirectoryId::read(&self.path.join(&self.marker.home))? == self.home_directory,
            "mirror identity is missing or changed; refusing to reconcile or send queued operations");
        Ok(())
    }

    pub(super) fn verify_local(&self, local: &super::local::Local) -> Result<()> {
        anyhow::ensure!(
            DirectoryId::read(&local.path("."))? == self.home_directory,
            "mirror Home changed while opening it; refusing to sync"
        );
        self.verify()
    }
}
