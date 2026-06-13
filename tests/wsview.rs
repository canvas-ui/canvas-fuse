use canvas_fuse::api::{Document, TreeInfo};
use canvas_fuse::state::{NodeContent, Tree, ROOT_INO};
use serde_json::json;
use std::time::SystemTime;

fn ti(id: &str, name: &str, tree_type: &str) -> TreeInfo {
    TreeInfo {
        id: id.to_string(),
        name: name.to_string(),
        tree_type: tree_type.to_string(),
    }
}

fn note(id: u64, title: &str, content: &str) -> Document {
    Document {
        id,
        schema: "data/abstraction/note".to_string(),
        data: json!({ "title": title, "content": content }),
        updated_at: SystemTime::UNIX_EPOCH,
        locations: Vec::new(),
        size: None,
        checksum: None,
    }
}

fn names_in(tree: &Tree, ino: u64) -> Vec<String> {
    let mut v: Vec<String> = tree
        .list(ino)
        .unwrap()
        .iter()
        .map(|n| n.name.clone())
        .collect();
    v.sort();
    v
}

/// Resolve an ino by walking names from ROOT (path segments slash-separated).
fn ino_at(tree: &Tree, path: &[&str]) -> u64 {
    let mut ino = ROOT_INO;
    for seg in path {
        ino = tree
            .lookup(ino, seg)
            .unwrap_or_else(|| panic!("missing {seg}"))
            .ino;
    }
    ino
}

#[test]
fn workspace_mount_builds_tree_dirs() {
    let mut tree = Tree::workspace_rooted("ws-1".to_string(), "myws".to_string());
    assert!(tree.is_workspace());
    tree.apply_trees(&[
        ti("t-ctx", "tree", "context"),
        ti("t-dir", "directory", "directory"),
    ]);
    assert_eq!(names_in(&tree, ROOT_INO), vec!["directory", "tree"]);
    assert_eq!(tree.ws_id().as_deref(), Some("ws-1"));
    assert_eq!(
        tree.ws_tree_meta("tree"),
        Some(("t-ctx".to_string(), "context".to_string()))
    );
}

#[test]
fn paths_materialize_nested_dirs_and_prune() {
    let mut tree = Tree::workspace_rooted("ws-1".to_string(), "myws".to_string());
    tree.apply_trees(&[ti("t-dir", "directory", "directory")]);
    tree.apply_tree_paths(
        "directory",
        &[
            "/".to_string(),
            "/foo".to_string(),
            "/foo/bar".to_string(),
            "/baz".to_string(),
        ],
    );
    // Nested dirs exist.
    let _bar = ino_at(&tree, &["directory", "foo", "bar"]);
    assert_eq!(
        names_in(&tree, ino_at(&tree, &["directory"])),
        vec!["baz", "foo"]
    );

    // Drop /baz and /foo/bar — they disappear, /foo stays.
    tree.apply_tree_paths("directory", &["/".to_string(), "/foo".to_string()]);
    assert_eq!(names_in(&tree, ino_at(&tree, &["directory"])), vec!["foo"]);
    assert!(names_in(&tree, ino_at(&tree, &["directory", "foo"])).is_empty());
}

#[test]
fn documents_materialize_as_flat_files() {
    let mut tree = Tree::workspace_rooted("ws-1".to_string(), "myws".to_string());
    tree.apply_trees(&[ti("t-dir", "directory", "directory")]);
    tree.apply_tree_paths("directory", &["/".to_string(), "/foo".to_string()]);
    tree.apply_tree_documents(
        "directory",
        "/foo",
        &[note(7, "Hello World", "# Hello World\n\nbody")],
    );

    let foo = ino_at(&tree, &["directory", "foo"]);
    let files = names_in(&tree, foo);
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with(".md"), "got {files:?}");

    // The note file classifies back to its (tree, path, doc) for the write path.
    let file_ino = tree.lookup(foo, &files[0]).unwrap().ino;
    let (tree_name, _id, ttype, path, doc_id) = tree.tree_file(file_ino).unwrap();
    assert_eq!(
        (tree_name.as_str(), ttype.as_str(), path.as_str(), doc_id),
        ("directory", "directory", "/foo", 7)
    );

    // Content is the note body, rendered inline.
    let node = tree.get(file_ino).unwrap();
    match &node.content {
        NodeContent::Inline(b) => assert!(b.starts_with(b"# Hello World")),
        other => panic!("expected inline, got {other:?}"),
    }
}

#[test]
fn dir_target_classification_and_rename_reindex() {
    let mut tree = Tree::workspace_rooted("ws-1".to_string(), "myws".to_string());
    tree.apply_trees(&[ti("t-dir", "directory", "directory")]);
    tree.apply_tree_paths(
        "directory",
        &["/".to_string(), "/foo".to_string(), "/foo/bar".to_string()],
    );
    tree.apply_tree_documents("directory", "/foo/bar", &[note(3, "Deep", "x")]);

    let foo = ino_at(&tree, &["directory", "foo"]);
    let (tn, _id, tt, path) = tree.locate_tree_dir(foo).unwrap();
    assert_eq!(
        (tn.as_str(), tt.as_str(), path.as_str()),
        ("directory", "directory", "/foo")
    );

    // Rename /foo -> /renamed: path maps and the nested doc's path follow.
    tree.rename_tree_path(foo, "renamed", "directory");
    let renamed = ino_at(&tree, &["directory", "renamed"]);
    let (_tn, _id, _tt, new_path) = tree.locate_tree_dir(renamed).unwrap();
    assert_eq!(new_path, "/renamed");
    // The deep doc file is now under /renamed/bar.
    let deep_ino = ino_at(&tree, &["directory", "renamed", "bar"]);
    let bar_files = names_in(&tree, deep_ino);
    let deep_file = tree.lookup(deep_ino, &bar_files[0]).unwrap().ino;
    let (_n, _i, _t, p, _d) = tree.tree_file(deep_file).unwrap();
    assert_eq!(p, "/renamed/bar");
}
