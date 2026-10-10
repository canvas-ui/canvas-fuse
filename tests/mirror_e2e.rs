//! End-to-end mirror engine tests against an in-memory hub (`fake_hub`):
//! no FUSE mount, no real server. Each test owns a data dir, a real Home
//! folder and a hub.

mod fake_hub;

use canvas_fuse::mirror::store::EntryState;
use canvas_fuse::mirror::sync::{Mirror, MirrorConfig, SyncState};
use canvas_fuse::mirror::{ConflictMode, DeviceIdentity, MirrorOptions};
use fake_hub::{sha_hex, FakeHub};
use std::sync::Arc;

struct Rig {
    hub: FakeHub,
    mirror: Arc<Mirror>,
    _dir: tempfile::TempDir,
}

impl Rig {
    /// The real file, read the way a user would (no mirror in between).
    fn home_read(&self, key: &str) -> Option<Vec<u8>> {
        std::fs::read(self._dir.path().join("Home").join(key)).ok()
    }
}

fn rig_with(url: &str, opts: MirrorOptions) -> (Arc<Mirror>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mirror = open_mirror(&dir, url, opts);
    (mirror, dir)
}

fn open_mirror(dir: &tempfile::TempDir, url: &str, opts: MirrorOptions) -> Arc<Mirror> {
    try_open_mirror(dir, url, opts).unwrap()
}

fn try_open_mirror(
    dir: &tempfile::TempDir,
    url: &str,
    opts: MirrorOptions,
) -> anyhow::Result<Arc<Mirror>> {
    Mirror::open(MirrorConfig {
        tls: None,
        data_dir: dir.path().join("data"),
        home_dir: dir.path().join("Home"),
        server: url.to_string(),
        token: "token".into(),
        workspace_id: "ws1".into(),
        backend: "workspace:home".into(),
        opts,
        device: DeviceIdentity {
            id: "dev-a".into(),
            name: "laptop".into(),
        },
        mountpoint: dir.path().join("Home"),
        status_path: Some(dir.path().join("status.json")),
    })
}

fn rig(opts: MirrorOptions) -> Rig {
    let hub = FakeHub::start();
    let (mirror, dir) = rig_with(&hub.url, opts);
    Rig {
        hub,
        mirror,
        _dir: dir,
    }
}

fn opts() -> MirrorOptions {
    MirrorOptions {
        poll_secs: 30,
        ..Default::default()
    }
}

fn settle(mirror: &Mirror) {
    for _ in 0..4 {
        mirror.cycle(false);
    }
}

#[test]
fn daily_empty_binary_and_unicode_files_roundtrip_between_two_mirrors() {
    let hub = FakeHub::start();
    let (a, _a_dir) = rig_with(&hub.url, opts());
    let (b, b_dir) = rig_with(&hub.url, opts());
    a.cycle(true);
    b.cycle(true);
    let cases: &[(&str, &[u8])] = &[
        ("empty.txt", b""),
        (
            "Architektúra/Žehňa/Fotky/photo #1 %.jpg",
            &[0, 255, 1, 128, 10],
        ),
        ("Documents/notes.txt", b"first draft\n"),
    ];
    for (key, bytes) in cases {
        a.commit_write(key, bytes).unwrap();
    }
    settle(&a);
    settle(&b);
    for (key, bytes) in cases {
        assert_eq!(
            std::fs::read(b_dir.path().join("Home").join(key)).unwrap(),
            *bytes
        );
        assert_eq!(b.entry(key).unwrap().state, EntryState::Clean);
    }
    b.commit_write("Documents/notes.txt", b"reviewed\n")
        .unwrap();
    settle(&b);
    settle(&a);
    assert_eq!(
        a.local.read_all("Documents/notes.txt").unwrap(),
        b"reviewed\n"
    );
}

#[test]
fn two_offline_writers_preserve_both_versions_and_converge() {
    let hub = FakeHub::start();
    hub.lock().put("shared.txt", b"base");
    let (a, _a_dir) = rig_with(&hub.url, opts());
    let (b, _b_dir) = rig_with(&hub.url, opts());
    a.cycle(true);
    b.cycle(true);
    a.commit_write("shared.txt", b"writer a").unwrap();
    b.commit_write("shared.txt", b"writer b").unwrap();
    settle(&a);
    settle(&b);
    settle(&a);
    let objects = hub.lock().objects.clone();
    assert_eq!(objects.len(), 2);
    assert!(objects.values().any(|o| o.bytes == b"writer a"));
    assert!(objects.values().any(|o| o.bytes == b"writer b"));
    for (key, o) in objects {
        assert_eq!(a.local.read_all(&key).unwrap(), o.bytes);
        assert_eq!(b.local.read_all(&key).unwrap(), o.bytes);
    }
}

#[test]
fn a_new_file_deleted_before_upload_never_reaches_the_hub() {
    let r = rig(opts());
    r.mirror.cycle(true);
    r.mirror.commit_write("scratch.txt", b"temporary").unwrap();
    r.mirror.delete_local("scratch.txt").unwrap();
    settle(&r.mirror);
    assert!(r.hub.lock().entries("put").is_empty());
    assert_eq!(r.mirror.status().pending, 0);
}

#[test]
fn a_local_edit_survives_a_remote_delete() {
    let r = rig(opts());
    r.hub.lock().put("work.txt", b"base");
    r.mirror.cycle(true);
    r.mirror.commit_write("work.txt", b"unsynced edit").unwrap();
    r.hub.lock().delete("work.txt");
    settle(&r.mirror);
    assert_eq!(r.hub.lock().bytes_of("work.txt").unwrap(), b"unsynced edit");
    assert_eq!(r.home_read("work.txt").unwrap(), b"unsynced edit");
}

#[test]
fn keep_delete_policy_never_deletes_the_remote_copy() {
    let r = rig(MirrorOptions {
        deletes: canvas_fuse::mirror::DeleteMode::Keep,
        ..opts()
    });
    r.hub.lock().put("keep.txt", b"remote copy");
    r.mirror.cycle(true);
    r.mirror.delete_local("keep.txt").unwrap();
    settle(&r.mirror);
    r.mirror.cycle(true);
    assert_eq!(r.hub.lock().bytes_of("keep.txt").unwrap(), b"remote copy");
    assert!(r.hub.lock().entries("delete").is_empty());
}

#[test]
fn corrupted_download_is_retried_without_waiting_for_a_full_scan() {
    let r = rig(opts());
    r.mirror.cycle(true);
    r.hub.lock().put("photo.jpg", b"original photo");
    r.hub.lock().corrupt_downloads.insert("photo.jpg".into());
    r.mirror.cycle(false);
    settle(&r.mirror);
    assert_eq!(r.home_read("photo.jpg").unwrap(), b"original photo");
    assert_eq!(
        r.mirror.entry("photo.jpg").unwrap().sha256,
        sha_hex(b"original photo")
    );
}

#[test]
fn a_corrupt_download_without_etag_never_replaces_a_good_local_copy() {
    let r = rig(opts());
    r.hub.lock().put("photo.jpg", b"good local photo");
    r.mirror.cycle(true);
    let cursor = r.mirror.status().cursor;
    r.hub.lock().put("photo.jpg", b"updated remote photo");
    r.hub.lock().corrupt_downloads.insert("photo.jpg".into());
    r.hub
        .lock()
        .downloads_without_etag
        .insert("photo.jpg".into());
    r.mirror.cycle(false);
    assert_eq!(r.home_read("photo.jpg").unwrap(), b"good local photo");
    assert_eq!(r.mirror.status().cursor, cursor);
    settle(&r.mirror);
    assert_eq!(r.home_read("photo.jpg").unwrap(), b"updated remote photo");
}

#[test]
fn a_stale_partial_download_larger_than_the_new_remote_file_recovers() {
    let r = rig(opts());
    r.mirror.cycle(true);
    std::fs::write(
        r._dir.path().join("Home/.small.txt.canvas-part"),
        b"old much larger partial content",
    )
    .unwrap();
    r.hub.lock().put("small.txt", b"new");
    settle(&r.mirror);
    assert_eq!(r.home_read("small.txt").unwrap(), b"new");
    assert!(!r._dir.path().join("Home/.small.txt.canvas-part").exists());
}

#[test]
fn matching_partial_download_resumes_and_never_uploads_the_staging_file() {
    let r = rig(opts());
    r.mirror.cycle(true);
    std::fs::write(r._dir.path().join("Home/.large.bin.canvas-part"), b"first-").unwrap();
    r.hub.lock().put("large.bin", b"first-second");
    settle(&r.mirror);
    assert_eq!(r.home_read("large.bin").unwrap(), b"first-second");
    assert!(r
        .hub
        .lock()
        .entries("put")
        .iter()
        .all(|e| !e.key.ends_with("canvas-part")));
}

#[test]
fn a_failed_local_trash_move_keeps_the_ledger_and_retries() {
    let r = rig(opts());
    r.hub.lock().put("Docs/a.txt", b"recoverable");
    r.mirror.cycle(true);
    let blocker = r._dir.path().join("data/trash/Docs");
    std::fs::write(&blocker, b"not a directory").unwrap();
    r.hub.lock().delete("Docs/a.txt");
    r.mirror.cycle(false);
    assert!(
        r.mirror.store.entry("Docs/a.txt").is_some(),
        "failed trash move must not forget local bytes"
    );
    assert_eq!(r.home_read("Docs/a.txt").unwrap(), b"recoverable");
    std::fs::remove_file(blocker).unwrap();
    settle(&r.mirror);
    assert!(r.home_read("Docs/a.txt").is_none());
    assert_eq!(r.mirror.trash_list().len(), 1);
    assert!(
        r.hub.lock().bytes_of("Docs/a.txt").is_none(),
        "failed trash must not resurrect remote data"
    );
}

#[test]
fn an_editor_atomic_save_over_an_existing_file_updates_the_remote() {
    let r = rig(opts());
    r.hub.lock().put("document.txt", b"old");
    r.mirror.cycle(true);
    r.mirror
        .commit_write("save.tmp", b"new editor content")
        .unwrap();
    r.mirror.rename_local("save.tmp", "document.txt").unwrap();
    settle(&r.mirror);
    assert_eq!(
        r.hub.lock().bytes_of("document.txt").unwrap(),
        b"new editor content"
    );
    assert!(r.hub.lock().bytes_of("save.tmp").is_none());
    assert_eq!(r.home_read("document.txt").unwrap(), b"new editor content");
    assert_eq!(r.mirror.status().pending, 0);
}

#[test]
fn chained_local_file_renames_before_sync_keep_only_the_final_name() {
    let r = rig(opts());
    r.hub.lock().put("a.txt", b"same bytes");
    r.mirror.cycle(true);
    r.mirror.rename_local("a.txt", "b.txt").unwrap();
    r.mirror.rename_local("b.txt", "c.txt").unwrap();
    settle(&r.mirror);
    let objects = r.hub.lock().objects.clone();
    assert_eq!(objects.keys().cloned().collect::<Vec<_>>(), vec!["c.txt"]);
    assert_eq!(objects["c.txt"].bytes, b"same bytes");
}

#[test]
fn local_saves_during_a_conflict_upload_are_not_overwritten_by_the_pull() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let r = rig(opts());
    r.hub.lock().put("doc.txt", b"base");
    r.mirror.cycle(true);
    r.mirror
        .commit_write("doc.txt", b"first local edit")
        .unwrap();
    r.hub.lock().put("doc.txt", b"remote edit");
    let weak = Arc::downgrade(&r.mirror);
    let once = AtomicBool::new(false);
    r.hub.lock().on_request = Some(Arc::new(move |request| {
        if request.starts_with("PUT ")
            && request.contains("conflict")
            && !once.swap(true, Ordering::Relaxed)
        {
            weak.upgrade()
                .unwrap()
                .commit_write("doc.txt", b"latest local edit")
                .unwrap();
        }
    }));
    settle(&r.mirror);
    let st = r.hub.lock();
    for bytes in [
        b"first local edit".as_slice(),
        b"latest local edit",
        b"remote edit",
    ] {
        assert!(
            st.objects.values().any(|o| o.bytes == bytes)
                || r.home_read("doc.txt").as_deref() == Some(bytes),
            "lost {}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn replacing_a_synced_file_with_another_synced_file_preserves_the_source_bytes() {
    let r = rig(opts());
    r.hub.lock().put("source.txt", b"replacement");
    r.hub.lock().put("target.txt", b"old target");
    r.mirror.cycle(true);
    r.mirror.rename_local("source.txt", "target.txt").unwrap();
    settle(&r.mirror);
    assert_eq!(r.hub.lock().bytes_of("target.txt").unwrap(), b"replacement");
    assert!(r.hub.lock().bytes_of("source.txt").is_none());
    assert_eq!(r.home_read("target.txt").unwrap(), b"replacement");
}

#[test]
fn a_recreated_file_during_its_delete_request_is_uploaded_as_new() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let r = rig(opts());
    r.hub.lock().put("work.txt", b"old");
    r.mirror.cycle(true);
    r.mirror.delete_local("work.txt").unwrap();
    let weak = Arc::downgrade(&r.mirror);
    let once = AtomicBool::new(false);
    r.hub.lock().on_request = Some(Arc::new(move |request| {
        if request.starts_with("DELETE ") && !once.swap(true, Ordering::Relaxed) {
            weak.upgrade()
                .unwrap()
                .commit_write("work.txt", b"recreated")
                .unwrap();
        }
    }));
    settle(&r.mirror);
    assert_eq!(r.home_read("work.txt").unwrap(), b"recreated");
    assert_eq!(r.hub.lock().bytes_of("work.txt").unwrap(), b"recreated");
}

#[test]
fn denied_listing_never_trashes_previously_synced_files() {
    let r = rig(opts());
    r.hub.lock().put("important.txt", b"keep me");
    r.mirror.cycle(true);
    let cursor = r.mirror.status().cursor;
    r.hub.lock().failures.push((
        "GET /rest/v2/workspaces/ws1/backends/file/workspace%3Ahome/objects".into(),
        403,
    ));
    r.mirror.cycle(true);
    assert_eq!(r.home_read("important.txt").unwrap(), b"keep me");
    assert!(r.mirror.trash_list().is_empty());
    assert_eq!(r.mirror.status().cursor, cursor);
}

#[test]
fn revoked_credentials_pause_sync_without_losing_pending_writes() {
    let r = rig(opts());
    r.mirror.cycle(true);
    r.mirror
        .commit_write("private.txt", b"offline work")
        .unwrap();
    r.hub.lock().failures.push((
        "PUT /rest/v2/workspaces/ws1/backends/file/workspace%3Ahome/objects/private.txt".into(),
        401,
    ));
    r.mirror.cycle(false);
    assert_eq!(r.mirror.status().state, SyncState::Paused);
    assert_eq!(
        r.mirror.entry("private.txt").unwrap().state,
        EntryState::Dirty
    );
    assert!(r.mirror.status().pending > 0);
    r.mirror.cycle(false);
    assert!(r.hub.lock().bytes_of("private.txt").is_none());
    r.mirror.cycle(true);
    assert_eq!(
        r.hub.lock().bytes_of("private.txt").unwrap(),
        b"offline work"
    );
}

#[test]
fn offline_equal_length_edit_with_preserved_mtime_is_detected_after_restart() {
    let hub = FakeHub::start();
    hub.lock().put("work.txt", b"before");
    let (m, dir) = rig_with(&hub.url, opts());
    m.cycle(true);
    m.scan_local(); // remember the previous scan's stamp
    let path = dir.path().join("Home/work.txt");
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    drop(m);
    std::fs::write(&path, b"edited").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let restarted = open_mirror(&dir, &hub.url, opts());
    restarted.cycle(true);
    assert_eq!(hub.lock().bytes_of("work.txt").unwrap(), b"edited");
}

#[test]
fn unreadable_local_subtree_does_not_queue_remote_deletions() {
    use std::os::unix::fs::PermissionsExt;
    let r = rig(opts());
    r.hub.lock().put("private/a.txt", b"valuable");
    r.mirror.cycle(true);
    let dir = r._dir.path().join("Home/private");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o0)).unwrap();
    let scan = r.mirror.scan_local();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(scan.removed, 0, "permission failure is not a deletion");
    assert!(r
        .mirror
        .store
        .jobs()
        .iter()
        .all(|j| !matches!(j.kind, canvas_fuse::mirror::store::JobKind::Delete { .. })));
}

#[test]
fn initial_listing_pulls_every_file_into_the_folder() {
    let r = rig(opts());
    let sha_a = r.hub.lock().put("Docs/a.md", b"hello a");
    let sha_b = r.hub.lock().put("b.txt", b"hello b");
    r.mirror.cycle(true);

    let a = r.mirror.entry("Docs/a.md").expect("Docs/a.md listed");
    assert_eq!(a.sha256, sha_a);
    assert_eq!(a.size, 7);
    assert_eq!(a.state, EntryState::Clean);
    assert_eq!(r.mirror.store.base("Docs/a.md").unwrap().sha256, sha_a);
    let b = r.mirror.entry("b.txt").expect("b.txt listed");
    assert_eq!(b.sha256, sha_b);

    // Every file is a real file in the folder, nothing waits for a read.
    assert_eq!(r.home_read("Docs/a.md").unwrap(), b"hello a");
    assert_eq!(r.home_read("b.txt").unwrap(), b"hello b");
    assert!(r.mirror.store.has_dir("Docs"));

    let st = r.mirror.status();
    assert_eq!(st.state, SyncState::Idle);
    assert_eq!(st.cursor, Some(2));
    assert_eq!(st.pending, 0);
    assert_eq!(st.entries, 2);
    // The hub got a status report with our client type and path.
    let reports = r.hub.lock().reports.clone();
    assert!(!reports.is_empty());
    assert_eq!(reports[0]["client"], "fuse");
    assert_eq!(reports[0]["cursor"], 2);
    assert_eq!(reports[0]["direction"], "bi");
    // And the status file is there for `canvas-fuse status`.
    let raw = std::fs::read_to_string(r._dir.path().join("status.json")).unwrap();
    assert!(raw.contains("\"workspaceId\": \"ws1\""));
}

#[test]
fn push_new_then_edit_with_if_match() {
    let r = rig(opts());
    r.mirror.cycle(true);

    let e = r.mirror.commit_write("notes/todo.md", b"first").unwrap();
    assert_eq!(e.state, EntryState::Dirty);
    assert_eq!(r.mirror.status().pending, 1);
    r.mirror.cycle(false);

    assert_eq!(r.hub.lock().bytes_of("notes/todo.md").unwrap(), b"first");
    let puts = r.hub.lock().entries("put");
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].if_none_match.as_deref(), Some("*"));
    assert_eq!(puts[0].origin.as_deref(), Some("dev-a"));
    assert_eq!(
        r.mirror.entry("notes/todo.md").unwrap().state,
        EntryState::Clean
    );
    let base = r.mirror.store.base("notes/todo.md").unwrap();
    assert_eq!(base.sha256, sha_hex(b"first"));
    assert_eq!(base.remote_seq, 1);
    assert_eq!(r.mirror.status().pending, 0);

    // Our own echo on the feed changes nothing.
    r.mirror.cycle(false);
    assert_eq!(r.mirror.status().cursor, Some(1));
    assert_eq!(
        r.mirror.entry("notes/todo.md").unwrap().state,
        EntryState::Clean
    );

    // Edit → PUT with If-Match = the base.
    r.mirror.commit_write("notes/todo.md", b"second").unwrap();
    r.mirror.cycle(false);
    let puts = r.hub.lock().entries("put");
    assert_eq!(puts.len(), 2);
    assert_eq!(
        puts[1].if_match.as_deref(),
        Some(sha_hex(b"first").as_str())
    );
    assert_eq!(r.hub.lock().bytes_of("notes/todo.md").unwrap(), b"second");
    assert_eq!(
        r.mirror.store.base("notes/todo.md").unwrap().sha256,
        sha_hex(b"second")
    );
}

#[test]
fn precondition_failure_goes_to_the_inbox_and_adopts_the_hub_version() {
    let r = rig(MirrorOptions {
        conflicts: ConflictMode::Prompt,
        ..opts()
    });
    let base_sha = r.hub.lock().put("shared.txt", b"base");
    r.mirror.cycle(true);

    // Both sides edit. The hub's write is silent (no log entry) so our push
    // meets the 412 rather than the feed catching it first.
    r.mirror.commit_write("shared.txt", b"ours").unwrap();
    let hub_sha = r.hub.lock().put_silent("shared.txt", b"theirs");
    r.mirror.cycle(false);

    let inbox = r.hub.lock().inbox.clone();
    assert_eq!(inbox.len(), 1, "our version went to the conflict inbox");
    assert_eq!(inbox[0].conflict_of, "shared.txt");
    assert_eq!(inbox[0].key, "shared.txt");
    assert_eq!(inbox[0].mode, "inbox");
    assert_eq!(inbox[0].bytes, b"ours");
    assert_eq!(inbox[0].base_sha.as_deref(), Some(base_sha.as_str()));
    assert_eq!(inbox[0].device.as_deref(), Some("dev-a"));
    assert_eq!(inbox[0].device_name.as_deref(), Some("laptop"));
    // The hub keeps the name; we adopted its bytes.
    assert_eq!(r.hub.lock().bytes_of("shared.txt").unwrap(), b"theirs");
    let e = r.mirror.entry("shared.txt").unwrap();
    assert_eq!(e.sha256, hub_sha);
    assert_eq!(e.state, EntryState::Clean);
    assert_eq!(r.mirror.store.base("shared.txt").unwrap().sha256, hub_sha);
    assert_eq!(r.home_read("shared.txt").unwrap(), b"theirs");
    // Our bytes are kept as a conflict snapshot, and the record says so.
    assert!(r.mirror.local.has_conflict_bytes(&sha_hex(b"ours")));
    let conflicts = r.mirror.conflicts();
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].uploaded);
    assert_eq!(conflicts[0].inbox_doc_id, Some(4242));
    assert_eq!(r.mirror.status().conflicts, 1);
    assert_eq!(r.mirror.status().pending, 0);

    // Resolution on the hub clears the record and the snapshot.
    r.mirror.conflict_resolved("shared.txt");
    assert_eq!(r.mirror.status().conflicts, 0);
    assert!(!r.mirror.local.has_conflict_bytes(&sha_hex(b"ours")));
}

#[test]
fn conflict_via_the_feed_writes_a_conflict_copy_by_default() {
    let r = rig(opts());
    r.hub.lock().put("doc.md", b"v1");
    r.mirror.cycle(true);
    r.mirror.commit_write("doc.md", b"mine").unwrap();
    r.hub.lock().put("doc.md", b"theirs"); // logged: the feed reports it
    r.mirror.cycle(false);

    let keys: Vec<String> = r.hub.lock().objects.keys().cloned().collect();
    assert_eq!(keys.len(), 2, "hub has the original and the copy: {keys:?}");
    let copy = keys
        .iter()
        .find(|k| k.starts_with("doc (conflict from laptop "))
        .expect("conflict copy named after the device");
    assert!(copy.ends_with(".md"));
    assert_eq!(r.hub.lock().bytes_of(copy).unwrap(), b"mine");
    assert_eq!(r.hub.lock().bytes_of("doc.md").unwrap(), b"theirs");
    assert_eq!(r.hub.lock().inbox[0].mode, "rename");
    assert_eq!(r.mirror.entry("doc.md").unwrap().sha256, sha_hex(b"theirs"));
    assert_eq!(r.home_read("doc.md").unwrap(), b"theirs");
    // The copy comes back to us through the feed as an ordinary file.
    r.mirror.cycle(false);
    assert!(r.mirror.entry(copy).is_some());
    assert_eq!(r.home_read(copy).unwrap(), b"mine");
    assert_eq!(
        r.mirror.conflicts()[0].copy_key.as_deref(),
        Some(copy.as_str())
    );
}

#[test]
fn pull_lands_in_the_folder_with_the_hub_mtime() {
    let r = rig(opts());
    r.mirror.cycle(true);
    r.hub.lock().put("c.txt", b"one");
    r.mirror.cycle(false);
    let e = r.mirror.entry("c.txt").expect("pulled");
    assert_eq!(e.sha256, sha_hex(b"one"));
    assert_eq!(r.home_read("c.txt").unwrap(), b"one");
    assert_eq!(r.mirror.bytes_for_edit("c.txt").unwrap(), b"one");
    // The file carries the hub's mtime, and the entry the file's stamp.
    let hub_mtime = r.hub.lock().objects["c.txt"].mtime;
    let st = r.mirror.local.stat("c.txt").unwrap();
    assert_eq!(st.mtime, hub_mtime);
    assert_eq!(e.mtime, st.mtime);

    // A hub change replaces the file in place.
    r.hub.lock().put("c.txt", b"two");
    r.mirror.cycle(false);
    let e = r.mirror.entry("c.txt").unwrap();
    assert_eq!(e.sha256, sha_hex(b"two"));
    assert_eq!(r.home_read("c.txt").unwrap(), b"two");
    assert_eq!(
        r.mirror.store.base("c.txt").unwrap().sha256,
        sha_hex(b"two")
    );
    // No part file left behind.
    assert!(!r._dir.path().join("Home/.c.txt.canvas-part").exists());
}

#[test]
fn rename_both_ways() {
    let r = rig(opts());
    r.hub.lock().put("old.txt", b"bytes");
    r.mirror.cycle(true);
    assert_eq!(r.mirror.bytes_for_edit("old.txt").unwrap(), b"bytes");

    // Local mv: store re-keyed at once, hub told with If-Match.
    r.mirror.rename_local("old.txt", "sub/new.txt").unwrap();
    assert!(r.mirror.entry("old.txt").is_none());
    assert!(r.mirror.entry("sub/new.txt").is_some());
    r.mirror.cycle(false);
    let renames = r.hub.lock().entries("rename");
    assert_eq!(renames.len(), 1);
    assert_eq!(renames[0].from.as_deref(), Some("old.txt"));
    assert_eq!(renames[0].key, "sub/new.txt");
    assert_eq!(
        renames[0].if_match.as_deref(),
        Some(sha_hex(b"bytes").as_str())
    );
    assert!(r.hub.lock().bytes_of("old.txt").is_none());
    assert_eq!(r.hub.lock().bytes_of("sub/new.txt").unwrap(), b"bytes");
    assert_eq!(r.mirror.status().pending, 0);
    assert_eq!(
        r.mirror.entry("sub/new.txt").unwrap().state,
        EntryState::Clean
    );

    // Hub-side mv: re-keyed locally, no bytes moved.
    r.hub.lock().rename("sub/new.txt", "final.txt");
    r.mirror.cycle(false);
    assert!(r.mirror.entry("sub/new.txt").is_none());
    let e = r.mirror.entry("final.txt").expect("re-keyed from the feed");
    assert_eq!(e.sha256, sha_hex(b"bytes"));
    assert_eq!(e.state, EntryState::Clean);
    assert_eq!(
        r.mirror.store.base("final.txt").unwrap().sha256,
        sha_hex(b"bytes")
    );
    assert!(r.mirror.store.base("sub/new.txt").is_none());
    // Nothing was queued: the rename was theirs.
    assert_eq!(r.mirror.status().pending, 0);
}

#[test]
fn hub_delete_goes_to_trash_and_restore_pushes_as_new() {
    let r = rig(opts());
    r.hub.lock().put("keep.txt", b"precious");
    r.mirror.cycle(true);
    assert_eq!(r.home_read("keep.txt").unwrap(), b"precious");

    r.hub.lock().delete("keep.txt");
    r.mirror.cycle(false);
    assert!(r.mirror.entry("keep.txt").is_none());
    assert!(r.mirror.store.base("keep.txt").is_none());
    let trash = r.mirror.trash_list();
    assert_eq!(trash.len(), 1);
    assert_eq!(trash[0].0, "keep.txt");
    assert!(r.home_read("keep.txt").is_none(), "gone from the folder");
    assert_eq!(
        std::fs::read(r._dir.path().join("data/trash/keep.txt")).unwrap(),
        b"precious",
        "bytes survive in the local trash"
    );

    r.mirror.trash_restore("keep.txt").unwrap();
    assert_eq!(r.home_read("keep.txt").unwrap(), b"precious");
    r.mirror.cycle(false);
    assert_eq!(r.hub.lock().bytes_of("keep.txt").unwrap(), b"precious");
    let puts = r.hub.lock().entries("put");
    assert_eq!(puts.last().unwrap().if_none_match.as_deref(), Some("*"));
    assert!(r.mirror.trash_list().is_empty());
    assert_eq!(r.mirror.entry("keep.txt").unwrap().state, EntryState::Clean);
}

#[test]
fn local_delete_propagates_with_if_match_and_edit_beats_delete() {
    let r = rig(opts());
    r.hub.lock().put("gone.txt", b"x");
    r.hub.lock().put("edited.txt", b"y");
    r.mirror.cycle(true);

    assert!(r.mirror.delete_local("gone.txt").unwrap());
    assert!(
        r.mirror.entry("gone.txt").is_none(),
        "tombstone hides the entry"
    );
    // The hub edits the other one under us before we delete it.
    assert!(r.mirror.delete_local("edited.txt").unwrap());
    let new_sha = r.hub.lock().put_silent("edited.txt", b"y2");
    r.mirror.cycle(false);

    assert!(r.hub.lock().bytes_of("gone.txt").is_none());
    let dels = r.hub.lock().entries("delete");
    assert_eq!(dels.len(), 1);
    assert_eq!(dels[0].if_match.as_deref(), Some(sha_hex(b"x").as_str()));
    assert!(r.mirror.store.entry("gone.txt").is_none());
    // Edit beats delete: the hub's edit came back.
    assert_eq!(r.hub.lock().bytes_of("edited.txt").unwrap(), b"y2");
    let e = r.mirror.entry("edited.txt").expect("restored from the hub");
    assert_eq!(e.sha256, new_sha);
    assert_eq!(r.mirror.status().pending, 0);
}

#[test]
fn cursor_too_old_rebuilds_from_the_listing() {
    let r = rig(opts());
    r.hub.lock().put("a.txt", b"a");
    r.mirror.cycle(true);
    assert_eq!(r.mirror.status().cursor, Some(1));

    // Lots happens while we are away, and the hub forgets the log.
    {
        let mut h = r.hub.lock();
        h.put("b.txt", b"b");
        h.delete("a.txt");
        h.put("c.txt", b"c");
        h.trim_log();
    }
    r.mirror.cycle(false);
    assert!(
        r.mirror.entry("a.txt").is_none(),
        "listing shows a.txt gone"
    );
    assert!(r.mirror.entry("b.txt").is_some());
    assert!(r.mirror.entry("c.txt").is_some());
    assert_eq!(r.mirror.trash_list().len(), 1);
    assert_eq!(
        r.mirror.status().cursor,
        Some(4),
        "tails from the listing head"
    );
    assert_eq!(r.mirror.status().state, SyncState::Idle);
    let reqs = r.hub.lock().requests.clone();
    assert!(reqs.iter().any(|q| q.ends_with("/changes")));
    assert!(reqs.iter().filter(|q| q.ends_with("/objects")).count() >= 2);
}

#[test]
fn offline_queue_drains_when_the_hub_comes_back() {
    let port = FakeHub::free_port();
    let (mirror, _dir) = rig_with(&format!("http://127.0.0.1:{port}"), opts());

    // Nobody is listening: every local op still succeeds and queues.
    mirror
        .commit_write("offline/a.txt", b"written offline")
        .unwrap();
    mirror.mkdir_local("offline/empty").unwrap();
    mirror.cycle(true);
    assert_eq!(mirror.status().state, SyncState::Offline);
    assert!(mirror.status().last_error.is_some());
    assert_eq!(mirror.status().pending, 2);
    assert_eq!(
        mirror.entry("offline/a.txt").unwrap().state,
        EntryState::Dirty
    );
    // Everything written is a real file, readable with or without a hub.
    assert_eq!(
        mirror.bytes_for_edit("offline/a.txt").unwrap(),
        b"written offline"
    );
    assert_eq!(
        std::fs::read(_dir.path().join("Home/offline/a.txt")).unwrap(),
        b"written offline"
    );
    assert!(_dir.path().join("Home/offline/empty").is_dir());
    mirror
        .rename_local("offline/a.txt", "offline/b.txt")
        .unwrap();
    assert!(mirror.entry("offline/b.txt").is_some());
    assert_eq!(
        mirror.status().pending,
        2,
        "rename re-keys the pending push"
    );

    let hub = FakeHub::start_on(port);
    mirror.cycle(false);
    assert_eq!(mirror.status().state, SyncState::Idle);
    assert_eq!(mirror.status().pending, 0);
    assert_eq!(
        hub.lock().bytes_of("offline/b.txt").unwrap(),
        b"written offline"
    );
    assert!(hub.lock().bytes_of("offline/a.txt").is_none());
    assert!(_dir.path().join("Home/offline/b.txt").is_file());
    assert!(!_dir.path().join("Home/offline/a.txt").exists());
    assert_eq!(hub.lock().mkdirs, vec!["offline/empty".to_string()]);
    assert_eq!(
        mirror.entry("offline/b.txt").unwrap().state,
        EntryState::Clean
    );
    hub.stop();
}

#[test]
fn excluded_keys_are_never_pushed() {
    let r = rig(opts());
    r.mirror.cycle(true);
    r.mirror.commit_write(".env", b"secret").unwrap();
    r.mirror
        .commit_write("proj/node_modules/x.js", b"dep")
        .unwrap();
    r.mirror.commit_write("proj/src/x.js", b"src").unwrap();
    r.mirror.cycle(false);
    let keys: Vec<String> = r.hub.lock().objects.keys().cloned().collect();
    assert_eq!(keys, vec!["proj/src/x.js".to_string()]);
    assert_eq!(r.mirror.status().skipped, 2);
    assert_eq!(r.mirror.status().pending, 0);
    // Still there locally.
    assert!(r.mirror.entry(".env").is_some());
}

#[test]
fn store_survives_a_restart_with_cursor_and_queue() {
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    let open = || open_mirror(&dir, &hub.url, opts());
    hub.lock().put("seed.txt", b"seed");
    {
        let m = open();
        m.cycle(true);
        m.commit_write("queued.txt", b"q").unwrap();
        // No cycle: the push stays queued in redb.
    }
    let m = open();
    assert_eq!(m.status().cursor, Some(1));
    assert_eq!(m.status().pending, 1);
    assert!(m.entry("seed.txt").is_some());
    m.cycle(false);
    assert_eq!(hub.lock().bytes_of("queued.txt").unwrap(), b"q");
}

#[test]
fn delayed_notifications_do_not_conflict_with_current_local_edits() {
    let r = rig(opts());
    r.hub.lock().put("doc.txt", b"base");
    r.mirror.cycle(true);
    r.hub.lock().put("doc.txt", b"intermediate");
    r.hub.lock().put("doc.txt", b"current");
    r.mirror.commit_write("doc.txt", b"current").unwrap();
    r.mirror.cycle(true);
    r.mirror.store.set_cursor(1).unwrap();
    r.mirror.commit_write("doc.txt", b"local edit").unwrap();
    r.mirror.cycle(false);
    assert!(r.mirror.conflicts().is_empty());
    assert!(r.hub.lock().inbox.is_empty());
    assert_eq!(r.hub.lock().bytes_of("doc.txt").unwrap(), b"local edit");
}

#[test]
fn delayed_delete_does_not_resurrect_a_recreated_file() {
    let r = rig(opts());
    r.hub.lock().put("doc.txt", b"base");
    r.mirror.cycle(true);
    r.hub.lock().delete("doc.txt");
    r.hub.lock().put("doc.txt", b"replacement");
    r.mirror.cycle(false);
    assert_eq!(
        r.mirror.entry("doc.txt").unwrap().sha256,
        sha_hex(b"replacement")
    );
    assert_eq!(r.mirror.status().pending, 0);
    assert!(r.mirror.conflicts().is_empty());
}

#[test]
fn full_listing_preserves_pending_local_rename() {
    let r = rig(opts());
    r.hub.lock().put("old.txt", b"bytes");
    r.mirror.cycle(true);
    r.mirror.bytes_for_edit("old.txt").unwrap();
    r.mirror.rename_local("old.txt", "new.txt").unwrap();
    r.mirror.cycle(true);
    r.mirror.cycle(false);
    assert!(r.hub.lock().bytes_of("old.txt").is_none());
    assert!(r.mirror.entry("old.txt").is_none());
    assert_eq!(r.hub.lock().bytes_of("new.txt").unwrap(), b"bytes");
    assert_eq!(r.mirror.entry("new.txt").unwrap().state, EntryState::Clean);
    assert!(r.mirror.conflicts().is_empty());
}

#[test]
fn delayed_rename_does_not_move_a_recreated_source() {
    let r = rig(opts());
    r.hub.lock().put("old.txt", b"original");
    r.mirror.cycle(true);
    r.hub.lock().rename("old.txt", "new.txt");
    r.hub.lock().put("old.txt", b"recreated");
    r.hub.lock().put("new.txt", b"edited target");
    r.mirror.cycle(false);
    assert_eq!(
        r.mirror.entry("old.txt").unwrap().sha256,
        sha_hex(b"recreated")
    );
    assert_eq!(
        r.mirror.entry("new.txt").unwrap().sha256,
        sha_hex(b"edited target")
    );
    assert!(r.mirror.conflicts().is_empty());
    assert_eq!(r.mirror.status().pending, 0);
}

// ── the folder while no daemon runs ──────────────────────────────────────────

/// Files added offline to an initialized local replica are pushed.
#[test]
fn files_added_to_an_initialized_folder_before_sync_are_pushed() {
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    drop(open_mirror(&dir, &hub.url, opts()));
    std::fs::create_dir_all(dir.path().join("Home/Docs")).unwrap();
    std::fs::write(dir.path().join("Home/Docs/plan.md"), b"local plan").unwrap();
    std::fs::write(dir.path().join("Home/README"), b"readme").unwrap();
    std::fs::create_dir_all(dir.path().join("Home/Empty")).unwrap();
    hub.lock().put("remote.txt", b"from the hub");

    let m = open_mirror(&dir, &hub.url, opts());
    // The scan ran at open: both files are entries, dirty, with pushes
    // queued; both directory names get published ahead of their contents.
    assert_eq!(m.entry("Docs/plan.md").unwrap().state, EntryState::Dirty);
    assert_eq!(m.entry("README").unwrap().state, EntryState::Dirty);
    assert!(m.store.has_dir("Docs"));
    assert!(m.store.has_dir("Empty"));
    assert_eq!(m.status().pending, 4);

    m.cycle(true);
    assert_eq!(hub.lock().bytes_of("Docs/plan.md").unwrap(), b"local plan");
    assert_eq!(hub.lock().bytes_of("README").unwrap(), b"readme");
    let mut dirs = hub.lock().mkdirs.clone();
    dirs.sort();
    assert_eq!(dirs, vec!["Docs".to_string(), "Empty".to_string()]);
    assert_eq!(
        std::fs::read(dir.path().join("Home/remote.txt")).unwrap(),
        b"from the hub"
    );
    assert_eq!(m.entry("Docs/plan.md").unwrap().state, EntryState::Clean);
    assert_eq!(m.status().pending, 0);
}

/// The point of the exercise: edit, add and delete in the folder with the
/// daemon down (a plane), then start it again — everything reconciles.
#[test]
fn offline_edits_are_reconciled_when_the_daemon_comes_back() {
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    let base_sha = hub.lock().put("notes.md", b"v1");
    hub.lock().put("old.txt", b"to be deleted");
    hub.lock().put("same.txt", b"untouched");
    {
        let m = open_mirror(&dir, &hub.url, opts());
        m.cycle(true);
        assert_eq!(m.status().pending, 0);
    }
    // No daemon. The folder is just a folder.
    let home = dir.path().join("Home");
    assert_eq!(std::fs::read(home.join("notes.md")).unwrap(), b"v1");
    std::fs::write(home.join("notes.md"), b"v2 written on the plane").unwrap();
    std::fs::remove_file(home.join("old.txt")).unwrap();
    std::fs::create_dir_all(home.join("trip")).unwrap();
    std::fs::write(home.join("trip/new.md"), b"brand new").unwrap();
    // Touch without a change: same bytes, new stamp.
    let f = std::fs::File::options()
        .write(true)
        .open(home.join("same.txt"))
        .unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .unwrap();
    drop(f);
    // Meanwhile the hub moved too, on a file we did not touch.
    hub.lock().put("remote-only.txt", b"landed while away");

    let m = open_mirror(&dir, &hub.url, opts());
    assert_eq!(m.entry("notes.md").unwrap().state, EntryState::Dirty);
    assert_eq!(m.entry("trip/new.md").unwrap().state, EntryState::Dirty);
    assert_eq!(m.entry("same.txt").unwrap().state, EntryState::Clean);
    assert!(m.entry("old.txt").is_none(), "tombstoned");
    // push notes, mkdir trip, push new, delete old — nothing for same.txt.
    assert_eq!(m.status().pending, 4);

    m.cycle(false);
    assert_eq!(
        hub.lock().bytes_of("notes.md").unwrap(),
        b"v2 written on the plane"
    );
    let puts = hub.lock().entries("put");
    let notes_put = puts.iter().rev().find(|p| p.key == "notes.md").unwrap();
    assert_eq!(notes_put.if_match.as_deref(), Some(base_sha.as_str()));
    assert_eq!(hub.lock().bytes_of("trip/new.md").unwrap(), b"brand new");
    assert!(hub.lock().bytes_of("old.txt").is_none());
    assert_eq!(hub.lock().bytes_of("same.txt").unwrap(), b"untouched");
    assert_eq!(
        std::fs::read(home.join("remote-only.txt")).unwrap(),
        b"landed while away"
    );
    assert_eq!(m.status().pending, 0);
    assert!(m.conflicts().is_empty());
}

/// Edited on the plane AND on the hub: a conflict, not an overwrite. Our
/// bytes get a conflict-copy name, the hub's take the name, nothing is lost.
#[test]
fn offline_edit_that_collides_with_a_hub_edit_is_a_conflict() {
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    hub.lock().put("shared.md", b"base");
    {
        let m = open_mirror(&dir, &hub.url, opts());
        m.cycle(true);
    }
    std::fs::write(dir.path().join("Home/shared.md"), b"ours, offline").unwrap();
    hub.lock().put("shared.md", b"theirs, online");

    let m = open_mirror(&dir, &hub.url, opts());
    m.cycle(false);
    let inbox = hub.lock().inbox.clone();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].bytes, b"ours, offline");
    assert_eq!(inbox[0].mode, "rename");
    assert_ne!(inbox[0].key, "shared.md");
    assert_eq!(
        hub.lock().bytes_of(&inbox[0].key).unwrap(),
        b"ours, offline"
    );
    assert_eq!(
        std::fs::read(dir.path().join("Home/shared.md")).unwrap(),
        b"theirs, online"
    );
    assert!(m.local.has_conflict_bytes(&sha_hex(b"ours, offline")));
    assert_eq!(m.status().conflicts, 1);
    m.cycle(false);
    assert_eq!(
        std::fs::read(dir.path().join("Home").join(&inbox[0].key)).unwrap(),
        b"ours, offline"
    );
}

/// A file removed from the folder while the hub changed it comes back:
/// edit beats delete.
#[test]
fn offline_delete_of_a_hub_edited_file_brings_the_edit_back() {
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    hub.lock().put("keep.md", b"v1");
    {
        let m = open_mirror(&dir, &hub.url, opts());
        m.cycle(true);
    }
    std::fs::remove_file(dir.path().join("Home/keep.md")).unwrap();
    hub.lock().put_silent("keep.md", b"v2");

    let m = open_mirror(&dir, &hub.url, opts());
    m.cycle(false);
    assert_eq!(
        std::fs::read(dir.path().join("Home/keep.md")).unwrap(),
        b"v2"
    );
    assert_eq!(m.entry("keep.md").unwrap().state, EntryState::Clean);
}

/// A hub-side rename moves the real file; a local rename of a directory
/// moves the real directory.
#[test]
fn renames_move_real_files_and_directories() {
    let r = rig(opts());
    r.hub.lock().put("a/one.txt", b"1");
    r.hub.lock().put("a/two.txt", b"2");
    r.mirror.cycle(true);

    r.hub.lock().rename("a/one.txt", "b/uno.txt");
    r.mirror.cycle(false);
    assert!(r.home_read("a/one.txt").is_none());
    assert_eq!(r.home_read("b/uno.txt").unwrap(), b"1");
    assert!(r.mirror.store.has_dir("b"));

    r.mirror.rename_dir_local("a", "c").unwrap();
    assert_eq!(r.home_read("c/two.txt").unwrap(), b"2");
    assert!(!r._dir.path().join("Home/a").exists());
    r.mirror.cycle(false);
    assert_eq!(r.hub.lock().bytes_of("c/two.txt").unwrap(), b"2");
    assert!(r.hub.lock().bytes_of("a/two.txt").is_none());
    assert!(r.hub.lock().rmdirs.is_empty());
    assert_eq!(
        r.hub.lock().directory_renames,
        vec![("a".into(), "c".into())]
    );
}

/// Old caches have no local directory identity: refuse before migration and
/// preserve every cached byte and legacy ledger for explicit recovery.
#[test]
fn an_unbound_generation_1_cache_is_preserved_and_refused() {
    use redb::{Database, TableDefinition};
    use serde_json::json;
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let cache = data.join("cache");
    let clean_sha = hub.lock().put("Docs/clean.txt", b"clean");
    let old_sha = hub.lock().put("edited.txt", b"old");
    let never_sha = hub.lock().put("never.txt", b"never fetched");
    let put_cache = |bytes: &[u8]| -> String {
        let sha = sha_hex(bytes);
        let p = cache.join(&sha[..2]).join(&sha);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        sha
    };
    put_cache(b"clean");
    let mine = put_cache(b"mine, unpushed");
    let lock = put_cache(b"lock");
    {
        std::fs::create_dir_all(&data).unwrap();
        let db = Database::create(data.join("mirror.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let t = TableDefinition::<&str, &[u8]>::new("entries_v1");
            let mut e = tx.open_table(t).unwrap();
            let entry = |sha: &str, state: &str| {
                json!({ "sha256": sha, "size": 5, "mtime": 1_700_000_000_000u64, "state": state })
                    .to_string()
            };
            e.insert("Docs/clean.txt", entry(&clean_sha, "clean").as_bytes())
                .unwrap();
            e.insert("edited.txt", entry(&mine, "dirty").as_bytes())
                .unwrap();
            e.insert("never.txt", entry(&never_sha, "clean").as_bytes())
                .unwrap();
            // An editor lock file the old mirror held dirty-but-skipped.
            e.insert("Docs/.~lock.plan.xlsx#", entry(&lock, "dirty").as_bytes())
                .unwrap();
            let t = TableDefinition::<&str, &[u8]>::new("base_v1");
            let mut b = tx.open_table(t).unwrap();
            let base = |sha: &str| {
                json!({ "sha256": sha, "size": 5, "mtime": 1_700_000_000_000u64, "remote_seq": 1 })
                    .to_string()
            };
            b.insert("Docs/clean.txt", base(&clean_sha).as_bytes())
                .unwrap();
            b.insert("edited.txt", base(&old_sha).as_bytes()).unwrap();
            b.insert("never.txt", base(&never_sha).as_bytes()).unwrap();
            // The old cursor and listing marker, which the new store retires.
            let t = TableDefinition::<&str, u64>::new("cursor_v1");
            tx.open_table(t).unwrap().insert("cursor", 3u64).unwrap();
        }
        tx.commit().unwrap();
    }

    assert!(try_open_mirror(&dir, &hub.url, opts()).is_err());
    assert!(hub.lock().requests.is_empty());
    assert_eq!(
        std::fs::read(cache.join(&mine[..2]).join(&mine)).unwrap(),
        b"mine, unpushed"
    );
    assert!(!dir.path().join("Home/.workspace.json").exists());
    let db = Database::open(data.join("mirror.redb")).unwrap();
    let tx = db.begin_read().unwrap();
    assert!(tx
        .open_table(TableDefinition::<&str, &[u8]>::new("entries_v1"))
        .unwrap()
        .get("edited.txt")
        .unwrap()
        .is_some());
}

#[test]
fn a_thousand_photos_move_with_one_request_and_no_file_transfers() {
    use std::os::unix::fs::MetadataExt;
    let r = rig(opts());
    for n in 0..1000 {
        r.hub.lock().put(
            &format!("Architektúra/Domček/{n}.jpg"),
            format!("photo-{n}").as_bytes(),
        );
    }
    r.mirror.cycle(true);
    let before = std::fs::metadata(r._dir.path().join("Home/Architektúra/Domček/0.jpg"))
        .unwrap()
        .ino();
    r.hub.lock().requests.clear();
    r.mirror
        .rename_dir_local("Architektúra/Domček", "Architektúra/Fotky")
        .unwrap();
    assert_eq!(r.mirror.store.jobs().len(), 1);
    // A full reconcile must not resurrect the old server paths before the move.
    r.mirror.cycle(true);
    r.mirror.cycle(false);
    let st = r.hub.lock();
    assert_eq!(st.directory_renames.len(), 1);
    assert_eq!(
        st.requests
            .iter()
            .filter(|q| q.ends_with("/objects/rename"))
            .count(),
        1
    );
    assert!(
        !st.requests.iter().any(|q| q.starts_with("HEAD ")
            || q.starts_with("PUT ")
            || q.starts_with("DELETE ")
            || (q.starts_with("GET ") && q.contains("/objects/"))),
        "{:?}",
        st.requests
    );
    assert_eq!(st.objects.len(), 1000);
    assert!(st
        .objects
        .keys()
        .all(|k| k.starts_with("Architektúra/Fotky/")));
    assert_eq!(
        std::fs::metadata(r._dir.path().join("Home/Architektúra/Fotky/0.jpg"))
            .unwrap()
            .ino(),
        before
    );
    assert!(!r._dir.path().join("Home/Architektúra/Domček").exists());
}

#[test]
fn directory_move_orders_dirty_children_after_the_move() {
    let r = rig(opts());
    r.hub.lock().put("old/edited.txt", b"base");
    r.hub.lock().put("old/deleted.txt", b"remove");
    r.mirror.cycle(true);
    r.mirror.commit_write("old/edited.txt", b"edited").unwrap();
    r.mirror.commit_write("old/new.txt", b"new").unwrap();
    r.mirror.delete_local("old/deleted.txt").unwrap();
    r.mirror.rename_dir_local("old", "new").unwrap();
    r.hub.lock().requests.clear();
    r.mirror.cycle(false);
    let st = r.hub.lock();
    assert_eq!(st.bytes_of("new/edited.txt").unwrap(), b"edited");
    assert_eq!(st.bytes_of("new/new.txt").unwrap(), b"new");
    assert!(st.bytes_of("new/deleted.txt").is_none());
    let rename = st
        .requests
        .iter()
        .position(|q| q.ends_with("/objects/rename"))
        .unwrap();
    let delete = st
        .requests
        .iter()
        .position(|q| q.starts_with("DELETE ") && q.ends_with("/objects/old/deleted.txt"))
        .unwrap();
    assert!(
        delete < rename,
        "earlier structural changes use the old remote namespace"
    );
    assert!(
        st.requests
            .iter()
            .enumerate()
            .filter(|(_, q)| q.starts_with("PUT "))
            .all(|(i, _)| i > rename),
        "content uploads follow the directory rename"
    );
}

#[test]
fn lost_directory_rename_reply_can_be_retried_after_restart() {
    use canvas_fuse::mirror::store::JobKind;
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    hub.lock().put("old/a.jpg", b"photo");
    {
        let m = open_mirror(&dir, &hub.url, opts());
        m.cycle(true);
        m.rename_dir_local("old", "new").unwrap();
        let job = m.store.jobs().pop().unwrap();
        let JobKind::RenameDir {
            from,
            to,
            operation_id,
            ..
        } = job.kind
        else {
            panic!("directory operation expected");
        };
        // Server committed; client never recorded the successful response.
        m.hub.rename_directory(&from, &to, &operation_id).unwrap();
    }
    let m = open_mirror(&dir, &hub.url, opts());
    m.cycle(true);
    assert!(m.store.jobs().is_empty());
    assert_eq!(hub.lock().directory_renames.len(), 1);
    assert_eq!(
        std::fs::read(dir.path().join("Home/new/a.jpg")).unwrap(),
        b"photo"
    );
    assert!(!dir.path().join("Home/old").exists());
}

#[test]
fn refused_directory_move_keeps_children_blocked_and_both_trees_intact() {
    let r = rig(opts());
    r.hub.lock().put("old/a.txt", b"base");
    r.mirror.cycle(true);
    r.mirror.rename_dir_local("old", "new").unwrap();
    r.mirror.commit_write("new/a.txt", b"ours").unwrap();
    r.hub.lock().put("new/a.txt", b"theirs");
    r.mirror.cycle(true);
    assert_eq!(r.home_read("new/a.txt").unwrap(), b"ours");
    assert!(!r._dir.path().join("Home/old").exists());
    assert_eq!(r.hub.lock().bytes_of("old/a.txt").unwrap(), b"base");
    assert_eq!(r.hub.lock().bytes_of("new/a.txt").unwrap(), b"theirs");
    assert!(r.mirror.status().failed > 0);
    assert!(r.mirror.status().pending >= 2);
}

#[test]
fn interrupted_local_directory_move_recovers_its_ledger_after_restart() {
    use canvas_fuse::mirror::store::JobKind;
    use std::os::unix::fs::MetadataExt;
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    hub.lock().put("old/a.jpg", b"photo");
    {
        let m = open_mirror(&dir, &hub.url, opts());
        m.cycle(true);
        let st = std::fs::metadata(dir.path().join("Home/old")).unwrap();
        m.store
            .enqueue(JobKind::RenameDir {
                from: "old".into(),
                to: "new".into(),
                operation_id: "local-recovery".into(),
                dev: st.dev(),
                ino: st.ino(),
                local_applied: false,
                remote: true,
            })
            .unwrap();
        m.local.rename("old", "new").unwrap();
        // Crash before applying the ledger transaction.
    }
    let m = open_mirror(&dir, &hub.url, opts());
    m.cycle(true);
    assert!(m.store.jobs().is_empty());
    assert!(m.store.entry("old/a.jpg").is_none());
    assert_eq!(
        m.store.entry("new/a.jpg").unwrap().sha256,
        sha_hex(b"photo")
    );
    assert_eq!(hub.lock().bytes_of("new/a.jpg").unwrap(), b"photo");
}

#[test]
fn unpushed_and_empty_directories_keep_working() {
    let r = rig(opts());
    r.mirror.cycle(true);
    r.mirror.mkdir_local("empty").unwrap();
    r.mirror.rename_dir_local("empty", "empty-new").unwrap();
    r.mirror.cycle(false);
    assert!(r.hub.lock().mkdirs.contains(&"empty-new".to_string()));
    // Implicit parent from a newly created file has no pending Mkdir.
    r.mirror.commit_write("old/a.txt", b"new photo").unwrap();
    r.mirror.rename_dir_local("old", "new").unwrap();
    r.mirror.cycle(false);
    assert_eq!(r.hub.lock().bytes_of("new/a.txt").unwrap(), b"new photo");
    assert!(r.mirror.store.jobs().is_empty());
}

#[test]
fn edit_outside_fuse_during_a_pending_move_is_preserved_on_reconcile() {
    let r = rig(opts());
    r.hub.lock().put("old/a.txt", b"base");
    r.mirror.cycle(true);
    r.mirror.rename_dir_local("old", "new").unwrap();
    std::fs::write(r._dir.path().join("Home/new/a.txt"), b"our outside edit").unwrap();
    r.hub.lock().put("old/a.txt", b"their edit");
    r.mirror.cycle(true);
    r.mirror.cycle(false);
    assert_eq!(r.hub.lock().bytes_of("new/a.txt").unwrap(), b"their edit");
    assert!(r
        .hub
        .lock()
        .inbox
        .iter()
        .any(|c| c.bytes == b"our outside edit"));
}

#[test]
fn offline_additions_upload_before_the_first_remote_listing_or_download() {
    let hub = FakeHub::start();
    let dir = tempfile::tempdir().unwrap();
    for n in 0..20 {
        hub.lock().put(&format!("remote/{n}.jpg"), b"remote photo");
    }
    drop(open_mirror(&dir, &hub.url, opts()));
    std::fs::create_dir_all(dir.path().join("Home/Working/Empty")).unwrap();
    std::fs::write(dir.path().join("Home/Working/new.txt"), b"offline work").unwrap();
    let m = open_mirror(&dir, &hub.url, opts());
    m.cycle(true);
    let st = hub.lock();
    assert_eq!(st.bytes_of("Working/new.txt").unwrap(), b"offline work");
    let upload = st
        .requests
        .iter()
        .position(|q| q.starts_with("PUT "))
        .unwrap();
    let listing = st
        .requests
        .iter()
        .position(|q| q.ends_with("/objects"))
        .unwrap();
    assert!(upload < listing, "{:?}", st.requests);
    assert!(st.mkdirs.contains(&"Working".into()));
    assert!(st.mkdirs.contains(&"Working/Empty".into()));
}

#[test]
fn fresh_activity_preempts_a_discovered_upload_backlog() {
    use canvas_fuse::mirror::store::JobPriority;
    let r = rig(opts());
    r.mirror.cycle(true);
    for n in 0..5 {
        r.mirror
            .commit_write(&format!("backlog-{n}.txt"), b"offline")
            .unwrap();
    }
    for mut job in r.mirror.store.jobs() {
        job.priority = JobPriority::Discovered;
        r.mirror.store.update_job(&job).unwrap();
    }
    r.mirror.commit_write("active.txt", b"working now").unwrap();
    let weak = Arc::downgrade(&r.mirror);
    r.hub.lock().on_request = Some(Arc::new(move |request| {
        if request.ends_with("/objects/active.txt") && request.starts_with("PUT ") {
            weak.upgrade()
                .unwrap()
                .commit_write("urgent.txt", b"just saved")
                .unwrap();
        }
    }));
    r.hub.lock().requests.clear();
    r.mirror.cycle(false);
    let st = r.hub.lock();
    let puts: Vec<_> = st
        .requests
        .iter()
        .filter(|q| q.starts_with("PUT "))
        .collect();
    assert!(puts[0].ends_with("/active.txt"), "{puts:?}");
    assert!(puts[1].ends_with("/urgent.txt"), "{puts:?}");
    assert_eq!(puts.len(), 7);
}

#[test]
fn uploads_during_a_listing_preempt_downloads_without_using_stale_metadata() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let r = rig(opts());
    r.hub.lock().put("z.txt", b"base");
    r.mirror.cycle(true);
    for n in 0..20 {
        r.hub.lock().put(&format!("remote/{n}.txt"), b"download");
    }
    let weak = Arc::downgrade(&r.mirror);
    let once = AtomicBool::new(false);
    r.hub.lock().on_request = Some(Arc::new(move |request| {
        if request.ends_with("/objects") && !once.swap(true, Ordering::Relaxed) {
            let m = weak.upgrade().unwrap();
            m.mkdir_local("remote").unwrap();
            m.commit_write("active.txt", b"created during listing")
                .unwrap();
            m.commit_write("z.txt", b"edited during listing").unwrap();
        }
    }));
    r.hub.lock().requests.clear();
    r.mirror.cycle(true);
    assert_eq!(
        r.home_read("active.txt").unwrap(),
        b"created during listing"
    );
    assert_eq!(r.home_read("z.txt").unwrap(), b"edited during listing");
    assert!(r.mirror.conflicts().is_empty());
    let st = r.hub.lock();
    let first_download = st
        .requests
        .iter()
        .position(|q| q.starts_with("GET ") && q.contains("/objects/"))
        .unwrap();
    let last_upload = st
        .requests
        .iter()
        .rposition(|q| q.starts_with("PUT "))
        .unwrap();
    assert!(last_upload < first_download, "{:?}", st.requests);
    assert_eq!(
        st.bytes_of("active.txt").unwrap(),
        b"created during listing"
    );
}

#[test]
fn directory_preparation_respects_exclusions_and_preserves_local_path_conflicts() {
    use std::os::unix::fs::symlink;
    let mut options = opts();
    options.ignore.push("Excluded/**".into());
    let r = rig(options);
    let outside = r._dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, r.mirror.local.path("Link")).unwrap();
    r.mirror.commit_write("Blocked", b"local file").unwrap();
    {
        let mut hub = r.hub.lock();
        for key in [
            "Blocked/nested/a.txt",
            "Link/nested/a.txt",
            "Excluded/nested/a.txt",
            "Visible/Deep/a.txt",
        ] {
            hub.put(key, b"remote");
        }
    }
    r.mirror.cycle(true);
    assert_eq!(r.home_read("Blocked").unwrap(), b"local file");
    assert!(
        !outside.join("nested").exists(),
        "must not follow a local symlink"
    );
    assert!(!r.mirror.local.path("Excluded").exists());
    assert!(r.mirror.local.path("Visible/Deep").is_dir());
    assert!(!r.mirror.store.has_dir("Blocked"));
    assert!(!r.mirror.store.has_dir("Link"));
    assert!(
        r.mirror.store.cursor().is_none(),
        "failed preparation must be retryable"
    );
}

#[test]
fn restart_after_failed_initial_download_preserves_the_skeleton_without_deleting_remote_files() {
    let hub = FakeHub::start();
    hub.lock().put("Architecture/photo.jpg", b"photo");
    hub.lock().put("Timesheets/2026/data.txt", b"data");
    hub.lock()
        .corrupt_downloads
        .insert("Architecture/photo.jpg".into());
    let dir = tempfile::tempdir().unwrap();
    {
        let m = open_mirror(&dir, &hub.url, opts());
        m.cycle(true);
        assert!(m.store.entries("").is_empty());
        assert!(m.local.path("Timesheets/2026").is_dir());
    }
    let m = open_mirror(&dir, &hub.url, opts());
    m.cycle(true);
    assert_eq!(
        m.local.read_all("Architecture/photo.jpg").unwrap(),
        b"photo"
    );
    assert_eq!(
        m.local.read_all("Timesheets/2026/data.txt").unwrap(),
        b"data"
    );
    assert!(hub.lock().mkdirs.is_empty());
    assert!(!hub.lock().requests.iter().any(|q| q.starts_with("DELETE ")));
    assert_eq!(m.status().pending, 0);
}

#[test]
fn an_old_hub_never_receives_a_recursive_delete_as_a_rmdir_fallback() {
    let r = rig(opts());
    r.mirror.mkdir_local("Empty").unwrap();
    r.mirror.cycle(true);
    r.hub
        .lock()
        .failures
        .push(("POST /rest/v2/workspaces/ws1/home/rmdir".into(), 404));
    assert!(r.mirror.rmdir_local("Empty").unwrap());
    r.mirror.cycle(false);
    assert_eq!(r.mirror.status().failed, 1);
    assert_eq!(r.mirror.status().pending, 1);
    assert!(!r
        .hub
        .lock()
        .requests
        .iter()
        .any(|q| q.starts_with("DELETE ")));
}

#[test]
fn a_save_during_a_background_download_is_preserved_and_uploaded_next() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let r = rig(opts());
    r.mirror.cycle(true);
    r.hub.lock().put("a.txt", b"remote");
    r.hub.lock().put("b.txt", b"background");
    let weak = Arc::downgrade(&r.mirror);
    let once = AtomicBool::new(false);
    r.hub.lock().on_request = Some(Arc::new(move |request| {
        if request.starts_with("GET ")
            && request.ends_with("/objects/a.txt")
            && !once.swap(true, Ordering::Relaxed)
        {
            let m = weak.upgrade().unwrap();
            m.commit_write("a.txt", b"our concurrent save").unwrap();
            m.commit_write("urgent.txt", b"fresh activity").unwrap();
        }
    }));
    r.hub.lock().requests.clear();
    r.mirror.cycle(true);
    let st = r.hub.lock();
    assert_eq!(st.bytes_of("urgent.txt").unwrap(), b"fresh activity");
    assert!(st.inbox.iter().any(|c| c.bytes == b"our concurrent save"));
    let upload = st
        .requests
        .iter()
        .position(|q| q.starts_with("PUT ") && q.ends_with("/urgent.txt"))
        .unwrap();
    let next_download = st
        .requests
        .iter()
        .position(|q| q.starts_with("GET ") && q.ends_with("/objects/b.txt"))
        .unwrap();
    assert!(upload < next_download, "{:?}", st.requests);
}

#[test]
fn inotify_uploads_direct_additions_edits_and_imported_subtrees_without_polling() {
    use canvas_fuse::mirror::sync::EngineMsg;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    let mut options = opts();
    options.poll_secs = 3600;
    let r = rig(options);
    r.mirror.cycle(true);
    let stop = Arc::new(AtomicBool::new(false));
    let tx = r.mirror.spawn_engine(stop.clone()).unwrap();
    let wait = |key: &str, bytes: &[u8]| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if r.hub.lock().bytes_of(key).as_deref() == Some(bytes) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "inotify did not upload {key}; status: {:?}",
            r.mirror.status()
        );
    };
    // Let the engine finish its startup pass, so no scan can discover this.
    assert!(r.mirror.sync_now(Duration::from_secs(5)));
    std::fs::write(r._dir.path().join("Home/active.txt"), b"new").unwrap();
    wait("active.txt", b"new");
    std::fs::write(r._dir.path().join("Home/active.txt"), b"edited").unwrap();
    wait("active.txt", b"edited");
    // Millisecond timestamps and equal lengths cannot hide a notified save.
    let modified = std::fs::metadata(r._dir.path().join("Home/active.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(r._dir.path().join("Home/active.txt"))
        .unwrap();
    std::io::Write::write_all(&mut file, b"EDITED").unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    wait("active.txt", b"EDITED");
    std::fs::create_dir_all(r._dir.path().join("import/sub")).unwrap();
    std::fs::write(
        r._dir.path().join("import/sub/photo.jpg"),
        b"imported photo",
    )
    .unwrap();
    std::fs::rename(
        r._dir.path().join("import"),
        r._dir.path().join("Home/Imported"),
    )
    .unwrap();
    wait("Imported/sub/photo.jpg", b"imported photo");
    // Descendant watches must have been installed for the imported tree.
    std::fs::write(
        r._dir.path().join("Home/Imported/sub/next.jpg"),
        b"next photo",
    )
    .unwrap();
    wait("Imported/sub/next.jpg", b"next photo");
    stop.store(true, Ordering::Relaxed);
    tx.send(EngineMsg::Stop).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Arc::strong_count(&r.mirror) > 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(Arc::strong_count(&r.mirror), 1, "engine stopped");
}
