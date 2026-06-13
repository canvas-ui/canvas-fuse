use crate::api::Document;
use serde_json::Value;

/// Schema dirs always present under every context dir, even when empty —
/// apps anchor to these paths (e.g. an Obsidian vault rooted at Notes/),
/// so the skeleton must not vanish on a context switch.
pub const SCHEMA_DIRS: &[&str] = &[
    "Tabs", "Notes", "Todos", "Files", "Emails", "Links", "Other",
];

pub enum Content {
    /// Bytes rendered locally from the document JSON
    Inline(Vec<u8>),
    /// Blob served by the workspace content route, fetched lazily on read
    Remote { size: u64 },
}

pub struct Rendered {
    pub dir: &'static str,
    pub base_name: String,
    pub content: Content,
}

impl Rendered {
    fn inline(dir: &'static str, base_name: String, content: Vec<u8>) -> Self {
        Self {
            dir,
            base_name,
            content: Content::Inline(content),
        }
    }
}

pub fn render(doc: &Document) -> Rendered {
    match doc.schema.as_str() {
        "data/abstraction/tab" => render_link(doc, "Tabs"),
        "data/abstraction/link" => render_link(doc, "Links"),
        "data/abstraction/note" => render_note(doc),
        "data/abstraction/todo" => render_todo(doc),
        "data/abstraction/file" => render_file(doc),
        "data/abstraction/email" => render_json(doc, "Emails"),
        _ => render_json(doc, "Other"),
    }
}

/// File docs are pure blobs: the display name lives in the location URLs
/// (file://{WORKSPACE_ROOT}/path/name.ext, stored://backend/key), the bytes
/// come from the server's content route. Without a known size we cannot
/// promise reads (getattr must match), so fall back to JSON metadata.
fn render_file(doc: &Document) -> Rendered {
    let name = doc
        .locations
        .iter()
        .find_map(|url| location_basename(url))
        .unwrap_or_else(|| format!("file-{}", doc.id));
    match doc.size {
        Some(size) => Rendered {
            dir: "Files",
            base_name: sanitize_filename(&name),
            content: Content::Remote { size },
        },
        None => Rendered {
            dir: "Files",
            base_name: format!("{}.json", sanitize_filename(&name)),
            content: Content::Inline(serde_json::to_vec_pretty(&doc.data).unwrap_or_default()),
        },
    }
}

/// Basename of a location URL path, percent-decoded. Returns None for URLs
/// without a usable final segment.
fn location_basename(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let path = rest.split(['?', '#']).next()?;
    let base = path.rsplit('/').next()?;
    if base.is_empty() {
        return None;
    }
    Some(percent_decode(base))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Keep real filenames (incl. spaces, unicode) but strip path separators and
/// characters that break shells/filesystems outright.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '\0'))
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn str_field<'a>(data: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| data.get(k).and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
}

fn render_link(doc: &Document, dir: &'static str) -> Rendered {
    let url = str_field(&doc.data, &["url"]).unwrap_or("about:blank");
    let title = str_field(&doc.data, &["title"])
        .map(str::to_string)
        .or_else(|| host_of(url))
        .unwrap_or_else(|| format!("tab-{}", doc.id));
    Rendered::inline(
        dir,
        format!("{}.url", slug(&title)),
        format!("[InternetShortcut]\r\nURL={url}\r\n").into_bytes(),
    )
}

fn render_note(doc: &Document) -> Rendered {
    let title = str_field(&doc.data, &["title"])
        .map(str::to_string)
        .unwrap_or_else(|| format!("note-{}", doc.id));
    let mut content = str_field(&doc.data, &["content"]).unwrap_or("").to_string();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    Rendered::inline(
        "Notes",
        format!("{}.md", slug(&title)),
        content.into_bytes(),
    )
}

fn render_todo(doc: &Document) -> Rendered {
    let title = str_field(&doc.data, &["title"])
        .map(str::to_string)
        .unwrap_or_else(|| format!("todo-{}", doc.id));
    let done = doc
        .data
        .get("completed")
        .or_else(|| doc.data.get("done"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mark = if done { "x" } else { " " };
    let mut content = format!("- [{mark}] {title}\n");
    if let Some(desc) = str_field(&doc.data, &["description"]) {
        content.push('\n');
        content.push_str(desc);
        content.push('\n');
    }
    Rendered::inline(
        "Todos",
        format!("{}.md", slug(&title)),
        content.into_bytes(),
    )
}

fn render_json(doc: &Document, dir: &'static str) -> Rendered {
    let title = str_field(&doc.data, &["title", "name", "subject", "filename"])
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}-{}", short_schema(&doc.schema), doc.id));
    let content = serde_json::to_vec_pretty(&doc.data).unwrap_or_default();
    Rendered::inline(dir, format!("{}.json", slug(&title)), content)
}

fn short_schema(schema: &str) -> &str {
    schema.rsplit('/').next().unwrap_or("doc")
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Filesystem-safe, human-readable name. Must be pure: same input, same output,
/// across runs and devices — collision handling happens in the view builder.
pub fn slug(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for ch in input.trim().chars() {
        let mapped = match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '(' | ')' | '[' | ']' => Some(ch),
            ' ' | '\t' | '-' | '/' | '\\' | ':' | '|' => None,
            _ if ch.is_alphanumeric() => Some(ch), // keep unicode letters/digits
            _ => continue,
        };
        match mapped {
            Some(c) => {
                out.push(c);
                last_dash = false;
            }
            None => {
                if !last_dash && !out.is_empty() {
                    out.push('-');
                    last_dash = true;
                }
            }
        }
        if out.chars().count() >= 80 {
            break;
        }
    }
    let trimmed = out.trim_matches(['-', '.']).to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

/// Insert a collision suffix before the extension: notes.md + 123 -> notes.123.md
pub fn with_id_suffix(base: &str, id: u64) -> String {
    match base.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}.{id}.{ext}"),
        None => format!("{base}.{id}"),
    }
}
