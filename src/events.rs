use crate::state::Tree;
use crate::worker::Job;
use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use rust_socketio::{client::Client, ClientBuilder, Event, Payload, RawClient, TransportType};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::Arc;

#[derive(Default)]
struct SubscriberInner {
    client: Option<RawClient>,
    /// Channels the server has *confirmed* subscribed (via its `subscribed`
    /// event), stored by full channel name (`context:<id>` / `workspace:<id>`).
    /// A subscribe that was denied (e.g. workspace still starting → "Access
    /// denied") never lands here, so the subscribe keeps getting retried.
    confirmed: HashSet<String>,
}

/// Tracks the live ws client + confirmed subscriptions so any thread (the
/// refresh worker) can idempotently ensure a context is subscribed.
#[derive(Clone, Default)]
pub struct Subscriber(Arc<Mutex<SubscriberInner>>);

impl Subscriber {
    fn set_client(&self, client: RawClient) {
        self.0.lock().client = Some(client);
    }

    /// New socket (fresh connect/reconnect): nothing is subscribed anymore.
    fn reset_confirmed(&self) {
        self.0.lock().confirmed.clear();
    }

    /// Server acked a subscription (keyed by full channel name).
    fn confirm(&self, channel: &str) {
        self.0.lock().confirmed.insert(channel.to_string());
        log::debug!("ws subscription confirmed for {channel}");
    }

    /// Emit a subscribe for an arbitrary channel, deduped by the confirmed set.
    fn subscribe_raw(&self, channel: &str) {
        let inner = self.0.lock();
        if inner.confirmed.contains(channel) {
            return;
        }
        if let Some(client) = inner.client.as_ref() {
            if let Err(e) = client.emit("subscribe", json!({ "channel": channel })) {
                log::warn!("ws subscribe {channel} failed: {e}");
            }
        }
    }

    /// Ensure a context channel is subscribed. Safe to call on every refresh.
    pub fn subscribe(&self, ctx_id: &str) {
        self.subscribe_raw(&format!("context:{ctx_id}"));
    }

    /// Ensure a workspace channel is subscribed (workspace-mount live updates).
    pub fn subscribe_workspace(&self, ws_id: &str) {
        self.subscribe_raw(&format!("workspace:{ws_id}"));
    }
}

fn channel_from_payload(payload: &Payload) -> Option<String> {
    let value = first_payload_value(payload)?;
    value
        .get("channel")
        .and_then(Value::as_str)
        .map(str::to_string)
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
    subscriber: Subscriber,
) -> Result<EventClient> {
    // Mount mode is fixed for the life of the process; decide once.
    let ws_mode = tree.read().is_workspace();

    let sub_auth = subscriber.clone();
    let auth_tree = tree;
    let auth_tx = tx.clone();
    let on_authenticated = move |_payload: Payload, raw: RawClient| {
        sub_auth.set_client(raw);
        // Fresh socket: prior confirmations are void.
        sub_auth.reset_confirmed();
        if ws_mode {
            // Workspace mount: one channel carries every tree/document change.
            if let Some(ws_id) = auth_tree.read().ws_id() {
                log::info!("ws authenticated, subscribing to workspace:{ws_id}");
                sub_auth.subscribe_workspace(&ws_id);
            }
        } else {
            log::info!("ws authenticated, subscribing to context channels");
            // Any context the server isn't ready for (workspace down) stays
            // unconfirmed and gets retried after its next successful refresh.
            for ctx_id in auth_tree.read().context_ids() {
                sub_auth.subscribe(&ctx_id);
            }
        }
        // Catch up on anything missed while disconnected
        let _ = auth_tx.send(Job::RefreshAll);
    };

    // Server acks each subscription with a `subscribed` event; record it so the
    // worker stops re-issuing that subscribe.
    let sub_ack = subscriber.clone();
    let on_subscribed = move |payload: Payload, _: RawClient| {
        if let Some(channel) = channel_from_payload(&payload) {
            sub_ack.confirm(&channel);
        }
    };

    let event_tx = tx;
    let on_any = move |event: Event, payload: Payload, _raw: RawClient| {
        let name = match &event {
            Event::Custom(name) => name.as_str(),
            _ => return,
        };
        // Workspace mounts reconcile the whole tree on any structural or
        // document change — there's no per-context job to target.
        if ws_mode {
            if name.starts_with("tree.") || name.starts_with("document.") {
                log::debug!("ws event (workspace): {name}");
                let _ = event_tx.send(Job::RefreshAll);
            }
            return;
        }
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
        .on("subscribed", on_subscribed)
        .on(Event::Error, |payload: Payload, _| {
            // Retryable errors (e.g. workspace still starting) are expected and
            // self-heal via the next successful refresh — log them quietly.
            let val = first_payload_value(&payload);
            let retryable = val
                .as_ref()
                .and_then(|v| v.get("retryable"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let msg = val
                .as_ref()
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{payload:?}"));
            if retryable {
                log::debug!("ws not-ready (will retry): {msg}");
            } else {
                log::warn!("ws error: {msg}");
            }
        })
        .on_any(on_any)
        .connect()?;

    Ok(EventClient { client, subscriber })
}

/// Supervise the ws connection: retry the initial connect with backoff until it
/// succeeds (a server/workspace still starting up must not leave the mount
/// permanently ws-less). Once connected, rust_socketio auto-reconnects, so we
/// just hold the client alive until `stop`. The shared `subscriber` means the
/// worker's on_new_context subscribes correctly whenever the ws comes up.
pub fn supervise(
    server: String,
    token: String,
    tx: Sender<Job>,
    tree: Arc<RwLock<Tree>>,
    subscriber: Subscriber,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let mut delay = Duration::from_secs(1);
    let client = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match connect(
            &server,
            &token,
            tx.clone(),
            tree.clone(),
            subscriber.clone(),
        ) {
            Ok(c) => break c,
            Err(e) => {
                log::warn!(
                    "ws connect failed ({e:#}); retrying in {}s (resync still covers updates)",
                    delay.as_secs()
                );
                // Sleep in short steps so stop is honored promptly
                let mut slept = Duration::ZERO;
                while slept < delay {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    slept += Duration::from_millis(250);
                }
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    };
    log::info!("ws connected");
    // Hold the client alive (dropping it disconnects) until teardown.
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
    }
    client.disconnect();
}
