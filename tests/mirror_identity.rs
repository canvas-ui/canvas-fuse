//! Destination mistakes must fail before even the initial local scan can
//! manufacture deletes from another replica's ledger.
mod fake_hub;

use canvas_fuse::mirror::identity::{preflight, MARKER};
use canvas_fuse::mirror::store::{EntryState, Store};
use canvas_fuse::mirror::sync::{Mirror, MirrorConfig, SyncState};
use canvas_fuse::mirror::{DeviceIdentity, MirrorOptions};
use fake_hub::FakeHub;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

fn config(root: &Path, url: &str) -> MirrorConfig {
    MirrorConfig {
        data_dir: root.join("state"),
        home_dir: root.join("Universe/Home"),
        server: url.into(),
        token: "never-save-this-token".into(),
        tls: None,
        workspace_id: "ws1".into(),
        backend: "workspace:home".into(),
        opts: MirrorOptions::default(),
        device: DeviceIdentity {
            id: "workstation".into(),
            name: "Workstation".into(),
        },
        mountpoint: root.join("Universe"),
        status_path: None,
    }
}

fn rejected(result: anyhow::Result<Arc<Mirror>>, expected: &str) {
    let error = match result {
        Ok(_) => panic!("unsafe destination was accepted"),
        Err(e) => format!("{e:#}"),
    };
    assert!(error.contains(expected), "{error}");
}

fn marker(root: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(root.join("Universe").join(MARKER)).unwrap()).unwrap()
}

#[test]
fn nonempty_unmarked_destinations_are_refused_without_creating_state_or_contacting_hub() {
    let hub = FakeHub::start();
    for item in [
        "test.txt",
        ".hidden",
        "workspace.json",
        "Home/a.txt",
        "empty/",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), &hub.url);
        let path = cfg.mountpoint.join(item);
        if item.ends_with('/') {
            std::fs::create_dir_all(&path).unwrap();
        } else {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"unrelated test workspace").unwrap();
        }
        assert!(preflight(&cfg.mountpoint).is_err());
        rejected(Mirror::open(cfg), "non-empty");
        assert!(path.exists());
        assert!(!dir.path().join("state").exists());
        assert!(!dir.path().join("Universe").join(MARKER).exists());
    }
    assert!(hub.lock().requests.is_empty());
}

#[test]
fn empty_destination_gets_a_private_local_marker_before_sync_and_can_restart_offline() {
    let dir = tempfile::tempdir().unwrap();
    let url = "http://127.0.0.1:9";
    let m = Mirror::open(config(dir.path(), url)).unwrap();
    let json = marker(dir.path());
    assert_eq!(json["workspaceId"], "ws1");
    assert_eq!(json["server"], url);
    assert_eq!(json["home"], "Home");
    assert_eq!(json["mirrorId"].as_str().unwrap().len(), 32);
    assert!(!json.to_string().contains("never-save-this-token"));
    assert_eq!(m.status().pending, 0);
    drop(m);
    std::fs::write(dir.path().join("Universe/Home/offline.txt"), b"plane notes").unwrap();
    let reopened = Mirror::open(config(dir.path(), url)).unwrap();
    assert_eq!(
        reopened.entry("offline.txt").unwrap().state,
        EntryState::Dirty
    );
    assert_eq!(marker(dir.path()), json);
}

#[test]
fn an_empty_second_folder_cannot_reuse_a_populated_ledger_or_its_pending_delete() {
    let hub = FakeHub::start();
    for n in 0..1000 {
        hub.lock().put(&format!("photo-{n}.jpg"), b"photo");
    }
    let first = tempfile::tempdir().unwrap();
    let m = Mirror::open(config(first.path(), &hub.url)).unwrap();
    m.cycle(true);
    assert_eq!(m.status().entries, 1000);
    m.delete_local("photo-0.jpg").unwrap();
    let jobs = m.store.jobs();
    drop(m);
    hub.lock().requests.clear();

    let second = tempfile::tempdir().unwrap();
    let mut cfg = config(second.path(), &hub.url);
    cfg.data_dir = first.path().join("state");
    rejected(Mirror::open(cfg), "existing mirror state");
    assert!(hub.lock().requests.is_empty());
    assert_eq!(hub.lock().objects.len(), 1000);
    let store = Store::open(&first.path().join("state/mirror.redb")).unwrap();
    assert_eq!(
        store.jobs(),
        jobs,
        "startup must not invent 999 more deletes"
    );
    assert_eq!(store.entry_count(), 1000);
}

#[test]
fn wrong_server_workspace_backend_or_layout_is_refused_before_scanning() {
    let dir = tempfile::tempdir().unwrap();
    let url = "http://127.0.0.1:9";
    drop(Mirror::open(config(dir.path(), url)).unwrap());
    for mismatch in 0..4 {
        let mut cfg = config(dir.path(), url);
        match mismatch {
            0 => cfg.server = "https://other.example".into(),
            1 => cfg.workspace_id = "another-workspace-with-the-same-name".into(),
            2 => cfg.backend = "workspace:data".into(),
            _ => cfg.home_dir = cfg.mountpoint.clone(),
        }
        rejected(Mirror::open(cfg), "does not match");
    }
    // Equivalent spelling of the same endpoint is fine.
    Mirror::open(config(dir.path(), "http://127.0.0.1:9/")).unwrap();
}

#[test]
fn copied_marker_cannot_attach_another_folder_to_the_original_ledger() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let url = "http://127.0.0.1:9";
    drop(Mirror::open(config(first.path(), url)).unwrap());
    std::fs::create_dir_all(second.path().join("Universe/Home")).unwrap();
    std::fs::copy(
        first.path().join("Universe").join(MARKER),
        second.path().join("Universe").join(MARKER),
    )
    .unwrap();
    let mut cfg = config(second.path(), url);
    cfg.data_dir = first.path().join("state");
    rejected(Mirror::open(cfg), "directory identity changed");
}

#[test]
fn copied_database_cannot_cross_two_valid_replica_markers() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let url = "http://127.0.0.1:9";
    let a = Mirror::open(config(first.path(), url)).unwrap();
    a.commit_write("unpushed.txt", b"keep this").unwrap();
    drop(a);
    drop(Mirror::open(config(second.path(), url)).unwrap());
    std::fs::copy(
        first.path().join("state/mirror.redb"),
        second.path().join("state/mirror.redb"),
    )
    .unwrap();
    rejected(
        Mirror::open(config(second.path(), url)),
        "another local replica",
    );
    let store = Store::open(&second.path().join("state/mirror.redb")).unwrap();
    assert_eq!(
        store.entry("unpushed.txt").unwrap().state,
        EntryState::Dirty
    );
}

#[test]
fn missing_or_replaced_home_is_not_recreated_and_interpreted_as_deletion() {
    for replace in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let url = "http://127.0.0.1:9";
        let m = Mirror::open(config(dir.path(), url)).unwrap();
        m.commit_write("original.txt", b"keep").unwrap();
        drop(m);
        let home = dir.path().join("Universe/Home");
        std::fs::rename(&home, dir.path().join("saved-home")).unwrap();
        if replace {
            std::fs::create_dir(&home).unwrap();
        }
        assert!(Mirror::open(config(dir.path(), url)).is_err());
        assert_eq!(home.exists(), replace);
        assert_eq!(
            std::fs::read(dir.path().join("saved-home/original.txt")).unwrap(),
            b"keep"
        );
    }
}

#[test]
fn malformed_symlinked_or_deleted_marker_is_never_overwritten() {
    use std::os::unix::fs::symlink;
    for mode in ["malformed", "symlink", "deleted", "future-version"] {
        let dir = tempfile::tempdir().unwrap();
        let url = "http://127.0.0.1:9";
        drop(Mirror::open(config(dir.path(), url)).unwrap());
        let path = dir.path().join("Universe").join(MARKER);
        match mode {
            "malformed" => std::fs::write(&path, "{truncated").unwrap(),
            "symlink" => {
                std::fs::rename(&path, dir.path().join("marker-backup")).unwrap();
                symlink(dir.path().join("marker-backup"), &path).unwrap();
            }
            "deleted" => std::fs::remove_file(&path).unwrap(),
            _ => {
                let mut json = marker(dir.path());
                json["version"] = 9000.into();
                std::fs::write(&path, json.to_string()).unwrap();
            }
        }
        assert!(Mirror::open(config(dir.path(), url)).is_err(), "{mode}");
        if mode == "deleted" {
            assert!(!path.exists());
        }
        if mode == "malformed" {
            assert_eq!(std::fs::read_to_string(path).unwrap(), "{truncated");
        }
    }
}

#[test]
fn marker_loss_while_running_pauses_before_sending_already_queued_operations() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start();
    hub.lock().put("important.txt", b"remote");
    let m = Mirror::open(config(dir.path(), &hub.url)).unwrap();
    m.cycle(true);
    m.delete_local("important.txt").unwrap();
    std::fs::remove_file(dir.path().join("Universe").join(MARKER)).unwrap();
    hub.lock().requests.clear();
    m.cycle(true);
    assert_eq!(m.status().state, SyncState::Paused);
    assert!(hub.lock().requests.is_empty());
    assert_eq!(hub.lock().bytes_of("important.txt").unwrap(), b"remote");
    assert_eq!(m.status().pending, 1);
}

#[test]
fn laptop_and_workstation_have_independent_local_identities_for_the_same_workspace() {
    let hub = FakeHub::start();
    hub.lock().put("shared.txt", b"original");
    let workstation = tempfile::tempdir().unwrap();
    let laptop = tempfile::tempdir().unwrap();
    let a = Mirror::open(config(workstation.path(), &hub.url)).unwrap();
    let mut cfg = config(laptop.path(), &hub.url);
    cfg.device = DeviceIdentity {
        id: "laptop".into(),
        name: "Laptop".into(),
    };
    let b = Mirror::open(cfg).unwrap();
    a.cycle(true);
    b.cycle(true);
    let am = marker(workstation.path());
    let bm = marker(laptop.path());
    assert_eq!(am["workspaceId"], bm["workspaceId"]);
    assert_ne!(am["mirrorId"], bm["mirrorId"]);
    a.commit_write("shared.txt", b"workstation edit").unwrap();
    a.cycle(false);
    b.cycle(false);
    assert_eq!(b.local.read_all("shared.txt").unwrap(), b"workstation edit");
    b.commit_write("laptop.txt", b"laptop notes").unwrap();
    b.cycle(false);
    a.cycle(false);
    assert_eq!(a.local.read_all("laptop.txt").unwrap(), b"laptop notes");
    assert!(hub.lock().objects.keys().all(|key| !key.contains(MARKER)));
}

#[test]
fn simultaneous_processes_cannot_share_a_folder_even_with_different_state_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let url = "http://127.0.0.1:9";
    let _m = Mirror::open(config(dir.path(), url)).unwrap();
    let mut cfg = config(dir.path(), url);
    cfg.data_dir = dir.path().join("other-state");
    rejected(Mirror::open(cfg), "already in use");
    // Nor can a home-only mirror open the Home of a running full mount.
    let mut cfg = config(dir.path(), url);
    cfg.mountpoint = cfg.home_dir.clone();
    cfg.data_dir = dir.path().join("other-state");
    rejected(Mirror::open(cfg), "already in use");
    assert!(!dir.path().join("other-state").exists());
}

#[test]
fn home_only_layout_keeps_the_marker_local_and_supports_normal_offline_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start();
    hub.lock().put("remove.txt", b"remove deliberately");
    let make_config = || {
        let mut cfg = config(dir.path(), &hub.url);
        cfg.home_dir = cfg.mountpoint.clone();
        cfg
    };
    let m = Mirror::open(make_config()).unwrap();
    m.cycle(true);
    assert_eq!(marker(dir.path())["home"], ".");
    drop(m);
    std::fs::remove_file(dir.path().join("Universe/remove.txt")).unwrap();
    let m = Mirror::open(make_config()).unwrap();
    m.cycle(true);
    assert!(hub.lock().bytes_of("remove.txt").is_none());
    assert_eq!(hub.lock().entries("delete").len(), 1);
    assert!(m.entry(MARKER).is_none());
}

#[test]
fn mount_locations_get_separate_default_databases() {
    use canvas_fuse::runtime::workspace_mount_data_dir;
    let a = workspace_mount_data_dir("canvas.example", "Universe", Path::new("/work/Universe"));
    let b = workspace_mount_data_dir("canvas.example", "Universe", Path::new("/test/Universe"));
    assert_ne!(a, b);
    assert_eq!(
        a,
        workspace_mount_data_dir("canvas.example", "Universe", Path::new("/work/Universe"))
    );
}

#[test]
fn changed_hub_instance_pauses_before_sending_a_pending_delete() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start();
    hub.lock().put("important.txt", b"keep this");
    let m = Mirror::open(config(dir.path(), &hub.url)).unwrap();
    m.cycle(true);
    m.delete_local("important.txt").unwrap();
    hub.lock().instance_id = Some("replacement-server".into());
    hub.lock().requests.clear();
    m.cycle(false);
    assert_eq!(m.status().state, SyncState::Paused);
    assert_eq!(hub.lock().bytes_of("important.txt").unwrap(), b"keep this");
    assert_eq!(hub.lock().requests, ["GET /rest/v2/ping"]);
    assert_eq!(m.status().pending, 1);
}

#[test]
fn replacing_home_while_running_pauses_without_generating_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start();
    hub.lock().put("important.txt", b"keep this");
    let m = Mirror::open(config(dir.path(), &hub.url)).unwrap();
    m.cycle(true);
    std::fs::rename(dir.path().join("Universe/Home"), dir.path().join("saved")).unwrap();
    std::fs::create_dir(dir.path().join("Universe/Home")).unwrap();
    hub.lock().requests.clear();
    m.cycle(true);
    assert_eq!(m.status().state, SyncState::Paused);
    assert_eq!(m.status().pending, 0);
    assert!(hub.lock().requests.is_empty());
}

#[test]
fn filesystem_operations_cannot_clobber_the_home_only_identity_marker() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), "http://127.0.0.1:9");
    cfg.home_dir = cfg.mountpoint.clone();
    let m = Mirror::open(cfg).unwrap();
    let original = marker(dir.path());
    m.commit_write("temp.txt", b"editor save").unwrap();
    assert!(m.open_local_write(MARKER, false, true).is_err());
    assert!(m.commit_write(MARKER, b"clobber").is_err());
    assert!(m.delete_local(MARKER).is_err());
    assert!(m.rename_local("temp.txt", MARKER).is_err());
    assert!(m.rename_local(MARKER, "old-marker.json").is_err());
    assert_eq!(marker(dir.path()), original);
}

#[test]
fn state_directory_cannot_be_inside_the_mounted_folder_even_through_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("Universe")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("Universe"), dir.path().join("alias")).unwrap();
    for path in ["Universe/state", "alias/state"] {
        let mut cfg = config(dir.path(), "http://127.0.0.1:9");
        cfg.data_dir = dir.path().join(path);
        rejected(Mirror::open(cfg), "outside the mountpoint");
        assert!(!dir.path().join(path).exists());
    }
}

// Simulate the mount-time device number that an earlier boot wrote, keeping
// the marker and its exact ledger binding together as a real remount would.
fn previous_device(root: &Path, legacy: bool) -> Value {
    let mut json = marker(root);
    for field in ["rootDirectory", "homeDirectory"] {
        json[field]["device"] = (json[field]["device"].as_u64().unwrap() + 123).into();
    }
    let raw = serde_json::to_string(&json).unwrap();
    std::fs::write(root.join("Universe").join(MARKER), &raw).unwrap();
    let store = Store::open(&root.join("state/mirror.redb")).unwrap();
    store.set_meta("local-replica-binding", &raw).unwrap();
    if legacy {
        store.remove_meta("local-replica-directories").unwrap();
    }
    json
}

#[test]
fn persistent_identity_survives_device_renumbering_without_resetting_history() {
    for home_only in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let hub = FakeHub::start();
        hub.lock().put("keep.txt", b"keep");
        hub.lock().put("remove.txt", b"remove offline");
        let make_config = || {
            let mut cfg = config(dir.path(), &hub.url);
            if home_only {
                cfg.home_dir = cfg.mountpoint.clone();
            }
            cfg
        };
        let m = Mirror::open(make_config()).unwrap();
        m.cycle(true);
        let cursor = m.store.cursor();
        m.commit_write("queued.txt", b"queued before shutdown")
            .unwrap();
        drop(m);
        let marker_before = previous_device(dir.path(), false);
        let home = make_config().home_dir;
        std::fs::remove_file(home.join("remove.txt")).unwrap();
        std::fs::write(home.join("flight.txt"), b"flight notes").unwrap();
        hub.lock().requests.clear();

        let m = Mirror::open(make_config()).unwrap();
        assert!(
            hub.lock().requests.is_empty(),
            "opening must be entirely local"
        );
        assert_eq!(m.store.cursor(), cursor);
        assert!(m.store.base("keep.txt").is_some());
        assert_eq!(
            marker(dir.path()),
            marker_before,
            "no two-file marker migration"
        );
        assert_eq!(m.entry("flight.txt").unwrap().state, EntryState::Dirty);
        assert_eq!(
            m.store.entry("remove.txt").unwrap().state,
            EntryState::Tombstone
        );
        m.cycle(true);
        assert_eq!(hub.lock().bytes_of("flight.txt").unwrap(), b"flight notes");
        assert_eq!(
            hub.lock().bytes_of("queued.txt").unwrap(),
            b"queued before shutdown"
        );
        assert_eq!(hub.lock().bytes_of("keep.txt").unwrap(), b"keep");
        assert!(hub.lock().bytes_of("remove.txt").is_none());
    }
}

#[test]
fn legacy_device_change_requires_explicit_migration_then_reopens_normally_offline() {
    let dir = tempfile::tempdir().unwrap();
    let url = "http://127.0.0.1:9";
    let m = Mirror::open(config(dir.path(), url)).unwrap();
    m.commit_write("notes.txt", b"offline work").unwrap();
    let jobs = m.store.jobs();
    drop(m);
    let old = previous_device(dir.path(), true);
    rejected(
        Mirror::open(config(dir.path(), url)),
        "--accept-device-change",
    );
    let store = Store::open(&dir.path().join("state/mirror.redb")).unwrap();
    assert_eq!(store.jobs(), jobs);
    assert!(store.meta("local-replica-directories").is_none());
    drop(store);
    let mut cfg = config(dir.path(), url);
    cfg.opts.accept_device_change = true;
    let m = Mirror::open(cfg).unwrap();
    assert_eq!(m.store.jobs(), jobs);
    assert!(m.store.meta("local-replica-directories").is_some());
    assert_eq!(marker(dir.path()), old);
    drop(m);
    let m = Mirror::open(config(dir.path(), url)).unwrap();
    assert_eq!(m.local.read_all("notes.txt").unwrap(), b"offline work");
}

#[test]
fn legacy_replica_upgrades_automatically_when_original_directory_ids_match() {
    let dir = tempfile::tempdir().unwrap();
    let url = "http://127.0.0.1:9";
    let m = Mirror::open(config(dir.path(), url)).unwrap();
    m.store.remove_meta("local-replica-directories").unwrap();
    let old = marker(dir.path());
    drop(m);
    let m = Mirror::open(config(dir.path(), url)).unwrap();
    assert!(m.store.meta("local-replica-directories").is_some());
    assert_eq!(marker(dir.path()), old);
}

#[test]
fn device_migration_cannot_override_a_persistent_filesystem_or_birth_time_mismatch() {
    for field in ["filesystemId", "created"] {
        let dir = tempfile::tempdir().unwrap();
        let url = "http://127.0.0.1:9";
        let m = Mirror::open(config(dir.path(), url)).unwrap();
        m.commit_write("important.txt", b"keep").unwrap();
        let jobs = m.store.jobs();
        let mut saved: Value =
            serde_json::from_str(&m.store.meta("local-replica-directories").unwrap()).unwrap();
        saved[1][field] = serde_json::json!([123456789, 123456789]);
        m.store
            .set_meta("local-replica-directories", &saved.to_string())
            .unwrap();
        drop(m);
        let mut cfg = config(dir.path(), url);
        cfg.opts.accept_device_change = true;
        rejected(Mirror::open(cfg), "mirror directory identity changed");
        let store = Store::open(&dir.path().join("state/mirror.redb")).unwrap();
        assert_eq!(store.jobs(), jobs);
        assert_eq!(
            store.meta("local-replica-directories").unwrap(),
            saved.to_string()
        );
    }
}

#[test]
fn device_migration_still_rejects_copied_markers_replaced_home_and_wrong_databases() {
    for mode in ["copied", "replaced", "wrong-database", "missing-database"] {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let url = "http://127.0.0.1:9";
        drop(Mirror::open(config(first.path(), url)).unwrap());
        previous_device(first.path(), true);
        let mut cfg = config(first.path(), url);
        cfg.opts.accept_device_change = true;
        match mode {
            "copied" => {
                std::fs::create_dir_all(second.path().join("Universe/Home")).unwrap();
                std::fs::copy(
                    first.path().join("Universe").join(MARKER),
                    second.path().join("Universe").join(MARKER),
                )
                .unwrap();
                cfg.mountpoint = second.path().join("Universe");
                cfg.home_dir = cfg.mountpoint.join("Home");
            }
            "replaced" => {
                std::fs::rename(&cfg.home_dir, first.path().join("saved")).unwrap();
                std::fs::create_dir(&cfg.home_dir).unwrap();
            }
            "wrong-database" => {
                drop(Mirror::open(config(second.path(), url)).unwrap());
                cfg.data_dir = second.path().join("state");
            }
            _ => cfg.data_dir = second.path().join("state"),
        }
        assert!(Mirror::open(cfg).is_err(), "{mode}");
    }
}

#[test]
fn offline_workspace_resolution_requires_a_matching_marker_uuid_server_and_name() {
    use canvas_fuse::mirror::identity::cached_workspace;
    let dir = tempfile::tempdir().unwrap();
    let url = "http://127.0.0.1:9";
    let cfg = config(dir.path(), url);
    assert!(
        cached_workspace(&cfg.mountpoint, &cfg.data_dir, url, "Universe")
            .unwrap()
            .is_none()
    );
    drop(Mirror::open(config(dir.path(), url)).unwrap());
    let resolve =
        |server: &str, name: &str| cached_workspace(&cfg.mountpoint, &cfg.data_dir, server, name);
    // The UUID works even if the name cache is missing/corrupted.
    assert_eq!(resolve(url, "ws1").unwrap().unwrap().id, "ws1");
    assert!(resolve(url, "Universe").unwrap().is_none());
    std::fs::write(
        cfg.data_dir.join("workspace.json"),
        r#"{"id":"ws1","name":"Universe","selector":"universe"}"#,
    )
    .unwrap();
    assert_eq!(resolve(url, "Universe").unwrap().unwrap().id, "ws1");
    assert_eq!(resolve(url, "universe").unwrap().unwrap().name, "Universe");
    assert!(resolve("http://other.example", "Universe").is_err());
    assert!(resolve(url, "Augmentd").unwrap().is_none());
    // A stale/wrong data directory cannot route a mirror to another workspace.
    std::fs::write(
        cfg.data_dir.join("workspace.json"),
        r#"{"id":"different","name":"Universe"}"#,
    )
    .unwrap();
    assert!(resolve(url, "Universe").unwrap().is_none());
    assert_eq!(resolve(url, "ws1").unwrap().unwrap().id, "ws1");
}

#[test]
fn interrupted_directory_moves_recover_after_device_renumbering() {
    use canvas_fuse::mirror::store::JobKind;
    use std::os::unix::fs::MetadataExt;
    for legacy_job in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let hub = FakeHub::start();
        hub.lock().put("old/a.txt", b"keep photo");
        let m = Mirror::open(config(dir.path(), &hub.url)).unwrap();
        m.cycle(true);
        let st = std::fs::metadata(m.local.path("old")).unwrap();
        // Obtain the real persistent identity written by a normal directory move.
        m.rename_dir_local("old", "new").unwrap();
        let job = m
            .store
            .jobs()
            .into_iter()
            .find(|j| matches!(j.kind, JobKind::RenameDir { .. }))
            .unwrap();
        let JobKind::RenameDir {
            directory_identity, ..
        } = job.kind
        else {
            unreachable!()
        };
        m.store.remove_job(job.seq).unwrap();
        // Restore the pre-commit ledger to simulate a crash between rename(2)
        // and committing the ledger transaction.
        m.store.rekey("new/a.txt", "old/a.txt").unwrap();
        m.store.remove_dir("new").unwrap();
        m.store.add_dir("old").unwrap();
        m.store
            .enqueue(JobKind::RenameDir {
                from: "old".into(),
                to: "new".into(),
                operation_id: "reboot-recovery".into(),
                dev: st.dev() + 123,
                ino: st.ino(),
                directory_identity: if legacy_job { None } else { directory_identity },
                local_applied: false,
                remote: true,
            })
            .unwrap();
        drop(m);
        previous_device(dir.path(), false);
        let m = Mirror::open(config(dir.path(), &hub.url)).unwrap();
        m.cycle(true);
        assert!(m.store.jobs().is_empty(), "{:?}", m.status());
        assert!(m.store.entry("old/a.txt").is_none());
        assert_eq!(m.local.read_all("new/a.txt").unwrap(), b"keep photo");
        assert_eq!(hub.lock().bytes_of("new/a.txt").unwrap(), b"keep photo");
    }
}
