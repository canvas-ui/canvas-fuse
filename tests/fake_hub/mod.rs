//! An in-memory canvas-server hub speaking the subset of
//! `docs/sync-protocol.md` the mirror uses: objects listing, change feed
//! (with a trimmable log for `410`), HEAD/GET/PUT/DELETE objects with
//! `If-Match` / `If-None-Match: *` / `X-Canvas-Sha256`, rename, the
//! conflict inbox (`X-Canvas-Conflict-Of`), mirror status reports, and the
//! older `/home/mkdir` route. Enough to drive the engine end to end without
//! a FUSE mount or a real server.

#![allow(dead_code, clippy::too_many_arguments)]

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

#[derive(Debug, Clone)]
pub struct Obj {
    pub bytes: Vec<u8>,
    pub sha: String,
    pub mtime: u64,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub seq: u64,
    pub op: String,
    pub key: String,
    pub from: Option<String>,
    pub sha: Option<String>,
    pub size: u64,
    pub mtime: u64,
    pub origin: Option<String>,
    /// What the request carried, for assertions.
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InboxItem {
    pub key: String,
    pub conflict_of: String,
    pub mode: String,
    pub sha: String,
    pub base_sha: Option<String>,
    pub device: Option<String>,
    pub device_name: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
pub struct HubState {
    pub objects: BTreeMap<String, Obj>,
    pub log: Vec<LogEntry>,
    pub next_seq: u64,
    /// Entries with seq <= this are gone; a `since` below it is `410`.
    pub trimmed: u64,
    pub inbox: Vec<InboxItem>,
    pub reports: Vec<Value>,
    pub mkdirs: Vec<String>,
    pub rmdirs: Vec<String>,
    pub requests: Vec<String>,
}

pub fn sha_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

impl HubState {
    fn head(&self) -> u64 {
        self.next_seq
    }

    fn log_op(
        &mut self,
        op: &str,
        key: &str,
        from: Option<String>,
        sha: Option<String>,
        size: u64,
        mtime: u64,
        origin: Option<String>,
        if_match: Option<String>,
        if_none_match: Option<String>,
    ) -> u64 {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.log.push(LogEntry {
            seq,
            op: op.to_string(),
            key: key.to_string(),
            from,
            sha,
            size,
            mtime,
            origin,
            if_match,
            if_none_match,
        });
        seq
    }

    /// A write made "on the hub" (another device, a user drop): logged, no
    /// origin.
    pub fn put(&mut self, key: &str, bytes: &[u8]) -> String {
        let sha = sha_hex(bytes);
        let mtime = now_ms();
        self.objects.insert(
            key.to_string(),
            Obj {
                bytes: bytes.to_vec(),
                sha: sha.clone(),
                mtime,
            },
        );
        self.log_op(
            "put",
            key,
            None,
            Some(sha.clone()),
            bytes.len() as u64,
            mtime,
            None,
            None,
            None,
        );
        sha
    }

    /// A write that bypasses the log: simulates the race where the hub's
    /// bytes change between a mirror's catch-up and its push.
    pub fn put_silent(&mut self, key: &str, bytes: &[u8]) -> String {
        let sha = sha_hex(bytes);
        self.objects.insert(
            key.to_string(),
            Obj {
                bytes: bytes.to_vec(),
                sha: sha.clone(),
                mtime: now_ms(),
            },
        );
        sha
    }

    pub fn delete(&mut self, key: &str) {
        if let Some(o) = self.objects.remove(key) {
            self.log_op(
                "delete",
                key,
                None,
                None,
                o.bytes.len() as u64,
                now_ms(),
                None,
                None,
                None,
            );
        }
    }

    pub fn rename(&mut self, from: &str, to: &str) {
        if let Some(o) = self.objects.remove(from) {
            let sha = o.sha.clone();
            let size = o.bytes.len() as u64;
            let mtime = o.mtime;
            self.objects.insert(to.to_string(), o);
            self.log_op(
                "rename",
                to,
                Some(from.to_string()),
                Some(sha),
                size,
                mtime,
                None,
                None,
                None,
            );
        }
    }

    /// Forget the log up to `head`: the next `changes?since=<older>` is 410.
    pub fn trim_log(&mut self) {
        self.trimmed = self.next_seq;
        self.log.clear();
    }

    pub fn sha_of(&self, key: &str) -> Option<String> {
        self.objects.get(key).map(|o| o.sha.clone())
    }

    pub fn bytes_of(&self, key: &str) -> Option<Vec<u8>> {
        self.objects.get(key).map(|o| o.bytes.clone())
    }

    pub fn entries(&self, op: &str) -> Vec<LogEntry> {
        self.log.iter().filter(|e| e.op == op).cloned().collect()
    }
}

pub struct FakeHub {
    pub state: Arc<Mutex<HubState>>,
    pub url: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeHub {
    pub fn start() -> Self {
        Self::start_on(0)
    }

    pub fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    pub fn start_on(port: u16) -> Self {
        let server = Server::http(format!("127.0.0.1:{port}")).expect("binding fake hub");
        let port = server.server_addr().to_ip().unwrap().port();
        let state = Arc::new(Mutex::new(HubState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let st = state.clone();
        let sp = stop.clone();
        let thread = std::thread::spawn(move || {
            while !sp.load(Ordering::Relaxed) {
                match server.recv_timeout(Duration::from_millis(50)) {
                    Ok(Some(req)) => handle(req, &st),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            state,
            url: format!("http://127.0.0.1:{port}"),
            stop,
            thread: Some(thread),
        }
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap()
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for FakeHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn header(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn strip_etag(v: &str) -> String {
    v.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .to_lowercase()
}

fn envelope(
    status: u16,
    payload: Value,
    message: &str,
    code: Option<&str>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut body = json!({
        "status": if status < 400 { "success" } else { "error" },
        "statusCode": status,
        "message": message,
        "payload": payload,
    });
    if let Some(c) = code {
        body["code"] = Value::String(c.to_string());
    }
    Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn query(url: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some((_, q)) = url.split_once('?') {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                out.insert(k.to_string(), url_decode(v));
            }
        }
    }
    out
}

fn describe(o: &Obj) -> Vec<Header> {
    vec![
        Header::from_bytes("ETag", format!("\"{}\"", o.sha)).unwrap(),
        Header::from_bytes("X-Canvas-Sha256", o.sha.clone()).unwrap(),
        Header::from_bytes("X-Canvas-Size", o.bytes.len().to_string()).unwrap(),
        Header::from_bytes("X-Canvas-Mtime", o.mtime.to_string()).unwrap(),
        Header::from_bytes("Accept-Ranges", "bytes").unwrap(),
    ]
}

fn handle(mut req: Request, state: &Arc<Mutex<HubState>>) {
    let url = req.url().to_string();
    let method = req.method().clone();
    let path = url.split('?').next().unwrap_or("").to_string();
    let q = query(&url);
    state
        .lock()
        .unwrap()
        .requests
        .push(format!("{method} {path}"));

    let mut body = Vec::new();
    let _ = req.as_reader().read_to_end(&mut body);

    let backend_prefix = "/rest/v2/workspaces/ws1/backends/file/workspace%3Ahome";
    let resp: Response<std::io::Cursor<Vec<u8>>> = if path == "/rest/v2/ping" {
        envelope(
            200,
            json!({ "instanceId": "fake-hub-1", "version": "0.0.0" }),
            "pong",
            None,
        )
    } else if path == "/rest/v2/workspaces/ws1" {
        envelope(200, json!({ "id": "ws1", "name": "ws1" }), "OK", None)
    } else if path == backend_prefix {
        envelope(
            200,
            json!({ "name": "workspace:home", "effectiveExclusions": ["**/.*", "**/.*/**", "**/node_modules/**"] }),
            "OK",
            None,
        )
    } else if path == format!("{backend_prefix}/objects") && method == Method::Get {
        let st = state.lock().unwrap();
        let prefix = q.get("prefix").cloned().unwrap_or_default();
        let after = q.get("cursor").cloned();
        let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(1000);
        let mut objects = Vec::new();
        let mut next: Option<String> = None;
        for (k, o) in st.objects.iter() {
            if !k.starts_with(&prefix) {
                continue;
            }
            if let Some(a) = &after {
                if k <= a {
                    continue;
                }
            }
            if objects.len() >= limit {
                next = Some(
                    objects
                        .last()
                        .map(|v: &Value| v["key"].as_str().unwrap().to_string())
                        .unwrap(),
                );
                break;
            }
            objects.push(json!({ "key": k, "sha256": o.sha, "size": o.bytes.len(), "mtime": o.mtime, "mimeType": "application/octet-stream" }));
        }
        envelope(
            200,
            json!({ "objects": objects, "cursor": next, "head": st.head() }),
            "OK",
            None,
        )
    } else if path == format!("{backend_prefix}/changes") {
        let st = state.lock().unwrap();
        let since: u64 = q.get("since").and_then(|s| s.parse().ok()).unwrap_or(0);
        let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(1000);
        if since < st.trimmed {
            envelope(
                410,
                json!({ "since": since, "head": st.head(), "oldest": st.trimmed + 1 }),
                "Cursor too old",
                Some("CURSOR_TOO_OLD"),
            )
        } else {
            let changes: Vec<Value> = st
                .log
                .iter()
                .filter(|e| e.seq > since)
                .take(limit)
                .map(|e| {
                    json!({
                        "seq": e.seq, "ts": e.mtime, "op": e.op, "key": e.key, "from": e.from,
                        "sha256": e.sha, "size": e.size, "mtime": e.mtime, "origin": e.origin,
                    })
                })
                .collect();
            envelope(
                200,
                json!({ "changes": changes, "head": st.head(), "oldest": st.trimmed + 1, "cursor": since }),
                "OK",
                None,
            )
        }
    } else if path == format!("{backend_prefix}/objects/rename") && method == Method::Post {
        let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let from = b["from"].as_str().unwrap_or("").to_string();
        let to = b["to"].as_str().unwrap_or("").to_string();
        let if_match = b["ifMatch"].as_str().map(strip_etag);
        let origin = b["origin"]
            .as_str()
            .map(str::to_string)
            .or_else(|| header(&req, "X-Canvas-Origin"));
        let mut st = state.lock().unwrap();
        match st.objects.get(&from).cloned() {
            None => envelope(404, Value::Null, "not found", Some("NOT_FOUND")),
            Some(o) => {
                if let Some(m) = &if_match {
                    if *m != o.sha {
                        let cur =
                            json!({ "sha256": o.sha, "size": o.bytes.len(), "mtime": o.mtime });
                        envelope(
                            412,
                            json!({ "current": cur }),
                            "precondition failed",
                            Some("PRECONDITION_FAILED"),
                        )
                    } else if st.objects.contains_key(&to) {
                        envelope(
                            409,
                            json!({ "key": to }),
                            "target exists",
                            Some("TARGET_EXISTS"),
                        )
                    } else {
                        do_rename(&mut st, &from, &to, origin, if_match)
                    }
                } else if st.objects.contains_key(&to) {
                    envelope(
                        409,
                        json!({ "key": to }),
                        "target exists",
                        Some("TARGET_EXISTS"),
                    )
                } else {
                    do_rename(&mut st, &from, &to, origin, if_match)
                }
            }
        }
    } else if let Some(raw_key) = path.strip_prefix(&format!("{backend_prefix}/objects/")) {
        let key = url_decode(raw_key);
        let if_match = header(&req, "If-Match").map(|v| strip_etag(&v));
        let if_none_match = header(&req, "If-None-Match").map(|v| v.trim().to_string());
        let origin = header(&req, "X-Canvas-Origin");
        match method {
            Method::Head => {
                let st = state.lock().unwrap();
                match st.objects.get(&key) {
                    None => envelope(404, Value::Null, "not found", Some("NOT_FOUND")),
                    Some(o) => {
                        let mut r = Response::from_data(Vec::new()).with_status_code(200);
                        for h in describe(o) {
                            r = r.with_header(h);
                        }
                        r
                    }
                }
            }
            Method::Get => {
                let st = state.lock().unwrap();
                match st.objects.get(&key) {
                    None => envelope(404, Value::Null, "not found", Some("NOT_FOUND")),
                    Some(o) => {
                        let range = header(&req, "Range");
                        let (status, data, extra) =
                            match range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
                                Some(spec) => {
                                    let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
                                    let start: usize = a.parse().unwrap_or(0);
                                    let end: usize = if b.is_empty() {
                                        o.bytes.len().saturating_sub(1)
                                    } else {
                                        b.parse().unwrap_or(0)
                                    };
                                    let end = end.min(o.bytes.len().saturating_sub(1));
                                    if start >= o.bytes.len() {
                                        (416, Vec::new(), None)
                                    } else {
                                        (
                                            206,
                                            o.bytes[start..=end].to_vec(),
                                            Some(format!("bytes {start}-{end}/{}", o.bytes.len())),
                                        )
                                    }
                                }
                                None => (200, o.bytes.clone(), None),
                            };
                        let mut r = Response::from_data(data).with_status_code(status);
                        for h in describe(o) {
                            r = r.with_header(h);
                        }
                        if let Some(cr) = extra {
                            r = r.with_header(Header::from_bytes("Content-Range", cr).unwrap());
                        }
                        r
                    }
                }
            }
            Method::Put => {
                let mut st = state.lock().unwrap();
                let sha = sha_hex(&body);
                if let Some(claimed) = header(&req, "X-Canvas-Sha256") {
                    if claimed.to_lowercase() != sha {
                        return req
                            .respond(envelope(
                                422,
                                json!({ "expected": claimed, "actual": sha }),
                                "checksum mismatch",
                                Some("CHECKSUM_MISMATCH"),
                            ))
                            .unwrap();
                    }
                }
                if key.split('/').any(|s| s.starts_with('.')) {
                    return req
                        .respond(envelope(
                            409,
                            json!({ "key": key }),
                            "excluded",
                            Some("KEY_EXCLUDED"),
                        ))
                        .unwrap();
                }
                if let Some(conflict_of) = header(&req, "X-Canvas-Conflict-Of") {
                    let mode =
                        header(&req, "X-Canvas-Conflict-Mode").unwrap_or_else(|| "inbox".into());
                    st.inbox.push(InboxItem {
                        key: key.clone(),
                        conflict_of: conflict_of.clone(),
                        mode: mode.clone(),
                        sha: sha.clone(),
                        base_sha: header(&req, "X-Canvas-Base-Sha256"),
                        device: origin.clone(),
                        device_name: header(&req, "X-Canvas-Device-Name"),
                        bytes: body.clone(),
                    });
                    if mode == "rename" {
                        // Written like any object, plus the record.
                        let mtime = now_ms();
                        st.objects.insert(
                            key.clone(),
                            Obj {
                                bytes: body.clone(),
                                sha: sha.clone(),
                                mtime,
                            },
                        );
                        st.log_op(
                            "put",
                            &key,
                            None,
                            Some(sha.clone()),
                            body.len() as u64,
                            mtime,
                            origin,
                            None,
                            None,
                        );
                    }
                    let hub_sha = st.objects.get(&conflict_of).map(|o| o.sha.clone());
                    return req.respond(envelope(201, json!({ "docId": 4242, "key": key, "conflictOf": conflict_of, "sha256": sha, "hubDocId": 1, "hubSha256": hub_sha }), "Conflict recorded", None)).unwrap();
                }
                let current = st.objects.get(&key).cloned();
                let describe_cur = |c: &Option<Obj>| {
                    c.as_ref()
                        .map(
                            |o| json!({ "sha256": o.sha, "size": o.bytes.len(), "mtime": o.mtime }),
                        )
                        .unwrap_or(Value::Null)
                };
                if if_none_match.as_deref() == Some("*") && current.is_some() {
                    envelope(
                        412,
                        json!({ "current": describe_cur(&current) }),
                        "precondition failed",
                        Some("PRECONDITION_FAILED"),
                    )
                } else if let Some(m) = &if_match {
                    match &current {
                        Some(o) if o.sha == *m => {
                            put_obj(&mut st, &key, body, sha, origin, if_match, if_none_match)
                        }
                        _ => envelope(
                            412,
                            json!({ "current": describe_cur(&current) }),
                            "precondition failed",
                            Some("PRECONDITION_FAILED"),
                        ),
                    }
                } else {
                    put_obj(&mut st, &key, body, sha, origin, if_match, if_none_match)
                }
            }
            Method::Delete => {
                let mut st = state.lock().unwrap();
                match st.objects.get(&key).cloned() {
                    None => envelope(404, Value::Null, "not found", Some("NOT_FOUND")),
                    Some(o) => {
                        if let Some(m) = &if_match {
                            if *m != o.sha {
                                let cur = json!({ "sha256": o.sha, "size": o.bytes.len(), "mtime": o.mtime });
                                return req
                                    .respond(envelope(
                                        412,
                                        json!({ "current": cur }),
                                        "precondition failed",
                                        Some("PRECONDITION_FAILED"),
                                    ))
                                    .unwrap();
                            }
                        }
                        st.objects.remove(&key);
                        let seq = st.log_op(
                            "delete",
                            &key,
                            None,
                            None,
                            o.bytes.len() as u64,
                            now_ms(),
                            origin,
                            if_match,
                            None,
                        );
                        envelope(
                            200,
                            json!({ "key": key, "sha256": o.sha, "seq": seq, "docId": 1 }),
                            "deleted",
                            None,
                        )
                    }
                }
            }
            _ => envelope(405, Value::Null, "method", None),
        }
    } else if path.starts_with("/rest/v2/workspaces/ws1/mirrors/") && path.ends_with("/status") {
        let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let mut st = state.lock().unwrap();
        st.reports.push(b);
        let head = st.head();
        envelope(200, json!({ "mirror": {}, "head": head }), "recorded", None)
    } else if path == "/rest/v2/workspaces/ws1/home/mkdir" {
        let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        state
            .lock()
            .unwrap()
            .mkdirs
            .push(b["path"].as_str().unwrap_or("").to_string());
        envelope(200, Value::Null, "created", None)
    } else if let Some(p) = path.strip_prefix("/rest/v2/workspaces/ws1/home/") {
        state.lock().unwrap().rmdirs.push(url_decode(p));
        envelope(200, Value::Null, "deleted", None)
    } else {
        envelope(404, Value::Null, &format!("no route {method} {path}"), None)
    };
    let _ = req.respond(resp);
}

fn put_obj(
    st: &mut HubState,
    key: &str,
    body: Vec<u8>,
    sha: String,
    origin: Option<String>,
    if_match: Option<String>,
    if_none_match: Option<String>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let previous = st.objects.get(key).cloned();
    let unchanged = previous.as_ref().map(|p| p.sha == sha).unwrap_or(false);
    let mtime = now_ms();
    let size = body.len() as u64;
    st.objects.insert(
        key.to_string(),
        Obj {
            bytes: body,
            sha: sha.clone(),
            mtime,
        },
    );
    let seq = st.log_op(
        "put",
        key,
        None,
        Some(sha.clone()),
        size,
        mtime,
        origin,
        if_match,
        if_none_match,
    );
    let payload = json!({
        "key": key, "sha256": sha, "size": size, "mtime": mtime, "seq": seq, "docId": 1,
        "previous": previous.map(|p| json!({ "sha256": p.sha })),
        "unchanged": unchanged,
    });
    envelope(
        if previous_is_none(&payload) { 201 } else { 200 },
        payload,
        "ok",
        None,
    )
}

fn previous_is_none(payload: &Value) -> bool {
    payload["previous"].is_null()
}

fn do_rename(
    st: &mut HubState,
    from: &str,
    to: &str,
    origin: Option<String>,
    if_match: Option<String>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let o = st.objects.remove(from).unwrap();
    let sha = o.sha.clone();
    let size = o.bytes.len() as u64;
    let mtime = o.mtime;
    st.objects.insert(to.to_string(), o);
    let seq = st.log_op(
        "rename",
        to,
        Some(from.to_string()),
        Some(sha.clone()),
        size,
        mtime,
        origin,
        if_match,
        None,
    );
    envelope(
        200,
        json!({ "from": from, "to": to, "sha256": sha, "seq": seq, "docId": 1 }),
        "renamed",
        None,
    )
}
