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
        let ignore = self.recovery_preflight("push", dry_run)?;
        let mut report = PushReport {
            dry_run,
            ..PushReport::default()
        };
        let mut files = self.local.walk();
        files.sort_by(|a, b| a.0.cmp(&b.0));
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
            report.record(item);
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

    /// The mirror image of `push_local`: download remote files that are absent
    /// locally, never replacing or removing a local file. Absent paths the
    /// ledger remembers as deleted or renamed away are restored too, and the
    /// queued upstream delete/rename for them is dropped, so both sides end up
    /// with the union and only files that differ remain to review.
    pub fn pull_remote(&self, dry_run: bool) -> Result<PushReport> {
        anyhow::ensure!(self.opts.protected,
            "recovery pull requires a --mirror --protected mount so background sync cannot replace local files");
        let _cycle = self.cycle_gate.lock();
        let ignore = self.recovery_preflight("pull", dry_run)?;
        let mut report = PushReport {
            dry_run,
            ..PushReport::default()
        };
        let mut remote: BTreeMap<String, RemoteStat> = BTreeMap::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.hub.list_objects("", cursor.as_deref(), LIST_PAGE)?;
            for o in page.objects {
                remote.insert(
                    super::super::normalize_key(&o.key),
                    RemoteStat {
                        sha256: o.sha256,
                        size: o.size,
                        mtime: o.mtime,
                    },
                );
            }
            match page.cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        log::info!(
            "mirror recovery pull{}: inspecting {} remote files; local files will be kept",
            if dry_run { " preview" } else { "" },
            remote.len()
        );
        for (key, stat) in &remote {
            let skipped = |detail: &str| PushItem {
                key: key.clone(),
                action: "skipped".into(),
                size: stat.size,
                detail: Some(detail.into()),
            };
            let result = if ignore.is_ignored(key) {
                Ok(skipped("excluded from mirror sync"))
            } else if self.pending_directory_move(key) {
                Ok(skipped("under a pending directory move; review it first"))
            } else {
                self.pull_recovery_file(key, stat, dry_run)
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
            report.record(item);
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

    /// Shared by push and pull: same destination, same hub, same exclusions.
    fn recovery_preflight(&self, verb: &str, dry_run: bool) -> Result<IgnoreRules> {
        self.verify_destination()?;
        let instance = self.hub.instance_id()?;
        if let Some(previous) = self.store.meta(META_INSTANCE) {
            anyhow::ensure!(
                instance.as_deref() == Some(previous.as_str()),
                "hub instance identity changed; refusing recovery {verb}"
            );
        }
        let mut patterns: Vec<String> = super::super::MIRROR_IGNORE_DEFAULTS
            .iter()
            .map(|s| s.to_string())
            .collect();
        patterns.extend(self.hub.exclusions()?);
        patterns.extend(self.opts.ignore.iter().cloned());
        if !dry_run {
            if let Some(instance) = instance {
                self.store.set_meta(META_INSTANCE, &instance)?;
            }
        }
        Ok(IgnoreRules::new(patterns))
    }

    fn pull_recovery_file(
        &self,
        key: &str,
        r: &RemoteStat,
        dry_run: bool,
    ) -> Result<PushItem, HubError> {
        self.verify_destination()?;
        super::super::local::check_key(key).map_err(|e| HubError::Other(e.to_string()))?;
        let item = |action: &str, detail: Option<&str>| PushItem {
            key: key.into(),
            action: action.into(),
            size: r.size,
            detail: detail.map(str::to_string),
        };
        let guard = self.directory_moves.lock();
        if self.local.exists(key) {
            return Ok(item("kept", None));
        }
        if !self.protected_download_allowed(key) || self.is_open_for_write(key) {
            return Ok(item(
                "skipped",
                Some("path is a symlink, open, or under a non-directory"),
            ));
        }
        if dry_run {
            return Ok(item("download", None));
        }
        let epoch = self.directory_epoch.load(Ordering::Relaxed);
        drop(guard);
        // Land only into a still-empty slot; anything that appeared locally
        // meanwhile wins and is left for normal review.
        let landing = || {
            let guard = self.directory_moves.lock();
            if self.directory_epoch.load(Ordering::Relaxed) != epoch
                || !self.protected_download_allowed(key)
                || self.is_open_for_write(key)
            {
                return Err(HubError::LocalChanged(key.into()));
            }
            Ok(guard)
        };
        let (h, _guard) = match self
            .local
            .fetch(&self.hub, key, &r.sha256, r.mtime, &landing)
        {
            Ok(result) => result,
            Err(HubError::NotFound) => return Ok(item("skipped", Some("gone upstream"))),
            Err(HubError::LocalChanged(_)) => {
                return Ok(item("kept", Some("appeared locally during download")))
            }
            Err(e) => return Err(e),
        };
        self.store
            .put_entry(
                key,
                &Entry {
                    sha256: h.sha256.clone(),
                    size: h.size,
                    mtime: h.mtime,
                    state: EntryState::Clean,
                },
            )
            .map_err(|e| HubError::Other(e.to_string()))?;
        self.store
            .put_base(
                key,
                &Base {
                    sha256: h.sha256,
                    size: h.size,
                    mtime: r.mtime,
                    remote_seq: 0,
                },
            )
            .map_err(|e| HubError::Other(e.to_string()))?;
        // The local delete/rename these jobs describe is now undone.
        for job in self.store.jobs() {
            if matches!(&job.kind,
                JobKind::Delete { key: k, .. } | JobKind::Push { key: k }
                | JobKind::Conflict { key: k, .. } | JobKind::Rename { from: k, .. } if k == key)
            {
                self.store
                    .remove_job(job.seq)
                    .map_err(|e| HubError::Other(e.to_string()))?;
            }
        }
        let _ = self.store.remove_blocked_for(key);
        self.record_dirs_for(key);
        self.view_upsert(key, h.size, h.mtime);
        Ok(item("download", None))
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
            let _ = self.store.remove_blocked_for(key);
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
