//! homeportd: serves this machine's shares to Homeport clients over the tailnet.

// Linux and Windows are stubs for now: much of the shared code is unused there.
#![cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]

mod auth;
mod config;
mod congestion;
mod db;
mod index;
mod names;
mod platform;
mod server;
mod thumbs;
mod tls;

use std::ffi::OsString;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use homeport_proto as proto;
use tracing_subscriber::EnvFilter;

use config::{Config, ShareConfig};
use platform::{FsOps, IdScheme, Service, ServiceOps, Sys, SysOps, Trash, TrashOps, Watcher, WatcherOps};

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Config file. Defaults to `server.toml` in the platform's config
    /// directory (macOS: ~/Library/Application Support/Homeport).
    #[arg(long, env = "HOMEPORT_CONFIG", global = true)]
    config: Option<PathBuf>,
    #[command(flatten)]
    run: RunArgs,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Address to bind. Defaults to this node's tailnet IPv4 (`tailscale ip -4`).
    #[arg(long)]
    bind: Option<IpAddr>,
    /// UDP port (default 47820).
    #[arg(long)]
    port: Option<u16>,
    /// State directory (certificate and index). Defaults to the platform's
    /// (macOS: ~/Library/Application Support/Homeport/server).
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Share as NAME=PATH; repeatable. Replaces the config file's shares.
    #[arg(long = "share", value_parser = parse_share)]
    shares: Vec<(String, PathBuf)>,
    /// Tailscale login allowed to connect (repeatable; adds to the config's
    /// `allow_logins`). Default: the login owning this node.
    #[arg(long = "allow-login")]
    allow_logins: Vec<String>,
    /// Server name shown to clients (default: the host name).
    #[arg(long)]
    name: Option<String>,
    /// Tailscale CLI. Defaults to the platform's usual location.
    #[arg(long, env = "HOMEPORT_TAILSCALE")]
    tailscale: Option<PathBuf>,
    /// Directory listings kept in memory.
    #[arg(long, default_value_t = 4096)]
    cache_dirs: usize,
    /// Testing: index-assigned item ids resolved by walking the index (the
    /// Linux/Windows code path) instead of the platform's own ids.
    #[arg(long, hide = true)]
    test_assigned_ids: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Manage shared folders in the config file.
    Share {
        #[command(subcommand)]
        cmd: ShareCmd,
    },
    /// Run homeportd at login (macOS LaunchAgent).
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Print a pairing link and code for adding this server to a Mac.
    Pair(PairArgs),
    /// Show this server's name, address, fingerprint, shares and allowed logins.
    Info(PairArgs),
}

#[derive(clap::Args, Debug)]
struct PairArgs {
    /// Address clients use (`ip:port`). Default: this node's tailnet IPv4
    /// and the configured port.
    #[arg(long)]
    addr: Option<SocketAddr>,
}

#[derive(Subcommand, Debug)]
enum ShareCmd {
    /// Share a folder.
    Add {
        path: PathBuf,
        /// Name clients see (default: the folder's name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Stop sharing a folder.
    Remove { name: String },
    /// Show the shares homeportd will serve.
    List,
}

#[derive(Subcommand, Debug)]
enum ServiceCmd {
    /// Install and start the service, running this binary.
    Install {
        /// Only print what would be installed.
        #[arg(long)]
        print: bool,
        /// Extra arguments for homeportd (after `--`).
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Stop and remove the service.
    Uninstall,
    /// Show whether the service is installed and running.
    Status,
}

fn parse_share(s: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = s.split_once('=').ok_or("expected NAME=PATH")?;
    config::check_share_name(name)?;
    Ok((name.to_owned(), PathBuf::from(path)))
}

#[derive(Debug, thiserror::Error)]
enum MainError {
    #[error("cannot find the home directory")]
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
    Config(#[from] config::ConfigError),
    #[error("{0}")]
    Usage(String),
    #[error("{what}: {source}")]
    Io { what: String, source: std::io::Error },
}

fn io_err(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> MainError {
    let what = what.into();
    move |source| MainError::Io { what, source }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let args = Args::parse();
    let result = match args.cmd {
        None => run(args.config, args.run).await.inspect_err(|e| tracing::error!(error = %e, "homeportd failed")),
        Some(Cmd::Share { cmd }) => share_cmd(args.config, cmd),
        Some(Cmd::Service { cmd }) => service_cmd(args.config, cmd),
        Some(Cmd::Pair(a)) => pair_cmd(args.config, a, false).await,
        Some(Cmd::Info(a)) => pair_cmd(args.config, a, true).await,
    };
    if let Err(e) = result {
        eprintln!("homeportd: {e}");
        std::process::exit(1);
    }
}

fn home() -> Result<PathBuf, MainError> {
    Sys::home().ok_or(MainError::NoHome)
}

fn config_path(given: Option<PathBuf>, home: &Path) -> PathBuf {
    given.unwrap_or_else(|| Sys::config_dir(home).join("server.toml"))
}

/// Shares from the config, else the platform defaults (`true` if defaults).
fn config_shares(cfg: &Config, home: &Path) -> (Vec<(String, PathBuf)>, bool) {
    match &cfg.shares {
        Some(s) => (s.iter().map(|s| (s.name.clone(), config::expand(&s.path, home))).collect(), false),
        None => (Sys::default_shares(home), true),
    }
}

fn share_cmd(config: Option<PathBuf>, cmd: ShareCmd) -> Result<(), MainError> {
    let home = home()?;
    let path = config_path(config, &home);
    let mut cfg = Config::load(&path)?;
    let (current, defaults) = config_shares(&cfg, &home);
    match cmd {
        ShareCmd::List => {
            println!("config: {}{}", path.display(), if defaults { " (no shares set: platform defaults)" } else { "" });
            if current.is_empty() {
                println!("no shares; add one with `homeportd share add <path>`");
            }
            for (name, p) in current {
                println!("{name:<16} {}", p.display());
            }
            return Ok(());
        }
        ShareCmd::Add { path: dir, name } => {
            let dir = std::fs::canonicalize(&dir).map_err(io_err(format!("share {}", dir.display())))?;
            if !dir.is_dir() {
                return Err(MainError::Usage(format!("{} is not a folder", dir.display())));
            }
            let name = match name {
                Some(n) => n,
                None => dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into()),
            };
            config::check_share_name(&name).map_err(MainError::Usage)?;
            if current.iter().any(|(n, _)| *n == name) {
                return Err(MainError::Usage(format!("a share named {name:?} exists; pass --name")));
            }
            if let Some((n, _)) = current.iter().find(|(_, p)| *p == dir) {
                return Err(MainError::Usage(format!("{} is already shared as {n:?}", dir.display())));
            }
            let mut shares = current;
            shares.push((name.clone(), dir.clone()));
            cfg.shares = Some(shares.into_iter().map(|(name, path)| ShareConfig { name, path }).collect());
            cfg.save(&path)?;
            println!("shared {} as {name:?} in {}; restart homeportd to apply", dir.display(), path.display());
        }
        ShareCmd::Remove { name } => {
            let mut shares = current;
            let before = shares.len();
            shares.retain(|(n, _)| *n != name);
            if shares.len() == before {
                return Err(MainError::Usage(format!("no share named {name:?}")));
            }
            cfg.shares = Some(shares.into_iter().map(|(name, path)| ShareConfig { name, path }).collect());
            cfg.save(&path)?;
            println!("removed share {name:?} from {}; restart homeportd to apply", path.display());
        }
    }
    Ok(())
}

fn service_cmd(config: Option<PathBuf>, cmd: ServiceCmd) -> Result<(), MainError> {
    let report = match cmd {
        ServiceCmd::Install { print, args } => {
            let exe = std::env::current_exe().map_err(io_err("find this executable"))?;
            let mut all: Vec<OsString> = Vec::new();
            if let Some(c) = config {
                let c = std::path::absolute(&c).map_err(io_err("config path"))?;
                all.push("--config".into());
                all.push(c.into_os_string());
            }
            all.extend(args);
            Service::install(&exe, &all, print)
        }
        ServiceCmd::Uninstall => Service::uninstall(),
        ServiceCmd::Status => Service::status(),
    };
    println!("{}", report.map_err(io_err("service"))?);
    Ok(())
}

/// `pair` and `info`: what a client needs to add this server.
async fn pair_cmd(config: Option<PathBuf>, args: PairArgs, info: bool) -> Result<(), MainError> {
    let home = home()?;
    let cfg_path = config_path(config, &home);
    let cfg = Config::load(&cfg_path)?;
    let data_dir = cfg.data_dir.as_ref().map_or_else(|| Sys::data_dir(&home), |d| config::expand(d, &home));
    // The identity the server uses (created now if it has never run).
    let identity = tls::load_or_create(&data_dir)?;
    let ts = auth::Tailscale::find(cfg.tailscale.clone());
    let port = cfg.port.unwrap_or(proto::DEFAULT_PORT);
    let addr = match (args.addr, cfg.bind) {
        (Some(a), _) => a.to_string(),
        (None, Some(ip)) => SocketAddr::new(ip, port).to_string(),
        (None, None) => match ts.ipv4().await {
            Ok(ip) => SocketAddr::new(ip, port).to_string(),
            Err(e) => {
                return Err(MainError::Usage(format!(
                    "cannot find this node's tailnet IP ({e}); is Tailscale running? Or pass --addr <ip:port>"
                )));
            }
        },
    };
    let name = cfg.name.clone().unwrap_or_else(Sys::host_name);
    let fp = &identity.fingerprint;
    let raw = fingerprint_bytes(fp);
    let code = proto::pairing_code(&raw);
    if !info {
        println!("Add this server in Homeport (menu bar > Add Server…):\n");
        println!("  Name         {name}");
        println!("  Address      {addr}");
        println!("  Code         {code}\n");
        println!("Pick \"{name}\" in the list and check that Homeport shows the same code, or paste this link:\n");
        println!("  {}\n", proto::pairing_link(&addr, fp, &name));
        println!("Short form (for a QR code):\n");
        println!("  {}", proto::pairing_short(&addr, fp));
        return Ok(());
    }
    let (shares, defaults) = config_shares(&cfg, &home);
    println!("name         {name}");
    println!("os           {}", platform::Fs::OS_NAME);
    println!("version      {}", env!("CARGO_PKG_VERSION"));
    println!("address      {addr}");
    println!("fingerprint  {fp}");
    println!("code         {code}");
    println!("config       {}", cfg_path.display());
    println!("data         {}", data_dir.display());
    println!("tailscale    {}", ts.bin.display());
    let logins = if cfg.allow_logins.is_empty() {
        match ts.self_node().await.map(|n| auth::effective_allowlist(Vec::new(), Some(&n))) {
            Ok(Ok(l)) => format!("{} (owner of this node)", l.join(", ")),
            Ok(Err(e)) => e.to_string(),
            Err(e) => format!("owner of this node (unknown: {e})"),
        }
    } else {
        cfg.allow_logins.join(", ")
    };
    println!("allow        {logins}");
    println!("shares       {}", if defaults { "(platform defaults)" } else { "" });
    for (n, p) in shares {
        println!("  {n:<16} {}", p.display());
    }
    Ok(())
}

/// The fingerprint's 32 bytes (it is always 64 hex digits from `tls`).
fn fingerprint_bytes(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
    }
    out
}

async fn run(config: Option<PathBuf>, args: RunArgs) -> Result<(), MainError> {
    let home = home()?;
    let cfg_path = config_path(config, &home);
    let cfg = Config::load(&cfg_path)?;
    let data_dir = match (&args.data_dir, &cfg.data_dir) {
        (Some(d), _) => d.clone(),
        (None, Some(d)) => config::expand(d, &home),
        (None, None) => Sys::data_dir(&home),
    };
    let shares = if args.shares.is_empty() { config_shares(&cfg, &home).0 } else { args.shares.clone() };
    if shares.is_empty() {
        return Err(MainError::Usage(format!(
            "no shares configured: run `homeportd share add <path>` (config {})",
            cfg_path.display()
        )));
    }

    let identity = tls::load_or_create(&data_dir)?;
    println!("homeportd certificate SHA-256: {}", identity.fingerprint);
    tracing::info!(fingerprint = %identity.fingerprint, path = %data_dir.join("cert.sha256").display(), "server identity");

    let ts = auth::Tailscale::find(args.tailscale.clone().or_else(|| cfg.tailscale.clone()));
    let tailnet_ip = ts.ipv4().await;
    let bind_ip = match (args.bind.or(cfg.bind), &tailnet_ip) {
        (Some(ip), _) => ip,
        (None, Ok(ip)) => *ip,
        (None, Err(e)) => {
            tracing::error!(error = %e, "cannot find the tailnet IP; pass --bind");
            return Err(MainError::Auth(auth::AuthError::Missing("tailnet IP")));
        }
    };
    let node = match ts.self_node().await {
        Ok(n) => Some(n),
        Err(e) if bind_ip.is_loopback() => {
            tracing::warn!(error = %e, "tailscale unavailable; only loopback peers are allowed");
            None
        }
        Err(e) => return Err(e.into()),
    };
    let configured: Vec<String> = cfg.allow_logins.iter().chain(&args.allow_logins).cloned().collect();
    let allow = auth::effective_allowlist(configured, node.as_ref())?;
    tracing::info!(logins = ?allow, "allowed Tailscale logins");
    let mut self_ips = vec![bind_ip];
    if let Ok(ip) = tailnet_ip {
        self_ips.push(ip);
    }
    let authz = Arc::new(auth::Authorizer::new(ts, allow, self_ips));

    std::fs::create_dir_all(&data_dir).map_err(io_err(format!("create {}", data_dir.display())))?;
    let scheme = if args.test_assigned_ids { IdScheme::Assigned } else { platform::Fs::ID_SCHEME };
    let (db, loaded) = db::Db::open(&data_dir.join("index.sqlite"), scheme)?;
    if let Err(e) = thumbs::init(Sys::cache_dir(&home).join("thumbnails")) {
        tracing::warn!(error = %e, "thumbnail cache unavailable");
    }
    let staging = data_dir.join("staging");
    index::clear_staging(&staging).map_err(io_err(format!("staging {}", staging.display())))?;
    let trash = match Trash::open(&home, &data_dir) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(error = %e, "no trash: deletes from clients will be refused");
            None
        }
    };
    let cursor = loaded.watch_cursor.clone();
    let writes = index::WriteDirs { staging, trash };
    let index = Arc::new(index::Index::new(
        shares,
        db.clone(),
        loaded,
        args.cache_dirs,
        writes,
        scheme,
        args.test_assigned_ids,
    )?);
    for s in index.shares() {
        tracing::info!(share = %s.name, path = %s.path.display(), case_sensitive = s.caps.case_sensitive, "share");
    }

    // Change notifications, resumed from the stored cursor when possible.
    let roots: Vec<PathBuf> = index.shares().iter().map(|s| s.path.clone()).collect();
    let (ev_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let watcher = Watcher::start(&roots, cursor.as_deref(), ev_tx).map_err(io_err("watch the shares"))?;
    index.set_watcher(watcher)?;
    let ev_index = index.clone();
    let ev_db = db.clone();
    let ev_task = tokio::spawn(async move {
        while let Some(mut batch) = events.recv().await {
            while let Ok(more) = events.try_recv() {
                batch.changes.extend(more.changes);
                if more.cursor.is_some() {
                    batch.cursor = more.cursor;
                }
            }
            let started = std::time::Instant::now();
            if !batch.changes.is_empty() {
                match ev_index.apply_changes(&batch.changes).await {
                    Ok(n) if n > 0 => tracing::debug!(
                        changes = batch.changes.len(),
                        dirs = n,
                        ms = started.elapsed().as_millis() as u64,
                        "applied changes"
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "applying changes failed"),
                }
            }
            if let Some(c) = batch.cursor {
                let _ = ev_db.send(db::Op::WatchCursor(c));
            }
        }
    });

    let info = Arc::new(server_info(&index, args.name.or(cfg.name)));
    tracing::info!(name = %info.name, os = ?info.os, caps = format!("{:#x}", info.caps), "server");
    let addr = SocketAddr::new(bind_ip, args.port.or(cfg.port).unwrap_or(proto::DEFAULT_PORT));
    let endpoint = server::endpoint(&identity, addr)?;
    tracing::info!(%addr, "listening");
    let serve = tokio::spawn(server::serve(endpoint.clone(), index.clone(), authz, info));

    Sys::shutdown_signal().await.map_err(io_err("signal handler"))?;
    tracing::info!("shutting down");
    endpoint.close(quinn::VarInt::from_u32(0), b"server shutting down");
    let _ = tokio::time::timeout(Duration::from_secs(2), endpoint.wait_idle()).await;
    serve.abort();
    ev_task.abort();
    let _ = db.flush().await;
    db.close();
    Ok(())
}

/// What `Hello` tells clients about this server. A capability is advertised
/// only when every share has it.
fn server_info(index: &index::Index, name: Option<String>) -> proto::ServerInfo {
    use proto::caps;
    let shares = index.shares();
    let all = |f: fn(&platform::FsCaps) -> bool| shares.iter().all(|s| f(&s.caps));
    let mut c = 0;
    let bits: [(bool, u32); 8] = [
        (thumbs::available(), caps::THUMBNAILS),
        (index.has_trash(), caps::TRASH),
        (all(|c| c.xattrs), caps::XATTRS),
        (all(|c| c.tags), caps::TAGS),
        (all(|c| c.symlinks), caps::SYMLINKS),
        (all(|c| c.set_crtime), caps::SET_CRTIME),
        (all(|c| c.hidden), caps::HIDDEN),
        (all(|c| c.mode), caps::MODE),
    ];
    for (on, bit) in bits {
        if on {
            c |= bit;
        }
    }
    proto::ServerInfo {
        name: name.unwrap_or_else(Sys::host_name),
        os: platform::Fs::OS,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        case_sensitive: shares.iter().any(|s| s.caps.case_sensitive),
        normalization_sensitive: shares.iter().any(|s| s.caps.normalization_sensitive),
        caps: c,
    }
}
