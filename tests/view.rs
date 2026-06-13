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
        size: None,
        checksum: None,
    }
}

fn note(id: u64, title: &str, content: &str) -> Document {
    doc(
        id,
        "data/abstraction/note",
        json!({ "title": title, "content": content }),
    )
}

fn tab(id: u64, title: &str, url: &str) -> Document {
    doc(
        id,
        "data/abstraction/tab",
        json!({ "title": title, "url": url }),
    )
}

fn file(id: u64, location: &str, size: Option<u64>, checksum: &str) -> Document {
    let mut d = doc(id, "data/abstraction/file", json!({}));
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

fn schema_dir_ino(tree: &Tree, ctx_id: &str, dir: &str) -> u64 {
    let ctx_ino = tree.context_ino(ctx_id).unwrap();
    tree.lookup(ctx_ino, dir).unwrap().ino
}

#[test]
fn skeleton_dirs_always_present() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/work")]);
    tree.apply_documents("work", &[], &names);

    let ctx_ino = tree.context_ino("work").unwrap();
    let entries = names_in(&tree, ctx_ino);
    for dir in [
        "Tabs", "Notes", "Todos", "Files", "Emails", "Links", "Other",
    ] {
        assert!(entries.contains(&dir.to_string()), "missing {dir}");
    }
    assert!(entries.contains(&".context.json".to_string()));
}

#[test]
fn title_collisions_get_id_suffix_and_stick() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/work")]);

    let docs = vec![note(1, "Meeting", "a"), note(2, "Meeting", "b")];
    tree.apply_documents("work", &docs, &names);

    let notes_ino = schema_dir_ino(&tree, "work", "Notes");
    assert_eq!(
        names_in(&tree, notes_ino),
        vec!["Meeting.2.md", "Meeting.md"]
    );

    // Doc 1 leaves; doc 2 must NOT inherit the clean name (sticky map)
    tree.apply_documents("work", &[note(2, "Meeting", "b")], &names);
    assert_eq!(names_in(&tree, notes_ino), vec!["Meeting.2.md"]);
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
    let tabs_ino = schema_dir_ino(&tree, "work", "Tabs");
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
        names_in(&tree, tabs_ino),
        vec!["Docs.url", "Other-ticket.url"]
    );
    // surviving doc keeps its inode → open handles stay valid
    assert_eq!(
        tree.lookup(tabs_ino, "Docs.url").unwrap().ino,
        shared_tab_ino
    );
    // removals are reported for inotify push
    let removed: Vec<&str> = inv.removed.iter().map(|(_, _, n)| n.as_str()).collect();
    assert!(removed.contains(&"Jira-ticket.url"));
    assert!(removed.contains(&"Standup.md"));
}

#[test]
fn content_change_reports_inode_invalidation() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/w")]);
    tree.apply_documents("work", &[note(1, "Plan", "v1")], &names);

    let notes_ino = schema_dir_ino(&tree, "work", "Notes");
    let ino = tree.lookup(notes_ino, "Plan.md").unwrap().ino;

    let inv = tree.apply_documents("work", &[note(1, "Plan", "v2 updated")], &names);
    assert!(inv.changed.contains(&ino));
    let node = tree.lookup(notes_ino, "Plan.md").unwrap();
    assert_eq!(inline_bytes(&node.content), b"v2 updated\n");
    assert_eq!(node.size(), 11);
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

    let files_ino = schema_dir_ino(&tree, "work", "Files");
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
            assert_eq!(*size, 123456);
            assert_eq!(checksum.as_deref(), Some("sha256/abc123"));
        }
        other => panic!("expected remote content, got {other:?}"),
    }
}

#[test]
fn file_doc_without_size_degrades_to_metadata_json() {
    let (_tmp, names) = store();
    let mut tree = Tree::new();
    tree.apply_contexts(&[ctx("work", "/w")]);

    let f = file(8, "file://{WORKSPACE_ROOT}/notes.txt", None, "sha256/def");
    tree.apply_documents("work", &[f], &names);

    let files_ino = schema_dir_ino(&tree, "work", "Files");
    let node = tree
        .lookup(files_ino, "notes.txt.json")
        .expect("json fallback");
    assert!(matches!(node.content, NodeContent::Inline(_)));
}

#[test]
fn context_rooted_mount_puts_schema_dirs_at_root() {
    let (_tmp, names) = store();
    let mut tree = Tree::context_rooted("mbag".to_string());
    tree.apply_contexts(&[ctx("mbag", "/work")]);
    tree.apply_documents("mbag", &[note(1, "Hello", "hi")], &names);

    // The context's schema dirs hang directly off ROOT — no "Contexts" wrapper.
    let root_entries = names_in(&tree, ROOT_INO);
    assert!(root_entries.contains(&"Notes".to_string()));
    assert!(root_entries.contains(&"Tabs".to_string()));
    assert!(root_entries.contains(&".context.json".to_string()));
    assert!(!root_entries.contains(&"Contexts".to_string()));

    // The context dir IS root.
    assert_eq!(tree.context_ino("mbag"), Some(ROOT_INO));

    // The note materializes under the root-level Notes dir, and the write path
    // still classifies it as belonging to the rooted context.
    let notes_ino = tree.lookup(ROOT_INO, "Notes").unwrap().ino;
    assert_eq!(names_in(&tree, notes_ino), vec!["Hello.md"]);
    assert_eq!(
        tree.locate_schema_dir(notes_ino),
        Some(("mbag".to_string(), "Notes".to_string()))
    );
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
