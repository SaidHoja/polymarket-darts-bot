# In-play leg study and the double-fill bug (2026-10-28)

Question: can a game-state-aware market maker do better in play than `two_phase`, by reacting to leg changes, waiting for the book to settle, or quoting wider on the side that just moved?

Data: the first captures with real game state, `captures/game-2026-10-27.jsonl` and `game-2026-10-28.jsonl`, aligned on wall clock with the market frames (`captures/2026-10-2{7,8}.jsonl`), the core journals and our private fills. That is 27 MODUS Super Series matches (all first to 4 legs, no score corrections) and about 118 usable leg changes. Scripts are in [`data-analysis/in-play-leg-study-2026-10-28/`](../../../../data-analysis/in-play-leg-study-2026-10-28/) (see [Reproducing](#reproducing)).

**Summary.**
- The score feed does not put us ahead of the book. The book moves in the ~2 s around the venue's score stamp, and we see the stamp 0.6 s after it.
- The feed is still useful for two things: knowing when *not* to quote (legs have a predictable clock), and which side to widen after a leg (the book keeps drifting toward the leg winner, and takers who fade it lose).
- Our live in-play fills lost 17¢ per contract at +60 s. Two concrete bugs contributed:
  - `two_phase` keys its in-play width off the scheduled start, but MODUS matches often start early.
  - The core re-places a quote straight after it fills, using a book that hasn't caught up yet. [Detail below](#the-double-fill-bug).

## Findings

### 1. The book moves at the score stamp, not after it

Mean signed move toward the leg winner, against the mid 20 s before the venue's score stamp (`updatedAt`):

| Time from stamp | −4 s | −2 s | −1 s | 0 | +1 s | +5 s | +8 s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Mean move | +0.6¢ | +1.7¢ | +2.2¢ | +3.9¢ | +4.9¢ | +5.5¢ | +5.9¢ |
| Legs moved ≥ 2¢ | 14% | 21% | 27% | 39% | 44% | 43% | 44% |

- 66% of the first ≥ 2¢ moves toward the leg winner happen before the stamp, and 38% happen more than 5 s before it. People watching the stream are faster than the venue's own score.
- Our poll sees the stamp 0.6 s after it (p50 0.61 s, p90 1.05 s).
- The ~14 s lead in [connectivity §7](../../../polymarket-docs/polymarket-connectivity.md#7-live-game-state) was a single sample and does not hold in aggregate. Taking on score changes isn't viable.

### 2. After the stamp, the book keeps drifting toward the leg winner

- From the moment we see the stamp to +60 s, the mid moves another +4.3¢ toward the winner on average. The median is only +0.5¢, so a few legs where the book is stale carry the mean.
- Share of the move (−60 s → +60 s) already done, for legs that moved ≥ 3¢: about 0 at −5 s, 0.62 at the stamp, 0.74 at +5 s and 1.0 at +30 s. So the book has mostly settled by +30 s.
- Takers in the +2 to +30 s window mostly fade the move: 26% trade with the leg winner. They lose. Maker markouts at +60 s in that window:

| Window from stamp | Maker sold the leg winner | Maker bought the leg winner |
| --- | --- | --- |
| +2 to +10 s | n = 5, −19¢ | n = 14, +21¢ |
| +10 to +30 s | n = 18, −9¢ | n = 51, +4¢ |

Splitting by leg winner leaks hindsight into windows *before* the stamp, so only post-stamp windows are used here. Counts are small.

### 3. Legs have a usable clock

- No leg ended in under 60 s. Leg duration: p10 87 s, median 124 s, p90 161 s.

| Time into the leg | 60 s | 80 s | 100 s | 120 s | 140 s |
| --- | ---: | ---: | ---: | ---: | ---: |
| P(leg ends within 15 s) | 0.02 | 0.16 | 0.24 | 0.37 | 0.57 |

- So there is a window of roughly 0–70 s after a stamp in which a leg-ending jump is very unlikely, followed by a fast-rising risk.
- Maker markouts by time into the leg are noisy (n = 5–99 per 30 s bucket) and don't yet show a clean pattern.

### 4. The leg model overstates the step

- A first-to-4 Markov model with τ (hold rate) = 0.6 and δ (strength difference) solved from the settled mid predicts a median step of 14.3¢.
- The book's realised step, from 45 s after the previous leg to 60 s after this one, is a median 5.0¢. That is a mean ratio of 0.62 and correlation 0.46.
- The maximum-likelihood τ on these 143 legs is 0.67, but the likelihood is nearly flat from 0.5 to 0.7.
- Conclusion: the model is useful as a skew signal once scaled down (×0.6), not as the fair value on its own.

### 5. MODUS matches start early

The first live frame came before the scheduled `game_start_ns` by up to 15 min. 12 of 27 matches went live more than 5 min early. `two_phase` widens only at `widen_before_start_s` (300 s) before the scheduled start (`within_start` in `crates/pm-core/src/strategy/spread.rs`), so in those matches it quoted the 10¢ pre-match width in play. `pre_match` checks `game.frame.live`; `two_phase` and `full_match` don't.

### 6. Thin books

- In play, the median spread is 12–15¢.
- Notional over the two days: $489 in play and $66 pre-match, across 27 markets. That is about $18 of in-play volume per match.
- Much of "others' book" is another bot's 1-lot quotes. Centring on others' mid, we ratchet up together with it and often end up as the best price ourselves.

### 7. Our live in-play fills (two_phase, 10-28)

| Phase | Contracts | Markout at +60 s |
| --- | ---: | ---: |
| In play | 10.8 | −$1.83 (−17¢ per contract) |
| Pre-match | 2.0 | +$0.16 |

- Five in-play fills came 0–2 s before a stamp. The game feed can't prevent those; only pulling on book moves or being wider late in the leg can.
- Four came 4–6 s after a stamp. A game-aware pull would have avoided them.
- Two were the double fills described next.

## The double-fill bug

**What happens.**
1. Our quote is filled by an aggressor who is sweeping the book.
2. The core re-decides the market in the same tick, using a book that doesn't yet show the sweep.
3. It places a new order 1¢ behind the one that just filled, straight into the aggressor's path.
4. The market data arrives 10–20 ms later and the strategy wants to cancel. But the new order has no venue id yet, so the cancel waits for the acknowledgement (~70 ms), and the order is filled 1–8 ms after that.

### Case A: 11:21:05, `aec-modus-davdav-darbev-2026-10-28`, ask side

| UTC | What happened |
| --- | --- |
| 11:21:03.938 | Others' best bid 0.54 is pulled. Fair moves 0.57 → 0.53, and we modify to bid 0.38 / ask 0.68. Others' offers are 0.60 and 0.72. |
| **11:21:05.174** | **Private socket: our ask 0.68 is filled.** In the same tick the strategy, still using the 11:21:03 book (fair 0.53), decides ask 0.69, and the core sends **Place sell 0.69**. |
| 11:21:05.185 | Public trades arrive, 11 ms after our fill: 0.60 × 0.1, 0.60 × 1, 0.68 × 1, taker buying. The provisional view drops those offers, others' spread exceeds `max_spread`, fair becomes `None` and the strategy wants no ask. The order is already sent, so the cancel waits for its acknowledgement. |
| 11:21:05.242 | Book: best bid 0.56, best offer 0.91. Fair is 0.735. |
| 11:21:05.245 | New ack for the 0.69 order, 71 ms after the Place. The cancel goes out. |
| **11:21:05.253** | **Our 0.69 ask is filled**, 8 ms after the ack. |
| 11:21:05.686 | The game poll shows 2-0. The leg had been won about 0.5 s before we saw it. |

Mid at +60 s: 0.895. Markout −$0.215 on the first fill and −$0.205 on the re-placed one.

### Case B: 13:02:47, `aec-modus-andbae-darbev-2026-10-28`, bid side

| UTC | What happened |
| --- | --- |
| 13:02:44–47 | Our bid ratchets 0.59 → 0.61 together with another bot's 1-lot bids (0.60, 0.59). We are the best bid. The leg stamp was ~5 s earlier; `two_phase` ignores game state. |
| **13:02:47.664** | **Private socket: our bid 0.61 is filled.** Others' best bid is now the other bot's 0.60, which the same taker has already hit, but the book doesn't show it yet. The core sends **Place buy 0.60**. |
| 13:02:47.683 | Public trades arrive, 19 ms after our fill: 0.61 × 1 and 0.60 × 1. Fair becomes `None`, and the cancel is deferred to the acknowledgement. |
| 13:02:47.704 | Book: best bid 0.15, offers 0.68 × 52. |
| 13:02:47.731 | New ack for the 0.60 order, 67 ms after the Place. The cancel goes out. |
| **13:02:47.732** | **Our 0.60 bid is filled**, 1 ms after the ack. |
| 13:02:47.804 | Offers 0.61 × 53. The seller is still dumping. |

Mid at +60 s: 0.345. Markout −$0.265 on the first fill and −$0.255 on the re-placed one.

### Root cause

| # | Cause | Where |
| --- | --- | --- |
| 1 | The private socket is ahead of market data. Our fill arrived 11 ms and 19 ms before the public trades, and 40–70 ms before the book. | Venue behaviour |
| 2 | A fill marks the market `urgent` and `dirty`, so it is re-decided in the same tick. | `crates/pm-core/src/core.rs`, `OmEvent::Fill` in `on_om_events` |
| 3 | The strategy prices off "others' book", which is the provisional view minus our resting orders. Our filled order is gone from it, but the other levels the same aggressor took are still there. The mid is computed over levels that no longer exist. | `core.rs`, `decide_and_act` |
| 4 | Nothing slows quoting after a fill. The strategies ignore `MarketView.filled`, and `RiskState::on_fill` only enforces the fill-rate limit (`max_fills_per_market_per_min = 10`). | `strategy/spread.rs` `decide`; `risk.rs` `on_fill` |
| 5 | An empty slot with a target places immediately. | `orders.rs` `sync_side` |
| 6 | A cancel of an order that is sent but not yet acknowledged is deferred until the ack, so it can't beat a taker who is already there. | `orders.rs` `cancel_rec`, the `(PendingNew, None)` arm |

Causes 1 and 6 are fixed constraints. Causes 2–5 are ours.

### Scope

- The 10-28 core journal has 10 full fills. In 5 of them the same tick placed a new order on the same side.
- 2 of those 5 were filled again. Both were 1¢ behind the price that had just filled, so the stale mid had put them right in the aggressor's path.
- The other 3 landed 3–18¢ away and didn't fill.
- The two re-fills cost $0.46 at +60 s, about a quarter of the in-play markout loss.
- Case B's first fill would also have been avoided by a game-aware pull, since the stamp was seen ~5 s earlier. Case A's would not.

### Fix options

| Option | What | Pros | Cons |
| --- | --- | --- | --- |
| **A. Fill fence (core)** | After a fill, don't place or improve on the filled side until market data catches up: a trade or book with `transact_ns` ≥ the fill's, or a timeout (e.g. 250 ms). Cancels and moves away from the touch are still allowed. | Works for every strategy. Small change. Addresses the stale book directly. | Doesn't help when the aggressor keeps coming after the book catches up. |
| **B. Post-fill cooldown and widening (strategy)** | In play, after a fill, skip that side for N s, then quote it wider by X, decaying back. | Handles informed flow that keeps hitting (case B's seller was still dumping 53 lots). | Needs to know which side filled. `MarketView.filled` is a bool, so add the side, or compare position. |
| C. Use our fill as a provisional trade | Apply our own fill to the provisional view as soon as it arrives. | Removes levels better than ours. | Can't know about levels behind ours. In case B the 0.60 behind us was also gone. Not enough alone. |

**Recommendation: A, then B.**
- A is a correctness fix: we should never decide on a book we know is stale.
- B is the strategy half, and it fits the game-aware design below.
- Test for A: a `pm-core` tick test where the bid fills, the `Execution` (fill) arrives before the matching `Trade`, and the tick must not `Place` on the bid. A later trade or book with `transact_ns` ≥ the fill's lifts the fence.
- `pm-backtest` delivers executions after ~20 ms and trades after ~54 ms, so the ordering exists in simulation too. I haven't checked whether the sim reproduces the second fill; that depends on later historical prints at our price.

## Proposed strategy: game-state-aware in-play market maker

This builds on `Phase` (`BetweenLegs`, `InLeg`, `MatchPoint`) and on `GameState.last_leg_change_mono`, keeping `MarketView` darts-specific.

| State | Trigger | Behaviour |
| --- | --- | --- |
| Pre-match | not `game.frame.live` | As now (`pre_match` / `two_phase` pre width). The `live` flag ends it, not only the scheduled time. |
| Post-leg | 0 to `post_leg_s` (~30 s) after we see a score change | Cancel immediately (the core already sets `must_act`). Shift the centre toward the model by (model − mid) × 0.6 × a weight that decays to 0 by `post_leg_s`. Keep the side that buys the leg winner at normal width; widen the side that sells it by `toxic_side_extra`, or drop it. |
| Mid-leg | `post_leg_s` to `safe_until_s` (~70 s) into the leg | Normal in-play width around others' mid. Lowest risk of a leg ending. |
| Late leg | over `safe_until_s` | Widen in line with the leg-end risk (§3), or pull. Never be the best price; at most match it. |
| Match point | either player one leg from winning | Reduce-only or off. |
| After a fill | any in-play state | Fill fence (A) plus cooldown and widening (B). |
| Book jump or trade through | any in-play state | Pull for a few seconds. This catches the two-thirds of moves that start before the stamp. The old market maker's jump guard was only tested pre-match. |

## Plan

1. **Fixes for losses we've seen.**
   - The fill fence (A).
   - `two_phase` and `full_match` treat `game.frame.live` as the start.
2. **Backtest on real game state.**
   - `pm-backtest` fakes game state from the scheduled start. Make it replay `game-*.jsonl` frames as `Event::Game`.
   - Turn the scripts in `data-analysis/in-play-leg-study-2026-10-28/` into a maintained event study.
3. **The `in_play` strategy kind**, with the state machine above. Parameters `post_leg_s`, `safe_until_s`, a late-leg widening curve, `toxic_side_extra`, `fill_cooldown_s` and a match-point mode.
4. **Model skew.** A first-to-N win-probability table per league, τ fitted by league, scaled ×0.6. Only after step 3 shows the defensive version works.
5. **Optional: faster game polling while live.** The book's move straddles the stamp by about ±1 s. Check against the shared rate limit first.

## Caveats

- Two days, one league (MODUS, first to 4), about 118 legs. The trade-markout buckets have n = 5–99.
- PDC, sets formats and deeper books may behave differently. Keep recording game journals and re-run.
- The prize is small (~$18 of in-play volume per match). Most of the value is defensive: stopping the −17¢ per contract.

## Reproducing

From the repo root (each script reads `captures/`):

```sh
D=data-analysis/in-play-leg-study-2026-10-28
python3 $D/legstudy.py . 1.0 /dev/null        # §1–2, §4: offsets, share of move done, model step, trades
python3 $D/legstudy2.py . 1.0                 # §1 fine timing, first-move timing, settle, τ MLE, jump sizes
python3 $D/legstudy3.py .                     # §2 maker markouts by window, §6 notional
python3 $D/elapsed.py .                       # §3 leg durations, leg-end hazard, markouts by time in leg
python3 $D/trace.py 28 davdav-darbev 11 21 3 3.5   # case A timeline from the core journal
python3 $D/trace.py 28 andbae-darbev 13 2 44 4.5   # case B timeline
```

The second argument to `legstudy.py` is the dust filter (minimum level size counted as the touch). The results above use 1 contract.
