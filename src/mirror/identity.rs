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

impl Destination {
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
        anyhow::ensure!(marker.root_directory == root_directory && marker.home_directory == home_directory,
            "mirror directory identity changed at {}; a copied marker or replaced directory cannot reuse sync history",
            cfg.mountpoint.display());
        let mut destination = Self {
            root,
            _home: home_file,
            path,
            marker,
            fresh,
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
            && DirectoryId::read(&self.path.join(&self.marker.home))? == self.marker.home_directory,
            "mirror identity is missing or changed; refusing to reconcile or send queued operations");
        Ok(())
    }

    pub(super) fn verify_local(&self, local: &super::local::Local) -> Result<()> {
        anyhow::ensure!(
            DirectoryId::read(&local.path("."))? == self.marker.home_directory,
            "mirror Home changed while opening it; refusing to sync"
        );
        self.verify()
    }
}
