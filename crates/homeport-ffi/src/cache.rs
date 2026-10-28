//! Local metadata cache (SQLite) so the File Provider extension answers
//! enumerations of folders it has seen before without the network, also after
//! it restarts. Directory listings are revalidated against the server with
//! `if_dir_rev`.

use std::path::Path;

use homeport_proto::{Item, Kind};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS items (
    id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    kind INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL,
    mode INTEGER NOT NULL,
    rev INTEGER NOT NULL,
    content_rev INTEGER NOT NULL,
    link_target TEXT
);
CREATE INDEX IF NOT EXISTS items_parent ON items(parent_id, name);
CREATE TABLE IF NOT EXISTS dirs (
    id TEXT PRIMARY KEY,
    dir_rev INTEGER NOT NULL,
    total INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS fetched (
    id TEXT PRIMARY KEY,
    content_rev INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
";

/// An item plus its symlink target (symlinks only).
#[derive(Clone, Debug)]
pub struct CachedItem {
    pub item: Item,
    pub link_target: Option<String>,
}

pub struct Cache {
    db: Connection,
}

fn kind_to_i64(k: Kind) -> i64 {
    match k {
        Kind::File => 0,
        Kind::Dir => 1,
        Kind::Symlink => 2,
        Kind::Other => 3,
    }
}

fn kind_from_i64(k: i64) -> Kind {
    match k {
        0 => Kind::File,
        1 => Kind::Dir,
        2 => Kind::Symlink,
        _ => Kind::Other,
    }
}

const COLS: &str = "id, parent_id, name, kind, size, mtime_ns, ctime_ns, mode, rev, content_rev, link_target";

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<CachedItem> {
    Ok(CachedItem {
        item: Item {
            id: r.get(0)?,
            parent_id: r.get(1)?,
            name: r.get(2)?,
            kind: kind_from_i64(r.get(3)?),
            size: r.get::<_, i64>(4)? as u64,
            mtime_ns: r.get(5)?,
            ctime_ns: r.get(6)?,
            mode: r.get::<_, i64>(7)? as u32,
            rev: r.get::<_, i64>(8)? as u64,
            content_rev: r.get::<_, i64>(9)? as u64,
        },
        link_target: r.get(10)?,
    })
}

fn upsert(tx: &Connection, c: &CachedItem) -> rusqlite::Result<()> {
    let i = &c.item;
    tx.prepare_cached(&format!("INSERT OR REPLACE INTO items ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"))?
        .execute(params![
            i.id,
            i.parent_id,
            i.name,
            kind_to_i64(i.kind),
            i.size as i64,
            i.mtime_ns,
            i.ctime_ns,
            i.mode as i64,
            i.rev as i64,
            i.content_rev as i64,
            c.link_target
        ])?;
    Ok(())
}

impl Cache {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        // Keep the extension small: a modest page cache is plenty.
        db.execute_batch("PRAGMA cache_size = -2048;")?;
        Ok(Self { db })
    }

    pub fn meta(&self, key: &str) -> rusqlite::Result<Option<i64>> {
        self.db.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    pub fn set_meta(&self, key: &str, value: i64) -> rusqlite::Result<()> {
        self.db.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    }

    /// The server's index changed: every directory revision is meaningless,
    /// so force a relisting (items stay as a stale fallback for offline use).
    pub fn forget_dir_revs(&self) -> rusqlite::Result<()> {
        self.db.execute("UPDATE dirs SET dir_rev = -1", [])?;
        Ok(())
    }

    pub fn clear(&self) -> rusqlite::Result<()> {
        self.db.execute_batch("DELETE FROM items; DELETE FROM dirs; DELETE FROM meta; DELETE FROM fetched;")
    }

    /// `(dir_rev, total)` of a cached listing (`total` as of its last full
    /// listing). A revision of `None` means the
    /// listing is cached but must be refetched before it can be trusted.
    pub fn dir(&self, id: &str) -> rusqlite::Result<Option<(Option<u64>, u32)>> {
        self.db
            .query_row("SELECT dir_rev, total FROM dirs WHERE id = ?1", [id], |r| {
                let rev: i64 = r.get(0)?;
                Ok((u64::try_from(rev).ok(), r.get::<_, i64>(1)? as u32))
            })
            .optional()
    }

    pub fn item(&self, id: &str) -> rusqlite::Result<Option<CachedItem>> {
        self.db
            .prepare_cached(&format!("SELECT {COLS} FROM items WHERE id = ?1"))?
            .query_row([id], row_to_item)
            .optional()
    }

    pub fn page(&self, dir: &str, offset: u32, limit: u32) -> rusqlite::Result<Vec<CachedItem>> {
        let mut st = self
            .db
            .prepare_cached(&format!("SELECT {COLS} FROM items WHERE parent_id = ?1 ORDER BY name LIMIT ?2 OFFSET ?3"))?;
        st.query_map(params![dir, limit, offset], row_to_item)?.collect()
    }

    /// Records that a file's content (at `content_rev`) was downloaded.
    pub fn note_fetched(&self, id: &str, content_rev: u64) -> rusqlite::Result<()> {
        self.db
            .prepare_cached("INSERT OR REPLACE INTO fetched (id, content_rev) VALUES (?1, ?2)")?
            .execute(params![id, content_rev as i64])?;
        Ok(())
    }

    /// Small visible files whose current content was never downloaded: in
    /// `dir` smallest first, or (`None`) anywhere in random order.
    pub fn small_unfetched(&self, dir: Option<&str>, max_size: u64, limit: u32) -> rusqlite::Result<Vec<CachedItem>> {
        let cols = COLS.split(", ").map(|c| format!("i.{c}")).collect::<Vec<_>>().join(", ");
        let filter = "i.kind = 0 AND i.size > 0 AND i.size < ?1 AND substr(i.name, 1, 1) != '.' \
            AND NOT EXISTS (SELECT 1 FROM fetched f WHERE f.id = i.id AND f.content_rev = i.content_rev)";
        match dir {
            Some(d) => {
                let mut st = self.db.prepare_cached(&format!(
                    "SELECT {cols} FROM items i WHERE i.parent_id = ?3 AND {filter} ORDER BY i.size LIMIT ?2"
                ))?;
                st.query_map(params![max_size as i64, limit, d], row_to_item)?.collect()
            }
            None => {
                let mut st = self
                    .db
                    .prepare_cached(&format!("SELECT {cols} FROM items i WHERE {filter} ORDER BY random() LIMIT ?2"))?;
                st.query_map(params![max_size as i64, limit], row_to_item)?.collect()
            }
        }
    }

    /// Every item in a cached directory (for the working set).
    pub fn listed_items(&self, offset: u32, limit: u32) -> rusqlite::Result<Vec<CachedItem>> {
        let mut st = self.db.prepare_cached(&format!(
            "SELECT {COLS} FROM items WHERE parent_id IN (SELECT id FROM dirs) ORDER BY id LIMIT ?1 OFFSET ?2"
        ))?;
        st.query_map(params![limit, offset], row_to_item)?.collect()
    }

    /// Replaces a directory's listing.
    pub fn store_listing(&mut self, dir: &str, dir_rev: u64, items: &[CachedItem]) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM items WHERE parent_id = ?1", [dir])?;
        for c in items {
            upsert(&tx, c)?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO dirs (id, dir_rev, total) VALUES (?1, ?2, ?3)",
            params![dir, dir_rev as i64, items.len() as i64],
        )?;
        tx.commit()
    }

    /// Applies a change batch. Changed items are kept only when their parent's
    /// listing is cached; deleted directories drop their cached listing.
    pub fn apply_changes(&mut self, changed: &[CachedItem], deleted: &[String]) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        for id in deleted {
            tx.execute("DELETE FROM items WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM items WHERE parent_id = ?1", [id])?;
            tx.execute("DELETE FROM dirs WHERE id = ?1", [id])?;
        }
        for c in changed {
            let cached: bool =
                tx.query_row("SELECT EXISTS(SELECT 1 FROM dirs WHERE id = ?1)", [&c.item.parent_id], |r| r.get(0))?;
            if cached {
                upsert(&tx, c)?;
            } else {
                // Moved somewhere this client has not listed.
                tx.execute("DELETE FROM items WHERE id = ?1", [&c.item.id])?;
            }
        }
        tx.commit()
    }
}
