//! homeportd: serves basebook's shares to Homeport clients over the tailnet.

mod auth;
mod congestion;
mod db;
mod fsevents;
mod fsops;
mod index;
mod server;
mod thumbs;
mod tls;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;

const DEFAULT_TAILSCALE: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address to bind. Defaults to this node's tailnet IPv4 (`tailscale ip -4`).
    #[arg(long)]
    bind: Option<IpAddr>,
    #[arg(long, default_value_t = homeport_proto::DEFAULT_PORT)]
    port: u16,
    /// State directory (certificate and index). Defaults to
    /// ~/Library/Application Support/Homeport/server.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Share as NAME=PATH; repeatable. Defaults to dev=~/dev and Downloads=~/Downloads.
    #[arg(long = "share", value_parser = parse_share)]
    shares: Vec<(String, PathBuf)>,
    /// Tailscale CLI.
    #[arg(long, env = "HOMEPORT_TAILSCALE", default_value = DEFAULT_TAILSCALE)]
    tailscale: PathBuf,
    /// Directory listings kept in memory.
    #[arg(long, default_value_t = 4096)]
    cache_dirs: usize,
}

fn parse_share(s: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = s.split_once('=').ok_or("expected NAME=PATH")?;
    if name.is_empty() || name.contains('/') || name.contains(':') {
        return Err("share name must be non-empty without '/' or ':'".into());
    }
    Ok((name.to_owned(), PathBuf::from(path)))
}

#[derive(Debug, thiserror::Error)]
enum MainError {
    #[error("HOME is not set")]
    NoHome,
    #[error(transparent)]
    Tls(#[from] tls::TlsError),
    #[error("tailscale: {0}")]
    Auth(#[from] auth::AuthError),
    #[error(transparent)]
    Db(#[from] db::DbError),
    #[error(transparent)]
    Index(#[from] index::IndexError),
    #[error(transparent)]
    Server(#[from] server::ServerError),
    #[error(transparent)]
    FsEvents(#[from] fsevents::FsEventsError),
    #[error("signal handler: {0}")]
    Signal(std::io::Error),
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    if let Err(e) = run(Args::parse()).await {
        tracing::error!(error = %e, "homeportd failed");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), MainError> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or(MainError::NoHome)?);
    let data_dir = args.data_dir.unwrap_or_else(|| home.join("Library/Application Support/Homeport/server"));
    let shares = if args.shares.is_empty() {
        vec![("dev".to_owned(), home.join("dev")), ("Downloads".to_owned(), home.join("Downloads"))]
    } else {
        args.shares
    };

    let identity = tls::load_or_create(&data_dir)?;
    println!("homeportd certificate SHA-256: {}", identity.fingerprint);
    tracing::info!(fingerprint = %identity.fingerprint, path = %data_dir.join("cert.sha256").display(), "server identity");

    let ts = auth::Tailscale { bin: args.tailscale };
    let tailnet_ip = ts.ipv4().await;
    let bind_ip = match (args.bind, &tailnet_ip) {
        (Some(ip), _) => ip,
        (None, Ok(ip)) => *ip,
        (None, Err(e)) => {
            tracing::error!(error = %e, "cannot find the tailnet IP; pass --bind");
            return Err(MainError::Auth(auth::AuthError::Missing("tailnet IP")));
        }
    };
    let self_user = match ts.self_user().await {
        Ok(u) => u,
        Err(e) if bind_ip.is_loopback() => {
            tracing::warn!(error = %e, "tailscale unavailable; only loopback peers are allowed");
            0
        }
        Err(e) => return Err(e.into()),
    };
    let mut self_ips = vec![bind_ip];
    if let Ok(ip) = tailnet_ip {
        self_ips.push(ip);
    }
    let authz = Arc::new(auth::Authorizer::new(ts, self_user, self_ips));

    std::fs::create_dir_all(&data_dir)
        .map_err(|source| tls::TlsError::Io { path: data_dir.display().to_string(), source })?;
    let (db, loaded) = db::Db::open(&data_dir.join("index.sqlite"))?;
    let thumbs_dir = home.join("Library/Caches/Homeport/thumbnails");
    if let Err(e) = thumbs::init(thumbs_dir) {
        tracing::warn!(error = %e, "thumbnail cache unavailable");
    }
    let stored_event = loaded.last_event_id;
    let stored_uuid = loaded.volume_uuid.clone();
    let writes = index::WriteDirs::prepare(data_dir.join("staging"), home.join(".Trash"))
        .map_err(|source| tls::TlsError::Io { path: data_dir.join("staging").display().to_string(), source })?;
    let index = Arc::new(index::Index::new(shares, db.clone(), loaded, args.cache_dirs, writes)?);
    for s in index.shares() {
        tracing::info!(share = %s.name, path = %s.path.display(), "share");
    }

    // FSEvents, resumed from the stored event ID when the volume is the same.
    let uuid = fsevents::volume_uuid(index.volume().dev);
    let since = match (&uuid, &stored_uuid, stored_event) {
        (Some(u), Some(s), Some(id)) if u == s => Some(id),
        _ => None,
    };
    if since.is_none() {
        if let Some(u) = &uuid {
            db.send(db::Op::VolumeUuid(u.clone()))?;
        }
        db.send(db::Op::EventId(fsevents::current_event_id()))?;
    }
    // A live stream from now, plus (when resuming) a history stream that
    // replays what happened while we were down. They are separate because
    // FSEvents holds live events back until a replay finishes, which can
    // take seconds.
    let paths: Vec<_> = index.shares().iter().map(|s| s.path.as_path()).collect();
    let (ev_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let watcher = fsevents::watch(&paths, None, 0.1, 0, ev_tx.clone())?;
    let mut history = match since {
        Some(id) => Some(fsevents::watch(&paths, Some(id), 0.1, 1, ev_tx)?),
        None => None,
    };
    tracing::info!(resume_from = ?since, volume = ?uuid, "watching FSEvents");
    let ev_index = index.clone();
    let ev_db = db.clone();
    let ev_task = tokio::spawn(async move {
        while let Some((tag, mut batch)) = events.recv().await {
            let mut history_done = tag == 1 && batch.iter().any(|e| e.1 & fsevents::flags::HISTORY_DONE != 0);
            while let Ok((t, more)) = events.try_recv() {
                history_done |= t == 1 && more.iter().any(|e| e.1 & fsevents::flags::HISTORY_DONE != 0);
                batch.extend(more);
            }
            if history_done && history.take().is_some() {
                tracing::info!("FSEvents history replay done");
            }
            let max_id = batch.iter().map(|e| e.2).max().unwrap_or(0);
            if batch.iter().any(|e| e.1 & fsevents::flags::EVENT_IDS_WRAPPED != 0) {
                tracing::warn!("FSEvents event IDs wrapped");
            }
            if batch.iter().any(|e| e.1 & fsevents::flags::ROOT_CHANGED != 0) {
                tracing::warn!("a share root moved or was deleted");
            }
            let evs: Vec<_> =
                batch.into_iter().filter(|e| e.1 & fsevents::flags::HISTORY_DONE == 0).map(|e| (e.0, e.1)).collect();
            let started = std::time::Instant::now();
            match ev_index.apply_events(&evs).await {
                Ok(n) if n > 0 => tracing::debug!(
                    events = evs.len(),
                    dirs = n,
                    ms = started.elapsed().as_millis() as u64,
                    "applied FSEvents"
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "applying FSEvents failed"),
            }
            // Only advance the stored ID from the live stream once history is done,
            // so a crash mid-replay replays again.
            if max_id > 0 && history.is_none() {
                let _ = ev_db.send(db::Op::EventId(max_id));
            }
        }
    });

    let addr = SocketAddr::new(bind_ip, args.port);
    let endpoint = server::endpoint(&identity, addr)?;
    tracing::info!(%addr, "listening");
    let serve = tokio::spawn(server::serve(endpoint.clone(), index.clone(), authz));

    let mut term =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(MainError::Signal)?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    endpoint.close(quinn::VarInt::from_u32(0), b"server shutting down");
    let _ = tokio::time::timeout(Duration::from_secs(2), endpoint.wait_idle()).await;
    serve.abort();
    drop(watcher);
    ev_task.abort();
    let _ = db.flush().await;
    db.close();
    Ok(())
}
