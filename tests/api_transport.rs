use canvas_fuse::api::ApiClient;
use flate2::{write::GzEncoder, Compression};
use serde_json::json;
use std::io::{Read, Write};
use std::time::Duration;
use tiny_http::{Header, Response, Server};

#[test]
fn live_directory_move_is_one_request_with_a_retry_id() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let api = ApiClient::new(&format!("http://{}", server.server_addr()), "test").unwrap();
    let thread = std::thread::spawn(move || {
        let mut req = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(req.method().as_str(), "POST");
        assert!(req.url().ends_with("/objects/rename"));
        let body: serde_json::Value = serde_json::from_reader(req.as_reader()).unwrap();
        assert_eq!(body["from"], "Architektúra/Domček");
        assert_eq!(body["to"], "Architektúra/Fotky");
        assert_eq!(body["directory"], true);
        assert_eq!(body["operationId"].as_str().unwrap().len(), 32);
        req.respond(Response::from_string(
            r#"{"payload":{"directory":true,"state":"complete"}}"#,
        ))
        .unwrap();
        assert!(server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none());
    });
    api.rename_directory(
        "ws",
        "workspace:home",
        "Architektúra/Domček",
        "Architektúra/Fotky",
    )
    .unwrap();
    thread.join().unwrap();
}

#[test]
fn gzip_email_listing_preserves_full_documents_across_pages() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let api = ApiClient::new(&format!("http://{}", server.server_addr()), "test").unwrap();
    let emails: Vec<_> = (1..=501)
        .map(|id| json!({
            "id": id,
            "schema": "data/schema/message/email",
            "data": {
                "subject": format!("Message {id}"),
                "body": "Complete cached email — žluťoučký. ".repeat(100),
                "bodyHtml": "<p>Complete HTML body.</p>".repeat(100),
                "headers": { "received": "Original message headers" },
                "attachments": [{ "filename": "invoice.pdf", "url": "stored://workspace:data/invoice" }]
            }
        }))
        .collect();
    let payloads = emails.clone();
    let thread = std::thread::spawn(move || {
        for offset in [0, 500] {
            let req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert!(
                req.headers().iter().any(|h| {
                    h.field.equiv("Accept-Encoding")
                        && h.value.as_str().split(',').any(|v| v.trim() == "gzip")
                }),
                "client must negotiate gzip"
            );
            assert!(req
                .url()
                .contains("treeNameOrTreeId=t-backends&treeType=directory"));
            assert!(req
                .url()
                .contains("context=%2Fimap%2Fme%40example.com%2Finbox"));
            assert!(req.url().ends_with(&format!("limit=500&offset={offset}")));
            let bytes = serde_json::to_vec(&json!({
                "payload": &payloads[offset..(offset + 500).min(payloads.len())],
                "totalCount": payloads.len(),
            }))
            .unwrap();
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(&bytes).unwrap();
            let compressed = encoder.finish().unwrap();
            assert!(compressed.len() < bytes.len() / 5);
            req.respond(
                Response::from_data(compressed)
                    .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
                    .with_header(Header::from_bytes("Content-Encoding", "gzip").unwrap()),
            )
            .unwrap();
        }
    });
    let result = api.list_tree_documents(
        "ws",
        "t-backends",
        "directory",
        "/imap/me@example.com/inbox",
    );
    thread.join().unwrap();
    let docs = result.unwrap();
    assert_eq!(docs.len(), emails.len());
    for (doc, expected) in docs.iter().zip(&emails) {
        assert_eq!(doc.id, expected["id"].as_u64().unwrap());
        assert_eq!(doc.schema, expected["schema"]);
        assert_eq!(doc.data, expected["data"]);
    }
}

#[test]
fn uncompressed_responses_still_work_and_malformed_json_is_identified() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let api = ApiClient::new(&format!("http://{}", server.server_addr()), "test").unwrap();
    let thread = std::thread::spawn(move || {
        for body in [
            r#"{"payload":[{"id":"t-1","name":"context","type":"context"}]}"#,
            "not JSON",
        ] {
            let req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            req.respond(Response::from_string(body)).unwrap();
        }
    });
    let trees = api.list_trees("ws").unwrap();
    assert_eq!(trees[0].name, "context");
    let error = api.list_trees("ws").unwrap_err();
    thread.join().unwrap();
    assert!(error.to_string().contains("invalid JSON (HTTP 200 OK)"));
}

#[test]
fn interrupted_response_is_reported_as_a_body_transfer_failure() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let api = ApiClient::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        "test",
    )
    .unwrap();
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
        }
        // Successful headers, followed by an incomplete body and a closed connection.
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{}")
            .unwrap();
    });
    let error = api.list_trees("ws").unwrap_err();
    thread.join().unwrap();
    assert!(error
        .to_string()
        .contains("reading response body failed (HTTP 200 OK)"));
    assert!(!error.to_string().contains("invalid JSON"));
}
