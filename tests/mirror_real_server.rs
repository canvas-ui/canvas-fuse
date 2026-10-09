//! Opt-in cross-repository contract test using real HTTP, Stored and SynapsD.
//! Run with the sibling canvas-server dependencies installed:
//! cargo test --test mirror_real_server -- --ignored
use canvas_fuse::mirror::sync::{Mirror, MirrorConfig};
use canvas_fuse::mirror::{DeviceIdentity, MirrorOptions};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn mirror(root: &Path, url: &str) -> Arc<Mirror> {
    Mirror::open(MirrorConfig {
        tls: None,
        data_dir: root.join("state"),
        home_dir: root.join("Home"),
        server: url.into(),
        token: "isolated-test".into(),
        workspace_id: "mirror-test".into(),
        backend: "workspace:home".into(),
        opts: MirrorOptions::default(),
        device: DeviceIdentity {
            id: "test-device".into(),
            name: "test laptop".into(),
        },
        mountpoint: root.join("Home"),
        status_path: None,
    })
    .unwrap()
}

#[test]
#[ignore = "requires Node and the sibling canvas-server dependencies"]
fn real_server_daily_workflows_and_index_lifecycle() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../canvas-server/tests/fixtures/mirror-hub.mjs");
    let ready = root.join("ready.json");
    let mut server = Server(
        Command::new("node")
            .current_dir(fixture.parent().unwrap().join("../.."))
            .arg(fixture)
            .arg(root.join("hub"))
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    while !ready.exists() {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "test server exited before becoming ready"
        );
        assert!(Instant::now() < deadline, "test server startup timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    let info: Value = serde_json::from_slice(&std::fs::read(ready).unwrap()).unwrap();
    let url = info["url"].as_str().unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        "Bearer isolated-test".parse().unwrap(),
    );
    let client = reqwest::blocking::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let objects =
        format!("{url}/rest/v2/workspaces/mirror-test/backends/file/workspace%3Ahome/objects");
    let put = |key: &str, bytes: &[u8]| -> Value {
        let response = client
            .put(format!("{objects}/{key}"))
            .header("Content-Type", "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .unwrap();
        let status = response.status();
        let body = response.text().unwrap();
        assert!(status.is_success(), "PUT {key}: {status} {body}");
        serde_json::from_str::<Value>(&body).unwrap()["payload"].clone()
    };
    let get = |key: &str| -> Vec<u8> {
        client
            .get(format!("{objects}/{key}"))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .unwrap()
            .to_vec()
    };
    put("Remote/initial.txt", b"from the server");
    let device = root.join("device");
    let m = mirror(&device, url);
    m.cycle(true);
    assert_eq!(
        m.local.read_all("Remote/initial.txt").unwrap(),
        b"from the server"
    );
    assert!(m.status().last_error.is_none(), "{:?}", m.status());

    m.commit_write("Docs/empty.txt", b"").unwrap();
    m.commit_write("Docs/draft.txt", b"first draft").unwrap();
    m.cycle(false);
    assert_eq!(get("Docs/empty.txt"), b"");
    assert_eq!(get("Docs/draft.txt"), b"first draft");
    m.commit_write("editor.tmp", b"editor save").unwrap();
    m.rename_local("editor.tmp", "Docs/draft.txt").unwrap();
    m.cycle(false);
    assert_eq!(get("Docs/draft.txt"), b"editor save");

    // Files remain ordinary files when the daemon is absent.
    drop(m);
    std::fs::write(device.join("Home/Docs/offline.txt"), b"offline addition").unwrap();
    std::fs::write(device.join("Home/Docs/draft.txt"), b"offline edit").unwrap();
    let m = mirror(&device, url);
    m.cycle(true);
    assert_eq!(get("Docs/offline.txt"), b"offline addition");
    assert_eq!(get("Docs/draft.txt"), b"offline edit");
    m.rename_dir_local("Docs", "Projects").unwrap();
    m.cycle(false);
    assert_eq!(get("Projects/draft.txt"), b"offline edit");

    // Two conflicting saves in the same minute must each survive.
    for (ours, theirs) in [
        (
            b"local revision one".as_slice(),
            b"remote revision one".as_slice(),
        ),
        (
            b"local revision two".as_slice(),
            b"remote revision two".as_slice(),
        ),
    ] {
        m.commit_write("Projects/draft.txt", ours).unwrap();
        put("Projects/draft.txt", theirs);
        for _ in 0..3 {
            m.cycle(false);
        }
    }
    let listing: Value = client.get(&objects).send().unwrap().json().unwrap();
    let mut copies = Vec::new();
    for file in listing["payload"]["objects"].as_array().unwrap() {
        let key = file["key"].as_str().unwrap();
        if key.contains("conflict") {
            copies.push(get(key));
        }
    }
    assert!(copies.iter().any(|b| b == b"local revision one"));
    assert!(copies.iter().any(|b| b == b"local revision two"));

    // The last-location policy applies immediately through the real API.
    let record = put("delete.txt", b"last indexed copy");
    m.cycle(false);
    m.delete_local("delete.txt").unwrap();
    m.cycle(false);
    let document: Value = client
        .get(format!("{url}/test/document/{}", record["docId"]))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert!(document["document"].is_null(), "{document}");

    client
        .patch(format!(
            "{url}/rest/v2/workspaces/mirror-test/sync/settings"
        ))
        .json(&json!({"orphanPolicy":"keep"}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap();
    let record = put("retained.txt", b"keep indexed metadata");
    m.cycle(false);
    m.delete_local("retained.txt").unwrap();
    m.cycle(false);
    let document: Value = client
        .get(format!("{url}/test/document/{}", record["docId"]))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(document["document"]["locations"], json!([]));
    assert!(document["document"]["orphanedAt"].is_string());
}
