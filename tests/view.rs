use canvas_fuse::api::{ContextInfo, Document};
use canvas_fuse::names::NameStore;
use canvas_fuse::state::{NodeContent, Tree, CONTEXTS_INO, ROOT_INO};
use serde_json::json;
use std::time::SystemTime;

fn ctx(id: &str, url: &str) -> ContextInfo {
    ContextInfo {
        id: id.to_string(),
        url: url.to_string(),
        workspace_id: Some("ws-test".to_string()),
        raw: json!({ "id": id, "url": url, "workspaceId": "ws-test" }),
    }
}

fn doc(id: u64, schema: &str, data: serde_json::Value) -> Document {
    Document {
        id,
        schema: schema.to_string(),
        data,
        updated_at: SystemTime::UNIX_EPOCH,
        locations: Vec::new(),
        display_name: None,
        size: None,
        checksum: None,
        raw: None,
    }
}

fn note(id: u64, title: &str, content: &str) -> Document {
    doc(
        id,
        "data/schema/note",
        json!({ "title": title, "content": content }),
    )
}

fn tab(id: u64, title: &str, url: &str) -> Document {
    doc(id, "data/schema/tab", json!({ "title": title, "url": url }))
}

fn file(id: u64, location: &str, size: Option<u64>, checksum: &str) -> Document {
    let mut d = doc(id, "data/schema/file", json!({}));
    d.locations = vec![location.to_string()];
    d.size = size;
    d.checksum = Some(checksum.to_string());
    d
}

fn inline_bytes(content: &NodeContent) -> &[u8] {
    match content {
        NodeContent::Inline(b) => b.as_slice(),
        other => panic!("expected inline content, got {other:?}"),
    }
}

fn store() -> (tempfile::TempDir, NameStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = NameStore::open(&dir.path().join("names.redb")).unwrap();
    (dir, store)
}

fn names_in(tree: &Tree, ino: u64) -> Vec<String> {
    tree.list(ino)
        .unwrap()
        .iter()
        .map(|n| n.name.clone())
        .collect()
}

/// Where a context's documents live: the context folder itself, flat.
fn docs_ino(tree: &Tree, ctx_id: &str) -> u64 {
    tree.context_ino(ctx_id).unwrap()
}

/// The derived, read-only grouping.
fn by_schema_ino(tree: &Tree, ctx_id: &str, dir: &str) -> Option<u64> {
    let by = tree
        .lookup(tree.context_ino(ctx_id).unwrap(), ".by-schema")?
        .ino;
    tree.lookup(by, dir).map(|n| n.ino)
}

/// Document files only — `.context.json` and `.by-schema` are furniture.
fn doc_names(tree: &Tree, ctx_id: &str) -> Vec<String> {
    let mut v: Vec<String> = names_in(tree, docs_ino(tree, ctx_id))
        .into_iter()
        .filter(|n| n != ".context.json" && n != ".by-schema")
        .collect();
    v.sort();
    v
}

/// A context is FLAT: its documents are its files. The only furniture is the
/// meta file and the derived `.by-schema/` grouping — no schema skeleton, so
/// nothing infers meaning from which folder you are standing in.
#[test]
fn context_folder_is_flat() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/work")]);
    tree.apply_documents("work", &[], &names);

    let entries = names_in(&tree, tree.context_ino("work").unwrap());
    assert!(entries.contains(&".context.json".to_string()));
    assert!(entries.contains(&".by-schema".to_string()));
    for gone in [
        "Tabs", "Notes", "Todos", "Files", "Emails", "Links", "Other",
    ] {
        assert!(
            !entries.contains(&gone.to_string()),
            "{gone} should not be a folder"
        );
    }
}

/// The same documents, grouped — derived and read-only.
#[test]
fn by_schema_groups_the_same_documents() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/work")]);
    tree.apply_documents(
        "work",
        &[
            note(1, "Idea", "body"),
            tab(2, "Rust", "https://rust-lang.org"),
        ],
        &names,
    );

    assert_eq!(doc_names(&tree, "work"), vec!["Idea.note.md", "Rust.url"]);
    assert_eq!(
        names_in(
            &tree,
            by_schema_ino(&tree, "work", "Notes").expect("Notes group")
        ),
        vec!["Idea.note.md"]
    );
    assert_eq!(
        names_in(
            &tree,
            by_schema_ino(&tree, "work", "Tabs").expect("Tabs group")
        ),
        vec!["Rust.url"]
    );
    // Empty groups are not materialized — there is nothing to look at.
    assert!(by_schema_ino(&tree, "work", "Emails").is_none());
}

#[test]
fn title_collisions_get_id_suffix_and_stick() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/work")]);

    let docs = vec![note(1, "Meeting", "a"), note(2, "Meeting", "b")];
    tree.apply_documents("work", &docs, &names);

    assert_eq!(
        doc_names(&tree, "work"),
        vec!["Meeting.note.md", "Meeting.note_2.md"]
    );

    // Doc 1 leaves; doc 2 must NOT inherit the clean name (sticky map)
    tree.apply_documents("work", &[note(2, "Meeting", "b")], &names);
    assert_eq!(doc_names(&tree, "work"), vec!["Meeting.note_2.md"]);
}

#[test]
fn context_switch_diffs_and_keeps_inodes_stable() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/work/jira-1234")]);

    // view of jira-1234: two tabs, one note
    tree.apply_documents(
        "work",
        &[
            tab(10, "Jira ticket", "https://jira/1234"),
            tab(11, "Docs", "https://docs"),
            note(12, "Standup", "notes"),
        ],
        &names,
    );
    let tabs_ino = docs_ino(&tree, "work");
    let shared_tab_ino = tree.lookup(tabs_ino, "Docs.url").unwrap().ino;

    // switch to jira-3333: Docs tab survives, ticket tab replaced, note gone
    let inv = tree.apply_documents(
        "work",
        &[
            tab(11, "Docs", "https://docs"),
            tab(20, "Other ticket", "https://jira/3333"),
        ],
        &names,
    );

    assert_eq!(
        doc_names(&tree, "work"),
        vec!["Docs.url", "Other ticket.url"]
    );
    // surviving doc keeps its inode → open handles stay valid
    assert_eq!(
        tree.lookup(tabs_ino, "Docs.url").unwrap().ino,
        shared_tab_ino
    );
    // removals are reported for inotify push
    let removed: Vec<&str> = inv.removed.iter().map(|(_, _, n)| n.as_str()).collect();
    assert!(removed.contains(&"Jira ticket.url"));
    assert!(removed.contains(&"Standup.note.md"));
}

#[test]
fn content_change_reports_inode_invalidation() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/w")]);
    tree.apply_documents("work", &[note(1, "Plan", "v1")], &names);

    let notes_ino = docs_ino(&tree, "work");
    let ino = tree.lookup(notes_ino, "Plan.note.md").unwrap().ino;

    let inv = tree.apply_documents("work", &[note(1, "Plan", "v2 updated")], &names);
    assert!(inv.changed.contains(&ino));
    let node = tree.lookup(notes_ino, "Plan.note.md").unwrap();
    // The note IS its content — served verbatim, no trailing newline invented.
    assert_eq!(inline_bytes(&node.content), b"v2 updated");
    assert_eq!(node.size(), 10);
}

#[test]
fn removed_context_disappears() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("a", "/a"), ctx("b", "/b")]);
    tree.apply_documents("a", &[note(1, "n", "c")], &names);

    let (inv, _) = tree.apply_contexts(&[ctx("b", "/b")]);
    assert!(tree.context_ino("a").is_none());
    assert!(!inv.removed.is_empty());
    assert_eq!(names_in(&tree, CONTEXTS_INO), vec!["b".to_string()]);
}

#[test]
fn url_switch_updates_context_meta() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/work/jira-1234")]);
    tree.apply_documents("work", &[], &names);

    let inv = tree.update_context_meta(&ctx("work", "/work/jira-3333"));
    assert_eq!(inv.changed.len(), 1);

    let ctx_ino = tree.context_ino("work").unwrap();
    let meta = tree.lookup(ctx_ino, ".context.json").unwrap();
    let body = String::from_utf8(inline_bytes(&meta.content).to_vec()).unwrap();
    assert!(body.contains("jira-3333"));
}

#[test]
fn file_docs_use_location_basename_and_remote_content() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/w")]);

    let f = file(
        7,
        "file://{WORKSPACE_ROOT}/reports/Q2%20Report.pdf",
        Some(123456),
        "sha256/abc123",
    );
    tree.apply_documents("work", &[f], &names);

    let files_ino = docs_ino(&tree, "work");
    let node = tree.lookup(files_ino, "Q2 Report.pdf").expect("file entry");
    assert_eq!(node.size(), 123456);
    match &node.content {
        NodeContent::Remote {
            workspace_id,
            doc_id,
            size,
            checksum,
        } => {
            assert_eq!(workspace_id, "ws-test");
            assert_eq!(*doc_id, 7);
            assert_eq!(*size, Some(123456));
            assert_eq!(checksum.as_deref(), Some("sha256/abc123"));
        }
        other => panic!("expected remote content, got {other:?}"),
    }
}

#[test]
fn file_doc_without_size_shown_as_is_size_unresolved() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/w")]);

    let f = file(8, "file://{WORKSPACE_ROOT}/notes.txt", None, "sha256/def");
    tree.apply_documents("work", &[f], &names);

    // Shown as the real file (not a .json stub) with size unresolved (None) —
    // fsimpl resolves it from the blob on first stat.
    let files_ino = docs_ino(&tree, "work");
    let node = tree.lookup(files_ino, "notes.txt").expect("file entry");
    assert!(matches!(
        node.content,
        NodeContent::Remote { size: None, .. }
    ));
}

#[test]
fn context_rooted_mount_puts_the_documents_at_root() {
    let (_tmp, names) = store();
    let mut tree = Tree::context_rooted("mbag".to_string());
    tree.apply_contexts(&[ctx("mbag", "/work")]);
    tree.apply_documents("mbag", &[note(1, "Hello", "hi")], &names);

    // Rooted at one context: its files hang directly off ROOT — no "Contexts"
    // wrapper, and no schema skeleton.
    let root_entries = names_in(&tree, ROOT_INO);
    assert!(root_entries.contains(&"Hello.note.md".to_string()));
    assert!(root_entries.contains(&".context.json".to_string()));
    assert!(root_entries.contains(&".by-schema".to_string()));
    assert!(!root_entries.contains(&"Contexts".to_string()));
    assert!(!root_entries.contains(&"Notes".to_string()));

    // The context dir IS root.
    assert_eq!(tree.context_ino("mbag"), Some(ROOT_INO));

    // The write path still classifies a file here as belonging to the context.
    let file_ino = tree.lookup(ROOT_INO, "Hello.note.md").unwrap().ino;
    assert_eq!(tree.doc_for_ino(file_ino), Some(("mbag".to_string(), 1)));
    assert_eq!(tree.locate_context_dir(ROOT_INO), Some("mbag".to_string()));
}

#[test]
fn global_mount_keeps_contexts_wrapper() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("a", "/a"), ctx("b", "/b")]);
    tree.apply_documents("a", &[], &names);

    // Global mount: root holds "Contexts", which holds the per-context dirs.
    assert_eq!(names_in(&tree, ROOT_INO), vec!["Contexts".to_string()]);
    let mut ctxs = names_in(&tree, CONTEXTS_INO);
    ctxs.sort();
    assert_eq!(ctxs, vec!["a".to_string(), "b".to_string()]);
}

/// A document created through the mount is in the flat view before the server
/// confirms it — the write path adopts it there. The grouping has to follow it,
/// and keying the rebuild off "did the flat directory gain a node" meant it
/// never did: the node was already present, so nothing looked dirty.
#[test]
fn by_schema_follows_a_document_the_flat_view_already_had() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/work")]);
    tree.apply_documents("work", &[note(1, "Plan", "a")], &names);

    // A local create: the file exists on the mount before the refresh sees it.
    let docs_ino = docs_ino(&tree, "work");
    let new_tab = tab(2, "Reddit", "https://reddit.com");
    tree.adopt_document(
        docs_ino,
        "Reddit.url",
        "work",
        2,
        9_999,
        std::sync::Arc::new(b"[InternetShortcut]\nURL=https://reddit.com\n".to_vec()),
    );

    // The refresh now returns it too, and finds the flat view already correct.
    tree.apply_documents("work", &[note(1, "Plan", "a"), new_tab], &names);

    let tabs = by_schema_ino(&tree, "work", "Tabs").expect("Tabs group");
    assert!(tree.lookup(tabs, "Reddit.url").is_some());
}

/// POSIX updates a directory's mtime on every link and unlink. This view never
/// did, so a context could swap its entire document set and still stat
/// identical — and anything that asks "has this folder changed?" by timestamp
/// (KDE's lister re-stats before re-listing, `find -newer`, backup tools)
/// concluded it had not. That is the F5 people were pressing.
#[test]
fn a_directory_mtime_follows_its_entries() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/work")]);
    tree.apply_documents("work", &[note(1, "Plan", "a")], &names);

    let dir = docs_ino(&tree, "work");
    let before = tree.get(dir).unwrap().mtime;

    // A document arrives: the folder gained an entry.
    tree.apply_documents(
        "work",
        &[note(1, "Plan", "a"), note(2, "Later", "b")],
        &names,
    );
    let after_add = tree.get(dir).unwrap().mtime;
    assert!(
        after_add > before,
        "adding an entry must move the dir mtime"
    );

    // And leaves again.
    tree.apply_documents("work", &[note(1, "Plan", "a")], &names);
    assert!(
        tree.get(dir).unwrap().mtime > after_add,
        "removing an entry must move the dir mtime"
    );

    // A content edit changes no entry, so the directory is untouched — the
    // file's own mtime carries that, and churning the dir would have every
    // watcher re-listing on every keystroke.
    let steady = tree.get(dir).unwrap().mtime;
    tree.apply_documents("work", &[note(1, "Plan", "edited")], &names);
    assert_eq!(tree.get(dir).unwrap().mtime, steady);
}
