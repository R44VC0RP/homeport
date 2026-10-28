//! Impairment parameters, named profiles and the `set key=value` syntax.

use std::fmt;
use std::time::Duration;

/// Gilbert-Elliott burst loss: a two-state Markov chain evaluated per packet.
#[derive(Clone, Debug, PartialEq)]
pub struct Ge {
    /// P(good → bad) per packet.
    pub p: f64,
    /// P(bad → good) per packet.
    pub r: f64,
    pub loss_good: f64,
    pub loss_bad: f64,
}

impl Ge {
    /// Bursty loss averaging `mean`, with bad runs of `burst` packets on
    /// average during which half the packets are lost.
    pub fn bursty(mean: f64, burst: f64) -> Self {
        let loss_bad = 0.5;
        let r = 1.0 / burst.max(1.0);
        let pi_bad = (mean / loss_bad).clamp(0.0, 0.99);
        Self { p: r * pi_bad / (1.0 - pi_bad), r, loss_good: 0.0, loss_bad }
    }

    pub fn mean_loss(&self) -> f64 {
        let pi_bad = if self.p + self.r > 0.0 { self.p / (self.p + self.r) } else { 0.0 };
        pi_bad * self.loss_bad + (1.0 - pi_bad) * self.loss_good
    }
}

/// One direction of the link.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dir {
    /// One-way propagation delay.
    pub delay: Duration,
    /// Uniform jitter: each packet gets `delay ± jitter`.
    pub jitter: Duration,
    /// Independent random loss.
    pub loss: f64,
    pub ge: Option<Ge>,
    /// Bottleneck rate; `None` is unlimited.
    pub rate_bps: Option<u64>,
    /// Bottleneck queue limit (tail drop); `None` picks 200 ms of the rate, at least 64 KB.
    pub queue_bytes: Option<u64>,
    /// Token bucket depth (bytes that pass at line rate after idle).
    pub burst_bytes: u64,
    pub dup: f64,
    /// Let jitter reorder packets (otherwise delivery stays FIFO).
    pub reorder: bool,
}

pub const DEFAULT_BURST: u64 = 3000;

impl Dir {
    fn new(delay: Duration, jitter: Duration, loss: f64, rate_bps: Option<u64>) -> Self {
        Self { delay, jitter, loss, rate_bps, burst_bytes: DEFAULT_BURST, ..Self::default() }
    }

    pub fn queue_limit(&self) -> u64 {
        match (self.queue_bytes, self.rate_bps) {
            (Some(q), _) => q,
            (None, Some(r)) => (r / 8 / 5).max(64 * 1024),
            (None, None) => u64::MAX,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RandomOutage {
    pub min: Duration,
    pub max: Duration,
    /// Mean gap between outages (exponential).
    pub mean_gap: Duration,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outages {
    /// `(period, length)`: dark for the last `length` of every `period`.
    pub every: Option<(Duration, Duration)>,
    pub random: Option<RandomOutage>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    /// Client → server.
    pub up: Dir,
    /// Server → client.
    pub down: Dir,
    pub outages: Outages,
}

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

/// Symmetric delay given as added RTT ± RTT jitter (split evenly between
/// the directions), loss per direction, rates as down/up.
fn link(name: &str, rtt: u64, rtt_jitter: u64, loss: f64, down: Option<u64>, up: Option<u64>) -> Profile {
    let half = |v: u64| Duration::from_micros(v * 500);
    Profile {
        name: name.into(),
        up: Dir::new(half(rtt), half(rtt_jitter), loss, up),
        down: Dir::new(half(rtt), half(rtt_jitter), loss, down),
        outages: Outages::default(),
    }
}

pub const NAMES: &[&str] =
    &["off", "good-wifi", "busy-wifi", "lte", "bad-cell", "edge", "derp-relay", "flaky", "train"];

pub fn named(name: &str) -> Option<Profile> {
    const M: u64 = 1_000_000;
    const K: u64 = 1_000;
    let mut p = match name {
        "off" => link(name, 0, 0, 0.0, None, None),
        "good-wifi" => link(name, 5, 0, 0.0, None, None),
        "busy-wifi" => link(name, 30, 20, 0.01, Some(30 * M), Some(30 * M)),
        "lte" => link(name, 50, 15, 0.005, Some(20 * M), Some(8 * M)),
        "bad-cell" => {
            let mut p = link(name, 150, 80, 0.0, Some(1_500 * K), Some(500 * K));
            for d in [&mut p.up, &mut p.down] {
                d.ge = Some(Ge::bursty(0.03, 4.0));
                d.queue_bytes = Some(256 * 1024);
            }
            p
        }
        "edge" => link(name, 400, 150, 0.08, Some(200 * K), Some(100 * K)),
        "derp-relay" => link(name, 60, 0, 0.0, Some(8 * M), Some(8 * M)),
        "flaky" => {
            let mut p = named("lte")?;
            p.outages.every = Some((ms(30_000), ms(5_000)));
            p
        }
        "train" => {
            let mut p = named("bad-cell")?;
            p.outages.random = Some(RandomOutage { min: ms(2_000), max: ms(20_000), mean_gap: ms(45_000) });
            p
        }
        _ => return None,
    };
    p.name = name.into();
    Some(p)
}

pub fn fmt_dur(d: Duration) -> String {
    let us = d.as_micros();
    if us == 0 {
        "0".into()
    } else if us.is_multiple_of(1_000_000) {
        format!("{}s", us / 1_000_000)
    } else if us.is_multiple_of(1000) {
        format!("{}ms", us / 1000)
    } else {
        format!("{:.1}ms", us as f64 / 1000.0)
    }
}

pub fn fmt_rate(bps: u64) -> String {
    if bps >= 1_000_000 { format!("{}mbit", bps as f64 / 1e6) } else { format!("{}kbit", bps as f64 / 1e3) }
}

impl fmt::Display for Dir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "delay={} jitter={}", fmt_dur(self.delay), fmt_dur(self.jitter))?;
        if self.loss > 0.0 {
            write!(f, " loss={}%", self.loss * 100.0)?;
        }
        if let Some(ge) = &self.ge {
            write!(
                f,
                " burstloss={:.2}%/{:.1} (p={:.4} r={:.3} h={})",
                ge.mean_loss() * 100.0,
                1.0 / ge.r,
                ge.p,
                ge.r,
                ge.loss_bad
            )?;
        }
        match self.rate_bps {
            Some(r) => {
                write!(f, " rate={} queue={}KB burst={}B", fmt_rate(r), self.queue_limit() / 1024, self.burst_bytes)?
            }
            None => write!(f, " rate=unlimited")?,
        }
        if self.dup > 0.0 {
            write!(f, " dup={}%", self.dup * 100.0)?;
        }
        if self.reorder {
            write!(f, " reorder=on")?;
        }
        Ok(())
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "profile {}", self.name)?;
        writeln!(f, "  up   {}", self.up)?;
        writeln!(f, "  down {}", self.down)?;
        if let Some((period, len)) = self.outages.every {
            writeln!(f, "  blackout {} every {}", fmt_dur(len), fmt_dur(period))?;
        }
        if let Some(r) = &self.outages.random {
            writeln!(f, "  random blackouts {}-{}, mean gap {}", fmt_dur(r.min), fmt_dur(r.max), fmt_dur(r.mean_gap))?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

fn bad(what: &str, v: &str) -> ParseError {
    ParseError(format!("bad {what} {v:?}"))
}

fn split_num(v: &str) -> (&str, &str) {
    let i = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
    (&v[..i], v[i..].trim())
}

/// `250ms`, `1.5s`, `2m`, `800us`; a bare number is milliseconds.
pub fn parse_dur(v: &str) -> Result<Duration, ParseError> {
    let (n, unit) = split_num(v);
    let n: f64 = n.parse().map_err(|_| bad("duration", v))?;
    let secs = match unit {
        "" | "ms" => n / 1e3,
        "us" | "µs" => n / 1e6,
        "s" => n,
        "m" | "min" => n * 60.0,
        _ => return Err(bad("duration", v)),
    };
    Ok(Duration::from_secs_f64(secs))
}

/// `1%` or `0.01`.
pub fn parse_frac(v: &str) -> Result<f64, ParseError> {
    let f = match v.strip_suffix('%') {
        Some(p) => p.parse::<f64>().map(|x| x / 100.0),
        None => v.parse::<f64>(),
    }
    .map_err(|_| bad("fraction", v))?;
    if (0.0..=1.0).contains(&f) { Ok(f) } else { Err(bad("fraction", v)) }
}

/// `1.5mbit`, `200kbit`, `30M` (bits per second); `0`/`off` is unlimited.
pub fn parse_rate(v: &str) -> Result<Option<u64>, ParseError> {
    if v == "0" || v == "off" || v == "unlimited" {
        return Ok(None);
    }
    let (n, unit) = split_num(v);
    let n: f64 = n.parse().map_err(|_| bad("rate", v))?;
    let mul = match unit.to_ascii_lowercase().trim_end_matches("/s").trim_end_matches("ps") {
        "" | "bit" | "b" => 1.0,
        "k" | "kbit" | "kb" => 1e3,
        "m" | "mbit" | "mb" => 1e6,
        "g" | "gbit" | "gb" => 1e9,
        _ => return Err(bad("rate", v)),
    };
    Ok(Some((n * mul) as u64))
}

/// `256k`, `1M`, `64KB`, `1500` (bytes, binary multiples).
pub fn parse_bytes(v: &str) -> Result<u64, ParseError> {
    let (n, unit) = split_num(v);
    let n: f64 = n.parse().map_err(|_| bad("size", v))?;
    let mul = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        _ => return Err(bad("size", v)),
    };
    Ok((n * mul) as u64)
}

fn parse_on(v: &str) -> Result<bool, ParseError> {
    match v {
        "on" | "1" | "true" | "yes" => Ok(true),
        "off" | "0" | "false" | "no" => Ok(false),
        _ => Err(bad("switch", v)),
    }
}

/// Applies `set` arguments: `[up|down|both] key=value ...`. Direction words
/// switch the target for the keys after them (default both).
pub fn apply_set(p: &mut Profile, args: &[&str]) -> Result<(), ParseError> {
    let (mut up, mut down) = (true, true);
    for a in args {
        match *a {
            "up" => (up, down) = (true, false),
            "down" => (up, down) = (false, true),
            "both" => (up, down) = (true, true),
            kv => {
                let (k, v) = kv.split_once('=').ok_or_else(|| ParseError(format!("expected key=value, got {kv:?}")))?;
                match k {
                    "every" | "blackout-every" => {
                        p.outages.every = if v == "off" {
                            None
                        } else {
                            let (len, period) = v.split_once('/').ok_or_else(|| bad("every (LEN/PERIOD)", v))?;
                            let (len, period) = (parse_dur(len)?, parse_dur(period)?);
                            if period.is_zero() || len > period {
                                return Err(bad("every (LEN/PERIOD)", v));
                            }
                            Some((period, len))
                        };
                    }
                    "random" | "random-blackouts" => {
                        p.outages.random = if v == "off" {
                            None
                        } else {
                            let (range, gap) = v.split_once('/').ok_or_else(|| bad("random (MIN-MAX/GAP)", v))?;
                            let (min, max) = range.split_once('-').ok_or_else(|| bad("random (MIN-MAX/GAP)", v))?;
                            let (min, max, mean_gap) = (parse_dur(min)?, parse_dur(max)?, parse_dur(gap)?);
                            if max < min || mean_gap.is_zero() {
                                return Err(bad("random (MIN-MAX/GAP)", v));
                            }
                            Some(RandomOutage { min, max, mean_gap })
                        };
                    }
                    _ => {
                        let mut dirs = Vec::new();
                        if up {
                            dirs.push(&mut p.up);
                        }
                        if down {
                            dirs.push(&mut p.down);
                        }
                        for d in dirs {
                            set_dir(d, k, v)?;
                        }
                    }
                }
            }
        }
    }
    p.name = format!("{}*", p.name.trim_end_matches('*'));
    Ok(())
}

fn set_dir(d: &mut Dir, k: &str, v: &str) -> Result<(), ParseError> {
    match k {
        "delay" => d.delay = parse_dur(v)?,
        "jitter" => d.jitter = parse_dur(v)?,
        "loss" => d.loss = parse_frac(v)?,
        "burstloss" => {
            d.ge = if v == "off" {
                None
            } else {
                let (mean, len) = v.split_once('/').unwrap_or((v, "4"));
                let len: f64 = len.parse().map_err(|_| bad("burst length", len))?;
                Some(Ge::bursty(parse_frac(mean)?, len))
            }
        }
        "rate" => d.rate_bps = parse_rate(v)?,
        "queue" => d.queue_bytes = if v == "auto" { None } else { Some(parse_bytes(v)?) },
        "burst" => d.burst_bytes = parse_bytes(v)?,
        "dup" => d.dup = parse_frac(v)?,
        "reorder" => d.reorder = parse_on(v)?,
        _ => return Err(ParseError(format!("unknown key {k:?}"))),
    }
    Ok(())
}
