# Backtest data-quality review

Reviewed on 2026-10-28. Scope: the replay data path from `data-analysis/polymarket_us_darts_history.parquet` through `data-analysis/export_backtest.py` and `data-analysis/backtest/` into `crates/pm-backtest` (`data.rs`, `book.rs`, the quality gate in `main.rs`, and the parts of `sim.rs` that consume replay data), plus `data-analysis/strategy-research/`. The simulator logic itself was reviewed separately (BT-01 to BT-13, in git history at `acd3324:code-review/pm-backtest.md`). This one covers whether the input data supports the conclusions drawn from it. No application code or data was changed.

Severity follows the other reports: **P1** = high-impact control/accounting failure, **P2** = functional or methodology defect, **P3** = smaller correctness, observability or maintainability issue. No P1 finding was established.

## Summary

The row-level data is clean. Prices are always on tick, trades are always consistent with the rebuilt book, there are no duplicates and no crossed books. The problems are **coverage and continuity**, and the replay tooling does not measure them:

- The **dev split is a different and much worse dataset than the holdout**. In 24 of 71 dev markets the captured price ends on the opposite side of the settled outcome. In the holdout this happens in 0 of 154 settled markets (DQ-01).
- There are **85 feed reconnects**, each after a median of about 6 minutes of silence. Most post-reconnect snapshots disagree with the rebuilt book, but the audit skips exactly those snapshots (DQ-02).
- **31% of in-session re-snapshots disagree** with the rebuilt book even without a reconnect (DQ-03).
- The `--min-agreement` gate passes all 242 markets, so **nothing is ever excluded** (DQ-04).

## Data vs. tooling

The backtest has one data source: `data-analysis/polymarket_us_darts_history.parquet`. `export_backtest.py` copies its FIX match-winner rows (308 markets) into the per-market CSVs in `data-analysis/backtest/`. It re-orders them and removes exact duplicates, of which there were none. `pm-backtest` reads only those CSVs.

| Kind | Findings | Where the problem is |
| --- | --- | --- |
| **The captured data itself** | DQ-01 weak August coverage; DQ-02 reconnect outages; DQ-03 snapshots that disagree with the rebuilt book; DQ-05 captures that stop before the match; DQ-06 missing and 0.5 settlements; DQ-09 missing `event_time` on snapshots | The Parquet. The export passes these through unchanged. Code cannot repair them; it can only detect them, work around them, or exclude the affected data. |
| **The code that uses the data** | DQ-04 a filter that can't fail; DQ-07 a gap count that mixes quiet and outage; DQ-08 sequence numbers documented as able to find gaps; DQ-10 `extract.py` replaying differently; DQ-11 audit not saved | `export_backtest.py`, `pm-backtest`, `strategy-research/`. These fail to detect the data problems or handle them inconsistently. |

Where each number came from:

- **Read from the Parquet directly:** sequence resets, the venue-wide silences before reconnects, and the missing `event_time` on snapshot rows. These needed every FIX row across all markets, and the export keeps only darts match-winner markets.
- **Read from the exported CSVs:** everything per market (book rebuilds, trade checks, snapshot comparisons, coverage, settlement against last price).

## What was measured

The released `target/release/pm-backtest audit` was run for `dev`, `holdout` and `all` on the current export (308 markets, 242 with trades, 2,100,417 rows). Read-only Python scans then rebuilt the books independently from the CSVs and read the Parquet FIX columns directly for sequence and silence analysis. Scan scripts were kept in a session scratch directory and are not committed. The method for each number is stated in the finding that uses it.

`audit --split all` totals: 2,028,629 messages, 13,365 trades, `trade_agreement` 1.000, 1,001 snapshots (641 after reset), 36 snapshot mismatches (20 at touch), 0 crossed, 3 unknown changes, 46,371 gaps over 60 s.

### Checks that passed

These were checked over all 242 traded markets and found no issue:

| Check | Result |
| --- | --- |
| Prices off the market's tick, or outside (0, 1) | 0 |
| Add rows with size ≤ 0; book rows with empty side; an order ID changing side | 0 |
| Exact duplicate rows dropped by the exporter | 0 |
| Null `msg_seq`; rows before the first snapshot marker | 0 (every market starts with `M`) |
| Trades with no aggressor | 0 |
| Trades at a price with no resting level on the passive side (at message start) | 0 of 13,365 |
| Trades priced better than the touch; trade size larger than the level | 0 / 0 |
| Trades through the touch (multi-level sweeps within one message) | 3,163, all at a resting deeper level |
| Rebuilt book crossed after any message | 0 |
| `ev_ns` going backwards between messages | 5 messages in 5 markets |
| Receive lag `rx − ev` | median 10.8 ms, p99 88 ms, p99.9 228 ms, max 3.4 s |

FIX trades and book rows are internally consistent, so the L3 replay mechanics can be trusted inside continuous stretches of data.

## Findings

### DQ-01 — P2: The dev split is a different, lower-quality dataset than the holdout

**Locations:** `crates/pm-backtest/src/report.rs:15–27` (`DEV_END`, `HOLDOUT_START`, `split_of`); `crates/pm-backtest/src/main.rs:39–40`.

Split by the backtester's own `split_of` rule:

| Traded markets | dev | holdout |
| --- | ---: | ---: |
| Markets | 71 | 171 |
| Settled 0/1 / void 0.5 / no settlement | 71 / 0 / 0 | 154 / 5 / 12 |
| Markets with any trade after `game_start` | 34 (48%) | 155 (91%) |
| Share of trades after `game_start` | 10.7% | 43.1% |
| Median capture end, minutes after `game_start` | 7.0 | 18.6 |
| **Last captured trade on the wrong side of 0.5 vs. settlement** | **24 (34%)** | **0** |
| League mix (modus / pdcdarts) | 65 / 6 | 122 / 49 |

All 24 contradicting markets are in August. Typical cases: `aec-modus-reecol-scocam-2026-08-14` last trades at 0.44 three minutes into play and then settles 1. `aec-modus-michun-ryabra-2026-08-14` last trades at 0.93 fourteen minutes *before* the start and then settles 0. In the dev split, the period that decides settlement was mostly not captured.

**Impact:**

- Dev settlement PnL is mostly inventory carried through unobserved play. `Pnl::inventory` (`sim.rs:937–948`) will dominate, and its value is close to a coin flip per market.
- Dev markouts carry the last mid forward (a known limitation), so they measure prematch noise.
- Parameters tuned on dev are tuned on prematch modus quoting. The holdout is half in-play and has more pdcdarts. A dev-to-holdout performance change cannot be read as overfitting, because the inputs changed.

**Suggested fix:**

- Report per-split coverage (in-play trade share, capture end versus start) with every run.
- Add a coverage exclusion (see DQ-04).
- Consider calling the August data a "prematch-only" set instead of the development set for in-play strategies.
- If dev must stay as is, cut dev PnL into prematch and in-play segments so they are not mixed.

### DQ-02 — P2: Reconnect outages are invisible to the sim and skipped by the audit

**Locations:** `crates/pm-backtest/src/book.rs:205–223` (`reset` → `snapshots_after_reset`, no comparison); `crates/pm-backtest/src/sim.rs:757–771`.

`msg_seq` is a venue-wide FIX session counter, not a per-market one (4.36 M of 4.82 M consecutive FIX messages skip by more than 1, across all sports). A drop in it means a new session. The Parquet has **86 sequence resets**. 85 are mid-stream reconnects, and every one is followed by a snapshot within 50 messages. The venue-wide FIX silence before a reset is **65 s minimum, 305 s p25, 370 s median, 464 s p75**. Each reconnect is about six minutes of lost data for every market.

Per market, this shows up as 641 post-reset snapshots in 173 of the 242 traded markets. The audit counts them but does not compare them, on the grounds that "differences there are expected". Comparing them anyway:

| Re-snapshot after a reset | n | L2 differs | Touch price differs |
| --- | ---: | ---: | ---: |
| Prematch | 617 | 351 (57%) | 154 (25%) |
| In play | 24 | 12 (50%) | 12 (50%) |

**Impact:**

- For about six minutes before each of these snapshots, the sim's book (and so the core's view and the queue-ahead estimates for our orders) is stale, while the strategy keeps quoting.
- No trades are recorded in the outage, so our quotes cannot fill or be adversely selected while the true book has moved. The sim is optimistic about outage periods. It never sees the risk the live bot would face (a stale feed, then a jump).
- The 24 in-play reconnects are in 21 markets. Half of them moved the touch.

The backtest design wanted the sim to "pull quotes, as the live bot would on a stale feed"; it doesn't. This finding measures how big it is.

**Suggested fix:**

- Mark each pre-reset silence in the export: the reset is known, and so is the last message time.
- Have the sim withdraw quotes from the last pre-outage message until the post-reset snapshot.
- Compare post-reset snapshots in the audit, and report them separately as "outage drift" instead of discarding them.
- Optionally exclude a market's in-play segment when an in-play outage moved the touch.

### DQ-03 — P2: A third of in-session re-snapshots disagree with the rebuilt book

**Locations:** `crates/pm-backtest/src/book.rs:211–219`; `crates/pm-backtest/src/main.rs:117–126`.

Without any sequence reset, 36 of 118 in-session re-snapshots (31%) differ from the L3 book rebuilt from incremental rows, and 20 of those differ at the touch. Both the Rust audit and the independent Python rebuild agree on this. Across those snapshots, 67 order IDs vanished and 81 appeared without matching delete/add rows.

Most cluster in modus markets on 2026-10-06. Several markets re-snapshot after exactly 1,926.1 s of per-market silence, while the venue-wide FIX feed was active (no global silence over 5 minutes then). Others differ after only 0.0–0.3 s. Two explanations fit:

- (a) Incremental rows were lost inside a live session.
- (b) The venue removes or expires orders (GTD expiry, a mass-cancel at a state change) without publishing deletes.

Neither can be told apart from this data, because `msg_seq` is venue-wide and per-market loss cannot be detected (DQ-08).

**Impact:**

- The rebuilt book can hold phantom orders, or miss real ones, for minutes until the next snapshot. Phantom orders count as queue-ahead liquidity in fill decisions (`sim.rs:759–770` only removes IDs at snapshot time) and feed the projected L2 the core sees.
- The quality gate ignores this metric, so affected markets are not flagged.

**Suggested fix:**

- Include `snapshot_touch_mismatches` in the exclusion or flag criteria (DQ-04).
- Sample the source JSON for a few mismatching orders: check whether `ExpireTime`/`TimeInForce` explains the vanished IDs. If GTD expiry explains them, expire orders in the replay at their expiry time.

### DQ-04 — P2: The only exclusion gate cannot fail on this data

**Locations:** `crates/pm-backtest/src/main.rs:41–43,117–126`; `crates/pm-backtest/src/book.rs:55–56,70–75,227–233`.

`trade_agreement` asks whether *any* resting level exists at the trade's price on the passive side at message start. On this export it is 1.000 for every market (13,365 / 13,365), so `--min-agreement 0.99` excludes 0 of 242 markets in every split. Stricter variants (price not better than the touch, size not larger than the level) also pass 100%. The trades are fine. The gate measures something that is not the problem.

Things that do go wrong, and that no gate checks:

- Coverage: 37 dev markets with no in-play trades; 22 markets whose capture ends before `game_start` (DQ-05).
- Outages and snapshot drift (DQ-02, DQ-03).
- Settlement consistency (DQ-01).

**Suggested fix:**

- Replace the single threshold with a per-market quality record and named exclusion reasons: no settlement, capture ends before start, in-play coverage below X, in-play outage that moved the touch, in-session touch mismatches above N.
- Keep `trade_agreement` as an informational column.
- Report how many markets each reason removes per split, so exclusions cannot quietly reshape the sample.

### DQ-05 — P2: Markets with no in-play data are scored on settlement

**Locations:** `crates/pm-backtest/src/main.rs:113–115` (`trades > 0` is the only inclusion rule); `crates/pm-backtest/src/sim.rs:928–948`.

22 traded markets stop recording before their scheduled `game_start`, by 5–16 hours. They are exactly the 22 markets whose capture ends with a non-empty book (the other 220 end with both sides empty, which is consistent with the venue clearing the book at close). They fall on the last capture day of each window: 10 on 2026-08-19 (dev) and 12 on 2026-10-15 (holdout). 10 of them have a settlement and are included in PnL.

There, any inventory held at the last message is marked to settlement hours later with no market path. `aec-modus-andbae-dylsle-2026-08-19` ends with mid 0.795 seven hours before start and settles 0.

**Suggested fix:** Exclude markets whose last message is before `game_start` (or before start + some minimum), or flatten at the last valid mid for these markets instead of settling them. Either way, give the exclusion a reason in `summary.json`.

### DQ-06 — P3: Unsettled and void markets are handled silently

**Locations:** `crates/pm-backtest/src/data.rs:33–34`; `crates/pm-backtest/src/sim.rs:928`; `crates/pm-backtest/src/report.rs:83`.

- 12 traded holdout markets (all 2026-10-15) have `settlement: null`. They are simulated, then contribute fills and request counts but no PnL. The difference shows up only as `markets` vs `markets_with_pnl`.
- 5 holdout markets settle at 0.5 (all modus, 2026-10-13). They are settled at 0.5 like any other, which gives a halved, near-random PnL contribution.

Neither condition appears in `excluded` or the audit output.

**Suggested fix:**

- Skip `settlement == None` markets up front with an explicit reason, so fill and request statistics come from the same population as PnL.
- Report void markets as their own group, or exclude them, and confirm from venue docs that 0.5 is the actual void payout.

### DQ-07 — P3: `gaps_over_60s` mixes quiet markets with outages

**Location:** `crates/pm-backtest/src/book.rs:66–67,201–203`.

46,371 gaps over 60 s across 241 markets is mostly normal prematch quiet, so the number carries little information. Better signals are available:

- Venue-wide FIX silences: 222 over 5 minutes, 65 of them ending in a reset; 39 over 15 minutes.
- In-play gaps over 60 s: 481 in 107 markets. The longest are 72 minutes (`aec-pdcdarts-jamwad-lukhum-2026-10-03`) and 63 minutes (`aec-modus-jactwe-scomit-2026-10-13`).

**Suggested fix:** Have the exporter write the venue-wide silences, which need all FIX rows and so cannot be computed per market. Then classify each per-market gap as an outage (overlaps a venue silence or reset) or quiet. Report in-play gaps separately from prematch gaps.

### DQ-08 — P3: Per-market message loss cannot be detected; document it

**Locations:** `data-analysis/export_backtest.py:9`; `crates/pm-backtest/src/book.rs:205–206`.

`msg_seq` is venue-wide, so a gap in a market's sequence numbers is normal and cannot show loss. The audit's `reset` heuristic (sequence goes down) is still correct for reconnects. The data README and the original backtest plan described `msg_seq` as if it could support sequence-gap checks ("sequence resets, … book gaps"). Write down that only re-snapshot comparison (DQ-03) can detect loss inside a session. This explains why DQ-03 is the main integrity signal.

### DQ-09 — P3: Snapshot timestamps use a different clock

**Locations:** `data-analysis/export_backtest.py:17,133`; `crates/pm-backtest/src/sim.rs:258–264`.

38,949 FIX `book_snapshot` rows have a null `event_time`, so the exporter falls back to `sending_time` for them. All other exported rows use `event_time`. The sim builds its timeline as a running maximum of `ev_ns`. A snapshot stamped with a later send time can push the clock forward, and the next incremental messages are then clamped to it. With 5 observed backward steps the effect is small. Export a `ev_src` flag (or use one clock consistently for ordering) so it can be measured, not assumed.

### DQ-10 — P3: The strategy-research loader replays differently from `pm-backtest`

**Location:** `data-analysis/strategy-research/extract.py:26–47,51–54`.

`extract.py` rebuilds books for the taker study with rules that differ from `L3Book`/`audit`:

| Issue | What happens |
| --- | --- |
| Snapshot recap trades | Appends the 380 recap trades inside snapshot messages (158 markets) to `trades`. The sim correctly ignores them. Their 158 IDs never appear as live trades, so they are not just repeats of later rows. |
| Unknown changes | Drops a `U` for an unknown order, where the Rust book inserts it (3 rows). |
| Timestamps | Stamps a top-of-book change with the receive time of the message's last row, not `ev_ns`. The Rust replay uses `ev_ns`. |
| Split | Assigns its own split from the first top-of-book time. `common.py` overwrites it from `game_start`, so the pickled `split` field is misleading. |
| Missing `game_start` | `datetime.fromisoformat(None)` crashes (none currently). |

`taker.py` does not read `trades`, so recap trades don't affect current results. The divergence will surface as soon as someone uses them.

**Suggested fix:** Drop `T` rows inside snapshot messages, match the Rust unknown-change rule, and remove the pickled `split`. Better still, have `pm-backtest` export its rebuilt top-of-book and trade series, so research and backtest share one replay.

### DQ-11 — P3: The data-quality report the plan requires is not saved

**Locations:** `crates/pm-backtest/src/main.rs:139–167`.

The backtest design asked for a per-market data-quality report saved with the file checksum, code revision and config. `audit` prints a table and totals to stdout only. `run` saves `Quality` inside `markets.csv` only as `trade_agreement`. Most of the counters (`snapshot_touch_mismatches`, `snapshots_after_reset`, `gaps_over_60s`, `unknown_changes`) never reach a run's artifacts.

**Suggested fix:** Have `audit` write `quality.json`/`quality.csv` with `source_sha256` and git revision. Add the full `Quality` record plus the new coverage fields from DQ-01, DQ-04 and DQ-05 to `markets.csv`.

## Relation to the pm-backtest review

- **BT-01** (unknown delete creates a fill): this export has 3 unknown changes in total, all in the holdout. The trigger exists but is rare.
- **BT-04** (repeated trade IDs): confirmed again. No repeated non-snapshot trade IDs, and no snapshot recap ID is ever repeated as a live trade.
- "No data-gap withdrawal" and "Markouts beyond observed time" are listed there as known limitations. DQ-02, DQ-05 and DQ-07 measure their size on this data: about 6 minutes per reconnect, 85 reconnects, and 24 dev markets whose decisive play is unobserved.
- "Snapshots and queue priority": DQ-03 shows that in-session snapshots are not just reorderings. IDs disappear and appear, so reconciling queue-ahead against snapshots matters in practice.

## Proposed fixes for handling the data

The bad stretches cannot be repaired, because the missing messages are not in the Parquet. The aim is that every result says which data it rests on, never trades on data that is not there, and never treats unobserved time as observed. The fixes are grouped by where they go, then ordered.

### 1. Export: record what is missing (`export_backtest.py`)

The exporter is the only step that sees every FIX row, so venue-wide facts must be computed here.

- **`outages.json`.** List every reconnect: the last message before the sequence reset, and the first snapshot after it. These 85 intervals are known outages. Also list venue-wide silences over a threshold (start with 120 s) without a reset, marked `suspected`. They could be quiet periods or losses.
- **Per-market coverage in `markets.json`:**
  - first and last receive time relative to `game_start`;
  - row and trade counts after `game_start`;
  - `capture_end`: `book_cleared` when the last book is empty on both sides, otherwise `truncated`;
  - the last valid mid (spread ≤ 0.10), its time, and whether it is on the same side of 0.5 as settlement.

  Everything DQ-01, DQ-05 and DQ-06 needs is then in the manifest, so filtering can happen before any CSV is loaded (this also fixes BT-10's ordering).
- **`ev_src` column** (`e` event time / `s` sending time) so DQ-09 can be measured.
- **Flag snapshot recap trades** with their own kind, e.g. `R`. Then no loader has to work out from position that they are not live trades (DQ-10).

### 2. Replay: decide per market, with reasons (`pm-backtest`)

Replace `--min-agreement` with a quality record per market. Load it from the manifest plus `audit`, and save it as `quality.csv` / `quality.json` with `source_sha256` and the git revision (DQ-11). Each market gets zero or more **named exclusion reasons**, and every run reports how many markets each reason removed in each split:

| Reason | Rule (starting threshold) | Affected now |
| --- | --- | ---: |
| `no_settlement` | `settlement` is null | 12 |
| `void` | settlement is 0.5. Report as its own group, not in the headline PnL. | 5 |
| `ends_before_start` | `capture_end == truncated` and last message before `game_start` | 22 (10 settled) |
| `no_inplay` | no trades after `game_start`. Applies only to strategies that trade in play. | 37 dev, 16 holdout |
| `inplay_outage_moved_touch` | a reconnect during play after which the snapshot's touch differs | ≤ 21 |
| `session_drift` | in-session snapshot touch mismatches > 0 | ≤ 20 snapshots |
| `settlement_contradicts_tape` | last trade on the wrong side of 0.5. Informational: the outcome is real, but the path to it was not observed. | 24 (all dev) |

Keep `trade_agreement` as an informational column. Make the policy a CLI option (`--exclude default|none|<list>`) so a run can show how much the exclusions change the result.

### 3. Simulation: no quoting or marking on missing time (`sim.rs`)

- **Replay outages as disconnects (DQ-02).** At each outage start from `outages.json`, deliver `Event::Connection { feed: Markets, state: Down }` to the core after the normal feed delay. Deliver `Up` with the post-reset snapshot. `Core` already handles this the way the live bot does (`crates/pm-core/src/core.rs:282–293`): it un-seeds market data and cancels every market. Most of the outage fix is therefore event scheduling, not new strategy logic.
  - The sim should also stop matching our resting orders against the frozen book during the outage. There are no trades in the outage anyway, so it is enough to reject fills timestamped inside one.
- **Compare post-reset snapshots in `audit`** and report them as `outage_drift`, not skip them (DQ-02).
- **No markouts across missing time.** `mid_at` should return `None` past the last message and inside outages, and the fill row should record how old the observation is. This resolves the markouts limitation for this data.
- **Split PnL by what was observed.** Next to `spread` and `inventory`, report `unobserved_inventory`: settlement minus the last observed valid mid, times the position held at the last message. On the dev split this will be large and near-random. Separating it stops it from swamping the parts of PnL the strategy actually controls.

### 4. Methodology: splits that compare like with like (DQ-01)

The August data has prematch quotes, mostly modus, and little in-play. The October data has 43% of trades in play. Two options, and **this is a decision for us**:

- **A. Keep the splits; restrict what dev is used for.** Tune only prematch behaviour on dev. Treat in-play parameters as untuned until there is more in-play data. This is the safest choice if the holdout has already been used.
- **B. Re-split October, only if the holdout has not yet been run or used for any decision.** Possible split: dev = 2026-10-01 → 10-10 (103 traded markets); holdout = 10-11 → 10-15 (68, about 56 usable after `no_settlement`). August becomes a prematch-only supplementary set. Both new halves then have comparable in-play coverage and league mix. The cost is a small holdout, so confidence intervals will be wide.

Either way, always report prematch and in-play results separately, and report results per league.

### 5. Investigate before modelling (DQ-03)

Take 5–10 orders that vanish between in-session snapshots with no delete row (2026-10-06 modus markets first). Read their `source_record_json` for expiry time, time-in-force, and the market state around the vanishing.

- If GTD expiry explains them: export the expiry time and expire orders in `L3Book` at that time. That would fix most `session_drift` and queue-ahead errors at the source.
- If it does not: keep `session_drift` as an exclusion reason. Note that losses inside a session cannot be detected (DQ-08).

### 6. Collect better data going forward

The live bot already records what it sees. Make sure that recording keeps these, so the next dataset avoids the same problems:

- explicit connect, disconnect and heartbeat markers, so outages are recorded, not inferred;
- capture through to settlement or market close for every match;
- order expiry fields;
- the WebSocket L2 feed the bot actually trades on, alongside FIX (they differ).

Once live sessions exist, the planned replay of live journals through the sim is also the best check on whether these exclusions are calibrated.

### Order

1. Export coverage, `outages.json`, and the quality record with named exclusions, saved per run (§1, §2). This is small and immediately makes every existing result interpretable.
2. Choose option A or B for the splits (§4) before any further tuning.
3. Disconnect replay, no markouts across missing time, and `unobserved_inventory` (§3).
4. The DQ-03 investigation (§5); expiry modelling if it confirms expiry.
5. Clean-ups: `extract.py` alignment, `ev_src`, recap-trade kind (DQ-07, DQ-09, DQ-10).
6. Requirements for future data collection (§6).
