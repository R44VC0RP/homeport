//! UniFFI bindings over `homeport-client` for `Homeport.app` and its File
//! Provider extension.
//!
//! [`Core`] owns a small tokio runtime, one pinned QUIC client and the local
//! metadata cache. Every async method runs on that runtime, so Swift can call
//! it from any executor.
//!
//! Cache invariant: the cache reflects the server as of its stored anchor
//! (`index_id`, `seq`) for every directory it holds, because every anchor the
//! extension hands to the system first brings the cache up to it. Cached
//! listings can therefore be answered without the network; a background
//! revalidation (`if_dir_rev`) catches anything the change feed missed and
//! queues it for the next working-set enumeration.

mod cache;

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::StreamExt;
use homeport_client::{ChangeEvent, Client, Config, Error as ClientError, ListResult, LinkState};
use homeport_proto::{self as proto, ErrorCode, Item, Kind, Meta, Priority};

use cache::{Cache, CachedItem};

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("server unreachable: {msg}")]
    Unreachable { msg: String },
    #[error("not found: {msg}")]
    NotFound { msg: String },
    #[error("sync anchor expired")]
    AnchorExpired { index_id: u64, seq: u64 },
    #[error("cancelled")]
    Cancelled,
    #[error("server error: {msg}")]
    Server { msg: String },
    #[error("io: {msg}")]
    Io { msg: String },
    #[error("configuration: {msg}")]
    Config { msg: String },
    /// The name is taken by item `id` (now in the cache).
    #[error("name is taken by {id}")]
    Collision { id: String },
    #[error("folder is not empty")]
    NotEmpty,
    #[error("server disk is full")]
    NoSpace,
    /// The server refused the change for good (permissions, bad name,
    /// unsupported item); retrying will not help.
    #[error("rejected: {msg}")]
    Rejected { msg: String },
}

impl From<ClientError> for CoreError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Unreachable(msg) => Self::Unreachable { msg },
            ClientError::Server { code: ErrorCode::NotFound | ErrorCode::OutsideShare, message } => {
                Self::NotFound { msg: message }
            }
            ClientError::Server { message, .. } => Self::Server { msg: message },
            ClientError::AnchorExpired { index_id, seq } => Self::AnchorExpired { index_id, seq },
            e @ ClientError::Protocol(_) => Self::Server { msg: e.to_string() },
            ClientError::Config(msg) => Self::Config { msg },
            ClientError::Collision(item) => Self::Collision { id: item.id },
            ClientError::Local(msg) => Self::Io { msg },
        }
    }
}

/// Errors of write calls: permanent refusals are told apart from transient
/// failures, and the item a collision names is cached for the caller.
fn write_error(s: &Shared, e: ClientError) -> CoreError {
    match e {
        ClientError::Collision(item) => {
            let id = item.id.clone();
            if let Err(e) = s.store_written(*item, None, None) {
                tracing::warn!(error = %e, "caching a collision's item");
            }
            CoreError::Collision { id }
        }
        ClientError::Server { code: ErrorCode::NotEmpty, .. } => CoreError::NotEmpty,
        ClientError::Server { code: ErrorCode::NoSpace, .. } => CoreError::NoSpace,
        ClientError::Server {
            code:
                ErrorCode::Denied
                | ErrorCode::BadName
                | ErrorCode::OutsideShare
                | ErrorCode::Unsupported
                | ErrorCode::BadRequest
                | ErrorCode::NotADirectory
                | ErrorCode::NotAFile,
            message,
        } => CoreError::Rejected { msg: message },
        e => e.into(),
    }
}

impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Io { msg: format!("cache: {e}") }
    }
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io { msg: e.to_string() }
    }
}

type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, Clone, uniffi::Record)]
pub struct CoreConfig {
    /// `ip:port` of homeportd.
    pub server: String,
    /// Hex SHA-256 of the server certificate.
    pub fingerprint: String,
    /// Stable per client machine: the server keys its change-feed filter on it.
    pub client_id: String,
    /// SQLite metadata cache; `None` keeps no cache (the menu bar app).
    pub cache_path: Option<String>,
    pub request_deadline_ms: u32,
    pub worker_threads: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ItemKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct FfiItem {
    pub id: String,
    pub parent_id: String,
    pub name: String,
    pub kind: ItemKind,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub mode: u32,
    pub rev: u64,
    pub content_rev: u64,
    /// Relative target inside the same share (symlinks only).
    pub symlink_target: Option<String>,
    /// 0 when unknown.
    pub crtime_ns: i64,
    /// BSD flags (`UF_HIDDEN` = 0x8000).
    pub flags: u32,
    /// Finder tags (raw `_kMDItemUserTags` value).
    pub tags: Option<Vec<u8>>,
}

impl From<CachedItem> for FfiItem {
    fn from(c: CachedItem) -> Self {
        let i = c.item;
        Self {
            id: i.id,
            parent_id: i.parent_id,
            name: i.name,
            kind: match i.kind {
                Kind::File => ItemKind::File,
                Kind::Dir => ItemKind::Dir,
                Kind::Symlink => ItemKind::Symlink,
                Kind::Other => ItemKind::Other,
            },
            size: i.size,
            mtime_ns: i.mtime_ns,
            ctime_ns: i.ctime_ns,
            mode: i.mode,
            rev: i.rev,
            content_rev: i.content_rev,
            symlink_target: c.link_target,
            crtime_ns: i.crtime_ns,
            flags: i.flags,
            tags: i.tags,
        }
    }
}

/// Metadata to apply on the server; `None` leaves a field alone.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct FfiMeta {
    pub mtime_ns: Option<i64>,
    pub crtime_ns: Option<i64>,
    pub mode: Option<u32>,
    pub hidden: Option<bool>,
    /// Whether `tags` should be applied (`tags == None` then removes them).
    pub set_tags: bool,
    pub tags: Option<Vec<u8>>,
    pub xattrs: HashMap<String, Vec<u8>>,
}

impl From<FfiMeta> for Meta {
    fn from(m: FfiMeta) -> Self {
        let mut xattrs: Vec<(String, Vec<u8>)> = m.xattrs.into_iter().collect();
        xattrs.sort();
        Meta {
            mtime_ns: m.mtime_ns,
            crtime_ns: m.crtime_ns,
            mode: m.mode,
            hidden: m.hidden,
            tags: m.set_tags.then_some(m.tags),
            xattrs,
        }
    }
}

/// Result of a content upload.
#[derive(Debug, Clone, uniffi::Record)]
pub struct WriteOutcome {
    /// The item as the server has it now. On a conflict, the server's own
    /// version (the client must fetch it).
    pub item: FfiItem,
    /// The copy holding the upload, when it conflicted.
    pub conflict: Option<FfiItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Anchor {
    pub index_id: u64,
    pub seq: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct DirState {
    pub dir_rev: u64,
    pub total: u32,
    /// Answered from the local cache without waiting for the server.
    pub from_cache: bool,
    /// The cached copy could not be revalidated (server unreachable).
    pub stale: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ChangeSet {
    pub anchor: Anchor,
    pub changed: Vec<FfiItem>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ReadDone {
    pub bytes: u64,
    pub size: u64,
    pub content_rev: u64,
    pub wire_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LinkClass {
    Good,
    Degraded,
    Poor,
    Offline,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct LinkInfo {
    pub state: LinkClass,
    /// QUIC smoothed RTT.
    pub rtt_ms: Option<f64>,
    pub ping_ms: Option<f64>,
    pub loss: f64,
    pub throughput_bps: Option<f64>,
    pub consecutive_misses: u32,
    /// At least one probe has completed (before that, `Offline` means "unknown").
    pub probed: bool,
}

#[uniffi::export(foreign)]
pub trait ProgressSink: Send + Sync {
    fn progress(&self, done: u64);
}

#[uniffi::export(foreign)]
pub trait ChangeListener: Send + Sync {
    /// The server reported changes in directories this client has listed.
    fn changed(&self);
}

/// Cancels an in-flight `read_to_file`.
#[derive(uniffi::Object)]
pub struct CancelToken {
    tx: tokio::sync::watch::Sender<bool>,
}

#[uniffi::export]
impl CancelToken {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self { tx: tokio::sync::watch::channel(false).0 })
    }

    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }
}

/// A running change-feed subscription; stops when cancelled or dropped.
#[derive(uniffi::Object)]
pub struct Subscription {
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[uniffi::export]
impl Subscription {
    pub fn cancel(&self) {
        if let Some(t) = lock(&self.task).take() {
            t.abort();
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Names never shown to the system (Finder metadata from the server's disk).
fn is_noise(name: &str) -> bool {
    matches!(name, ".DS_Store" | ".localized" | ".Spotlight-V100" | ".Trashes" | ".fseventsd" | ".TemporaryItems" | "Icon\r")
        || name.starts_with("._")
}

#[derive(Default)]
struct Pending {
    changed: HashMap<String, CachedItem>,
    deleted: HashSet<String>,
}

struct Shared {
    client: Client,
    cache: Option<Mutex<Cache>>,
    pending: Mutex<Pending>,
    revalidating: Mutex<HashSet<String>>,
    /// Circuit breaker: set (to the probe count at the time) when a request
    /// found the server unreachable. Until a later probe succeeds, requests
    /// fail at once instead of each waiting out its deadline; the system
    /// retries failed calls several times, which would multiply the wait.
    tripped_at_probe: Mutex<Option<u64>>,
}

#[derive(uniffi::Object)]
pub struct Core {
    rt: tokio::runtime::Runtime,
    shared: Arc<Shared>,
}

const ANCHOR_INDEX: &str = "anchor_index_id";
const ANCHOR_SEQ: &str = "anchor_seq";

/// An empty file to upload (the system gives no URL for zero-byte files).
fn tempfile_empty() -> Result<std::fs::File> {
    let path = std::env::temp_dir().join(format!("homeport-empty-{}", std::process::id()));
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(f)
}

impl Shared {
    /// Records an item the server just wrote in the cache (moving it out of
    /// `old_parent`), so the change feed's report of it is not news.
    fn store_written(&self, item: Item, old_parent: Option<String>, link_target: Option<String>) -> Result<CachedItem> {
        let mut link_target = link_target;
        if let Some(c) = self.cache() {
            if link_target.is_none()
                && let Some(old) = c.item(&item.id)?
            {
                link_target = old.link_target;
            }
            let ci = CachedItem { item, link_target };
            c.put_item(&ci, old_parent.as_deref())?;
            return Ok(ci);
        }
        Ok(CachedItem { item, link_target })
    }

    fn check_breaker(&self) -> Result<()> {
        let mut t = lock(&self.tripped_at_probe);
        let Some(at) = *t else { return Ok(()) };
        let link = self.client.link();
        if link.probes > at && link.consecutive_misses == 0 {
            *t = None;
            return Ok(());
        }
        Err(CoreError::Unreachable { msg: "server did not answer recently".into() })
    }

    fn note_result<T>(&self, r: &Result<T>) {
        if let Err(CoreError::Unreachable { .. }) = r {
            let mut t = lock(&self.tripped_at_probe);
            if t.is_none() {
                *t = Some(self.client.link().probes);
            }
        }
    }

    /// Runs a user-facing request behind the circuit breaker.
    async fn guarded<T>(&self, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        self.check_breaker()?;
        let r = fut.await;
        self.note_result(&r);
        r
    }

    fn cache(&self) -> Option<MutexGuard<'_, Cache>> {
        self.cache.as_ref().map(lock)
    }

    fn cache_anchor(&self) -> Result<Option<Anchor>> {
        let Some(c) = self.cache() else { return Ok(None) };
        Ok(match (c.meta(ANCHOR_INDEX)?, c.meta(ANCHOR_SEQ)?) {
            (Some(i), Some(s)) => Some(Anchor { index_id: i as u64, seq: s as u64 }),
            _ => None,
        })
    }

    fn set_cache_anchor(&self, a: Anchor) -> Result<()> {
        if let Some(c) = self.cache() {
            let cur = (c.meta(ANCHOR_INDEX)?, c.meta(ANCHOR_SEQ)?);
            if cur.0 == Some(a.index_id as i64) && cur.1.is_some_and(|s| s as u64 >= a.seq) {
                return Ok(());
            }
            if cur.0.is_some_and(|i| i != a.index_id as i64) {
                c.forget_dir_revs()?;
            }
            c.set_meta(ANCHOR_INDEX, a.index_id as i64)?;
            c.set_meta(ANCHOR_SEQ, a.seq as i64)?;
        }
        Ok(())
    }

    /// The server's current anchor. A `Changes` request beyond the current
    /// seq is answered with `SyncAnchorExpired` carrying the current anchor.
    async fn server_anchor(&self) -> Result<Anchor> {
        if self.client.hello().is_none() {
            self.client.stat(proto::ROOT_ID, Priority::Visible).await?;
        }
        let index_id = self.client.hello().map_or(0, |h| h.index_id);
        let mut s = self.client.changes(u64::MAX, index_id, false).await?;
        match s.next().await {
            Err(ClientError::AnchorExpired { index_id, seq }) => Ok(Anchor { index_id, seq }),
            Err(e) => Err(e.into()),
            Ok(_) => Err(CoreError::Server { msg: "server did not report its anchor".into() }),
        }
    }

    /// Fills in symlink targets and drops noise and symlinks that leave their share.
    async fn prepare(&self, items: Vec<Item>) -> Result<Vec<CachedItem>> {
        let known: HashMap<String, String> = {
            let cache = self.cache();
            let mut m = HashMap::new();
            if let Some(c) = cache {
                for i in items.iter().filter(|i| i.kind == Kind::Symlink) {
                    if let Some(ci) = c.item(&i.id)?
                        && ci.item.content_rev == i.content_rev
                        && let Some(t) = ci.link_target
                    {
                        m.insert(i.id.clone(), t);
                    }
                }
            }
            m
        };
        let futs = items.into_iter().filter(|i| !is_noise(&i.name)).map(|item| {
            let known = known.get(&item.id).cloned();
            async move {
                if item.kind != Kind::Symlink {
                    return Ok(Some(CachedItem { item, link_target: None }));
                }
                if let Some(t) = known {
                    return Ok(Some(CachedItem { item, link_target: Some(t) }));
                }
                match self.read_link(&item.id).await {
                    Ok(t) => Ok(Some(CachedItem { item, link_target: Some(t) })),
                    Err(CoreError::NotFound { .. }) => Ok(None),
                    Err(e) => Err(e),
                }
            }
        });
        let out: Vec<Result<Option<CachedItem>>> = futures::stream::iter(futs).buffered(16).collect().await;
        out.into_iter().filter_map(Result::transpose).collect()
    }

    async fn read_link(&self, id: &str) -> Result<String> {
        Ok(self.client.read_link(id, Priority::Visible).await?)
    }

    /// Fetches a listing from the server and stores it. Returns the new
    /// listing's diff against the previous cached copy.
    async fn fetch_dir(&self, dir: &str, if_rev: Option<u64>) -> Result<(DirState, Option<Pending>)> {
        match self.client.list_dir(dir, if_rev, Priority::Visible).await? {
            ListResult::NotModified { dir_rev } => {
                let total = self.cache().and_then(|c| c.dir(dir).ok().flatten()).map_or(0, |d| d.1);
                Ok((DirState { dir_rev, total, from_cache: false, stale: false }, None))
            }
            ListResult::Listing { dir_rev, items } => {
                let items = self.prepare(items.as_ref().clone()).await?;
                let total = items.len() as u32;
                let mut diff = None;
                if let Some(mut c) = self.cache() {
                    if c.dir(dir)?.is_some() {
                        let old = c.page(dir, 0, u32::MAX)?;
                        let new_ids: HashSet<&str> = items.iter().map(|i| i.item.id.as_str()).collect();
                        let old_by_id: HashMap<&str, &CachedItem> =
                            old.iter().map(|i| (i.item.id.as_str(), i)).collect();
                        let mut p = Pending::default();
                        for i in &items {
                            if old_by_id.get(i.item.id.as_str()).is_none_or(|o| o.item != i.item) {
                                p.changed.insert(i.item.id.clone(), i.clone());
                            }
                        }
                        for o in &old {
                            if !new_ids.contains(o.item.id.as_str()) {
                                p.deleted.insert(o.item.id.clone());
                            }
                        }
                        diff = Some(p);
                    }
                    c.store_listing(dir, dir_rev, &items)?;
                }
                Ok((DirState { dir_rev, total, from_cache: false, stale: false }, diff))
            }
        }
    }

    /// Applies server changes to the cache; returns what the system should hear about.
    fn apply(&self, changed: Vec<CachedItem>, deleted: Vec<String>) -> Result<(Vec<CachedItem>, Vec<String>)> {
        let Some(mut c) = self.cache() else { return Ok((changed, deleted)) };
        // A folder's first listing indexes it on the server, so its items come
        // back in the change feed with the listing's own revisions. The
        // system already has those from the enumeration: reporting them again
        // makes fileproviderd reconcile every child a second time.
        let mut unchanged = HashSet::new();
        for i in &changed {
            if let Some(old) = c.item(&i.item.id)?
                && old.item == i.item
                && old.link_target == i.link_target
            {
                unchanged.insert(i.item.id.clone());
            }
        }
        c.apply_changes(&changed, &deleted)?;
        let mut kept = Vec::with_capacity(changed.len());
        for i in changed {
            if unchanged.contains(&i.item.id) {
                continue;
            }
            if i.item.parent_id == proto::ROOT_ID || c.dir(&i.item.parent_id)?.is_some() {
                kept.push(i);
            }
        }
        Ok((kept, deleted))
    }

    async fn changes_since(&self, since: Anchor) -> Result<ChangeSet> {
        let mut stream = self.client.changes(since.seq, since.index_id, false).await?;
        let mut changed = Vec::new();
        let mut deleted = Vec::new();
        let mut seq = since.seq;
        loop {
            match stream.next().await {
                Ok(Some(ChangeEvent::Batch { seq: s, changed: c, deleted: d, caught_up })) => {
                    changed.extend(c);
                    deleted.extend(d.into_iter().map(|d| d.id));
                    seq = s;
                    if caught_up {
                        break;
                    }
                }
                Ok(None) => break,
                Err(ClientError::AnchorExpired { index_id, seq }) => {
                    self.set_cache_anchor(Anchor { index_id, seq })?;
                    return Err(CoreError::AnchorExpired { index_id, seq });
                }
                Err(e) => return Err(e.into()),
            }
        }
        // Changed symlinks that now leave their share disappear.
        let ids: HashSet<String> = changed.iter().map(|i| i.id.clone()).collect();
        let prepared = self.prepare(changed).await?;
        let kept_ids: HashSet<&str> = prepared.iter().map(|i| i.item.id.as_str()).collect();
        deleted.extend(ids.iter().filter(|id| !kept_ids.contains(id.as_str())).cloned());
        let (mut changed, mut deleted) = self.apply(prepared, deleted)?;
        let anchor = Anchor { index_id: since.index_id, seq };
        self.set_cache_anchor(anchor)?;
        // Plus whatever background revalidation found.
        let p = std::mem::take(&mut *lock(&self.pending));
        let seen: HashSet<String> = changed.iter().map(|i| i.item.id.clone()).collect();
        changed.extend(p.changed.into_values().filter(|i| !seen.contains(&i.item.id)));
        deleted.extend(p.deleted);
        let changed_ids: HashSet<&str> = changed.iter().map(|i| i.item.id.as_str()).collect();
        deleted.retain(|d| !changed_ids.contains(d.as_str()));
        deleted.sort();
        deleted.dedup();
        Ok(ChangeSet { anchor, changed: changed.into_iter().map(Into::into).collect(), deleted })
    }
}

impl Core {
    /// Runs `f` on the core's runtime.
    async fn run<T, F, Fut>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Shared>) -> Fut,
        Fut: std::future::Future<Output = Result<T>> + Send + 'static,
    {
        self.rt.spawn(f(self.shared.clone())).await.map_err(|e| CoreError::Io { msg: format!("task: {e}") })?
    }
}

#[uniffi::export]
impl Core {
    #[uniffi::constructor]
    pub fn new(config: CoreConfig) -> Result<Arc<Self>> {
        let server: SocketAddr =
            config.server.parse().map_err(|e| CoreError::Config { msg: format!("server {:?}: {e}", config.server) })?;
        let fingerprint = homeport_client::parse_fingerprint(&config.fingerprint)
            .ok_or_else(|| CoreError::Config { msg: "fingerprint must be 64 hex digits".into() })?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.worker_threads.clamp(1, 8) as usize)
            .thread_name("homeport-core")
            .enable_all()
            .build()?;
        let mut cfg = Config::new(server, fingerprint, config.client_id.clone());
        cfg.request_deadline = Duration::from_millis(u64::from(config.request_deadline_ms.max(100)));
        let client = {
            let _g = rt.enter();
            Client::new(cfg)?
        };
        let cache = match &config.cache_path {
            Some(p) => {
                let p = PathBuf::from(p);
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                Some(Mutex::new(Cache::open(&p)?))
            }
            None => None,
        };
        let shared = Arc::new(Shared {
            client,
            cache,
            pending: Mutex::new(Pending::default()),
            revalidating: Mutex::new(HashSet::new()),
            tripped_at_probe: Mutex::new(None),
        });
        Ok(Arc::new(Self { rt, shared }))
    }

    /// The current anchor, after bringing the cache up to it. Offline, the
    /// cache's own anchor (what the cached data reflects).
    pub async fn sync_anchor(&self) -> Result<Anchor> {
        self.run(|s: Arc<Shared>| async move {
            let cached = s.cache_anchor()?;
            let current = match s.guarded(s.server_anchor()).await {
                Ok(a) => a,
                Err(CoreError::Unreachable { msg }) => return cached.ok_or(CoreError::Unreachable { msg }),
                Err(e) => return Err(e),
            };
            match cached {
                Some(c) if c.index_id == current.index_id && c.seq < current.seq => match s.changes_since(c).await {
                    // Keep these for the system's next enumerateChanges.
                    Ok(cs) => {
                        let mut p = lock(&s.pending);
                        for i in cs.changed {
                            let link_target = i.symlink_target.clone();
                            let item = ffi_to_item(i);
                            p.changed.insert(item.id.clone(), CachedItem { item, link_target });
                        }
                        p.deleted.extend(cs.deleted);
                        Ok(cs.anchor)
                    }
                    Err(CoreError::AnchorExpired { index_id, seq }) => Ok(Anchor { index_id, seq }),
                    Err(e) => Err(e),
                },
                Some(c) if c.index_id == current.index_id => Ok(c),
                _ => {
                    s.set_cache_anchor(current)?;
                    Ok(current)
                }
            }
        })
        .await
    }

    /// Changes since `since` (applied to the cache), plus anything background
    /// revalidation found.
    pub async fn changes(&self, since: Anchor) -> Result<ChangeSet> {
        self.run(move |s: Arc<Shared>| async move { s.guarded(s.changes_since(since)).await }).await
    }

    /// Makes sure `dir_id`'s listing is in the cache. A cached listing is
    /// answered at once and revalidated in the background.
    pub async fn list(&self, dir_id: String) -> Result<DirState> {
        let shared = self.shared.clone();
        let cached = shared.cache().map(|c| c.dir(&dir_id)).transpose()?.flatten();
        if let Some((Some(rev), total)) = cached {
            let first = lock(&shared.revalidating).insert(dir_id.clone());
            if first {
                let s = shared.clone();
                self.rt.spawn(async move {
                    match s.fetch_dir(&dir_id, Some(rev)).await {
                        Ok((_, Some(diff))) if !diff.changed.is_empty() || !diff.deleted.is_empty() => {
                            tracing::info!(dir = dir_id, "revalidation found changes");
                            let mut p = lock(&s.pending);
                            p.changed.extend(diff.changed);
                            p.deleted.extend(diff.deleted);
                        }
                        Ok(_) => {}
                        Err(e) => tracing::debug!(dir = dir_id, error = %e, "revalidation failed"),
                    }
                    lock(&s.revalidating).remove(&dir_id);
                });
            }
            return Ok(DirState { dir_rev: rev, total, from_cache: true, stale: false });
        }
        self.run(move |s: Arc<Shared>| async move {
            match s.guarded(s.fetch_dir(&dir_id, None)).await {
                Ok((st, _)) => Ok(st),
                Err(CoreError::Unreachable { msg }) => match cached {
                    // A listing from before a server index change: better than nothing offline.
                    Some((_, total)) => Ok(DirState { dir_rev: 0, total, from_cache: true, stale: true }),
                    None => Err(CoreError::Unreachable { msg }),
                },
                Err(e) => Err(e),
            }
        })
        .await
    }

    /// Whether background revalidation has found changes the system has not heard about yet.
    pub fn has_pending_changes(&self) -> bool {
        let p = lock(&self.shared.pending);
        !p.changed.is_empty() || !p.deleted.is_empty()
    }

    /// One page of a cached listing, sorted by name.
    pub fn page(&self, dir_id: String, offset: u32, limit: u32) -> Result<Vec<FfiItem>> {
        let Some(c) = self.shared.cache() else { return Ok(Vec::new()) };
        Ok(c.page(&dir_id, offset, limit)?.into_iter().map(Into::into).collect())
    }

    /// One page of every item in a cached directory (the working set).
    pub fn working_set_page(&self, offset: u32, limit: u32) -> Result<Vec<FfiItem>> {
        let Some(c) = self.shared.cache() else { return Ok(Vec::new()) };
        Ok(c.listed_items(offset, limit)?.into_iter().map(Into::into).collect())
    }

    /// Records a completed download of `id` at `content_rev`.
    pub fn note_fetched(&self, id: String, content_rev: u64) -> Result<()> {
        if let Some(c) = self.shared.cache() {
            c.note_fetched(&id, content_rev)?;
        }
        Ok(())
    }

    /// Small files never downloaded at their current content: in `dir_id`
    /// smallest first, or (`None`) anywhere in the cache in random order.
    pub fn small_unfetched_files(&self, dir_id: Option<String>, max_size: u64, limit: u32) -> Result<Vec<FfiItem>> {
        let Some(c) = self.shared.cache() else { return Ok(Vec::new()) };
        Ok(c.small_unfetched(dir_id.as_deref(), max_size, limit)?.into_iter().map(Into::into).collect())
    }

    pub fn cached_item(&self, id: String) -> Result<Option<FfiItem>> {
        let Some(c) = self.shared.cache() else { return Ok(None) };
        Ok(c.item(&id)?.map(Into::into))
    }

    /// Metadata of one item: the cache if it has it, else the server.
    pub async fn item(&self, id: String) -> Result<FfiItem> {
        if let Some(i) = self.cached_item(id.clone())? {
            return Ok(i);
        }
        self.run(move |s: Arc<Shared>| async move {
            s.guarded(async {
                let item = s.client.stat(&id, Priority::Visible).await?;
                let mut v = s.prepare(vec![item]).await?;
                v.pop().map(Into::into).ok_or(CoreError::NotFound { msg: id.clone() })
            })
            .await
        })
        .await
    }

    /// Streams `[offset, offset+len)` (`len == 0`: to the end) of a file into
    /// `path` at the same offsets. Fails fast when the server is unreachable.
    pub async fn read_to_file(
        &self,
        id: String,
        path: String,
        offset: u64,
        len: u64,
        progress: Option<Arc<dyn ProgressSink>>,
        cancel: Arc<CancelToken>,
    ) -> Result<ReadDone> {
        let mut cancelled = cancel.tx.subscribe();
        self.run(move |s: Arc<Shared>| async move {
            let file = OpenOptions::new().write(true).create(true).truncate(offset == 0 && len == 0).open(&path)?;
            let read = async {
                let mut stream = s.client.read_stream(&id, offset, len, false, Priority::UserOpen).await?;
                let mut done = 0u64;
                while let Some((off, chunk)) = stream.next_chunk().await? {
                    file.write_all_at(&chunk, off)?;
                    done += chunk.len() as u64;
                    if let Some(p) = &progress {
                        p.progress(done);
                    }
                }
                let (bytes, size, content_rev) = stream.end().unwrap_or_default();
                if len == 0 {
                    file.set_len(offset + bytes)?;
                }
                Ok(ReadDone { bytes, size, content_rev, wire_bytes: stream.wire_bytes() })
            };
            s.check_breaker()?;
            let r = tokio::select! {
                r = read => r,
                _ = cancelled.wait_for(|c| *c) => Err(CoreError::Cancelled),
            };
            s.note_result(&r);
            r
        })
        .await
    }

    /// A PNG thumbnail made on the server, or `None` when it has none.
    pub async fn thumbnail(&self, id: String, max_pixels: u32) -> Result<Option<Vec<u8>>> {
        self.run(move |s: Arc<Shared>| async move {
            // Behind the breaker, but a slow thumbnail (Quick Look on the
            // server can outlast the deadline) must not trip it for everything else.
            s.check_breaker()?;
            let png = s.client.thumbnail(&id, max_pixels).await?;
            Ok((!png.is_empty()).then(|| png.to_vec()))
        })
        .await
    }

    /// Calls `listener.changed()` whenever the server reports changes in
    /// directories this client id has listed. Reconnects on its own.
    pub fn subscribe(&self, listener: Arc<dyn ChangeListener>) -> Arc<Subscription> {
        let s = self.shared.clone();
        let task = self.rt.spawn(async move {
            loop {
                if let Ok(anchor) = s.server_anchor().await {
                    match s.client.changes(anchor.seq, anchor.index_id, true).await {
                        Ok(mut stream) => loop {
                            match stream.next().await {
                                Ok(Some(ChangeEvent::Batch { changed, deleted, .. })) => {
                                    if !changed.is_empty() || !deleted.is_empty() {
                                        listener.changed();
                                    }
                                }
                                Err(ClientError::AnchorExpired { .. }) => {
                                    listener.changed();
                                    break;
                                }
                                Ok(None) | Err(_) => break,
                            }
                        },
                        Err(e) => tracing::debug!(error = %e, "change feed"),
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        Arc::new(Subscription { task: Mutex::new(Some(task)) })
    }

    /// Creates a file (content from `content_path`), folder or symlink in
    /// `parent_id`. `op_id` (the system's template identifier) makes
    /// retries safe. Fails with `Collision` when the name is taken.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_item(
        &self,
        op_id: String,
        parent_id: String,
        name: String,
        kind: ItemKind,
        content_path: Option<String>,
        link_target: Option<String>,
        meta: FfiMeta,
        may_exist: bool,
        progress: Option<Arc<dyn ProgressSink>>,
        cancel: Arc<CancelToken>,
    ) -> Result<FfiItem> {
        let mut cancelled = cancel.tx.subscribe();
        self.run(move |s: Arc<Shared>| async move {
            s.check_breaker()?;
            let kind = match kind {
                ItemKind::File => Kind::File,
                ItemKind::Dir => Kind::Dir,
                ItemKind::Symlink => Kind::Symlink,
                ItemKind::Other => return Err(CoreError::Rejected { msg: "unsupported item kind".into() }),
            };
            let target_copy = link_target.clone();
            let file = match (&content_path, kind) {
                (Some(p), Kind::File) => Some(std::fs::File::open(p)?),
                (None, Kind::File) => None,
                _ => None,
            };
            // Small creates go ahead of bulk ones.
            let size = file.as_ref().map(|f| f.metadata().map(|m| m.len())).transpose()?.unwrap_or(0);
            let priority = if size < 1 << 20 { Priority::Visible } else { Priority::Background };
            let report = |n: u64| {
                if let Some(p) = &progress {
                    p.progress(n);
                }
            };
            let empty;
            let file = match file {
                Some(f) => Some(f),
                None if kind == Kind::File => {
                    // An empty file: the system passes no URL for zero bytes.
                    empty = tempfile_empty()?;
                    Some(empty)
                }
                None => None,
            };
            let upload = file.map(|f| homeport_client::Upload { file: f, size, progress: &report });
            let fut = s.client.create(
                &op_id,
                &parent_id,
                &name,
                kind,
                link_target,
                meta.into(),
                may_exist,
                upload,
                priority,
            );
            let r = tokio::select! {
                r = fut => r.map_err(|e| write_error(&s, e)),
                _ = cancelled.wait_for(|c| *c) => Err(CoreError::Cancelled),
            };
            s.note_result(&r);
            let item = r?;
            let c = s.store_written(item, None, target_copy)?;
            Ok(c.into())
        })
        .await
    }

    /// Replaces a file's content from `content_path`. `base_content_rev`
    /// is the version the edit started from (`None`: unknown, no check).
    pub async fn write_content(
        &self,
        id: String,
        base_content_rev: Option<u64>,
        content_path: String,
        meta: FfiMeta,
        progress: Option<Arc<dyn ProgressSink>>,
        cancel: Arc<CancelToken>,
    ) -> Result<WriteOutcome> {
        let mut cancelled = cancel.tx.subscribe();
        self.run(move |s: Arc<Shared>| async move {
            s.check_breaker()?;
            let file = std::fs::File::open(&content_path)?;
            let size = file.metadata()?.len();
            let report = |n: u64| {
                if let Some(p) = &progress {
                    p.progress(n);
                }
            };
            let upload = homeport_client::Upload { file, size, progress: &report };
            // An edit someone is waiting on goes ahead of everything else.
            let priority = if size < 8 << 20 { Priority::UserOpen } else { Priority::Background };
            let device = device_name();
            let fut = s.client.write_content(&id, base_content_rev, meta.into(), &device, upload, priority);
            let r = tokio::select! {
                r = fut => r.map_err(|e| write_error(&s, e)),
                _ = cancelled.wait_for(|c| *c) => Err(CoreError::Cancelled),
            };
            s.note_result(&r);
            let (item, conflict) = r?;
            let item = s.store_written(item, None, None)?;
            let conflict = conflict.map(|c| s.store_written(c, None, None)).transpose()?;
            Ok(WriteOutcome { item: item.into(), conflict: conflict.map(Into::into) })
        })
        .await
    }

    /// Renames and/or moves an item and applies metadata.
    pub async fn modify_item(
        &self,
        id: String,
        new_parent_id: Option<String>,
        new_name: Option<String>,
        meta: FfiMeta,
    ) -> Result<FfiItem> {
        self.run(move |s: Arc<Shared>| async move {
            let old_parent = s.cache().and_then(|c| c.item(&id).ok().flatten()).map(|c| c.item.parent_id);
            let r = s.guarded(async { s.client.modify(&id, new_parent_id, new_name, meta.into()).await.map_err(|e| write_error(&s, e)) }).await;
            let item = r?;
            Ok(s.store_written(item, old_parent, None)?.into())
        })
        .await
    }

    /// Moves an item to basebook's Trash. With `listed` it appears in the
    /// trash container (Finder's Move to Trash); without, it is a delete.
    pub async fn trash_item(&self, id: String, listed: bool, recursive: bool) -> Result<FfiItem> {
        self.run(move |s: Arc<Shared>| async move {
            let old_parent = s.cache().and_then(|c| c.item(&id).ok().flatten()).map(|c| c.item.parent_id);
            let item = s.guarded(async { s.client.trash(&id, listed, recursive).await.map_err(|e| write_error(&s, e)) }).await?;
            if listed {
                Ok(s.store_written(item, old_parent, None)?.into())
            } else {
                if let Some(c) = s.cache() {
                    c.remove_item(&id)?;
                }
                Ok(CachedItem { item, link_target: None }.into())
            }
        })
        .await
    }

    /// Puts a trash container item back as `parent_id/name`.
    pub async fn untrash_item(&self, id: String, parent_id: String, name: String) -> Result<FfiItem> {
        self.run(move |s: Arc<Shared>| async move {
            let item =
                s.guarded(async { s.client.untrash(&id, &parent_id, &name).await.map_err(|e| write_error(&s, e)) }).await?;
            Ok(s.store_written(item, Some(proto::TRASH_ID.to_owned()), None)?.into())
        })
        .await
    }

    /// Drops an item from the trash container; it stays in basebook's Trash.
    pub async fn forget_item(&self, id: String) -> Result<()> {
        self.run(move |s: Arc<Shared>| async move {
            s.guarded(async { s.client.forget(&id).await.map_err(|e| write_error(&s, e)) }).await?;
            if let Some(c) = s.cache() {
                c.remove_item(&id)?;
            }
            Ok(())
        })
        .await
    }

    pub fn link(&self) -> LinkInfo {
        let s = self.shared.client.link();
        LinkInfo {
            state: match s.state {
                LinkState::Good => LinkClass::Good,
                LinkState::Degraded => LinkClass::Degraded,
                LinkState::Poor => LinkClass::Poor,
                LinkState::Offline => LinkClass::Offline,
            },
            rtt_ms: s.rtt.map(|d| d.as_secs_f64() * 1000.0),
            ping_ms: s.ping_rtt.map(|d| d.as_secs_f64() * 1000.0),
            loss: s.loss,
            throughput_bps: s.throughput_bps,
            consecutive_misses: s.consecutive_misses,
            probed: s.probes > 0,
        }
    }

    /// One app-level ping; milliseconds.
    pub async fn ping(&self) -> Result<f64> {
        self.run(|s: Arc<Shared>| async move { Ok(s.client.ping().await?.as_secs_f64() * 1000.0) }).await
    }

    pub fn clear_cache(&self) -> Result<()> {
        if let Some(c) = self.shared.cache() {
            c.clear()?;
        }
        *lock(&self.shared.pending) = Pending::default();
        Ok(())
    }
}

/// Names never uploaded: Finder metadata and editor temporary files.
#[uniffi::export]
pub fn is_junk_name(name: String) -> bool {
    proto::is_junk(&name)
}

fn ffi_to_item(i: FfiItem) -> Item {
    Item {
        id: i.id,
        parent_id: i.parent_id,
        name: i.name,
        kind: match i.kind {
            ItemKind::File => Kind::File,
            ItemKind::Dir => Kind::Dir,
            ItemKind::Symlink => Kind::Symlink,
            ItemKind::Other => Kind::Other,
        },
        size: i.size,
        mtime_ns: i.mtime_ns,
        ctime_ns: i.ctime_ns,
        mode: i.mode,
        rev: i.rev,
        content_rev: i.content_rev,
        crtime_ns: i.crtime_ns,
        flags: i.flags,
        tags: i.tags,
    }
}

/// This Mac's short host name ("airbook"), for conflict copy names.
fn device_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its length.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let name = if ok {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    name.split('.').next().unwrap_or_default().to_owned()
}
