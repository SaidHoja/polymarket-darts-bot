//! The risk state file (`pm_core::RiskState`): the daily kill, the day's PnL, market losses and
//! indefinite pauses, so a restart doesn't clear them. Loaded once at startup; the core thread offers
//! the state when it may have changed and a small thread writes it (temp file, then rename, so a
//! crash never leaves half a file). Not on the core thread: a write can stall on a slow disk.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::thread::JoinHandle;

use pm_core::RiskState;

/// The saved state, `None` if there's no file yet. An unreadable file is an error: starting without
/// it would clear the limits it holds, so the operator has to look at it (or delete it) first.
pub fn load(path: &Path) -> Result<Option<RiskState>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| {
            format!(
                "{}: {e} (fix or delete it; it holds the daily kill and loss limits)",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn write(path: &Path, state: &RiskState) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, path)
}

pub struct Saver {
    tx: Option<SyncSender<RiskState>>,
    thread: Option<JoinHandle<()>>,
    last: Option<RiskState>,
}

impl Saver {
    pub fn start(path: PathBuf, initial: Option<RiskState>) -> Result<Saver, String> {
        let (tx, rx) = sync_channel::<RiskState>(1);
        let thread = std::thread::Builder::new()
            .name("risk-file".into())
            .spawn(move || {
                while let Ok(state) = rx.recv() {
                    if let Err(e) = write(&path, &state) {
                        tracing::error!(
                            "risk state: can't write {}: {e}; a restart would lose the loss limits",
                            path.display()
                        );
                    }
                }
            })
            .map_err(|e| format!("risk state thread: {e}"))?;
        Ok(Saver {
            tx: Some(tx),
            thread: Some(thread),
            last: initial,
        })
    }

    /// Save `state` if it changed. Never blocks: if a write is still running, a later offer saves it.
    pub fn offer(&mut self, state: RiskState) {
        if self.last.as_ref() == Some(&state) {
            return;
        }
        let Some(tx) = &self.tx else { return };
        match tx.try_send(state.clone()) {
            Ok(()) => self.last = Some(state),
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => self.tx = None,
        }
    }

    /// Save `state` and wait until it (and anything queued before it) is written.
    pub fn finish(mut self, state: RiskState) {
        if let Some(tx) = self.tx.take() {
            if self.last.as_ref() != Some(&state) {
                let _ = tx.send(state);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_changes_and_loads_them_back() {
        let dir = std::env::temp_dir().join(format!("pmbot-risk-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("risk-state.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load(&path).unwrap(), None);

        let mut state = RiskState {
            day: 20_000,
            daily_pnl: -1.25,
            killed: true,
            ..Default::default()
        };
        state.paused.insert("m".into(), "operator pause".into());
        let mut saver = Saver::start(path.clone(), None).unwrap();
        saver.offer(state.clone());
        state.daily_pnl = -2.0;
        saver.finish(state.clone());
        assert_eq!(load(&path).unwrap(), Some(state));

        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
