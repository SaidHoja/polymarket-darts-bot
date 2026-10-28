//! I/O tasks: discovery, game-state poller, markets socket, private socket.
//! They only parse, validate, timestamp and journal; decisions are the core's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use pm_types::time::parse_rfc3339_ns;
use pm_types::{ConnState, Event, Feed, Slug};
use pm_venue::decode::{self, MarketMsg, PrivateMsg};
use pm_venue::ws::{self, ConnectError};
use pm_venue::{Budget, RestClient, Signer};
use rand::Rng;
use serde_json::json;
use tokio::sync::{watch, Notify};
use tokio_tungstenite::tungstenite::Message;

use crate::inbox::Inbox;
use crate::journal::{wall_ns, Journal};
use crate::metrics::{Metrics, Stage};

pub struct Ctx {
    pub inbox: Arc<Inbox>,
    pub journal: Journal,
    pub rest: Arc<RestClient>,
    pub signer: Arc<Signer>,
    pub start: Instant,
    /// Slugs the markets socket should be subscribed to (set by discovery).
    pub slugs: watch::Sender<Vec<Slug>>,
    /// Run discovery now (after a market's parameters were rejected).
    pub refresh: Arc<Notify>,
    pub include_futures: bool,
    /// Feed actual unchanged REST poll responses to diagnostic probe strategies.
    pub probe_poll_inputs: bool,
    /// Subscribe only to these `sportsMarketType`s; `None` = everything (record mode).
    pub market_types: Option<Vec<String>>,
    pub metrics: Arc<Metrics>,
}

impl Ctx {
    pub fn mono(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

fn jitter(d: Duration) -> Duration {
    d.mul_f64(rand::thread_rng().gen_range(0.5..1.0))
}

const DISCOVERY_EVERY: Duration = Duration::from_secs(300);
const LIVENESS_PING: Duration = Duration::from_secs(5);
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- discovery

pub async fn discovery(ctx: Arc<Ctx>) {
    let mut failures = 0u32;
    loop {
        let mut kinds = vec![("sport", false)];
        if ctx.include_futures {
            kinds.push(("futures", true));
        }
        let mut markets = Vec::new();
        let mut ok = true;
        for (kind, is_futures) in kinds {
            let sent = Instant::now();
            let result = ctx.rest.darts_events(kind, Budget::Low).await;
            ctx.metrics.record_since(Stage::RestRead, sent);
            match result {
                Ok(events) => {
                    let (m, _, _) =
                        decode::decode_events_page(&json!({"events": events}), is_futures);
                    markets.extend(m);
                }
                Err(e) => {
                    ok = false;
                    tracing::warn!("discovery ({kind}) failed: {e}");
                }
            }
        }
        if ok {
            failures = 0;
            let mut seen = BTreeSet::new();
            markets.retain(|m| seen.insert(m.slug.clone()));
            let slugs: Vec<Slug> = markets
                .iter()
                .filter(|m| {
                    ctx.market_types
                        .as_ref()
                        .is_none_or(|t| t.contains(&m.sports_market_type))
                })
                .map(|m| m.slug.clone())
                .collect();
            tracing::info!(
                "discovery: {} markets, {} subscribed",
                markets.len(),
                slugs.len()
            );
            ctx.journal.record(
                "discovery-",
                wall_ns(),
                || json!({"recv_at_ns": wall_ns(), "markets": markets}),
            );
            ctx.inbox.control(Event::Markets(markets));
            let _ = ctx.slugs.send(slugs);
        } else {
            failures += 1;
            if failures >= 3 {
                tracing::error!(
                    "discovery failed {failures} times in a row; keeping the previous market set"
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(if ok { DISCOVERY_EVERY } else { Duration::from_secs(30) }) => {}
            _ = ctx.refresh.notified() => tracing::info!("discovery: refresh requested"),
        }
    }
}

// ---------------------------------------------------------------- game state

/// What makes a game frame worth forwarding: score, period, live, ended, venue update time.
type GameKey = (Option<(u16, u16)>, String, bool, bool, Option<i64>);

pub async fn game_poller(ctx: Arc<Ctx>) {
    let mut last: BTreeMap<String, GameKey> = BTreeMap::new();
    let mut errors = 0u32;
    loop {
        let mut interval = Duration::from_secs(30);
        let sent = Instant::now();
        let result = ctx.rest.darts_events("sport", Budget::Low).await;
        let received = Instant::now();
        ctx.metrics.record_since(Stage::RestRead, sent);
        match result {
            Ok(events) => {
                errors = 0;
                let now = wall_ns();
                for event in &events {
                    let frame = decode::decode_game(event);
                    let key = (
                        frame.score,
                        frame.period.clone(),
                        frame.live,
                        frame.ended,
                        frame.updated_ns,
                    );
                    if frame.live && !frame.ended {
                        interval = interval.min(Duration::from_secs(1));
                    } else if event
                        .get("startTime")
                        .and_then(|v| v.as_str())
                        .and_then(parse_rfc3339_ns)
                        .is_some_and(|s| {
                            s - now < 30 * 60 * 1_000_000_000
                                && s - now > -6 * 3_600 * 1_000_000_000
                        })
                    {
                        interval = interval.min(Duration::from_secs(5));
                    }
                    if ctx.probe_poll_inputs || last.get(&frame.event_id) != Some(&key) {
                        last.insert(frame.event_id.clone(), key);
                        ctx.journal.record(
                            "game-",
                            now,
                            || json!({"recv_at_ns": now, "mono": ctx.mono(), "game": frame}),
                        );
                        ctx.inbox.game(frame, received);
                    }
                }
                ctx.inbox.game_polled();
            }
            Err(e) => {
                errors += 1;
                if errors <= 3 || errors % 30 == 0 {
                    tracing::warn!("game poll failed ({errors}): {e}");
                }
                interval = Duration::from_secs(1);
            }
        }
        tokio::time::sleep(interval).await;
    }
}

// ---------------------------------------------------------------- markets socket

pub async fn markets_ws(ctx: Arc<Ctx>) {
    let mut slugs_rx = ctx.slugs.subscribe();
    let mut conn = 0u64;
    let mut backoff = Duration::from_millis(250);
    loop {
        let slugs = slugs_rx.borrow_and_update().clone();
        if slugs.is_empty() {
            if slugs_rx.changed().await.is_err() {
                return;
            }
            continue;
        }
        let started = Instant::now();
        match ws::connect_signed(ws::MARKETS_WS_URL, &ctx.signer).await {
            Err(ConnectError::Unauthorized(body)) => {
                tracing::error!("markets socket: 401 {body}; check credentials and clock");
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
            Err(e) => tracing::warn!("markets socket connect failed: {e}"),
            Ok(mut socket) => {
                conn += 1;
                let mut subscribed: BTreeSet<Slug> = slugs.iter().cloned().collect();
                // The first book per market after subscribing is a snapshot stamped with the market's
                // last change, possibly hours ago; only later books measure delivery lag.
                let mut seeded: BTreeSet<Slug> = BTreeSet::new();
                let mut next_group = 0usize;
                let mut ok = true;
                for (i, msg) in ws::market_subscriptions(&slugs).into_iter().enumerate() {
                    next_group = i / 2 + 1;
                    if socket.send(Message::Text(msg.into())).await.is_err() {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    tracing::info!(
                        "markets socket conn={conn} subscribed {} markets",
                        subscribed.len()
                    );
                    ctx.inbox.control(Event::Connection {
                        feed: Feed::Markets,
                        state: ConnState::Up,
                    });
                    let mut ping = tokio::time::interval(LIVENESS_PING);
                    let mut last_rx = Instant::now();
                    let mut ping_sent: Option<Instant> = None;
                    loop {
                        tokio::select! {
                            msg = socket.next() => match msg {
                                Some(Ok(Message::Text(text))) => {
                                    last_rx = Instant::now();
                                    let (wall, mono) = (wall_ns(), ctx.mono());
                                    ctx.journal.raw("", conn, text.as_str(), wall, mono);
                                    match decode::decode_market_frame(text.as_str()) {
                                        Ok(MarketMsg::Book(book)) => {
                                            if !seeded.insert(book.slug.clone()) {
                                                if let Some(t) = book.transact_ns {
                                                    ctx.metrics.record(Stage::BookLag, wall - t);
                                                }
                                            }
                                            ctx.inbox.book(book, last_rx);
                                            ctx.metrics.record_since(Stage::Feed, last_rx);
                                        }
                                        Ok(MarketMsg::Trade(trade)) => {
                                            if let Some(t) = trade.trade_ns {
                                                ctx.metrics.record(Stage::TradeLag, wall - t);
                                            }
                                            ctx.inbox.trade(trade, last_rx);
                                            ctx.metrics.record_since(Stage::Feed, last_rx);
                                        }
                                        // Some markets would get no books or trades while the socket looks healthy:
                                        // reconnect, which subscribes everything again.
                                        Ok(MarketMsg::SubscriptionError { request_id, error }) => {
                                            tracing::error!("markets socket: subscription {request_id} failed: {error}; reconnecting");
                                            break;
                                        }
                                        Ok(MarketMsg::Other) => {}
                                        Err(e) => tracing::warn!("undecodable market frame: {e}"),
                                    }
                                }
                                Some(Ok(Message::Close(frame))) => {
                                    tracing::warn!("markets socket closed: {frame:?}");
                                    break;
                                }
                                Some(Ok(Message::Pong(_))) => {
                                    last_rx = Instant::now();
                                    if let Some(sent) = ping_sent.take() {
                                        ctx.metrics.record_since(Stage::WsRttMarkets, sent);
                                    }
                                }
                                Some(Ok(_)) => last_rx = Instant::now(),
                                Some(Err(e)) => {
                                    tracing::warn!("markets socket error: {e}");
                                    break;
                                }
                                None => break,
                            },
                            _ = ping.tick() => {
                                if last_rx.elapsed() > LIVENESS_TIMEOUT {
                                    tracing::warn!("markets socket silent for {:?}; reconnecting", last_rx.elapsed());
                                    break;
                                }
                                ping_sent = Some(Instant::now());
                                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                                    break;
                                }
                            }
                            changed = slugs_rx.changed() => {
                                if changed.is_err() {
                                    return;
                                }
                                let new: Vec<Slug> = slugs_rx.borrow_and_update().iter().filter(|s| !subscribed.contains(*s)).cloned().collect();
                                if new.is_empty() {
                                    continue;
                                }
                                let mut failed = false;
                                for group in new.chunks(ws::MAX_SLUGS) {
                                    for (id, kind) in [(format!("md-{next_group}"), ws::MARKET_DATA), (format!("tr-{next_group}"), ws::TRADE)] {
                                        if socket.send(Message::Text(ws::subscribe(&id, kind, group).into())).await.is_err() {
                                            failed = true;
                                        }
                                    }
                                    next_group += 1;
                                }
                                if failed {
                                    break;
                                }
                                tracing::info!("markets socket: subscribed {} new markets", new.len());
                                subscribed.extend(new);
                            }
                        }
                    }
                }
                ctx.inbox.markets_down();
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_millis(250);
        }
        let wait = jitter(backoff);
        tracing::info!("markets socket reconnecting in {wait:?}");
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

// ---------------------------------------------------------------- private socket

/// Startup per connection: subscribe, collect the order snapshot (buffering updates),
/// read positions and balance over REST, then hand everything to the core and mark the feed up.
pub async fn private_ws(ctx: Arc<Ctx>) {
    let mut conn = 0u64;
    let mut backoff = Duration::from_millis(250);
    loop {
        let started = Instant::now();
        match ws::connect_signed(ws::PRIVATE_WS_URL, &ctx.signer).await {
            Err(ConnectError::Unauthorized(body)) => {
                tracing::error!("private socket: 401 {body}; check credentials and clock");
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
            Err(e) => tracing::warn!("private socket connect failed: {e}"),
            Ok(mut socket) => {
                conn += 1;
                let result = run_private(&ctx, &mut socket, conn).await;
                if let Err(e) = result {
                    tracing::warn!("private socket conn={conn}: {e}");
                }
                ctx.inbox.control(Event::Connection {
                    feed: Feed::Private,
                    state: ConnState::Down,
                });
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_millis(250);
        }
        let wait = jitter(backoff);
        tracing::info!("private socket reconnecting in {wait:?}");
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn private_event(msg: PrivateMsg) -> Option<Event> {
    match msg {
        PrivateMsg::Execution(e) => Some(Event::Execution(e)),
        PrivateMsg::Position(p) => Some(Event::Position(p)),
        PrivateMsg::Balance(b) => Some(Event::Balance(b)),
        // `run_private` ends the connection on these before they get here.
        PrivateMsg::SubscriptionError { .. } | PrivateMsg::Snapshot { .. } | PrivateMsg::Other => {
            None
        }
    }
}

async fn run_private(ctx: &Ctx, socket: &mut ws::Socket, conn: u64) -> Result<(), String> {
    for msg in ws::private_subscriptions() {
        socket
            .send(Message::Text(msg.into()))
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut snapshot = Vec::new();
    let mut buffered = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    // 1. Order snapshot (accumulate until eof); everything else is buffered.
    loop {
        let msg = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| "no order snapshot within 10 s".to_string())?;
        match msg {
            Some(Ok(Message::Text(text))) => {
                ctx.journal
                    .raw("private-", conn, text.as_str(), wall_ns(), ctx.mono());
                match decode::decode_private_frame(text.as_str()) {
                    Ok(PrivateMsg::Snapshot { orders, eof }) => {
                        snapshot.extend(orders);
                        if eof {
                            break;
                        }
                    }
                    // Without every subscription the snapshot can't be trusted (an auth failure arrives
                    // with an empty `eof` snapshot attached), so start the connection again.
                    Ok(PrivateMsg::SubscriptionError { request_id, error }) => {
                        return Err(format!(
                            "subscription {request_id} failed during startup: {error}"
                        ));
                    }
                    Ok(other) => buffered.extend(private_event(other)),
                    Err(e) => return Err(format!("undecodable private frame during startup: {e}")),
                }
            }
            Some(Ok(Message::Close(_))) | None => return Err("closed during startup".into()),
            Some(Err(e)) => return Err(e.to_string()),
            Some(Ok(_)) => {}
        }
    }
    // 2. Starting positions and balance from REST, on every connection.
    let sent = Instant::now();
    let positions = ctx
        .rest
        .positions(Budget::Normal)
        .await
        .map_err(|e| format!("positions: {e}"))?;
    ctx.metrics.record_since(Stage::RestRead, sent);
    let sent = Instant::now();
    let balance = ctx
        .rest
        .balance(Budget::Normal)
        .await
        .map_err(|e| format!("balance: {e}"))?;
    ctx.metrics.record_since(Stage::RestRead, sent);
    tracing::info!(
        "private socket conn={conn} ready: {} open orders, {} positions",
        snapshot.len(),
        positions.len()
    );
    // 3. Hand over in order, then mark the feed up.
    ctx.inbox.feedback(Event::OrderSnapshot(snapshot));
    ctx.inbox.feedback(Event::Positions(positions));
    if let Some(b) = balance {
        ctx.inbox.feedback(Event::Balance(b));
    }
    for event in buffered {
        ctx.inbox.feedback(event);
    }
    ctx.inbox.control(Event::Connection {
        feed: Feed::Private,
        state: ConnState::Up,
    });

    let mut ping = tokio::time::interval(LIVENESS_PING);
    let mut last_rx = Instant::now();
    let mut ping_sent: Option<Instant> = None;
    loop {
        tokio::select! {
            msg = socket.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    last_rx = Instant::now();
                    ctx.journal.raw("private-", conn, text.as_str(), wall_ns(), ctx.mono());
                    match decode::decode_private_frame(text.as_str()) {
                        Ok(PrivateMsg::Snapshot { orders, eof }) => {
                            tracing::warn!("unexpected order snapshot after startup ({} orders, eof {eof})", orders.len());
                        }
                        Ok(PrivateMsg::SubscriptionError { request_id, error }) => {
                            return Err(format!("subscription {request_id} failed: {error}"));
                        }
                        Ok(other) => {
                            if let Some(event) = private_event(other) {
                                ctx.inbox.feedback(event);
                            }
                        }
                        // A dropped execution could be a lost fill: reconnect, which puts every order
                        // in doubt and re-reads the snapshot and positions.
                        Err(e) => return Err(format!("undecodable private frame: {e}")),
                    }
                }
                Some(Ok(Message::Close(frame))) => return Err(format!("closed: {frame:?}")),
                Some(Ok(Message::Pong(_))) => {
                    last_rx = Instant::now();
                    if let Some(sent) = ping_sent.take() {
                        ctx.metrics.record_since(Stage::WsRttPrivate, sent);
                    }
                }
                Some(Ok(_)) => last_rx = Instant::now(),
                Some(Err(e)) => return Err(e.to_string()),
                None => return Err("stream ended".into()),
            },
            _ = ping.tick() => {
                if last_rx.elapsed() > LIVENESS_TIMEOUT {
                    return Err(format!("silent for {:?}", last_rx.elapsed()));
                }
                ping_sent = Some(Instant::now());
                socket.send(Message::Ping(Vec::new().into())).await.map_err(|e| e.to_string())?;
            }
        }
    }
}
