//! Input queues between the I/O tasks and the core thread.
//!
//! | Input                           | Queue                                    |
//! | ------------------------------- | ---------------------------------------- |
//! | control (connection, admin, …)  | unbounded FIFO, never dropped            |
//! | order feedback                  | unbounded FIFO, never dropped/reordered  |
//! | books                           | latest-value slot per market (conflates) |
//! | game state                      | latest-value slot per event (conflates)  |
//! | trades                          | bounded FIFO; overflow is counted        |
//!
//! Every push rings one doorbell; many pushes collapse into one wakeup.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use pm_types::{BookFrame, ConnState, Event, Feed, GameFrame, Slug, TradeFrame};

const TRADE_QUEUE: usize = 10_000;
pub const TRADES_PER_TICK: usize = 1_000;

pub struct Inbox {
    control_tx: Sender<Event>,
    control_rx: Receiver<Event>,
    feedback_tx: Sender<Event>,
    feedback_rx: Receiver<Event>,
    trades_tx: Sender<(TradeFrame, Instant)>,
    trades_rx: Receiver<(TradeFrame, Instant)>,
    books: Mutex<BTreeMap<Slug, (BookFrame, Instant)>>,
    games: Mutex<BTreeMap<String, (GameFrame, Instant)>>,
    game_polled: AtomicBool,
    doorbell_tx: Sender<()>,
    doorbell_rx: Receiver<()>,
    epoch: Instant,
    /// When the oldest undrained event was pushed: ns since `epoch` + 1, 0 = nothing pending.
    oldest: AtomicU64,
    /// Trades lost because the trade queue was full: the core fell behind.
    trades_dropped: AtomicU64,
    /// Books replaced by a newer one for the same market before the core read them. Expected under load.
    books_conflated: AtomicU64,
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Inbox {
    pub fn new() -> Self {
        let (control_tx, control_rx) = unbounded();
        let (feedback_tx, feedback_rx) = unbounded();
        let (trades_tx, trades_rx) = bounded(TRADE_QUEUE);
        let (doorbell_tx, doorbell_rx) = bounded(1);
        Inbox {
            control_tx,
            control_rx,
            feedback_tx,
            feedback_rx,
            trades_tx,
            trades_rx,
            books: Mutex::new(BTreeMap::new()),
            games: Mutex::new(BTreeMap::new()),
            game_polled: AtomicBool::new(false),
            doorbell_tx,
            doorbell_rx,
            epoch: Instant::now(),
            oldest: AtomicU64::new(0),
            trades_dropped: AtomicU64::new(0),
            books_conflated: AtomicU64::new(0),
        }
    }

    pub fn ring(&self) {
        let now = self.epoch.elapsed().as_nanos() as u64 + 1;
        let _ = self
            .oldest
            .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
        let _ = self.doorbell_tx.try_send(());
    }

    pub fn control(&self, event: Event) {
        let _ = self.control_tx.send(event);
        self.ring();
    }

    /// The markets socket went down. Its books and trades still queued belong to the old connection:
    /// drained after the `Down` (controls go first), a book would seed the market again before
    /// the new connection has sent anything. The feed task is their only producer, so nothing
    /// older can arrive after this.
    pub fn markets_down(&self) {
        self.books.lock().unwrap().clear();
        while self.trades_rx.try_recv().is_ok() {}
        self.control(Event::Connection {
            feed: Feed::Markets,
            state: ConnState::Down,
        });
    }

    pub fn feedback(&self, event: Event) {
        let _ = self.feedback_tx.send(event);
        self.ring();
    }

    pub fn trade(&self, trade: TradeFrame, received: Instant) {
        match self.trades_tx.try_send((trade, received)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.trades_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.ring();
    }

    pub fn book(&self, book: BookFrame, received: Instant) {
        let replaced = self
            .books
            .lock()
            .unwrap()
            .insert(book.slug.clone(), (book, received));
        if replaced.is_some() {
            self.books_conflated.fetch_add(1, Ordering::Relaxed);
        }
        self.ring();
    }

    pub fn game(&self, game: GameFrame, received: Instant) {
        self.games
            .lock()
            .unwrap()
            .insert(game.event_id.clone(), (game, received));
        self.ring();
    }

    pub fn game_polled(&self) {
        self.game_polled.store(true, Ordering::Relaxed);
        self.ring();
    }

    /// Sleep until something is pushed or the timeout passes.
    pub fn wait(&self, timeout: Duration) {
        let _ = self.doorbell_rx.recv_timeout(timeout);
    }

    /// `wait`, but polling for the first `spin` of it: input that arrives meanwhile is picked up
    /// without the thread going to sleep and being woken again. True if it went to sleep.
    pub fn wait_spinning(&self, timeout: Duration, spin: Duration) -> bool {
        let start = Instant::now();
        let spin = spin.min(timeout);
        while start.elapsed() < spin {
            if self.doorbell_rx.try_recv().is_ok() {
                return false;
            }
            std::hint::spin_loop();
        }
        self.wait(timeout.saturating_sub(start.elapsed()));
        true
    }

    /// How long the oldest undrained event has been waiting, if any.
    pub fn pending_for(&self) -> Option<Duration> {
        let oldest = self.oldest.load(Ordering::Relaxed);
        (oldest > 0).then(|| {
            self.epoch
                .elapsed()
                .saturating_sub(Duration::from_nanos(oldest - 1))
        })
    }

    pub fn trades_dropped(&self) -> u64 {
        self.trades_dropped.load(Ordering::Relaxed)
    }

    pub fn books_conflated(&self) -> u64 {
        self.books_conflated.load(Ordering::Relaxed)
    }

    pub fn pending(&self) -> usize {
        self.control_rx.len()
            + self.feedback_rx.len()
            + self.trades_rx.len()
            + self.books.lock().unwrap().len()
            + self.games.lock().unwrap().len()
            + self.game_polled.load(Ordering::Relaxed) as usize
    }

    /// Everything queued, in drain-priority order (the core sorts again, stably).
    pub fn drain(&self) -> Vec<Event> {
        self.drain_timed()
            .0
            .into_iter()
            .map(|(event, _)| event)
            .collect()
    }

    /// `drain`, plus how long the oldest drained event waited in the inbox.
    pub fn drain_timed(&self) -> (Vec<(Event, Option<Instant>)>, Option<Duration>) {
        let oldest = self.oldest.swap(0, Ordering::Relaxed);
        let waited = (oldest > 0).then(|| {
            self.epoch
                .elapsed()
                .saturating_sub(Duration::from_nanos(oldest - 1))
        });
        let mut out: Vec<_> = self.control_rx.try_iter().map(|e| (e, None)).collect();
        out.extend(self.feedback_rx.try_iter().map(|e| (e, None)));
        let games = std::mem::take(&mut *self.games.lock().unwrap());
        out.extend(
            games
                .into_values()
                .map(|(g, at)| (Event::Game(g), Some(at))),
        );
        if self.game_polled.swap(false, Ordering::Relaxed) {
            out.push((Event::GamePolled, None));
        }
        out.extend(
            self.trades_rx
                .try_iter()
                .take(TRADES_PER_TICK)
                .map(|(t, at)| (Event::Trade(t), Some(at))),
        );
        let books = std::mem::take(&mut *self.books.lock().unwrap());
        out.extend(
            books
                .into_values()
                .map(|(b, at)| (Event::Book(b), Some(at))),
        );
        if !self.trades_rx.is_empty() {
            self.ring(); // the rest go into the next tick
        }
        (out, waited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_types::{MarketState, Px, Qty};
    use std::sync::Arc;

    fn book(slug: &str, ns: i64) -> BookFrame {
        BookFrame {
            slug: Arc::from(slug),
            bids: vec![(Px(4000), Qty(1))],
            offers: vec![],
            state: MarketState::Open,
            transact_ns: Some(ns),
            shares_traded: None,
            settlement: None,
        }
    }

    #[test]
    fn spinning_wait_returns_on_input_and_times_out_without() {
        let inbox = Arc::new(Inbox::new());
        let t = Instant::now();
        assert!(
            inbox.wait_spinning(Duration::from_millis(20), Duration::from_millis(5)),
            "should report sleeping"
        );
        assert!(
            t.elapsed() >= Duration::from_millis(20),
            "{:?}",
            t.elapsed()
        );
        assert!(inbox.pending_for().is_none());

        let pusher = {
            let inbox = inbox.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(2));
                inbox.feedback(Event::GamePolled);
            })
        };
        let t = Instant::now();
        assert!(
            !inbox.wait_spinning(Duration::from_secs(5), Duration::from_secs(1)),
            "input came while spinning"
        );
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
        pusher.join().unwrap();
        assert!(inbox
            .pending_for()
            .is_some_and(|d| d < Duration::from_millis(500)));
        assert_eq!(inbox.drain().len(), 1);
        assert!(inbox.pending_for().is_none());
    }

    #[test]
    fn books_conflate_per_market_and_feedback_never_drops() {
        let inbox = Inbox::new();
        inbox.book(book("a", 1), Instant::now());
        inbox.book(book("a", 2), Instant::now());
        inbox.book(book("b", 1), Instant::now());
        inbox.feedback(Event::GamePolled);
        inbox.feedback(Event::GamePolled);
        let events = inbox.drain();
        let books: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Book(b) => Some((b.slug.to_string(), b.transact_ns)),
                _ => None,
            })
            .collect();
        assert_eq!(books, vec![("a".into(), Some(2)), ("b".into(), Some(1))]);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::GamePolled))
                .count(),
            2
        );
        assert_eq!(inbox.books_conflated(), 1);
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn a_full_trade_queue_counts_what_it_drops() {
        let inbox = Inbox::new();
        let trade = TradeFrame {
            id: "t".into(),
            slug: Arc::from("a"),
            px: Px(4000),
            qty: Qty(1),
            taker_side: pm_types::Side::Buy,
            trade_ns: None,
        };
        for _ in 0..TRADE_QUEUE + 3 {
            inbox.trade(trade.clone(), Instant::now());
        }
        assert_eq!(inbox.trades_dropped(), 3);
        assert_eq!(inbox.books_conflated(), 0);
    }

    #[test]
    fn markets_down_discards_the_old_connections_books_and_trades() {
        let inbox = Inbox::new();
        inbox.book(book("a", 1), Instant::now());
        inbox.trade(
            TradeFrame {
                id: "t".into(),
                slug: Arc::from("a"),
                px: Px(4000),
                qty: Qty(1),
                taker_side: pm_types::Side::Buy,
                trade_ns: None,
            },
            Instant::now(),
        );
        inbox.feedback(Event::GamePolled);
        inbox.markets_down();
        assert_eq!(
            inbox.drain(),
            vec![
                Event::Connection {
                    feed: Feed::Markets,
                    state: ConnState::Down
                },
                Event::GamePolled
            ]
        );
    }

    #[test]
    fn a_successful_game_poll_counts_as_pending_work() {
        let inbox = Inbox::new();
        inbox.game_polled();
        assert_eq!(inbox.pending(), 1);
        inbox.drain();
        assert_eq!(inbox.pending(), 0);
    }

    #[test]
    fn timestamps_follow_surviving_books_and_do_not_leak_across_drains() {
        let inbox = Inbox::new();
        let old = Instant::now() - Duration::from_millis(50);
        let latest = Instant::now();
        inbox.book(book("a", 1), old);
        inbox.book(book("a", 2), latest);
        inbox.book(book("b", 1), old);
        let (events, _) = inbox.drain_timed();
        assert!(events
            .iter()
            .any(|(e, at)| matches!(e, Event::Book(b) if &*b.slug == "a") && *at == Some(latest)));
        assert!(events
            .iter()
            .any(|(e, at)| matches!(e, Event::Book(b) if &*b.slug == "b") && *at == Some(old)));
        inbox.feedback(Event::GamePolled);
        assert_eq!(inbox.drain_timed().0, vec![(Event::GamePolled, None)]);
    }
}
