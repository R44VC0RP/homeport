//! The lazy directory index.
//!
//! A directory is scanned the first time a client lists it and is then kept
//! current by the platform watcher. Every change is stamped with the next
//! value of a monotonic `seq`; listings are cached pre-encoded in an LRU keyed
//! by directory ID together with their `dir_rev`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use bytes::Bytes;
use homeport_proto::{self as proto, Deleted, Item, ItemId, Kind, Response};
use lru::LruCache;

use crate::db::{Db, DbError, Loaded, Op, Row, Trashed};
use crate::names;
use crate::platform::{
    Change, Dir, DirOps, Entry, ErrKind, File, FileKey, FileOps, Fs, FsCaps, FsOps, IdScheme, RawName, Trash, TrashOps,
    Volume, VolumeOps, Watcher, WatcherOps,
};

mod ids;
mod writes;
pub use ids::Ids;
pub use writes::{CreateReq, STAGING_NAME, Staged, clear_staging};

/// Directories with at most this many entries have every entry's Finder
/// tags read on each scan; larger ones only for entries known to have tags.
const TAG_SCAN_MAX: usize = 512;
/// Recent item locations kept in memory for the index walk (assigned ids).
const LOCATIONS: usize = 1 << 16;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("no such item")]
    NotFound,
    #[error("item is outside the shares")]
    OutsideShare,
    #[error("not a directory")]
    NotADirectory,
    #[error("not a regular file")]
    NotAFile,
    #[error("bad item id {0:?}")]
    BadId(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("frame: {0}")]
    Frame(#[from] proto::FrameError),
    #[error("index task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("index lock poisoned")]
    Poisoned,
    #[error("name is taken")]
    Collision(Box<Item>),
    #[error("folder is not empty")]
    NotEmpty,
    #[error("disk is full")]
    NoSpace,
    #[error("name not allowed: {0}")]
    BadName(String),
    #[error("permission denied")]
    Denied,
    #[error("upload incomplete: {0}")]
    Incomplete(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("the file is in use on the server")]
    Busy,
    #[error("the file is a cloud placeholder; not downloading it")]
    CloudOnly,
    #[error("{0}")]
    Unsupported(String),
}

impl IndexError {
    pub fn code(&self) -> proto::ErrorCode {
        use proto::ErrorCode as C;
        match self {
            Self::NotFound => C::NotFound,
            Self::OutsideShare => C::OutsideShare,
            Self::NotADirectory => C::NotADirectory,
            Self::NotAFile => C::NotAFile,
            Self::BadId(_) | Self::BadRequest(_) => C::BadRequest,
            Self::NotEmpty => C::NotEmpty,
            Self::NoSpace => C::NoSpace,
            Self::BadName(_) => C::BadName,
            Self::Denied => C::Denied,
            Self::Incomplete(_) => C::Incomplete,
            Self::Busy => C::Busy,
            Self::CloudOnly => C::CloudOnly,
            Self::Unsupported(_) => C::Unsupported,
            _ => C::Internal,
        }
    }

    pub fn from_io(e: io::Error) -> Self {
        match Fs::classify(&e) {
            ErrKind::NotFound => Self::NotFound,
            ErrKind::NotADirectory | ErrKind::Symlink => Self::NotADirectory,
            ErrKind::NotEmpty => Self::NotEmpty,
            ErrKind::NoSpace => Self::NoSpace,
            ErrKind::Denied => Self::Denied,
            ErrKind::BadName => Self::BadName(e.to_string()),
            ErrKind::Busy => Self::Busy,
            ErrKind::CloudOnly => Self::CloudOnly,
            ErrKind::Unsupported => Self::Unsupported(e.to_string()),
            _ => Self::Io(e),
        }
    }
}

impl From<names::NameError> for IndexError {
    fn from(e: names::NameError) -> Self {
        Self::BadName(e.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct Share {
    pub name: String,
    pub id: ItemId,
    pub path: PathBuf,
    pub key: FileKey,
    /// The share root's numeric item id.
    pub num_id: u64,
    pub caps: FsCaps,
    /// Where uploads for this share are received (same volume).
    pub staging: PathBuf,
}

impl Share {
    /// Names here can collide on a case- and normalization-folding client.
    fn needs_aliases(&self) -> bool {
        self.caps.case_sensitive || self.caps.normalization_sensitive
    }
}

/// A directory listing ready to send.
pub struct Listing {
    pub dir_id: ItemId,
    pub dir_rev: u64,
    pub rows: Vec<Row>,
    /// `Items` frames, encoded on first use.
    frames: OnceLock<Vec<Bytes>>,
}

impl Listing {
    pub fn frames(&self) -> Result<&[Bytes], IndexError> {
        if let Some(f) = self.frames.get() {
            return Ok(f);
        }
        let f = encode_frames(&self.dir_id, self.dir_rev, &self.rows)?;
        Ok(self.frames.get_or_init(|| f))
    }

    pub fn find(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.item.id == id)
    }
}

pub enum ListOutcome {
    NotModified { dir_rev: u64 },
    Listing(Arc<Listing>),
}

struct State {
    seq: u64,
    lru: LruCache<ItemId, Arc<Listing>>,
    /// Every indexed directory: its `dir_rev` and last known path.
    indexed: HashMap<ItemId, (u64, PathBuf)>,
    have: HashMap<String, HashSet<ItemId>>,
}

/// Server-wide pieces the index needs for writes.
pub struct WriteDirs {
    /// Uploads land here when a share is on the state directory's volume.
    pub staging: PathBuf,
    pub trash: Option<Trash>,
}

pub struct Index {
    shares: Vec<Share>,
    ids: Ids,
    /// Resolve ids by walking rows even where the platform resolves keys.
    walk_only: bool,
    pub db: Arc<Db>,
    pub index_id: u64,
    pub tombstone_floor: u64,
    state: Mutex<State>,
    dir_locks: Mutex<HashMap<ItemId, Weak<tokio::sync::Mutex<()>>>>,
    root: Arc<Listing>,
    writes: WriteDirs,
    /// Recent creates: client op id -> file id.
    ops: Mutex<HashMap<String, ItemId>>,
    /// Items in the trash container.
    trashed: Mutex<HashMap<ItemId, Trashed>>,
    /// Recent rows' (parent, raw name), ahead of the database (assigned ids).
    locations: Mutex<LruCache<u64, (ItemId, RawName)>>,
    watcher: OnceLock<Watcher>,
}

/// What an ID refers to.
enum Target {
    Root,
    Path {
        path: PathBuf,
        share: usize,
    },
    /// In the server's Trash, put there by a client.
    Trashed {
        path: PathBuf,
    },
}

/// Whether a fresh scan of an item shows nothing new. Rows indexed before
/// creation times, flags and tags were recorded (crtime 0) are compared on
/// the old fields only, so upgrading does not report every item as changed;
/// likewise rows without a key or raw name (schema 2).
fn same_attrs(old: &Row, new: &Row) -> bool {
    let (o, n) = (&old.item, &new.item);
    let base = o.parent_id == n.parent_id
        && o.name == n.name
        && o.kind == n.kind
        && o.size == n.size
        && o.mtime_ns == n.mtime_ns
        && o.ctime_ns == n.ctime_ns
        && o.mode == n.mode
        && (old.key.is_unset() || old.key == new.key)
        && old.raw == new.raw;
    base && (o.crtime_ns == 0 || (o.crtime_ns == n.crtime_ns && o.flags == n.flags && o.tags == n.tags))
}

fn encode_frames(dir_id: &str, dir_rev: u64, rows: &[Row]) -> Result<Vec<Bytes>, IndexError> {
    let total = u32::try_from(rows.len()).unwrap_or(u32::MAX);
    let pages: Vec<&[Row]> = if rows.is_empty() { vec![&[]] } else { rows.chunks(proto::PAGE_SIZE).collect() };
    let n = pages.len();
    let mut frames = Vec::with_capacity(n);
    for (i, page) in pages.into_iter().enumerate() {
        frames.push(proto::encode_frame(&Response::Items {
            dir_id: dir_id.to_owned(),
            dir_rev,
            total,
            page: i as u32,
            last: i + 1 == n,
            items: page.iter().map(|r| r.item.clone()).collect(),
        })?);
    }
    Ok(frames)
}

fn encode_listing(dir_id: &str, dir_rev: u64, rows: Vec<Row>) -> Listing {
    Listing { dir_id: dir_id.to_owned(), dir_rev, rows, frames: OnceLock::new() }
}

/// A row for a scanned entry (revisions unset; display name without alias).
pub(crate) fn row_of(id: ItemId, parent_id: &str, e: Entry, tags: Option<Vec<u8>>) -> Row {
    let (name, name_flags) = e.name.display();
    let item = Item {
        id,
        parent_id: parent_id.to_owned(),
        name,
        kind: e.kind,
        size: e.size,
        mtime_ns: e.mtime_ns,
        ctime_ns: e.ctime_ns,
        mode: e.mode,
        rev: 0,
        content_rev: 0,
        crtime_ns: e.crtime_ns,
        flags: e.flags | name_flags,
        tags,
    };
    Row { item, key: e.key, raw: e.name }
}

fn virtual_dir(id: &str, name: &str) -> Item {
    Item {
        id: id.to_owned(),
        parent_id: proto::ROOT_ID.to_owned(),
        name: name.to_owned(),
        kind: Kind::Dir,
        size: 0,
        mtime_ns: 0,
        ctime_ns: 0,
        mode: 0o755,
        rev: 1,
        content_rev: 1,
        crtime_ns: 0,
        flags: 0,
        tags: None,
    }
}

/// Re-derives display names in a directory on a sensitive filesystem and
/// aliases collisions. Returns the indices whose name or flags changed.
fn apply_aliases(rows: &mut [Row]) -> Vec<usize> {
    let before: Vec<(String, u32)> = rows.iter().map(|r| (r.item.name.clone(), r.item.flags)).collect();
    let mut work: Vec<(u64, String, u32)> = rows
        .iter()
        .map(|r| {
            let (name, nf) = r.raw.display();
            let flags =
                (r.item.flags & !(proto::item_flags::NAME_ALIASED | proto::item_flags::NAME_UNREPRESENTABLE)) | nf;
            (r.item.id.parse::<u64>().unwrap_or(u64::MAX), name, flags)
        })
        .collect();
    let mut named: Vec<names::Named<'_>> =
        work.iter_mut().map(|(order, name, flags)| names::Named { order: *order, name, flags }).collect();
    names::assign_aliases(&mut named);
    let mut changed = Vec::new();
    for (i, (r, (_, name, flags))) in rows.iter_mut().zip(work).enumerate() {
        if before[i].0 != name || before[i].1 != flags {
            changed.push(i);
        }
        r.item.name = name;
        r.item.flags = flags;
    }
    changed
}

impl Index {
    pub fn new(
        shares: Vec<(String, PathBuf)>,
        db: Arc<Db>,
        loaded: Loaded,
        lru_capacity: usize,
        writes: WriteDirs,
        scheme: IdScheme,
        walk_only: bool,
    ) -> Result<Self, IndexError> {
        let ids = Ids::new(scheme, db.clone(), loaded.vol_slots, loaded.next_id);
        let staging_vol = Volume::of(&writes.staging)?.key();
        let mut out = Vec::new();
        let mut root_items = Vec::new();
        for (name, path) in shares {
            let path = std::fs::canonicalize(&path)?;
            let st = Fs::lstat(&path).map_err(IndexError::from_io)?;
            if st.kind != Kind::Dir {
                return Err(IndexError::NotADirectory);
            }
            let vol = Volume::of(&path).map_err(IndexError::from_io)?;
            let staging = if vol.key() == staging_vol {
                writes.staging.clone()
            } else {
                // Uploads must be renamed into place: stage on the share's volume.
                let s = path.join(STAGING_NAME);
                writes::clear_staging(&s)?;
                s
            };
            ids.register_volume(vol)?;
            let num_id = ids.get_or_assign(&st.key)?;
            let id = format!("{}{name}", proto::SHARE_ID_PREFIX);
            let mut item = virtual_dir(&id, &name);
            (item.size, item.mtime_ns, item.ctime_ns, item.mode, item.crtime_ns) =
                (st.size, st.mtime_ns, st.ctime_ns, st.mode, st.crtime_ns);
            root_items.push(Row { item, key: st.key, raw: RawName(name.clone().into()) });
            let caps = Fs::caps(&path);
            out.push(Share { name, id, path, key: st.key, num_id, caps, staging });
        }
        if out.is_empty() {
            return Err(IndexError::Io(io::Error::other("no shares configured")));
        }
        let root = Arc::new(encode_listing(proto::ROOT_ID, 1, root_items));
        let cap = NonZeroUsize::new(lru_capacity.max(1)).unwrap_or(NonZeroUsize::MIN);
        let state = State { seq: loaded.seq, lru: LruCache::new(cap), indexed: loaded.dirs, have: loaded.have };
        let loc_cap = NonZeroUsize::new(LOCATIONS).unwrap_or(NonZeroUsize::MIN);
        Ok(Self {
            shares: out,
            ids,
            walk_only,
            db,
            index_id: loaded.index_id,
            tombstone_floor: loaded.tombstone_floor,
            state: Mutex::new(state),
            dir_locks: Mutex::new(HashMap::new()),
            root,
            writes,
            ops: Mutex::new(loaded.ops),
            trashed: Mutex::new(loaded.trashed),
            locations: Mutex::new(LruCache::new(loc_cap)),
            watcher: OnceLock::new(),
        })
    }

    pub fn shares(&self) -> &[Share] {
        &self.shares
    }

    /// Hands the index the watcher, which then hears about every directory
    /// that becomes indexed (non-recursive watchers need that).
    pub fn set_watcher(&self, w: Watcher) -> Result<(), IndexError> {
        let paths: Vec<PathBuf> = self.state()?.indexed.values().map(|(_, p)| p.clone()).collect();
        for s in &self.shares {
            w.watch_dir(&s.path);
        }
        for p in &paths {
            w.watch_dir(p);
        }
        let _ = self.watcher.set(w);
        Ok(())
    }

    pub fn roots(&self) -> Vec<Item> {
        self.root.rows.iter().map(|r| r.item.clone()).collect()
    }

    pub fn has_trash(&self) -> bool {
        self.writes.trash.is_some()
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, IndexError> {
        self.state.lock().map_err(|_| IndexError::Poisoned)
    }

    pub fn committed_seq(&self) -> u64 {
        *self.db.committed.borrow()
    }

    /// Item ID for a numeric id (share roots get their `root:` IDs).
    fn id_str(&self, n: u64) -> ItemId {
        match self.shares.iter().find(|s| s.num_id == n) {
            Some(s) => s.id.clone(),
            None => n.to_string(),
        }
    }

    /// The id of a key if it has one.
    fn known_id(&self, key: &FileKey) -> Result<Option<ItemId>, IndexError> {
        Ok(self.ids.lookup(key)?.map(|n| self.id_str(n)))
    }

    /// The id of a key, assigning one if needed.
    fn id_for(&self, key: &FileKey) -> Result<ItemId, IndexError> {
        Ok(self.id_str(self.ids.get_or_assign(key)?))
    }

    fn share_of(&self, path: &Path) -> Option<usize> {
        self.shares.iter().position(|s| path.starts_with(&s.path))
    }

    /// Maps an ID to its current path, rejecting anything outside the shares.
    fn resolve(&self, id: &str) -> Result<Target, IndexError> {
        match self.resolve_any(id)? {
            Target::Trashed { .. } => Err(IndexError::OutsideShare),
            t => Ok(t),
        }
    }

    fn is_trashed(&self, id: &str) -> Result<bool, IndexError> {
        Ok(self.trashed.lock().map_err(|_| IndexError::Poisoned)?.contains_key(id))
    }

    /// Like `resolve`, also accepting items in the trash container.
    fn resolve_any(&self, id: &str) -> Result<Target, IndexError> {
        if id == proto::ROOT_ID {
            return Ok(Target::Root);
        }
        if let Some(name) = id.strip_prefix(proto::SHARE_ID_PREFIX) {
            let share = self.shares.iter().position(|s| s.name == name).ok_or(IndexError::NotFound)?;
            return Ok(Target::Path { path: self.shares[share].path.clone(), share });
        }
        let n: u64 = id.parse().map_err(|_| IndexError::BadId(id.to_owned()))?;
        if let Some(share) = self.shares.iter().position(|s| s.num_id == n) {
            return Ok(Target::Path { path: self.shares[share].path.clone(), share });
        }
        let key = self.ids.key_of(n)?.ok_or(IndexError::NotFound)?;
        let path = match self.platform_path(&key)? {
            Some(p) => p,
            None => match self.trashed_path(id)? {
                Some(p) => p,
                None => self.walk(n, &key)?,
            },
        };
        match self.share_of(&path) {
            Some(share) => Ok(Target::Path { path, share }),
            None if self.writes.trash.as_ref().is_some_and(|t| t.contains(&path)) && self.is_trashed(id)? => {
                Ok(Target::Trashed { path })
            }
            None => Err(IndexError::OutsideShare),
        }
    }

    /// The OS's path for a key, when it can resolve keys.
    fn platform_path(&self, key: &FileKey) -> Result<Option<PathBuf>, IndexError> {
        if self.walk_only {
            return Ok(None);
        }
        match self.ids.with_volume(key, |v| v.path_of(key)) {
            Some(r) => r.map_err(IndexError::from_io),
            None => Ok(None),
        }
    }

    /// Where a trashed item went, when the platform cannot resolve keys.
    fn trashed_path(&self, id: &str) -> Result<Option<PathBuf>, IndexError> {
        Ok(self.trashed.lock().map_err(|_| IndexError::Poisoned)?.get(id).and_then(|t| t.path.clone()))
    }

    /// Records where rows live, for `walk` (assigned ids only).
    fn note_locations<'a>(&self, rows: impl IntoIterator<Item = &'a Row>) {
        if self.ids.scheme() != IdScheme::Assigned && !self.walk_only {
            return;
        }
        if let Ok(mut loc) = self.locations.lock() {
            for r in rows {
                if let Ok(n) = r.item.id.parse::<u64>() {
                    loc.put(n, (r.item.parent_id.clone(), r.raw.clone()));
                }
            }
        }
    }

    fn location(&self, n: u64) -> Result<Option<(ItemId, RawName)>, IndexError> {
        if let Some(l) = self.locations.lock().map_err(|_| IndexError::Poisoned)?.get(&n) {
            return Ok(Some(l.clone()));
        }
        Ok(self.db.row(&n.to_string())?.map(|r| (r.item.parent_id, r.raw)))
    }

    /// Path of an item from the index's own rows (parent, raw name) up to its
    /// share root, checked against the item's key.
    fn walk(&self, n: u64, key: &FileKey) -> Result<PathBuf, IndexError> {
        let mut comps: Vec<RawName> = Vec::new();
        let mut cur = n;
        for _ in 0..4096 {
            let (parent, raw) = self.location(cur)?.ok_or(IndexError::NotFound)?;
            comps.push(raw);
            let share = match parent.strip_prefix(proto::SHARE_ID_PREFIX) {
                Some(name) => self.shares.iter().find(|s| s.name == name),
                None => {
                    cur = parent.parse().map_err(|_| IndexError::NotFound)?;
                    self.shares.iter().find(|s| s.num_id == cur)
                }
            };
            if let Some(s) = share {
                let mut path = s.path.clone();
                for c in comps.iter().rev() {
                    path.push(c.as_os_str());
                }
                let found = Fs::lstat(&path).map_err(IndexError::from_io)?;
                if !found.key.same_file(key) {
                    return Err(IndexError::NotFound);
                }
                // No component may have become a symlink (out of the share).
                #[cfg(unix)]
                if let Some(parent) = path.parent()
                    && std::fs::canonicalize(parent).ok().as_deref() != Some(parent)
                {
                    return Err(IndexError::NotFound);
                }
                return Ok(path);
            }
        }
        Err(IndexError::NotFound)
    }

    fn parent_id_of(&self, path: &Path, share: usize) -> Result<ItemId, IndexError> {
        if path == self.shares[share].path {
            return Ok(proto::ROOT_ID.to_owned());
        }
        let parent = path.parent().ok_or(IndexError::NotFound)?;
        let st = Fs::lstat(parent).map_err(IndexError::from_io)?;
        self.id_for(&st.key)
    }

    fn dir_lock(&self, id: &str) -> Result<Arc<tokio::sync::Mutex<()>>, IndexError> {
        let mut locks = self.dir_locks.lock().map_err(|_| IndexError::Poisoned)?;
        if let Some(l) = locks.get(id).and_then(Weak::upgrade) {
            return Ok(l);
        }
        if locks.len() > 1024 {
            locks.retain(|_, w| w.strong_count() > 0);
        }
        let l = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(id.to_owned(), Arc::downgrade(&l));
        Ok(l)
    }

    fn note_have(&self, client: Option<&str>, dir_id: &str) -> Result<(), IndexError> {
        let Some(client) = client else { return Ok(()) };
        let mut st = self.state()?;
        let set = st.have.entry(client.to_owned()).or_default();
        if set.insert(dir_id.to_owned()) {
            drop(st);
            self.db.send(Op::Have { client_id: client.to_owned(), dir_id: dir_id.to_owned() })?;
        }
        Ok(())
    }

    /// Canonical form of a directory ID (numeric share-root IDs become `root:<name>`).
    fn canonical(&self, id: &str) -> ItemId {
        match id.parse::<u64>() {
            Ok(n) => self.id_str(n),
            Err(_) => id.to_owned(),
        }
    }

    pub async fn list(
        self: &Arc<Self>,
        id: &str,
        if_rev: Option<u64>,
        client: Option<&str>,
    ) -> Result<ListOutcome, IndexError> {
        let id = self.canonical(id);
        let listing = if id == proto::ROOT_ID {
            self.root.clone()
        } else if id == proto::TRASH_ID {
            let this = self.clone();
            Arc::new(tokio::task::spawn_blocking(move || this.trash_listing()).await??)
        } else {
            let cached = self.state()?.lru.get(&id).cloned();
            match cached {
                Some(l) => l,
                None => self.refresh(&id, true).await?,
            }
        };
        self.note_have(client, &id)?;
        Ok(match if_rev {
            Some(r) if r == listing.dir_rev => ListOutcome::NotModified { dir_rev: r },
            _ => ListOutcome::Listing(listing),
        })
    }

    /// Rescans one directory and records any changes. With `use_cache`, a
    /// listing cached while waiting for the directory lock is returned as is.
    pub async fn refresh(self: &Arc<Self>, id: &str, use_cache: bool) -> Result<Arc<Listing>, IndexError> {
        Ok(self.refresh_inner(id, use_cache).await?.0)
    }

    /// Like `refresh`, also returning indexed directories that left this one.
    async fn refresh_inner(
        self: &Arc<Self>,
        id: &str,
        use_cache: bool,
    ) -> Result<(Arc<Listing>, Vec<ItemId>), IndexError> {
        let lock = self.dir_lock(id)?;
        let _guard = lock.lock().await;
        self.refresh_locked(id, use_cache).await
    }

    /// `refresh_inner` for a caller that holds the directory's lock.
    async fn refresh_locked(
        self: &Arc<Self>,
        id: &str,
        use_cache: bool,
    ) -> Result<(Arc<Listing>, Vec<ItemId>), IndexError> {
        let (old, prev_rev) = {
            let mut st = self.state()?;
            if use_cache && let Some(l) = st.lru.get(id) {
                return Ok((l.clone(), Vec::new()));
            }
            let prev_rev = st.indexed.get(id).map(|(r, _)| *r);
            (st.lru.peek(id).cloned(), prev_rev)
        };
        if old.is_none() && prev_rev.is_some() {
            // The rows are read back from the database: make sure our own
            // recent writes are in it.
            self.db.flush().await?;
        }
        let this = self.clone();
        let id_owned = id.to_owned();
        let scanned = tokio::task::spawn_blocking(move || -> Result<_, IndexError> {
            let target = this.resolve(&id_owned)?;
            let (path, share) = match target {
                Target::Path { path, share } => (path, share),
                Target::Trashed { .. } => return Err(IndexError::OutsideShare),
                Target::Root => return Err(IndexError::BadId(id_owned)),
            };
            let entries = Dir::open(&path, None).and_then(|d| d.scan()).map_err(IndexError::from_io)?;
            let old_rows = match old {
                Some(l) => l.rows.clone(),
                None if prev_rev.is_some() => this.db.dir_rows(&id_owned)?,
                None => Vec::new(),
            };
            let entries = this.with_tags(&path, share, entries, &old_rows);
            Ok((path, share, entries, old_rows))
        })
        .await?;
        let (path, share, entries, old_rows) = match scanned {
            Ok(v) => v,
            Err(IndexError::NotFound | IndexError::NotADirectory | IndexError::OutsideShare) if prev_rev.is_some() => {
                self.drop_dir(id)?;
                return Err(IndexError::NotFound);
            }
            Err(e) => return Err(e),
        };
        let (dir_rev, rows, removed) = self.reconcile(id, &path, share, old_rows, entries, prev_rev)?;
        let listing = Arc::new(encode_listing(id, dir_rev, rows));
        let mut st = self.state()?;
        st.lru.put(id.to_owned(), listing.clone());
        let removed = removed.into_iter().filter(|r| st.indexed.contains_key(r)).collect();
        Ok((listing, removed))
    }

    fn drop_dir(&self, id: &str) -> Result<(), IndexError> {
        let mut st = self.state()?;
        st.lru.pop(id);
        if let Some((_, path)) = st.indexed.remove(id) {
            st.seq += 1;
            let seq = st.seq;
            for set in st.have.values_mut() {
                set.remove(id);
            }
            self.db.send(Op::DropDir { dir_id: id.to_owned(), seq })?;
            if let Some(w) = self.watcher.get() {
                w.unwatch_dir(&path);
            }
            tracing::debug!(dir = id, "dropped vanished directory");
        }
        Ok(())
    }

    /// Pairs scanned entries with their Finder tags (hidden entries dropped):
    /// read for every entry of a small folder, else only where the previous
    /// scan found some.
    fn with_tags(&self, dir: &Path, share: usize, entries: Vec<Entry>, old: &[Row]) -> Vec<(Entry, Option<Vec<u8>>)> {
        let all = entries.len() <= TAG_SCAN_MAX;
        let tagged: HashSet<FileKey> = if all {
            HashSet::new()
        } else {
            old.iter()
                .filter(|r| r.item.tags.is_some())
                .filter_map(|r| {
                    if !r.key.is_unset() {
                        return Some(r.key);
                    }
                    self.ids.key_of(r.item.id.parse().ok()?).ok().flatten()
                })
                .collect()
        };
        let at_root = dir == self.shares[share].path;
        let staging_here = at_root && self.shares[share].staging.starts_with(dir);
        entries
            .into_iter()
            .filter(|e| !Fs::skip(e.name.as_os_str()) && !(staging_here && e.name.as_os_str() == STAGING_NAME))
            .map(|e| {
                let read = all || tagged.contains(&e.key);
                let tags = if read { Fs::tags(&dir.join(e.name.as_os_str())) } else { None };
                (e, tags)
            })
            .collect()
    }

    /// Ids for a directory's scanned entries. Assigned ids follow the key,
    /// except that a new key at a name whose old key vanished from this
    /// directory keeps the old id (a save replaced the file: temp + rename,
    /// vim, Office, JetBrains). A key moved in from elsewhere keeps its id;
    /// new keys get new ids.
    fn entry_ids(&self, old: &[Row], entries: &[(Entry, Option<Vec<u8>>)]) -> Result<Vec<ItemId>, IndexError> {
        if self.ids.scheme() == IdScheme::Direct {
            return entries
                .iter()
                .map(|(e, _)| if e.key.is_unset() { Err(IndexError::NotFound) } else { self.id_for(&e.key) })
                .collect();
        }
        let by_key: HashMap<&FileKey, usize> = old.iter().enumerate().map(|(i, r)| (&r.key, i)).collect();
        let present: HashSet<&FileKey> = entries.iter().map(|(e, _)| &e.key).collect();
        let mut used = vec![false; old.len()];
        let mut out: Vec<Option<ItemId>> = vec![None; entries.len()];
        // 1. Unchanged: same key, same name.
        for (i, (e, _)) in entries.iter().enumerate() {
            if let Some(&j) = by_key.get(&e.key)
                && old[j].raw == e.name
            {
                used[j] = true;
                out[i] = Some(old[j].item.id.clone());
            }
        }
        // 2. Replaced: same name and kind, the old key is gone from the directory.
        let by_name: HashMap<(&RawName, Kind), usize> = old
            .iter()
            .enumerate()
            .filter(|(j, r)| !used[*j] && !present.contains(&r.key))
            .map(|(j, r)| ((&r.raw, r.item.kind), j))
            .collect();
        for (i, (e, _)) in entries.iter().enumerate() {
            if out[i].is_none()
                && let Some(&j) = by_name.get(&(&e.name, e.kind))
                && !used[j]
            {
                used[j] = true;
                let id = old[j].item.id.clone();
                if let Ok(n) = id.parse::<u64>() {
                    self.ids.repoint(n, &e.key)?;
                }
                out[i] = Some(id);
            }
        }
        // 3. Renamed within the directory; 4. moved in, or new.
        for (i, (e, _)) in entries.iter().enumerate() {
            if out[i].is_some() {
                continue;
            }
            out[i] = Some(match by_key.get(&e.key) {
                Some(&j) if !used[j] => {
                    used[j] = true;
                    old[j].item.id.clone()
                }
                _ => self.id_for(&e.key)?,
            });
        }
        Ok(out.into_iter().map(|o| o.unwrap_or_default()).collect())
    }

    /// Diffs a scan against the previous rows, stamping changes with a new seq.
    fn reconcile(
        &self,
        dir_id: &str,
        path: &Path,
        share: usize,
        old: Vec<Row>,
        entries: Vec<(Entry, Option<Vec<u8>>)>,
        prev_rev: Option<u64>,
    ) -> Result<(u64, Vec<Row>, Vec<ItemId>), IndexError> {
        let ids = self.entry_ids(&old, &entries)?;
        let direct = self.ids.scheme() == IdScheme::Direct;
        let mut fresh: Vec<Row> = Vec::with_capacity(entries.len());
        for ((mut e, tags), id) in entries.into_iter().zip(ids) {
            if direct && !self.ids.encodable(&e.key) {
                // Another volume mounted inside the share: not served.
                e.kind = Kind::Other;
            }
            fresh.push(row_of(id, dir_id, e, tags));
        }
        if self.shares[share].needs_aliases() {
            apply_aliases(&mut fresh);
        }
        let mut old: HashMap<ItemId, Row> = old.into_iter().map(|r| (r.item.id.clone(), r)).collect();
        let mut st = self.state()?;
        let mut new_seq: Option<u64> = None;
        let mut counter = st.seq;
        fn alloc(new_seq: &mut Option<u64>, counter: &mut u64) -> u64 {
            *new_seq.get_or_insert_with(|| {
                *counter += 1;
                *counter
            })
        }
        let mut rows = Vec::with_capacity(fresh.len());
        let mut upserts = Vec::new();
        for mut row in fresh {
            match old.remove(&row.item.id) {
                Some(o) if same_attrs(&o, &row) => rows.push(o),
                Some(o) => {
                    let s = alloc(&mut new_seq, &mut counter);
                    row.item.rev = s;
                    let (oi, ni) = (&o.item, &row.item);
                    let content_changed = oi.size != ni.size
                        || oi.mtime_ns != ni.mtime_ns
                        || oi.kind != ni.kind
                        || (!o.key.is_unset() && o.key != row.key);
                    row.item.content_rev = if content_changed { s } else { oi.content_rev };
                    upserts.push(row.clone());
                    rows.push(row);
                }
                None => {
                    let s = alloc(&mut new_seq, &mut counter);
                    row.item.rev = s;
                    row.item.content_rev = s;
                    upserts.push(row.clone());
                    rows.push(row);
                }
            }
        }
        let removed: Vec<ItemId> = old.into_keys().collect();
        if !removed.is_empty() || prev_rev.is_none() {
            alloc(&mut new_seq, &mut counter);
        }
        st.seq = counter;
        let changed = new_seq.is_some();
        let dir_rev = new_seq.or(prev_rev).unwrap_or(counter);
        let prev_path = st.indexed.get(dir_id).map(|(_, p)| p.clone());
        let path_changed = prev_path.as_deref() != Some(path);
        st.indexed.insert(dir_id.to_owned(), (dir_rev, path.to_owned()));
        if path_changed && let Some(w) = self.watcher.get() {
            if let Some(p) = &prev_path {
                w.unwatch_dir(p);
            }
            w.watch_dir(path);
        }
        if changed || path_changed {
            self.note_locations(&upserts);
            // Sent under the state lock so the writer sees seqs in order.
            self.db.send(Op::Listing {
                dir_id: dir_id.to_owned(),
                dir_rev,
                path: path.to_owned(),
                upserts,
                removed: removed.clone(),
                seq: dir_rev,
            })?;
        }
        drop(st);
        if changed && prev_rev.is_some() {
            tracing::debug!(dir = dir_id, dir_rev, removed = removed.len(), "directory changed");
        }
        rows.sort_unstable_by(|a, b| a.item.name.cmp(&b.item.name));
        Ok((dir_rev, rows, removed))
    }

    /// Current attributes of one item.
    pub async fn stat(self: &Arc<Self>, id: &str) -> Result<Item, IndexError> {
        let id = self.canonical(id);
        if id == proto::ROOT_ID || id == proto::TRASH_ID {
            return Ok(virtual_dir(&id, ""));
        }
        if let Some(r) = self.root.find(&id) {
            return Ok(r.item.clone());
        }
        let this = self.clone();
        let idc = id.clone();
        let (fresh, row) = tokio::task::spawn_blocking(move || -> Result<_, IndexError> {
            let (path, share) = match this.resolve_any(&idc)? {
                Target::Path { path, share } => (path, share),
                Target::Trashed { .. } => {
                    return Ok((
                        None,
                        this.trashed_item(&idc)?.map(|item| Row {
                            item,
                            key: FileKey::default(),
                            raw: RawName::default(),
                        }),
                    ));
                }
                Target::Root => return Err(IndexError::NotFound),
            };
            let e = Fs::lstat(&path).map_err(IndexError::from_io)?;
            let parent_id = this.parent_id_of(&path, share)?;
            let tags = Fs::tags(&path);
            Ok((Some(row_of(idc.clone(), &parent_id, e, tags)), this.db.row(&idc)?))
        })
        .await??;
        let Some(fresh) = fresh else { return row.map(|r| r.item).ok_or(IndexError::NotFound) };
        if let Some(row) = row.filter(|r| same_attrs(r, &fresh)) {
            return Ok(row.item);
        }
        // The index is behind the filesystem (or never saw the item, or the
        // listing shows it under an alias): rescan its parent if that is
        // indexed so the item gets a proper rev.
        if self.state()?.indexed.contains_key(&fresh.item.parent_id) {
            let listing = self.refresh(&fresh.item.parent_id, false).await?;
            if let Some(r) = listing.find(&id) {
                return Ok(r.item.clone());
            }
        }
        Ok(fresh.item)
    }

    /// Path and content rev of a regular file (for thumbnails).
    pub fn file_path(&self, id: &str) -> Result<(PathBuf, u64), IndexError> {
        let Target::Path { path, .. } = self.resolve(id)? else { return Err(IndexError::NotAFile) };
        let e = Fs::lstat(&path).map_err(IndexError::from_io)?;
        if e.kind != Kind::File {
            return Err(IndexError::NotAFile);
        }
        if e.flags & proto::item_flags::CLOUD_ONLY != 0 && Fs::OS != proto::Os::MacOs {
            return Err(IndexError::CloudOnly);
        }
        let content_rev = self.db.row(id)?.map_or(0, |r| r.item.content_rev);
        Ok((path, content_rev))
    }

    /// The key an item id names.
    fn key_of(&self, id: &str) -> Result<FileKey, IndexError> {
        let n: u64 = id.parse().map_err(|_| IndexError::BadId(id.to_owned()))?;
        if let Some(s) = self.shares.iter().find(|s| s.num_id == n) {
            return Ok(s.key);
        }
        self.ids.key_of(n)?.ok_or(IndexError::NotFound)
    }

    /// Opens a regular file for reading. Returns the file, its size, name and content rev.
    pub fn open_file(&self, id: &str) -> Result<(File, u64, String, u64), IndexError> {
        let path = match self.resolve_any(id)? {
            Target::Path { path, .. } | Target::Trashed { path } => path,
            Target::Root => return Err(IndexError::NotAFile),
        };
        let file = Fs::open_read(&path).map_err(|e| match Fs::classify(&e) {
            ErrKind::Symlink => IndexError::NotAFile,
            _ => IndexError::from_io(e),
        })?;
        let st = file.stat()?;
        if st.kind != Kind::File {
            return Err(IndexError::NotAFile);
        }
        if !st.key.same_file(&self.key_of(id)?) {
            // Replaced between resolving and opening.
            return Err(IndexError::NotFound);
        }
        let name = RawName::of_path(&path).map(|n| n.display().0).unwrap_or_default();
        let content_rev = self.db.row(id)?.map_or(0, |r| r.item.content_rev);
        Ok((file, st.size, name, content_rev))
    }

    /// A symlink's target as a path relative to the link's directory, which
    /// must resolve (lexically) inside the link's share.
    pub fn read_link(&self, id: &str) -> Result<String, IndexError> {
        let Target::Path { path, share } = self.resolve(id)? else { return Err(IndexError::NotFound) };
        let target = std::fs::read_link(&path).map_err(|e| match Fs::classify(&e) {
            ErrKind::BadName => IndexError::NotFound,
            _ => IndexError::from_io(e),
        })?;
        let dir = path.parent().ok_or(IndexError::OutsideShare)?;
        let root = &self.shares[share].path;
        let rel_dir = dir.strip_prefix(root).map_err(|_| IndexError::OutsideShare)?;
        // The target's components relative to the share root.
        let mut stack: Vec<std::ffi::OsString> = Vec::new();
        let rest = if target.is_absolute() {
            target.strip_prefix(root).map_err(|_| IndexError::OutsideShare)?.to_path_buf()
        } else {
            stack.extend(rel_dir.components().map(|c| c.as_os_str().to_owned()));
            target.clone()
        };
        for c in rest.components() {
            match c {
                std::path::Component::Normal(n) => stack.push(n.to_owned()),
                std::path::Component::ParentDir => {
                    stack.pop().ok_or(IndexError::OutsideShare)?;
                }
                std::path::Component::CurDir => {}
                _ => return Err(IndexError::OutsideShare),
            }
        }
        if !target.is_absolute() {
            return Ok(target.to_string_lossy().into_owned());
        }
        // Absolute inside the share: rewrite relative to the link's directory.
        let depth = rel_dir.components().count();
        let mut out = PathBuf::new();
        let common = rel_dir.components().zip(stack.iter()).take_while(|(a, b)| a.as_os_str() == b.as_os_str()).count();
        for _ in common..depth {
            out.push("..");
        }
        for c in &stack[common..] {
            out.push(c);
        }
        if out.as_os_str().is_empty() {
            out.push(".");
        }
        Ok(out.to_string_lossy().into_owned())
    }

    /// Filters a change set to the directories `client` has listed. A
    /// tombstone for an item that moved to another listed directory is
    /// dropped: the client sees the move as a change of parent.
    pub fn filter_for_client(
        &self,
        client: &str,
        changed: Vec<Item>,
        deleted: Vec<crate::db::Tombstone>,
    ) -> Result<(Vec<Item>, Vec<Deleted>), IndexError> {
        let st = self.state()?;
        let Some(have) = st.have.get(client) else { return Ok((Vec::new(), Vec::new())) };
        // Every client sees the trash container.
        let has = |p: &str| p == proto::TRASH_ID || have.contains(p);
        let changed: Vec<Item> = changed.into_iter().filter(|i| has(&i.parent_id)).collect();
        let alive: HashSet<&str> = changed.iter().map(|i| i.id.as_str()).collect();
        let deleted = deleted
            .into_iter()
            .filter(|(d, now)| has(&d.parent_id) && !alive.contains(d.id.as_str()) && !now.as_deref().is_some_and(has))
            .map(|(d, _)| d)
            .collect();
        Ok((changed, deleted))
    }

    /// The indexed directory containing directory `id` (last seen at `path`).
    fn indexed_parent(&self, id: &str, path: &Path) -> Result<Option<ItemId>, IndexError> {
        if id.starts_with(proto::SHARE_ID_PREFIX) {
            return Ok(None);
        }
        let Some(parent) = path.parent() else { return Ok(None) };
        let Ok(st) = Fs::lstat(parent) else { return Ok(None) };
        let Some(pid) = self.known_id(&st.key)? else { return Ok(None) };
        Ok(self.state()?.indexed.contains_key(&pid).then_some(pid))
    }

    /// Applies one batch of watcher hints: rescans affected indexed directories.
    pub async fn apply_changes(self: &Arc<Self>, changes: &[Change]) -> Result<usize, IndexError> {
        let mut dirs: Vec<ItemId> = Vec::new();
        let mut seen: HashSet<ItemId> = HashSet::new();
        {
            let st = self.state()?;
            if changes.contains(&Change::All) {
                tracing::warn!("change notifications were lost; rescanning every indexed directory");
                dirs.extend(st.indexed.keys().cloned());
            } else {
                for c in changes {
                    if let Change::Subtree(path) = c {
                        for (id, (_, p)) in &st.indexed {
                            if p.starts_with(path) && seen.insert(id.clone()) {
                                dirs.push(id.clone());
                            }
                        }
                    }
                }
            }
        }
        let this = self.clone();
        let paths: Vec<PathBuf> =
            changes.iter().filter_map(|c| if let Change::Path(p) = c { Some(p.clone()) } else { None }).collect();
        let mut candidates = tokio::task::spawn_blocking(move || {
            let mut out: Vec<(Option<FileKey>, PathBuf)> = Vec::new();
            let mut stat_seen: HashSet<PathBuf> = HashSet::new();
            for path in paths {
                if this.share_of(&path).is_none() {
                    continue;
                }
                for p in [Some(path.as_path()), path.parent()].into_iter().flatten() {
                    if stat_seen.insert(p.to_owned()) {
                        let key = Fs::lstat(p).ok().filter(|e| e.kind == Kind::Dir).map(|e| e.key);
                        out.push((key, p.to_owned()));
                    }
                }
            }
            out
        })
        .await?;
        {
            for (key, path) in candidates.drain(..) {
                let known = match key {
                    Some(k) => self.known_id(&k)?,
                    None => None,
                };
                let st = self.state()?;
                let id = match known {
                    Some(id) => id,
                    // Gone (or never had an id): find an indexed directory last seen at this path.
                    None => match st.indexed.iter().find(|(_, (_, p))| *p == path) {
                        Some((id, _)) => id.clone(),
                        None => continue,
                    },
                };
                if st.indexed.contains_key(&id) && seen.insert(id.clone()) {
                    dirs.push(id);
                }
            }
        }
        let mut n = 0;
        while let Some(id) = dirs.pop() {
            n += 1;
            let before = self.state()?.indexed.get(&id).map(|(r, p)| (*r, p.clone()));
            match self.refresh_inner(&id, false).await {
                Ok((listing, gone)) => {
                    // A directory that left may have moved elsewhere in the shares, or be gone.
                    dirs.extend(gone.into_iter().filter(|g| seen.insert(g.clone())));
                    // Its own mtime changed too, which only its parent's listing shows.
                    if let Some((_, path)) = before.filter(|(r, _)| listing.dir_rev != *r)
                        && let Some(parent) = self.indexed_parent(&id, &path)?
                        && seen.insert(parent.clone())
                    {
                        dirs.push(parent);
                    }
                }
                Err(IndexError::NotFound) => {}
                Err(e) => tracing::warn!(dir = %id, error = %e, "rescan failed"),
            }
        }
        Ok(n)
    }
}
