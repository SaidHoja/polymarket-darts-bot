//! Latency histograms for every stage of the live pipeline.
//!
//! ```text
//! venue ──book_lag/trade_lag──▶ feed task ──feed──▶ inbox ──queue──▶ core loop
//!       ◀──ws_rtt──                     (core asleep: ──wake──▶ core loop)
//! core loop ──core_pre──▶ tick ──tick──▶ ──core_post──▶ commands
//! input received ──input_to_drain──▶ core loop        └────── decide ──────┘
//! commands ──gw_handoff──▶ gateway ──gw_wait──▶ batch ──gw_spawn──▶ task ──gw_build──▶ HTTP
//!          ──rest_*──▶ response … private socket ──ack (emit → ack seen by core)
//! ```
//!
//! Lock-free: any thread records with relaxed atomics. Buckets are log-spaced
//! (~9% wide) from 1 µs to ~25 min in both directions (cross-clock stages can go
//! negative), so a percentile is accurate to about ±5%; |values| < 1 µs read as ±1 µs.
//! Each stage keeps a since-start histogram and a window that `report` resets; the last
//! `HISTORY` reported windows are kept for the UI's metrics page.
//!
//! A few stages are queue lengths rather than times (`Stage::is_count`). They share the same
//! histograms, recorded as 1 µs per item, so they get the same ±5% resolution; `Stage::show`
//! and `summary_json` convert them back to items.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::{json, Value};

/// Buckets per sign. Magnitude bucket i holds [GROWTH^i, GROWTH^(i+1)) µs; bucket 0 also takes < 1 µs.
const BUCKETS: usize = 256;
const GROWTH: f64 = 1.09;
/// Reported windows kept (one a minute: an hour).
const HISTORY: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Venue `transactTime` → our receive, books, excluding each market's subscribe snapshot.
    /// Cross-clock: includes our clock's offset from the venue's (check with `sntp time.apple.com`).
    BookLag,
    /// Venue `tradeTime` → our receive, trades. Cross-clock.
    TradeLag,
    /// WebSocket ping → pong, markets socket.
    WsRttMarkets,
    /// WebSocket ping → pong, private socket.
    WsRttPrivate,
    /// Market frame received → pushed into the inbox (raw journal, decode, conflation).
    Feed,
    /// Oldest event pushed into the inbox → drained by the core.
    Queue,
    /// Oldest event pushed into the inbox → the core thread running again, when it was asleep
    /// (not spinning). The OS wake-up cost that `core_spin_us` trades a CPU for.
    Wake,
    /// Input received → drained by the core, for inputs that led to an order command
    /// (feed + inbox wait, including any wake-up or earlier pass still running).
    InputToDrain,
    /// Core drains its inputs → tick starts (input attribution, journal copy), on passes that send orders.
    CorePre,
    /// `Core::tick` CPU time.
    Tick,
    /// Tick ends → its commands are handed to the gateway, on passes that send orders.
    CorePost,
    /// One whole core-loop iteration minus its wait: drain + tick + send + log + journal hand-off.
    CoreBusy,
    /// Oldest event of a tick → its commands handed to the gateway (queue + tick).
    Decide,
    /// Core hands a command to the gateway channel → the gateway task receives it (a task wake-up).
    GwHandoff,
    /// Command reaches the gateway → taken off its queue in a batch (priority queue + rate limiter).
    GwWait,
    /// Batch taken → its send task starts running (a task wake-up).
    GwSpawn,
    /// Send task starts → the order request is built, signed and handed to the HTTP client.
    GwBuild,
    RestCreate,
    RestModify,
    RestCancel,
    RestCancelAll,
    /// Reads: discovery, game poll, reconciliation queries.
    RestRead,
    /// Core emits a place/modify → core sees the venue's ack (whole round trip).
    Ack,
    InputToSend,
    InputToAck,
    /// Count: events the core drained in one pass, after book conflation (passes with none skipped).
    InboxDepth,
    /// Count: commands waiting in the gateway's queue each time it takes a batch to send.
    GwDepth,
}

impl Stage {
    pub const ALL: [Stage; 27] = [
        Stage::BookLag,
        Stage::TradeLag,
        Stage::WsRttMarkets,
        Stage::WsRttPrivate,
        Stage::Feed,
        Stage::Queue,
        Stage::Wake,
        Stage::InputToDrain,
        Stage::CorePre,
        Stage::Tick,
        Stage::CorePost,
        Stage::CoreBusy,
        Stage::Decide,
        Stage::GwHandoff,
        Stage::GwWait,
        Stage::GwSpawn,
        Stage::GwBuild,
        Stage::RestCreate,
        Stage::RestModify,
        Stage::RestCancel,
        Stage::RestCancelAll,
        Stage::RestRead,
        Stage::Ack,
        Stage::InputToSend,
        Stage::InputToAck,
        Stage::InboxDepth,
        Stage::GwDepth,
    ];

    /// Queue lengths, in items, rather than times.
    pub fn is_count(self) -> bool {
        matches!(self, Stage::InboxDepth | Stage::GwDepth)
    }

    /// A recorded value for display: ms for times, items for counts.
    pub fn show(self, ns: f64) -> String {
        if self.is_count() {
            items(ns)
        } else {
            ms(ns)
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Stage::BookLag => "book_lag",
            Stage::TradeLag => "trade_lag",
            Stage::WsRttMarkets => "ws_rtt_markets",
            Stage::WsRttPrivate => "ws_rtt_private",
            Stage::Feed => "feed",
            Stage::Queue => "queue",
            Stage::Wake => "wake",
            Stage::InputToDrain => "input_to_drain",
            Stage::CorePre => "core_pre",
            Stage::Tick => "tick",
            Stage::CorePost => "core_post",
            Stage::CoreBusy => "core_busy",
            Stage::Decide => "decide",
            Stage::GwHandoff => "gw_handoff",
            Stage::GwWait => "gw_wait",
            Stage::GwSpawn => "gw_spawn",
            Stage::GwBuild => "gw_build",
            Stage::RestCreate => "rest_create",
            Stage::RestModify => "rest_modify",
            Stage::RestCancel => "rest_cancel",
            Stage::RestCancelAll => "rest_cancel_all",
            Stage::RestRead => "rest_read",
            Stage::Ack => "ack",
            Stage::InputToSend => "input_to_send",
            Stage::InputToAck => "input_to_ack",
            Stage::InboxDepth => "inbox_depth",
            Stage::GwDepth => "gw_depth",
        }
    }

    /// One line for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Stage::BookLag => "Venue book timestamp → received (cross-clock)",
            Stage::TradeLag => "Venue trade timestamp → received (cross-clock)",
            Stage::WsRttMarkets => "Ping round trip, markets socket",
            Stage::WsRttPrivate => "Ping round trip, private socket",
            Stage::Feed => "Market frame received → in the inbox (journal, decode)",
            Stage::Queue => "Event waiting in the inbox for the core",
            Stage::Wake => "Sleeping core thread woken by input",
            Stage::InputToDrain => "Input received → picked up by the core (orders only)",
            Stage::CorePre => "Core picks up input → tick starts",
            Stage::Tick => "Core decision step (state, strategy, risk, orders)",
            Stage::CorePost => "Tick ends → commands handed to the gateway",
            Stage::CoreBusy => "Whole core-loop pass",
            Stage::Decide => "Oldest event → commands handed to the gateway",
            Stage::GwHandoff => "Core → gateway task receives the command",
            Stage::GwWait => "Gateway queue until taken in a batch",
            Stage::GwSpawn => "Batch taken → its send task runs",
            Stage::GwBuild => "Send task → request built, signed, handed to HTTP",
            Stage::RestCreate => "REST create order",
            Stage::RestModify => "REST modify order",
            Stage::RestCancel => "REST cancel order",
            Stage::RestCancelAll => "REST cancel all",
            Stage::RestRead => "REST reads (discovery, game poll, reconciliation)",
            Stage::Ack => "Order emitted → venue ack seen by the core",
            Stage::InputToSend => "Input received → order request sent",
            Stage::InputToAck => "Input received → venue ack",
            Stage::InboxDepth => "Events the core picks up in one pass (items)",
            Stage::GwDepth => "Commands queued in the gateway when it sends (items)",
        }
    }
}

pub struct Histogram {
    /// Index `BUCKETS - 1 - i` = negative magnitude bucket i; `BUCKETS + i` = positive bucket i.
    buckets: [AtomicU64; 2 * BUCKETS],
    count: AtomicU64,
    sum_ns: AtomicI64,
    max_ns: AtomicI64,
    /// Values < 0 (cross-clock stages with our clock behind the venue's).
    negative: AtomicU64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub count: u64,
    pub mean_ns: f64,
    pub p50_ns: f64,
    pub p90_ns: f64,
    pub p99_ns: f64,
    pub max_ns: i64,
    pub negative: u64,
}

fn magnitude_bucket(ns: u64) -> usize {
    let us = ns as f64 / 1_000.0;
    if us <= 1.0 {
        return 0;
    }
    ((us.ln() / GROWTH.ln()) as usize).min(BUCKETS - 1)
}

fn bucket_of(ns: i64) -> usize {
    if ns < 0 {
        BUCKETS - 1 - magnitude_bucket(ns.unsigned_abs())
    } else {
        BUCKETS + magnitude_bucket(ns as u64)
    }
}

/// Geometric midpoint of a bucket, in ns.
fn bucket_value(index: usize) -> f64 {
    let mid = |i: usize| GROWTH.powf(i as f64 + 0.5) * 1_000.0;
    if index < BUCKETS {
        -mid(BUCKETS - 1 - index)
    } else {
        mid(index - BUCKETS)
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_ns: AtomicI64::new(0),
            max_ns: AtomicI64::new(i64::MIN),
            negative: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    pub fn record(&self, ns: i64) {
        self.buckets[bucket_of(ns)].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        if ns < 0 {
            self.negative.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Read, and optionally reset. Concurrent records may straddle the reset; that's fine for monitoring.
    pub fn summary(&self, reset: bool) -> Summary {
        let take = |a: &AtomicU64| {
            if reset {
                a.swap(0, Ordering::Relaxed)
            } else {
                a.load(Ordering::Relaxed)
            }
        };
        let counts: Vec<u64> = self.buckets.iter().map(take).collect();
        let count = take(&self.count);
        let negative = take(&self.negative);
        let sum = if reset {
            self.sum_ns.swap(0, Ordering::Relaxed)
        } else {
            self.sum_ns.load(Ordering::Relaxed)
        };
        let max = if reset {
            self.max_ns.swap(i64::MIN, Ordering::Relaxed)
        } else {
            self.max_ns.load(Ordering::Relaxed)
        };
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return Summary::default();
        }
        let pct = |q: f64| {
            let rank = ((q * total as f64).ceil() as u64).max(1);
            let mut seen = 0;
            for (i, c) in counts.iter().enumerate() {
                seen += c;
                if seen >= rank {
                    return bucket_value(i).min(max as f64);
                }
            }
            max as f64
        };
        Summary {
            count,
            mean_ns: sum as f64 / count.max(1) as f64,
            p50_ns: pct(0.50),
            p90_ns: pct(0.90),
            p99_ns: pct(0.99),
            max_ns: max,
            negative,
        }
    }
}

#[derive(Default)]
struct Pair {
    total: Histogram,
    window: Histogram,
}

/// One reported window.
#[derive(Debug, Clone)]
pub struct Window {
    pub wall_ns: i64,
    pub stages: Vec<(Stage, Summary)>,
}

impl Window {
    pub fn get(&self, stage: Stage) -> Option<&Summary> {
        self.stages
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, m)| m)
    }
}

#[derive(Default)]
pub struct Metrics {
    stages: [Pair; Stage::ALL.len()],
    history: Mutex<VecDeque<Window>>,
}

/// Nanoseconds per item for count stages.
const NS_PER_ITEM: f64 = 1_000.0;

/// Queue lengths are whole numbers: a percentile rounds to the nearest item (exact up to ~10,
/// ±5% above). Means keep one decimal.
fn items(ns: f64) -> String {
    format!("{:.0}", ns / NS_PER_ITEM)
}

pub(crate) fn ms(ns: f64) -> String {
    let ms = ns / 1e6;
    if ms.abs() >= 100.0 {
        format!("{ms:.0}")
    } else if ms.abs() >= 1.0 {
        format!("{ms:.1}")
    } else {
        format!("{ms:.3}")
    }
}

impl Metrics {
    pub fn record(&self, stage: Stage, ns: i64) {
        let pair = &self.stages[stage as usize];
        pair.total.record(ns);
        pair.window.record(ns);
    }

    /// A queue length, for a count stage.
    pub fn record_count(&self, stage: Stage, items: usize) {
        debug_assert!(stage.is_count());
        self.record(stage, (items as f64 * NS_PER_ITEM) as i64);
    }

    pub fn record_since(&self, stage: Stage, since: std::time::Instant) {
        self.record(stage, since.elapsed().as_nanos() as i64);
    }

    fn summaries(&self, window: bool, reset: bool) -> Vec<(Stage, Summary)> {
        Stage::ALL
            .iter()
            .map(|s| {
                let pair = &self.stages[*s as usize];
                (
                    *s,
                    if window {
                        pair.window.summary(reset)
                    } else {
                        pair.total.summary(false)
                    },
                )
            })
            .filter(|(_, sum)| sum.count > 0)
            .collect()
    }

    /// Since-start summaries of every stage that has recorded something.
    pub fn totals(&self) -> Vec<(Stage, Summary)> {
        self.summaries(false, false)
    }

    /// Reported windows, oldest first.
    pub fn history(&self) -> Vec<Window> {
        self.history.lock().unwrap().iter().cloned().collect()
    }

    /// One log line of window p50/p99 (ms) per stage; resets the window and keeps it in the history.
    pub fn report_line(&self, wall_ns: i64) -> (String, Value) {
        let window = self.summaries(true, true);
        {
            let mut history = self.history.lock().unwrap();
            if history.len() == HISTORY {
                history.pop_front();
            }
            history.push_back(Window {
                wall_ns,
                stages: window.clone(),
            });
        }
        let line = window
            .iter()
            .map(|(s, m)| format!("{} {}/{}", s.name(), s.show(m.p50_ns), s.show(m.p99_ns)))
            .collect::<Vec<_>>()
            .join(" | ");
        let json = Value::Object(
            window
                .iter()
                .map(|(s, m)| (s.name().to_string(), summary_json(*s, m)))
                .collect(),
        );
        (format!("p50/p99 (ms, or items for *_depth): {line}"), json)
    }

    /// Full since-start table, for the admin socket.
    pub fn table(&self) -> String {
        let mut out = format!(
            "{:<16} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9}\n",
            "stage (ms/items)", "n", "mean", "p50", "p90", "p99", "max"
        );
        for (s, m) in self.summaries(false, false) {
            let _ = write!(
                out,
                "{:<16} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9}",
                s.name(),
                m.count,
                s.show(m.mean_ns),
                s.show(m.p50_ns),
                s.show(m.p90_ns),
                s.show(m.p99_ns),
                s.show(m.max_ns as f64)
            );
            if m.negative > 0 {
                let _ = write!(
                    out,
                    "  ({} < 0: our clock is behind the venue's)",
                    m.negative
                );
            }
            out.push('\n');
        }
        out
    }
}

/// Times as `*_ms`; counts as plain `mean`/`p50`/… in items.
pub(crate) fn summary_json(stage: Stage, m: &Summary) -> Value {
    if stage.is_count() {
        let n = |ns: f64| (ns / NS_PER_ITEM).round();
        return json!({"n": m.count, "mean": m.mean_ns / NS_PER_ITEM, "p50": n(m.p50_ns), "p90": n(m.p90_ns), "p99": n(m.p99_ns), "max": n(m.max_ns as f64)});
    }
    json!({"n": m.count, "mean_ms": m.mean_ns / 1e6, "p50_ms": m.p50_ns / 1e6, "p90_ms": m.p90_ns / 1e6, "p99_ms": m.p99_ns / 1e6, "max_ms": m.max_ns as f64 / 1e6})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_within_bucket_resolution() {
        let h = Histogram::default();
        for i in 1..=1000 {
            h.record(i * 1_000_000); // 1..1000 ms
        }
        let s = h.summary(false);
        assert_eq!(s.count, 1000);
        assert!((s.p50_ns / 500e6 - 1.0).abs() < 0.06, "{}", s.p50_ns);
        assert!((s.p99_ns / 990e6 - 1.0).abs() < 0.06, "{}", s.p99_ns);
        assert_eq!(s.max_ns, 1_000_000_000);
        assert!((s.mean_ns - 500.5e6).abs() < 1.0);
    }

    #[test]
    fn negative_values_keep_their_order() {
        let h = Histogram::default();
        for ms in [-200i64, -190, -180, 10, 20] {
            h.record(ms * 1_000_000);
        }
        let s = h.summary(false);
        assert_eq!(s.negative, 3);
        assert!((s.p50_ns / -190e6 - 1.0).abs() < 0.06, "{}", s.p50_ns);
        assert!(s.p99_ns > 15e6, "{}", s.p99_ns);
    }

    #[test]
    fn window_resets_and_total_keeps() {
        let m = Metrics::default();
        m.record(Stage::Tick, 50_000);
        m.record(Stage::BookLag, -3_000_000);
        let (line, _) = m.report_line(1);
        assert!(line.contains("tick") && line.contains("book_lag"));
        assert_eq!(m.report_line(2).0, "p50/p99 (ms, or items for *_depth): ");
        let history = m.history();
        assert_eq!(
            history.iter().map(|w| w.wall_ns).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(history[0].get(Stage::Tick).map(|s| s.count), Some(1));
        assert!(history[1].stages.is_empty());
        let table = m.table();
        assert!(table.contains("tick") && table.contains("clock is behind"));
    }

    #[test]
    fn counts_read_back_in_items() {
        let m = Metrics::default();
        for n in [1, 1, 2, 3, 40] {
            m.record_count(Stage::InboxDepth, n);
        }
        let (line, json) = m.report_line(1);
        let s = &json["inbox_depth"];
        assert_eq!(s["n"], 5);
        assert_eq!(s["max"], 40.0);
        assert_eq!(
            (s["p50"].as_f64(), s["mean"].as_f64()),
            (Some(2.0), Some(9.4)),
            "{s}"
        );
        assert!(s.get("p50_ms").is_none());
        assert!(line.contains("inbox_depth 2/"), "{line}"); // p99 of 40 reads 39: ±5% above ~10 items
    }
}
