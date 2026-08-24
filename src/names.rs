use anyhow::Result;
use redb::{Database, TableDefinition};
use std::path::Path;

// (contextId \x1f schemaDir \x1f docId) -> assigned filename
//
// The table name carries a generation. A sticky name outlives the renderer that
// produced it — that is the point — so when the renderer changes what a document
// is CALLED (an email became `<from>-<subject>.eml` instead of a `.json` blob of
// its fields), every existing assignment would pin the old name forever and the
// new rendering would only ever be visible on documents nobody had seen yet.
// Bumping the generation retires those assignments in one step; collision
// suffixes stay sticky from there on.
const FILENAMES: TableDefinition<&str, &str> = TableDefinition::new("filenames_v2");
const LEGACY_FILENAMES: TableDefinition<&str, &str> = TableDefinition::new("filenames");

/// Persistent filename assignments. Once a (context, dir, doc) triple gets a
/// filename it keeps it across restarts, so collision suffixes stay sticky and
/// links held by external apps (Obsidian, shell history) never silently retarget.
pub struct NameStore {
    db: Database,
}

fn key(ctx: &str, dir: &str, doc_id: u64) -> String {
    format!("{ctx}\u{1f}{dir}\u{1f}{doc_id}")
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
        let _ = tx.delete_table(LEGACY_FILENAMES);
        tx.commit()?;
        Ok(Self { db })
    }

    pub fn get(&self, ctx: &str, dir: &str, doc_id: u64) -> Option<String> {
        let tx = self.db.begin_read().ok()?;
        let table = tx.open_table(FILENAMES).ok()?;
        table
            .get(key(ctx, dir, doc_id).as_str())
            .ok()
            .flatten()
            .map(|v| v.value().to_string())
    }

    pub fn put(&self, ctx: &str, dir: &str, doc_id: u64, name: &str) -> Result<()> {
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(FILENAMES)?;
            table.insert(key(ctx, dir, doc_id).as_str(), name)?;
        }
        tx.commit()?;
        Ok(())
    }
}
