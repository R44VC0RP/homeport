//! Async Homeport client.
//!
//! - One QUIC connection, one bidirectional stream per RPC, stream priority
//!   from the request class.
//! - Identical concurrent `list_dir` and `read_range` calls share one request;
//!   the request is cancelled (its stream reset) when every caller drops it.
//! - Every request has a deadline. When the link is Offline, or the server
//!   does not answer within the deadline, calls fail with
//!   [`Error::Unreachable`] instead of waiting.

mod link;
mod tls;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared, WeakShared};
use homeport_proto::{
    self as proto, Deleted, ErrorCode, Item, ItemId, Kind, ListDirReq, Meta, Priority, Request, RequestBody, Response,
};
use quinn::{Connection, Endpoint, RecvStream};
use tokio::sync::Notify;

pub use link::{LinkConfig, LinkEstimator, LinkSnapshot, LinkState};
pub use tls::parse_fingerprint;

#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    #[error("server unreachable: {0}")]
    Unreachable(String),
    #[error("server error {code:?}: {message}")]
    Server { code: ErrorCode, message: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("sync anchor expired (server index {index_id}, seq {seq})")]
    AnchorExpired { index_id: u64, seq: u64 },
    #[error("configuration: {0}")]
    Config(String),
    #[error("name is taken by item {}", .0.id)]
    Collision(Box<Item>),
    #[error("local file: {0}")]
    Local(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<quinn::ConnectionError> for Error {
    fn from(e: quinn::ConnectionError) -> Self {
        Self::Unreachable(e.to_string())
    }
}
impl From<quinn::WriteError> for Error {
    fn from(e: quinn::WriteError) -> Self {
        Self::Unreachable(e.to_string())
    }
}
impl From<quinn::ReadExactError> for Error {
    fn from(e: quinn::ReadExactError) -> Self {
        match e {
            quinn::ReadExactError::FinishedEarly(_) => Self::Protocol("stream ended mid-frame".into()),
            quinn::ReadExactError::ReadError(e) => Self::Unreachable(e.to_string()),
        }
    }
}
impl From<proto::FrameError> for Error {
    fn from(e: proto::FrameError) -> Self {
        Self::Protocol(e.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub server: SocketAddr,
    pub fingerprint: [u8; 32],
    pub client_id: String,
    /// Time allowed for the first response frame (and between frames).
    pub request_deadline: Duration,
    pub connect_timeout: Duration,
    pub ping_interval: Duration,
    pub idle_ping_interval: Duration,
    pub ping_timeout: Duration,
    pub link: LinkConfig,
}

impl Config {
    pub fn new(server: SocketAddr, fingerprint: [u8; 32], client_id: impl Into<String>) -> Self {
        Self {
            server,
            fingerprint,
            client_id: client_id.into(),
            request_deadline: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(3),
            ping_interval: Duration::from_secs(5),
            idle_ping_interval: Duration::from_secs(30),
            ping_timeout: Duration::from_secs(1),
            link: LinkConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HelloInfo {
    pub index_id: u64,
    pub seq: u64,
    pub roots: Vec<Item>,
}

#[derive(Clone, Debug)]
pub enum ListResult {
    NotModified { dir_rev: u64 },
    Listing { dir_rev: u64, items: Arc<Vec<Item>> },
}

impl ListResult {
    pub fn dir_rev(&self) -> u64 {
        match self {
            Self::NotModified { dir_rev } | Self::Listing { dir_rev, .. } => *dir_rev,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReadResult {
    pub data: Bytes,
    pub size: u64,
    pub content_rev: u64,
    /// Bytes on the wire (after compression).
    pub wire_bytes: u64,
}

#[derive(Clone, Debug)]
pub enum ChangeEvent {
    Batch { seq: u64, changed: Vec<Item>, deleted: Vec<Deleted>, caught_up: bool },
}

type ListKey = (ItemId, Option<u64>);
type ReadKey = (ItemId, u64, u64, bool);
type ListFuture = BoxFuture<'static, Result<ListResult>>;
type ReadFuture = BoxFuture<'static, Result<Arc<ReadResult>>>;
type SharedList = Shared<ListFuture>;
type SharedRead = Shared<ReadFuture>;

struct Inner {
    cfg: Config,
    endpoint: Endpoint,
    conn: Mutex<Option<Connection>>,
    connecting: tokio::sync::Mutex<()>,
    hello: Mutex<Option<HelloInfo>>,
    link: Mutex<LinkEstimator>,
    last_activity: Mutex<Instant>,
    probe_now: Arc<Notify>,
    list_flights: Mutex<HashMap<ListKey, WeakShared<ListFuture>>>,
    read_flights: Mutex<HashMap<ReadKey, WeakShared<ReadFuture>>>,
    pinger: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(h) = lock(&self.pinger).take() {
            h.abort();
        }
        self.endpoint.close(quinn::VarInt::from_u32(0), b"client dropped");
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.initial_mtu(1200);
    let mut mtu = quinn::MtuDiscoveryConfig::default();
    mtu.upper_bound(1232);
    t.mtu_discovery_config(Some(mtu));
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(30_000))));
    t.max_concurrent_uni_streams(quinn::VarInt::from_u32(0));
    t.stream_receive_window(quinn::VarInt::from_u32(8 * 1024 * 1024));
    t.receive_window(quinn::VarInt::from_u32(32 * 1024 * 1024));
    t.send_window(8 * 1024 * 1024);
    t
}

/// Handle to a Homeport server. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

/// An in-flight response stream.
pub struct Responses {
    recv: RecvStream,
    deadline: Option<Duration>,
    inner: Arc<Inner>,
}

impl Responses {
    async fn read_frame(&mut self) -> Result<Option<Response>> {
        let mut prefix = [0u8; 4];
        match self.recv.read_exact(&mut prefix).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = proto::check_len(prefix)?;
        let mut buf = vec![0u8; len];
        self.recv.read_exact(&mut buf).await?;
        Ok(Some(proto::decode(&buf)?))
    }

    /// Next response frame; errors on server `Error` frames and on end of stream.
    pub async fn next(&mut self) -> Result<Response> {
        let frame = match self.deadline {
            Some(d) => match tokio::time::timeout(d, self.read_frame()).await {
                Ok(r) => r,
                Err(_) => {
                    self.inner.probe_now.notify_one();
                    return Err(Error::Unreachable(format!("no response within {} ms", d.as_millis())));
                }
            },
            None => self.read_frame().await,
        }?;
        match frame {
            Some(Response::Error { code, message }) => Err(Error::Server { code, message }),
            Some(Response::Collision { existing }) => Err(Error::Collision(Box::new(existing))),
            Some(r) => Ok(r),
            None => Err(Error::Protocol("response ended early".into())),
        }
    }

    fn set_deadline(&mut self, d: Option<Duration>) {
        self.deadline = d;
    }
}

/// Streaming file content.
pub struct ReadStream {
    resp: Responses,
    started: Instant,
    wire: u64,
    done: Option<(u64, u64, u64)>,
}

impl ReadStream {
    /// Next `(offset, bytes)` chunk, or `None` at the end.
    pub async fn next_chunk(&mut self) -> Result<Option<(u64, Bytes)>> {
        if self.done.is_some() {
            return Ok(None);
        }
        match self.resp.next().await? {
            Response::Data { offset, compressed, raw_len, bytes } => {
                self.wire += bytes.len() as u64;
                let data = if compressed {
                    zstd::bulk::decompress(&bytes, raw_len as usize)
                        .map_err(|e| Error::Protocol(format!("zstd: {e}")))?
                } else {
                    bytes
                };
                Ok(Some((offset, Bytes::from(data))))
            }
            Response::DataEnd { total, size, content_rev } => {
                self.done = Some((total, size, content_rev));
                lock(&self.resp.inner.link).on_transfer(Instant::now(), self.wire, self.started.elapsed());
                Ok(None)
            }
            other => Err(unexpected(&other)),
        }
    }

    /// `(total bytes, file size, content_rev)` once the stream has ended.
    pub fn end(&self) -> Option<(u64, u64, u64)> {
        self.done
    }

    pub fn wire_bytes(&self) -> u64 {
        self.wire
    }
}

pub struct ChangesStream {
    resp: Responses,
    follow: bool,
}

impl ChangesStream {
    /// Next batch; `None` when a non-follow stream is done.
    pub async fn next(&mut self) -> Result<Option<ChangeEvent>> {
        match self.resp.next().await {
            Ok(Response::ChangeBatch { seq, changed, deleted, caught_up }) => {
                if caught_up && self.follow {
                    // From here on batches arrive only when something changes.
                    self.resp.set_deadline(None);
                }
                Ok(Some(ChangeEvent::Batch { seq, changed, deleted, caught_up }))
            }
            Ok(Response::SyncAnchorExpired { index_id, seq }) => Err(Error::AnchorExpired { index_id, seq }),
            Ok(other) => Err(unexpected(&other)),
            Err(Error::Protocol(_)) if !self.follow => Ok(None),
            Err(e) => Err(e),
        }
    }
}

fn unexpected(r: &Response) -> Error {
    let name = format!("{r:?}");
    Error::Protocol(format!("unexpected response {}", name.split([' ', '(', '{']).next().unwrap_or("?")))
}

/// Uploads are sent in pieces of this size.
const UPLOAD_CHUNK: usize = 256 * 1024;
/// A piece may wait this long for flow control before the link counts as down.
const UPLOAD_STALL: Duration = Duration::from_secs(20);
/// Time allowed for the server's answer once a write request is complete.
const WRITE_DEADLINE: Duration = Duration::from_secs(30);

/// Content for an upload.
pub struct Upload<'a> {
    pub file: std::fs::File,
    pub size: u64,
    /// Called with the bytes sent so far.
    pub progress: &'a (dyn Fn(u64) + Send + Sync),
}

impl Client {
    /// Sends a write request, followed by `upload`'s content, and returns
    /// its one answer.
    async fn write_request(&self, priority: Priority, body: RequestBody, upload: Option<Upload<'_>>) -> Result<Response> {
        let inner = self.inner.clone();
        *lock(&inner.last_activity) = Instant::now();
        if lock(&inner.link).fail_fast() {
            return Err(Error::Unreachable("link is offline".into()));
        }
        let frame = proto::encode_frame(&Request { priority, body })?;
        let budget = inner.cfg.request_deadline.max(Duration::from_secs(2));
        let open = async {
            let conn = inner.connection(budget).await?;
            let (mut send, recv) = conn.open_bi().await?;
            send.set_priority(priority.stream_priority()).ok();
            send.write_chunk(frame).await?;
            Ok::<_, Error>((send, recv))
        };
        let (mut send, recv) = match tokio::time::timeout(budget, open).await {
            Ok(r) => r?,
            Err(_) => {
                inner.probe_now.notify_one();
                return Err(Error::Unreachable(format!("no connection within {} ms", budget.as_millis())));
            }
        };
        if let Some(up) = upload {
            let started = Instant::now();
            let mut file = up.file;
            let mut buf = vec![0u8; UPLOAD_CHUNK];
            let mut sent: u64 = 0;
            while sent < up.size {
                let want = (up.size - sent).min(UPLOAD_CHUNK as u64) as usize;
                let n = std::io::Read::read(&mut file, &mut buf[..want]).map_err(|e| Error::Local(e.to_string()))?;
                if n == 0 {
                    // The file shrank under us; the server rejects the short upload.
                    break;
                }
                match tokio::time::timeout(UPLOAD_STALL, send.write_all(&buf[..n])).await {
                    Ok(r) => r?,
                    Err(_) => {
                        inner.probe_now.notify_one();
                        let _ = send.reset(quinn::VarInt::from_u32(0));
                        return Err(Error::Unreachable("upload stalled".into()));
                    }
                }
                sent += n as u64;
                (up.progress)(sent);
            }
            lock(&inner.link).on_transfer(Instant::now(), sent, started.elapsed());
        }
        send.finish().ok();
        let mut resp = Responses { recv, deadline: Some(WRITE_DEADLINE), inner };
        resp.next().await
    }

    /// Creates a file (with `upload`), folder or symlink in `parent_id`.
    /// `op_id` makes retries safe: a repeated create answers the item the
    /// first one made. `Error::Collision` when the name is taken.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        op_id: &str,
        parent_id: &str,
        name: &str,
        kind: Kind,
        link_target: Option<String>,
        meta: Meta,
        may_exist: bool,
        upload: Option<Upload<'_>>,
        priority: Priority,
    ) -> Result<Item> {
        let size = upload.as_ref().map_or(0, |u| u.size);
        let body = RequestBody::Create {
            op_id: op_id.to_owned(),
            parent_id: parent_id.to_owned(),
            name: name.to_owned(),
            kind,
            size,
            link_target,
            meta,
            may_exist,
        };
        match self.write_request(priority, body, upload).await? {
            Response::Item(i) => Ok(i),
            other => Err(unexpected(&other)),
        }
    }

    /// Replaces a file's content. Returns the item and, when the file had
    /// changed since `base_content_rev`, the conflict copy holding the upload.
    pub async fn write_content(
        &self,
        id: &str,
        base_content_rev: Option<u64>,
        meta: Meta,
        device: &str,
        upload: Upload<'_>,
        priority: Priority,
    ) -> Result<(Item, Option<Item>)> {
        let body = RequestBody::WriteContent {
            id: id.to_owned(),
            base_content_rev,
            size: upload.size,
            meta,
            device: device.to_owned(),
        };
        match self.write_request(priority, body, Some(upload)).await? {
            Response::Written { item, conflict } => Ok((item, conflict)),
            other => Err(unexpected(&other)),
        }
    }

    /// Renames, moves and/or changes metadata.
    pub async fn modify(
        &self,
        id: &str,
        new_parent_id: Option<String>,
        new_name: Option<String>,
        meta: Meta,
    ) -> Result<Item> {
        let body = RequestBody::Modify { id: id.to_owned(), new_parent_id, new_name, meta };
        match self.write_request(Priority::Visible, body, None).await? {
            Response::Item(i) => Ok(i),
            other => Err(unexpected(&other)),
        }
    }

    /// Moves an item to the server's Trash (`listed`: into the trash container).
    pub async fn trash(&self, id: &str, listed: bool, recursive: bool) -> Result<Item> {
        let body = RequestBody::Trash { id: id.to_owned(), listed, recursive };
        match self.write_request(Priority::Visible, body, None).await? {
            Response::Item(i) => Ok(i),
            other => Err(unexpected(&other)),
        }
    }

    /// Puts a trash container item back.
    pub async fn untrash(&self, id: &str, parent_id: &str, name: &str) -> Result<Item> {
        let body = RequestBody::Untrash { id: id.to_owned(), parent_id: parent_id.to_owned(), name: name.to_owned() };
        match self.write_request(Priority::Visible, body, None).await? {
            Response::Item(i) => Ok(i),
            other => Err(unexpected(&other)),
        }
    }

    /// Drops an item from the trash container (it stays in the server's Trash).
    pub async fn forget(&self, id: &str) -> Result<()> {
        match self.write_request(Priority::Visible, RequestBody::Forget { id: id.to_owned() }, None).await? {
            Response::End => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    /// Creates a client without connecting.
    pub fn new(cfg: Config) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| Error::Config(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(tls::PinnedVerifier::new(cfg.fingerprint, provider)))
            .with_no_client_auth();
        tls.alpn_protocols = vec![proto::ALPN.to_vec()];
        let crypto =
            quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|e| Error::Config(e.to_string()))?;
        let mut qc = quinn::ClientConfig::new(Arc::new(crypto));
        qc.transport_config(Arc::new(transport_config()));
        let bind: SocketAddr = if cfg.server.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { ([0u16; 8], 0).into() };
        let mut endpoint = Endpoint::client(bind).map_err(|e| Error::Config(format!("bind UDP socket: {e}")))?;
        endpoint.set_default_client_config(qc);
        let inner = Arc::new(Inner {
            link: Mutex::new(LinkEstimator::new(cfg.link.clone())),
            cfg,
            endpoint,
            conn: Mutex::new(None),
            connecting: tokio::sync::Mutex::new(()),
            hello: Mutex::new(None),
            last_activity: Mutex::new(Instant::now()),
            probe_now: Arc::new(Notify::new()),
            list_flights: Mutex::new(HashMap::new()),
            read_flights: Mutex::new(HashMap::new()),
            pinger: Mutex::new(None),
        });
        let handle = tokio::spawn(ping_loop(Arc::downgrade(&inner), inner.probe_now.clone()));
        *lock(&inner.pinger) = Some(handle);
        Ok(Self { inner })
    }

    /// Creates a client and connects (with `Hello`) before returning.
    pub async fn connect(cfg: Config) -> Result<Self> {
        let c = Self::new(cfg)?;
        let deadline = c.inner.cfg.connect_timeout;
        c.inner.connection(deadline).await?;
        Ok(c)
    }

    pub fn config(&self) -> &Config {
        &self.inner.cfg
    }

    pub fn hello(&self) -> Option<HelloInfo> {
        lock(&self.inner.hello).clone()
    }

    pub fn link(&self) -> LinkSnapshot {
        let mut s = lock(&self.inner.link).snapshot();
        if let Some(c) = lock(&self.inner.conn).as_ref() {
            s.rtt = Some(c.rtt());
        }
        s
    }

    /// Sends one app-level ping and feeds the link estimator.
    pub async fn ping(&self) -> Result<Duration> {
        self.inner.probe().await
    }

    async fn request(&self, priority: Priority, body: RequestBody) -> Result<Responses> {
        self.inner.clone().request(priority, body, false).await
    }

    /// Lists a directory. Identical concurrent calls share one request.
    pub async fn list_dir(&self, id: &str, if_dir_rev: Option<u64>, priority: Priority) -> Result<ListResult> {
        let key: ListKey = (id.to_owned(), if_dir_rev);
        let fut: SharedList = {
            let mut flights = lock(&self.inner.list_flights);
            match flights.get(&key).and_then(WeakShared::upgrade) {
                Some(f) => f,
                None => {
                    let this = self.clone();
                    let (id, rev) = key.clone();
                    let f = async move { this.list_dir_uncoalesced(&id, rev, priority).await }.boxed().shared();
                    if let Some(w) = f.downgrade() {
                        flights.insert(key.clone(), w);
                    }
                    f
                }
            }
        };
        let r = fut.await;
        lock(&self.inner.list_flights).remove(&key);
        r
    }

    async fn list_dir_uncoalesced(&self, id: &str, if_dir_rev: Option<u64>, priority: Priority) -> Result<ListResult> {
        let req = RequestBody::ListDir(ListDirReq { id: id.to_owned(), if_dir_rev });
        let mut resp = self.request(priority, req).await?;
        let mut items = Vec::new();
        loop {
            match resp.next().await? {
                Response::NotModified { dir_rev, .. } => return Ok(ListResult::NotModified { dir_rev }),
                Response::Items { dir_rev, total, last, items: page, .. } => {
                    if items.is_empty() {
                        items.reserve(total as usize);
                    }
                    items.extend(page);
                    if last {
                        return Ok(ListResult::Listing { dir_rev, items: Arc::new(items) });
                    }
                }
                other => return Err(unexpected(&other)),
            }
        }
    }

    /// Lists many directories in one request (for prefetch).
    pub async fn list_many(
        &self,
        dirs: Vec<(ItemId, Option<u64>)>,
        priority: Priority,
    ) -> Result<Vec<(ItemId, Result<ListResult>)>> {
        let reqs = dirs.into_iter().map(|(id, if_dir_rev)| ListDirReq { id, if_dir_rev }).collect();
        let mut resp = self.request(priority, RequestBody::ListMany(reqs)).await?;
        let mut partial: HashMap<ItemId, Vec<Item>> = HashMap::new();
        let mut out = Vec::new();
        loop {
            match resp.next().await? {
                Response::End => return Ok(out),
                Response::NotModified { dir_id, dir_rev } => {
                    out.push((dir_id, Ok(ListResult::NotModified { dir_rev })))
                }
                Response::Items { dir_id, dir_rev, last, items, .. } => {
                    let acc = partial.entry(dir_id.clone()).or_default();
                    acc.extend(items);
                    if last {
                        let items = partial.remove(&dir_id).unwrap_or_default();
                        out.push((dir_id, Ok(ListResult::Listing { dir_rev, items: Arc::new(items) })));
                    }
                }
                Response::ListError { dir_id, code, message } => {
                    out.push((dir_id, Err(Error::Server { code, message })))
                }
                other => return Err(unexpected(&other)),
            }
        }
    }

    pub async fn stat(&self, id: &str, priority: Priority) -> Result<Item> {
        let mut resp = self.request(priority, RequestBody::Stat { id: id.to_owned() }).await?;
        match resp.next().await? {
            Response::Item(i) => Ok(i),
            other => Err(unexpected(&other)),
        }
    }

    /// A symlink's target, relative to its directory and inside its share
    /// (`Server { code: OutsideShare }` otherwise).
    pub async fn read_link(&self, id: &str, priority: Priority) -> Result<String> {
        let mut resp = self.request(priority, RequestBody::ReadLink { id: id.to_owned() }).await?;
        match resp.next().await? {
            Response::LinkTarget { target } => Ok(target),
            other => Err(unexpected(&other)),
        }
    }

    /// Streams file content. Dropping the stream cancels the transfer.
    pub async fn read_stream(
        &self,
        id: &str,
        offset: u64,
        len: u64,
        compress: bool,
        priority: Priority,
    ) -> Result<ReadStream> {
        let body = RequestBody::Read { id: id.to_owned(), offset, len, compress };
        let resp = self.request(priority, body).await?;
        Ok(ReadStream { resp, started: Instant::now(), wire: 0, done: None })
    }

    /// A PNG thumbnail made on the server; empty when it has none for the file.
    pub async fn thumbnail(&self, id: &str, max_pixels: u32) -> Result<Bytes> {
        let body = RequestBody::Thumbnail { id: id.to_owned(), max_pixels };
        let resp = self.request(Priority::Thumbnail, body).await?;
        let mut s = ReadStream { resp, started: Instant::now(), wire: 0, done: None };
        let mut buf = BytesMut::new();
        while let Some((_, chunk)) = s.next_chunk().await? {
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    }

    /// Reads a byte range into memory. Identical concurrent calls share one request.
    pub async fn read_range(
        &self,
        id: &str,
        offset: u64,
        len: u64,
        compress: bool,
        priority: Priority,
    ) -> Result<Arc<ReadResult>> {
        let key: ReadKey = (id.to_owned(), offset, len, compress);
        let fut: SharedRead = {
            let mut flights = lock(&self.inner.read_flights);
            match flights.get(&key).and_then(WeakShared::upgrade) {
                Some(f) => f,
                None => {
                    let this = self.clone();
                    let id = id.to_owned();
                    let f = async move {
                        let mut s = this.read_stream(&id, offset, len, compress, priority).await?;
                        let mut buf = BytesMut::new();
                        while let Some((_, chunk)) = s.next_chunk().await? {
                            buf.extend_from_slice(&chunk);
                        }
                        let (_, size, content_rev) = s.end().unwrap_or_default();
                        Ok(Arc::new(ReadResult { data: buf.freeze(), size, content_rev, wire_bytes: s.wire_bytes() }))
                    }
                    .boxed()
                    .shared();
                    if let Some(w) = f.downgrade() {
                        flights.insert(key.clone(), w);
                    }
                    f
                }
            }
        };
        let r = fut.await;
        lock(&self.inner.read_flights).remove(&key);
        r
    }

    /// Change feed for the directories this client has listed.
    pub async fn changes(&self, since_seq: u64, index_id: u64, follow: bool) -> Result<ChangesStream> {
        let body = RequestBody::Changes { since_seq, index_id, follow };
        let resp = self.request(Priority::Visible, body).await?;
        Ok(ChangesStream { resp, follow })
    }

    /// Resolves `dev/foo/bar` (relative to the root container) by successive listings.
    pub async fn resolve_path(&self, path: &str, priority: Priority) -> Result<Item> {
        let mut cur = self.stat(proto::ROOT_ID, priority).await?;
        for comp in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            let ListResult::Listing { items, .. } = self.list_dir(&cur.id, None, priority).await? else {
                return Err(Error::Protocol("unconditional listing answered NotModified".into()));
            };
            let found = items
                .iter()
                .find(|i| i.name == comp)
                .or_else(|| items.iter().find(|i| i.name.eq_ignore_ascii_case(comp)));
            cur = found.cloned().ok_or_else(|| Error::Server {
                code: ErrorCode::NotFound,
                message: format!("{comp:?} not found in {}", cur.id),
            })?;
        }
        Ok(cur)
    }
}

impl Inner {
    fn live_conn(&self) -> Option<Connection> {
        let mut g = lock(&self.conn);
        match g.as_ref() {
            Some(c) if c.close_reason().is_none() => Some(c.clone()),
            Some(_) => {
                *g = None;
                lock(&self.link).on_connection_lost();
                None
            }
            None => None,
        }
    }

    async fn connection(self: &Arc<Self>, deadline: Duration) -> Result<Connection> {
        if let Some(c) = self.live_conn() {
            return Ok(c);
        }
        let _g = self.connecting.lock().await;
        if let Some(c) = self.live_conn() {
            return Ok(c);
        }
        let wait = deadline.min(self.cfg.connect_timeout);
        let connecting =
            self.endpoint.connect(self.cfg.server, proto::SERVER_NAME).map_err(|e| Error::Config(e.to_string()))?;
        let conn = tokio::time::timeout(wait, connecting)
            .await
            .map_err(|_| Error::Unreachable(format!("connect timed out after {} ms", wait.as_millis())))??;
        // Hello on the new connection.
        let (mut send, recv) = conn.open_bi().await?;
        send.set_priority(Priority::UserOpen.stream_priority()).ok();
        let hello = RequestBody::Hello { protocol: proto::PROTOCOL_VERSION, client_id: self.cfg.client_id.clone() };
        send.write_all(&proto::encode_frame(&Request { priority: Priority::UserOpen, body: hello })?).await?;
        send.finish().ok();
        let mut resp = Responses { recv, deadline: Some(wait), inner: self.clone() };
        match resp.next().await? {
            Response::Hello { index_id, seq, roots, .. } => {
                let prev = lock(&self.hello).replace(HelloInfo { index_id, seq, roots });
                if prev.is_some_and(|p| p.index_id != index_id) {
                    tracing::warn!(index_id, "server index changed; cached state must be re-enumerated");
                }
            }
            other => return Err(unexpected(&other)),
        }
        tracing::debug!(server = %self.cfg.server, "connected");
        *lock(&self.conn) = Some(conn.clone());
        Ok(conn)
    }

    async fn request(self: Arc<Self>, priority: Priority, body: RequestBody, probe: bool) -> Result<Responses> {
        if !probe {
            *lock(&self.last_activity) = Instant::now();
            if lock(&self.link).fail_fast() {
                return Err(Error::Unreachable("link is offline".into()));
            }
        }
        let deadline = if probe { self.cfg.ping_timeout } else { self.cfg.request_deadline };
        let frame = proto::encode_frame(&Request { priority, body })?;
        // A request never waits longer than its deadline, even to reconnect;
        // probes may take the full connect timeout.
        let budget = if probe { self.cfg.connect_timeout } else { deadline };
        let open = async {
            let conn = self.connection(budget).await?;
            let (mut send, recv) = conn.open_bi().await?;
            send.set_priority(priority.stream_priority()).ok();
            send.write_chunk(frame).await?;
            send.finish().ok();
            Ok::<_, Error>(recv)
        };
        let recv = match tokio::time::timeout(budget, open).await {
            Ok(r) => r?,
            Err(_) => {
                self.probe_now.notify_one();
                return Err(Error::Unreachable(format!("no connection within {} ms", budget.as_millis())));
            }
        };
        Ok(Responses { recv, deadline: Some(deadline), inner: self })
    }

    async fn probe(self: &Arc<Self>) -> Result<Duration> {
        let started = Instant::now();
        let nonce = rand_nonce();
        let result = async {
            let mut resp = self.clone().request(Priority::UserOpen, RequestBody::Ping { nonce }, true).await?;
            match resp.next().await? {
                Response::Pong { nonce: n } if n == nonce => Ok(started.elapsed()),
                other => Err(unexpected(&other)),
            }
        };
        let result = tokio::time::timeout(self.cfg.ping_timeout + self.cfg.connect_timeout, result)
            .await
            .unwrap_or_else(|_| Err(Error::Unreachable("ping timed out".into())));
        let conn = self.live_conn();
        let mut link = lock(&self.link);
        match (&result, conn) {
            (Ok(rtt), Some(c)) => {
                let s = c.stats();
                link.on_probe_ok(Instant::now(), *rtt, Some(c.rtt()), (s.path.sent_packets, s.path.lost_packets));
            }
            (Ok(rtt), None) => link.on_probe_ok(Instant::now(), *rtt, None, (0, 0)),
            (Err(_), _) => link.on_probe_miss(),
        }
        result
    }
}

fn rand_nonce() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(Instant::now())
}

async fn ping_loop(inner: Weak<Inner>, probe_now: Arc<Notify>) {
    let mut next = Duration::ZERO;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(next) => {}
            _ = probe_now.notified() => {}
        }
        let Some(i) = inner.upgrade() else { return };
        let ok = i.probe().await.is_ok();
        let state = lock(&i.link).state();
        let idle = lock(&i.last_activity).elapsed() > Duration::from_secs(60);
        next = if !ok || state == LinkState::Offline {
            Duration::from_secs(2)
        } else if idle {
            i.cfg.idle_ping_interval
        } else {
            i.cfg.ping_interval
        };
    }
}
