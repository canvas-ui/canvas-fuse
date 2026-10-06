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

/// A blob stored in the workspace, as a File document references it.
#[derive(Debug, Clone)]
pub struct BlobRef {
    pub url: String,
    pub checksum: Option<String>,
    pub size: u64,
    pub mime_type: Option<String>,
}

/// One entry of the workspace home drive — a real file or folder, not a
/// document. Home is a passthrough: the file IS the file.
#[derive(Debug, Clone)]
pub struct HomeEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub mtime: Option<String>,
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
    /// locations[].url — where the bytes are, NOT where the name comes from.
    pub locations: Vec<String>,
    /// The document's display name, resolved server-style at parse time (see
    /// `resolve_display_name`). None when nothing names it and the renderer
    /// must derive one.
    pub display_name: Option<String>,
    /// Whether the server says this document is filed AT the path being listed,
    /// rather than showing through from a path below it (a context path lists
    /// its whole subtree). Decides who keeps the plain filename when two
    /// documents in one folder answer to the same name.
    ///
    /// True when the server said nothing — an older server, or a listing with
    /// no placement to report. That is the old behaviour: everything counts as
    /// filed here, and id order breaks the tie.
    pub linked_here: bool,
    /// metadata.size — getattr size for blob-backed docs
    pub size: Option<u64>,
    /// checksumArray[0] — blob cache key (content-addressed dedupe)
    pub checksum: Option<String>,
    /// The whole record, kept ONLY for schemas this build has no renderer for —
    /// those are served as their own JSON, and `data` alone is not the record.
    /// None for everything else, so the common case carries no second copy.
    pub raw: Option<Value>,
}

pub struct ApiClient {
    http: reqwest::blocking::Client,
    base: String,
    token: String,
}

impl ApiClient {
    pub fn new(server: &str, token: &str) -> Result<Self> {
        Self::with_tls(server, token, None)
    }

    pub fn with_tls(
        server: &str,
        token: &str,
        identity: Option<&crate::tls::ClientIdentity>,
    ) -> Result<Self> {
        let http = crate::tls::http_builder(server, identity)?
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

    /// Store bytes in the workspace blob store and get back the location a File
    /// document references. The byte half of writing a plain file.
    pub fn upload_blob(&self, ws: &str, bytes: Vec<u8>) -> Result<BlobRef> {
        self.post_blob(
            &format!(
                "{}/rest/v2/workspaces/{}/blobs",
                self.base,
                encode_segment(ws)
            ),
            bytes,
        )
    }

    /// The same, addressed by context. The bytes land in the context's backing
    /// workspace either way — a context is a view, not a place things live —
    /// but a context mount has a context id and no workspace name, and this
    /// route answers to the context's own permissions.
    pub fn upload_context_blob(&self, context_id: &str, bytes: Vec<u8>) -> Result<BlobRef> {
        self.post_blob(
            &format!(
                "{}/rest/v2/contexts/{}/blobs",
                self.base,
                encode_segment(context_id)
            ),
            bytes,
        )
    }

    fn post_blob(&self, url: &str, bytes: Vec<u8>) -> Result<BlobRef> {
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .with_context(|| format!("POST {url}: invalid JSON (HTTP {status})"))?;
        if !status.is_success() {
            anyhow::bail!(
                "POST {url}: HTTP {status}: {}",
                body.get("message").and_then(Value::as_str).unwrap_or("?")
            );
        }
        let payload = body.get("payload").unwrap_or(&body);
        Ok(BlobRef {
            url: payload
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("blob upload returned no url"))?
                .to_string(),
            checksum: payload
                .get("checksum")
                .and_then(Value::as_str)
                .map(str::to_string),
            size: payload.get("size").and_then(Value::as_u64).unwrap_or(0),
            mime_type: payload
                .get("mimeType")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    // ── Home drive ───────────────────────────────────────────────────────────
    // Real files, addressed by path. Reads take a byte window (the server
    // honours Range), writes replace whole files — the same shape the write
    // overlay already uses for documents.

    pub fn list_home(&self, ws: &str, path: &str) -> Result<Vec<HomeEntry>> {
        let body = self.get_json(&format!(
            "/rest/v2/workspaces/{}/home/{}",
            encode_segment(ws),
            encode_path(path)
        ))?;
        let entries = body
            .get("payload")
            .and_then(|p| p.get("entries"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(entries
            .iter()
            .filter_map(|e| {
                Some(HomeEntry {
                    name: e.get("name").and_then(Value::as_str)?.to_string(),
                    size: e.get("size").and_then(Value::as_u64).unwrap_or(0),
                    is_dir: e
                        .get("isDirectory")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    mtime: e.get("mtime").and_then(Value::as_str).map(str::to_string),
                })
            })
            .collect())
    }

    /// A byte window of a home file. `end` is inclusive, as in the HTTP header.
    pub fn read_home_range(&self, ws: &str, path: &str, start: u64, end: u64) -> Result<Vec<u8>> {
        let url = format!(
            "{}/rest/v2/workspaces/{}/home/{}?download",
            self.base,
            encode_segment(ws),
            encode_path(path)
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
            .send()
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET {url}: HTTP {status}");
        }
        let body = resp.bytes()?.to_vec();
        // A server that ignored the Range answered with the whole file; trim so
        // the caller always gets the window it asked for.
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            Ok(body)
        } else {
            let from = (start as usize).min(body.len());
            let to = ((end as usize).saturating_add(1)).min(body.len());
            Ok(body[from..to].to_vec())
        }
    }

    pub fn write_home(&self, ws: &str, path: &str, bytes: Vec<u8>) -> Result<()> {
        let url = format!(
            "{}/rest/v2/workspaces/{}/home/{}",
            self.base,
            encode_segment(ws),
            encode_path(path)
        );
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .with_context(|| format!("PUT {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("PUT {url}: HTTP {status}");
        }
        Ok(())
    }

    pub fn mkdir_home(&self, ws: &str, path: &str) -> Result<()> {
        self.send_json(
            reqwest::Method::POST,
            &format!("/rest/v2/workspaces/{}/home/mkdir", encode_segment(ws)),
            // Relative to the drive root: the server reads a leading slash as
            // an attempt to escape it. Every other home call goes through
            // encode_path(), which trims for the same reason.
            &serde_json::json!({ "path": path.trim_start_matches('/') }),
        )?;
        Ok(())
    }

    pub fn remove_home(&self, ws: &str, path: &str) -> Result<()> {
        let url = format!(
            "{}/rest/v2/workspaces/{}/home/{}",
            self.base,
            encode_segment(ws),
            encode_path(path)
        );
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .with_context(|| format!("DELETE {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("DELETE {url}: HTTP {status}");
        }
        Ok(())
    }

    /// Rename a file on a path-addressed backend (`POST …/objects/rename`,
    /// the sync protocol's route): same bytes, same document, new key. The
    /// hub renames files only; a folder move is one call per file.
    pub fn rename_object(
        &self,
        ws: &str,
        backend: &str,
        from: &str,
        to: &str,
        if_match: Option<&str>,
    ) -> Result<()> {
        let mut body = serde_json::json!({ "from": from, "to": to });
        if let Some(m) = if_match {
            body["ifMatch"] = Value::String(m.to_string());
        }
        self.send_json(
            reqwest::Method::POST,
            &format!(
                "/rest/v2/workspaces/{}/backends/file/{}/objects/rename",
                encode_segment(ws),
                encode_segment(backend)
            ),
            &body,
        )?;
        Ok(())
    }

    /// Permanently delete documents that are in the trash. The one call that
    /// destroys — same as "delete from trash" in a file manager.
    pub fn empty_trash(&self, ws: &str, ids: &[u64]) -> Result<()> {
        self.send_json(
            reqwest::Method::DELETE,
            &format!("/rest/v2/workspaces/{}/trash", encode_segment(ws)),
            &serde_json::json!({ "documentIds": ids }),
        )?;
        Ok(())
    }

    /// Documents sitting in the workspace trash — what a filesystem delete
    /// parked there when it removed a document's last placement.
    pub fn list_trash(&self, ws: &str) -> Result<Vec<Document>> {
        let body = self.get_json(&format!("/rest/v2/workspaces/{}/trash", encode_segment(ws)))?;
        Ok(extract_documents(&body)
            .iter()
            .filter_map(parse_document)
            .collect())
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

    /// File an EXISTING document at a tree path.
    ///
    /// The link half of a move: `POST /documents` with `documentIds` adds a
    /// placement without touching content, so a cross-directory move costs two
    /// small requests instead of streaming the bytes through the mount.
    pub fn link_tree_document(
        &self,
        ws: &str,
        tree: &str,
        tree_type: &str,
        path: &str,
        ids: &[u64],
    ) -> Result<()> {
        self.send_json(
            reqwest::Method::POST,
            &format!("/rest/v2/workspaces/{}/documents", encode_segment(ws)),
            &serde_json::json!({
                "documentIds": ids,
                "treeNameOrTreeId": tree,
                "treeType": tree_type,
                "context": path,
            }),
        )?;
        Ok(())
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
    /// Detach documents from a tree path.
    ///
    /// `trash_if_orphaned` applies the filesystem rule the server owns: when
    /// this removes a document's LAST placement it is filed into the workspace
    /// trash instead of becoming reachable only through the flat
    /// workspace-wide list. `rm` on a mount means "take it out of this folder",
    /// and nothing a mount does should make a document unreachable.
    pub fn remove_tree_document(
        &self,
        ws: &str,
        tree: &str,
        tree_type: &str,
        path: &str,
        ids: &[u64],
        trash_if_orphaned: bool,
    ) -> Result<()> {
        let trash = if trash_if_orphaned {
            "&trashIfOrphaned=true"
        } else {
            ""
        };
        self.send_json(
            reqwest::Method::DELETE,
            &format!(
                "/rest/v2/workspaces/{}/documents/remove?treeNameOrTreeId={}&treeType={}&context={}{}",
                encode_segment(ws),
                encode_segment(tree),
                encode_segment(tree_type),
                encode_component(path),
                trash
            ),
            &serde_json::json!(ids),
        )?;
        Ok(())
    }
}

/// Percent-encode a multi-segment path, keeping the separators.
fn encode_path(path: &str) -> String {
    path.trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
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

/// The name a document should be shown under, in the server's order (see
/// `displayFilename()` in transports/webdav/vfs-shared.js — the two must agree
/// or the same file is called different things on the two wires):
///
///   1. `metadata.filename` — the document's own name, set by a rename;
///   2. `data.filename` — the same for JSON abstractions;
///   3. the name on the canvas-owned copy (`stored://workspace:*`);
///   4. any location name, by a STABLE sort of the url — never array order,
///      which is rebuilt per backend scan;
///
/// A `stored://` key is a content HASH and is never a name; the renderer falls
/// back to deriving one.
fn resolve_display_name(doc: &Value) -> Option<String> {
    let trimmed = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    if let Some(name) = trimmed(doc.get("metadata").and_then(|m| m.get("filename"))) {
        return Some(name);
    }
    if let Some(name) = trimmed(doc.get("data").and_then(|d| d.get("filename"))) {
        return Some(name);
    }

    let locations = doc.get("locations").and_then(Value::as_array)?;
    let named = |loc: &Value| trimmed(loc.get("metadata").and_then(|m| m.get("filename")));

    if let Some(name) = locations
        .iter()
        .find(|loc| {
            loc.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with("stored://workspace:"))
        })
        .and_then(named)
    {
        return Some(name);
    }

    let mut sorted: Vec<&Value> = locations.iter().collect();
    sorted.sort_by_key(|loc| {
        loc.get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    });
    sorted.iter().find_map(|loc| named(loc))
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
    // The document's own size first, then any location that measured what it
    // holds — IMAP records the raw message size on the location, not on the
    // document. Same order as the server's storedSize().
    let size = doc
        .get("metadata")
        .and_then(|m| m.get("size"))
        .and_then(Value::as_u64)
        .or_else(|| {
            doc.get("locations")
                .and_then(Value::as_array)?
                .iter()
                .find_map(|loc| loc.get("metadata")?.get("size")?.as_u64())
        });
    let checksum = doc
        .get("checksumArray")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_string);
    let display_name = resolve_display_name(doc);
    let linked_here = doc
        .get("linkedHere")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let raw = if crate::render::has_renderer(&schema) {
        None
    } else {
        Some(doc.clone())
    };
    Some(Document {
        id,
        display_name,
        schema,
        data,
        updated_at,
        locations,
        linked_here,
        size,
        checksum,
        raw,
    })
}
