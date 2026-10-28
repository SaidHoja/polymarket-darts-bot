//! Limits, pre-trade checks, pauses and kill switches. See `docs/risk.md`.

use std::collections::{BTreeMap, VecDeque};

use pm_types::{Px, Qty, RejectClass, Side, Slug};
use serde::{Deserialize, Serialize};

use crate::config::{secs, RiskConfig};
use crate::portfolio::{worst_case_loss, Position, Resting};
use crate::strategy::LeagueCheck;

const MINUTE: u64 = 60_000_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum RiskAction {
    CancelMarket(Slug),
    CancelAll,
    QueryBalance,
    RefreshMarket(Slug),
    Alert(String),
}

#[derive(Debug, Clone)]
struct Pause {
    until: Option<u64>,
    reason: String,
}

#[derive(Debug, Default)]
pub struct Risk {
    pub cfg: RiskConfig,
    /// Operator pause of everything.
    pub admin_paused: bool,
    /// Daily loss kill: UTC day it was triggered on.
    pub killed_day: Option<i64>,
    paused: BTreeMap<Slug, Pause>,
    rejects: VecDeque<u64>,
    market_rejects: BTreeMap<Slug, VecDeque<u64>>,
    book_moved: BTreeMap<Slug, VecDeque<u64>>,
    fills: BTreeMap<Slug, VecDeque<u64>>,
    pub orders_blocked_until: u64,
    pub slow_until: u64,
    /// Marked PnL per market when the session first saw it, less any loss carried in.
    market_pnl_start: BTreeMap<Slug, f64>,
    /// PnL per market since it was first seen, as of the last loss check.
    market_pnl: BTreeMap<Slug, f64>,
    /// Losses restored from before this session, for markets not marked yet.
    market_carry: BTreeMap<Slug, f64>,
    pub leagues: BTreeMap<String, LeagueCheck>,
}

/// Risk state that has to survive a restart (stress-test plan D1), or restarting would clear the
/// daily kill and every loss limit. The shell saves it and restores it at startup; timed pauses and
/// rate windows are left out, they only last minutes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RiskState {
    /// UTC day number the daily figures are for.
    pub day: i64,
    /// Realised plus marked PnL on `day`.
    pub daily_pnl: f64,
    /// The daily loss kill fired on `day`.
    pub killed: bool,
    /// Loss per market towards `market_loss_limit` (losses only; a gain isn't carried).
    pub market_loss: BTreeMap<String, f64>,
    /// Indefinite market pauses (operator, market loss limit): slug → reason.
    pub paused: BTreeMap<String, String>,
}

fn push_window(q: &mut VecDeque<u64>, now: u64) -> usize {
    q.push_back(now);
    while q.front().is_some_and(|t| now.saturating_sub(*t) > MINUTE) {
        q.pop_front();
    }
    q.len()
}

/// Inputs to the gate for one market.
pub struct GateCtx<'a> {
    pub slug: &'a Slug,
    pub tick: Px,
    pub min_qty: Qty,
    pub fair: f64,
    pub best_bid: Option<Px>,
    pub best_ask: Option<Px>,
    pub position: &'a Position,
    /// Resting orders in this market that the quotes don't replace (in-doubt orders).
    pub other_resting: Resting,
    /// Worst-case loss of every other market, including what was approved earlier this tick.
    pub account_other: f64,
    /// Collateral other markets' resting orders would take beyond their positions, including what was
    /// approved earlier this tick. The venue checks open orders per market only, so this is ours to share.
    pub committed_other: f64,
    /// Buying power, when known (the venue's figure less fills since, see `Portfolio::on_fill`).
    pub buying_power: Option<f64>,
    /// Active orders outside this market's quote slots.
    pub orders_elsewhere: usize,
    /// This market currently has a working order on that side (bid, ask).
    pub has_working: (bool, bool),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gated {
    pub bid: Option<(Px, Qty)>,
    pub ask: Option<(Px, Qty)>,
    pub vetoes: Vec<&'static str>,
    /// This market's worst-case loss with the approved quotes, for the account reservation.
    pub worst_case: f64,
}

impl Risk {
    pub fn new(cfg: RiskConfig) -> Self {
        Risk {
            cfg,
            ..Default::default()
        }
    }

    /// Pause a market. A pause never weakens one already in force: an indefinite pause (operator,
    /// loss limit) stays until resumed, and a timed one keeps the later expiry.
    pub fn pause(&mut self, slug: &Slug, now: u64, for_s: Option<f64>, reason: impl Into<String>) {
        let until = for_s.map(|s| now + secs(s));
        if let Some(p) = self.paused.get_mut(slug) {
            if p.until.is_none_or(|u| now < u) {
                if let Some(current) = p.until {
                    if until.is_none_or(|u| u > current) {
                        *p = Pause {
                            until,
                            reason: reason.into(),
                        };
                    }
                }
                return;
            }
        }
        self.paused.insert(
            slug.clone(),
            Pause {
                until,
                reason: reason.into(),
            },
        );
    }

    /// Whether a timed block (market pause, order block, slow exchange) ended in `(from, to]`.
    pub fn expired_between(&self, from: u64, to: u64) -> bool {
        let ended = |u: u64| u > from && u <= to;
        ended(self.orders_blocked_until)
            || ended(self.slow_until)
            || self.paused.values().any(|p| p.until.is_some_and(ended))
    }

    pub fn resume(&mut self, slug: Option<&str>) {
        match slug {
            Some(s) => {
                self.paused.remove(s);
            }
            None => {
                self.paused.clear();
                self.admin_paused = false;
            }
        }
    }

    pub fn paused_reason(&mut self, slug: &str, now: u64, day: i64) -> Option<String> {
        if self.admin_paused {
            return Some("operator pause".into());
        }
        if self.killed_day == Some(day) {
            return Some("daily loss limit".into());
        }
        if now < self.orders_blocked_until {
            return Some("reject storm".into());
        }
        if let Some(p) = self.paused.get(slug) {
            if p.until.is_none_or(|u| now < u) {
                return Some(p.reason.clone());
            }
            self.paused.remove(slug);
        }
        None
    }

    /// A retired market's rate windows and expired pause. Indefinite and unexpired pauses stay, so a
    /// relisted market is still paused; so does the loss-limit baseline, which goes with its position.
    pub fn forget_market(&mut self, slug: &str, now: u64) {
        self.market_rejects.remove(slug);
        self.book_moved.remove(slug);
        self.fills.remove(slug);
        self.market_pnl.remove(slug);
        if self
            .paused
            .get(slug)
            .is_some_and(|p| p.until.is_some_and(|u| now >= u))
        {
            self.paused.remove(slug);
        }
    }

    /// This market's own pause reason, without expiring it (for display).
    pub fn market_pause(&self, slug: &str, now: u64) -> Option<&str> {
        self.paused
            .get(slug)
            .filter(|p| p.until.is_none_or(|u| now < u))
            .map(|p| p.reason.as_str())
    }

    pub fn league(&self, league: &str, preset_verified: bool) -> LeagueCheck {
        match self.leagues.get(league) {
            Some(c) => *c,
            None if preset_verified => LeagueCheck::Verified,
            None => LeagueCheck::Unverified,
        }
    }

    pub fn on_orientation(&mut self, league: &str, agrees: bool) -> Vec<RiskAction> {
        let current = self
            .leagues
            .get(league)
            .copied()
            .unwrap_or(LeagueCheck::Unverified);
        if current == LeagueCheck::Failed {
            return Vec::new();
        }
        if agrees {
            self.leagues
                .insert(league.to_string(), LeagueCheck::Verified);
            vec![RiskAction::Alert(format!(
                "score orientation verified for league {league}"
            ))]
        } else {
            self.leagues.insert(league.to_string(), LeagueCheck::Failed);
            vec![RiskAction::Alert(format!(
                "score orientation check FAILED for league {league}: in-play disabled"
            ))]
        }
    }

    pub fn on_reject(&mut self, slug: &Slug, class: RejectClass, now: u64) -> Vec<RiskAction> {
        let mut out = Vec::new();
        match class {
            RejectClass::BookMoved => {
                let n = push_window(self.book_moved.entry(slug.clone()).or_default(), now);
                if n > self.cfg.max_book_moved_per_market_per_min {
                    self.pause(
                        slug,
                        now,
                        Some(self.cfg.market_pause_s),
                        "repeated post-only rejects",
                    );
                    out.push(RiskAction::CancelMarket(slug.clone()));
                }
                return out; // normal: doesn't count towards the storm limit
            }
            RejectClass::Transient => {
                self.slow_until = now + secs(self.cfg.slow_exchange_s);
                out.push(RiskAction::Alert(
                    "exchange latency stopgap (Global Rate Limit Exceeded)".into(),
                ));
            }
            RejectClass::OurBug => {
                self.pause(
                    slug,
                    now,
                    Some(self.cfg.market_pause_s),
                    "order rejected as invalid",
                );
                out.push(RiskAction::RefreshMarket(slug.clone()));
                out.push(RiskAction::Alert(format!(
                    "{slug}: invalid order rejected; refreshing market parameters"
                )));
            }
            RejectClass::MarketUnavailable => {
                self.pause(
                    slug,
                    now,
                    Some(self.cfg.market_pause_s),
                    "market unavailable",
                );
                out.push(RiskAction::CancelMarket(slug.clone()));
            }
            RejectClass::Funding => {
                self.pause(
                    slug,
                    now,
                    Some(self.cfg.reject_pause_s),
                    "insufficient buying power",
                );
                out.push(RiskAction::QueryBalance);
            }
            RejectClass::Fatal => {
                self.orders_blocked_until = u64::MAX;
                out.push(RiskAction::Alert(
                    "authentication failed: order sending stopped".into(),
                ));
            }
            RejectClass::RateLimited => {}
            RejectClass::Unknown => {
                let n = push_window(self.market_rejects.entry(slug.clone()).or_default(), now);
                if n >= 3 {
                    self.pause(slug, now, Some(self.cfg.market_pause_s), "unknown rejects");
                }
            }
        }
        if push_window(&mut self.rejects, now) > self.cfg.max_rejects_per_min {
            self.orders_blocked_until = self
                .orders_blocked_until
                .max(now + secs(self.cfg.reject_pause_s));
            out.push(RiskAction::Alert("reject storm: new orders paused".into()));
        }
        out
    }

    pub fn on_fill(&mut self, slug: &Slug, now: u64) -> Vec<RiskAction> {
        if push_window(self.fills.entry(slug.clone()).or_default(), now)
            > self.cfg.max_fills_per_market_per_min
        {
            self.pause(slug, now, Some(self.cfg.fill_pause_s), "fill rate limit");
            return vec![
                RiskAction::CancelMarket(slug.clone()),
                RiskAction::Alert(format!("{slug}: fill rate limit hit")),
            ];
        }
        Vec::new()
    }

    /// Daily and per-market loss limits, checked on housekeeping.
    pub fn check_losses(
        &mut self,
        daily_pnl: f64,
        day: i64,
        market_pnl: &[(Slug, f64)],
        now: u64,
    ) -> Vec<RiskAction> {
        let mut out = Vec::new();
        if daily_pnl < -self.cfg.daily_loss_limit && self.killed_day != Some(day) {
            self.killed_day = Some(day);
            out.push(RiskAction::CancelAll);
            out.push(RiskAction::Alert(format!(
                "daily loss limit: {daily_pnl:.2} — everything cancelled until the next UTC day"
            )));
        }
        for (slug, pnl) in market_pnl {
            let start = *self
                .market_pnl_start
                .entry(slug.clone())
                .or_insert_with(|| pnl - self.market_carry.remove(slug).unwrap_or(0.0));
            self.market_pnl.insert(slug.clone(), pnl - start);
            // A timed pause in force doesn't stop the indefinite loss pause from being applied.
            let indefinitely_paused = self.paused.get(slug).is_some_and(|p| p.until.is_none());
            if pnl - start < -self.cfg.market_loss_limit && !indefinitely_paused {
                self.pause(slug, now, None, "market loss limit");
                out.push(RiskAction::CancelMarket(slug.clone()));
                out.push(RiskAction::Alert(format!(
                    "{slug}: market loss limit ({:.2})",
                    pnl - start
                )));
            }
        }
        out
    }

    /// Per-market losses (measured, or carried and not marked yet) and indefinite pauses, for `RiskState`.
    pub fn persisted(&self) -> (BTreeMap<String, f64>, BTreeMap<String, String>) {
        let losses = self
            .market_carry
            .iter()
            .chain(&self.market_pnl)
            .filter(|(_, pnl)| **pnl < 0.0)
            .map(|(slug, pnl)| (slug.to_string(), *pnl))
            .collect();
        let paused = self
            .paused
            .iter()
            .filter(|(_, p)| p.until.is_none())
            .map(|(slug, p)| (slug.to_string(), p.reason.clone()))
            .collect();
        (losses, paused)
    }

    /// Take back what `persisted` saved. Losses count from the market's first mark this session.
    pub fn restore(&mut self, losses: &BTreeMap<String, f64>, paused: &BTreeMap<String, String>) {
        for (slug, pnl) in losses {
            self.market_carry.insert(Slug::from(slug.as_str()), *pnl);
        }
        for (slug, reason) in paused {
            self.paused.insert(
                Slug::from(slug.as_str()),
                Pause {
                    until: None,
                    reason: reason.clone(),
                },
            );
        }
    }

    /// Pre-trade checks and limits for one market's quotes.
    pub fn gate(&self, ctx: &GateCtx, bid: Option<(Px, Qty)>, ask: Option<(Px, Qty)>) -> Gated {
        let mut g = Gated::default();
        let max_order = Qty::from_f64(self.cfg.max_order_size);
        let max_pos = Qty::from_f64(self.cfg.max_position);
        let check =
            |side: Side, px: Px, qty: Qty, vetoes: &mut Vec<&'static str>| -> Option<(Px, Qty)> {
                if !px.is_on_tick(ctx.tick) || px < Px::MIN || px > Px::MAX {
                    vetoes.push("price off tick or out of range");
                    return None;
                }
                if (px.as_f64() - ctx.fair).abs() > self.cfg.max_dev_from_fair {
                    vetoes.push("price too far from fair");
                    return None;
                }
                let band = Px::from_f64(self.cfg.book_band);
                let lo = ctx.best_bid.map(|b| Px(b.0 - band.0));
                let hi = ctx.best_ask.map(|a| Px(a.0 + band.0));
                if lo.is_some_and(|lo| px < lo) && side == Side::Sell
                    || hi.is_some_and(|hi| px > hi) && side == Side::Buy
                {
                    vetoes.push("price outside the book band");
                    return None;
                }
                let mut q = qty.min(max_order);
                q = match side {
                    Side::Buy => q.min(max_pos - ctx.position.qty),
                    _ => q.min(max_pos + ctx.position.qty),
                };
                let q = q.round_down(ctx.min_qty);
                if q <= Qty::ZERO {
                    vetoes.push("position limit");
                    return None;
                }
                Some((px, q))
            };
        let mut b = bid.and_then(|(p, q)| check(Side::Buy, p, q, &mut g.vetoes));
        let mut a = ask.and_then(|(p, q)| check(Side::Sell, p, q, &mut g.vetoes));
        if let (Some(bb), Some(aa)) = (b, a) {
            if bb.0 >= aa.0 {
                g.vetoes.push("self-cross");
                a = None;
            }
        }
        // Order count.
        let new_orders = [
            b.is_some() && !ctx.has_working.0,
            a.is_some() && !ctx.has_working.1,
        ]
        .iter()
        .filter(|x| **x)
        .count();
        let current =
            ctx.orders_elsewhere + ctx.has_working.0 as usize + ctx.has_working.1 as usize;
        if current + new_orders > self.cfg.max_orders_total {
            g.vetoes.push("order count limit");
            if !ctx.has_working.1 {
                a = None;
            }
            if !ctx.has_working.0 {
                b = None;
            }
        }
        // Worst-case loss: per market, then account. Buying power must cover every market's resting
        // orders together; the position's own collateral is already out of it.
        let position_worst = worst_case_loss(ctx.position, &Resting::default());
        let worst = |b: Option<(Px, Qty)>, a: Option<(Px, Qty)>| {
            let mut r = ctx.other_resting;
            if let Some((p, q)) = b {
                r.add(Side::Buy, q, p);
            }
            if let Some((p, q)) = a {
                r.add(Side::Sell, q, p);
            }
            worst_case_loss(ctx.position, &r)
        };
        let fits = |b, a| {
            let w = worst(b, a);
            w <= self.cfg.max_market_loss + 1e-9
                && ctx.account_other + w <= self.cfg.max_account_loss + 1e-9
                && ctx.buying_power.is_none_or(|bp| {
                    ctx.committed_other + (w - position_worst) <= bp.max(0.0) + 1e-9
                })
        };
        let base = worst(None, None);
        // Clip each side to the largest size that fits, bid first. Risk-reducing quotes always fit.
        for side in [Side::Buy, Side::Sell] {
            let current = if side == Side::Buy { b } else { a };
            let Some((px, q)) = current else { continue };
            let with = |qty: Qty| {
                if side == Side::Buy {
                    (Some((px, qty)), a)
                } else {
                    (b, Some((px, qty)))
                }
            };
            let (wb, wa) = with(q);
            if fits(wb, wa) || worst(wb, wa) <= base + 1e-9 {
                continue;
            }
            let steps = q.0 / ctx.min_qty.0.max(1);
            let (mut lo, mut hi) = (0i64, steps);
            while lo < hi {
                let mid = (lo + hi + 1) / 2;
                let (wb, wa) = with(Qty(mid * ctx.min_qty.0));
                if fits(wb, wa) {
                    lo = mid
                } else {
                    hi = mid - 1
                }
            }
            let clipped = (lo > 0).then(|| (px, Qty(lo * ctx.min_qty.0)));
            g.vetoes.push(if clipped.is_some() {
                "clipped by loss limit"
            } else {
                "loss limit"
            });
            if side == Side::Buy {
                b = clipped
            } else {
                a = clipped
            }
        }
        g.bid = b;
        g.ask = a;
        g.worst_case = worst(b, a);
        g
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ctx<'a>(slug: &'a Slug, pos: &'a Position, account_other: f64) -> GateCtx<'a> {
        GateCtx {
            slug,
            tick: Px(100),
            min_qty: Qty(100),
            fair: 0.5,
            best_bid: Some(Px(4500)),
            best_ask: Some(Px(5500)),
            position: pos,
            other_resting: Resting::default(),
            account_other,
            committed_other: 0.0,
            buying_power: None,
            orders_elsewhere: 0,
            has_working: (false, false),
        }
    }

    #[test]
    fn gate_checks_and_clips() {
        let risk = Risk::new(RiskConfig::default());
        let slug: Slug = Arc::from("m");
        let flat = Position::default();
        let g = risk.gate(
            &ctx(&slug, &flat, 0.0),
            Some((Px(4700), Qty(30_000))),
            Some((Px(5300), Qty(30_000))),
        );
        assert_eq!(g.bid, Some((Px(4700), Qty(30_000))));
        // Off tick and too far from fair.
        let g = risk.gate(
            &ctx(&slug, &flat, 0.0),
            Some((Px(4750), Qty(10_000))),
            Some((Px(7000), Qty(10_000))),
        );
        assert!(g.bid.is_none() && g.ask.is_none());
        // Max order size caps at 5 contracts.
        let g = risk.gate(
            &ctx(&slug, &flat, 0.0),
            Some((Px(4700), Qty(100_000))),
            None,
        );
        assert_eq!(g.bid.unwrap().1, Qty(50_000));
        // Account nearly full: the bid is clipped to fit the remaining $1.
        let g = risk.gate(
            &ctx(&slug, &flat, 39.0),
            Some((Px(4700), Qty(50_000))),
            None,
        );
        let (_, q) = g.bid.unwrap();
        assert!(q.as_f64() * 0.47 <= 1.0 + 1e-9 && q > Qty::ZERO, "{q:?}");
        // Long at the position limit: no bid, the ask still goes out.
        let mut long = Position::default();
        long.apply(Side::Buy, Qty(200_000), Px(5000), 0.0);
        let g = risk.gate(
            &ctx(&slug, &long, 0.0),
            Some((Px(4700), Qty(10_000))),
            Some((Px(5300), Qty(10_000))),
        );
        assert!(g.bid.is_none());
        assert!(g.ask.is_some());
    }

    #[test]
    fn reject_storm_blocks_orders() {
        let mut risk = Risk::new(RiskConfig::default());
        let slug: Slug = Arc::from("m");
        for i in 0..=risk.cfg.max_rejects_per_min as u64 {
            risk.on_reject(&slug, RejectClass::Transient, i);
        }
        assert!(risk.paused_reason("m", 100, 0).is_some());
        // Book-moved rejects are normal and don't count.
        let mut risk = Risk::new(RiskConfig::default());
        for i in 0..5 {
            risk.on_reject(&slug, RejectClass::BookMoved, i);
        }
        assert!(risk.paused_reason("m", 100, 0).is_none());
    }

    #[test]
    fn automatic_pauses_never_weaken_one_in_force() {
        const SEC: u64 = 1_000_000_000;
        let slug: Slug = Arc::from("m");
        // An operator pause outlives a later timed pause; only a resume clears it.
        let mut risk = Risk::new(RiskConfig::default());
        risk.pause(&slug, 0, None, "operator pause");
        risk.on_reject(&slug, RejectClass::Funding, SEC);
        assert_eq!(
            risk.paused_reason("m", 1_000 * SEC, 0).as_deref(),
            Some("operator pause")
        );
        risk.resume(Some("m"));
        assert!(risk.paused_reason("m", 1_000 * SEC, 0).is_none());
        // A shorter timed pause doesn't cut a longer one short; a longer one extends it.
        risk.pause(&slug, 0, Some(60.0), "long");
        risk.pause(&slug, SEC, Some(5.0), "short");
        assert_eq!(
            risk.paused_reason("m", 30 * SEC, 0).as_deref(),
            Some("long")
        );
        risk.pause(&slug, 30 * SEC, Some(60.0), "longer");
        assert_eq!(
            risk.paused_reason("m", 80 * SEC, 0).as_deref(),
            Some("longer")
        );
        // The loss limit still pauses indefinitely while a timed pause is in force.
        let mut risk = Risk::new(RiskConfig::default());
        risk.pause(&slug, 0, Some(60.0), "fill rate limit");
        risk.check_losses(0.0, 0, &[(slug.clone(), 0.0)], 0);
        risk.check_losses(
            0.0,
            0,
            &[(slug.clone(), -risk.cfg.market_loss_limit - 1.0)],
            SEC,
        );
        assert_eq!(
            risk.paused_reason("m", 1_000 * SEC, 0).as_deref(),
            Some("market loss limit")
        );
    }

    #[test]
    fn market_losses_and_indefinite_pauses_survive_a_restart() {
        const SEC: u64 = 1_000_000_000;
        let (a, b): (Slug, Slug) = (Arc::from("a"), Arc::from("b"));
        let mut risk = Risk::new(RiskConfig::default());
        risk.check_losses(0.0, 0, &[(a.clone(), 1.0), (b.clone(), 0.0)], 0);
        risk.check_losses(0.0, 0, &[(a.clone(), -2.0), (b.clone(), 0.5)], SEC);
        risk.pause(&b, SEC, None, "operator pause");
        risk.pause(&a, SEC, Some(60.0), "fill rate limit");
        let (losses, paused) = risk.persisted();
        assert_eq!(losses.len(), 1, "gains aren't carried");
        assert!((losses["a"] + 3.0).abs() < 1e-9);
        assert_eq!(
            paused.keys().collect::<Vec<_>>(),
            vec!["b"],
            "timed pauses aren't kept"
        );

        let mut after = Risk::new(RiskConfig::default());
        after.restore(&losses, &paused);
        assert_eq!(
            after.paused_reason("b", 0, 0).as_deref(),
            Some("operator pause")
        );
        // Saved again before the market is marked: the carried loss isn't lost.
        assert_eq!(after.persisted().0, losses);
        // First mark after the restart: the $3 lost before it still counts, so $2.50 more trips the limit.
        after.check_losses(0.0, 0, &[(a.clone(), 7.0)], 0);
        assert!(after.paused_reason("a", SEC, 0).is_none());
        let actions = after.check_losses(0.0, 0, &[(a.clone(), 4.5)], SEC);
        assert!(actions.contains(&RiskAction::CancelMarket(a.clone())));
        assert_eq!(
            after.paused_reason("a", 2 * SEC, 0).as_deref(),
            Some("market loss limit")
        );
    }

    #[test]
    fn buying_power_is_shared_across_markets() {
        let risk = Risk::new(RiskConfig::default());
        let slug: Slug = Arc::from("m");
        let flat = Position::default();
        // $4 buying power, $3 already committed to another market's orders: $1 is left for this bid.
        let c = GateCtx {
            committed_other: 3.0,
            buying_power: Some(4.0),
            ..ctx(&slug, &flat, 3.0)
        };
        let g = risk.gate(&c, Some((Px(5000), Qty(50_000))), None);
        let (_, q) = g.bid.unwrap();
        assert!(q.notional(Px(5000)) <= 1.0 + 1e-9 && q > Qty::ZERO, "{q:?}");
        // Another market's position (exposure, not collateral the orders need) doesn't use it up.
        let c = GateCtx {
            committed_other: 0.0,
            buying_power: Some(4.0),
            ..ctx(&slug, &flat, 3.0)
        };
        assert_eq!(
            risk.gate(&c, Some((Px(5000), Qty(50_000))), None).bid,
            Some((Px(5000), Qty(50_000)))
        );
        // This market's own position isn't counted twice: only the orders' extra collateral is.
        let mut long = Position::default();
        long.apply(Side::Buy, Qty(50_000), Px(5000), 0.0);
        let c = GateCtx {
            buying_power: Some(2.5),
            ..ctx(&slug, &long, 0.0)
        };
        assert_eq!(
            risk.gate(&c, Some((Px(4700), Qty(50_000))), None).bid,
            Some((Px(4700), Qty(50_000)))
        );
    }
}
