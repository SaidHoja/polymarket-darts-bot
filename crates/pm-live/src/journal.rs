//! Append-only JSONL journals, one file per stream per UTC day.
//!
//! Raw frames use the `{"conn", "raw", "recv_at"}` format that
//! `captures/analysis/*.py` read (plus `mono`). Core ticks go to
//! `core-<date>.jsonl` with their events and commands, so a session can be
//! replayed through the core. Writing happens on its own thread; if it falls
//! behind, lines are dropped and counted rather than blocking trading. Lines that
//! can't be written (disk full, a file that won't open) are counted separately.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{bounded, Sender};
use pm_types::time::{format_rfc3339_micros, format_rfc3339_secs};
use serde_json::json;

const QUEUE: usize = 100_000;

pub fn wall_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

enum Payload {
    Text(String),
    /// Serialised on the journal thread, so callers on the hot path only move owned data.
    Lazy(Box<dyn FnOnce() -> String + Send>),
}

struct Line {
    prefix: &'static str,
    wall_ns: i64,
    payload: Payload,
}

enum Msg {
    Line(Line),
    /// Write and flush everything queued before this, then answer.
    Flush(Sender<()>),
}

#[derive(Clone)]
pub struct Journal {
    tx: Option<Sender<Msg>>,
    /// Lines not queued because the writer was behind.
    pub dropped: Arc<AtomicU64>,
    /// Lines queued but not written: the file wouldn't open, or a write or flush failed.
    pub write_errors: Arc<AtomicU64>,
}

/// Counts failed writes, logging the first and then every 1,000th, so a full disk is visible
/// without flooding the log.
struct Errors(Arc<AtomicU64>);

impl Errors {
    fn add(&self, lines: u64, what: std::fmt::Arguments) {
        let before = self.0.fetch_add(lines, Ordering::Relaxed);
        if before == 0 || before / 1_000 != (before + lines) / 1_000 {
            tracing::error!("journal: {what} ({} lines lost so far)", before + lines);
        }
    }
}

impl Journal {
    pub fn disabled() -> Self {
        Journal {
            tx: None,
            dropped: Arc::new(AtomicU64::new(0)),
            write_errors: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn start(dir: PathBuf) -> std::io::Result<Self> {
        Self::start_with(dir, |f| f)
    }

    /// `wrap` lets tests substitute a failing writer.
    fn start_with<W: Write + Send + 'static>(
        dir: PathBuf,
        wrap: impl Fn(File) -> W + Send + 'static,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let (tx, rx) = bounded::<Msg>(QUEUE);
        let write_errors = Arc::new(AtomicU64::new(0));
        let errors = Errors(write_errors.clone());
        thread::Builder::new()
            .name("journal".into())
            .spawn(move || {
                // Per stream: the day it's writing, the writer, and lines written since the last flush.
                let mut files: BTreeMap<&'static str, (String, BufWriter<W>, u64)> =
                    BTreeMap::new();
                let flush = |files: &mut BTreeMap<&'static str, (String, BufWriter<W>, u64)>| {
                    for (prefix, (day, w, pending)) in files.iter_mut() {
                        if let Err(e) = w.flush() {
                            errors.add(*pending, format_args!("flushing {prefix}{day}.jsonl: {e}"));
                        }
                        *pending = 0;
                    }
                };
                while let Ok(first) = rx.recv() {
                    for msg in std::iter::once(first).chain(rx.try_iter()) {
                        let line = match msg {
                            Msg::Line(line) => line,
                            Msg::Flush(done) => {
                                flush(&mut files);
                                let _ = done.send(());
                                continue;
                            }
                        };
                        let day = format_rfc3339_secs(line.wall_ns)[..10].to_string();
                        if files.get(line.prefix).is_none_or(|(d, _, _)| *d != day) {
                            let path = dir.join(format!("{}{day}.jsonl", line.prefix));
                            match OpenOptions::new().create(true).append(true).open(&path) {
                                Ok(f) => {
                                    // Replacing yesterday's writer flushes it on drop; report a failure first.
                                    if let Some((old_day, mut w, pending)) =
                                        files.remove(line.prefix)
                                    {
                                        if let Err(e) = w.flush() {
                                            errors.add(
                                                pending,
                                                format_args!(
                                                    "flushing {}{old_day}.jsonl: {e}",
                                                    line.prefix
                                                ),
                                            );
                                        }
                                    }
                                    files.insert(line.prefix, (day, BufWriter::new(wrap(f)), 0));
                                }
                                Err(e) => {
                                    errors.add(1, format_args!("opening {}: {e}", path.display()));
                                    continue;
                                }
                            }
                        }
                        let text = match line.payload {
                            Payload::Text(t) => t,
                            Payload::Lazy(f) => f(),
                        };
                        let (day, w, pending) = files.get_mut(line.prefix).expect("opened above");
                        match w
                            .write_all(text.as_bytes())
                            .and_then(|()| w.write_all(b"\n"))
                        {
                            Ok(()) => *pending += 1,
                            Err(e) => errors
                                .add(1, format_args!("writing {}{day}.jsonl: {e}", line.prefix)),
                        }
                    }
                    flush(&mut files);
                }
            })?;
        Ok(Journal {
            tx: Some(tx),
            dropped: Arc::new(AtomicU64::new(0)),
            write_errors,
        })
    }

    /// Write out everything queued so far, waiting up to `timeout`. False if it didn't finish in time.
    pub fn flush(&self, timeout: Duration) -> bool {
        let Some(tx) = &self.tx else { return true };
        let (done_tx, done_rx) = bounded(1);
        tx.send_timeout(Msg::Flush(done_tx), timeout).is_ok()
            && done_rx.recv_timeout(timeout).is_ok()
    }

    pub fn enabled(&self) -> bool {
        self.tx.is_some()
    }

    fn send(&self, prefix: &'static str, wall_ns: i64, payload: Payload) {
        if let Some(tx) = &self.tx {
            if tx
                .try_send(Msg::Line(Line {
                    prefix,
                    wall_ns,
                    payload,
                }))
                .is_err()
            {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A raw venue frame, in the `{"conn", "raw", "recv_at"}` capture format.
    pub fn raw(&self, prefix: &'static str, conn: u64, raw: &str, wall_ns: i64, mono_ns: u64) {
        if self.tx.is_none() {
            return;
        }
        let text = json!({"conn": conn, "raw": raw, "recv_at": format_rfc3339_micros(wall_ns), "mono": mono_ns}).to_string();
        self.send(prefix, wall_ns, Payload::Text(text));
    }

    /// `build` runs only when journaling is on, so a disabled journal costs nothing to call.
    pub fn record(
        &self,
        prefix: &'static str,
        wall_ns: i64,
        build: impl FnOnce() -> serde_json::Value,
    ) {
        if self.tx.is_none() {
            return;
        }
        self.send(prefix, wall_ns, Payload::Text(build().to_string()));
    }

    /// Like `record`, but `build` runs on the journal thread.
    pub fn record_lazy(
        &self,
        prefix: &'static str,
        wall_ns: i64,
        build: impl FnOnce() -> serde_json::Value + Send + 'static,
    ) {
        if self.tx.is_none() {
            return;
        }
        self.send(
            prefix,
            wall_ns,
            Payload::Lazy(Box::new(move || build().to_string())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts `ok` bytes, then fails every write and flush, like a disk that fills up.
    struct FillsUp {
        ok: usize,
    }

    impl Write for FillsUp {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.ok == 0 {
                return Err(std::io::Error::other("no space left"));
            }
            let n = buf.len().min(self.ok);
            self.ok -= n;
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.ok == 0 {
                Err(std::io::Error::other("no space left"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_disabled_journal_never_builds_its_records() {
        let journal = Journal::disabled();
        journal.record("x-", 0, || panic!("built a record nobody will write"));
        journal.record_lazy("x-", 0, || panic!("built a record nobody will write"));
        assert!(journal.flush(Duration::from_secs(1)));
    }

    #[test]
    fn write_failures_are_counted_and_flush_waits_for_the_writer() {
        let dir = std::env::temp_dir().join(format!("pmbot-journal-test-{}", std::process::id()));
        // A healthy journal: flush returns once the lines are on disk.
        let journal = Journal::start(dir.clone()).unwrap();
        for i in 0..100 {
            journal.record("ok-", 0, || json!({"i": i}));
        }
        assert!(journal.flush(Duration::from_secs(5)));
        let text = std::fs::read_to_string(dir.join("ok-1970-01-01.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 100);
        assert_eq!(journal.write_errors.load(Ordering::Relaxed), 0);

        // The buffered writer only reaches the disk on flush, so the loss shows up there.
        let journal = Journal::start_with(dir.clone(), |_| FillsUp { ok: 10 }).unwrap();
        for i in 0..5 {
            journal.record("full-", 0, || json!({"i": i}));
        }
        assert!(journal.flush(Duration::from_secs(5)));
        assert_eq!(journal.write_errors.load(Ordering::Relaxed), 5);
        assert_eq!(journal.dropped.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
