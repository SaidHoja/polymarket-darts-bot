//! Exchange simulator for one market.
//!
//! The simulator holds the true L3 book at exchange time plus our orders in it.
//! The core sees what the live bot would: L2 books and trades delayed by sampled
//! feed lag, REST results and executions delayed by sampled order latency, and
//! synthetic game-state polls. One timeline (exchange ns) drives everything;
//! the core's monotonic clock is derived from it, so a run is deterministic for
//! a given seed.
//!
//! Fills (see `FillModel`): a trade at our price fills us once the queue ahead
//! of us is used up; a trade through our price fills us outright; a historical
//! order that would have crossed our resting quote fills us. Our fills never
//! change the historical book (no-impact assumption).

use std::cmp::Reverse;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use pm_core::{Clock, Config, Core};
use pm_types::{
    BalanceFrame, BookFrame, Command, ConnState, Event, ExecKind, ExecutionFrame, Feed, FillInfo,
    GameFrame, MarketParams, MarketState, NewOrder, OrderId, OrderState, PositionFrame, Priority,
    Px, Qty, Reject, RejectClass, RestKind, RestOutcome, RestResult, Side, Slug, Tif, Token,
    TradeFrame, VenueOrder,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;

use crate::book::{Effect, L3Book};
use crate::data::{Kind, MarketMeta, Message, Row};

const MS: f64 = 1e6;
const SEC: i64 = 1_000_000_000;
/// A book wider than this has no meaningful mid (same rule the core uses for marks).
const MID_MAX_SPREAD: f64 = 0.10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FillModel {
    /// Fill when a trade prints at our price after the queue ahead is used up.
    Optimistic,
    /// As optimistic, but orders joining our level within `tie_window_ns` of us count as ahead.
    Base,
    /// Fill only when price trades through our level (or an order crosses it).
    Pessimistic,
}

impl FillModel {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "optimistic" => Ok(FillModel::Optimistic),
            "base" => Ok(FillModel::Base),
            "pessimistic" => Ok(FillModel::Pessimistic),
            other => Err(format!(
                "fill model {other:?}: optimistic | base | pessimistic"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            FillModel::Optimistic => "optimistic",
            FillModel::Base => "base",
            FillModel::Pessimistic => "pessimistic",
        }
    }
}

/// Simulation assumptions. Latency defaults come from live captures
/// (`docs/polymarket-docs/polymarket-connectivity.md` §9); spreads (sigma) are assumptions.
#[derive(Debug, Clone, Serialize)]
pub struct SimParams {
    pub fill_model: FillModel,
    /// Scales every latency below (sensitivity runs: 0.5, 1, 2, 4).
    pub lag_mult: f64,
    /// Exchange → our receive, books: lognormal with this median.
    pub book_lag_ms: f64,
    pub trade_lag_ms: f64,
    pub md_lag_sigma: f64,
    /// One-way latency of a request to the exchange, and of the response / execution back.
    pub order_oneway_ms: f64,
    pub order_lag_sigma: f64,
    /// Gateway token bucket (requests per second, also the burst size); unused when the config sets `[execution].rate_limit_enabled = false`.
    pub rate_per_s: f64,
    /// Base model: orders joining our level this soon after us are treated as ahead.
    pub tie_window_ns: i64,
    /// Synthesize game state: live from the scheduled start, polled every second.
    pub game_state: bool,
    pub balance: f64,
    pub book_depth: usize,
    pub seed: u64,
}

impl Default for SimParams {
    fn default() -> Self {
        SimParams {
            fill_model: FillModel::Base,
            lag_mult: 1.0,
            book_lag_ms: 63.0,
            trade_lag_ms: 54.0,
            md_lag_sigma: 0.5,
            order_oneway_ms: 20.0,
            order_lag_sigma: 0.3,
            rate_per_s: 15.0,
            tie_window_ns: 1_000_000,
            game_state: true,
            balance: 1_000.0,
            book_depth: 20,
            seed: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FillRow {
    pub slug: String,
    pub t_ns: i64,
    pub side: &'static str,
    pub px: f64,
    pub qty: f64,
    pub fee: f64,
    /// `queue` (our turn at our price), `through` (traded through us), `cross` (an order crossed us).
    pub how: &'static str,
    pub queue_ahead: f64,
    pub mid: Option<f64>,
    /// Per-contract markouts vs mid (positive = good for us); `None` without a mid then.
    pub mo_1s: Option<f64>,
    pub mo_10s: Option<f64>,
    pub mo_60s: Option<f64>,
    pub mo_settle: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Pnl {
    pub total: f64,
    /// Σ (mid at fill − price) per side: the edge captured when filled.
    pub spread: f64,
    /// Σ (settlement − mid at fill) per side: what holding the inventory did.
    pub inventory: f64,
    pub fees: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SimStats {
    pub replay_hours: f64,
    pub ticks: u64,
    /// Place / modify / cancel / cancel-all requests.
    pub order_requests: u64,
    /// Reconciliation reads (positions, balance, open orders, order lookups).
    pub read_requests: u64,
    pub req_per_s_p50: f64,
    pub req_per_s_p99: f64,
    pub req_per_s_max: u32,
    /// Seconds in which the core wanted more requests than the token bucket allowed.
    pub rate_limited_s: u64,
    pub rejects: BTreeMap<String, u64>,
    pub uptime_bid: f64,
    pub uptime_ask: f64,
    pub max_abs_inventory: f64,
    pub twa_abs_inventory: f64,
    pub inventory_at_start: Option<f64>,
    /// Largest peak-to-trough fall of cash + position × mid.
    pub max_drawdown: f64,
    pub books_sent: u64,
    pub command_hash: String,
}

#[derive(Debug, Clone)]
pub struct SimOutput {
    pub fills: Vec<FillRow>,
    pub pnl: Option<Pnl>,
    pub stats: SimStats,
}

fn fnv1a(text: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

struct OurOrder {
    venue: VenueOrder,
    tif: Tif,
    /// Historical orders ahead of us at our price, front first, with what's left of them.
    ahead: Vec<(u32, Qty)>,
    joined_ns: i64,
}

enum Action {
    Place {
        id: OrderId,
        order: NewOrder,
    },
    Modify {
        order_id: OrderId,
        px: Px,
        qty: Qty,
        tif: Tif,
    },
    Cancel {
        order_id: OrderId,
    },
    CancelAll,
    Query(Command),
}

enum Item {
    /// Hand to the core at this (local) time.
    Deliver(Event),
    /// Our request reaches the exchange.
    Arrive(Action),
    /// Game-state poll completes.
    Poll,
    Expire(OrderId, i64),
}

pub struct Sim<'a> {
    p: SimParams,
    meta: &'a MarketMeta,
    slug: Slug,
    params: MarketParams,
    rows: &'a [Row],
    msgs: &'a [Message],
    exch_ns: Vec<i64>,
    book: L3Book,
    core: Core,
    rng: StdRng,
    t0: i64,
    now: i64,
    end: i64,
    heap: BinaryHeap<Reverse<(i64, u64)>>,
    items: HashMap<u64, Item>,
    seq: u64,
    pending: Vec<Event>,
    md_last: i64,
    priv_last: i64,
    gw_queue: BTreeMap<(Priority, u64), Command>,
    /// `[execution].rate_limit_enabled`: off sends every request at once, as the live gateway does.
    rate_limited: bool,
    tokens: f64,
    tokens_at: i64,
    orders: BTreeMap<OrderId, OurOrder>,
    done: HashMap<OrderId, VenueOrder>,
    crossed_us: HashSet<u32>,
    next_id: u64,
    next_exec: u64,
    position: Qty,
    cash: f64,
    fees: f64,
    cum_traded: Qty,
    last_l2: crate::book::L2,
    mids: Vec<(i64, Option<f64>)>,
    fills: Vec<FillRow>,
    req_per_s: BTreeMap<i64, u32>,
    limited_s: HashSet<i64>,
    stats: SimStats,
    hasher: DefaultHasher,
    last_t: i64,
    bid_up_ns: i64,
    ask_up_ns: i64,
    inv_ns: f64,
    peak: f64,
}

impl<'a> Sim<'a> {
    pub fn new(
        meta: &'a MarketMeta,
        rows: &'a [Row],
        msgs: &'a [Message],
        cfg: Config,
        p: SimParams,
    ) -> Self {
        let params = meta.params(meta.fee_coefficient.unwrap_or(0.0695));
        let mut exch_ns = Vec::with_capacity(msgs.len());
        let mut last = i64::MIN;
        for m in msgs {
            last = last.max(m.ev_ns);
            exch_ns.push(last);
        }
        let t0 = exch_ns.first().copied().unwrap_or(0) - SEC;
        let end = exch_ns.last().copied().unwrap_or(0);
        let rng = StdRng::seed_from_u64(p.seed ^ fnv1a(&meta.slug));
        let rate_limited = cfg.execution.rate_limit_enabled;
        Sim {
            slug: params.slug.clone(),
            params,
            meta,
            rows,
            msgs,
            exch_ns,
            book: L3Book::default(),
            core: Core::new(cfg),
            rng,
            t0,
            now: t0,
            end,
            heap: BinaryHeap::new(),
            items: HashMap::new(),
            seq: 0,
            pending: Vec::new(),
            md_last: t0,
            priv_last: t0,
            gw_queue: BTreeMap::new(),
            rate_limited,
            tokens: p.rate_per_s,
            tokens_at: t0,
            orders: BTreeMap::new(),
            done: HashMap::new(),
            crossed_us: HashSet::new(),
            next_id: 0,
            next_exec: 0,
            position: Qty::ZERO,
            cash: 0.0,
            fees: 0.0,
            cum_traded: Qty::ZERO,
            last_l2: (Vec::new(), Vec::new()),
            mids: Vec::new(),
            fills: Vec::new(),
            req_per_s: BTreeMap::new(),
            limited_s: HashSet::new(),
            stats: SimStats::default(),
            hasher: DefaultHasher::new(),
            last_t: t0,
            bid_up_ns: 0,
            ask_up_ns: 0,
            inv_ns: 0.0,
            peak: 0.0,
            p,
        }
    }

    // ------------------------------------------------------------ scheduling

    fn schedule(&mut self, at: i64, item: Item) {
        let at = at.max(self.now); // e.g. a GTD expiry already in the past
        self.seq += 1;
        self.heap.push(Reverse((at, self.seq)));
        self.items.insert(self.seq, item);
    }

    fn lag(&mut self, median_ms: f64, sigma: f64) -> i64 {
        // Lognormal via Box–Muller.
        let (u1, u2): (f64, f64) = (self.rng.gen_range(f64::EPSILON..1.0), self.rng.gen());
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        (median_ms * MS * (sigma * z).exp() * self.p.lag_mult) as i64
    }

    fn order_lag(&mut self) -> i64 {
        self.lag(self.p.order_oneway_ms, self.p.order_lag_sigma)
    }

    /// Market-data socket: one ordered stream.
    fn deliver_md(&mut self, event: Event, median_ms: f64) {
        let lag = self.lag(median_ms, self.p.md_lag_sigma);
        let at = (self.now + lag).max(self.md_last);
        self.md_last = at;
        self.schedule(at, Item::Deliver(event));
    }

    /// Private socket: one ordered stream.
    fn deliver_private(&mut self, event: Event) {
        let lag = self.order_lag();
        let at = (self.now + lag).max(self.priv_last);
        self.priv_last = at;
        self.schedule(at, Item::Deliver(event));
    }

    fn clock(&self) -> Clock {
        Clock {
            mono: (self.now - self.t0 + 1) as u64,
            wall_ns: self.now,
        }
    }

    fn core_deadline(&self) -> i64 {
        self.t0 + self.core.next_deadline() as i64 - 1
    }

    // ------------------------------------------------------------ main loop

    pub fn run(mut self) -> SimOutput {
        if self.msgs.is_empty() {
            return self.finish();
        }
        let mut startup = vec![
            Event::Markets(vec![self.params.clone()]),
            Event::Connection {
                feed: Feed::Markets,
                state: ConnState::Up,
            },
            Event::OrderSnapshot(Vec::new()),
            Event::Positions(Vec::new()),
            Event::Balance(self.balance()),
            Event::Connection {
                feed: Feed::Private,
                state: ConnState::Up,
            },
        ];
        if self.p.game_state {
            if let Some(start) = self.params.game_start_ns {
                startup.push(Event::Game(self.game_frame(start <= self.t0)));
                startup.push(Event::GamePolled);
                let first_poll = start.max(self.t0) + SEC;
                if start > self.t0 {
                    let frame = self.game_frame(true);
                    self.schedule(start + SEC, Item::Deliver(Event::Game(frame)));
                }
                self.schedule(first_poll, Item::Poll);
            }
        }
        self.pending = startup;
        self.tick();

        let mut mi = 0;
        loop {
            let t_msg = self.exch_ns.get(mi).copied().unwrap_or(i64::MAX);
            let t_heap = self.heap.peek().map_or(i64::MAX, |r| r.0 .0);
            let t_core = self.core_deadline();
            let t_gw = self.gateway_ready_at();
            let t = t_msg.min(t_heap).min(t_core).min(t_gw);
            if t == i64::MAX || t > self.end {
                break;
            }
            let t = t.max(self.now); // an overdue core deadline runs now
            self.advance(t);
            if t_msg == t {
                self.on_message(mi);
                mi += 1;
            } else if t_heap == t {
                while self.heap.peek().is_some_and(|r| r.0 .0 == t) {
                    let Reverse((_, seq)) = self.heap.pop().unwrap();
                    if let Some(item) = self.items.remove(&seq) {
                        self.on_item(item);
                    }
                }
            }
            if !self.pending.is_empty() || t >= t_core {
                self.tick();
            }
            self.dispatch();
        }
        self.advance(self.end);
        self.finish()
    }

    fn tick(&mut self) {
        let events = std::mem::take(&mut self.pending);
        let clock = self.clock();
        let out = self.core.tick(clock, events);
        self.stats.ticks += 1;
        for cmd in out.commands {
            format!("{}:{cmd:?}", clock.mono).hash(&mut self.hasher);
            self.enqueue(cmd);
        }
    }

    fn advance(&mut self, t: i64) {
        let dt = t - self.last_t;
        if dt > 0 {
            let live = |s: Side| self.orders.values().any(|o| o.venue.side == s);
            if live(Side::Buy) {
                self.bid_up_ns += dt;
            }
            if live(Side::Sell) {
                self.ask_up_ns += dt;
            }
            self.inv_ns += self.position.abs().as_f64() * dt as f64;
            if let Some(start) = self.params.game_start_ns {
                if self.last_t < start && start <= t && self.stats.inventory_at_start.is_none() {
                    self.stats.inventory_at_start = Some(self.position.as_f64());
                }
            }
        }
        self.last_t = t;
        self.now = t;
    }

    fn on_item(&mut self, item: Item) {
        match item {
            Item::Deliver(e) => self.pending.push(e),
            Item::Arrive(a) => self.on_arrive(a),
            Item::Poll => {
                self.pending.push(Event::GamePolled);
                if self.now + SEC <= self.end {
                    self.schedule(self.now + SEC, Item::Poll);
                }
            }
            Item::Expire(id, at) => {
                if self.orders.get(&id).is_some_and(|o| o.tif == Tif::Gtd(at)) {
                    let mut o = self.orders.remove(&id).unwrap();
                    o.venue.state = OrderState::Expired;
                    self.exec(&o.venue, ExecKind::Expired, None, None);
                    self.done.insert(id, o.venue);
                    self.book_changed();
                }
            }
        }
    }

    fn game_frame(&self, live: bool) -> GameFrame {
        GameFrame {
            event_id: self.params.event_id.clone(),
            event_slug: self.params.event_slug.clone(),
            period: if live { "LIVE".into() } else { String::new() },
            live,
            ended: false,
            score: None,
            updated_ns: None,
        }
    }

    // ------------------------------------------------------------ gateway

    fn refill(&mut self) {
        let dt = (self.now - self.tokens_at) as f64 / 1e9;
        self.tokens = (self.tokens + dt * self.p.rate_per_s).min(self.p.rate_per_s);
        self.tokens_at = self.now;
    }

    fn gateway_ready_at(&mut self) -> i64 {
        if self.gw_queue.is_empty() {
            return i64::MAX;
        }
        self.refill();
        if !self.rate_limited || self.tokens >= 1.0 {
            self.now
        } else {
            self.now + ((1.0 - self.tokens) / self.p.rate_per_s * 1e9).ceil() as i64
        }
    }

    fn enqueue(&mut self, cmd: Command) {
        match cmd {
            Command::Withdraw { token } => {
                let key = self
                    .gw_queue
                    .iter()
                    .find(|(_, c)| token_of(c) == Some(token))
                    .map(|(k, _)| *k);
                if let Some(key) = key {
                    let cmd = self.gw_queue.remove(&key).unwrap();
                    let kind = rest_kind(&cmd).unwrap();
                    self.pending.push(Event::RestResult(RestResult {
                        kind,
                        tokens: vec![token],
                        outcome: RestOutcome::Withdrawn,
                    }));
                }
            }
            Command::RefreshMarket { .. } => {}
            cmd => {
                self.seq += 1;
                self.gw_queue.insert((cmd.priority(), self.seq), cmd);
            }
        }
    }

    fn dispatch(&mut self) {
        if self.gw_queue.is_empty() {
            return;
        }
        self.refill();
        while !self.rate_limited || self.tokens >= 1.0 {
            let Some((_, cmd)) = self.gw_queue.pop_first() else {
                break;
            };
            if self.rate_limited {
                self.tokens -= 1.0;
            }
            if rest_kind(&cmd).is_some() {
                self.stats.order_requests += 1;
            } else {
                self.stats.read_requests += 1;
            }
            *self.req_per_s.entry(self.now.div_euclid(SEC)).or_default() += 1;
            self.send(cmd);
        }
        if !self.gw_queue.is_empty() {
            self.limited_s.insert(self.now.div_euclid(SEC));
        }
    }

    fn send(&mut self, cmd: Command) {
        let (out, back) = (self.order_lag(), self.order_lag());
        let accepted = |kind, token, ids| {
            Event::RestResult(RestResult {
                kind,
                tokens: vec![token],
                outcome: RestOutcome::Accepted { ids },
            })
        };
        let (arrive, response) = match cmd {
            Command::Place { token, order, .. } => {
                self.next_id += 1;
                let id: OrderId = Arc::from(format!("SIM{}", self.next_id));
                (
                    Action::Place {
                        id: id.clone(),
                        order,
                    },
                    Some(accepted(RestKind::Create, token, vec![Some(id)])),
                )
            }
            Command::Modify {
                token,
                order_id,
                px,
                qty,
                tif,
                ..
            } => (
                Action::Modify {
                    order_id,
                    px,
                    qty,
                    tif,
                },
                Some(accepted(RestKind::Modify, token, Vec::new())),
            ),
            Command::Cancel {
                token, order_id, ..
            } => (
                Action::Cancel { order_id },
                Some(accepted(RestKind::Cancel, token, Vec::new())),
            ),
            Command::CancelAll { token, .. } => (
                Action::CancelAll,
                Some(accepted(RestKind::CancelAll, token, Vec::new())),
            ),
            query => (Action::Query(query), None),
        };
        self.schedule(self.now + out, Item::Arrive(arrive));
        if let Some(r) = response {
            self.schedule(self.now + out + back, Item::Deliver(r));
        }
    }

    // ------------------------------------------------------------ exchange: our actions

    fn crosses(&self, side: Side, px: Px) -> bool {
        match side {
            Side::Buy => self.book.best(Side::Sell).is_some_and(|(a, _)| px >= a),
            _ => self.book.best(Side::Buy).is_some_and(|(b, _)| px <= b),
        }
    }

    fn exec(
        &mut self,
        order: &VenueOrder,
        kind: ExecKind,
        fill: Option<FillInfo>,
        reject: Option<Reject>,
    ) {
        self.next_exec += 1;
        let frame = ExecutionFrame {
            exec_id: format!("X{}", self.next_exec),
            order: order.clone(),
            kind,
            fill,
            reject,
            unsolicited: None,
            transact_ns: Some(self.now),
        };
        self.deliver_private(Event::Execution(frame));
    }

    fn reject(&mut self, order: &VenueOrder, class: RejectClass, text: &str) {
        *self
            .stats
            .rejects
            .entry(format!("{class:?}: {text}"))
            .or_default() += 1;
        let reject = Reject {
            class,
            reason: String::new(),
            text: text.to_string(),
        };
        self.exec(order, ExecKind::Rejected, None, Some(reject));
    }

    fn reject_missing(&mut self, id: &OrderId) {
        if let Some(order) = self.done.get(id).cloned() {
            self.reject(&order, RejectClass::Unknown, "order not open");
        }
    }

    fn on_arrive(&mut self, action: Action) {
        match action {
            Action::Place { id, order } => {
                let side = order.intent.side();
                let venue = VenueOrder {
                    id: id.clone(),
                    slug: order.slug.clone(),
                    side,
                    intent: order.intent,
                    px: order.px,
                    qty: order.qty,
                    cum_qty: Qty::ZERO,
                    leaves_qty: order.qty,
                    state: OrderState::New,
                };
                if order.post_only && self.crosses(side, order.px) {
                    let mut v = venue;
                    v.state = OrderState::Rejected;
                    self.reject(&v, RejectClass::BookMoved, "post-only order would cross");
                    self.done.insert(id, v);
                } else {
                    let ahead = self.book.level(side, order.px);
                    self.exec(&venue, ExecKind::New, None, None);
                    let Tif::Gtd(at) = order.tif;
                    self.schedule(at, Item::Expire(id.clone(), at));
                    self.orders.insert(
                        id,
                        OurOrder {
                            venue,
                            tif: order.tif,
                            ahead,
                            joined_ns: self.now,
                        },
                    );
                    self.book_changed();
                }
            }
            Action::Modify {
                order_id,
                px,
                qty,
                tif,
            } => {
                let Some(o) = self.orders.get(&order_id) else {
                    return self.reject_missing(&order_id);
                };
                let side = o.venue.side;
                if self.crosses(side, px) {
                    let v = o.venue.clone();
                    return self.reject(
                        &v,
                        RejectClass::BookMoved,
                        "post-only replace would cross",
                    );
                }
                let requeue = px != o.venue.px || qty > o.venue.qty;
                let ahead = requeue.then(|| self.book.level(side, px));
                let now = self.now;
                let o = self.orders.get_mut(&order_id).unwrap();
                o.venue.px = px;
                o.venue.qty = qty;
                o.venue.leaves_qty = Qty((qty.0 - o.venue.cum_qty.0).max(0));
                o.tif = tif;
                if let Some(ahead) = ahead {
                    o.ahead = ahead;
                    o.joined_ns = now;
                }
                let v = o.venue.clone();
                if v.leaves_qty <= Qty::ZERO {
                    return self.cancel(&order_id);
                }
                self.exec(&v, ExecKind::Replace, None, None);
                let Tif::Gtd(at) = tif;
                self.schedule(at, Item::Expire(order_id, at));
                self.book_changed();
            }
            Action::Cancel { order_id } => {
                if self.orders.contains_key(&order_id) {
                    self.cancel(&order_id);
                } else {
                    self.reject_missing(&order_id);
                }
            }
            Action::CancelAll => {
                let ids: Vec<OrderId> = self.orders.keys().cloned().collect();
                for id in ids {
                    self.cancel(&id);
                }
            }
            Action::Query(cmd) => {
                let response = match cmd {
                    Command::QueryPositions => Event::Positions(if self.position.is_zero() {
                        Vec::new()
                    } else {
                        vec![PositionFrame {
                            slug: self.slug.clone(),
                            net: self.position,
                            cost: None,
                            resolution: false,
                        }]
                    }),
                    Command::QueryBalance => Event::Balance(self.balance()),
                    Command::QueryOpenOrders { slugs } => Event::OpenOrders {
                        slugs,
                        orders: self.orders.values().map(|o| o.venue.clone()).collect(),
                    },
                    Command::QueryOrder { order_id } => {
                        let order = self
                            .orders
                            .get(&order_id)
                            .map(|o| o.venue.clone())
                            .or_else(|| self.done.get(&order_id).cloned());
                        Event::OrderLookup {
                            id: order_id,
                            order,
                        }
                    }
                    _ => return,
                };
                let back = self.order_lag();
                self.schedule(self.now + back, Item::Deliver(response));
            }
        }
    }

    fn cancel(&mut self, id: &OrderId) {
        if let Some(mut o) = self.orders.remove(id) {
            o.venue.state = OrderState::Canceled;
            self.exec(&o.venue, ExecKind::Canceled, None, None);
            self.done.insert(id.clone(), o.venue);
            self.book_changed();
        }
    }

    fn balance(&self) -> BalanceFrame {
        let open: f64 = self
            .orders
            .values()
            .map(|o| o.venue.leaves_qty.notional(o.venue.px))
            .sum();
        let current = self.p.balance + self.cash;
        BalanceFrame {
            current_balance: current,
            buying_power: current - open,
            open_orders: open,
        }
    }

    /// Record a fill of `qty` at `px` and send the execution. Updates `venue` in place.
    /// Our orders are post-only, so every fill is a maker fill: no fee.
    fn fill_order(
        &mut self,
        venue: &mut VenueOrder,
        qty: Qty,
        px: Px,
        how: &'static str,
        ahead: Qty,
    ) {
        if qty <= Qty::ZERO {
            return;
        }
        let fee = 0.0;
        venue.cum_qty = Qty(venue.cum_qty.0 + qty.0);
        venue.leaves_qty = Qty(venue.leaves_qty.0 - qty.0);
        venue.state = if venue.leaves_qty <= Qty::ZERO {
            OrderState::Filled
        } else {
            OrderState::PartiallyFilled
        };
        let sign = venue.side.sign();
        self.position = Qty(self.position.0 + sign * qty.0);
        self.cash -= sign as f64 * px.as_f64() * qty.as_f64() + fee;
        self.fees += fee;
        self.stats.max_abs_inventory = self
            .stats
            .max_abs_inventory
            .max(self.position.abs().as_f64());
        self.fills.push(FillRow {
            slug: self.meta.slug.clone(),
            t_ns: self.now,
            side: if sign > 0 { "buy" } else { "sell" },
            px: px.as_f64(),
            qty: qty.as_f64(),
            fee,
            how,
            queue_ahead: ahead.as_f64(),
            mid: self.mid(),
            mo_1s: None,
            mo_10s: None,
            mo_60s: None,
            mo_settle: None,
        });
        let info = FillInfo {
            last_shares: qty,
            last_px: px,
            trade_id: format!("F{}", self.fills.len()),
            aggressor: false,
            commission: Some(fee),
        };
        let kind = if venue.leaves_qty <= Qty::ZERO {
            ExecKind::Fill
        } else {
            ExecKind::PartialFill
        };
        self.exec(venue, kind, Some(info), None);
    }

    /// Fill one of our resting orders; removes it when done.
    fn fill_resting(&mut self, id: &OrderId, qty: Qty, how: &'static str) {
        let Some(o) = self.orders.get_mut(id) else {
            return;
        };
        let qty = qty.min(o.venue.leaves_qty);
        let ahead = Qty(o.ahead.iter().map(|(_, q)| q.0).sum());
        let mut venue = o.venue.clone();
        let px = venue.px;
        self.fill_order(&mut venue, qty, px, how, ahead);
        if venue.leaves_qty <= Qty::ZERO {
            self.orders.remove(id);
            self.done.insert(id.clone(), venue);
        } else if let Some(o) = self.orders.get_mut(id) {
            o.venue = venue;
        }
        self.book_changed();
    }

    // ------------------------------------------------------------ exchange: history

    fn on_message(&mut self, mi: usize) {
        let m = self.msgs[mi];
        let all: &'a [Row] = self.rows;
        let rows = &all[m.start..m.end];
        if m.snapshot {
            self.book.load_snapshot(rows);
            let book = &self.book;
            for o in self.orders.values_mut() {
                let (side, px) = (o.venue.side, o.venue.px);
                o.ahead.retain_mut(|(id, q)| match book.get(*id) {
                    Some(r) if r.side == side && r.px == px => {
                        *q = (*q).min(r.qty);
                        true
                    }
                    _ => false,
                });
            }
        } else {
            // Trades first: they consume the queue front-first; the delete/change rows that
            // follow for the same orders can then only lower what's left (no double count).
            for r in rows.iter().filter(|r| r.kind == Kind::Trade) {
                self.on_trade(r);
            }
            for r in rows
                .iter()
                .filter(|r| matches!(r.kind, Kind::Add | Kind::Change | Kind::Delete))
            {
                let effect = self.book.apply(r);
                self.on_book_row(r, effect);
            }
        }
        self.book_changed();
        let mid = self.mid();
        if self.mids.last().is_none_or(|(_, last)| *last != mid) {
            self.mids.push((self.now, mid));
        }
        if let Some(mid) = mid {
            let mtm = self.cash + self.position.as_f64() * mid;
            self.peak = self.peak.max(mtm);
            self.stats.max_drawdown = self.stats.max_drawdown.max(self.peak - mtm);
        }
    }

    fn on_trade(&mut self, r: &Row) {
        self.cum_traded = Qty(self.cum_traded.0 + r.qty.0);
        let trade = TradeFrame {
            id: format!("T{}", r.id),
            slug: self.slug.clone(),
            px: r.px,
            qty: r.qty,
            taker_side: r.aggressor,
            trade_ns: Some(self.now),
        };
        self.deliver_md(Event::Trade(trade), self.p.trade_lag_ms);
        if r.aggressor == Side::Unknown {
            return;
        }
        let passive = r.aggressor.opposite();
        let ids: Vec<OrderId> = self
            .orders
            .iter()
            .filter(|(_, o)| o.venue.side == passive)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let o = self.orders.get_mut(&id).unwrap();
            let px = o.venue.px;
            let through = if passive == Side::Buy {
                r.px < px
            } else {
                r.px > px
            };
            if through {
                self.fill_resting(&id, r.qty, "through");
                continue;
            }
            if r.px != px {
                continue;
            }
            // The trade takes the queue ahead of us first.
            let mut left = r.qty.0;
            for (_, q) in o.ahead.iter_mut() {
                let take = q.0.min(left);
                q.0 -= take;
                left -= take;
                if left == 0 {
                    break;
                }
            }
            o.ahead.retain(|(_, q)| q.0 > 0);
            if left > 0 && self.p.fill_model != FillModel::Pessimistic {
                self.fill_resting(&id, Qty(left), "queue");
            }
        }
    }

    fn on_book_row(&mut self, r: &Row, effect: Effect) {
        let now = self.now;
        let tie = self.p.fill_model == FillModel::Base;
        let window = self.p.tie_window_ns;
        let mut crossing: Vec<OrderId> = Vec::new();
        for (id, o) in self.orders.iter_mut() {
            match effect {
                Effect::Removed => o.ahead.retain(|(oid, _)| *oid != r.id),
                Effect::Requeued => o.ahead.retain(|(oid, _)| *oid != r.id),
                Effect::SizeDown(q) => {
                    for (oid, left) in o.ahead.iter_mut() {
                        if *oid == r.id {
                            *left = (*left).min(q);
                        }
                    }
                }
                Effect::Added | Effect::Unknown => {}
            }
            let joined = matches!(effect, Effect::Added | Effect::Requeued | Effect::Unknown);
            if joined
                && tie
                && r.side == o.venue.side
                && r.px == o.venue.px
                && now - o.joined_ns <= window
            {
                o.ahead.push((r.id, r.qty));
            }
            // A new order that would have crossed our resting quote trades with it.
            let crosses = r.side == o.venue.side.opposite()
                && if o.venue.side == Side::Buy {
                    r.px <= o.venue.px
                } else {
                    r.px >= o.venue.px
                };
            if joined && crosses && !self.crossed_us.contains(&r.id) {
                crossing.push(id.clone());
            }
        }
        if let Some(id) = crossing.first() {
            self.crossed_us.insert(r.id);
            self.fill_resting(&id.clone(), r.qty, "cross");
        }
    }

    /// Send the core a new L2 frame if what it would see changed: the true book plus our
    /// orders, minus historical levels that our quotes would already have traded with.
    fn book_changed(&mut self) {
        let (mut bids, mut offers) = self.book.l2(self.p.book_depth);
        let our_bid = self
            .orders
            .values()
            .filter(|o| o.venue.side == Side::Buy)
            .map(|o| o.venue.px)
            .max();
        let our_ask = self
            .orders
            .values()
            .filter(|o| o.venue.side == Side::Sell)
            .map(|o| o.venue.px)
            .min();
        if let Some(b) = our_bid {
            offers.retain(|(px, _)| *px > b);
        }
        if let Some(a) = our_ask {
            bids.retain(|(px, _)| *px < a);
        }
        for o in self.orders.values() {
            let levels = if o.venue.side == Side::Buy {
                &mut bids
            } else {
                &mut offers
            };
            match levels.iter_mut().find(|(px, _)| *px == o.venue.px) {
                Some(level) => level.1 = Qty(level.1 .0 + o.venue.leaves_qty.0),
                None => levels.push((o.venue.px, o.venue.leaves_qty)),
            }
        }
        bids.sort_by(|a, b| b.0.cmp(&a.0));
        offers.sort_by(|a, b| a.0.cmp(&b.0));
        if (bids.clone(), offers.clone()) == self.last_l2 {
            return;
        }
        if matches!((bids.first(), offers.first()), (Some(b), Some(a)) if b.0 >= a.0) {
            return; // the historical book itself is crossed: the venue wouldn't publish this
        }
        self.last_l2 = (bids.clone(), offers.clone());
        self.stats.books_sent += 1;
        let frame = BookFrame {
            slug: self.slug.clone(),
            bids,
            offers,
            state: MarketState::Open,
            transact_ns: Some(self.now),
            shares_traded: Some(self.cum_traded),
            settlement: None,
        };
        self.deliver_md(Event::Book(frame), self.p.book_lag_ms);
    }

    // ------------------------------------------------------------ results

    /// Mid of the historical book, if its spread is narrow enough to mean something.
    fn mid(&self) -> Option<f64> {
        let (b, a) = (self.book.best(Side::Buy)?, self.book.best(Side::Sell)?);
        (a.0.as_f64() - b.0.as_f64() <= MID_MAX_SPREAD + 1e-9)
            .then(|| (a.0.as_f64() + b.0.as_f64()) / 2.0)
    }

    fn mid_at(&self, t: i64) -> Option<f64> {
        let i = self.mids.partition_point(|(ts, _)| *ts <= t);
        if i > 0 {
            self.mids[i - 1].1
        } else {
            None
        }
    }

    fn finish(mut self) -> SimOutput {
        let settle = self.meta.settlement;
        for i in 0..self.fills.len() {
            let f = &self.fills[i];
            let sign = if f.side == "buy" { 1.0 } else { -1.0 };
            let mo = |m: Option<f64>| m.map(|m| sign * (m - f.px));
            let (m1, m10, m60) = (
                mo(self.mid_at(f.t_ns + SEC)),
                mo(self.mid_at(f.t_ns + 10 * SEC)),
                mo(self.mid_at(f.t_ns + 60 * SEC)),
            );
            let f = &mut self.fills[i];
            (f.mo_1s, f.mo_10s, f.mo_60s, f.mo_settle) =
                (m1, m10, m60, settle.map(|s| sign * (s - f.px)));
        }
        let pnl = settle.map(|s| {
            let mut p = Pnl {
                fees: self.fees,
                ..Pnl::default()
            };
            for f in &self.fills {
                let sign = if f.side == "buy" { 1.0 } else { -1.0 };
                let mid = f.mid.unwrap_or(f.px);
                p.spread += sign * (mid - f.px) * f.qty;
                p.inventory += sign * (s - mid) * f.qty;
            }
            p.total = self.cash + self.position.as_f64() * s;
            p
        });
        let span = (self.end - self.t0).max(1) as f64;
        let mut per_s: Vec<u32> = self.req_per_s.values().copied().collect();
        per_s.sort_unstable();
        let pct = |q: f64| {
            per_s
                .get(((per_s.len() as f64 - 1.0) * q).round() as usize)
                .copied()
                .unwrap_or(0) as f64
        };
        self.stats.replay_hours = span / 3.6e12;
        self.stats.req_per_s_p50 = pct(0.5);
        self.stats.req_per_s_p99 = pct(0.99);
        self.stats.req_per_s_max = per_s.last().copied().unwrap_or(0);
        self.stats.rate_limited_s = self.limited_s.len() as u64;
        self.stats.uptime_bid = self.bid_up_ns as f64 / span;
        self.stats.uptime_ask = self.ask_up_ns as f64 / span;
        self.stats.twa_abs_inventory = self.inv_ns / span;
        self.stats.command_hash = format!("{:016x}", self.hasher.finish());
        SimOutput {
            fills: self.fills,
            pnl,
            stats: self.stats,
        }
    }
}

fn token_of(cmd: &Command) -> Option<Token> {
    match cmd {
        Command::Place { token, .. }
        | Command::Modify { token, .. }
        | Command::Cancel { token, .. }
        | Command::CancelAll { token, .. } => Some(*token),
        _ => None,
    }
}

fn rest_kind(cmd: &Command) -> Option<RestKind> {
    match cmd {
        Command::Place { .. } => Some(RestKind::Create),
        Command::Modify { .. } => Some(RestKind::Modify),
        Command::Cancel { .. } => Some(RestKind::Cancel),
        Command::CancelAll { .. } => Some(RestKind::CancelAll),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{messages, parse_rows};

    const T0: i64 = 1_786_000_000_000_000_000;

    fn meta() -> MarketMeta {
        MarketMeta {
            slug: "aec-pdcdarts-a-b-2026-08-10".into(),
            title: "A vs B".into(),
            event_id: "e".into(),
            event_slug: "pdcdarts-a-b".into(),
            league: "pdcdarts".into(),
            sports_market_type: "darts_match_winner".into(),
            tick: "0.01".into(),
            min_qty: "0.01".into(),
            fee_coefficient: Some(0.0695),
            game_start: None,
            yes_name: None,
            yes_ordering: Some("home".into()),
            settlement: Some(1.0),
            rows: 0,
            trades: 0,
        }
    }

    /// Book 0.45 (A, 10) / 0.55 (B, 10); at +5 s the given rows; at +60 s a quiet change to end the replay.
    fn csv(at_5s: &str) -> String {
        let t = |s: i64| T0 + s * SEC;
        let mut out = format!(
            "rx_ns,ev_ns,msg_seq,entry_index,kind,side,price,size,id,aggressor
{0},{0},1,-1,M,,,,,
{0},{0},1,0,S,b,0.45,10,A,
{0},{0},1,1,S,o,0.55,10,B,
",
            t(0)
        );
        for line in at_5s.lines().filter(|l| !l.trim().is_empty()) {
            out.push_str(&format!("{0},{0},2,{line}\n", t(5)));
        }
        out.push_str(&format!("{0},{0},3,0,U,o,0.55,9,B,\n", t(60)));
        out
    }

    fn run(csv: &str, profile: &str, fill_model: FillModel) -> SimOutput {
        let rows = parse_rows(csv).unwrap();
        let msgs = messages(&rows);
        let cfg: Config = toml::from_str(&format!("[strategies.default]\n{profile}")).unwrap();
        let p = SimParams {
            fill_model,
            ..SimParams::default()
        };
        let m = meta();
        Sim::new(&m, &rows, &msgs, cfg, p).run()
    }

    /// Quotes at others' touch on the 0.45 / 0.55 book.
    const JOIN: &str = "kind = \"full_match\"\nhalf_spread = 0.05\nmax_spread = 0.10";

    #[test]
    fn no_trade_sends_no_orders_and_makes_nothing() {
        let out = run(
            &csv("0,T,,0.45,12,T1,s\n1,D,b,0.45,10,A,"),
            "kind = \"off\"",
            FillModel::Optimistic,
        );
        assert_eq!(out.stats.order_requests, 0);
        assert!(out.fills.is_empty());
        assert_eq!(out.pnl.unwrap().total, 0.0);
    }

    #[test]
    fn trade_that_only_takes_the_queue_ahead_does_not_fill_us() {
        // The feed sends the delete of A before the trade; counting both would move us up twice.
        let out = run(
            &csv("0,D,b,0.45,10,A,\n1,T,,0.45,10,T1,s"),
            JOIN,
            FillModel::Optimistic,
        );
        assert!(out.fills.is_empty(), "{:?}", out.fills);
        assert!(
            out.stats.order_requests >= 2,
            "the touch quote should have been placed"
        );
    }

    #[test]
    fn queue_fill_after_queue_ahead_unless_pessimistic() {
        let rows = "0,D,b,0.45,10,A,\n1,T,,0.45,12,T1,s";
        let out = run(&csv(rows), JOIN, FillModel::Optimistic);
        assert_eq!(out.fills.len(), 1, "{:?}", out.fills);
        let f = &out.fills[0];
        assert_eq!(
            (f.side, f.px, f.qty, f.how, f.queue_ahead),
            ("buy", 0.45, 1.0, "queue", 0.0)
        );
        assert!(run(&csv(rows), JOIN, FillModel::Pessimistic)
            .fills
            .is_empty());
    }

    #[test]
    fn trade_through_fills_in_every_model() {
        for model in [
            FillModel::Optimistic,
            FillModel::Base,
            FillModel::Pessimistic,
        ] {
            let out = run(&csv("0,T,,0.44,3,T1,s\n1,D,b,0.45,10,A,"), JOIN, model);
            assert_eq!(out.fills.len(), 1, "{model:?}");
            assert_eq!((out.fills[0].how, out.fills[0].px), ("through", 0.45));
        }
    }

    #[test]
    fn pnl_decomposes_and_conserves() {
        let out = run(
            &csv("0,D,b,0.45,10,A,\n1,T,,0.45,12,T1,s"),
            JOIN,
            FillModel::Optimistic,
        );
        let p = out.pnl.unwrap();
        // Bought 1 at 0.45 with mid 0.50, settled at 1: +0.05 spread, +0.50 inventory, no maker fee.
        assert!((p.total - 0.55).abs() < 1e-9, "{p:?}");
        assert!(
            (p.spread - 0.05).abs() < 1e-9 && (p.inventory - 0.50).abs() < 1e-9 && p.fees == 0.0,
            "{p:?}"
        );
        assert!((p.total - (p.spread + p.inventory - p.fees)).abs() < 1e-9);
    }

    #[test]
    fn same_seed_same_command_stream() {
        let data = csv("0,D,b,0.45,10,A,\n1,T,,0.45,12,T1,s\n2,A,o,0.50,4,C,");
        let a = run(&data, JOIN, FillModel::Base);
        let b = run(&data, JOIN, FillModel::Base);
        assert_eq!(a.stats.command_hash, b.stats.command_hash);
        assert_eq!(a.fills.len(), b.fills.len());
        assert_ne!(
            a.stats.command_hash,
            format!("{:016x}", DefaultHasher::new().finish()),
            "commands were hashed"
        );
    }
}
