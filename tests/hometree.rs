//! `Tree` Home nodes fed by key (mirror mode): directories implied by keys,
//! explicit empty dirs, upserts, removals with pruning, renames, snapshots.

use canvas_fuse::state::{NodeContent, Tree, HOME_INO};
use std::collections::HashSet;
use std::time::SystemTime;

fn tree() -> Tree {
    let mut t = Tree::workspace_rooted("ws1".into(), "ws1".into());
    t.set_home_mirrored(true);
    t
}

fn names(t: &Tree, ino: u64) -> Vec<String> {
    let mut v: Vec<String> = t
        .list(ino)
        .unwrap()
        .iter()
        .map(|n| n.name.clone())
        .collect();
    v.sort();
    v
}

#[test]
fn upsert_creates_the_directories_a_key_passes_through() {
    let mut t = tree();
    let inv = t.upsert_home_key("Docs/2026/plan.md", 12, SystemTime::UNIX_EPOCH);
    assert_eq!(inv.added.len(), 3, "Docs, 2026, plan.md");
    let docs = t.home_ino_for_key("Docs").unwrap();
    assert_eq!(names(&t, HOME_INO), vec!["Docs"]);
    assert_eq!(names(&t, docs), vec!["2026"]);
    let file = t.home_ino_for_key("Docs/2026/plan.md").unwrap();
    let node = t.get(file).unwrap();
    assert_eq!(
        node.content,
        NodeContent::HomeFile {
            path: "/Docs/2026/plan.md".into(),
            size: 12
        }
    );
    assert_eq!(t.home_key(file).as_deref(), Some("Docs/2026/plan.md"));
    // Every implied directory reads as listed: nothing to fetch.
    assert!(t.home_path(docs).unwrap().1);

    // Same key again with a new size: a change, not a new node.
    let inv = t.upsert_home_key("Docs/2026/plan.md", 20, SystemTime::now());
    assert_eq!(inv.changed, vec![file]);
    assert!(inv.added.is_empty());
    assert_eq!(t.get(file).unwrap().size(), 20);
}

#[test]
fn remove_prunes_empty_parents_but_keeps_explicit_dirs() {
    let mut t = tree();
    t.upsert_home_key("A/B/c.txt", 1, SystemTime::UNIX_EPOCH);
    t.upsert_home_key("A/d.txt", 1, SystemTime::UNIX_EPOCH);
    let keep: HashSet<String> = HashSet::new();
    let inv = t.remove_home_key("A/B/c.txt", &keep);
    // c.txt and the now-empty B go; A still holds d.txt.
    assert_eq!(inv.removed.len(), 2);
    assert!(t.home_ino_for_key("A/B").is_none());
    assert!(t.home_ino_for_key("A/d.txt").is_some());

    t.upsert_home_key("A/B/c.txt", 1, SystemTime::UNIX_EPOCH);
    let keep: HashSet<String> = ["A/B".to_string()].into_iter().collect();
    t.remove_home_key("A/B/c.txt", &keep);
    assert!(t.home_ino_for_key("A/B").is_some(), "explicit dir survives");

    // Removing a directory key takes its subtree.
    t.remove_home_key("A", &HashSet::new());
    assert!(t.home_ino_for_key("A").is_none());
    assert_eq!(names(&t, HOME_INO), Vec::<String>::new());
}

#[test]
fn rename_by_key_and_by_ino_rekey_the_subtree() {
    let mut t = tree();
    t.upsert_home_key("old/x.txt", 1, SystemTime::UNIX_EPOCH);
    t.upsert_home_key("old/sub/y.txt", 1, SystemTime::UNIX_EPOCH);
    let old = t.home_ino_for_key("old").unwrap();
    let y = t.home_ino_for_key("old/sub/y.txt").unwrap();

    // Hub-side: key to key, into a directory that does not exist yet.
    let inv = t.rename_home_key("old", "moved/new");
    assert!(inv.added.contains(&old));
    assert_eq!(
        t.home_ino_for_key("moved/new").unwrap(),
        old,
        "ino survives"
    );
    assert_eq!(t.home_ino_for_key("moved/new/sub/y.txt").unwrap(), y);
    assert_eq!(t.home_key(y).as_deref(), Some("moved/new/sub/y.txt"));
    assert!(t.home_ino_for_key("old").is_none());

    // Write-path: ino to parent+name.
    let moved = t.home_ino_for_key("moved").unwrap();
    t.rename_home(y, moved, "y2.txt");
    assert_eq!(t.home_key(y).as_deref(), Some("moved/y2.txt"));
    assert!(t.home_ino_for_key("moved/new/sub/y.txt").is_none());
    let (path, _) = t.home_path(y).unwrap();
    assert_eq!(path, "/moved/y2.txt");
}

#[test]
fn snapshot_replaces_the_tree_and_keeps_surviving_inos() {
    let mut t = tree();
    t.upsert_home_key("keep.txt", 1, SystemTime::UNIX_EPOCH);
    t.upsert_home_key("gone/z.txt", 1, SystemTime::UNIX_EPOCH);
    let keep = t.home_ino_for_key("keep.txt").unwrap();

    let files = vec![
        ("keep.txt".to_string(), 5, SystemTime::UNIX_EPOCH),
        ("new/n.txt".to_string(), 1, SystemTime::UNIX_EPOCH),
    ];
    let dirs = vec!["Empty".to_string()];
    let inv = t.apply_home_snapshot(&files, &dirs);
    assert_eq!(t.home_ino_for_key("keep.txt").unwrap(), keep);
    assert_eq!(t.get(keep).unwrap().size(), 5);
    assert!(inv.changed.contains(&keep));
    assert!(t.home_ino_for_key("gone").is_none());
    assert!(t.home_ino_for_key("new/n.txt").is_some());
    assert!(t.home_ino_for_key("Empty").is_some());
    assert_eq!(names(&t, HOME_INO), vec!["Empty", "keep.txt", "new"]);
}

#[test]
fn home_only_mount_supports_lazy_listing_and_mirror_operations_at_root() {
    use canvas_fuse::api::HomeEntry;
    use canvas_fuse::state::ROOT_INO;
    let mut t = Tree::workspace_selected(
        "ws1".into(),
        "ws1".into(),
        canvas_fuse::WorkspaceSelection {
            trees: vec![],
            home: true,
            ..Default::default()
        },
    );
    assert_eq!(t.home_root_ino(), ROOT_INO);
    assert!(names(&t, ROOT_INO).is_empty());
    assert_eq!(t.home_path(ROOT_INO), Some(("/".into(), false)));
    t.apply_home_entries(
        ROOT_INO,
        &[HomeEntry {
            name: "hello.txt".into(),
            is_dir: false,
            size: 5,
            mtime: None,
        }],
    );
    assert_eq!(names(&t, ROOT_INO), vec!["hello.txt"]);
    assert_eq!(t.home_key(ROOT_INO).as_deref(), Some(""));
    t.set_home_mirrored(true);
    t.apply_home_snapshot(&[("Docs/a.txt".into(), 3, SystemTime::UNIX_EPOCH)], &[]);
    assert_eq!(names(&t, ROOT_INO), vec!["Docs"]);
    t.rename_home_key("Docs/a.txt", "Other/b.txt");
    assert!(t.home_ino_for_key("Other/b.txt").is_some());
    t.remove_home_key("Other/b.txt", &HashSet::new());
    t.apply_home_snapshot(&[], &[]);
    assert!(names(&t, ROOT_INO).is_empty());
    assert!(t.get(ROOT_INO).is_some());
}
