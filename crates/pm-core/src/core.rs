//! The tick loop.
//!
//! ```text
//! 1. Drain   events, sorted by priority class (control, feedback, game, trades, books)
//! 2. Update  each event goes to its owner → derived events + dirty markets
//! 3. React   risk handles urgent derived events (cancel, pause, kill)
//! 4. Decide  strategy runs for dirty markets only
//! 5. Gate    risk clips or vetoes targets, with account reservations
//! 6. Act     order manager diffs targets vs working orders → commands
//! 7. Record  the shell journals the TickOutput
//! ```
//!
//! The core is synchronous and deterministic: it reads no clocks (the shell
//! passes `Clock`), and iterates only ordered maps.

use std::collections::{BTreeMap, BTreeSet};

use pm_types::time::NS_PER_SEC;
use pm_types::{
    AdminCmd, Command, ConnState, Event, Feed, Intent, MarketParams, MarketState, Priority, Px,
    Qty, Side, Slug, Tif,
};
use serde::Serialize;

use crate::config::{secs, Config};
use crate::game::{GameChange, GameStore};
use crate::market_data::{BookView, MarketData, MdChange};
use crate::orders::{LocalState, OmEvent, OrderManager, QuoteSide, QuoteTarget};
use crate::portfolio::{worst_case_loss, Portfolio, Resting};
use crate::risk::{GateCtx, Risk, RiskAction, RiskState};
use crate::strategy::{Decision, LeagueCheck, MarketView, Off, Phase, Strategy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// Monotonic nanoseconds (process-relative live, simulated in backtests).
    pub mono: u64,
    /// Unix nanoseconds, for GTD expiries, game start times and the UTC day.
    pub wall_ns: i64,
}

/// Something the core noticed, for the journal and the log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Derived {
    pub kind: &'static str,
    pub slug: Option<Slug>,
    pub detail: String,
}

#[derive(Debug, Default, Serialize)]
pub struct TickOutput {
    pub commands: Vec<Command>,
    pub derived: Vec<Derived>,
    pub decisions: Vec<(Slug, Decision)>,
    /// Confirmed private execution reports, for shell correlation (not REST snapshots).
    pub execution_acks: Vec<(pm_types::Token, pm_types::RestKind)>,
    /// Latency samples observed this tick, in nanoseconds.
    pub latencies: Vec<(&'static str, u64)>,
}

pub(crate) struct Market {
    pub(crate) params: MarketParams,
    pub(crate) md: MarketData,
    pub(crate) active: bool,
    /// This market's own strategy instance, from the profile its route picked.
    pub(crate) strategy: Box<dyn Strategy>,
    /// Profile name (and route), for status and logs.
    pub(crate) strategy_label: String,
    /// The strategy's next time-driven change (wall ns), from its last decision.
    pub(crate) wake_ns: Option<i64>,
    /// When discovery stopped listing it (mono), while it isn't listed.
    pub(crate) delisted_at: Option<u64>,
    /// The YES settlement price, once the market has resolved and the price is known.
    pub(crate) settlement: Option<Px>,
    /// The venue reported the resolution (a `RESOLUTION` position change) before the price arrived.
    pub(crate) resolving: bool,
    /// When the settlement price was last asked for over REST (mono).
    pub(crate) settlement_asked: Option<u64>,
    /// The book was found stale at the last housekeeping (see `MarketData::stale_reason`).
    pub(crate) book_stale: bool,
}

/// How often to re-ask for a settlement price the venue hasn't published yet.
const SETTLEMENT_QUERY_S: f64 = 30.0;
/// The early positions read after a private reconnect or a difference: long enough for executions
/// already in flight to arrive.
const POSITIONS_REREAD_S: f64 = 3.0;

/// How long a delisted market's state is kept before it is dropped, for a relisting or late feedback.
const RETIRE_AFTER_S: f64 = 3_600.0;

pub struct Core {
    pub cfg: Config,
    pub(crate) markets: BTreeMap<Slug, Market>,
    pub(crate) games: GameStore,
    pub om: OrderManager,
    pub portfolio: Portfolio,
    pub risk: Risk,
    pub(crate) markets_up: bool,
    pub(crate) private_up: bool,
    next_housekeeping: u64,
    /// Last housekeeping run (mono, wall day, game staleness), to see what changed since.
    last_housekeeping: (u64, i64, bool),
    last_redecide_all: u64,
    last_positions_query: u64,
    /// A positions read due sooner than the regular one: after a reconnect, or to confirm a difference.
    positions_reread_at: Option<u64>,
    last_balance_query: u64,
    pub(crate) last_decisions: BTreeMap<Slug, Decision>,
}

/// Per-tick scratch state.
#[derive(Default)]
struct Tick {
    dirty: BTreeSet<Slug>,
    input_updated: BTreeSet<Slug>,
    urgent: BTreeSet<Slug>,
    filled: BTreeSet<Slug>,
    must_act: BTreeSet<Slug>,
    commands: Vec<Command>,
    derived: Vec<Derived>,
    actions: Vec<RiskAction>,
    execution_acks: Vec<(pm_types::Token, pm_types::RestKind)>,
    latencies: Vec<(&'static str, u64)>,
}

impl Tick {
    fn note(&mut self, kind: &'static str, slug: Option<&Slug>, detail: impl Into<String>) {
        self.derived.push(Derived {
            kind,
            slug: slug.cloned(),
            detail: detail.into(),
        });
    }
}

fn utc_day(wall_ns: i64) -> i64 {
    wall_ns.div_euclid(86_400 * NS_PER_SEC)
}

/// Others' book: the provisional view minus our own resting orders, where that book shows them.
/// Executions usually arrive before the book that includes them, so our changes newer than the
/// book are undone first: a replaced order still counts at its old price, a new one not at all.
/// Otherwise the book's copy of our old order reads as someone else's, the strategy reprices, and
/// the next ack does it again (10-29 UTC live run: 28.7k modifies in 25 minutes).
fn others_view(md: &MarketData, om: &OrderManager, slug: &str) -> BookView {
    let mut others = md.provisional_view();
    let mut ours: Vec<(Side, Px, Qty)> = Vec::new();
    let mut add =
        |side: Side, px: Px, qty: Qty| match ours.iter_mut().find(|o| o.0 == side && o.1 == px) {
            Some(o) => o.2 += qty,
            None => ours.push((side, px, qty)),
        };
    for rec in om.active_in(slug).filter(|r| r.on_book()) {
        add(rec.side, rec.px, rec.leaves_qty);
    }
    if let Some(book_ns) = md.book.as_ref().and_then(|b| b.transact_ns) {
        for c in om.unshown(slug, book_ns) {
            add(c.side, c.px, -c.qty);
        }
    }
    for (side, px, qty) in ours {
        if qty > Qty::ZERO {
            others.remove(side, px, qty);
        }
    }
    others
}

impl Core {
    pub fn new(cfg: Config) -> Self {
        Core {
            risk: Risk::new(cfg.risk.clone()),
            cfg,
            markets: BTreeMap::new(),
            games: GameStore::default(),
            om: OrderManager::default(),
            portfolio: Portfolio::default(),
            markets_up: false,
            private_up: false,
            next_housekeeping: 0,
            last_housekeeping: (0, 0, false),
            last_redecide_all: 0,
            last_positions_query: 0,
            positions_reread_at: None,
            last_balance_query: 0,
            last_decisions: BTreeMap::new(),
        }
    }

    /// When the shell should wake the core even if no event arrives.
    pub fn next_deadline(&self) -> u64 {
        self.next_housekeeping
    }

    pub fn tick(&mut self, clock: Clock, mut events: Vec<Event>) -> TickOutput {
        events.sort_by_key(Event::priority);
        let mut t = Tick::default();
        for event in events {
            self.update(event, clock, &mut t);
        }
        self.drain_om_outbox(&mut t);
        if clock.mono >= self.next_housekeeping {
            self.housekeeping(clock, &mut t);
            self.next_housekeeping = clock.mono + self.cfg.execution.housekeeping_ms * 1_000_000;
        }
        self.react(clock, &mut t);
        self.decide_and_act(clock, &mut t);
        self.drain_om_outbox(&mut t);
        let mut decisions = Vec::new();
        for slug in &t.dirty {
            if let Some(d) = self.last_decisions.get(slug) {
                decisions.push((slug.clone(), d.clone()));
            }
        }
        TickOutput {
            commands: t.commands,
            derived: t.derived,
            decisions,
            latencies: t.latencies,
            execution_acks: t.execution_acks,
        }
    }

    fn drain_om_outbox(&mut self, t: &mut Tick) {
        t.commands.append(&mut self.om.outbox);
    }

    fn markets_of_event(&self, event_id: &str) -> Vec<Slug> {
        self.markets
            .values()
            .filter(|m| m.params.event_id == event_id)
            .map(|m| m.params.slug.clone())
            .collect()
    }

    // ------------------------------------------------------------------ 2. update

    fn update(&mut self, event: Event, clock: Clock, t: &mut Tick) {
        let now = clock.mono;
        match event {
            Event::Markets(list) => self.on_markets(list, now, t),
            Event::Book(mut book) => {
                let depth = self.cfg.markets.book_depth;
                if depth > 0 {
                    book.bids.truncate(depth);
                    book.offers.truncate(depth);
                }
                let slug = book.slug.clone();
                // Before the book checks: the last books of a resolved market may be rejected as stale.
                if let Some(px) = book.settlement {
                    self.settle(&slug, px, t);
                }
                let Some(m) = self.markets.get_mut(&slug) else {
                    return;
                };
                let changes = m.md.on_book(book, m.params.tick, m.params.min_qty);
                if let Some(ns) = m.md.book.as_ref().and_then(|b| b.transact_ns) {
                    self.om.book_seen(&slug, ns);
                }
                if !changes
                    .iter()
                    .any(|c| matches!(c, MdChange::Stale | MdChange::Invalid(_)))
                {
                    m.md.last_book_mono = Some(now);
                    t.input_updated.insert(slug.clone());
                    if m.strategy.input_driven() || m.strategy.uses_book_depth() {
                        t.dirty.insert(slug.clone());
                    }
                }
                for change in changes {
                    match change {
                        MdChange::Seeded | MdChange::TouchChanged => {
                            t.dirty.insert(slug.clone());
                        }
                        MdChange::StateChanged(state) => {
                            t.note("market_state", Some(&slug), state.wire());
                            t.dirty.insert(slug.clone());
                            if state != MarketState::Open {
                                t.urgent.insert(slug.clone());
                                t.actions.push(RiskAction::CancelMarket(slug.clone()));
                            }
                        }
                        MdChange::MissedVolume(q) => {
                            t.note("missed_volume", Some(&slug), q.to_string())
                        }
                        MdChange::Invalid(problems) => {
                            t.note("invalid_book", Some(&slug), problems.join(", "))
                        }
                        MdChange::Stale | MdChange::Sweep => {}
                    }
                }
            }
            Event::Trade(trade) => {
                let slug = trade.slug.clone();
                if let Some(m) = self.markets.get_mut(&slug) {
                    if !m.md.on_trade(trade, now).is_empty() {
                        t.input_updated.insert(slug.clone());
                        t.dirty.insert(slug);
                    }
                }
            }
            Event::Game(frame) => {
                let event_id = frame.event_id.clone();
                let changes = self.games.on_game(frame, now);
                for slug in self.markets_of_event(&event_id) {
                    t.input_updated.insert(slug.clone());
                    if changes.is_empty() && !self.markets[&slug].strategy.input_driven() {
                        continue;
                    }
                    t.dirty.insert(slug.clone());
                    for change in &changes {
                        match change {
                            GameChange::LegChanged { prev, new } => {
                                t.note("leg_changed", Some(&slug), format!("{prev:?} -> {new:?}"));
                                t.urgent.insert(slug.clone());
                                t.must_act.insert(slug.clone());
                            }
                            GameChange::Live => t.note("match_live", Some(&slug), ""),
                            GameChange::Ended | GameChange::Cancelled => {
                                t.note("match_over", Some(&slug), format!("{change:?}"));
                                t.urgent.insert(slug.clone());
                                t.actions.push(RiskAction::CancelMarket(slug.clone()));
                            }
                            GameChange::UnknownPeriod(p) => {
                                t.note("unknown_period", Some(&slug), p.clone())
                            }
                        }
                    }
                }
            }
            Event::GamePolled => self.games.on_poll(now),
            Event::OrderSnapshot(orders) => {
                let (events, commands) = self.om.reconcile(None, &orders, now);
                t.commands.extend(commands);
                self.on_om_events(events, clock, t);
            }
            Event::Execution(exec) => {
                let events = self.om.on_execution(exec, now);
                self.on_om_events(events, clock, t);
            }
            Event::RestResult(result) => {
                let events = self.om.on_rest_result(&result, now);
                self.on_om_events(events, clock, t);
            }
            Event::OpenOrders { slugs, orders } => {
                let (events, commands) = self.om.reconcile(Some(&slugs), &orders, now);
                t.commands.extend(commands);
                self.on_om_events(events, clock, t);
            }
            Event::OrderLookup { id, order } => {
                let (events, commands) = self.om.on_order_lookup(&id, order.as_ref(), now);
                t.commands.extend(commands);
                self.on_om_events(events, clock, t);
            }
            Event::Position(p) => {
                t.note(
                    "venue_position",
                    Some(&p.slug),
                    format!(
                        "{}{}",
                        p.net,
                        if p.resolution { " (resolution)" } else { "" }
                    ),
                );
                if p.resolution {
                    if let Some(m) = self.markets.get_mut(&p.slug) {
                        m.resolving = m.settlement.is_none();
                    }
                    self.ask_settlement(&p.slug, now, t);
                }
            }
            Event::Settlement { slug, px } => match px {
                Some(px) => self.settle(&slug, px, t),
                None => t.note("settlement_pending", Some(&slug), "not published yet"),
            },
            Event::Positions(list) => {
                let marks: BTreeMap<Slug, f64> = self.marks();
                // A market that has resolved without a known price would otherwise be "reconciled" to the
                // venue's flat position at a guessed mark. Leave it to the settlement price instead.
                let awaiting: BTreeSet<Slug> = self
                    .markets
                    .iter()
                    .filter(|(slug, m)| self.awaiting_settlement(slug, m))
                    .map(|(s, _)| s.clone())
                    .collect();
                for slug in &awaiting {
                    self.ask_settlement(slug, now, t);
                }
                let (mismatches, held) = self.portfolio.on_venue_positions(
                    &list,
                    Qty(100),
                    |s| marks.get(s).copied().unwrap_or(0.5),
                    |s| awaiting.contains(s),
                );
                for m in &held {
                    t.note(
                        "position_differs",
                        Some(&m.slug),
                        format!(
                            "local {} venue {}; reading again to confirm",
                            m.local, m.venue
                        ),
                    );
                }
                if !held.is_empty() {
                    self.positions_reread_at = Some(now + secs(POSITIONS_REREAD_S));
                }
                for m in mismatches {
                    t.note(
                        "position_mismatch",
                        Some(&m.slug),
                        format!("local {} venue {}", m.local, m.venue),
                    );
                    self.risk.pause(
                        &m.slug,
                        now,
                        Some(self.cfg.risk.market_pause_s),
                        "position mismatch",
                    );
                    t.actions.push(RiskAction::CancelMarket(m.slug.clone()));
                    t.dirty.insert(m.slug);
                }
            }
            Event::Balance(b) => self.portfolio.balance = Some(b),
            Event::Connection { feed, state } => {
                t.note("connection", None, format!("{feed:?} {state:?}"));
                match (feed, state) {
                    (Feed::Markets, ConnState::Down) => {
                        self.markets_up = false;
                        for m in self.markets.values_mut() {
                            m.md.unseed();
                        }
                        let slugs: Vec<Slug> = self.markets.keys().cloned().collect();
                        for slug in slugs {
                            t.actions.push(RiskAction::CancelMarket(slug.clone()));
                            t.dirty.insert(slug);
                        }
                    }
                    (Feed::Markets, ConnState::Up) => self.markets_up = true,
                    (Feed::Private, ConnState::Down) => {
                        self.private_up = false;
                        self.om.on_private_down(now);
                        t.actions.push(RiskAction::CancelAll);
                        t.dirty.extend(self.markets.keys().cloned());
                    }
                    (Feed::Private, ConnState::Up) => {
                        self.private_up = true;
                        // Executions buffered at startup may already be in the positions just read.
                        self.positions_reread_at = Some(now + secs(POSITIONS_REREAD_S));
                        t.dirty.extend(self.markets.keys().cloned());
                    }
                    (Feed::Game, _) => {}
                }
            }
            Event::Admin(cmd) => {
                t.note("admin", None, format!("{cmd:?}"));
                match cmd {
                    AdminCmd::Pause(None) => {
                        self.risk.admin_paused = true;
                        t.dirty.extend(self.markets.keys().cloned());
                    }
                    AdminCmd::Pause(Some(slug)) => {
                        let slug: Slug = slug.into();
                        self.risk.pause(&slug, now, None, "operator pause");
                        t.dirty.insert(slug);
                    }
                    AdminCmd::Resume(slug) => {
                        self.risk.resume(slug.as_deref());
                        t.dirty.extend(self.markets.keys().cloned());
                    }
                    AdminCmd::CancelAll => {
                        // Cancel and stay paused until the operator resumes.
                        self.risk.admin_paused = true;
                        t.actions.push(RiskAction::CancelAll);
                        t.dirty.extend(self.markets.keys().cloned());
                    }
                }
            }
        }
    }

    fn on_markets(&mut self, list: Vec<MarketParams>, now: u64, t: &mut Tick) {
        let wanted: Vec<MarketParams> = list
            .into_iter()
            .filter(|p| {
                self.cfg
                    .markets
                    .types
                    .iter()
                    .any(|ty| *ty == p.sports_market_type)
            })
            .collect();
        let listed: BTreeSet<Slug> = wanted.iter().map(|p| p.slug.clone()).collect();
        for params in wanted {
            let slug = params.slug.clone();
            match self.markets.get_mut(&slug) {
                None => {
                    // A config that passed `validate` always resolves; if not, don't trade the market.
                    let (strategy, strategy_label) = match self.cfg.strategy_for(&params) {
                        Ok(r) => (
                            r.config.build(),
                            format!("{} [{}]", r.label(), r.config.kind()),
                        ),
                        Err(e) => {
                            t.note("strategy_error", Some(&slug), e);
                            (
                                Box::new(Off) as Box<dyn Strategy>,
                                "off (config error)".to_string(),
                            )
                        }
                    };
                    t.note(
                        "market_added",
                        Some(&slug),
                        format!("{} | strategy {strategy_label}", params.title),
                    );
                    self.markets.insert(
                        slug.clone(),
                        Market {
                            params,
                            md: MarketData::default(),
                            active: true,
                            strategy,
                            strategy_label,
                            wake_ns: None,
                            delisted_at: None,
                            settlement: None,
                            resolving: false,
                            settlement_asked: None,
                            book_stale: false,
                        },
                    );
                }
                Some(m) => {
                    if m.params.tick != params.tick || m.params.min_qty != params.min_qty {
                        t.note(
                            "market_params_changed",
                            Some(&slug),
                            format!("tick {} min {}", params.tick, params.min_qty),
                        );
                        t.actions.push(RiskAction::CancelMarket(slug.clone()));
                    }
                    if !m.active {
                        t.note("market_relisted", Some(&slug), "");
                    }
                    m.params = params;
                    m.active = true;
                    m.delisted_at = None;
                }
            }
            t.dirty.insert(slug);
        }
        // A route that matches nothing is almost always a typo in its matcher.
        for (i, route) in self.cfg.routes.iter().enumerate() {
            if !self
                .markets
                .values()
                .any(|m| m.active && listed.contains(&m.params.slug) && route.matches(&m.params))
            {
                t.note(
                    "route_unused",
                    None,
                    format!(
                        "route {i} (strategy {:?}) matches none of the listed markets",
                        route.strategy
                    ),
                );
            }
        }
        for (slug, m) in self.markets.iter_mut() {
            if m.active && !listed.contains(slug) {
                m.active = false;
                m.delisted_at = Some(now);
                t.note("market_removed", Some(slug), "");
                t.actions.push(RiskAction::CancelMarket(slug.clone()));
                t.dirty.insert(slug.clone());
            }
        }
    }

    /// A resolved market with a position still open locally and no settlement price yet.
    fn awaiting_settlement(&self, slug: &str, m: &Market) -> bool {
        m.settlement.is_none()
            && (m.resolving
                || matches!(m.md.state(), MarketState::Expired | MarketState::Terminated))
            && !self.portfolio.position(slug).qty.is_zero()
    }

    /// Ask for the settlement price over REST, at most every `SETTLEMENT_QUERY_S` per market.
    fn ask_settlement(&mut self, slug: &str, now: u64, t: &mut Tick) {
        let Some(m) = self.markets.get_mut(slug) else {
            return;
        };
        if m.settlement.is_some()
            || m.settlement_asked
                .is_some_and(|at| now.saturating_sub(at) < secs(SETTLEMENT_QUERY_S))
        {
            return;
        }
        m.settlement_asked = Some(now);
        t.commands.push(Command::QuerySettlement {
            slug: m.params.slug.clone(),
        });
    }

    /// A market resolved at `px` (YES price): close the local position there, as the venue does,
    /// instead of reconciling it later at a mark. Once per market.
    fn settle(&mut self, slug: &Slug, px: Px, t: &mut Tick) {
        let known = match self.markets.get_mut(slug) {
            Some(m) if m.settlement.is_some() => return,
            Some(m) => {
                m.settlement = Some(px);
                m.resolving = false;
                true
            }
            // Retired or never listed: only a position left over from before matters.
            None => false,
        };
        match self.portfolio.settle(slug, px) {
            Some(qty) => {
                t.note(
                    "settled",
                    Some(slug),
                    format!("YES settled at {px}; closed {qty}"),
                );
                t.actions.push(RiskAction::QueryBalance);
            }
            None if known => t.note(
                "settled",
                Some(slug),
                format!("YES settled at {px}; no position"),
            ),
            None => return,
        }
        if known {
            t.dirty.insert(slug.clone());
        }
    }

    /// Drop markets delisted for `RETIRE_AFTER_S` with no open orders and a flat position, with their
    /// decisions, risk counters and (once no market refers to it) their game. Otherwise every market
    /// ever listed stays in memory and in the loops over all markets. Positions stay: a flat one still
    /// carries realised PnL. A relisted market starts fresh.
    fn retire_markets(&mut self, now: u64, t: &mut Tick) {
        let gone: Vec<Slug> = self
            .markets
            .iter()
            .filter(|(slug, m)| {
                m.delisted_at
                    .is_some_and(|at| now.saturating_sub(at) >= secs(RETIRE_AFTER_S))
                    && self.om.active_in(slug).next().is_none()
                    && self.portfolio.position(slug).qty.is_zero()
            })
            .map(|(s, _)| s.clone())
            .collect();
        for slug in gone {
            let m = self.markets.remove(&slug).expect("listed above");
            self.last_decisions.remove(&slug);
            self.risk.forget_market(&slug, now);
            self.om.forget_unshown(&slug);
            if !self
                .markets
                .values()
                .any(|other| other.params.event_id == m.params.event_id)
            {
                self.games.games.remove(&m.params.event_id);
            }
            t.note("market_retired", Some(&slug), "");
        }
    }

    fn on_om_events(&mut self, events: Vec<OmEvent>, clock: Clock, t: &mut Tick) {
        let now = clock.mono;
        for event in events {
            match event {
                OmEvent::ExecutionAck { token, kind } => t.execution_acks.push((token, kind)),
                OmEvent::Acked {
                    slug, latency_ns, ..
                } => {
                    t.note(
                        "ack",
                        Some(&slug),
                        format!("{:.1} ms", latency_ns as f64 / 1e6),
                    );
                    t.latencies.push(("ack", latency_ns));
                    t.dirty.insert(slug);
                }
                OmEvent::Fill(f) => {
                    let fee = f.fee.unwrap_or_else(|| {
                        let theta = self
                            .markets
                            .get(&f.slug)
                            .map(|m| m.params.fee_coefficient)
                            .unwrap_or(0.0);
                        if f.aggressor {
                            theta * f.qty.as_f64() * f.px.as_f64() * (1.0 - f.px.as_f64())
                        } else {
                            0.0
                        }
                    });
                    self.portfolio.on_fill(&f.slug, f.side, f.qty, f.px, fee);
                    self.portfolio.log_fill(clock.wall_ns, &f, fee);
                    t.note(
                        "fill",
                        Some(&f.slug),
                        format!(
                            "{:?} {} @ {} fee {:.4} aggressor {} trade {}",
                            f.side, f.qty, f.px, fee, f.aggressor, f.trade_id
                        ),
                    );
                    t.actions.extend(self.risk.on_fill(&f.slug, now));
                    t.filled.insert(f.slug.clone());
                    t.urgent.insert(f.slug.clone());
                    t.dirty.insert(f.slug);
                }
                OmEvent::Done { slug, reason, .. } => {
                    if !matches!(reason, crate::orders::DoneReason::Withdrawn) {
                        t.note("order_done", Some(&slug), format!("{reason:?}"));
                    }
                    t.dirty.insert(slug);
                }
                OmEvent::Replaced { slug, .. } => {
                    t.dirty.insert(slug);
                }
                OmEvent::Rejected {
                    slug,
                    class,
                    reason,
                    text,
                    ..
                } => {
                    t.note("reject", Some(&slug), format!("{class:?} {reason} {text}"));
                    t.actions.extend(self.risk.on_reject(&slug, class, now));
                    t.urgent.insert(slug.clone());
                    t.dirty.insert(slug);
                }
                OmEvent::UnsolicitedCancel { slug, reason, .. } => {
                    t.note("unsolicited_cancel", Some(&slug), reason);
                    t.actions.push(RiskAction::QueryBalance);
                }
                OmEvent::InDoubt { slug, .. } => {
                    t.note("order_in_doubt", Some(&slug), "");
                    t.dirty.insert(slug);
                }
                OmEvent::UnknownOrder { slug, order_id } => {
                    t.note("unknown_order", Some(&slug), order_id.to_string());
                    self.risk.pause(
                        &slug,
                        now,
                        Some(self.cfg.risk.market_pause_s),
                        "unknown order on the venue",
                    );
                    t.dirty.insert(slug);
                }
                OmEvent::CancelAllFailed { detail } => t.note("cancel_all_failed", None, detail),
            }
        }
    }

    // ------------------------------------------------------------------ housekeeping

    pub(crate) fn marks(&self) -> BTreeMap<Slug, f64> {
        let mut out = BTreeMap::new();
        for (slug, m) in &self.markets {
            let mark = m.strategy.fair().or_else(|| {
                let v = m.md.venue_view();
                v.microprice()
                    .filter(|_| v.spread().is_some_and(|s| s <= 0.10))
            });
            if let Some(mark) = mark {
                out.insert(slug.clone(), mark);
            }
        }
        out
    }

    /// Marks for the loss limits, deliberately pessimistic: for an open position, the worse
    /// of the strategy's fair and what others would pay to take it off us now (their best bid for a
    /// long, best ask for a short), else the last trade. A wide or one-sided book then shows the loss
    /// instead of leaving the position at cost (stress-test plan B2, B3).
    pub(crate) fn loss_marks(&self) -> BTreeMap<Slug, f64> {
        let mut out = self.marks();
        for (slug, pos) in &self.portfolio.positions {
            let Some(m) = self.markets.get(slug).filter(|_| !pos.qty.is_zero()) else {
                continue;
            };
            let long = pos.qty > Qty::ZERO;
            let others = others_view(&m.md, &self.om, slug);
            let exit = if long {
                others.best_bid()
            } else {
                others.best_ask()
            }
            .map(|l| l.0.as_f64());
            let mark = match (m.strategy.fair(), exit) {
                (Some(fair), Some(exit)) => {
                    Some(if long { fair.min(exit) } else { fair.max(exit) })
                }
                (fair, exit) => fair
                    .or(exit)
                    .or_else(|| m.md.last_trade.map(|(px, _)| px.as_f64())),
            };
            match mark {
                Some(mark) => out.insert(slug.clone(), mark),
                None => out.remove(slug),
            };
        }
        out
    }

    fn book_stale(&self, m: &Market, now: u64, live: bool) -> Option<&'static str> {
        let r = &self.cfg.risk;
        m.md.stale_reason(
            now,
            secs(r.book_stale_after_trade_s),
            live.then(|| secs(r.book_stale_live_s)),
        )
    }

    fn live(&self, m: &Market) -> bool {
        self.games
            .get(&m.params.event_id)
            .is_some_and(|g| g.frame.live && !g.frame.ended)
    }

    fn housekeeping(&mut self, clock: Clock, t: &mut Tick) {
        let now = clock.mono;
        let (events, commands) = self.om.check_timeouts(&self.cfg.execution, now);
        t.commands.extend(commands);
        self.on_om_events(events, clock, t);

        // Game polling going stale blocks live markets, and recovering unblocks them.
        let stale = self.games.stale(now, secs(self.cfg.risk.game_stale_s));
        if stale || stale != self.last_housekeeping.2 {
            for (slug, m) in &self.markets {
                if self
                    .games
                    .get(&m.params.event_id)
                    .is_some_and(|g| g.frame.live && !g.frame.ended)
                {
                    t.dirty.insert(slug.clone());
                }
            }
        }

        // A book that stopped updating pulls that market's quotes; recovering re-decides it.
        let flipped: Vec<(Slug, Option<&'static str>)> = self
            .markets
            .iter()
            .filter(|(_, m)| m.active && m.md.seeded)
            .filter_map(|(slug, m)| {
                let stale = self.book_stale(m, now, self.live(m));
                (stale.is_some() != m.book_stale).then(|| (slug.clone(), stale))
            })
            .collect();
        for (slug, stale) in flipped {
            if let Some(m) = self.markets.get_mut(&slug) {
                m.book_stale = stale.is_some();
            }
            t.note(
                "book_stale",
                Some(&slug),
                stale.unwrap_or("book updating again"),
            );
            t.urgent.insert(slug.clone());
            t.dirty.insert(slug);
        }

        // GTD refresh for quotes that are still wanted.
        let before = (self.cfg.execution.gtd_refresh_before_s * 1e9) as i64;
        // Diagnostic quotes expire naturally unless a new quote decision reprices them.
        // Do not refresh them before the strategy can enforce its stop conditions.
        let expiries: BTreeMap<Slug, Tif> = self
            .markets
            .iter()
            .filter(|(_, m)| m.strategy.quote_lifetime_s().is_none())
            .map(|(s, m)| (s.clone(), self.quote_tif(&m.params, clock)))
            .collect();
        let refresh = self.om.refresh_expiring(
            clock.wall_ns,
            before,
            |rec| {
                let tif = expiries.get(&rec.slug)?;
                match (tif, rec.tif) {
                    (Tif::Gtd(new), Tif::Gtd(old)) if *new > old => Some(*tif),
                    _ => None,
                }
            },
            now,
        );
        t.commands.extend(refresh);

        if self.private_up {
            let reread = self.positions_reread_at.is_some_and(|at| now >= at);
            if reread
                || now.saturating_sub(self.last_positions_query)
                    >= secs(self.cfg.execution.reconcile_positions_s)
            {
                self.last_positions_query = now;
                self.positions_reread_at = None;
                t.commands.push(Command::QueryPositions);
            }
            if now.saturating_sub(self.last_balance_query)
                >= secs(self.cfg.execution.reconcile_balance_s)
            {
                self.last_balance_query = now;
                t.commands.push(Command::QueryBalance);
            }
        }

        let marks = self.loss_marks();
        let total = self.portfolio.total_marked(|s| marks.get(s).copied());
        let day = utc_day(clock.wall_ns);
        let daily = self.portfolio.daily_pnl(total, clock.wall_ns);
        let per_market: Vec<(Slug, f64)> = self
            .portfolio
            .positions
            .iter()
            .filter_map(|(s, p)| marks.get(s).map(|m| (s.clone(), p.marked(*m))))
            .collect();
        t.actions
            .extend(self.risk.check_losses(daily, day, &per_market, now));

        self.om.prune(now, secs(600.0));
        self.retire_markets(now, t);

        // Re-decide only markets whose decision can have changed with no input: a strategy time
        // boundary passed, a risk block expired, or the UTC day rolled (daily kill).
        let (last, last_day, _) = self.last_housekeeping;
        let backstop = self.cfg.execution.redecide_all_s > 0.0
            && now.saturating_sub(self.last_redecide_all)
                >= secs(self.cfg.execution.redecide_all_s);
        if backstop || self.risk.expired_between(last, now) || day != last_day {
            self.last_redecide_all = now;
            t.dirty.extend(
                self.markets
                    .iter()
                    .filter(|(_, m)| m.active)
                    .map(|(s, _)| s.clone()),
            );
        } else {
            for (slug, m) in &mut self.markets {
                if m.active && m.wake_ns.is_some_and(|w| w <= clock.wall_ns) {
                    m.wake_ns = None;
                    t.dirty.insert(slug.clone());
                }
            }
        }
        self.last_housekeeping = (now, day, stale);
    }

    // ------------------------------------------------------------------ 3. react

    fn react(&mut self, clock: Clock, t: &mut Tick) {
        let now = clock.mono;
        let actions = std::mem::take(&mut t.actions);
        let mut cancelled: BTreeSet<Slug> = BTreeSet::new();
        let mut cancel_all = false;
        for action in actions {
            match action {
                RiskAction::CancelAll if !cancel_all => {
                    cancel_all = true;
                    let token = self.om.token();
                    t.commands
                        .extend(self.om.cancel_everything(Priority::CancelAll, now));
                    t.commands.push(Command::CancelAll {
                        token,
                        slugs: Vec::new(),
                    });
                }
                RiskAction::CancelAll => {}
                RiskAction::CancelMarket(slug) => {
                    if !cancel_all && cancelled.insert(slug.clone()) {
                        t.commands
                            .extend(self.om.cancel_slug(&slug, Priority::Cancel, now));
                    }
                }
                RiskAction::QueryBalance => {
                    if !t.commands.contains(&Command::QueryBalance) {
                        t.commands.push(Command::QueryBalance);
                    }
                }
                RiskAction::RefreshMarket(slug) => t.commands.push(Command::RefreshMarket { slug }),
                RiskAction::Alert(text) => t.note("alert", None, text),
            }
        }
    }

    // ------------------------------------------------------------------ 4-6. decide, gate, act

    fn quote_tif(&self, params: &MarketParams, clock: Clock) -> Tif {
        let s = &self.cfg.execution;
        let inplay = clock.wall_ns + (s.inplay_gtd_s * 1e9) as i64;
        let expiry = match params.game_start_ns {
            Some(start) => {
                let pre = start + (s.pre_match_gtd_after_start_s * 1e9) as i64;
                if pre > clock.wall_ns + (s.inplay_gtd_s * 1e9) as i64 {
                    pre
                } else {
                    inplay
                }
            }
            None => clock.wall_ns + 3_600 * NS_PER_SEC,
        };
        let expiry = self
            .markets
            .get(&params.slug)
            .and_then(|m| m.strategy.quote_lifetime_s())
            .map_or(expiry, |s| {
                expiry.min(clock.wall_ns + s as i64 * NS_PER_SEC)
            });
        // Whole seconds, rounded up, as the venue expects.
        Tif::Gtd((expiry + NS_PER_SEC - 1).div_euclid(NS_PER_SEC) * NS_PER_SEC)
    }

    /// Why a market can't be quoted right now, if it can't.
    fn not_quotable(&mut self, slug: &Slug, clock: Clock, live: bool) -> Option<String> {
        let m = self.markets.get(slug)?;
        if !m.active {
            return Some("not listed".into());
        }
        if !self.markets_up || !m.md.seeded {
            return Some("book not seeded".into());
        }
        if !self.private_up {
            return Some("private stream not ready".into());
        }
        if m.md.state() != MarketState::Open {
            return Some("market not open".into());
        }
        if let Some(reason) = self.book_stale(m, clock.mono, live) {
            return Some(reason.into());
        }
        if live
            && self
                .games
                .stale(clock.mono, secs(self.cfg.risk.game_stale_s))
        {
            return Some("game state stale".into());
        }
        if self.om.has_in_doubt(slug) {
            return Some("order in doubt".into());
        }
        let league = m.params.league.clone();
        let preset = self.cfg.league(&league).orientation_verified;
        if live && self.risk.league(&league, preset) == LeagueCheck::Failed {
            return Some("score orientation failed".into());
        }
        self.risk
            .paused_reason(slug, clock.mono, utc_day(clock.wall_ns))
    }

    /// Worst-case loss of a market's position and active orders. A modify in flight (or in doubt) can
    /// land either way, so it counts as the worse of the order as confirmed and as requested. Bid and
    /// ask fill scenarios are separate and each quote side has one order, so comparing "all as
    /// confirmed" with "all as requested" covers every mix.
    pub(crate) fn market_worst(&self, slug: &str) -> f64 {
        let (mut confirmed, mut requested) = (Resting::default(), Resting::default());
        for rec in self.om.active_in(slug) {
            confirmed.add(rec.side, rec.leaves_qty, rec.px);
            match rec.modify_to {
                Some((px, qty, _)) => requested.add(rec.side, qty, px),
                None => requested.add(rec.side, rec.leaves_qty, rec.px),
            }
        }
        let pos = self.portfolio.position(slug);
        worst_case_loss(&pos, &confirmed).max(worst_case_loss(&pos, &requested))
    }

    /// (worst case with orders, worst case of the position alone) for one market.
    fn market_exposure(&self, slug: &str) -> (f64, f64) {
        (
            self.market_worst(slug),
            worst_case_loss(&self.portfolio.position(slug), &Resting::default()),
        )
    }

    fn decide_and_act(&mut self, clock: Clock, t: &mut Tick) {
        let now = clock.mono;
        // Account exposure per market (positions in every market, plus active orders).
        let mut worst: BTreeMap<Slug, (f64, f64)> = BTreeMap::new();
        let mut slugs: BTreeSet<Slug> = self.portfolio.positions.keys().cloned().collect();
        slugs.extend(self.om.active().map(|r| r.slug.clone()));
        for slug in slugs {
            let w = self.market_exposure(&slug);
            worst.insert(slug, w);
        }

        // Priority: urgent markets, then in-play, then by slug.
        let mut order: Vec<Slug> = t.dirty.iter().cloned().collect();
        let live_of = |core: &Core, slug: &Slug| {
            core.markets
                .get(slug)
                .and_then(|m| core.games.get(&m.params.event_id))
                .is_some_and(|g| g.frame.live)
        };
        order.sort_by_key(|s| (!t.urgent.contains(s), !live_of(self, s), s.clone()));

        for slug in order {
            let Some(m) = self.markets.get(&slug) else {
                continue;
            };
            let game = self.games.get(&m.params.event_id).cloned();
            let live = game
                .as_ref()
                .is_some_and(|g| g.frame.live && !g.frame.ended);
            let blocked = self.not_quotable(&slug, clock, live);
            let m = self.markets.get_mut(&slug).expect("checked above");
            let league_cfg = self.cfg.league(&m.params.league);
            let league_check = self
                .risk
                .league(&m.params.league, league_cfg.orientation_verified);

            let others = others_view(&m.md, &self.om, &slug);
            let working = |q: QuoteSide| {
                self.om
                    .slot(&slug, q)
                    .filter(|r| r.state == LocalState::Live)
                    .map(|r| (r.px, r.leaves_qty))
            };
            let position = self.portfolio.position(&slug);
            let view = MarketView {
                params: &m.params,
                league: &league_cfg,
                league_check,
                others: &others,
                last_trade: m.md.last_trade,
                game: game.as_ref(),
                position: position.qty,
                working_bid: working(QuoteSide::Bid),
                working_ask: working(QuoteSide::Ask),
                quotable: blocked.is_none(),
                input_updated: t.input_updated.contains(&slug),
                filled: t.filled.contains(&slug),
                slow_exchange: now < self.risk.slow_until,
                now,
                wall_ns: clock.wall_ns,
            };
            let input_driven = m.strategy.input_driven();
            let decision = m.strategy.decide(&view);
            m.wake_ns = m.strategy.wake_at(&m.params).filter(|w| *w > clock.wall_ns);
            if let Some(reason) = m.strategy.diagnostic_reason() {
                t.note("gate", Some(&slug), reason);
            }
            let params = m.params.clone();
            let venue_best = (
                m.md.venue_view().best_bid().map(|l| l.0),
                m.md.venue_view().best_ask().map(|l| l.0),
            );

            if let Some(agrees) = decision.orientation {
                let actions = self.risk.on_orientation(&params.league, agrees);
                for a in actions {
                    if let RiskAction::Alert(text) = a {
                        t.note("orientation", Some(&slug), text);
                    }
                }
            }

            // Gate with the account reservation.
            let (mut bid, mut ask) = (None, None);
            if blocked.is_none() {
                if let Some(fair) = decision.fair {
                    let mut other_resting = Resting::default();
                    let mut has_working = (false, false);
                    for rec in self.om.active_in(&slug) {
                        match rec.quote_side {
                            Some(QuoteSide::Bid) => has_working.0 = true,
                            Some(QuoteSide::Ask) => has_working.1 = true,
                            None => other_resting.add(rec.side, rec.leaves_qty, rec.px),
                        }
                    }
                    let (mut account_other, mut committed_other) = (0.0, 0.0);
                    for (_, (w, p)) in worst.iter().filter(|(s, _)| **s != slug) {
                        account_other += w;
                        committed_other += w - p;
                    }
                    let orders_elsewhere = self
                        .om
                        .active()
                        .filter(|r| r.slug != slug || r.quote_side.is_none())
                        .count();
                    let ctx = GateCtx {
                        slug: &slug,
                        tick: params.tick,
                        min_qty: params.min_qty,
                        fair,
                        best_bid: venue_best.0,
                        best_ask: venue_best.1,
                        position: &position,
                        other_resting,
                        account_other,
                        committed_other,
                        buying_power: self.portfolio.balance.as_ref().map(|b| b.buying_power),
                        orders_elsewhere,
                        has_working,
                    };
                    let gated = self.risk.gate(&ctx, decision.bid, decision.ask);
                    for v in &gated.vetoes {
                        t.note("gate", Some(&slug), *v);
                    }
                    let (current, position_worst) = self.market_exposure(&slug);
                    worst.insert(
                        slug.clone(),
                        (gated.worst_case.max(current), position_worst),
                    );
                    bid = gated.bid;
                    ask = gated.ask;
                }
            }

            let tif = self.quote_tif(&params, clock);
            // The latency probe's reprices are its measurements: one-tick moves included.
            let min_ticks = if input_driven {
                1
            } else {
                self.cfg.execution.reprice_min_ticks
            };
            let min_move = Px(params.tick.0 * i64::from(min_ticks));
            let pos = position.qty;
            let bid_target = bid.map(|(px, qty)| QuoteTarget {
                px,
                qty,
                tif,
                intent: if pos <= -qty {
                    Intent::SellShort
                } else {
                    Intent::BuyLong
                },
            });
            let ask_target = ask.map(|(px, qty)| QuoteTarget {
                px,
                qty,
                tif,
                intent: if pos >= qty {
                    Intent::SellLong
                } else {
                    Intent::BuyShort
                },
            });
            let prio = |reduces: bool| {
                if reduces {
                    Priority::Reduce
                } else {
                    Priority::Quote
                }
            };
            // Probe placements/reprices need new input; safety cancellations never do.
            let may_quote = !input_driven || t.input_updated.contains(&slug);
            if may_quote || bid_target.is_none() {
                t.commands.extend(self.om.sync_side(
                    &slug,
                    QuoteSide::Bid,
                    bid_target,
                    min_move,
                    prio(pos < Qty::ZERO),
                    now,
                ));
            }
            if may_quote || ask_target.is_none() {
                t.commands.extend(self.om.sync_side(
                    &slug,
                    QuoteSide::Ask,
                    ask_target,
                    min_move,
                    prio(pos > Qty::ZERO),
                    now,
                ));
            }

            // A leg change must cancel or reprice this tick: in-flight quotes get cancelled.
            if t.must_act.contains(&slug) {
                let in_flight: Vec<_> = self
                    .om
                    .active_in(&slug)
                    .filter(|r| {
                        r.quote_side.is_some()
                            && matches!(
                                r.state,
                                LocalState::PendingNew | LocalState::PendingReplace
                            )
                    })
                    .map(|r| r.token)
                    .collect();
                if !in_flight.is_empty() {
                    t.commands
                        .extend(self.om.cancel_slug(&slug, Priority::Cancel, now));
                }
            }

            if let Some(reason) = &blocked {
                if decision.phase != Some(Phase::Closed)
                    && self
                        .last_decisions
                        .get(&slug)
                        .is_none_or(|d| d.bid.is_some() || d.ask.is_some())
                {
                    t.note("not_quoting", Some(&slug), reason.clone());
                }
            }
            let mut recorded = decision;
            recorded.bid = bid;
            recorded.ask = ask;
            self.last_decisions.insert(slug, recorded);
        }
    }

    // ------------------------------------------------------------------ restart

    /// What has to survive a restart; see `RiskState`.
    pub fn risk_state(&self, wall_ns: i64) -> RiskState {
        let day = utc_day(wall_ns);
        let (market_loss, paused) = self.risk.persisted();
        RiskState {
            day,
            daily_pnl: if self.portfolio.day == Some(day) {
                self.portfolio.daily
            } else {
                0.0
            },
            killed: self.risk.killed_day == Some(day),
            market_loss,
            paused,
        }
    }

    /// Continue from a saved `RiskState`, before the first tick: the day's PnL and kill if it is still
    /// that UTC day, market losses if it was saved today or yesterday (match markets are listed a day
    /// ahead at most, so older ones are done with), and every indefinite pause.
    pub fn restore_risk_state(&mut self, state: &RiskState, wall_ns: i64) {
        let day = utc_day(wall_ns);
        if state.day == day {
            self.portfolio.carry_day(day, state.daily_pnl);
            if state.killed {
                self.risk.killed_day = Some(day);
            }
        }
        let losses = if day - state.day <= 1 {
            state.market_loss.clone()
        } else {
            BTreeMap::new()
        };
        self.risk.restore(&losses, &state.paused);
    }

    // ------------------------------------------------------------------ status

    pub fn status(&self) -> String {
        let active = self.markets.values().filter(|m| m.active).count();
        let seeded = self
            .markets
            .values()
            .filter(|m| m.active && m.md.seeded)
            .count();
        let quoting = self
            .last_decisions
            .values()
            .filter(|d| d.bid.is_some() || d.ask.is_some())
            .count();
        let orders = self.om.active().count();
        let positions: Vec<String> = self
            .portfolio
            .positions
            .iter()
            .filter(|(_, p)| !p.qty.is_zero())
            .map(|(s, p)| format!("{s}={}", p.qty))
            .collect();
        let mut by_strategy: BTreeMap<&str, usize> = BTreeMap::new();
        for m in self.markets.values().filter(|m| m.active) {
            *by_strategy.entry(&m.strategy_label).or_default() += 1;
        }
        let strategies: Vec<String> = by_strategy
            .iter()
            .map(|(l, n)| format!("{l}: {n}"))
            .collect();
        format!(
            "markets {active} (seeded {seeded}, quoting {quoting}) | strategies [{}] | feeds markets={} private={} | orders {orders} | fills {} | positions [{}]{}",
            strategies.join(", "), self.markets_up, self.private_up, self.portfolio.fills, positions.join(" "),
            if self.risk.admin_paused { " | PAUSED" } else { "" },
        )
    }

    pub fn market_count(&self) -> usize {
        self.markets.len()
    }

    pub fn price_of(&self, slug: &str) -> Option<Px> {
        self.markets
            .get(slug)
            .and_then(|m| m.strategy.fair())
            .map(Px::from_f64)
    }
}
