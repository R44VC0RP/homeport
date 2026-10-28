//! SQLite persistence for the index. One writer thread owns the write
//! connection and batches operations into transactions; reads use a separate
//! WAL connection.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use homeport_proto::{Deleted, Item, Kind};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{oneshot, watch};

/// Tombstones kept for `Changes`; older anchors get `SyncAnchorExpired`.
const TOMBSTONE_KEEP: i64 = 200_000;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database writer stopped")]
    WriterGone,
    #[error("database lock poisoned")]
    Poisoned,
}

pub enum Op {
    Listing { dir_id: String, dir_rev: u64, path: PathBuf, upserts: Vec<Item>, removed: Vec<String>, seq: u64 },
    DropDir { dir_id: String, seq: u64 },
    Have { client_id: String, dir_id: String },
    EventId(u64),
    VolumeUuid(String),
    Flush(oneshot::Sender<()>),
}

/// A tombstone and, if the item still exists elsewhere, its current parent.
pub type Tombstone = (Deleted, Option<String>);

/// State loaded at startup.
pub struct Loaded {
    pub index_id: u64,
    pub seq: u64,
    pub tombstone_floor: u64,
    pub last_event_id: Option<u64>,
    pub volume_uuid: Option<String>,
    pub dirs: HashMap<String, (u64, PathBuf)>,
    pub have: HashMap<String, HashSet<String>>,
}

pub struct Db {
    tx: Mutex<Option<mpsc::Sender<Op>>>,
    read: Mutex<Connection>,
    writer: Mutex<Option<JoinHandle<()>>>,
    pub committed: watch::Receiver<u64>,
}

fn open(path: &Path) -> Result<Connection, DbError> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

fn kind_to_i(k: Kind) -> i64 {
    match k {
        Kind::File => 0,
        Kind::Dir => 1,
        Kind::Symlink => 2,
        Kind::Other => 3,
    }
}

fn kind_from_i(i: i64) -> Kind {
    match i {
        0 => Kind::File,
        1 => Kind::Dir,
        2 => Kind::Symlink,
        _ => Kind::Other,
    }
}

fn row_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: r.get(0)?,
        parent_id: r.get(1)?,
        name: r.get(2)?,
        kind: kind_from_i(r.get(3)?),
        size: r.get::<_, i64>(4)? as u64,
        mtime_ns: r.get(5)?,
        ctime_ns: r.get(6)?,
        mode: r.get::<_, i64>(7)? as u32,
        rev: r.get::<_, i64>(8)? as u64,
        content_rev: r.get::<_, i64>(9)? as u64,
    })
}

const ITEM_COLS: &str = "file_id, parent_id, name, kind, size, mtime_ns, ctime_ns, mode, rev, content_rev";

fn meta_get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()
}

fn meta_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

impl Db {
    pub fn open(path: &Path) -> Result<(Arc<Self>, Loaded), DbError> {
        let conn = open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS items(
                file_id TEXT PRIMARY KEY, parent_id TEXT NOT NULL, name TEXT NOT NULL,
                kind INTEGER NOT NULL, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
                ctime_ns INTEGER NOT NULL, mode INTEGER NOT NULL, rev INTEGER NOT NULL,
                content_rev INTEGER NOT NULL, hash BLOB);
             CREATE INDEX IF NOT EXISTS items_parent ON items(parent_id, name);
             CREATE INDEX IF NOT EXISTS items_rev ON items(rev);
             CREATE TABLE IF NOT EXISTS dirs(dir_id TEXT PRIMARY KEY, dir_rev INTEGER NOT NULL, path TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS tombstones(seq INTEGER NOT NULL, item_id TEXT NOT NULL, parent_id TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS tombstones_seq ON tombstones(seq);
             CREATE TABLE IF NOT EXISTS have(client_id TEXT NOT NULL, dir_id TEXT NOT NULL, PRIMARY KEY(client_id, dir_id)) WITHOUT ROWID;",
        )?;

        let index_id = match meta_get(&conn, "index_id")?.and_then(|v| v.parse().ok()) {
            Some(id) => id,
            None => {
                let id: u64 = rand::random::<u64>() >> 1;
                meta_set(&conn, "index_id", &id.to_string())?;
                id
            }
        };
        let stored_seq: u64 = meta_get(&conn, "seq")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        // Writes are persisted asynchronously; skip ahead so a seq handed out
        // before an unclean exit is never reused for a different change.
        let seq = stored_seq + 1000;
        meta_set(&conn, "seq", &seq.to_string())?;

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM tombstones", [], |r| r.get(0))?;
        if count > TOMBSTONE_KEEP {
            let cut: i64 = conn.query_row(
                "SELECT seq FROM tombstones ORDER BY seq DESC LIMIT 1 OFFSET ?1",
                [TOMBSTONE_KEEP],
                |r| r.get(0),
            )?;
            conn.execute("DELETE FROM tombstones WHERE seq <= ?1", [cut])?;
            meta_set(&conn, "tombstone_floor", &cut.to_string())?;
        }
        let tombstone_floor = meta_get(&conn, "tombstone_floor")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let last_event_id = meta_get(&conn, "last_event_id")?.and_then(|v| v.parse().ok());
        let volume_uuid = meta_get(&conn, "volume_uuid")?;

        let mut dirs = HashMap::new();
        {
            let mut st = conn.prepare("SELECT dir_id, dir_rev, path FROM dirs")?;
            let rows =
                st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, String>(2)?)))?;
            for row in rows {
                let (id, rev, path) = row?;
                dirs.insert(id, (rev, PathBuf::from(path)));
            }
        }
        let mut have: HashMap<String, HashSet<String>> = HashMap::new();
        {
            let mut st = conn.prepare("SELECT client_id, dir_id FROM have")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (c, d) = row?;
                have.entry(c).or_default().insert(d);
            }
        }

        let read = open(path)?;
        let (tx, rx) = mpsc::channel();
        let (committed_tx, committed) = watch::channel(seq);
        let writer = std::thread::Builder::new()
            .name("homeport-db".into())
            .spawn(move || writer_loop(conn, rx, committed_tx))
            .map_err(|e| DbError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))?;
        let db = Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            read: Mutex::new(read),
            writer: Mutex::new(Some(writer)),
            committed,
        });
        let loaded = Loaded { index_id, seq, tombstone_floor, last_event_id, volume_uuid, dirs, have };
        Ok((db, loaded))
    }

    pub fn send(&self, op: Op) -> Result<(), DbError> {
        let tx = self.tx.lock().map_err(|_| DbError::Poisoned)?;
        tx.as_ref().ok_or(DbError::WriterGone)?.send(op).map_err(|_| DbError::WriterGone)
    }

    /// Waits until everything sent so far is committed.
    pub async fn flush(&self) -> Result<(), DbError> {
        let (tx, rx) = oneshot::channel();
        self.send(Op::Flush(tx))?;
        rx.await.map_err(|_| DbError::WriterGone)
    }

    /// Stops the writer after it commits its queue.
    pub fn close(&self) {
        if let Ok(mut tx) = self.tx.lock() {
            tx.take();
        }
        let handle = self.writer.lock().ok().and_then(|mut w| w.take());
        if let Some(h) = handle {
            let _ = h.join();
        }
    }

    fn with_read<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, DbError> {
        let conn = self.read.lock().map_err(|_| DbError::Poisoned)?;
        Ok(f(&conn)?)
    }

    pub fn dir_items(&self, dir_id: &str) -> Result<Vec<Item>, DbError> {
        self.with_read(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {ITEM_COLS} FROM items WHERE parent_id = ?1"))?;
            st.query_map([dir_id], row_item)?.collect()
        })
    }

    pub fn item(&self, id: &str) -> Result<Option<Item>, DbError> {
        self.with_read(|c| {
            let mut st = c.prepare_cached(&format!("SELECT {ITEM_COLS} FROM items WHERE file_id = ?1"))?;
            st.query_row([id], row_item).optional()
        })
    }

    /// Items changed and tombstones recorded in `(since, upto]`. Each
    /// tombstone carries the item's current parent if it still exists (moved).
    pub fn changes(&self, since: u64, upto: u64) -> Result<(Vec<Item>, Vec<Tombstone>), DbError> {
        self.with_read(|c| {
            let mut st =
                c.prepare_cached(&format!("SELECT {ITEM_COLS} FROM items WHERE rev > ?1 AND rev <= ?2 ORDER BY rev"))?;
            let items = st.query_map(params![since as i64, upto as i64], row_item)?.collect::<Result<Vec<_>, _>>()?;
            let mut st = c.prepare_cached(
                "SELECT t.item_id, t.parent_id, t.seq, i.parent_id FROM tombstones t
                 LEFT JOIN items i ON i.file_id = t.item_id
                 WHERE t.seq > ?1 AND t.seq <= ?2 ORDER BY t.seq",
            )?;
            let deleted = st
                .query_map(params![since as i64, upto as i64], |r| {
                    let d = Deleted { id: r.get(0)?, parent_id: r.get(1)?, seq: r.get::<_, i64>(2)? as u64 };
                    Ok((d, r.get::<_, Option<String>>(3)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((items, deleted))
        })
    }
}

fn apply(conn: &Connection, op: &Op, max_seq: &mut u64) -> rusqlite::Result<()> {
    match op {
        Op::Listing { dir_id, dir_rev, path, upserts, removed, seq } => {
            let mut up = conn.prepare_cached(
                "INSERT INTO items(file_id, parent_id, name, kind, size, mtime_ns, ctime_ns, mode, rev, content_rev)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(file_id) DO UPDATE SET parent_id = excluded.parent_id, name = excluded.name,
                   kind = excluded.kind, size = excluded.size, mtime_ns = excluded.mtime_ns,
                   ctime_ns = excluded.ctime_ns, mode = excluded.mode, rev = excluded.rev,
                   content_rev = excluded.content_rev",
            )?;
            for it in upserts {
                up.execute(params![
                    it.id,
                    it.parent_id,
                    it.name,
                    kind_to_i(it.kind),
                    it.size as i64,
                    it.mtime_ns,
                    it.ctime_ns,
                    i64::from(it.mode),
                    it.rev as i64,
                    it.content_rev as i64
                ])?;
            }
            let mut del = conn.prepare_cached("DELETE FROM items WHERE file_id = ?1 AND parent_id = ?2")?;
            let mut tomb =
                conn.prepare_cached("INSERT INTO tombstones(seq, item_id, parent_id) VALUES (?1, ?2, ?3)")?;
            for id in removed {
                del.execute(params![id, dir_id])?;
                tomb.execute(params![*seq as i64, id, dir_id])?;
            }
            conn.prepare_cached(
                "INSERT INTO dirs(dir_id, dir_rev, path) VALUES (?1, ?2, ?3)
                 ON CONFLICT(dir_id) DO UPDATE SET dir_rev = excluded.dir_rev, path = excluded.path",
            )?
            .execute(params![dir_id, *dir_rev as i64, path.to_string_lossy()])?;
            *max_seq = (*max_seq).max(*seq);
        }
        Op::DropDir { dir_id, seq } => {
            conn.execute("DELETE FROM dirs WHERE dir_id = ?1", [dir_id])?;
            conn.execute("DELETE FROM items WHERE parent_id = ?1", [dir_id])?;
            conn.execute("DELETE FROM have WHERE dir_id = ?1", [dir_id])?;
            *max_seq = (*max_seq).max(*seq);
        }
        Op::Have { client_id, dir_id } => {
            conn.prepare_cached("INSERT OR IGNORE INTO have(client_id, dir_id) VALUES (?1, ?2)")?
                .execute(params![client_id, dir_id])?;
        }
        Op::EventId(id) => meta_set(conn, "last_event_id", &id.to_string())?,
        Op::VolumeUuid(u) => meta_set(conn, "volume_uuid", u)?,
        Op::Flush(_) => {}
    }
    Ok(())
}

fn writer_loop(mut conn: Connection, rx: mpsc::Receiver<Op>, committed: watch::Sender<u64>) {
    let mut seq = *committed.borrow();
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < 512 {
            match rx.try_recv() {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }
        let mut max_seq = seq;
        let result = (|| -> rusqlite::Result<()> {
            let tx = conn.transaction()?;
            for op in &batch {
                apply(&tx, op, &mut max_seq)?;
            }
            if max_seq > seq {
                meta_set(&tx, "seq", &max_seq.to_string())?;
            }
            tx.commit()
        })();
        match result {
            Ok(()) => {
                if max_seq > seq {
                    seq = max_seq;
                    committed.send_replace(seq);
                }
            }
            Err(e) => tracing::error!(error = %e, ops = batch.len(), "index write failed"),
        }
        for op in batch {
            if let Op::Flush(done) = op {
                let _ = done.send(());
            }
        }
    }
}
