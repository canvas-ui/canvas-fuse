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
        schema: "data/schema/note".to_string(),
        data: json!({ "title": title, "content": content }),
        updated_at: SystemTime::UNIX_EPOCH,
        locations: Vec::new(),
        display_name: None,
        size: None,
        checksum: None,
        raw: None,
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
    // One mount, the same roots the WebDAV view exposes.
    assert_eq!(names_in(&tree, ROOT_INO), vec!["Home", "Trash", "Trees"]);
    assert_eq!(
        names_in(&tree, ino_at(&tree, &["Trees"])),
        vec!["directory", "tree"]
    );
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
    let _bar = ino_at(&tree, &["Trees", "directory", "foo", "bar"]);
    assert_eq!(
        names_in(&tree, ino_at(&tree, &["Trees", "directory"])),
        vec!["baz", "foo"]
    );

    // Drop /baz and /foo/bar — they disappear, /foo stays.
    tree.apply_tree_paths("directory", &["/".to_string(), "/foo".to_string()]);
    assert_eq!(
        names_in(&tree, ino_at(&tree, &["Trees", "directory"])),
        vec!["foo"]
    );
    assert!(names_in(&tree, ino_at(&tree, &["Trees", "directory", "foo"])).is_empty());
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

    let foo = ino_at(&tree, &["Trees", "directory", "foo"]);
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

    let foo = ino_at(&tree, &["Trees", "directory", "foo"]);
    let (tn, _id, tt, path) = tree.locate_tree_dir(foo).unwrap();
    assert_eq!(
        (tn.as_str(), tt.as_str(), path.as_str()),
        ("directory", "directory", "/foo")
    );

    // Rename /foo -> /renamed: path maps and the nested doc's path follow.
    tree.rename_tree_path(foo, "renamed", "directory");
    let renamed = ino_at(&tree, &["Trees", "directory", "renamed"]);
    let (_tn, _id, _tt, new_path) = tree.locate_tree_dir(renamed).unwrap();
    assert_eq!(new_path, "/renamed");
    // The deep doc file is now under /renamed/bar.
    let deep_ino = ino_at(&tree, &["Trees", "directory", "renamed", "bar"]);
    let bar_files = names_in(&tree, deep_ino);
    let deep_file = tree.lookup(deep_ino, &bar_files[0]).unwrap().ino;
    let (_n, _i, _t, p, _d) = tree.tree_file(deep_file).unwrap();
    assert_eq!(p, "/renamed/bar");
}

/// A cross-directory move is a re-tag, so the local view has to follow the
/// document to its new folder — with its path index updated, not just its
/// parent pointer. Before this, `mv` between folders returned EXDEV and the
/// kernel fell back to copy+unlink, streaming every byte through the mount.
#[test]
fn moving_a_file_between_dirs_reindexes_its_path() {
    let mut tree = Tree::workspace_rooted("ws-1".to_string(), "myws".to_string());
    tree.apply_trees(&[ti("t-dir", "directory", "directory")]);
    tree.apply_tree_paths(
        "directory",
        &["/".to_string(), "/src".to_string(), "/dst".to_string()],
    );
    tree.apply_tree_documents("directory", "/src", &[note(7, "Moving Note", "body")]);

    let src = ino_at(&tree, &["Trees", "directory", "src"]);
    let dst = ino_at(&tree, &["Trees", "directory", "dst"]);
    let name = names_in(&tree, src)[0].clone();
    let file_ino = tree.lookup(src, &name).unwrap().ino;

    tree.move_tree_file(file_ino, dst, "renamed.md", "directory", "directory");

    assert!(names_in(&tree, src).is_empty(), "source folder is empty");
    assert_eq!(names_in(&tree, dst), vec!["renamed.md"]);

    // The write path resolves a file through tree_file(); a stale path there
    // would send the next edit to the folder the document just left.
    let (tree_name, _id, _ttype, path, doc_id) = tree.tree_file(file_ino).unwrap();
    assert_eq!(tree_name, "directory");
    assert_eq!(path, "/dst");
    assert_eq!(doc_id, 7);
    assert_eq!(tree.ws_ino_for_doc("directory", "/dst", 7), Some(file_ino));
}

/// Folder moves are tree operations: the node moves and everything filed under
/// it comes along, with every descendant path reindexed.
#[test]
fn moving_a_folder_carries_its_documents() {
    let mut tree = Tree::workspace_rooted("ws-1".to_string(), "myws".to_string());
    tree.apply_trees(&[ti("t-dir", "directory", "directory")]);
    tree.apply_tree_paths(
        "directory",
        &[
            "/".to_string(),
            "/projects".to_string(),
            "/projects/alpha".to_string(),
            "/archive".to_string(),
        ],
    );
    tree.apply_tree_documents("directory", "/projects/alpha", &[note(9, "Inside", "body")]);

    let alpha = ino_at(&tree, &["Trees", "directory", "projects", "alpha"]);
    let archive = ino_at(&tree, &["Trees", "directory", "archive"]);
    let file_ino = tree
        .lookup(alpha, &names_in(&tree, alpha)[0].clone())
        .unwrap()
        .ino;

    tree.move_tree_path_node(alpha, archive, "alpha", "directory");

    assert_eq!(names_in(&tree, archive), vec!["alpha"]);
    let (_t, _i, _tt, path, doc_id) = tree.tree_file(file_ino).unwrap();
    assert_eq!(path, "/archive/alpha", "descendant paths must be reindexed");
    assert_eq!(doc_id, 9);
}

/// The trash is a root of its own, flat, and its files still classify back to a
/// (tree, path, doc) — the write path uses that to route `rm` there to the
/// permanent delete instead of a detach that the server would simply undo.
#[test]
fn trash_materializes_as_a_flat_root() {
    let mut tree = Tree::workspace_rooted("ws-1".to_string(), "myws".to_string());
    tree.apply_trees(&[ti("t-dir", "directory", "directory")]);
    tree.apply_trash_documents(&[note(11, "Deleted Note", "body")]);

    let trash = ino_at(&tree, &["Trash"]);
    let files = names_in(&tree, trash);
    assert_eq!(files.len(), 1);

    let file_ino = tree.lookup(trash, &files[0]).unwrap().ino;
    let (tree_name, _id, _ttype, path, doc_id) = tree.tree_file(file_ino).unwrap();
    assert_eq!(tree_name, canvas_fuse::state::TRASH_TREE_NAME);
    assert_eq!(path, canvas_fuse::state::TRASH_PATH);
    assert_eq!(doc_id, 11);

    // Emptied server-side: the file disappears on the next reconcile.
    tree.apply_trash_documents(&[]);
    assert!(names_in(&tree, trash).is_empty());
}

/// Home is a real drive, so it is materialized folder by folder on demand — a
/// listing arrives for one directory at a time and its subfolders stay unloaded
/// until something looks into them.
#[test]
fn home_directories_load_lazily() {
    use canvas_fuse::api::HomeEntry;
    use canvas_fuse::state::HOME_INO;

    let entry = |name: &str, is_dir: bool, size: u64| HomeEntry {
        name: name.to_string(),
        size,
        is_dir,
        mtime: None,
    };

    let mut tree = Tree::workspace_rooted("ws-1".to_string(), "myws".to_string());
    assert_eq!(tree.home_path(HOME_INO), Some(("/".to_string(), false)));

    tree.apply_home_entries(
        HOME_INO,
        &[entry("notes.txt", false, 12), entry("projects", true, 0)],
    );

    assert_eq!(names_in(&tree, HOME_INO), vec!["notes.txt", "projects"]);
    // The root is loaded now; the subfolder is not, and knows its own path.
    assert_eq!(tree.home_path(HOME_INO), Some(("/".to_string(), true)));
    let projects = ino_at(&tree, &["Home", "projects"]);
    assert_eq!(
        tree.home_path(projects),
        Some(("/projects".to_string(), false))
    );

    // Files carry the path a range read addresses, and their size.
    let file = ino_at(&tree, &["Home", "notes.txt"]);
    assert_eq!(tree.home_file(file), Some(("/notes.txt".to_string(), 12)));

    tree.apply_home_entries(projects, &[entry("alpha.md", false, 5)]);
    assert_eq!(
        tree.home_file(ino_at(&tree, &["Home", "projects", "alpha.md"])),
        Some(("/projects/alpha.md".to_string(), 5))
    );

    // A file that disappeared from the drive disappears from the view.
    tree.apply_home_entries(HOME_INO, &[entry("projects", true, 0)]);
    assert_eq!(names_in(&tree, HOME_INO), vec!["projects"]);
    // ...and the subfolder that survived keeps what it had loaded.
    assert_eq!(
        names_in(&tree, ino_at(&tree, &["Home", "projects"])),
        vec!["alpha.md"]
    );
}

#[test]
fn path_of_resolves_mount_relative_paths() {
    let mut tree = Tree::workspace_rooted("ws-1".to_string(), "myws".to_string());
    tree.apply_trees(&[ti("t-dir", "directory", "directory")]);
    tree.apply_tree_paths(
        "directory",
        &["/".to_string(), "/foo".to_string(), "/foo/bar".to_string()],
    );

    // Root is the empty relative path; nested dirs walk parent links back up.
    assert_eq!(tree.path_of(ROOT_INO), Some(std::path::PathBuf::from("")));
    let bar = ino_at(&tree, &["Trees", "directory", "foo", "bar"]);
    assert_eq!(
        tree.path_of(bar),
        Some(std::path::PathBuf::from("Trees/directory/foo/bar"))
    );
    // Unknown inos resolve to nothing (the nudge queue then drops them).
    assert_eq!(tree.path_of(999_999), None);
}
