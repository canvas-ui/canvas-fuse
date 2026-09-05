//! Control channel for the CLI subcommands (`sync now`, `pin`, `conflicts`,
//! `trash`): a unix socket next to the mount's state file
//! (`<state dir>/mounts/<name>.<hash>.sock`). One request per connection —
//! a JSON object on one line — answered with one JSON object.
//!
//! A socket rather than a signal + command file because every command has
//! an answer (`pin list`, `conflicts`, "sync finished") and a signal has
//! none. The daemon owns its redb exclusively, so the CLI cannot read the
//! store itself; it asks.
//!
//! Requests: `{"cmd":"sync"}`, `{"cmd":"pin","op":"add|rm|list","glob":…}`,
//! `{"cmd":"conflicts"}`, `{"cmd":"trash","op":"list|restore","key":…}`,
//! `{"cmd":"status"}`. Responses: `{"ok":true, …}` or
//! `{"ok":false,"error":"…"}`.

use super::sync::Mirror;
use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct Control {
    path: PathBuf,
}

impl Control {
    pub fn spawn(path: &Path, mirror: Arc<Mirror>, stop: Arc<AtomicBool>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A socket file left by a crashed daemon would refuse the bind.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)
            .with_context(|| format!("binding control socket {}", path.display()))?;
        listener.set_nonblocking(true)?;
        let sock_path = path.to_path_buf();
        std::thread::Builder::new()
            .name("canvas-fuse-control".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let m = mirror.clone();
                            // Each request on its own thread: `sync now` can
                            // take a while and must not block `status`.
                            let _ = std::thread::Builder::new()
                                .name("canvas-fuse-control-req".into())
                                .spawn(move || handle(stream, &m));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        Err(e) => {
                            log::warn!("control socket accept: {e}");
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                }
                let _ = std::fs::remove_file(&sock_path);
            })?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn handle(mut stream: UnixStream, mirror: &Mirror) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        if reader.read_line(&mut line).is_err() {
            return;
        }
    }
    let req: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(
                stream,
                "{}",
                json!({ "ok": false, "error": format!("bad request: {e}") })
            );
            return;
        }
    };
    let resp = dispatch(&req, mirror);
    let _ = writeln!(stream, "{resp}");
}

fn dispatch(req: &Value, mirror: &Mirror) -> Value {
    let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    match cmd {
        "status" => json!({ "ok": true, "mirror": mirror.status() }),
        "sync" => {
            let finished = mirror.sync_now(Duration::from_secs(600));
            json!({ "ok": true, "finished": finished, "mirror": mirror.status() })
        }
        "pin" => match op {
            "list" => json!({ "ok": true, "pins": mirror.pins() }),
            "add" => {
                let Some(glob) = req.get("glob").and_then(Value::as_str) else {
                    return json!({ "ok": false, "error": "glob required" });
                };
                match mirror.add_pin(glob) {
                    Ok(()) => json!({ "ok": true, "pins": mirror.pins() }),
                    Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
                }
            }
            "rm" => {
                let Some(glob) = req.get("glob").and_then(Value::as_str) else {
                    return json!({ "ok": false, "error": "glob required" });
                };
                match mirror.remove_pin(glob) {
                    Ok(had) => json!({ "ok": true, "removed": had, "pins": mirror.pins() }),
                    Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
                }
            }
            _ => json!({ "ok": false, "error": "pin op must be add|rm|list" }),
        },
        "conflicts" => json!({ "ok": true, "conflicts": mirror.conflicts() }),
        "trash" => match op {
            "list" => {
                let items: Vec<Value> = mirror
                    .trash_list()
                    .into_iter()
                    .map(|(key, t)| {
                        json!({
                            "key": key,
                            "sha256": t.sha256,
                            "size": t.size,
                            "ts": t.ts,
                            "deletedAt": chrono::DateTime::<chrono::Utc>::from(
                                std::time::UNIX_EPOCH + Duration::from_millis(t.ts)
                            ).to_rfc3339(),
                            "cached": mirror.cache.has(&t.sha256),
                        })
                    })
                    .collect();
                json!({ "ok": true, "trash": items })
            }
            "restore" => {
                let Some(key) = req.get("key").and_then(Value::as_str) else {
                    return json!({ "ok": false, "error": "key required" });
                };
                match mirror.trash_restore(&super::normalize_key(key)) {
                    Ok(()) => json!({ "ok": true, "restored": key }),
                    Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
                }
            }
            _ => json!({ "ok": false, "error": "trash op must be list|restore" }),
        },
        other => json!({ "ok": false, "error": format!("unknown command {other:?}") }),
    }
}

/// CLI side: send one request, read one response.
pub fn request(path: &Path, req: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(path).with_context(|| {
        format!(
            "connecting to {} (is the mount running in --mirror mode?)",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(660)))?;
    writeln!(stream, "{req}")?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let resp: Value = serde_json::from_str(line.trim()).context("invalid response")?;
    if resp.get("ok").and_then(Value::as_bool) != Some(true) {
        anyhow::bail!(
            "{}",
            resp.get("error")
                .and_then(Value::as_str)
                .unwrap_or("request failed")
        );
    }
    Ok(resp)
}
