//! Congestion control: the configured controller, with its window held to
//! about two bandwidth-delay products, and to one while latency-sensitive
//! requests are running.
//!
//! Stream priorities only order what leaves this server; bulk data already
//! in flight sits in the bottleneck's queue (Wi-Fi, cellular, a DERP relay)
//! in front of any urgent answer. Both Cubic and quinn's BBR keep several
//! bandwidth-delay products in flight there (busy-wifi: ~650 KB queued,
//! +200 ms for every listing during a download). While the connection has
//! an urgent request (and for a moment after, so browsing keeps the queue
//! short), the window is capped at about one BDP: the measured delivery
//! rate times the path's minimum RTT plus some slack for jitter. Bulk
//! transfers lose a little rate for that time; urgent requests stop waiting
//! behind them.

use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;

/// The cap stays this long after the last urgent request ends.
const LINGER: Duration = Duration::from_secs(1);
/// Delivery-rate samples older than this are dropped.
const RATE_WINDOW: Duration = Duration::from_secs(3);
/// Without urgent requests the window is still held to this many BDPs:
/// quinn's BBR otherwise keeps far more in flight on slow links (bad-cell:
/// 650 KB against a 28 KB BDP, a 1.4 s queue), and an urgent request that
/// arrives then waits for that queue to drain.
const BULK_GAIN: u64 = 2;
/// Smallest capped window, in datagrams.
const MIN_PACKETS: u64 = 10;

/// Urgent-request state shared between a connection's handlers and its
/// controller.
#[derive(Default)]
pub struct Urgency {
    active: AtomicUsize,
    until: Mutex<Option<Instant>>,
}

impl Urgency {
    /// Marks an urgent request as running until the guard drops.
    pub fn begin(self: &Arc<Self>) -> UrgentGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        UrgentGuard(self.clone())
    }

    fn on(&self) -> bool {
        self.active.load(Ordering::Relaxed) > 0
            || self.until.lock().unwrap_or_else(|p| p.into_inner()).is_some_and(|u| Instant::now() < u)
    }
}

pub struct UrgentGuard(Arc<Urgency>);

impl Drop for UrgentGuard {
    fn drop(&mut self) {
        *self.0.until.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now() + LINGER);
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct CappedFactory {
    pub inner: Arc<dyn ControllerFactory + Send + Sync>,
}

impl ControllerFactory for CappedFactory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Capped {
            inner: self.inner.clone().build(now, current_mtu),
            urgency: Arc::default(),
            mtu: u64::from(current_mtu),
            min_rtt: None,
            interval: None,
            rates: Vec::new(),
        })
    }
}

/// The urgency handle of a connection built by [`CappedFactory`].
pub fn urgency(conn: &quinn::Connection) -> Option<Arc<Urgency>> {
    conn.congestion_state().into_any().downcast::<Capped>().ok().map(|c| c.urgency)
}

struct Capped {
    inner: Box<dyn Controller>,
    urgency: Arc<Urgency>,
    mtu: u64,
    min_rtt: Option<Duration>,
    /// Current delivery-rate interval: `(start, bytes acked)`.
    interval: Option<(Instant, u64)>,
    /// `(at, bytes per second)`.
    rates: Vec<(Instant, f64)>,
}

impl Capped {
    fn cap(&self) -> Option<u64> {
        let rate = self.rates.iter().map(|(_, r)| *r).reduce(f64::max)?;
        let min_rtt = self.min_rtt?;
        // Jitter slack: a quarter of the RTT, at least 10 ms.
        let rtt = min_rtt + (min_rtt / 4).max(Duration::from_millis(10));
        Some(((rate * rtt.as_secs_f64()) as u64).max(MIN_PACKETS * self.mtu))
    }
}

impl Controller for Capped {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(&mut self, now: Instant, sent: Instant, bytes: u64, app_limited: bool, rtt: &RttEstimator) {
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
        self.min_rtt = Some(rtt.min());
        // Delivery rate over intervals of at least one smoothed RTT.
        let span = rtt.get().max(Duration::from_millis(20));
        // Under the cap the rate only shows the cap, so the samples taken
        // before it hold until it lifts.
        let capped = self.urgency.on();
        match &mut self.interval {
            Some((start, acked)) => {
                *acked += bytes;
                let el = now.saturating_duration_since(*start);
                if el >= span {
                    if !app_limited && !capped {
                        self.rates.push((now, *acked as f64 / el.as_secs_f64()));
                    }
                    tracing::trace!(
                        rate_kbit = *acked as f64 * 8.0 / el.as_secs_f64() / 1000.0,
                        app_limited,
                        capped,
                        cap = ?self.cap(),
                        window = self.inner.window(),
                        "delivery sample"
                    );
                    self.interval = Some((now, 0));
                }
            }
            None => self.interval = Some((now, 0)),
        }
        if !capped {
            self.rates.retain(|(at, _)| now.saturating_duration_since(*at) < RATE_WINDOW);
        }
    }

    fn on_end_acks(&mut self, now: Instant, in_flight: u64, app_limited: bool, largest: Option<u64>) {
        self.inner.on_end_acks(now, in_flight, app_limited, largest);
    }

    fn on_congestion_event(&mut self, now: Instant, sent: Instant, persistent: bool, lost_bytes: u64) {
        self.inner.on_congestion_event(now, sent, persistent, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = u64::from(new_mtu);
        self.inner.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        let w = self.inner.window();
        match self.cap() {
            Some(cap) if self.urgency.on() => w.min(cap),
            Some(cap) => w.min(cap * BULK_GAIN),
            None => w,
        }
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Capped {
            inner: self.inner.clone_box(),
            urgency: self.urgency.clone(),
            mtu: self.mtu,
            min_rtt: self.min_rtt,
            interval: self.interval,
            rates: self.rates.clone(),
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}
