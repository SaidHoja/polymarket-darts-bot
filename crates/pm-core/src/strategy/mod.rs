//! Strategies: what to quote in one market. The market makers are in `spread`
//! (`pre_match`, `full_match`, `two_phase`, `two_phase_vwap`); `off` and the diagnostic
//! `latency_probe` are the others.
//!
//! Each market gets its own strategy instance, built from the profile its route
//! picks (`Config::strategy_for`). A strategy never sees raw frames, order ids or
//! the network: per tick it gets a `MarketView` and returns a `Decision`. The core
//! turns decisions into orders, and risk can veto anything.
//!
//! Adding a strategy:
//! 1. a module with its config struct (`#[serde(default, deny_unknown_fields)]`) and a type implementing `Strategy`;
//! 2. a variant in `StrategyConfig` and an arm in `StrategyConfig::build`.
//!
//! Then `kind = "<variant in snake_case>"` selects it in a profile.

pub mod latency_probe;
pub mod spread;

use pm_types::{MarketParams, Px, Qty};
use serde::{Deserialize, Serialize};

use crate::config::LeagueConfig;
use crate::game::GameState;
use crate::market_data::BookView;

pub use latency_probe::{LatencyProbe, LatencyProbeConfig};
pub use spread::{FullMatch, FullMatchConfig, PreMatch, PreMatchConfig, TwoPhase, TwoPhaseConfig};

/// One market's strategy. State lives in the instance; one instance per market.
pub trait Strategy {
    fn decide(&mut self, view: &MarketView) -> Decision;

    /// Current fair value of YES, if the strategy has one. Used to mark positions;
    /// the core falls back to the venue microprice when this is `None`.
    fn fair(&self) -> Option<f64>;

    /// Optional shorter GTD lifetime for diagnostic strategies.
    fn quote_lifetime_s(&self) -> Option<u64> {
        None
    }

    /// Diagnostic orders must originate from newly received market/game input.
    fn input_driven(&self) -> bool {
        false
    }

    /// Re-decide on every accepted book frame, including changes below the best bid and ask.
    fn uses_book_depth(&self) -> bool {
        false
    }

    fn diagnostic_reason(&self) -> Option<&'static str> {
        None
    }

    /// Wall-clock ns at which this strategy's decision changes with no new input (e.g. a
    /// scheduled-start cutoff). The core re-decides the market then; markets are otherwise
    /// only re-decided on input.
    fn wake_at(&self, _params: &MarketParams) -> Option<i64> {
        None
    }
}

/// Strategy profile: `kind` picks the variant, the rest are its parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // config, built once per market; boxing buys nothing
pub enum StrategyConfig {
    /// Quote around others' mid before the match only.
    PreMatch(PreMatchConfig),
    /// Quote around others' mid at one width, before the start and in play.
    FullMatch(FullMatchConfig),
    /// Quote around others' mid, one width pre-match and a wider one from just before the start.
    TwoPhase(TwoPhaseConfig),
    /// The two-phase timing and widths, centred on others' top-three-level book VWAP.
    TwoPhaseVwap(TwoPhaseConfig),
    /// Track the market but never quote. Existing quotes in it get cancelled.
    Off,
    /// Bounded, distant minimum-size quotes for operator-run latency diagnostics.
    LatencyProbe(LatencyProbeConfig),
}

impl Default for StrategyConfig {
    fn default() -> Self {
        StrategyConfig::PreMatch(PreMatchConfig::default())
    }
}

impl StrategyConfig {
    pub fn build(&self) -> Box<dyn Strategy> {
        match self {
            StrategyConfig::PreMatch(cfg) => Box::new(PreMatch::new(cfg.clone())),
            StrategyConfig::FullMatch(cfg) => Box::new(FullMatch::new(cfg.clone())),
            StrategyConfig::TwoPhase(cfg) => Box::new(TwoPhase::new(cfg.clone())),
            StrategyConfig::TwoPhaseVwap(cfg) => Box::new(TwoPhase::new_vwap(cfg.clone())),
            StrategyConfig::Off => Box::new(Off),
            StrategyConfig::LatencyProbe(cfg) => Box::new(LatencyProbe::new(cfg.clone())),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            StrategyConfig::PreMatch(_) => "pre_match",
            StrategyConfig::FullMatch(_) => "full_match",
            StrategyConfig::TwoPhase(_) => "two_phase",
            StrategyConfig::TwoPhaseVwap(_) => "two_phase_vwap",
            StrategyConfig::Off => "off",
            StrategyConfig::LatencyProbe(_) => "latency_probe",
        }
    }

    /// This profile with some parameters replaced, e.g. a route's `set = { size = 2.0 }`.
    /// Unknown parameter names are errors.
    pub fn with_overrides(
        &self,
        set: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<Self, String> {
        if set.is_empty() {
            return Ok(self.clone());
        }
        if set.contains_key("kind") {
            return Err("`set` can't change `kind`; route to a different profile instead".into());
        }
        if matches!(self, StrategyConfig::Off) {
            return Err(format!(
                "strategy `off` has no parameters to set ({:?})",
                set.keys().collect::<Vec<_>>()
            ));
        }
        let mut value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        let obj = value.as_object_mut().ok_or("profile isn't a table")?;
        for (k, v) in set {
            obj.insert(k.clone(), v.clone());
        }
        serde_json::from_value(value).map_err(|e| format!("bad override for {}: {e}", self.kind()))
    }
}

/// Match phase as the strategy sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Phase {
    PreMatch,
    StartWindow,
    BetweenLegs,
    InLeg,
    MatchPoint,
    Closed,
}

impl Phase {
    pub fn in_play(self) -> bool {
        matches!(self, Phase::BetweenLegs | Phase::InLeg | Phase::MatchPoint)
    }
}

/// Live check that the score's home/away maps to YES/NO the way we assume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LeagueCheck {
    Unverified,
    Verified,
    Failed,
}

pub struct MarketView<'a> {
    pub params: &'a MarketParams,
    pub league: &'a LeagueConfig,
    pub league_check: LeagueCheck,
    /// Provisional book minus our own resting orders.
    pub others: &'a BookView,
    pub last_trade: Option<(Px, u64)>,
    pub game: Option<&'a GameState>,
    pub position: Qty,
    pub working_bid: Option<(Px, Qty)>,
    pub working_ask: Option<(Px, Qty)>,
    pub quotable: bool,
    /// Accepted book/trade or a received game frame for this market in this tick.
    pub input_updated: bool,
    /// A fill happened in this market this tick.
    pub filled: bool,
    pub slow_exchange: bool,
    pub now: u64,
    pub wall_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub fair: Option<f64>,
    /// The strategy's view of the match phase, if it tracks one.
    pub phase: Option<Phase>,
    pub bid: Option<(Px, Qty)>,
    pub ask: Option<(Px, Qty)>,
    /// Result of an orientation check that completed this tick.
    pub orientation: Option<bool>,
}

impl Decision {
    /// No fair value, no orders.
    pub fn none() -> Self {
        Decision {
            fair: None,
            phase: None,
            bid: None,
            ask: None,
            orientation: None,
        }
    }
}

/// Never quotes.
pub struct Off;

impl Strategy for Off {
    fn decide(&mut self, _view: &MarketView) -> Decision {
        Decision::none()
    }

    fn fair(&self) -> Option<f64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Deserialize)]
    struct Profiles {
        strategies: BTreeMap<String, StrategyConfig>,
    }

    fn parse(text: &str) -> Result<BTreeMap<String, StrategyConfig>, String> {
        toml::from_str::<Profiles>(text)
            .map(|p| p.strategies)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn profiles_parse_by_kind_with_defaults() {
        let p = parse(
            "[strategies.a]\nkind = \"two_phase\"\nsize = 2.5\n[strategies.b]\nkind = \"off\"\n",
        )
        .unwrap();
        let StrategyConfig::TwoPhase(a) = &p["a"] else {
            panic!()
        };
        assert_eq!(a.size, 2.5);
        assert_eq!(
            a.live_half_spread,
            TwoPhaseConfig::default().live_half_spread
        );
        assert_eq!(p["b"], StrategyConfig::Off);
    }

    #[test]
    fn typos_and_unknown_kinds_are_errors() {
        assert!(
            parse("[strategies.a]\nkind = \"pre_match\"\nhalf_sprad = 0.1\n")
                .unwrap_err()
                .contains("half_sprad")
        );
        assert!(
            parse("[strategies.a]\nkind = \"full_match\"\nstop_before_start_s = 0.0\n").is_err(),
            "pre_match-only key"
        );
        assert!(parse("[strategies.a]\nkind = \"martingale\"\n").is_err());
        assert!(
            parse("[strategies.a]\nsize = 1.0\n").is_err(),
            "kind is required"
        );
    }

    #[test]
    fn overrides_replace_only_named_parameters() {
        let base = StrategyConfig::default();
        let set = BTreeMap::from([
            ("size".to_string(), serde_json::json!(0.5)),
            ("half_spread".to_string(), serde_json::json!(0.10)),
        ]);
        let StrategyConfig::PreMatch(c) = base.with_overrides(&set).unwrap() else {
            panic!()
        };
        assert_eq!((c.size, c.half_spread), (0.5, 0.10));
        assert_eq!(c.max_inventory, PreMatchConfig::default().max_inventory);

        let typo = BTreeMap::from([("sise".to_string(), serde_json::json!(0.5))]);
        assert!(base.with_overrides(&typo).unwrap_err().contains("sise"));
        let wrong_type = BTreeMap::from([("size".to_string(), serde_json::json!("big"))]);
        assert!(base.with_overrides(&wrong_type).is_err());
        assert!(StrategyConfig::Off.with_overrides(&set).is_err());
    }
}
