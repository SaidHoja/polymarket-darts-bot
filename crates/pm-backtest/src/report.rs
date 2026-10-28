//! Per-market results and grouped summaries. Field definitions: `docs/metrics.md`.

use std::fmt::Write as _;

use pm_types::time::parse_rfc3339_ns;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Serialize;

use crate::book::Quality;
use crate::data::MarketMeta;
use crate::sim::{FillRow, Pnl, SimStats};

/// Development data ends before the August–September gap; the holdout starts after it.
pub const DEV_END: &str = "2026-08-20T00:00:00Z";
pub const HOLDOUT_START: &str = "2026-09-29T00:00:00Z";

pub fn split_of(meta: &MarketMeta, first_ns: i64) -> &'static str {
    let t = meta.game_start_ns().unwrap_or(first_ns);
    if t < parse_rfc3339_ns(DEV_END).unwrap() {
        "dev"
    } else if t >= parse_rfc3339_ns(HOLDOUT_START).unwrap() {
        "holdout"
    } else {
        "gap"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MarketResult {
    pub slug: String,
    pub league: String,
    pub split: &'static str,
    pub game_start: Option<String>,
    pub settlement: Option<f64>,
    pub excluded: Option<String>,
    pub quality: Quality,
    pub pnl: Option<Pnl>,
    pub fills: usize,
    pub contracts: f64,
    pub stats: SimStats,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Group {
    pub name: String,
    pub markets: usize,
    pub markets_with_pnl: usize,
    pub pnl: f64,
    /// 95% bootstrap interval of total PnL, resampling markets.
    pub pnl_ci95: (f64, f64),
    pub pnl_per_market: f64,
    pub spread: f64,
    pub inventory: f64,
    pub fees: f64,
    pub fills: usize,
    pub contracts: f64,
    /// Quantity-weighted mean markout per contract (dollars).
    pub mo_1s: Option<f64>,
    pub mo_10s: Option<f64>,
    pub mo_60s: Option<f64>,
    pub mo_settle: Option<f64>,
    pub fills_by_how: std::collections::BTreeMap<&'static str, usize>,
    pub order_requests: u64,
    pub read_requests: u64,
    pub req_per_s_max: u32,
    pub rate_limited_s: u64,
    pub mean_uptime: f64,
    pub max_drawdown: f64,
    pub rejects: u64,
}

fn bootstrap(values: &[f64], seed: u64) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let mut sums: Vec<f64> = (0..2000)
        .map(|_| {
            (0..values.len())
                .map(|_| values[rng.gen_range(0..values.len())])
                .sum()
        })
        .collect();
    sums.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (
        sums[(0.025 * sums.len() as f64) as usize],
        sums[(0.975 * sums.len() as f64) as usize],
    )
}

pub fn summarize(name: &str, results: &[&MarketResult], fills: &[&FillRow], seed: u64) -> Group {
    let used: Vec<&&MarketResult> = results.iter().filter(|r| r.excluded.is_none()).collect();
    let pnls: Vec<f64> = used
        .iter()
        .filter_map(|r| r.pnl.as_ref().map(|p| p.total))
        .collect();
    let mut g = Group {
        name: name.to_string(),
        markets: used.len(),
        markets_with_pnl: pnls.len(),
        ..Group::default()
    };
    g.pnl = pnls.iter().sum();
    g.pnl_ci95 = bootstrap(&pnls, seed);
    g.pnl_per_market = if pnls.is_empty() {
        0.0
    } else {
        g.pnl / pnls.len() as f64
    };
    for r in &used {
        if let Some(p) = &r.pnl {
            g.spread += p.spread;
            g.inventory += p.inventory;
            g.fees += p.fees;
        }
        g.order_requests += r.stats.order_requests;
        g.read_requests += r.stats.read_requests;
        g.req_per_s_max = g.req_per_s_max.max(r.stats.req_per_s_max);
        g.rate_limited_s += r.stats.rate_limited_s;
        g.mean_uptime += (r.stats.uptime_bid + r.stats.uptime_ask) / 2.0 / used.len() as f64;
        g.max_drawdown = g.max_drawdown.max(r.stats.max_drawdown);
        g.rejects += r.stats.rejects.values().sum::<u64>();
    }
    g.fills = fills.len();
    g.contracts = fills.iter().map(|f| f.qty).sum();
    let wmean = |pick: fn(&FillRow) -> Option<f64>| {
        let (num, den) = fills
            .iter()
            .filter_map(|f| pick(f).map(|m| (m * f.qty, f.qty)))
            .fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
        (den > 0.0).then(|| num / den)
    };
    g.mo_1s = wmean(|f| f.mo_1s);
    g.mo_10s = wmean(|f| f.mo_10s);
    g.mo_60s = wmean(|f| f.mo_60s);
    g.mo_settle = wmean(|f| f.mo_settle);
    for f in fills {
        *g.fills_by_how.entry(f.how).or_default() += 1;
    }
    g
}

type Filter = Box<dyn Fn(&MarketResult) -> bool>;

/// Groups the plan asks for: everything, per date window (split), per league.
pub fn groups(results: &[MarketResult], fills: &[FillRow], seed: u64) -> Vec<Group> {
    let mut keys: Vec<(String, Filter)> = vec![("all".into(), Box::new(|_| true))];
    for s in ["dev", "holdout"] {
        // Named by the scheduled starts it covers, e.g. "dev 08-04..08-19".
        let days: Vec<&str> = results
            .iter()
            .filter(|r| r.split == s)
            .filter_map(|r| r.game_start.as_deref()?.get(5..10))
            .collect();
        if let (Some(first), Some(last)) = (days.iter().min(), days.iter().max()) {
            keys.push((
                format!("{s} {first}..{last}"),
                Box::new(move |r: &MarketResult| r.split == s),
            ));
        } else if results.iter().any(|r| r.split == s) {
            keys.push((
                s.to_string(),
                Box::new(move |r: &MarketResult| r.split == s),
            ));
        }
    }
    let mut leagues: Vec<String> = results.iter().map(|r| r.league.clone()).collect();
    leagues.sort();
    leagues.dedup();
    for l in leagues {
        let name = format!("league={l}");
        keys.push((name, Box::new(move |r: &MarketResult| r.league == l)));
    }
    keys.into_iter()
        .map(|(name, keep)| {
            let rs: Vec<&MarketResult> = results.iter().filter(|r| keep(r)).collect();
            let slugs: std::collections::HashSet<&str> = rs
                .iter()
                .filter(|r| r.excluded.is_none())
                .map(|r| r.slug.as_str())
                .collect();
            let fs: Vec<&FillRow> = fills
                .iter()
                .filter(|f| slugs.contains(f.slug.as_str()))
                .collect();
            summarize(&name, &rs, &fs, seed)
        })
        .collect()
}

fn opt(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{:+.4}", v))
}

pub fn table(variant: &str, groups: &[Group]) -> String {
    let mut out = format!("\n== {variant}\n");
    let _ = writeln!(
        out,
        "{:<22} {:>4} {:>9} {:>21} {:>8} {:>8} {:>6} {:>6} {:>8} {:>8} {:>8} {:>7} {:>6}",
        "group",
        "n",
        "pnl $",
        "95% CI",
        "spread",
        "invent.",
        "fees",
        "fills",
        "mo10s",
        "mo60s",
        "mo_set",
        "uptime",
        "orders"
    );
    for g in groups {
        let _ = writeln!(out, "{:<22} {:>4} {:>9.2} {:>21} {:>8.2} {:>8.2} {:>6.2} {:>6} {:>8} {:>8} {:>8} {:>6.0}% {:>6}",
            g.name, g.markets_with_pnl, g.pnl, format!("[{:.2}, {:.2}]", g.pnl_ci95.0, g.pnl_ci95.1), g.spread, g.inventory, g.fees,
            g.fills, opt(g.mo_10s), opt(g.mo_60s), opt(g.mo_settle), g.mean_uptime * 100.0, g.order_requests);
    }
    out
}

pub fn markets_csv(results: &[MarketResult]) -> String {
    let mut out = String::from("slug,league,split,settlement,excluded,trade_agreement,fills,contracts,pnl,spread,inventory,fees,order_requests,read_requests,req_per_s_max,rate_limited_s,uptime_bid,uptime_ask,max_abs_inventory,inventory_at_start,max_drawdown,replay_hours,command_hash\n");
    for r in results {
        let p = r.pnl.clone().unwrap_or_default();
        let has = r.pnl.is_some();
        let f = |v: f64| {
            if has {
                format!("{v:.4}")
            } else {
                String::new()
            }
        };
        let _ = writeln!(
            out,
            "{},{},{},{},{},{:.4},{},{},{},{},{},{},{},{},{},{},{:.3},{:.3},{},{},{:.4},{:.2},{}",
            r.slug,
            r.league,
            r.split,
            r.settlement.map_or(String::new(), |s| s.to_string()),
            r.excluded.clone().unwrap_or_default(),
            r.quality.trade_agreement(),
            r.fills,
            r.contracts,
            f(p.total),
            f(p.spread),
            f(p.inventory),
            f(p.fees),
            r.stats.order_requests,
            r.stats.read_requests,
            r.stats.req_per_s_max,
            r.stats.rate_limited_s,
            r.stats.uptime_bid,
            r.stats.uptime_ask,
            r.stats.max_abs_inventory,
            r.stats
                .inventory_at_start
                .map_or(String::new(), |v| v.to_string()),
            r.stats.max_drawdown,
            r.stats.replay_hours,
            r.stats.command_hash
        );
    }
    out
}

pub fn fills_csv(fills: &[FillRow]) -> String {
    let mut out = String::from(
        "slug,t_ns,side,px,qty,fee,how,queue_ahead,mid,mo_1s,mo_10s,mo_60s,mo_settle\n",
    );
    let o = |v: Option<f64>| v.map_or(String::new(), |v| format!("{v:.4}"));
    for f in fills {
        let _ = writeln!(
            out,
            "{},{},{},{},{},{:.6},{},{},{},{},{},{},{}",
            f.slug,
            f.t_ns,
            f.side,
            f.px,
            f.qty,
            f.fee,
            f.how,
            f.queue_ahead,
            o(f.mid),
            o(f.mo_1s),
            o(f.mo_10s),
            o(f.mo_60s),
            o(f.mo_settle)
        );
    }
    out
}
