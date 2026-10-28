//! `pmbot`: the live shell around `pm-core`.
//!
//! I/O runs on a tokio runtime (feeds, gateway, admin socket); the core runs on
//! one dedicated thread that drains the inbox, ticks, and hands commands to the
//! gateway. Modes:
//! - `record`: feeds and journals only, no core, no orders.
//! - `paper` (default): real market data, the core decides, a local gateway acks without filling.
//! - `live`: real orders.

mod admin;
#[cfg(test)]
mod bench;
mod feeds;
mod gateway;
mod inbox;
mod journal;
mod metrics;
mod priority;
mod risk_file;
mod timing;
mod ui;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use pm_core::{Clock, Config, Core, TickOutput};
use pm_types::{AdminCmd, Event};
use pm_venue::{load_credentials, Budget, RateLimiter, RestClient, Signer};
use serde_json::json;
use tokio::sync::{mpsc, watch, Notify};

use crate::gateway::Gate;
use crate::inbox::Inbox;
use crate::journal::{wall_ns, Journal};
use crate::metrics::{Metrics, Stage};

/// Planned REST rate: the venue allows 20 req/s (maybe 25); we run at 15.
const REST_RATE: f64 = 15.0;
/// Core stuck this long with events waiting → the watchdog cancels everything.
const WATCHDOG_STALL: Duration = Duration::from_secs(2);
/// Longest wait between the watchdog's cancel-all attempts while they keep failing.
const WATCHDOG_RETRY_MAX: Duration = Duration::from_secs(30);
/// At shutdown: how long to wait for creates and modifies already sent, for the core thread to
/// stop, and for the journal to be written out.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
const STATUS_EVERY: Duration = Duration::from_secs(10);
const METRICS_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    Record,
    Paper,
    Live,
}

#[derive(Parser, Debug)]
#[command(name = "pmbot", about = "Polymarket US darts market maker")]
struct Args {
    /// TOML config (strategy, risk, execution, leagues). Defaults apply when absent.
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "paper")]
    mode: Mode,
    #[arg(long, default_value = ".env")]
    env_file: PathBuf,
    /// Raw frames and core ticks, one JSONL file per stream per day. "none" disables.
    #[arg(long, default_value = "captures")]
    journal_dir: String,
    /// Admin socket directory.
    #[arg(long, default_value = "run")]
    run_dir: PathBuf,
    /// Risk state kept across restarts (daily kill, the day's PnL, market losses, indefinite pauses).
    /// Default `<run-dir>/risk-state-<mode>.json`; "none" disables, so a restart clears them.
    #[arg(long)]
    risk_state: Option<String>,
    /// Also discover futures (outright) markets.
    #[arg(long)]
    include_futures: bool,
    /// Serve the operator web UI here, e.g. 127.0.0.1:8787. Off when omitted.
    #[arg(long)]
    ui: Option<SocketAddr>,
    /// Allow `--ui` on a non-loopback address. The UI can cancel orders and shut the bot down.
    #[arg(long)]
    ui_allow_remote: bool,
}

fn load_config(path: Option<&Path>) -> Result<Config, String> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    cfg.validate()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(cfg)
}

/// Ctrl-C, SIGTERM (`kill`, systemd) or SIGHUP (the terminal closed) all stop the bot the same way.
async fn stop_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let (Ok(mut term), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            return tokio::select! {
                _ = tokio::signal::ctrl_c() => "Ctrl-C",
                _ = term.recv() => "SIGTERM",
                _ = hup.recv() => "SIGHUP",
            };
        }
        tracing::error!("can't listen for SIGTERM/SIGHUP; only Ctrl-C cancels orders before exit");
    }
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl-C"
}

/// Cancel every order on the account and confirm it with an open-orders read. First waits (up to
/// `SHUTDOWN_WAIT`) for creates and modifies already sent, since a cancel-all doesn't cover an order
/// the venue hasn't accepted yet; `gate` must already be halted. Three rounds, then gives up.
async fn cancel_everything(rest: &RestClient, gate: &Gate) -> Result<(), String> {
    if !gate.idle(SHUTDOWN_WAIT).await {
        tracing::warn!(
            "creates or modifies still in flight after {SHUTDOWN_WAIT:?}; cancelling anyway"
        );
    }
    for round in 1..=3u32 {
        match tokio::time::timeout(SHUTDOWN_WAIT, rest.cancel_all(&[], Budget::Prepaid)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("cancel-all failed: {e}"),
            Err(_) => tracing::error!("cancel-all timed out"),
        }
        match tokio::time::timeout(SHUTDOWN_WAIT, rest.open_orders(&[], Budget::Prepaid)).await {
            Ok(Ok(orders)) if orders.is_empty() => return Ok(()),
            Ok(Ok(orders)) => tracing::warn!(
                "{} orders still open after cancel-all (round {round})",
                orders.len()
            ),
            Ok(Err(e)) => tracing::warn!("open-orders check failed: {e}"),
            Err(_) => tracing::warn!("open-orders check timed out"),
        }
        tokio::time::sleep(Duration::from_millis(250 * u64::from(round))).await;
    }
    Err("cancel-all not confirmed: orders may still be open (check the venue's open orders)".into())
}

/// Set when the core thread ends, however it ends (return or panic).
struct CoreExit(Arc<AtomicBool>, Arc<Notify>);

impl Drop for CoreExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
        self.1.notify_one();
    }
}

fn main() {
    // Log lines go to a background writer thread, so a slow stdout (e.g. a stalled `| tee`)
    // never blocks the core or the feeds. If the writer falls 128k lines behind, new lines are
    // dropped and counted (`log_dropped` on the metrics page) instead of waiting.
    let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());
    let log_dropped = writer.error_counter();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(writer)
        .init();
    let args = Args::parse();
    let result = run(args, log_dropped);
    if let Err(e) = &result {
        tracing::error!("{e}");
    }
    // Flushes the queued lines; `process::exit` would skip it.
    drop(guard);
    if result.is_err() {
        std::process::exit(1);
    }
}

fn run(
    args: Args,
    log_dropped: tracing_appender::non_blocking::ErrorCounter,
) -> Result<(), String> {
    let cfg = load_config(args.config.as_deref())?;
    if let Some(addr) = args.ui {
        if !addr.ip().is_loopback() && !args.ui_allow_remote {
            return Err(format!(
                "--ui {addr} is not a loopback address; pass --ui-allow-remote to serve it anyway"
            ));
        }
    }
    let credentials = load_credentials(&args.env_file)?;
    let signer = Arc::new(Signer::new(&credentials)?);
    let watchdog_signer = signer.clone();
    drop(credentials);
    let journal = if args.journal_dir == "none" {
        Journal::disabled()
    } else {
        Journal::start(PathBuf::from(&args.journal_dir)).map_err(|e| format!("journal: {e}"))?
    };
    std::fs::create_dir_all(&args.run_dir)
        .map_err(|e| format!("{}: {e}", args.run_dir.display()))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let rest = Arc::new(RestClient::new(
        Some(signer.clone()),
        RateLimiter::with_enabled(REST_RATE, cfg.execution.rate_limit_enabled)?,
    )?);
    let inbox = Arc::new(Inbox::new());
    let start = Instant::now();
    let (slugs_tx, _) = watch::channel(Vec::new());
    let refresh = Arc::new(Notify::new());
    let metrics = Arc::new(Metrics::default());
    let ctx = Arc::new(feeds::Ctx {
        inbox: inbox.clone(),
        journal: journal.clone(),
        rest: rest.clone(),
        signer,
        start,
        slugs: slugs_tx,
        refresh: refresh.clone(),
        include_futures: args.include_futures,
        probe_poll_inputs: cfg
            .strategies
            .values()
            .any(|s| matches!(s, pm_core::StrategyConfig::LatencyProbe(_))),
        market_types: if args.mode == Mode::Record {
            None
        } else {
            Some(cfg.markets.types.clone())
        },
        metrics: metrics.clone(),
    });
    tracing::info!("pmbot starting in {:?} mode; local REST limiter enabled={} rate={REST_RATE}/s; HTTP 429 backoff always enabled", args.mode, rest.limiter.enabled());

    runtime.spawn(feeds::discovery(ctx.clone()));
    runtime.spawn(feeds::game_poller(ctx.clone()));
    runtime.spawn(feeds::markets_ws(ctx.clone()));
    {
        let (metrics, journal, limiter, inbox) = (
            metrics.clone(),
            journal.clone(),
            rest.limiter.clone(),
            inbox.clone(),
        );
        runtime.spawn(async move {
            let mut every = tokio::time::interval(METRICS_EVERY);
            every.tick().await;
            let mut trades_dropped = 0;
            loop {
                every.tick().await;
                let (line, value) = metrics.report_line(wall_ns());
                tracing::info!("{line}");
                let dropped = inbox.trades_dropped();
                if dropped > trades_dropped {
                    tracing::warn!("inbox: {} trades dropped in the last {}s ({dropped} since start); the core is falling behind", dropped - trades_dropped, METRICS_EVERY.as_secs());
                    trades_dropped = dropped;
                }
                journal.record("metrics-", wall_ns(), || json!({"wall_ns": wall_ns(), "window_s": METRICS_EVERY.as_secs(), "rate_limit_enabled": limiter.enabled(), "http_429_total": limiter.http_429_total(), "trades_dropped": inbox.trades_dropped(), "books_conflated": inbox.books_conflated(), "stages": value}));
            }
        });
    }
    if args.mode == Mode::Record {
        if args.ui.is_some() {
            tracing::warn!("--ui is ignored in record mode (there is no core)");
        }
        runtime.spawn(feeds::private_ws(ctx.clone()));
        runtime.block_on(async {
            let mut tick = tokio::time::interval(STATUS_EVERY);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let drained = inbox.drain().len();
                        tracing::info!("record: {drained} events since last status, journal dropped {}, trades dropped {}", journal.dropped.load(Ordering::Relaxed), inbox.trades_dropped());
                    }
                    signal = stop_signal() => {
                        tracing::warn!("{signal}: stopping");
                        break;
                    }
                }
            }
        });
        journal.flush(SHUTDOWN_WAIT);
        return Ok(());
    }

    // Per mode, so a paper run never touches the live file.
    let risk_path = match args.risk_state.as_deref() {
        Some("none") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => Some(args.run_dir.join(format!(
            "risk-state-{}.json",
            if args.mode == Mode::Live {
                "live"
            } else {
                "paper"
            }
        ))),
    };
    let risk_saved = risk_path
        .as_deref()
        .map(risk_file::load)
        .transpose()?
        .flatten();
    let risk_saver = risk_path
        .map(|path| risk_file::Saver::start(path, risk_saved.clone()))
        .transpose()?;
    if risk_saver.is_none() {
        tracing::warn!("--risk-state none: a restart clears the daily kill, the day's loss and market loss limits");
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<timing::Outbound>();
    let gate = Arc::new(Gate::default());
    if args.mode == Mode::Live {
        runtime.spawn(feeds::private_ws(ctx.clone()));
        runtime.spawn(gateway::run_gateway(
            rest.clone(),
            cmd_rx,
            inbox.clone(),
            refresh.clone(),
            metrics.clone(),
            gate.clone(),
        ));
    } else {
        runtime.spawn(gateway::run_paper_gateway(cmd_rx, inbox.clone()));
    }
    let status = Arc::new(Mutex::new(String::from("starting")));
    runtime.spawn(admin::serve(
        args.run_dir.join("pmbot.sock"),
        inbox.clone(),
        status.clone(),
        metrics.clone(),
    ));
    // The UI gets its own thread and runtime; the core only feeds it a snapshot while someone is watching.
    let shutdown = Arc::new(Notify::new());
    let ui = args.ui.map(|addr| {
        let shared = Arc::new(ui::UiShared::new(start));
        let mode = if args.mode == Mode::Live {
            "live"
        } else {
            "paper"
        };
        let health = ui::Health {
            metrics: metrics.clone(),
            limiter: rest.limiter.clone(),
            journal_dropped: journal.dropped.clone(),
            journal_write_errors: journal.write_errors.clone(),
            log_dropped: Some(log_dropped.clone()),
        };
        ui::spawn(
            addr,
            shared.clone(),
            inbox.clone(),
            shutdown.clone(),
            mode,
            health,
        );
        shared
    });

    // Watchdog: the core thread bumps `heartbeat` every pass. If it stalls with work waiting, or has
    // ended, nothing else will pull our quotes: stop new and changed orders, then cancel everything
    // over REST and keep trying, with backoff, until an open-orders read confirms it. Sending resumes
    // if the core recovers (its cancels then arrive as executions like any other). It has its own OS
    // thread, runtime and HTTP client (connection pool and 429 backoff), so a stuck runtime or a
    // wedged connection on the main client can't stop it.
    let heartbeat = Arc::new(AtomicU64::new(0));
    let core_ended = Arc::new(AtomicBool::new(false));
    let core_exit = Arc::new(Notify::new());
    if args.mode == Mode::Live {
        let rest = RestClient::new(
            Some(watchdog_signer),
            RateLimiter::with_enabled(REST_RATE, false)?,
        )?;
        let watchdog_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("watchdog runtime: {e}"))?;
        let (heartbeat, inbox, gate, core_ended) = (
            heartbeat.clone(),
            inbox.clone(),
            gate.clone(),
            core_ended.clone(),
        );
        std::thread::Builder::new().name("watchdog".into()).spawn(move || watchdog_runtime.block_on(async move {
            let (mut emergency, mut confirmed) = (false, false);
            let mut retry = Duration::from_secs(1);
            let mut next_try = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                // Shutdown cancels and confirms on its own, and stops the core on purpose.
                if gate.shutting_down() {
                    return;
                }
                let last = heartbeat.load(Ordering::Relaxed);
                let stalled = start.elapsed().as_nanos() as u64 - last > WATCHDOG_STALL.as_nanos() as u64;
                let ended = core_ended.load(Ordering::SeqCst);
                if (stalled && inbox.pending() > 0) || ended {
                    if !emergency {
                        emergency = true;
                        gate.set_emergency(true);
                        tracing::error!("watchdog: core thread {}; stopping order sends and cancelling everything", if ended { "ended" } else { "stalled" });
                    }
                    if !confirmed && Instant::now() >= next_try {
                        match cancel_everything(&rest, &gate).await {
                            Ok(()) => {
                                confirmed = true;
                                tracing::warn!("watchdog: cancel-all confirmed, no open orders");
                            }
                            Err(e) => {
                                tracing::error!("watchdog: {e}; retrying in {retry:?}");
                                next_try = Instant::now() + retry;
                                retry = (retry * 2).min(WATCHDOG_RETRY_MAX);
                            }
                        }
                    }
                } else if emergency && !stalled {
                    tracing::warn!("watchdog: core thread recovered; order sends resume");
                    gate.set_emergency(false);
                    (emergency, confirmed, retry) = (false, false, Duration::from_secs(1));
                }
            }
        })).map_err(|e| format!("watchdog thread: {e}"))?;
    }

    // Core thread. `stop` ends it at shutdown; `CoreExit` reports however it ends.
    let stop = Arc::new(AtomicBool::new(false));
    let core_thread = {
        let limiter = rest.limiter.clone();
        let (inbox, journal, status, heartbeat, metrics) = (
            inbox.clone(),
            journal.clone(),
            status.clone(),
            heartbeat.clone(),
            metrics.clone(),
        );
        let (stop, exit) = (
            stop.clone(),
            CoreExit(core_ended.clone(), core_exit.clone()),
        );
        std::thread::Builder::new().name("core".into()).spawn(move || {
            let _exit = exit;
            priority::raise_current_thread("core");
            let spin = Duration::from_micros(cfg.execution.core_spin_us);
            let mut core = Core::new(cfg);
            let mut risk_saver = risk_saver;
            if let Some(saved) = &risk_saved {
                core.restore_risk_state(saved, wall_ns());
                let s = core.risk_state(wall_ns());
                let line = format!("risk state restored: daily PnL {:.2}, daily kill {}, {} market losses, paused [{}]",
                    s.daily_pnl, if s.killed { "IN FORCE" } else { "off" }, s.market_loss.len(), s.paused.keys().cloned().collect::<Vec<_>>().join(" "));
                if s.killed { tracing::warn!("{line}") } else { tracing::info!("{line}") }
            }
            let mut timing = timing::InputTiming::default();
            let mut last_status = Instant::now();
            let mut last_logged = Instant::now();
            let mut last_gate: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            loop {
                if stop.load(Ordering::SeqCst) {
                    if let Some(saver) = risk_saver.take() {
                        saver.finish(core.risk_state(wall_ns()));
                    }
                    return;
                }
                let mono = start.elapsed().as_nanos() as u64;
                heartbeat.store(mono, Ordering::Relaxed);
                let wait = core.next_deadline().saturating_sub(mono);
                if wait > 0 && inbox.wait_spinning(Duration::from_nanos(wait).min(Duration::from_secs(1)), spin) {
                    if let Some(age) = inbox.pending_for() {
                        metrics.record(Stage::Wake, age.as_nanos() as i64);
                    }
                }
                let busy_start = Instant::now();
                let (inputs, waited) = inbox.drain_timed();
                let drained = Instant::now();
                if !inputs.is_empty() {
                    metrics.record_count(Stage::InboxDepth, inputs.len());
                }
                let sources = timing.inputs(&inputs);
                let events: Vec<Event> = inputs.into_iter().map(|(event, _)| event).collect();
                let record = !events.is_empty();
                // Journal a copy after the commands are out, so serialisation stays off the reaction path.
                let journaled = (record && journal.enabled()).then(|| events.clone());
                let tick_start = Instant::now();
                let clock = Clock { mono: start.elapsed().as_nanos() as u64, wall_ns: wall_ns() };
                let out = core.tick(clock, events);
                let tick_end = Instant::now();
                metrics.record(Stage::Tick, tick_end.duration_since(tick_start).as_nanos() as i64);
                if args.mode == Mode::Live {
                    timing.acknowledged(&out.execution_acks, &metrics);
                }
                timing.prune(&core.om);
                let mut origins: Vec<Instant> = Vec::new();
                for cmd in &out.commands {
                    let outbound = timing.command(cmd.clone(), &sources);
                    if let Some(at) = outbound.received {
                        if !origins.contains(&at) {
                            origins.push(at);
                        }
                    }
                    if cmd_tx.send(outbound).is_err() {
                        tracing::error!("core: the gateway has stopped; ending the core thread");
                        if let Some(saver) = risk_saver.take() {
                            saver.finish(core.risk_state(wall_ns()));
                        }
                        return;
                    }
                }
                // The pieces of `input_to_send` on the core thread, for passes that send orders from input.
                if !origins.is_empty() {
                    metrics.record(Stage::CorePre, tick_start.duration_since(drained).as_nanos() as i64);
                    metrics.record_since(Stage::CorePost, tick_end);
                    for at in origins {
                        metrics.record(Stage::InputToDrain, drained.saturating_duration_since(at).as_nanos() as i64);
                    }
                }
                if let Some(waited) = waited {
                    metrics.record(Stage::Queue, waited.as_nanos() as i64);
                    if !out.commands.is_empty() {
                        metrics.record(Stage::Decide, (waited + tick_start.elapsed()).as_nanos() as i64);
                    }
                }
                for (name, ns) in &out.latencies {
                    if *name == "ack" {
                        metrics.record(Stage::Ack, *ns as i64);
                    }
                }
                // Kills, pauses and resumes are saved at once; PnL drift once a second below.
                let risk_changed = out.derived.iter().any(|d| d.kind == "alert" || d.kind == "admin");
                for d in &out.derived {
                    // Gate reasons repeat every tick; log a market's reason only when it changes.
                    if d.kind == "gate" {
                        let key = d.slug.as_deref().unwrap_or("-").to_string();
                        if last_gate.get(&key) == Some(&d.detail) {
                            continue;
                        }
                        last_gate.insert(key, d.detail.clone());
                    }
                    tracing::info!(slug = d.slug.as_deref().unwrap_or("-"), "{}: {}", d.kind, d.detail);
                }
                if record || !out.commands.is_empty() {
                    let tick_us = tick_start.elapsed().as_micros() as u64;
                    let TickOutput { commands, derived, decisions, .. } = out;
                    journal.record_lazy("core-", clock.wall_ns, move || json!({
                        "mono": clock.mono,
                        "wall_ns": clock.wall_ns,
                        "events": journaled,
                        "tick_us": tick_us,
                        "commands": commands,
                        "derived": derived.iter().map(|d| json!({"kind": d.kind, "slug": d.slug, "detail": d.detail})).collect::<Vec<_>>(),
                        "decisions": decisions,
                    }));
                }
                metrics.record_since(Stage::CoreBusy, busy_start);
                if let Some(saver) = risk_saver.as_mut().filter(|_| risk_changed || last_status.elapsed() >= Duration::from_secs(1)) {
                    saver.offer(core.risk_state(clock.wall_ns));
                }
                if last_status.elapsed() >= Duration::from_secs(1) {
                    let line = format!("{} | rate_limit_enabled={} http_429_total={} trades_dropped={} books_conflated={} journal_dropped={}", core.status(), limiter.enabled(), limiter.http_429_total(), inbox.trades_dropped(), inbox.books_conflated(), journal.dropped.load(Ordering::Relaxed));
                    *status.lock().unwrap() = line.clone();
                    if let Some(ui) = ui.as_ref().filter(|ui| ui.wanted()) {
                        let clock = Clock { mono: start.elapsed().as_nanos() as u64, wall_ns: wall_ns() };
                        let mut snapshot = core.snapshot(clock);
                        snapshot.details = ui.focused().iter().filter_map(|slug| core.market_detail(slug, clock)).collect();
                        ui.publish(snapshot);
                    }
                    if last_logged.elapsed() >= STATUS_EVERY {
                        last_logged = Instant::now();
                        tracing::info!("{line}");
                    }
                    last_status = Instant::now();
                }
            }
        }).map_err(|e| e.to_string())?
    };

    // Shutdown: a signal, the UI, or the core thread ending. Stop new and changed orders, have the core
    // cancel (so its journal records it), then cancel everything over REST once creates already sent
    // have landed, and confirm it. Then stop the core and write out the journal.
    let core_failed = runtime.block_on(async {
        tokio::select! {
            signal = stop_signal() => { tracing::warn!("{signal}: shutting down"); false }
            _ = shutdown.notified() => { tracing::warn!("shutdown requested from the UI"); false }
            _ = core_exit.notified() => { tracing::error!("the core thread ended; shutting down"); true }
        }
    });
    tracing::warn!("shutting down: cancelling all orders");
    gate.shut();
    inbox.control(Event::Admin(AdminCmd::CancelAll));
    let cancelled = runtime.block_on(async {
        if args.mode == Mode::Live {
            cancel_everything(&rest, &gate).await
        } else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(())
        }
    });
    match &cancelled {
        Ok(()) if args.mode == Mode::Live => tracing::info!("cancel-all confirmed: no open orders"),
        Ok(()) => {}
        Err(e) => tracing::error!("{e}"),
    }
    stop.store(true, Ordering::SeqCst);
    inbox.ring();
    let deadline = Instant::now() + SHUTDOWN_WAIT;
    while !core_thread.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !core_thread.is_finished() {
        tracing::error!("core thread still busy after {SHUTDOWN_WAIT:?}; exiting without it");
    }
    if !journal.flush(SHUTDOWN_WAIT) {
        tracing::error!(
            "journal not written out within {SHUTDOWN_WAIT:?}; the last lines may be missing"
        );
    }
    let _ = std::fs::remove_file(args.run_dir.join("pmbot.sock"));
    tracing::info!("bye");
    match (cancelled, core_failed) {
        (Err(e), _) => Err(e),
        (Ok(()), true) => Err("the core thread ended unexpectedly".into()),
        (Ok(()), false) => Ok(()),
    }
}
