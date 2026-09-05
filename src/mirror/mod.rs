//! Mirror mode: `Home/` as an offline-capable device mirror of the hub's
//! `workspace:home` backend (Dropbox/iCloud style) instead of a live
//! passthrough.
//!
//! The pieces, each a file here:
//!
//! - `store`     — the persistent Home tree, base ledger, job queue, pins,
//!                 cache index, conflicts and trash (one redb, `mirror.redb`).
//! - `cache`     — sha256-addressed content cache on disk with a byte budget.
//! - `hub`       — the objects-protocol client (`docs/sync-protocol.md`).
//! - `reconcile` — the pure three-way decision per key.
//! - `sync`      — the engine thread: change feed, push/pull, conflicts,
//!                 pins, eviction, status; plus the `Mirror` facade the FUSE
//!                 layer talks to (local writes, reads, renames, deletes).
//! - `control`   — the unix-socket control channel the CLI subcommands use.
//!
//! Identity on the wire is the KEY (relative `/`-separated path, NFC) and the
//! DIGEST of the bytes. Hub document ids are never stored — they are recycled
//! and would silently re-bind to something else.

pub mod cache;
pub mod control;
pub mod hub;
pub mod reconcile;
pub mod store;
pub mod sync;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use unicode_normalization::UnicodeNormalization;

/// What the hub calls the backend a mirror follows. Only this one exists
/// today; the address is kept as data so a second backend is a config
/// change, not a code change.
pub const DEFAULT_BACKEND: &str = "workspace:home";

/// The keys the hub refuses no matter what the user configures. Mirrored
/// here so they are never queued in the first place — a job that can only
/// ever fail is noise in the status and in the log.
pub const MIRROR_IGNORE_DEFAULTS: &[&str] = &["**/.*", "**/.*/**", ".workspace", ".workspace/**"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConflictMode {
    /// Upload our version to the hub's conflict inbox and adopt the hub's
    /// version at the key. The user resolves in the web UI / CLI.
    #[default]
    Prompt,
    /// Dropbox style: our version is written next to the hub's under
    /// `<stem> (conflict from <device> <date>).<ext>`.
    Rename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DeleteMode {
    /// `rm` in the mount deletes on the hub (with a precondition).
    #[default]
    Propagate,
    /// `rm` only drops the local copy; the hub keeps the file, and the mount
    /// re-lists it on the next full reconcile.
    Keep,
}

#[derive(Debug, Clone)]
pub struct MirrorOptions {
    pub pins: Vec<String>,
    pub cache_budget_bytes: u64,
    pub conflicts: ConflictMode,
    pub deletes: DeleteMode,
    pub ignore: Vec<String>,
    pub poll_secs: u64,
}

impl Default for MirrorOptions {
    fn default() -> Self {
        Self {
            pins: Vec::new(),
            cache_budget_bytes: 4096 * 1024 * 1024,
            conflicts: ConflictMode::Prompt,
            deletes: DeleteMode::Propagate,
            ignore: Vec::new(),
            poll_secs: 30,
        }
    }
}

/// Who this mirror is to the hub. The id goes out as `X-Canvas-Origin` on
/// every mutation so the change feed can hand our own echoes back to us
/// labelled; the name is what a conflict entry shows a human.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub id: String,
    pub name: String,
}

impl DeviceIdentity {
    /// `~/.canvas/device.json` (`deviceId`) when canvas-cli registered this
    /// machine; otherwise a stable hash of hostname + user, so an unregistered
    /// device still has ONE id across restarts and its echoes stay recognizable.
    pub fn resolve() -> Self {
        let hostname = hostname();
        let from_file = dirs::home_dir()
            .map(|h| h.join(".canvas").join("device.json"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| {
                v.get("deviceId")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
        let id = from_file.unwrap_or_else(|| {
            let user = std::env::var("USER").unwrap_or_default();
            let mut hash: u64 = 0xcbf29ce484222325;
            for b in format!("{hostname}\u{1f}{user}").bytes() {
                hash ^= b as u64;
                hash = hash.wrapping_mul(0x100000001b3);
            }
            format!("fuse-{hash:016x}")
        });
        Self { id, name: hostname }
    }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..end]) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    "localhost".to_string()
}

/// Canonical key form: NFC, `/`-separated, no leading/trailing slashes, no
/// empty segments. The hub normalizes the same way, so a key round-trips
/// byte-for-byte and the base ledger never misses because of a stray slash.
pub fn normalize_key(raw: &str) -> String {
    raw.nfc()
        .collect::<String>()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Parent key of a key (`a/b/c` → `a/b`, `a` → ``).
pub fn parent_key(key: &str) -> &str {
    key.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

pub fn leaf_of(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// Compiled ignore rules: the hub's effective exclusions + our defaults +
/// `--ignore`. Anything matching is never uploaded and never asked for.
#[derive(Debug, Clone, Default)]
pub struct IgnoreRules {
    patterns: Vec<glob::Pattern>,
}

impl IgnoreRules {
    pub fn new<I: IntoIterator<Item = S>, S: AsRef<str>>(patterns: I) -> Self {
        let mut out = Vec::new();
        for p in patterns {
            let p = p.as_ref().trim();
            if p.is_empty() {
                continue;
            }
            match glob::Pattern::new(p) {
                Ok(pat) => out.push(pat),
                Err(e) => log::warn!("ignore pattern {p:?} is not a valid glob: {e}"),
            }
        }
        Self { patterns: out }
    }

    pub fn is_ignored(&self, key: &str) -> bool {
        // Dotfiles are refused by the hub outright; check the segments
        // directly so the rule holds even if the pattern list is empty.
        if key.split('/').any(|seg| seg.starts_with('.')) {
            return true;
        }
        let opts = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        self.patterns.iter().any(|p| p.matches_with(key, opts))
    }
}

/// Pin globs: a key is pinned when it or any ancestor directory matches.
/// `Docs/` and `Docs` both pin the whole subtree; `*.md` pins every markdown
/// file at the root; `**/*.pdf` every PDF anywhere.
pub fn is_pinned(pins: &[String], key: &str) -> bool {
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    for pin in pins {
        let pin = pin.trim().trim_start_matches('/');
        if pin.is_empty() {
            continue;
        }
        let dir = pin.trim_end_matches('/');
        if key == dir || key.starts_with(&format!("{dir}/")) {
            return true;
        }
        if let Ok(pat) = glob::Pattern::new(dir) {
            if pat.matches_with(key, opts) {
                return true;
            }
            // A glob naming a directory pins everything under it.
            let mut prefix = String::new();
            for seg in key.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(seg);
                if prefix != key && pat.matches_with(&prefix, opts) {
                    return true;
                }
            }
        }
    }
    false
}

/// `<stem> (conflict from <device> <YYYY-MM-DD HHmm>).<ext>` next to the
/// original, the Dropbox spelling. The extension is the LAST one only —
/// `notes.tar.gz` keeps `.gz`, since that is what apps key their handlers on.
pub fn conflict_copy_key(key: &str, device: &str, at: chrono::DateTime<chrono::Utc>) -> String {
    let (dir, name) = match key.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, key),
    };
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, Some(e)),
        _ => (name, None),
    };
    let stamp = at.format("%Y-%m-%d %H%M");
    let device = device.replace(['/', '\\'], "_");
    let leaf = match ext {
        Some(e) => format!("{stem} (conflict from {device} {stamp}).{e}"),
        None => format!("{stem} (conflict from {device} {stamp})"),
    };
    match dir {
        Some(d) => format!("{d}/{leaf}"),
        None => leaf,
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Where the mirror keeps its things inside the mount's data dir.
pub fn store_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("mirror.redb")
}

pub fn cache_dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_copy_name_keeps_the_last_extension_only() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-05T14:07:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            conflict_copy_key("Docs/report.md", "laptop", at),
            "Docs/report (conflict from laptop 2026-09-05 1407).md"
        );
        assert_eq!(
            conflict_copy_key("notes.tar.gz", "laptop", at),
            "notes.tar (conflict from laptop 2026-09-05 1407).gz"
        );
        assert_eq!(
            conflict_copy_key("README", "my/host", at),
            "README (conflict from my_host 2026-09-05 1407)"
        );
        // A leading dot is not an extension separator.
        assert_eq!(
            conflict_copy_key(".env", "h", at),
            ".env (conflict from h 2026-09-05 1407)"
        );
    }

    #[test]
    fn keys_normalize_to_relative_nfc_slash_paths() {
        assert_eq!(normalize_key("/a//b/"), "a/b");
        assert_eq!(normalize_key("caf\u{65}\u{301}"), "caf\u{e9}");
        assert_eq!(parent_key("a/b/c"), "a/b");
        assert_eq!(parent_key("a"), "");
        assert_eq!(leaf_of("a/b/c"), "c");
    }

    #[test]
    fn ignore_rules_cover_dotfiles_and_globs() {
        let rules = IgnoreRules::new(MIRROR_IGNORE_DEFAULTS.iter().chain(["**/node_modules/**"].iter()));
        assert!(rules.is_ignored(".git"));
        assert!(rules.is_ignored("a/.cache/x"));
        assert!(rules.is_ignored("proj/node_modules/x/y.js"));
        assert!(!rules.is_ignored("proj/src/y.js"));
        assert!(!rules.is_ignored("README.md"));
    }

    #[test]
    fn pins_match_subtrees_and_globs() {
        let pins = vec!["Docs/".to_string(), "*.md".to_string(), "Photos/2026*".to_string()];
        assert!(is_pinned(&pins, "Docs/a/b.txt"));
        assert!(is_pinned(&pins, "Docs"));
        assert!(is_pinned(&pins, "notes.md"));
        assert!(!is_pinned(&pins, "sub/notes.md"));
        assert!(is_pinned(&pins, "Photos/2026-07/x.jpg"));
        assert!(!is_pinned(&pins, "Photos/2025/x.jpg"));
        assert!(!is_pinned(&pins, "Other/a.txt"));
    }
}
