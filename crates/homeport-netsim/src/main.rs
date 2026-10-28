//! `homeport-netsim`: a UDP impairment proxy for testing Homeport (QUIC) on
//! bad networks without touching either machine's network settings.
//!
//! Clients send to the proxy's listen address; each client address gets its
//! own upstream socket to the real server, so any number of clients and
//! ports work. Both directions go through [`link`] with their own delay,
//! jitter, loss, burst loss, bandwidth cap, queue and duplication; outages
//! (scheduled, random or manual) drop everything in both directions.
//!
//! Control: a line protocol on 127.0.0.1 (`homeport-netsim ctl ...`).

mod link;
mod profile;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use rand::{Rng, SeedableRng};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Notify, broadcast};

use crate::link::{Msg, Packet, Stats};
use crate::profile::{Profile, fmt_dur};

const SESSION_IDLE: Duration = Duration::from_secs(120);
const SOCKET_BUFFER: libc::c_int = 4 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(name = "homeport-netsim", version, about)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(clap::Args, Debug, Clone)]
struct RunArgs {
    /// Address clients send to.
    #[arg(long, default_value = "100.84.220.87:47830")]
    listen: SocketAddr,
    /// The real server.
    #[arg(long, default_value = "100.84.220.87:47820")]
    upstream: SocketAddr,
    /// Control socket (loopback only).
    #[arg(long, env = "HOMEPORT_NETSIM_CONTROL", default_value = "127.0.0.1:47831")]
    control: SocketAddr,
    /// Starting profile.
    #[arg(long, default_value = "off")]
    profile: String,
    /// Extra `key=value` settings applied on top of the profile (see `ctl help`).
    #[arg(long = "set", num_args = 1.., allow_hyphen_values = true)]
    set: Vec<String>,
    /// Only forward clients from these IPs (repeatable). homeportd sees every
    /// proxied client as this machine, which it always allows, so without a
    /// list anyone who can reach the listen address gets in.
    #[arg(long = "allow")]
    allow: Vec<IpAddr>,
    /// Print a stats line every second even when idle.
    #[arg(long)]
    verbose: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the proxy (the default).
    Run,
    /// Send a command to a running proxy, e.g. `ctl profile bad-cell`, `ctl blackout 10s`, `ctl stats`, `ctl watch`.
    Ctl {
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// List the named profiles.
    Profiles,
}

#[derive(Debug)]
enum Manual {
    None,
    On,
    Until(Instant),
}

struct Session {
    sock: Arc<StdUdpSocket>,
    last_seen: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

struct Shared {
    args: RunArgs,
    started: Instant,
    profile: Mutex<Profile>,
    profile_gen: AtomicU64,
    up_tx: Sender<Msg>,
    down_tx: Sender<Msg>,
    up: Arc<Stats>,
    down: Arc<Stats>,
    dark: Arc<AtomicBool>,
    manual: Mutex<Manual>,
    outage_changed: Notify,
    listen_std: Arc<StdUdpSocket>,
    sessions: Mutex<HashMap<SocketAddr, Session>>,
    last_line: Mutex<String>,
    lines: broadcast::Sender<String>,
}

impl std::fmt::Display for Manual {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("off"),
            Self::On => f.write_str("on"),
            Self::Until(t) => {
                let left = t.saturating_duration_since(Instant::now());
                write!(f, "{} left", fmt_dur(Duration::from_millis(left.as_millis() as u64)))
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    fn millis(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn set_profile(&self, p: Profile) {
        let _ = self.up_tx.send(Msg::Params(p.up.clone()));
        let _ = self.down_tx.send(Msg::Params(p.down.clone()));
        let outages_changed = lock(&self.profile).outages != p.outages;
        *lock(&self.profile) = p;
        if outages_changed {
            self.profile_gen.fetch_add(1, Relaxed);
        }
        self.outage_changed.notify_one();
    }
}

fn buffers(fd: libc::c_int) {
    for opt in [libc::SO_RCVBUF, libc::SO_SNDBUF] {
        // SAFETY: setsockopt with a valid fd and a pointer to a c_int of the stated size.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&SOCKET_BUFFER as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

/// A std socket for sending from the link threads and a tokio clone of it
/// for receiving.
fn udp_pair(sock: StdUdpSocket) -> std::io::Result<(Arc<StdUdpSocket>, UdpSocket)> {
    buffers(sock.as_raw_fd());
    sock.set_nonblocking(true)?;
    let rx = UdpSocket::from_std(sock.try_clone()?)?;
    Ok((Arc::new(sock), rx))
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let r = match args.cmd {
        None | Some(Cmd::Run) => run(args.run).await,
        Some(Cmd::Ctl { words }) => ctl(args.run.control, &words.join(" ")).await,
        Some(Cmd::Profiles) => {
            for n in profile::NAMES {
                if let Some(p) = profile::named(n) {
                    print!("{p}");
                }
            }
            Ok(())
        }
    };
    if let Err(e) = r {
        eprintln!("homeport-netsim: {e}");
        std::process::exit(1);
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

async fn ctl(control: SocketAddr, line: &str) -> Result<(), BoxError> {
    let mut s =
        TcpStream::connect(control).await.map_err(|e| format!("connect {control}: {e} (is the proxy running?)"))?;
    s.write_all(format!("{line}\n").as_bytes()).await?;
    let mut lines = BufReader::new(s).lines();
    while let Some(l) = lines.next_line().await? {
        println!("{l}");
    }
    Ok(())
}

async fn run(args: RunArgs) -> Result<(), BoxError> {
    if !args.control.ip().is_loopback() {
        return Err("--control must be a loopback address".into());
    }
    let mut prof = profile::named(&args.profile)
        .ok_or_else(|| format!("unknown profile {:?}; try: {}", args.profile, profile::NAMES.join(", ")))?;
    if !args.set.is_empty() {
        let words: Vec<&str> = args.set.iter().flat_map(|s| s.split_whitespace()).collect();
        profile::apply_set(&mut prof, &words)?;
    }
    let (listen_std, listen) = udp_pair(StdUdpSocket::bind(args.listen)?)?;
    let control = TcpListener::bind(args.control).await?;

    let dark = Arc::new(AtomicBool::new(false));
    let (up_tx, up_rx) = std::sync::mpsc::channel();
    let (down_tx, down_rx) = std::sync::mpsc::channel();
    let (up, down) = (Arc::new(Stats::default()), Arc::new(Stats::default()));
    for (name, rx, params, stats) in
        [("netsim-up", up_rx, prof.up.clone(), up.clone()), ("netsim-down", down_rx, prof.down.clone(), down.clone())]
    {
        let dark = dark.clone();
        std::thread::Builder::new().name(name.into()).spawn(move || link::run(rx, params, stats, dark))?;
    }
    println!("listening on {} → {}; control {}", args.listen, args.upstream, args.control);
    if args.allow.is_empty() {
        println!(
            "warning: no --allow list; any client that reaches {} bypasses homeportd's tailscale check",
            args.listen
        );
    } else {
        let ips: Vec<String> = args.allow.iter().map(IpAddr::to_string).collect();
        println!("allowing {}", ips.join(", "));
    }
    print!("{prof}");
    let sh = Arc::new(Shared {
        args,
        started: Instant::now(),
        profile: Mutex::new(prof),
        profile_gen: AtomicU64::new(0),
        up_tx,
        down_tx,
        up,
        down,
        dark,
        manual: Mutex::new(Manual::None),
        outage_changed: Notify::new(),
        listen_std,
        sessions: Mutex::new(HashMap::new()),
        last_line: Mutex::new(String::new()),
        lines: broadcast::channel(64).0,
    });
    tokio::spawn(outages(sh.clone()));
    tokio::spawn(stats_loop(sh.clone()));
    tokio::spawn(reap_sessions(sh.clone()));
    tokio::spawn(control_loop(sh.clone(), control));

    let mut buf = vec![0u8; 65536];
    loop {
        tokio::select! {
            r = listen.recv_from(&mut buf) => {
                let (n, from) = match r {
                    Ok(v) => v,
                    // e.g. ICMP port unreachable surfacing as ECONNREFUSED.
                    Err(_) => continue,
                };
                let at = Instant::now();
                if !sh.args.allow.is_empty() && !sh.args.allow.contains(&from.ip()) {
                    continue;
                }
                let sock = match session(&sh, from) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("session for {from}: {e}");
                        continue;
                    }
                };
                let _ = sh.up_tx.send(Msg::Packet(Packet { at, data: buf[..n].to_vec(), sock, to: None }));
            }
            _ = tokio::signal::ctrl_c() => {
                println!("stopping");
                return Ok(());
            }
        }
    }
}

/// The upstream socket for a client, creating the session on first contact.
fn session(sh: &Arc<Shared>, from: SocketAddr) -> std::io::Result<Arc<StdUdpSocket>> {
    let mut sessions = lock(&sh.sessions);
    if let Some(s) = sessions.get(&from) {
        s.last_seen.store(sh.millis(), Relaxed);
        return Ok(s.sock.clone());
    }
    let bind_ip = match sh.args.listen.ip() {
        ip if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        ip => ip,
    };
    let std = StdUdpSocket::bind(SocketAddr::new(bind_ip, 0))?;
    std.connect(sh.args.upstream)?;
    let (sock, rx) = udp_pair(std)?;
    let last_seen = Arc::new(AtomicU64::new(sh.millis()));
    let task = tokio::spawn(upstream_rx(sh.clone(), rx, from, last_seen.clone()));
    println!("{:>7.1}s session {from} via {}", sh.started.elapsed().as_secs_f64(), sock.local_addr()?);
    sessions.insert(from, Session { sock: sock.clone(), last_seen, task });
    Ok(sock)
}

async fn upstream_rx(sh: Arc<Shared>, rx: UdpSocket, client: SocketAddr, last_seen: Arc<AtomicU64>) {
    let mut buf = vec![0u8; 65536];
    loop {
        let n = match rx.recv(&mut buf).await {
            Ok(n) => n,
            Err(_) => continue,
        };
        let at = Instant::now();
        last_seen.store(sh.millis(), Relaxed);
        let pkt = Packet { at, data: buf[..n].to_vec(), sock: sh.listen_std.clone(), to: Some(client) };
        let _ = sh.down_tx.send(Msg::Packet(pkt));
    }
}

async fn reap_sessions(sh: Arc<Shared>) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let now = sh.millis();
        lock(&sh.sessions).retain(|addr, s| {
            let keep = now.saturating_sub(s.last_seen.load(Relaxed)) < SESSION_IDLE.as_millis() as u64;
            if !keep {
                s.task.abort();
                println!("{:>7.1}s session {addr} expired", sh.started.elapsed().as_secs_f64());
            }
            keep
        });
    }
}

fn exp_gap(rng: &mut impl Rng, mean: Duration) -> Duration {
    let u: f64 = rng.random_range(f64::EPSILON..1.0);
    Duration::from_secs_f64(-u.ln() * mean.as_secs_f64())
}

/// Keeps `dark` in step with the scheduled, random and manual outages.
async fn outages(sh: Arc<Shared>) {
    let mut rng = rand::rngs::StdRng::from_os_rng();
    let mut seen_gen = u64::MAX;
    let mut t0 = Instant::now();
    let mut random: Option<(Instant, Instant)> = None;
    loop {
        let now = Instant::now();
        let o = lock(&sh.profile).outages.clone();
        let generation = sh.profile_gen.load(Relaxed);
        if generation != seen_gen {
            seen_gen = generation;
            t0 = now;
            random = None;
        }
        let mut dark = false;
        let mut why = "";
        let mut next = now + Duration::from_secs(3600);
        match *lock(&sh.manual) {
            Manual::On => (dark, why) = (true, "manual"),
            Manual::Until(t) if t > now => {
                (dark, why) = (true, "manual");
                next = next.min(t);
            }
            _ => {}
        }
        if let Some((period, len)) = o.every {
            let phase = Duration::from_nanos((now - t0).as_nanos().rem_euclid(period.as_nanos()) as u64);
            let start = period - len;
            if phase >= start {
                if !dark {
                    (dark, why) = (true, "scheduled");
                }
                next = next.min(now + (period - phase));
            } else {
                next = next.min(now + (start - phase));
            }
        }
        if let Some(r) = &o.random {
            if random.is_none_or(|(_, end)| now >= end) {
                let start = now + exp_gap(&mut rng, r.mean_gap);
                let len = Duration::from_secs_f64(rng.random_range(r.min.as_secs_f64()..=r.max.as_secs_f64()));
                random = Some((start, start + len));
            }
            if let Some((start, end)) = random {
                if now >= start {
                    if !dark {
                        (dark, why) = (true, "random");
                    }
                    next = next.min(end);
                } else {
                    next = next.min(start);
                }
            }
        }
        let was = sh.dark.swap(dark, Relaxed);
        if was != dark {
            let t = sh.started.elapsed().as_secs_f64();
            if dark {
                let left = next.saturating_duration_since(now);
                println!(
                    "{t:>7.1}s blackout start ({why}, {})",
                    fmt_dur(Duration::from_millis(left.as_millis() as u64))
                );
            } else {
                println!("{t:>7.1}s blackout end");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep_until(next.into()) => {}
            _ = sh.outage_changed.notified() => {}
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Snap {
    rx_pkts: u64,
    rx_bytes: u64,
    tx_pkts: u64,
    tx_bytes: u64,
    drop_loss: u64,
    drop_burst: u64,
    drop_queue: u64,
    drop_outage: u64,
    send_err: u64,
    dups: u64,
}

fn snap(s: &Stats) -> Snap {
    Snap {
        rx_pkts: s.rx_pkts.load(Relaxed),
        rx_bytes: s.rx_bytes.load(Relaxed),
        tx_pkts: s.tx_pkts.load(Relaxed),
        tx_bytes: s.tx_bytes.load(Relaxed),
        drop_loss: s.drop_loss.load(Relaxed),
        drop_burst: s.drop_burst.load(Relaxed),
        drop_queue: s.drop_queue.load(Relaxed),
        drop_outage: s.drop_outage.load(Relaxed),
        send_err: s.send_err.load(Relaxed),
        dups: s.dups.load(Relaxed),
    }
}

struct Interval {
    line: String,
    delay_ms: Option<f64>,
    late_avg_us: Option<f64>,
    late_max_us: u64,
    rx_pkts: u64,
}

fn interval(name: &str, s: &Stats, now: Snap, prev: Snap, secs: f64) -> Interval {
    let n = s.delay_n.swap(0, Relaxed);
    let sum = s.delay_sum_us.swap(0, Relaxed);
    let late = s.late_sum_us.swap(0, Relaxed);
    let late_max = s.late_max_us.swap(0, Relaxed);
    let delay_ms = (n > 0).then(|| sum as f64 / n as f64 / 1000.0);
    let rx = now.rx_pkts - prev.rx_pkts;
    let line = format!(
        "{name} {:>5} pkt {:>7.2} Mbit/s out {:>7.2} | drop loss {:>3} burst {:>3} queue {:>3} outage {:>3}{} dup {} | q {:>4} KB fl {:>4} | +{}",
        rx,
        (now.rx_bytes - prev.rx_bytes) as f64 * 8.0 / 1e6 / secs,
        (now.tx_bytes - prev.tx_bytes) as f64 * 8.0 / 1e6 / secs,
        now.drop_loss - prev.drop_loss,
        now.drop_burst - prev.drop_burst,
        now.drop_queue - prev.drop_queue,
        now.drop_outage - prev.drop_outage,
        match now.send_err - prev.send_err {
            0 => String::new(),
            e => format!(" senderr {e}"),
        },
        now.dups - prev.dups,
        s.queue_bytes.load(Relaxed) / 1024,
        s.in_flight.load(Relaxed),
        delay_ms.map_or("-".into(), |d| format!("{d:.1}ms")),
    );
    Interval {
        line,
        delay_ms,
        late_avg_us: (n > 0).then(|| late as f64 / n as f64),
        late_max_us: late_max,
        rx_pkts: rx + (now.tx_pkts - prev.tx_pkts),
    }
}

async fn stats_loop(sh: Arc<Shared>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut pu, mut pd) = (snap(&sh.up), snap(&sh.down));
    let mut last = Instant::now();
    let mut was_dark = false;
    loop {
        tick.tick().await;
        let secs = last.elapsed().as_secs_f64();
        last = Instant::now();
        let (nu, nd) = (snap(&sh.up), snap(&sh.down));
        let u = interval("up  ", &sh.up, nu, pu, secs);
        let d = interval("down", &sh.down, nd, pd, secs);
        (pu, pd) = (nu, nd);
        let dark = sh.dark.load(Relaxed);
        let rtt = match (u.delay_ms, d.delay_ms) {
            (Some(a), Some(b)) => format!("{:.1} ms", a + b),
            _ => "-".into(),
        };
        let late_avg = [u.late_avg_us, d.late_avg_us].into_iter().flatten().fold(0.0f64, f64::max);
        let line = format!(
            "{:>7.1}s {} sessions {} | rtt+ {rtt} | timing late avg {:.0} max {} us\n  {}\n  {}",
            sh.started.elapsed().as_secs_f64(),
            if dark { "DARK" } else { "up  " },
            lock(&sh.sessions).len(),
            late_avg,
            u.late_max_us.max(d.late_max_us),
            u.line,
            d.line
        );
        if sh.args.verbose || u.rx_pkts + d.rx_pkts > 0 || dark != was_dark {
            println!("{line}");
        }
        was_dark = dark;
        *lock(&sh.last_line) = line.clone();
        let _ = sh.lines.send(line);
    }
}

const HELP: &str = "commands:
  profile NAME                 switch profile (resets manual settings)
  profiles                     list profiles
  show                         current settings
  set [up|down|both] KEY=VAL…  delay=25ms jitter=10ms (one-way) loss=1% burstloss=3%/4
                               rate=1.5mbit|off queue=256k|auto burst=3000 dup=0.1% reorder=on|off
                               every=5s/30s|off  random=2s-20s/45s|off
  blackout DURATION|on|off     manual outage
  stats                        totals and the last second
  watch                        stream the per-second stats
  sessions                     client sessions";

async fn control_loop(sh: Arc<Shared>, listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let sh = sh.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let Ok(Some(line)) = lines.next_line().await else { return };
            let words: Vec<&str> = line.split_whitespace().collect();
            if words.first() == Some(&"watch") {
                let mut rx = sh.lines.subscribe();
                while let Ok(l) = rx.recv().await {
                    if w.write_all(format!("{l}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
                return;
            }
            let reply = command(&sh, &words);
            println!("{:>7.1}s ctl: {line}", sh.started.elapsed().as_secs_f64());
            let _ = w.write_all(format!("{}\n", reply.trim_end()).as_bytes()).await;
        });
    }
}

fn command(sh: &Shared, words: &[&str]) -> String {
    match words {
        ["profile", name] => match profile::named(name) {
            Some(p) => {
                let text = p.to_string();
                sh.set_profile(p);
                text
            }
            None => format!("unknown profile {name:?}; try: {}", profile::NAMES.join(", ")),
        },
        ["profiles"] => profile::NAMES.iter().filter_map(|n| profile::named(n)).map(|p| p.to_string()).collect(),
        ["show"] => {
            let mut s = lock(&sh.profile).to_string();
            s += &format!("  manual blackout {}, dark now: {}\n", lock(&sh.manual), sh.dark.load(Relaxed));
            s
        }
        ["set", rest @ ..] => {
            let mut p = lock(&sh.profile).clone();
            match profile::apply_set(&mut p, rest) {
                Ok(()) => {
                    let text = p.to_string();
                    sh.set_profile(p);
                    text
                }
                Err(e) => format!("error: {e}"),
            }
        }
        ["blackout", arg] => {
            let m = match *arg {
                "on" => Manual::On,
                "off" => Manual::None,
                d => match profile::parse_dur(d) {
                    Ok(d) => Manual::Until(Instant::now() + d),
                    Err(e) => return format!("error: {e}"),
                },
            };
            let text = format!("blackout {m}");
            *lock(&sh.manual) = m;
            sh.outage_changed.notify_one();
            text
        }
        ["stats"] => {
            let mut s = String::new();
            for (name, st) in [("up", &sh.up), ("down", &sh.down)] {
                let n = snap(st);
                s += &format!(
                    "{name:<4} rx {} pkt {:.1} MB  tx {} pkt {:.1} MB  drops loss {} burst {} queue {} outage {} senderr {}  dups {}\n",
                    n.rx_pkts,
                    n.rx_bytes as f64 / 1e6,
                    n.tx_pkts,
                    n.tx_bytes as f64 / 1e6,
                    n.drop_loss,
                    n.drop_burst,
                    n.drop_queue,
                    n.drop_outage,
                    n.send_err,
                    n.dups
                );
            }
            s += "last second:\n";
            s += &lock(&sh.last_line);
            s
        }
        ["sessions"] => {
            let now = sh.millis();
            lock(&sh.sessions)
                .iter()
                .map(|(a, s)| {
                    let via = s.sock.local_addr().map_or("?".into(), |l| l.to_string());
                    format!("{a} via {via}, idle {} ms\n", now.saturating_sub(s.last_seen.load(Relaxed)))
                })
                .collect()
        }
        _ => HELP.into(),
    }
}
