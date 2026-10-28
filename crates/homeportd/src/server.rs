//! QUIC endpoint and RPC handlers.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use homeport_proto::{self as proto, ErrorCode, ListDirReq, Request, RequestBody, Response};
use quinn::{RecvStream, SendStream};
use tokio::task::JoinSet;

use crate::auth::Authorizer;
use crate::index::{CreateReq, Index, IndexError, ListOutcome, Staged};
use crate::tls::Identity;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// An upload may stall this long between chunks before it is abandoned.
const UPLOAD_IDLE: Duration = Duration::from_secs(60);
const CHANGE_BATCH: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("quic crypto config: {0}")]
    Crypto(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("bind {addr}: {source}")]
    Bind { addr: SocketAddr, source: std::io::Error },
}

#[derive(Debug, thiserror::Error)]
enum StreamError {
    #[error("read request: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("read upload: {0}")]
    ReadBody(#[from] quinn::ReadError),
    #[error("write: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("stream closed: {0}")]
    Closed(#[from] quinn::ClosedStream),
    #[error("request timed out")]
    Timeout,
    #[error("frame: {0}")]
    Frame(#[from] proto::FrameError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("task: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Congestion controller for what the server sends: `HOMEPORT_CC` = `bbr`
/// (default) | `cubic` | `newreno`. Under random loss (Wi-Fi, cellular)
/// Cubic backs off on every lost packet and fills the bottleneck queue;
/// BBR paces at the measured delivery rate.
fn congestion() -> Arc<dyn quinn::congestion::ControllerFactory + Send + Sync> {
    match std::env::var("HOMEPORT_CC").unwrap_or_default().to_ascii_lowercase().as_str() {
        "cubic" => Arc::new(quinn::congestion::CubicConfig::default()),
        "newreno" | "reno" => Arc::new(quinn::congestion::NewRenoConfig::default()),
        "" | "bbr" => Arc::new(quinn::congestion::BbrConfig::default()),
        other => {
            tracing::warn!(cc = other, "unknown HOMEPORT_CC; using bbr");
            Arc::new(quinn::congestion::BbrConfig::default())
        }
    }
}

pub fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    // HOMEPORT_URGENT_CAP=0 turns the in-flight cap off (for comparisons).
    if std::env::var("HOMEPORT_URGENT_CAP").is_ok_and(|v| v == "0") {
        t.congestion_controller_factory(congestion());
    } else {
        t.congestion_controller_factory(Arc::new(crate::congestion::CappedFactory { inner: congestion() }));
    }
    t.initial_mtu(1200);
    let mut mtu = quinn::MtuDiscoveryConfig::default();
    mtu.upper_bound(1232);
    t.mtu_discovery_config(Some(mtu));
    t.keep_alive_interval(Some(Duration::from_secs(5)));
    t.max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(30_000))));
    t.max_concurrent_bidi_streams(quinn::VarInt::from_u32(512));
    t.max_concurrent_uni_streams(quinn::VarInt::from_u32(0));
    t.stream_receive_window(quinn::VarInt::from_u32(8 * 1024 * 1024));
    t.receive_window(quinn::VarInt::from_u32(32 * 1024 * 1024));
    t.send_window(32 * 1024 * 1024);
    // Windows UDP segmentation offload (USO) over Tailscale's Wintun adapter
    // breaks bulk reads: the macOS client aborts large transfers with "too
    // many gaps in stream buffer". Without it exon-desk sends 130 MiB/s.
    #[cfg(windows)]
    t.enable_segmentation_offload(false);
    t
}

pub fn endpoint(identity: &Identity, bind: SocketAddr) -> Result<quinn::Endpoint, ServerError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    tls.alpn_protocols = vec![proto::ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    sc.transport_config(Arc::new(transport_config()));
    quinn::Endpoint::server(sc, bind).map_err(|source| ServerError::Bind { addr: bind, source })
}

struct Conn {
    conn: quinn::Connection,
    urgency: Option<Arc<crate::congestion::Urgency>>,
    index: Arc<Index>,
    info: Arc<proto::ServerInfo>,
    client_id: OnceLock<String>,
    peer: SocketAddr,
}

pub async fn serve(endpoint: quinn::Endpoint, index: Arc<Index>, auth: Arc<Authorizer>, info: Arc<proto::ServerInfo>) {
    while let Some(incoming) = endpoint.accept().await {
        let index = index.clone();
        let auth = auth.clone();
        let info = info.clone();
        tokio::spawn(async move {
            let peer = incoming.remote_address();
            if !auth.allow(peer.ip()).await {
                tracing::warn!(%peer, "refused unauthorized peer");
                incoming.refuse();
                return;
            }
            let conn = match incoming.accept() {
                Ok(c) => c.await,
                Err(e) => {
                    tracing::warn!(%peer, error = %e, "accept failed");
                    return;
                }
            };
            match conn {
                Ok(conn) => {
                    let urgency = crate::congestion::urgency(&conn);
                    let ctx =
                        Arc::new(Conn { conn: conn.clone(), urgency, index, info, client_id: OnceLock::new(), peer });
                    handle_conn(conn, ctx).await
                }
                Err(e) => tracing::debug!(%peer, error = %e, "handshake failed"),
            }
        });
    }
}

async fn handle_conn(conn: quinn::Connection, ctx: Arc<Conn>) {
    tracing::info!(peer = %ctx.peer, "connected");
    loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_stream(&ctx, send, recv).await {
                        tracing::debug!(peer = %ctx.peer, error = %e, "stream ended with error");
                    }
                });
            }
            Err(e) => {
                tracing::info!(peer = %ctx.peer, client = ?ctx.client_id.get(), reason = %e, "disconnected");
                return;
            }
        }
    }
}

fn error_frame(code: ErrorCode, message: impl Into<String>) -> Result<Bytes, proto::FrameError> {
    proto::encode_frame(&Response::Error { code, message: message.into() })
}

async fn handle_stream(ctx: &Conn, mut send: SendStream, mut recv: RecvStream) -> Result<(), StreamError> {
    // One request frame; uploads follow it with their content.
    let header = async {
        let mut prefix = [0u8; 4];
        recv.read_exact(&mut prefix).await?;
        let len = u32::from_be_bytes(prefix) as usize;
        if len > proto::MAX_REQUEST {
            return Ok(None);
        }
        let mut buf = vec![0u8; len];
        recv.read_exact(&mut buf).await?;
        Ok::<_, StreamError>(Some(buf))
    };
    let raw = tokio::time::timeout(REQUEST_TIMEOUT, header).await.map_err(|_| StreamError::Timeout)??;
    let req: Request = match raw.map(|b| proto::decode(&b)) {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            send.write_chunk(error_frame(ErrorCode::BadRequest, e.to_string())?).await?;
            return Ok(send.finish()?);
        }
        None => {
            send.write_chunk(error_frame(ErrorCode::BadRequest, "request too large")?).await?;
            return Ok(send.finish()?);
        }
    };
    send.set_priority(req.priority.stream_priority())?;
    let _urgent = ctx.urgency.as_ref().filter(|_| is_urgent(&req)).map(|u| u.begin());
    let started = Instant::now();
    let kind = request_kind(&req.body);
    let result = dispatch(ctx, &mut send, &mut recv, req.body).await;
    match result {
        Ok(()) => {}
        Err(StreamError::Index(IndexError::Collision(existing))) => {
            write(&mut send, &Response::Collision { existing: *existing }).await?;
        }
        Err(StreamError::Index(e)) => {
            if !matches!(e, IndexError::NotFound) {
                tracing::info!(kind, error = %e, "request failed");
            }
            send.write_chunk(error_frame(e.code(), e.to_string())?).await?;
        }
        Err(e) => return Err(e),
    }
    send.finish()?;
    let us = started.elapsed().as_micros() as u64;
    if us > 500_000 {
        tracing::info!(kind, ms = us / 1000, "slow request");
    } else {
        tracing::debug!(kind, us, "request");
    }
    Ok(())
}

/// Whether someone is waiting on this request right now: user opens and
/// visible listings, except whole-file or large reads (those are the bulk
/// transfers the in-flight cap holds back) and probes.
fn is_urgent(req: &Request) -> bool {
    use proto::Priority;
    if !matches!(req.priority, Priority::UserOpen | Priority::Visible) {
        return false;
    }
    match &req.body {
        RequestBody::Ping { .. }
        | RequestBody::Hello { .. }
        | RequestBody::Changes { .. }
        | RequestBody::Create { .. }
        | RequestBody::WriteContent { .. } => false,
        RequestBody::Read { len, .. } => (1..=1 << 20).contains(len),
        _ => true,
    }
}

fn request_kind(b: &RequestBody) -> &'static str {
    match b {
        RequestBody::Hello { .. } => "hello",
        RequestBody::ListDir(_) => "list",
        RequestBody::ListMany(_) => "list_many",
        RequestBody::Stat { .. } => "stat",
        RequestBody::Read { .. } => "read",
        RequestBody::Changes { .. } => "changes",
        RequestBody::Ping { .. } => "ping",
        RequestBody::ReadLink { .. } => "readlink",
        RequestBody::Thumbnail { .. } => "thumbnail",
        RequestBody::Create { .. } => "create",
        RequestBody::WriteContent { .. } => "write",
        RequestBody::Modify { .. } => "modify",
        RequestBody::Trash { .. } => "trash",
        RequestBody::Untrash { .. } => "untrash",
        RequestBody::Forget { .. } => "forget",
    }
}

/// Receives an upload's `size` bytes into a staging file.
async fn receive(recv: &mut RecvStream, dir: &std::path::Path, size: u64) -> Result<Staged, StreamError> {
    use std::io::Write;
    let staged = Staged::new(dir);
    let mut file =
        std::fs::OpenOptions::new().write(true).create_new(true).open(staged.path()).map_err(IndexError::from_io)?;
    let mut got: u64 = 0;
    loop {
        let chunk = tokio::time::timeout(UPLOAD_IDLE, recv.read_chunk(1 << 20, true))
            .await
            .map_err(|_| StreamError::Timeout)??;
        let Some(chunk) = chunk else { break };
        got += chunk.bytes.len() as u64;
        if got > size {
            return Err(IndexError::Incomplete(format!("more than the announced {size} bytes")).into());
        }
        file.write_all(&chunk.bytes).map_err(IndexError::from_io)?;
    }
    if got != size {
        return Err(IndexError::Incomplete(format!("{got} of {size} bytes")).into());
    }
    Ok(staged)
}

async fn write(send: &mut SendStream, r: &Response) -> Result<(), StreamError> {
    send.write_chunk(proto::encode_frame(r)?).await?;
    Ok(())
}

async fn write_outcome(send: &mut SendStream, dir_id: &str, o: ListOutcome) -> Result<(), StreamError> {
    match o {
        ListOutcome::NotModified { dir_rev } => {
            write(send, &Response::NotModified { dir_id: dir_id.to_owned(), dir_rev }).await
        }
        ListOutcome::Listing(l) => {
            for f in l.frames()? {
                send.write_chunk(f.clone()).await?;
            }
            Ok(())
        }
    }
}

async fn dispatch(
    ctx: &Conn,
    send: &mut SendStream,
    recv: &mut RecvStream,
    body: RequestBody,
) -> Result<(), StreamError> {
    let index = &ctx.index;
    let client = ctx.client_id.get().map(String::as_str);
    match body {
        RequestBody::Create { op_id, parent_id, name, kind, size, link_target, meta, may_exist } => {
            if let Some(id) = index.op_replay(&op_id)
                && let Ok(item) = index.stat(&id).await
            {
                let _ = recv.stop(quinn::VarInt::from_u32(0));
                return write(send, &Response::Item(item)).await;
            }
            let staged = match kind {
                proto::Kind::File => Some(receive(recv, &index.staging_for(&parent_id), size).await?),
                _ => None,
            };
            let req = CreateReq { op_id, parent_id, name, kind, link_target, meta, may_exist };
            let item = index.create(req, staged, client).await?;
            write(send, &Response::Item(item)).await
        }
        RequestBody::WriteContent { id, base_content_rev, size, meta, device } => {
            let staged = receive(recv, &index.staging_for(&id), size).await?;
            let (item, conflict) = index.write_content(&id, base_content_rev, staged, meta, &device).await?;
            write(send, &Response::Written { item, conflict }).await
        }
        RequestBody::Modify { id, new_parent_id, new_name, meta } => {
            let item = index.modify(&id, new_parent_id.as_deref(), new_name.as_deref(), meta).await?;
            write(send, &Response::Item(item)).await
        }
        RequestBody::Trash { id, listed, recursive } => {
            let item = index.trash(&id, listed, recursive).await?;
            write(send, &Response::Item(item)).await
        }
        RequestBody::Untrash { id, parent_id, name } => {
            let item = index.untrash(&id, &parent_id, &name).await?;
            write(send, &Response::Item(item)).await
        }
        RequestBody::Forget { id } => {
            index.forget(&id).await?;
            write(send, &Response::End).await
        }
        RequestBody::Hello { protocol, client_id } => {
            if protocol != proto::PROTOCOL_VERSION {
                let msg = format!("server speaks protocol {}, client {protocol}", proto::PROTOCOL_VERSION);
                send.write_chunk(error_frame(ErrorCode::VersionMismatch, msg)?).await?;
                return Ok(());
            }
            let _ = ctx.client_id.set(client_id.clone());
            tracing::info!(peer = %ctx.peer, client = client_id, "hello");
            write(
                send,
                &Response::Hello {
                    protocol: proto::PROTOCOL_VERSION,
                    index_id: index.index_id,
                    seq: index.committed_seq(),
                    roots: index.roots(),
                    server: (*ctx.info).clone(),
                },
            )
            .await
        }
        RequestBody::ListDir(ListDirReq { id, if_dir_rev }) => {
            let o = index.list(&id, if_dir_rev, client).await?;
            write_outcome(send, &id, o).await
        }
        RequestBody::ListMany(reqs) => {
            let mut set = JoinSet::new();
            for r in reqs {
                let index = index.clone();
                let client = client.map(str::to_owned);
                set.spawn(async move {
                    let o = index.list(&r.id, r.if_dir_rev, client.as_deref()).await;
                    (r.id, o)
                });
            }
            while let Some(res) = set.join_next().await {
                let (id, o) = res?;
                match o {
                    Ok(o) => write_outcome(send, &id, o).await?,
                    Err(e) => {
                        let r = Response::ListError { dir_id: id, code: e.code(), message: e.to_string() };
                        write(send, &r).await?;
                    }
                }
            }
            write(send, &Response::End).await
        }
        RequestBody::Stat { id } => {
            let item = index.stat(&id).await?;
            write(send, &Response::Item(item)).await
        }
        RequestBody::Read { id, offset, len, compress } => {
            read(index, send, id, offset, len, compress, chunk_size(&ctx.conn)).await
        }
        RequestBody::Changes { since_seq, index_id, follow } => {
            changes(index, send, client, since_seq, index_id, follow).await
        }
        RequestBody::Ping { nonce } => write(send, &Response::Pong { nonce }).await,
        // A PNG as `Data` frames then `DataEnd`; no data means no thumbnail.
        RequestBody::Thumbnail { id, max_pixels } => {
            let png = crate::thumbs::get(index, &id, max_pixels).await?.unwrap_or_default();
            for (i, chunk) in png.chunks(proto::READ_CHUNK).enumerate() {
                let offset = (i * proto::READ_CHUNK) as u64;
                let data =
                    Response::Data { offset, compressed: false, raw_len: chunk.len() as u32, bytes: chunk.to_vec() };
                write(send, &data).await?;
            }
            let total = png.len() as u64;
            write(send, &Response::DataEnd { total, size: total, content_rev: 0 }).await
        }
        RequestBody::ReadLink { id } => {
            let idx = index.clone();
            let target = tokio::task::spawn_blocking(move || idx.read_link(&id)).await??;
            write(send, &Response::LinkTarget { target }).await
        }
    }
}

/// `Data` chunk size for the connection: about 100 ms of what it currently
/// delivers (congestion window per RTT), 16 KiB to `READ_CHUNK`. Slow links
/// get small frames, so progress and cancellation stay responsive.
fn chunk_size(conn: &quinn::Connection) -> usize {
    let p = conn.stats().path;
    let rtt = p.rtt.as_secs_f64().max(0.001);
    let per_100ms = (p.cwnd as f64 / rtt * 0.1) as usize;
    per_100ms.clamp(16 * 1024, proto::READ_CHUNK).next_power_of_two().min(proto::READ_CHUNK)
}

async fn read(
    index: &Arc<Index>,
    send: &mut SendStream,
    id: String,
    offset: u64,
    len: u64,
    compress: bool,
    chunk: usize,
) -> Result<(), StreamError> {
    let idx = index.clone();
    let (fd, size, name, content_rev) = tokio::task::spawn_blocking(move || idx.open_file(&id)).await??;
    let end = if len == 0 { size } else { size.min(offset.saturating_add(len)) };
    let mut compress = compress && !proto::is_precompressed(&name);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Bytes, StreamError>>(8);
    let producer = tokio::task::spawn_blocking(move || {
        use crate::platform::FileOps;
        let mut buf = vec![0u8; chunk];
        let mut pos = offset;
        let mut incompressible = 0;
        while pos < end {
            let want = (end - pos).min(chunk as u64) as usize;
            let n = match fd.read_at(&mut buf[..want], pos) {
                Ok(n) => n,
                Err(e) => {
                    let _ = tx.blocking_send(Err(IndexError::from_io(e).into()));
                    return pos - offset;
                }
            };
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            let mut frame = None;
            if compress {
                match zstd::bulk::compress(chunk, 1) {
                    Ok(z) if z.len() < chunk.len() - chunk.len() / 8 => {
                        frame = Some(Response::Data { offset: pos, compressed: true, raw_len: n as u32, bytes: z });
                    }
                    _ => {
                        incompressible += 1;
                        compress = incompressible < 2;
                    }
                }
            }
            let frame = frame.unwrap_or_else(|| Response::Data {
                offset: pos,
                compressed: false,
                raw_len: n as u32,
                bytes: chunk.to_vec(),
            });
            let encoded = proto::encode_frame(&frame).map_err(StreamError::from);
            if tx.blocking_send(encoded).is_err() {
                return pos - offset;
            }
            pos += n as u64;
        }
        pos - offset
    });
    while let Some(frame) = rx.recv().await {
        send.write_chunk(frame?).await?;
    }
    let total = producer.await?;
    write(send, &Response::DataEnd { total, size, content_rev }).await
}

async fn changes(
    index: &Arc<Index>,
    send: &mut SendStream,
    client: Option<&str>,
    since: u64,
    index_id: u64,
    follow: bool,
) -> Result<(), StreamError> {
    let mut committed = index.db.committed.clone();
    let current = *committed.borrow_and_update();
    if index_id != index.index_id || since > current || since < index.tombstone_floor {
        return write(send, &Response::SyncAnchorExpired { index_id: index.index_id, seq: current }).await;
    }
    let mut since = since;
    let mut first = true;
    loop {
        let upto = *committed.borrow_and_update();
        if upto > since || first {
            let idx = index.clone();
            let (changed, deleted) =
                tokio::task::spawn_blocking(move || idx.db.changes(since, upto)).await?.map_err(IndexError::from)?;
            let (changed, deleted) = match client {
                Some(c) => index.filter_for_client(c, changed, deleted)?,
                None => (Vec::new(), Vec::new()),
            };
            if first || !changed.is_empty() || !deleted.is_empty() {
                send_batches(send, since, upto, changed, deleted).await?;
            }
            since = upto;
            first = false;
        }
        if !follow {
            return Ok(());
        }
        tokio::select! {
            r = committed.changed() => if r.is_err() { return Ok(()) },
            _ = send.stopped() => return Ok(()),
        }
    }
}

async fn send_batches(
    send: &mut SendStream,
    since: u64,
    upto: u64,
    changed: Vec<proto::Item>,
    deleted: Vec<proto::Deleted>,
) -> Result<(), StreamError> {
    let mut changed = changed.into_iter().peekable();
    let mut deleted = deleted.into_iter().peekable();
    loop {
        let c: Vec<_> = changed.by_ref().take(CHANGE_BATCH).collect();
        let d: Vec<_> = deleted.by_ref().take(CHANGE_BATCH).collect();
        let last = changed.peek().is_none() && deleted.peek().is_none();
        // Intermediate batches keep the old anchor so a client that stops
        // mid-way never skips changes.
        let seq = if last { upto } else { since };
        write(send, &Response::ChangeBatch { seq, changed: c, deleted: d, caught_up: last }).await?;
        if last {
            return Ok(());
        }
    }
}
