use anyhow::{Context as _, Result};
use serde_json::Value;
use std::path::PathBuf;

/// Resolved server endpoint + credential.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub server: String,
    pub token: String,
    pub tls: Option<crate::tls::ClientIdentity>,
    /// Where the values came from, for status/error messages
    pub source: String,
}

fn canvas_config_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CANVAS_USER_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home).join("config"));
    }
    dirs::home_dir().map(|h| {
        h.join(if cfg!(windows) { "Canvas" } else { ".canvas" })
            .join("config")
    })
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
    resolve_with_tls(server_flag, token_flag, remote_flag, None, None)
}

pub fn resolve_with_tls(
    server_flag: Option<&str>,
    token_flag: Option<&str>,
    remote_flag: Option<&str>,
    cert_flag: Option<&str>,
    key_flag: Option<&str>,
) -> Result<Endpoint> {
    let env_cert = std::env::var("CANVAS_TLS_CERT")
        .ok()
        .filter(|v| !v.is_empty());
    let env_key = std::env::var("CANVAS_TLS_KEY")
        .ok()
        .filter(|v| !v.is_empty());
    let tls_override = if cert_flag.is_some() || key_flag.is_some() {
        Some((cert_flag.map(str::to_string), key_flag.map(str::to_string)))
    } else if env_cert.is_some() || env_key.is_some() {
        Some((env_cert, env_key))
    } else {
        None
    };
    let tls_override = tls_override.map(|(cert, key)| -> Result<crate::tls::TlsFiles> {
        Ok(crate::tls::TlsFiles { cert_file: cert.context("provide both --tls-cert and --tls-key / CANVAS_TLS_CERT and CANVAS_TLS_KEY")?.into(), key_file: key.context("provide both --tls-cert and --tls-key / CANVAS_TLS_CERT and CANVAS_TLS_KEY")?.into() })
    }).transpose()?;
    let env_server = std::env::var("CANVAS_SERVER")
        .ok()
        .filter(|v| !v.is_empty());
    let env_token = std::env::var("CANVAS_API_TOKEN")
        .ok()
        .filter(|v| !v.is_empty());

    let server = server_flag.map(str::to_string).or(env_server);
    let token = token_flag.map(str::to_string).or(env_token);

    if let (Some(server), Some(token), None) = (&server, &token, remote_flag) {
        return Ok(Endpoint {
            server: server.clone(),
            token: token.clone(),
            tls: tls_override
                .as_ref()
                .map(|files| files.load(server))
                .transpose()?,
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

    let remote = remotes.get(&remote_name).with_context(|| {
        let known: Vec<String> = remotes
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        format!(
            "remote \"{remote_name}\" not found in remotes.json (known: {})",
            known.join(", ")
        )
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

    let files = match tls_override {
        Some(files) => Some(files),
        None => remote
            .get("tls")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value::<crate::tls::TlsFiles>(v.clone()))
            .transpose()
            .context("invalid remote TLS configuration")?,
    };
    if files.is_some()
        && (server_flag.is_some()
            || !std::env::var("CANVAS_SERVER")
                .unwrap_or_default()
                .is_empty())
    {
        // URL overrides must not silently inherit an identity for a different host.
        if cert_flag.is_none()
            && key_flag.is_none()
            && std::env::var("CANVAS_TLS_CERT")
                .unwrap_or_default()
                .is_empty()
        {
            if let Some(original) = remote.get("url").and_then(Value::as_str) {
                anyhow::ensure!(reqwest::Url::parse(original)?.origin() == reqwest::Url::parse(&remote_server)?.origin(), "server override changes origin; provide explicit TLS files instead of inheriting the remote identity");
            }
        }
    }
    let tls = files
        .as_ref()
        .map(|files| files.load(&remote_server))
        .transpose()?;
    Ok(Endpoint {
        tls,
        server: remote_server,
        token: remote_token,
        source: format!("remote {remote_name}"),
    })
}

/// Resolve only the server URL (for unauthenticated commands like ping).
pub fn resolve_server(server_flag: Option<&str>, remote_flag: Option<&str>) -> Result<String> {
    resolve(server_flag, Some(""), remote_flag).map(|e| e.server)
}
