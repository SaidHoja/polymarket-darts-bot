//! Positions, cash, fees and PnL from our own fill accounting. Venue position
//! reports are cross-checks.

use std::collections::{BTreeMap, VecDeque};

use pm_types::{BalanceFrame, PositionFrame, Px, Qty, Side, Slug};
use serde::Serialize;

use crate::orders::FillRec;

/// Fills kept for the operator UI, newest last.
const FILL_LOG: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Position {
    /// Signed net YES contracts.
    pub qty: Qty,
    /// Net cash paid for the position (buys − sells), excluding fees.
    pub cost: f64,
    pub fees: f64,
}

impl Position {
    pub fn apply(&mut self, side: Side, qty: Qty, px: Px, fee: f64) {
        let signed = Qty(qty.0 * side.sign());
        self.qty += signed;
        self.cost += signed.as_f64() * px.as_f64();
        self.fees += fee;
    }

    /// PnL if the market settles at `value` (1, 0 or 0.5).
    pub fn pnl_at(&self, value: f64) -> f64 {
        self.qty.as_f64() * value - self.cost - self.fees
    }

    /// Marked PnL at a price.
    pub fn marked(&self, mark: f64) -> f64 {
        self.pnl_at(mark)
    }
}

/// Resting orders in one market, for worst-case exposure.
#[derive(Debug, Clone, Copy, Default)]
pub struct Resting {
    pub bid_qty: f64,
    pub bid_cash: f64,
    pub ask_qty: f64,
    pub ask_cash: f64,
}

impl Resting {
    pub fn add(&mut self, side: Side, qty: Qty, px: Px) {
        match side {
            Side::Buy => {
                self.bid_qty += qty.as_f64();
                self.bid_cash += qty.notional(px);
            }
            Side::Sell => {
                self.ask_qty += qty.as_f64();
                self.ask_cash += qty.notional(px);
            }
            Side::Unknown => {}
        }
    }
}

/// Worst loss over YES/NO settlement with all bids filling, or all asks filling, or neither.
/// Settlement at 0.5 (cancelled match) is included.
pub fn worst_case_loss(pos: &Position, resting: &Resting) -> f64 {
    let scenarios = [
        (pos.qty.as_f64(), pos.cost),
        (
            pos.qty.as_f64() + resting.bid_qty,
            pos.cost + resting.bid_cash,
        ),
        (
            pos.qty.as_f64() - resting.ask_qty,
            pos.cost - resting.ask_cash,
        ),
    ];
    let mut worst = 0.0f64;
    for (q, cost) in scenarios {
        for value in [0.0, 0.5, 1.0] {
            worst = worst.max(-(q * value - cost - pos.fees));
        }
    }
    worst
}

/// One of our fills, for the operator UI. The journal has the full record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FillLog {
    pub wall_ns: i64,
    pub slug: Slug,
    pub side: Side,
    pub qty: f64,
    pub px: f64,
    pub fee: f64,
    pub aggressor: bool,
    /// Signed net YES contracts after this fill.
    pub position_after: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mismatch {
    pub slug: Slug,
    pub local: Qty,
    pub venue: Qty,
}

#[derive(Debug, Default)]
pub struct Portfolio {
    pub positions: BTreeMap<Slug, Position>,
    pub balance: Option<BalanceFrame>,
    /// Whether any venue position snapshot has been adopted yet.
    seeded: bool,
    /// Differences seen once and not adopted yet: (local, venue) per market.
    unconfirmed: BTreeMap<Slug, (Qty, Qty)>,
    pub fills: u64,
    /// The last `FILL_LOG` fills, oldest first.
    pub recent_fills: VecDeque<FillLog>,
    /// UTC day of the daily loss figure.
    pub day: Option<i64>,
    /// PnL already made on `day` before this session (restored at startup).
    pub day_carry: f64,
    /// Marked total the day is measured from. Taken once venue positions are known, so positions
    /// inherited at startup don't count as today's PnL.
    day_base: Option<f64>,
    /// The daily PnL as last computed.
    pub daily: f64,
}

impl Portfolio {
    pub fn position(&self, slug: &str) -> Position {
        self.positions.get(slug).copied().unwrap_or_default()
    }

    /// Apply one of our fills. The buying-power mirror moves by the change in the position's worst
    /// case: exact for opening trades, never more than the venue's figure for closing ones, until the
    /// next balance query replaces it.
    pub fn on_fill(&mut self, slug: &Slug, side: Side, qty: Qty, px: Px, fee: f64) {
        let pos = self.positions.entry(slug.clone()).or_default();
        let before = worst_case_loss(pos, &Resting::default());
        pos.apply(side, qty, px, fee);
        let after = worst_case_loss(pos, &Resting::default());
        if let Some(b) = self.balance.as_mut() {
            b.buying_power -= after - before;
        }
        self.fills += 1;
    }

    /// Remember a fill for the UI (after `on_fill`, so `position_after` includes it).
    pub fn log_fill(&mut self, wall_ns: i64, f: &FillRec, fee: f64) {
        if self.recent_fills.len() == FILL_LOG {
            self.recent_fills.pop_front();
        }
        let position_after = self.position(&f.slug).qty.as_f64();
        self.recent_fills.push_back(FillLog {
            wall_ns,
            slug: f.slug.clone(),
            side: f.side,
            qty: f.qty.as_f64(),
            px: f.px.as_f64(),
            fee,
            aggressor: f.aggressor,
            position_after,
        });
    }

    /// Close a resolved market's position at its YES settlement price, fee-free, as the venue does.
    /// Returns the quantity closed, if there was any. Buying power changes too: re-read the balance.
    pub fn settle(&mut self, slug: &str, px: Px) -> Option<Qty> {
        let pos = self.positions.get_mut(slug).filter(|p| !p.qty.is_zero())?;
        let qty = pos.qty;
        let side = if qty > Qty::ZERO {
            Side::Sell
        } else {
            Side::Buy
        };
        pos.apply(side, qty.abs(), px, 0.0);
        Some(qty)
    }

    /// Venue positions (REST). The first snapshot is adopted as the starting state; later ones are
    /// compared. A difference ≥ `min_qty` is adopted, as a reconstructed fill at `mark`, only when the
    /// next read shows the same one with the local position unchanged: a read can include a fill
    /// whose execution hasn't arrived yet (or the other way round), and adopting that would count
    /// the fill twice. Returns (adopted, held for a re-read). Markets for which `skip` is true are left
    /// alone (a resolution whose price isn't known yet).
    pub fn on_venue_positions(
        &mut self,
        venue: &[PositionFrame],
        min_qty: Qty,
        mark: impl Fn(&str) -> f64,
        skip: impl Fn(&str) -> bool,
    ) -> (Vec<Mismatch>, Vec<Mismatch>) {
        let (mut out, mut held) = (Vec::new(), Vec::new());
        let venue_map: BTreeMap<&str, &PositionFrame> =
            venue.iter().map(|p| (&*p.slug, p)).collect();
        if !self.seeded {
            self.seeded = true;
            for p in venue {
                if p.net.is_zero() {
                    continue;
                }
                let cost = p.cost.unwrap_or(p.net.as_f64() * 0.5);
                self.positions.insert(
                    p.slug.clone(),
                    Position {
                        qty: p.net,
                        cost,
                        fees: 0.0,
                    },
                );
            }
            return (out, held);
        }
        let previous = std::mem::take(&mut self.unconfirmed);
        let mut slugs: Vec<Slug> = self.positions.keys().cloned().collect();
        slugs.extend(venue.iter().map(|p| p.slug.clone()));
        slugs.sort();
        slugs.dedup();
        for slug in slugs.into_iter().filter(|s| !skip(s)) {
            let local = self.position(&slug).qty;
            let remote = venue_map.get(&*slug).map(|p| p.net).unwrap_or(Qty::ZERO);
            let diff = remote - local;
            if diff.abs() < min_qty.max(Qty(1)) {
                continue;
            }
            if previous.get(&slug) != Some(&(local, remote)) {
                self.unconfirmed.insert(slug.clone(), (local, remote));
                held.push(Mismatch {
                    slug,
                    local,
                    venue: remote,
                });
                continue;
            }
            let side = if diff > Qty::ZERO {
                Side::Buy
            } else {
                Side::Sell
            };
            self.positions.entry(slug.clone()).or_default().apply(
                side,
                diff.abs(),
                Px::from_f64(mark(&slug)),
                0.0,
            );
            out.push(Mismatch {
                slug,
                local,
                venue: remote,
            });
        }
        (out, held)
    }

    pub fn total_marked(&self, mark: impl Fn(&str) -> Option<f64>) -> f64 {
        self.positions
            .iter()
            .map(|(slug, p)| {
                let fallback = if p.qty.is_zero() {
                    0.5
                } else {
                    (p.cost / p.qty.as_f64()).clamp(0.0, 1.0)
                };
                p.marked(mark(slug).unwrap_or(fallback))
            })
            .sum()
    }

    /// Daily PnL: what was carried into the UTC day, plus the change in the marked total since the
    /// first mark after venue positions were seeded.
    pub fn daily_pnl(&mut self, total_marked: f64, wall_ns: i64) -> f64 {
        let day = wall_ns.div_euclid(86_400 * 1_000_000_000);
        if self.day != Some(day) {
            self.day = Some(day);
            self.day_carry = 0.0;
            self.day_base = None;
        }
        if self.day_base.is_none() && self.seeded {
            self.day_base = Some(total_marked);
        }
        self.daily = self.day_carry + self.day_base.map_or(0.0, |base| total_marked - base);
        self.daily
    }

    /// Continue `day` from `pnl`, made before this session.
    pub fn carry_day(&mut self, day: i64, pnl: f64) {
        self.day = Some(day);
        self.day_carry = pnl;
        self.day_base = None;
        self.daily = pnl;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn worst_case_counts_resting_orders() {
        let mut pos = Position::default();
        pos.apply(Side::Buy, Qty(100_000), Px(4000), 0.0); // long 10 @ 0.40
        assert!((worst_case_loss(&pos, &Resting::default()) - 4.0).abs() < 1e-9);
        let mut resting = Resting::default();
        resting.add(Side::Buy, Qty(50_000), Px(3800)); // +5 @ 0.38 if bids fill
        resting.add(Side::Sell, Qty(100_000), Px(4500)); // asks closing the long only reduce risk
        let w = worst_case_loss(&pos, &resting);
        assert!((w - (4.0 + 1.9)).abs() < 1e-9, "{w}");
        // A short loses 1 − p if YES settles.
        let mut short = Position::default();
        short.apply(Side::Sell, Qty(10_000), Px(3000), 0.0);
        assert!((worst_case_loss(&short, &Resting::default()) - 0.7).abs() < 1e-9);
    }

    #[test]
    fn fills_move_the_buying_power_mirror() {
        let mut p = Portfolio {
            balance: Some(BalanceFrame {
                current_balance: 10.0,
                buying_power: 10.0,
                open_orders: 0.0,
            }),
            ..Default::default()
        };
        let slug: Slug = Arc::from("m");
        let bp = |p: &Portfolio| p.balance.as_ref().unwrap().buying_power;
        p.on_fill(&slug, Side::Buy, Qty(20_000), Px(4000), 0.0); // buy 2 @ 0.40: −0.80
        assert!((bp(&p) - 9.2).abs() < 1e-9, "{}", bp(&p));
        p.on_fill(&slug, Side::Sell, Qty(20_000), Px(3500), 0.0); // close at a loss: +0.70 back
        assert!((bp(&p) - 9.9).abs() < 1e-9, "{}", bp(&p));
        p.on_fill(&slug, Side::Sell, Qty(10_000), Px(3000), 0.0); // short 1 @ 0.30: −0.70
        assert!((bp(&p) - 9.2).abs() < 1e-9, "{}", bp(&p));
    }

    #[test]
    fn venue_positions_seed_then_reconcile() {
        let mut p = Portfolio::default();
        let slug: Slug = Arc::from("m");
        let v = |q| {
            vec![PositionFrame {
                slug: slug.clone(),
                net: Qty(q),
                cost: Some(1.0),
                resolution: false,
            }]
        };
        let read = |p: &mut Portfolio, q| p.on_venue_positions(&v(q), Qty(100), |_| 0.5, |_| false);
        assert_eq!(read(&mut p, 20_000), (vec![], vec![]));
        assert_eq!(p.position("m").qty, Qty(20_000));
        p.on_fill(&slug, Side::Buy, Qty(10_000), Px(5000), 0.0);
        assert_eq!(read(&mut p, 30_000), (vec![], vec![]));
        // A difference is held the first time, and adopted when the next read repeats it.
        let diff = Mismatch {
            slug: slug.clone(),
            local: Qty(30_000),
            venue: Qty(40_000),
        };
        assert_eq!(read(&mut p, 40_000), (vec![], vec![diff.clone()]));
        assert_eq!(p.position("m").qty, Qty(30_000));
        assert_eq!(read(&mut p, 40_000), (vec![diff], vec![]));
        assert_eq!(p.position("m").qty, Qty(40_000));
    }

    #[test]
    fn daily_pnl_starts_after_the_seed_and_carries_across_a_restart() {
        const DAY: i64 = 86_400 * 1_000_000_000;
        let mut p = Portfolio::default();
        // Before venue positions are known, nothing counts: the seed's marked PnL isn't today's.
        assert_eq!(p.daily_pnl(-3.0, DAY), 0.0);
        p.on_venue_positions(&[], Qty(100), |_| 0.5, |_| false);
        assert_eq!(p.daily_pnl(-3.0, DAY), 0.0);
        assert!((p.daily_pnl(-4.5, DAY) + 1.5).abs() < 1e-9);
        // A restart the same day continues from what was lost.
        let mut p = Portfolio::default();
        p.carry_day(1, -1.5);
        assert_eq!(p.daily_pnl(10.0, DAY), -1.5);
        p.on_venue_positions(&[], Qty(100), |_| 0.5, |_| false);
        assert_eq!(p.daily_pnl(10.0, DAY), -1.5);
        assert!((p.daily_pnl(9.0, DAY) + 2.5).abs() < 1e-9);
        // The next UTC day starts from zero.
        assert_eq!(p.daily_pnl(9.0, 2 * DAY), 0.0);
    }

    /// LIVE-04: the venue's read already includes a fill whose execution arrives after it.
    #[test]
    fn a_fill_the_read_got_first_is_not_counted_twice() {
        let mut p = Portfolio::default();
        let slug: Slug = Arc::from("m");
        let v = |q| {
            vec![PositionFrame {
                slug: slug.clone(),
                net: Qty(q),
                cost: Some(1.0),
                resolution: false,
            }]
        };
        let read = |p: &mut Portfolio, q| p.on_venue_positions(&v(q), Qty(100), |_| 0.5, |_| false);
        read(&mut p, 0);
        assert_eq!(read(&mut p, 10_000).1.len(), 1, "held, not adopted");
        p.on_fill(&slug, Side::Buy, Qty(10_000), Px(4000), 0.0);
        assert_eq!(read(&mut p, 10_000), (vec![], vec![]));
        assert_eq!(p.position("m").qty, Qty(10_000));
        // A changed difference starts again rather than adopting the new one at once.
        assert_eq!(read(&mut p, 20_000).1.len(), 1);
        assert_eq!(read(&mut p, 30_000).1.len(), 1);
        assert_eq!(p.position("m").qty, Qty(10_000));
    }
}
