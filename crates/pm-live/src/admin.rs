//! Operator socket: `run/pmbot.sock`, one text command per connection.
//!
//! ```text
//! echo status | nc -U run/pmbot.sock
//! echo metrics | nc -U run/pmbot.sock             # latency table since start
//! echo "pause <slug>" | nc -U run/pmbot.sock      # no slug = everything
//! echo "resume <slug>" | nc -U run/pmbot.sock
//! echo cancel-all | nc -U run/pmbot.sock
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pm_types::{AdminCmd, Event};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::inbox::Inbox;
use crate::metrics::Metrics;

#[derive(Debug, PartialEq)]
pub enum Request {
    Cmd(AdminCmd),
    Status,
    Metrics,
}

/// Extra words are an error, not ignored: `pause one two` must not quietly pause only `one`.
pub fn parse(line: &str) -> Result<Request, String> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().map(str::to_string);
    let takes_slug = matches!(cmd, "pause" | "resume");
    if parts.next().is_some() || (arg.is_some() && !takes_slug) {
        return Err(format!("too many arguments for {cmd:?}; try status | metrics | pause [slug] | resume [slug] | cancel-all"));
    }
    match cmd {
        "pause" => Ok(Request::Cmd(AdminCmd::Pause(arg))),
        "resume" => Ok(Request::Cmd(AdminCmd::Resume(arg))),
        "cancel-all" => Ok(Request::Cmd(AdminCmd::CancelAll)),
        "status" => Ok(Request::Status),
        "metrics" => Ok(Request::Metrics),
        other => Err(format!("unknown command {other:?}; try status | metrics | pause [slug] | resume [slug] | cancel-all")),
    }
}

pub async fn serve(
    path: PathBuf,
    inbox: Arc<Inbox>,
    status: Arc<Mutex<String>>,
    metrics: Arc<Metrics>,
) {
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("admin socket {}: {e}", path.display());
            return;
        }
    };
    tracing::info!("admin socket at {}", path.display());
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let (inbox, status, metrics) = (inbox.clone(), status.clone(), metrics.clone());
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            if BufReader::new(read).read_line(&mut line).await.is_err() {
                return;
            }
            let reply = match parse(line.trim()) {
                Ok(Request::Cmd(cmd)) => {
                    tracing::warn!("admin: {cmd:?}");
                    inbox.control(Event::Admin(cmd));
                    "ok\n".to_string()
                }
                Ok(Request::Status) => format!("{}\n", status.lock().unwrap()),
                Ok(Request::Metrics) => metrics.table(),
                Err(e) => format!("error: {e}\n"),
            };
            let _ = write.write_all(reply.as_bytes()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse("pause"), Ok(Request::Cmd(AdminCmd::Pause(None))));
        assert_eq!(
            parse("resume abc"),
            Ok(Request::Cmd(AdminCmd::Resume(Some("abc".into()))))
        );
        assert_eq!(parse("cancel-all"), Ok(Request::Cmd(AdminCmd::CancelAll)));
        assert_eq!(parse("status"), Ok(Request::Status));
        assert_eq!(parse("metrics"), Ok(Request::Metrics));
        assert!(parse("sell everything").is_err());
        assert!(parse("pause one two").is_err());
        assert!(parse("cancel-all unexpected").is_err());
        assert!(parse("status now").is_err());
    }
}
