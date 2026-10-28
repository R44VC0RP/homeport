//! `homeport`: headless client for a live `homeportd`.

mod bench;

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use homeport_client::{ChangeEvent, Client, Config, Error, ListResult, parse_fingerprint};
use homeport_proto::{Item, Kind, Meta, Priority};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "homeport", version, about)]
struct Args {
    /// Server address.
    #[arg(long, env = "HOMEPORT_SERVER", default_value = "100.84.220.87:47820")]
    server: SocketAddr,
    /// Server certificate SHA-256 (hex). Defaults to the pinned file in
    /// ~/Library/Application Support/Homeport/client/server.sha256, then the
    /// local server's cert.sha256.
    #[arg(long, env = "HOMEPORT_FINGERPRINT")]
    fingerprint: Option<String>,
    /// Per-request deadline before failing with "unreachable".
    #[arg(long, default_value_t = 1000)]
    deadline_ms: u64,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List a directory (path like `dev/foo`, or an item id).
    Ls { target: String },
    /// Show one item.
    Stat { target: String },
    /// Write file content to stdout.
    Cat {
        target: String,
        /// Byte range START-END (end exclusive) or START-.
        #[arg(long)]
        range: Option<String>,
        #[arg(long)]
        compress: bool,
    },
    /// Write the server's PNG thumbnail of a file to stdout.
    Thumb {
        target: String,
        #[arg(long, default_value_t = 256)]
        size: u32,
    },
    /// Print the change feed for directories this client has listed.
    Changes {
        #[arg(long, default_value_t = 0)]
        since: u64,
        #[arg(long)]
        follow: bool,
    },
    /// Link state, RTT and loss.
    Status {
        #[arg(long, default_value_t = 5)]
        probes: u32,
        /// Keep running for this many seconds, listing the root once a second
        /// and printing the link state (shows fast-fail and recovery).
        #[arg(long)]
        watch: Option<u64>,
    },
    /// Measure time-to-listing and read throughput.
    Bench(bench::BenchArgs),
    /// Upload a local file as DIR/NAME (or replace TARGET's content with --replace).
    Put {
        local: PathBuf,
        /// Destination folder (path or id), or with --replace the file to overwrite.
        dest: String,
        #[arg(long)]
        name: Option<String>,
        /// Overwrite the content of an existing file (base: its current content rev).
        #[arg(long)]
        replace: bool,
        /// With --replace: the base content rev (default: current, i.e. no conflict).
        #[arg(long)]
        base: Option<u64>,
    },
    /// Create a folder.
    Mkdir { dir: String, name: String },
    /// Rename and/or move an item.
    Mv {
        target: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Move an item to the server's Trash (listed in the trash container).
    Trash {
        target: String,
        #[arg(long)]
        recursive: bool,
        /// Not shown in the trash container (a delete).
        #[arg(long)]
        unlisted: bool,
    },
    /// Put a trashed item (by id) back.
    Untrash { id: String, dir: String, name: String },
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Client(#[from] Error),
    #[error("{0}")]
    Usage(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn app_support() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library/Application Support/Homeport")
}

fn fingerprint(arg: Option<String>) -> Result<[u8; 32], CliError> {
    let text = match arg {
        Some(s) => s,
        None => {
            let dir = app_support();
            [dir.join("client/server.sha256"), dir.join("server/cert.sha256")]
                .iter()
                .find_map(|p| std::fs::read_to_string(p).ok())
                .ok_or_else(|| CliError::Usage("no server fingerprint; pass --fingerprint".into()))?
        }
    };
    parse_fingerprint(text.trim()).ok_or_else(|| CliError::Usage("fingerprint must be 64 hex digits".into()))
}

fn client_id() -> String {
    let path = app_support().join("client/id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_owned();
        if !id.is_empty() {
            return id;
        }
    }
    let id = format!("cli-{:016x}", rand::random::<u64>());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, &id);
    id
}

fn is_id(s: &str) -> bool {
    s == "root" || s == "trash" || s.starts_with("root:") || (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

async fn target_item(c: &Client, target: &str) -> Result<Item, Error> {
    if is_id(target) {
        c.stat(target, Priority::Visible).await
    } else {
        c.resolve_path(target, Priority::Visible).await
    }
}

/// `YYYY-MM-DD HH:MM:SS` (UTC) from nanoseconds since the epoch.
pub fn fmt_time(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn kind_char(k: Kind) -> char {
    match k {
        Kind::Dir => 'd',
        Kind::File => '-',
        Kind::Symlink => 'l',
        Kind::Other => '?',
    }
}

fn print_item(out: &mut impl Write, i: &Item) -> std::io::Result<()> {
    writeln!(
        out,
        "{}{:04o} {:>12} {} rev {:>6} {:>20}  {}",
        kind_char(i.kind),
        i.mode,
        i.size,
        fmt_time(i.mtime_ns),
        i.rev,
        i.id,
        i.name
    )
}

fn parse_range(r: &str) -> Result<(u64, u64), CliError> {
    let bad = || CliError::Usage(format!("bad range {r:?}; use START-END or START-"));
    let (a, b) = r.split_once('-').ok_or_else(bad)?;
    let start: u64 = a.parse().map_err(|_| bad())?;
    let len = if b.is_empty() { 0 } else { b.parse::<u64>().map_err(|_| bad())?.checked_sub(start).ok_or_else(bad)? };
    Ok((start, len))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .init();
    if let Err(e) = run(Args::parse()).await {
        eprintln!("homeport: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), CliError> {
    let mut cfg = Config::new(args.server, fingerprint(args.fingerprint)?, client_id());
    cfg.request_deadline = Duration::from_millis(args.deadline_ms);
    let c = Client::connect(cfg).await?;
    let mut out = std::io::stdout().lock();
    match args.cmd {
        Cmd::Ls { target } => {
            let dir = target_item(&c, &target).await?;
            match c.list_dir(&dir.id, None, Priority::Visible).await? {
                ListResult::Listing { dir_rev, items } => {
                    for i in items.iter() {
                        print_item(&mut out, i)?;
                    }
                    writeln!(out, "{} entries, dir_rev {dir_rev}, id {}", items.len(), dir.id)?;
                }
                ListResult::NotModified { dir_rev } => writeln!(out, "not modified (dir_rev {dir_rev})")?,
            }
        }
        Cmd::Stat { target } => {
            let i = target_item(&c, &target).await?;
            print_item(&mut out, &i)?;
            writeln!(out, "parent {} content_rev {} ctime {}", i.parent_id, i.content_rev, fmt_time(i.ctime_ns))?;
        }
        Cmd::Cat { target, range, compress } => {
            let item = target_item(&c, &target).await?;
            let (offset, len) = range.as_deref().map(parse_range).transpose()?.unwrap_or((0, 0));
            let mut s = c.read_stream(&item.id, offset, len, compress, Priority::UserOpen).await?;
            while let Some((_, chunk)) = s.next_chunk().await? {
                out.write_all(&chunk)?;
            }
            out.flush()?;
        }
        Cmd::Thumb { target, size } => {
            let item = target_item(&c, &target).await?;
            let started = std::time::Instant::now();
            let png = c.thumbnail(&item.id, size).await?;
            eprintln!("{} bytes in {:.1} ms", png.len(), started.elapsed().as_secs_f64() * 1000.0);
            out.write_all(&png)?;
            out.flush()?;
        }
        Cmd::Changes { since, follow } => {
            let hello = c.hello().ok_or_else(|| CliError::Usage("no hello".into()))?;
            let since = if since == 0 { hello.seq } else { since };
            eprintln!("index {} seq {} (from {since})", hello.index_id, hello.seq);
            let mut s = c.changes(since, hello.index_id, follow).await?;
            while let Some(ChangeEvent::Batch { seq, changed, deleted, caught_up }) = s.next().await? {
                for i in &changed {
                    write!(out, "changed ")?;
                    print_item(&mut out, i)?;
                }
                for d in &deleted {
                    writeln!(out, "deleted {} from {} (seq {})", d.id, d.parent_id, d.seq)?;
                }
                writeln!(out, "-- seq {seq}{}", if caught_up { " (caught up)" } else { "" })?;
                out.flush()?;
                if caught_up && !follow {
                    break;
                }
            }
        }
        Cmd::Status { watch: Some(secs), .. } => {
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(secs) {
                let t = std::time::Instant::now();
                let r = c.list_dir("root", None, Priority::Visible).await;
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let l = c.link();
                let outcome = match r {
                    Ok(_) => "ok".to_owned(),
                    Err(e) => format!("error: {e}"),
                };
                writeln!(
                    out,
                    "{:>6.1}s  link {:<8} rtt {:>8}  list {ms:>7.2} ms  {outcome}",
                    start.elapsed().as_secs_f64(),
                    l.state.to_string(),
                    l.rtt.map_or("-".into(), |r| format!("{:.2}ms", r.as_secs_f64() * 1000.0)),
                )?;
                out.flush()?;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        Cmd::Status { probes, watch: None } => {
            for _ in 0..probes {
                match c.ping().await {
                    Ok(rtt) => writeln!(out, "ping {:.2} ms", rtt.as_secs_f64() * 1000.0)?,
                    Err(e) => writeln!(out, "ping failed: {e}")?,
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let l = c.link();
            let h = c.hello();
            writeln!(out, "server     {}", c.config().server)?;
            writeln!(out, "link       {}", l.state)?;
            writeln!(
                out,
                "quic rtt   {}",
                l.rtt.map_or("-".into(), |r| format!("{:.2} ms", r.as_secs_f64() * 1000.0))
            )?;
            writeln!(
                out,
                "ping rtt   {}",
                l.ping_rtt.map_or("-".into(), |r| format!("{:.2} ms", r.as_secs_f64() * 1000.0))
            )?;
            writeln!(out, "loss       {:.2} %", l.loss * 100.0)?;
            writeln!(out, "probes     {} ({} missed)", l.probes, l.misses)?;
            if let Some(h) = h {
                writeln!(out, "index      {} seq {}", h.index_id, h.seq)?;
                let roots: Vec<_> = h.roots.iter().map(|r| r.id.as_str()).collect();
                writeln!(out, "roots      {}", roots.join(", "))?;
            }
        }
        Cmd::Bench(b) => bench::run(&c, b, &mut out).await?,
        Cmd::Put { local, dest, name, replace, base } => {
            let file = std::fs::File::open(&local)?;
            let size = file.metadata()?.len();
            let progress = |_: u64| {};
            let up = homeport_client::Upload { file, size, progress: &progress };
            let started = std::time::Instant::now();
            let target = target_item(&c, &dest).await?;
            let device = "cli";
            if replace {
                let base = base.or(Some(target.content_rev));
                let (item, conflict) =
                    c.write_content(&target.id, base, Meta::default(), device, up, Priority::UserOpen).await?;
                print_item(&mut out, &item)?;
                if let Some(cp) = conflict {
                    write!(out, "conflict copy ")?;
                    print_item(&mut out, &cp)?;
                }
            } else {
                let name = name
                    .or_else(|| local.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .ok_or_else(|| CliError::Usage("no name".into()))?;
                let op = format!("cli-{:016x}", rand::random::<u64>());
                let item = c
                    .create(&op, &target.id, &name, Kind::File, None, Meta::default(), false, Some(up), Priority::UserOpen)
                    .await?;
                print_item(&mut out, &item)?;
            }
            writeln!(out, "{size} bytes in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0)?;
        }
        Cmd::Mkdir { dir, name } => {
            let d = target_item(&c, &dir).await?;
            let op = format!("cli-{:016x}", rand::random::<u64>());
            let item = c.create(&op, &d.id, &name, Kind::Dir, None, Meta::default(), false, None, Priority::Visible).await?;
            print_item(&mut out, &item)?;
        }
        Cmd::Mv { target, to, name } => {
            let t = target_item(&c, &target).await?;
            let to = match to {
                Some(d) => Some(target_item(&c, &d).await?.id),
                None => None,
            };
            let item = c.modify(&t.id, to, name, Meta::default()).await?;
            print_item(&mut out, &item)?;
        }
        Cmd::Trash { target, recursive, unlisted } => {
            let t = target_item(&c, &target).await?;
            let item = c.trash(&t.id, !unlisted, recursive).await?;
            print_item(&mut out, &item)?;
        }
        Cmd::Untrash { id, dir, name } => {
            let d = target_item(&c, &dir).await?;
            let item = c.untrash(&id, &d.id, &name).await?;
            print_item(&mut out, &item)?;
        }
    }
    Ok(())
}
