//! Order gateway: the only component that sends trading REST. It owns the rate
//! budget, a priority queue (cancel-all > cancels > reducing > quotes > refresh >
//! reads), batching (≤ 20 per batch) and `Withdraw`. Every action produces exactly
//! one `RestResult` back to the core.
//!
//! The paper gateway has the same interface but acks locally and never fills.
//!
//! A [`Gate`] shared with shutdown and the watchdog stops creates and modifies (queued ones are
//! withdrawn) while cancels still go out, and counts those already sent, so a final cancel-all
//! can wait until nothing it can't see is still on its way to the venue.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pm_types::{
    BalanceFrame, Command, Event, ExecKind, ExecutionFrame, NewOrder, OrderId, OrderState,
    Priority, RejectClass, RestKind, RestOutcome, RestResult, Slug, Token, VenueOrder,
};
use pm_venue::decode::classify_reject;
use pm_venue::rest::{modify_body, BATCH_LIMIT};
use pm_venue::{Budget, RestClient, RestError};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Notify;

use crate::inbox::Inbox;
use crate::metrics::{Metrics, Stage};
use crate::timing::Outbound;

fn token_of(cmd: &Command) -> Option<Token> {
    match cmd {
        Command::Place { token, .. }
        | Command::Modify { token, .. }
        | Command::Cancel { token, .. }
        | Command::CancelAll { token, .. } => Some(*token),
        _ => None,
    }
}

fn kind_of(cmd: &Command) -> Option<RestKind> {
    match cmd {
        Command::Place { .. } => Some(RestKind::Create),
        Command::Modify { .. } => Some(RestKind::Modify),
        Command::Cancel { .. } => Some(RestKind::Cancel),
        Command::CancelAll { .. } => Some(RestKind::CancelAll),
        _ => None,
    }
}

fn outcome_of(err: &RestError) -> RestOutcome {
    match err {
        RestError::Status { status: 401, body } => RestOutcome::AuthFailed { body: body.clone() },
        RestError::Status { status: 429, .. } => RestOutcome::RateLimited,
        RestError::Status { status, body } if *status >= 400 && *status < 500 => {
            let class = match classify_reject("", body) {
                RejectClass::Unknown => RejectClass::OurBug,
                c => c,
            };
            RestOutcome::GatewayRejected {
                status: *status,
                body: body.clone(),
                class,
            }
        }
        other => RestOutcome::Ambiguous {
            error: other.to_string(),
        },
    }
}

/// Worth one retry: 429 (after the limiter's back-off) and the latency stopgap.
fn retryable(err: &RestError) -> bool {
    match err {
        RestError::Status { status: 429, .. } => true,
        RestError::Status { body, .. } => body
            .to_ascii_lowercase()
            .contains("global rate limit exceeded"),
        _ => false,
    }
}

/// Whether new or changed orders may be sent, and how many such requests are in flight.
#[derive(Default)]
pub struct Gate {
    /// Shutting down: never lifted.
    shutdown: AtomicBool,
    /// The watchdog's: lifted when the core recovers.
    emergency: AtomicBool,
    in_flight: AtomicUsize,
}

/// One create or modify request on its way; dropping it ends the count.
struct Flight<'a>(&'a Gate);

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, SeqCst);
    }
}

impl Gate {
    pub fn shut(&self) {
        self.shutdown.store(true, SeqCst);
    }

    pub fn set_emergency(&self, on: bool) {
        self.emergency.store(on, SeqCst);
    }

    pub fn shutting_down(&self) -> bool {
        self.shutdown.load(SeqCst)
    }

    pub fn halted(&self) -> bool {
        self.shutdown.load(SeqCst) || self.emergency.load(SeqCst)
    }

    /// Counted before checking `halted` (both sequentially consistent), so a halt either sees this
    /// request in `in_flight` or this request sees the halt: never neither.
    fn enter(&self) -> Option<Flight<'_>> {
        self.in_flight.fetch_add(1, SeqCst);
        let flight = Flight(self);
        (!self.halted()).then_some(flight)
    }

    /// Wait until no create or modify is in flight; false if `timeout` passed first.
    pub async fn idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.in_flight.load(SeqCst) > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        true
    }
}

fn changes_risk(cmd: &Command) -> bool {
    matches!(cmd, Command::Place { .. } | Command::Modify { .. })
}

fn withdrawn(cmd: &Command) -> Option<Event> {
    Some(Event::RestResult(RestResult {
        kind: kind_of(cmd)?,
        tokens: vec![token_of(cmd)?],
        outcome: RestOutcome::Withdrawn,
    }))
}

/// Queued commands with the time they reached the gateway, in send order.
type Queue = BTreeMap<(Priority, u64), (Command, Instant, Option<Instant>)>;

pub async fn run_gateway(
    rest: Arc<RestClient>,
    mut rx: UnboundedReceiver<Outbound>,
    inbox: Arc<Inbox>,
    refresh: Arc<Notify>,
    metrics: Arc<Metrics>,
    gate: Arc<Gate>,
) {
    let mut queue: Queue = BTreeMap::new();
    let mut seq = 0u64;
    let enqueue = |outbound: Outbound, queue: &mut Queue, seq: &mut u64| {
        let Outbound {
            command: cmd,
            received,
            sent,
        } = outbound;
        metrics.record_since(Stage::GwHandoff, sent);
        match cmd {
            Command::Withdraw { token } => {
                let key = queue
                    .iter()
                    .find(|(_, (c, _, _))| token_of(c) == Some(token))
                    .map(|(k, _)| *k);
                if let Some(key) = key {
                    let (cmd, _, _) = queue.remove(&key).unwrap();
                    inbox.feedback(Event::RestResult(RestResult {
                        kind: kind_of(&cmd).unwrap(),
                        tokens: vec![token],
                        outcome: RestOutcome::Withdrawn,
                    }));
                }
            }
            Command::RefreshMarket { .. } => refresh.notify_one(),
            cmd => {
                *seq += 1;
                queue.insert((cmd.priority(), *seq), (cmd, Instant::now(), received));
            }
        }
    };
    loop {
        while let Ok(cmd) = rx.try_recv() {
            enqueue(cmd, &mut queue, &mut seq);
        }
        if gate.halted() {
            // Halted: creates and modifies go back to the core as withdrawn; cancels still go out.
            queue.retain(|_, (cmd, _, _)| {
                if !changes_risk(cmd) {
                    return true;
                }
                inbox.feedback(withdrawn(cmd).expect("creates and modifies have a kind and token"));
                false
            });
        }
        if queue.is_empty() {
            match rx.recv().await {
                Some(cmd) => enqueue(cmd, &mut queue, &mut seq),
                None => return,
            }
            continue;
        }
        tokio::select! {
            biased;
            cmd = rx.recv() => match cmd {
                Some(cmd) => enqueue(cmd, &mut queue, &mut seq),
                None => return,
            },
            _ = rest.limiter.acquire() => {
                metrics.record_count(Stage::GwDepth, queue.len());
                let batch = take_batch(&mut queue, &rest).await;
                let taken = Instant::now();
                for (_, queued, _) in &batch {
                    metrics.record(Stage::GwWait, taken.duration_since(*queued).as_nanos() as i64);
                }
                let batch = batch.into_iter().map(|(command, _, received)| Outbound { command, received, sent: taken }).collect();
                tokio::spawn(execute(rest.clone(), inbox.clone(), metrics.clone(), gate.clone(), batch, taken));
            }
        }
    }
}

/// The first queued action plus more of the same kind, one extra token each, up to the batch limit.
async fn take_batch(
    queue: &mut Queue,
    rest: &RestClient,
) -> Vec<(Command, Instant, Option<Instant>)> {
    let Some((_, first)) = queue.pop_first() else {
        return Vec::new();
    };
    let batchable = matches!(
        first.0,
        Command::Place { .. } | Command::Modify { .. } | Command::Cancel { .. }
    );
    let mut batch = vec![first];
    if !batchable {
        return batch;
    }
    let same = |c: &Command| std::mem::discriminant(c) == std::mem::discriminant(&batch[0].0);
    let keys: Vec<(Priority, u64)> = queue
        .iter()
        .filter(|(_, (c, _, _))| same(c))
        .map(|(k, _)| *k)
        .take(BATCH_LIMIT - 1)
        .collect();
    for key in keys {
        if !rest.limiter.try_acquire().await {
            break;
        }
        batch.push(queue.remove(&key).unwrap());
    }
    batch
}

/// `taken`: when the gateway took this batch off its queue (`gw_spawn` starts here).
async fn execute(
    rest: Arc<RestClient>,
    inbox: Arc<Inbox>,
    metrics: Arc<Metrics>,
    gate: Arc<Gate>,
    batch: Vec<Outbound>,
    taken: Instant,
) {
    // Creates and modifies are counted while in flight; halted in the meantime, they aren't sent.
    let _flight = if changes_risk(&batch[0].command) {
        match gate.enter() {
            Some(flight) => Some(flight),
            None => {
                for o in &batch {
                    inbox.feedback(
                        withdrawn(&o.command).expect("creates and modifies have a kind and token"),
                    );
                }
                return;
            }
        }
    } else {
        None
    };
    let started = Instant::now();
    metrics.record(
        Stage::GwSpawn,
        started.duration_since(taken).as_nanos() as i64,
    );
    let is_order = matches!(
        batch[0].command,
        Command::Place { .. }
            | Command::Modify { .. }
            | Command::Cancel { .. }
            | Command::CancelAll { .. }
    );
    let origins: Vec<Instant> = batch.iter().filter_map(|o| o.received).collect();
    let observed = std::sync::atomic::AtomicBool::new(false);
    let send_metrics = metrics.clone();
    let rest = rest.with_dispatch_observer(Arc::new(move || {
        // Retries retain the original origin, but send latency is sampled once.
        if !observed.swap(true, std::sync::atomic::Ordering::Relaxed) {
            if is_order {
                send_metrics.record_since(Stage::GwBuild, started);
            }
            for at in &origins {
                send_metrics.record_since(Stage::InputToSend, *at);
            }
        }
    }));
    let batch: Vec<Command> = batch.into_iter().map(|o| o.command).collect();
    let result = |kind, tokens: Vec<Token>, outcome| {
        Event::RestResult(RestResult {
            kind,
            tokens,
            outcome,
        })
    };
    let stage = match &batch[0] {
        Command::Place { .. } => Stage::RestCreate,
        Command::Modify { .. } => Stage::RestModify,
        Command::Cancel { .. } => Stage::RestCancel,
        Command::CancelAll { .. } => Stage::RestCancelAll,
        _ => Stage::RestRead,
    };
    let sent = Instant::now();
    match &batch[0] {
        Command::Place { .. } => {
            let tokens: Vec<Token> = batch.iter().filter_map(token_of).collect();
            let orders: Vec<NewOrder> = batch
                .iter()
                .filter_map(|c| match c {
                    Command::Place { order, .. } => Some(order.clone()),
                    _ => None,
                })
                .collect();
            let mut outcome = rest.create_orders(&orders, Budget::Prepaid).await;
            if let Err(e) = &outcome {
                if retryable(e) && !gate.halted() {
                    tracing::warn!("create retry after: {e}");
                    outcome = rest.create_orders(&orders, Budget::Normal).await;
                }
            }
            let outcome = match outcome {
                Ok(ids) => RestOutcome::Accepted { ids },
                Err(e) => {
                    tracing::warn!("create failed: {e}");
                    outcome_of(&e)
                }
            };
            inbox.feedback(result(RestKind::Create, tokens, outcome));
        }
        Command::Modify { .. } => {
            let tokens: Vec<Token> = batch.iter().filter_map(token_of).collect();
            let entries: Vec<_> = batch
                .iter()
                .filter_map(|c| match c {
                    Command::Modify {
                        order_id,
                        slug,
                        px,
                        qty,
                        tif,
                        post_only,
                        ..
                    } => Some(modify_body(order_id, slug, *px, *qty, *tif, *post_only)),
                    _ => None,
                })
                .collect();
            let mut outcome = rest.modify_orders(entries.clone(), Budget::Prepaid).await;
            if let Err(e) = &outcome {
                if retryable(e) && !gate.halted() {
                    outcome = rest.modify_orders(entries, Budget::Normal).await;
                }
            }
            let outcome = match outcome {
                Ok(()) => RestOutcome::Accepted { ids: Vec::new() },
                Err(e) => {
                    tracing::warn!("modify failed: {e}");
                    outcome_of(&e)
                }
            };
            inbox.feedback(result(RestKind::Modify, tokens, outcome));
        }
        Command::Cancel { .. } => {
            let tokens: Vec<Token> = batch.iter().filter_map(token_of).collect();
            let pairs: Vec<(OrderId, Slug)> = batch
                .iter()
                .filter_map(|c| match c {
                    Command::Cancel { order_id, slug, .. } => {
                        Some((order_id.clone(), slug.clone()))
                    }
                    _ => None,
                })
                .collect();
            let mut outcome = rest.cancel_orders(&pairs, Budget::Prepaid).await;
            if matches!(&outcome, Err(e) if retryable(e)) {
                outcome = rest.cancel_orders(&pairs, Budget::Normal).await;
            }
            let outcome = match outcome {
                Ok(()) => RestOutcome::Accepted { ids: Vec::new() },
                Err(e) => {
                    tracing::warn!("cancel failed: {e}");
                    outcome_of(&e)
                }
            };
            inbox.feedback(result(RestKind::Cancel, tokens, outcome));
        }
        Command::CancelAll { token, slugs } => {
            let outcome = match rest.cancel_all(slugs, Budget::Prepaid).await {
                Ok(()) => RestOutcome::Accepted { ids: Vec::new() },
                Err(e) => {
                    tracing::error!("cancel-all failed: {e}");
                    outcome_of(&e)
                }
            };
            inbox.feedback(result(RestKind::CancelAll, vec![*token], outcome));
        }
        Command::QueryOpenOrders { slugs } => {
            match rest.open_orders(slugs, Budget::Prepaid).await {
                Ok(orders) => inbox.feedback(Event::OpenOrders {
                    slugs: slugs.clone(),
                    orders,
                }),
                Err(e) => tracing::warn!("open orders query failed: {e}"),
            }
        }
        Command::QueryOrder { order_id } => match rest.order(order_id, Budget::Prepaid).await {
            Ok(order) => inbox.feedback(Event::OrderLookup {
                id: order_id.clone(),
                order,
            }),
            Err(e) => tracing::warn!("order lookup failed: {e}"),
        },
        Command::QueryPositions => match rest.positions(Budget::Prepaid).await {
            Ok(p) => inbox.feedback(Event::Positions(p)),
            Err(e) => tracing::warn!("positions query failed: {e}"),
        },
        Command::QueryBalance => match rest.balance(Budget::Prepaid).await {
            Ok(Some(b)) => inbox.feedback(Event::Balance(b)),
            Ok(None) => {}
            Err(e) => tracing::warn!("balance query failed: {e}"),
        },
        Command::QuerySettlement { slug } => match rest.settlement(slug, Budget::Prepaid).await {
            Ok(px) => inbox.feedback(Event::Settlement {
                slug: slug.clone(),
                px,
            }),
            Err(e) => tracing::warn!("settlement query for {slug} failed: {e}"),
        },
        Command::Withdraw { .. } | Command::RefreshMarket { .. } => {}
    }
    // Includes the one retry, if any: this is what the core waited for.
    metrics.record_since(stage, sent);
}

/// Paper trading: acks, modifies and cancels locally, never fills.
pub async fn run_paper_gateway(mut rx: UnboundedReceiver<Outbound>, inbox: Arc<Inbox>) {
    let mut orders: BTreeMap<OrderId, VenueOrder> = BTreeMap::new();
    let mut next = 0u64;
    let exec = |order: &VenueOrder, kind: ExecKind, n: u64| {
        Event::Execution(ExecutionFrame {
            exec_id: format!("paper-exec-{n}"),
            order: order.clone(),
            kind,
            fill: None,
            reject: None,
            unsolicited: None,
            transact_ns: None,
        })
    };
    let feedback = |e| inbox.feedback(e);
    feedback(Event::OrderSnapshot(Vec::new()));
    feedback(Event::Positions(Vec::new()));
    feedback(Event::Balance(BalanceFrame {
        current_balance: 100.0,
        buying_power: 100.0,
        open_orders: 0.0,
    }));
    inbox.control(Event::Connection {
        feed: pm_types::Feed::Private,
        state: pm_types::ConnState::Up,
    });
    while let Some(outbound) = rx.recv().await {
        let cmd = outbound.command;
        tokio::time::sleep(Duration::from_millis(20)).await;
        next += 1;
        match cmd {
            Command::Place { token, order, .. } => {
                let id: OrderId = format!("PAPER{next}").into();
                tracing::info!(
                    "[paper] place {} {:?} {} @ {}",
                    order.slug,
                    order.intent,
                    order.qty,
                    order.px
                );
                feedback(Event::RestResult(RestResult {
                    kind: RestKind::Create,
                    tokens: vec![token],
                    outcome: RestOutcome::Accepted {
                        ids: vec![Some(id.clone())],
                    },
                }));
                let venue = VenueOrder {
                    id: id.clone(),
                    slug: order.slug,
                    side: order.intent.side(),
                    intent: order.intent,
                    px: order.px,
                    qty: order.qty,
                    cum_qty: pm_types::Qty::ZERO,
                    leaves_qty: order.qty,
                    state: OrderState::New,
                };
                feedback(exec(&venue, ExecKind::New, next));
                orders.insert(id, venue);
            }
            Command::Modify {
                token,
                order_id,
                px,
                qty,
                ..
            } => {
                feedback(Event::RestResult(RestResult {
                    kind: RestKind::Modify,
                    tokens: vec![token],
                    outcome: RestOutcome::Accepted { ids: Vec::new() },
                }));
                if let Some(o) = orders.get_mut(&order_id) {
                    tracing::info!(
                        "[paper] modify {} {:?} -> {} @ {}",
                        o.slug,
                        o.intent,
                        qty,
                        px
                    );
                    o.px = px;
                    o.qty = qty;
                    o.leaves_qty = qty;
                    feedback(exec(o, ExecKind::Replace, next));
                }
            }
            Command::Cancel {
                token, order_id, ..
            } => {
                feedback(Event::RestResult(RestResult {
                    kind: RestKind::Cancel,
                    tokens: vec![token],
                    outcome: RestOutcome::Accepted { ids: Vec::new() },
                }));
                if let Some(mut o) = orders.remove(&order_id) {
                    tracing::info!("[paper] cancel {} {:?}", o.slug, o.intent);
                    o.state = OrderState::Canceled;
                    feedback(exec(&o, ExecKind::Canceled, next));
                }
            }
            Command::CancelAll { token, slugs } => {
                feedback(Event::RestResult(RestResult {
                    kind: RestKind::CancelAll,
                    tokens: vec![token],
                    outcome: RestOutcome::Accepted { ids: Vec::new() },
                }));
                let ids: Vec<OrderId> = orders
                    .iter()
                    .filter(|(_, o)| slugs.is_empty() || slugs.contains(&o.slug))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    let mut o = orders.remove(&id).unwrap();
                    o.state = OrderState::Canceled;
                    next += 1;
                    feedback(exec(&o, ExecKind::Canceled, next));
                }
            }
            Command::QueryOpenOrders { slugs } => {
                let list = orders
                    .values()
                    .filter(|o| slugs.is_empty() || slugs.contains(&o.slug))
                    .cloned()
                    .collect();
                feedback(Event::OpenOrders {
                    slugs,
                    orders: list,
                });
            }
            Command::QueryOrder { order_id } => {
                let order = orders.get(&order_id).cloned();
                feedback(Event::OrderLookup {
                    id: order_id,
                    order,
                });
            }
            Command::QueryPositions => feedback(Event::Positions(Vec::new())),
            Command::QueryBalance => feedback(Event::Balance(BalanceFrame {
                current_balance: 100.0,
                buying_power: 100.0,
                open_orders: 0.0,
            })),
            // Paper positions are always flat, so this isn't asked for in practice.
            Command::QuerySettlement { slug } => feedback(Event::Settlement { slug, px: None }),
            Command::Withdraw { .. } | Command::RefreshMarket { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compare_local_dispatch_of_thirty_actions_with_and_without_throttle() {
        for enabled in [true, false] {
            let rest = RestClient::new(
                None,
                pm_venue::RateLimiter::with_enabled(15.0, enabled).unwrap(),
            )
            .unwrap();
            let now = Instant::now();
            let mut queue = Queue::new();
            for n in 0..30 {
                let command = Command::Cancel {
                    token: Token(n),
                    order_id: format!("o{n}").into(),
                    slug: "m".into(),
                    priority: Priority::Cancel,
                };
                queue.insert((Priority::Cancel, n), (command, now, Some(now)));
            }
            let mut batches = Vec::new();
            // Exercise real budget/batch dispatch selection, without sending HTTP requests.
            while !queue.is_empty() {
                rest.limiter.acquire().await;
                let batch = take_batch(&mut queue, &rest).await;
                assert!(batch.len() <= BATCH_LIMIT);
                batches.push(batch.len());
            }
            assert_eq!(batches.iter().sum::<usize>(), 30);
            if !enabled {
                assert_eq!(batches, vec![20, 10]);
            }
        }
    }

    #[tokio::test]
    async fn batching_preserves_each_commands_input_timestamp() {
        let rest = RestClient::new(None, pm_venue::RateLimiter::new(15.0).unwrap()).unwrap();
        let old = Instant::now() - Duration::from_millis(50);
        let recent = Instant::now();
        let cancel = |n| Command::Cancel {
            token: Token(n),
            order_id: format!("o{n}").into(),
            slug: "m".into(),
            priority: Priority::Cancel,
        };
        let mut queue = Queue::new();
        queue.insert((Priority::Cancel, 1), (cancel(1), recent, Some(old)));
        queue.insert((Priority::Cancel, 2), (cancel(2), recent, Some(recent)));
        queue.insert((Priority::Read, 3), (Command::QueryBalance, recent, None));
        let batch = take_batch(&mut queue, &rest).await;
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].2, Some(old));
        assert_eq!(batch[1].2, Some(recent));
        assert_eq!(queue.len(), 1);
    }

    #[tokio::test]
    async fn a_halt_withdraws_queued_and_unsent_creates_and_waits_for_sent_ones() {
        let gate = Arc::new(Gate::default());
        // A request counted before the halt holds `idle` until it finishes.
        let flight = gate.enter().expect("not halted");
        gate.shut();
        assert!(gate.enter().is_none(), "nothing new once halted");
        assert!(!gate.idle(Duration::from_millis(30)).await);
        drop(flight);
        assert!(gate.idle(Duration::from_millis(30)).await);

        // Taken off the queue but not yet sent: withdrawn, never sent.
        let inbox = Arc::new(Inbox::new());
        let rest =
            Arc::new(RestClient::new(None, pm_venue::RateLimiter::new(15.0).unwrap()).unwrap());
        let place = Command::Place {
            token: Token(7),
            order: NewOrder {
                slug: "m".into(),
                intent: pm_types::Intent::BuyLong,
                px: pm_types::Px(100),
                qty: pm_types::Qty(100),
                tif: pm_types::Tif::Gtd(0),
                post_only: true,
            },
            priority: Priority::Quote,
        };
        let now = Instant::now();
        execute(
            rest,
            inbox.clone(),
            Arc::new(Metrics::default()),
            gate.clone(),
            vec![Outbound {
                command: place,
                received: None,
                sent: now,
            }],
            now,
        )
        .await;
        assert_eq!(
            inbox.drain(),
            vec![Event::RestResult(RestResult {
                kind: RestKind::Create,
                tokens: vec![Token(7)],
                outcome: RestOutcome::Withdrawn
            })]
        );

        // The watchdog's halt lifts again; shutdown's doesn't.
        let gate = Gate::default();
        gate.set_emergency(true);
        assert!(gate.halted());
        gate.set_emergency(false);
        assert!(!gate.halted());
    }
}
