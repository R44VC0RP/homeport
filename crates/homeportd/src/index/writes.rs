//! Client writes: creates, content updates, renames and moves, Trash.
//!
//! Every operation resolves items by id, works through handles of the
//! directories involved (`*at`-style calls that never follow a symlink in the
//! last component), holds those directories' locks, and records its own
//! result in the index (with a new seq) before answering. The rescan the
//! watcher triggers then finds nothing new, so a client's write never comes
//! back to it as a remote change.
//!
//! Names from clients are display names: they become raw names through
//! `RawName::from_display`, and a name equal to an item's current (possibly
//! aliased) display name means "keep the raw name". On filesystems that tell
//! case or normalization apart, a name that folds to a sibling's is a
//! collision even though the filesystem would accept it.

use std::ffi::OsStr;

use homeport_proto::Meta;

use super::*;
use crate::platform::{Durability, SetMeta, Sys, SysOps, same_content};

/// Per-share staging directory, used when a share is not on the state
/// directory's volume. Hidden from listings.
pub const STAGING_NAME: &str = ".homeport-staging";

/// Creates a staging directory and clears uploads left by a previous run.
pub fn clear_staging(staging: &Path) -> io::Result<()> {
    std::fs::create_dir_all(staging)?;
    for e in std::fs::read_dir(staging)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("up-") {
            let _ = std::fs::remove_file(e.path());
        } else if name.starts_with("backup-") {
            // A content write did not finish: its target may be partial.
            tracing::warn!(path = %e.path().display(), "backup of an interrupted write left in staging");
        }
    }
    Ok(())
}

/// A received upload in a staging directory; removed on drop if still there.
pub struct Staged {
    pub dir: PathBuf,
    pub name: String,
}

impl Staged {
    pub fn new(dir: &Path) -> Self {
        let n: u64 = rand::random();
        Self { dir: dir.to_owned(), name: format!("up-{n:016x}") }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    fn os_name(&self) -> &OsStr {
        OsStr::new(&self.name)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.path());
    }
}

/// An item to record in a directory's listing.
struct Upsert {
    entry: Entry,
    tags: Option<Vec<u8>>,
    /// `None`: the content changed (a new content rev).
    content_rev: Option<u64>,
    /// The item's id when known (it stays even if the file key changed).
    id: Option<ItemId>,
}

enum Created {
    New(Entry),
    Existing(Entry),
    Taken(Entry),
}

enum Written {
    InPlace(Entry),
    Same(Entry),
    Conflict { original: Entry, copy: Entry },
}

pub struct CreateReq {
    pub op_id: String,
    pub parent_id: ItemId,
    pub name: String,
    pub kind: Kind,
    pub link_target: Option<String>,
    pub meta: Meta,
    pub may_exist: bool,
}

fn set_meta(m: &Meta) -> SetMeta<'_> {
    SetMeta {
        mode: m.mode,
        hidden: m.hidden,
        tags: m.tags.as_ref().map(|t| t.as_deref()),
        xattrs: &m.xattrs,
        crtime_ns: m.crtime_ns,
        mtime_ns: m.mtime_ns,
    }
}

fn io_err(e: io::Error) -> IndexError {
    IndexError::from_io(e)
}

fn is_exists(e: &io::Error) -> bool {
    Fs::classify(e) == ErrKind::Exists
}

/// `name` with ` (suffix)` before its extension: `a.txt` -> `a (suffix).txt`.
fn with_suffix(name: &str, suffix: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({suffix}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({suffix})"),
    }
}

fn conflict_name(name: &str, device: &str, n: u32) -> String {
    let device = if device.is_empty() { "client" } else { device };
    let suffix = format!("{device} conflict {}", Sys::local_stamp());
    if n <= 1 { with_suffix(name, &suffix) } else { with_suffix(name, &format!("{suffix} {n}")) }
}

/// Renames `from` in `src` to the first free name `name_for(n)` in `dst`.
fn rename_free(src: &Dir, from: &OsStr, dst: &Dir, name_for: impl Fn(u32) -> String) -> Result<RawName, IndexError> {
    for n in 1..1000 {
        let to = RawName::from_display(&name_for(n))?;
        match src.rename_noreplace(from, dst, to.as_os_str()) {
            Ok(()) => return Ok(to),
            Err(e) if is_exists(&e) => continue,
            Err(e) => return Err(io_err(e)),
        }
    }
    Err(IndexError::Io(io::Error::other("no free name")))
}

fn raw_of(path: &Path) -> Result<RawName, IndexError> {
    RawName::of_path(path).ok_or(IndexError::NotFound)
}

/// A sibling (other than `except`) whose name folds like `name`.
fn fold_collision<'a>(rows: &'a [Row], name: &str, except: Option<&str>) -> Option<&'a Row> {
    let key = names::fold_key(name);
    rows.iter().find(|r| Some(r.item.id.as_str()) != except && names::fold_key(&r.item.name) == key)
}

impl Index {
    /// Where an upload for an item in (or into) `id` should be received.
    pub fn staging_for(&self, id: &str) -> PathBuf {
        let share = match self.canonical(id).strip_prefix(proto::SHARE_ID_PREFIX) {
            Some(name) => self.shares.iter().position(|s| s.name == name),
            None => match self.resolve(id) {
                Ok(Target::Path { share, .. }) => Some(share),
                _ => None,
            },
        };
        share.map_or_else(|| self.writes.staging.clone(), |s| self.shares[s].staging.clone())
    }

    /// The item an earlier create with this op id made, if any.
    pub fn op_replay(&self, op_id: &str) -> Option<ItemId> {
        self.ops.lock().ok()?.get(op_id).cloned()
    }

    fn record_op(&self, op_id: &str, id: &str) -> Result<(), IndexError> {
        let mut ops = self.ops.lock().map_err(|_| IndexError::Poisoned)?;
        if ops.len() > 200_000 {
            ops.clear();
        }
        ops.insert(op_id.to_owned(), id.to_owned());
        drop(ops);
        self.db.send(Op::CreateDone { op_id: op_id.to_owned(), file_id: id.to_owned() })?;
        Ok(())
    }

    /// File key of a directory item (share roots have `root:` IDs).
    fn dir_key(&self, id: &str) -> Option<FileKey> {
        match id.strip_prefix(proto::SHARE_ID_PREFIX) {
            Some(name) => self.shares.iter().find(|s| s.name == name).map(|s| s.key),
            None => self.key_of(id).ok(),
        }
    }

    /// Path of a share directory that may receive items.
    fn writable_dir(&self, id: &str) -> Result<(ItemId, PathBuf, usize), IndexError> {
        let id = self.canonical(id);
        if id == proto::ROOT_ID || id == proto::TRASH_ID {
            return Err(IndexError::Denied);
        }
        match self.resolve(&id)? {
            Target::Path { path, share } => Ok((id, path, share)),
            _ => Err(IndexError::NotADirectory),
        }
    }

    fn trash_impl(&self) -> Result<&Trash, IndexError> {
        self.writes.trash.as_ref().ok_or_else(|| IndexError::Unsupported("this server has no trash".into()))
    }

    /// Current rows of a directory, indexing it first if needed. Call with its lock held.
    async fn prepare_dir(self: &Arc<Self>, dir_id: &str) -> Result<Vec<Row>, IndexError> {
        let (cached, indexed) = {
            let mut st = self.state()?;
            (st.lru.get(dir_id).cloned(), st.indexed.contains_key(dir_id))
        };
        if let Some(l) = cached {
            return Ok(l.rows.clone());
        }
        if indexed {
            self.db.flush().await?;
        }
        let (listing, _) = self.refresh_locked(dir_id, false).await?;
        Ok(listing.rows.clone())
    }

    /// Records changes the server made itself in `dir_id` (whose lock the
    /// caller holds), stamped with one new seq. Returns the upserted items.
    fn commit_local(
        &self,
        dir_id: &str,
        dir_path: &Path,
        mut rows: Vec<Row>,
        ups: Vec<Upsert>,
        removed: &[ItemId],
    ) -> Result<Vec<Item>, IndexError> {
        // Ids first: they may need the database.
        let mut fresh = Vec::with_capacity(ups.len());
        for u in ups {
            if Fs::skip(u.entry.name.as_os_str()) {
                continue;
            }
            let id = match u.id {
                Some(id) => {
                    if let Ok(n) = id.parse::<u64>()
                        && self.ids.scheme() == IdScheme::Assigned
                    {
                        self.ids.repoint(n, &u.entry.key)?;
                    }
                    id
                }
                None => self.id_for(&u.entry.key)?,
            };
            let content_rev = u.content_rev;
            fresh.push((row_of(id, dir_id, u.entry, u.tags), content_rev));
        }
        let aliases = self.share_of(dir_path).is_some_and(|s| self.shares[s].needs_aliases());
        let mut st = self.state()?;
        st.seq += 1;
        let seq = st.seq;
        rows.retain(|r| !removed.contains(&r.item.id));
        let mut out_ids: Vec<ItemId> = Vec::with_capacity(fresh.len());
        for (mut row, content_rev) in fresh {
            row.item.rev = seq;
            row.item.content_rev = content_rev.unwrap_or(seq);
            out_ids.push(row.item.id.clone());
            match rows.iter().position(|r| r.item.id == row.item.id) {
                Some(p) => rows[p] = row,
                None => rows.push(row),
            }
        }
        if aliases {
            // A new name can alias (or un-alias) a sibling: that is a change too.
            for i in apply_aliases(&mut rows) {
                if !out_ids.contains(&rows[i].item.id) {
                    rows[i].item.rev = seq;
                    out_ids.push(rows[i].item.id.clone());
                }
            }
        }
        let upserts: Vec<Row> =
            out_ids.iter().filter_map(|id| rows.iter().find(|r| r.item.id == *id).cloned()).collect();
        rows.sort_unstable_by(|a, b| a.item.name.cmp(&b.item.name));
        let listing = Arc::new(encode_listing(dir_id, seq, rows));
        st.lru.put(dir_id.to_owned(), listing);
        let prev = st.indexed.insert(dir_id.to_owned(), (seq, dir_path.to_owned()));
        if prev.is_none()
            && let Some(w) = self.watcher.get()
        {
            w.watch_dir(dir_path);
        }
        self.note_locations(&upserts);
        let out = upserts.iter().map(|r| r.item.clone()).collect();
        // Sent under the state lock so the writer sees seqs in order.
        self.db.send(Op::Listing {
            dir_id: dir_id.to_owned(),
            dir_rev: seq,
            path: dir_path.to_owned(),
            upserts,
            removed: removed.to_vec(),
            seq,
        })?;
        Ok(out)
    }

    /// Runs `f` with the locks of `ids` held (in a fixed order).
    async fn locked<T>(
        self: &Arc<Self>,
        ids: &[&str],
        f: impl AsyncFnOnce() -> Result<T, IndexError>,
    ) -> Result<T, IndexError> {
        let mut ids: Vec<&str> = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let locks = ids.iter().map(|id| self.dir_lock(id)).collect::<Result<Vec<_>, _>>()?;
        let mut guards = Vec::with_capacity(locks.len());
        for l in &locks {
            guards.push(l.lock().await);
        }
        f().await
    }

    /// The listing's item for an entry, recording it if the index is behind.
    fn known_or_commit(&self, dir_id: &str, dir_path: &Path, rows: Vec<Row>, e: Entry) -> Result<Item, IndexError> {
        if let Some(id) = self.known_id(&e.key)?
            && let Some(r) =
                rows.iter().find(|r| r.item.id == id && r.item.size == e.size && r.item.mtime_ns == e.mtime_ns)
        {
            return Ok(r.item.clone());
        }
        let tags = Fs::tags(&dir_path.join(e.name.as_os_str()));
        let mut out = self.commit_local(
            dir_id,
            dir_path,
            rows,
            vec![Upsert { entry: e, tags, content_rev: None, id: None }],
            &[],
        )?;
        out.pop().ok_or(IndexError::NotFound)
    }

    /// Creates a file (from `staged`), folder or symlink.
    pub async fn create(
        self: &Arc<Self>,
        req: CreateReq,
        staged: Option<Staged>,
        client: Option<&str>,
    ) -> Result<Item, IndexError> {
        let raw = RawName::from_display(&req.name)?;
        if let Some(id) = self.op_replay(&req.op_id)
            && let Ok(item) = self.stat(&id).await
        {
            return Ok(item);
        }
        let (parent_id, ppath, share) = self.writable_dir(&req.parent_id)?;
        let pkey = self.dir_key(&parent_id);
        let item = self
            .locked(&[&parent_id], async || {
                let rows = self.prepare_dir(&parent_id).await?;
                // A sibling the client would see as the same name (the
                // filesystem may not): use its raw name so it is found.
                let raw = match self.shares[share].needs_aliases().then(|| fold_collision(&rows, &req.name, None)) {
                    Some(Some(r)) => r.raw.clone(),
                    _ => raw.clone(),
                };
                let this = self.clone();
                let path = ppath.clone();
                let kind = req.kind;
                let target = req.link_target.clone();
                let meta = req.meta.clone();
                let may_exist = req.may_exist;
                let outcome = tokio::task::spawn_blocking(move || -> Result<Created, IndexError> {
                    let r = raw.as_os_str();
                    let dir = Dir::open(&path, pkey.as_ref()).map_err(IndexError::from_io)?;
                    if let Ok(e) = dir.stat(r) {
                        let e = Entry { name: dir.stored_name(r).unwrap_or(e.name.clone()), ..e };
                        let same = may_exist
                            && e.kind == kind
                            && match kind {
                                Kind::Dir => true,
                                Kind::File => match (&staged, dir.open_file(e.name.as_os_str(), false)) {
                                    (Some(s), Ok(cur)) => {
                                        Fs::open_read(&s.path()).and_then(|up| same_content(&up, &cur)).unwrap_or(false)
                                    }
                                    _ => false,
                                },
                                _ => false,
                            };
                        return Ok(if same { Created::Existing(e) } else { Created::Taken(e) });
                    }
                    let res = match kind {
                        Kind::Dir => {
                            dir.mkdir(r).and_then(|()| dir.open_meta(r, Kind::Dir)?.apply_meta(&set_meta(&meta)))
                        }
                        Kind::File => {
                            let s =
                                staged.as_ref().ok_or_else(|| IndexError::BadRequest("file without content".into()))?;
                            let sd = Dir::open(&s.dir, None).map_err(IndexError::from_io)?;
                            let fd = sd.open_file(s.os_name(), false).map_err(IndexError::from_io)?;
                            fd.apply_meta(&set_meta(&meta)).map_err(IndexError::from_io)?;
                            fd.sync(Durability::Barrier).map_err(IndexError::from_io)?;
                            sd.rename_noreplace(s.os_name(), &dir, r)
                        }
                        Kind::Symlink => {
                            let t = target
                                .as_deref()
                                .ok_or_else(|| IndexError::BadRequest("symlink without target".into()))?;
                            this.check_link_target(share, &path, t)?;
                            dir.symlink(t, r)
                        }
                        Kind::Other => return Err(IndexError::BadRequest("unsupported item kind".into())),
                    };
                    match res {
                        Ok(()) => Ok(Created::New(dir.stat(r).map_err(IndexError::from_io)?)),
                        Err(e) if is_exists(&e) => {
                            let cur = dir.stat(r).map_err(IndexError::from_io)?;
                            Ok(Created::Taken(Entry { name: dir.stored_name(r).unwrap_or(cur.name.clone()), ..cur }))
                        }
                        Err(e) => Err(io_err(e)),
                    }
                })
                .await??;
                match outcome {
                    Created::New(e) => {
                        let tags = match &req.meta.tags {
                            Some(t) => t.clone(),
                            None => Fs::tags(&ppath.join(e.name.as_os_str())),
                        };
                        let mut out = self.commit_local(
                            &parent_id,
                            &ppath,
                            rows,
                            vec![Upsert { entry: e, tags, content_rev: None, id: None }],
                            &[],
                        )?;
                        out.pop().ok_or(IndexError::NotFound)
                    }
                    Created::Existing(e) => self.known_or_commit(&parent_id, &ppath, rows, e),
                    Created::Taken(e) => {
                        Err(IndexError::Collision(Box::new(self.known_or_commit(&parent_id, &ppath, rows, e)?)))
                    }
                }
            })
            .await?;
        self.record_op(&req.op_id, &item.id)?;
        if item.kind == Kind::Dir {
            // Its contents are this client's to hear about from now on.
            self.note_have(client, &item.id)?;
        }
        Ok(item)
    }

    /// A symlink must resolve (lexically) inside its share.
    fn check_link_target(&self, share: usize, dir: &Path, target: &str) -> Result<(), IndexError> {
        let root = &self.shares[share].path;
        let target = Path::new(target);
        let full = if target.is_absolute() { target.to_owned() } else { dir.join(target) };
        let mut stack: Vec<&std::ffi::OsStr> = Vec::new();
        let mut prefix: Option<PathBuf> = None;
        for c in full.components() {
            match c {
                std::path::Component::Normal(n) => stack.push(n),
                std::path::Component::ParentDir => {
                    stack.pop();
                }
                std::path::Component::Prefix(p) => prefix = Some(PathBuf::from(p.as_os_str())),
                _ => {}
            }
        }
        let mut resolved = prefix.unwrap_or_default();
        resolved.push(std::path::MAIN_SEPARATOR_STR);
        resolved.extend(stack);
        if resolved.starts_with(root) { Ok(()) } else { Err(IndexError::OutsideShare) }
    }

    /// Replaces a file's content, or keeps the upload as a conflict copy
    /// when the file changed since `base_content_rev`. Returns the item and
    /// the conflict copy.
    pub async fn write_content(
        self: &Arc<Self>,
        id: &str,
        base_content_rev: Option<u64>,
        staged: Staged,
        meta: Meta,
        device: &str,
    ) -> Result<(Item, Option<Item>), IndexError> {
        let (path, share) = match self.resolve(id)? {
            Target::Path { path, share } => (path, share),
            _ => return Err(IndexError::NotAFile),
        };
        let this = self.clone();
        let p = path.clone();
        let parent_id = tokio::task::spawn_blocking(move || this.parent_id_of(&p, share)).await??;
        let ppath = path.parent().ok_or(IndexError::NotFound)?.to_owned();
        let name = raw_of(&path)?;
        let key = self.key_of(id)?;
        let device = device.to_owned();
        self.locked(&[&parent_id], async || {
            let rows = self.prepare_dir(&parent_id).await?;
            let old = rows.iter().find(|r| r.item.id == id).map(|r| r.item.clone());
            let pp = ppath.clone();
            let base = old.clone();
            let display = old.as_ref().map_or_else(|| name.display().0, |o| o.name.clone());
            let name = name.clone();
            let outcome = tokio::task::spawn_blocking(move || -> Result<Written, IndexError> {
                let old = base;
                let nm = name.as_os_str();
                let dir = Dir::open(&pp, None).map_err(IndexError::from_io)?;
                let cur = dir.stat(nm).map_err(IndexError::from_io)?;
                if !cur.key.same_file(&key) {
                    return Err(IndexError::NotFound);
                }
                if cur.kind != Kind::File {
                    return Err(IndexError::NotAFile);
                }
                let sd = Dir::open(&staged.dir, None).map_err(IndexError::from_io)?;
                let up = sd.open_file(staged.os_name(), false).map_err(IndexError::from_io)?;
                let changed = |o: &Item| o.size != cur.size || o.mtime_ns != cur.mtime_ns;
                let conflict = match base_content_rev {
                    None => false,
                    Some(b) => old.as_ref().is_none_or(|o| o.content_rev != b || changed(o)),
                };
                if conflict {
                    let target = dir.open_file(nm, false).map_err(IndexError::from_io)?;
                    if same_content(&up, &target).map_err(IndexError::from_io)? {
                        // A retry of a write that already happened, or the same edit on both sides.
                        target.apply_meta(&set_meta(&meta)).map_err(IndexError::from_io)?;
                        return Ok(Written::Same(dir.stat(nm).map_err(IndexError::from_io)?));
                    }
                    up.apply_meta(&set_meta(&meta)).map_err(IndexError::from_io)?;
                    up.sync(Durability::Full).map_err(IndexError::from_io)?;
                    let copy = rename_free(&sd, staged.os_name(), &dir, |n| conflict_name(&display, &device, n))?;
                    return Ok(Written::Conflict {
                        original: cur,
                        copy: dir.stat(copy.as_os_str()).map_err(IndexError::from_io)?,
                    });
                }
                let e = Fs::replace_content(&dir, nm, &up, &sd, &set_meta(&meta)).map_err(IndexError::from_io)?;
                Ok(Written::InPlace(e))
            })
            .await??;
            let tags_of = |e: &Entry| Fs::tags(&ppath.join(e.name.as_os_str()));
            let id = Some(id.to_owned());
            match outcome {
                Written::InPlace(e) => {
                    let tags = tags_of(&e);
                    let up = Upsert { entry: e, tags, content_rev: None, id };
                    let mut out = self.commit_local(&parent_id, &ppath, rows, vec![up], &[])?;
                    Ok((out.pop().ok_or(IndexError::NotFound)?, None))
                }
                Written::Same(e) => {
                    let keep =
                        old.as_ref().filter(|o| o.size == e.size && o.mtime_ns == e.mtime_ns).map(|o| o.content_rev);
                    let tags = tags_of(&e);
                    let up = Upsert { entry: e, tags, content_rev: keep, id };
                    let mut out = self.commit_local(&parent_id, &ppath, rows, vec![up], &[])?;
                    Ok((out.pop().ok_or(IndexError::NotFound)?, None))
                }
                Written::Conflict { original, copy } => {
                    let mut ups = Vec::new();
                    let current = old.clone().filter(|o| o.size == original.size && o.mtime_ns == original.mtime_ns);
                    if current.is_none() {
                        let tags = tags_of(&original);
                        ups.push(Upsert { entry: original, tags, content_rev: None, id });
                    }
                    let tags = tags_of(&copy);
                    ups.push(Upsert { entry: copy, tags, content_rev: None, id: None });
                    let mut out = self.commit_local(&parent_id, &ppath, rows, ups, &[])?;
                    let copy = out.pop().ok_or(IndexError::NotFound)?;
                    let item = match current {
                        Some(c) => c,
                        None => out.pop().ok_or(IndexError::NotFound)?,
                    };
                    tracing::info!(id = ?item.id, copy = %copy.name, "write conflict: upload kept as a copy");
                    Ok((item, Some(copy)))
                }
            }
        })
        .await
    }

    /// Renames and/or moves an item, and applies metadata.
    pub async fn modify(
        self: &Arc<Self>,
        id: &str,
        new_parent_id: Option<&str>,
        new_name: Option<&str>,
        meta: Meta,
    ) -> Result<Item, IndexError> {
        let id = self.canonical(id);
        if id == proto::ROOT_ID || id.starts_with(proto::SHARE_ID_PREFIX) {
            return if new_parent_id.is_none() && new_name.is_none() {
                self.stat(&id).await
            } else {
                Err(IndexError::Denied)
            };
        }
        let (path, share) = match self.resolve(&id)? {
            Target::Path { path, share } => (path, share),
            _ => return Err(IndexError::NotFound),
        };
        let this = self.clone();
        let p = path.clone();
        let from_id = tokio::task::spawn_blocking(move || this.parent_id_of(&p, share)).await??;
        let from_path = path.parent().ok_or(IndexError::NotFound)?.to_owned();
        let cur_raw = raw_of(&path)?;
        let (to_id, to_path, to_share) = match new_parent_id.map(|p| self.canonical(p)) {
            Some(p) if p != from_id => self.writable_dir(&p)?,
            _ => (from_id.clone(), from_path.clone(), share),
        };
        let new_raw = match new_name {
            Some(n) => Some(RawName::from_display(n)?),
            None => None,
        };
        let key = self.key_of(&id)?;
        let same_dir = to_id == from_id;
        self.locked(&[&from_id, &to_id], async || {
            let from_rows = self.prepare_dir(&from_id).await?;
            let to_rows = if same_dir { Vec::new() } else { self.prepare_dir(&to_id).await? };
            let old = from_rows.iter().find(|r| r.item.id == id).cloned();
            // The current display name (maybe an alias) means "keep the raw name".
            let raw = match (&new_raw, &old) {
                (Some(_), Some(o)) if new_name == Some(o.item.name.as_str()) => cur_raw.clone(),
                (Some(r), _) => r.clone(),
                (None, _) => cur_raw.clone(),
            };
            let moving = !same_dir || raw != cur_raw;
            if moving && to_path.starts_with(&path) {
                return Err(IndexError::BadRequest("cannot move a folder into itself".into()));
            }
            if moving
                && self.shares[to_share].needs_aliases()
                && let Some(name) = new_name.or(old.as_ref().map(|o| o.item.name.as_str()))
                && let Some(taken) = fold_collision(if same_dir { &from_rows } else { &to_rows }, name, Some(&id))
            {
                return Err(IndexError::Collision(Box::new(taken.item.clone())));
            }
            let (fp, tp, nm, cn) = (from_path.clone(), to_path.clone(), raw.clone(), cur_raw.clone());
            let res = tokio::task::spawn_blocking(move || -> Result<Result<Entry, Entry>, IndexError> {
                let (nm, cn) = (nm.as_os_str(), cn.as_os_str());
                let from = Dir::open(&fp, None).map_err(IndexError::from_io)?;
                let to = if same_dir { None } else { Some(Dir::open(&tp, None).map_err(IndexError::from_io)?) };
                let to_ref = to.as_ref().unwrap_or(&from);
                let cur = from.stat(cn).map_err(IndexError::from_io)?;
                if !cur.key.same_file(&key) {
                    return Err(IndexError::NotFound);
                }
                if moving && let Err(e) = from.rename_noreplace(cn, to_ref, nm) {
                    if is_exists(&e) {
                        let taken = to_ref.stat(nm).map_err(IndexError::from_io)?;
                        let stored = to_ref.stored_name(nm).unwrap_or(taken.name.clone());
                        return Ok(Err(Entry { name: stored, ..taken }));
                    }
                    return Err(io_err(e));
                }
                if !meta.is_empty() {
                    match to_ref.open_meta(nm, cur.kind) {
                        Ok(fd) => fd.apply_meta(&set_meta(&meta)).map_err(IndexError::from_io)?,
                        Err(e) => tracing::debug!(error = %e, "cannot open item for metadata"),
                    }
                }
                Ok(Ok(to_ref.stat(nm).map_err(IndexError::from_io)?))
            })
            .await??;
            let e = match res {
                Ok(e) => e,
                Err(taken) => {
                    let rows = if same_dir { from_rows } else { to_rows };
                    return Err(IndexError::Collision(Box::new(self.known_or_commit(&to_id, &to_path, rows, taken)?)));
                }
            };
            let tags = Fs::tags(&to_path.join(e.name.as_os_str()));
            let content_rev = old.as_ref().map(|o| o.item.content_rev);
            let up = Upsert { entry: e, tags, content_rev, id: Some(id.clone()) };
            let item = if same_dir {
                self.commit_local(&to_id, &to_path, from_rows, vec![up], &[])?
            } else {
                // Into the new folder first: a client reading the change feed
                // in between must never see the item as deleted.
                let out = self.commit_local(&to_id, &to_path, to_rows, vec![up], &[])?;
                self.commit_local(&from_id, &from_path, from_rows, Vec::new(), std::slice::from_ref(&id))?;
                out
            };
            item.into_iter().next().ok_or(IndexError::NotFound)
        })
        .await
    }

    /// Next seq for a change outside any directory listing; `f` sends its
    /// database op under the state lock so seqs reach the writer in order.
    fn with_seq(&self, f: impl FnOnce(u64) -> Result<(), IndexError>) -> Result<u64, IndexError> {
        let mut st = self.state()?;
        st.seq += 1;
        let seq = st.seq;
        f(seq)?;
        Ok(seq)
    }

    /// Moves an item to the server's Trash. With `listed`, it shows in the
    /// trash container so the client can put it back.
    pub async fn trash(self: &Arc<Self>, id: &str, listed: bool, recursive: bool) -> Result<Item, IndexError> {
        let id = self.canonical(id);
        if id == proto::ROOT_ID || id.starts_with(proto::SHARE_ID_PREFIX) || id == proto::TRASH_ID {
            return Err(IndexError::Denied);
        }
        self.trash_impl()?;
        let (path, share) = match self.resolve_any(&id)? {
            Target::Path { path, share } => (path, share),
            Target::Trashed { .. } => {
                let item = self.trashed_item(&id)?.ok_or(IndexError::NotFound)?;
                if !listed {
                    // Deleted from the client's trash: it stays in the server's.
                    self.forget(&id).await?;
                }
                return Ok(item);
            }
            Target::Root => return Err(IndexError::Denied),
        };
        let this = self.clone();
        let p = path.clone();
        let parent_id = tokio::task::spawn_blocking(move || this.parent_id_of(&p, share)).await??;
        let ppath = path.parent().ok_or(IndexError::NotFound)?.to_owned();
        let name = raw_of(&path)?;
        let key = self.key_of(&id)?;
        self.locked(&[&parent_id], async || {
            let rows = self.prepare_dir(&parent_id).await?;
            let old = rows.iter().find(|r| r.item.id == id).map(|r| r.item.clone());
            let shown = old.as_ref().map_or_else(|| name.display().0, |o| o.name.clone());
            let this = self.clone();
            let (pp, nm, full) = (ppath.clone(), name.clone(), path.clone());
            let (to, e) = tokio::task::spawn_blocking(move || -> Result<(PathBuf, Entry), IndexError> {
                let trash = this.trash_impl()?;
                let nm = nm.as_os_str();
                let dir = Dir::open(&pp, None).map_err(IndexError::from_io)?;
                let cur = dir.stat(nm).map_err(IndexError::from_io)?;
                if !cur.key.same_file(&key) {
                    return Err(IndexError::NotFound);
                }
                if cur.kind == Kind::Dir
                    && !recursive
                    && Dir::open(&full, None)
                        .and_then(|d| d.scan())
                        .map_err(IndexError::from_io)?
                        .iter()
                        .any(|e| !Fs::skip(e.name.as_os_str()))
                {
                    return Err(IndexError::NotEmpty);
                }
                let to = trash.trash(&dir, nm).map_err(io_err)?;
                let e = Fs::lstat(&to).map_err(IndexError::from_io)?;
                Ok((to, e))
            })
            .await??;
            tracing::info!(id, name = %shown, to = %to.display(), listed, "moved to Trash");
            let tags = Fs::tags(&to);
            let mut item = row_of(id.clone(), proto::TRASH_ID, e, tags).item;
            item.name = shown.clone();
            item.content_rev = old.as_ref().map_or(0, |o| o.content_rev);
            if listed {
                // The trash row first: the removal below is then a move, not a deletion.
                self.with_seq(|seq| {
                    item.rev = seq;
                    if item.content_rev == 0 {
                        item.content_rev = seq;
                    }
                    let t = Trashed {
                        orig_parent: parent_id.clone(),
                        orig_name: shown.clone(),
                        item: item.clone(),
                        path: Some(to.clone()),
                    };
                    self.trashed.lock().map_err(|_| IndexError::Poisoned)?.insert(id.clone(), t.clone());
                    Ok(self.db.send(Op::TrashSet { id: id.clone(), trashed: t, seq })?)
                })?;
            }
            self.commit_local(&parent_id, &ppath, rows, Vec::new(), std::slice::from_ref(&id))?;
            Ok(item)
        })
        .await
    }

    /// Puts an item from the trash container back as `parent_id/name`.
    pub async fn untrash(self: &Arc<Self>, id: &str, parent_id: &str, name: &str) -> Result<Item, IndexError> {
        let raw = RawName::from_display(name)?;
        let rec = self.trashed.lock().map_err(|_| IndexError::Poisoned)?.get(id).cloned();
        let rec = rec.ok_or(IndexError::NotFound)?;
        let Target::Trashed { path } = self.resolve_any(id)? else {
            // Already back in a share (put back on the server): just move it.
            self.forget(id).await?;
            return self.modify(id, Some(parent_id), Some(name), Meta::default()).await;
        };
        let (parent_id, ppath, share) = self.writable_dir(parent_id)?;
        let key = self.key_of(id)?;
        let item = self
            .locked(&[&parent_id], async || {
                let rows = self.prepare_dir(&parent_id).await?;
                if self.shares[share].needs_aliases()
                    && let Some(taken) = fold_collision(&rows, name, Some(id))
                {
                    return Err(IndexError::Collision(Box::new(taken.item.clone())));
                }
                let this = self.clone();
                let (src, pp, nm) = (path.clone(), ppath.clone(), raw.clone());
                let res = tokio::task::spawn_blocking(move || -> Result<Result<Entry, Entry>, IndexError> {
                    let trash = this.trash_impl()?;
                    let nm = nm.as_os_str();
                    if !Fs::lstat(&src).map_err(IndexError::from_io)?.key.same_file(&key) {
                        return Err(IndexError::NotFound);
                    }
                    let to = Dir::open(&pp, None).map_err(IndexError::from_io)?;
                    match trash.put_back(&src, &to, nm) {
                        Ok(()) => Ok(Ok(to.stat(nm).map_err(IndexError::from_io)?)),
                        Err(e) if is_exists(&e) => {
                            let taken = to.stat(nm).map_err(IndexError::from_io)?;
                            Ok(Err(Entry { name: to.stored_name(nm).unwrap_or(taken.name.clone()), ..taken }))
                        }
                        Err(e) => Err(io_err(e)),
                    }
                })
                .await??;
                let e = match res {
                    Ok(e) => e,
                    Err(taken) => {
                        return Err(IndexError::Collision(Box::new(
                            self.known_or_commit(&parent_id, &ppath, rows, taken)?,
                        )));
                    }
                };
                let tags = Fs::tags(&ppath.join(e.name.as_os_str()));
                let up = Upsert { entry: e, tags, content_rev: Some(rec.item.content_rev), id: Some(id.to_owned()) };
                let mut out = self.commit_local(&parent_id, &ppath, rows, vec![up], &[])?;
                out.pop().ok_or(IndexError::NotFound)
            })
            .await?;
        self.trashed.lock().map_err(|_| IndexError::Poisoned)?.remove(id);
        self.with_seq(|seq| Ok(self.db.send(Op::TrashForget { id: id.to_owned(), tombstone: false, seq })?))?;
        tracing::info!(id, name, "put back from Trash");
        Ok(item)
    }

    /// Drops an item from the trash container (it stays in the server's Trash).
    pub async fn forget(self: &Arc<Self>, id: &str) -> Result<(), IndexError> {
        if self.trashed.lock().map_err(|_| IndexError::Poisoned)?.remove(id).is_some() {
            self.with_seq(|seq| Ok(self.db.send(Op::TrashForget { id: id.to_owned(), tombstone: true, seq })?))?;
        }
        Ok(())
    }

    /// A trash container item with fresh attributes, if it is still in the Trash.
    pub(super) fn trashed_item(&self, id: &str) -> Result<Option<Item>, IndexError> {
        let rec = self.trashed.lock().map_err(|_| IndexError::Poisoned)?.get(id).cloned();
        let Some(rec) = rec else { return Ok(None) };
        let Some(trash) = self.writes.trash.as_ref() else { return Ok(None) };
        let Ok(key) = self.key_of(id) else { return Ok(None) };
        let path = match self.platform_path(&key) {
            Ok(Some(p)) => p,
            Ok(None) => match rec.path.clone() {
                Some(p) => p,
                None => return Ok(None),
            },
            Err(_) => return Ok(None),
        };
        if !trash.contains(&path) {
            return Ok(None);
        }
        let Ok(e) = Fs::lstat(&path) else { return Ok(None) };
        if !e.key.same_file(&key) {
            return Ok(None);
        }
        let mut item = row_of(id.to_owned(), proto::TRASH_ID, e, rec.item.tags.clone()).item;
        item.name = rec.item.name.clone();
        item.rev = rec.item.rev;
        item.content_rev = rec.item.content_rev;
        Ok(Some(item))
    }

    /// The trash container's listing. Items no longer in the server's Trash
    /// (emptied, or put back there) leave it.
    pub(super) fn trash_listing(&self) -> Result<Listing, IndexError> {
        let ids: Vec<ItemId> = self.trashed.lock().map_err(|_| IndexError::Poisoned)?.keys().cloned().collect();
        let mut rows = Vec::new();
        for id in ids {
            match self.trashed_item(&id)? {
                Some(item) => rows.push(Row { item, key: FileKey::default(), raw: RawName::default() }),
                None => {
                    if self.trashed.lock().map_err(|_| IndexError::Poisoned)?.remove(&id).is_some() {
                        self.with_seq(|seq| {
                            Ok(self.db.send(Op::TrashForget { id: id.clone(), tombstone: true, seq })?)
                        })?;
                    }
                }
            }
        }
        rows.sort_unstable_by(|a, b| a.item.name.cmp(&b.item.name));
        // Not cached: a new revision every time, so clients always take it.
        let rev = self.state()?.seq;
        Ok(encode_listing(proto::TRASH_ID, rev, rows))
    }
}
