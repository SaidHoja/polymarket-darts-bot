# Risk stress test: plan (2026-10-28)

Question: can a strategy, a hostile market or an operational failure push `pmbot` past the limits in its live configs? Every hypothesis below ends in a test that either breaks a limit or shows it holds.

Scope: `Risk` (`crates/pm-core/src/risk.rs`), exposure and PnL (`portfolio.rs`), the gate and loss checks in `core.rs`, and the live configs `config/pmbot-m3-live.toml` and `config/pmbot-two-phase-live.toml`. The current behaviour is in [docs/risk.md](../docs/risk.md); "plan 03" below is the original risk design, now removed (in git history at `acd3324:plans/03-risk.md`).

## Limits under test

| Limit | Default | m3 live | two_phase live | Enforced at |
| --- | ---: | ---: | ---: | --- |
| `max_position` | 20 | 6 | 8 | `risk.rs:284` (gate, new quote only) |
| `max_order_size` | 5 | 1 | 2 | gate |
| `max_market_loss` | $10 | $6 | $6 | gate `fits` |
| `max_account_loss` | $40 | $30 | $30 | gate `fits` with `account_other` |
| `max_orders_total` | 40 | 40 | 40 | gate |
| `daily_loss_limit` | $15 | $30 | $30 | `check_losses`, housekeeping |
| `market_loss_limit` | $5 | $2 | $2 | `check_losses`, housekeeping |
| `max_fills_per_market_per_min` | 10 | 10 | 10 | `on_fill` |
| `max_rejects_per_min` | 10 | 10 | 10 | `on_reject` |
| `max_dev_from_fair` / `book_band` | 0.16 / 0.10 | 0.16 / 0.10 | 0.16 / 0.10 | gate |

Already visible from the table: in both live configs `daily_loss_limit` ($30) equals `max_account_loss` ($30). The daily kill can only fire once realised losses have been recycled into new exposure, so it's a weaker backstop than plan 03 intended (plan 03 had $15 against $40).

## Invariants

Each test checks these after every core tick. A violation is a finding.

| # | Invariant |
| --- | --- |
| I1 | Per market: `market_worst` (position plus every active, in-flight and in-doubt order as filled, CAN at 0.50 included) ≤ `max_market_loss` |
| I2 | Sum of I1 over markets ≤ `max_account_loss` |
| I3 | \|position\| ≤ `max_position` in every market, also after fills on orders that were in flight or in doubt |
| I4 | Every order ≤ `max_order_size`, on tick, inside 0.01–0.99, within `max_dev_from_fair` of fair and inside the book band |
| I5 | Active orders ≤ `max_orders_total`; never a bid ≥ our own ask in the same market |
| I6 | Every order is GTD and post-only (quotes) |
| I7 | After the daily kill, no new order until the next UTC day, **including across a restart** |
| I8 | After a market loss pause, no new order in that market until an operator resume, including across a restart |
| I9 | Realised plus marked daily loss ≤ `daily_loss_limit` + one fill's worth of slippage |
| I10 | A paused or blocked market has no resting orders within one tick (plus the cancel round trip) |

## Method

Three harnesses, cheapest first. All live in the repo so they become regression tests.

1. **Gate unit and property tests** (`risk.rs` tests). Random `GateCtx` (position, other resting, `account_other`, buying power, fair, book) and random targets. Assert I1, I2, I4, I5 on what `gate` returns. Add `proptest` as a dev-dependency of `pm-core`.
2. **Core scenario tests** (`crates/pm-core/tests/`, new file `risk_stress.rs`, same helpers as `tick.rs`). Scripted event sequences through `Core::tick`, with an invariant checker run on every `TickOutput`. This is where an adversarial strategy and adversarial venue meet the real pipeline.
3. **Backtest stress** (`pm-backtest`). Real captures, the adversarial strategy, the `pessimistic` fill model plus a new `toxic` one, and synthetic shocks. Measures how far losses get before a limit stops them, not only whether an invariant breaks.

4. **Live trading tests** with real orders and a small loss budget ([section E](#e-live-trading-tests)). These cover what no simulator can: the venue's own behaviour (cancel on disconnect, self-match, position cost in REST, GTD timing), our real latency, and whether the limits fire on real fills.

### The adversarial strategy

A test-only `StrategyConfig::Adversary` (behind `#[cfg(any(test, feature = "stress"))]` so it can't be routed in live config). It implements `Strategy` and, depending on its mode, returns targets that try to break each gate check. Risk is supposed to be the only line of defence, so the adversary ignores `max_inventory`, skew and the price band.

| Mode | Targets every tick | Tries to break |
| --- | --- | --- |
| `max_size` | Both sides at 1000 contracts, 1 tick inside the touch | I3, I4, clipping in `fits` |
| `one_way` | Bid only, always, at the ask − 1 tick | I3 via repeated fills, I1 |
| `edge_price` | Bid at 0.99 / ask at 0.01 (and 0.00, 1.00, off tick) | I4, range check, CAN worst case |
| `fair_spoof` | Reports `fair` = whatever makes its quote pass (fair = quote price) | `max_dev_from_fair` relies on the strategy's own fair |
| `self_cross` | Bid ≥ ask, and bid above an existing in-doubt ask | I5 (gate only compares the two new quotes) |
| `churn` | Flips bid/ask price every tick | reject storm, order count, `PendingReplace` reservation |
| `many_markets` | `max_size` on every market at once | I2, account reservation ordering within a tick |

`fair_spoof` matters most: the gate checks `|px − fair|` where `fair` comes from `decision.fair`, so a strategy bug that reports a wrong fair disables the fat-finger check except for the book band.

### The adversarial venue

Extensions to the `tick.rs` helpers and the backtest `Sim`:

- **Toxic fills**: every resting order fills in full on the next tick, and the book then jumps 20¢ against us (and, for settlement scenarios, to 0 or 1).
- **Late/duplicate executions**: a fill arrives after its cancel was acked; the same fill arrives twice; a fill arrives for an order in `PendingReplace` at the old size.
- **Silent markets socket**: connected, no frames. Book unchanged while trades print elsewhere.
- **Missing private fills**: the venue position moves but no execution arrives (reconciliation is the only way to learn).
- **Venue position seed with `cost = None`**.

## Hypotheses

Each has where it lives, the attack, the test, and what would count as broken. Priority: P1 = could lose more than a limit allows in live today, P2 = weakens a limit, P3 = hygiene.

### A. Strategy-side attacks on the gate

| # | P | Hypothesis | Test | Broken if |
| --- | --- | --- | --- | --- |
| A1 | P1 | `max_dev_from_fair` uses the strategy's own `fair` (`risk.rs` gate, `core.rs:857`), so a wrong fair defeats the fat-finger check. Only `book_band` (10¢ beyond the touch) remains, and it's one-sided: bids are only bounded above, asks below. | `fair_spoof` + `edge_price` against a 40¢-wide book | An order more than 16¢ from an *independent* fair (venue mid) passes |
| A2 | P2 | The position limit checks only the new quote against the current position (`risk.rs:284`). In-doubt orders and orders outside the two quote slots (`other_resting`) aren't counted, so fills on them can overshoot `max_position`. | `one_way` with an in-doubt bid resting, then fill both | \|position\| > `max_position` |
| A3 | P2 | The self-cross check compares only the two new quotes (`risk.rs:298`), not `other_resting` or the resting order of a slot being modified. | `self_cross` with an in-doubt ask below the new bid | A new bid ≥ any of our resting asks reaches the venue |
| A4 | P2 | Risk-reducing quotes always pass (`worst(wb, wa) <= base`). A strategy that holds a long and posts an ask also posts a bid that is "reducing" in combination. Check that the bid/ask clip order (bid first) can't let a risk-increasing side through on the back of the other side's reduction. | Property test on `gate` over random positions and two-sided targets | I1 or I2 fails on the returned quotes |
| A5 | P2 | Account reservation within one tick: `worst` is computed at tick start and updated per gated market (`core.rs` `decide_and_act`). Markets not dirty this tick keep their tick-start figure. Check that a fill arriving in the same tick for a non-dirty market can't leave `account_other` low. | `many_markets` with fills landing on non-dirty markets in the same tick | I2 fails |
| A6 | P3 | Order count: `max_orders_total` is 40, but with 2 slots per market the real cap is markets × 2 plus in-doubt orders. | `churn` across 25 markets with in-doubt orders | Active orders > 40 |

### B. Market-side attacks (no strategy bug needed)

| # | P | Hypothesis | Test | Broken if |
| --- | --- | --- | --- | --- |
| B1 | P1 | **Book spoofing moves fair.** `spread.rs` fair is others' mid when the spread ≤ `max_spread` (0.40). In a thin darts book, one order narrowing the spread drags our quotes, and the fat-finger check moves with them (A1). Someone can walk our bid up, sell into it, and cancel. | Backtest: inject a spoof order that steps the best bid up 2¢ a second, a taker that hits our bid, then pulls the spoof. Repeat. Count contracts and loss before a limit trips | Loss reaches `market_loss_limit` before any limit reacts *to the pattern itself*; record how many cycles it takes |
| B2 | P1 | **Loss limits are blind on wide books.** `marks()` (`core.rs:562`) uses fair, else microprice only if spread ≤ 10¢. With no mark, `total_marked` (`portfolio.rs:214`) falls back to cost basis, i.e. PnL 0. A book that blows out after a leg change (exactly when we're picked off) hides the loss from both `daily_loss_limit` and `market_loss_limit`. | Toxic fill, then widen the book to 45¢ and leave it there | `check_losses` sees ~0 while the position is underwater by > `market_loss_limit` |
| B3 | P1 | **The mark is manipulable**, so a limit can be tripped or hidden by the same spoof as B1. Hiding a loss is the dangerous direction. | Hold a long, spoof the mid up, check marked PnL | Marked PnL > PnL at the true mid by more than the spread |
| B4 | P2 | **Leg-change gap risk.** The leg study (2026-10-28) shows the book moves ~4–6¢ around the score stamp and we see it 0.6 s late; worst-case loss bounds settlement loss, not one leg's gap. Check that `max_market_loss` with all quotes filling at the stale price stays within `market_loss_limit` + one leg's move. | Backtest on the 27 MODUS matches with the `toxic` model at every leg stamp | Per-market loss exceeds `market_loss_limit` by more than one fill |
| B5 | P1 | **Silent markets socket.** There's no staleness check on book frames, only on connection state and game polling. A connected socket that stops delivering leaves quotes resting at a stale fair. | Scenario: stop book events, keep trades/game events, move the true price | Quotes stay up for > `game_stale_s`, or a fill lands at a price > 16¢ from the true mid |
| B6 | P2 | **Fill-rate pause vs size.** 10 fills/min at size 1–2 is 10–20 contracts, more than `max_position`, so the position limit binds first. Confirm the fill-rate pause adds nothing and decide whether it should be lower. | `one_way` with toxic fills | Report only |
| B7 | P2 | **CAN / walkover at extreme prices.** Worst case includes settlement at 0.50, but a short at 0.05 (long NO at 0.95) loses 0.45 per contract on CAN. Check the 0.02–0.98 band plus `max_market_loss` keep that bounded. | `edge_price` with CAN settlement | Settlement loss > `max_market_loss` |
| B8 | P2 | **Early start.** MODUS matches often start before the scheduled time (leg study). `pre_match` relies on `game_start_ns` and game state to stop; pre-match GTD ends at start + 5 min. Measure how long pre-match quotes rest in play. | Backtest the 27 matches with early starts | Pre-match quotes rest > 1 s after the first live game frame |

### C. Accounting attacks

| # | P | Hypothesis | Test | Broken if |
| --- | --- | --- | --- | --- |
| C1 | P1 | **Venue seed without cost.** `on_venue_positions` seeds `cost = net × 0.5` when the venue omits cost (`portfolio.rs:183`). A long 6 bought at 0.90 is then seen as costing $3, so worst case and loss limits are understated by $2.40. | Seed with `cost: None`, then quote | `market_worst` < true worst case |
| C2 | P2 | **Missed fill.** If an execution is lost, the position is only corrected when two REST reads agree (`reconcile_positions_s` apart). Meanwhile the gate quotes against the old position. | Venue position +2, no execution; adversary `one_way` | \|true position\| > `max_position` before reconciliation adopts |
| C3 | P2 | **Duplicate / late fill.** A fill after a cancel ack, or the same `trade_id` twice. | Scripted | Position counted twice, or a fill after cancel is dropped |
| C4 | P2 | **Buying-power mirror unknown.** With `balance = None` the buying-power check is skipped (`buying_power.is_none_or`). Startup before the first balance read is unchecked. | Start without a balance frame and quote | Committed > real buying power; expect venue `Funding` rejects |
| C5 | P3 | Fees on resting orders aren't in worst case (makers pay none today). If a quote crosses (post-only failure), a taker fee lands outside the reserved worst case. | Scenario with an aggressor fill | Worst case after the fill > reserved figure by more than the fee |

### D. Operational and state loss

| # | P | Hypothesis | Test | Broken if |
| --- | --- | --- | --- | --- |
| D1 | P1 | **Restart resets every loss limit.** `killed_day`, `market_pnl_start`, the market loss pauses and `day_start_pnl` live only in memory (`Risk`, `Portfolio`). After a restart the daily baseline is the current marked PnL and the venue seed overwrites cost, so prior realised losses vanish. A crash loop, or an operator restarting to "clear" a kill, gets a fresh $30 each time. | Core test: trip the daily kill, build a new `Core` from the same venue snapshot | I7 or I8 fails (expected to fail today) |
| D2 | P2 | **`resume` all doesn't clear the daily kill** (`risk.rs` `resume` leaves `killed_day`), but plan 03 says "operator resume". Decide which is intended; today only a restart clears it (see D1). | Unit test | Report |
| D3 | P2 | **Hot reload isn't implemented**, though plan 03 says limits only go down via hot reload. Changing limits needs a restart, which hits D1. | Read-through | Report |
| D4 | P2 | **Missing plan 03 limits.** Not in `RiskConfig`: taker notional per hour, the Thursday maintenance window cancel, the "more pessimistic mark" for the daily check, the 2-resting-orders-per-market limit (structural today). | Read-through, then decide each | Report |
| D5 | P2 | Private socket drop with orders resting; process kill with orders resting; network pull (GTD backstop). | Paper drill, then one live drill at minimum size | Any order still open after cancel-all + REST check, or after GTD expiry |
| D6 | P3 | Reject storm: 10 rejects/min blocks orders for 30 s. `BookMoved` doesn't count. A `churn` strategy in a fast book can keep one market permanently paused via `max_book_moved_per_market_per_min`. | `churn` scenario | Report pause duty cycle |

### E. Live trading tests

Real orders on the venue. These spend a small amount of money on purpose, so they run under tight limits, a fixed budget and stop rules.

#### Budget and stop rules

- **Campaign budget: $10 total realised loss.** Stop the campaign and write up if it's reached. Per session: $3.
- **Size: 1 contract per order, always.** At most $1 at risk per contract.
- An operator watches every session, with the UI open (`--ui 127.0.0.1:8787`) and `venv/bin/python -m pmus cancel-all -y` ready in a second terminal.
- Before and after each test: `pmus balances`, `pmus positions`, `pmus orders`. Write the numbers in the results log below. Start every test flat with no open orders, unless the test says otherwise.
- Journals on (default `captures/`), plus `pmus record --private` in parallel as an independent record of what the venue saw.
- Not inside the Thursday 06:00–08:00 ET maintenance window (except L10), and not with any other process using the key.
- Abort a test on anything unexpected that isn't the thing being tested: cancel-all, flatten by hand if needed, write it down.

#### Stress config

A dedicated `config/pmbot-stress-live.toml` with limits small enough that a breach is cheap and the limits fire within minutes:

```toml
[markets]
types = ["darts_match_winner"]

[strategies.default]
kind = "two_phase"
size = 1.0
pre_half_spread = 0.10
live_half_spread = 0.15
max_inventory = 2.0

[strategies.off]
kind = "off"

# Route to one or two chosen markets per session; everything else off.
[[routes]]
slug = "<chosen slug>"
strategy = "default"

[[routes]]
slug = "*"
strategy = "off"

[risk]
max_position = 2.0
max_order_size = 1.0
max_orders_total = 6
max_market_loss = 1.5
max_account_loss = 3.0
daily_loss_limit = 1.0
market_loss_limit = 0.30
```

Individual tests change one limit at a time from this base, noted in each test.

#### Tests

Ordered cheapest and safest first. "Cost" is the expected loss, not a cap.

| # | Tests | Procedure | Pass if | Cost |
| --- | --- | --- | --- | --- |
| L1 | Graceful shutdown (D5) | Quote one pre-match market. SIGTERM with both quotes resting. Then repeat with SIGHUP and Ctrl-C | Exit code 0, `pmus orders` empty within 3 s, the log shows the REST check | ~$0 |
| L2 | Hard kill, GTD backstop | Set `pre_match_gtd_after_start_s` so expiry is ~2 min out (or use an in-play market with `inplay_gtd_s = 60`). `kill -9` with quotes resting. Poll `pmus orders` every 5 s | Orders gone at their GTD expiry, ± 2 s. Record the actual delay | ~$0, unless filled while orphaned (≤ $2) |
| L3 | Network pull, venue cancel on disconnect | With quotes resting, block the venue (`pfctl` rule on the venue hosts, or turn Wi-Fi off) for 30 s, then restore. Check from a second network (phone hotspot + `pmus orders`) whether orders survived | Records whether the venue sends `UNSOLICITED_CXL_REASON_CONNECTION_LOSS` (open question in plan 03). The bot must not quote again until it has resynced | ~$0–2 |
| L4 | Unknown order on the venue | While the bot runs, place a far-off post-only order by hand: `pmus order <slug> buy-yes 0.05 1 -y` | Bot cancels it within `unknown_order_s` (5 s) + one round trip, pauses that market 60 s, alerts | ~$0 |
| L5 | Out-of-band position, reconciliation | Bot running on market X. By hand, take 1 contract on X: `pmus order X buy-yes <ask> 1 --allow-take -y` | Bot adopts +1 on the second matching REST read (≤ ~120 s), then quotes with `max_position` and worst case counting it. Record how long it quoted against the wrong position (C2) | Spread on 1 contract, ~5–15¢ |
| L6 | Venue seed and its cost (C1) | Hold 1 contract bought at a known price (from L5). Restart the bot. Compare the seeded `cost` in the journal/UI with the real cost, and check whether REST positions include cost at all | Seeded cost = real cost. If REST gives none, C1 is confirmed live | ~$0 extra |
| L7 | Self-match behaviour | Bot quoting market X with a bid resting. By hand, sell 1 into our own bid with `--allow-take` | Records what the venue does (rejects, cancels the resting one, or trades). If it trades, the bot sees a fill and the venue shows net 0, so reconciliation has to fix it: check that it does | ~$0–0.10 |
| L8 | Market loss limit fires on real fills | Base config, in-play on one MODUS match. Wait for natural adverse fills | `market loss limit` pause and cancel within one housekeeping tick of crossing −$0.30. Realised plus marked loss on that market ≤ $0.30 + one fill's adverse move. Record the overshoot | ≤ ~$0.60 |
| L9 | Daily kill fires, then restart (D1 live) | Base config, two in-play markets, until the daily kill fires at −$1.00. Then restart the bot (same day, same config) | Kill: cancel-all and no new orders. **After the restart it must not quote**: expected to fail today, which confirms D1 with real money. Stop it as soon as it quotes | ≤ ~$1.50 |
| L10 | Wide-book blind spot (B2) live | Hold 1 contract in a market whose book is wider than 10¢ and wider than `max_spread` (thin pre-match or right after a leg). Compare UI expected PnL with the PnL at the venue's bid | Report how far the marked PnL and the true liquidation value differ, and whether `check_losses` saw it | Spread on 1 contract |
| L11 | Leg-change gap (B4 live) | `two_phase` in play, `market_loss_limit = 1.0`, one MODUS match start to finish | Per-market realised plus marked loss never passes `market_loss_limit` by more than one fill. Record every fill's +60 s markout against the leg study numbers | ≤ ~$1.50 |
| L12 | Early start (B8 live) | `pre_match` on MODUS matches that start before schedule | Time from the first live game frame to zero resting pre-match quotes. Fail if > 2 s | Possible adverse fill, ≤ $1 |
| L13 | Operator controls under load | In play, both sides resting on 2 markets: UI Pause, then `echo cancel-all \| nc -U run/pmbot.sock`, then Resume | Zero open orders within 1 s of each cancel (record the latency). No quote while paused | ~$0 |
| L14 | Maintenance window | Leave the bot running (no resting orders before it) into Thursday 06:00 ET | Records the market state changes the venue sends and that the bot doesn't quote until the first OPEN book after it (plan 03 wants a cancel 5 min before; it isn't implemented, D4) | ~$0 |

Not tested live: the reject storm, `Global Rate Limit Exceeded` and HTTP 429. Forcing those means sending bad or excess orders, which risks the key being throttled. They stay in the simulator (D6).

#### Results log

One row per run, in this file's follow-up write-up:

| Date | Test | Market | Config change | Balance before → after | Realised PnL | Pass/fail | Notes / journal file |
| --- | --- | --- | --- | --- | --- | --- | --- |

## Order of work

1. **Invariant checker + core harness** (`risk_stress.rs`): a `check(&Core, &TickOutput, &RiskConfig)` that asserts I1–I6 and I10. Run it across every existing `tick.rs` scenario first, as a smoke test.
2. **Cheap P1s that are pure core tests**: D1, C1, B2, A1 (with `fair_spoof`), B5. Each is under 50 lines.
3. **Adversary strategy** (feature `stress`), all modes, through the harness. Then the `gate` property tests (A2–A6).
4. **Backtest stress**: `toxic` fill model and the spoof injector, run `two_phase` and `pre_match` live configs plus `Adversary` over the October captures. Output per run: max per-market and account worst case seen, max realised loss per market, how many minutes a limit took to fire, and which limit fired first.
5. **Live tests, round 1 (no fixes yet)**: L1–L7 (venue behaviour, mostly free), then L8–L13. The point is to confirm the core findings with real money before fixing them. L1–L4 can run as soon as the stress config exists, alongside steps 1–4.
6. **Fixes**, each with the core test that failed.
7. **Live tests, round 2**: rerun every live test that failed, against the fixed build, with the same config.
8. Write up findings in `investigations/risk-stress-test-<date>.md`, same format as the leg study: summary, findings with numbers, fixes proposed and their tests.

## Likely fixes to evaluate (after the tests confirm the problem)

Not to be done before the tests show the problem:

- Persist risk state (`killed_day`, loss baselines, indefinite pauses, the day's realised PnL) to a small file under `run/`, and load it at startup (D1).
- Gate fat-finger checks against a core-owned reference price (venue mid or last trade with a staleness bound) as well as the strategy's fair (A1, B1).
- Mark with the worse of fair and the pessimistic side of the book (bid for longs, ask for shorts) when there's no tight mark, instead of cost basis (B2, B3).
- Book-frame staleness timer per market while a game is live, or while we have resting orders (B5).
- Count `other_resting` (in-doubt and slotless orders) in the position-limit check and self-cross check (A2, A3).
- When the venue gives no cost, seed the worst case as if bought at 1.0 (long) / sold at 0.0 (short) until a fill or REST history gives the real one (C1).
- Set `daily_loss_limit` below `max_account_loss` in both live configs (e.g. $15 against $30).
