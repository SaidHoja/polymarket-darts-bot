//! Order manager: targets → actions, the local order state machine, ack
//! correlation without a client order id, fills, rejects and reconciliation.
//!
//! The private socket is the only source of truth. REST results tell us an id
//! (creates) or that a request was refused or lost; nothing is assumed live or
//! cancelled until an execution says so.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use pm_types::{
    Command, ExecKind, ExecutionFrame, Intent, NewOrder, OrderId, OrderState, Priority, Px, Qty,
    RejectClass, RestKind, RestOutcome, RestResult, Side, Slug, Tif, Token, VenueOrder,
};

use crate::config::{secs, ExecConfig};

const EXEC_ID_MEMORY: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QuoteSide {
    Bid = 0,
    Ask = 1,
}

impl QuoteSide {
    pub fn side(self) -> Side {
        match self {
            QuoteSide::Bid => Side::Buy,
            QuoteSide::Ask => Side::Sell,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneReason {
    Filled,
    Canceled,
    Rejected,
    Expired,
    Withdrawn,
    /// Not on the venue when we looked (cancelled or filled while we weren't watching).
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalState {
    /// Sent; no ack yet. `venue_id` is set once the REST result arrives.
    PendingNew,
    Live,
    PendingReplace,
    PendingCancel,
    /// Outcome unknown (lost REST result, socket drop). Counts as fully live for risk.
    InDoubt,
    Done(DoneReason),
}

impl LocalState {
    pub fn is_done(self) -> bool {
        matches!(self, LocalState::Done(_))
    }

    /// An action is outstanding, so the slot can't take a new one.
    pub fn in_flight(self) -> bool {
        matches!(
            self,
            LocalState::PendingNew
                | LocalState::PendingReplace
                | LocalState::PendingCancel
                | LocalState::InDoubt
        )
    }
}

#[derive(Debug, Clone)]
pub struct OrderRec {
    pub token: Token,
    pub venue_id: Option<OrderId>,
    pub slug: Slug,
    pub quote_side: Option<QuoteSide>,
    pub side: Side,
    pub intent: Intent,
    pub px: Px,
    pub qty: Qty,
    pub cum_qty: Qty,
    pub leaves_qty: Qty,
    pub tif: Tif,
    pub post_only: bool,
    pub state: LocalState,
    pub sent_mono: u64,
    pub acked_mono: Option<u64>,
    /// When the current outstanding action (cancel, modify, query) started.
    pub action_mono: u64,
    pub rest_done: bool,
    pub cancel_wanted: bool,
    /// When the last order or open-orders query for this record went out, and how many have gone
    /// unanswered. A read that fails or never returns is retried with backoff.
    pub query_at: Option<u64>,
    pub queries: u32,
    /// Cancels the gateway refused (4xx, 429, 401). Resent with backoff, then looked up instead.
    pub cancel_refusals: u32,
    /// Modify in flight: the values we asked for.
    pub modify_to: Option<(Px, Qty, Tif)>,
}

/// Longest wait before re-sending an unanswered order or open-orders query.
const QUERY_RETRY_MAX_NS: u64 = 60_000_000_000;
/// Refused cancels before the order is looked up instead of cancelled again.
const CANCEL_REFUSALS_BEFORE_LOOKUP: u32 = 3;

impl OrderRec {
    /// Resting at `px` for `leaves_qty` as far as we know, so the venue's book includes it.
    pub fn on_book(&self) -> bool {
        matches!(
            self.state,
            LocalState::Live | LocalState::PendingCancel | LocalState::PendingReplace
        )
    }

    /// No query is out, or the last went unanswered for 4× `timeout_ns`, doubling per retry up to a minute.
    fn query_due(&self, now: u64, timeout_ns: u64) -> bool {
        let wait = timeout_ns
            .saturating_mul(4 << self.queries.min(4))
            .min(QUERY_RETRY_MAX_NS);
        self.query_at
            .is_none_or(|at| now.saturating_sub(at) >= wait)
    }

    fn mark_queried(&mut self, now: u64) {
        if self.query_at.is_some() {
            self.queries += 1;
        }
        self.query_at = Some(now);
    }

    fn query_answered(&mut self) {
        self.query_at = None;
        self.queries = 0;
    }
}

/// What the order manager tells the rest of the core.
#[derive(Debug, Clone, PartialEq)]
pub enum OmEvent {
    /// Private execution acknowledgement only; never snapshot/REST reconciliation.
    ExecutionAck {
        token: Token,
        kind: RestKind,
    },
    Acked {
        slug: Slug,
        token: Token,
        latency_ns: u64,
    },
    Fill(FillRec),
    Done {
        slug: Slug,
        token: Token,
        reason: DoneReason,
    },
    Replaced {
        slug: Slug,
        token: Token,
    },
    Rejected {
        slug: Slug,
        token: Token,
        class: RejectClass,
        reason: String,
        text: String,
    },
    UnsolicitedCancel {
        slug: Slug,
        token: Token,
        reason: String,
    },
    InDoubt {
        slug: Slug,
        token: Token,
    },
    UnknownOrder {
        slug: Slug,
        order_id: OrderId,
    },
    CancelAllFailed {
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FillRec {
    pub slug: Slug,
    pub token: Option<Token>,
    pub order_id: OrderId,
    pub side: Side,
    pub qty: Qty,
    pub px: Px,
    pub fee: Option<f64>,
    pub aggressor: bool,
    pub trade_id: String,
}

/// A desired quote for one side of one market.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuoteTarget {
    pub px: Px,
    pub qty: Qty,
    pub intent: Intent,
    pub tif: Tif,
}

#[derive(Debug, Default)]
pub struct OrderManager {
    pub records: BTreeMap<Token, OrderRec>,
    /// The records that aren't done, so the per-tick scans skip the finished records kept until
    /// `prune`. Added in `new_record` and wherever a done record is set working again, removed in `finish`.
    active: ActiveIndex,
    /// Finished tokens in the order they finished, for `prune`.
    done: VecDeque<Token>,
    by_id: HashMap<OrderId, Token>,
    slots: BTreeMap<(Slug, QuoteSide), Token>,
    /// Executions for ids we don't know yet (ack before the REST result), with first-seen time.
    unmatched: BTreeMap<OrderId, (u64, Vec<ExecutionFrame>)>,
    seen_exec: VecDeque<String>,
    seen_exec_set: HashSet<String>,
    seen_fills: HashSet<(OrderId, String)>,
    next_token: u64,
    /// Open-order queries in flight, by slug ("" = all).
    open_queries: BTreeMap<Slug, u64>,
    /// Changes to our resting orders that the market's last book may not show yet, oldest first.
    /// See `unshown`.
    unshown: BTreeMap<Slug, VecDeque<Unshown>>,
    /// Commands produced while handling feedback (cancel-on-ack); the core drains it every tick.
    pub outbox: Vec<Command>,
    pub stats: OmStats,
}

/// One change to what we rest at a price, from an execution: `qty` was added (negative: removed)
/// at exchange time `transact_ns`. A book whose `transactTime` is at or after it includes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Unshown {
    pub transact_ns: i64,
    pub side: Side,
    pub px: Px,
    pub qty: Qty,
}

/// Per market. Books normally catch up within ~100 ms, so this only fills up if books stop.
const UNSHOWN_MAX: usize = 64;

/// Tokens of the active records, all together and per market, each in token order.
#[derive(Debug, Default)]
struct ActiveIndex {
    all: BTreeSet<Token>,
    by_slug: BTreeMap<Slug, BTreeSet<Token>>,
}

impl ActiveIndex {
    fn insert(&mut self, slug: &Slug, token: Token) {
        if self.all.insert(token) {
            self.by_slug.entry(slug.clone()).or_default().insert(token);
        }
    }

    fn remove(&mut self, slug: &str, token: Token) {
        if self.all.remove(&token) {
            if let Some(tokens) = self.by_slug.get_mut(slug) {
                tokens.remove(&token);
                if tokens.is_empty() {
                    self.by_slug.remove(slug);
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct OmStats {
    pub placed: u64,
    pub modified: u64,
    pub cancelled: u64,
    pub acks: u64,
    pub fills: u64,
    pub rejects: u64,
    pub duplicate_execs: u64,
    pub unknown_orders: u64,
}

impl OrderManager {
    pub fn token(&mut self) -> Token {
        self.next_token += 1;
        Token(self.next_token)
    }

    pub fn slot(&self, slug: &str, side: QuoteSide) -> Option<&OrderRec> {
        let token = self
            .slots
            .iter()
            .find(|((s, q), _)| &**s == slug && *q == side)
            .map(|(_, t)| *t)?;
        self.records.get(&token)
    }

    /// Non-terminal records (working, in flight or in doubt), in token order.
    pub fn active(&self) -> impl Iterator<Item = &OrderRec> {
        self.active.all.iter().map(|t| &self.records[t])
    }

    pub fn active_in(&self, slug: &str) -> impl Iterator<Item = &OrderRec> {
        self.active
            .by_slug
            .get(slug)
            .into_iter()
            .flatten()
            .map(|t| &self.records[t])
    }

    pub fn has_in_doubt(&self, slug: &str) -> bool {
        self.active_in(slug).any(|r| r.state == LocalState::InDoubt)
    }

    /// Our changes in `slug` newer than a book at `book_ns`, which that book doesn't show yet. The
    /// private socket runs ahead of market data (median ~46 ms in the 10-29 UTC captures), so the book
    /// behind an ack still has the order where it was.
    pub fn unshown(&self, slug: &str, book_ns: i64) -> impl Iterator<Item = &Unshown> {
        self.unshown
            .get(slug)
            .into_iter()
            .flatten()
            .filter(move |c| c.transact_ns > book_ns)
    }

    /// A book at `book_ns` arrived: it shows every change up to then.
    pub fn book_seen(&mut self, slug: &str, book_ns: i64) {
        if let Some(changes) = self.unshown.get_mut(slug) {
            changes.retain(|c| c.transact_ns > book_ns);
            if changes.is_empty() {
                self.unshown.remove(slug);
            }
        }
    }

    pub fn forget_unshown(&mut self, slug: &str) {
        self.unshown.remove(slug);
    }

    fn note_unshown(&mut self, slug: &Slug, change: Unshown) {
        let changes = self.unshown.entry(slug.clone()).or_default();
        if changes.len() == UNSHOWN_MAX {
            changes.pop_front();
        }
        changes.push_back(change);
    }

    fn finish(&mut self, token: Token, reason: DoneReason, out: &mut Vec<OmEvent>) {
        let Some(rec) = self.records.get_mut(&token) else {
            return;
        };
        if rec.state.is_done() {
            return;
        }
        rec.state = LocalState::Done(reason);
        self.active.remove(&rec.slug, token);
        self.done.push_back(token);
        if let Some(q) = rec.quote_side {
            let key = (rec.slug.clone(), q);
            if self.slots.get(&key) == Some(&token) {
                self.slots.remove(&key);
            }
        }
        out.push(OmEvent::Done {
            slug: rec.slug.clone(),
            token,
            reason,
        });
    }

    /// Drop terminal records older than `keep_ns` so the map doesn't grow forever. Walks the finished
    /// tokens oldest first and stops at the first one still too young, so a record can outlive
    /// `keep_ns` by as long as the one ahead of it was kept.
    pub fn prune(&mut self, now: u64, keep_ns: u64) {
        while let Some(&token) = self.done.front() {
            match self.records.get(&token) {
                Some(r) if r.state.is_done() => {
                    if now.saturating_sub(r.action_mono.max(r.sent_mono)) <= keep_ns {
                        break;
                    }
                    let rec = self.records.remove(&token).expect("checked above");
                    if let Some(id) = rec.venue_id {
                        if self.by_id.get(&id) == Some(&token) {
                            self.by_id.remove(&id);
                        }
                    }
                }
                // Already pruned, or working again (it is queued again if it finishes).
                _ => {}
            }
            self.done.pop_front();
        }
    }

    // ------------------------------------------------------------------ sending

    fn new_record(
        &mut self,
        slug: &Slug,
        quote_side: Option<QuoteSide>,
        target: &QuoteTarget,
        post_only: bool,
        now: u64,
    ) -> Token {
        let token = self.token();
        self.records.insert(
            token,
            OrderRec {
                token,
                venue_id: None,
                slug: slug.clone(),
                quote_side,
                side: target.intent.side(),
                intent: target.intent,
                px: target.px,
                qty: target.qty,
                cum_qty: Qty::ZERO,
                leaves_qty: target.qty,
                tif: target.tif,
                post_only,
                state: LocalState::PendingNew,
                sent_mono: now,
                acked_mono: None,
                action_mono: now,
                rest_done: false,
                cancel_wanted: false,
                query_at: None,
                queries: 0,
                cancel_refusals: 0,
                modify_to: None,
            },
        );
        self.active.insert(slug, token);
        if let Some(q) = quote_side {
            self.slots.insert((slug.clone(), q), token);
        }
        self.stats.placed += 1;
        token
    }

    pub fn place(
        &mut self,
        slug: &Slug,
        quote_side: Option<QuoteSide>,
        target: QuoteTarget,
        post_only: bool,
        priority: Priority,
        now: u64,
    ) -> Command {
        let token = self.new_record(slug, quote_side, &target, post_only, now);
        Command::Place {
            token,
            order: NewOrder {
                slug: slug.clone(),
                intent: target.intent,
                px: target.px,
                qty: target.qty,
                tif: target.tif,
                post_only,
            },
            priority,
        }
    }

    fn cancel_rec(&mut self, token: Token, priority: Priority, now: u64) -> Option<Command> {
        let rec = self.records.get_mut(&token)?;
        match (rec.state, &rec.venue_id) {
            (LocalState::Live, Some(id)) => {
                rec.state = LocalState::PendingCancel;
                rec.action_mono = now;
                rec.cancel_wanted = true;
                self.stats.cancelled += 1;
                Some(Command::Cancel {
                    token,
                    order_id: id.clone(),
                    slug: rec.slug.clone(),
                    priority,
                })
            }
            (LocalState::PendingNew, None) => {
                // Not sent yet or no id yet: withdraw from the gateway queue, cancel on ack otherwise.
                rec.cancel_wanted = true;
                Some(Command::Withdraw { token })
            }
            (LocalState::PendingNew, Some(id)) | (LocalState::InDoubt, Some(id)) => {
                // Cancels may be sent before the ack arrives; unknown ids are ignored by the venue.
                rec.cancel_wanted = true;
                rec.action_mono = now;
                Some(Command::Cancel {
                    token,
                    order_id: id.clone(),
                    slug: rec.slug.clone(),
                    priority,
                })
            }
            _ => {
                rec.cancel_wanted = true;
                None
            }
        }
    }

    pub fn cancel_slug(&mut self, slug: &str, priority: Priority, now: u64) -> Vec<Command> {
        let tokens: Vec<Token> = self.active_in(slug).map(|r| r.token).collect();
        tokens
            .into_iter()
            .filter_map(|t| self.cancel_rec(t, priority, now))
            .collect()
    }

    pub fn cancel_everything(&mut self, priority: Priority, now: u64) -> Vec<Command> {
        let tokens: Vec<Token> = self.active().map(|r| r.token).collect();
        tokens
            .into_iter()
            .filter_map(|t| self.cancel_rec(t, priority, now))
            .collect()
    }

    /// Diff one side of one market against its target. A live quote
    /// within `min_move` of the target price stays where it is (`Px(1)`: only an exact match stays).
    pub fn sync_side(
        &mut self,
        slug: &Slug,
        side: QuoteSide,
        target: Option<QuoteTarget>,
        min_move: Px,
        priority: Priority,
        now: u64,
    ) -> Vec<Command> {
        let current = self.slots.get(&(slug.clone(), side)).copied();
        let Some(token) = current else {
            return match target {
                Some(t) => vec![self.place(slug, Some(side), t, true, priority, now)],
                None => Vec::new(),
            };
        };
        let rec = self.records.get(&token).expect("slot points at a record");
        let Some(t) = target else {
            return if rec.cancel_wanted && rec.state != LocalState::Live {
                Vec::new()
            } else {
                self.cancel_rec(token, Priority::Cancel, now)
                    .into_iter()
                    .collect()
            };
        };
        if rec.state != LocalState::Live {
            return Vec::new(); // in flight: the market is re-marked dirty when it resolves
        }
        // A changed intent is replaced by cancel + new, even at the same price and size.
        if rec.intent != t.intent {
            return self
                .cancel_rec(token, Priority::Cancel, now)
                .into_iter()
                .collect();
        }
        // Hysteresis: keep a quote less than `min_move` from the target price, down to half the target
        // size but never above it, since the target is the most the risk gate approved.
        let px_close = (rec.px.0 - t.px.0).abs() < min_move.0.max(1);
        let qty_close = rec.leaves_qty.0 * 2 >= t.qty.0 && rec.leaves_qty <= t.qty;
        if px_close && qty_close {
            return Vec::new();
        }
        // A partly filled order is replaced by cancel + new rather than modify.
        if rec.cum_qty > Qty::ZERO {
            return self
                .cancel_rec(token, Priority::Cancel, now)
                .into_iter()
                .collect();
        }
        self.modify(token, t.px, t.qty, t.tif, priority, now)
            .into_iter()
            .collect()
    }

    fn modify(
        &mut self,
        token: Token,
        px: Px,
        qty: Qty,
        tif: Tif,
        priority: Priority,
        now: u64,
    ) -> Option<Command> {
        let rec = self.records.get_mut(&token)?;
        let id = rec.venue_id.clone()?;
        rec.state = LocalState::PendingReplace;
        rec.action_mono = now;
        rec.modify_to = Some((px, qty, tif));
        self.stats.modified += 1;
        Some(Command::Modify {
            token,
            order_id: id,
            slug: rec.slug.clone(),
            px,
            qty,
            tif,
            post_only: rec.post_only,
            priority,
        })
    }

    /// Re-send live quotes whose GTD expires soon with a new expiry.
    pub fn refresh_expiring(
        &mut self,
        wall_ns: i64,
        before_ns: i64,
        new_tif: impl Fn(&OrderRec) -> Option<Tif>,
        now: u64,
    ) -> Vec<Command> {
        let due: Vec<(Token, Tif)> = self
            .active()
            .filter(|r| {
                r.state == LocalState::Live
                    && r.quote_side.is_some()
                    && !r.cancel_wanted
                    && r.cum_qty.is_zero()
            })
            .filter(|r| matches!(r.tif, Tif::Gtd(t) if t - wall_ns < before_ns))
            .filter_map(|r| new_tif(r).map(|tif| (r.token, tif)))
            .collect();
        due.into_iter()
            .filter_map(|(token, tif)| {
                let (px, qty) = {
                    let r = &self.records[&token];
                    (r.px, r.leaves_qty)
                };
                self.modify(token, px, qty, tif, Priority::Refresh, now)
            })
            .collect()
    }

    // ------------------------------------------------------------------ feedback

    pub fn on_rest_result(&mut self, result: &RestResult, now: u64) -> Vec<OmEvent> {
        let mut out = Vec::new();
        match (&result.kind, &result.outcome) {
            (RestKind::Create, RestOutcome::Accepted { ids }) => {
                for (i, token) in result.tokens.iter().enumerate() {
                    let id = ids.get(i).cloned().flatten();
                    let Some(rec) = self.records.get_mut(token) else {
                        continue;
                    };
                    rec.rest_done = true;
                    match id {
                        Some(id) => {
                            rec.venue_id = Some(id.clone());
                            self.by_id.insert(id.clone(), *token);
                            if let Some((_, parked)) = self.unmatched.remove(&id) {
                                for exec in parked {
                                    self.apply(*token, exec, now, &mut out);
                                }
                            }
                        }
                        None => {
                            rec.state = LocalState::InDoubt;
                            rec.action_mono = now;
                            self.active.insert(&rec.slug, *token);
                            out.push(OmEvent::InDoubt {
                                slug: rec.slug.clone(),
                                token: *token,
                            });
                        }
                    }
                }
            }
            (RestKind::Create, outcome) => {
                for token in &result.tokens {
                    let Some(rec) = self.records.get_mut(token) else {
                        continue;
                    };
                    rec.rest_done = true;
                    let slug = rec.slug.clone();
                    match outcome {
                        RestOutcome::Ambiguous { .. } => {
                            rec.state = LocalState::InDoubt;
                            rec.action_mono = now;
                            self.active.insert(&rec.slug, *token);
                            out.push(OmEvent::InDoubt {
                                slug,
                                token: *token,
                            });
                        }
                        RestOutcome::Withdrawn => {
                            self.finish(*token, DoneReason::Withdrawn, &mut out)
                        }
                        _ => {
                            let (class, text) = rest_failure(outcome);
                            self.stats.rejects += 1;
                            out.push(OmEvent::Rejected {
                                slug,
                                token: *token,
                                class,
                                reason: "REST".into(),
                                text,
                            });
                            self.finish(*token, DoneReason::Rejected, &mut out);
                        }
                    }
                }
            }
            (RestKind::Modify | RestKind::Cancel, outcome) => {
                for token in &result.tokens {
                    let Some(rec) = self.records.get_mut(token) else {
                        continue;
                    };
                    match outcome {
                        // Echoes only: wait for REPLACE / CANCELED. Ambiguous: the timeout looks the order up.
                        RestOutcome::Accepted { .. } | RestOutcome::Ambiguous { .. } => {}
                        _ => {
                            // The action never reached the exchange: the order is as it was.
                            if matches!(
                                rec.state,
                                LocalState::PendingReplace | LocalState::PendingCancel
                            ) {
                                rec.state = LocalState::Live;
                            }
                            rec.modify_to = None;
                            if *outcome != RestOutcome::Withdrawn {
                                if result.kind == RestKind::Cancel {
                                    rec.cancel_refusals += 1;
                                    rec.action_mono = now;
                                }
                                let (class, text) = rest_failure(outcome);
                                out.push(OmEvent::Rejected {
                                    slug: rec.slug.clone(),
                                    token: *token,
                                    class,
                                    reason: "REST".into(),
                                    text,
                                });
                            }
                        }
                    }
                }
            }
            (RestKind::CancelAll, RestOutcome::Accepted { .. }) => {}
            (RestKind::CancelAll, outcome) => out.push(OmEvent::CancelAllFailed {
                detail: format!("{outcome:?}"),
            }),
        }
        out
    }

    pub fn on_execution(&mut self, exec: ExecutionFrame, now: u64) -> Vec<OmEvent> {
        let mut out = Vec::new();
        if !exec.exec_id.is_empty() {
            if self.seen_exec_set.contains(&exec.exec_id) {
                self.stats.duplicate_execs += 1;
                return out;
            }
            self.seen_exec_set.insert(exec.exec_id.clone());
            self.seen_exec.push_back(exec.exec_id.clone());
            if self.seen_exec.len() > EXEC_ID_MEMORY {
                if let Some(old) = self.seen_exec.pop_front() {
                    self.seen_exec_set.remove(&old);
                }
            }
        }
        let token = self
            .by_id
            .get(&exec.order.id)
            .copied()
            .or_else(|| self.adopt_replacement(&exec));
        match token {
            Some(token) => self.apply(token, exec, now, &mut out),
            None => {
                let entry = self
                    .unmatched
                    .entry(exec.order.id.clone())
                    .or_insert((now, Vec::new()));
                entry.1.push(exec);
            }
        }
        out
    }

    /// A REPLACE may carry a new order id: adopt it if exactly one modify is pending on that side.
    fn adopt_replacement(&mut self, exec: &ExecutionFrame) -> Option<Token> {
        if exec.kind != ExecKind::Replace {
            return None;
        }
        let first = {
            let mut candidates = self
                .active()
                .filter(|r| {
                    r.state == LocalState::PendingReplace
                        && r.slug == exec.order.slug
                        && r.side == exec.order.side
                })
                .map(|r| r.token);
            match (candidates.next(), candidates.next()) {
                (Some(t), None) => t,
                _ => return None,
            }
        };
        self.by_id.insert(exec.order.id.clone(), first);
        Some(first)
    }

    fn apply(&mut self, token: Token, exec: ExecutionFrame, now: u64, out: &mut Vec<OmEvent>) {
        let Some(rec) = self.records.get_mut(&token) else {
            return;
        };
        let slug = rec.slug.clone();
        let before = rec.on_book().then_some((rec.px, rec.leaves_qty));
        let transact_ns = exec.transact_ns;
        // Fills leave through the public trades instead (the provisional overlay).
        let moves_book = matches!(
            exec.kind,
            ExecKind::New
                | ExecKind::Replace
                | ExecKind::Canceled
                | ExecKind::Expired
                | ExecKind::DoneForDay
        );
        if rec.venue_id.as_ref() != Some(&exec.order.id) {
            rec.venue_id = Some(exec.order.id.clone());
            self.by_id.insert(exec.order.id.clone(), token);
        }
        // The venue's quantities win over our arithmetic.
        rec.cum_qty = exec.order.cum_qty;
        rec.leaves_qty = exec.order.leaves_qty;
        match exec.kind {
            ExecKind::New => {
                if matches!(rec.state, LocalState::PendingNew | LocalState::InDoubt) {
                    rec.state = LocalState::Live;
                    rec.acked_mono = Some(now);
                    self.stats.acks += 1;
                    out.push(OmEvent::Acked {
                        slug: slug.clone(),
                        token,
                        latency_ns: now.saturating_sub(rec.sent_mono),
                    });
                    out.push(OmEvent::ExecutionAck {
                        token,
                        kind: RestKind::Create,
                    });
                }
                if rec.cancel_wanted && rec.state == LocalState::Live {
                    // Cancel was wanted before the ack: send it now.
                    let cmd = self.cancel_rec(token, Priority::Cancel, now);
                    self.outbox.extend(cmd);
                }
            }
            ExecKind::PartialFill | ExecKind::Fill => {
                if matches!(rec.state, LocalState::PendingNew | LocalState::InDoubt) {
                    rec.state = LocalState::Live;
                    rec.acked_mono.get_or_insert(now);
                }
                if let Some(fill) = &exec.fill {
                    let key = (
                        exec.order.id.clone(),
                        if fill.trade_id.is_empty() {
                            exec.exec_id.clone()
                        } else {
                            fill.trade_id.clone()
                        },
                    );
                    if self.seen_fills.insert(key) {
                        self.stats.fills += 1;
                        out.push(OmEvent::Fill(FillRec {
                            slug: slug.clone(),
                            token: Some(token),
                            order_id: exec.order.id.clone(),
                            side: exec.order.side,
                            qty: fill.last_shares,
                            px: fill.last_px,
                            fee: fill.commission,
                            aggressor: fill.aggressor,
                            trade_id: fill.trade_id.clone(),
                        }));
                    }
                }
                if exec.kind == ExecKind::Fill
                    || exec.order.leaves_qty <= Qty::ZERO
                    || exec.order.state == OrderState::Filled
                {
                    self.finish(token, DoneReason::Filled, out);
                }
            }
            ExecKind::Replace => {
                if rec.state == LocalState::PendingReplace {
                    out.push(OmEvent::ExecutionAck {
                        token,
                        kind: RestKind::Modify,
                    });
                }
                rec.px = exec.order.px;
                rec.qty = exec.order.qty;
                if let Some((_, _, tif)) = rec.modify_to.take() {
                    rec.tif = tif;
                }
                rec.state = LocalState::Live;
                self.active.insert(&rec.slug, token);
                out.push(OmEvent::Replaced { slug, token });
            }
            ExecKind::Canceled => {
                if let Some(reason) = &exec.unsolicited {
                    out.push(OmEvent::UnsolicitedCancel {
                        slug,
                        token,
                        reason: reason.clone(),
                    });
                }
                self.finish(token, DoneReason::Canceled, out);
            }
            ExecKind::Rejected => {
                let reject = exec.reject.clone().unwrap_or(pm_types::Reject {
                    class: RejectClass::Unknown,
                    reason: String::new(),
                    text: String::new(),
                });
                self.stats.rejects += 1;
                out.push(OmEvent::Rejected {
                    slug,
                    token,
                    class: reject.class,
                    reason: reject.reason,
                    text: reject.text,
                });
                if exec.order.state.is_terminal() || rec.state == LocalState::PendingNew {
                    self.finish(token, DoneReason::Rejected, out);
                } else {
                    // A rejected modify or cancel leaves the order as it was.
                    rec.state = LocalState::Live;
                    rec.modify_to = None;
                    self.active.insert(&rec.slug, token);
                }
            }
            ExecKind::Expired | ExecKind::DoneForDay => {
                self.finish(token, DoneReason::Expired, out)
            }
            ExecKind::Unknown => {}
        }
        let (Some(transact_ns), true) = (transact_ns, moves_book) else {
            return;
        };
        let rec = &self.records[&token];
        let after = rec.on_book().then_some((rec.px, rec.leaves_qty));
        if before == after {
            return;
        }
        let (side, slug) = (rec.side, rec.slug.clone());
        for (px, qty) in before.map(|(px, q)| (px, -q)).into_iter().chain(after) {
            self.note_unshown(
                &slug,
                Unshown {
                    transact_ns,
                    side,
                    px,
                    qty,
                },
            );
        }
    }

    /// Private socket dropped: no replay, so every working order is in doubt until the next snapshot.
    pub fn on_private_down(&mut self, now: u64) {
        for token in &self.active.all {
            let rec = self
                .records
                .get_mut(token)
                .expect("active tokens have records");
            rec.state = LocalState::InDoubt;
            rec.action_mono = now;
            rec.query_answered();
        }
    }

    /// Reconcile against the venue's open orders: the snapshot (all markets) or a REST read for some slugs.
    pub fn reconcile(
        &mut self,
        slugs: Option<&[Slug]>,
        orders: &[VenueOrder],
        now: u64,
    ) -> (Vec<OmEvent>, Vec<Command>) {
        let mut out = Vec::new();
        let mut commands = Vec::new();
        let in_scope =
            |slug: &str| slugs.is_none_or(|s| s.is_empty() || s.iter().any(|x| &**x == slug));
        let venue: HashMap<&OrderId, &VenueOrder> = orders.iter().map(|o| (&o.id, o)).collect();
        let tokens: Vec<Token> = self
            .active()
            .filter(|r| in_scope(&r.slug))
            .map(|r| r.token)
            .collect();
        let mut claimed: HashSet<OrderId> = HashSet::new();
        for token in tokens {
            let rec = &self.records[&token];
            match &rec.venue_id {
                Some(id) => {
                    claimed.insert(id.clone());
                    if let Some(o) = venue.get(id) {
                        let rec = self.records.get_mut(&token).unwrap();
                        rec.cum_qty = o.cum_qty;
                        rec.leaves_qty = o.leaves_qty;
                        rec.px = o.px;
                        rec.query_answered();
                        if matches!(rec.state, LocalState::InDoubt | LocalState::PendingNew) {
                            // The venue's order is the truth now, whichever way an in-doubt modify went.
                            rec.state = LocalState::Live;
                            rec.modify_to = None;
                            rec.acked_mono.get_or_insert(now);
                            out.push(OmEvent::Acked {
                                slug: rec.slug.clone(),
                                token,
                                latency_ns: now.saturating_sub(rec.sent_mono),
                            });
                        }
                        if rec.cancel_wanted {
                            commands.extend(self.cancel_rec(token, Priority::Cancel, now));
                        }
                    } else if rec.state != LocalState::PendingNew || rec.rest_done {
                        self.finish(token, DoneReason::Gone, &mut out);
                    }
                }
                None if rec.rest_done || rec.state == LocalState::InDoubt => {
                    // A lost create: claim a venue order with the same fields, if exactly one matches.
                    let mut matches = orders.iter().filter(|o| {
                        o.slug == rec.slug
                            && o.side == rec.side
                            && o.px == rec.px
                            && o.qty == rec.qty
                            && !self.by_id.contains_key(&o.id)
                            && !claimed.contains(&o.id)
                    });
                    match (matches.next(), matches.next()) {
                        (Some(o), None) => {
                            let id = o.id.clone();
                            claimed.insert(id.clone());
                            self.by_id.insert(id.clone(), token);
                            let rec = self.records.get_mut(&token).unwrap();
                            rec.venue_id = Some(id);
                            rec.state = LocalState::Live;
                            if rec.cancel_wanted {
                                commands.extend(self.cancel_rec(token, Priority::Cancel, now));
                            }
                        }
                        _ => self.finish(token, DoneReason::Gone, &mut out),
                    }
                }
                None => {} // REST result still pending
            }
        }
        // Venue orders we don't know: someone else is using the key, or we lost track. Cancel them.
        for o in orders.iter().filter(|o| in_scope(&o.slug)) {
            if !self.by_id.contains_key(&o.id) && !claimed.contains(&o.id) {
                self.stats.unknown_orders += 1;
                out.push(OmEvent::UnknownOrder {
                    slug: o.slug.clone(),
                    order_id: o.id.clone(),
                });
                let token = self.token();
                commands.push(Command::Cancel {
                    token,
                    order_id: o.id.clone(),
                    slug: o.slug.clone(),
                    priority: Priority::Cancel,
                });
            }
        }
        if let Some(slugs) = slugs {
            for s in slugs {
                self.open_queries.remove(s);
            }
            if slugs.is_empty() {
                self.open_queries.clear();
            }
        } else {
            self.open_queries.clear();
        }
        (out, commands)
    }

    /// Result of `GET /v1/order/{id}`.
    pub fn on_order_lookup(
        &mut self,
        id: &OrderId,
        order: Option<&VenueOrder>,
        now: u64,
    ) -> (Vec<OmEvent>, Vec<Command>) {
        let mut out = Vec::new();
        let mut commands = Vec::new();
        let Some(&token) = self.by_id.get(id) else {
            return (out, commands);
        };
        match order {
            None => self.finish(token, DoneReason::Gone, &mut out),
            Some(o) if o.state.is_terminal() => {
                let reason = match o.state {
                    OrderState::Filled => DoneReason::Filled,
                    OrderState::Rejected => DoneReason::Rejected,
                    OrderState::Expired => DoneReason::Expired,
                    _ => DoneReason::Canceled,
                };
                self.finish(token, reason, &mut out);
            }
            Some(o) => {
                let rec = self.records.get_mut(&token).unwrap();
                rec.px = o.px;
                rec.cum_qty = o.cum_qty;
                rec.leaves_qty = o.leaves_qty;
                rec.state = LocalState::Live;
                rec.modify_to = None;
                rec.query_answered();
                self.active.insert(&rec.slug, token);
                if rec.cancel_wanted {
                    commands.extend(self.cancel_rec(token, Priority::Cancel, now));
                }
            }
        }
        (out, commands)
    }

    /// Timeouts: ack, cancel/replace, in doubt, unmatched executions.
    pub fn check_timeouts(&mut self, cfg: &ExecConfig, now: u64) -> (Vec<OmEvent>, Vec<Command>) {
        let mut out = Vec::new();
        let mut commands = Vec::new();
        let ack_ns = secs(cfg.ack_timeout_s);
        let cancel_ns = secs(cfg.cancel_timeout_s);
        let mut query_slugs: Vec<Slug> = Vec::new();
        let tokens: Vec<Token> = self.active().map(|r| r.token).collect();
        for token in tokens {
            let rec = self.records.get_mut(&token).unwrap();
            let age = now.saturating_sub(rec.action_mono);
            match rec.state {
                LocalState::PendingNew
                    if rec.rest_done && rec.venue_id.is_some() && age > ack_ns =>
                {
                    rec.state = LocalState::InDoubt;
                    rec.action_mono = now;
                    out.push(OmEvent::InDoubt {
                        slug: rec.slug.clone(),
                        token,
                    });
                }
                LocalState::InDoubt if age > ack_ns && rec.query_due(now, ack_ns) => {
                    if !query_slugs.contains(&rec.slug) {
                        query_slugs.push(rec.slug.clone());
                    }
                }
                LocalState::PendingCancel | LocalState::PendingReplace
                    if age > cancel_ns && rec.query_due(now, cancel_ns) =>
                {
                    if let Some(id) = rec.venue_id.clone() {
                        rec.mark_queried(now);
                        commands.push(Command::QueryOrder { order_id: id });
                    }
                }
                LocalState::Live if rec.cancel_wanted => {
                    // A cancel the gateway refused: retry after the cancel timeout, doubling, and
                    // after a few refusals ask the venue instead (the order may already be gone).
                    if rec.cancel_refusals >= CANCEL_REFUSALS_BEFORE_LOOKUP {
                        if rec.query_due(now, cancel_ns) {
                            if let Some(id) = rec.venue_id.clone() {
                                rec.mark_queried(now);
                                commands.push(Command::QueryOrder { order_id: id });
                            }
                        }
                    } else if rec.cancel_refusals == 0
                        || age >= cancel_ns << (rec.cancel_refusals - 1)
                    {
                        commands.extend(self.cancel_rec(token, Priority::Cancel, now));
                    }
                }
                _ => {}
            }
        }
        // Queries that never came back can be re-sent.
        self.open_queries
            .retain(|_, t| now.saturating_sub(*t) < ack_ns * 4);
        for slug in query_slugs {
            if self.open_queries.contains_key(&slug) {
                continue;
            }
            self.open_queries.insert(slug.clone(), now);
            for token in &self.active.all {
                let rec = self
                    .records
                    .get_mut(token)
                    .expect("active tokens have records");
                if rec.state == LocalState::InDoubt && rec.slug == slug {
                    rec.mark_queried(now);
                }
            }
            commands.push(Command::QueryOpenOrders { slugs: vec![slug] });
        }

        // Executions for ids nobody claimed.
        let attach_ns = secs(cfg.unmatched_attach_s);
        let unknown_ns = secs(cfg.unknown_order_s);
        let ids: Vec<OrderId> = self.unmatched.keys().cloned().collect();
        for id in ids {
            let (first_seen, execs) = self.unmatched[&id].clone();
            let age = now.saturating_sub(first_seen);
            if age < attach_ns {
                continue;
            }
            let order = &execs[0].order;
            let claimant = {
                let mut candidates = self
                    .active()
                    .filter(|r| {
                        r.state == LocalState::PendingNew
                            && r.venue_id.is_none()
                            && r.slug == order.slug
                            && r.side == order.side
                            && r.px == order.px
                            && r.qty == order.qty
                    })
                    .map(|r| r.token);
                match (candidates.next(), candidates.next()) {
                    (Some(t), None) => Some(t),
                    _ => None,
                }
            };
            if let Some(token) = claimant {
                self.unmatched.remove(&id);
                self.by_id.insert(id.clone(), token);
                for exec in execs {
                    self.apply(token, exec, now, &mut out);
                }
                continue;
            }
            if age >= unknown_ns {
                self.unmatched.remove(&id);
                self.stats.unknown_orders += 1;
                // Fills on an order we can't place still change our position.
                for exec in &execs {
                    if let Some(fill) = &exec.fill {
                        if self.seen_fills.insert((id.clone(), fill.trade_id.clone())) {
                            out.push(OmEvent::Fill(FillRec {
                                slug: order.slug.clone(),
                                token: None,
                                order_id: id.clone(),
                                side: order.side,
                                qty: fill.last_shares,
                                px: fill.last_px,
                                fee: fill.commission,
                                aggressor: fill.aggressor,
                                trade_id: fill.trade_id.clone(),
                            }));
                        }
                    }
                }
                out.push(OmEvent::UnknownOrder {
                    slug: order.slug.clone(),
                    order_id: id.clone(),
                });
                let last_state = execs
                    .last()
                    .map(|e| e.order.state)
                    .unwrap_or(OrderState::Unknown);
                if !last_state.is_terminal() {
                    let token = self.token();
                    commands.push(Command::Cancel {
                        token,
                        order_id: id.clone(),
                        slug: order.slug.clone(),
                        priority: Priority::Cancel,
                    });
                }
            }
        }
        (out, commands)
    }
}

fn rest_failure(outcome: &RestOutcome) -> (RejectClass, String) {
    match outcome {
        RestOutcome::GatewayRejected { class, body, .. } => (*class, body.clone()),
        RestOutcome::RateLimited => (RejectClass::RateLimited, "429".into()),
        RestOutcome::AuthFailed { body } => (RejectClass::Fatal, body.clone()),
        RestOutcome::Ambiguous { error } => (RejectClass::Unknown, error.clone()),
        RestOutcome::Withdrawn => (RejectClass::Unknown, "withdrawn".into()),
        RestOutcome::Accepted { .. } => (RejectClass::Unknown, String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_types::{FillInfo, Reject};
    use std::sync::Arc;

    fn slug() -> Slug {
        Arc::from("m")
    }

    fn target(px: i64) -> QuoteTarget {
        QuoteTarget {
            px: Px(px),
            qty: Qty(10_000),
            intent: Intent::BuyLong,
            tif: Tif::Gtd(0),
        }
    }

    fn exec(
        id: &str,
        exec_id: &str,
        kind: ExecKind,
        state: OrderState,
        px: i64,
        cum: i64,
        fill: Option<(i64, &str)>,
    ) -> ExecutionFrame {
        ExecutionFrame {
            exec_id: exec_id.into(),
            order: VenueOrder {
                id: Arc::from(id),
                slug: slug(),
                side: Side::Buy,
                intent: Intent::BuyLong,
                px: Px(px),
                qty: Qty(10_000),
                cum_qty: Qty(cum),
                leaves_qty: Qty(10_000 - cum),
                state,
            },
            kind,
            fill: fill.map(|(q, t)| FillInfo {
                last_shares: Qty(q),
                last_px: Px(px),
                trade_id: t.into(),
                aggressor: false,
                commission: None,
            }),
            reject: (kind == ExecKind::Rejected).then(|| Reject {
                class: RejectClass::BookMoved,
                reason: "R".into(),
                text: "post only".into(),
            }),
            unsolicited: None,
            transact_ns: None,
        }
    }

    fn accepted(token: Token, id: &str) -> RestResult {
        RestResult {
            kind: RestKind::Create,
            tokens: vec![token],
            outcome: RestOutcome::Accepted {
                ids: vec![Some(Arc::from(id))],
            },
        }
    }

    /// The open-order index lists exactly the records that aren't done.
    fn check_index(om: &OrderManager) {
        let open: Vec<Token> = om
            .records
            .values()
            .filter(|r| !r.state.is_done())
            .map(|r| r.token)
            .collect();
        assert_eq!(om.active.all.iter().copied().collect::<Vec<_>>(), open);
        for (slug, tokens) in &om.active.by_slug {
            assert!(!tokens.is_empty());
            assert!(tokens
                .iter()
                .all(|t| om.records[t].slug == *slug && om.active.all.contains(t)));
        }
        assert_eq!(
            om.active.by_slug.values().map(BTreeSet::len).sum::<usize>(),
            open.len()
        );
    }

    fn placed_token(cmds: &[Command]) -> Token {
        match &cmds[0] {
            Command::Place { token, .. } => *token,
            other => panic!("expected place, got {other:?}"),
        }
    }

    #[test]
    fn ack_before_rest_result_is_parked_then_applied() {
        let mut om = OrderManager::default();
        let token = placed_token(&om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            Px(1),
            Priority::Quote,
            0,
        ));
        // Ack arrives on the socket before the REST response.
        assert!(om
            .on_execution(
                exec("O1", "E1", ExecKind::New, OrderState::New, 4000, 0, None),
                1
            )
            .is_empty());
        let events = om.on_rest_result(&accepted(token, "O1"), 2);
        assert!(matches!(events[0], OmEvent::Acked { .. }));
        assert!(events.contains(&OmEvent::ExecutionAck {
            token,
            kind: RestKind::Create
        }));
        assert_eq!(
            om.slot("m", QuoteSide::Bid).unwrap().state,
            LocalState::Live
        );
        // Same target: nothing to do. New price: modify.
        assert!(om
            .sync_side(
                &slug(),
                QuoteSide::Bid,
                Some(target(4000)),
                Px(1),
                Priority::Quote,
                3
            )
            .is_empty());
        let cmds = om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4100)),
            Px(1),
            Priority::Quote,
            3,
        );
        assert!(matches!(cmds[0], Command::Modify { px: Px(4100), .. }));
        // In flight: no further action until it resolves.
        assert!(om
            .sync_side(
                &slug(),
                QuoteSide::Bid,
                Some(target(4200)),
                Px(1),
                Priority::Quote,
                4
            )
            .is_empty());
        let events = om.on_execution(
            exec(
                "O1",
                "E2",
                ExecKind::Replace,
                OrderState::New,
                4100,
                0,
                None,
            ),
            5,
        );
        assert!(events.contains(&OmEvent::ExecutionAck {
            token,
            kind: RestKind::Modify
        }));
        assert!(om
            .on_execution(
                exec(
                    "O1",
                    "E2",
                    ExecKind::Replace,
                    OrderState::New,
                    4100,
                    0,
                    None
                ),
                6
            )
            .is_empty());
        assert_eq!(om.slot("m", QuoteSide::Bid).unwrap().px, Px(4100));
    }

    #[test]
    fn rest_acceptance_and_snapshot_are_not_private_execution_ack_samples() {
        let mut om = OrderManager::default();
        let token = placed_token(&om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            Px(1),
            Priority::Quote,
            0,
        ));
        let rest = om.on_rest_result(&accepted(token, "O1"), 1);
        assert!(!rest
            .iter()
            .any(|e| matches!(e, OmEvent::ExecutionAck { .. })));
        let venue = exec(
            "O1",
            "unused",
            ExecKind::New,
            OrderState::New,
            4000,
            0,
            None,
        )
        .order;
        let (events, _) = om.reconcile(None, &[venue], 2);
        assert!(!events
            .iter()
            .any(|e| matches!(e, OmEvent::ExecutionAck { .. })));
    }

    #[test]
    fn fills_are_deduplicated_and_free_the_slot() {
        let mut om = OrderManager::default();
        let token = placed_token(&om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            Px(1),
            Priority::Quote,
            0,
        ));
        om.on_rest_result(&accepted(token, "O1"), 1);
        om.on_execution(
            exec("O1", "E1", ExecKind::New, OrderState::New, 4000, 0, None),
            2,
        );
        let partial = om.on_execution(
            exec(
                "O1",
                "E2",
                ExecKind::PartialFill,
                OrderState::PartiallyFilled,
                4000,
                4_000,
                Some((4_000, "T1")),
            ),
            3,
        );
        assert!(matches!(&partial[0], OmEvent::Fill(f) if f.qty == Qty(4_000)));
        // Same execution again (e.g. replayed): ignored.
        assert!(om
            .on_execution(
                exec(
                    "O1",
                    "E2",
                    ExecKind::PartialFill,
                    OrderState::PartiallyFilled,
                    4000,
                    4_000,
                    Some((4_000, "T1"))
                ),
                4
            )
            .is_empty());
        let done = om.on_execution(
            exec(
                "O1",
                "E3",
                ExecKind::Fill,
                OrderState::Filled,
                4000,
                10_000,
                Some((6_000, "T2")),
            ),
            5,
        );
        assert!(done.iter().any(|e| matches!(
            e,
            OmEvent::Done {
                reason: DoneReason::Filled,
                ..
            }
        )));
        assert!(om.slot("m", QuoteSide::Bid).is_none());
        check_index(&om);
    }

    #[test]
    fn fill_during_cancel_race() {
        let mut om = OrderManager::default();
        let token = placed_token(&om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            Px(1),
            Priority::Quote,
            0,
        ));
        om.on_rest_result(&accepted(token, "O1"), 1);
        om.on_execution(
            exec("O1", "E1", ExecKind::New, OrderState::New, 4000, 0, None),
            2,
        );
        let cmds = om.sync_side(&slug(), QuoteSide::Bid, None, Px(1), Priority::Quote, 3);
        assert!(matches!(cmds[0], Command::Cancel { .. }));
        let events = om.on_execution(
            exec(
                "O1",
                "E2",
                ExecKind::Fill,
                OrderState::Filled,
                4000,
                10_000,
                Some((10_000, "T1")),
            ),
            4,
        );
        assert!(events.iter().any(|e| matches!(e, OmEvent::Fill(_))));
        assert!(om.slot("m", QuoteSide::Bid).is_none());
    }

    #[test]
    fn rejects_and_ambiguous_results() {
        let mut om = OrderManager::default();
        let token = placed_token(&om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            Px(1),
            Priority::Quote,
            0,
        ));
        om.on_rest_result(&accepted(token, "O1"), 1);
        let events = om.on_execution(
            exec(
                "O1",
                "E1",
                ExecKind::Rejected,
                OrderState::Rejected,
                4000,
                0,
                None,
            ),
            2,
        );
        assert!(matches!(
            events[0],
            OmEvent::Rejected {
                class: RejectClass::BookMoved,
                ..
            }
        ));
        assert!(om.slot("m", QuoteSide::Bid).is_none());

        let token = placed_token(&om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            Px(1),
            Priority::Quote,
            10,
        ));
        om.on_rest_result(
            &RestResult {
                kind: RestKind::Create,
                tokens: vec![token],
                outcome: RestOutcome::Ambiguous {
                    error: "timeout".into(),
                },
            },
            11,
        );
        assert_eq!(om.records[&token].state, LocalState::InDoubt);
        // After the ack timeout it asks for the open orders of that market.
        let cfg = ExecConfig::default();
        let (_, cmds) = om.check_timeouts(&cfg, 11 + secs(cfg.ack_timeout_s) + 1);
        assert!(matches!(&cmds[0], Command::QueryOpenOrders { .. }));
        // The venue has it: claimed by its fields.
        let venue = exec("O9", "x", ExecKind::New, OrderState::New, 4000, 0, None).order;
        om.reconcile(Some(&[slug()]), &[venue], 20);
        assert_eq!(om.records[&token].state, LocalState::Live);
        assert_eq!(om.records[&token].venue_id.as_deref(), Some("O9"));
    }

    #[test]
    fn snapshot_cancels_unknown_and_retires_missing() {
        let mut om = OrderManager::default();
        let token = placed_token(&om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            Px(1),
            Priority::Quote,
            0,
        ));
        om.on_rest_result(&accepted(token, "O1"), 1);
        om.on_execution(
            exec("O1", "E1", ExecKind::New, OrderState::New, 4000, 0, None),
            2,
        );
        om.on_private_down(3);
        assert_eq!(om.records[&token].state, LocalState::InDoubt);
        let stranger = exec("OX", "x", ExecKind::New, OrderState::New, 3000, 0, None).order;
        let (events, cmds) = om.reconcile(None, &[stranger], 4);
        assert!(events.iter().any(|e| matches!(
            e,
            OmEvent::Done {
                reason: DoneReason::Gone,
                ..
            }
        )));
        assert!(events
            .iter()
            .any(|e| matches!(e, OmEvent::UnknownOrder { .. })));
        assert!(matches!(&cmds[0], Command::Cancel { order_id, .. } if &**order_id == "OX"));
        check_index(&om);
    }

    #[test]
    fn finished_orders_leave_the_index_and_are_pruned() {
        let mut om = OrderManager::default();
        let mut tokens = Vec::new();
        for (i, at) in [0, 100, 200].into_iter().enumerate() {
            let (id, eid) = (format!("O{i}"), format!("E{i}"));
            let token = placed_token(&om.sync_side(
                &slug(),
                QuoteSide::Bid,
                Some(target(4000)),
                Px(1),
                Priority::Quote,
                at,
            ));
            om.on_rest_result(&accepted(token, &id), at);
            om.on_execution(
                exec(&id, &eid, ExecKind::New, OrderState::New, 4000, 0, None),
                at,
            );
            om.on_execution(
                exec(
                    &id,
                    &format!("{eid}c"),
                    ExecKind::Canceled,
                    OrderState::Canceled,
                    4000,
                    0,
                    None,
                ),
                at,
            );
            tokens.push(token);
        }
        check_index(&om);
        assert_eq!(om.active().count(), 0);

        // A lookup that finds a finished order still working brings it back into the index.
        let working = exec("O1", "x", ExecKind::New, OrderState::New, 4000, 0, None).order;
        om.on_order_lookup(&Arc::from("O1"), Some(&working), 250);
        check_index(&om);
        assert_eq!(
            om.active().map(|r| r.token).collect::<Vec<_>>(),
            vec![tokens[1]]
        );

        // Only finished records older than the cutoff go; the revived one stays.
        om.prune(260, 100);
        assert_eq!(
            om.records.keys().copied().collect::<Vec<_>>(),
            vec![tokens[1], tokens[2]]
        );
        om.prune(1_000, 100);
        assert_eq!(
            om.records.keys().copied().collect::<Vec<_>>(),
            vec![tokens[1]]
        );
        assert!(om.by_id.contains_key("O1") && !om.by_id.contains_key("O0"));
        check_index(&om);

        // Finishing it again queues it for pruning again.
        om.on_execution(
            exec(
                "O1",
                "E9",
                ExecKind::Canceled,
                OrderState::Canceled,
                4000,
                0,
                None,
            ),
            1_000,
        );
        om.prune(2_000, 100);
        assert!(om.records.is_empty() && om.done.is_empty());
        check_index(&om);
    }

    #[test]
    fn unmatched_execution_attaches_by_fields_then_unknown() {
        let cfg = ExecConfig::default();
        let mut om = OrderManager::default();
        om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            Px(1),
            Priority::Quote,
            0,
        );
        om.on_execution(
            exec("O1", "E1", ExecKind::New, OrderState::New, 4000, 0, None),
            1,
        );
        let (events, _) = om.check_timeouts(&cfg, 1 + secs(cfg.unmatched_attach_s) + 1);
        assert!(events.iter().any(|e| matches!(e, OmEvent::Acked { .. })));

        om.on_execution(
            exec(
                "O2",
                "E2",
                ExecKind::Fill,
                OrderState::Filled,
                2000,
                10_000,
                Some((10_000, "T9")),
            ),
            100,
        );
        let (events, _) = om.check_timeouts(&cfg, 100 + secs(cfg.unknown_order_s) + 1);
        assert!(events
            .iter()
            .any(|e| matches!(e, OmEvent::Fill(f) if f.token.is_none())));
        assert!(events
            .iter()
            .any(|e| matches!(e, OmEvent::UnknownOrder { .. })));
    }

    fn live_bid() -> (OrderManager, Token) {
        let mut om = OrderManager::default();
        let token = placed_token(&om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            Px(1),
            Priority::Quote,
            0,
        ));
        om.on_rest_result(&accepted(token, "O1"), 1);
        om.on_execution(
            exec("O1", "E1", ExecKind::New, OrderState::New, 4000, 0, None),
            2,
        );
        (om, token)
    }

    /// The venue keeps the id across a modify and reports the working order as `REPLACED`
    /// (seen on the socket and from `GET /v1/order/{id}` and `/v1/orders/open`).
    #[test]
    fn replaced_order_stays_live_until_it_fills() {
        let (mut om, token) = live_bid();
        let cmds = om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4100)),
            Px(1),
            Priority::Quote,
            3,
        );
        assert!(
            matches!(cmds[..], [Command::Modify { px: Px(4100), .. }]),
            "{cmds:?}"
        );
        om.on_execution(
            exec(
                "O1",
                "E2",
                ExecKind::Replace,
                OrderState::Replaced,
                4100,
                0,
                None,
            ),
            4,
        );
        assert_eq!(
            om.slot("m", QuoteSide::Bid).unwrap().state,
            LocalState::Live
        );

        // A lookup answering REPLACED is a live order, not a cancelled one.
        let replaced = exec(
            "O1",
            "x",
            ExecKind::Replace,
            OrderState::Replaced,
            4100,
            0,
            None,
        )
        .order;
        let (events, _) = om.on_order_lookup(&Arc::from("O1"), Some(&replaced), 5);
        assert!(
            !events.iter().any(|e| matches!(e, OmEvent::Done { .. })),
            "{events:?}"
        );
        assert_eq!(om.records[&token].state, LocalState::Live);

        // A rejected second modify leaves it working.
        om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4200)),
            Px(1),
            Priority::Quote,
            6,
        );
        let events = om.on_execution(
            exec(
                "O1",
                "E3",
                ExecKind::Rejected,
                OrderState::Replaced,
                4100,
                0,
                None,
            ),
            7,
        );
        assert!(
            !events.iter().any(|e| matches!(e, OmEvent::Done { .. })),
            "{events:?}"
        );
        assert_eq!(om.slot("m", QuoteSide::Bid).unwrap().px, Px(4100));

        let done = om.on_execution(
            exec(
                "O1",
                "E4",
                ExecKind::Fill,
                OrderState::Filled,
                4100,
                10_000,
                Some((10_000, "T1")),
            ),
            8,
        );
        assert!(done.iter().any(|e| matches!(
            e,
            OmEvent::Done {
                reason: DoneReason::Filled,
                ..
            }
        )));
        assert!(om.slot("m", QuoteSide::Bid).is_none());
    }

    #[test]
    fn hysteresis_never_keeps_more_than_the_target() {
        let (mut om, _) = live_bid();
        let sized = |qty| QuoteTarget {
            qty: Qty(qty),
            ..target(4000)
        };
        // Up to half below the target is close enough; above it is not, since risk may have clipped it.
        assert!(om
            .sync_side(
                &slug(),
                QuoteSide::Bid,
                Some(sized(20_000)),
                Px(1),
                Priority::Quote,
                3
            )
            .is_empty());
        let cmds = om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(sized(5_000)),
            Px(1),
            Priority::Quote,
            3,
        );
        assert!(
            matches!(
                cmds[..],
                [Command::Modify {
                    qty: Qty(5_000),
                    ..
                }]
            ),
            "{cmds:?}"
        );
    }

    #[test]
    fn changed_intent_is_replaced_at_the_same_price_and_size() {
        let (mut om, _) = live_bid();
        let closing = QuoteTarget {
            intent: Intent::SellShort,
            ..target(4000)
        };
        let cmds = om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(closing),
            Px(1),
            Priority::Quote,
            3,
        );
        assert!(matches!(cmds[..], [Command::Cancel { .. }]), "{cmds:?}");
    }

    #[test]
    fn unanswered_queries_are_retried_with_backoff() {
        let cfg = ExecConfig::default();
        let s = |x: f64| secs(x);
        let open_queries = |cmds: &[Command]| {
            cmds.iter()
                .filter(|c| matches!(c, Command::QueryOpenOrders { .. }))
                .count()
        };
        let (mut om, token) = live_bid();
        om.on_private_down(0);
        // First query after the ack timeout, then 4×, 8×, 16× it (12 s, 24 s, 48 s), capped at a minute.
        let mut sent = Vec::new();
        for t in (1..=200).map(|i| s(i as f64)) {
            if open_queries(&om.check_timeouts(&cfg, t).1) > 0 {
                sent.push(t / s(1.0));
            }
        }
        assert_eq!(sent, vec![4, 16, 40, 88, 148], "{sent:?}");
        assert_eq!(om.records[&token].state, LocalState::InDoubt);

        // An order lookup for a stuck cancel is retried the same way, and an answer resets it.
        let (mut om, token) = live_bid();
        om.cancel_rec(token, Priority::Cancel, s(10.0));
        let lookups = |om: &mut OrderManager, t| {
            om.check_timeouts(&cfg, t)
                .1
                .iter()
                .filter(|c| matches!(c, Command::QueryOrder { .. }))
                .count()
        };
        assert_eq!(lookups(&mut om, s(13.0)), 1);
        assert_eq!(lookups(&mut om, s(20.0)), 0);
        assert_eq!(lookups(&mut om, s(21.0)), 1);
        let still_there = exec("O1", "x", ExecKind::New, OrderState::New, 4000, 0, None).order;
        om.on_order_lookup(&Arc::from("O1"), Some(&still_there), s(22.0));
        assert_eq!(
            (om.records[&token].query_at, om.records[&token].queries),
            (None, 0)
        );
        check_index(&om);
    }

    /// A cancel the gateway keeps refusing is retried after 2 s, then 4 s, and after three refusals
    /// the order is looked up (with the query backoff) instead of cancelled every housekeeping pass.
    #[test]
    fn refused_cancels_back_off_then_look_the_order_up() {
        let cfg = ExecConfig::default();
        let s = |x: f64| secs(x);
        let (mut om, token) = live_bid();
        let refuse = |om: &mut OrderManager, t| {
            om.on_rest_result(
                &RestResult {
                    kind: RestKind::Cancel,
                    tokens: vec![token],
                    outcome: RestOutcome::GatewayRejected {
                        status: 400,
                        body: "order not found".into(),
                        class: RejectClass::Unknown,
                    },
                },
                t,
            )
        };
        let sent = |om: &mut OrderManager, t| {
            let cmds = om.check_timeouts(&cfg, t).1;
            (
                cmds.iter()
                    .filter(|c| matches!(c, Command::Cancel { .. }))
                    .count(),
                cmds.iter()
                    .filter(|c| matches!(c, Command::QueryOrder { .. }))
                    .count(),
            )
        };
        om.cancel_rec(token, Priority::Cancel, s(10.0));
        refuse(&mut om, s(10.0));
        assert_eq!(om.records[&token].state, LocalState::Live);
        assert_eq!(sent(&mut om, s(11.0)), (0, 0));
        assert_eq!(sent(&mut om, s(12.0)), (1, 0));
        refuse(&mut om, s(12.0));
        assert_eq!(sent(&mut om, s(15.0)), (0, 0));
        assert_eq!(sent(&mut om, s(16.0)), (1, 0));
        refuse(&mut om, s(16.0));
        assert_eq!(sent(&mut om, s(16.5)), (0, 1));
        assert_eq!(sent(&mut om, s(17.0)), (0, 0));
        assert_eq!(sent(&mut om, s(24.5)), (0, 1));
        // The venue no longer has it: done, nothing more is sent.
        om.on_order_lookup(&Arc::from("O1"), None, s(25.0));
        assert!(om.records[&token].state.is_done());
        assert_eq!(sent(&mut om, s(60.0)), (0, 0));
        check_index(&om);
    }

    /// Each execution that changes what we rest is logged with its exchange time until a book at or
    /// after that time arrives; a quote moves only when the target is `min_move` away.
    #[test]
    fn book_changes_are_held_until_a_book_shows_them_and_small_moves_wait() {
        let mut om = OrderManager::default();
        let at = |e: ExecutionFrame, ns: i64| ExecutionFrame {
            transact_ns: Some(ns),
            ..e
        };
        let unshown = |om: &OrderManager, book_ns: i64| -> Vec<(i64, i64)> {
            om.unshown("m", book_ns)
                .map(|c| (c.px.0, c.qty.0))
                .collect()
        };
        let two_ticks = Px(200);
        let token = placed_token(&om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4000)),
            two_ticks,
            Priority::Quote,
            0,
        ));
        om.on_rest_result(&accepted(token, "O1"), 1);
        let new = exec("O1", "E1", ExecKind::New, OrderState::New, 4000, 0, None);
        om.on_execution(at(new, 10), 2);
        assert_eq!(unshown(&om, 9), vec![(4000, 10_000)]);
        assert!(unshown(&om, 10).is_empty());

        // One tick away: stays. Two ticks: modifies.
        assert!(om
            .sync_side(
                &slug(),
                QuoteSide::Bid,
                Some(target(4100)),
                two_ticks,
                Priority::Quote,
                3
            )
            .is_empty());
        let cmds = om.sync_side(
            &slug(),
            QuoteSide::Bid,
            Some(target(4200)),
            two_ticks,
            Priority::Quote,
            3,
        );
        assert!(matches!(cmds[0], Command::Modify { px: Px(4200), .. }));
        let replace = exec(
            "O1",
            "E2",
            ExecKind::Replace,
            OrderState::New,
            4200,
            0,
            None,
        );
        om.on_execution(at(replace, 20), 4);
        // A book from before the replace still has us at 0.40.
        assert_eq!(unshown(&om, 15), vec![(4000, -10_000), (4200, 10_000)]);

        om.book_seen("m", 10);
        assert_eq!(unshown(&om, 0).len(), 2);
        om.book_seen("m", 20);
        assert!(unshown(&om, 0).is_empty());

        let cancel = exec(
            "O1",
            "E3",
            ExecKind::Canceled,
            OrderState::Canceled,
            4200,
            0,
            None,
        );
        om.on_execution(at(cancel, 30), 5);
        assert_eq!(unshown(&om, 25), vec![(4200, -10_000)]);
        om.forget_unshown("m");
        assert!(unshown(&om, 0).is_empty());
    }
}
