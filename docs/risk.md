# Risk

How `pmbot` bounds what it can lose: what it measures, the limits it enforces, what stops it quoting, and what pulls orders when the bot itself can't. This describes the code as it is. Where it differs from the original risk design, this doc says so. Open questions and planned tests are in [the stress-test plan](../investigations/risk-stress-test-plan-2026-10-28.md).

## Principles

- **Worst-case loss, not VaR.** A binary contract settles at 0, 1 or 0.50 (cancelled match), so the most a position can lose is exact. Every exposure figure is the worst of those three outcomes.
- **Resting orders count as filled.** A quote is exposure the moment it's on the venue. Limits apply to position plus every resting, in-flight and in-doubt order filling in the worse direction.
- **Risk is the last line, not the strategy.** Strategies have their own guards (`max_inventory`, `max_spread`, price band in [spread.rs](../crates/pm-core/src/strategy/spread.rs)), but the risk gate assumes nothing about them and checks every target.
- **Clip before veto.** A quote that would breach a loss limit is cut to the largest size that fits, not dropped. Quotes that only reduce exposure always pass.
- **Fail flat.** When state is uncertain (feed down, order in doubt, position mismatch, game data stale) the bot cancels and stops quoting the affected markets until it's certain again.
- **Never rely on one cancel path.** Cancels go out from the core, then the watchdog, then the shutdown hook, then GTD expiry on the venue, then the out-of-process dead-man switch.

## Where it lives

| Piece | File |
| --- | --- |
| Limits, gate, pauses, kill switch | [`pm-core/src/risk.rs`](../crates/pm-core/src/risk.rs) |
| Positions, worst-case loss, buying-power mirror, daily PnL | [`pm-core/src/portfolio.rs`](../crates/pm-core/src/portfolio.rs) |
| Limit values (`RiskConfig`) | [`pm-core/src/config.rs`](../crates/pm-core/src/config.rs) |
| Wiring: triggers, quotability, account reservation, housekeeping | [`pm-core/src/core.rs`](../crates/pm-core/src/core.rs) |
| Watchdog, shutdown cancel-all, send gate | [`pm-live/src/main.rs`](../crates/pm-live/src/main.rs), [`gateway.rs`](../crates/pm-live/src/gateway.rs) |
| Risk state file (survives restarts) | [`pm-live/src/risk_file.rs`](../crates/pm-live/src/risk_file.rs) |
| Out-of-process cancel-all (CLI and Lambda dead-man) | [`pmus/deadman.py`](../pmus/deadman.py) |

Risk sits inside the core tick ([architecture](architecture/architecture.md)): **react** applies urgent risk actions (cancel market, cancel all, pause) before strategies run, then **gate** clips or vetoes each strategy's targets before the order manager sees them.

## Exposure accounting

Positions come from our own fill accounting (`Portfolio::on_fill`). Venue position reads are a cross-check, not the source.

`worst_case_loss(position, resting)` takes three fill scenarios (nothing fills, every bid fills, every ask fills) and, for each, settlement at 0, 0.50 and 1. The result is the largest loss among them.

| Holding | Worst-case loss |
| --- | --- |
| Long C at average cost p | C · p |
| Short C at p | C · (1 − p) |
| Resting bid C at p | adds C · p if it fills |
| Resting ask C at p (opening a short) | adds C · (1 − p) if it fills |

Per market (`Core::market_worst`), active orders count at their confirmed size *and* at a pending modify's requested size; the worse of the two is used, since a modify in flight can land either way.

Account exposure is the sum over markets. The venue only checks collateral per market, so total resting exposure could exceed the balance there. We don't allow that: the gate counts every market's orders against one shared buying-power figure.

**Buying-power mirror.** The venue's buying power is read over REST (`reconcile_balance_s`). Between reads, each fill moves the local figure by the change in the position's worst case. This avoids the venue auto-cancelling unfunded orders after a fill.

**Reconciliation.** Venue positions are read every `reconcile_positions_s`. The first read seeds positions. A later difference is adopted only when two reads in a row show it with the local position unchanged, because a read can include a fill whose execution hasn't arrived yet. An adopted difference pauses the market for `market_pause_s` and cancels its orders.

**Marking for the loss limits** (`Core::loss_marks`) is pessimistic. An open position is marked at the worse of the strategy's fair and what others would pay to take it off us now: their best bid for a long, their best ask for a short, with our own orders taken out of the book. With only one of the two, that one is used; with neither, the last trade. So a wide or one-sided book shows the loss instead of leaving the position at cost. A position with no fair, no exit price and no trade is left out of the per-market check and marked at cost in the daily total.

**Daily PnL** is measured from the first mark after venue positions are seeded, so positions inherited at startup don't count as the day's PnL. Loss carried in from earlier in the same UTC day (see [Restarts](#restarts)) is added on top.

## Limits

All in `[risk]` in the config. Defaults are sized for a $100 account; live configs are tighter.

| Limit | Default | m3 / two_phase live | Checked | When hit |
| --- | ---: | ---: | --- | --- |
| `max_position` (net contracts per market) | 20 | 6 / 8 | Gate | Size cut so the fill can't pass it; only the reducing side quotes at the limit |
| `max_order_size` (contracts) | 5 | 1 / 2 | Gate | Size cut |
| `max_market_loss` (worst case, position + orders) | $10 | $6 | Gate | Size clipped to fit |
| `max_account_loss` (worst case, all markets) | $40 | $30 | Gate | Size clipped to fit; later markets in the tick get what's left |
| Buying power (venue figure, mirrored) | — | — | Gate | Size clipped to fit |
| `max_orders_total` | 40 | 40 | Gate | New orders vetoed; working ones kept |
| `max_dev_from_fair` | 0.16 | 0.16 | Gate | Quote vetoed (fat finger) |
| `book_band` (beyond the venue touch) | 0.10 | 0.10 | Gate | Quote vetoed (fat finger) |
| `daily_loss_limit` (realised + marked, UTC day) | $15 | $15 | Housekeeping | **Kill:** cancel all, no quoting until the next UTC day |
| `market_loss_limit` (since the session first saw the market) | $5 | $2 | Housekeeping | Cancel the market, pause it until an operator resume |
| `max_fills_per_market_per_min` | 10 | 10 | On fill | Cancel the market, pause `fill_pause_s` (300 s) |
| `max_rejects_per_min` (all markets) | 10 | 10 | On reject | Block all new orders `reject_pause_s` (30 s) |
| `max_book_moved_per_market_per_min` | 5 | 5 | On reject | Cancel the market, pause `market_pause_s` (60 s) |

Config loading rejects a `daily_loss_limit` that isn't below `max_account_loss`, so the daily kill fires before the whole account budget is at risk.

Changing a limit needs a restart. There is no hot reload. The loss state survives the restart ([Restarts](#restarts)).

Two more `[risk]` settings block quoting rather than limit size: `book_stale_after_trade_s` (10 s) and `book_stale_live_s` (90 s). See the next section.

## The gate

`Risk::gate` runs on every market the strategy wants to quote, every tick, in this order:

1. **Price sanity.** On the market's tick, inside 0.01–0.99.
2. **Fat finger.** Within `max_dev_from_fair` of the strategy's fair, and bids no higher than best ask + `book_band`, asks no lower than best bid − `book_band`.
3. **Size.** Capped by `max_order_size` and by the room left under `max_position` after the fill, rounded down to `min_qty`.
4. **Self-cross.** If our bid ≥ our ask, the ask is dropped.
5. **Order count.** New orders beyond `max_orders_total` are vetoed; a side already working is kept.
6. **Loss and funding.** Each side, bid first, is binary-searched down to the largest size where the market's worst case ≤ `max_market_loss`, the account's ≤ `max_account_loss`, and the collateral the orders need beyond their positions, across all markets, ≤ buying power. A side that doesn't increase the worst case passes untouched.

The core runs markets one at a time and adds each gated market's worst case to the account total before gating the next, so two markets can't both claim the same headroom in one tick. Urgent and in-play markets are gated first.

Every veto or clip is journalled as a `gate` note with its reason. Nothing is retried; the strategy's next decision is gated afresh.

All quotes are post-only and GTD. The bot never sends market orders.

## When a market isn't quotable

Before gating, `Core::not_quotable` blocks a market (its quotes are cancelled and the reason journalled as `not_quoting`) if any of these hold:

| Reason | Clears when |
| --- | --- |
| Market delisted | Relisted |
| Markets socket down or book not seeded | Reconnected and reseeded |
| Private stream not ready | Reconnected and resynced |
| Market state not OPEN, match ended or cancelled | Never for that match |
| Book stale: a trade with no book after it for `book_stale_after_trade_s` (10 s), or, while live, no book for `book_stale_live_s` (90 s) | A book arrives |
| Game state stale (no poll for `game_stale_s`, 5 s) while live | Polling recovers |
| An order in this market is in doubt | Order manager resolves it |
| Score orientation check failed for the league (in play) | Restart (this result isn't saved) |
| Operator pause (all markets) | Operator resume |
| Operator pause (one market) | Operator resume; survives a restart |
| Daily loss kill | Next UTC day; survives a restart, and `resume` doesn't clear it |
| Market loss limit | Operator resume; survives a restart |
| Reject storm, timed market pause (fill rate, rejects, mismatch, unknown order) | Pause expires, or operator resume |

A timed pause never shortens one already in force, and an indefinite one (operator, market loss limit) stays until resumed.

**Stale books.** The venue sends a book only when it changes, so a quiet book is normal: in-play gaps reached 51 s in the 10-28 captures. A trade with no book after it isn't normal; in those captures the next book followed every trade within 5 s. Either condition means the market's book feed has stopped while the socket stays up. Housekeeping notes `book_stale`, pulls that market's quotes, and re-decides it when books resume. Setting either threshold to 0 turns that check off.

## Event-driven triggers

Risk actions fire from events, not just from the gate:

| Trigger | Scope | Action |
| --- | --- | --- |
| Markets socket down | All markets | Cancel each market |
| Private socket down | All | Cancel all |
| Market leaves OPEN, delisted, tick/min size changed, or match ends/cancelled | Market | Cancel |
| Leg change | Market | Strategy must cancel or reprice this tick; quotes still in flight are cancelled |
| Position mismatch confirmed | Market | Cancel, pause 60 s |
| Unknown order on the venue | Market | Cancel it, pause 60 s (could be another process on the key) |
| Reject `BookMoved` (post-only would cross) | Market | Counted; over the per-market limit → cancel, pause |
| Reject `Transient` (`Global Rate Limit Exceeded`) | All | Strategies told the exchange is slow for `slow_exchange_s` |
| Reject `OurBug` (invalid order) | Market | Pause, refresh market params |
| Reject `MarketUnavailable` | Market | Cancel, pause |
| Reject `Funding` | Market | Pause `reject_pause_s`, re-read balance |
| Reject `Fatal` (auth) | All | Stop all order sending until restart |
| Operator `pause` / `resume` / `cancel-all` | As given | Immediate; `cancel-all` also pauses everything |

Rejects other than `BookMoved` also count toward the account-wide reject storm limit.

## Backstops outside the core

In order of how far things have to go wrong before each one matters:

1. **Send gate** (`gateway.rs`). When halted (shutdown or watchdog emergency), queued creates and modifies are withdrawn; cancels still go out.
2. **Watchdog** (`pm-live` main). If the core thread hasn't ticked for 2 s with input waiting, or has exited, it halts sends and runs cancel-all over REST, confirming with an open-orders read and retrying with backoff up to 30 s. Sends resume if the core recovers. It runs on its own OS thread, with its own Tokio runtime and HTTP client, so a stuck main runtime or a wedged connection can't stop it.
3. **Shutdown hook.** Ctrl-C, SIGTERM and SIGHUP wait up to 2 s for creates already sent, then cancel-all and confirm, three rounds.
4. **GTD on every order.** Pre-match quotes expire `pre_match_gtd_after_start_s` (300 s) after the scheduled start; in-play quotes after `inplay_gtd_s` (120 s), refreshed while still wanted. This covers `kill -9`, a lost network and a dead host.
5. **Dead-man switch** (`pmus/deadman.py`). The same cancel-all-and-verify as the shutdown hook, run from `pmus cancel-all` or as a Lambda independent of the bot's host ([AWS deployment](potential-improvements/aws-deployment.md)).

## Restarts

Restarting must not reset the loss limits. Otherwise a crash loop, or an operator restarting to clear a kill, would get a fresh budget each time. `pm-live` keeps `RiskState` in a file: `--risk-state`, default `run/risk-state-<mode>.json`, so paper and live have separate files. `none` disables it.

| Saved | Restored when |
| --- | --- |
| The day's PnL and whether the daily kill fired | Same UTC day |
| Per-market losses towards `market_loss_limit` (losses only, not gains) | Saved today or yesterday |
| Indefinite market pauses (operator, market loss limit) | Always, until an operator `resume` |

Timed pauses, rate windows and score-orientation results aren't saved; they last minutes or are re-checked.

The core offers the state after any tick with an alert or admin command, and once a second. A separate thread writes it as a temp file and then a rename. Shutdown writes it last. An unreadable file stops startup rather than silently clearing the limits. Deleting the file while the bot is stopped starts the day over.

## Known gaps

From the [stress-test plan](../investigations/risk-stress-test-plan-2026-10-28.md), not yet fixed or tested:

- **Fat-finger check trusts the strategy's fair.** A wrong fair moves the allowed band with it; only `book_band` remains independent (A1). The pessimistic loss mark doesn't depend on fair alone, but the gate does.
- **Position limit and self-cross look only at the two quote slots,** not at in-doubt or other resting orders (A2, A3).
- **Venue seed without cost** assumes 0.50, understating worst case for positions bought far from it (C1).
- **The daily kill can't be cleared by an operator.** `resume` leaves it in place, and now a restart does too; only the next UTC day or deleting the state file clears it (D2). This may be what we want, but the original design said "or operator resume".
- **Not implemented from the original design:** taker notional per hour, cancel before the Thursday maintenance window, per-event exposure.
