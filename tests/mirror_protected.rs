//! Protected sync and explicit recovery against a real local folder and a
//! protocol fake. No production data or kernel FUSE mount is needed.
mod fake_hub;

use canvas_fuse::mirror::store::{EntryState, JobKind, Trashed};
use canvas_fuse::mirror::sync::{Mirror, MirrorConfig};
use canvas_fuse::mirror::{DeviceIdentity, MirrorOptions};
use fake_hub::{sha_hex, FakeHub};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

fn open(dir: &tempfile::TempDir, hub: &FakeHub, protected: bool) -> Arc<Mirror> {
    Mirror::open(MirrorConfig {
        data_dir: dir.path().join("data"),
        home_dir: dir.path().join("Home"),
        mountpoint: dir.path().join("Home"),
        server: hub.url.clone(),
        token: "test".into(),
        tls: None,
        workspace_id: "ws1".into(),
        backend: "workspace:home".into(),
        opts: MirrorOptions {
            protected,
            ..Default::default()
        },
        device: DeviceIdentity {
            id: "test-device".into(),
            name: "test".into(),
        },
        status_path: None,
    })
    .unwrap()
}

fn settle(m: &Mirror) {
    for _ in 0..3 {
        m.cycle(false);
    }
}

#[test]
fn upstream_deletes_overwrites_and_renames_are_logged_without_touching_local_files() {
    for full in [false, true] {
        let hub = FakeHub::start();
        for key in ["deleted.txt", "changed.txt", "renamed.txt"] {
            hub.lock().put(key, b"trusted");
        }
        let dir = tempfile::tempdir().unwrap();
        let m = open(&dir, &hub, false);
        m.cycle(true);
        let bases = m.store.bases("");
        drop(m);
        let m = open(&dir, &hub, true);
        hub.lock().delete("deleted.txt");
        hub.lock().put("changed.txt", b"bad automation");
        hub.lock().rename("renamed.txt", "new-name.txt");
        hub.lock().put("additive.txt", b"new file");
        m.cycle(full);
        settle(&m);
        for key in ["deleted.txt", "changed.txt", "renamed.txt"] {
            assert_eq!(m.local.read_all(key).unwrap(), b"trusted");
            assert_eq!(
                m.store.base(key),
                bases.iter().find(|(k, _)| k == key).map(|(_, b)| b.clone())
            );
        }
        assert_eq!(m.local.read_all("additive.txt").unwrap(), b"new file");
        assert!(m.trash_list().is_empty());
        assert!(m.conflicts().is_empty());
        assert!(m.status().protected);
        let audit = m.store.blocked_actions();
        assert!(audit
            .iter()
            .any(|a| a.key == "deleted.txt" && a.action == "trash local file"));
        assert!(audit
            .iter()
            .any(|a| a.key == "changed.txt" && a.action == "replace local file"));
        m.cycle(true);
        let audited = m.store.blocked_actions();
        m.cycle(true);
        assert_eq!(
            m.store.blocked_actions(),
            audited,
            "repeated polls must not rewrite audit timestamps"
        );
        drop(m);
        let restarted = open(&dir, &hub, true);
        assert_eq!(
            restarted.store.blocked_actions(),
            audited,
            "audit survives restart"
        );
    }
}

#[test]
fn previously_queued_destructive_jobs_stay_pending_and_do_not_starve_new_files() {
    let hub = FakeHub::start();
    for key in ["delete.txt", "edit.txt", "move.txt", "Folder/photo.jpg"] {
        hub.lock().put(key, b"base");
    }
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, false);
    m.cycle(true);
    m.delete_local("delete.txt").unwrap();
    m.commit_write("edit.txt", b"new local bytes").unwrap();
    m.rename_local("move.txt", "moved.txt").unwrap();
    m.rename_dir_local("Folder", "Renamed").unwrap();
    m.mkdir_local("Empty").unwrap();
    m.rmdir_local("Empty").unwrap();
    let original_jobs = m.store.jobs();
    drop(m);
    let m = open(&dir, &hub, true);
    m.commit_write("new.txt", b"additive upload").unwrap();
    hub.lock().requests.clear();
    m.cycle(true);
    settle(&m);
    for key in ["delete.txt", "edit.txt", "move.txt", "Folder/photo.jpg"] {
        assert_eq!(hub.lock().objects[key].bytes, b"base");
    }
    assert_eq!(hub.lock().objects["new.txt"].bytes, b"additive upload");
    assert!(!hub.lock().objects.contains_key("moved.txt"));
    assert!(!hub.lock().objects.contains_key("Renamed/photo.jpg"));
    for job in original_jobs
        .iter()
        .filter(|j| !matches!(j.kind, JobKind::Mkdir { .. }))
    {
        assert!(
            m.store.jobs().iter().any(|j| j.kind == job.kind),
            "deferred job lost: {:?}",
            job.kind
        );
    }
    assert!(m.store.jobs().iter().all(|j| j.attempts == 0));
    assert!(!hub.lock().requests.iter().any(|r| r.starts_with("DELETE ")
        || r.ends_with("/objects/rename")
        || r.ends_with("/home/rmdir")));
    assert_eq!(m.local.read_all("edit.txt").unwrap(), b"new local bytes");
}

#[test]
fn an_unbased_create_collision_preserves_both_versions_without_conflict_side_effects() {
    let hub = FakeHub::start();
    hub.lock().put("collision.txt", b"remote");
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.commit_write("collision.txt", b"local").unwrap();
    m.cycle(true);
    settle(&m);
    assert_eq!(hub.lock().objects["collision.txt"].bytes, b"remote");
    assert_eq!(m.local.read_all("collision.txt").unwrap(), b"local");
    assert!(hub.lock().inbox.is_empty());
    assert!(m.conflicts().is_empty());
    assert!(m.store.base("collision.txt").is_none());
    assert!(m
        .store
        .jobs()
        .iter()
        .any(|j| matches!(&j.kind, JobKind::Push { key } if key == "collision.txt")));
}

#[test]
fn a_local_file_appearing_during_download_is_never_replaced() {
    let hub = FakeHub::start();
    hub.lock().put("race.txt", b"remote");
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    let local = m.local.clone();
    hub.lock().on_request = Some(Arc::new(move |request| {
        if request.starts_with("GET ") && request.ends_with("/objects/race.txt") {
            local.write_atomic("race.txt", b"local user write").unwrap();
        }
    }));
    m.cycle(true);
    hub.lock().on_request = None;
    settle(&m);
    assert_eq!(m.local.read_all("race.txt").unwrap(), b"local user write");
    assert_eq!(hub.lock().objects["race.txt"].bytes, b"remote");
}

#[test]
fn protection_keeps_expired_trash_and_conflict_bytes_even_after_new_additions() {
    let hub = FakeHub::start();
    hub.lock().put("old.txt", b"retained");
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, false);
    m.cycle(true);
    hub.lock().delete("old.txt");
    settle(&m);
    m.store
        .put_trashed(
            "old.txt",
            &Trashed {
                sha256: sha_hex(b"retained"),
                size: 8,
                ts: 1,
            },
        )
        .unwrap();
    let conflict_path = m.local.conflict_path(&sha_hex(b"conflict"));
    std::fs::write(&conflict_path, b"conflict").unwrap();
    std::fs::File::open(&conflict_path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH)
        .unwrap();
    drop(m);
    let m = open(&dir, &hub, true);
    m.cycle(false);
    assert_eq!(
        std::fs::read(m.local.trash_path("old.txt")).unwrap(),
        b"retained"
    );
    assert_eq!(std::fs::read(&conflict_path).unwrap(), b"conflict");
    hub.lock().put("old.txt", b"new remote file");
    settle(&m);
    assert_eq!(m.local.read_all("old.txt").unwrap(), b"new remote file");
    assert_eq!(
        std::fs::read(m.local.trash_path("old.txt")).unwrap(),
        b"retained"
    );
    assert!(m
        .store
        .blocked_actions()
        .iter()
        .any(|a| a.action == "expire local trash copy"));
}

#[test]
fn disabling_protection_rechecks_current_state_instead_of_replaying_old_actions() {
    let hub = FakeHub::start();
    hub.lock().put("keep.txt", b"base");
    hub.lock().put("delete.txt", b"base");
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.cycle(true);
    hub.lock().delete("keep.txt");
    hub.lock().delete("delete.txt");
    settle(&m);
    assert_eq!(m.local.read_all("keep.txt").unwrap(), b"base");
    let audit = m.store.blocked_actions();
    drop(m);
    // No feed entry: recovery must use a fresh listing despite the advanced cursor.
    hub.lock().put_silent("keep.txt", b"repaired");
    let m = open(&dir, &hub, false);
    m.cycle(false);
    assert_eq!(m.local.read_all("keep.txt").unwrap(), b"repaired");
    assert!(!m.local.exists("delete.txt"));
    assert_eq!(m.store.blocked_actions(), audit);
}

#[test]
fn recovery_preview_is_read_only_and_push_keeps_remote_extras_with_conditional_uploads() {
    let hub = FakeHub::start();
    hub.lock().put("replace.txt", b"untrusted");
    hub.lock().put("same.txt", b"same");
    hub.lock().put("remote-only.txt", b"keep upstream");
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.commit_write("replace.txt", b"trusted").unwrap();
    m.commit_write("same.txt", b"same").unwrap();
    m.commit_write("new/empty.txt", b"").unwrap();
    std::fs::write(m.local.path(".ignored"), b"private").unwrap();
    std::os::unix::fs::symlink("replace.txt", m.local.path("link.txt")).unwrap();
    let entries = m.store.entries("");
    let jobs = m.store.jobs();
    let bases = m.store.bases("");
    let cursor = m.store.cursor();
    let report = m.push_local(true).unwrap();
    assert_eq!(
        (
            report.created,
            report.replaced,
            report.unchanged,
            report.skipped,
            report.failed
        ),
        (1, 1, 1, 1, 0)
    );
    assert!(report.dry_run);
    assert_eq!(m.store.entries(""), entries);
    assert_eq!(m.store.bases(""), bases);
    assert_eq!(m.store.jobs(), jobs);
    assert_eq!(m.store.cursor(), cursor);
    assert!(hub
        .lock()
        .requests
        .iter()
        .all(|r| r.starts_with("GET ") || r.starts_with("HEAD ")));
    assert_eq!(
        std::fs::read_dir(dir.path().join("data/uploads"))
            .unwrap()
            .count(),
        0
    );
    assert!(!m.local.exists("remote-only.txt"));
    let report = m.push_local(false).unwrap();
    assert_eq!(
        (
            report.created,
            report.replaced,
            report.unchanged,
            report.failed
        ),
        (1, 1, 1, 0)
    );
    assert_eq!(hub.lock().objects["replace.txt"].bytes, b"trusted");
    assert_eq!(
        hub.lock().objects["remote-only.txt"].bytes,
        b"keep upstream"
    );
    assert_eq!(hub.lock().objects["new/empty.txt"].bytes, b"");
    assert!(!hub.lock().objects.contains_key(".ignored"));
    assert!(!hub.lock().objects.contains_key("link.txt"));
    let state = hub.lock();
    let replaced = state
        .log
        .iter()
        .rev()
        .find(|e| e.key == "replace.txt")
        .unwrap();
    assert_eq!(replaced.if_match, Some(sha_hex(b"untrusted")));
    let created = state
        .log
        .iter()
        .rev()
        .find(|e| e.key == "new/empty.txt")
        .unwrap();
    assert_eq!(created.if_none_match.as_deref(), Some("*"));
    drop(state);
    let again = m.push_local(false).unwrap();
    assert_eq!((again.created, again.replaced, again.unchanged), (0, 0, 3));
    assert!(m
        .store
        .entries("")
        .iter()
        .all(|(_, e)| e.state == EntryState::Clean));
}

#[test]
fn recovery_does_not_drain_pending_deletes_or_directory_renames() {
    let hub = FakeHub::start();
    hub.lock().put("delete.txt", b"keep upstream");
    hub.lock().put("Folder/a.txt", b"photo");
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.cycle(true);
    m.delete_local("delete.txt").unwrap();
    m.rename_dir_local("Folder", "Moved").unwrap();
    let pending = m.store.jobs();
    let report = m.push_local(false).unwrap();
    assert_eq!(report.created, 1);
    assert_eq!(hub.lock().objects["Folder/a.txt"].bytes, b"photo");
    assert_eq!(hub.lock().objects["Moved/a.txt"].bytes, b"photo");
    assert_eq!(hub.lock().objects["delete.txt"].bytes, b"keep upstream");
    assert_eq!(m.store.jobs(), pending);
    assert!(hub.lock().directory_renames.is_empty());
    settle(&m);
    assert!(hub.lock().objects.contains_key("delete.txt"));
}

#[test]
fn recovery_never_overwrites_a_concurrent_remote_change() {
    let hub = FakeHub::start();
    hub.lock().put("race.txt", b"old remote");
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.commit_write("race.txt", b"local").unwrap();
    let state = Arc::downgrade(&hub.state);
    let once = AtomicBool::new(false);
    hub.lock().on_request = Some(Arc::new(move |request| {
        if request.starts_with("PUT ") && !once.swap(true, Ordering::SeqCst) {
            state
                .upgrade()
                .unwrap()
                .lock()
                .unwrap()
                .put("race.txt", b"concurrent remote");
        }
    }));
    let report = m.push_local(false).unwrap();
    assert_eq!(report.failed, 1);
    assert!(report.items[0]
        .detail
        .as_ref()
        .unwrap()
        .contains("precondition failed"));
    assert_eq!(hub.lock().objects["race.txt"].bytes, b"concurrent remote");
    assert_eq!(m.local.read_all("race.txt").unwrap(), b"local");
    assert_eq!(m.entry("race.txt").unwrap().state, EntryState::Dirty);
}

#[test]
fn recovery_allows_local_edits_during_upload_and_does_not_mark_them_clean() {
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.commit_write("race.txt", b"snapshot").unwrap();
    let weak = Arc::downgrade(&m);
    hub.lock().on_request = Some(Arc::new(move |request| {
        if request.starts_with("PUT ") {
            weak.upgrade()
                .unwrap()
                .commit_write("race.txt", b"newer local edit")
                .unwrap();
        }
    }));
    let report = m.push_local(false).unwrap();
    assert_eq!(report.created, 1);
    assert!(report.items[0]
        .detail
        .as_ref()
        .unwrap()
        .contains("newer local work"));
    assert_eq!(m.entry("race.txt").unwrap().state, EntryState::Dirty);
    assert_eq!(m.local.read_all("race.txt").unwrap(), b"newer local edit");
    assert_eq!(hub.lock().objects["race.txt"].bytes, b"snapshot");
}

#[test]
fn recovery_requires_protection_and_refuses_replaced_hub_identity() {
    let hub = FakeHub::start();
    hub.lock().instance_id = Some("original".into());
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, false);
    assert!(m
        .push_local(true)
        .unwrap_err()
        .to_string()
        .contains("--protected"));
    assert!(hub.lock().requests.is_empty());
    m.cycle(true);
    drop(m);
    let m = open(&dir, &hub, true);
    m.commit_write("new.txt", b"local").unwrap();
    hub.lock().instance_id = Some("replacement".into());
    hub.lock().requests.clear();
    assert!(m
        .push_local(false)
        .unwrap_err()
        .to_string()
        .contains("identity changed"));
    assert!(!hub.lock().requests.iter().any(|r| r.starts_with("PUT ")));
}

#[test]
fn queued_conflicts_and_resolution_notifications_preserve_originals_and_snapshots() {
    use canvas_fuse::mirror::{store::Conflict, ConflictMode};
    let hub = FakeHub::start();
    let remote = hub.lock().put("conflict.txt", b"remote");
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.commit_write("conflict.txt", b"local").unwrap();
    let sha = m.local.conflict_snapshot("conflict.txt").unwrap();
    m.store
        .put_conflict(&Conflict {
            key: "conflict.txt".into(),
            local_sha256: sha.clone(),
            hub_sha256: Some(remote),
            base_sha256: None,
            ts: 1,
            mode: ConflictMode::Rename,
            uploaded: false,
            inbox_doc_id: None,
            copy_key: None,
            resolved: false,
        })
        .unwrap();
    m.store.remove_jobs_for("conflict.txt").unwrap();
    m.store
        .enqueue(JobKind::Conflict {
            key: "conflict.txt".into(),
            local_sha256: sha.clone(),
            base_sha256: None,
        })
        .unwrap();
    m.cycle(true);
    assert!(hub.lock().inbox.is_empty());
    assert_eq!(hub.lock().objects.len(), 1);
    assert_eq!(m.local.read_all("conflict.txt").unwrap(), b"local");
    m.conflict_resolved("conflict.txt");
    assert!(m.store.conflict("conflict.txt").is_some());
    assert_eq!(
        std::fs::read(m.local.conflict_path(&sha)).unwrap(),
        b"local"
    );
    assert!(m
        .store
        .jobs()
        .iter()
        .any(|j| matches!(j.kind, JobKind::Conflict { .. })));
    let audit = m.store.blocked_actions();
    m.conflict_resolved("unrelated.txt");
    assert_eq!(m.store.blocked_actions(), audit);
}

#[test]
fn control_socket_exposes_audit_and_preview_defaults_to_no_mutations() {
    use canvas_fuse::mirror::control;
    use serde_json::json;
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.commit_write("new.txt", b"local").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let path = dir.path().join("control.sock");
    let _control = control::Control::spawn(&path, m.clone(), stop.clone()).unwrap();
    let preview = control::request(&path, &json!({"cmd":"push"})).unwrap();
    assert_eq!(preview["report"]["dryRun"], true);
    assert_eq!(preview["report"]["created"], 1);
    assert!(hub.lock().objects.is_empty());
    let push = control::request(&path, &json!({"cmd":"push", "dryRun":false})).unwrap();
    assert_eq!(push["report"]["created"], 1);
    hub.lock().delete("new.txt");
    m.cycle(true);
    let audit = control::request(&path, &json!({"cmd":"blocked"})).unwrap();
    assert_eq!(audit["actions"][0]["key"], "new.txt");
    assert_eq!(audit["actions"][0]["action"], "trash local file");
    stop.store(true, Ordering::Relaxed);
}

#[test]
fn recovery_upload_failures_keep_local_files_and_pending_jobs() {
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.commit_write("a.txt", b"a").unwrap();
    m.commit_write("b.txt", b"b").unwrap();
    let jobs = m.store.jobs();
    hub.lock().failures.push((
        "PUT /rest/v2/workspaces/ws1/backends/file/workspace%3Ahome/objects/a.txt".into(),
        401,
    ));
    let report = m.push_local(false).unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(
        report.items.len(),
        1,
        "authentication failure stops the remaining uploads"
    );
    assert!(hub.lock().objects.is_empty());
    assert_eq!(m.store.jobs(), jobs);
    assert_eq!(m.local.read_all("a.txt").unwrap(), b"a");
    assert_eq!(m.local.read_all("b.txt").unwrap(), b"b");
}

#[test]
fn additive_downloads_preserve_symlinks_and_do_not_follow_linked_parents() {
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    let m = open(&dir, &hub, true);
    m.cycle(true);
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("target.txt"), b"outside bytes").unwrap();
    std::os::unix::fs::symlink(outside.join("target.txt"), m.local.path("link.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, m.local.path("linked-dir")).unwrap();
    hub.lock().put("link.txt", b"remote replacement");
    hub.lock().put("linked-dir/new.txt", b"remote creation");
    m.cycle(false);
    assert!(std::fs::symlink_metadata(m.local.path("link.txt"))
        .unwrap()
        .is_symlink());
    assert_eq!(
        std::fs::read(outside.join("target.txt")).unwrap(),
        b"outside bytes"
    );
    assert!(!outside.join("new.txt").exists());
    assert!(!hub.lock().requests.iter().any(|r| r.starts_with("GET ")
        && (r.ends_with("/objects/link.txt") || r.ends_with("/objects/linked-dir/new.txt"))));
}
