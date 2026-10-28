//! CPU cost of one core tick: the in-process part of "book arrives → order command out".
//! `cargo bench -p pm-core`
//!
//! Setup: `MARKETS` seeded markets, both feeds up, every quote placed and acked.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use pm_core::{Clock, Config, Core};
use pm_types::*;
use pm_venue::decode::{decode_market_frame, MarketMsg};

const SEC: u64 = 1_000_000_000;
const MARKETS: usize = 40;
/// The slug of the captured frame in `tests/fixtures/market_frame.jsonl`.
const FIXTURE_SLUG: &str = "aec-modus-graush-studut-2026-10-21";

fn slug(i: usize) -> Slug {
    if i == 0 {
        Arc::from(FIXTURE_SLUG)
    } else {
        Arc::from(format!("aec-pdcdarts-m{i}-2026-10-23"))
    }
}

fn params(i: usize) -> MarketParams {
    MarketParams {
        slug: slug(i),
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

/// Live darts books are 20–45 levels deep per side.
const LEVELS: i64 = 40;

/// A `LEVELS`-deep book around `mid` (in 1e-4 units).
fn book(i: usize, ns: i64, mid: i64) -> Event {
    let bids = (0..LEVELS)
        .map(|k| (Px(mid - 500 - 100 * k), Qty(50_000 + 10_000 * k)))
        .collect();
    let offers = (0..LEVELS)
        .map(|k| (Px(mid + 500 + 100 * k), Qty(50_000 + 10_000 * k)))
        .collect();
    Event::Book(BookFrame {
        slug: slug(i),
        bids,
        offers,
        state: MarketState::Open,
        transact_ns: Some(ns),
        shares_traded: Some(Qty::ZERO),
        settlement: None,
    })
}

fn clock(mono: u64) -> Clock {
    Clock {
        mono,
        wall_ns: 1_000 * SEC as i64 + mono as i64,
    }
}

/// Returns the core and the mono time it's at.
fn ready_core() -> (Core, u64) {
    ready_core_n(MARKETS)
}

/// `ready_core` with `n` markets.
fn ready_core_n(n: usize) -> (Core, u64) {
    let mut cfg = Config::default();
    cfg.risk.max_orders_total = 4 * n;
    let mut core = Core::new(cfg);
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
    startup.extend((0..n).map(|i| book(i, 1, 5000)));
    let out = core.tick(clock(SEC), startup);
    // Ack every placed quote: REST accept, then the NEW execution.
    let mut acks = Vec::new();
    let mut execs = Vec::new();
    for (n, cmd) in out.commands.iter().enumerate() {
        if let Command::Place { token, order, .. } = cmd {
            let id: OrderId = Arc::from(format!("O{n}"));
            acks.push(Event::RestResult(RestResult {
                kind: RestKind::Create,
                tokens: vec![*token],
                outcome: RestOutcome::Accepted {
                    ids: vec![Some(id.clone())],
                },
            }));
            execs.push(Event::Execution(ExecutionFrame {
                exec_id: format!("new-{n}"),
                order: VenueOrder {
                    id,
                    slug: order.slug.clone(),
                    side: order.intent.side(),
                    intent: order.intent,
                    px: order.px,
                    qty: order.qty,
                    cum_qty: Qty::ZERO,
                    leaves_qty: order.qty,
                    state: OrderState::New,
                },
                kind: ExecKind::New,
                fill: None,
                reject: None,
                unsolicited: None,
                transact_ns: None,
            }));
        }
    }
    assert_eq!(acks.len(), 2 * n, "every market should quote both sides");
    core.tick(clock(SEC + 1_000_000), acks);
    core.tick(clock(SEC + 2_000_000), execs);
    (core, SEC + 2_000_000)
}

fn command_count(core_and_now: (Core, u64), events: Vec<Event>, dt: u64) -> usize {
    let (mut core, now) = core_and_now;
    core.tick(clock(now + dt), events).commands.len()
}

fn bench(c: &mut Criterion) {
    let fixture: serde_json::Value = serde_json::from_str(
        include_str!("../../../tests/fixtures/market_frame.jsonl")
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    let raw = fixture["raw"].as_str().unwrap().to_string();

    // Sanity: say what each scenario does, so the numbers can be read correctly.
    eprintln!(
        "commands per tick: book_unchanged={} book_requote={} burst={} housekeeping={}",
        command_count(ready_core(), vec![book(1, 2, 5000)], 1_000),
        command_count(ready_core(), vec![book(1, 2, 5400)], 5 * SEC),
        command_count(
            ready_core(),
            (0..MARKETS).map(|i| book(i, 2, 5400)).collect(),
            5 * SEC
        ),
        command_count(ready_core(), vec![], SEC)
    );

    let mut g = c.benchmark_group("tick");
    g.bench_function("book_unchanged", |b| {
        b.iter_batched_ref(
            ready_core,
            |(core, now)| core.tick(clock(*now + 1_000), black_box(vec![book(1, 2, 5000)])),
            BatchSize::SmallInput,
        )
    });
    g.bench_function("book_requote", |b| {
        b.iter_batched_ref(
            ready_core,
            |(core, now)| core.tick(clock(*now + 5 * SEC), black_box(vec![book(1, 2, 5400)])),
            BatchSize::SmallInput,
        )
    });
    g.bench_function("burst_40_books_requote", |b| {
        b.iter_batched_ref(
            ready_core,
            |(core, now)| {
                core.tick(
                    clock(*now + 5 * SEC),
                    black_box((0..MARKETS).map(|i| book(i, 2, 5400)).collect()),
                )
            },
            BatchSize::SmallInput,
        )
    });
    g.bench_function("housekeeping_idle", |b| {
        b.iter_batched_ref(
            ready_core,
            |(core, now)| core.tick(clock(*now + SEC), black_box(vec![])),
            BatchSize::SmallInput,
        )
    });
    // Housekeeping runs every 250 ms; these split a requote from it, and use the live market count.
    g.bench_function("book_requote_between_housekeeping", |b| {
        b.iter_batched_ref(
            ready_core,
            |(core, now)| core.tick(clock(*now + 1_000), black_box(vec![book(1, 2, 5400)])),
            BatchSize::SmallInput,
        )
    });
    g.bench_function("15_markets/book_unchanged", |b| {
        b.iter_batched_ref(
            || ready_core_n(15),
            |(core, now)| core.tick(clock(*now + 1_000), black_box(vec![book(1, 2, 5000)])),
            BatchSize::SmallInput,
        )
    });
    g.bench_function("15_markets/book_requote", |b| {
        b.iter_batched_ref(
            || ready_core_n(15),
            |(core, now)| core.tick(clock(*now + 1_000), black_box(vec![book(1, 2, 5400)])),
            BatchSize::SmallInput,
        )
    });
    g.bench_function("15_markets/housekeeping_idle", |b| {
        b.iter_batched_ref(
            || ready_core_n(15),
            |(core, now)| core.tick(clock(*now + SEC), black_box(vec![])),
            BatchSize::SmallInput,
        )
    });
    g.finish();

    // The full in-process path for one real 40-level frame: JSON text → decoded book → tick → commands.
    c.bench_function("e2e/decode_and_tick_real_frame", |b| {
        b.iter_batched_ref(
            ready_core,
            |(core, now)| {
                let MarketMsg::Book(frame) = decode_market_frame(black_box(&raw)).unwrap() else {
                    unreachable!()
                };
                core.tick(clock(*now + 5 * SEC), vec![Event::Book(frame)])
            },
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
