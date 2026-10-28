//! CPU cost of the live shell around the core: feed hand-off, inbox, metrics, journal and UI.
//! `pm-live` is a binary, so these are ignored tests rather than criterion benches:
//!
//!   cargo test --release -p pm-live bench -- --ignored --nocapture --test-threads=1

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use pm_core::{Clock, Config, Core};
use pm_types::*;
use pm_venue::decode::{decode_market_frame, MarketMsg};

use crate::inbox::Inbox;
use crate::journal::Journal;
use crate::metrics::{Metrics, Stage};

pub(crate) const SEC: u64 = 1_000_000_000;

/// Median of 7 runs of `iters` calls, in ns per call.
pub(crate) fn time(name: &str, iters: u32, mut f: impl FnMut()) -> f64 {
    for _ in 0..iters / 10 + 1 {
        f();
    }
    let mut runs: Vec<f64> = (0..7)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..iters {
                f();
            }
            t.elapsed().as_nanos() as f64 / iters as f64
        })
        .collect();
    runs.sort_by(f64::total_cmp);
    let ns = runs[3];
    println!("BENCH {name:<44} {:>10.2} µs", ns / 1e3);
    ns
}

/// The real 40-level book frame from a capture.
pub(crate) fn raw_frame() -> String {
    let line = include_str!("../../../tests/fixtures/market_frame.jsonl")
        .lines()
        .next()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    v["raw"].as_str().unwrap().to_string()
}

fn book_frame() -> BookFrame {
    let MarketMsg::Book(b) = decode_market_frame(&raw_frame()).unwrap() else {
        unreachable!()
    };
    b
}

fn params(i: usize) -> MarketParams {
    MarketParams {
        slug: Arc::from(format!("aec-pdcdarts-m{i}-2026-10-23")),
        title: format!("A{i} vs B{i}"),
        event_id: format!("ev{i}"),
        event_slug: format!("pdcdarts-m{i}"),
        league: "pdcdarts".into(),
        sports_market_type: "darts_match_winner".into(),
        tick: Px(100),
        min_qty: Qty(100),
        fee_coefficient: 0.0695,
        game_start_ns: Some(1_000_000 * SEC as i64),
        yes_name: Some(format!("A{i}")),
        no_name: Some(format!("B{i}")),
        yes_ordering: Some(TeamOrdering::Home),
        is_futures: false,
    }
}

fn book(i: usize, mid: i64) -> Event {
    let bids = (0..40)
        .map(|k| (Px(mid - 500 - 100 * k), Qty(50_000 + 10_000 * k)))
        .collect();
    let offers = (0..40)
        .map(|k| (Px(mid + 500 + 100 * k), Qty(50_000 + 10_000 * k)))
        .collect();
    Event::Book(BookFrame {
        slug: params(i).slug,
        bids,
        offers,
        state: MarketState::Open,
        transact_ns: Some(1),
        shares_traded: Some(Qty::ZERO),
        settlement: None,
    })
}

/// A core quoting `n` markets, as live.
pub(crate) fn quoting_core(n: usize) -> Core {
    let mut core = Core::new(Config::default());
    let mut startup = vec![
        Event::Markets((0..n).map(params).collect()),
        Event::Connection {
            feed: Feed::Markets,
            state: ConnState::Up,
        },
        Event::OrderSnapshot(vec![]),
        Event::Positions(vec![]),
        Event::Balance(BalanceFrame {
            current_balance: 1_000.0,
            buying_power: 1_000.0,
            open_orders: 0.0,
        }),
        Event::Connection {
            feed: Feed::Private,
            state: ConnState::Up,
        },
    ];
    startup.extend((0..n).map(|i| book(i, 5000)));
    core.tick(
        Clock {
            mono: SEC,
            wall_ns: 1_000 * SEC as i64,
        },
        startup,
    );
    core
}

#[test]
#[ignore]
fn bench_shell() {
    let raw = raw_frame();
    println!("frame: {} bytes", raw.len());

    let dir = std::env::temp_dir().join(format!("pmbot-bench-{}", std::process::id()));
    let journal = Journal::start(dir.clone()).unwrap();
    time("feed/journal_raw_frame (escape + enqueue)", 20_000, || {
        journal.raw("bench-", 1, black_box(&raw), 0, 0)
    });
    time("feed/decode_book_frame", 20_000, || {
        black_box(decode_market_frame(black_box(&raw)).unwrap());
    });

    let inbox = Inbox::new();
    let frame = book_frame();
    time("inbox/book_push_and_drain", 50_000, || {
        inbox.book(frame.clone(), Instant::now());
        black_box(inbox.drain_timed());
    });
    time("inbox/frame_clone (part of push)", 50_000, || {
        black_box(frame.clone());
    });

    let metrics = Metrics::default();
    time("metrics/record", 1_000_000, || {
        metrics.record(Stage::Tick, black_box(123_456))
    });
    time("metrics/table (admin socket)", 2_000, || {
        black_box(metrics.table());
    });

    let event = Event::Book(frame.clone());
    time("core_loop/clone_events_for_journal", 50_000, || {
        black_box(vec![event.clone()]);
    });
    time("journal_thread/serialise_tick_record", 20_000, || {
        black_box(serde_json::json!({"mono": 1, "wall_ns": 2, "events": [&event], "tick_us": 3, "commands": [], "derived": [], "decisions": []}).to_string());
    });

    let core = quoting_core(15);
    let clock = Clock {
        mono: 2 * SEC,
        wall_ns: 1_001 * SEC as i64,
    };
    time(
        "core/snapshot_15_markets (UI, 1/s while open)",
        5_000,
        || {
            black_box(core.snapshot(clock));
        },
    );
    let slug = core.snapshot(clock).markets[0].slug.clone();
    time(
        "core/market_detail (UI, 1/s per open market page)",
        5_000,
        || {
            black_box(core.market_detail(&slug, clock));
        },
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Replays a live core journal through a fresh core, timing each tick with no scheduling in the
/// way: the real order history and market set, run hot. Compares with the `tick_us` live recorded.
///
///   PMBOT_REPLAY=captures/core-2026-10-28.jsonl PMBOT_REPLAY_CONFIG=config/pmbot-two-phase-live.toml \
///   PMBOT_REPLAY_FROM_NS=1793156910000000000 \
///   cargo test --release -p pm-live bench_replay_core_journal -- --ignored --nocapture
#[test]
#[ignore]
fn bench_replay_core_journal() {
    let Ok(path) = std::env::var("PMBOT_REPLAY") else {
        return println!("set PMBOT_REPLAY");
    };
    let cfg_path = std::env::var("PMBOT_REPLAY_CONFIG").ok();
    let from_ns: i64 = std::env::var("PMBOT_REPLAY_FROM_NS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let cfg = crate::load_config(cfg_path.as_deref().map(std::path::Path::new)).unwrap();
    let mut core = Core::new(cfg);
    let text = std::fs::read_to_string(&path).unwrap();
    // (class, replay ns, live µs)
    let mut samples: Vec<(&str, f64, f64)> = Vec::new();
    let mut started = false;
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let (Some(mono), Some(wall_ns), Some(live_us)) = (
            row["mono"].as_u64(),
            row["wall_ns"].as_i64(),
            row["tick_us"].as_f64(),
        ) else {
            continue;
        };
        if wall_ns < from_ns {
            continue;
        }
        let events: Vec<Event> = match &row["events"] {
            serde_json::Value::Null => Vec::new(),
            v => match serde_json::from_value(v.clone()) {
                Ok(e) => e,
                Err(_) => continue,
            },
        };
        // Start at the restart: the first tick that carries the market list.
        if !started {
            if !events.iter().any(|e| matches!(e, Event::Markets(_))) {
                continue;
            }
            started = true;
        }
        let t = Instant::now();
        let out = core.tick(Clock { mono, wall_ns }, events);
        let ns = t.elapsed().as_nanos() as f64;
        let class = match (out.decisions.len(), out.commands.is_empty()) {
            (0, true) => "no decision",
            (_, true) => "decisions, no commands",
            (1, false) => "1 decision, commands",
            (_, false) => "2+ decisions, commands",
        };
        samples.push((class, ns, live_us));
        black_box(out);
    }
    println!("{} ticks replayed", samples.len());
    let pct = |v: &mut Vec<f64>, q: f64| {
        v.sort_by(f64::total_cmp);
        v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
    };
    for class in [
        "no decision",
        "decisions, no commands",
        "1 decision, commands",
        "2+ decisions, commands",
    ] {
        let mut replay: Vec<f64> = samples
            .iter()
            .filter(|s| s.0 == class)
            .map(|s| s.1 / 1e3)
            .collect();
        let mut live: Vec<f64> = samples
            .iter()
            .filter(|s| s.0 == class)
            .map(|s| s.2)
            .collect();
        if replay.is_empty() {
            continue;
        }
        println!("{class:<24} n={:6}  replay p50 {:7.1} p90 {:7.1} p99 {:7.1} max {:8.1} µs | live p50 {:6.0} p90 {:6.0} p99 {:6.0} µs",
            replay.len(), pct(&mut replay, 0.5), pct(&mut replay, 0.9), pct(&mut replay, 0.99), pct(&mut replay, 1.0),
            pct(&mut live, 0.5), pct(&mut live, 0.9), pct(&mut live, 0.99));
    }
}
