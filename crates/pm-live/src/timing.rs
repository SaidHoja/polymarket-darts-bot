//! Live-only timing metadata. Attribution is to the oldest surviving same-market
//! input in this batch, not to an arbitrary global inbox event or a previous tick.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use pm_core::orders::{LocalState, OrderManager};
use pm_types::{Command, ConnState, Event, Priority, RestKind, Slug, Token};

use crate::metrics::{Metrics, Stage};

pub struct Outbound {
    pub command: Command,
    pub received: Option<Instant>,
    /// When the core handed it to the gateway channel (`gw_handoff` starts here).
    pub sent: Instant,
}

#[derive(Default)]
pub struct InputTiming {
    markets: BTreeMap<Slug, String>,
    pending: BTreeMap<Token, (RestKind, Instant)>,
}

impl InputTiming {
    pub fn inputs(&mut self, inputs: &[(Event, Option<Instant>)]) -> BTreeMap<Slug, Instant> {
        for (event, _) in inputs {
            if let Event::Markets(markets) = event {
                self.markets = markets
                    .iter()
                    .map(|m| (m.slug.clone(), m.event_id.clone()))
                    .collect();
            }
            if matches!(
                event,
                Event::Connection {
                    state: ConnState::Down,
                    ..
                }
            ) {
                self.pending.clear();
            }
        }
        let mut sources = BTreeMap::<Slug, Instant>::new();
        let mut note = |slug: &Slug, at: Instant| {
            sources
                .entry(slug.clone())
                .and_modify(|old| *old = (*old).min(at))
                .or_insert(at);
        };
        for (event, received) in inputs {
            let Some(at) = *received else { continue };
            match event {
                Event::Book(b) => note(&b.slug, at),
                Event::Trade(t) => note(&t.slug, at),
                Event::Game(g) => {
                    for (slug, _) in self.markets.iter().filter(|(_, id)| **id == g.event_id) {
                        note(slug, at);
                    }
                }
                _ => {}
            }
        }
        sources
    }

    pub fn command(&mut self, command: Command, sources: &BTreeMap<Slug, Instant>) -> Outbound {
        let slug = match &command {
            Command::Place { order, .. } => Some(&order.slug),
            Command::Modify { slug, priority, .. } if *priority != Priority::Refresh => Some(slug),
            Command::Cancel { slug, .. } => Some(slug),
            _ => None,
        };
        let received = slug.and_then(|s| sources.get(s)).copied();
        match &command {
            Command::Place { token, .. } | Command::Modify { token, .. } => {
                self.pending.remove(token);
                if let Some(at) = received {
                    let kind = if matches!(command, Command::Place { .. }) {
                        RestKind::Create
                    } else {
                        RestKind::Modify
                    };
                    self.pending.insert(*token, (kind, at));
                }
            }
            Command::Withdraw { token } | Command::Cancel { token, .. } => {
                self.pending.remove(token);
            }
            Command::CancelAll { .. } => self.pending.clear(),
            _ => {}
        }
        Outbound {
            command,
            received,
            sent: Instant::now(),
        }
    }

    pub fn acknowledged(&mut self, acks: &[(Token, RestKind)], metrics: &Metrics) {
        for (token, kind) in acks {
            if self
                .pending
                .get(token)
                .is_some_and(|(expected, _)| expected == kind)
            {
                let (_, at) = self.pending.remove(token).unwrap();
                metrics.record_since(Stage::InputToAck, at);
            }
        }
    }

    pub fn prune(&mut self, om: &OrderManager) {
        self.pending.retain(|token, (_, at)| {
            at.elapsed() < Duration::from_secs(60)
                && om.records.get(token).is_some_and(|r| {
                    matches!(r.state, LocalState::PendingNew | LocalState::PendingReplace)
                })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_types::{Intent, NewOrder, Px, Qty, Tif};

    fn place(token: u64, slug: &str) -> Command {
        Command::Place {
            token: Token(token),
            order: NewOrder {
                slug: slug.into(),
                intent: Intent::BuyLong,
                px: Px(100),
                qty: Qty(100),
                tif: Tif::Gtd(100),
                post_only: true,
            },
            priority: Priority::Quote,
        }
    }

    #[test]
    fn correlation_is_per_market_and_ack_kind_and_only_once() {
        let mut timing = InputTiming::default();
        let at = Instant::now() - Duration::from_millis(10);
        let sources = BTreeMap::from([("a".into(), at)]);
        assert_eq!(timing.command(place(1, "a"), &sources).received, Some(at));
        assert!(timing.command(place(2, "b"), &sources).received.is_none());
        let metrics = Metrics::default();
        timing.acknowledged(&[(Token(1), RestKind::Modify)], &metrics);
        assert!(!metrics.table().contains("input_to_ack"));
        timing.acknowledged(&[(Token(1), RestKind::Create)], &metrics);
        let first = metrics.table();
        assert!(first.contains("input_to_ack"));
        timing.acknowledged(&[(Token(1), RestKind::Create)], &metrics);
        assert_eq!(metrics.table(), first);
        assert!(timing
            .command(place(3, "a"), &BTreeMap::new())
            .received
            .is_none());
    }

    #[test]
    fn withdrawal_and_refresh_discard_old_origins() {
        let mut timing = InputTiming::default();
        let sources = BTreeMap::from([("a".into(), Instant::now())]);
        timing.command(place(1, "a"), &sources);
        timing.command(Command::Withdraw { token: Token(1) }, &sources);
        assert!(timing.pending.is_empty());
        let c = Command::Modify {
            token: Token(1),
            order_id: "o".into(),
            slug: "a".into(),
            px: Px(100),
            qty: Qty(100),
            tif: Tif::Gtd(100),
            post_only: true,
            priority: Priority::Refresh,
        };
        assert!(timing.command(c, &sources).received.is_none());
        assert!(timing.pending.is_empty());
    }

    #[test]
    fn oldest_same_market_input_is_used_and_disconnect_clears_pending() {
        use pm_types::{BookFrame, Feed, MarketState};
        let mut timing = InputTiming::default();
        let old = Instant::now() - Duration::from_millis(20);
        let new = Instant::now();
        let book = |slug: &str| {
            Event::Book(BookFrame {
                slug: slug.into(),
                bids: vec![],
                offers: vec![],
                state: MarketState::Open,
                transact_ns: None,
                shares_traded: None,
                settlement: None,
            })
        };
        let sources = timing.inputs(&[
            (book("a"), Some(new)),
            (book("a"), Some(old)),
            (book("b"), Some(new)),
        ]);
        assert_eq!(sources["a"], old);
        assert_eq!(sources["b"], new);
        timing.command(place(1, "a"), &sources);
        assert!(timing.inputs(&[]).is_empty());
        timing.inputs(&[(
            Event::Connection {
                feed: Feed::Private,
                state: ConnState::Down,
            },
            None,
        )]);
        assert!(timing.pending.is_empty());
    }
}
