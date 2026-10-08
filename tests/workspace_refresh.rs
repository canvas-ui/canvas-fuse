use canvas_fuse::{
    api::ApiClient, names::NameStore, state::Tree, worker::Worker, WorkspaceSelection,
};
use parking_lot::{Mutex, RwLock};
use serde_json::json;
use std::sync::Arc;
use tiny_http::{Response, Server};

struct TreeHub {
    server: Arc<Server>,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TreeHub {
    fn new() -> Self {
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let http = server.clone();
        let seen = requests.clone();
        let thread = std::thread::spawn(move || {
            while let Ok(req) = http.recv() {
                seen.lock().push(req.url().to_string());
                let payload = if req.url().ends_with("/trees") {
                    json!([
                        { "id": "t-context", "name": "context", "type": "context" },
                        { "id": "t-directory", "name": "directory", "type": "directory" },
                        { "id": "t-backends", "name": "backends", "type": "directory" }
                    ])
                } else if req.url().ends_with("/paths") {
                    json!(["/", "/inbox"])
                } else {
                    json!([])
                };
                req.respond(Response::from_string(
                    json!({ "payload": payload }).to_string(),
                ))
                .unwrap();
            }
        });
        Self {
            server,
            requests,
            thread: Some(thread),
        }
    }
}

impl Drop for TreeHub {
    fn drop(&mut self) {
        self.server.unblock();
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn bootstrap_and_refresh_only_fetch_selected_tree_paths_and_documents() {
    let hub = TreeHub::new();
    let api =
        Arc::new(ApiClient::new(&format!("http://{}", hub.server.server_addr()), "test").unwrap());
    let dir = tempfile::tempdir().unwrap();
    let names = Arc::new(NameStore::open(&dir.path().join("names.redb")).unwrap());
    for (selection, expected, trash) in [
        (
            WorkspaceSelection::default(),
            vec!["context", "directory"],
            true,
        ),
        (
            WorkspaceSelection {
                include_backends: true,
                ..Default::default()
            },
            vec!["context", "directory", "backends"],
            true,
        ),
        (
            WorkspaceSelection {
                trees: vec!["backends".into()],
                ..Default::default()
            },
            vec!["backends"],
            false,
        ),
        (
            WorkspaceSelection {
                trees: vec!["context".into()],
                ..Default::default()
            },
            vec!["context"],
            false,
        ),
        (
            WorkspaceSelection {
                home: true,
                ..Default::default()
            },
            vec![],
            false,
        ),
    ] {
        let worker = Worker {
            api: api.clone(),
            tree: Arc::new(RwLock::new(Tree::workspace_selected(
                "ws1".into(),
                "ws1".into(),
                selection,
            ))),
            names: names.clone(),
            notifier: None,
            ensure_subscribed: None,
            context_filter: None,
            context_workspace_id: None,
            refresh_lock: None,
            nudger: None,
            mirror_invalidations: None,
            mirror: None,
        };
        // Mount bootstrap and subsequent poll/socket refreshes share this path.
        for _ in 0..2 {
            hub.requests.lock().clear();
            worker.refresh_all();
            let requests = hub.requests.lock();
            for name in ["context", "directory", "backends"] {
                let included = expected.contains(&name);
                assert_eq!(worker.tree.read().ws_tree_meta(name).is_some(), included);
                assert_eq!(
                    requests
                        .iter()
                        .filter(|url| url.ends_with(&format!("/trees/t-{name}/paths")))
                        .count(),
                    usize::from(included)
                );
                assert_eq!(
                    requests
                        .iter()
                        .filter(
                            |url| url.contains(&format!("/documents?treeNameOrTreeId=t-{name}&"))
                        )
                        .count(),
                    if included { 2 } else { 0 }
                );
            }
            assert_eq!(requests.iter().any(|url| url.ends_with("/trash")), trash);
            if expected.is_empty() {
                assert!(
                    requests.is_empty(),
                    "Home-only mounts should make no document API calls"
                );
            }
        }
    }
}
