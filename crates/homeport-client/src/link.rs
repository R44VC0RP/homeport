//! Link-quality estimator with hysteresis.
//!
//! Downgrades are immediate (Offline only after several missed probes);
//! upgrades need consistently better probes over a window, except that one
//! answered probe lifts Offline to Degraded so requests stop failing fast.

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
    /// Better probes needed, spanning at least `upgrade_window`, to climb a state.
    pub upgrade_probes: u32,
    pub upgrade_window: Duration,
    /// Throughput samples older than this are ignored.
    pub throughput_max_age: Duration,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            offline_after_misses: 3,
            upgrade_probes: 3,
            upgrade_window: Duration::from_secs(15),
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
    /// Smoothed packet loss fraction.
    pub loss: f64,
    /// Recent throughput estimate in bits per second.
    pub throughput_bps: Option<f64>,
    pub consecutive_misses: u32,
    pub probes: u64,
    pub misses: u64,
}

#[derive(Debug)]
pub struct LinkEstimator {
    cfg: LinkConfig,
    state: LinkState,
    established: bool,
    consecutive_misses: u32,
    upgrade: Option<(LinkState, Instant, u32)>,
    rtt: Option<Duration>,
    ping_rtt: Option<Duration>,
    loss: f64,
    last_counts: Option<(u64, u64)>,
    throughput: Option<(f64, Instant)>,
    probes: u64,
    misses: u64,
}

impl LinkEstimator {
    pub fn new(cfg: LinkConfig) -> Self {
        Self {
            cfg,
            state: LinkState::Offline,
            established: false,
            consecutive_misses: 0,
            upgrade: None,
            rtt: None,
            ping_rtt: None,
            loss: 0.0,
            last_counts: None,
            throughput: None,
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

    fn classify(&self, now: Instant) -> LinkState {
        let rtt = self.rtt.or(self.ping_rtt).unwrap_or_default();
        let tput =
            self.throughput.filter(|(_, at)| now.duration_since(*at) < self.cfg.throughput_max_age).map(|(bps, _)| bps);
        let ms = rtt.as_secs_f64() * 1000.0;
        if ms > 1400.0 || (ms > 270.0 && tput.is_some_and(|t| t < 700_000.0)) {
            LinkState::Poor
        } else if ms > 300.0 || self.loss > 0.03 || tput.is_some_and(|t| t < 2_000_000.0) {
            LinkState::Degraded
        } else {
            LinkState::Good
        }
    }

    fn settle(&mut self, now: Instant, target: LinkState) {
        if !self.established {
            self.established = true;
            self.state = target;
            self.upgrade = None;
            return;
        }
        if self.state == LinkState::Offline {
            // The server answered: stop failing fast at once. Climbing past
            // Degraded still needs the usual run of good probes.
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
        let (cand, since, n) = match self.upgrade {
            // Track the worst state seen while climbing.
            Some((c, since, n)) => (c.max(target), since, n + 1),
            None => (target, now, 1),
        };
        if n >= self.cfg.upgrade_probes && now.duration_since(since) >= self.cfg.upgrade_window {
            self.state = cand;
            self.upgrade = None;
        } else {
            self.upgrade = Some((cand, since, n));
        }
    }

    /// A probe answered. `counts` are QUIC's cumulative (sent, lost) packets.
    pub fn on_probe_ok(&mut self, now: Instant, ping_rtt: Duration, quic_rtt: Option<Duration>, counts: (u64, u64)) {
        self.probes += 1;
        self.consecutive_misses = 0;
        self.ping_rtt = Some(ping_rtt);
        self.rtt = quic_rtt;
        if let Some((sent0, lost0)) = self.last_counts {
            let sent = counts.0.saturating_sub(sent0);
            let lost = counts.1.saturating_sub(lost0);
            if sent > 0 {
                let sample = lost as f64 / sent as f64;
                self.loss = self.loss * 0.7 + sample * 0.3;
            }
        }
        self.last_counts = Some(counts);
        let target = self.classify(now);
        self.settle(now, target);
    }

    pub fn on_probe_miss(&mut self) {
        self.probes += 1;
        self.misses += 1;
        self.consecutive_misses += 1;
        self.upgrade = None;
        if self.consecutive_misses >= self.cfg.offline_after_misses {
            self.state = LinkState::Offline;
            self.established = true;
        }
    }

    /// The connection is gone (counts restart on the next one).
    pub fn on_connection_lost(&mut self) {
        self.last_counts = None;
        self.rtt = None;
    }

    pub fn on_transfer(&mut self, now: Instant, bytes: u64, elapsed: Duration) {
        if bytes < 256 * 1024 || elapsed.is_zero() {
            return;
        }
        let bps = bytes as f64 * 8.0 / elapsed.as_secs_f64();
        let bps = match self.throughput {
            Some((old, _)) => old * 0.5 + bps * 0.5,
            None => bps,
        };
        self.throughput = Some((bps, now));
    }

    pub fn snapshot(&self) -> LinkSnapshot {
        LinkSnapshot {
            state: self.state,
            rtt: self.rtt,
            ping_rtt: self.ping_rtt,
            loss: self.loss,
            throughput_bps: self.throughput.map(|(b, _)| b),
            consecutive_misses: self.consecutive_misses,
            probes: self.probes,
            misses: self.misses,
        }
    }
}
