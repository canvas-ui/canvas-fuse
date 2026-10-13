//! Exercise the real mount preparation, stopping only at the kernel FUSE
//! boundary so these checks run without /dev/fuse or an actual mounted volume.
#[path = "../tests/fake_hub/mod.rs"]
mod fake_hub;

use super::*;
use mirror::store::{EntryState, Store};
use mirror::sync::{Mirror, MirrorConfig};
use mirror::{DeviceIdentity, MirrorOptions};

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
