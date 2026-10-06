//! Run against the isolated nginx fixture; never requires a kernel mount.
use anyhow::{Context, Result};
use canvas_fuse::{
    api::ApiClient,
    mirror::{hub::HubClient, DeviceIdentity},
    tls::{http_builder, TlsFiles},
};
use rust_socketio::{ClientBuilder, Event, TransportType};
use serde_json::json;
fn main() -> Result<()> {
    let url = std::env::var("CANVAS_TLS_URL")?;
    let dir = std::path::PathBuf::from(std::env::var("CANVAS_TLS_FIXTURE")?);
    let identity = TlsFiles {
        cert_file: dir.join("client.chain.crt"),
        key_file: dir.join("client.key"),
    }
    .load(&url)?;
    assert!(TlsFiles {
        cert_file: dir.join("client.chain.crt"),
        key_file: dir.join("other.key")
    }
    .load(&url)
    .is_err());
    let api = ApiClient::with_tls(&url, "canvas-test-token", Some(&identity))?;
    assert!(api.ping()?.0["cn"]
        .as_str()
        .context("no nginx identity")?
        .contains("Canvas client"));
    let hub = HubClient::with_tls(
        &url,
        "canvas-test-token",
        "workspace",
        "workspace:home",
        &DeviceIdentity::resolve(),
        Some(&identity),
    )?;
    assert!(hub.ping()?["cn"]
        .as_str()
        .context("no mirror identity")?
        .contains("Canvas client"));
    let http = http_builder(&url, Some(&identity))?.build()?;
    assert!(http.get(format!("{url}/outside")).send().is_err());
    let body: serde_json::Value = http
        .put(format!("{url}/upload"))
        .body(vec![0u8; 128 * 1024])
        .send()?
        .json()?;
    assert_eq!(body["payload"]["bytes"], 128 * 1024);
    let (connected_tx, connected_rx) = std::sync::mpsc::channel();
    let socket = ClientBuilder::new(&url)
        .transport_type(TransportType::Websocket)
        .tls_config(identity.connector()?)
        .auth(json!({"token":"canvas-test-token"}))
        .on(Event::Connect, move |_, _| {
            let _ = connected_tx.send(());
        })
        .on(Event::Error, |p, _| eprintln!("WS error: {p:?}"))
        .reconnect(false)
        .connect()?;
    connected_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .context("Socket.IO namespace handshake")?;
    let (tx, rx) = std::sync::mpsc::channel();
    socket.emit_with_ack(
        "echo",
        json!("test"),
        std::time::Duration::from_secs(5),
        move |payload, _| {
            let _ = tx.send(format!("{payload:?}"));
        },
    )?;
    let ack = rx.recv_timeout(std::time::Duration::from_secs(5))?;
    assert!(ack.contains("Canvas client"));
    socket.disconnect()?;
    println!("FUSE native TLS passed: REST, mirror, upload, origin scope, Socket.IO");
    Ok(())
}
