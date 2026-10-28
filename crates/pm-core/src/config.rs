//! Core configuration. Every field has a default so a partial TOML file works,
//! and unknown keys are errors, so a typo can't silently fall back to a default.
//! Prices and spreads are in dollars (YES probability units), sizes in contracts.
//!
//! Strategies are named profiles (`[strategies.<name>]`, each a `kind` plus its
//! parameters). `[[routes]]` pick a profile per market, first match wins; markets
//! no route matches use the profile named `default`. See `config/pmbot.example.toml`.

use std::collections::BTreeMap;

use pm_types::MarketParams;
use serde::{Deserialize, Serialize};

use crate::strategy::StrategyConfig;

/// The profile used when no route matches. Built in if not configured.
pub const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub markets: MarketsConfig,
    /// Named strategy profiles.
    pub strategies: BTreeMap<String, StrategyConfig>,
    /// Which profile each market uses; first match wins.
    pub routes: Vec<Route>,
    pub risk: RiskConfig,
    pub execution: ExecConfig,
    /// Per-league settings keyed by league slug (`modus`, `pdcdarts`, …).
    pub leagues: BTreeMap<String, LeagueConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarketsConfig {
    /// `sportsMarketType`s the bot tracks: `darts_match_winner` only by default (futures are out of scope for v1).
    pub types: Vec<String>,
    /// Price levels per side the core keeps from each book, best first; deeper levels are dropped
    /// before the book is applied, so the strategy, risk band and UI never see them. Default 20; 0 = all.
    pub book_depth: usize,
    /// Best price levels per side included in the displayed book VWAP. Default 3; 0 = all retained levels.
    pub vwap_depth: usize,
}

impl Default for MarketsConfig {
    fn default() -> Self {
        MarketsConfig {
            types: vec!["darts_match_winner".into()],
            book_depth: 20,
            vwap_depth: 3,
        }
    }
}

/// Picks a profile for the markets it matches. Every given matcher must match;
/// `slug`, `event` and `league` accept `*` wildcards (league slugs carry a season,
/// e.g. `modus-super-series-2026`, so `modus*` is usually what you want).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Route {
    pub slug: Option<String>,
    pub event: Option<String>,
    pub league: Option<String>,
    pub market_type: Option<String>,
    /// Profile name.
    pub strategy: String,
    /// Parameter overrides on top of the profile, e.g. `set = { base_size = 0.5 }`.
    pub set: BTreeMap<String, serde_json::Value>,
}

/// `*` matches any run of characters; everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first)
        || !text[first.len()..].ends_with(last)
        || text.len() < first.len() + last.len()
    {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

impl Route {
    pub fn matches(&self, p: &MarketParams) -> bool {
        self.slug.as_deref().is_none_or(|g| glob_match(g, &p.slug))
            && self
                .event
                .as_deref()
                .is_none_or(|g| glob_match(g, &p.event_slug))
            && self
                .league
                .as_deref()
                .is_none_or(|g| glob_match(g, &p.league))
            && self
                .market_type
                .as_deref()
                .is_none_or(|t| t == p.sports_market_type)
    }
}

/// The strategy a market resolved to, for logging.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub profile: String,
    /// Index into `routes`, if a route matched.
    pub route: Option<usize>,
    pub config: StrategyConfig,
}

impl Resolved {
    pub fn label(&self) -> String {
        match self.route {
            Some(i) => format!("{} (route {i})", self.profile),
            None => self.profile.clone(),
        }
    }
}

impl Config {
    pub fn league(&self, league: &str) -> LeagueConfig {
        self.leagues.get(league).cloned().unwrap_or_default()
    }

    fn profile(&self, name: &str) -> Result<StrategyConfig, String> {
        match self.strategies.get(name) {
            Some(cfg) => Ok(cfg.clone()),
            None if name == DEFAULT_PROFILE => Ok(StrategyConfig::default()),
            None => Err(format!("unknown strategy profile {name:?}")),
        }
    }

    /// The strategy for a market: the first matching route's profile plus its overrides, else `default`.
    pub fn strategy_for(&self, params: &MarketParams) -> Result<Resolved, String> {
        let route = self.routes.iter().position(|r| r.matches(params));
        let profile = route
            .map_or(DEFAULT_PROFILE, |i| self.routes[i].strategy.as_str())
            .to_string();
        let mut config = self.profile(&profile)?;
        if let Some(i) = route {
            config = config
                .with_overrides(&self.routes[i].set)
                .map_err(|e| format!("route {i}: {e}"))?;
        }
        Ok(Resolved {
            profile,
            route,
            config,
        })
    }

    /// Catch config mistakes at startup rather than when a market appears.
    pub fn validate(&self) -> Result<(), String> {
        // TOML allows `nan` and `inf`. Serialising turns them into JSON nulls, which then fail to
        // deserialise as numbers, so a round trip catches them in every section. Left in, `inf`
        // would make a limit like `max_position` saturate to "unlimited".
        let value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        serde_json::from_value::<Config>(value).map_err(|e| {
            format!("every number in the config must be finite (no nan or inf): {e}")
        })?;
        self.profile(DEFAULT_PROFILE)?;
        // The daily kill has to fire before the whole account budget is at risk, not with it.
        if self.risk.daily_loss_limit >= self.risk.max_account_loss {
            return Err(format!(
                "risk.daily_loss_limit ({}) must be below risk.max_account_loss ({})",
                self.risk.daily_loss_limit, self.risk.max_account_loss
            ));
        }
        for profile in self.strategies.values() {
            if let StrategyConfig::LatencyProbe(cfg) = profile {
                cfg.validate()?;
            }
        }
        for (i, r) in self.routes.iter().enumerate() {
            let base = self
                .profile(&r.strategy)
                .map_err(|e| format!("route {i}: {e}"))?;
            let resolved = base
                .with_overrides(&r.set)
                .map_err(|e| format!("route {i}: {e}"))?;
            if let StrategyConfig::LatencyProbe(cfg) = resolved {
                cfg.validate()?;
            }
            if r.slug.is_none()
                && r.event.is_none()
                && r.league.is_none()
                && r.market_type.is_none()
            {
                return Err(format!("route {i} has no matcher (slug, event, league or market_type); it would match everything"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RiskConfig {
    /// Net contracts per market.
    pub max_position: f64,
    /// Worst-case loss per market, position plus resting orders, in dollars.
    pub max_market_loss: f64,
    /// Worst-case loss across all markets, counting every resting order.
    pub max_account_loss: f64,
    pub max_order_size: f64,
    pub max_orders_total: usize,
    /// Realised plus marked loss in a UTC day that stops everything.
    pub daily_loss_limit: f64,
    /// Loss in one market since the session started that pauses it.
    pub market_loss_limit: f64,
    pub max_fills_per_market_per_min: usize,
    pub fill_pause_s: f64,
    pub max_rejects_per_min: usize,
    pub reject_pause_s: f64,
    pub max_book_moved_per_market_per_min: usize,
    pub market_pause_s: f64,
    /// Fat-finger: order price within this of fair…
    pub max_dev_from_fair: f64,
    /// …and within this of the venue's touch.
    pub book_band: f64,
    /// No successful game poll for this long while a match is live = stale.
    pub game_stale_s: f64,
    /// A trade with no book after it for this long: the market's book feed has stopped, so its
    /// quotes are pulled until a book arrives. 0: off.
    pub book_stale_after_trade_s: f64,
    /// While the match is live, no book at all for this long counts the same. 0: off.
    pub book_stale_live_s: f64,
    /// After a `Transient` reject (exchange slow), strategies are told the exchange is slow for this long.
    pub slow_exchange_s: f64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        RiskConfig {
            max_position: 20.0,
            max_market_loss: 10.0,
            max_account_loss: 40.0,
            max_order_size: 5.0,
            max_orders_total: 40,
            daily_loss_limit: 15.0,
            market_loss_limit: 5.0,
            max_fills_per_market_per_min: 10,
            fill_pause_s: 300.0,
            max_rejects_per_min: 10,
            reject_pause_s: 30.0,
            max_book_moved_per_market_per_min: 5,
            market_pause_s: 60.0,
            max_dev_from_fair: 0.16,
            book_band: 0.10,
            game_stale_s: 5.0,
            book_stale_after_trade_s: 10.0,
            book_stale_live_s: 90.0,
            slow_exchange_s: 60.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecConfig {
    /// Live shell only: proactive shared REST token bucket. HTTP 429 backoff
    /// remains active when false. Does not change simulation assumptions.
    pub rate_limit_enabled: bool,
    /// Accepted by REST but no ack this long → in doubt, query open orders.
    pub ack_timeout_s: f64,
    /// No cancel/replace outcome this long → look the order up.
    pub cancel_timeout_s: f64,
    /// An execution for an unknown id is matched to a pending order by its fields after this long.
    pub unmatched_attach_s: f64,
    /// …and declared an unknown order after this long.
    pub unknown_order_s: f64,
    pub reconcile_positions_s: f64,
    pub reconcile_balance_s: f64,
    /// Timeouts, quote-expiry refresh, reconciliation and loss checks run this often.
    pub housekeeping_ms: u64,
    /// Live shell only: after each core pass, poll the inbox this long before sleeping, so input
    /// that arrives meanwhile is picked up without a thread wake-up, on a CPU that is still warm.
    /// 0: sleep at once. The core never sleeps more than 1 s at a time, so ≥ 1_000_000 means it
    /// never sleeps and keeps one CPU fully busy.
    pub core_spin_us: u64,
    /// Backstop: re-decide every market this often even with no input, e.g. after account-wide
    /// room frees up in another market (0: never). Time boundaries are handled exactly.
    pub redecide_all_s: f64,
    /// Pre-match quotes expire this long after the scheduled start.
    pub pre_match_gtd_after_start_s: f64,
    /// In-play quotes expire after this long and are refreshed while wanted.
    pub inplay_gtd_s: f64,
    pub gtd_refresh_before_s: f64,
    /// A live quote moves only when its target is at least this many ticks away (size changes
    /// still move it). 1: any price change moves it. 2 halved order requests in the 10-28
    /// two-phase backtests (mid and VWAP, dev and holdout) with PnL level or better.
    pub reprice_min_ticks: u32,
}

impl Default for ExecConfig {
    fn default() -> Self {
        ExecConfig {
            rate_limit_enabled: true,
            ack_timeout_s: 3.0,
            cancel_timeout_s: 2.0,
            unmatched_attach_s: 2.0,
            unknown_order_s: 5.0,
            reconcile_positions_s: 60.0,
            reconcile_balance_s: 300.0,
            housekeeping_ms: 250,
            core_spin_us: 0,
            redecide_all_s: 5.0,
            pre_match_gtd_after_start_s: 300.0,
            inplay_gtd_s: 120.0,
            gtd_refresh_before_s: 30.0,
            reprice_min_ticks: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LeagueConfig {
    /// Match format: first to this many legs. Unknown → no in-play model (consensus only).
    pub legs_to_win: Option<u16>,
    /// Probability that a player wins a leg they throw first in.
    pub hold_rate: f64,
    /// Skip the live orientation check for this league.
    pub orientation_verified: bool,
}

impl Default for LeagueConfig {
    fn default() -> Self {
        LeagueConfig {
            legs_to_win: None,
            hold_rate: 0.6,
            orientation_verified: false,
        }
    }
}

pub fn secs(s: f64) -> u64 {
    (s.max(0.0) * 1e9) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::{PreMatchConfig, StrategyConfig};
    use std::sync::Arc;

    fn market(slug: &str, league: &str) -> MarketParams {
        MarketParams {
            slug: Arc::from(slug),
            title: String::new(),
            event_id: "e".into(),
            event_slug: slug.trim_start_matches("aec-").into(),
            league: league.into(),
            sports_market_type: "darts_match_winner".into(),
            tick: pm_types::Px(100),
            min_qty: pm_types::Qty(100),
            fee_coefficient: 0.0,
            game_start_ns: None,
            yes_name: None,
            no_name: None,
            yes_ordering: None,
            is_futures: false,
        }
    }

    fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn non_finite_numbers_are_rejected() {
        assert!(parse("[risk]\nmax_position = 5.0\n").is_ok());
        for text in [
            "[risk]\nmax_position = inf\n",
            "[risk]\nbook_band = nan\n",
            "[strategies.default]\nkind = \"pre_match\"\nsize = -inf\n",
        ] {
            let err = parse(text).unwrap_err();
            assert!(err.contains("finite"), "{text}: {err}");
        }
    }

    fn size(r: &Resolved) -> f64 {
        match &r.config {
            StrategyConfig::PreMatch(c) => c.size,
            _ => 0.0,
        }
    }

    #[test]
    fn example_config_parses_and_validates() {
        let cfg = parse(include_str!("../../../config/pmbot.example.toml")).unwrap();
        assert_eq!(cfg.markets.types, vec!["darts_match_winner".to_string()]);
        assert_eq!(cfg.strategies.len(), 3);
        assert!(cfg.execution.rate_limit_enabled);
    }

    #[test]
    fn two_phase_vwap_live_config_matches_the_mid_profile_except_for_its_center() {
        let mid = parse(include_str!("../../../config/pmbot-two-phase-live.toml")).unwrap();
        let vwap = parse(include_str!(
            "../../../config/pmbot-two-phase-vwap-live.toml"
        ))
        .unwrap();
        let mut expected = serde_json::to_value(mid).unwrap();
        expected["strategies"]["default"]["kind"] = "two_phase_vwap".into();
        assert_eq!(serde_json::to_value(&vwap).unwrap(), expected);
        let strategy = vwap.strategies["default"].build();
        assert!(strategy.uses_book_depth());
        assert!(!strategy.input_driven());
        let overridden = vwap.strategies["default"]
            .with_overrides(&BTreeMap::from([("size".into(), serde_json::json!(2.0))]))
            .unwrap();
        assert_eq!(overridden.kind(), "two_phase_vwap");
        let StrategyConfig::TwoPhaseVwap(cfg) = overridden else {
            panic!()
        };
        assert_eq!(cfg.size, 2.0);
    }

    #[test]
    fn empty_config_uses_built_in_default_pre_match() {
        let cfg = parse("").unwrap();
        assert!(
            cfg.execution.rate_limit_enabled,
            "limiter remains enabled by default"
        );
        let r = cfg
            .strategy_for(&market("aec-pdcdarts-a-b", "pdcdarts"))
            .unwrap();
        assert_eq!((r.profile.as_str(), r.route), ("default", None));
        assert_eq!(
            r.config,
            StrategyConfig::PreMatch(PreMatchConfig::default())
        );
    }

    #[test]
    fn routes_pick_per_ticker_first_match_wins() {
        let cfg = parse(
            r#"
            [strategies.default]
            kind = "pre_match"
            [strategies.off]
            kind = "off"
            [[routes]]
            slug = "aec-pdcdarts-luklit-wilcon-*"
            strategy = "default"
            set = { size = 2.0 }
            [[routes]]
            slug = "*-luklit-*"
            strategy = "off"
            [[routes]]
            league = "modus*"
            strategy = "off"
        "#,
        )
        .unwrap();
        let exact = cfg
            .strategy_for(&market("aec-pdcdarts-luklit-wilcon-2026-10-23", "pdcdarts"))
            .unwrap();
        assert_eq!((exact.route, size(&exact)), (Some(0), 2.0));
        let player = cfg
            .strategy_for(&market("aec-pdcdarts-joncla-luklit-2026-10-24", "pdcdarts"))
            .unwrap();
        assert_eq!(
            (player.route, &player.config),
            (Some(1), &StrategyConfig::Off)
        );
        let league = cfg
            .strategy_for(&market(
                "aec-modus-a-b-2026-10-23",
                "modus-super-series-2026",
            ))
            .unwrap();
        assert_eq!(league.route, Some(2));
        let other = cfg
            .strategy_for(&market("aec-pdcdarts-a-b-2026-10-23", "pdcdarts"))
            .unwrap();
        assert_eq!(
            (other.route, size(&other)),
            (None, PreMatchConfig::default().size)
        );
    }

    #[test]
    fn mistakes_fail_validation() {
        let err = |text: &str| parse(text).unwrap_err();
        assert!(err("[[routes]]\nslug = \"x\"\nstrategy = \"nope\"\n")
            .contains("unknown strategy profile"));
        assert!(
            err("[[routes]]\nslug = \"x\"\nstrategy = \"default\"\nset = { sise = 1.0 }\n")
                .contains("sise")
        );
        assert!(err("[[routes]]\nstrategy = \"default\"\n").contains("no matcher"));
        assert!(err("[[routes]]\nslugs = \"x\"\nstrategy = \"default\"\n").contains("slugs"));
        assert!(err("[risk]\nmax_postion = 1.0\n").contains("max_postion"));
        assert!(
            err("[strategy]\nsize = 1.0\n").contains("strategy"),
            "old [strategy] section is rejected"
        );
    }

    #[test]
    fn glob() {
        assert!(
            glob_match("a*", "abc")
                && glob_match("*c", "abc")
                && glob_match("a*c", "abc")
                && glob_match("*b*", "abc")
        );
        assert!(glob_match("abc", "abc") && !glob_match("abd", "abc"));
        assert!(!glob_match("ab*bc", "abc"), "parts can't overlap");
        assert!(glob_match("*", "") && glob_match("a**c", "ac"));
    }
}
