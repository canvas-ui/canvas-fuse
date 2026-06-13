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
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct TreeInfo {
    pub id: String,
    pub name: String,
    /// "context" | "directory"
    pub tree_type: String,
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

    fn send_json(&self, method: reqwest::Method, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .request(method.clone(), &url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .with_context(|| format!("{method} {url}"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .with_context(|| format!("{method} {url}: invalid JSON (HTTP {status})"))?;
        if !status.is_success() {
            anyhow::bail!(
                "{method} {url}: HTTP {status}: {}",
                body.get("message").and_then(Value::as_str).unwrap_or("?")
            );
        }
        Ok(body)
    }

    /// Full document JSON (payload of GET /contexts/:id/documents/:docId).
    pub fn get_document(&self, context_id: &str, doc_id: u64) -> Result<Value> {
        let body = self.get_json(&format!(
            "/rest/v2/contexts/{context_id}/documents/{doc_id}"
        ))?;
        Ok(body.get("payload").cloned().unwrap_or(Value::Null))
    }

    /// Insert new documents into a context; returns created doc ids.
    pub fn create_documents(&self, context_id: &str, docs: Vec<Value>) -> Result<Vec<u64>> {
        let body = self.send_json(
            reqwest::Method::POST,
            &format!("/rest/v2/contexts/{context_id}/documents"),
            &serde_json::json!({ "documents": docs }),
        )?;
        Ok(extract_result_ids(&body))
    }

    /// Update existing documents (objects must carry id). Returns the
    /// resulting doc ids — synapsd mints a NEW id when checksum-relevant
    /// fields change (content-addressed versioning), so callers must rebind.
    pub fn update_documents(&self, context_id: &str, docs: Vec<Value>) -> Result<Vec<u64>> {
        let body = self.send_json(
            reqwest::Method::PUT,
            &format!("/rest/v2/contexts/{context_id}/documents"),
            &serde_json::json!({ "documents": docs }),
        )?;
        Ok(extract_result_ids(&body))
    }

    /// Unlink documents from this context (organizational removal).
    pub fn remove_documents(&self, context_id: &str, ids: &[u64]) -> Result<()> {
        self.send_json(
            reqwest::Method::DELETE,
            &format!("/rest/v2/contexts/{context_id}/documents/remove"),
            &serde_json::json!(ids),
        )?;
        Ok(())
    }

    /// Destroy documents in the database (used only for our own transient
    /// docs left behind by editors' atomic-rename save pattern).
    pub fn delete_documents(&self, context_id: &str, ids: &[u64]) -> Result<()> {
        self.send_json(
            reqwest::Method::DELETE,
            &format!("/rest/v2/contexts/{context_id}/documents"),
            &serde_json::json!(ids),
        )?;
        Ok(())
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
            let total = body.get("totalCount").and_then(Value::as_u64).unwrap_or(0) as usize;
            offset += batch_len;
            if batch_len < PAGE_SIZE || (total > 0 && offset >= total) {
                break;
            }
        }
        Ok(docs)
    }

    // ── Workspace tree mount ─────────────────────────────────────────────────

    /// Resolve a workspace by name or id to its canonical id + name.
    pub fn get_workspace(&self, name_or_id: &str) -> Result<WorkspaceInfo> {
        let body = self.get_json(&format!(
            "/rest/v2/workspaces/{}",
            encode_segment(name_or_id)
        ))?;
        let ws = body.get("payload").cloned().unwrap_or(Value::Null);
        let ws = ws.get("workspace").cloned().unwrap_or(ws);
        let id = ws
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(name_or_id)
            .to_string();
        let name = ws
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(name_or_id)
            .to_string();
        Ok(WorkspaceInfo { id, name })
    }

    /// All trees in a workspace (context + directory types).
    pub fn list_trees(&self, ws: &str) -> Result<Vec<TreeInfo>> {
        let body = self.get_json(&format!("/rest/v2/workspaces/{}/trees", encode_segment(ws)))?;
        let payload = body
            .get("payload")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for t in payload {
            let Some(name) = t.get("name").and_then(Value::as_str) else {
                continue;
            };
            let id = t
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_string();
            let tree_type = t
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("context")
                .to_string();
            out.push(TreeInfo {
                id,
                name: name.to_string(),
                tree_type,
            });
        }
        Ok(out)
    }

    /// Flat list of every path present in a tree (e.g. "/", "/foo", "/foo/bar").
    pub fn list_tree_paths(&self, ws: &str, tree: &str) -> Result<Vec<String>> {
        let body = self.get_json(&format!(
            "/rest/v2/workspaces/{}/trees/{}/paths",
            encode_segment(ws),
            encode_segment(tree)
        ))?;
        let payload = body
            .get("payload")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(payload
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect())
    }

    /// Documents linked at one tree path (non-recursive — exactly that node).
    pub fn list_tree_documents(
        &self,
        ws: &str,
        tree: &str,
        tree_type: &str,
        path: &str,
    ) -> Result<Vec<Document>> {
        let mut docs = Vec::new();
        let mut offset = 0usize;
        loop {
            let body = self.get_json(&format!(
                "/rest/v2/workspaces/{}/documents?treeNameOrTreeId={}&treeType={}&context={}&limit={PAGE_SIZE}&offset={offset}",
                encode_segment(ws),
                encode_segment(tree),
                encode_segment(tree_type),
                encode_component(path),
            ))?;
            let batch = extract_documents(&body);
            let batch_len = batch.len();
            docs.extend(batch.iter().filter_map(parse_document));
            let total = body.get("totalCount").and_then(Value::as_u64).unwrap_or(0) as usize;
            offset += batch_len;
            if batch_len < PAGE_SIZE || (total > 0 && offset >= total) {
                break;
            }
        }
        Ok(docs)
    }

    /// Create a directory/context path node (mkdir).
    pub fn insert_tree_path(&self, ws: &str, tree: &str, path: &str) -> Result<()> {
        self.send_json(
            reqwest::Method::PUT,
            &format!(
                "/rest/v2/workspaces/{}/trees/{}/path/{}",
                encode_segment(ws),
                encode_segment(tree),
                encode_tree_path(path)
            ),
            &serde_json::json!({}),
        )?;
        Ok(())
    }

    /// Remove a path node (rmdir / rm -r).
    pub fn remove_tree_path(
        &self,
        ws: &str,
        tree: &str,
        path: &str,
        recursive: bool,
    ) -> Result<()> {
        self.send_json(
            reqwest::Method::DELETE,
            &format!(
                "/rest/v2/workspaces/{}/trees/{}/path/{}?recursive={recursive}",
                encode_segment(ws),
                encode_segment(tree),
                encode_tree_path(path)
            ),
            &Value::Null,
        )?;
        Ok(())
    }

    /// Move/rename a path node within a tree (mv of a folder).
    pub fn move_tree_path(&self, ws: &str, tree: &str, from: &str, to: &str) -> Result<()> {
        self.send_json(
            reqwest::Method::PATCH,
            &format!(
                "/rest/v2/workspaces/{}/trees/{}/path/{}",
                encode_segment(ws),
                encode_segment(tree),
                encode_tree_path(from)
            ),
            &serde_json::json!({ "to": to, "recursive": true }),
        )?;
        Ok(())
    }

    /// Insert a document at a tree path; returns the created doc id(s).
    pub fn put_tree_document(
        &self,
        ws: &str,
        tree: &str,
        tree_type: &str,
        path: &str,
        doc: Value,
    ) -> Result<Vec<u64>> {
        let body = self.send_json(
            reqwest::Method::POST,
            &format!("/rest/v2/workspaces/{}/documents", encode_segment(ws)),
            &serde_json::json!({
                "documents": [doc],
                "treeNameOrTreeId": tree,
                "treeType": tree_type,
                "context": path,
            }),
        )?;
        Ok(extract_result_ids(&body))
    }

    /// Update existing documents at the workspace level (objects carry id).
    pub fn update_workspace_documents(
        &self,
        ws: &str,
        tree: &str,
        tree_type: &str,
        path: &str,
        docs: Vec<Value>,
    ) -> Result<Vec<u64>> {
        let body = self.send_json(
            reqwest::Method::PUT,
            &format!("/rest/v2/workspaces/{}/documents", encode_segment(ws)),
            &serde_json::json!({
                "documents": docs,
                "treeNameOrTreeId": tree,
                "treeType": tree_type,
                "context": path,
            }),
        )?;
        Ok(extract_result_ids(&body))
    }

    /// Full document JSON at the workspace level (for GET-merge-PUT edits).
    pub fn get_workspace_document(&self, ws: &str, doc_id: u64) -> Result<Value> {
        let body = self.get_json(&format!(
            "/rest/v2/workspaces/{}/documents/{doc_id}",
            encode_segment(ws)
        ))?;
        Ok(body.get("payload").cloned().unwrap_or(Value::Null))
    }

    /// Unlink documents from a tree path (organizational removal, like `rm`).
    pub fn remove_tree_document(
        &self,
        ws: &str,
        tree: &str,
        tree_type: &str,
        path: &str,
        ids: &[u64],
    ) -> Result<()> {
        self.send_json(
            reqwest::Method::DELETE,
            &format!(
                "/rest/v2/workspaces/{}/documents/remove?treeNameOrTreeId={}&treeType={}&context={}",
                encode_segment(ws),
                encode_segment(tree),
                encode_segment(tree_type),
                encode_component(path)
            ),
            &serde_json::json!(ids),
        )?;
        Ok(())
    }
}

/// Percent-encode one URL path segment (no '/' allowed through).
fn encode_segment(s: &str) -> String {
    encode_with(s, false)
}

/// Percent-encode a query-string value.
fn encode_component(s: &str) -> String {
    encode_with(s, false)
}

/// Encode a tree path into splat form for `/path/*` routes: each segment
/// percent-encoded, joined by literal '/', leading slash stripped.
fn encode_tree_path(path: &str) -> String {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| encode_with(seg, false))
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_with(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// putMany/linkMany result: {successful: [{index, id}], failed: [...]} — but
// tolerate a bare array of ids or docs
fn extract_result_ids(body: &Value) -> Vec<u64> {
    let payload = match body.get("payload") {
        Some(p) => p,
        None => return Vec::new(),
    };
    if let Some(arr) = payload.get("successful").and_then(Value::as_array) {
        return arr
            .iter()
            .filter_map(|e| e.get("id").and_then(Value::as_u64).or_else(|| e.as_u64()))
            .collect();
    }
    if let Some(arr) = payload.as_array() {
        return arr
            .iter()
            .filter_map(|e| e.as_u64().or_else(|| e.get("id").and_then(Value::as_u64)))
            .collect();
    }
    Vec::new()
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
