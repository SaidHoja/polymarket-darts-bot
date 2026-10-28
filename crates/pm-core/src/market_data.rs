//! Per-market book and trade state.
//!
//! A book frame is state: it replaces the venue book atomically. A trade frame
//! is an event: it goes into the trade log and a provisional overlay until a
//! book with `transactTime ≥ tradeTime` confirms it.

use std::collections::{HashSet, VecDeque};

use pm_types::{BookFrame, Level, MarketState, Px, Qty, Side, TradeFrame};

const TRADE_ID_MEMORY: usize = 512;
const SWEEP_MEMORY: usize = 64;

/// One aggressive order: trade frames grouped by `(tradeTime, taker side)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Sweep {
    pub trade_ns: Option<i64>,
    pub taker_side: Side,
    pub qty: Qty,
    pub worst_px: Px,
    pub trade_ids: Vec<String>,
    pub seen_mono: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MdChange {
    Seeded,
    TouchChanged,
    StateChanged(MarketState),
    /// Cumulative `sharesTraded` rose by more than the trades we received.
    MissedVolume(Qty),
    Invalid(Vec<&'static str>),
    Stale,
    Sweep,
}

/// A book view: venue book with provisional trades applied, optionally minus our own orders.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BookView {
    pub bids: Vec<Level>,
    pub offers: Vec<Level>,
}

impl BookView {
    pub fn best_bid(&self) -> Option<Level> {
        self.bids.first().copied()
    }

    pub fn best_ask(&self) -> Option<Level> {
        self.offers.first().copied()
    }

    /// Remove `qty` at `px` from one side (our own resting order), dropping empty levels.
    pub fn remove(&mut self, side: Side, px: Px, qty: Qty) {
        let levels = if side == Side::Buy {
            &mut self.bids
        } else {
            &mut self.offers
        };
        if let Some(level) = levels.iter_mut().find(|l| l.0 == px) {
            level.1 -= qty;
        }
        levels.retain(|l| l.1 > Qty::ZERO);
    }

    /// Size-weighted mid (micro-price) when both sides exist.
    pub fn microprice(&self) -> Option<f64> {
        let (b, a) = (self.best_bid()?, self.best_ask()?);
        let (bq, aq) = (b.1.as_f64(), a.1.as_f64());
        if bq + aq <= 0.0 {
            return Some((b.0.as_f64() + a.0.as_f64()) / 2.0);
        }
        Some((b.0.as_f64() * aq + a.0.as_f64() * bq) / (bq + aq))
    }

    /// Size-weighted average price of the best `depth` price levels per side, and their contracts.
    /// A depth of 0 includes all retained levels. A short or missing side uses the levels available.
    pub fn vwap(&self, depth: usize) -> Option<(f64, f64)> {
        let depth = if depth == 0 { usize::MAX } else { depth };
        let (notional, qty) = self
            .bids
            .iter()
            .take(depth)
            .chain(self.offers.iter().take(depth))
            .fold((0.0, 0.0), |(n, q), l| {
                (n + l.0.as_f64() * l.1.as_f64(), q + l.1.as_f64())
            });
        (qty > 0.0).then(|| (notional / qty, qty))
    }

    pub fn spread(&self) -> Option<f64> {
        Some(self.best_ask()?.0.as_f64() - self.best_bid()?.0.as_f64())
    }

    /// Best level on each side with at least `min` contracts: the touch without dust orders.
    pub fn touch_min(&self, min: Qty) -> (Option<Level>, Option<Level>) {
        let first = |levels: &[Level]| levels.iter().find(|l| l.1 >= min).copied();
        (first(&self.bids), first(&self.offers))
    }

    /// Micro-price and spread of the touch ignoring levels smaller than `min`.
    pub fn microprice_spread_min(&self, min: Qty) -> Option<(f64, f64)> {
        let (Some(b), Some(a)) = self.touch_min(min) else {
            return None;
        };
        let (bq, aq) = (b.1.as_f64(), a.1.as_f64());
        let mp = if bq + aq <= 0.0 {
            (b.0.as_f64() + a.0.as_f64()) / 2.0
        } else {
            (b.0.as_f64() * aq + a.0.as_f64() * bq) / (bq + aq)
        };
        Some((mp, a.0.as_f64() - b.0.as_f64()))
    }
}

#[derive(Debug, Default)]
pub struct MarketData {
    pub book: Option<BookFrame>,
    /// A book has arrived on the current markets connection.
    pub seeded: bool,
    /// When the last book was applied (mono), for the staleness check.
    pub last_book_mono: Option<u64>,
    /// Trades not yet in a book, with when each arrived (mono).
    provisional: Vec<(TradeFrame, u64)>,
    recent_ids: VecDeque<String>,
    recent_set: HashSet<String>,
    pub sweeps: VecDeque<Sweep>,
    pub last_trade: Option<(Px, u64)>,
    volume_baseline: Option<Qty>,
    received_since_baseline: Qty,
    shortfall_strikes: u8,
    pub trades_lost: bool,
    pub stats: MdStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MdStats {
    pub books_applied: u64,
    pub books_older: u64,
    pub books_invalid: u64,
    pub trades: u64,
    pub trades_duplicate: u64,
    pub provisional_mismatch: u64,
}

impl MarketData {
    /// Markets connection dropped: the next frame per ticker is a fresh seed; trades in the gap are gone.
    pub fn unseed(&mut self) {
        self.seeded = false;
        self.last_book_mono = None;
        self.provisional.clear();
        self.sweeps.clear();
        self.volume_baseline = None;
        self.received_since_baseline = Qty::ZERO;
        self.shortfall_strikes = 0;
    }

    pub fn on_book(&mut self, book: BookFrame, tick: Px, min_qty: Qty) -> Vec<MdChange> {
        let mut out = Vec::new();
        if let (Some(current), Some(new_ns)) = (&self.book, book.transact_ns) {
            if self.seeded && current.transact_ns.is_some_and(|cur| new_ns < cur) {
                self.stats.books_older += 1;
                return vec![MdChange::Stale];
            }
        }
        let problems = book.problems(Some(tick));
        if !problems.is_empty() {
            self.stats.books_invalid += 1;
            return vec![MdChange::Invalid(problems)];
        }
        if !self.seeded {
            out.push(MdChange::Seeded);
        }
        let old_touch = self
            .book
            .as_ref()
            .map(|b| (b.best_bid(), b.best_ask(), b.state));
        if old_touch.is_some_and(|(_, _, s)| s != book.state)
            || (old_touch.is_none() && book.state != MarketState::Open)
        {
            out.push(MdChange::StateChanged(book.state));
        }
        // Trades with tradeTime ≤ this book's transactTime are already in it.
        let confirmed: Vec<(TradeFrame, u64)> = match book.transact_ns {
            Some(ns) => {
                let (done, pending): (Vec<_>, Vec<_>) = self
                    .provisional
                    .drain(..)
                    .partition(|(t, _)| t.trade_ns.is_some_and(|t| t <= ns));
                self.provisional = pending;
                done
            }
            None => std::mem::take(&mut self.provisional),
        };
        if !confirmed.is_empty() {
            let predicted = apply_trades(
                &self.venue_view_of(self.book.as_ref()),
                confirmed.iter().map(|(t, _)| t),
            );
            let actual = BookView {
                bids: book.bids.clone(),
                offers: book.offers.clone(),
            };
            if predicted.best_bid() != actual.best_bid()
                || predicted.best_ask() != actual.best_ask()
            {
                self.stats.provisional_mismatch += 1;
            }
        }
        if let Some(shares) = book.shares_traded {
            if let Some(missed) = self.check_volume(shares, min_qty) {
                out.push(MdChange::MissedVolume(missed));
            }
        }
        let new_touch = (book.best_bid(), book.best_ask());
        if old_touch.map(|(b, a, _)| (b, a)) != Some(new_touch) {
            out.push(MdChange::TouchChanged);
        }
        self.book = Some(book);
        self.seeded = true;
        self.stats.books_applied += 1;
        out
    }

    /// Compare the venue's cumulative volume with what we received. Flags only a shortfall that persists across two books,
    /// because a trade can arrive just after the book that includes it.
    fn check_volume(&mut self, shares: Qty, min_qty: Qty) -> Option<Qty> {
        let tolerance = Qty(min_qty.0.max(1));
        let Some(base) = self.volume_baseline else {
            self.volume_baseline = Some(shares);
            self.received_since_baseline = Qty::ZERO;
            return None;
        };
        let shortfall = (shares - base) - self.received_since_baseline;
        if shortfall >= tolerance {
            self.shortfall_strikes += 1;
            if self.shortfall_strikes >= 2 {
                self.volume_baseline = Some(shares);
                self.received_since_baseline = Qty::ZERO;
                self.shortfall_strikes = 0;
                return Some(shortfall);
            }
        } else {
            self.shortfall_strikes = 0;
            if shortfall <= Qty::ZERO {
                // Caught up: move the baseline forward so old history doesn't accumulate.
                self.volume_baseline = Some(shares);
                self.received_since_baseline = -shortfall.min(Qty::ZERO);
            }
        }
        None
    }

    pub fn on_trade(&mut self, trade: TradeFrame, now: u64) -> Vec<MdChange> {
        if !trade.id.is_empty() {
            if self.recent_set.contains(&trade.id) {
                self.stats.trades_duplicate += 1;
                return Vec::new();
            }
            self.recent_set.insert(trade.id.clone());
            self.recent_ids.push_back(trade.id.clone());
            if self.recent_ids.len() > TRADE_ID_MEMORY {
                if let Some(old) = self.recent_ids.pop_front() {
                    self.recent_set.remove(&old);
                }
            }
        }
        self.stats.trades += 1;
        self.received_since_baseline += trade.qty;
        self.last_trade = Some((trade.px, now));
        let worse = |a: Px, b: Px| {
            if trade.taker_side == Side::Buy {
                a.max(b)
            } else {
                a.min(b)
            }
        };
        match self.sweeps.back_mut() {
            Some(s)
                if s.trade_ns == trade.trade_ns
                    && s.taker_side == trade.taker_side
                    && trade.trade_ns.is_some() =>
            {
                s.qty += trade.qty;
                s.worst_px = worse(s.worst_px, trade.px);
                s.trade_ids.push(trade.id.clone());
            }
            _ => {
                self.sweeps.push_back(Sweep {
                    trade_ns: trade.trade_ns,
                    taker_side: trade.taker_side,
                    qty: trade.qty,
                    worst_px: trade.px,
                    trade_ids: vec![trade.id.clone()],
                    seen_mono: now,
                });
                if self.sweeps.len() > SWEEP_MEMORY {
                    self.sweeps.pop_front();
                }
            }
        }
        let current_ns = self.book.as_ref().and_then(|b| b.transact_ns);
        let already_in_book = matches!((trade.trade_ns, current_ns), (Some(t), Some(b)) if t <= b);
        if !already_in_book {
            self.provisional.push((trade, now));
        }
        vec![MdChange::Sweep]
    }

    fn venue_view_of(&self, book: Option<&BookFrame>) -> BookView {
        book.map(|b| BookView {
            bids: b.bids.clone(),
            offers: b.offers.clone(),
        })
        .unwrap_or_default()
    }

    pub fn venue_view(&self) -> BookView {
        self.venue_view_of(self.book.as_ref())
    }

    /// Venue book with unconfirmed trades taken off the passive side.
    pub fn provisional_view(&self) -> BookView {
        apply_trades(&self.venue_view(), self.provisional.iter().map(|(t, _)| t))
    }

    /// Why the book can't be trusted any more, if it can't: a trade it hasn't caught up with after
    /// `after_trade` ns, or, when `live` is given, no book for that long. The venue only sends a book
    /// when it changes, so a quiet book is normal (in-play gaps reach ~50 s); a trade with no book
    /// after it is not (10-28 captures: the next book came within 5 s of every trade). 0 disables.
    pub fn stale_reason(
        &self,
        now: u64,
        after_trade: u64,
        live: Option<u64>,
    ) -> Option<&'static str> {
        let unconfirmed = |(t, seen): &(TradeFrame, u64)| {
            t.trade_ns.is_some() && now.saturating_sub(*seen) >= after_trade
        };
        if after_trade > 0 && self.provisional.iter().any(unconfirmed) {
            return Some("book stale: a trade with no book after it");
        }
        match (live.filter(|l| *l > 0), self.last_book_mono) {
            (Some(limit), Some(at)) if now.saturating_sub(at) >= limit => {
                Some("book stale: no book while live")
            }
            _ => None,
        }
    }

    pub fn state(&self) -> MarketState {
        self.book
            .as_ref()
            .map(|b| b.state)
            .unwrap_or(MarketState::Unknown)
    }
}

/// Whether a level at the first price is taken by a trade at the second.
type Crosses = fn(Px, Px) -> bool;

/// A taker BUY at p takes from offers at ≤ p (best first); a SELL takes from bids at ≥ p.
fn apply_trades<'a>(view: &BookView, trades: impl IntoIterator<Item = &'a TradeFrame>) -> BookView {
    let mut out = view.clone();
    for t in trades {
        let (levels, crosses): (&mut Vec<Level>, Crosses) = match t.taker_side {
            Side::Buy => (&mut out.offers, |level, px| level <= px),
            Side::Sell => (&mut out.bids, |level, px| level >= px),
            Side::Unknown => continue,
        };
        let mut left = t.qty;
        for level in levels.iter_mut() {
            if left <= Qty::ZERO || !crosses(level.0, t.px) {
                break;
            }
            let take = left.min(level.1);
            level.1 -= take;
            left -= take;
        }
        levels.retain(|l| l.1 > Qty::ZERO);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn book(ns: i64, bids: &[(i64, i64)], offers: &[(i64, i64)], shares: Option<i64>) -> BookFrame {
        BookFrame {
            slug: Arc::from("m"),
            bids: bids.iter().map(|&(p, q)| (Px(p), Qty(q))).collect(),
            offers: offers.iter().map(|&(p, q)| (Px(p), Qty(q))).collect(),
            state: MarketState::Open,
            transact_ns: Some(ns),
            shares_traded: shares.map(Qty),
            settlement: None,
        }
    }

    fn trade(id: &str, ns: i64, px: i64, qty: i64, side: Side) -> TradeFrame {
        TradeFrame {
            id: id.into(),
            slug: Arc::from("m"),
            px: Px(px),
            qty: Qty(qty),
            taker_side: side,
            trade_ns: Some(ns),
        }
    }

    #[test]
    fn provisional_trade_then_confirming_book() {
        let mut md = MarketData::default();
        md.on_book(
            book(
                100,
                &[(4000, 10_000)],
                &[(4500, 10_000), (4600, 20_000)],
                Some(0),
            ),
            Px(100),
            Qty(100),
        );
        md.on_trade(trade("t1", 150, 4600, 15_000, Side::Buy), 1);
        let view = md.provisional_view();
        assert_eq!(view.best_ask(), Some((Px(4600), Qty(15_000))));
        // Duplicate is ignored.
        assert!(md
            .on_trade(trade("t1", 150, 4600, 15_000, Side::Buy), 2)
            .is_empty());
        // The confirming book retires the provisional trade.
        md.on_book(
            book(160, &[(4000, 10_000)], &[(4600, 15_000)], Some(15_000)),
            Px(100),
            Qty(100),
        );
        assert_eq!(md.provisional_view(), md.venue_view());
        assert_eq!(md.stats.provisional_mismatch, 0);
    }

    #[test]
    fn a_trade_without_a_book_after_it_or_a_silent_live_book_is_stale() {
        const SEC: u64 = 1_000_000_000;
        let (after_trade, live) = (10 * SEC, 90 * SEC);
        let mut md = MarketData::default();
        md.on_book(
            book(100, &[(4000, 10_000)], &[(4500, 10_000)], Some(0)),
            Px(100),
            Qty(100),
        );
        md.last_book_mono = Some(0);
        // Quiet is fine before the live limit, and pre-match it's always fine.
        assert_eq!(md.stale_reason(89 * SEC, after_trade, Some(live)), None);
        assert_eq!(md.stale_reason(1_000 * SEC, after_trade, None), None);
        assert!(md.stale_reason(90 * SEC, after_trade, Some(live)).is_some());
        // A trade the book hasn't caught up with is stale after 10 s, until a book includes it.
        md.on_trade(trade("t1", 150, 4500, 1_000, Side::Buy), 100 * SEC);
        assert_eq!(md.stale_reason(109 * SEC, after_trade, None), None);
        assert!(md.stale_reason(110 * SEC, after_trade, None).is_some());
        md.on_book(
            book(150, &[(4000, 10_000)], &[(4500, 9_000)], Some(1_000)),
            Px(100),
            Qty(100),
        );
        assert_eq!(md.stale_reason(110 * SEC, after_trade, None), None);
        // A trade the current book already has never counts; 0 turns a rule off.
        md.on_trade(trade("t0", 120, 4500, 1_000, Side::Buy), 100 * SEC);
        assert_eq!(md.stale_reason(500 * SEC, after_trade, None), None);
        md.on_trade(trade("t2", 200, 4500, 1_000, Side::Buy), 100 * SEC);
        assert_eq!(md.stale_reason(500 * SEC, 0, Some(0)), None);
    }

    #[test]
    fn older_and_invalid_books_dropped() {
        let mut md = MarketData::default();
        md.on_book(
            book(100, &[(4000, 1)], &[(4500, 1)], None),
            Px(100),
            Qty(100),
        );
        assert_eq!(
            md.on_book(
                book(90, &[(4100, 1)], &[(4500, 1)], None),
                Px(100),
                Qty(100)
            ),
            vec![MdChange::Stale]
        );
        assert!(matches!(
            md.on_book(
                book(110, &[(4600, 1)], &[(4500, 1)], None),
                Px(100),
                Qty(100)
            )[0],
            MdChange::Invalid(_)
        ));
        assert_eq!(md.book.as_ref().unwrap().best_bid().unwrap().0, Px(4000));
    }

    #[test]
    fn book_vwap_weights_included_levels_by_size() {
        assert_eq!(BookView::default().vwap(3), None);
        let view = BookView {
            bids: vec![(Px(4000), Qty(10_000)), (Px(3000), Qty(10_000))],
            offers: vec![(Px(6000), Qty(20_000))],
        };
        let (px, qty) = view.vwap(3).unwrap();
        assert!((px - 0.475).abs() < 1e-9, "{px}");
        assert!((qty - 4.0).abs() < 1e-9, "{qty}");
    }

    #[test]
    fn book_vwap_limits_each_side_independently_and_ignores_deep_size() {
        let view = BookView {
            bids: vec![
                (Px(4500), Qty(10_000)),
                (Px(4000), Qty(10_000)),
                (Px(3500), Qty(10_000)),
                (Px(100), Qty(1_000_000)),
            ],
            offers: vec![
                (Px(5500), Qty(10_000)),
                (Px(6000), Qty(10_000)),
                (Px(6500), Qty(20_000)),
                (Px(9900), Qty(10_000)),
            ],
        };
        let (px, qty) = view.vwap(3).unwrap();
        assert!((px - 3.65 / 7.0).abs() < 1e-9, "{px}");
        assert_eq!(qty, 7.0);
        assert_eq!(view.vwap(1), Some((0.5, 2.0)));
        assert_eq!(view.vwap(0), view.vwap(5));
        assert_eq!(view.vwap(0).unwrap().1, 108.0);
        let one_sided = BookView {
            bids: view.bids.clone(),
            offers: vec![],
        };
        let (px, qty) = one_sided.vwap(3).unwrap();
        assert!((px - 0.4).abs() < 1e-9, "{px}");
        assert_eq!(qty, 3.0);
    }

    #[test]
    fn sweep_groups_frames_and_missed_volume() {
        let mut md = MarketData::default();
        md.on_book(
            book(100, &[(4000, 10_000)], &[(4500, 10_000)], Some(0)),
            Px(100),
            Qty(100),
        );
        md.on_trade(trade("a", 200, 4500, 10_000, Side::Buy), 1);
        md.on_trade(trade("b", 200, 4600, 5_000, Side::Buy), 1);
        assert_eq!(md.sweeps.len(), 1);
        assert_eq!(md.sweeps[0].qty, Qty(15_000));
        assert_eq!(md.sweeps[0].worst_px, Px(4600));
        // Venue reports 5 contracts more than we saw, on two books in a row.
        let c1 = md.on_book(
            book(300, &[(4000, 10_000)], &[(4700, 1_000)], Some(65_000)),
            Px(100),
            Qty(100),
        );
        assert!(!c1.iter().any(|c| matches!(c, MdChange::MissedVolume(_))));
        let c2 = md.on_book(
            book(400, &[(4000, 10_000)], &[(4700, 1_000)], Some(65_000)),
            Px(100),
            Qty(100),
        );
        assert!(c2.contains(&MdChange::MissedVolume(Qty(50_000))));
    }
}
