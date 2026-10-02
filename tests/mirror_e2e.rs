//! End-to-end mirror engine tests against an in-memory hub (`fake_hub`):
//! no FUSE mount, no real server. Each test owns a data dir and a hub.

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

fn rig_with(url: &str, opts: MirrorOptions) -> (Arc<Mirror>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mirror = Mirror::open(MirrorConfig {
        data_dir: dir.path().join("data"),
        server: url.to_string(),
        token: "token".into(),
        workspace_id: "ws1".into(),
        backend: "workspace:home".into(),
        opts,
        device: DeviceIdentity {
            id: "dev-a".into(),
            name: "laptop".into(),
        },
        mountpoint: dir.path().join("mnt"),
        status_path: Some(dir.path().join("status.json")),
    })
    .unwrap();
    (mirror, dir)
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
        cache_budget_bytes: 10 * 1024 * 1024,
        poll_secs: 30,
        ..Default::default()
    }
}

#[test]
fn initial_listing_builds_the_tree_and_materializes_pins() {
    let r = rig(MirrorOptions {
        pins: vec!["Docs/".into()],
        ..opts()
    });
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

    // Pinned bytes are on disk; unpinned ones wait for a read.
    assert!(r.mirror.cache.has(&sha_a));
    assert!(!r.mirror.cache.has(&sha_b));
    assert!(r.mirror.store.cache_meta(&sha_a).unwrap().pin_refs > 0);

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
    let r = rig(opts());
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
    // Our bytes are still in the cache, by digest, and the record says so.
    assert!(r.mirror.cache.has(&sha_hex(b"ours")));
    let conflicts = r.mirror.conflicts();
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].uploaded);
    assert_eq!(conflicts[0].inbox_doc_id, Some(4242));
    assert_eq!(r.mirror.status().conflicts, 1);
    assert_eq!(r.mirror.status().pending, 0);

    // Resolution on the hub clears the record.
    r.mirror.conflict_resolved("shared.txt");
    assert_eq!(r.mirror.status().conflicts, 0);
}

#[test]
fn conflict_via_the_feed_in_rename_mode_writes_a_conflict_copy() {
    let r = rig(MirrorOptions {
        conflicts: ConflictMode::Rename,
        ..opts()
    });
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
    // The copy comes back to us through the feed as an ordinary object.
    r.mirror.cycle(false);
    assert!(r.mirror.entry(copy).is_some());
    assert_eq!(
        r.mirror.conflicts()[0].copy_key.as_deref(),
        Some(copy.as_str())
    );
}

#[test]
fn pull_on_change_and_refetch_when_previously_cached() {
    let r = rig(opts());
    r.mirror.cycle(true);
    r.hub.lock().put("c.txt", b"one");
    r.mirror.cycle(false);
    let e = r.mirror.entry("c.txt").expect("pulled as a placeholder");
    assert_eq!(e.sha256, sha_hex(b"one"));
    assert!(!r.mirror.cache.has(&e.sha256), "unpinned: bytes on demand");

    // First read fetches.
    assert_eq!(r.mirror.bytes_for_edit("c.txt").unwrap(), b"one");
    assert!(r.mirror.cache.has(&sha_hex(b"one")));

    // A hub change to a file we had cached is fetched eagerly.
    r.hub.lock().put("c.txt", b"two");
    r.mirror.cycle(false);
    let e = r.mirror.entry("c.txt").unwrap();
    assert_eq!(e.sha256, sha_hex(b"two"));
    assert!(r.mirror.cache.has(&sha_hex(b"two")));
    assert_eq!(
        r.mirror.store.base("c.txt").unwrap().sha256,
        sha_hex(b"two")
    );
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
    let r = rig(MirrorOptions {
        pins: vec!["keep.txt".into()],
        ..opts()
    });
    r.hub.lock().put("keep.txt", b"precious");
    r.mirror.cycle(true);
    assert!(r.mirror.cache.has(&sha_hex(b"precious")));

    r.hub.lock().delete("keep.txt");
    r.mirror.cycle(false);
    assert!(r.mirror.entry("keep.txt").is_none());
    assert!(r.mirror.store.base("keep.txt").is_none());
    let trash = r.mirror.trash_list();
    assert_eq!(trash.len(), 1);
    assert_eq!(trash[0].0, "keep.txt");
    assert!(r.mirror.cache.has(&sha_hex(b"precious")), "bytes survive");

    r.mirror.trash_restore("keep.txt").unwrap();
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
    // Cached bytes are readable, uncached ones are not.
    assert_eq!(
        mirror.bytes_for_edit("offline/a.txt").unwrap(),
        b"written offline"
    );
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
    let open = || {
        Mirror::open(MirrorConfig {
            data_dir: dir.path().join("data"),
            server: hub.url.clone(),
            token: "t".into(),
            workspace_id: "ws1".into(),
            backend: "workspace:home".into(),
            opts: opts(),
            device: DeviceIdentity {
                id: "dev-a".into(),
                name: "laptop".into(),
            },
            mountpoint: dir.path().join("mnt"),
            status_path: None,
        })
        .unwrap()
    };
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
