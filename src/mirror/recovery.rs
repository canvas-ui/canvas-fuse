//! Explicit local-authoritative recovery. Never drain normal jobs, pull remote
//! data, delete remote-only objects, or use the change-feed cursor as a plan.

use super::*;
use crate::mirror::protection::{PushItem, PushReport};

impl Mirror {
    /// Recover from a bad upstream view while the mount remains protected.
    /// A preview reads files/remote metadata only. Real pushes snapshot local
    /// bytes and use the freshly observed remote digest as a CAS precondition.
    pub fn push_local(&self, dry_run: bool) -> Result<PushReport> {
        anyhow::ensure!(self.opts.protected,
            "recovery push requires a --mirror --protected mount so background sync cannot undo recovery or delete remote-only files");
        let _cycle = self.cycle_gate.lock();
        self.verify_destination()?;
        let instance = self.hub.instance_id()?;
        if let Some(previous) = self.store.meta(META_INSTANCE) {
            anyhow::ensure!(
                instance.as_deref() == Some(previous.as_str()),
                "hub instance identity changed; refusing recovery push"
            );
        }
        let mut patterns: Vec<String> = super::super::MIRROR_IGNORE_DEFAULTS
            .iter()
            .map(|s| s.to_string())
            .collect();
        patterns.extend(self.hub.exclusions()?);
        patterns.extend(self.opts.ignore.iter().cloned());
        let ignore = IgnoreRules::new(patterns);
        let mut report = PushReport {
            dry_run,
            ..PushReport::default()
        };
        let mut files = self.local.walk();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        if !dry_run {
            if let Some(instance) = instance {
                self.store.set_meta(META_INSTANCE, &instance)?;
            }
        }
        log::info!(
            "mirror recovery push{}: inspecting local files; remote-only files will be kept",
            if dry_run { " preview" } else { "" }
        );
        for (key, stat) in files {
            if stat.is_dir {
                continue;
            }
            let result = if ignore.is_ignored(&key) {
                Ok(PushItem {
                    key: key.clone(),
                    action: "skipped".into(),
                    size: stat.size,
                    detail: Some("excluded from mirror sync".into()),
                })
            } else {
                self.push_recovery_file(&key, dry_run)
            };
            let (item, stop) = match result {
                Ok(item) => (item, false),
                Err(e) => {
                    let stop = e.is_offline()
                        || matches!(e, HubError::Unauthorized | HubError::UnsafeMirror(_));
                    (
                        PushItem {
                            key: key.clone(),
                            action: "failed".into(),
                            size: stat.size,
                            detail: Some(e.to_string()),
                        },
                        stop,
                    )
                }
            };
            match item.action.as_str() {
                "create" => report.created += 1,
                "replace" => report.replaced += 1,
                "unchanged" => report.unchanged += 1,
                "skipped" => report.skipped += 1,
                _ => report.failed += 1,
            }
            log::info!(
                "mirror recovery{}: {} {}{}",
                if dry_run { " preview" } else { "" },
                item.action,
                item.key,
                item.detail
                    .as_ref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            );
            report.items.push(item);
            if stop {
                break;
            }
        }
        if !dry_run {
            self.rt.lock().protected_jobs.clear();
            self.write_status(true);
        }
        Ok(report)
    }

    fn push_recovery_file(&self, key: &str, dry_run: bool) -> Result<PushItem, HubError> {
        self.verify_destination()?;
        super::super::local::check_key(key).map_err(|e| HubError::Other(e.to_string()))?;
        let guard = self.directory_moves.lock();
        let stamp = self
            .local
            .stat(key)
            .ok_or_else(|| HubError::LocalChanged(key.into()))?;
        if stamp.is_dir || self.is_open_for_write(key) {
            return Ok(PushItem {
                key: key.into(),
                action: "skipped".into(),
                size: stamp.size,
                detail: Some("local file is open for writing or no longer a regular file".into()),
            });
        }
        let epoch = self.directory_epoch.load(Ordering::Relaxed);
        drop(guard);
        // Preview must not create upload snapshots or mutate the ledger.
        let snapshot = if dry_run {
            None
        } else {
            Some(
                self.local
                    .upload_snapshot(key)
                    .map_err(|e| HubError::Other(e.to_string()))?,
            )
        };
        let hashed = match &snapshot {
            Some(snapshot) => snapshot.hashed.clone(),
            None => self
                .local
                .hash(key)
                .map_err(|e| HubError::Other(e.to_string()))?,
        };
        let remote = self.hub.head_object(key)?;
        let action = match &remote {
            None => "create",
            Some(remote) if remote.sha256 == hashed.sha256 => "unchanged",
            Some(_) => "replace",
        };
        let guard = self.directory_moves.lock();
        if self.directory_epoch.load(Ordering::Relaxed) != epoch
            || self.local.stat(key).as_ref() != Some(&stamp)
            || self.is_open_for_write(key)
            || snapshot
                .as_ref()
                .is_some_and(|s| s.fingerprint != stamp.fingerprint)
        {
            return Err(HubError::LocalChanged(key.into()));
        }
        drop(guard);
        if !dry_run {
            let (sha, mtime, seq) = if action == "unchanged" {
                (hashed.sha256.clone(), remote.as_ref().unwrap().mtime, 0)
            } else {
                let options = PutOptions {
                    if_match: remote.as_ref().map(|r| r.sha256.clone()),
                    if_none_match_any: remote.is_none(),
                    sha256: Some(hashed.sha256.clone()),
                    mtime: Some(hashed.mtime),
                    ..PutOptions::default()
                };
                let result = self.hub.put_object(
                    key,
                    &PutBody::File(snapshot.as_ref().unwrap().path.clone()),
                    &options,
                )?;
                if !result.sha256.is_empty() && result.sha256 != hashed.sha256 {
                    return Err(HubError::Other(
                        "recovery upload returned an unexpected digest".into(),
                    ));
                }
                (hashed.sha256.clone(), result.mtime, result.seq)
            };
            let _guard = self.directory_moves.lock();
            self.verify_destination()?;
            if self.directory_epoch.load(Ordering::Relaxed) != epoch
                || self.local.stat(key).as_ref() != Some(&stamp)
                || self.is_open_for_write(key)
            {
                // The upload is a valid snapshot, but a newer local edit or
                // rename must not be marked clean or have its jobs discarded.
                return Ok(PushItem {
                    key: key.into(),
                    action: action.into(),
                    size: hashed.size,
                    detail: Some("uploaded snapshot; newer local work remains pending".into()),
                });
            }
            self.store
                .put_base(
                    key,
                    &Base {
                        sha256: sha.clone(),
                        size: hashed.size,
                        mtime,
                        remote_seq: seq,
                    },
                )
                .map_err(|e| HubError::Other(e.to_string()))?;
            self.store
                .put_entry(
                    key,
                    &Entry {
                        sha256: sha,
                        size: hashed.size,
                        mtime: hashed.mtime,
                        state: EntryState::Clean,
                    },
                )
                .map_err(|e| HubError::Other(e.to_string()))?;
            // Only acknowledge uploads covered by this snapshot. Deletes and
            // renames remain deferred by protection, never executed by recovery.
            for job in self.store.jobs() {
                if matches!(&job.kind, JobKind::Push { key: k } | JobKind::Conflict { key: k, .. } if k == key)
                {
                    self.store
                        .remove_job(job.seq)
                        .map_err(|e| HubError::Other(e.to_string()))?;
                }
            }
            if let Some(mut conflict) = self.store.conflict(key) {
                conflict.resolved = true;
                self.store
                    .put_conflict(&conflict)
                    .map_err(|e| HubError::Other(e.to_string()))?;
            }
            self.store
                .set_meta(&format!("local-stamp:{key}"), &stamp.fingerprint)
                .map_err(|e| HubError::Other(e.to_string()))?;
            self.store
                .remove_meta(&format!("upload-pending:{key}"))
                .map_err(|e| HubError::Other(e.to_string()))?;
            self.record_dirs_for(key);
            self.view_upsert(key, hashed.size, hashed.mtime);
        }
        Ok(PushItem {
            key: key.into(),
            action: action.into(),
            size: hashed.size,
            detail: None,
        })
    }
}
