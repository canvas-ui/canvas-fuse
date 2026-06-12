use anyhow::{Context as _, Result};
use serde_json::Value;
use std::time::SystemTime;

const PAGE_SIZE: usize = 500;

#[derive(Debug, Clone)]
pub struct ContextInfo {
    pub id: String,
    pub url: String,
    pub workspace_id: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct Document {
    pub id: u64,
    pub schema: String,
    pub data: Value,
    pub updated_at: SystemTime,
    /// locations[].url — for file docs the basename is the display name
    pub locations: Vec<String>,
    /// metadata.size — getattr size for blob-backed docs
    pub size: Option<u64>,
    /// checksumArray[0] — blob cache key (content-addressed dedupe)
    pub checksum: Option<String>,
}

pub struct ApiClient {
    http: reqwest::blocking::Client,
    base: String,
    token: String,
}

impl ApiClient {
    pub fn new(server: &str, token: &str) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base: server.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    pub fn server(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .with_context(|| format!("GET {url}: invalid JSON (HTTP {status})"))?;
        if !status.is_success() {
            anyhow::bail!(
                "GET {url}: HTTP {status}: {}",
                body.get("message").and_then(Value::as_str).unwrap_or("?")
            );
        }
        Ok(body)
    }

    pub fn list_contexts(&self) -> Result<Vec<ContextInfo>> {
        let body = self.get_json("/rest/v2/contexts")?;
        let payload = body
            .get("payload")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for ctx in payload {
            let Some(id) = ctx.get("id").and_then(Value::as_str) else {
                continue;
            };
            let url = ctx.get("url").and_then(Value::as_str).unwrap_or("/");
            out.push(ContextInfo {
                id: id.to_string(),
                url: url.to_string(),
                workspace_id: ctx
                    .get("workspaceId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                raw: ctx.clone(),
            });
        }
        Ok(out)
    }

    /// Unauthenticated server ping; returns (payload, round-trip time).
    pub fn ping(&self) -> Result<(Value, std::time::Duration)> {
        let started = std::time::Instant::now();
        let body = self.get_json("/rest/v2/ping")?;
        let rtt = started.elapsed();
        Ok((body.get("payload").cloned().unwrap_or(Value::Null), rtt))
    }

    pub fn get_context(&self, context_id: &str) -> Result<ContextInfo> {
        let body = self.get_json(&format!("/rest/v2/contexts/{context_id}"))?;
        let payload = body.get("payload").cloned().unwrap_or(Value::Null);
        // payload is either the context object or wraps it as {context: {...}}
        let ctx = payload.get("context").cloned().unwrap_or(payload);
        let id = ctx
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(context_id)
            .to_string();
        let url = ctx
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();
        Ok(ContextInfo {
            id,
            url,
            workspace_id: ctx
                .get("workspaceId")
                .and_then(Value::as_str)
                .map(str::to_string),
            raw: ctx,
        })
    }

    /// Fetch a blob-backed document's bytes via the workspace content route
    /// (server resolves stored:// / file://{WORKSPACE_ROOT} locations).
    pub fn fetch_content(&self, workspace_id: &str, doc_id: u64) -> Result<Vec<u8>> {
        let url = format!(
            "{}/rest/v2/workspaces/{workspace_id}/documents/{doc_id}/content",
            self.base
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET {url}: HTTP {status}");
        }
        Ok(resp.bytes()?.to_vec())
    }

    pub fn list_documents(&self, context_id: &str) -> Result<Vec<Document>> {
        let mut docs = Vec::new();
        let mut offset = 0usize;
        loop {
            let body = self.get_json(&format!(
                "/rest/v2/contexts/{context_id}/documents?limit={PAGE_SIZE}&offset={offset}"
            ))?;
            let batch = extract_documents(&body);
            let batch_len = batch.len();
            docs.extend(batch.iter().filter_map(parse_document));
            let total = body
                .get("totalCount")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            offset += batch_len;
            if batch_len < PAGE_SIZE || (total > 0 && offset >= total) {
                break;
            }
        }
        Ok(docs)
    }
}

// payload is usually the document array itself, but tolerate it being
// wrapped in {data: [...]} or {documents: [...]} depending on ResponseObject path
fn extract_documents(body: &Value) -> Vec<Value> {
    let payload = match body.get("payload") {
        Some(p) => p,
        None => return Vec::new(),
    };
    if let Some(arr) = payload.as_array() {
        return arr.clone();
    }
    for key in ["data", "documents"] {
        if let Some(arr) = payload.get(key).and_then(Value::as_array) {
            return arr.clone();
        }
    }
    Vec::new()
}

fn parse_document(doc: &Value) -> Option<Document> {
    let id = doc.get("id").and_then(Value::as_u64)?;
    let schema = doc.get("schema").and_then(Value::as_str)?.to_string();
    let data = doc.get("data").cloned().unwrap_or(Value::Null);
    let updated_at = doc
        .get("updatedAt")
        .or_else(|| doc.get("createdAt"))
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(SystemTime::from)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let locations = doc
        .get("locations")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("url").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let size = doc
        .get("metadata")
        .and_then(|m| m.get("size"))
        .and_then(Value::as_u64);
    let checksum = doc
        .get("checksumArray")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(Document {
        id,
        schema,
        data,
        updated_at,
        locations,
        size,
        checksum,
    })
}
