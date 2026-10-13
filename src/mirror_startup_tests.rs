//! Exercise the real mount preparation, stopping only at the kernel FUSE
//! boundary so these checks run without /dev/fuse or an actual mounted volume.
#[path = "../tests/fake_hub/mod.rs"]
mod fake_hub;

use super::*;
use mirror::store::{EntryState, Store};
use mirror::sync::{Mirror, MirrorConfig};
use mirror::{DeviceIdentity, MirrorOptions};

fn named_opts(root: &std::path::Path, server: &str) -> MountOptions {
    MountOptions {
        server: server.into(),
        token: "test".into(),
        tls: None,
        mountpoint: root.join("Augmentd"),
        data_dir: root.join("state"),
        enable_ws: true,
        resync_secs: 60,
        contexts: None,
        context_root: None,
        contexts_at_root: false,
        workspace: Some("Augmentd".into()),
        selection: WorkspaceSelection::default(),
        context_workspace: None,
        blob_cache_bytes: 1024,
        enable_nudge: false,
        mirror: Some(MirrorOptions::default()),
    }
}

fn old_named_replica(root: &std::path::Path, server: &str) -> MountOptions {
    let opts = named_opts(root, server);
    let m = Mirror::open(MirrorConfig {
        data_dir: opts.data_dir.clone(),
        home_dir: opts.mountpoint.join("Home"),
        server: server.into(),
        token: "test".into(),
        tls: None,
        workspace_id: "ws1".into(),
        backend: mirror::DEFAULT_BACKEND.into(),
        opts: MirrorOptions::default(),
        device: DeviceIdentity {
            id: "test".into(),
            name: "test".into(),
        },
        mountpoint: opts.mountpoint.clone(),
        status_path: None,
    })
    .unwrap();
    m.commit_write("queued.txt", b"queued before shutdown")
        .unwrap();
    drop(m);
    std::fs::write(opts.mountpoint.join("Home/flight.txt"), b"offline work").unwrap();
    opts
}

fn simulate_legacy_reboot(opts: &MountOptions) -> String {
    let marker_path = opts.mountpoint.join(mirror::identity::MARKER);
    let mut marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
    for field in ["rootDirectory", "homeDirectory"] {
        marker[field]["device"] = (marker[field]["device"].as_u64().unwrap() + 123).into();
    }
    let original_marker = serde_json::to_string(&marker).unwrap();
    std::fs::write(&marker_path, &original_marker).unwrap();
    let store = Store::open(&opts.data_dir.join("mirror.redb")).unwrap();
    store
        .set_meta("local-replica-binding", &original_marker)
        .unwrap();
    store.remove_meta("local-replica-directories").unwrap();
    original_marker
}

#[test]
fn legacy_name_cache_is_repaired_online_then_remounts_without_network() {
    for cache in [
        None,
        Some("broken JSON"),
        Some(r#"{"id":"other","name":"Augmentd"}"#),
        Some(r#"{"id":"ws1","name":"Old name"}"#),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let hub = fake_hub::FakeHub::start();
        hub.lock().workspaces.insert(
            "Augmentd".into(),
            serde_json::json!({"id":"ws1","name":"Augmentd"}),
        );
        let mut opts = old_named_replica(dir.path(), &hub.url);
        let marker_path = opts.mountpoint.join(mirror::identity::MARKER);
        let cache_path = opts.data_dir.join("workspace.json");
        if let Some(cache) = cache {
            std::fs::write(&cache_path, cache).unwrap();
        }
        // Reproduce a pre-persistent-identity marker/ledger pair after ZFS
        // renumbered st_dev on reboot, with no usable display-name cache.
        let original_marker = simulate_legacy_reboot(&opts);
        let store = Store::open(&opts.data_dir.join("mirror.redb")).unwrap();
        let queued = store.jobs();
        drop(store);
        opts.mirror.as_mut().unwrap().accept_device_change = true;
        let mut mounted = false;
        let result = mount_with_session(opts, |_fs, _path| {
            mounted = true;
            assert_eq!(hub.lock().requests, ["GET /rest/v2/workspaces/Augmentd"]);
            anyhow::bail!("test stops at kernel boundary")
        });
        assert!(mounted, "{:?}", result.err());
        assert_eq!(
            std::fs::read_to_string(&marker_path).unwrap(),
            original_marker
        );
        let cache: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
        assert_eq!(
            cache,
            serde_json::json!({"id":"ws1","name":"Augmentd","selector":"Augmentd"})
        );
        let store = Store::open(&dir.path().join("state/mirror.redb")).unwrap();
        assert!(store.meta("local-replica-directories").is_some());
        assert!(queued.iter().all(|job| store.jobs().contains(job)));
        assert_eq!(store.jobs().len(), 2);
        assert_eq!(store.entry("flight.txt").unwrap().state, EntryState::Dirty);
        drop(store);

        hub.lock().requests.clear();
        hub.lock()
            .failures
            .push(("GET /rest/v2/workspaces/Augmentd".into(), 503));
        let mut remounted = false;
        let result = mount_with_session(named_opts(dir.path(), &hub.url), |_fs, _path| {
            remounted = true;
            anyhow::bail!("test stops at kernel boundary")
        });
        assert!(remounted, "{:?}", result.err());
        assert!(
            hub.lock().requests.is_empty(),
            "repaired replica must remount offline without the migration flag"
        );
    }
}

#[test]
fn unverified_workspace_names_fail_without_scanning_or_replacing_the_cache() {
    for failure in ["wrong-uuid", "unauthorized", "missing", "unavailable"] {
        let dir = tempfile::tempdir().unwrap();
        let hub = fake_hub::FakeHub::start();
        let opts = old_named_replica(dir.path(), &hub.url);
        let marker = std::fs::read(opts.mountpoint.join(mirror::identity::MARKER)).unwrap();
        let cache_path = opts.data_dir.join("workspace.json");
        let old_cache = br#"{"id":"unrelated","name":"Augmentd"}"#;
        std::fs::write(&cache_path, old_cache).unwrap();
        match failure {
            "wrong-uuid" => {
                hub.lock().workspaces.insert(
                    "Augmentd".into(),
                    serde_json::json!({"id":"ws2","name":"Augmentd"}),
                );
            }
            "unauthorized" => hub
                .lock()
                .failures
                .push(("GET /rest/v2/workspaces/Augmentd".into(), 401)),
            "unavailable" => hub
                .lock()
                .failures
                .push(("GET /rest/v2/workspaces/Augmentd".into(), 503)),
            _ => {}
        }
        let error = mount_with_session(opts, |_, _| panic!("unsafe mount accepted"))
            .err()
            .unwrap();
        let message = format!("{error:#}");
        assert!(
            message.contains(if failure == "wrong-uuid" {
                "belongs to UUID ws1"
            } else {
                "retry the same mount command"
            }),
            "{message}"
        );
        assert_eq!(std::fs::read(&cache_path).unwrap(), old_cache);
        assert_eq!(
            std::fs::read(dir.path().join("Augmentd/.workspace.json")).unwrap(),
            marker
        );
        assert_eq!(hub.lock().requests, ["GET /rest/v2/workspaces/Augmentd"]);
        let store = Store::open(&dir.path().join("state/mirror.redb")).unwrap();
        assert_eq!(
            store.jobs().len(),
            1,
            "rejected name must not scan local files"
        );
        assert!(store.entry("flight.txt").is_none());
    }
}

#[test]
fn verified_remote_name_cannot_override_legacy_replica_checks() {
    for missing_ledger in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let hub = fake_hub::FakeHub::start();
        hub.lock().workspaces.insert(
            "Augmentd".into(),
            serde_json::json!({"id":"ws1","name":"Augmentd"}),
        );
        let mut opts = old_named_replica(dir.path(), &hub.url);
        simulate_legacy_reboot(&opts);
        if missing_ledger {
            opts.data_dir = dir.path().join("new-state");
        } else {
            std::fs::rename(opts.mountpoint.join("Home"), dir.path().join("saved-home")).unwrap();
            std::fs::create_dir(opts.mountpoint.join("Home")).unwrap();
        }
        opts.mirror.as_mut().unwrap().accept_device_change = true;
        let cache_path = opts.data_dir.join("workspace.json");
        let result = mount_with_session(opts, |_, _| panic!("unsafe mount accepted"));
        assert!(result.is_err());
        assert!(
            !cache_path.exists(),
            "cache repair must follow directory and ledger validation"
        );
        let store = Store::open(&dir.path().join("state/mirror.redb")).unwrap();
        assert_eq!(store.jobs().len(), 1);
    }
}

#[test]
fn existing_mirror_reaches_the_kernel_mount_without_any_network_requests() {
    for home_only in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let hub = fake_hub::FakeHub::start();
        let mountpoint = dir.path().join("Universe");
        let data_dir = dir.path().join("state");
        let home_dir = if home_only {
            mountpoint.clone()
        } else {
            mountpoint.join("Home")
        };
        let m = Mirror::open(MirrorConfig {
            data_dir: data_dir.clone(),
            home_dir: home_dir.clone(),
            server: hub.url.clone(),
            token: "test".into(),
            tls: None,
            workspace_id: "ws1".into(),
            backend: mirror::DEFAULT_BACKEND.into(),
            opts: MirrorOptions::default(),
            device: DeviceIdentity {
                id: "test".into(),
                name: "test".into(),
            },
            mountpoint: mountpoint.clone(),
            status_path: None,
        })
        .unwrap();
        m.commit_write("queued.txt", b"queued before shutdown")
            .unwrap();
        drop(m);
        std::fs::write(
            data_dir.join("workspace.json"),
            r#"{"id":"ws1","name":"Universe"}"#,
        )
        .unwrap();
        std::fs::write(home_dir.join("flight.txt"), b"work done offline").unwrap();
        let opts = MountOptions {
            server: hub.url.clone(),
            token: "test".into(),
            tls: None,
            mountpoint: mountpoint.clone(),
            data_dir: data_dir.clone(),
            enable_ws: true,
            resync_secs: 60,
            contexts: None,
            context_root: None,
            contexts_at_root: false,
            workspace: Some("Universe".into()),
            selection: WorkspaceSelection {
                home: true,
                // Resuming a combined layout must not block on tree validation.
                trees: if home_only {
                    vec![]
                } else {
                    vec!["tags".into()]
                },
                include_backends: false,
            },
            context_workspace: None,
            blob_cache_bytes: 1024,
            enable_nudge: false,
            mirror: Some(MirrorOptions::default()),
        };
        let mut reached_mount = false;
        let result = mount_with_session(opts, |_fs, path| {
            reached_mount = true;
            assert_eq!(path, mountpoint);
            assert!(
                hub.lock().requests.is_empty(),
                "no ping, workspace lookup or remote bootstrap before mounting"
            );
            assert_eq!(
                std::fs::read(home_dir.join("flight.txt")).unwrap(),
                b"work done offline"
            );
            anyhow::bail!("test stops at the kernel boundary")
        });
        assert!(reached_mount, "{:?}", result.err());
        assert!(hub.lock().requests.is_empty());
        let store = Store::open(&data_dir.join("mirror.redb")).unwrap();
        assert_eq!(store.entry("flight.txt").unwrap().state, EntryState::Dirty);
        assert_eq!(store.entry("queued.txt").unwrap().state, EntryState::Dirty);
        assert_eq!(store.jobs().len(), 2);
    }
}
