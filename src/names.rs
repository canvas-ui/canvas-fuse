use anyhow::Result;
use redb::{Database, TableDefinition};
use std::path::Path;

// (contextId \x1f workspaceId \x1f schemaDir \x1f docId) -> assigned filename
//
// The workspace is part of the key because document ids are per workspace —
// every workspace numbers from 100000 — and a context is a pointer that can be
// re-aimed at another workspace (`mbag://` today, `universe://` last month).
// Keyed by context alone, the note that is id 100056 in one workspace inherited
// the sticky name of the tab that was id 100056 in the other, and the mount
// showed a `.url` whose bytes were the note.
//
// The table name carries a generation. A sticky name outlives the renderer that
// produced it — that is the point — so when the renderer changes what a document
// is CALLED (an email became `<from>-<subject>.eml` instead of a `.json` blob of
// its fields), every existing assignment would pin the old name forever and the
// new rendering would only ever be visible on documents nobody had seen yet.
// Bumping the generation retires those assignments in one step; collision
// suffixes stay sticky from there on.
const FILENAMES: TableDefinition<&str, &str> = TableDefinition::new("filenames_v4");
const LEGACY_FILENAMES: [TableDefinition<&str, &str>; 3] = [
    TableDefinition::new("filenames"),
    TableDefinition::new("filenames_v2"),
    TableDefinition::new("filenames_v3"),
];

/// Persistent filename assignments. Once a (context, workspace, dir, doc) key gets a
/// filename it keeps it across restarts, so collision suffixes stay sticky and
/// links held by external apps (Obsidian, shell history) never silently retarget.
pub struct NameStore {
    db: Database,
}

fn key(ctx: &str, ws: &str, dir: &str, doc_id: u64) -> String {
    format!("{ctx}\u{1f}{ws}\u{1f}{dir}\u{1f}{doc_id}")
}

impl NameStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        // Ensure the table exists so later reads don't fail on a fresh DB, and
        // reclaim any retired generation while we hold the write transaction.
        let tx = db.begin_write()?;
        tx.open_table(FILENAMES)?;
        for legacy in LEGACY_FILENAMES {
            let _ = tx.delete_table(legacy);
        }
        tx.commit()?;
        Ok(Self { db })
    }

    /// `ws` is the workspace the document id belongs to; empty when unknown
    /// (a context the server reported without one), which scopes the
    /// assignment to "whatever this context points at" — the old behaviour.
    pub fn get(&self, ctx: &str, ws: &str, dir: &str, doc_id: u64) -> Option<String> {
        let tx = self.db.begin_read().ok()?;
        let table = tx.open_table(FILENAMES).ok()?;
        table
            .get(key(ctx, ws, dir, doc_id).as_str())
            .ok()
            .flatten()
            .map(|v| v.value().to_string())
    }

    pub fn put(&self, ctx: &str, ws: &str, dir: &str, doc_id: u64, name: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(FILENAMES)?;
            table.insert(key(ctx, ws, dir, doc_id).as_str(), name)?;
        }
        tx.commit()?;
        Ok(())
    }
}
