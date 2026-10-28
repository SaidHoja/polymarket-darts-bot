//! Read-only view of the core for the operator UI.
//!
//! Built by the shell at most once a second, after a tick's commands are out,
//! and only while someone is watching. Nothing here mutates the core.
//! `market_detail` (full book, orders, fills) is built only for markets whose page is open.

use pm_types::time::NS_PER_SEC;
use std::collections::BTreeMap;

use pm_types::{BalanceFrame, Level, MarketState, Side, Slug, Tif};
use serde::Serialize;

use crate::core::{Clock, Core};
use crate::portfolio::{FillLog, Position};
use crate::strategy::Phase;

/// The venue mid counts as the current price only when the spread is at most this.
const MID_MAX_SPREAD: f64 = 0.10;
/// Fills in the snapshot (all markets) and on a market page.
const FILLS_SHOWN: usize = 20;
const TRADES_SHOWN: usize = 20;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub wall_ns: i64,
    pub markets_up: bool,
    pub private_up: bool,
    pub admin_paused: bool,
    /// Daily loss kill triggered today.
    pub killed: bool,
    pub orders_blocked: bool,
    pub slow: bool,
    pub balance: Option<BalanceFrame>,
    pub fills: u64,
    pub open_orders: usize,
    pub fees: f64,
    /// Positions marked at the venue mid (else the expected price, else average cost).
    pub current_pnl: f64,
    /// Positions marked at the expected price: strategy fair, else microprice (`Core::marks`).
    pub expected_pnl: f64,
    /// Open positions with no mid / no expected price, valued at average cost in the totals.
    pub unmarked_current: usize,
    pub unmarked_expected: usize,
    /// Expected PnL since the first mark of the UTC day, as the daily loss limit sees it.
    pub daily_pnl: f64,
    pub daily_loss_limit: f64,
    pub account_worst: f64,
    pub max_account_loss: f64,
    pub max_market_loss: f64,
    /// Active markets and any market we hold a position in.
    pub markets: Vec<MarketRow>,
    /// Our latest fills across all markets, newest first.
    pub recent_fills: Vec<FillLog>,
    /// Markets whose page is open (`Core::market_detail`), filled in by the shell.
    pub details: Vec<MarketDetail>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MarketRow {
    pub slug: String,
    pub title: String,
    pub yes_name: Option<String>,
    pub no_name: Option<String>,
    pub league: String,
    pub strategy: String,
    pub active: bool,
    pub phase: Option<Phase>,
    pub live: bool,
    pub score: Option<(u16, u16)>,
    /// (price, size)
    pub best_bid: Option<(f64, f64)>,
    pub best_ask: Option<(f64, f64)>,
    pub mid: Option<f64>,
    pub spread: Option<f64>,
    /// Size-weighted venue price and contracts in the best `vwap_depth` levels per side (`BookView::vwap`).
    pub vwap: Option<(f64, f64)>,
    /// Configured price levels per side for VWAP; 0 includes all retained levels.
    pub vwap_depth: usize,
    pub fair: Option<f64>,
    /// The price expected PnL marks at.
    pub expected_px: Option<f64>,
    /// Signed net YES contracts.
    pub qty: f64,
    pub avg_cost: Option<f64>,
    pub pnl_if_yes: f64,
    pub pnl_if_no: f64,
    pub worst_case: f64,
    pub current_pnl: Option<f64>,
    pub expected_pnl: Option<f64>,
    /// Our resting orders: (price, leaves).
    pub our_bids: Vec<(f64, f64)>,
    pub our_asks: Vec<(f64, f64)>,
    pub paused: Option<String>,
    /// Our last fill here, if it is still in the fill log.
    pub last_fill_ns: Option<i64>,
}

/// One market's page: full book, our orders and fills, recent trades.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MarketDetail {
    pub slug: String,
    pub event_slug: String,
    pub market_type: String,
    pub state: Option<MarketState>,
    /// Venue `transactTime` of the book shown.
    pub book_ns: Option<i64>,
    /// Cumulative contracts traded on the venue.
    pub shares_traded: Option<f64>,
    pub tick: f64,
    pub min_qty: f64,
    pub fee_coefficient: f64,
    pub game_start_ns: Option<i64>,
    /// Venue book, best first, with our resting size at each level.
    pub bids: Vec<DepthLevel>,
    pub offers: Vec<DepthLevel>,
    pub orders: Vec<OrderRow>,
    /// The strategy's last targets: (price, size).
    pub target_bid: Option<(f64, f64)>,
    pub target_ask: Option<(f64, f64)>,
    pub fees: f64,
    /// Newest first.
    pub fills: Vec<FillLog>,
    pub trades: Vec<TradeRow>,
    pub books_applied: u64,
    pub trades_seen: u64,
    pub trades_lost: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DepthLevel {
    pub px: f64,
    pub qty: f64,
    pub ours: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderRow {
    pub side: Side,
    pub intent: String,
    pub state: String,
    pub px: f64,
    pub qty: f64,
    pub filled: f64,
    pub leaves: f64,
    pub age_s: f64,
    pub expires_ns: Option<i64>,
    pub post_only: bool,
}

/// One aggressive order on the venue (trade frames grouped, `Sweep`).
#[derive(Debug, Clone, Serialize)]
pub struct TradeRow {
    pub trade_ns: Option<i64>,
    pub taker_side: Side,
    pub qty: f64,
    pub worst_px: f64,
}

fn fallback(p: &Position) -> f64 {
    if p.qty.is_zero() {
        0.0
    } else {
        (p.cost / p.qty.as_f64()).clamp(0.0, 1.0)
    }
}

impl Core {
    pub fn snapshot(&self, clock: Clock) -> Snapshot {
        let now = clock.mono;
        let day = clock.wall_ns.div_euclid(86_400 * NS_PER_SEC);
        let marks = self.marks();
        let mut s = Snapshot {
            wall_ns: clock.wall_ns,
            markets_up: self.markets_up,
            private_up: self.private_up,
            admin_paused: self.risk.admin_paused,
            killed: self.risk.killed_day == Some(day),
            orders_blocked: now < self.risk.orders_blocked_until,
            slow: now < self.risk.slow_until,
            balance: self.portfolio.balance.clone(),
            fills: self.portfolio.fills,
            open_orders: self.om.active().count(),
            daily_loss_limit: self.cfg.risk.daily_loss_limit,
            max_account_loss: self.cfg.risk.max_account_loss,
            max_market_loss: self.cfg.risk.max_market_loss,
            ..Default::default()
        };

        let mut slugs: Vec<_> = self
            .markets
            .iter()
            .filter(|(_, m)| m.active)
            .map(|(s, _)| s.clone())
            .collect();
        slugs.extend(
            self.portfolio
                .positions
                .iter()
                .filter(|(_, p)| !p.qty.is_zero())
                .map(|(s, _)| s.clone()),
        );
        slugs.sort();
        slugs.dedup();

        // Totals cover every position, including flat ones in markets that have gone (their realised PnL).
        let mut prices = BTreeMap::new();
        for slug in self.portfolio.positions.keys() {
            prices.insert(
                slug.clone(),
                (self.current_px(slug, &marks), marks.get(slug).copied()),
            );
        }
        for (slug, pos) in &self.portfolio.positions {
            let (current_px, expected_px) = prices[slug];
            let open = !pos.qty.is_zero();
            s.fees += pos.fees;
            s.current_pnl += pos.marked(current_px.unwrap_or_else(|| fallback(pos)));
            s.expected_pnl += pos.marked(expected_px.unwrap_or_else(|| fallback(pos)));
            s.unmarked_current += usize::from(open && current_px.is_none());
            s.unmarked_expected += usize::from(open && expected_px.is_none());
        }

        for slug in slugs {
            let pos = self.portfolio.position(&slug);
            let market = self.markets.get(&slug);
            let view = market.map(|m| m.md.venue_view()).unwrap_or_default();
            let (bid, ask) = (view.best_bid(), view.best_ask());
            let mid = bid
                .zip(ask)
                .map(|(b, a)| (b.0.as_f64() + a.0.as_f64()) / 2.0);
            let spread = view.spread();
            let expected_px = marks.get(&slug).copied();
            let current_px = self.current_px(&slug, &marks);
            let open = !pos.qty.is_zero();
            let worst = self.market_worst(&slug);
            s.account_worst += worst;

            let mut row = MarketRow {
                slug: slug.to_string(),
                best_bid: bid.map(|l| (l.0.as_f64(), l.1.as_f64())),
                best_ask: ask.map(|l| (l.0.as_f64(), l.1.as_f64())),
                mid,
                spread,
                vwap: view.vwap(self.cfg.markets.vwap_depth),
                vwap_depth: self.cfg.markets.vwap_depth,
                expected_px,
                qty: pos.qty.as_f64(),
                avg_cost: open.then(|| pos.cost / pos.qty.as_f64()),
                pnl_if_yes: pos.pnl_at(1.0),
                pnl_if_no: pos.pnl_at(0.0),
                worst_case: worst,
                current_pnl: current_px.map(|p| pos.marked(p)),
                expected_pnl: expected_px.map(|p| pos.marked(p)),
                paused: self.risk.market_pause(&slug, now).map(str::to_string),
                last_fill_ns: self
                    .portfolio
                    .recent_fills
                    .iter()
                    .rev()
                    .find(|f| f.slug == slug)
                    .map(|f| f.wall_ns),
                ..Default::default()
            };
            if let Some(m) = market {
                let game = self.games.get(&m.params.event_id).map(|g| &g.frame);
                row.title = m.params.title.clone();
                row.yes_name = m.params.yes_name.clone();
                row.no_name = m.params.no_name.clone();
                row.league = m.params.league.clone();
                row.strategy = m.strategy_label.clone();
                row.active = m.active;
                row.fair = m.strategy.fair();
                row.phase = self.last_decisions.get(&slug).and_then(|d| d.phase);
                row.live = game.is_some_and(|g| g.live && !g.ended);
                row.score = game.and_then(|g| g.score);
            }
            for rec in self.om.active_in(&slug) {
                let level = (rec.px.as_f64(), rec.leaves_qty.as_f64());
                match rec.side {
                    Side::Buy => row.our_bids.push(level),
                    Side::Sell => row.our_asks.push(level),
                    Side::Unknown => {}
                }
            }
            row.our_bids.sort_by(|a, b| b.0.total_cmp(&a.0));
            row.our_asks.sort_by(|a, b| a.0.total_cmp(&b.0));
            s.markets.push(row);
        }
        s.recent_fills = self
            .portfolio
            .recent_fills
            .iter()
            .rev()
            .take(FILLS_SHOWN)
            .cloned()
            .collect();
        // The figure the daily kill uses (pessimistic marks, carried across restarts).
        s.daily_pnl = if self.portfolio.day == Some(day) {
            self.portfolio.daily
        } else {
            0.0
        };
        // Open positions first, then live matches, then by title.
        s.markets.sort_by(|a, b| {
            (a.qty == 0.0, !a.live, &a.title).cmp(&(b.qty == 0.0, !b.live, &b.title))
        });
        s
    }

    /// Full detail for one market page. `None` if we neither track the market nor hold a position.
    pub fn market_detail(&self, slug: &str, clock: Clock) -> Option<MarketDetail> {
        let market = self.markets.get(slug);
        let pos = self.portfolio.positions.get(slug);
        if market.is_none() && pos.is_none() {
            return None;
        }
        let mut d = MarketDetail {
            slug: slug.to_string(),
            fees: pos.map_or(0.0, |p| p.fees),
            fills: self
                .portfolio
                .recent_fills
                .iter()
                .rev()
                .filter(|f| &*f.slug == slug)
                .take(FILLS_SHOWN)
                .cloned()
                .collect(),
            ..Default::default()
        };
        let active: Vec<_> = self.om.active_in(slug).collect();
        let ours = |side: Side, px: f64| {
            active
                .iter()
                .filter(|r| r.side == side && r.px.as_f64() == px)
                .map(|r| r.leaves_qty.as_f64())
                .sum()
        };
        let depth = |levels: &[Level], side: Side| -> Vec<DepthLevel> {
            levels
                .iter()
                .map(|l| DepthLevel {
                    px: l.0.as_f64(),
                    qty: l.1.as_f64(),
                    ours: ours(side, l.0.as_f64()),
                })
                .collect()
        };
        if let Some(m) = market {
            d.event_slug = m.params.event_slug.clone();
            d.market_type = m.params.sports_market_type.clone();
            d.tick = m.params.tick.as_f64();
            d.min_qty = m.params.min_qty.as_f64();
            d.fee_coefficient = m.params.fee_coefficient;
            d.game_start_ns = m.params.game_start_ns;
            if let Some(book) = &m.md.book {
                d.state = Some(book.state);
                d.book_ns = book.transact_ns;
                d.shares_traded = book.shares_traded.map(|q| q.as_f64());
                d.bids = depth(&book.bids, Side::Buy);
                d.offers = depth(&book.offers, Side::Sell);
            }
            d.trades =
                m.md.sweeps
                    .iter()
                    .rev()
                    .take(TRADES_SHOWN)
                    .map(|s| TradeRow {
                        trade_ns: s.trade_ns,
                        taker_side: s.taker_side,
                        qty: s.qty.as_f64(),
                        worst_px: s.worst_px.as_f64(),
                    })
                    .collect();
            d.books_applied = m.md.stats.books_applied;
            d.trades_seen = m.md.stats.trades;
            d.trades_lost = m.md.trades_lost;
        }
        if let Some(dec) = self.last_decisions.get(slug) {
            let level =
                |l: Option<(pm_types::Px, pm_types::Qty)>| l.map(|(p, q)| (p.as_f64(), q.as_f64()));
            d.target_bid = level(dec.bid);
            d.target_ask = level(dec.ask);
        }
        d.orders = active
            .iter()
            .map(|r| OrderRow {
                side: r.side,
                intent: format!("{:?}", r.intent),
                state: format!("{:?}", r.state),
                px: r.px.as_f64(),
                qty: r.qty.as_f64(),
                filled: r.cum_qty.as_f64(),
                leaves: r.leaves_qty.as_f64(),
                age_s: clock.mono.saturating_sub(r.sent_mono) as f64 / 1e9,
                expires_ns: match r.tif {
                    Tif::Gtd(ns) => Some(ns),
                },
                post_only: r.post_only,
            })
            .collect();
        d.orders.sort_by(|a, b| b.px.total_cmp(&a.px));
        Some(d)
    }

    /// Venue mid when the spread is tight enough, else the expected price.
    fn current_px(&self, slug: &str, marks: &BTreeMap<Slug, f64>) -> Option<f64> {
        let mid = self.markets.get(slug).and_then(|m| {
            let view = m.md.venue_view();
            let (b, a) = (view.best_bid()?, view.best_ask()?);
            (view.spread()? <= MID_MAX_SPREAD).then(|| (b.0.as_f64() + a.0.as_f64()) / 2.0)
        });
        mid.or_else(|| marks.get(slug).copied())
    }
}
