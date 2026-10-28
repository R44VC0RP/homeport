//! Link-quality estimator with hysteresis.
//!
//! Inputs:
//! - app-level probes (pings): liveness, and the only way to reach Offline;
//! - connection samples taken every few hundred milliseconds: QUIC smoothed
//!   RTT, the client's sent/lost packet counters and received bytes.
//!
//! Every metric is windowed so the state follows the link of the last few
//! seconds, not the connection's history:
//! - RTT is the minimum smoothed RTT over [`LinkConfig::window`], so one
//!   queueing spike does not downgrade the link;
//! - loss is lost/sent over the window, and samples overlapping an outage
//!   (or just after it, when QUIC declares everything sent into the outage
//!   lost at once) are dropped (loss uses a longer window, [`LinkConfig::loss_window`]);
//! - throughput is the best delivery rate measured over the window while a
//!   transfer was running for the whole sample (so idle and app-limited
//!   periods do not count), kept for [`LinkConfig::throughput_max_age`].
//!
//! Downgrades are immediate (Offline only after consecutive missed probes);
//! upgrades need the better class to hold for [`LinkConfig::upgrade_hold`],
//! except that one answered probe lifts Offline to Degraded so requests stop
//! failing fast.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LinkState {
    Good,
    Degraded,
    Poor,
    Offline,
}

impl std::fmt::Display for LinkState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Good => "good",
            Self::Degraded => "degraded",
            Self::Poor => "poor",
            Self::Offline => "offline",
        })
    }
}

#[derive(Clone, Debug)]
pub struct LinkConfig {
    /// Consecutive missed probes before Offline.
    pub offline_after_misses: u32,
    /// How long a better class must hold before the state climbs to it.
    pub upgrade_hold: Duration,
    /// Window of the RTT and throughput samples.
    pub window: Duration,
    /// Window of the loss samples (longer: an idle client sends few packets).
    pub loss_window: Duration,
    /// After an outage, loss samples are ignored for this long.
    pub outage_grace: Duration,
    /// Fewest packets sent in the window before loss counts.
    pub min_loss_packets: u64,
    /// A throughput measurement is forgotten this long after the last sample.
    pub throughput_max_age: Duration,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            offline_after_misses: 2,
            upgrade_hold: Duration::from_secs(8),
            window: Duration::from_secs(10),
            loss_window: Duration::from_secs(30),
            outage_grace: Duration::from_secs(2),
            min_loss_packets: 30,
            throughput_max_age: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinkSnapshot {
    pub state: LinkState,
    /// QUIC smoothed RTT.
    pub rtt: Option<Duration>,
    /// Last application-level ping round trip.
    pub ping_rtt: Option<Duration>,
    /// Packet loss over the window (client-sent packets).
    pub loss: f64,
    /// Recent delivery rate in bits per second, measured during transfers.
    pub throughput_bps: Option<f64>,
    pub consecutive_misses: u32,
    pub probes: u64,
    pub misses: u64,
}

/// One connection sample (see [`LinkEstimator::on_sample`]).
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub rtt: Duration,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub rx_bytes: u64,
    /// A transfer has been running without a break since this instant.
    pub busy_since: Option<Instant>,
}

#[derive(Debug)]
pub struct LinkEstimator {
    cfg: LinkConfig,
    state: LinkState,
    established: bool,
    consecutive_misses: u32,
    /// Better class and since when it has held.
    upgrade: Option<(LinkState, Instant)>,
    rtt: Option<Duration>,
    rtts: VecDeque<(Instant, Duration)>,
    ping_rtt: Option<Duration>,
    /// `(at, sent, lost)` deltas.
    losses: VecDeque<(Instant, u64, u64)>,
    last_sample: Option<(Instant, Sample)>,
    /// Start of the current throughput measurement: `(at, rx_bytes)`.
    rate_start: Option<(Instant, u64)>,
    rates: VecDeque<(Instant, f64)>,
    throughput: Option<(f64, Instant)>,
    /// No loss samples before this (an outage just ended).
    quiet_until: Option<Instant>,
    probes: u64,
    misses: u64,
}

const RTT_CAP: Duration = Duration::from_secs(5);

impl LinkEstimator {
    pub fn new(cfg: LinkConfig) -> Self {
        Self {
            cfg,
            state: LinkState::Offline,
            established: false,
            consecutive_misses: 0,
            upgrade: None,
            rtt: None,
            rtts: VecDeque::new(),
            ping_rtt: None,
            losses: VecDeque::new(),
            last_sample: None,
            rate_start: None,
            rates: VecDeque::new(),
            throughput: None,
            quiet_until: None,
            probes: 0,
            misses: 0,
        }
    }

    pub fn state(&self) -> LinkState {
        self.state
    }

    /// Whether requests should fail fast instead of being attempted.
    pub fn fail_fast(&self) -> bool {
        self.established && self.state == LinkState::Offline
    }

    /// Round trip without a queue in front: the lower of QUIC's smoothed
    /// RTT and the last ping (QUIC's estimate stays high after a transfer
    /// filled a queue until new acks come in; an idle client sends few).
    /// Time spent queued behind other traffic does not count against
    /// deadlines anyway (see `Responses::read_some`).
    pub fn base_rtt(&self) -> Duration {
        let r = match (self.rtt, self.ping_rtt) {
            (Some(a), Some(b)) => a.min(b),
            (a, b) => a.or(b).unwrap_or_default(),
        };
        r.min(RTT_CAP)
    }

    /// Silence a request tolerates: `base`, stretched on long paths to
    /// cover lost packets (QUIC resends after ~2 RTT, then doubles the
    /// wait) plus the server's own time.
    pub fn request_timeout(&self, base: Duration) -> Duration {
        base.max(self.base_rtt() * 5 + Duration::from_millis(750))
    }

    /// Like [`Self::request_timeout`], shorter: probes only decide when the
    /// link counts as offline, and that takes consecutive misses.
    pub fn probe_timeout(&self, base: Duration) -> Duration {
        base.max(self.base_rtt() * 3 + Duration::from_millis(250))
    }

    fn windowed_rtt(&self) -> Option<Duration> {
        self.rtts.iter().map(|(_, r)| *r).min().or(self.rtt).or(self.ping_rtt)
    }

    fn windowed_loss(&self) -> Option<f64> {
        let (sent, lost) = self.losses.iter().fold((0, 0), |(s, l), (_, ds, dl)| (s + ds, l + dl));
        (sent >= self.cfg.min_loss_packets).then(|| lost as f64 / sent as f64)
    }

    fn current_throughput(&self, now: Instant) -> Option<f64> {
        self.throughput.filter(|(_, at)| now.duration_since(*at) < self.cfg.throughput_max_age).map(|(bps, _)| bps)
    }

    fn classify(&self, now: Instant) -> LinkState {
        let ms = self.windowed_rtt().unwrap_or_default().as_secs_f64() * 1000.0;
        let loss = self.windowed_loss().unwrap_or(0.0);
        let tput = self.current_throughput(now);
        // Without a transfer to measure, heavy loss on a long path stands
        // in for "slow" (EDGE-class links).
        let slow = tput.map_or(loss > 0.06, |t| t < 700_000.0);
        if ms > 1400.0 || (ms > 270.0 && slow) {
            LinkState::Poor
        } else if ms > 300.0 || loss > 0.03 || tput.is_some_and(|t| t < 2_000_000.0) {
            LinkState::Degraded
        } else {
            LinkState::Good
        }
    }

    fn settle(&mut self, now: Instant) {
        let target = self.classify(now);
        if !self.established {
            self.established = true;
            self.state = target;
            self.upgrade = None;
            return;
        }
        if self.state == LinkState::Offline {
            // The server answered: stop failing fast at once. Climbing past
            // Degraded still needs the better class to hold.
            self.state = target.max(LinkState::Degraded);
            self.upgrade = None;
            return;
        }
        if target >= self.state {
            // Same or worse: downgrade at once.
            self.state = target;
            self.upgrade = None;
            return;
        }
        let (cand, since) = match self.upgrade {
            // Track the worst class seen while climbing.
            Some((c, since)) => (c.max(target), since),
            None => (target, now),
        };
        if now.duration_since(since) >= self.cfg.upgrade_hold {
            self.state = cand;
            self.upgrade = None;
        } else {
            self.upgrade = Some((cand, since));
        }
    }

    fn prune(&mut self, now: Instant) {
        let w = self.cfg.window;
        let old = |at: &Instant| now.duration_since(*at) > w;
        while self.rtts.front().is_some_and(|(at, _)| old(at)) {
            self.rtts.pop_front();
        }
        while self.losses.front().is_some_and(|(at, _, _)| now.duration_since(*at) > self.cfg.loss_window) {
            self.losses.pop_front();
        }
        while self.rates.front().is_some_and(|(at, _)| old(at)) {
            self.rates.pop_front();
        }
    }

    /// A periodic connection sample.
    pub fn on_sample(&mut self, now: Instant, s: Sample) {
        self.rtt = Some(s.rtt);
        self.rtts.push_back((now, s.rtt.min(RTT_CAP)));
        let outage = self.consecutive_misses > 0 || self.state == LinkState::Offline;
        let quiet = outage || self.quiet_until.is_some_and(|q| now < q);
        if let Some((_, prev)) = self.last_sample {
            let sent = s.sent_packets.saturating_sub(prev.sent_packets);
            let lost = s.lost_packets.saturating_sub(prev.lost_packets);
            if !quiet && sent > 0 {
                self.losses.push_back((now, sent, lost.min(sent)));
            }
        }
        // Throughput: delivered bytes over a span during which a transfer
        // ran throughout, at least a second and a few round trips long.
        match (s.busy_since, self.rate_start) {
            (Some(busy), Some((start, rx0))) if busy <= start && !outage => {
                let span = now.duration_since(start);
                let min_span = Duration::from_secs(1).max(s.rtt * 4);
                if span >= min_span {
                    let bytes = s.rx_bytes.saturating_sub(rx0);
                    // A span with nothing delivered is a stall, not a rate.
                    if bytes > 0 {
                        let bps = bytes as f64 * 8.0 / span.as_secs_f64();
                        self.rates.push_back((now, bps));
                    }
                    self.rate_start = Some((now, s.rx_bytes));
                }
            }
            (Some(_), _) if !outage => self.rate_start = Some((now, s.rx_bytes)),
            _ => self.rate_start = None,
        }
        self.prune(now);
        // The best recent rate: early samples of a transfer are held back by
        // slow start, later ones show what the link delivers.
        if let Some(best) = self.rates.iter().map(|(_, r)| *r).reduce(f64::max) {
            self.throughput = Some((best, now));
        }
        self.last_sample = Some((now, s));
        if self.established && self.state != LinkState::Offline {
            self.settle(now);
        }
    }

    /// A probe answered.
    pub fn on_probe_ok(&mut self, now: Instant, ping_rtt: Duration) {
        self.probes += 1;
        if self.consecutive_misses > 0 || self.state == LinkState::Offline {
            // Packets sent into the outage are declared lost now that acks
            // flow again; they say nothing about the link from here on.
            self.quiet_until = Some(now + self.cfg.outage_grace);
            self.losses.clear();
            self.rates.clear();
            self.rate_start = None;
        }
        self.consecutive_misses = 0;
        self.ping_rtt = Some(ping_rtt);
        self.settle(now);
    }

    pub fn on_probe_miss(&mut self) {
        self.probes += 1;
        self.misses += 1;
        self.consecutive_misses += 1;
        self.upgrade = None;
        self.rate_start = None;
        if self.consecutive_misses >= self.cfg.offline_after_misses {
            self.state = LinkState::Offline;
            self.established = true;
        }
    }

    /// The connection is gone (counters restart on the next one).
    pub fn on_connection_lost(&mut self) {
        self.last_sample = None;
        self.rate_start = None;
        self.rtt = None;
        self.rtts.clear();
    }

    pub fn snapshot(&self) -> LinkSnapshot {
        LinkSnapshot {
            state: self.state,
            rtt: self.rtt,
            ping_rtt: self.ping_rtt,
            loss: self.windowed_loss().unwrap_or(0.0),
            throughput_bps: self.throughput.map(|(b, _)| b),
            consecutive_misses: self.consecutive_misses,
            probes: self.probes,
            misses: self.misses,
        }
    }
}
