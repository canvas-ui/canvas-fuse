use crate::state::Tree;
use crate::worker::Job;
use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use rust_socketio::{client::Client, ClientBuilder, Event, Payload, RawClient, TransportType};
use serde_json::{json, Value};
use std::sync::mpsc::Sender;
use std::sync::Arc;

/// Holds the latest connected RawClient so threads outside socket.io
/// callbacks (the refresh worker) can subscribe to new context channels.
#[derive(Clone, Default)]
pub struct Subscriber(Arc<Mutex<Option<RawClient>>>);

impl Subscriber {
    pub fn subscribe(&self, ctx_id: &str) {
        if let Some(client) = self.0.lock().as_ref() {
            let channel = format!("context:{ctx_id}");
            if let Err(e) = client.emit("subscribe", json!({ "channel": channel })) {
                log::warn!("ws subscribe {channel} failed: {e}");
            } else {
                log::debug!("ws subscribed to {channel}");
            }
        }
    }
}

pub struct EventClient {
    client: Client,
    pub subscriber: Subscriber,
}

impl EventClient {
    pub fn disconnect(&self) {
        let _ = self.client.disconnect();
    }
}

fn first_payload_value(payload: &Payload) -> Option<Value> {
    match payload {
        Payload::Text(values) => values.first().cloned(),
        _ => None,
    }
}

fn context_id_of(payload: &Payload) -> Option<String> {
    let value = first_payload_value(payload)?;
    for key in ["contextId", "id"] {
        if let Some(id) = value.get(key).and_then(Value::as_str) {
            return Some(id.to_string());
        }
    }
    None
}

/// Connect to the canvas-server socket.io endpoint (push-only bridge:
/// subscribe to context:<id> channels, map events to refresh jobs).
pub fn connect(
    server: &str,
    token: &str,
    tx: Sender<Job>,
    tree: Arc<RwLock<Tree>>,
) -> Result<EventClient> {
    let subscriber = Subscriber::default();

    let sub_slot = subscriber.clone();
    let auth_tree = tree;
    let auth_tx = tx.clone();
    let on_authenticated = move |_payload: Payload, raw: RawClient| {
        log::info!("ws authenticated, subscribing to context channels");
        *sub_slot.0.lock() = Some(raw.clone());
        let contexts = auth_tree.read().context_ids();
        for ctx_id in contexts {
            let channel = format!("context:{ctx_id}");
            if let Err(e) = raw.emit("subscribe", json!({ "channel": channel })) {
                log::warn!("ws subscribe {channel} failed: {e}");
            }
        }
        // Catch up on anything missed while disconnected
        let _ = auth_tx.send(Job::RefreshAll);
    };

    let event_tx = tx;
    let on_any = move |event: Event, payload: Payload, _raw: RawClient| {
        let name = match &event {
            Event::Custom(name) => name.as_str(),
            _ => return,
        };
        let relevant = name.starts_with("document.")
            || name == "context.url.set"
            || name == "context.updated"
            || name == "context.created"
            || name == "context.deleted";
        if !relevant {
            return;
        }
        log::debug!("ws event: {name}");
        let job = match (name, context_id_of(&payload)) {
            ("context.created" | "context.deleted", _) => Job::RefreshAll,
            (_, Some(ctx_id)) => Job::RefreshContext(ctx_id),
            (_, None) => Job::RefreshAll,
        };
        let _ = event_tx.send(job);
    };

    let client = ClientBuilder::new(server)
        // canvas-server registers socket.io with transports: ['websocket'],
        // so the default polling handshake gets rejected with a JSON error
        .transport_type(TransportType::Websocket)
        .auth(json!({ "token": token }))
        .reconnect(true)
        .reconnect_on_disconnect(true)
        .reconnect_delay(1_000, 30_000)
        .on("authenticated", on_authenticated)
        .on(Event::Error, |payload: Payload, _| {
            log::warn!("ws error: {payload:?}");
        })
        .on_any(on_any)
        .connect()?;

    Ok(EventClient { client, subscriber })
}
