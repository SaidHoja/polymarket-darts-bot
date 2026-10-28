//! A continuous diagnostic quote strategy. Distant prices reduce, but do not
//! eliminate, fill risk. Never takes or automatically liquidates a position.
use pm_types::{MarketParams, Px, Qty};
use serde::{Deserialize, Serialize};

use super::{Decision, MarketView, Strategy};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LatencyProbeConfig {
    /// Skip markets whose minimum order quantity exceeds this many contracts.
    pub max_size: f64,
    /// Minimum time between reprices of one market; each reprice also needs a received input.
    pub reprice_s: f64,
}

impl Default for LatencyProbeConfig {
    fn default() -> Self {
        Self {
            max_size: 0.1,
            reprice_s: 2.0,
        }
    }
}

impl LatencyProbeConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.max_size.is_finite() || self.max_size <= 0.0 || self.max_size > 1.0 {
            return Err("latency_probe max_size must be finite and in (0, 1] contracts".into());
        }
        if !self.reprice_s.is_finite() || self.reprice_s < 1.0 {
            return Err("latency_probe reprice_s must be finite and >= 1".into());
        }
        Ok(())
    }
}

pub struct LatencyProbe {
    cfg: LatencyProbeConfig,
    started: Option<u64>,
    stopped: bool,
    fair: Option<f64>,
    last_reprice: u64,
    alternate: bool,
    reason: Option<&'static str>,
}

impl LatencyProbe {
    pub fn new(cfg: LatencyProbeConfig) -> Self {
        Self {
            cfg,
            started: None,
            stopped: false,
            fair: None,
            last_reprice: 0,
            alternate: false,
            reason: None,
        }
    }
}

impl Strategy for LatencyProbe {
    fn decide(&mut self, v: &MarketView) -> Decision {
        self.reason = None;
        self.fair = v.others.microprice();
        let mut d = Decision {
            fair: self.fair,
            ..Decision::none()
        };
        if v.filled || v.position != Qty::ZERO {
            self.stopped = true;
        }
        if self.stopped || self.cfg.validate().is_err() || !v.quotable || v.slow_exchange {
            self.reason = Some("probe: stopped, gated, or exchange slow");
            return d;
        }
        // Only a known pre-match game, at least five minutes before its start.
        if v.game.is_none_or(|g| {
            g.frame.live || g.frame.ended || g.frame.cancelled() || !g.known_period()
        }) || v
            .params
            .game_start_ns
            .is_none_or(|s| s <= v.wall_ns.saturating_add(300_000_000_000))
        {
            self.reason =
                Some("probe: requires a known pre-match game more than five minutes away");
            return d;
        }
        let (Some((bid, _)), Some((ask, _))) = (v.others.best_bid(), v.others.best_ask()) else {
            self.reason = Some("probe: waiting for a two-sided book");
            return d;
        };
        // Wide books are expected in these markets. Check distance to the
        // opposing executable touch per side, not the width of the spread.
        if ask <= bid {
            self.reason = Some("probe: crossed or locked book");
            return d;
        }
        let qty = v.params.min_qty;
        if qty <= Qty::ZERO || qty > Qty::from_f64(self.cfg.max_size) || v.params.tick <= Px::ZERO {
            self.reason = Some("probe: unsupported minimum size or tick");
            return d;
        }
        if self.started.is_none() {
            if !v.input_updated {
                self.reason = Some("probe: waiting for received market/game input");
                return d;
            }
            self.started = Some(v.now);
            self.last_reprice = v.now;
        } else if v.input_updated
            && v.now.saturating_sub(self.last_reprice) >= (self.cfg.reprice_s * 1e9) as u64
        {
            self.alternate = !self.alternate;
            self.last_reprice = v.now;
        }
        let low = Px(if self.alternate { 200 } else { 100 }).ceil_to(v.params.tick);
        let high = Px(if self.alternate { 9800 } else { 9900 }).floor_to(v.params.tick);
        // Never improve/join the same-side touch; stay >= $0.20 from the
        // opposing touch. Fall back to the outer quote if the inner one fails.
        let bid_ok = |p: Px| p >= Px(100) && p <= Px(200) && p < bid && ask.0 - p.0 >= 2000;
        let ask_ok = |p: Px| p >= Px(9800) && p <= Px(9900) && p > ask && p.0 - bid.0 >= 2000;
        let outer_bid = Px(100).ceil_to(v.params.tick);
        let outer_ask = Px(9900).floor_to(v.params.tick);
        d.bid = [low, outer_bid]
            .into_iter()
            .find(|p| bid_ok(*p))
            .map(|p| (p, qty));
        d.ask = [high, outer_ask]
            .into_iter()
            .find(|p| ask_ok(*p))
            .map(|p| (p, qty));
        if d.bid.is_none() && d.ask.is_none() {
            self.reason = Some("probe: no distant quote behind the current touch");
        }
        d
    }

    fn fair(&self) -> Option<f64> {
        self.fair
    }

    fn quote_lifetime_s(&self) -> Option<u64> {
        Some(15)
    }

    fn input_driven(&self) -> bool {
        true
    }

    fn diagnostic_reason(&self) -> Option<&'static str> {
        self.reason
    }

    fn wake_at(&self, params: &MarketParams) -> Option<i64> {
        params.game_start_ns.map(|s| s - 300_000_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::LeagueConfig, game::GameState, market_data::BookView, strategy::LeagueCheck,
    };
    use pm_types::{GameFrame, MarketParams};

    #[test]
    fn quotes_continuously_but_stops_permanently_after_a_fill_or_position() {
        let params = MarketParams {
            slug: "m".into(),
            title: "test".into(),
            event_id: "e".into(),
            event_slug: "e".into(),
            league: "l".into(),
            sports_market_type: "darts_match_winner".into(),
            tick: Px(100),
            min_qty: Qty(100),
            fee_coefficient: 0.0,
            game_start_ns: Some(1_000_000_000_000),
            yes_name: None,
            no_name: None,
            yes_ordering: None,
            is_futures: false,
        };
        let league = LeagueConfig::default();
        let book = BookView {
            bids: vec![(Px(4500), Qty(10000))],
            offers: vec![(Px(5500), Qty(10000))],
        };
        let game = GameState {
            frame: GameFrame {
                event_id: "e".into(),
                event_slug: "e".into(),
                score: None,
                period: "NS".into(),
                live: false,
                ended: false,
                updated_ns: None,
            },
            last_leg_change_mono: None,
        };
        let mut v = MarketView {
            params: &params,
            league: &league,
            league_check: LeagueCheck::Unverified,
            others: &book,
            last_trade: None,
            game: Some(&game),
            position: Qty::ZERO,
            working_bid: None,
            working_ask: None,
            quotable: true,
            input_updated: true,
            filled: false,
            slow_exchange: false,
            now: 1,
            wall_ns: 0,
        };
        let mut probe = LatencyProbe::new(LatencyProbeConfig::default());
        v.quotable = false;
        assert!(probe.decide(&v).bid.is_none());
        v.quotable = true;
        v.game = None;
        assert!(probe.decide(&v).bid.is_none());
        v.game = Some(&game);
        v.input_updated = false;
        assert!(probe.decide(&v).bid.is_none());
        v.input_updated = true;
        let d = probe.decide(&v);
        assert_eq!(d.bid, Some((Px(100), Qty(100))));
        assert_eq!(d.ask, Some((Px(9900), Qty(100))));
        v.now += 10_000_000_000;
        v.input_updated = false;
        assert_eq!(probe.decide(&v).bid.unwrap().0, Px(100));
        v.input_updated = true;
        assert_eq!(probe.decide(&v).bid.unwrap().0, Px(200));
        v.filled = true;
        assert!(probe.decide(&v).bid.is_none());
        v.filled = false;
        assert!(probe.decide(&v).bid.is_none());
        let mut probe = LatencyProbe::new(LatencyProbeConfig::default());
        assert!(probe.decide(&v).bid.is_some());
        for elapsed in [120_000_000_000, 600_000_000_000, 86_400_000_000_000] {
            v.now += elapsed;
            assert!(
                probe.decide(&v).bid.is_some(),
                "no session-duration timeout"
            );
            assert!(probe.decide(&v).ask.is_some());
        }
        let mut probe = LatencyProbe::new(LatencyProbeConfig::default());
        v.position = Qty(1);
        assert!(probe.decide(&v).bid.is_none());
        v.position = Qty::ZERO;
        assert!(
            probe.decide(&v).bid.is_none(),
            "observing a position latches the stop"
        );
    }

    #[test]
    fn rejects_invalid_probe_parameters() {
        let mut cfg = LatencyProbeConfig {
            max_size: f64::NAN,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        cfg.max_size = 0.1;
        cfg.reprice_s = 0.0;
        assert!(cfg.validate().is_err());
    }
}
