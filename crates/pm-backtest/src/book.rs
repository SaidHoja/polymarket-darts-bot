//! Order-by-order (L3) book rebuilt from FIX rows.
//!
//! Trades don't touch the book: the feed sends change/delete rows for the
//! passive orders a trade consumes, in the same message. A size decrease keeps
//! queue priority; a size increase or price change sends the order to the back.

use std::collections::{BTreeMap, HashMap};

use pm_types::{Px, Qty, Side};
use serde::Serialize;

use crate::data::{Kind, Message, Row};

/// Aggregated (bids, offers), best first.
pub type L2 = (Vec<(Px, Qty)>, Vec<(Px, Qty)>);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resting {
    pub side: Side,
    pub px: Px,
    pub qty: Qty,
    pub prio: u64,
}

#[derive(Debug, Default, Clone)]
pub struct L3Book {
    orders: HashMap<u32, Resting>,
    /// price → priority → order id
    bids: BTreeMap<Px, BTreeMap<u64, u32>>,
    offers: BTreeMap<Px, BTreeMap<u64, u32>>,
    next_prio: u64,
}

/// What a book row did to an existing order, for queue tracking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Effect {
    Added,
    /// Same price, size not increased: keeps its place.
    SizeDown(Qty),
    /// Size increase or price change: back of the (new) queue.
    Requeued,
    Removed,
    /// Change or delete for an order we don't have.
    Unknown,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Quality {
    pub messages: u64,
    pub snapshots: u64,
    pub trades: u64,
    /// Trades whose price had resting liquidity on the passive side when the message began.
    pub trades_at_book: u64,
    pub trades_no_aggressor: u64,
    /// Re-snapshots within a feed session whose L2 differed from the rebuilt book…
    pub snapshot_mismatches: u64,
    /// …of which the best bid or offer (price or size) differed.
    pub snapshot_touch_mismatches: u64,
    /// Re-snapshots after a `msg_seq` reset (reconnect): differences there are expected.
    pub snapshots_after_reset: u64,
    /// Messages after which the rebuilt book was crossed.
    pub crossed: u64,
    pub unknown_changes: u64,
    /// Gaps of more than 60 s between messages (quiet market or missing data: indistinguishable).
    pub gaps_over_60s: u64,
}

impl Quality {
    pub fn trade_agreement(&self) -> f64 {
        let checked = self.trades - self.trades_no_aggressor;
        if checked == 0 {
            1.0
        } else {
            self.trades_at_book as f64 / checked as f64
        }
    }
}

impl L3Book {
    fn side_mut(&mut self, side: Side) -> &mut BTreeMap<Px, BTreeMap<u64, u32>> {
        if side == Side::Buy {
            &mut self.bids
        } else {
            &mut self.offers
        }
    }

    fn side_ref(&self, side: Side) -> &BTreeMap<Px, BTreeMap<u64, u32>> {
        if side == Side::Buy {
            &self.bids
        } else {
            &self.offers
        }
    }

    pub fn get(&self, id: u32) -> Option<&Resting> {
        self.orders.get(&id)
    }

    fn insert(&mut self, id: u32, side: Side, px: Px, qty: Qty) {
        self.next_prio += 1;
        let prio = self.next_prio;
        self.side_mut(side).entry(px).or_default().insert(prio, id);
        self.orders.insert(
            id,
            Resting {
                side,
                px,
                qty,
                prio,
            },
        );
    }

    fn remove(&mut self, id: u32) -> Option<Resting> {
        let r = self.orders.remove(&id)?;
        let levels = self.side_mut(r.side);
        if let Some(level) = levels.get_mut(&r.px) {
            level.remove(&r.prio);
            if level.is_empty() {
                levels.remove(&r.px);
            }
        }
        Some(r)
    }

    pub fn clear(&mut self) {
        self.orders.clear();
        self.bids.clear();
        self.offers.clear();
    }

    /// Apply one book row (not a trade, marker or snapshot entry).
    pub fn apply(&mut self, row: &Row) -> Effect {
        match row.kind {
            Kind::Add => {
                self.remove(row.id);
                self.insert(row.id, row.side, row.px, row.qty);
                Effect::Added
            }
            Kind::Change => match self.orders.get_mut(&row.id) {
                Some(r) if r.px == row.px && r.side == row.side && row.qty <= r.qty => {
                    r.qty = row.qty;
                    if row.qty <= Qty::ZERO {
                        self.remove(row.id);
                        return Effect::Removed;
                    }
                    Effect::SizeDown(row.qty)
                }
                Some(_) => {
                    self.remove(row.id);
                    self.insert(row.id, row.side, row.px, row.qty);
                    Effect::Requeued
                }
                None => {
                    self.insert(row.id, row.side, row.px, row.qty);
                    Effect::Unknown
                }
            },
            Kind::Delete => match self.remove(row.id) {
                Some(_) => Effect::Removed,
                None => Effect::Unknown,
            },
            Kind::Marker | Kind::Snap | Kind::Trade => Effect::Unknown,
        }
    }

    /// Replace the whole book with a snapshot's entries (in their priority order).
    pub fn load_snapshot(&mut self, entries: &[Row]) {
        self.clear();
        for r in entries.iter().filter(|r| r.kind == Kind::Snap) {
            self.insert(r.id, r.side, r.px, r.qty);
        }
    }

    pub fn best(&self, side: Side) -> Option<(Px, Qty)> {
        let level = if side == Side::Buy {
            self.bids.iter().next_back()
        } else {
            self.offers.iter().next()
        };
        level.map(|(px, l)| {
            (
                *px,
                l.values()
                    .map(|id| self.orders[id].qty)
                    .fold(Qty::ZERO, |a, b| Qty(a.0 + b.0)),
            )
        })
    }

    pub fn mid(&self) -> Option<f64> {
        let (b, a) = (self.best(Side::Buy)?, self.best(Side::Sell)?);
        Some((b.0.as_f64() + a.0.as_f64()) / 2.0)
    }

    /// Orders at one price, front of the queue first.
    pub fn level(&self, side: Side, px: Px) -> Vec<(u32, Qty)> {
        self.side_ref(side)
            .get(&px)
            .map(|l| l.values().map(|id| (*id, self.orders[id].qty)).collect())
            .unwrap_or_default()
    }

    pub fn has_level(&self, side: Side, px: Px) -> bool {
        self.side_ref(side).contains_key(&px)
    }

    /// Aggregated levels, best first, `depth` per side.
    pub fn l2(&self, depth: usize) -> L2 {
        let agg = |(px, l): (&Px, &BTreeMap<u64, u32>)| {
            (*px, Qty(l.values().map(|id| self.orders[id].qty.0).sum()))
        };
        (
            self.bids.iter().rev().take(depth).map(agg).collect(),
            self.offers.iter().take(depth).map(agg).collect(),
        )
    }

    /// Visible levels on one side, best first, for walking a take.
    pub fn levels(&self, side: Side) -> Vec<(Px, Qty)> {
        let (b, o) = self.l2(usize::MAX);
        if side == Side::Buy {
            b
        } else {
            o
        }
    }

    pub fn crossed(&self) -> bool {
        matches!((self.best(Side::Buy), self.best(Side::Sell)), (Some(b), Some(a)) if b.0 >= a.0)
    }
}

/// Replay a whole market through the book without a strategy: data quality and
/// the historical mid timeline (for markouts).
pub fn audit(rows: &[Row], messages: &[Message]) -> (Quality, Vec<(i64, f64)>) {
    let mut book = L3Book::default();
    let mut q = Quality::default();
    let mut mids = Vec::new();
    let mut seeded = false;
    let mut last_rx: Option<i64> = None;
    let mut last_seq: Option<i64> = None;
    for m in messages {
        q.messages += 1;
        if last_rx.is_some_and(|t| m.rx_ns - t > 60_000_000_000) {
            q.gaps_over_60s += 1;
        }
        last_rx = Some(m.rx_ns);
        let reset = last_seq.is_some_and(|s| rows[m.start].seq < s);
        last_seq = Some(rows[m.start].seq);
        let rows = &rows[m.start..m.end];
        if m.snapshot {
            q.snapshots += 1;
            if seeded && reset {
                q.snapshots_after_reset += 1;
            } else if seeded {
                let mut fresh = L3Book::default();
                fresh.load_snapshot(rows);
                if fresh.l2(usize::MAX) != book.l2(usize::MAX) {
                    q.snapshot_mismatches += 1;
                    if fresh.best(Side::Buy) != book.best(Side::Buy)
                        || fresh.best(Side::Sell) != book.best(Side::Sell)
                    {
                        q.snapshot_touch_mismatches += 1;
                    }
                }
            }
            book.load_snapshot(rows);
            seeded = true;
        } else {
            for r in rows.iter().filter(|r| r.kind == Kind::Trade) {
                q.trades += 1;
                if r.aggressor == Side::Unknown {
                    q.trades_no_aggressor += 1;
                } else if book.has_level(r.aggressor.opposite(), r.px) {
                    q.trades_at_book += 1;
                }
            }
            for r in rows
                .iter()
                .filter(|r| matches!(r.kind, Kind::Add | Kind::Change | Kind::Delete))
            {
                if book.apply(r) == Effect::Unknown {
                    q.unknown_changes += 1;
                }
            }
        }
        if book.crossed() {
            q.crossed += 1;
        }
        if let Some(mid) = book.mid() {
            if mids.last().is_none_or(|(_, last)| *last != mid) {
                mids.push((m.ev_ns, mid));
            }
        }
    }
    (q, mids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{messages, parse_rows};

    fn row(kind: Kind, id: u32, side: Side, px: i64, qty: i64) -> Row {
        Row {
            rx_ns: 0,
            ev_ns: 0,
            seq: 0,
            kind,
            side,
            px: Px(px),
            qty: Qty(qty),
            id,
            aggressor: Side::Unknown,
        }
    }

    #[test]
    fn priority_kept_on_size_down_lost_on_size_up_or_price_change() {
        let mut b = L3Book::default();
        b.apply(&row(Kind::Add, 1, Side::Buy, 5000, 100));
        b.apply(&row(Kind::Add, 2, Side::Buy, 5000, 100));
        assert_eq!(
            b.apply(&row(Kind::Change, 1, Side::Buy, 5000, 50)),
            Effect::SizeDown(Qty(50))
        );
        assert_eq!(
            b.level(Side::Buy, Px(5000)),
            vec![(1, Qty(50)), (2, Qty(100))]
        );
        assert_eq!(
            b.apply(&row(Kind::Change, 1, Side::Buy, 5000, 80)),
            Effect::Requeued
        );
        assert_eq!(
            b.level(Side::Buy, Px(5000)),
            vec![(2, Qty(100)), (1, Qty(80))]
        );
        assert_eq!(
            b.apply(&row(Kind::Change, 2, Side::Buy, 4900, 100)),
            Effect::Requeued
        );
        assert_eq!(b.best(Side::Buy), Some((Px(5000), Qty(80))));
        assert_eq!(
            b.apply(&row(Kind::Delete, 1, Side::Buy, 5000, 0)),
            Effect::Removed
        );
        assert_eq!(b.best(Side::Buy), Some((Px(4900), Qty(100))));
        assert_eq!(
            b.apply(&row(Kind::Delete, 9, Side::Buy, 5000, 0)),
            Effect::Unknown
        );
    }

    #[test]
    fn audit_counts_trades_at_book_and_snapshot_agreement() {
        let csv = "rx_ns,ev_ns,msg_seq,entry_index,kind,side,price,size,id,aggressor
100,90,7,-1,M,,,,,
100,90,7,0,S,b,0.6,300,A1,
100,90,7,1,S,o,0.65,10,A2,
200,195,8,0,U,b,0.6,250,A1,
200,195,8,1,T,,0.6,50,T1,s
250,245,9,0,T,,0.55,1,T2,s
300,290,10,-1,M,,,,,
300,290,10,0,S,b,0.6,250,A1,
300,290,10,1,S,o,0.65,10,A2,
";
        let rows = parse_rows(csv).unwrap();
        let (q, mids) = audit(&rows, &messages(&rows));
        assert_eq!(
            (
                q.trades,
                q.trades_at_book,
                q.snapshots,
                q.snapshot_mismatches,
                q.crossed
            ),
            (2, 1, 2, 0, 0)
        );
        assert!((q.trade_agreement() - 0.5).abs() < 1e-9);
        assert_eq!(mids, vec![(90, 0.625)]);
    }
}
