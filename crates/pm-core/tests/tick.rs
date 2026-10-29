//! End-to-end tick tests: events in, commands out.

use std::sync::Arc;

use pm_core::{Clock, Config, Core};
use pm_types::*;

const SEC: u64 = 1_000_000_000;
const SLUG: &str = "aec-pdcdarts-a-b";

fn params() -> MarketParams {
    MarketParams {
        slug: Arc::from(SLUG),
        title: "A vs B".into(),
        event_id: "ev1".into(),
        event_slug: "pdcdarts-a-b".into(),
        league: "pdcdarts".into(),
        sports_market_type: "darts_match_winner".into(),
        tick: Px(100),
        min_qty: Qty(100),
        fee_coefficient: 0.0695,
        game_start_ns: Some(1_000_000 * SEC as i64),
        yes_name: Some("A".into()),
        no_name: Some("B".into()),
        yes_ordering: Some(TeamOrdering::Home),
        is_futures: false,
    }
}

fn book(ns: i64, bid: i64, ask: i64) -> Event {
    Event::Book(BookFrame {
        slug: Arc::from(SLUG),
        bids: vec![(Px(bid), Qty(100_000))],
        offers: vec![(Px(ask), Qty(100_000))],
        state: MarketState::Open,
        transact_ns: Some(ns),
        shares_traded: Some(Qty::ZERO),
        settlement: None,
    })
}

fn game(score: Option<(u16, u16)>, live: bool) -> Event {
    Event::Game(GameFrame {
        event_id: "ev1".into(),
        event_slug: "pdcdarts-a-b".into(),
        period: if live { "LIVE".into() } else { String::new() },
        live,
        ended: false,
        score,
        updated_ns: None,
    })
}

fn clock(mono: u64) -> Clock {
    Clock {
        mono,
        wall_ns: 1_000 * SEC as i64 + mono as i64,
    }
}

fn places(cmds: &[Command]) -> Vec<(Token, NewOrder)> {
    cmds.iter()
        .filter_map(|c| match c {
            Command::Place { token, order, .. } => Some((*token, order.clone())),
            _ => None,
        })
        .collect()
}

fn cancels(cmds: &[Command]) -> usize {
    cmds.iter()
        .filter(|c| {
            matches!(
                c,
                Command::Cancel { .. } | Command::Withdraw { .. } | Command::CancelAll { .. }
            )
        })
        .count()
}

/// A core with one market, both feeds up and a seeded book; returns the two placed quote tokens.
fn ready_core() -> (Core, Vec<(Token, NewOrder)>) {
    let mut core = Core::new(Config::default());
    let startup = vec![
        Event::Markets(vec![params()]),
        Event::Connection {
            feed: Feed::Markets,
            state: ConnState::Up,
        },
        Event::OrderSnapshot(vec![]),
        Event::Positions(vec![]),
        Event::Balance(BalanceFrame {
            current_balance: 100.0,
            buying_power: 100.0,
            open_orders: 0.0,
        }),
        Event::Connection {
            feed: Feed::Private,
            state: ConnState::Up,
        },
        book(1, 4500, 5500),
    ];
    let out = core.tick(clock(SEC), startup);
    let placed = places(&out.commands);
    (core, placed)
}

fn ack(core: &mut Core, now: u64, token: Token, id: &str, order: &NewOrder) {
    core.tick(
        clock(now),
        vec![Event::RestResult(RestResult {
            kind: RestKind::Create,
            tokens: vec![token],
            outcome: RestOutcome::Accepted {
                ids: vec![Some(Arc::from(id))],
            },
        })],
    );
    core.tick(
        clock(now + 1),
        vec![Event::Execution(ExecutionFrame {
            exec_id: format!("new-{id}"),
            order: VenueOrder {
                id: Arc::from(id),
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
        })],
    );
}

#[test]
fn no_orders_until_private_stream_ready() {
    let mut core = Core::new(Config::default());
    let out = core.tick(
        clock(SEC),
        vec![
            Event::Markets(vec![params()]),
            Event::Connection {
                feed: Feed::Markets,
                state: ConnState::Up,
            },
            book(1, 4500, 5500),
        ],
    );
    assert!(places(&out.commands).is_empty());
    assert!(out.derived.iter().any(|d| d.kind == "not_quoting"));
}

#[test]
fn quotes_both_sides_once_ready() {
    let (_, placed) = ready_core();
    assert_eq!(placed.len(), 2, "{placed:?}");
    let bid = placed
        .iter()
        .find(|(_, o)| o.intent == Intent::BuyLong)
        .unwrap();
    let ask = placed
        .iter()
        .find(|(_, o)| o.intent == Intent::BuyShort)
        .unwrap();
    assert!(bid.1.px < ask.1.px);
    assert!(bid.1.post_only && matches!(bid.1.tif, Tif::Gtd(_)));
}

#[test]
fn probe_default_uses_discovery_with_exposure_caps_short_expiry_and_no_session_timeout() {
    // The run config may carry experiments (e.g. `core_spin_us`), so check both files
    // for the probe's safety properties rather than requiring them to be identical.
    for text in [
        include_str!("../../../config/latency-probe.example.toml"),
        include_str!("../../../config/latency-probe.toml"),
    ] {
        check_probe_config(toml::from_str(text).unwrap());
    }
}

fn check_probe_config(cfg: Config) {
    cfg.validate().unwrap();
    assert!(cfg.routes.is_empty());
    assert_eq!(cfg.risk.max_market_loss, 2.0);
    assert_eq!(cfg.risk.max_account_loss, 20.0);
    assert_eq!(cfg.risk.max_orders_total, 40);
    assert!(
        !cfg.execution.rate_limit_enabled,
        "probe config selects the unthrottled comparison"
    );
    let mut another = params();
    another.slug = "another-discovered-market".into();
    for market in [params(), another] {
        assert_eq!(
            cfg.strategy_for(&market).unwrap().config.kind(),
            "latency_probe"
        );
    }
    let (mut core, placed) = startup_with(cfg);
    assert!(
        placed.is_empty(),
        "probe waits for known pre-match game state"
    );
    let now = 2 * SEC;
    let out = core.tick(clock(now), vec![game(None, false)]);
    let placed = places(&out.commands);
    assert_eq!(placed.len(), 2, "{:?}", out.derived);
    for (i, (token, order)) in placed.iter().enumerate() {
        assert_eq!(order.qty, params().min_qty);
        assert!(order.post_only);
        assert!(matches!(order.px, Px(100) | Px(9900)));
        assert_eq!(order.tif, Tif::Gtd(clock(now).wall_ns + 15 * SEC as i64));
        ack(
            &mut core,
            now + 10 + i as u64 * 10,
            *token,
            &format!("probe-{i}"),
            order,
        );
    }
    let out = core.tick(clock(now + 600 * SEC), vec![game(None, false)]);
    assert_eq!(
        out.commands
            .iter()
            .filter(|c| matches!(c, Command::Modify { .. }))
            .count(),
        2,
        "probe still reprices after ten minutes: {:?}",
        out.commands
    );
    assert_eq!(cancels(&out.commands), 0);
}

#[test]
fn probe_quotes_observed_wide_books_and_only_reprices_on_received_input() {
    #[derive(serde::Deserialize)]
    struct Fixture {
        wall_ns: i64,
        events: Vec<Event>,
    }
    // Public market/game inputs captured from the session that placed no orders.
    let fixture: Fixture = serde_json::from_str(include_str!(
        "../../../tests/fixtures/probe-wide-books.json"
    ))
    .unwrap();
    let cfg: Config = toml::from_str(include_str!("../../../config/latency-probe.toml")).unwrap();
    let mut core = Core::new(cfg);
    let games: Vec<Event> = fixture
        .events
        .iter()
        .filter(|e| matches!(e, Event::Game(_)))
        .cloned()
        .collect();
    let mut inputs = fixture.events;
    inputs.extend([
        Event::Connection {
            feed: Feed::Markets,
            state: ConnState::Up,
        },
        Event::Connection {
            feed: Feed::Private,
            state: ConnState::Up,
        },
        Event::OrderSnapshot(vec![]),
        Event::Positions(vec![]),
        Event::Balance(BalanceFrame {
            current_balance: 100.0,
            buying_power: 100.0,
            open_orders: 0.0,
        }),
    ]);
    let clock_at = |s: u64| Clock {
        mono: s * SEC,
        wall_ns: fixture.wall_ns + s as i64 * SEC as i64,
    };
    let out = core.tick(clock_at(1), inputs);
    let placed = places(&out.commands);
    let quoted: std::collections::BTreeSet<_> =
        placed.iter().map(|(_, o)| o.slug.clone()).collect();
    assert_eq!(quoted.len(), 15, "{:?}", out.derived);
    assert_eq!(placed.len(), 30);
    for (i, (token, order)) in placed.iter().enumerate() {
        assert_eq!(order.qty, Qty(100));
        assert!(order.post_only && matches!(order.px, Px(100) | Px(9900)));
        let id: OrderId = format!("probe-{i}").into();
        let out = core.tick(
            clock_at(2),
            vec![
                Event::RestResult(RestResult {
                    kind: RestKind::Create,
                    tokens: vec![*token],
                    outcome: RestOutcome::Accepted {
                        ids: vec![Some(id.clone())],
                    },
                }),
                Event::Execution(ExecutionFrame {
                    exec_id: format!("ack-{i}"),
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
                }),
            ],
        );
        assert_eq!(out.execution_acks, vec![(*token, RestKind::Create)]);
        assert!(
            out.commands.is_empty(),
            "feedback alone must not create probe traffic"
        );
    }
    let timer = core.tick(clock_at(12), vec![]);
    // Reads (the positions re-read after the private stream came up) are fine; orders are not.
    assert!(
        timer
            .commands
            .iter()
            .all(|c| matches!(c, Command::QueryPositions | Command::QueryBalance)),
        "timer reprices would have no received-input origin: {:?}",
        timer.commands
    );
    // These are identical game frames from a later successful real poll.
    let received = core.tick(clock_at(12), games);
    assert!(
        received
            .commands
            .iter()
            .any(|c| matches!(c, Command::Modify { .. })),
        "{:?}",
        received.derived
    );
    assert!(received
        .commands
        .iter()
        .all(|c| matches!(c, Command::Modify { .. })));
}

#[test]
fn quiet_book_sends_nothing_after_ack() {
    let (mut core, placed) = ready_core();
    for (i, (token, order)) in placed.iter().enumerate() {
        ack(
            &mut core,
            2 * SEC + i as u64 * 10,
            *token,
            &format!("O{i}"),
            order,
        );
    }
    // A deeper-book-only change: same touch, nothing to do.
    let out = core.tick(clock(3 * SEC), vec![book(2, 4500, 5500)]);
    assert!(out.commands.is_empty(), "{:?}", out.commands);
}

#[test]
fn feed_down_and_book_in_same_tick_never_quotes() {
    let (mut core, placed) = ready_core();
    for (i, (token, order)) in placed.iter().enumerate() {
        ack(
            &mut core,
            2 * SEC + i as u64 * 10,
            *token,
            &format!("O{i}"),
            order,
        );
    }
    let out = core.tick(
        clock(3 * SEC),
        vec![
            book(5, 4600, 5400),
            Event::Connection {
                feed: Feed::Markets,
                state: ConnState::Down,
            },
        ],
    );
    assert!(places(&out.commands).is_empty());
    assert_eq!(cancels(&out.commands), 2);
}

#[test]
fn pre_match_pulls_quotes_when_the_game_goes_live() {
    let (mut core, placed) = ready_core();
    for (i, (token, order)) in placed.iter().enumerate() {
        ack(
            &mut core,
            2 * SEC + i as u64 * 10,
            *token,
            &format!("O{i}"),
            order,
        );
    }
    // Live before the scheduled start: the game feed wins.
    let out = core.tick(
        clock(3 * SEC),
        vec![game(Some((0, 0)), true), Event::GamePolled],
    );
    assert_eq!(cancels(&out.commands), 2, "{:?}", out.commands);
    assert!(places(&out.commands).is_empty());
}

#[test]
fn quotes_come_down_at_the_scheduled_start_without_new_input() {
    let mut cfg = Config::default();
    cfg.execution.redecide_all_s = 0.0; // only the strategy's own wake-up can pull the quotes
    let mut core = Core::new(cfg);
    // Starts at wall 1_600 s, the same UTC day as clock(): no day roll to re-decide everything.
    let mut market = params();
    market.game_start_ns = Some(1_600 * SEC as i64);
    let out = core.tick(
        clock(SEC),
        vec![
            Event::Markets(vec![market]),
            Event::Connection {
                feed: Feed::Markets,
                state: ConnState::Up,
            },
            Event::OrderSnapshot(vec![]),
            Event::Positions(vec![]),
            Event::Balance(BalanceFrame {
                current_balance: 100.0,
                buying_power: 100.0,
                open_orders: 0.0,
            }),
            Event::Connection {
                feed: Feed::Private,
                state: ConnState::Up,
            },
            book(1, 4500, 5500),
        ],
    );
    let placed = places(&out.commands);
    assert_eq!(placed.len(), 2);
    for (i, (token, order)) in placed.iter().enumerate() {
        ack(
            &mut core,
            2 * SEC + i as u64 * 10,
            *token,
            &format!("O{i}"),
            order,
        );
    }
    // A housekeeping tick with nothing due re-decides nothing.
    let quiet = core.tick(clock(2 * SEC + 300_000_000), vec![]);
    assert!(quiet.decisions.is_empty(), "{:?}", quiet.decisions);
    let before = core.tick(clock(599 * SEC), vec![]);
    assert_eq!(cancels(&before.commands), 0);
    let out = core.tick(clock(600 * SEC + 1), vec![]);
    assert_eq!(cancels(&out.commands), 2, "{:?}", out.commands);
}

#[test]
fn fill_updates_position_and_skews() {
    let (mut core, placed) = ready_core();
    let (token, order) = placed
        .iter()
        .find(|(_, o)| o.intent == Intent::BuyLong)
        .unwrap()
        .clone();
    ack(&mut core, 2 * SEC, token, "B1", &order);
    let out = core.tick(
        clock(3 * SEC),
        vec![Event::Execution(ExecutionFrame {
            exec_id: "fill-1".into(),
            order: VenueOrder {
                id: Arc::from("B1"),
                slug: Arc::from(SLUG),
                side: Side::Buy,
                intent: Intent::BuyLong,
                px: order.px,
                qty: order.qty,
                cum_qty: order.qty,
                leaves_qty: Qty::ZERO,
                state: OrderState::Filled,
            },
            kind: ExecKind::Fill,
            fill: Some(FillInfo {
                last_shares: order.qty,
                last_px: order.px,
                trade_id: "T1".into(),
                aggressor: false,
                commission: Some(0.0),
            }),
            reject: None,
            unsolicited: None,
            transact_ns: None,
        })],
    );
    assert_eq!(core.portfolio.position(SLUG).qty, order.qty);
    assert!(out.derived.iter().any(|d| d.kind == "fill"));
    // The bid slot is free and gets a new (wider, post-fill) quote.
    let new_bid = places(&out.commands)
        .into_iter()
        .find(|(_, o)| o.intent == Intent::BuyLong)
        .unwrap();
    assert!(new_bid.1.px <= order.px);
}

#[test]
fn private_down_cancels_everything() {
    let (mut core, placed) = ready_core();
    for (i, (token, order)) in placed.iter().enumerate() {
        ack(
            &mut core,
            2 * SEC + i as u64 * 10,
            *token,
            &format!("O{i}"),
            order,
        );
    }
    let out = core.tick(
        clock(3 * SEC),
        vec![Event::Connection {
            feed: Feed::Private,
            state: ConnState::Down,
        }],
    );
    assert!(out
        .commands
        .iter()
        .any(|c| matches!(c, Command::CancelAll { .. })));
    assert!(places(&out.commands).is_empty());
}

#[test]
fn ticks_are_deterministic() {
    let run = || {
        let (mut core, placed) = ready_core();
        let mut all = format!("{placed:?}");
        for (i, (token, order)) in placed.iter().enumerate() {
            ack(
                &mut core,
                2 * SEC + i as u64 * 10,
                *token,
                &format!("O{i}"),
                order,
            );
        }
        for i in 0..20u64 {
            let out = core.tick(
                clock(3 * SEC + i * SEC / 2),
                vec![book(10 + i as i64, 4500 + (i as i64 % 3) * 100, 5500)],
            );
            all.push_str(&format!("{:?}", out.commands));
        }
        all
    };
    assert_eq!(run(), run());
}

fn startup_with(cfg: Config) -> (Core, Vec<(Token, NewOrder)>) {
    let mut core = Core::new(cfg);
    let out = core.tick(
        clock(SEC),
        vec![
            Event::Markets(vec![params()]),
            Event::Connection {
                feed: Feed::Markets,
                state: ConnState::Up,
            },
            Event::OrderSnapshot(vec![]),
            Event::Positions(vec![]),
            Event::Balance(BalanceFrame {
                current_balance: 100.0,
                buying_power: 100.0,
                open_orders: 0.0,
            }),
            Event::Connection {
                feed: Feed::Private,
                state: ConnState::Up,
            },
            book(1, 4500, 5500),
        ],
    );
    let placed = places(&out.commands);
    (core, placed)
}

fn routed(route_toml: &str) -> Config {
    let cfg: Config = toml::from_str(&format!(
        "[strategies.off]\nkind = \"off\"\n[[routes]]\n{route_toml}"
    ))
    .unwrap();
    cfg.validate().unwrap();
    cfg
}

#[test]
fn route_to_off_profile_never_quotes() {
    let (core, placed) = startup_with(routed(&format!("slug = \"{SLUG}\"\nstrategy = \"off\"\n")));
    assert!(placed.is_empty());
    assert!(
        core.status().contains("off (route 0) [off]: 1"),
        "{}",
        core.status()
    );
}

#[test]
fn route_overrides_apply_to_matching_ticker_only() {
    let (_, placed) = startup_with(routed(
        "slug = \"aec-pdcdarts-*\"\nstrategy = \"default\"\nset = { size = 3.0 }\n",
    ));
    assert_eq!(placed.len(), 2);
    assert!(
        placed.iter().all(|(_, o)| o.qty == Qty::from_f64(3.0)),
        "{placed:?}"
    );

    let (_, placed) = startup_with(routed(
        "slug = \"aec-modus-*\"\nstrategy = \"default\"\nset = { size = 3.0 }\n",
    ));
    assert!(
        placed.iter().all(|(_, o)| o.qty == Qty::from_f64(1.0)),
        "{placed:?}"
    );
}

#[test]
fn snapshot_reports_pnl_and_exposure() {
    let (mut core, placed) = ready_core();
    let snap = core.snapshot(clock(SEC + 1));
    assert_eq!(snap.markets.len(), 1);
    let row = &snap.markets[0];
    assert_eq!(row.mid, Some(0.5));
    assert_eq!(row.our_bids.len() + row.our_asks.len(), placed.len());
    assert_eq!((row.qty, row.pnl_if_yes, row.pnl_if_no), (0.0, 0.0, 0.0));

    let (token, order) = placed
        .iter()
        .find(|(_, o)| o.intent == Intent::BuyLong)
        .unwrap()
        .clone();
    ack(&mut core, 2 * SEC, token, "B1", &order);
    core.tick(
        clock(3 * SEC),
        vec![Event::Execution(ExecutionFrame {
            exec_id: "fill-1".into(),
            order: VenueOrder {
                id: Arc::from("B1"),
                slug: Arc::from(SLUG),
                side: Side::Buy,
                intent: Intent::BuyLong,
                px: order.px,
                qty: order.qty,
                cum_qty: order.qty,
                leaves_qty: Qty::ZERO,
                state: OrderState::Filled,
            },
            kind: ExecKind::Fill,
            fill: Some(FillInfo {
                last_shares: order.qty,
                last_px: order.px,
                trade_id: "T1".into(),
                aggressor: false,
                commission: Some(0.0),
            }),
            reject: None,
            unsolicited: None,
            transact_ns: None,
        })],
    );
    let snap = core.snapshot(clock(3 * SEC + 1));
    let row = &snap.markets[0];
    let (q, px) = (order.qty.as_f64(), order.px.as_f64());
    assert_eq!(row.qty, q);
    assert!((row.pnl_if_yes - q * (1.0 - px)).abs() < 1e-9);
    assert!((row.pnl_if_no + q * px).abs() < 1e-9);
    // Mid is 0.50 with a 10 ¢ spread, so current PnL marks there.
    assert!(
        (snap.current_pnl - q * (0.5 - px)).abs() < 1e-9,
        "{}",
        snap.current_pnl
    );
    assert_eq!(row.current_pnl, Some(snap.current_pnl));
    assert!(row.worst_case >= q * px - 1e-9);
    assert_eq!(snap.unmarked_current, 0);
}

#[test]
fn venue_position_seed_preserves_outcome_pnl_and_risk() {
    // Venue costs are purchases of the held outcome, while the core accounts in signed YES cash.
    // The short examples are from the 2026-10-29 account and execution captures.
    for (net, cost, realized, bid, ask, yes, no, marked, worst) in [
        (
            "2.0000", "0.8000", "0.0000", 4500, 5500, 1.20, -0.80, 0.20, 0.80,
        ),
        (
            "-1.0000", "0.6400", "0.0000", 2900, 3500, -0.64, 0.36, 0.04, 0.64,
        ),
        (
            "-4.0000", "1.3500", "-0.0500", 6300, 6800, -1.40, 2.60, -0.02, 1.40,
        ),
        (
            "-1.0000", "0.6200", "0.0000", 2800, 3100, -0.62, 0.38, 0.085, 0.62,
        ),
        (
            "-2.5000", "1.2000", "0.0000", 4700, 4900, -1.20, 1.30, 0.10, 1.20,
        ),
        (
            "1.0000", "0.4000", "0.1000", 4500, 5500, 0.70, -0.30, 0.20, 0.30,
        ),
    ] {
        let body = serde_json::json!({
            "positions": {
                SLUG: {
                    "netPositionDecimal": net,
                    "cost": { "value": cost, "currency": "USD" },
                    "realized": { "value": realized, "currency": "USD" }
                }
            },
            "eof": true
        });
        let (positions, _, _) = pm_venue::decode::decode_positions_page(&body).unwrap();
        let mut core = Core::new(Config::default());
        core.tick(
            clock(SEC),
            vec![
                Event::Markets(vec![params()]),
                Event::Positions(positions.clone()),
                book(1, bid, ask),
            ],
        );
        // A repeated REST read must not apply the basis or realised PnL twice.
        core.tick(clock(SEC + 1), vec![Event::Positions(positions)]);
        let snapshot = core.snapshot(clock(SEC + 2));
        let row = &snapshot.markets[0];
        for (actual, expected) in [
            (row.pnl_if_yes, yes),
            (row.pnl_if_no, no),
            (snapshot.current_pnl, marked),
            (snapshot.account_worst, worst),
        ] {
            assert!(
                (actual - expected).abs() < 1e-9,
                "net {net}, cost {cost}, realised {realized}: {actual} != {expected}"
            );
        }
        // Closing at the same mark preserves the PnL that was shown before the close.
        let pos = core.portfolio.position(SLUG);
        let side = if pos.qty > Qty::ZERO {
            Side::Sell
        } else {
            Side::Buy
        };
        core.portfolio.on_fill(
            &Arc::from(SLUG),
            side,
            pos.qty.abs(),
            Px((bid + ask) / 2),
            0.0,
        );
        let closed = core.portfolio.position(SLUG);
        assert_eq!(closed.qty, Qty::ZERO);
        assert!((closed.pnl_at(0.0) - marked).abs() < 1e-9);
        assert!((closed.pnl_at(1.0) - marked).abs() < 1e-9);
    }
}

#[test]
fn market_detail_has_book_orders_and_fills() {
    let (mut core, placed) = ready_core();
    assert!(core
        .market_detail("no-such-market", clock(SEC + 1))
        .is_none());
    let d = core.market_detail(SLUG, clock(SEC + 1)).unwrap();
    assert!(!d.bids.is_empty() && !d.offers.is_empty());
    assert_eq!(d.orders.len(), placed.len());
    assert!(d.fills.is_empty());

    let (token, order) = placed
        .iter()
        .find(|(_, o)| o.intent == Intent::BuyLong)
        .unwrap()
        .clone();
    ack(&mut core, 2 * SEC, token, "B1", &order);
    // Our acked bid shows on the ladder only if it sits at a venue level; it is always in the orders.
    assert!(core
        .market_detail(SLUG, clock(2 * SEC + 1))
        .unwrap()
        .orders
        .iter()
        .any(|o| o.side == Side::Buy && o.px == order.px.as_f64()));
    core.tick(
        clock(3 * SEC),
        vec![Event::Execution(ExecutionFrame {
            exec_id: "fill-1".into(),
            order: VenueOrder {
                id: Arc::from("B1"),
                slug: Arc::from(SLUG),
                side: Side::Buy,
                intent: Intent::BuyLong,
                px: order.px,
                qty: order.qty,
                cum_qty: order.qty,
                leaves_qty: Qty::ZERO,
                state: OrderState::Filled,
            },
            kind: ExecKind::Fill,
            fill: Some(FillInfo {
                last_shares: order.qty,
                last_px: order.px,
                trade_id: "T1".into(),
                aggressor: false,
                commission: Some(0.0),
            }),
            reject: None,
            unsolicited: None,
            transact_ns: None,
        })],
    );
    let c = clock(3 * SEC + 1);
    let d = core.market_detail(SLUG, c).unwrap();
    assert_eq!(d.fills.len(), 1);
    assert_eq!(
        (d.fills[0].side, d.fills[0].qty, d.fills[0].position_after),
        (Side::Buy, order.qty.as_f64(), order.qty.as_f64())
    );
    assert_eq!(d.fills[0].wall_ns, c.wall_ns - 1);
    let snap = core.snapshot(c);
    assert_eq!(snap.recent_fills.len(), 1);
    assert_eq!(snap.markets[0].last_fill_ns, Some(d.fills[0].wall_ns));
}

#[test]
fn book_depth_drops_levels_beyond_the_configured_depth() {
    let deep = Event::Book(BookFrame {
        slug: Arc::from(SLUG),
        bids: vec![
            (Px(4500), Qty(100_000)),
            (Px(4000), Qty(100_000)),
            (Px(3000), Qty(100_000)),
        ],
        offers: vec![
            (Px(5500), Qty(100_000)),
            (Px(6000), Qty(100_000)),
            (Px(7000), Qty(100_000)),
        ],
        state: MarketState::Open,
        transact_ns: Some(2),
        shares_traded: Some(Qty::ZERO),
        settlement: None,
    });
    // Include all retained levels in VWAP to measure book retention independently of its display depth.
    let contracts = |mut cfg: Config| {
        cfg.markets.vwap_depth = 0;
        let (mut core, _) = startup_with(cfg);
        core.tick(clock(2 * SEC), vec![deep.clone()]);
        core.snapshot(clock(2 * SEC)).markets[0].vwap.unwrap().1
    };
    assert_eq!(
        contracts(toml::from_str("[markets]\nbook_depth = 1\n").unwrap()),
        20.0
    );
    assert_eq!(
        contracts(toml::from_str("[markets]\nbook_depth = 2\n").unwrap()),
        40.0
    );
    assert_eq!(
        contracts(toml::from_str("[markets]\nbook_depth = 0\n").unwrap()),
        60.0
    );
    assert_eq!(Config::default().markets.book_depth, 20);
}

#[test]
fn snapshot_vwap_depth_limits_price_and_contracts_without_truncating_the_book() {
    let deep = Event::Book(BookFrame {
        slug: Arc::from(SLUG),
        bids: vec![
            (Px(4500), Qty(100_000)),
            (Px(4000), Qty(100_000)),
            (Px(3500), Qty(100_000)),
            (Px(100), Qty(1_000_000)),
        ],
        offers: vec![
            (Px(5500), Qty(100_000)),
            (Px(6000), Qty(100_000)),
            (Px(6500), Qty(100_000)),
            (Px(9900), Qty(100_000)),
        ],
        state: MarketState::Open,
        transact_ns: Some(2),
        shares_traded: Some(Qty::ZERO),
        settlement: None,
    });
    let snapshot = |cfg: Config| {
        let (mut core, _) = startup_with(cfg);
        core.tick(clock(2 * SEC), vec![deep.clone()]);
        let detail = core.market_detail(SLUG, clock(2 * SEC)).unwrap();
        assert_eq!((detail.bids.len(), detail.offers.len()), (4, 4));
        core.snapshot(clock(2 * SEC)).markets.remove(0)
    };
    let row = snapshot(Config::default());
    assert_eq!(row.vwap_depth, 3);
    let (px, qty) = row.vwap.unwrap();
    assert!((px - 0.5).abs() < 1e-9, "{px}");
    assert_eq!(qty, 60.0);
    let row = snapshot(toml::from_str("[markets]\nvwap_depth = 5\n").unwrap());
    assert_eq!(row.vwap_depth, 5);
    let (px, qty) = row.vwap.unwrap();
    assert!((px - 40.9 / 170.0).abs() < 1e-9, "{px}");
    assert_eq!(qty, 170.0);
}

#[test]
fn two_phase_vwap_reacts_to_deeper_changes_independently_of_the_display_depth() {
    use pm_core::strategy::{StrategyConfig, TwoPhaseConfig};
    for use_vwap in [false, true] {
        let mut cfg = Config::default();
        cfg.markets.vwap_depth = 1;
        let profile = TwoPhaseConfig::default();
        cfg.strategies.insert(
            "default".into(),
            if use_vwap {
                StrategyConfig::TwoPhaseVwap(profile)
            } else {
                StrategyConfig::TwoPhase(profile)
            },
        );
        let (mut core, _) = startup_with(cfg);
        assert_eq!(core.snapshot(clock(SEC)).markets[0].fair, Some(0.5));
        // The touch's prices AND sizes are identical to the initial book; only depth changed.
        core.tick(
            clock(2 * SEC),
            vec![Event::Book(BookFrame {
                slug: Arc::from(SLUG),
                bids: vec![
                    (Px(4500), Qty(100_000)),
                    (Px(4000), Qty(1_000_000)),
                    (Px(3500), Qty(100_000)),
                ],
                offers: vec![
                    (Px(5500), Qty(100_000)),
                    (Px(6000), Qty(100_000)),
                    (Px(6500), Qty(100_000)),
                ],
                state: MarketState::Open,
                transact_ns: Some(2),
                shares_traded: Some(Qty::ZERO),
                settlement: None,
            })],
        );
        let row = core.snapshot(clock(2 * SEC)).markets.remove(0);
        let expected = if use_vwap { 0.44 } else { 0.5 };
        assert!((row.fair.unwrap() - expected).abs() < 1e-9, "{row:?}");
        assert_eq!(row.vwap, Some((0.5, 20.0)));
    }
}

#[test]
fn two_phase_vwap_removes_our_size_before_selecting_the_three_levels() {
    let mut cfg = Config::default();
    cfg.strategies.insert(
        "default".into(),
        pm_core::StrategyConfig::TwoPhaseVwap(Default::default()),
    );
    let (mut core, placed) = startup_with(cfg);
    assert_eq!(placed.len(), 2);
    for (i, (token, order)) in placed.iter().enumerate() {
        ack(
            &mut core,
            SEC + 10 * i as u64,
            *token,
            &format!("Q{i}"),
            order,
        );
    }
    // Our 0.40 / 0.60 quotes each occupy the second venue level. After removing them,
    // all three OTHER levels must contribute; their intentionally asymmetric sizes give fair 0.50.
    let out = core.tick(
        clock(2 * SEC),
        vec![Event::Book(BookFrame {
            slug: Arc::from(SLUG),
            bids: vec![
                (Px(4500), Qty(100_000)),
                (Px(4000), Qty(10_000)),
                (Px(3900), Qty(20_000)),
                (Px(3800), Qty(10_000)),
            ],
            offers: vec![
                (Px(5500), Qty(100_000)),
                (Px(6000), Qty(10_000)),
                (Px(6200), Qty(15_000)),
                (Px(6600), Qty(10_000)),
            ],
            state: MarketState::Open,
            transact_ns: Some(2),
            shares_traded: Some(Qty::ZERO),
            settlement: None,
        })],
    );
    assert!((core.snapshot(clock(2 * SEC)).markets[0].fair.unwrap() - 0.5).abs() < 1e-9);
    assert!(
        out.commands.iter().all(|c| !matches!(
            c,
            Command::Place { .. } | Command::Modify { .. } | Command::Cancel { .. }
        )),
        "{:?}",
        out.commands
    );
}

fn exec_at(id: &str, order: &NewOrder, px: Px, kind: ExecKind, transact_ns: i64) -> Event {
    Event::Execution(ExecutionFrame {
        exec_id: format!("{kind:?}-{id}-{transact_ns}"),
        order: VenueOrder {
            id: Arc::from(id),
            slug: order.slug.clone(),
            side: order.intent.side(),
            intent: order.intent,
            px,
            qty: order.qty,
            cum_qty: Qty::ZERO,
            leaves_qty: order.qty,
            state: OrderState::New,
        },
        kind,
        fill: None,
        reject: None,
        unsolicited: None,
        transact_ns: Some(transact_ns),
    })
}

/// Others' three levels per side, plus our own size at `ours` (bid, ask) where the book shows it.
fn vwap_book(ns: i64, ours: (i64, i64), our_qty: Qty) -> Event {
    let side = |levels: &[(i64, i64)], mine: i64, bids: bool| {
        let mut v: Vec<Level> = levels.iter().map(|&(p, q)| (Px(p), Qty(q))).collect();
        match v.iter_mut().find(|l| l.0 == Px(mine)) {
            Some(l) => l.1 += our_qty,
            None => v.push((Px(mine), our_qty)),
        }
        v.sort_by_key(|l| if bids { -l.0 .0 } else { l.0 .0 });
        v
    };
    Event::Book(BookFrame {
        slug: Arc::from(SLUG),
        bids: side(
            &[(4500, 100_000), (4400, 100_000), (3000, 100_000)],
            ours.0,
            true,
        ),
        offers: side(
            &[(5500, 100_000), (5600, 100_000), (8000, 100_000)],
            ours.1,
            false,
        ),
        state: MarketState::Open,
        transact_ns: Some(ns),
        shares_traded: Some(Qty::ZERO),
        settlement: None,
    })
}

fn order_commands(cmds: &[Command]) -> Vec<&Command> {
    cmds.iter()
        .filter(|c| {
            matches!(
                c,
                Command::Place { .. } | Command::Modify { .. } | Command::Cancel { .. }
            )
        })
        .collect()
}

/// The 10-29 UTC live loop: a replace ack arrives ~50 ms before the book that shows it. Until then the
/// book still has our order at the old price, which must not count as someone else's; otherwise a
/// one-contract order pushes a real level out of the top three, fair jumps, and the ack's
/// re-decide modifies straight back.
#[test]
fn two_phase_vwap_does_not_reprice_off_its_own_order_before_the_book_shows_the_replace() {
    let mut cfg = Config::default();
    cfg.execution.reprice_min_ticks = 1; // the one-tick bid move below must go out
    cfg.strategies.insert(
        "default".into(),
        pm_core::StrategyConfig::TwoPhaseVwap(Default::default()),
    );
    let (mut core, placed) = startup_with(cfg);
    let [(bid_token, bid), (ask_token, ask)] = [placed[0].clone(), placed[1].clone()];
    assert_eq!((bid.px, ask.px), (Px(4000), Px(6000)));
    for (i, (token, order)) in [(bid_token, &bid), (ask_token, &ask)]
        .into_iter()
        .enumerate()
    {
        core.tick(
            clock(SEC + 10 * i as u64),
            vec![Event::RestResult(RestResult {
                kind: RestKind::Create,
                tokens: vec![token],
                outcome: RestOutcome::Accepted {
                    ids: vec![Some(Arc::from(format!("Q{i}")))],
                },
            })],
        );
        core.tick(
            clock(SEC + 10 * i as u64 + 1),
            vec![exec_at(&format!("Q{i}"), order, order.px, ExecKind::New, 2)],
        );
    }

    // Others' book: bids 0.45/0.44/0.30, offers 0.55/0.56/0.80, 10 each: fair 31/60 ≈ 0.5167.
    let out = core.tick(clock(2 * SEC), vec![vwap_book(3, (4000, 6000), bid.qty)]);
    let modifies: Vec<(OrderId, Px)> = out
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::Modify { order_id, px, .. } => Some((order_id.clone(), *px)),
            _ => None,
        })
        .collect();
    assert_eq!(
        modifies,
        vec![(Arc::from("Q0"), Px(4100)), (Arc::from("Q1"), Px(6200))]
    );
    let fair = core.snapshot(clock(2 * SEC)).markets[0].fair.unwrap();

    // The replace acks, ahead of any book that shows them: the book still has us at 0.40/0.60.
    for (i, (order, px)) in [(&bid, Px(4100)), (&ask, Px(6200))].into_iter().enumerate() {
        let out = core.tick(
            clock(3 * SEC + i as u64),
            vec![exec_at(&format!("Q{i}"), order, px, ExecKind::Replace, 4)],
        );
        assert!(
            order_commands(&out.commands).is_empty(),
            "{:?}",
            out.commands
        );
    }
    let row = core.snapshot(clock(3 * SEC)).markets.remove(0);
    assert!((row.fair.unwrap() - fair).abs() < 1e-9, "{row:?}");

    // The book catches up with our new prices: still nothing to do.
    let out = core.tick(clock(4 * SEC), vec![vwap_book(5, (4100, 6200), bid.qty)]);
    assert!(
        order_commands(&out.commands).is_empty(),
        "{:?}",
        out.commands
    );
    assert!((core.snapshot(clock(4 * SEC)).markets[0].fair.unwrap() - fair).abs() < 1e-9);
}

/// A modify that raises a quote's size still reserves the larger size while it's in flight, so the
/// next market can't take the same account headroom. (Code review CORE-01.)
#[test]
fn pending_modify_keeps_its_reservation_for_the_next_market() {
    let mut cfg = Config::default();
    cfg.risk.max_account_loss = 2.0;
    cfg.strategies.insert(
        "default".into(),
        pm_core::StrategyConfig::FullMatch(pm_core::strategy::FullMatchConfig {
            size: 5.0,
            price_band_lo: 0.5,
            ..Default::default()
        }),
    );
    let mut core = Core::new(cfg);
    let market = |slug: &str| MarketParams {
        slug: Arc::from(slug),
        event_id: slug.into(),
        event_slug: slug.into(),
        game_start_ns: None,
        ..params()
    };
    let book_in = |slug: &str, ns: i64, bid_qty: i64| {
        Event::Book(BookFrame {
            slug: Arc::from(slug),
            bids: vec![(Px(4500), Qty(bid_qty))],
            offers: vec![(Px(5500), Qty(100_000))],
            state: MarketState::Open,
            transact_ns: Some(ns),
            shares_traded: None,
            settlement: None,
        })
    };
    // Another market's exposure leaves room for one contract only, so A's ask is clipped to one.
    core.portfolio
        .on_fill(&Arc::from("c"), Side::Buy, Qty(33_000), Px(5000), 0.0);
    let out = core.tick(
        clock(SEC),
        vec![
            Event::Markets(vec![market("a"), market("b")]),
            Event::Connection {
                feed: Feed::Markets,
                state: ConnState::Up,
            },
            Event::Connection {
                feed: Feed::Private,
                state: ConnState::Up,
            },
            book_in("a", 1, 100_000),
        ],
    );
    let (token, order) = places(&out.commands).into_iter().next().unwrap();
    assert_eq!((order.intent.side(), order.qty), (Side::Sell, Qty(10_000)));
    ack(&mut core, SEC + 1, token, "O", &order);
    // That room frees up: A's ask grows to five contracts, and the modify is in flight.
    core.portfolio
        .on_fill(&Arc::from("c"), Side::Sell, Qty(33_000), Px(5000), 0.0);
    let out = core.tick(clock(SEC + 3), vec![book_in("a", 2, 100_001)]);
    assert!(
        out.commands.iter().any(|c| matches!(
            c,
            Command::Modify {
                qty: Qty(50_000),
                ..
            }
        )),
        "{:?}",
        out.commands
    );
    // B quotes before the modify is confirmed: it only gets what A's larger size leaves.
    let out = core.tick(clock(SEC + 4), vec![book_in("b", 1, 100_000)]);
    let pending_a = 5.0 * (1.0 - order.px.as_f64());
    let b_worst: f64 = places(&out.commands)
        .iter()
        .map(|(_, o)| match o.intent.side() {
            Side::Buy => o.qty.notional(o.px),
            _ => o.qty.as_f64() * (1.0 - o.px.as_f64()),
        })
        .fold(0.0, f64::max);
    assert!(b_worst > 0.0, "B should still quote with what is left");
    assert!(
        pending_a + b_worst <= 2.0 + 1e-9,
        "A {pending_a} + B {b_worst} over the $2 cap"
    );
}

#[test]
fn delisted_market_is_retired_once_flat_and_quiet() {
    let (mut core, placed) = ready_core();
    for (i, (token, order)) in placed.iter().enumerate() {
        ack(
            &mut core,
            2 * SEC + i as u64 * 10,
            *token,
            &format!("O{i}"),
            order,
        );
    }
    let out = core.tick(clock(3 * SEC), vec![Event::Markets(vec![])]);
    assert_eq!(cancels(&out.commands), 2);
    let retired = |derived: &[pm_core::Derived]| derived.iter().any(|d| d.kind == "market_retired");

    // Its orders are still being cancelled after the grace period: kept.
    let out = core.tick(clock(3_700 * SEC), vec![]);
    assert!(!retired(&out.derived));
    assert_eq!(core.market_count(), 1);

    for (i, (_, order)) in placed.iter().enumerate() {
        let id = format!("O{i}");
        core.tick(
            clock(3_701 * SEC),
            vec![Event::Execution(ExecutionFrame {
                exec_id: format!("cxl-{id}"),
                order: VenueOrder {
                    id: Arc::from(id.as_str()),
                    slug: order.slug.clone(),
                    side: order.intent.side(),
                    intent: order.intent,
                    px: order.px,
                    qty: order.qty,
                    cum_qty: Qty::ZERO,
                    leaves_qty: Qty::ZERO,
                    state: OrderState::Canceled,
                },
                kind: ExecKind::Canceled,
                fill: None,
                reject: None,
                unsolicited: None,
                transact_ns: None,
            })],
        );
    }
    let out = core.tick(clock(3_702 * SEC), vec![]);
    assert!(retired(&out.derived), "{:?}", out.derived);
    assert_eq!(core.market_count(), 0);

    // Relisted later: a fresh market.
    let out = core.tick(clock(3_800 * SEC), vec![Event::Markets(vec![params()])]);
    assert!(out.derived.iter().any(|d| d.kind == "market_added"));
    assert_eq!(core.market_count(), 1);
}

/// A core with a filled long of one bid's size in `SLUG`, bought at that bid.
fn long_position() -> (Core, NewOrder) {
    let (mut core, placed) = ready_core();
    let (token, order) = placed
        .iter()
        .find(|(_, o)| o.intent == Intent::BuyLong)
        .unwrap()
        .clone();
    ack(&mut core, 2 * SEC, token, "B1", &order);
    core.tick(
        clock(3 * SEC),
        vec![Event::Execution(ExecutionFrame {
            exec_id: "fill-1".into(),
            order: VenueOrder {
                id: Arc::from("B1"),
                slug: Arc::from(SLUG),
                side: Side::Buy,
                intent: Intent::BuyLong,
                px: order.px,
                qty: order.qty,
                cum_qty: order.qty,
                leaves_qty: Qty::ZERO,
                state: OrderState::Filled,
            },
            kind: ExecKind::Fill,
            fill: Some(FillInfo {
                last_shares: order.qty,
                last_px: order.px,
                trade_id: "T1".into(),
                aggressor: false,
                commission: Some(0.0),
            }),
            reject: None,
            unsolicited: None,
            transact_ns: None,
        })],
    );
    assert_eq!(core.portfolio.position(SLUG).qty, order.qty);
    (core, order)
}

fn expired_book(settlement: Option<Px>) -> Event {
    Event::Book(BookFrame {
        slug: Arc::from(SLUG),
        bids: vec![],
        offers: vec![],
        state: MarketState::Expired,
        transact_ns: Some(10),
        shares_traded: None,
        settlement,
    })
}

fn flat_on_venue() -> Event {
    Event::Positions(vec![PositionFrame {
        slug: Arc::from(SLUG),
        net: Qty::ZERO,
        cost: None,
        resolution: false,
    }])
}

#[test]
fn settlement_on_the_expired_book_closes_the_position_at_the_outcome() {
    let (mut core, order) = long_position();
    let out = core.tick(clock(4 * SEC), vec![expired_book(Some(Px(10_000)))]);
    assert!(
        out.derived.iter().any(|d| d.kind == "settled"),
        "{:?}",
        out.derived
    );
    let pos = core.portfolio.position(SLUG);
    assert!(pos.qty.is_zero());
    // Bought at the bid, paid out $1: realised PnL is the full edge, not half of it.
    let expected = order.qty.as_f64() * (1.0 - order.px.as_f64());
    assert!(
        (pos.pnl_at(0.5) - expected).abs() < 1e-9,
        "{} vs {expected}",
        pos.pnl_at(0.5)
    );
    // The venue's flat position is then no mismatch, and a repeat of the price changes nothing.
    let out = core.tick(
        clock(5 * SEC),
        vec![flat_on_venue(), expired_book(Some(Px(10_000)))],
    );
    assert!(
        !out.derived
            .iter()
            .any(|d| d.kind == "position_mismatch" || d.kind == "settled"),
        "{:?}",
        out.derived
    );
}

#[test]
fn resolution_without_a_price_asks_rest_and_never_reconciles_at_a_guess() {
    let (mut core, order) = long_position();
    let resolution = Event::Position(PositionFrame {
        slug: Arc::from(SLUG),
        net: Qty::ZERO,
        cost: None,
        resolution: true,
    });
    let out = core.tick(clock(4 * SEC), vec![resolution, expired_book(None)]);
    assert!(
        out.commands
            .iter()
            .any(|c| matches!(c, Command::QuerySettlement { slug } if &**slug == SLUG)),
        "{:?}",
        out.commands
    );

    // The periodic positions read sees the venue flat: left alone, and not asked again within 30 s.
    let out = core.tick(
        clock(10 * SEC),
        vec![
            flat_on_venue(),
            Event::Settlement {
                slug: Arc::from(SLUG),
                px: None,
            },
        ],
    );
    assert!(
        !out.derived.iter().any(|d| d.kind == "position_mismatch"),
        "{:?}",
        out.derived
    );
    assert!(!out
        .commands
        .iter()
        .any(|c| matches!(c, Command::QuerySettlement { .. })));
    assert_eq!(core.portfolio.position(SLUG).qty, order.qty);
    let out = core.tick(clock(40 * SEC), vec![flat_on_venue()]);
    assert!(out
        .commands
        .iter()
        .any(|c| matches!(c, Command::QuerySettlement { .. })));

    // Settled at 0: the long loses what it paid.
    let out = core.tick(
        clock(41 * SEC),
        vec![Event::Settlement {
            slug: Arc::from(SLUG),
            px: Some(Px(0)),
        }],
    );
    assert!(out.derived.iter().any(|d| d.kind == "settled"));
    let pos = core.portfolio.position(SLUG);
    assert!(pos.qty.is_zero());
    assert!((pos.pnl_at(0.5) + order.qty.as_f64() * order.px.as_f64()).abs() < 1e-9);
}

fn trade(ns: i64) -> Event {
    Event::Trade(TradeFrame {
        id: format!("t{ns}"),
        slug: Arc::from(SLUG),
        px: Px(5500),
        qty: Qty(100),
        taker_side: Side::Buy,
        trade_ns: Some(ns),
    })
}

#[test]
fn a_trade_with_no_book_after_it_pulls_the_quotes_until_a_book_arrives() {
    let (mut core, placed) = ready_core();
    for (i, (token, order)) in placed.iter().enumerate() {
        ack(
            &mut core,
            2 * SEC + 10 * i as u64,
            *token,
            &format!("Q{i}"),
            order,
        );
    }
    core.tick(clock(3 * SEC), vec![trade(5)]);
    let out = core.tick(clock(12 * SEC), vec![]);
    assert_eq!(cancels(&out.commands), 0, "not yet: {:?}", out.commands);
    let out = core.tick(clock(13 * SEC + 1), vec![]);
    assert!(
        out.derived.iter().any(|d| d.kind == "book_stale"),
        "{:?}",
        out.derived
    );
    assert_eq!(cancels(&out.commands), placed.len());
    // A book that includes the trade makes the market quotable again.
    let out = core.tick(clock(14 * SEC), vec![book(6, 4500, 5500)]);
    let out2 = core.tick(clock(15 * SEC), vec![]);
    assert!(out
        .derived
        .iter()
        .chain(&out2.derived)
        .any(|d| d.kind == "book_stale" && d.detail.contains("again")));
}

#[test]
fn loss_limits_mark_a_long_at_others_bid_when_the_book_blows_out() {
    let (mut core, order) = long_position();
    // 90 ¢ wide: the strategy has no fair and the old mark fell back to cost (PnL 0).
    core.tick(clock(4 * SEC), vec![book(2, 500, 9500)]);
    core.tick(clock(5 * SEC), vec![]);
    let expected = order.qty.as_f64() * (0.05 - order.px.as_f64());
    let snap = core.snapshot(clock(5 * SEC));
    assert!(
        (snap.daily_pnl - expected).abs() < 1e-9,
        "{} vs {expected}",
        snap.daily_pnl
    );
    // With a tight book, the worse of fair (mid) and others' bid is the bid.
    core.tick(clock(6 * SEC), vec![book(3, 4400, 4600)]);
    core.tick(clock(7 * SEC), vec![]);
    let expected = order.qty.as_f64() * (0.44 - order.px.as_f64());
    assert!((core.snapshot(clock(7 * SEC)).daily_pnl - expected).abs() < 1e-9);
}

#[test]
fn the_daily_kill_and_operator_pauses_survive_a_restart() {
    let mut cfg = Config::default();
    cfg.risk.daily_loss_limit = 0.05;
    let (mut core, placed) = startup_with(cfg.clone());
    let (token, order) = placed
        .iter()
        .find(|(_, o)| o.intent == Intent::BuyLong)
        .unwrap()
        .clone();
    ack(&mut core, 2 * SEC, token, "B1", &order);
    core.tick(
        clock(3 * SEC),
        vec![Event::Execution(ExecutionFrame {
            exec_id: "fill-1".into(),
            order: VenueOrder {
                id: Arc::from("B1"),
                slug: Arc::from(SLUG),
                side: Side::Buy,
                intent: Intent::BuyLong,
                px: order.px,
                qty: order.qty,
                cum_qty: order.qty,
                leaves_qty: Qty::ZERO,
                state: OrderState::Filled,
            },
            kind: ExecKind::Fill,
            fill: Some(FillInfo {
                last_shares: order.qty,
                last_px: order.px,
                trade_id: "T1".into(),
                aggressor: false,
                commission: Some(0.0),
            }),
            reject: None,
            unsolicited: None,
            transact_ns: None,
        })],
    );
    let out = core.tick(
        clock(4 * SEC),
        vec![
            book(2, 500, 9500),
            Event::Admin(AdminCmd::Pause(Some("other-market".into()))),
        ],
    );
    assert!(
        out.commands
            .iter()
            .any(|c| matches!(c, Command::CancelAll { .. })),
        "kill fired: {:?}",
        out.derived
    );
    let state = core.risk_state(clock(5 * SEC).wall_ns);
    assert!(state.killed && state.daily_pnl < -0.05);
    assert_eq!(
        state.paused.get("other-market").map(String::as_str),
        Some("operator pause")
    );

    // Same UTC day, fresh process: no quotes, and the day's loss is still there.
    let json = serde_json::to_string(&state).unwrap();
    let mut core = Core::new(cfg.clone());
    core.restore_risk_state(&serde_json::from_str(&json).unwrap(), clock(SEC).wall_ns);
    let out = core.tick(
        clock(SEC),
        vec![
            Event::Markets(vec![params()]),
            Event::Connection {
                feed: Feed::Markets,
                state: ConnState::Up,
            },
            Event::OrderSnapshot(vec![]),
            Event::Positions(vec![]),
            Event::Balance(BalanceFrame {
                current_balance: 100.0,
                buying_power: 100.0,
                open_orders: 0.0,
            }),
            Event::Connection {
                feed: Feed::Private,
                state: ConnState::Up,
            },
            book(1, 4500, 5500),
        ],
    );
    assert!(places(&out.commands).is_empty(), "{:?}", out.commands);
    assert!(core.snapshot(clock(SEC)).killed);
    assert_eq!(core.risk_state(clock(SEC).wall_ns), state);

    // The next UTC day the kill is over, but the operator pause isn't.
    let next_day = 86_400 * SEC as i64;
    let mut core = Core::new(cfg);
    core.restore_risk_state(&state, clock(SEC).wall_ns + next_day);
    let after = core.risk_state(clock(SEC).wall_ns + next_day);
    assert!(!after.killed && after.daily_pnl == 0.0);
    assert_eq!(after.paused, state.paused);
}
