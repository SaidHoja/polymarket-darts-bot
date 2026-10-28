//! `pm-backtest`: replay history through `pm-core` against a simulated exchange.
//!
//! ```sh
//! venv/bin/python data-analysis/export_backtest.py        # once: Parquet -> data-analysis/backtest/
//! cargo run --release -p pm-backtest -- audit
//! cargo run --release -p pm-backtest -- run --config config/backtest.example.toml \
//!     --strategy off,m2_10c_pre,m1_15c --fill-model base,pessimistic
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use clap::{Parser, Subcommand};
use pm_backtest::book::{audit, Quality};
use pm_backtest::data::{load_manifest, load_rows, messages, MarketMeta, Message, Row};
use pm_backtest::report::{self, MarketResult};
use pm_backtest::sim::{FillModel, FillRow, Sim, SimParams};
use pm_core::{Config, StrategyConfig};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "pm-backtest",
    about = "Replay Polymarket US darts history through pm-core"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct DataArgs {
    /// Export from data-analysis/export_backtest.py.
    #[arg(long, default_value = "data-analysis/backtest")]
    data: PathBuf,
    /// Only these markets (repeatable).
    #[arg(long)]
    market: Vec<String>,
    /// all, dev (before the Aug–Sep gap) or holdout (after it). Results are reported for all and per window either way.
    #[arg(long, default_value = "all")]
    split: String,
    /// Markets whose trades agree with the rebuilt book less often than this are excluded.
    #[arg(long, default_value_t = 0.99)]
    min_agreement: f64,
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

#[derive(Subcommand)]
enum Cmd {
    /// Rebuild every book without trading and report data quality.
    Audit {
        #[command(flatten)]
        data: DataArgs,
    },
    /// Run strategies. Every combination of --strategy × --fill-model × --lag is one variant.
    Run {
        #[command(flatten)]
        data: DataArgs,
        /// pm-core config (strategy profiles, risk, execution); same format as the live bot.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Profiles from the config to force on every market (comma-separated). `config` = the config's own routes.
        #[arg(long, default_value = "config", value_delimiter = ',')]
        strategy: Vec<String>,
        #[arg(long, default_value = "base", value_delimiter = ',')]
        fill_model: Vec<String>,
        /// Latency multipliers (sensitivity: 0.5,1,2,4).
        #[arg(long, default_value = "1", value_delimiter = ',')]
        lag: Vec<f64>,
        /// Don't synthesize game state (live from the scheduled start).
        #[arg(long)]
        no_game_state: bool,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value = "backtests")]
        out: PathBuf,
    },
}

struct Loaded {
    meta: MarketMeta,
    rows: Vec<Row>,
    msgs: Vec<Message>,
    quality: Quality,
    split: &'static str,
    excluded: Option<String>,
}

fn parallel<T: Send + Sync, R: Send>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let threads = if threads == 0 {
        std::thread::available_parallelism().map_or(4, |n| n.get())
    } else {
        threads
    };
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..threads.min(items.len().max(1)) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= items.len() {
                    break;
                }
                let r = f(&items[i]);
                out.lock().unwrap()[i] = Some(r);
            });
        }
    });
    out.into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.unwrap())
        .collect()
}

fn load(args: &DataArgs) -> Result<(String, Vec<Loaded>), String> {
    let manifest = load_manifest(&args.data)?;
    if !matches!(args.split.as_str(), "dev" | "holdout" | "all") {
        return Err(format!("--split {:?}: dev | holdout | all", args.split));
    }
    if args.split == "holdout" {
        eprintln!("warning: holdout split. Parameters should be frozen on dev before this run, and it should be run once.");
    }
    let metas: Vec<MarketMeta> = manifest
        .markets
        .into_iter()
        .filter(|m| m.trades > 0 && (args.market.is_empty() || args.market.contains(&m.slug)))
        .collect();
    let loaded: Vec<Result<Loaded, String>> = parallel(&metas, args.threads, |meta| {
        let rows = load_rows(&args.data, &meta.slug)?;
        let msgs = messages(&rows);
        let (quality, _) = audit(&rows, &msgs);
        let split = report::split_of(meta, rows.first().map_or(0, |r| r.rx_ns));
        let excluded = if quality.trade_agreement() < args.min_agreement {
            Some(format!(
                "trade agreement {:.3} < {}",
                quality.trade_agreement(),
                args.min_agreement
            ))
        } else {
            None
        };
        Ok(Loaded {
            meta: meta.clone(),
            rows,
            msgs,
            quality,
            split,
            excluded,
        })
    });
    let mut out = Vec::new();
    for l in loaded {
        let l = l?;
        if args.split == "all" || l.split == args.split {
            out.push(l);
        }
    }
    Ok((manifest.source_sha256, out))
}

fn audit_cmd(args: DataArgs) -> Result<(), String> {
    let (_, markets) = load(&args)?;
    let mut total = Quality::default();
    println!(
        "{:<45} {:>8} {:>6} {:>7} {:>5} {:>6} {:>6} {:>7} {:>5}",
        "market", "messages", "trades", "agree", "snaps", "mismat", "touch", "crossed", "gaps"
    );
    for m in &markets {
        let q = &m.quality;
        println!(
            "{:<45} {:>8} {:>6} {:>7.3} {:>5} {:>6} {:>6} {:>7} {:>5}{}",
            m.meta.slug,
            q.messages,
            q.trades,
            q.trade_agreement(),
            q.snapshots,
            q.snapshot_mismatches,
            q.snapshot_touch_mismatches,
            q.crossed,
            q.gaps_over_60s,
            if m.excluded.is_some() {
                "  EXCLUDED"
            } else {
                ""
            }
        );
        total.messages += q.messages;
        total.trades += q.trades;
        total.trades_at_book += q.trades_at_book;
        total.trades_no_aggressor += q.trades_no_aggressor;
        total.snapshots += q.snapshots;
        total.snapshot_mismatches += q.snapshot_mismatches;
        total.snapshot_touch_mismatches += q.snapshot_touch_mismatches;
        total.snapshots_after_reset += q.snapshots_after_reset;
        total.gaps_over_60s += q.gaps_over_60s;
        total.crossed += q.crossed;
        total.unknown_changes += q.unknown_changes;
    }
    let excluded = markets.iter().filter(|m| m.excluded.is_some()).count();
    println!(
        "\n{} markets, {} excluded (< {} trade agreement)",
        markets.len(),
        excluded,
        args.min_agreement
    );
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"total": total, "trade_agreement": total.trade_agreement()})
        )
        .unwrap()
    );
    Ok(())
}

fn strategy_config(base: &Config, name: &str) -> Result<Config, String> {
    let mut cfg = base.clone();
    if name != "config" {
        let profile: StrategyConfig = base.strategies.get(name).cloned().ok_or_else(|| {
            format!(
                "strategy {name:?} isn't a profile in the config (have: {:?})",
                base.strategies.keys().collect::<Vec<_>>()
            )
        })?;
        cfg.routes.clear();
        cfg.strategies
            .insert(pm_core::config::DEFAULT_PROFILE.into(), profile);
    }
    cfg.validate()?;
    Ok(cfg)
}

fn git_revision() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[allow(clippy::too_many_arguments)]
fn run_cmd(
    data: DataArgs,
    config: Option<PathBuf>,
    strategies: Vec<String>,
    fill_models: Vec<String>,
    lags: Vec<f64>,
    no_game_state: bool,
    seed: u64,
    out: PathBuf,
) -> Result<(), String> {
    let base: Config = match &config {
        Some(path) => toml::from_str(
            &std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?,
        )
        .map_err(|e| format!("{}: {e}", path.display()))
        .and_then(|cfg: Config| {
            cfg.validate()
                .map(|()| cfg)
                .map_err(|e| format!("{}: {e}", path.display()))
        })?,
        None => Config::default(),
    };
    let fill_models: Vec<FillModel> = fill_models
        .iter()
        .map(|s| FillModel::parse(s))
        .collect::<Result<_, _>>()?;
    let cfgs: Vec<(String, Config)> = strategies
        .iter()
        .map(|s| strategy_config(&base, s).map(|c| (s.clone(), c)))
        .collect::<Result<_, _>>()?;

    let started = Instant::now();
    let (checksum, markets) = load(&data)?;
    eprintln!(
        "loaded {} markets ({} excluded) in {:.1}s",
        markets.len(),
        markets.iter().filter(|m| m.excluded.is_some()).count(),
        started.elapsed().as_secs_f64()
    );
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut sweep = String::from("variant,strategy,fill_model,lag,markets,pnl,ci_lo,ci_hi,spread,inventory,fees,fills,mo_10s,mo_60s,mo_settle,order_requests\n");

    for (name, cfg) in &cfgs {
        for fm in &fill_models {
            for lag in &lags {
                let variant = format!("{name}__{}__lag{lag}", fm.name());
                let params = SimParams {
                    fill_model: *fm,
                    lag_mult: *lag,
                    game_state: !no_game_state,
                    seed,
                    ..SimParams::default()
                };
                let t = Instant::now();
                let outputs: Vec<(MarketResult, Vec<FillRow>)> =
                    parallel(&markets, data.threads, |m| run_one(m, cfg, &params));
                let (results, fills): (Vec<MarketResult>, Vec<Vec<FillRow>>) =
                    outputs.into_iter().unzip();
                let fills: Vec<FillRow> = fills.into_iter().flatten().collect();
                let groups = report::groups(&results, &fills, seed);
                print!("{}", report::table(&variant, &groups));
                eprintln!("  ({:.1}s)", t.elapsed().as_secs_f64());

                let dir = out.join(&variant);
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let summary = json!({
                    "variant": variant, "strategy": name, "seed": seed, "data_sha256": checksum, "git_revision": git_revision(),
                    "split": data.split, "min_agreement": data.min_agreement, "sim": params, "config": cfg, "groups": groups,
                    "excluded": results.iter().filter_map(|r| r.excluded.as_ref().map(|e| json!({"slug": r.slug, "reason": e}))).collect::<Vec<_>>(),
                });
                write(
                    &dir.join("summary.json"),
                    &serde_json::to_string_pretty(&summary).unwrap(),
                )?;
                write(&dir.join("markets.csv"), &report::markets_csv(&results))?;
                write(&dir.join("fills.csv"), &report::fills_csv(&fills))?;
                let all = &groups[0];
                sweep.push_str(&format!("{variant},{name},{},{lag},{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{},{},{},{},{}\n",
                    fm.name(), all.markets_with_pnl, all.pnl, all.pnl_ci95.0, all.pnl_ci95.1, all.spread, all.inventory, all.fees, all.fills,
                    all.mo_10s.map_or(String::new(), |v| format!("{v:.5}")), all.mo_60s.map_or(String::new(), |v| format!("{v:.5}")),
                    all.mo_settle.map_or(String::new(), |v| format!("{v:.5}")), all.order_requests));
            }
        }
    }
    write(&out.join("sweep.csv"), &sweep)?;
    eprintln!(
        "\nwrote {} (total {:.1}s)",
        out.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn run_one(m: &Loaded, cfg: &Config, params: &SimParams) -> (MarketResult, Vec<FillRow>) {
    let mut result = MarketResult {
        slug: m.meta.slug.clone(),
        league: m.meta.league.clone(),
        split: m.split,
        game_start: m.meta.game_start.clone(),
        settlement: m.meta.settlement,
        excluded: m.excluded.clone(),
        quality: m.quality.clone(),
        pnl: None,
        fills: 0,
        contracts: 0.0,
        stats: Default::default(),
    };
    if m.excluded.is_some() {
        return (result, Vec::new());
    }
    let out = Sim::new(&m.meta, &m.rows, &m.msgs, cfg.clone(), params.clone()).run();
    result.pnl = out.pnl;
    result.fills = out.fills.len();
    result.contracts = out.fills.iter().map(|f| f.qty).sum();
    result.stats = out.stats;
    (result, out.fills)
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn main() {
    let result = match Cli::parse().cmd {
        Cmd::Audit { data } => audit_cmd(data),
        Cmd::Run {
            data,
            config,
            strategy,
            fill_model,
            lag,
            no_game_state,
            seed,
            out,
        } => run_cmd(
            data,
            config,
            strategy,
            fill_model,
            lag,
            no_game_state,
            seed,
            out,
        ),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
