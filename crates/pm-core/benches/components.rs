//! CPU cost of the pieces a core tick is made of, one market at a time.
//! `cargo bench -p pm-core --bench components`

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use pm_core::config::{LeagueConfig, RiskConfig};
use pm_core::market_data::MarketData;
use pm_core::portfolio::{Position, Resting};
use pm_core::risk::{GateCtx, Risk};
use pm_core::strategy::{LeagueCheck, MarketView, PreMatch, PreMatchConfig, Strategy};
use pm_types::*;

const SEC: i64 = 1_000_000_000;
/// Live darts books are 20–45 levels deep per side.
const LEVELS: i64 = 40;

fn slug() -> Slug {
    Arc::from("aec-pdcdarts-a-b-2026-10-23")
}

fn params() -> MarketParams {
    MarketParams {
        slug: slug(),
        title: "A vs B".into(),
        event_id: "ev".into(),
        event_slug: "pdcdarts-a-b".into(),
        league: "pdcdarts".into(),
        sports_market_type: "darts_match_winner".into(),
        tick: Px(100),
        min_qty: Qty(100),
        fee_coefficient: 0.0695,
        game_start_ns: Some(1_000_000 * SEC),
        yes_name: None,
        no_name: None,
        yes_ordering: Some(TeamOrdering::Home),
        is_futures: false,
    }
}

fn frame(ns: i64, mid: i64) -> BookFrame {
    let bids = (0..LEVELS)
        .map(|k| (Px(mid - 500 - 100 * k), Qty(50_000 + 10_000 * k)))
        .collect();
    let offers = (0..LEVELS)
        .map(|k| (Px(mid + 500 + 100 * k), Qty(50_000 + 10_000 * k)))
        .collect();
    BookFrame {
        slug: slug(),
        bids,
        offers,
        state: MarketState::Open,
        transact_ns: Some(ns),
        shares_traded: Some(Qty::ZERO),
        settlement: None,
    }
}

fn seeded() -> MarketData {
    let mut md = MarketData::default();
    md.on_book(frame(1, 5000), Px(100), Qty(100));
    md
}

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("component");

    g.bench_function("market_data/on_book_40_levels", |b| {
        b.iter_batched_ref(
            seeded,
            |md| md.on_book(black_box(frame(2, 5100)), Px(100), Qty(100)),
            BatchSize::SmallInput,
        )
    });
    let md = seeded();
    g.bench_function("market_data/provisional_view", |b| {
        b.iter(|| black_box(&md).provisional_view())
    });
    let view = md.provisional_view();
    g.bench_function("book_view/remove_our_two_quotes", |b| {
        b.iter_batched_ref(
            || view.clone(),
            |v| {
                v.remove(Side::Buy, Px(4400), Qty(100));
                v.remove(Side::Sell, Px(5600), Qty(100));
            },
            BatchSize::SmallInput,
        )
    });
    g.bench_function("book_view/vwap_3_levels", |b| {
        b.iter(|| black_box(&view).vwap(3))
    });

    let (p, league) = (params(), LeagueConfig::default());
    let mv = MarketView {
        params: &p,
        league: &league,
        league_check: LeagueCheck::Unverified,
        others: &view,
        last_trade: None,
        game: None,
        position: Qty::ZERO,
        working_bid: None,
        working_ask: None,
        quotable: true,
        input_updated: true,
        filled: false,
        slow_exchange: false,
        now: 1,
        wall_ns: 1_000 * SEC,
    };
    let mut strategy = PreMatch::new(PreMatchConfig::default());
    g.bench_function("strategy/pre_match_decide", |b| {
        b.iter(|| strategy.decide(black_box(&mv)))
    });

    let risk = Risk::new(RiskConfig::default());
    let (s, pos) = (slug(), Position::default());
    let ctx = GateCtx {
        slug: &s,
        tick: Px(100),
        min_qty: Qty(100),
        fair: 0.5,
        best_bid: Some(Px(4500)),
        best_ask: Some(Px(5500)),
        position: &pos,
        other_resting: Resting::default(),
        account_other: 0.0,
        committed_other: 0.0,
        buying_power: Some(1_000.0),
        orders_elsewhere: 0,
        has_working: (true, true),
    };
    g.bench_function("risk/gate_both_quotes", |b| {
        b.iter(|| {
            risk.gate(
                black_box(&ctx),
                Some((Px(3500), Qty(10_000))),
                Some((Px(6500), Qty(10_000))),
            )
        })
    });
    g.bench_function("config/league_lookup_clone", |b| {
        let cfg = pm_core::Config::default();
        b.iter(|| cfg.league(black_box("pdcdarts")))
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
