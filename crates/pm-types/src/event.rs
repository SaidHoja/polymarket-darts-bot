//! Normalised input events. I/O tasks decode venue frames into these; the core
//! sees nothing else.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::num::{Px, Qty};
use crate::venue::{ExecKind, Feed, Intent, MarketState, OrderState, RejectClass, Side, Tif};

pub type Slug = Arc<str>;
pub type OrderId = Arc<str>;

/// Our id for an order request. Monotonic, journaled, never sent to the venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Token(pub u64);

pub type Level = (Px, Qty);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamOrdering {
    Home,
    Away,
}

/// One tradable market from discovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketParams {
    pub slug: Slug,
    pub title: String,
    pub event_id: String,
    pub event_slug: String,
    pub league: String,
    pub sports_market_type: String,
    pub tick: Px,
    pub min_qty: Qty,
    /// Taker fee coefficient Θ.
    pub fee_coefficient: f64,
    pub game_start_ns: Option<i64>,
    pub yes_name: Option<String>,
    pub no_name: Option<String>,
    /// `team.ordering` of the YES (long) side.
    pub yes_ordering: Option<TeamOrdering>,
    pub is_futures: bool,
}

/// A validated full L2 book from one `marketData` frame. Levels are best-first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookFrame {
    pub slug: Slug,
    pub bids: Vec<Level>,
    pub offers: Vec<Level>,
    pub state: MarketState,
    pub transact_ns: Option<i64>,
    /// Cumulative contracts traded (`stats.sharesTraded`).
    pub shares_traded: Option<Qty>,
    /// The YES settlement price (`stats.settlementPx`), once the market has resolved.
    #[serde(default)]
    pub settlement: Option<Px>,
}

impl BookFrame {
    pub fn best_bid(&self) -> Option<Level> {
        self.bids.first().copied()
    }

    pub fn best_ask(&self) -> Option<Level> {
        self.offers.first().copied()
    }

    /// Sanity checks before a book replaces the previous one.
    pub fn problems(&self, tick: Option<Px>) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.bids.windows(2).any(|w| w[0].0 <= w[1].0) {
            out.push("bids not strictly descending");
        }
        if self.offers.windows(2).any(|w| w[0].0 >= w[1].0) {
            out.push("offers not strictly ascending");
        }
        if let (Some(b), Some(a)) = (self.best_bid(), self.best_ask()) {
            if b.0 >= a.0 {
                out.push("crossed book");
            }
        }
        let levels = || self.bids.iter().chain(self.offers.iter());
        if levels().any(|l| l.1 <= Qty::ZERO) {
            out.push("non-positive quantity");
        }
        if levels().any(|l| l.0 < Px::MIN || l.0 > Px::MAX) {
            out.push("price out of range");
        }
        if let Some(tick) = tick {
            if levels().any(|l| !l.0.is_on_tick(tick)) {
                out.push("price off tick");
            }
        }
        out
    }
}

/// One public fill. `taker_side` is the only reliable direction, in YES-book terms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeFrame {
    pub id: String,
    pub slug: Slug,
    pub px: Px,
    pub qty: Qty,
    pub taker_side: Side,
    pub trade_ns: Option<i64>,
}

/// Live match state from the events endpoint. `score` is legs `(home, away)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameFrame {
    pub event_id: String,
    pub event_slug: String,
    pub period: String,
    pub live: bool,
    pub ended: bool,
    pub score: Option<(u16, u16)>,
    pub updated_ns: Option<i64>,
}

impl GameFrame {
    pub fn cancelled(&self) -> bool {
        self.period == "CAN"
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VenueOrder {
    pub id: OrderId,
    pub slug: Slug,
    pub side: Side,
    pub intent: Intent,
    pub px: Px,
    pub qty: Qty,
    pub cum_qty: Qty,
    pub leaves_qty: Qty,
    pub state: OrderState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FillInfo {
    pub last_shares: Qty,
    pub last_px: Px,
    pub trade_id: String,
    pub aggressor: bool,
    /// Fee charged on this execution, in dollars, when the venue reports it.
    pub commission: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reject {
    pub class: RejectClass,
    pub reason: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionFrame {
    pub exec_id: String,
    pub order: VenueOrder,
    pub kind: ExecKind,
    pub fill: Option<FillInfo>,
    pub reject: Option<Reject>,
    /// Exchange-initiated cancel reason, absent when the venue sends `*_UNDEFINED`.
    pub unsolicited: Option<String>,
    pub transact_ns: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionFrame {
    pub slug: Slug,
    /// Signed net YES position.
    pub net: Qty,
    /// Net YES cash paid (buys − sells), excluding fees, when reported. Venue outcome cost is
    /// normalized by the adapter, including reported realized PnL.
    pub cost: Option<f64>,
    /// This change is the market resolving (`LEDGER_ENTRY_TYPE_RESOLUTION`), not a trade.
    #[serde(default)]
    pub resolution: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalanceFrame {
    pub current_balance: f64,
    pub buying_power: f64,
    pub open_orders: f64,
}

/// What a REST call was for, so the core can route its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestKind {
    Create,
    Modify,
    Cancel,
    CancelAll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RestOutcome {
    /// Accepted by the gateway: per entry, in request order. For modify and cancel these are echoes only.
    Accepted { ids: Vec<Option<OrderId>> },
    /// Removed from the gateway queue before it was sent.
    Withdrawn,
    /// HTTP 400: the whole batch was refused before the exchange saw it.
    GatewayRejected {
        status: u16,
        body: String,
        class: RejectClass,
    },
    /// HTTP 429 after the gateway's own retries.
    RateLimited,
    /// HTTP 401.
    AuthFailed { body: String },
    /// Timeout, reset or 5xx: the request may or may not have taken effect.
    Ambiguous { error: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestResult {
    pub kind: RestKind,
    pub tokens: Vec<Token>,
    pub outcome: RestOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnState {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdminCmd {
    Pause(Option<String>),
    Resume(Option<String>),
    CancelAll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// Full discovery result; the core diffs it against what it has.
    Markets(Vec<MarketParams>),
    Book(BookFrame),
    Trade(TradeFrame),
    Game(GameFrame),
    /// A game-state poll succeeded (whether or not anything changed). Drives staleness.
    GamePolled,
    /// Complete open-order snapshot (after `eof`) from the private socket.
    OrderSnapshot(Vec<VenueOrder>),
    Execution(ExecutionFrame),
    Position(PositionFrame),
    /// Result of `GET /v1/markets/{slug}/settlement`: the YES settlement price, `None` while unresolved.
    Settlement {
        slug: Slug,
        px: Option<Px>,
    },
    /// Full position set from REST (startup, resync, reconciliation).
    Positions(Vec<PositionFrame>),
    Balance(BalanceFrame),
    RestResult(RestResult),
    /// Result of `GET /v1/orders/open` for these slugs (empty = all).
    OpenOrders {
        slugs: Vec<Slug>,
        orders: Vec<VenueOrder>,
    },
    /// Result of `GET /v1/order/{id}`; `None` for a 404.
    OrderLookup {
        id: OrderId,
        order: Option<VenueOrder>,
    },
    Connection {
        feed: Feed,
        state: ConnState,
    },
    Admin(AdminCmd),
}

impl Event {
    /// Drain priority within a tick (lower first).
    pub fn priority(&self) -> u8 {
        match self {
            Event::Connection { .. } | Event::Admin(_) | Event::Markets(_) => 0,
            Event::OrderSnapshot(_)
            | Event::Execution(_)
            | Event::Position(_)
            | Event::Settlement { .. }
            | Event::Positions(_)
            | Event::Balance(_)
            | Event::RestResult(_)
            | Event::OpenOrders { .. }
            | Event::OrderLookup { .. } => 1,
            Event::Game(_) | Event::GamePolled => 2,
            Event::Trade(_) => 3,
            Event::Book(_) => 4,
        }
    }
}

/// Priority of an outgoing action in the gateway queue (lower first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    CancelAll = 0,
    Cancel = 1,
    Reduce = 2,
    Quote = 3,
    Refresh = 4,
    Read = 5,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewOrder {
    pub slug: Slug,
    pub intent: Intent,
    /// Always the YES price.
    pub px: Px,
    pub qty: Qty,
    pub tif: Tif,
    pub post_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Place {
        token: Token,
        order: NewOrder,
        priority: Priority,
    },
    Modify {
        token: Token,
        order_id: OrderId,
        slug: Slug,
        px: Px,
        qty: Qty,
        tif: Tif,
        post_only: bool,
        priority: Priority,
    },
    Cancel {
        token: Token,
        order_id: OrderId,
        slug: Slug,
        priority: Priority,
    },
    /// `POST /v1/orders/open/cancel`; empty slugs = everything.
    CancelAll {
        token: Token,
        slugs: Vec<Slug>,
    },
    /// Drop a queued action if the gateway hasn't sent it yet.
    Withdraw {
        token: Token,
    },
    QueryOpenOrders {
        slugs: Vec<Slug>,
    },
    QueryOrder {
        order_id: OrderId,
    },
    QueryPositions,
    QueryBalance,
    /// `GET /v1/markets/{slug}/settlement`, answered with `Event::Settlement`.
    QuerySettlement {
        slug: Slug,
    },
    RefreshMarket {
        slug: Slug,
    },
}

impl Command {
    pub fn priority(&self) -> Priority {
        match self {
            Command::Place { priority, .. }
            | Command::Modify { priority, .. }
            | Command::Cancel { priority, .. } => *priority,
            Command::CancelAll { .. } => Priority::CancelAll,
            Command::Withdraw { .. } => Priority::CancelAll,
            _ => Priority::Read,
        }
    }
}
