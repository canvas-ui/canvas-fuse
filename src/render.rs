use crate::api::Document;
use serde_json::Value;

/// Schema ids are hierarchical under this prefix (`data/schema/message/email`).
const DATA_PREFIX: &str = "data/schema/";

const NOTE_SCHEMA: &str = "data/schema/note";
const TODO_SCHEMA: &str = "data/schema/task";
const TAB_SCHEMA: &str = "data/schema/tab";
const LINK_SCHEMA: &str = "data/schema/link";
const FILE_SCHEMA: &str = "data/schema/file";
const EMAIL_SCHEMA: &str = "data/schema/message/email";

pub enum Content {
    /// Bytes rendered locally from the document JSON
    Inline(Vec<u8>),
    /// Blob served by the workspace content route, fetched lazily on read.
    /// `size` is None when nothing records one — resolved lazily from the blob
    /// so the file is still shown as-is (not a .json stub).
    Remote { size: Option<u64> },
}

pub struct Rendered {
    /// Folder this document falls into in the derived `.by-schema/` view. The
    /// flat view — what a context actually IS — ignores it.
    pub dir: String,
    pub base_name: String,
    pub content: Content,
}

/// Whether this crate renders a schema as something other than its raw JSON.
/// Documents outside the list are served as the whole record, so the parser
/// keeps their JSON around (see `api::Document::raw`).
pub fn has_renderer(schema: &str) -> bool {
    matches!(
        schema,
        NOTE_SCHEMA | TODO_SCHEMA | TAB_SCHEMA | LINK_SCHEMA | FILE_SCHEMA | EMAIL_SCHEMA
    )
}

/// A document as a file: what it is called, and what its bytes are.
///
/// This mirrors `docName()` + `renderDoc()` in the server's
/// `transports/webdav/vfs-shared.js`. The two must agree: the same document
/// opened over WebDAV and over this mount is one file, and a client that sees
/// it under two names (or with two bodies) has no way to know it is looking at
/// one thing. Any change here is a change there.
pub fn render(doc: &Document) -> Rendered {
    Rendered {
        dir: schema_folder(&doc.schema),
        base_name: doc_name(doc),
        content: content(doc),
    }
}

/// The `.by-schema/` folder for a schema id, derived the way the server derives
/// it: the last id segment, capitalized and pluralized. Deriving beats a table —
/// a new schema gets a folder with no code change, and nothing lands in a
/// catch-all bucket. (A fixed list with an `Other` fallback is how every
/// document that was not one of six known schemas ended up in `Other`.)
fn schema_folder(schema: &str) -> String {
    let slug = schema
        .strip_prefix(DATA_PREFIX)
        .unwrap_or(schema)
        .rsplit('/')
        .next()
        .unwrap_or("doc");
    let mut chars = slug.chars();
    match chars.next() {
        Some(first) => format!("{}{}s", first.to_uppercase(), chars.as_str()),
        None => "Docs".to_string(),
    }
}

/// The name a document is filed under.
///
/// `display_name` is the resolved ladder from the record itself (see
/// `api::resolve_display_name`); only when nothing names the document does the
/// schema derive one.
pub fn doc_name(doc: &Document) -> String {
    if let Some(name) = &doc.display_name {
        return sanitize(name);
    }
    // Nothing named the document, but a location may still carry a name in its
    // path. Sorted, never in array order: `locations` is rebuilt per backend
    // scan, and a file that renames itself because a mirror was added and
    // landed first is a file nobody can keep track of.
    let mut urls: Vec<&str> = doc.locations.iter().map(String::as_str).collect();
    urls.sort_unstable();
    if let Some(name) = urls.iter().find_map(|url| name_bearing_basename(url)) {
        return sanitize(&name);
    }
    match doc.schema.as_str() {
        EMAIL_SCHEMA => email_name(doc),
        NOTE_SCHEMA => format!("{}.md", titled(doc, &["title"], "note")),
        TODO_SCHEMA => format!("{}.todo.json", titled(doc, &["title"], "todo")),
        // A link names its target `uri` and itself `label`; a tab uses
        // `url`/`title`. Both are one address you can open, so both are `.url`.
        TAB_SCHEMA | LINK_SCHEMA => format!(
            "{}.url",
            titled(doc, &["title", "label", "url", "uri"], "link")
        ),
        _ => format!("{}_{}.json", short_schema(&doc.schema), doc.id),
    }
}

/// Basename of a location URL, for schemes where the path IS a name.
///
/// Some schemes never name anything: `imap://<account>/INBOX;UID=56909` is a
/// slot in a mailbox, renumbered by the next resync, and a document named after
/// one tells a person nothing. Those stay anonymous so the schema can derive a
/// real name from the record instead.
///
/// A `stored://` key is only sometimes a name: file-backed keys are the real
/// workspace path (`stored://workspace:home/photos/OM_R2.png`), while cacache
/// and generated keys are content hashes. So a stored key has to look like a
/// filename before it may speak for the document — otherwise a file renames
/// itself to its own checksum.
fn name_bearing_basename(url: &str) -> Option<String> {
    if ADDRESS_ONLY_SCHEMES
        .iter()
        .any(|scheme| url.len() > scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme))
    {
        return None;
    }
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let key = match after_scheme.split_once('/') {
        Some((_, rest)) => rest,
        None => after_scheme,
    };
    let base = key
        .split(['?', '#'])
        .next()?
        .rsplit('/')
        .find(|s| !s.is_empty())?;
    let decoded = percent_decode(base);
    if url.len() >= 9
        && url[..9].eq_ignore_ascii_case("stored://")
        && !looks_like_filename(&decoded)
    {
        return None;
    }
    Some(decoded).filter(|s| !s.trim().is_empty())
}

const ADDRESS_ONLY_SCHEMES: &[&str] = &[
    "imap:", "imaps:", "pop3:", "pop3s:", "mailto:", "graph:", "ews:", "news:", "nntp:",
];

/// A name a person would recognise: has an extension and is not a bare digest.
fn looks_like_filename(base: &str) -> bool {
    let Some((stem, ext)) = base.rsplit_once('.') else {
        return false;
    };
    if ext.is_empty() || ext.len() > 12 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    !(stem.len() >= 16 && stem.chars().all(|c| c.is_ascii_hexdigit()))
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

/// The document's own title, sanitized, or `<kind>-<id>` when it has none.
fn titled(doc: &Document, keys: &[&str], kind: &str) -> String {
    match str_field(&doc.data, keys) {
        Some(title) => sanitize(title),
        None => format!("{kind}-{}", doc.id),
    }
}

/// A message as a file: `<from address>-<subject>.eml`.
///
/// The bytes behind an email document are the raw RFC 822 message, so `.eml` is
/// what it actually is — every mail client opens one. The name has to come from
/// the document because its locations are addresses, not names: left to those,
/// every message on the mount was called `INBOX;UID=56909`, which says nothing
/// and changes the moment the mailbox is renumbered.
fn email_name(doc: &Document) -> String {
    let from = email_address(doc.data.get("from")).unwrap_or_else(|| "unknown".to_string());
    let subject = str_field(&doc.data, &["title", "subject", "name"])
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "no-subject".to_string());
    format!("{}-{subject}.eml", sanitize(&from))
}

/// `from` is a string on some records and `{ address, name }` on others (the
/// Email schema accepts both); recipient fields arrive as lists.
fn email_address(from: Option<&Value>) -> Option<String> {
    match from? {
        Value::String(s) => Some(s.trim().to_lowercase()).filter(|s| !s.is_empty()),
        Value::Array(items) => items.iter().find_map(|v| email_address(Some(v))),
        Value::Object(_) => {
            let obj = from?;
            str_field(obj, &["address", "name"]).map(|s| s.trim().to_lowercase())
        }
        _ => None,
    }
}

pub fn content(doc: &Document) -> Content {
    match doc.schema.as_str() {
        NOTE_SCHEMA => Content::Inline(
            str_field(&doc.data, &["content"])
                .unwrap_or("")
                .as_bytes()
                .to_vec(),
        ),
        TAB_SCHEMA | LINK_SCHEMA => Content::Inline(
            format!(
                "[InternetShortcut]\nURL={}\n",
                str_field(&doc.data, &["url", "uri"]).unwrap_or("")
            )
            .into_bytes(),
        ),
        TODO_SCHEMA => Content::Inline(pretty(&doc.data)),
        EMAIL_SCHEMA => Content::Inline(render_email(doc)),
        // A file is pure bytes: the name came from its locations, the body
        // comes from the workspace content route, so it opens in whatever
        // application owns that extension.
        FILE_SCHEMA => Content::Remote { size: doc.size },
        // Nothing renders this schema — serve the record itself rather than an
        // empty file. `raw` is kept by the parser for exactly this case.
        _ => Content::Inline(pretty(doc.raw.as_ref().unwrap_or(&doc.data))),
    }
}

fn pretty(value: &Value) -> Vec<u8> {
    serde_json::to_vec_pretty(value).unwrap_or_default()
}

/// A message with no raw source, rebuilt as RFC 822 from its fields.
///
/// IMAP-ingested mail keeps the original MIME bytes and streams those (it is a
/// blob, and takes the `Remote` path); mail that arrived through an API (Graph,
/// Gmail) never had them. A file called `.eml` has to open in a mail client
/// either way, so the fields are written back out as a message rather than
/// served as the JSON record.
fn render_email(doc: &Document) -> Vec<u8> {
    let data = &doc.data;
    let headers = [
        ("From", party(data.get("from"))),
        ("To", party(data.get("to"))),
        ("Cc", party(data.get("cc"))),
        (
            "Subject",
            str_field(data, &["subject", "title"])
                .unwrap_or("")
                .to_string(),
        ),
        ("Date", header_date(data)),
        (
            "Message-ID",
            str_field(data, &["messageId"]).unwrap_or("").to_string(),
        ),
        (
            "In-Reply-To",
            str_field(data, &["inReplyTo"]).unwrap_or("").to_string(),
        ),
        ("MIME-Version", "1.0".to_string()),
        ("Content-Type", email_content_type(data).to_string()),
    ];

    let mut out = String::new();
    for (key, value) in headers {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        // A header value must not carry the line breaks that would end it.
        let folded: String = value
            .chars()
            .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
            .collect();
        out.push_str(key);
        out.push_str(": ");
        out.push_str(folded.trim());
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    let body = str_field(data, &["body", "bodyHtml", "bodyPreview"]).unwrap_or("");
    out.push_str(&body.replace("\r\n", "\n").replace('\n', "\r\n"));
    out.push_str("\r\n");
    out.into_bytes()
}

fn email_content_type(data: &Value) -> &'static str {
    let has_html = str_field(data, &["bodyHtml"]).is_some();
    let has_text = str_field(data, &["body"]).is_some();
    if has_html && !has_text {
        "text/html; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// RFC 822 `Date`, in the same shape the server writes (JS `toUTCString()`).
fn header_date(data: &Value) -> String {
    let raw = match str_field(data, &["date", "sentAt", "receivedAt"]) {
        Some(s) => s,
        None => return String::new(),
    };
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => dt
            .with_timezone(&chrono::Utc)
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string(),
        Err(_) => String::new(),
    }
}

/// One address list as a header value: `Name <addr>`, comma-separated.
fn party(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| party(Some(v)))
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Some(v) => {
            let address = str_field(v, &["address"]).unwrap_or("").trim();
            let name = str_field(v, &["name"]).unwrap_or("").trim();
            if !name.is_empty() && name != address {
                format!("{name} <{address}>")
            } else if !address.is_empty() {
                address.to_string()
            } else {
                name.to_string()
            }
        }
    }
}

/// The first of `keys` that carries a non-blank string.
///
/// Per key, not "first key present, then check it": an email whose `body` is an
/// empty string and whose content is in `bodyHtml` was answering with the empty
/// one and rendering a message with no body at all.
fn str_field<'a>(data: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| {
        data.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    })
}

fn short_schema(schema: &str) -> &str {
    schema.rsplit('/').next().unwrap_or("doc")
}

/// Keep the name a document actually has — spaces, unicode, case — and replace
/// only what a path cannot carry. Mirrors the server's `sanitize()`: characters
/// are substituted, never dropped, so two names that differ only in a separator
/// stay two names.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .take(120)
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A subject as one filename token: accents folded, everything that is not a
/// letter or a digit collapsed to a single '-'. Letters are matched by their
/// Unicode class, not `a-z`, so a Cyrillic or Greek subject keeps its words
/// instead of slugging away to nothing. Capped at 80 characters, which leaves
/// the whole name well inside the 255-byte limit alongside the address.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for ch in input.trim().chars() {
        let folded = fold(ch);
        if folded.is_alphanumeric() {
            for lower in folded.to_lowercase() {
                out.push(lower);
            }
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
        if out.chars().count() >= 80 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// Latin-1 Supplement / Latin Extended-A folded to ASCII — the same letters
/// NFKD-plus-mark-stripping yields on the server, without carrying a Unicode
/// normalization table for it. Everything else is left as it is, which is also
/// what the server does with it.
fn fold(ch: char) -> char {
    match ch {
        'À'..='Å' | 'à'..='å' | 'Ā'..='ą' => base(ch, 'a'),
        'Ç' | 'ç' | 'Ć'..='č' => base(ch, 'c'),
        'Ď' | 'ď' | 'Đ' | 'đ' => base(ch, 'd'),
        'È'..='Ë' | 'è'..='ë' | 'Ē'..='ě' => base(ch, 'e'),
        'Ĝ'..='ģ' => base(ch, 'g'),
        'Ĥ'..='ħ' => base(ch, 'h'),
        'Ì'..='Ï' | 'ì'..='ï' | 'Ĩ'..='ı' => base(ch, 'i'),
        'Ĵ' | 'ĵ' => base(ch, 'j'),
        'Ķ' | 'ķ' => base(ch, 'k'),
        'Ĺ'..='ł' => base(ch, 'l'),
        'Ñ' | 'ñ' | 'Ń'..='ň' => base(ch, 'n'),
        'Ò'..='Ö' | 'Ø' | 'ò'..='ö' | 'ø' | 'Ō'..='ő' => base(ch, 'o'),
        'Ŕ'..='ř' => base(ch, 'r'),
        'Ś'..='š' => base(ch, 's'),
        'Ţ'..='ŧ' => base(ch, 't'),
        'Ù'..='Ü' | 'ù'..='ü' | 'Ũ'..='ų' => base(ch, 'u'),
        'Ŵ' | 'ŵ' => base(ch, 'w'),
        'Ý' | 'ý' | 'ÿ' | 'Ŷ'..='Ÿ' => base(ch, 'y'),
        'Ź'..='ž' => base(ch, 'z'),
        other => other,
    }
}

/// Case-preserving fold: an uppercase letter folds to the uppercase ASCII base,
/// so `Č` and `č` stay distinct until slugify lowercases them.
fn base(ch: char, lower: char) -> char {
    if ch.is_uppercase() {
        lower.to_ascii_uppercase()
    } else {
        lower
    }
}

/// Insert a collision suffix before the extension: `notes.md` + 123 ->
/// `notes_123.md`. Same shape as the server's `docEntries()`.
pub fn with_id_suffix(base: &str, id: u64) -> String {
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}_{id}.{ext}"),
        _ => format!("{base}_{id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::SystemTime;

    fn doc(id: u64, schema: &str, data: Value) -> Document {
        Document {
            id,
            schema: schema.to_string(),
            data,
            updated_at: SystemTime::UNIX_EPOCH,
            locations: Vec::new(),
            display_name: None,
            size: None,
            checksum: None,
            raw: None,
        }
    }

    fn inline(doc: &Document) -> String {
        match content(doc) {
            Content::Inline(bytes) => String::from_utf8(bytes).unwrap(),
            Content::Remote { .. } => panic!("expected inline content"),
        }
    }

    #[test]
    fn each_schema_renders_as_what_it_is() {
        let note = doc(1, NOTE_SCHEMA, json!({"title": "Plan", "content": "body"}));
        assert_eq!(doc_name(&note), "Plan.md");
        assert_eq!(inline(&note), "body");

        let tab = doc(2, TAB_SCHEMA, json!({"title": "Docs", "url": "https://d"}));
        assert_eq!(doc_name(&tab), "Docs.url");
        assert_eq!(inline(&tab), "[InternetShortcut]\nURL=https://d\n");

        // A link is a URL like a tab is, and opens the same way.
        let link = doc(3, LINK_SCHEMA, json!({"label": "Ref", "uri": "https://r"}));
        assert_eq!(doc_name(&link), "Ref.url");
        assert_eq!(inline(&link), "[InternetShortcut]\nURL=https://r\n");

        let todo = doc(4, TODO_SCHEMA, json!({"title": "Ship", "completed": false}));
        assert_eq!(doc_name(&todo), "Ship.todo.json");
        assert!(inline(&todo).contains("\"completed\": false"));

        // A file is its own bytes, named by its location, never a JSON stub.
        let mut file = doc(5, FILE_SCHEMA, json!({}));
        file.locations = vec!["stored://workspace:home/photos/OM_R2.png".to_string()];
        file.size = Some(42);
        assert_eq!(doc_name(&file), "OM_R2.png");
        assert!(matches!(content(&file), Content::Remote { size: Some(42) }));
    }

    #[test]
    fn a_message_is_an_eml_named_by_sender_and_subject() {
        let mut email = doc(
            6,
            EMAIL_SCHEMA,
            json!({
                "from": {"address": "Ada@Example.COM", "name": "Ada L"},
                "to": [{"address": "b@example.com"}],
                "subject": "Re: lunch on Friday?",
                "body": "12:30 works.",
                "date": "2026-08-19T10:11:12Z",
            }),
        );
        // A mailbox slot is an address, not a name: it says nothing and is
        // renumbered by the next resync, so it must not name the file.
        email.locations = vec!["imap://work/INBOX;UID=56909".to_string()];

        assert_eq!(doc_name(&email), "ada@example.com-re-lunch-on-friday.eml");

        let body = inline(&email);
        assert!(body.starts_with("From: Ada L <Ada@Example.COM>\r\n"));
        assert!(body.contains("Subject: Re: lunch on Friday?\r\n"));
        assert!(body.contains("Date: Wed, 19 Aug 2026 10:11:12 GMT\r\n"));
        assert!(body.ends_with("\r\n\r\n12:30 works.\r\n"));
    }

    #[test]
    fn an_html_only_message_still_has_a_body() {
        // `body: ""` with the content in `bodyHtml` is what an IMAP sync
        // produces for HTML mail; taking the first key that EXISTS rather than
        // the first that says anything rendered a message with no body at all.
        let email = doc(
            10,
            EMAIL_SCHEMA,
            json!({"from": "a@b.c", "subject": "Hi", "body": "", "bodyHtml": "<p>hi</p>"}),
        );
        let rendered = inline(&email);
        assert!(rendered.contains("Content-Type: text/html; charset=utf-8"));
        assert!(rendered.ends_with("\r\n\r\n<p>hi</p>\r\n"));
    }

    #[test]
    fn a_named_document_keeps_its_name() {
        let mut note = doc(7, NOTE_SCHEMA, json!({"title": "Plan", "content": ""}));
        note.display_name = Some("weekly plan.md".to_string());
        assert_eq!(doc_name(&note), "weekly plan.md");

        // Path separators cannot survive, but everything else does — replaced,
        // never dropped, so two names stay two names.
        note.display_name = Some("a/b:c.md".to_string());
        assert_eq!(doc_name(&note), "a_b_c.md");
    }

    #[test]
    fn a_content_hash_never_names_a_file() {
        let mut file = doc(8, FILE_SCHEMA, json!({}));
        file.locations = vec![format!("stored://cache/{}.bin", "a".repeat(40))];
        assert_eq!(doc_name(&file), "file_8.json");
    }

    #[test]
    fn unknown_schemas_get_a_folder_not_a_bucket() {
        // Derived from the id, so a schema this build has never heard of still
        // lands somewhere meaningful instead of a catch-all `Other`.
        assert_eq!(schema_folder("data/schema/note"), "Notes");
        assert_eq!(schema_folder("data/schema/message/email"), "Emails");
        assert_eq!(schema_folder("data/schema/identity"), "Identitys");
        assert_eq!(schema_folder("data/schema/brand/new/thing"), "Things");

        let mut other = doc(9, "data/schema/event", json!({"title": "Standup"}));
        other.raw = Some(json!({"id": 9, "schema": "data/schema/event"}));
        assert_eq!(doc_name(&other), "event_9.json");
        // The record itself, not just its data — that is what the file claims.
        assert!(inline(&other).contains("\"schema\": \"data/schema/event\""));
    }

    #[test]
    fn slugs_fold_accents_and_keep_other_alphabets() {
        assert_eq!(slugify("Schöne Grüße!"), "schone-gruße");
        assert_eq!(slugify("Žluťoučký kůň"), "zlutoucky-kun");
        assert_eq!(slugify("Привет мир"), "привет-мир");
        assert_eq!(slugify("   ---   "), "");
    }

    #[test]
    fn collision_suffix_goes_before_the_extension() {
        assert_eq!(with_id_suffix("Meeting.md", 12), "Meeting_12.md");
        assert_eq!(with_id_suffix("README", 12), "README_12");
    }
}
