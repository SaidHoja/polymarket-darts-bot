//! Market-making strategies. Quotes sit a half-spread either side of others' mid or
//! top-three-level book VWAP and never cross their touch. The mid-based configs default
//! to their backtested settings; `two_phase_vwap` shares
//! the two-phase parameters and uses a different fair value.
//!
//! Shared parameters:
//! - `size`: contracts per quote.
//! - `max_spread`: don't quote when others' spread is wider than this (dollars).
//! - `max_inventory`: at |position| >= this, quote only the side that reduces it (0: off).
//! - `skew_per_contract`: shift both quotes by −skew × position (dollars per contract).
//! - `price_band_lo` / `price_band_hi`: never quote outside this YES price band.

use pm_types::{MarketParams, Px, Qty};
use serde::{Deserialize, Serialize};

use super::{Decision, MarketView, Strategy};

/// `pre_match`: quote before the match only, stopping `stop_before_start_s` before the
/// scheduled start, or earlier if the game feed reports the match live. Defaults are `m3_15c_pre_inv`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreMatchConfig {
    pub size: f64,
    /// Distance from mid to each quote (dollars).
    pub half_spread: f64,
    pub max_spread: f64,
    pub stop_before_start_s: f64,
    pub max_inventory: f64,
    pub skew_per_contract: f64,
    pub price_band_lo: f64,
    pub price_band_hi: f64,
}

impl Default for PreMatchConfig {
    fn default() -> Self {
        PreMatchConfig {
            size: 1.0,
            half_spread: 0.15,
            max_spread: 0.40,
            stop_before_start_s: 0.0,
            max_inventory: 3.0,
            skew_per_contract: 0.01,
            price_band_lo: 0.02,
            price_band_hi: 0.98,
        }
    }
}

/// `full_match`: one width before the start and in play, until the match ends. Defaults are `m1_15c`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FullMatchConfig {
    pub size: f64,
    pub half_spread: f64,
    pub max_spread: f64,
    pub max_inventory: f64,
    pub skew_per_contract: f64,
    pub price_band_lo: f64,
    pub price_band_hi: f64,
}

impl Default for FullMatchConfig {
    fn default() -> Self {
        FullMatchConfig {
            size: 1.0,
            half_spread: 0.15,
            max_spread: 0.40,
            max_inventory: 0.0,
            skew_per_contract: 0.0,
            price_band_lo: 0.02,
            price_band_hi: 0.98,
        }
    }
}

/// `two_phase`: `pre_half_spread` until `widen_before_start_s` before the scheduled start, then
/// `live_half_spread` through play. Defaults are `mix_10c_pre_15c_live`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TwoPhaseConfig {
    pub size: f64,
    pub pre_half_spread: f64,
    pub live_half_spread: f64,
    pub widen_before_start_s: f64,
    pub max_spread: f64,
    pub max_inventory: f64,
    pub skew_per_contract: f64,
    pub price_band_lo: f64,
    pub price_band_hi: f64,
}

impl Default for TwoPhaseConfig {
    fn default() -> Self {
        TwoPhaseConfig {
            size: 1.0,
            pre_half_spread: 0.10,
            live_half_spread: 0.15,
            widen_before_start_s: 300.0,
            max_spread: 0.40,
            max_inventory: 0.0,
            skew_per_contract: 0.0,
            price_band_lo: 0.02,
            price_band_hi: 0.98,
        }
    }
}

/// What one tick's quotes are built from, whichever strategy picked it.
struct Quote {
    size: f64,
    half_spread: f64,
    max_inventory: f64,
    skew_per_contract: f64,
    price_band_lo: f64,
    price_band_hi: f64,
}

/// At or past `secs` before the scheduled start (false when the start is unknown).
fn within_start(v: &MarketView, secs: f64) -> bool {
    v.params
        .game_start_ns
        .is_some_and(|start| v.wall_ns >= start - (secs * 1e9) as i64)
}

fn mid(v: &MarketView, max_spread: f64) -> Option<f64> {
    let (b, a) = (v.others.best_bid()?, v.others.best_ask()?);
    let spread = a.0.as_f64() - b.0.as_f64();
    (spread <= max_spread + 1e-9).then(|| (a.0.as_f64() + b.0.as_f64()) / 2.0)
}

/// `None` quote: report fair but don't quote.
fn decide(v: &MarketView, q: Option<Quote>, fair: Option<f64>) -> Decision {
    let mut d = Decision {
        fair,
        ..Decision::none()
    };
    let ended = v.game.is_some_and(|g| g.frame.ended || g.frame.cancelled());
    let (Some(fair), Some(q)) = (fair, q) else {
        return d;
    };
    let size = Qty::from_f64(q.size).round_down(v.params.min_qty);
    if !v.quotable || ended || size <= Qty::ZERO {
        return d;
    }
    let pos = v.position.as_f64();
    let center = fair - q.skew_per_contract * pos;
    let tick = v.params.tick;
    let (lo, hi) = (
        Px::from_f64(q.price_band_lo).ceil_to(tick),
        Px::from_f64(q.price_band_hi).floor_to(tick),
    );
    let mut bid = Px::from_f64(center - q.half_spread).floor_to(tick);
    let mut ask = Px::from_f64(center + q.half_spread).ceil_to(tick);
    // Post-only: stay strictly inside the other side.
    if let Some((a, _)) = v.others.best_ask() {
        bid = bid.min(Px(a.0 - tick.0));
    }
    if let Some((b, _)) = v.others.best_bid() {
        ask = ask.max(Px(b.0 + tick.0));
    }
    let capped = q.max_inventory > 0.0 && pos.abs() >= q.max_inventory - 1e-9;
    d.bid = (bid >= lo && bid <= hi && !(capped && pos > 0.0)).then_some((bid, size));
    d.ask = (ask >= lo && ask <= hi && ask > bid && !(capped && pos < 0.0)).then_some((ask, size));
    d
}

pub struct PreMatch {
    cfg: PreMatchConfig,
    fair: Option<f64>,
}

impl PreMatch {
    pub fn new(cfg: PreMatchConfig) -> Self {
        PreMatch { cfg, fair: None }
    }
}

/// `secs` before the scheduled start, in wall ns.
fn before_start(params: &MarketParams, secs: f64) -> Option<i64> {
    params
        .game_start_ns
        .map(|start| start - (secs * 1e9) as i64)
}

impl Strategy for PreMatch {
    fn wake_at(&self, params: &MarketParams) -> Option<i64> {
        before_start(params, self.cfg.stop_before_start_s)
    }

    fn decide(&mut self, v: &MarketView) -> Decision {
        let c = &self.cfg;
        let live = v.game.is_some_and(|g| g.frame.live);
        let q = (!live && !within_start(v, c.stop_before_start_s)).then_some(Quote {
            size: c.size,
            half_spread: c.half_spread,
            max_inventory: c.max_inventory,
            skew_per_contract: c.skew_per_contract,
            price_band_lo: c.price_band_lo,
            price_band_hi: c.price_band_hi,
        });
        let d = decide(v, q, mid(v, c.max_spread));
        self.fair = d.fair;
        d
    }

    fn fair(&self) -> Option<f64> {
        self.fair
    }
}

pub struct FullMatch {
    cfg: FullMatchConfig,
    fair: Option<f64>,
}

impl FullMatch {
    pub fn new(cfg: FullMatchConfig) -> Self {
        FullMatch { cfg, fair: None }
    }
}

impl Strategy for FullMatch {
    fn decide(&mut self, v: &MarketView) -> Decision {
        let c = &self.cfg;
        let q = Quote {
            size: c.size,
            half_spread: c.half_spread,
            max_inventory: c.max_inventory,
            skew_per_contract: c.skew_per_contract,
            price_band_lo: c.price_band_lo,
            price_band_hi: c.price_band_hi,
        };
        let d = decide(v, Some(q), mid(v, c.max_spread));
        self.fair = d.fair;
        d
    }

    fn fair(&self) -> Option<f64> {
        self.fair
    }
}

/// Price levels per side in `two_phase_vwap`'s fair, independent of the displayed `markets.vwap_depth`.
const VWAP_TRADING_DEPTH: usize = 3;

pub struct TwoPhase {
    cfg: TwoPhaseConfig,
    fair: Option<f64>,
    vwap_center: bool,
}

impl TwoPhase {
    pub fn new(cfg: TwoPhaseConfig) -> Self {
        TwoPhase {
            cfg,
            fair: None,
            vwap_center: false,
        }
    }

    /// The same phase timing and quoting rules, centred on others' best `VWAP_TRADING_DEPTH` price
    /// levels per side.
    pub fn new_vwap(cfg: TwoPhaseConfig) -> Self {
        TwoPhase {
            cfg,
            fair: None,
            vwap_center: true,
        }
    }
}

impl Strategy for TwoPhase {
    fn uses_book_depth(&self) -> bool {
        self.vwap_center
    }

    fn wake_at(&self, params: &MarketParams) -> Option<i64> {
        before_start(params, self.cfg.widen_before_start_s)
    }

    fn decide(&mut self, v: &MarketView) -> Decision {
        let c = &self.cfg;
        let half_spread = if within_start(v, c.widen_before_start_s) {
            c.live_half_spread
        } else {
            c.pre_half_spread
        };
        let q = Quote {
            size: c.size,
            half_spread,
            max_inventory: c.max_inventory,
            skew_per_contract: c.skew_per_contract,
            price_band_lo: c.price_band_lo,
            price_band_hi: c.price_band_hi,
        };
        // Keep the two-sided/max-spread checks even though VWAP alone permits a one-sided book.
        let fair = mid(v, c.max_spread).and_then(|mid| {
            if self.vwap_center {
                v.others.vwap(VWAP_TRADING_DEPTH).map(|(px, _)| px)
            } else {
                Some(mid)
            }
        });
        let d = decide(v, Some(q), fair);
        self.fair = d.fair;
        d
    }

    fn fair(&self) -> Option<f64> {
        self.fair
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::LeagueConfig, game::GameState, market_data::BookView, strategy::LeagueCheck,
    };
    use pm_types::{GameFrame, MarketParams};

    const START: i64 = 1_000_000_000_000;
    const MIN: i64 = 60_000_000_000;

    fn params() -> MarketParams {
        MarketParams {
            slug: "m".into(),
            title: "test".into(),
            event_id: "e".into(),
            event_slug: "e".into(),
            league: "l".into(),
            sports_market_type: "darts_match_winner".into(),
            tick: Px(100),
            min_qty: Qty(100),
            fee_coefficient: 0.0,
            game_start_ns: Some(START),
            yes_name: None,
            no_name: None,
            yes_ordering: None,
            is_futures: false,
        }
    }

    fn game(live: bool) -> GameState {
        GameState {
            frame: GameFrame {
                event_id: "e".into(),
                event_slug: "e".into(),
                score: None,
                period: "NS".into(),
                live,
                ended: false,
                updated_ns: None,
            },
            last_leg_change_mono: None,
        }
    }

    /// Quotes for a 0.45 / 0.55 book at `wall_ns` with `position` contracts.
    fn quotes(
        s: &mut dyn Strategy,
        wall_ns: i64,
        position: f64,
        game: Option<&GameState>,
    ) -> (Option<Px>, Option<Px>) {
        let (params, league) = (params(), LeagueConfig::default());
        let book = BookView {
            bids: vec![(Px(4500), Qty(100_000))],
            offers: vec![(Px(5500), Qty(100_000))],
        };
        let v = MarketView {
            params: &params,
            league: &league,
            league_check: LeagueCheck::Unverified,
            others: &book,
            last_trade: None,
            game,
            position: Qty::from_f64(position),
            working_bid: None,
            working_ask: None,
            quotable: true,
            input_updated: true,
            filled: false,
            slow_exchange: false,
            now: 1,
            wall_ns,
        };
        let d = s.decide(&v);
        assert_eq!(d.fair, Some(0.5));
        (d.bid.map(|q| q.0), d.ask.map(|q| q.0))
    }

    #[test]
    fn pre_match_stops_at_the_start_or_when_live_and_caps_inventory() {
        let mut s = PreMatch::new(PreMatchConfig::default());
        assert_eq!(
            quotes(&mut s, START - MIN, 0.0, None),
            (Some(Px(3500)), Some(Px(6500)))
        );
        assert_eq!(quotes(&mut s, START, 0.0, None), (None, None));
        assert_eq!(
            quotes(&mut s, START - MIN, 0.0, Some(&game(true))),
            (None, None)
        );
        // Long 3 = at the cap: only the ask, skewed down 3¢.
        assert_eq!(
            quotes(&mut s, START - MIN, 3.0, None),
            (None, Some(Px(6200)))
        );
    }

    #[test]
    fn full_match_keeps_one_width_in_play() {
        let mut s = FullMatch::new(FullMatchConfig::default());
        assert_eq!(
            quotes(&mut s, START - MIN, 0.0, None),
            (Some(Px(3500)), Some(Px(6500)))
        );
        assert_eq!(
            quotes(&mut s, START + MIN, 0.0, Some(&game(true))),
            (Some(Px(3500)), Some(Px(6500)))
        );
        assert_eq!(
            quotes(&mut s, START + MIN, 10.0, None),
            (Some(Px(3500)), Some(Px(6500))),
            "no cap by default"
        );
    }

    #[test]
    fn two_phase_widens_from_five_minutes_before_the_start() {
        let mut s = TwoPhase::new(TwoPhaseConfig::default());
        assert_eq!(
            quotes(&mut s, START - 6 * MIN, 0.0, None),
            (Some(Px(4000)), Some(Px(6000)))
        );
        assert_eq!(
            quotes(&mut s, START - 4 * MIN, 0.0, None),
            (Some(Px(3500)), Some(Px(6500)))
        );
        assert_eq!(
            quotes(&mut s, START + MIN, 0.0, Some(&game(true))),
            (Some(Px(3500)), Some(Px(6500)))
        );
    }

    #[test]
    fn two_phase_vwap_uses_three_levels_and_preserves_timing_inventory_and_book_checks() {
        let (params, league) = (params(), LeagueConfig::default());
        let book = BookView {
            bids: vec![
                (Px(4500), Qty(10_000)),
                (Px(4400), Qty(10_000)),
                (Px(4300), Qty(10_000)),
                (Px(100), Qty(100_000_000)),
            ],
            offers: vec![
                (Px(5500), Qty(30_000)),
                (Px(5600), Qty(30_000)),
                (Px(5700), Qty(30_000)),
                (Px(9900), Qty(100_000_000)),
            ],
        };
        let mut v = MarketView {
            params: &params,
            league: &league,
            league_check: LeagueCheck::Unverified,
            others: &book,
            last_trade: None,
            game: None,
            position: Qty::ZERO,
            working_bid: None,
            working_ask: None,
            quotable: true,
            input_updated: true,
            filled: false,
            slow_exchange: false,
            now: 1,
            wall_ns: START - 6 * MIN,
        };
        let mut s = TwoPhase::new_vwap(TwoPhaseConfig {
            max_inventory: 4.0,
            skew_per_contract: 0.01,
            ..Default::default()
        });
        assert!(s.uses_book_depth());
        assert_eq!(s.wake_at(&params), Some(START - 5 * MIN));
        let d = s.decide(&v);
        assert!((d.fair.unwrap() - 0.53).abs() < 1e-9);
        assert_eq!(
            (d.bid, d.ask),
            (Some((Px(4300), Qty(10_000))), Some((Px(6300), Qty(10_000))))
        );
        assert_eq!(s.fair(), d.fair);
        v.wall_ns = START - 5 * MIN;
        let d = s.decide(&v);
        assert_eq!((d.bid.unwrap().0, d.ask.unwrap().0), (Px(3800), Px(6800)));
        v.position = Qty::from_f64(4.0);
        let d = s.decide(&v);
        assert_eq!((d.bid, d.ask.map(|q| q.0)), (None, Some(Px(6400))));
        v.position = Qty::from_f64(-4.0);
        let d = s.decide(&v);
        assert_eq!((d.bid.map(|q| q.0), d.ask), (Some(Px(4200)), None));
        v.quotable = false;
        let d = s.decide(&v);
        assert!(d.fair.is_some() && d.bid.is_none() && d.ask.is_none());
        v.quotable = true;
        let mut ended = game(true);
        ended.frame.ended = true;
        v.game = Some(&ended);
        let d = s.decide(&v);
        assert!(d.bid.is_none() && d.ask.is_none());
        v.game = None;
        let one_sided = BookView {
            bids: book.bids.clone(),
            offers: vec![],
        };
        v.others = &one_sided;
        assert_eq!(s.decide(&v), Decision::none());
        let wide = BookView {
            bids: vec![(Px(1000), Qty(10_000))],
            offers: vec![(Px(9000), Qty(10_000))],
        };
        v.others = &wide;
        assert_eq!(s.decide(&v), Decision::none());
    }

    #[test]
    fn two_phase_vwap_stays_post_only_when_fair_is_outside_the_touch() {
        let (params, league) = (params(), LeagueConfig::default());
        let book = BookView {
            bids: vec![(Px(4500), Qty(10_000))],
            offers: vec![
                (Px(5500), Qty(10_000)),
                (Px(8000), Qty(1_000_000)),
                (Px(9000), Qty(1_000_000)),
            ],
        };
        let v = MarketView {
            params: &params,
            league: &league,
            league_check: LeagueCheck::Unverified,
            others: &book,
            last_trade: None,
            game: None,
            position: Qty::ZERO,
            working_bid: None,
            working_ask: None,
            quotable: true,
            input_updated: true,
            filled: false,
            slow_exchange: false,
            now: 1,
            wall_ns: START - 6 * MIN,
        };
        let mut s = TwoPhase::new_vwap(TwoPhaseConfig::default());
        let d = s.decide(&v);
        assert!(d.fair.unwrap() > 0.8);
        assert_eq!(d.bid.unwrap().0, Px(5400));
        assert!(d.ask.unwrap().0 > book.best_bid().unwrap().0);
    }
}
