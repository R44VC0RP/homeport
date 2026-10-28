//! Client writes: creates, content updates, renames and moves, Trash.
//!
//! Every operation resolves items by file ID, works through descriptors of
//! the directories involved (`*at` calls that never follow a symlink in the
//! last component), holds those directories' locks, and records its own
//! result in the index (with a new seq) before answering. The FSEvents rescan
//! that follows then finds nothing new, so a client's write never comes back
//! to it as a remote change.

use std::time::{SystemTime, UNIX_EPOCH};

use homeport_proto::Meta;

use super::*;
use crate::fsops::{Dir, SetMeta, Sync};

/// Where uploads land and where deletes go.
#[derive(Clone, Debug)]
pub struct WriteDirs {
    /// Uploads are received here; on the shares' volume so they can be renamed into place.
    pub staging: PathBuf,
    /// basebook's Trash (`~/.Trash`).
    pub trash: PathBuf,
}

impl WriteDirs {
    /// Creates the staging directory and clears uploads left by a previous run.
    pub fn prepare(staging: PathBuf, trash: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&staging)?;
        std::fs::create_dir_all(&trash)?;
        for e in std::fs::read_dir(&staging)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("up-") {
                let _ = std::fs::remove_file(e.path());
            } else if name.starts_with("backup-") {
                // A content write did not finish: its target may be partial.
                tracing::warn!(path = %e.path().display(), "backup of an interrupted write left in staging");
            }
        }
        Ok(Self { staging, trash: std::fs::canonicalize(&trash)? })
    }
}

/// A received upload in the staging directory; removed on drop if still there.
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
    match e.raw_os_error() {
        Some(libc::EINVAL) => IndexError::BadName,
        _ => IndexError::from_io(e),
    }
}

fn is_exists(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EEXIST)
}

/// `name` with ` (suffix)` before its extension: `a.txt` -> `a (suffix).txt`.
fn with_suffix(name: &str, suffix: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({suffix}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({suffix})"),
    }
}

/// Local time as `YYYY-MM-DD HHMM`.
fn local_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: zeroed `tm` is a valid out-parameter.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:04}-{:02}-{:02} {:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

fn conflict_name(name: &str, device: &str, n: u32) -> String {
    let device = if device.is_empty() { "client" } else { device };
    let suffix = format!("{device} conflict {}", local_stamp());
    if n <= 1 { with_suffix(name, &suffix) } else { with_suffix(name, &format!("{suffix} {n}")) }
}

/// Finder-style names for the Trash: `a.txt`, `a 2.txt`, `a 3.txt`, ...
fn trash_name(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_owned();
    }
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} {n}{}", &name[..i], &name[i..]),
        _ => format!("{name} {n}"),
    }
}

/// Renames `from` in `src` to the first free name `name_for(n)` in `dst`.
fn rename_free(src: &Dir, from: &str, dst: &Dir, name_for: impl Fn(u32) -> String) -> Result<String, IndexError> {
    for n in 1..1000 {
        let to = name_for(n);
        match src.rename_excl(from, dst, &to) {
            Ok(()) => return Ok(to),
            Err(e) if is_exists(&e) => continue,
            Err(e) => return Err(io_err(e)),
        }
    }
    Err(IndexError::Io(io::Error::other("no free name")))
}

fn file_name(path: &Path) -> Result<String, IndexError> {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or(IndexError::NotFound)
}

impl Index {
    pub fn staging_dir(&self) -> &Path {
        &self.writes.staging
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

    /// File ID of a directory item (share roots have `root:` IDs).
    fn dir_ino(&self, id: &str) -> Option<u64> {
        match id.strip_prefix(proto::SHARE_ID_PREFIX) {
            Some(name) => self.shares.iter().find(|s| s.name == name).map(|s| s.ino),
            None => id.parse().ok(),
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

    /// Current items of a directory, indexing it first if needed. Call with its lock held.
    async fn prepare_dir(self: &Arc<Self>, dir_id: &str) -> Result<Vec<Item>, IndexError> {
        let (cached, indexed) = {
            let mut st = self.state()?;
            (st.lru.get(dir_id).cloned(), st.indexed.contains_key(dir_id))
        };
        if let Some(l) = cached {
            return Ok(l.items.clone());
        }
        if indexed {
            self.db.flush().await?;
            let (listing, _) = self.refresh_locked(dir_id, false).await?;
            return Ok(listing.items.clone());
        }
        let (listing, _) = self.refresh_locked(dir_id, false).await?;
        Ok(listing.items.clone())
    }

    /// Records changes the server made itself in `dir_id` (whose lock the
    /// caller holds), stamped with one new seq. Returns the upserted items.
    fn commit_local(
        &self,
        dir_id: &str,
        dir_path: &Path,
        mut items: Vec<Item>,
        ups: Vec<Upsert>,
        removed: &[ItemId],
    ) -> Result<Vec<Item>, IndexError> {
        let mut st = self.state()?;
        st.seq += 1;
        let seq = st.seq;
        items.retain(|i| !removed.contains(&i.id));
        let mut out = Vec::with_capacity(ups.len());
        for u in ups {
            if u.entry.name == ".DS_Store" {
                continue;
            }
            let id = self.id_for(u.entry.ino);
            let mut item = item_of(id, dir_id, u.entry, u.tags);
            item.rev = seq;
            item.content_rev = u.content_rev.unwrap_or(seq);
            match items.iter().position(|i| i.id == item.id) {
                Some(p) => items[p] = item.clone(),
                None => items.push(item.clone()),
            }
            out.push(item);
        }
        items.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let listing = Arc::new(encode_listing(dir_id, seq, items)?);
        st.lru.put(dir_id.to_owned(), listing);
        st.indexed.insert(dir_id.to_owned(), (seq, dir_path.to_owned()));
        // Sent under the state lock so the writer sees seqs in order.
        self.db.send(Op::Listing {
            dir_id: dir_id.to_owned(),
            dir_rev: seq,
            path: dir_path.to_owned(),
            upserts: out.clone(),
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

    fn tags_at(path: &Path) -> Option<Vec<u8>> {
        fsops::get_xattr(path, fsops::TAGS_XATTR).ok().flatten()
    }

    /// The listing's item for an entry, recording it if the index is behind.
    fn known_or_commit(&self, dir_id: &str, dir_path: &Path, items: Vec<Item>, e: Entry) -> Result<Item, IndexError> {
        let id = self.id_for(e.ino);
        if let Some(i) = items.iter().find(|i| i.id == id && i.size == e.size && i.mtime_ns == e.mtime_ns) {
            return Ok(i.clone());
        }
        let tags = Self::tags_at(&dir_path.join(&e.name));
        let mut out = self.commit_local(dir_id, dir_path, items, vec![Upsert { entry: e, tags, content_rev: None }], &[])?;
        out.pop().ok_or(IndexError::NotFound)
    }

    /// Creates a file (from `staged`), folder or symlink.
    pub async fn create(
        self: &Arc<Self>,
        req: CreateReq,
        staged: Option<Staged>,
        client: Option<&str>,
    ) -> Result<Item, IndexError> {
        if !proto::valid_name(&req.name) {
            return Err(IndexError::BadName);
        }
        if let Some(id) = self.op_replay(&req.op_id)
            && let Ok(item) = self.stat(&id).await
        {
            return Ok(item);
        }
        let (parent_id, ppath, share) = self.writable_dir(&req.parent_id)?;
        let pino = self.dir_ino(&parent_id);
        let item = self
            .locked(&[&parent_id], async || {
                let items = self.prepare_dir(&parent_id).await?;
                let this = self.clone();
                let path = ppath.clone();
                let r = req.name.clone();
                let kind = req.kind;
                let target = req.link_target.clone();
                let meta = req.meta.clone();
                let may_exist = req.may_exist;
                let outcome = tokio::task::spawn_blocking(move || -> Result<Created, IndexError> {
                    let dir = Dir::open(&path, pino).map_err(IndexError::from_io)?;
                    if let Ok(e) = dir.stat(&r) {
                        let e = Entry { name: dir.stored_name(&r).unwrap_or(e.name.clone()), ..e };
                        let same = may_exist
                            && e.kind == kind
                            && match kind {
                                Kind::Dir => true,
                                Kind::File => match (&staged, dir.open_at(&e.name, libc::O_RDONLY, 0)) {
                                    (Some(s), Ok(cur)) => fsops::open_nofollow(&s.path(), libc::O_RDONLY)
                                        .and_then(|up| fsops::same_content(&up, &cur))
                                        .unwrap_or(false),
                                    _ => false,
                                },
                                _ => false,
                            };
                        return Ok(if same { Created::Existing(e) } else { Created::Taken(e) });
                    }
                    let res = match kind {
                        Kind::Dir => dir.mkdir(&r, 0o755).and_then(|()| {
                            let fd = dir.open_at(&r, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                            fsops::apply_meta(&fd, &set_meta(&meta))
                        }),
                        Kind::File => {
                            let s = staged.as_ref().ok_or_else(|| IndexError::BadRequest("file without content".into()))?;
                            let sd = Dir::open(&s.dir, None).map_err(IndexError::from_io)?;
                            let fd = sd.open_at(&s.name, libc::O_RDONLY, 0).map_err(IndexError::from_io)?;
                            fsops::apply_meta(&fd, &set_meta(&meta)).map_err(IndexError::from_io)?;
                            fsops::sync(&fd, Sync::Barrier).map_err(IndexError::from_io)?;
                            sd.rename_excl(&s.name, &dir, &r)
                        }
                        Kind::Symlink => {
                            let t = target.as_deref().ok_or_else(|| IndexError::BadRequest("symlink without target".into()))?;
                            this.check_link_target(share, &path, t)?;
                            dir.symlink(t, &r)
                        }
                        Kind::Other => return Err(IndexError::BadRequest("unsupported item kind".into())),
                    };
                    match res {
                        Ok(()) => Ok(Created::New(dir.stat(&r).map_err(IndexError::from_io)?)),
                        Err(e) if is_exists(&e) => {
                            let cur = dir.stat(&r).map_err(IndexError::from_io)?;
                            Ok(Created::Taken(Entry { name: dir.stored_name(&r).unwrap_or(cur.name.clone()), ..cur }))
                        }
                        Err(e) => Err(io_err(e)),
                    }
                })
                .await??;
                match outcome {
                    Created::New(e) => {
                        let tags = match &req.meta.tags {
                            Some(t) => t.clone(),
                            None => Self::tags_at(&ppath.join(&e.name)),
                        };
                        let mut out = self.commit_local(
                            &parent_id,
                            &ppath,
                            items,
                            vec![Upsert { entry: e, tags, content_rev: None }],
                            &[],
                        )?;
                        out.pop().ok_or(IndexError::NotFound)
                    }
                    Created::Existing(e) => self.known_or_commit(&parent_id, &ppath, items, e),
                    Created::Taken(e) => {
                        Err(IndexError::Collision(Box::new(self.known_or_commit(&parent_id, &ppath, items, e)?)))
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
        for c in full.components() {
            match c {
                std::path::Component::Normal(n) => stack.push(n),
                std::path::Component::ParentDir => {
                    stack.pop();
                }
                _ => {}
            }
        }
        let resolved: PathBuf = std::iter::once(std::ffi::OsStr::new("/")).chain(stack).collect();
        if resolved.starts_with(root) { Ok(()) } else { Err(IndexError::OutsideShare) }
    }

    /// Replaces a file's content in place, or keeps the upload as a
    /// conflict copy when the file changed since `base_content_rev`.
    /// Returns the item and the conflict copy.
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
        let name = file_name(&path)?;
        let ino: u64 = id.parse().map_err(|_| IndexError::NotAFile)?;
        let device = device.to_owned();
        self.locked(&[&parent_id], async || {
            let items = self.prepare_dir(&parent_id).await?;
            let old = items.iter().find(|i| i.id == id).cloned();
            let pp = ppath.clone();
            let base = old.clone();
            let outcome = tokio::task::spawn_blocking(move || -> Result<Written, IndexError> {
                let old = base;
                let dir = Dir::open(&pp, None).map_err(IndexError::from_io)?;
                let cur = dir.stat(&name).map_err(IndexError::from_io)?;
                if cur.ino != ino {
                    return Err(IndexError::NotFound);
                }
                if cur.kind != Kind::File {
                    return Err(IndexError::NotAFile);
                }
                let sd = Dir::open(&staged.dir, None).map_err(IndexError::from_io)?;
                let up = sd.open_at(&staged.name, libc::O_RDONLY, 0).map_err(IndexError::from_io)?;
                let changed = |o: &Item| o.size != cur.size || o.mtime_ns != cur.mtime_ns;
                let conflict = match base_content_rev {
                    None => false,
                    Some(b) => old.as_ref().is_none_or(|o| o.content_rev != b || changed(o)),
                };
                if conflict {
                    let target = dir.open_at(&name, libc::O_RDONLY, 0).map_err(IndexError::from_io)?;
                    if fsops::same_content(&up, &target).map_err(IndexError::from_io)? {
                        // A retry of a write that already happened, or the same edit on both sides.
                        fsops::apply_meta(&target, &set_meta(&meta)).map_err(IndexError::from_io)?;
                        return Ok(Written::Same(dir.stat(&name).map_err(IndexError::from_io)?));
                    }
                    fsops::apply_meta(&up, &set_meta(&meta)).map_err(IndexError::from_io)?;
                    fsops::sync(&up, Sync::Full).map_err(IndexError::from_io)?;
                    let copy = rename_free(&sd, &staged.name, &dir, |n| conflict_name(&name, &device, n))?;
                    return Ok(Written::Conflict {
                        original: cur,
                        copy: dir.stat(&copy).map_err(IndexError::from_io)?,
                    });
                }
                // In place, so the file keeps its ID; a clone guards against a crash mid-copy.
                let target = dir.open_at(&name, libc::O_RDWR, 0).map_err(IndexError::from_io)?;
                let backup = format!("backup-{ino}-{:x}", rand::random::<u32>());
                let backed_up = sd.clone_from(&target, &backup).is_ok();
                fsops::copy_over(&up, &target).map_err(IndexError::from_io)?;
                fsops::apply_meta(&target, &set_meta(&meta)).map_err(IndexError::from_io)?;
                fsops::sync(&target, Sync::Full).map_err(IndexError::from_io)?;
                if backed_up {
                    let _ = sd.unlink(&backup);
                }
                Ok(Written::InPlace(dir.stat(&name).map_err(IndexError::from_io)?))
            })
            .await??;
            let tags_of = |e: &Entry| Self::tags_at(&ppath.join(&e.name));
            match outcome {
                Written::InPlace(e) => {
                    let tags = tags_of(&e);
                    let mut out = self.commit_local(
                        &parent_id,
                        &ppath,
                        items,
                        vec![Upsert { entry: e, tags, content_rev: None }],
                        &[],
                    )?;
                    Ok((out.pop().ok_or(IndexError::NotFound)?, None))
                }
                Written::Same(e) => {
                    let keep = old.as_ref().filter(|o| o.size == e.size && o.mtime_ns == e.mtime_ns).map(|o| o.content_rev);
                    let tags = tags_of(&e);
                    let mut out = self.commit_local(
                        &parent_id,
                        &ppath,
                        items,
                        vec![Upsert { entry: e, tags, content_rev: keep }],
                        &[],
                    )?;
                    Ok((out.pop().ok_or(IndexError::NotFound)?, None))
                }
                Written::Conflict { original, copy } => {
                    let mut ups = Vec::new();
                    let current = old.clone().filter(|o| o.size == original.size && o.mtime_ns == original.mtime_ns);
                    if current.is_none() {
                        let tags = tags_of(&original);
                        ups.push(Upsert { entry: original, tags, content_rev: None });
                    }
                    let tags = tags_of(&copy);
                    ups.push(Upsert { entry: copy, tags, content_rev: None });
                    let mut out = self.commit_local(&parent_id, &ppath, items, ups, &[])?;
                    let copy = out.pop().ok_or(IndexError::NotFound)?;
                    let item = match current {
                        Some(c) => c,
                        None => out.pop().ok_or(IndexError::NotFound)?,
                    };
                    tracing::info!(id, copy = %copy.name, "write conflict: upload kept as a copy");
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
            return if new_parent_id.is_none() && new_name.is_none() { self.stat(&id).await } else { Err(IndexError::Denied) };
        }
        let (path, share) = match self.resolve(&id)? {
            Target::Path { path, share } => (path, share),
            _ => return Err(IndexError::NotFound),
        };
        let this = self.clone();
        let p = path.clone();
        let from_id = tokio::task::spawn_blocking(move || this.parent_id_of(&p, share)).await??;
        let from_path = path.parent().ok_or(IndexError::NotFound)?.to_owned();
        let cur_name = file_name(&path)?;
        let (to_id, to_path) = match new_parent_id.map(|p| self.canonical(p)) {
            Some(p) if p != from_id => {
                let (p, path, _) = self.writable_dir(&p)?;
                (p, path)
            }
            _ => (from_id.clone(), from_path.clone()),
        };
        let name = new_name.unwrap_or(&cur_name).to_owned();
        if !proto::valid_name(&name) {
            return Err(IndexError::BadName);
        }
        let ino: u64 = id.parse().map_err(|_| IndexError::BadId(id.clone()))?;
        let moving = to_id != from_id || name != cur_name;
        let same_dir = to_id == from_id;
        if moving && to_path.starts_with(&path) {
            return Err(IndexError::BadRequest("cannot move a folder into itself".into()));
        }
        self.locked(&[&from_id, &to_id], async || {
            let from_items = self.prepare_dir(&from_id).await?;
            let to_items = if same_dir { Vec::new() } else { self.prepare_dir(&to_id).await? };
            let old = from_items.iter().find(|i| i.id == id).cloned();
            let (fp, tp, nm, cn) = (from_path.clone(), to_path.clone(), name.clone(), cur_name.clone());
            let res = tokio::task::spawn_blocking(move || -> Result<Result<Entry, Entry>, IndexError> {
                let from = Dir::open(&fp, None).map_err(IndexError::from_io)?;
                let to = if same_dir { None } else { Some(Dir::open(&tp, None).map_err(IndexError::from_io)?) };
                let to_ref = to.as_ref().unwrap_or(&from);
                let cur = from.stat(&cn).map_err(IndexError::from_io)?;
                if cur.ino != ino {
                    return Err(IndexError::NotFound);
                }
                if moving && let Err(e) = from.rename_excl(&cn, to_ref, &nm) {
                    if is_exists(&e) {
                        let taken = to_ref.stat(&nm).map_err(IndexError::from_io)?;
                        let stored = to_ref.stored_name(&nm).unwrap_or(taken.name.clone());
                        return Ok(Err(Entry { name: stored, ..taken }));
                    }
                    return Err(io_err(e));
                }
                if !meta.is_empty() {
                    let flags = match cur.kind {
                        Kind::Dir => libc::O_RDONLY | libc::O_DIRECTORY,
                        Kind::Symlink => libc::O_RDONLY | libc::O_SYMLINK,
                        _ => libc::O_RDONLY,
                    };
                    let fd = to_ref.open_at(&nm, flags & !libc::O_NOFOLLOW, 0);
                    match fd {
                        Ok(fd) => fsops::apply_meta(&fd, &set_meta(&meta)).map_err(IndexError::from_io)?,
                        Err(e) => tracing::debug!(error = %e, "cannot open item for metadata"),
                    }
                }
                Ok(Ok(to_ref.stat(&nm).map_err(IndexError::from_io)?))
            })
            .await??;
            let e = match res {
                Ok(e) => e,
                Err(taken) => {
                    let items = if same_dir { from_items } else { to_items };
                    return Err(IndexError::Collision(Box::new(self.known_or_commit(&to_id, &to_path, items, taken)?)));
                }
            };
            let tags = Self::tags_at(&to_path.join(&e.name));
            let content_rev = old.as_ref().map(|o| o.content_rev);
            let up = Upsert { entry: e, tags, content_rev };
            let item = if same_dir {
                self.commit_local(&to_id, &to_path, from_items, vec![up], &[])?
            } else {
                // Into the new folder first: a client reading the change feed
                // in between must never see the item as deleted.
                let out = self.commit_local(&to_id, &to_path, to_items, vec![up], &[])?;
                self.commit_local(&from_id, &from_path, from_items, Vec::new(), std::slice::from_ref(&id))?;
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

    /// Moves an item to basebook's Trash. With `listed`, it shows in the
    /// trash container so the client can put it back.
    pub async fn trash(self: &Arc<Self>, id: &str, listed: bool, recursive: bool) -> Result<Item, IndexError> {
        let id = self.canonical(id);
        if id == proto::ROOT_ID || id.starts_with(proto::SHARE_ID_PREFIX) || id == proto::TRASH_ID {
            return Err(IndexError::Denied);
        }
        let (path, share) = match self.resolve_any(&id)? {
            Target::Path { path, share } => (path, share),
            Target::Trashed { .. } => return self.trashed_item(&id)?.ok_or(IndexError::NotFound),
            Target::Root => return Err(IndexError::Denied),
        };
        let this = self.clone();
        let p = path.clone();
        let parent_id = tokio::task::spawn_blocking(move || this.parent_id_of(&p, share)).await??;
        let ppath = path.parent().ok_or(IndexError::NotFound)?.to_owned();
        let name = file_name(&path)?;
        let ino: u64 = id.parse().map_err(|_| IndexError::BadId(id.clone()))?;
        let trash = self.writes.trash.clone();
        self.locked(&[&parent_id], async || {
            let items = self.prepare_dir(&parent_id).await?;
            let old = items.iter().find(|i| i.id == id).cloned();
            let (pp, nm, tr, full) = (ppath.clone(), name.clone(), trash.clone(), path.clone());
            let e = tokio::task::spawn_blocking(move || -> Result<Entry, IndexError> {
                let dir = Dir::open(&pp, None).map_err(IndexError::from_io)?;
                let cur = dir.stat(&nm).map_err(IndexError::from_io)?;
                if cur.ino != ino {
                    return Err(IndexError::NotFound);
                }
                if cur.kind == Kind::Dir
                    && !recursive
                    && fsops::scan_dir(&full).map_err(IndexError::from_io)?.iter().any(|e| e.name != ".DS_Store")
                {
                    return Err(IndexError::NotEmpty);
                }
                for n in 1..1000 {
                    let tname = trash_name(&nm, n);
                    let to = tr.join(&tname);
                    match dir.rename_to_path_excl(&nm, &to) {
                        Ok(()) => return fsops::lstat(&to).map_err(IndexError::from_io),
                        Err(e) if is_exists(&e) => continue,
                        Err(e) => return Err(io_err(e)),
                    }
                }
                Err(IndexError::Io(io::Error::other("no free name in the Trash")))
            })
            .await??;
            tracing::info!(id, name = %name, as_name = %e.name, listed, "moved to Trash");
            let tags = Self::tags_at(&trash.join(&e.name));
            let mut item = item_of(id.clone(), proto::TRASH_ID, e, tags);
            item.name = name.clone();
            item.content_rev = old.as_ref().map_or(0, |o| o.content_rev);
            if listed {
                // The trash row first: the removal below is then a move, not a deletion.
                self.with_seq(|seq| {
                    item.rev = seq;
                    if item.content_rev == 0 {
                        item.content_rev = seq;
                    }
                    let t = Trashed { orig_parent: parent_id.clone(), orig_name: name.clone(), item: item.clone() };
                    self.trashed.lock().map_err(|_| IndexError::Poisoned)?.insert(id.clone(), t.clone());
                    Ok(self.db.send(Op::TrashSet { id: id.clone(), trashed: t, seq })?)
                })?;
            }
            self.commit_local(&parent_id, &ppath, items, Vec::new(), std::slice::from_ref(&id))?;
            Ok(item)
        })
        .await
    }

    /// Puts an item from the trash container back as `parent_id/name`.
    pub async fn untrash(self: &Arc<Self>, id: &str, parent_id: &str, name: &str) -> Result<Item, IndexError> {
        if !proto::valid_name(name) {
            return Err(IndexError::BadName);
        }
        let rec = self.trashed.lock().map_err(|_| IndexError::Poisoned)?.get(id).cloned();
        let rec = rec.ok_or(IndexError::NotFound)?;
        let Target::Trashed { path } = self.resolve_any(id)? else {
            // Already back in a share (put back on basebook): just move it.
            self.forget(id).await?;
            return self.modify(id, Some(parent_id), Some(name), Meta::default()).await;
        };
        let (parent_id, ppath, _) = self.writable_dir(parent_id)?;
        let ino: u64 = id.parse().map_err(|_| IndexError::BadId(id.to_owned()))?;
        let item = self
            .locked(&[&parent_id], async || {
                let items = self.prepare_dir(&parent_id).await?;
                let (src, pp, nm) = (path.clone(), ppath.clone(), name.to_owned());
                let res = tokio::task::spawn_blocking(move || -> Result<Result<Entry, Entry>, IndexError> {
                    if fsops::lstat(&src).map_err(IndexError::from_io)?.ino != ino {
                        return Err(IndexError::NotFound);
                    }
                    let to = Dir::open(&pp, None).map_err(IndexError::from_io)?;
                    match to.rename_from_path_excl(&src, &nm) {
                        Ok(()) => Ok(Ok(to.stat(&nm).map_err(IndexError::from_io)?)),
                        Err(e) if is_exists(&e) => {
                            let taken = to.stat(&nm).map_err(IndexError::from_io)?;
                            Ok(Err(Entry { name: to.stored_name(&nm).unwrap_or(taken.name.clone()), ..taken }))
                        }
                        Err(e) => Err(io_err(e)),
                    }
                })
                .await??;
                let e = match res {
                    Ok(e) => e,
                    Err(taken) => {
                        return Err(IndexError::Collision(Box::new(self.known_or_commit(&parent_id, &ppath, items, taken)?)));
                    }
                };
                let tags = Self::tags_at(&ppath.join(&e.name));
                let up = Upsert { entry: e, tags, content_rev: Some(rec.item.content_rev) };
                let mut out = self.commit_local(&parent_id, &ppath, items, vec![up], &[])?;
                out.pop().ok_or(IndexError::NotFound)
            })
            .await?;
        self.trashed.lock().map_err(|_| IndexError::Poisoned)?.remove(id);
        self.with_seq(|seq| Ok(self.db.send(Op::TrashForget { id: id.to_owned(), tombstone: false, seq })?))?;
        tracing::info!(id, name, "put back from Trash");
        Ok(item)
    }

    /// Drops an item from the trash container (it stays in basebook's Trash).
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
        let ino: u64 = id.parse().map_err(|_| IndexError::BadId(id.to_owned()))?;
        let Ok(path) = self.volume.path_of(ino) else { return Ok(None) };
        if !path.starts_with(&self.writes.trash) {
            return Ok(None);
        }
        let Ok(e) = fsops::lstat(&path) else { return Ok(None) };
        let mut item = item_of(id.to_owned(), proto::TRASH_ID, e, rec.item.tags.clone());
        item.name = rec.item.name.clone();
        item.rev = rec.item.rev;
        item.content_rev = rec.item.content_rev;
        Ok(Some(item))
    }

    /// The trash container's listing. Items no longer in basebook's Trash
    /// (emptied, or put back there) leave it.
    pub(super) fn trash_listing(&self) -> Result<Listing, IndexError> {
        let ids: Vec<ItemId> = self.trashed.lock().map_err(|_| IndexError::Poisoned)?.keys().cloned().collect();
        let mut items = Vec::new();
        for id in ids {
            match self.trashed_item(&id)? {
                Some(i) => items.push(i),
                None => {
                    if self.trashed.lock().map_err(|_| IndexError::Poisoned)?.remove(&id).is_some() {
                        self.with_seq(|seq| Ok(self.db.send(Op::TrashForget { id: id.clone(), tombstone: true, seq })?))?;
                    }
                }
            }
        }
        items.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        // Not cached: a new revision every time, so clients always take it.
        let rev = self.state()?.seq;
        encode_listing(proto::TRASH_ID, rev, items)
    }
}
