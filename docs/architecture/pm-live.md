# `pm-live`

`pm-live` builds the `pmbot` binary: the asynchronous shell around [`pm-core`](pm-core.md). It loads configuration and credentials, runs the feeds, queues their input for the core thread, dispatches the core's commands through the order gateway, and provides the operator surfaces (admin socket and web UI), journals, metrics, the watchdog, and shutdown. It decides nothing about trading; it moves events in and commands out, and measures how long that takes. See [the repository overview](architecture.md) for the end-to-end flow.

It depends on all three library crates plus `tokio`, `tokio-tungstenite`, `axum` and `maud` (UI), `crossbeam-channel`, `clap`, `toml`, `tracing`, and `libc` on Unix (thread priority).

## Modules

| Module | Responsibility |
| --- | --- |
| [`main`](../../crates/pm-live/src/main.rs) | CLI, startup, mode selection, core thread loop, watchdog, shutdown |
| [`feeds`](../../crates/pm-live/src/feeds.rs) | Discovery, game poller, markets socket, private socket |
| [`inbox`](../../crates/pm-live/src/inbox.rs) | Priority and conflating queues between feeds and the core thread |
| [`gateway`](../../crates/pm-live/src/gateway.rs) | Live order gateway (priority queue, rate budget, batching) and paper gateway |
| [`timing`](../../crates/pm-live/src/timing.rs) | Input-to-send and input-to-ack attribution per command |
| [`metrics`](../../crates/pm-live/src/metrics.rs) | Lock-free log-bucket latency and queue-length histograms |
| [`journal`](../../crates/pm-live/src/journal.rs) | Append-only JSONL writer thread |
| [`admin`](../../crates/pm-live/src/admin.rs) | Unix socket operator commands |
| [`ui`](../../crates/pm-live/src/ui.rs) | Optional HTMX web UI on its own thread |
| [`priority`](../../crates/pm-live/src/priority.rs) | Best-effort scheduling priority for the core thread |
| [`risk_file`](../../crates/pm-live/src/risk_file.rs) | Loads the risk state file at startup and writes it from its own thread |
| [`bench`](../../crates/pm-live/src/bench.rs) | Ignored-test benchmarks of the shell and a core-journal replay |

## Threads and tasks

| Where | What runs there |
| --- | --- |
| Tokio runtime, 2 workers | Feed tasks, gateway and its per-batch send tasks, admin socket, metrics reporter, signal handling |
| `watchdog` OS thread (live) | Single-threaded Tokio runtime with its own `RestClient` (HTTP pool and 429 backoff, no proactive bucket) |
| `risk-file` OS thread (paper, live) | Writes the risk state file (temp file, then rename) |
| `core` OS thread | Waits on the inbox, ticks `Core`, sends commands, records metrics, publishes UI snapshots |
| Journal OS thread | Serializes and writes JSONL lines |
| UI OS thread (with `--ui`) | Single-threaded Tokio runtime serving `axum` |

Shared handles (`RestClient` with its `RateLimiter`, `Inbox`, `Metrics`, `Journal`) are passed as `Arc`s. The only lock the core thread takes in steady state is the status string, once a second.

## Startup and modes

`run` parses `Args` (`--config`, `--mode record|paper|live`, `--env-file`, `--journal-dir`, `--run-dir`, `--include-futures`, `--ui`, `--ui-allow-remote`), loads and validates the TOML `Config`, refuses a non-loopback `--ui` without the override, loads credentials and builds the signer, starts the journal, and creates one `RestClient` with a 15/s limiter (`REST_RATE`). It then spawns discovery, the game poller, the markets socket, and the metrics reporter.

- **`record`** also spawns the private socket, then drains the inbox every 10 s just to log counts. There is no core, gateway, admin socket, or UI.
- **`paper`** spawns the paper gateway, which seeds an empty snapshot, empty positions, a $100 balance, and a private-up event. It acknowledges creates, modifies, and cancels locally after 20 ms and never fills.
- **`live`** spawns the private socket, the real gateway, and the watchdog.

Paper and live both start the admin socket, the optional UI, and the core thread.

## Feeds

[`feeds`](../../crates/pm-live/src/feeds.rs) contains four long-running tasks sharing a `Ctx`: inbox, journal, REST client, signer, start instant, a `watch` channel of subscription slugs, a `Notify` for on-demand discovery, the market-type filter (`None` in record mode), and whether to forward unchanged game polls for latency probes. They parse, validate, timestamp, and journal; they do not decide.

- **`discovery`** pages darts events every 5 minutes (and futures with `--include-futures`), posts `Event::Markets`, and publishes the slug set. Failures keep the previous set and retry after 30 s. `RefreshMarket` from the core triggers an immediate run.
- **`game_poller`** polls at 1 s during live games, 5 s near a scheduled start, and 30 s otherwise. It forwards changed `GameFrame`s, keyed on score, period, live, ended, and update time, and signals `GamePolled` on every success.
- **`markets_ws`** subscribes in groups of 100 slugs, adds newly discovered slugs to the live connection, and pushes books and trades with their receive instants. It also records book and trade lag, excluding each market's subscribe snapshot.
- **`private_ws`** subscribes, buffers updates until the order snapshot's `eof`, fetches positions and balance over REST, then queues snapshot, account state, buffered updates, and `Connection Up` before entering its receive loop. A subscription error or an undecodable frame, during startup or afterwards, ends the connection, so the reconnect resubscribes and re-reads the snapshot and positions.

Both sockets ping every 5 s, treat 15 s of silence as dead, journal every raw frame, report `Connection Down` on exit, and reconnect with jittered exponential backoff capped at 10 s (30 s after a 401).

## Inbox

[`Inbox`](../../crates/pm-live/src/inbox.rs) separates inputs that must each be processed from inputs where only the latest value matters:

| Push method | Storage |
| --- | --- |
| `control` | Unbounded FIFO: connection, admin, discovery |
| `feedback` | Unbounded FIFO: executions, snapshots, REST results, reads |
| `book` | Latest frame per market; superseded frames counted in `books_conflated` |
| `game` | Latest frame per event |
| `trade` | FIFO capped at 10,000, at most 1,000 drained per pass; overflow counted in `trades_dropped` |

Both counters (`trades_dropped()`, `books_conflated()`) appear in the status line, `/metrics.json`, the metrics page and the metrics journal, and a warning is logged for any minute in which trades were dropped. Merged books are normal under load; dropped trades mean the core fell behind.

Every push rings a capacity-one doorbell, so bursts collapse into one wakeup. `wait_spinning(timeout, spin)` polls for the first `core_spin_us` before sleeping, trading CPU for wakeup latency. `drain_timed` returns events in priority order with their receive instants, plus how long the oldest one waited. `pending()` lets the watchdog tell an idle core from a stuck one.

## Core thread

Each iteration of the core loop:

1. Stores a heartbeat, then waits on the inbox until `Core::next_deadline()` (at most 1 s).
2. Drains, records inbox depth and queue wait, and lets `InputTiming` note the oldest input per market.
3. Ticks the core with a fresh `Clock` and records tick time.
4. Sends every command to the gateway channel as an `Outbound`, carrying the originating input's receive instant and the hand-off time.
5. Logs derived notes (gate reasons only when they change for a market) and journals the tick lazily, so serialization happens on the journal thread after commands are out.
6. Once a second, updates the status line and, if a UI page is being watched, builds and publishes a snapshot plus details for focused market pages.

`priority::raise_current_thread` asks for the user-interactive QoS class on macOS, or nice −10 on Linux (needs `CAP_SYS_NICE`), so a tick of tens of microseconds is less likely to land on an efficiency core or be preempted. Real-time scheduling is deliberately avoided.

## Gateway

[`run_gateway`](../../crates/pm-live/src/gateway.rs) is the only path for trading REST in normal operation. Commands enter a `BTreeMap` keyed by `(Priority, sequence)`. `Withdraw` removes a queued action and reports `RestOutcome::Withdrawn`; `RefreshMarket` pokes discovery. When a rate token is available, `take_batch` pops the highest-priority action and, for creates, modifies, and cancels, adds up to 19 more of the same kind, taking one extra token each with `try_acquire`. Each batch runs in its own spawned task with `Budget::Prepaid`, so priority governs dispatch order, not completion order.

Every action produces exactly one `RestResult` back to the core. HTTP 401 becomes `AuthFailed`, 429 becomes `RateLimited`, other 4xx become `GatewayRejected` with a reject class, and transport errors and 5xx become `Ambiguous`. A 429 or the "Global Rate Limit Exceeded" text gets one retry. Read commands become `OpenOrders`, `OrderLookup`, `Positions`, or `Balance` events.

## Operator surfaces

- **[Admin socket](../../crates/pm-live/src/admin.rs)** at `<run-dir>/pmbot.sock`: one text command per connection: `status`, `metrics`, `pause [slug]`, `resume [slug]`, `cancel-all`. Mutating commands go through `Inbox::control` as `AdminCmd`s; `ok` means queued.
- **[Web UI](../../crates/pm-live/src/ui.rs)** with `--ui <addr>`: server-rendered with `maud` and HTMX partials. Routes are `/`, `/market/{slug}`, `/metrics`, the JSON endpoints `/snapshot.json` and `/metrics.json`, and POSTs for pause, resume, cancel-all, and shutdown. The core publishes snapshots only while a page has polled in the last 5 s, and market details for at most 4 focused markets. POSTs need the per-process token header and a same-origin `Origin`. The metrics page reads `Metrics` directly, without involving the core.

## Journals and metrics

[`Journal`](../../crates/pm-live/src/journal.rs) writes one JSONL file per stream per UTC day: raw market and private frames in the `{"conn", "raw", "recv_at"}` capture format plus monotonic time, discovery, game, core ticks, and metrics windows. A 100,000-line bounded channel feeds the writer thread. When it is full, lines are dropped and counted rather than blocking trading. Write and flush failures are counted separately (`journal_write_errors` in the UI and `/snapshot.json`) and logged on the first and every 1,000th. `flush(timeout)` waits for the writer to drain the queue and flush every file; shutdown calls it. `record` builds its JSON only when journaling is on (it takes a closure); `record_lazy` also moves the building to the writer thread.

[`Metrics`](../../crates/pm-live/src/metrics.rs) keeps a since-start and a windowed histogram per `Stage`, using 256 log-spaced buckets per sign (about ±5% resolution) recorded with relaxed atomics. Stages follow a command's path: `book_lag`, `trade_lag`, `ws_rtt_*`, `queue`, `tick`, `core_busy`, `decide`, `gw_handoff`, `gw_wait`, `gw_spawn`, `gw_build`, `rest_*`, `ack`, `input_to_send`, `input_to_ack`, plus the queue-length stages `inbox_depth` and `gw_depth`. Windows are logged and journaled every 60 s, and the last 60 are kept for the UI. [`timing`](../../crates/pm-live/src/timing.rs) attributes each place, modify, or cancel to the oldest same-market input in its tick and closes `input_to_ack` when the core reports the matching private acknowledgement (live mode only). Field definitions are in [the metrics reference](../metrics.md).

## Watchdog and shutdown

The gateway and the shell share a `Gate` (`gateway.rs`): a shutdown flag, an emergency flag, and a count of creates and modifies in flight. While either flag is set the gateway answers queued Place/Modify with `Withdrawn`, does not send new ones, and does not retry. A request counts itself in flight before it checks the flags, so a create is either withdrawn or counted, never sent unseen.

In live mode, the watchdog checks every 500 ms from its own thread and runtime, with its own HTTP client, so a stuck main runtime or a wedged connection in the shared pool can't stop it. If the core heartbeat is more than 2 s old while the inbox has pending work (including `GamePolled`), or the core thread has ended, it sets the emergency flag and calls `cancel_everything` with backoff from 1 s up to 30 s until it is confirmed. It clears the flag when the core recovers.

`cancel_everything` waits up to 2 s for in-flight creates to finish, then runs up to 3 rounds of REST cancel-all, each followed by an open-orders read; it succeeds only when that read is empty.

Shutdown starts on Ctrl-C, SIGTERM, SIGHUP, the UI's shutdown button, or the core thread ending. The shell shuts the gate, queues a core cancel-all, runs `cancel_everything` (live mode), stops the core loop and waits up to 2 s for it, flushes the journal (2 s), and removes the admin socket. It exits with an error if the cancel was not confirmed or the core had died. When the markets socket disconnects, `Inbox::markets_down` drops that connection's queued books and trades before the `Down` event, and a markets subscription error ends the connection so it resubscribes.

## Risk state file

`--risk-state` (default `<run-dir>/risk-state-<mode>.json`, so paper never touches the live file; `none` disables) holds `pm_core::RiskState`: the UTC day, its PnL, whether the daily kill fired, per-market losses towards `market_loss_limit`, and indefinite pauses. It is loaded before the core starts and handed to `Core::restore_risk_state`; an unreadable file stops startup. The core thread offers `Core::risk_state` after a tick with an alert or admin command and once a second; `Saver` drops an unchanged state and never blocks the core (a full one-slot queue just waits for the next offer). When the core loop stops, it writes the final state and waits for the write. Deleting the file while the bot is stopped starts the day over.

## Tests and benches

Inline tests cover inbox conflation and timestamps, gateway batching and throttled dispatch, admin parsing, timing attribution, metric bucket accuracy, thread priority, and UI routes and guards (via `tower`). Because `pm-live` is a binary, its benchmarks are ignored tests in [`bench.rs`](../../crates/pm-live/src/bench.rs): `cargo test --release -p pm-live bench -- --ignored --nocapture --test-threads=1`. One of them replays a recorded `core-*.jsonl` journal through a fresh core and compares tick times with those recorded live.
