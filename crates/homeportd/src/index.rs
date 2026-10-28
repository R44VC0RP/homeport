//! The lazy directory index.
//!
//! A directory is scanned the first time a client lists it and is then kept
//! current by FSEvents. Every change is stamped with the next value of a
//! monotonic `seq`; listings are cached pre-encoded in an LRU keyed by
//! directory ID together with their `dir_rev`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use bytes::Bytes;
use homeport_proto::{self as proto, Deleted, Item, ItemId, Kind, Response};
use lru::LruCache;

use crate::db::{Db, DbError, Loaded, Op};
use crate::fsops::{self, Entry, Volume};

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
}

impl IndexError {
    pub fn code(&self) -> proto::ErrorCode {
        use proto::ErrorCode as C;
        match self {
            Self::NotFound => C::NotFound,
            Self::OutsideShare => C::OutsideShare,
            Self::NotADirectory => C::NotADirectory,
            Self::NotAFile => C::NotAFile,
            Self::BadId(_) => C::BadRequest,
            _ => C::Internal,
        }
    }

    fn from_io(e: io::Error) -> Self {
        match e.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ESTALE) => Self::NotFound,
            Some(libc::ENOTDIR) => Self::NotADirectory,
            Some(libc::ELOOP) => Self::NotADirectory,
            _ => Self::Io(e),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Share {
    pub name: String,
    pub id: ItemId,
    pub path: PathBuf,
    pub ino: u64,
}

/// A directory listing ready to send.
pub struct Listing {
    pub dir_rev: u64,
    pub items: Vec<Item>,
    /// Pre-encoded `Items` frames.
    pub frames: Vec<Bytes>,
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

pub struct Index {
    shares: Vec<Share>,
    volume: Volume,
    pub db: Arc<Db>,
    pub index_id: u64,
    pub tombstone_floor: u64,
    state: Mutex<State>,
    dir_locks: Mutex<HashMap<ItemId, Weak<tokio::sync::Mutex<()>>>>,
    root: Arc<Listing>,
}

/// What an ID refers to.
enum Target {
    Root,
    Path { path: PathBuf, share: usize },
}

fn same_attrs(a: &Item, b: &Item) -> bool {
    a.parent_id == b.parent_id
        && a.name == b.name
        && a.kind == b.kind
        && a.size == b.size
        && a.mtime_ns == b.mtime_ns
        && a.ctime_ns == b.ctime_ns
        && a.mode == b.mode
}

fn encode_listing(dir_id: &str, dir_rev: u64, items: Vec<Item>) -> Result<Listing, IndexError> {
    let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
    let pages: Vec<&[Item]> = if items.is_empty() { vec![&[]] } else { items.chunks(proto::PAGE_SIZE).collect() };
    let n = pages.len();
    let mut frames = Vec::with_capacity(n);
    for (i, page) in pages.into_iter().enumerate() {
        frames.push(proto::encode_frame(&Response::Items {
            dir_id: dir_id.to_owned(),
            dir_rev,
            total,
            page: i as u32,
            last: i + 1 == n,
            items: page.to_vec(),
        })?);
    }
    Ok(Listing { dir_rev, items, frames })
}

impl Index {
    pub fn new(
        shares: Vec<(String, PathBuf)>,
        db: Arc<Db>,
        loaded: Loaded,
        lru_capacity: usize,
    ) -> Result<Self, IndexError> {
        let mut out = Vec::new();
        let mut volume: Option<Volume> = None;
        let mut root_items = Vec::new();
        for (name, path) in shares {
            let path = std::fs::canonicalize(&path)?;
            let st = fsops::lstat(&path)?;
            if st.kind != Kind::Dir {
                return Err(IndexError::NotADirectory);
            }
            let vol = Volume::of(&path)?;
            match &volume {
                Some(v) if !v.same_as(&vol) => {
                    return Err(IndexError::Io(io::Error::other(format!(
                        "share {name} is on a different volume; all shares must share one volume"
                    ))));
                }
                Some(_) => {}
                None => volume = Some(vol),
            }
            let id = format!("{}{name}", proto::SHARE_ID_PREFIX);
            root_items.push(Item {
                id: id.clone(),
                parent_id: proto::ROOT_ID.to_owned(),
                name: name.clone(),
                kind: Kind::Dir,
                size: st.size,
                mtime_ns: st.mtime_ns,
                ctime_ns: st.ctime_ns,
                mode: st.mode,
                rev: 1,
                content_rev: 1,
            });
            out.push(Share { name, id, path, ino: st.ino });
        }
        let volume = volume.ok_or_else(|| IndexError::Io(io::Error::other("no shares configured")))?;
        let root = Arc::new(encode_listing(proto::ROOT_ID, 1, root_items)?);
        let cap = NonZeroUsize::new(lru_capacity.max(1)).unwrap_or(NonZeroUsize::MIN);
        let state = State { seq: loaded.seq, lru: LruCache::new(cap), indexed: loaded.dirs, have: loaded.have };
        Ok(Self {
            shares: out,
            volume,
            db,
            index_id: loaded.index_id,
            tombstone_floor: loaded.tombstone_floor,
            state: Mutex::new(state),
            dir_locks: Mutex::new(HashMap::new()),
            root,
        })
    }

    pub fn shares(&self) -> &[Share] {
        &self.shares
    }

    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    pub fn roots(&self) -> Vec<Item> {
        self.root.items.clone()
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, IndexError> {
        self.state.lock().map_err(|_| IndexError::Poisoned)
    }

    pub fn committed_seq(&self) -> u64 {
        *self.db.committed.borrow()
    }

    /// Item ID for a file ID (share roots get their `root:` IDs).
    fn id_for(&self, ino: u64) -> ItemId {
        match self.shares.iter().find(|s| s.ino == ino) {
            Some(s) => s.id.clone(),
            None => ino.to_string(),
        }
    }

    fn share_of(&self, path: &Path) -> Option<usize> {
        self.shares.iter().position(|s| path.starts_with(&s.path))
    }

    /// Maps an ID to its current path, rejecting anything outside the shares.
    fn resolve(&self, id: &str) -> Result<Target, IndexError> {
        if id == proto::ROOT_ID {
            return Ok(Target::Root);
        }
        if let Some(name) = id.strip_prefix(proto::SHARE_ID_PREFIX) {
            let share = self.shares.iter().position(|s| s.name == name).ok_or(IndexError::NotFound)?;
            return Ok(Target::Path { path: self.shares[share].path.clone(), share });
        }
        let ino: u64 = id.parse().map_err(|_| IndexError::BadId(id.to_owned()))?;
        if let Some(share) = self.shares.iter().position(|s| s.ino == ino) {
            let s = &self.shares[share];
            return Ok(Target::Path { path: s.path.clone(), share });
        }
        let path = self.volume.path_of(ino).map_err(IndexError::from_io)?;
        let share = self.share_of(&path).ok_or(IndexError::OutsideShare)?;
        Ok(Target::Path { path, share })
    }

    fn parent_id_of(&self, path: &Path, share: usize) -> Result<ItemId, IndexError> {
        if path == self.shares[share].path {
            return Ok(proto::ROOT_ID.to_owned());
        }
        let parent = path.parent().ok_or(IndexError::NotFound)?;
        let st = fsops::lstat(parent).map_err(IndexError::from_io)?;
        Ok(self.id_for(st.ino))
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
            Ok(ino) => self.id_for(ino),
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
        let (old, prev_rev) = {
            let mut st = self.state()?;
            if use_cache && let Some(l) = st.lru.get(id) {
                return Ok((l.clone(), Vec::new()));
            }
            let prev_rev = st.indexed.get(id).map(|(r, _)| *r);
            (st.lru.peek(id).cloned(), prev_rev)
        };
        let this = self.clone();
        let id_owned = id.to_owned();
        let scanned = tokio::task::spawn_blocking(move || -> Result<_, IndexError> {
            let target = this.resolve(&id_owned)?;
            let Target::Path { path, .. } = target else { return Err(IndexError::BadId(id_owned)) };
            let entries = fsops::scan_dir(&path).map_err(IndexError::from_io)?;
            let old_items = match old {
                Some(l) => l.items.clone(),
                None if prev_rev.is_some() => this.db.dir_items(&id_owned)?,
                None => Vec::new(),
            };
            Ok((path, entries, old_items))
        })
        .await?;
        let (path, entries, old_items) = match scanned {
            Ok(v) => v,
            Err(IndexError::NotFound | IndexError::NotADirectory | IndexError::OutsideShare) if prev_rev.is_some() => {
                self.drop_dir(id)?;
                return Err(IndexError::NotFound);
            }
            Err(e) => return Err(e),
        };
        let (dir_rev, items, removed) = self.reconcile(id, &path, old_items, entries, prev_rev)?;
        let listing = Arc::new(encode_listing(id, dir_rev, items)?);
        let mut st = self.state()?;
        st.lru.put(id.to_owned(), listing.clone());
        let removed = removed.into_iter().filter(|r| st.indexed.contains_key(r)).collect();
        Ok((listing, removed))
    }

    fn drop_dir(&self, id: &str) -> Result<(), IndexError> {
        let mut st = self.state()?;
        st.lru.pop(id);
        if st.indexed.remove(id).is_some() {
            st.seq += 1;
            let seq = st.seq;
            for set in st.have.values_mut() {
                set.remove(id);
            }
            self.db.send(Op::DropDir { dir_id: id.to_owned(), seq })?;
            tracing::debug!(dir = id, "dropped vanished directory");
        }
        Ok(())
    }

    /// Diffs a scan against the previous items, stamping changes with a new seq.
    fn reconcile(
        &self,
        dir_id: &str,
        path: &Path,
        old: Vec<Item>,
        entries: Vec<Entry>,
        prev_rev: Option<u64>,
    ) -> Result<(u64, Vec<Item>, Vec<ItemId>), IndexError> {
        let mut old: HashMap<ItemId, Item> = old.into_iter().map(|i| (i.id.clone(), i)).collect();
        let mut st = self.state()?;
        let mut new_seq: Option<u64> = None;
        let mut counter = st.seq;
        fn alloc(new_seq: &mut Option<u64>, counter: &mut u64) -> u64 {
            *new_seq.get_or_insert_with(|| {
                *counter += 1;
                *counter
            })
        }
        let mut items = Vec::with_capacity(entries.len());
        let mut upserts = Vec::new();
        for e in entries {
            let mut item = Item {
                id: self.id_for(e.ino),
                parent_id: dir_id.to_owned(),
                name: e.name,
                kind: e.kind,
                size: e.size,
                mtime_ns: e.mtime_ns,
                ctime_ns: e.ctime_ns,
                mode: e.mode,
                rev: 0,
                content_rev: 0,
            };
            match old.remove(&item.id) {
                Some(o) if same_attrs(&o, &item) => items.push(o),
                Some(o) => {
                    let s = alloc(&mut new_seq, &mut counter);
                    item.rev = s;
                    let content_changed = o.size != item.size || o.mtime_ns != item.mtime_ns || o.kind != item.kind;
                    item.content_rev = if content_changed { s } else { o.content_rev };
                    upserts.push(item.clone());
                    items.push(item);
                }
                None => {
                    let s = alloc(&mut new_seq, &mut counter);
                    item.rev = s;
                    item.content_rev = s;
                    upserts.push(item.clone());
                    items.push(item);
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
        let path_changed = st.indexed.get(dir_id).is_none_or(|(_, p)| p != path);
        st.indexed.insert(dir_id.to_owned(), (dir_rev, path.to_owned()));
        if changed || path_changed {
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
        items.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok((dir_rev, items, removed))
    }

    /// Current attributes of one item.
    pub async fn stat(self: &Arc<Self>, id: &str) -> Result<Item, IndexError> {
        let id = self.canonical(id);
        if id == proto::ROOT_ID {
            return Ok(Item {
                id: id.clone(),
                parent_id: id,
                name: String::new(),
                kind: Kind::Dir,
                size: 0,
                mtime_ns: 0,
                ctime_ns: 0,
                mode: 0o755,
                rev: 1,
                content_rev: 1,
            });
        }
        if let Some(it) = self.root.items.iter().find(|i| i.id == id) {
            return Ok(it.clone());
        }
        let this = self.clone();
        let idc = id.clone();
        let (fresh, row) = tokio::task::spawn_blocking(move || -> Result<_, IndexError> {
            let Target::Path { path, share, .. } = this.resolve(&idc)? else { return Err(IndexError::NotFound) };
            let e = fsops::lstat(&path).map_err(IndexError::from_io)?;
            let parent_id = this.parent_id_of(&path, share)?;
            let fresh = Item {
                id: idc.clone(),
                parent_id,
                name: e.name,
                kind: e.kind,
                size: e.size,
                mtime_ns: e.mtime_ns,
                ctime_ns: e.ctime_ns,
                mode: e.mode,
                rev: 0,
                content_rev: 0,
            };
            Ok((fresh, this.db.item(&idc)?))
        })
        .await??;
        if let Some(row) = row.filter(|r| same_attrs(r, &fresh)) {
            return Ok(row);
        }
        // The index is behind the filesystem (or never saw the item): rescan
        // its parent if that is indexed so the item gets a proper rev.
        if self.state()?.indexed.contains_key(&fresh.parent_id) {
            let listing = self.refresh(&fresh.parent_id, false).await?;
            if let Some(it) = listing.items.iter().find(|i| i.id == id) {
                return Ok(it.clone());
            }
        }
        Ok(fresh)
    }

    /// Opens a regular file for reading. Returns the file, its size, name and content rev.
    pub fn open_file(&self, id: &str) -> Result<(OwnedFd, u64, String, u64), IndexError> {
        let Target::Path { path, .. } = self.resolve(id)? else { return Err(IndexError::NotAFile) };
        let fd = fsops::open_nofollow(&path, libc::O_RDONLY).map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => IndexError::NotAFile,
            _ => IndexError::from_io(e),
        })?;
        let st = fsops::fstat(&fd)?;
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(IndexError::NotAFile);
        }
        if st.st_ino.to_string() != id {
            // Replaced between resolving and opening.
            return Err(IndexError::NotFound);
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let content_rev = self.db.item(id)?.map_or(0, |i| i.content_rev);
        Ok((fd, u64::try_from(st.st_size).unwrap_or(0), name, content_rev))
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
        let changed: Vec<Item> = changed.into_iter().filter(|i| have.contains(&i.parent_id)).collect();
        let alive: HashSet<&str> = changed.iter().map(|i| i.id.as_str()).collect();
        let deleted = deleted
            .into_iter()
            .filter(|(d, now)| {
                have.contains(&d.parent_id)
                    && !alive.contains(d.id.as_str())
                    && !now.as_ref().is_some_and(|p| have.contains(p))
            })
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
        let Ok(st) = fsops::lstat(parent) else { return Ok(None) };
        let pid = self.id_for(st.ino);
        Ok(self.state()?.indexed.contains_key(&pid).then_some(pid))
    }

    /// Applies one batch of FSEvents: rescans affected indexed directories.
    pub async fn apply_events(self: &Arc<Self>, events: &[(PathBuf, u32)]) -> Result<usize, IndexError> {
        use crate::fsevents::flags;
        let mut dirs: Vec<ItemId> = Vec::new();
        let mut seen: HashSet<ItemId> = HashSet::new();
        {
            let st = self.state()?;
            let full = events.iter().any(|(_, f)| f & (flags::USER_DROPPED | flags::KERNEL_DROPPED) != 0);
            if full {
                tracing::warn!("FSEvents dropped events; rescanning every indexed directory");
                dirs.extend(st.indexed.keys().cloned());
            } else {
                for (path, f) in events {
                    if f & flags::MUST_SCAN_SUBDIRS != 0 {
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
        let events = events.to_vec();
        let mut candidates = tokio::task::spawn_blocking(move || {
            let mut out: Vec<(Option<u64>, PathBuf)> = Vec::new();
            let mut stat_seen: HashSet<PathBuf> = HashSet::new();
            for (path, _) in events {
                if this.share_of(&path).is_none() {
                    continue;
                }
                for p in [Some(path.as_path()), path.parent()].into_iter().flatten() {
                    if stat_seen.insert(p.to_owned()) {
                        let ino = fsops::lstat(p).ok().filter(|e| e.kind == Kind::Dir).map(|e| e.ino);
                        out.push((ino, p.to_owned()));
                    }
                }
            }
            out
        })
        .await?;
        {
            let st = self.state()?;
            for (ino, path) in candidates.drain(..) {
                let id = match ino {
                    Some(ino) => self.id_for(ino),
                    // Gone: find an indexed directory last seen at this path.
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
