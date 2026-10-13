//! Exercise selector resolution and the real bootstrap up to the kernel mount.
use super::*;
use parking_lot::Mutex;
use serde_json::{json, Value};
use state::{NodeContent, Tree, ROOT_INO};
use std::collections::HashMap;
use tiny_http::{Response, Server};

#[derive(Default)]
struct HubState {
    contexts: Vec<Value>,
    documents: HashMap<String, Vec<Value>>,
    requests: Vec<String>,
    list_status: Option<u16>,
}

struct Hub {
    server: Arc<Server>,
    state: Arc<Mutex<HubState>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Hub {
    fn new(contexts: Vec<Value>) -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let state = Arc::new(Mutex::new(HubState {
            contexts,
            documents: HashMap::from([(
                "kt".into(),
                vec![json!({
                    "id": 42, "schema": "data/schema/note",
                    "data": {"title": "Project", "content": "context contents"}
                })],
            )]),
            ..Default::default()
        }));
        let http = server.clone();
        let data = state.clone();
        let thread = std::thread::spawn(move || {
            while let Ok(req) = http.recv() {
                let path = req.url().split('?').next().unwrap();
                let mut state = data.lock();
                state
                    .requests
                    .push(format!("{} {}", req.method(), req.url()));
                let (status, payload) = match path {
                    "/rest/v2/workspaces/mbag" => (200, json!({"id":"ws-mbag","name":"mbag"})),
                    "/rest/v2/contexts" => {
                        (state.list_status.unwrap_or(200), json!(state.contexts))
                    }
                    _ => match path.strip_prefix("/rest/v2/contexts/") {
                        Some(rest) if rest.ends_with("/documents") => {
                            let id = rest.trim_end_matches("/documents");
                            if state.contexts.iter().any(|c| c["id"] == id) {
                                (
                                    200,
                                    json!(state.documents.get(id).cloned().unwrap_or_default()),
                                )
                            } else {
                                (404, Value::Null)
                            }
                        }
                        Some(id) => state
                            .contexts
                            .iter()
                            .find(|c| c["id"] == id)
                            .map(|c| (200, c.clone()))
                            .unwrap_or((404, Value::Null)),
                        None => (404, Value::Null),
                    },
                };
                drop(state);
                req.respond(
                    Response::from_string(
                        json!({"payload":payload,"message":"test response"}).to_string(),
                    )
                    .with_status_code(status),
                )
                .unwrap();
            }
        });
        Self {
            server,
            state,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.server.server_addr())
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.server.unblock();
        self.thread.take().unwrap().join().unwrap();
    }
}

fn context(id: &str, name: &str, workspace: &str) -> Value {
    json!({"id":id,"name":name,"workspaceId":workspace,"url":"mbag://project"})
}

fn options(root: &std::path::Path, hub: &Hub, selector: &str, scoped: bool) -> MountOptions {
    MountOptions {
        server: hub.url(),
        token: "test".into(),
        tls: None,
        mountpoint: root.join("KT"),
        data_dir: root.join("state"),
        enable_ws: true,
        resync_secs: 60,
        contexts: Some(vec![selector.into()]),
        context_root: Some(selector.into()),
        contexts_at_root: false,
        workspace: None,
        selection: WorkspaceSelection::default(),
        context_workspace: scoped.then(|| "mbag".into()),
        blob_cache_bytes: 1024,
        enable_nudge: false,
        mirror: None,
    }
}

fn assert_project(tree: &Tree) {
    let file = tree
        .lookup(ROOT_INO, "Project.note.md")
        .expect("context document at mount root");
    assert_eq!(tree.context_ids(), vec!["kt".to_string()]);
    assert_eq!(tree.locate_context_dir(ROOT_INO).as_deref(), Some("kt"));
    assert_eq!(tree.doc_for_ino(file.ino), Some(("kt".into(), 42)));
    assert!(tree.lookup(ROOT_INO, ".context.json").is_some());
    match &file.content {
        NodeContent::Inline(bytes) => {
            assert!(String::from_utf8_lossy(bytes).contains("context contents"))
        }
        _ => panic!("expected inline note"),
    }
}

#[test]
fn named_and_mixed_case_contexts_populate_the_original_mountpoint() {
    for scoped in [false, true] {
        for selector in ["KT", "kt", "kT", "Kitchen Tasks", "kitchen tasks"] {
            let hub = Hub::new(vec![
                context("kt", "Kitchen Tasks", "ws-mbag"),
                context("other", "Other", "ws-other"),
            ]);
            let dir = tempfile::tempdir().unwrap();
            let mut mounted = false;
            let result =
                mount_with_session(options(dir.path(), &hub, selector, scoped), |fs, path| {
                    mounted = true;
                    assert_eq!(path, dir.path().join("KT"));
                    assert_project(&fs.test_tree().read());
                    anyhow::bail!("test stops at kernel boundary")
                });
            assert!(mounted, "{selector}: {:?}", result.err());
            let seen = &hub.state.lock().requests;
            assert!(seen
                .iter()
                .any(|r| r.starts_with("GET /rest/v2/contexts/kt/documents?")));
            assert!(!seen
                .iter()
                .any(|r| r.contains("/contexts/KT") || r.contains("/contexts/other")));
        }
    }
}

#[test]
fn context_scope_ambiguity_and_lookup_errors_are_reported_before_mounting() {
    let cases = [
        (
            vec![context("kt", "KT", "ws-other")],
            true,
            None,
            "not found",
        ),
        (
            vec![
                context("kt", "KT", "ws-mbag"),
                context("KT", "Legacy", "ws-mbag"),
            ],
            true,
            None,
            "ambiguous",
        ),
        (
            vec![
                context("first", "KT", "ws-mbag"),
                context("second", "KT", "ws-mbag"),
            ],
            true,
            None,
            "ambiguous",
        ),
        (
            vec![
                context("first", "KT", "ws-mbag"),
                context("second", "KT", "ws-other"),
            ],
            false,
            None,
            "ambiguous",
        ),
        (vec![], false, None, "not found"),
        (vec![], true, Some(401), "resolving selected contexts"),
        (vec![], true, Some(503), "resolving selected contexts"),
    ];
    for (contexts, scoped, status, expected) in cases {
        let hub = Hub::new(contexts);
        hub.state.lock().list_status = status;
        let dir = tempfile::tempdir().unwrap();
        let error = mount_with_session(options(dir.path(), &hub, "KT", scoped), |_, _| {
            panic!("invalid selection mounted")
        })
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert!(!hub
            .state
            .lock()
            .requests
            .iter()
            .any(|r| r.contains("/documents")));
    }
}

#[test]
fn workspace_scope_disambiguates_names_and_ids_take_precedence_over_labels() {
    let hub = Hub::new(vec![
        context("kt", "Target", "ws-mbag"),
        context("different", "KT", "ws-mbag"),
        context("kt", "KT", "ws-other"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let mut mounted = false;
    let _ = mount_with_session(options(dir.path(), &hub, "KT", true), |fs, _| {
        mounted = true;
        assert_project(&fs.test_tree().read());
        anyhow::bail!("test stops at kernel boundary")
    });
    assert!(mounted);
}

#[test]
fn canonical_selection_survives_refresh_rename_and_targeted_events() {
    let hub = Hub::new(vec![
        context("kt", "KT", "ws-mbag"),
        context("other", "Other", "ws-other"),
    ]);
    let api = Arc::new(api::ApiClient::new(&hub.url(), "test").unwrap());
    let selected =
        context_selection::ContextSelection::resolve(&api, Some(&["KT".into()]), Some("KT"), None)
            .unwrap();
    assert_eq!(selected.workspace_id.as_deref(), Some("ws-mbag"));
    let dir = tempfile::tempdir().unwrap();
    let subscriptions = Arc::new(Mutex::new(Vec::new()));
    let subscribed = subscriptions.clone();
    let worker = worker::Worker {
        api,
        tree: Arc::new(RwLock::new(Tree::context_rooted(selected.root.unwrap()))),
        names: Arc::new(names::NameStore::open(&dir.path().join("names.redb")).unwrap()),
        notifier: None,
        ensure_subscribed: Some(Box::new(move |id| subscribed.lock().push(id.to_string()))),
        context_filter: selected.filter,
        context_workspace_id: selected.workspace_id,
        refresh_lock: None,
        nudger: None,
        mirror_invalidations: None,
        mirror: None,
    };
    worker.refresh_all();
    assert_project(&worker.tree.read());
    hub.state.lock().contexts[0]["name"] = json!("Renamed display label");
    worker.refresh_all();
    assert_project(&worker.tree.read());
    worker.refresh_context("kt");
    assert_eq!(*subscriptions.lock(), vec!["kt", "kt", "kt"]);
    hub.state.lock().requests.clear();
    worker.refresh_context("other");
    assert!(hub.state.lock().requests.is_empty());
    // A context moved out of this workspace must not fetch new documents.
    hub.state.lock().contexts[0]["workspaceId"] = json!("ws-other");
    worker.refresh_context("kt");
    assert!(!hub
        .state
        .lock()
        .requests
        .iter()
        .any(|r| r.contains("/documents")));
    worker.refresh_all();
    assert!(worker
        .tree
        .read()
        .lookup(ROOT_INO, "Project.note.md")
        .is_none());
}

#[test]
fn multiple_selectors_deduplicate_and_cannot_span_workspaces() {
    let hub = Hub::new(vec![
        context("kt", "KT", "ws-mbag"),
        context("other", "Other", "ws-other"),
    ]);
    let api = api::ApiClient::new(&hub.url(), "test").unwrap();
    let selected = context_selection::ContextSelection::resolve(
        &api,
        Some(&["KT".into(), "kt".into()]),
        None,
        None,
    )
    .unwrap();
    assert_eq!(selected.filter.unwrap().len(), 1);
    assert!(context_selection::ContextSelection::resolve(
        &api,
        Some(&["KT".into(), "other".into()]),
        None,
        None
    )
    .is_err());
}

#[test]
fn an_existing_empty_context_still_mounts_with_its_metadata() {
    let hub = Hub::new(vec![context("kt", "KT", "ws-mbag")]);
    hub.state.lock().documents.clear();
    let dir = tempfile::tempdir().unwrap();
    let mut mounted = false;
    let _ = mount_with_session(options(dir.path(), &hub, "KT", true), |fs, _| {
        mounted = true;
        let tree = fs.test_tree().read();
        assert_eq!(tree.locate_context_dir(ROOT_INO).as_deref(), Some("kt"));
        assert!(tree.lookup(ROOT_INO, ".context.json").is_some());
        assert!(tree.lookup(ROOT_INO, "Project.note.md").is_none());
        anyhow::bail!("test stops at kernel boundary")
    });
    assert!(mounted);
}
