//! A durable audit of actions suppressed by protected mirror sync. These are
//! observations, never replayable commands: resuming normal sync must inspect
//! current local and remote state again.

use serde::{Deserialize, Serialize};

pub(crate) const RECHECK: &str = "protected-recheck";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedAction {
    pub action: String,
    pub key: String,
    pub destination: Option<String>,
    pub local_sha256: Option<String>,
    pub remote_sha256: Option<String>,
    pub observed_at: u64,
}

impl BlockedAction {
    pub(crate) fn id(&self) -> String {
        serde_json::to_string(&(&self.action, &self.key, &self.destination)).unwrap()
    }

    pub(crate) fn same_observation(&self, other: &Self) -> bool {
        self.action == other.action
            && self.key == other.key
            && self.destination == other.destination
            && self.local_sha256 == other.local_sha256
            && self.remote_sha256 == other.remote_sha256
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushReport {
    pub dry_run: bool,
    pub created: u64,
    pub replaced: u64,
    pub unchanged: u64,
    pub skipped: u64,
    pub failed: u64,
    /// Pull only: remote-only files fetched, and remote files left alone
    /// because a local file exists at that path.
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub kept: u64,
    pub items: Vec<PushItem>,
}

impl PushReport {
    pub(crate) fn record(&mut self, item: PushItem) {
        match item.action.as_str() {
            "create" => self.created += 1,
            "replace" => self.replaced += 1,
            "unchanged" => self.unchanged += 1,
            "download" => self.downloaded += 1,
            "kept" => self.kept += 1,
            "skipped" => self.skipped += 1,
            _ => self.failed += 1,
        }
        log::info!(
            "mirror recovery{}: {} {}{}",
            if self.dry_run { " preview" } else { "" },
            item.action,
            item.key,
            item.detail
                .as_ref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        );
        self.items.push(item);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushItem {
    pub key: String,
    pub action: String,
    pub size: u64,
    pub detail: Option<String>,
}
