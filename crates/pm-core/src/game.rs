//! Live match state per event.

use std::collections::BTreeMap;

use pm_types::GameFrame;

const KNOWN_PERIODS: [&str; 4] = ["", "NS", "LIVE", "CAN"];

#[derive(Debug, Clone, PartialEq)]
pub enum GameChange {
    /// Score changed (home, away). Also fires for corrections that lower a score.
    LegChanged {
        prev: Option<(u16, u16)>,
        new: Option<(u16, u16)>,
    },
    Live,
    Ended,
    Cancelled,
    UnknownPeriod(String),
}

#[derive(Debug, Clone)]
pub struct GameState {
    pub frame: GameFrame,
    pub last_leg_change_mono: Option<u64>,
}

impl GameState {
    pub fn known_period(&self) -> bool {
        KNOWN_PERIODS.contains(&self.frame.period.as_str())
    }
}

#[derive(Debug, Default)]
pub struct GameStore {
    pub games: BTreeMap<String, GameState>,
    pub last_poll_mono: Option<u64>,
}

impl GameStore {
    pub fn on_poll(&mut self, now: u64) {
        self.last_poll_mono = Some(now);
    }

    pub fn on_game(&mut self, frame: GameFrame, now: u64) -> Vec<GameChange> {
        let mut out = Vec::new();
        let Some(state) = self.games.get_mut(&frame.event_id) else {
            if frame.live {
                out.push(GameChange::Live);
            }
            if !KNOWN_PERIODS.contains(&frame.period.as_str()) {
                out.push(GameChange::UnknownPeriod(frame.period.clone()));
            }
            self.games.insert(
                frame.event_id.clone(),
                GameState {
                    frame,
                    last_leg_change_mono: None,
                },
            );
            return out;
        };
        let prev = &state.frame;
        // Ignore a poll that is older than what we have.
        if let (Some(old), Some(new)) = (prev.updated_ns, frame.updated_ns) {
            if new < old {
                return out;
            }
        }
        if frame.score != prev.score
            && (prev.score.is_some() || frame.score.is_some_and(|s| s != (0, 0)))
        {
            out.push(GameChange::LegChanged {
                prev: prev.score,
                new: frame.score,
            });
            state.last_leg_change_mono = Some(now);
        }
        if frame.live && !prev.live {
            out.push(GameChange::Live);
        }
        if frame.ended && !prev.ended {
            out.push(GameChange::Ended);
        }
        if frame.cancelled() && !prev.cancelled() {
            out.push(GameChange::Cancelled);
        }
        if frame.period != prev.period && !KNOWN_PERIODS.contains(&frame.period.as_str()) {
            out.push(GameChange::UnknownPeriod(frame.period.clone()));
        }
        state.frame = frame;
        out
    }

    pub fn get(&self, event_id: &str) -> Option<&GameState> {
        self.games.get(event_id)
    }

    /// Stale only matters while some match is live.
    pub fn stale(&self, now: u64, stale_ns: u64) -> bool {
        let any_live = self.games.values().any(|g| g.frame.live && !g.frame.ended);
        any_live
            && self
                .last_poll_mono
                .is_none_or(|t| now.saturating_sub(t) > stale_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(score: Option<(u16, u16)>, live: bool, period: &str) -> GameFrame {
        GameFrame {
            event_id: "e".into(),
            event_slug: "s".into(),
            period: period.into(),
            live,
            ended: false,
            score,
            updated_ns: None,
        }
    }

    #[test]
    fn detects_leg_changes_and_periods() {
        let mut g = GameStore::default();
        assert!(g.on_game(frame(None, false, ""), 0).is_empty());
        assert_eq!(
            g.on_game(frame(Some((0, 0)), true, "LIVE"), 1),
            vec![GameChange::Live]
        );
        assert_eq!(
            g.on_game(frame(Some((1, 0)), true, "LIVE"), 2),
            vec![GameChange::LegChanged {
                prev: Some((0, 0)),
                new: Some((1, 0))
            }]
        );
        assert_eq!(g.get("e").unwrap().last_leg_change_mono, Some(2));
        assert!(g.on_game(frame(Some((1, 0)), true, "LIVE"), 3).is_empty());
        assert_eq!(
            g.on_game(frame(Some((1, 0)), true, "XYZ"), 4),
            vec![GameChange::UnknownPeriod("XYZ".into())]
        );
        assert!(g.stale(10_000_000_000, 5_000_000_000));
        g.on_poll(9_000_000_000);
        assert!(!g.stale(10_000_000_000, 5_000_000_000));
    }
}
