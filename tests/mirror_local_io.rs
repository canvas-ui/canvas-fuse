//! The FUSE write facade against real disk and an explicitly stalled hub.
mod fake_hub;

use canvas_fuse::mirror::sync::{Mirror, MirrorConfig, ViewLink};
use canvas_fuse::mirror::{DeviceIdentity, MirrorOptions};
use canvas_fuse::{
    api::ApiClient,
    names::NameStore,
    state::{Tree, HOME_INO},
    writes::WriteStore,
};
use fake_hub::FakeHub;
use parking_lot::{Mutex, RwLock};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

struct Rig {
    writes: Arc<WriteStore>,
    tree: Arc<RwLock<Tree>>,
    mirror: Arc<Mirror>,
    hub: FakeHub,
    dir: tempfile::TempDir,
}

impl Rig {
    fn new(files: &[(&str, &[u8])]) -> Self {
        Self::with_initial_sync(files, true)
    }

    fn with_initial_sync(files: &[(&str, &[u8])], sync: bool) -> Self {
        let hub = FakeHub::start();
        for (key, bytes) in files {
            hub.lock().put(key, bytes);
        }
        let dir = tempfile::tempdir().unwrap();
        let mirror = Mirror::open(MirrorConfig {
            data_dir: dir.path().join("state"),
            home_dir: dir.path().join("Home"),
            server: hub.url.clone(),
            token: "test".into(),
            tls: None,
            workspace_id: "ws1".into(),
            backend: "workspace:home".into(),
            opts: MirrorOptions::default(),
            device: DeviceIdentity {
                id: "local-device".into(),
                name: "local".into(),
            },
            mountpoint: dir.path().join("Home"),
            status_path: None,
        })
        .unwrap();
        if sync {
            mirror.cycle(true);
        }
        let mut tree = Tree::workspace_rooted("ws1".into(), "ws1".into());
        tree.set_home_mirrored(true);
        mirror.snapshot_into(&mut tree);
        let tree = Arc::new(RwLock::new(tree));
        let writes = Arc::new(WriteStore::new(
            Arc::new(ApiClient::new(&hub.url, "test").unwrap()),
            tree.clone(),
            Arc::new(NameStore::open(&dir.path().join("names.redb")).unwrap()),
            Some(mirror.clone()),
        ));
        let (tx, _rx) = mpsc::channel();
        mirror.attach_view(ViewLink {
            tree: tree.clone(),
            refresh_lock: writes.home_sync_handle(),
            invalidations: Arc::new(Mutex::new(Vec::new())),
            job_tx: tx,
        });
        Self {
            writes,
            tree,
            mirror,
            hub,
            dir,
        }
    }
    fn read(&self, key: &str) -> Vec<u8> {
        std::fs::read(self.dir.path().join("Home").join(key)).unwrap()
    }
    fn settle(&self) {
        for _ in 0..5 {
            self.mirror.cycle(false);
        }
    }
}

fn save(writes: &WriteStore, parent: u64, name: &str, bytes: &[u8]) -> u64 {
    let ino = writes.create(parent, name).unwrap().ino;
    writes.write(ino, 0, bytes).unwrap();
    writes.flush_final(ino).unwrap();
    writes.release(ino);
    ino
}

/// Release the server even on failure, so a regression fails instead of hanging.
fn while_stalled(
    r: &Rig,
    prefix: &'static str,
    suffix: &'static str,
    local: impl FnOnce(Arc<WriteStore>, Arc<RwLock<Tree>>) + Send + 'static,
) {
    let mirror = r.mirror.clone();
    while_stalled_work(r, prefix, suffix, move || mirror.cycle(false), local);
}

fn while_stalled_work(
    r: &Rig,
    prefix: &'static str,
    suffix: &'static str,
    background: impl FnOnce() + Send + 'static,
    local: impl FnOnce(Arc<WriteStore>, Arc<RwLock<Tree>>) + Send + 'static,
) {
    while_request_stalled(
        r,
        move |request| request.starts_with(prefix) && request.ends_with(suffix),
        background,
        local,
    );
}

fn while_request_stalled(
    r: &Rig,
    matches: impl Fn(&str) -> bool + Send + Sync + 'static,
    background: impl FnOnce() + Send + 'static,
    local: impl FnOnce(Arc<WriteStore>, Arc<RwLock<Tree>>) + Send + 'static,
) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let once = AtomicBool::new(false);
    r.hub.lock().on_request = Some(Arc::new(move |request| {
        if matches(request) && !once.swap(true, Ordering::SeqCst) {
            entered_tx.send(()).unwrap();
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
        }
    }));
    let engine = std::thread::spawn(background);
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("hub request started");
    let writes = r.writes.clone();
    let tree = r.tree.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let io = std::thread::spawn(move || {
        local(writes, tree);
        let _ = done_tx.send(());
    });
    let completed = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let _ = release_tx.send(());
    engine.join().unwrap();
    io.join().unwrap();
    r.hub.lock().on_request = None;
    assert!(completed, "local filesystem operation waited for the hub");
}

#[test]
fn moving_an_unhydrated_folder_moves_remote_children_without_resurrecting_the_old_path() {
    let r = Rig::with_initial_sync(
        &[
            ("Architecture/first.jpg", b"photo"),
            ("Z/plan.txt", b"plan"),
        ],
        false,
    );
    let mirror = r.mirror.clone();
    while_stalled_work(
        &r,
        "GET ",
        "/objects/Architecture/first.jpg",
        move || mirror.cycle(true),
        |writes, tree| {
            assert!(tree.read().home_ino_for_key("Z").is_some());
            assert!(tree.read().home_ino_for_key("Z/plan.txt").is_none());
            writes.rename(HOME_INO, "Z", HOME_INO, "Kitchen").unwrap();
            let parent = tree.read().home_ino_for_key("Kitchen").unwrap();
            save(&writes, parent, "new.txt", b"new work");
        },
    );
    r.settle();
    assert!(!r.dir.path().join("Home/Z").exists());
    assert_eq!(r.read("Kitchen/plan.txt"), b"plan");
    assert_eq!(r.read("Kitchen/new.txt"), b"new work");
    assert_eq!(
        r.hub.lock().directory_renames,
        vec![("Z".into(), "Kitchen".into())]
    );
}

#[test]
fn removing_an_unhydrated_folder_cannot_delete_remote_children() {
    let r = Rig::with_initial_sync(
        &[
            ("Architecture/first.jpg", b"photo"),
            ("Z/plan.txt", b"plan"),
        ],
        false,
    );
    let mirror = r.mirror.clone();
    while_stalled_work(
        &r,
        "GET ",
        "/objects/Architecture/first.jpg",
        move || mirror.cycle(true),
        |writes, _| {
            writes.rmdir(HOME_INO, "Z").unwrap();
        },
    );
    r.settle();
    assert_eq!(r.hub.lock().bytes_of("Z/plan.txt").unwrap(), b"plan");
    assert_eq!(r.read("Z/plan.txt"), b"plan");
    assert!(!r
        .hub
        .lock()
        .requests
        .iter()
        .any(|q| q.starts_with("DELETE ")));
}

#[test]
fn paged_listing_exposes_all_folders_and_accepts_uploads_before_downloads_finish() {
    // Exercise both a fresh mount and an explicit resync of an existing mirror.
    for initial_sync in [false, true] {
        let r = Rig::with_initial_sync(&[], initial_sync);
        {
            let mut hub = r.hub.lock();
            for n in 0..2001 {
                hub.put(&format!("Architecture/Photos/{n:04}.jpg"), b"photo");
            }
            hub.put("Timesheets/2026/October.xlsx", b"timesheet");
            hub.put("Žehňa/Kitchen/plan.txt", b"plan");
            // Stop this pass after two transfers, with most files still pending.
            hub.corrupt_downloads
                .insert("Architecture/Photos/0001.jpg".into());
        }
        let mirror = r.mirror.clone();
        let check_mirror = r.mirror.clone();
        let home = r.dir.path().join("Home");
        while_stalled_work(
            &r,
            "GET ",
            "/objects/Architecture/Photos/0000.jpg",
            move || mirror.cycle(true),
            move |writes, tree| {
                for path in ["Architecture/Photos", "Timesheets/2026", "Žehňa/Kitchen"] {
                    assert!(home.join(path).is_dir(), "missing real folder {path}");
                    assert!(
                        tree.read().home_ino_for_key(path).is_some(),
                        "missing view folder {path}"
                    );
                    assert!(check_mirror.store.has_dir(path));
                }
                assert!(check_mirror.store.entries("").is_empty());
                assert!(check_mirror.store.bases("").is_empty());
                assert!(!home.join("Timesheets/2026/October.xlsx").exists());
                let parent = tree.read().home_ino_for_key("Timesheets/2026").unwrap();
                save(&writes, parent, "new.txt", b"working during initial sync");
                assert_eq!(
                    std::fs::read(home.join("Timesheets/2026/new.txt")).unwrap(),
                    b"working during initial sync"
                );
            },
        );
        let hub = r.hub.lock();
        assert_eq!(
            hub.bytes_of("Timesheets/2026/new.txt").unwrap(),
            b"working during initial sync"
        );
        let upload = hub
            .requests
            .iter()
            .position(|q| q.starts_with("PUT ") && q.ends_with("/new.txt"))
            .unwrap();
        let second_download = hub
            .requests
            .iter()
            .position(|q| q.starts_with("GET ") && q.ends_with("/0001.jpg"))
            .unwrap();
        assert!(upload < second_download);
        assert!(
            hub.mkdirs.is_empty(),
            "remote folders must not echo back as local mkdirs"
        );
        assert!(!hub.requests.iter().any(|q| q.starts_with("DELETE ")));
        drop(hub);
        assert!(r
            .mirror
            .store
            .entry("Timesheets/2026/October.xlsx")
            .is_none());
        assert!(r
            .mirror
            .store
            .base("Timesheets/2026/October.xlsx")
            .is_none());
        r.mirror.scan_local();
        assert!(
            !r.mirror.store.jobs().iter().any(|j| matches!(
                j.kind,
                canvas_fuse::mirror::store::JobKind::Delete { .. }
                    | canvas_fuse::mirror::store::JobKind::Rmdir { .. }
                    | canvas_fuse::mirror::store::JobKind::Mkdir { .. }
            )),
            "unfinished downloads must not become local deletions or new folders"
        );
    }
}

#[test]
fn writes_hit_disk_before_flush_and_support_sparse_partial_edits() {
    let r = Rig::new(&[]);
    let ino = r.writes.create(HOME_INO, "copy.bin").unwrap().ino;
    assert!(r.dir.path().join("Home/copy.bin").is_file());
    r.writes.write(ino, 0, b"first").unwrap();
    assert_eq!(r.read("copy.bin"), b"first");
    r.writes.write(ino, 8, b"last").unwrap();
    assert_eq!(r.read("copy.bin"), b"first\0\0\0last");
    r.writes.truncate(ino, 9).unwrap();
    assert_eq!(r.read("copy.bin"), b"first\0\0\0l");
    assert_eq!(
        r.writes.read_buffer(ino, 3, 6).unwrap().unwrap(),
        b"st\0\0\0l"
    );
    r.writes.flush_final(ino).unwrap();
    r.writes.release(ino);
    r.settle();
    assert_eq!(r.hub.lock().bytes_of("copy.bin").unwrap(), b"first\0\0\0l");
}

#[test]
fn remote_refresh_lock_does_not_block_local_create_flush_mkdir_rename_or_unlink() {
    let r = Rig::new(&[]);
    let remote_lock = r.writes.sync_handle();
    let held = remote_lock.lock();
    let writes = r.writes.clone();
    let (tx, rx) = mpsc::channel();
    let io = std::thread::spawn(move || {
        let dir = writes.mkdir(HOME_INO, "new").unwrap();
        save(&writes, dir, "file", b"disk first");
        writes.rename(dir, "file", dir, "renamed").unwrap();
        writes.unlink(dir, "renamed").unwrap();
        writes.rmdir(HOME_INO, "new").unwrap();
        let _ = tx.send(());
    });
    let finished = rx.recv_timeout(Duration::from_secs(2)).is_ok();
    drop(held);
    io.join().unwrap();
    assert!(finished, "Home operations waited for virtual tree refresh");
}

#[test]
fn newer_save_during_upload_stays_local_and_is_uploaded_next() {
    let r = Rig::new(&[("file.txt", b"base")]);
    let ino = r.tree.read().lookup(HOME_INO, "file.txt").unwrap().ino;
    r.writes.open_existing(ino, true).unwrap();
    r.writes.write(ino, 0, b"first upload").unwrap();
    r.writes.flush_final(ino).unwrap();
    r.writes.release(ino);
    while_stalled(&r, "PUT ", "/objects/file.txt", move |writes, _| {
        writes.open_existing(ino, true).unwrap();
        writes.write(ino, 0, b"newer local save").unwrap();
        writes.flush_final(ino).unwrap();
        writes.release(ino);
    });
    assert_eq!(r.read("file.txt"), b"newer local save");
    r.settle();
    assert_eq!(
        r.hub.lock().bytes_of("file.txt").unwrap(),
        b"newer local save"
    );
    assert!(r.mirror.conflicts().is_empty());
}

#[test]
fn folder_rename_during_upload_is_local_and_rekeys_the_completion() {
    let r = Rig::new(&[("old/file.txt", b"base")]);
    r.mirror.commit_write("old/file.txt", b"edited").unwrap();
    while_stalled(&r, "PUT ", "/objects/old/file.txt", |writes, _| {
        writes.rename(HOME_INO, "old", HOME_INO, "new").unwrap();
    });
    assert_eq!(r.read("new/file.txt"), b"edited");
    r.settle();
    assert_eq!(r.hub.lock().bytes_of("new/file.txt").unwrap(), b"edited");
    assert!(r.hub.lock().bytes_of("old/file.txt").is_none());
    assert!(r.mirror.conflicts().is_empty());
}

#[test]
fn local_save_during_download_is_never_replaced_by_the_delayed_response() {
    let r = Rig::new(&[("file.txt", b"base")]);
    r.hub.lock().put("file.txt", b"remote edit");
    let ino = r.tree.read().lookup(HOME_INO, "file.txt").unwrap().ino;
    let local_path = r.dir.path().join("Home/file.txt");
    while_stalled(&r, "GET ", "/objects/file.txt", move |writes, _| {
        writes.open_existing(ino, true).unwrap();
        writes.write(ino, 0, b"local edit").unwrap();
        writes.flush_final(ino).unwrap();
        writes.release(ino);
        assert_eq!(std::fs::read(&local_path).unwrap(), b"local edit");
        save(&writes, HOME_INO, "other.txt", b"also local");
    });
    r.settle();
    let st = r.hub.lock();
    assert!(st.objects.values().any(|o| o.bytes == b"local edit"));
    assert!(st.objects.values().any(|o| o.bytes == b"remote edit"));
    assert_eq!(st.bytes_of("other.txt").unwrap(), b"also local");
}

#[test]
fn open_file_handle_follows_a_directory_rename() {
    let r = Rig::new(&[("old/file.txt", b"base")]);
    let dir = r.tree.read().lookup(HOME_INO, "old").unwrap().ino;
    let ino = r.tree.read().lookup(dir, "file.txt").unwrap().ino;
    r.writes.open_existing(ino, false).unwrap();
    r.writes.rename(HOME_INO, "old", HOME_INO, "new").unwrap();
    r.writes.write(ino, 4, b" plus edit").unwrap();
    assert_eq!(r.read("new/file.txt"), b"base plus edit");
    r.writes.flush_final(ino).unwrap();
    r.writes.release(ino);
    r.settle();
    assert_eq!(
        r.hub.lock().bytes_of("new/file.txt").unwrap(),
        b"base plus edit"
    );
}

#[test]
fn folder_rename_during_download_does_not_resurrect_the_old_path() {
    let r = Rig::new(&[("old/file.txt", b"base")]);
    r.hub.lock().put("old/file.txt", b"remote edit");
    while_stalled(&r, "GET ", "/objects/old/file.txt", |writes, _| {
        writes.rename(HOME_INO, "old", HOME_INO, "new").unwrap();
    });
    r.settle();
    assert!(!r.dir.path().join("Home/old").exists());
    assert_eq!(r.read("new/file.txt"), b"remote edit");
    assert_eq!(
        r.hub.lock().bytes_of("new/file.txt").unwrap(),
        b"remote edit"
    );
    assert!(!r.dir.path().join("Home/new/.file.txt.canvas-part").exists());
}

#[test]
fn deleting_a_new_file_while_its_upload_runs_does_not_leave_a_remote_ghost() {
    let r = Rig::new(&[]);
    save(&r.writes, HOME_INO, "new.txt", b"transient");
    while_stalled(&r, "PUT ", "/objects/new.txt", |writes, _| {
        writes.unlink(HOME_INO, "new.txt").unwrap();
    });
    r.settle();
    assert!(!r.dir.path().join("Home/new.txt").exists());
    assert!(r.hub.lock().bytes_of("new.txt").is_none());
    assert_eq!(r.mirror.status().pending, 0);
}

#[test]
fn renaming_a_new_file_during_its_upload_preserves_one_remote_copy() {
    let r = Rig::new(&[]);
    save(&r.writes, HOME_INO, "new.txt", b"created");
    while_stalled(&r, "PUT ", "/objects/new.txt", |writes, _| {
        writes
            .rename(HOME_INO, "new.txt", HOME_INO, "renamed.txt")
            .unwrap();
    });
    r.settle();
    assert_eq!(r.read("renamed.txt"), b"created");
    assert_eq!(r.hub.lock().bytes_of("renamed.txt").unwrap(), b"created");
    assert!(r.hub.lock().bytes_of("new.txt").is_none());
    assert_eq!(r.mirror.status().pending, 0);
}

#[test]
fn editor_temp_rename_before_close_replaces_the_target_on_disk() {
    let r = Rig::new(&[("document.txt", b"old")]);
    let ino = r.writes.create(HOME_INO, "temporary.txt").unwrap().ino;
    r.writes.write(ino, 0, b"saved").unwrap();
    r.writes
        .rename(HOME_INO, "temporary.txt", HOME_INO, "document.txt")
        .unwrap();
    assert_eq!(r.read("document.txt"), b"saved");
    assert!(!r.dir.path().join("Home/temporary.txt").exists());
    r.writes.write(ino, 5, b" again").unwrap();
    r.writes.flush_final(ino).unwrap();
    r.writes.release(ino);
    r.settle();
    assert_eq!(
        r.hub.lock().bytes_of("document.txt").unwrap(),
        b"saved again"
    );
    assert!(r.hub.lock().bytes_of("temporary.txt").is_none());
}

#[test]
fn unlink_of_an_open_new_file_keeps_the_handle_but_never_recreates_the_name() {
    let r = Rig::new(&[]);
    let ino = r.writes.create(HOME_INO, "temporary.txt").unwrap().ino;
    r.writes.write(ino, 0, b"old handle").unwrap();
    r.writes.unlink(HOME_INO, "temporary.txt").unwrap();
    assert!(!r.dir.path().join("Home/temporary.txt").exists());
    save(&r.writes, HOME_INO, "temporary.txt", b"recreated");
    r.writes.write(ino, 0, b"more").unwrap();
    assert_eq!(r.writes.read_buffer(ino, 0, 4).unwrap().unwrap(), b"more");
    r.writes.flush_final(ino).unwrap();
    r.writes.release(ino);
    assert_eq!(r.read("temporary.txt"), b"recreated");
    r.settle();
    assert_eq!(
        r.hub.lock().bytes_of("temporary.txt").unwrap(),
        b"recreated"
    );
}

#[test]
fn more_local_renames_and_editor_saves_work_while_a_folder_move_is_pending() {
    let r = Rig::new(&[("old/file.txt", b"base")]);
    r.writes
        .rename(HOME_INO, "old", HOME_INO, "middle")
        .unwrap();
    while_stalled(&r, "POST ", "/objects/rename", |writes, tree| {
        let middle = tree.read().lookup(HOME_INO, "middle").unwrap().ino;
        let temp = writes.create(middle, "temporary.txt").unwrap().ino;
        writes.write(temp, 0, b"edited while moving").unwrap();
        writes
            .rename(middle, "temporary.txt", middle, "file.txt")
            .unwrap();
        writes.flush_final(temp).unwrap();
        writes.release(temp);
        writes
            .rename(HOME_INO, "middle", HOME_INO, "final")
            .unwrap();
    });
    r.settle();
    assert_eq!(r.read("final/file.txt"), b"edited while moving");
    assert_eq!(
        r.hub.lock().bytes_of("final/file.txt").unwrap(),
        b"edited while moving"
    );
    assert!(r.hub.lock().bytes_of("old/file.txt").is_none());
    assert!(r.hub.lock().bytes_of("middle/file.txt").is_none());
    assert_eq!(r.mirror.status().pending, 0);
}

#[test]
fn file_overwrite_then_parent_rename_preserves_remote_operation_order() {
    let r = Rig::new(&[("old/source.txt", b"source"), ("old/target.txt", b"target")]);
    let old = r.tree.read().lookup(HOME_INO, "old").unwrap().ino;
    r.writes
        .rename(old, "source.txt", old, "target.txt")
        .unwrap();
    r.writes.rename(HOME_INO, "old", HOME_INO, "new").unwrap();
    r.settle();
    assert_eq!(r.read("new/target.txt"), b"source");
    assert_eq!(r.hub.lock().bytes_of("new/target.txt").unwrap(), b"source");
    assert!(r.hub.lock().bytes_of("old/target.txt").is_none());
    assert_eq!(r.mirror.status().pending, 0);
}

#[test]
fn stalled_virtual_tree_refresh_does_not_block_mirrored_home() {
    let r = Rig::new(&[]);
    let worker = canvas_fuse::worker::Worker {
        api: Arc::new(ApiClient::new(&r.hub.url, "test").unwrap()),
        tree: r.tree.clone(),
        names: Arc::new(NameStore::open(&r.dir.path().join("worker-names.redb")).unwrap()),
        notifier: None,
        ensure_subscribed: None,
        context_filter: None,
        context_workspace_id: None,
        refresh_lock: Some(r.writes.sync_handle()),
        nudger: None,
        mirror_invalidations: None,
        mirror: Some(r.mirror.clone()),
    };
    while_stalled_work(
        &r,
        "GET ",
        "/trees",
        move || worker.refresh_all(),
        |writes, _| {
            let dir = writes.mkdir(HOME_INO, "local").unwrap();
            save(&writes, dir, "copy.bin", b"bytes on disk");
            writes.rename(dir, "copy.bin", dir, "renamed.bin").unwrap();
            writes.unlink(dir, "renamed.bin").unwrap();
            writes.rmdir(HOME_INO, "local").unwrap();
        },
    );
}

#[test]
fn same_path_rename_is_a_noop_even_with_an_open_handle() {
    let r = Rig::new(&[("file.txt", b"keep"), ("folder/child.txt", b"child")]);
    let ino = r.tree.read().lookup(HOME_INO, "file.txt").unwrap().ino;
    r.writes.open_existing(ino, false).unwrap();
    r.writes
        .rename(HOME_INO, "file.txt", HOME_INO, "file.txt")
        .unwrap();
    r.writes
        .rename(HOME_INO, "folder", HOME_INO, "folder")
        .unwrap();
    r.writes.flush_final(ino).unwrap();
    r.writes.release(ino);
    assert_eq!(r.read("file.txt"), b"keep");
    assert_eq!(r.read("folder/child.txt"), b"child");
    assert_eq!(r.mirror.status().pending, 0);
}

#[test]
fn an_unclosed_copy_survives_daemon_restart_and_is_discovered_for_upload() {
    let Rig {
        writes,
        tree,
        mirror,
        hub,
        dir,
    } = Rig::new(&[]);
    let ino = writes.create(HOME_INO, "unfinished.bin").unwrap().ino;
    let bytes = vec![37u8; 2 * 1024 * 1024];
    for (i, chunk) in bytes.chunks(65536).enumerate() {
        writes.write(ino, (i * 65536) as i64, chunk).unwrap();
    }
    assert_eq!(
        std::fs::read(dir.path().join("Home/unfinished.bin")).unwrap(),
        bytes
    );
    // No FUSE flush/release: the write facade disappears as on daemon exit.
    drop(writes);
    drop(tree);
    drop(mirror);
    let reopened = Mirror::open(MirrorConfig {
        data_dir: dir.path().join("state"),
        home_dir: dir.path().join("Home"),
        server: hub.url.clone(),
        token: "test".into(),
        tls: None,
        workspace_id: "ws1".into(),
        backend: "workspace:home".into(),
        opts: MirrorOptions::default(),
        device: DeviceIdentity {
            id: "local-device".into(),
            name: "local".into(),
        },
        mountpoint: dir.path().join("Home"),
        status_path: None,
    })
    .unwrap();
    reopened.cycle(true);
    assert_eq!(hub.lock().bytes_of("unfinished.bin").unwrap(), bytes);
}

#[test]
fn folder_move_during_conflict_upload_preserves_both_versions_and_copy_location() {
    let r = Rig::new(&[("old/file.txt", b"base")]);
    r.mirror.commit_write("old/file.txt", b"our edit").unwrap();
    r.hub.lock().put("old/file.txt", b"their edit");
    let engine = r.mirror.clone();
    while_request_stalled(
        &r,
        |q| q.starts_with("PUT ") && q.contains("conflict"),
        move || engine.cycle(false),
        |writes, _| {
            writes.rename(HOME_INO, "old", HOME_INO, "new").unwrap();
        },
    );
    r.settle();
    let state = r.hub.lock();
    assert!(
        state.objects.keys().all(|key| key.starts_with("new/")),
        "{:?}",
        state.objects.keys()
    );
    assert!(state.objects.values().any(|o| o.bytes == b"our edit"));
    assert!(state.objects.values().any(|o| o.bytes == b"their edit"));
    for conflict in r.mirror.conflicts() {
        assert!(conflict.key.starts_with("new/"));
        assert!(state
            .objects
            .contains_key(conflict.copy_key.as_ref().unwrap()));
    }
    assert_eq!(r.mirror.status().pending, 0);
}
