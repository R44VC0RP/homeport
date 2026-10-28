//! One direction of the impaired link.
//!
//! Each direction runs on its own OS thread that owns all of its state (RNG,
//! Gilbert-Elliott state, token bucket, delay queue), so no locks sit on the
//! packet path. Receivers stamp packets on arrival and hand them over a
//! channel; the thread decides drop / duplicate, puts the packet through the
//! token-bucket bottleneck (whose backlog is the tail-drop queue), adds
//! propagation delay and jitter, and keeps it in a min-heap until it is due.
//! It sleeps on the channel in shrinking steps (macOS timer leeway is about
//! a quarter of the timeout) and spins the last 300 µs, which keeps delivery
//! within tens of microseconds of the target (tokio timers have 1 ms
//! granularity plus the same leeway).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::profile::Dir;

/// Spin instead of sleeping for the last stretch before a packet is due.
const SPIN: Duration = Duration::from_micros(300);

pub struct Packet {
    pub at: Instant,
    pub data: Vec<u8>,
    pub sock: Arc<UdpSocket>,
    /// `None` sends on a connected socket.
    pub to: Option<SocketAddr>,
}

pub enum Msg {
    Packet(Packet),
    Params(Dir),
}

/// Counters for one direction. `*_us` sums and maxima are per stats interval
/// and are reset by the reader; the rest are cumulative.
#[derive(Default)]
pub struct Stats {
    pub rx_pkts: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub tx_pkts: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub drop_loss: AtomicU64,
    pub drop_burst: AtomicU64,
    pub drop_queue: AtomicU64,
    pub drop_outage: AtomicU64,
    pub send_err: AtomicU64,
    pub dups: AtomicU64,
    /// Bytes waiting in the bottleneck queue (gauge).
    pub queue_bytes: AtomicU64,
    /// Packets held in the delay queue (gauge).
    pub in_flight: AtomicU64,
    pub delay_sum_us: AtomicU64,
    pub delay_n: AtomicU64,
    pub late_sum_us: AtomicU64,
    pub late_max_us: AtomicU64,
}

struct Pending {
    due: Instant,
    seq: u64,
    pkt: Packet,
}

impl PartialEq for Pending {
    fn eq(&self, o: &Self) -> bool {
        (self.due, self.seq) == (o.due, o.seq)
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Pending {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (self.due, self.seq).cmp(&(o.due, o.seq))
    }
}

struct Shaper {
    p: Dir,
    rng: StdRng,
    ge_bad: bool,
    /// GCRA theoretical arrival time of the token bucket.
    tat: Instant,
    last_due: Instant,
    seq: u64,
    heap: BinaryHeap<Reverse<Pending>>,
    stats: Arc<Stats>,
}

impl Shaper {
    fn set_params(&mut self, p: Dir) {
        if self.p.ge != p.ge {
            self.ge_bad = false;
        }
        self.p = p;
    }

    fn backlog(&self, now: Instant) -> u64 {
        let Some(rate) = self.p.rate_bps else { return 0 };
        let tau = Duration::from_secs_f64(self.p.burst_bytes as f64 * 8.0 / rate as f64);
        let wait = self.tat.saturating_duration_since(now).saturating_sub(tau);
        (wait.as_secs_f64() * rate as f64 / 8.0) as u64
    }

    fn admit(&mut self, pkt: Packet, dark: bool) {
        let s = &self.stats;
        let len = pkt.data.len() as u64;
        s.rx_pkts.fetch_add(1, Relaxed);
        s.rx_bytes.fetch_add(len, Relaxed);
        if dark {
            s.drop_outage.fetch_add(1, Relaxed);
            return;
        }
        if self.p.loss > 0.0 && self.rng.random::<f64>() < self.p.loss {
            s.drop_loss.fetch_add(1, Relaxed);
            return;
        }
        if let Some(ge) = &self.p.ge {
            let flip = if self.ge_bad { ge.r } else { ge.p };
            if self.rng.random::<f64>() < flip {
                self.ge_bad = !self.ge_bad;
            }
            let loss = if self.ge_bad { ge.loss_bad } else { ge.loss_good };
            if self.rng.random::<f64>() < loss {
                s.drop_burst.fetch_add(1, Relaxed);
                return;
            }
        }
        let dup = self.p.dup > 0.0 && self.rng.random::<f64>() < self.p.dup;
        if dup {
            s.dups.fetch_add(1, Relaxed);
            let copy = Packet { at: pkt.at, data: pkt.data.clone(), sock: pkt.sock.clone(), to: pkt.to };
            self.schedule(copy);
        }
        self.schedule(pkt);
    }

    fn schedule(&mut self, pkt: Packet) {
        let at = pkt.at;
        let len = pkt.data.len() as u64;
        let depart = match self.p.rate_bps {
            Some(rate) => {
                if self.backlog(at) + len > self.p.queue_limit() {
                    self.stats.drop_queue.fetch_add(1, Relaxed);
                    return;
                }
                let tau = Duration::from_secs_f64(self.p.burst_bytes as f64 * 8.0 / rate as f64);
                self.tat = self.tat.max(at) + Duration::from_secs_f64(len as f64 * 8.0 / rate as f64);
                (self.tat.checked_sub(tau).unwrap_or(at)).max(at)
            }
            None => at,
        };
        let mut prop = self.p.delay;
        if !self.p.jitter.is_zero() {
            let j = self.p.jitter.as_secs_f64();
            let off = self.rng.random_range(-j..=j);
            prop = Duration::from_secs_f64((prop.as_secs_f64() + off).max(0.0));
        }
        let mut due = depart + prop;
        if !self.p.reorder {
            due = due.max(self.last_due);
        }
        self.last_due = self.last_due.max(due);
        self.seq += 1;
        self.heap.push(Reverse(Pending { due, seq: self.seq, pkt }));
    }

    fn flush(&mut self, now: Instant) {
        while self.heap.peek().is_some_and(|Reverse(p)| p.due <= now) {
            let Some(Reverse(p)) = self.heap.pop() else { break };
            let sent = Instant::now();
            let ok = send(&p.pkt);
            let s = &self.stats;
            match ok {
                Ok(()) => {
                    s.tx_pkts.fetch_add(1, Relaxed);
                    s.tx_bytes.fetch_add(p.pkt.data.len() as u64, Relaxed);
                }
                Err(_) => {
                    s.send_err.fetch_add(1, Relaxed);
                }
            }
            let added = sent.saturating_duration_since(p.pkt.at).as_micros() as u64;
            let late = sent.saturating_duration_since(p.due).as_micros() as u64;
            s.delay_sum_us.fetch_add(added, Relaxed);
            s.delay_n.fetch_add(1, Relaxed);
            s.late_sum_us.fetch_add(late, Relaxed);
            s.late_max_us.fetch_max(late, Relaxed);
        }
    }
}

fn send(p: &Packet) -> io::Result<()> {
    // The sockets are non-blocking (shared with tokio); retry briefly when
    // the send buffer is full rather than dropping.
    for _ in 0..200 {
        let r = match p.to {
            Some(to) => p.sock.send_to(&p.data, to),
            None => p.sock.send(&p.data),
        };
        match r {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::ENOBUFS) => {
                std::thread::sleep(Duration::from_micros(20));
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::ErrorKind::WouldBlock.into())
}

/// Runs one direction until every sender is gone.
pub fn run(rx: Receiver<Msg>, params: Dir, stats: Arc<Stats>, dark: Arc<AtomicBool>) {
    let now = Instant::now();
    let mut sh = Shaper {
        p: params,
        rng: StdRng::from_os_rng(),
        ge_bad: false,
        tat: now,
        last_due: now,
        seq: 0,
        heap: BinaryHeap::new(),
        stats,
    };
    loop {
        let next_due = sh.heap.peek().map(|Reverse(p)| p.due);
        let msg = match next_due {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(due) => {
                let wait = due.saturating_duration_since(Instant::now());
                if wait > SPIN {
                    // macOS adds timer leeway of about a quarter of the
                    // timeout (up to ~5 ms), so sleep in shrinking steps.
                    rx.recv_timeout((wait - SPIN).mul_f64(0.7))
                } else {
                    // Spin the last stretch, still taking packets as they come.
                    loop {
                        match rx.try_recv() {
                            Ok(m) => break Ok(m),
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                break Err(RecvTimeoutError::Disconnected);
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) if Instant::now() >= due => {
                                break Err(RecvTimeoutError::Timeout);
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => std::hint::spin_loop(),
                        }
                    }
                }
            }
        };
        match msg {
            Ok(Msg::Packet(p)) => sh.admit(p, dark.load(Relaxed)),
            Ok(Msg::Params(p)) => sh.set_params(p),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Drain whatever else arrived without sleeping again.
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Packet(p) => sh.admit(p, dark.load(Relaxed)),
                Msg::Params(p) => sh.set_params(p),
            }
        }
        let now = Instant::now();
        sh.flush(now);
        sh.stats.queue_bytes.store(sh.backlog(now), Relaxed);
        sh.stats.in_flight.store(sh.heap.len() as u64, Relaxed);
    }
}
