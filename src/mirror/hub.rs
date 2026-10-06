//! The objects-protocol client (`docs/sync-protocol.md` in canvas-server).
//! Blocking reqwest, one client per mount; every call is typed on the
//! outcomes the engine has to tell apart — a `412` is a decision, not an
//! error string — and a transport failure is `Offline`, which is a state the
//! mount lives in, not a bug.
//!
//! Bodies stream both ways: a `PUT` sends a cache file without loading it,
//! a `GET` is handed back as a reader for `cache::fetch` to spool. The
//! per-request timeout is only set on the small JSON calls; a 20 GiB upload
//! is bounded by the connect timeout and the read making progress.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteStat {
    pub sha256: String,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Debug, Clone)]
pub struct ListedObject {
    pub key: String,
    pub sha256: String,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Debug, Clone)]
pub struct Listing {
    pub objects: Vec<ListedObject>,
    pub cursor: Option<String>,
    pub head: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOp {
    Put,
    Delete,
    Rename,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub seq: u64,
    pub op: ChangeOp,
    pub key: String,
    pub from: Option<String>,
    pub sha256: Option<String>,
    pub size: u64,
    pub mtime: u64,
    pub origin: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Changes {
    pub changes: Vec<Change>,
    pub head: u64,
    pub oldest: u64,
}

#[derive(Debug, Clone, Default)]
pub struct PutOptions {
    pub if_match: Option<String>,
    /// `If-None-Match: *` — the key must be free.
    pub if_none_match_any: bool,
    pub sha256: Option<String>,
    pub mtime: Option<u64>,
    pub conflict_of: Option<String>,
    pub conflict_mode: Option<super::ConflictMode>,
    pub base_sha256: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PutResult {
    pub key: String,
    pub sha256: String,
    pub size: u64,
    pub mtime: u64,
    pub seq: u64,
    pub unchanged: bool,
    /// Conflict-inbox uploads answer with the inbox document's id. Reported
    /// to the user, never used as identity.
    pub doc_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum PutBody {
    File(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Debug)]
pub enum HubError {
    /// Could not reach the hub at all (connect/timeout/reset).
    Offline(String),
    /// Credentials refused — the device was revoked or the token expired.
    Unauthorized,
    /// `412`: the key is not what the caller's `If-Match` said. `current` is
    /// the hub's version, or None when the key is gone.
    PreconditionFailed {
        current: Option<RemoteStat>,
    },
    /// `410` on the change feed: rebuild from the listing.
    CursorTooOld {
        oldest: u64,
        head: u64,
    },
    NotFound,
    /// `409 TARGET_EXISTS` on rename.
    TargetExists,
    /// A `4xx` that will not change on retry (`INVALID_KEY`, `KEY_EXCLUDED`,
    /// read-only backend, …). The job is dropped and counted as skipped.
    Refused {
        status: u16,
        code: Option<String>,
        message: String,
    },
    /// `429`/`5xx` after the in-call retries ran out; backoff and retry later.
    Retryable {
        status: u16,
        message: String,
    },
    Other(String),
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubError::Offline(m) => write!(f, "offline: {m}"),
            HubError::Unauthorized => write!(f, "unauthorized"),
            HubError::PreconditionFailed { current } => {
                write!(f, "precondition failed (current: {current:?})")
            }
            HubError::CursorTooOld { oldest, head } => {
                write!(f, "cursor too old (oldest {oldest}, head {head})")
            }
            HubError::NotFound => write!(f, "not found"),
            HubError::TargetExists => write!(f, "target exists"),
            HubError::Refused {
                status,
                code,
                message,
            } => write!(
                f,
                "refused: HTTP {status} {} {message}",
                code.as_deref().unwrap_or("")
            ),
            HubError::Retryable { status, message } => write!(f, "HTTP {status}: {message}"),
            HubError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for HubError {}

impl HubError {
    pub fn is_offline(&self) -> bool {
        matches!(self, HubError::Offline(_))
    }

    /// Permanent for THIS job: retrying without a change of input is
    /// pointless.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            HubError::Refused { .. } | HubError::NotFound | HubError::TargetExists
        )
    }
}

fn transport_error(e: reqwest::Error) -> HubError {
    HubError::Offline(e.to_string())
}

pub struct HubClient {
    http: reqwest::blocking::Client,
    base: String,
    token: String,
    ws: String,
    backend: String,
    device_id: String,
    device_name: String,
}

const JSON_TIMEOUT: Duration = Duration::from_secs(30);
const RETRIES: u32 = 3;

impl HubClient {
    pub fn new(
        server: &str,
        token: &str,
        ws_id: &str,
        backend: &str,
        device: &super::DeviceIdentity,
    ) -> Result<Self> {
        Self::with_tls(server, token, ws_id, backend, device, None)
    }
    pub fn with_tls(
        server: &str,
        token: &str,
        ws_id: &str,
        backend: &str,
        device: &super::DeviceIdentity,
        identity: Option<&crate::tls::ClientIdentity>,
    ) -> Result<Self> {
        let http = crate::tls::http_builder(server, identity)?
            .connect_timeout(Duration::from_secs(15))
            // No overall timeout: object bodies can be huge. Small calls set
            // their own per request.
            .timeout(None)
            .build()?;
        Ok(Self {
            http,
            base: server.trim_end_matches('/').to_string(),
            token: token.to_string(),
            ws: ws_id.to_string(),
            backend: backend.to_string(),
            device_id: device.id.clone(),
            device_name: device.name.clone(),
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn workspace_id(&self) -> &str {
        &self.ws
    }

    pub fn backend(&self) -> &str {
        &self.backend
    }

    fn backend_url(&self) -> String {
        format!(
            "{}/rest/v2/workspaces/{}/backends/file/{}",
            self.base,
            encode_segment(&self.ws),
            encode_segment(&self.backend)
        )
    }

    pub fn object_url(&self, key: &str) -> String {
        format!("{}/objects/{}", self.backend_url(), encode_key(key))
    }

    // ── envelope handling ────────────────────────────────────────────────────

    /// Turn a non-success response into the typed error. Reads the body
    /// (JSON envelope) when there is one.
    fn classify(resp: reqwest::blocking::Response) -> HubError {
        let status = resp.status();
        let body: Value = resp.json().unwrap_or(Value::Null);
        let code = body.get("code").and_then(Value::as_str).map(str::to_string);
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let payload = body.get("payload").cloned().unwrap_or(Value::Null);
        match status.as_u16() {
            401 => HubError::Unauthorized,
            404 => HubError::NotFound,
            409 if code.as_deref() == Some("TARGET_EXISTS") => HubError::TargetExists,
            410 => HubError::CursorTooOld {
                oldest: payload.get("oldest").and_then(Value::as_u64).unwrap_or(0),
                head: payload.get("head").and_then(Value::as_u64).unwrap_or(0),
            },
            412 => HubError::PreconditionFailed {
                current: payload.get("current").and_then(parse_stat),
            },
            429 | 500..=599 => HubError::Retryable {
                status: status.as_u16(),
                message,
            },
            s => HubError::Refused {
                status: s,
                code,
                message,
            },
        }
    }

    /// Run a request builder with retries on transport errors that are
    /// plausibly transient and on 429/5xx. `build` is called per attempt so
    /// a streamed body can be reopened.
    fn with_retries<F>(&self, mut build: F) -> Result<reqwest::blocking::Response, HubError>
    where
        F: FnMut() -> Result<reqwest::blocking::RequestBuilder, HubError>,
    {
        let mut delay = Duration::from_millis(500);
        let mut last: Option<HubError> = None;
        for attempt in 0..RETRIES {
            let req = build()?.bearer_auth(&self.token);
            match req.send() {
                Ok(resp) => {
                    let s = resp.status().as_u16();
                    if s == 429 || (500..600).contains(&s) {
                        last = Some(Self::classify(resp));
                    } else {
                        return Ok(resp);
                    }
                }
                Err(e) => {
                    // Connection refused / DNS / reset: the hub is not there.
                    // No point hammering it — offline is a state, the engine
                    // comes back on the next nudge or poll.
                    return Err(transport_error(e));
                }
            }
            if attempt + 1 < RETRIES {
                std::thread::sleep(delay);
                delay *= 2;
            }
        }
        Err(last.unwrap_or_else(|| HubError::Other("retries exhausted".into())))
    }

    fn json_ok(resp: reqwest::blocking::Response) -> Result<Value, HubError> {
        if !resp.status().is_success() {
            return Err(Self::classify(resp));
        }
        resp.json::<Value>()
            .map_err(|e| HubError::Other(format!("invalid JSON: {e}")))
    }

    // ── identity / config ────────────────────────────────────────────────────

    /// `GET /ping` → the hub's stable instance id.
    pub fn ping(&self) -> Result<Value, HubError> {
        let url = format!("{}/rest/v2/ping", self.base);
        let resp = self.with_retries(|| Ok(self.http.get(&url).timeout(JSON_TIMEOUT)))?;
        let body = Self::json_ok(resp)?;
        Ok(body.get("payload").cloned().unwrap_or(Value::Null))
    }

    pub fn instance_id(&self) -> Result<Option<String>, HubError> {
        let payload = self.ping()?;
        Ok(payload
            .get("instanceId")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// The hub's effective exclusion patterns for the backend
    /// (`payload.effectiveExclusions`), so the mirror never queues what the
    /// hub will refuse. Empty when the hub does not report them.
    pub fn exclusions(&self) -> Result<Vec<String>, HubError> {
        let url = self.backend_url();
        let resp = self.with_retries(|| Ok(self.http.get(&url).timeout(JSON_TIMEOUT)))?;
        let body = Self::json_ok(resp)?;
        let payload = body.get("payload").cloned().unwrap_or(Value::Null);
        let payload = payload.get("backend").cloned().unwrap_or(payload);
        // `getBackend` nests them under `config`; older shapes put them on
        // the payload itself.
        let list = payload
            .get("config")
            .and_then(|c| c.get("effectiveExclusions"))
            .or_else(|| payload.get("effectiveExclusions"));
        Ok(list
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    // ── listing / feed ───────────────────────────────────────────────────────

    pub fn list_objects(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Listing, HubError> {
        let mut url = format!(
            "{}/objects?prefix={}&limit={limit}",
            self.backend_url(),
            encode_component(prefix)
        );
        if let Some(c) = cursor {
            url.push_str(&format!("&cursor={}", encode_component(c)));
        }
        let resp = self.with_retries(|| Ok(self.http.get(&url).timeout(JSON_TIMEOUT)))?;
        let body = Self::json_ok(resp)?;
        let payload = body.get("payload").cloned().unwrap_or(Value::Null);
        let objects = payload
            .get("objects")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|o| {
                        Some(ListedObject {
                            key: o.get("key")?.as_str()?.to_string(),
                            sha256: o
                                .get("sha256")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_lowercase(),
                            size: o.get("size").and_then(Value::as_u64).unwrap_or(0),
                            mtime: parse_ms(o.get("mtime")),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Listing {
            objects,
            cursor: payload
                .get("cursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string),
            head: payload.get("head").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    pub fn changes(&self, since: u64, limit: usize) -> Result<Changes, HubError> {
        let url = format!("{}/changes?since={since}&limit={limit}", self.backend_url());
        let resp = self.with_retries(|| Ok(self.http.get(&url).timeout(JSON_TIMEOUT)))?;
        let body = Self::json_ok(resp)?;
        let payload = body.get("payload").cloned().unwrap_or(Value::Null);
        let changes = payload
            .get("changes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        let op = match c.get("op").and_then(Value::as_str)? {
                            "put" => ChangeOp::Put,
                            "delete" => ChangeOp::Delete,
                            "rename" => ChangeOp::Rename,
                            _ => return None,
                        };
                        Some(Change {
                            seq: c.get("seq").and_then(Value::as_u64)?,
                            op,
                            key: c.get("key")?.as_str()?.to_string(),
                            from: c.get("from").and_then(Value::as_str).map(str::to_string),
                            sha256: c
                                .get("sha256")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(|s| s.to_lowercase()),
                            size: c.get("size").and_then(Value::as_u64).unwrap_or(0),
                            mtime: parse_ms(c.get("mtime")),
                            origin: c.get("origin").and_then(Value::as_str).map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Changes {
            changes,
            head: payload.get("head").and_then(Value::as_u64).unwrap_or(0),
            oldest: payload.get("oldest").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    // ── objects ──────────────────────────────────────────────────────────────

    /// `HEAD objects/<key>` → the hub's stat, None when the key is gone.
    pub fn head_object(&self, key: &str) -> Result<Option<RemoteStat>, HubError> {
        let url = self.object_url(key);
        let resp = self.with_retries(|| Ok(self.http.head(&url).timeout(JSON_TIMEOUT)))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(Self::classify(resp));
        }
        Ok(stat_from_headers(resp.headers()))
    }

    /// `GET objects/<key>`, optionally from byte `from` onward. Returns the
    /// response as a reader plus the ETag digest (when the hub sent one).
    pub fn get_object(
        &self,
        key: &str,
        from: Option<u64>,
    ) -> Result<(reqwest::blocking::Response, Option<String>), HubError> {
        let url = self.object_url(key);
        let resp = self.with_retries(|| {
            let mut req = self.http.get(&url);
            if let Some(from) = from {
                req = req.header(reqwest::header::RANGE, format!("bytes={from}-"));
            }
            Ok(req)
        })?;
        if !resp.status().is_success() {
            return Err(Self::classify(resp));
        }
        let etag = etag_of(resp.headers());
        Ok((resp, etag))
    }

    /// A byte window (inclusive end), for files too big to cache.
    pub fn get_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, HubError> {
        let url = self.object_url(key);
        let resp = self.with_retries(|| {
            Ok(self
                .http
                .get(&url)
                .header(reqwest::header::RANGE, format!("bytes={start}-{end}")))
        })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Self::classify(resp));
        }
        let body = resp.bytes().map_err(transport_error)?.to_vec();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            Ok(body)
        } else {
            let from = (start as usize).min(body.len());
            let to = ((end as usize).saturating_add(1)).min(body.len());
            Ok(body[from..to].to_vec())
        }
    }

    pub fn put_object(
        &self,
        key: &str,
        body: &PutBody,
        opts: &PutOptions,
    ) -> Result<PutResult, HubError> {
        let url = self.object_url(key);
        let resp = self.with_retries(|| {
            let mut req = self.http.put(&url);
            req = req.header("X-Canvas-Origin", &self.device_id);
            if let Some(m) = &opts.if_match {
                req = req.header(reqwest::header::IF_MATCH, format!("\"{m}\""));
            }
            if opts.if_none_match_any {
                req = req.header(reqwest::header::IF_NONE_MATCH, "*");
            }
            if let Some(s) = &opts.sha256 {
                req = req.header("X-Canvas-Sha256", s);
            }
            if let Some(t) = opts.mtime {
                req = req.header("X-Canvas-Mtime", t.to_string());
            }
            if let Some(c) = &opts.conflict_of {
                req = req.header("X-Canvas-Conflict-Of", c);
                req = req.header("X-Canvas-Device-Name", &self.device_name);
                if let Some(mode) = opts.conflict_mode {
                    let mode = match mode {
                        super::ConflictMode::Prompt => "inbox",
                        super::ConflictMode::Rename => "rename",
                    };
                    req = req.header("X-Canvas-Conflict-Mode", mode);
                }
                if let Some(b) = &opts.base_sha256 {
                    req = req.header("X-Canvas-Base-Sha256", b);
                }
            }
            req = req.header(
                reqwest::header::CONTENT_TYPE,
                opts.content_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
            );
            let req = match body {
                PutBody::Bytes(b) => req.body(b.clone()),
                PutBody::File(path) => {
                    let f = std::fs::File::open(path)
                        .map_err(|e| HubError::Other(format!("opening {}: {e}", path.display())))?;
                    let len = f
                        .metadata()
                        .map(|m| m.len())
                        .map_err(|e| HubError::Other(e.to_string()))?;
                    req.body(reqwest::blocking::Body::sized(f, len))
                }
            };
            Ok(req)
        })?;
        let body = Self::json_ok(resp)?;
        let payload = body.get("payload").cloned().unwrap_or(Value::Null);
        Ok(PutResult {
            key: payload
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_string(),
            sha256: payload
                .get("sha256")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase(),
            size: payload.get("size").and_then(Value::as_u64).unwrap_or(0),
            mtime: parse_ms(payload.get("mtime")),
            seq: payload.get("seq").and_then(Value::as_u64).unwrap_or(0),
            unchanged: payload
                .get("unchanged")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            doc_id: payload.get("docId").and_then(Value::as_u64),
        })
    }

    pub fn delete_object(&self, key: &str, if_match: Option<&str>) -> Result<(), HubError> {
        let url = self.object_url(key);
        let resp = self.with_retries(|| {
            let mut req = self
                .http
                .delete(&url)
                .timeout(JSON_TIMEOUT)
                .header("X-Canvas-Origin", &self.device_id);
            if let Some(m) = if_match {
                req = req.header(reqwest::header::IF_MATCH, format!("\"{m}\""));
            }
            Ok(req)
        })?;
        Self::json_ok(resp).map(|_| ())
    }

    pub fn rename_object(
        &self,
        from: &str,
        to: &str,
        if_match: Option<&str>,
    ) -> Result<(), HubError> {
        let url = format!("{}/objects/rename", self.backend_url());
        let mut body = serde_json::json!({ "from": from, "to": to, "origin": self.device_id });
        if let Some(m) = if_match {
            body["ifMatch"] = Value::String(m.to_string());
        }
        let resp = self.with_retries(|| {
            Ok(self
                .http
                .post(&url)
                .timeout(JSON_TIMEOUT)
                .header("X-Canvas-Origin", &self.device_id)
                .json(&body))
        })?;
        Self::json_ok(resp).map(|_| ())
    }

    // ── directories (older home routes) ──────────────────────────────────────

    pub fn mkdir(&self, key: &str) -> Result<(), HubError> {
        let url = format!(
            "{}/rest/v2/workspaces/{}/home/mkdir",
            self.base,
            encode_segment(&self.ws)
        );
        let body = serde_json::json!({ "path": key });
        let resp =
            self.with_retries(|| Ok(self.http.post(&url).timeout(JSON_TIMEOUT).json(&body)))?;
        Self::json_ok(resp).map(|_| ())
    }

    pub fn rmdir(&self, key: &str) -> Result<(), HubError> {
        let url = format!(
            "{}/rest/v2/workspaces/{}/home/{}",
            self.base,
            encode_segment(&self.ws),
            encode_key(key)
        );
        let resp = self.with_retries(|| Ok(self.http.delete(&url).timeout(JSON_TIMEOUT)))?;
        Self::json_ok(resp).map(|_| ())
    }

    // ── status ───────────────────────────────────────────────────────────────

    pub fn report_status(&self, status: &Value) -> Result<u64, HubError> {
        let url = format!(
            "{}/rest/v2/workspaces/{}/mirrors/{}/status",
            self.base,
            encode_segment(&self.ws),
            encode_segment(&self.device_id)
        );
        let resp =
            self.with_retries(|| Ok(self.http.post(&url).timeout(JSON_TIMEOUT).json(status)))?;
        let body = Self::json_ok(resp)?;
        Ok(body
            .get("payload")
            .and_then(|p| p.get("head"))
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }
}

fn parse_stat(v: &Value) -> Option<RemoteStat> {
    Some(RemoteStat {
        sha256: v.get("sha256")?.as_str()?.to_lowercase(),
        size: v.get("size").and_then(Value::as_u64).unwrap_or(0),
        mtime: parse_ms(v.get("mtime")),
    })
}

/// `mtime` arrives as ms (number) or ISO (string) depending on the route.
pub fn parse_ms(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f as u64))
            .unwrap_or(0),
        Some(Value::String(s)) => {
            if let Ok(n) = s.parse::<u64>() {
                n
            } else {
                chrono::DateTime::parse_from_rfc3339(s)
                    .map(|d| d.timestamp_millis().max(0) as u64)
                    .unwrap_or(0)
            }
        }
        _ => 0,
    }
}

fn etag_of(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("x-canvas-sha256")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .or_else(|| {
            headers
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim_start_matches("W/").trim_matches('"').to_lowercase())
        })
}

fn stat_from_headers(headers: &reqwest::header::HeaderMap) -> Option<RemoteStat> {
    let sha256 = etag_of(headers)?;
    let size = headers
        .get("x-canvas-size")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            headers
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0);
    let mtime = headers
        .get("x-canvas-mtime")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Some(RemoteStat {
        sha256,
        size,
        mtime,
    })
}

/// Object keys keep their slashes; each segment is percent-encoded.
pub fn encode_key(key: &str) -> String {
    key.split('/')
        .filter(|s| !s.is_empty())
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn encode_component(s: &str) -> String {
    encode_segment(s)
}

/// Convenience for tests and tools: a cache file path for a `PutBody`.
pub fn body_from_path(path: &Path) -> PutBody {
    PutBody::File(path.to_path_buf())
}
