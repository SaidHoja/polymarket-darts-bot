//! Operator web UI: `--ui 127.0.0.1:8787`.
//!
//! Kept off the trading path:
//! - It runs on its own OS thread with its own single-threaded runtime, so it never
//!   takes a tokio worker from the feeds or the gateway.
//! - It never touches `Core`. The core thread publishes a `Snapshot` once a second,
//!   after its commands are out, and only while a page has polled in the last few seconds.
//! - A market page (`/market/{slug}`) adds that market's full book, orders and fills to the
//!   snapshot, only while the page is polling and for at most `MAX_FOCUS` markets at once.
//! - Buttons go through `Inbox::control`, the same path as the admin socket.
//!
//! It can pause, resume, cancel everything and shut down. It cannot place orders or change limits.
//!
//! `/metrics` shows latency and pipeline health. It reads the lock-free `Metrics`
//! histograms directly, so watching it doesn't ask the core for snapshots.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Form, Path, State};
use axum::http::{header, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use maud::{html, Markup, DOCTYPE};
use pm_core::{DepthLevel, FillLog, MarketDetail, MarketRow, Snapshot};
use pm_types::{AdminCmd, Event, Side};
use pm_venue::rest::RateLimiter;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;

use crate::inbox::Inbox;
use crate::metrics::{ms, summary_json, Metrics, Stage, Summary, Window};

const HTMX: &str = include_str!("../assets/htmx.min.js");
const TOKEN_HEADER: &str = "x-pmbot-token";
/// The core stops building snapshots this long after the last poll.
const WATCH_WINDOW: Duration = Duration::from_secs(5);
/// A snapshot older than this is shown as stale.
const STALE_NS: i64 = 3_000_000_000;
/// Market pages the core builds detail for at once; the oldest-polled drops out.
const MAX_FOCUS: usize = 4;

/// Shared between the core thread (publisher) and the UI thread.
pub struct UiShared {
    start: Instant,
    /// `start`-relative nanoseconds of the last poll; 0 = never.
    last_poll: AtomicU64,
    snapshot: Mutex<Option<Arc<Snapshot>>>,
    /// Open market pages: (slug, `start`-relative ns of its last poll).
    focus: Mutex<Vec<(String, u64)>>,
}

impl UiShared {
    pub fn new(start: Instant) -> Self {
        UiShared {
            start,
            last_poll: AtomicU64::new(0),
            snapshot: Mutex::new(None),
            focus: Mutex::new(Vec::new()),
        }
    }

    fn now_ns(&self) -> u64 {
        (self.start.elapsed().as_nanos() as u64).max(1)
    }

    /// Markets with a page polled recently, for `Core::market_detail`.
    pub fn focused(&self) -> Vec<String> {
        let now = self.now_ns();
        let window = WATCH_WINDOW.as_nanos() as u64;
        self.focus
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, t)| now.saturating_sub(*t) < window)
            .map(|(s, _)| s.clone())
            .collect()
    }

    /// Whether anyone is watching, so the core thread can skip building snapshots.
    pub fn wanted(&self) -> bool {
        let last = self.last_poll.load(Ordering::Relaxed);
        last != 0
            && (self.start.elapsed().as_nanos() as u64).saturating_sub(last)
                < WATCH_WINDOW.as_nanos() as u64
    }

    pub fn publish(&self, snapshot: Snapshot) {
        *self.snapshot.lock().unwrap() = Some(Arc::new(snapshot));
    }

    fn latest(&self) -> Option<Arc<Snapshot>> {
        self.last_poll.store(self.now_ns(), Ordering::Relaxed);
        self.snapshot.lock().unwrap().clone()
    }

    /// `latest`, and ask the core for this market's detail from the next snapshot on.
    fn latest_for(&self, slug: &str) -> Option<Arc<Snapshot>> {
        let now = self.now_ns();
        {
            let mut focus = self.focus.lock().unwrap();
            match focus.iter_mut().find(|(s, _)| s == slug) {
                Some(entry) => entry.1 = now,
                None => focus.push((slug.to_string(), now)),
            }
            if focus.len() > MAX_FOCUS {
                focus.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
                focus.truncate(MAX_FOCUS);
            }
        }
        self.latest()
    }
}

/// What the metrics page reads besides the snapshot.
#[derive(Clone)]
pub struct Health {
    pub metrics: Arc<Metrics>,
    pub limiter: Arc<RateLimiter>,
    /// Journal lines dropped because the writer fell behind.
    pub journal_dropped: Arc<AtomicU64>,
    /// Journal lines the writer couldn't write (disk full, a file that wouldn't open).
    pub journal_write_errors: Arc<AtomicU64>,
    /// Log lines dropped because the non-blocking log writer fell behind.
    pub log_dropped: Option<tracing_appender::non_blocking::ErrorCounter>,
}

impl Health {
    fn log_dropped(&self) -> usize {
        self.log_dropped.as_ref().map_or(0, |c| c.dropped_lines())
    }
}

#[derive(Clone)]
struct App {
    shared: Arc<UiShared>,
    inbox: Arc<Inbox>,
    shutdown: Arc<Notify>,
    mode: &'static str,
    token: Arc<str>,
    health: Health,
}

/// Start the UI on its own thread. Failing to bind is logged and the bot carries on.
pub fn spawn(
    addr: SocketAddr,
    shared: Arc<UiShared>,
    inbox: Arc<Inbox>,
    shutdown: Arc<Notify>,
    mode: &'static str,
    health: Health,
) {
    let token: Arc<str> = format!("{:032x}", rand::random::<u128>()).into();
    let app = App {
        shared,
        inbox,
        shutdown,
        mode,
        token,
        health,
    };
    let spawned = std::thread::Builder::new()
        .name("ui".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => return tracing::error!("ui runtime: {e}"),
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => return tracing::error!("ui {addr}: {e}"),
                };
                tracing::info!("ui at http://{addr}");
                if let Err(e) = axum::serve(listener, router(app)).await {
                    tracing::error!("ui: {e}");
                }
            });
        });
    if let Err(e) = spawned {
        tracing::error!("ui thread: {e}");
    }
}

fn router(app: App) -> Router {
    Router::new()
        .route("/", get(page))
        .route(
            "/assets/htmx.min.js",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "text/javascript"),
                        (header::CACHE_CONTROL, "max-age=86400"),
                    ],
                    HTMX,
                )
            }),
        )
        .route("/partials/state", get(state_partial))
        .route("/market/{slug}", get(market_page))
        .route("/partials/market/{slug}", get(market_partial))
        .route("/snapshot.json", get(snapshot_json))
        .route("/metrics", get(metrics_page))
        .route("/partials/metrics", get(metrics_partial))
        .route("/metrics.json", get(metrics_json))
        .route("/admin/pause", post(pause))
        .route("/admin/resume", post(resume))
        .route("/admin/cancel-all", post(cancel_all))
        .route("/shutdown", post(shutdown))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// POSTs need the page's token and, if the browser sends one, a same-origin `Origin`.
async fn guard(State(app): State<App>, req: Request<axum::body::Body>, next: Next) -> Response {
    if req.method() == Method::POST {
        let headers = req.headers();
        if headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) != Some(&*app.token) {
            return (StatusCode::FORBIDDEN, "bad token").into_response();
        }
        if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let host = headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if origin != format!("http://{host}") {
                return (StatusCode::FORBIDDEN, "cross-origin").into_response();
            }
        }
    }
    next.run(req).await
}

#[derive(Deserialize, Default)]
struct SlugForm {
    slug: Option<String>,
}

fn send(app: &App, cmd: AdminCmd) {
    tracing::warn!("ui: {cmd:?}");
    app.inbox.control(Event::Admin(cmd));
}

fn slug_arg(form: SlugForm) -> Option<String> {
    form.slug.filter(|s| !s.is_empty())
}

async fn pause(State(app): State<App>, Form(form): Form<SlugForm>) -> Markup {
    send(&app, AdminCmd::Pause(slug_arg(form)));
    state(&app)
}

async fn resume(State(app): State<App>, Form(form): Form<SlugForm>) -> Markup {
    send(&app, AdminCmd::Resume(slug_arg(form)));
    state(&app)
}

async fn cancel_all(State(app): State<App>) -> Markup {
    send(&app, AdminCmd::CancelAll);
    state(&app)
}

async fn shutdown(State(app): State<App>) -> Markup {
    tracing::warn!("ui: shutdown");
    app.shutdown.notify_one();
    html! {
        section #state .stopped {
            h2 { "pmbot is shutting down" }
            p { "Cancel-all was sent. Check the logs, or the venue's open orders, to confirm nothing is resting." }
        }
    }
}

async fn snapshot_json(State(app): State<App>) -> Response {
    match app.shared.latest() {
        Some(s) => Json(s.as_ref()).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "no snapshot yet").into_response(),
    }
}

async fn state_partial(State(app): State<App>) -> Markup {
    state(&app)
}

/// Page shell: head, the title bar with navigation, and `content`.
fn layout(app: &App, title: &str, active: &str, controls: Markup, content: Markup) -> Markup {
    let hx_headers = format!(r#"{{"{TOKEN_HEADER}":"{}"}}"#, app.token);
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (maud::PreEscaped(CSS)) }
                script src="/assets/htmx.min.js" {}
            }
            body hx-headers=(hx_headers) {
                header .top {
                    div .brand {
                        h1 { "pmbot " span .badge .(app.mode) { (app.mode.to_uppercase()) } }
                        nav {
                            a href="/" .active[active == "markets"] { "Markets" }
                            a href="/metrics" .active[active == "metrics"] { "Metrics" }
                        }
                    }
                    (controls)
                }
                div #unreachable hidden { "pmbot is not responding. The numbers below are stale." }
                (content)
                script { (maud::PreEscaped(SCRIPT)) }
            }
        }
    }
}

async fn page(State(app): State<App>) -> Markup {
    let controls = html! {
                    div .controls {
                        button hx-post="/admin/pause" hx-target="#state" hx-swap="outerHTML" { "Pause all" }
                        button hx-post="/admin/resume" hx-target="#state" hx-swap="outerHTML" { "Resume all" }
                        button .warn hx-post="/admin/cancel-all" hx-target="#state" hx-swap="outerHTML"
                            hx-confirm="Cancel every order and pause? Resume all restarts quoting." { "Cancel all" }
                        button .danger hx-post="/shutdown" hx-target="#state" hx-swap="outerHTML"
                            hx-confirm=(format!("Shut down pmbot ({} mode)? This cancels all orders and exits the process.", app.mode)) { "Shutdown" }
                    }
    };
    layout(&app, "pmbot", "markets", controls, state(&app))
}

/// Slugs are `[a-z0-9-]`; anything else is refused before it reaches the focus list.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 128
        && slug
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

async fn market_page(State(app): State<App>, Path(slug): Path<String>) -> Response {
    if !valid_slug(&slug) {
        return (StatusCode::NOT_FOUND, "bad market slug").into_response();
    }
    layout(
        &app,
        &format!("pmbot · {slug}"),
        "markets",
        html! {},
        market_view(&app, &slug),
    )
    .into_response()
}

async fn market_partial(State(app): State<App>, Path(slug): Path<String>) -> Response {
    if !valid_slug(&slug) {
        return (StatusCode::NOT_FOUND, "bad market slug").into_response();
    }
    market_view(&app, &slug).into_response()
}

async fn metrics_page(State(app): State<App>) -> Markup {
    layout(
        &app,
        "pmbot metrics",
        "metrics",
        html! {},
        metrics_view(&app),
    )
}

async fn metrics_partial(State(app): State<App>) -> Markup {
    metrics_view(&app)
}

async fn metrics_json(State(app): State<App>) -> Json<serde_json::Value> {
    let h = &app.health;
    let stages = |s: &[(Stage, Summary)]| {
        serde_json::Value::Object(
            s.iter()
                .map(|(st, m)| (st.name().to_string(), summary_json(*st, m)))
                .collect(),
        )
    };
    Json(json!({
        "uptime_s": app.shared.start.elapsed().as_secs_f64(),
        "rate_limit_enabled": h.limiter.enabled(),
        "http_429_total": h.limiter.http_429_total(),
        "inbox_pending": app.inbox.pending(),
        "trades_dropped": app.inbox.trades_dropped(),
        "books_conflated": app.inbox.books_conflated(),
        "journal_dropped": h.journal_dropped.load(Ordering::Relaxed),
        "journal_write_errors": h.journal_write_errors.load(Ordering::Relaxed),
        "log_dropped": h.log_dropped(),
        "since_start": stages(&h.metrics.totals()),
        "minutes": h.metrics.history().iter().map(|w| json!({"wall_ns": w.wall_ns, "stages": stages(&w.stages)})).collect::<Vec<_>>(),
    }))
}

/// Stages by where the time goes, in the order the page shows them.
const GROUPS: [(&str, &[Stage]); 5] = [
    (
        "End to end",
        &[Stage::InputToSend, Stage::InputToAck, Stage::Ack],
    ),
    (
        "Inside the bot",
        &[
            Stage::Feed,
            Stage::InputToDrain,
            Stage::Queue,
            Stage::Wake,
            Stage::CorePre,
            Stage::Tick,
            Stage::CorePost,
            Stage::CoreBusy,
            Stage::Decide,
            Stage::GwHandoff,
            Stage::GwWait,
            Stage::GwSpawn,
            Stage::GwBuild,
        ],
    ),
    (
        "Venue REST",
        &[
            Stage::RestCreate,
            Stage::RestModify,
            Stage::RestCancel,
            Stage::RestCancelAll,
            Stage::RestRead,
        ],
    ),
    (
        "Network and feeds",
        &[
            Stage::WsRttMarkets,
            Stage::WsRttPrivate,
            Stage::BookLag,
            Stage::TradeLag,
        ],
    ),
    (
        "Queue lengths (items, not ms)",
        &[Stage::InboxDepth, Stage::GwDepth],
    ),
];

const ORDER_STAGES: [Stage; 4] = [
    Stage::RestCreate,
    Stage::RestModify,
    Stage::RestCancel,
    Stage::RestCancelAll,
];
/// Per-minute rows shown.
const MINUTES_SHOWN: usize = 30;

/// The polled region of `/metrics`.
fn metrics_view(app: &App) -> Markup {
    let h = &app.health;
    let totals = h.metrics.totals();
    let history = h.metrics.history();
    let last = history.last();
    let total = |s: Stage| totals.iter().find(|(x, _)| *x == s).map(|(_, m)| m);
    let n = |s: Stage| total(s).map_or(0, |m| m.count);
    let orders: u64 = ORDER_STAGES.iter().map(|s| n(*s)).sum();
    let e2e = last
        .and_then(|w| w.get(Stage::InputToAck))
        .or(total(Stage::InputToAck));
    let dropped = h.journal_dropped.load(Ordering::Relaxed);
    let write_errors = h.journal_write_errors.load(Ordering::Relaxed);
    let log_dropped = h.log_dropped();
    html! {
        section #metrics hx-get="/partials/metrics" hx-trigger="every 2s" hx-swap="outerHTML" {
            div .tiles {
                div .tile {
                    div .label { "Uptime" }
                    div .value { (duration(app.shared.start.elapsed())) }
                    div .sub { (history.len()) " min of history" }
                }
                div .tile {
                    div .label { "Input → venue ack" }
                    div .value { @if let Some(m) = e2e { (ms(m.p50_ns)) " ms" } @else { span .muted { "—" } } }
                    div .sub { "p50, " @if last.and_then(|w| w.get(Stage::InputToAck)).is_some() { "last minute" } @else { "since start" } }
                }
                div .tile {
                    div .label { "Order requests" }
                    div .value { (orders) }
                    div .sub { (n(Stage::RestCreate)) " create · " (n(Stage::RestModify)) " modify · " (n(Stage::RestCancel) + n(Stage::RestCancelAll)) " cancel" }
                }
                div .tile {
                    div .label { "HTTP 429s" }
                    div .value .neg[h.limiter.http_429_total() > 0] { (h.limiter.http_429_total()) }
                    div .sub { "local rate limit " (if h.limiter.enabled() { "on" } else { "off" }) }
                }
                div .tile {
                    div .label { "Inbox backlog" }
                    div .value { (app.inbox.pending()) }
                    div .sub { "events waiting for the core" }
                }
                div .tile {
                    div .label { "Trades dropped" }
                    div .value .neg[app.inbox.trades_dropped() > 0] { (app.inbox.trades_dropped()) }
                    div .sub { (app.inbox.books_conflated()) " books merged (expected under load)" }
                }
                div .tile {
                    div .label { "Journal dropped" }
                    div .value .neg[dropped > 0 || write_errors > 0] { (dropped + write_errors) }
                    div .sub { (dropped) " writer behind · " (write_errors) " write failures" }
                }
                div .tile {
                    div .label { "Log dropped" }
                    div .value .neg[log_dropped > 0] { (log_dropped) }
                    div .sub { "log lines lost: stdout behind" }
                }
            }
            h2 { "Latency (ms) and queue lengths" }
            p .sub {
                "Last minute is the last full 60 s window"
                @if let Some(w) = last { " (ending " (utc_hm(w.wall_ns)) " UTC)" }
                ". Cross-clock stages include our clock's offset from the venue's."
            }
            div .scroll {
                table .metrics {
                    thead {
                        tr { th {} th {} th colspan="3" .group { "Last minute" } th colspan="5" .group { "Since start" } }
                        tr { th { "Stage" } th { "What it measures" } th { "n" } th { "p50" } th { "p99" } th { "n" } th { "p50" } th { "p90" } th { "p99" } th { "max" } }
                    }
                    tbody {
                        @for (group, stages) in GROUPS {
                            tr .grouprow { td colspan="10" { (group) } }
                            @for s in stages.iter() {
                                @let lm = last.and_then(|w| w.get(*s));
                                @let t = total(*s);
                                tr {
                                    td .market { code { (s.name()) } }
                                    td .market .muted {
                                        (s.label())
                                        @if let Some(t) = t { @if t.negative > 0 { div .sub .warntext { (t.negative) " negative: our clock is behind the venue's" } } }
                                    }
                                    @match lm {
                                        Some(m) => { td { (m.count) } td { (s.show(m.p50_ns)) } td { (s.show(m.p99_ns)) } }
                                        None => { td .muted { "—" } td {} td {} }
                                    }
                                    @match t {
                                        Some(m) => { td { (m.count) } td { (s.show(m.p50_ns)) } td { (s.show(m.p90_ns)) } td { (s.show(m.p99_ns)) } td { (s.show(m.max_ns as f64)) } }
                                        None => { td .muted { "—" } td {} td {} td {} td {} }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            h2 { "Per minute" }
            p .sub { "Newest first, last " (MINUTES_SHOWN) " minutes. p50 / p99 in ms." }
            div .scroll {
                table .metrics {
                    thead { tr {
                        th { "Minute (UTC)" } th { "Order requests" } th { "Input → sent" } th { "Input → ack" }
                        th { "REST create p50" } th { "Ping (markets)" } th { "Tick p99" } th { "Book lag p50" }
                    } }
                    tbody {
                        @if history.is_empty() { tr { td colspan="8" .muted { "The first minute isn't over yet." } } }
                        @for w in history.iter().rev().take(MINUTES_SHOWN) { (minute_row(w)) }
                    }
                }
            }
        }
    }
}

fn minute_row(w: &Window) -> Markup {
    let orders: u64 = ORDER_STAGES
        .iter()
        .filter_map(|s| w.get(*s))
        .map(|m| m.count)
        .sum();
    let pair = |s: Stage| match w.get(s) {
        Some(m) => html! { (ms(m.p50_ns)) " / " (ms(m.p99_ns)) },
        None => html! { span .muted { "—" } },
    };
    let one = |s: Stage, f: fn(&Summary) -> f64| match w.get(s) {
        Some(m) => html! { (ms(f(m))) },
        None => html! { span .muted { "—" } },
    };
    html! {
        tr {
            td .market { (utc_hm(w.wall_ns)) }
            td { (orders) }
            td { (pair(Stage::InputToSend)) }
            td { (pair(Stage::InputToAck)) }
            td { (one(Stage::RestCreate, |m| m.p50_ns)) }
            td { (pair(Stage::WsRttMarkets)) }
            td { (one(Stage::Tick, |m| m.p99_ns)) }
            td { (one(Stage::BookLag, |m| m.p50_ns)) }
        }
    }
}

fn utc_hm(wall_ns: i64) -> String {
    let secs = wall_ns.div_euclid(1_000_000_000).rem_euclid(86_400);
    format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

fn duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, s % 3600 / 60)
    } else {
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

/// The polled region: account header and markets table.
fn state(app: &App) -> Markup {
    let Some(s) = app.shared.latest() else {
        return html! { section #state hx-get="/partials/state" hx-trigger="every 1s" hx-swap="outerHTML" { p .muted { "Waiting for the first snapshot from the core…" } } };
    };
    let stale = crate::journal::wall_ns() - s.wall_ns > STALE_NS;
    html! {
        section #state hx-get="/partials/state" hx-trigger="every 1s" hx-swap="outerHTML" {
            (banners(&s, stale))
            (account(&s))
            (positions(&s))
            h2 { "Markets" }
            (markets(&s.markets))
            (recent_fills(&s))
        }
    }
}

fn banners(s: &Snapshot, stale: bool) -> Markup {
    html! {
        @if stale { div .banner .bad { "Snapshot is stale: the core thread has not published for over 3 s." } }
        @if s.killed { div .banner .bad { "KILLED: daily loss limit hit. No quoting until the next UTC day." } }
        @if !s.markets_up || !s.private_up {
            div .banner .bad { "Feed down: markets=" (updown(s.markets_up)) " private=" (updown(s.private_up)) }
        }
        @if s.orders_blocked { div .banner .warn { "Orders blocked after a reject storm." } }
        @if s.admin_paused { div .banner .warn { "PAUSED by operator. Resume all to quote again." } }
        @if s.slow { div .banner .warn { "Exchange reported slow; strategies are widening." } }
    }
}

fn account(s: &Snapshot) -> Markup {
    let daily_used = if s.daily_loss_limit > 0.0 {
        (-s.daily_pnl / s.daily_loss_limit).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let worst_used = if s.max_account_loss > 0.0 {
        (s.account_worst / s.max_account_loss).clamp(0.0, 1.0)
    } else {
        0.0
    };
    html! {
        div .tiles {
            div .tile {
                div .label { "Current PnL" }
                div .value .(sign(s.current_pnl)) { (money(s.current_pnl)) }
                div .sub { "at venue mid" @if s.unmarked_current > 0 { " · " (s.unmarked_current) " unmarked" } }
            }
            div .tile {
                div .label { "Expected PnL" }
                div .value .(sign(s.expected_pnl)) { (money(s.expected_pnl)) }
                div .sub { "at fair / microprice" @if s.unmarked_expected > 0 { " · " (s.unmarked_expected) " unmarked" } }
            }
            div .tile {
                div .label { "Daily PnL" }
                div .value .(sign(s.daily_pnl)) { (money(s.daily_pnl)) }
                div .bar { div style=(format!("width:{:.0}%", daily_used * 100.0)) {} }
                div .sub { "limit −$" (format!("{:.2}", s.daily_loss_limit)) }
            }
            div .tile {
                div .label { "Worst case" }
                div .value { "$" (format!("{:.2}", s.account_worst)) }
                div .bar { div style=(format!("width:{:.0}%", worst_used * 100.0)) {} }
                div .sub { "account limit $" (format!("{:.2}", s.max_account_loss)) }
            }
            div .tile {
                div .label { "Cash" }
                @match &s.balance {
                    Some(b) => { div .value { "$" (format!("{:.2}", b.current_balance)) } div .sub { "buying power $" (format!("{:.2}", b.buying_power)) } }
                    None => { div .value .muted { "—" } }
                }
            }
            div .tile {
                div .label { "Activity" }
                div .value { (s.open_orders) " orders" }
                div .sub { (s.fills) " fills · fees $" (format!("{:.2}", s.fees)) }
            }
        }
    }
}

fn markets(rows: &[MarketRow]) -> Markup {
    html! {
        div .scroll {
            table {
                thead { tr {
                    th { "Market" } th { "Bid" } th { "Ask" } th { "Mid" } th { "VWAP" } th { "Fair" }
                    th { "Position" } th { "If YES" } th { "If NO" } th { "Worst" }
                    th { "Current" } th { "Expected" } th { "Our quotes" } th { "Status" } th {}
                } }
                tbody {
                    @if rows.is_empty() { tr { td colspan="15" .muted { "No active markets." } } }
                    @for r in rows { (market_row(r)) }
                }
            }
        }
    }
}

fn market_row(r: &MarketRow) -> Markup {
    let vals = serde_json::json!({ "slug": r.slug }).to_string();
    let title = if r.title.is_empty() {
        r.slug.as_str()
    } else {
        r.title.as_str()
    };
    html! {
        tr .held[r.qty != 0.0] {
            td .market {
                a .title title=(r.slug) href=(market_href(&r.slug)) { (title) }
                div .sub {
                    "YES " (r.yes_name.as_deref().unwrap_or("?")) " · " (r.league)
                    @if let Some(p) = r.phase { " · " (format!("{p:?}")) }
                    @if r.live { " · " span .live { "LIVE" } }
                    @if let Some((a, b)) = r.score { " " (a) "–" (b) }
                    " · " (r.strategy)
                }
            }
            td { (level(r.best_bid)) }
            td { (level(r.best_ask)) }
            td { (opt_px(r.mid)) @if let Some(s) = r.spread { div .sub { (format!("{:.0}¢", s * 100.0)) } } }
            td {
                (opt_px(r.vwap.map(|v| v.0)))
                @if let Some((_, q)) = r.vwap {
                    div .sub {
                        @if r.vwap_depth == 0 { "all kept levels" } @else { "top " (r.vwap_depth) "/side" }
                        " · " (qty(q)) " contracts"
                    }
                }
            }
            td { (opt_px(r.fair.or(r.expected_px))) @if r.fair.is_none() && r.expected_px.is_some() { div .sub { "micro" } } }
            td {
                @if r.qty == 0.0 { span .muted { "flat" } } @else {
                    (qty(r.qty)) @if let Some(c) = r.avg_cost { div .sub { "@ " (px(c)) } }
                }
            }
            td .(sign(r.pnl_if_yes)) { (money(r.pnl_if_yes)) }
            td .(sign(r.pnl_if_no)) { (money(r.pnl_if_no)) }
            td { "$" (format!("{:.2}", r.worst_case)) }
            td { (opt_money(r.current_pnl)) }
            td { (opt_money(r.expected_pnl)) }
            td .quotes {
                @for (p, q) in &r.our_bids { div .bid { "B " (px(*p)) " × " (qty(*q)) } }
                @for (p, q) in &r.our_asks { div .ask { "S " (px(*p)) " × " (qty(*q)) } }
                @if r.our_bids.is_empty() && r.our_asks.is_empty() { span .muted { "—" } }
            }
            td { @if let Some(p) = &r.paused { span .warntext { (p) } } }
            td {
                @if r.paused.is_some() {
                    button .small hx-post="/admin/resume" hx-vals=(vals) hx-target="#state" hx-swap="outerHTML" { "Resume" }
                } @else {
                    button .small hx-post="/admin/pause" hx-vals=(vals) hx-target="#state" hx-swap="outerHTML" { "Pause" }
                }
            }
        }
    }
}

fn market_href(slug: &str) -> String {
    format!("/market/{slug}")
}

fn title_of(r: &MarketRow) -> &str {
    if r.title.is_empty() {
        &r.slug
    } else {
        &r.title
    }
}

/// Markets we hold contracts in: waiting for us to trade out or for the market to settle.
fn positions(s: &Snapshot) -> Markup {
    let held: Vec<&MarketRow> = s.markets.iter().filter(|r| r.qty != 0.0).collect();
    html! {
        h2 { "Open positions" }
        p .sub { "Markets where our orders filled and we still hold contracts, until we trade out or the market settles. Prices are YES prices." }
        div .scroll {
            table {
                thead { tr {
                    th { "Market" } th { "Holding" } th { "Mid" } th { "Fair" } th { "Current" } th { "Expected" }
                    th { "If YES" } th { "If NO" } th { "Exit orders" } th { "Last fill" }
                } }
                tbody {
                    @if held.is_empty() { tr { td colspan="10" .muted { "No open positions." } } }
                    @for r in held {
                        tr .held {
                            td .market {
                                a .title href=(market_href(&r.slug)) { (title_of(r)) }
                                div .sub {
                                    (r.league)
                                    @if r.live { " · " span .live { "LIVE" } }
                                    @if let Some((a, b)) = r.score { " " (a) "–" (b) }
                                    @if let Some(p) = &r.paused { " · " span .warntext { (p) } }
                                }
                            }
                            td { (holding(r)) }
                            td { (opt_px(r.mid)) }
                            td { (opt_px(r.fair.or(r.expected_px))) }
                            td { (opt_money(r.current_pnl)) }
                            td { (opt_money(r.expected_pnl)) }
                            td .(sign(r.pnl_if_yes)) { (money(r.pnl_if_yes)) }
                            td .(sign(r.pnl_if_no)) { (money(r.pnl_if_no)) }
                            td .quotes {
                                // Orders that reduce the position: asks when long YES, bids when short.
                                @let exits = if r.qty > 0.0 { &r.our_asks } else { &r.our_bids };
                                @let (cls, tag) = if r.qty > 0.0 { ("ask", "S") } else { ("bid", "B") };
                                @for (p, q) in exits { div .(cls) { (tag) " " (px(*p)) " × " (qty(*q)) } }
                                @if exits.is_empty() { span .muted { "none" } }
                            }
                            td { @match r.last_fill_ns { Some(ns) => { (ago(s.wall_ns - ns)) }, None => { span .muted { "—" } } } }
                        }
                    }
                }
            }
        }
    }
}

/// "Long 10 YES @ 0.42" with the NO-side equivalent for shorts.
fn holding(r: &MarketRow) -> Markup {
    let avg = r.avg_cost.unwrap_or(0.0);
    html! {
        @if r.qty > 0.0 {
            "Long " (qty(r.qty)) " YES"
            div .sub { "@ " (px(avg)) " · " (r.yes_name.as_deref().unwrap_or("YES")) }
        } @else {
            "Short " (qty(-r.qty)) " YES"
            div .sub { "@ " (px(avg)) " = NO " (r.no_name.as_deref().unwrap_or("")) " @ " (px(1.0 - avg)) }
        }
    }
}

fn recent_fills(s: &Snapshot) -> Markup {
    let titles: std::collections::HashMap<&str, &str> = s
        .markets
        .iter()
        .map(|r| (r.slug.as_str(), title_of(r)))
        .collect();
    html! {
        h2 { "Recent fills" }
        div .scroll {
            (fills_table(&s.recent_fills, Some(&titles)))
        }
    }
}

/// Fills, newest first. `titles` adds a market column.
fn fills_table(
    fills: &[FillLog],
    titles: Option<&std::collections::HashMap<&str, &str>>,
) -> Markup {
    let cols = if titles.is_some() { 8 } else { 7 };
    html! {
        table {
            thead { tr {
                th { "Time (UTC)" } @if titles.is_some() { th { "Market" } }
                th { "Side" } th { "Qty" } th { "Price" } th { "Fee" } th { "Role" } th { "Position after" }
            } }
            tbody {
                @if fills.is_empty() { tr { td colspan=(cols) .muted { "No fills since start." } } }
                @for f in fills {
                    tr {
                        td .market { (utc_hms(f.wall_ns)) }
                        @if let Some(t) = titles {
                            td .market { a href=(market_href(&f.slug)) { (t.get(&*f.slug).copied().unwrap_or(&f.slug)) } }
                        }
                        td .(side_class(f.side)) { (side_name(f.side)) }
                        td { (qty(f.qty)) }
                        td { (px(f.px)) }
                        td { "$" (format!("{:.4}", f.fee)) }
                        td { (if f.aggressor { "taker" } else { "maker" }) }
                        td { (qty(f.position_after)) }
                    }
                }
            }
        }
    }
}

/// The polled region of `/market/{slug}`.
fn market_view(app: &App, slug: &str) -> Markup {
    let poll = format!("/partials/market/{slug}");
    let Some(s) = app.shared.latest_for(slug) else {
        return html! { section #market hx-get=(poll) hx-trigger="every 1s" hx-swap="outerHTML" { p .muted { "Waiting for the first snapshot from the core…" } } };
    };
    let stale = crate::journal::wall_ns() - s.wall_ns > STALE_NS;
    let row = s.markets.iter().find(|r| r.slug == slug);
    let detail = s.details.iter().find(|d| d.slug == slug);
    html! {
        section #market hx-get=(poll) hx-trigger="every 1s" hx-swap="outerHTML" {
            p { a href="/" { "← All markets" } }
            (banners(&s, stale))
            @match (row, detail) {
                (None, None) => {
                    h2 { code { (slug) } }
                    p .muted { "Not in the snapshot yet. The core adds a market's detail within a second of the page opening; if this stays, pmbot neither tracks this market nor holds a position in it." }
                }
                _ => {
                    (market_header(slug, row, detail))
                    @if let Some(r) = row { (market_tiles(r, detail)) }
                    @match detail {
                        Some(d) => (market_body(&s, row, d)),
                        None => p .muted { "Loading the book…" },
                    }
                }
            }
        }
    }
}

fn market_header(slug: &str, row: Option<&MarketRow>, detail: Option<&MarketDetail>) -> Markup {
    let vals = serde_json::json!({ "slug": slug }).to_string();
    html! {
        div .mhead {
            div {
                h2 .mtitle { (row.map_or(slug, title_of)) }
                div .sub {
                    @if let Some(r) = row {
                        "YES " (r.yes_name.as_deref().unwrap_or("?")) " · NO " (r.no_name.as_deref().unwrap_or("?")) " · " (r.league)
                        @if let Some(p) = r.phase { " · " (format!("{p:?}")) }
                        @if r.live { " · " span .live { "LIVE" } }
                        @if let Some((a, b)) = r.score { " " (a) "–" (b) }
                        " · " (r.strategy)
                    }
                    @if let Some(st) = detail.and_then(|d| d.state) { " · " (format!("{st:?}")) }
                    @if row.is_some_and(|r| !r.active) { " · " span .warntext { "not active" } }
                }
                @if let Some(p) = row.and_then(|r| r.paused.as_deref()) { div .warntext { "Paused: " (p) } }
            }
            div .controls {
                // The next poll shows the result; the response targets the markets page.
                @if row.is_some_and(|r| r.paused.is_some()) {
                    button hx-post="/admin/resume" hx-vals=(vals) hx-swap="none" { "Resume market" }
                } @else {
                    button hx-post="/admin/pause" hx-vals=(vals) hx-swap="none" { "Pause market" }
                }
            }
        }
    }
}

fn market_tiles(r: &MarketRow, detail: Option<&MarketDetail>) -> Markup {
    html! {
        div .tiles {
            div .tile {
                div .label { "Position" }
                @if r.qty == 0.0 { div .value .muted { "flat" } } @else { div .value { (holding(r)) } }
            }
            div .tile {
                div .label { "Mid" }
                div .value { (opt_px(r.mid)) }
                div .sub { @if let Some(sp) = r.spread { "spread " (format!("{:.0}¢", sp * 100.0)) } }
            }
            div .tile {
                div .label { "Fair" }
                div .value { (opt_px(r.fair.or(r.expected_px))) }
                div .sub { @if r.fair.is_none() && r.expected_px.is_some() { "microprice" } @else { "strategy" } }
            }
            div .tile {
                div .label { "Current PnL" }
                div .value { (opt_money(r.current_pnl)) }
                div .sub { "at venue mid" }
            }
            div .tile {
                div .label { "Expected PnL" }
                div .value { (opt_money(r.expected_pnl)) }
                div .sub { "at fair / microprice" }
            }
            div .tile {
                div .label { "If YES / If NO" }
                div .value { span .(sign(r.pnl_if_yes)) { (money(r.pnl_if_yes)) } " / " span .(sign(r.pnl_if_no)) { (money(r.pnl_if_no)) } }
                div .sub { "at settlement" }
            }
            div .tile {
                div .label { "Worst case" }
                div .value { "$" (format!("{:.2}", r.worst_case)) }
                div .sub { "incl. resting orders" @if let Some(d) = detail { " · fees $" (format!("{:.2}", d.fees)) } }
            }
        }
    }
}

fn market_body(s: &Snapshot, row: Option<&MarketRow>, d: &MarketDetail) -> Markup {
    html! {
        div .cols {
            div {
                h2 { "Book" }
                p .sub {
                    @match d.book_ns { Some(ns) => { "Venue book as of " (utc_hms(ns)) " UTC (" (ago(s.wall_ns - ns)) " before this snapshot)." }, None => { "No book yet." } }
                    " Our resting size is in the outer columns."
                }
                div .scroll { (ladder(row, d)) }
            }
            div {
                h2 { "Our orders" }
                div .scroll {
                    table {
                        thead { tr { th { "Side" } th { "Price" } th { "Size" } th { "Filled" } th { "Leaves" } th { "State" } th { "Age" } th { "Expires (UTC)" } } }
                        tbody {
                            @if d.orders.is_empty() { tr { td colspan="8" .muted { "No working orders." } } }
                            @for o in &d.orders {
                                tr {
                                    td .market .(side_class(o.side)) { (side_name(o.side)) div .sub { (o.intent) @if o.post_only { " · post-only" } } }
                                    td { (px(o.px)) }
                                    td { (qty(o.qty)) }
                                    td { (qty(o.filled)) }
                                    td { (qty(o.leaves)) }
                                    td { (o.state) }
                                    td { (format!("{:.0}s", o.age_s)) }
                                    td { @match o.expires_ns { Some(ns) => { (utc_hms(ns)) }, None => { span .muted { "—" } } } }
                                }
                            }
                        }
                    }
                }
                p .sub {
                    "Strategy targets: bid " (target(d.target_bid)) " · ask " (target(d.target_ask))
                }
                h2 { "Our fills here" }
                div .scroll { (fills_table(&d.fills, None)) }
                h2 { "Recent trades" }
                p .sub { "Every aggressive order on the venue (ours included), newest first." }
                div .scroll {
                    table {
                        thead { tr { th { "Time (UTC)" } th { "Taker" } th { "Qty" } th { "Worst price" } } }
                        tbody {
                            @if d.trades.is_empty() { tr { td colspan="4" .muted { "No trades seen on this connection." } } }
                            @for t in &d.trades {
                                tr {
                                    td .market { @match t.trade_ns { Some(ns) => { (utc_hms(ns)) }, None => { span .muted { "—" } } } }
                                    td .(side_class(t.taker_side)) { (side_name(t.taker_side)) }
                                    td { (qty(t.qty)) }
                                    td { (px(t.worst_px)) }
                                }
                            }
                        }
                    }
                }
                h2 { "Market" }
                div .scroll {
                    table .facts {
                        tbody {
                            tr { td .market { "Slug" } td { code { (d.slug) } } }
                            tr { td .market { "Event" } td { code { (d.event_slug) } } }
                            tr { td .market { "Type" } td { (d.market_type) } }
                            tr { td .market { "Game start" } td { @match d.game_start_ns { Some(ns) => { (utc_hms(ns)) " UTC (" (relative(s.wall_ns, ns)) ")" }, None => { span .muted { "—" } } } } }
                            tr { td .market { "Tick / min size" } td { (px(d.tick)) " / " (qty(d.min_qty)) } }
                            tr { td .market { "Taker fee Θ" } td { (format!("{:.4}", d.fee_coefficient)) } }
                            tr { td .market { "Venue volume" } td { @match d.shares_traded { Some(v) => { (qty(v)) " contracts" }, None => { span .muted { "—" } } } } }
                            tr { td .market { "Books / trades received" } td { (d.books_applied) " / " (d.trades_seen) @if d.trades_lost { " · " span .warntext { "trades lost" } } } }
                        }
                    }
                }
            }
        }
    }
}

/// Price ladder: asks above (worst at the top), the spread, then bids.
fn ladder(row: Option<&MarketRow>, d: &MarketDetail) -> Markup {
    let max = d
        .bids
        .iter()
        .chain(&d.offers)
        .map(|l| l.qty)
        .fold(0.0, f64::max);
    let width = |l: &DepthLevel| {
        if max > 0.0 {
            (l.qty / max * 100.0).round()
        } else {
            0.0
        }
    };
    let ours = |q: f64| {
        if q > 0.0 {
            html! { (qty(q)) }
        } else {
            html! {}
        }
    };
    html! {
        table .ladder {
            thead { tr { th { "Our bids" } th { "Bids" } th .px { "Price" } th { "Asks" } th { "Our asks" } } }
            tbody {
                @if d.bids.is_empty() && d.offers.is_empty() { tr { td colspan="5" .muted { "Empty book." } } }
                @for l in d.offers.iter().rev() {
                    tr .mine[l.ours > 0.0] {
                        td {} td {}
                        td .px .neg { (px(l.px)) }
                        td .depth .askbar style=(format!("--w:{}%", width(l))) { (qty(l.qty)) }
                        td .ours { (ours(l.ours)) }
                    }
                }
                tr .spreadrow {
                    td colspan="5" {
                        @if let Some(r) = row {
                            "spread " @match r.spread { Some(sp) => { (format!("{:.0}¢", sp * 100.0)) }, None => { "—" } }
                            " · mid " (opt_px(r.mid))
                            " · fair " (opt_px(r.fair.or(r.expected_px)))
                        }
                    }
                }
                @for l in &d.bids {
                    tr .mine[l.ours > 0.0] {
                        td .ours { (ours(l.ours)) }
                        td .depth .bidbar style=(format!("--w:{}%", width(l))) { (qty(l.qty)) }
                        td .px .pos { (px(l.px)) }
                        td {} td {}
                    }
                }
            }
        }
    }
}

fn target(t: Option<(f64, f64)>) -> Markup {
    match t {
        Some((p, q)) => html! { (px(p)) " × " (qty(q)) },
        None => html! { span .muted { "none" } },
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Buy => "Buy",
        Side::Sell => "Sell",
        Side::Unknown => "?",
    }
}

fn side_class(side: Side) -> &'static str {
    match side {
        Side::Buy => "pos",
        Side::Sell => "neg",
        Side::Unknown => "",
    }
}

fn utc_hms(wall_ns: i64) -> String {
    let secs = wall_ns.div_euclid(1_000_000_000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// A non-negative gap as "850 ms ago", "42s ago", "3m 05s ago".
fn ago(ns: i64) -> String {
    let ns = ns.max(0);
    if ns < 1_000_000_000 {
        format!("{} ms ago", ns / 1_000_000)
    } else {
        format!("{} ago", short_duration(ns))
    }
}

/// `then` relative to `now`: "in 2h 05m" or "12m 30s ago".
fn relative(now: i64, then: i64) -> String {
    if then >= now {
        format!("in {}", short_duration(then - now))
    } else {
        format!("{} ago", short_duration(now - then))
    }
}

fn short_duration(ns: i64) -> String {
    let s = ns / 1_000_000_000;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn updown(up: bool) -> &'static str {
    if up {
        "up"
    } else {
        "DOWN"
    }
}

fn sign(v: f64) -> &'static str {
    if v > 0.005 {
        "pos"
    } else if v < -0.005 {
        "neg"
    } else {
        ""
    }
}

fn money(v: f64) -> String {
    let v = if v.abs() < 0.005 { 0.0 } else { v };
    if v < 0.0 {
        format!("−${:.2}", -v)
    } else {
        format!("${v:.2}")
    }
}

fn opt_money(v: Option<f64>) -> Markup {
    match v {
        Some(v) => html! { span .(sign(v)) { (money(v)) } },
        None => html! { span .muted { "—" } },
    }
}

fn px(p: f64) -> String {
    format!("{p:.3}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

fn opt_px(p: Option<f64>) -> Markup {
    match p {
        Some(p) => html! { (px(p)) },
        None => html! { span .muted { "—" } },
    }
}

fn qty(q: f64) -> String {
    if q.fract() == 0.0 {
        format!("{q:.0}")
    } else {
        format!("{q:.2}")
    }
}

fn level(l: Option<(f64, f64)>) -> Markup {
    match l {
        Some((p, q)) => html! { (px(p)) div .sub { "× " (qty(q)) } },
        None => html! { span .muted { "—" } },
    }
}

const SCRIPT: &str = r#"
document.body.addEventListener('htmx:sendError', () => { document.getElementById('unreachable').hidden = false; });
document.body.addEventListener('htmx:afterSwap', () => { document.getElementById('unreachable').hidden = true; });
"#;

const CSS: &str = r#"
:root { --bg:#fafaf9; --fg:#1c1917; --muted:#78716c; --line:#e7e5e4; --tile:#fff; --pos:#15803d; --neg:#b91c1c; --warn:#b45309; --warnbg:#fef3c7; --badbg:#fee2e2; --accent:#2563eb; }
@media (prefers-color-scheme: dark) { :root { --bg:#0c0a09; --fg:#e7e5e4; --muted:#a8a29e; --line:#292524; --tile:#1c1917; --pos:#4ade80; --neg:#f87171; --warn:#fbbf24; --warnbg:#422006; --badbg:#450a0a; --accent:#60a5fa; } }
* { box-sizing:border-box; }
body { margin:0; padding:16px; background:var(--bg); color:var(--fg); font:14px/1.4 system-ui, sans-serif; font-variant-numeric:tabular-nums; }
h1 { font-size:18px; margin:0; } h2 { font-size:16px; }
.top { display:flex; flex-wrap:wrap; gap:12px; align-items:center; justify-content:space-between; margin-bottom:12px; }
.brand { display:flex; flex-wrap:wrap; gap:16px; align-items:center; }
nav { display:flex; gap:4px; }
nav a { padding:4px 10px; border-radius:6px; color:var(--muted); text-decoration:none; }
nav a:hover { color:var(--fg); } nav a.active { background:var(--line); color:var(--fg); }
h2 { margin:16px 0 4px; }
table.metrics th.group { text-align:center; border-bottom:none; }
table.metrics tr.grouprow td { text-align:left; font-weight:600; background:var(--bg); }
.badge { font-size:11px; padding:2px 6px; border-radius:4px; vertical-align:middle; background:var(--line); }
.badge.live { background:var(--neg); color:#fff; }
.controls { display:flex; flex-wrap:wrap; gap:6px; }
button { font:inherit; padding:6px 12px; border:1px solid var(--line); border-radius:6px; background:var(--tile); color:var(--fg); cursor:pointer; }
button:hover { border-color:var(--accent); }
button.warn { color:var(--warn); } button.danger { background:var(--neg); border-color:var(--neg); color:#fff; }
button.small { padding:2px 8px; font-size:12px; }
.banner { padding:8px 12px; border-radius:6px; margin-bottom:8px; }
.banner.warn { background:var(--warnbg); color:var(--warn); } .banner.bad, #unreachable { background:var(--badbg); color:var(--neg); }
#unreachable { padding:8px 12px; border-radius:6px; margin-bottom:8px; }
#unreachable[hidden] { display:none; }
.tiles { display:grid; grid-template-columns:repeat(auto-fill, minmax(160px, 1fr)); gap:8px; margin-bottom:12px; }
.tile { background:var(--tile); border:1px solid var(--line); border-radius:8px; padding:10px 12px; }
.label { font-size:12px; color:var(--muted); } .value { font-size:20px; font-weight:600; }
.sub { font-size:12px; color:var(--muted); }
.bar { height:4px; background:var(--line); border-radius:2px; margin:4px 0; overflow:hidden; } .bar > div { height:100%; background:var(--warn); }
.pos { color:var(--pos); } .neg { color:var(--neg); } .muted { color:var(--muted); } .warntext { color:var(--warn); }
.live { color:var(--neg); font-weight:600; }
.scroll { overflow-x:auto; border:1px solid var(--line); border-radius:8px; background:var(--tile); }
table { border-collapse:collapse; width:100%; white-space:nowrap; }
th, td { padding:6px 10px; text-align:right; border-bottom:1px solid var(--line); vertical-align:top; }
th { font-size:12px; color:var(--muted); font-weight:500; }
th:first-child, td.market { text-align:left; } td.market .title { font-weight:500; }
tr.held td { background:color-mix(in srgb, var(--accent) 6%, transparent); }
.quotes .bid { color:var(--pos); } .quotes .ask { color:var(--neg); }
.stopped { padding:24px; text-align:center; }
a { color:inherit; } a.title { font-weight:500; text-decoration:none; } a.title:hover { text-decoration:underline; }
.mhead { display:flex; flex-wrap:wrap; gap:12px; align-items:flex-start; justify-content:space-between; margin-bottom:12px; }
.mtitle { margin:0 0 2px; font-size:18px; }
.cols { display:grid; grid-template-columns:minmax(0, 1fr) minmax(0, 1.4fr); gap:16px; align-items:start; }
@media (max-width: 900px) { .cols { grid-template-columns:minmax(0, 1fr); } }
table.ladder td, table.ladder th { text-align:center; padding:3px 10px; }
table.ladder td.px { font-weight:600; }
table.ladder td.depth { position:relative; }
table.ladder td.bidbar { background:linear-gradient(to left, color-mix(in srgb, var(--pos) 18%, transparent) var(--w), transparent var(--w)); }
table.ladder td.askbar { background:linear-gradient(to right, color-mix(in srgb, var(--neg) 18%, transparent) var(--w), transparent var(--w)); }
table.ladder td.ours { color:var(--accent); font-weight:600; }
table.ladder tr.mine td.px { outline:2px solid var(--accent); outline-offset:-2px; }
table.ladder tr.spreadrow td { font-size:12px; color:var(--muted); background:var(--bg); }
table.facts td { text-align:left; }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn app() -> (App, Arc<Inbox>) {
        let inbox = Arc::new(Inbox::new());
        let shared = Arc::new(UiShared::new(Instant::now()));
        let health = Health {
            metrics: Arc::new(Metrics::default()),
            limiter: RateLimiter::with_enabled(15.0, false).unwrap(),
            journal_dropped: Arc::new(AtomicU64::new(0)),
            journal_write_errors: Arc::new(AtomicU64::new(0)),
            log_dropped: None,
        };
        let app = App {
            shared,
            inbox: inbox.clone(),
            shutdown: Arc::new(Notify::new()),
            mode: "paper",
            token: Arc::from("t0k"),
            health,
        };
        (app, inbox)
    }

    fn post(path: &str, token: Option<&str>, origin: Option<&str>, body: &str) -> Request<Body> {
        let mut b = Request::post(path)
            .header(header::HOST, "127.0.0.1:8787")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(t) = token {
            b = b.header(TOKEN_HEADER, t);
        }
        if let Some(o) = origin {
            b = b.header(header::ORIGIN, o);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn posts_need_token_and_same_origin() {
        let (app, inbox) = app();
        let r = router(app.clone());
        assert_eq!(
            r.clone()
                .oneshot(post("/admin/pause", None, None, ""))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            r.clone()
                .oneshot(post("/admin/pause", Some("nope"), None, ""))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let evil = post("/admin/pause", Some("t0k"), Some("http://evil.example"), "");
        assert_eq!(
            r.clone().oneshot(evil).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert!(inbox.drain().is_empty());
        let ok = post(
            "/admin/pause",
            Some("t0k"),
            Some("http://127.0.0.1:8787"),
            "slug=abc",
        );
        assert_eq!(r.oneshot(ok).await.unwrap().status(), StatusCode::OK);
        assert_eq!(
            inbox.drain(),
            vec![Event::Admin(AdminCmd::Pause(Some("abc".into())))]
        );
    }

    #[tokio::test]
    async fn empty_slug_means_everything() {
        let (app, inbox) = app();
        let r = router(app);
        r.clone()
            .oneshot(post("/admin/resume", Some("t0k"), None, ""))
            .await
            .unwrap();
        r.oneshot(post("/admin/cancel-all", Some("t0k"), None, ""))
            .await
            .unwrap();
        assert_eq!(
            inbox.drain(),
            vec![
                Event::Admin(AdminCmd::Resume(None)),
                Event::Admin(AdminCmd::CancelAll)
            ]
        );
    }

    #[tokio::test]
    async fn shutdown_notifies_main() {
        let (app, _) = app();
        let notify = app.shutdown.clone();
        let res = router(app)
            .oneshot(post("/shutdown", Some("t0k"), None, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_millis(100), notify.notified())
            .await
            .expect("shutdown notified");
    }

    #[tokio::test]
    async fn polling_marks_watched_and_renders_snapshot() {
        let (app, _) = app();
        assert!(!app.shared.wanted());
        let r = router(app.clone());
        let res = r
            .clone()
            .oneshot(Request::get("/partials/state").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(app.shared.wanted());

        app.shared.publish(Snapshot {
            wall_ns: crate::journal::wall_ns(),
            markets_up: true,
            private_up: true,
            current_pnl: -1.25,
            markets: vec![MarketRow {
                slug: "aec-x".into(),
                title: "A vs B".into(),
                qty: 10.0,
                pnl_if_yes: 6.0,
                pnl_if_no: -4.0,
                ..Default::default()
            }],
            ..Default::default()
        });
        let res = r
            .oneshot(Request::get("/partials/state").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.contains("A vs B")
                && body.contains("−$1.25")
                && body.contains("−$4.00")
                && body.contains("$6.00"),
            "{body}"
        );
        assert!(!body.contains("stale"));
    }

    async fn get(r: &Router, path: &str) -> (StatusCode, String) {
        let res = r
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        (
            status,
            String::from_utf8(
                axum::body::to_bytes(res.into_body(), 1 << 20)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn market_page_asks_for_detail_and_renders_book() {
        let (app, _) = app();
        let r = router(app.clone());
        assert_eq!(get(&r, "/market/bad%20slug").await.0, StatusCode::NOT_FOUND);
        assert!(app.shared.focused().is_empty());

        let (status, _) = get(&r, "/market/aec-x").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(app.shared.focused(), vec!["aec-x".to_string()]);
        assert!(app.shared.wanted());

        let now = crate::journal::wall_ns();
        let fill = FillLog {
            wall_ns: now - 5_000_000_000,
            slug: Arc::from("aec-x"),
            side: Side::Buy,
            qty: 10.0,
            px: 0.42,
            fee: 0.0,
            aggressor: false,
            position_after: 10.0,
        };
        app.shared.publish(Snapshot {
            wall_ns: now,
            markets_up: true,
            private_up: true,
            markets: vec![MarketRow {
                slug: "aec-x".into(),
                title: "A vs B".into(),
                qty: 10.0,
                avg_cost: Some(0.42),
                our_asks: vec![(0.47, 10.0)],
                last_fill_ns: Some(fill.wall_ns),
                ..Default::default()
            }],
            recent_fills: vec![fill.clone()],
            details: vec![MarketDetail {
                slug: "aec-x".into(),
                bids: vec![DepthLevel {
                    px: 0.44,
                    qty: 25.0,
                    ours: 0.0,
                }],
                offers: vec![DepthLevel {
                    px: 0.47,
                    qty: 30.0,
                    ours: 10.0,
                }],
                fills: vec![fill],
                ..Default::default()
            }],
            ..Default::default()
        });
        let (_, body) = get(&r, "/partials/market/aec-x").await;
        assert!(
            body.contains("A vs B")
                && body.contains("0.44")
                && body.contains("0.47")
                && body.contains("Long 10 YES"),
            "{body}"
        );
        assert!(
            body.contains("maker") && !body.contains("Loading the book"),
            "{body}"
        );

        // The markets page lists the position, links to its page, and shows the exit order.
        let (_, body) = get(&r, "/partials/state").await;
        assert!(
            body.contains("Open positions")
                && body.contains(r#"href="/market/aec-x""#)
                && body.contains("S 0.47 × 10"),
            "{body}"
        );
        assert!(body.contains("5s ago"), "{body}");
    }

    #[test]
    fn focus_keeps_the_most_recent_pages() {
        let shared = UiShared::new(Instant::now());
        for i in 0..MAX_FOCUS + 2 {
            shared.latest_for(&format!("m{i}"));
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut focused = shared.focused();
        focused.sort();
        assert_eq!(
            focused,
            (2..MAX_FOCUS + 2)
                .map(|i| format!("m{i}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn metrics_page_shows_stages_without_asking_for_snapshots() {
        let (app, inbox) = app();
        let book = pm_types::BookFrame {
            slug: Arc::from("a"),
            bids: vec![],
            offers: vec![],
            state: pm_types::MarketState::Open,
            transact_ns: None,
            shares_traded: None,
            settlement: None,
        };
        inbox.book(book.clone(), std::time::Instant::now());
        inbox.book(book, std::time::Instant::now());
        app.health.metrics.record(Stage::InputToAck, 120_000_000);
        app.health.metrics.report_line(1_793_150_400_000_000_000);
        app.health.metrics.record(Stage::RestCreate, 80_000_000);
        let r = router(app.clone());
        let res = r
            .clone()
            .oneshot(
                Request::get("/partials/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = String::from_utf8(
            axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            body.contains("input_to_ack")
                && body.contains("REST create order")
                && body.contains("01:20"),
            "{body}"
        );
        assert!(
            !app.shared.wanted(),
            "the metrics page must not make the core build snapshots"
        );

        let res = r
            .oneshot(Request::get("/metrics.json").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["since_start"]["rest_create"]["n"], 1);
        assert_eq!(v["minutes"][0]["stages"]["input_to_ack"]["n"], 1);
        assert_eq!(v["books_conflated"], 1);
        assert_eq!(v["trades_dropped"], 0);
    }

    #[test]
    #[ignore]
    fn bench_ui_render() {
        let (app, _) = app();
        let core = crate::bench::quoting_core(15);
        app.shared.publish(core.snapshot(pm_core::Clock {
            mono: 2 * crate::bench::SEC,
            wall_ns: crate::journal::wall_ns(),
        }));
        for minute in 0..60 {
            for s in Stage::ALL {
                for k in 0..50 {
                    app.health.metrics.record(s, 1_000_000 + k * 10_000);
                }
            }
            app.health.metrics.report_line(minute);
        }
        crate::bench::time("ui/markets_partial_15_markets", 2_000, || {
            std::hint::black_box(state(&app).into_string());
        });
        let slug = core
            .snapshot(pm_core::Clock {
                mono: 2 * crate::bench::SEC,
                wall_ns: 0,
            })
            .markets[0]
            .slug
            .clone();
        let mut snap = core.snapshot(pm_core::Clock {
            mono: 2 * crate::bench::SEC,
            wall_ns: crate::journal::wall_ns(),
        });
        snap.details = core
            .market_detail(
                &slug,
                pm_core::Clock {
                    mono: 2 * crate::bench::SEC,
                    wall_ns: crate::journal::wall_ns(),
                },
            )
            .into_iter()
            .collect();
        app.shared.publish(snap);
        crate::bench::time("ui/market_partial_40_levels", 2_000, || {
            std::hint::black_box(market_view(&app, &slug).into_string());
        });
        crate::bench::time("ui/metrics_partial_60_minutes", 2_000, || {
            std::hint::black_box(metrics_view(&app).into_string());
        });
    }
}
