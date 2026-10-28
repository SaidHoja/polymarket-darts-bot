# `pm-core`

`pm-core` is the trading core: market and game state, strategies, portfolio accounting, risk, and the order state machine, behind a single synchronous entry point. It performs no I/O and reads no clocks. Time comes in as a `Clock`, inputs as `pm-types` events, and outputs leave as commands. That lets [`pm-live`](pm-live.md) and the backtester drive the same code. Its only runtime dependencies are `pm-types`, `serde`, and `serde_json`; `pm-venue` and `toml` are dev-dependencies for tests and benches. See [the repository overview](architecture.md) for where it sits.

## Modules

| Module | Owns |
| --- | --- |
| [`core`](../../crates/pm-core/src/core.rs) | `Core`, `Clock`, `TickOutput`; the tick loop and per-market records |
| [`config`](../../crates/pm-core/src/config.rs) | `Config` and its sections; strategy profile routing; validation |
| [`market_data`](../../crates/pm-core/src/market_data.rs) | Per-market books, trades, provisional overlay, sweeps, `BookView` |
| [`game`](../../crates/pm-core/src/game.rs) | `GameStore`: per-event match state, change detection, poll staleness |
| [`strategy`](../../crates/pm-core/src/strategy/mod.rs) | The `Strategy` trait, `MarketView`, `Decision`, `StrategyConfig`, `Off` |
| [`strategy::spread`](../../crates/pm-core/src/strategy/spread.rs) | The market makers: `pre_match`, `full_match`, `two_phase`, `two_phase_vwap` |
| [`strategy::latency_probe`](../../crates/pm-core/src/strategy/latency_probe.rs) | Diagnostic distant quotes for [latency measurement](../latency-probe.md) |
| [`portfolio`](../../crates/pm-core/src/portfolio.rs) | Positions, cash, fees, marks, worst-case loss, fill log, venue reconciliation |
| [`risk`](../../crates/pm-core/src/risk.rs) | Pauses, kill switches, reject/fill reactions, the quote gate |
| [`orders`](../../crates/pm-core/src/orders.rs) | `OrderManager`: quote slots, target diffing, lifecycle, reconciliation, timeouts |
| [`snapshot`](../../crates/pm-core/src/snapshot.rs) | Read-only `Snapshot` and `MarketDetail` for the operator UI |

## Public interface

```text
Core::new(Config)
Core::tick(Clock, Vec<Event>) -> TickOutput
Core::next_deadline() -> u64               // when to wake with no input
Core::snapshot(Clock) / market_detail(slug, Clock)
Core::status() -> String
```

`Clock` has two fields: `mono` (monotonic ns, process-relative live and simulated in backtests) for durations and timeouts, and `wall_ns` for GTD expiries, scheduled starts, and the UTC day. `TickOutput` carries the commands to dispatch, `Derived` notes (a kind, an optional slug, and detail text, for the journal and log), the decisions of markets evaluated this tick, private-execution acknowledgements for the shell's latency correlation, and latency samples.

All state lives in ordered maps (`BTreeMap`/`BTreeSet`) and nothing reads a clock, so the same initial state, clocks, and event sequence always produce the same output.

## The tick

`Core::tick` runs these steps, using a per-tick scratch `Tick` that collects dirty, urgent, input-updated, filled, and must-act market sets, pending risk actions, commands, and notes:

1. **Drain.** Stably sort events by `Event::priority()`: control, feedback, game, trades, books.
2. **Update.** Route each event to its owner. Books are truncated to `markets.book_depth` levels (default 20) before `MarketData` sees them. Order events go to the `OrderManager`, whose `OmEvent`s update the portfolio and risk. Discovery lists add, update, or deactivate markets, with a new strategy instance per added market. Connection changes unseed books or put orders in doubt, and admin commands set pauses.
3. **Housekeeping** (every `housekeeping_ms`, default 250). Order timeouts; GTD refresh for quotes still wanted; periodic position and balance queries; daily and per-market loss checks; pruning of terminal records older than 600 s; dropping markets delisted for an hour that have no active orders and a flat position (with their decision, risk counters and game), so a long session doesn't keep every market it ever saw. Markets are re-decided only when a strategy's `wake_at` passed, a risk block expired, game polling went stale or recovered, or the UTC day rolled, plus a full re-decide every `redecide_all_s` as a backstop.
4. **React.** Turn the accumulated `RiskAction`s into commands: at most one cancel-all, de-duplicated per-market cancels, balance queries, market refreshes, and alerts.
5. **Decide, gate, act.** For each dirty market (urgent first, then in-play, then by slug), build a `MarketView`, call the strategy, gate the quotes through risk with the account reservation, and diff the approved targets against working orders.
6. **Return** the `TickOutput`. The shell dispatches commands before journaling.

A market that the core does not know, or that is not dirty, costs nothing in step 5. Touch-based strategies re-decide when the touch changes; `two_phase_vwap` also re-decides on accepted book updates below the touch, because those can change its fair value.

## Market data

`MarketData` treats each book frame as a complete, atomic replacement. Frames older than the current book, or with any `BookFrame::problems`, are rejected and reported as `MdChange::Stale` or `Invalid`. Other changes are `Seeded`, `TouchChanged`, `StateChanged`, `MissedVolume`, and `Sweep`.

Trades are events: deduplicated by ID (last 512 kept), grouped into `Sweep`s by trade time and taker side (last 64 kept), and applied as a provisional overlay that takes liquidity from the passive side until a book with `transactTime ≥ tradeTime` confirms them. The venue's cumulative `sharesTraded` is compared with trades received, and a shortfall that persists across two books is reported as missed volume.

`BookView` is what strategies and the gate read. It offers best levels, microprice, dust-filtered touch and microprice, and a depth-limited book VWAP. Before calling the strategy, the core removes its own live, pending-cancel, and pending-replace quantities from the provisional view, so the strategy sees others' liquidity only. It removes them where the book shows them, not where the order manager has them now: executions usually arrive before the book that includes them (median ~46 ms in the 10-29 UTC captures), so the order manager logs each new, replace, cancel and expiry with its exchange `transactTime` (`OrderManager::unshown`) and drops it once a book at or after that time arrives. Until then a replaced order still counts at its old price and a new one not at all. Without this, the book's copy of our old order read as someone else's, the strategy repriced on the ack, and the next ack did it again: the first live `two_phase_vwap` run sent 28.7k modifies in 25 minutes. Fills are left to the provisional trade overlay.

The UI's VWAP uses the latest accepted venue book, including our orders, and weights the best `markets.vwap_depth` price levels on each side by contracts (default 3; 0 includes all retained levels). Its quantity is the contracts included in that calculation. `markets.book_depth` independently limits the book retained by the core (default 20 per side); reducing the VWAP depth preserves the deeper book for other uses. Empty books have no VWAP, while short or one-sided books use the levels available. There is no dust or price-distance filter.

## Games

`GameStore` keeps the latest `GameFrame` per event and reports `GameChange`s: `LegChanged` (including corrections that lower a score), `Live`, `Ended`, `Cancelled`, and `UnknownPeriod`. Polls older than the stored state are ignored. A leg change marks the event's markets urgent and must-act, so in-flight quotes there are cancelled if they can't be repriced in the same tick. Ended or cancelled matches cancel the market. Staleness (`game_stale_s`, default 5 s without a successful poll) only matters while some match is live, and blocks quoting in live markets.

## Strategies

A strategy decides what to quote in one market. Each market owns its own boxed instance, so per-market state needs no keying.

```rust
trait Strategy {
    fn decide(&mut self, view: &MarketView) -> Decision;
    fn fair(&self) -> Option<f64>;
    fn quote_lifetime_s(&self) -> Option<u64> { None }
    fn input_driven(&self) -> bool { false }
    fn uses_book_depth(&self) -> bool { false }
    fn diagnostic_reason(&self) -> Option<&'static str> { None }
    fn wake_at(&self, params: &MarketParams) -> Option<i64> { None }
}
```

`MarketView` is darts-specific by design: market parameters, league configuration and score-orientation status, others' book, last trade, game state, position, working bid and ask, whether the market is quotable, and per-tick flags (new input, filled, slow exchange). `Decision` carries fair value, the strategy's `Phase` (`PreMatch`, `StartWindow`, `BetweenLegs`, `InLeg`, `MatchPoint`, `Closed`), an optional bid and ask, and the result of an orientation check, if one completed. A strategy never sees raw frames, order IDs, or the network, and risk can veto anything it returns.

`StrategyConfig` is a `kind`-tagged enum. Its variants are `pre_match`, `full_match`, `two_phase`, `two_phase_vwap`, `off`, and `latency_probe`, and each config struct rejects unknown fields. Route overrides (`set = { … }`) are applied through `with_overrides`, which also rejects unknown parameter names. Adding a strategy means adding a module with its config and `Strategy` impl, then a variant and a `build` arm.

The market makers in `spread` share one quoting routine. Fair value is others' mid for `pre_match`, `full_match`, and `two_phase`. `two_phase_vwap` uses others' top-three-level book VWAP instead, after provisional trades and removal of our resting orders; its trading depth is fixed at 3 regardless of `markets.vwap_depth`. Both sides must exist, and a market whose spread is wider than `max_spread` is not quoted. Quotes sit a half-spread either side of fair, are rounded outward to the tick, and are kept strictly inside the opposite touch so post-only orders don't cross. An optional inventory cap quotes only the reducing side, and an optional skew shifts both quotes against the position. `wake_at` reports their time boundaries (stop or widen before the start), so housekeeping can re-decide at exactly that moment. The VWAP variant shares `TwoPhaseConfig`; [its live config](../../config/pmbot-two-phase-vwap-live.toml) copies the mid-based live profile's widths, timing, inventory, risk and execution settings. `latency_probe` quotes distant, minimum-size orders only on new input, with a 15-second lifetime, and stops permanently after any fill or position.

## Configuration

`Config` has `markets`, `strategies` (named profiles), `routes`, `risk`, `execution`, and `leagues` sections. Every field has a default, so a partial TOML file works, and unknown keys are errors, so a typo cannot silently fall back to a default. `strategy_for(params)` returns the first route whose matchers (`slug`, `event`, and `league` with `*` globs, plus market type) all match, with its overrides applied, or the `default` profile, which is built in if not configured. `validate()` resolves every profile and route at startup and rejects `nan`/`inf` anywhere in the config (TOML accepts both). At runtime, a route that matches none of the listed markets produces a `route_unused` note.

`ExecConfig` holds the timing knobs the core uses (ack, cancel, and unknown-order timeouts; reconciliation intervals; housekeeping and re-decide intervals; GTD lifetimes and refresh lead) plus two that only the live shell reads: `rate_limit_enabled` and `core_spin_us`. `LeagueConfig` carries `legs_to_win`, `hold_rate`, and `orientation_verified` per league slug.

## Portfolio

`Portfolio` keeps one `Position` per market (signed YES contracts and net cash) and computes PnL at settlement values and at a mark. Fills from the order manager are the primary source. When the venue doesn't report a fee, the core computes a taker fee from the market's fee coefficient. The first full REST position snapshot is adopted as the starting state. Later snapshots are compared. A difference of at least `min_qty` is first held (`unconfirmed`, noted `position_differs`) and positions are read again after 3 s; only if the second read shows the same local and venue numbers is it adopted as a reconstructed fill at the mark and reported as a mismatch, which pauses and cancels that market. This stops a fill that the read saw before the private socket delivered it from being counted twice.

Settlement: `Portfolio::settle(slug, px)` closes the position at the settlement price. The price comes from `stats.settlementPx` on an expired book (`BookFrame.settlement`) or from `Event::Settlement`, the answer to `Command::QuerySettlement` (`GET /v1/markets/{slug}/settlement`). A private position update with `resolution` set (venue entry type `LEDGER_ENTRY_TYPE_RESOLUTION`) marks the market as resolving and asks REST, every 30 s until it has a price. The positions reconcile skips markets waiting for a settlement, so a resolution is never booked at a guessed price. Other private position messages are journaled as observations only.

`worst_case_loss(position, resting)` evaluates YES, NO, and 0.5 (cancelled match) settlement with all bids filled, all asks filled, or neither. Marks use the strategy's fair value, falling back to the venue microprice. The daily PnL baseline is the first mark of each UTC day. A bounded fill log (256 entries) serves the UI.

## Risk

`Risk` is separate from strategies and has three roles.

- **Eligibility.** `Core::not_quotable` combines market listing and open state, a seeded book, an up private stream, fresh game polling when live, no in-doubt orders, score orientation not failed for the league, and `paused_reason` (operator pause, daily kill, market pause, order block).
- **Reactions.** `on_reject` maps each `RejectClass` to actions: book-moved storms pause the market; `Transient` marks the exchange slow; `OurBug` refreshes market parameters; `MarketUnavailable` cancels the market; `Funding` queries the balance; `Fatal` stops order sending. A reject storm pauses new orders. `on_fill` enforces the per-market fill rate. `check_losses` applies the daily limit (cancel-all until the next UTC day) and the per-market limit.
- **The gate.** `gate(ctx, bid, ask)` vetoes off-tick or out-of-range prices, prices too far from fair (`max_dev_from_fair`), and prices outside the book band. It clips size to `max_order_size` and the position limit, rounds down to `min_qty`, and enforces order count, buying power, and worst-case market and account loss. It returns approved quotes, veto reasons, and the market's resulting worst case. The core adds that to the account reservation before gating the next market, so markets processed earlier in a tick get scarce capacity first.

## Order manager

`OrderManager` owns every order record, keyed by `Token`, and one slot per `(market, QuoteSide)`. Records without a slot still count toward limits and exposure. Finished records are kept for 600 s for late feedback, but an index of the active ones (overall and per market) means `active()`, `active_in()` and the order manager's own scans only visit working orders.

- **States:** `PendingNew`, `Live`, `PendingReplace`, `PendingCancel`, `InDoubt`, and `Done(reason)`, where the reason is `Filled`, `Canceled`, `Rejected`, `Expired`, `Withdrawn`, or `Gone`. `InDoubt` counts as fully live for risk.
- **Diffing:** `sync_side` places into an empty slot, cancels when the target disappears, and leaves in-flight records alone; their resolution re-marks the market dirty. It also leaves a live order alone when the price is less than `execution.reprice_min_ticks` from the target (default 2, so a one-tick move waits; 1 keeps only an unchanged price; the latency probe always uses 1) and the quantity is within a factor of two. Otherwise it modifies, unless the intent changed or the order is partly filled, in which case it cancels so the slot can be re-placed.
- **Feedback:** the private socket is the source of truth. A create `RestResult` only attaches a venue ID; executions drive transitions. An execution for an unknown ID is parked, attached to a pending order by matching fields after `unmatched_attach_s`, and declared unknown after `unknown_order_s`. A `REPLACE` carrying a new ID is adopted when exactly one modify is pending on that side. Execution IDs (last 4,096) and cumulative quantities prevent double-counting fills.
- **Reconciliation:** private-stream loss puts every working order in doubt. Snapshots and open-order reads retire missing orders as `Gone` and cancel unknown venue orders. Ack and cancel timeouts schedule `QueryOpenOrders` or `QueryOrder`. A cancel the gateway refuses is retried after `cancel_timeout_s`, then twice that, and after three refusals the order is looked up instead.
- Commands produced while handling feedback (for example, cancel-on-ack for an order no longer wanted) go to an outbox that the core drains twice per tick.

The core picks the intent from the position (a bid is `SellShort` when it reduces a short, an ask is `SellLong` when it reduces a long) and gives reducing orders `Priority::Reduce`.

## Snapshot

`Core::snapshot` builds a read-only `Snapshot` for the operator UI. It covers account health, current PnL (marked at the venue mid when the spread is at most 10¢, otherwise at the expected price), expected PnL (strategy fair), daily PnL, worst-case loss, one `MarketRow` per active or held market, and recent fills. `market_detail` adds a market's full book with our resting size per level, its orders, fills, recent sweeps, and the strategy's last targets. Nothing in the module mutates the core. The shell decides when to build snapshots.

## Tests and benches

Unit tests sit beside each module: numeric book handling, games, strategies, config parsing and routing, gate checks, reject storms, worst-case loss, and the order lifecycle (ack before REST result, fill/cancel races, deduplication, unmatched executions). [`tests/tick.rs`](../../crates/pm-core/tests/tick.rs) runs end-to-end ticks, events in and commands out. Criterion benches measure a full tick across many quoting markets ([`benches/tick.rs`](../../crates/pm-core/benches/tick.rs)) and individual components with realistic 20–45-level books ([`benches/components.rs`](../../crates/pm-core/benches/components.rs)): `cargo bench -p pm-core`.
