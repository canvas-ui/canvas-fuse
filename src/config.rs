use anyhow::{Context as _, Result};
use serde_json::Value;
use std::path::PathBuf;

/// Resolved server endpoint + credential.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub server: String,
    pub token: String,
    /// Where the values came from, for status/error messages
    pub source: String,
}

fn canvas_config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".canvas").join("config"))
}

fn read_json(path: &PathBuf) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Resolve server/token with precedence:
/// explicit flags > CANVAS_SERVER/CANVAS_API_TOKEN env > --remote from
/// ~/.canvas/config/remotes.json > boundRemote from cli-session.json.
pub fn resolve(
    server_flag: Option<&str>,
    token_flag: Option<&str>,
    remote_flag: Option<&str>,
) -> Result<Endpoint> {
    let env_server = std::env::var("CANVAS_SERVER").ok();
    let env_token = std::env::var("CANVAS_API_TOKEN").ok();

    let server = server_flag.map(str::to_string).or(env_server);
    let token = token_flag.map(str::to_string).or(env_token);

    if let (Some(server), Some(token)) = (&server, &token) {
        return Ok(Endpoint {
            server: server.clone(),
            token: token.clone(),
            source: "flags/env".to_string(),
        });
    }

    // Fall back to canvas-cli configuration
    let dir = canvas_config_dir().context("cannot determine home directory")?;
    let remotes = read_json(&dir.join("remotes.json"))
        .with_context(|| format!("no usable config in {} (pass --server/--token, set CANVAS_SERVER/CANVAS_API_TOKEN, or log in with canvas-cli)", dir.display()))?;

    let remote_name = match remote_flag {
        Some(name) => name.to_string(),
        None => read_json(&dir.join("cli-session.json"))
            .and_then(|s| {
                s.get("boundRemote")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .context("no --remote given and no boundRemote in cli-session.json")?,
    };

    let remote = remotes
        .get(&remote_name)
        .with_context(|| {
            let known: Vec<String> = remotes
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            format!("remote \"{remote_name}\" not found in remotes.json (known: {})", known.join(", "))
        })?;

    let remote_server = server
        .or_else(|| {
            remote
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .with_context(|| format!("remote \"{remote_name}\" has no url"))?;

    // Prefer auth.token, fall back to the device token; both are accepted
    // by the server's REST and ws auth paths
    let remote_token = token
        .or_else(|| {
            remote
                .get("auth")
                .and_then(|a| a.get("token"))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            remote
                .get("device")
                .and_then(|d| d.get("token"))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        })
        .with_context(|| format!("remote \"{remote_name}\" has no token"))?;

    Ok(Endpoint {
        server: remote_server,
        token: remote_token,
        source: format!("remote {remote_name}"),
    })
}

/// Resolve only the server URL (for unauthenticated commands like ping).
pub fn resolve_server(server_flag: Option<&str>, remote_flag: Option<&str>) -> Result<String> {
    resolve(server_flag, Some(""), remote_flag).map(|e| e.server)
}
