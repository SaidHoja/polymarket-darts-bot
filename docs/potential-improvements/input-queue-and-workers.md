> **Status: deferred, not part of the current build.** The bot ships with the single core thread from [pm-live](../architecture/pm-live.md#core-thread). Revisit this plan only after the load metrics in 06 have been watched for a while, including in-play sessions with several concurrent matches. **Trigger to start:** the receive → tick-start queue delay p99 stays above ~50 ms, or tick time becomes a meaningful share of the time from event to command, under real in-play load.
>
> Until then, avoid closing the door on it: keep per-market state in per-market structs, keep account-level state (portfolio, account risk, order manager) separate from per-market logic, and keep strategy calls free of cross-market reads. With that, this plan can be adopted later without rewriting the core.

This plan would replace the single core thread with **N market workers**. Each market is owned by one worker, chosen by estimated load. The tick steps in 06 still apply; they're split between the workers (everything that concerns one market) and one **coordinator** (everything shared between markets). With `workers = 1` the result is the single-thread design.

## Idea in one paragraph

The input queue carries **no payloads**. It's a set of *tickers that changed recently*, with a bitmask of *what* changed. The actual data (latest book, recent trades, game state, our orders and position) lives in per-market caches written by the I/O tasks and the coordinator. A worker takes a changed ticker off its partition's queue, reads whatever it needs from the caches, runs per-market state updates, strategy and per-market risk, and sends a *proposal* to the coordinator. The coordinator applies account-level risk, runs the order manager and emits commands.

```
 I/O tasks ──write──► per-market caches (book, trades, game, params)
     │                         ▲          ▲
     └─ mark(market, mask) ─►  │          │ read
                               │          │
          partition queues  [P0: {A,F}] [P1: {B}] [P2: {C,D,E}]     (dirty sets of MarketIds)
                               │          │          │
                            worker 0   worker 1   worker 2          (own their markets' strategy state)
                               └──────────┴────┬─────┘
                                               ▼ proposals {market, targets, input versions, derived events}
 order feedback ──────────────────────────► coordinator (1 thread): portfolio, account risk, order manager, kill switches
                                               │ writes order/position cache + mark(market, ORDERS)
                                               ▼
                                         order gateway
```

## Caches (the data)

One `MarketCache` per market. Each field has **exactly one writer**; readers get immutable snapshots without blocking it.

| Field | Writer | Readers | Structure |
| --- | --- | --- | --- |
| `book` | Markets WS task | Owning worker, coordinator (risk band checks) | `ArcSwap<BookFrame>` + version (= `transactTime`, monotonic) |
| `trades` | Markets WS task | Owning worker | SPSC ring per market (e.g. 1,024); the worker tracks its read cursor; overrun → `trades_lost` flag |
| `game` | Game poller | Owning worker, coordinator | `ArcSwap<GameState>` + version (increments on change) |
| `params` | Discovery task | Owning worker, coordinator | `ArcSwap<MarketParams>` + version |
| `orders_view` | Coordinator | Owning worker | `ArcSwap<OrdersView>`: working orders, in-flight flags, position, recent fills and rejects for this market, + version |
| `health` | Coordinator | Owning worker | `quotable` flag and reasons |

Raw frames are still journaled by the I/O tasks on receive.

## The input queue (what changed)

### Change mask

```rust
bitflags! { struct Changed: u16 {
    BOOK = 1; TRADES = 2; GAME = 4; ORDERS = 8; PARAMS = 16; TIMER = 32; HEALTH = 64;
}}
const URGENT: Changed = GAME | ORDERS | HEALTH;
```

### Per-market dirty word and partition queues

- Each market has an `AtomicU16` **pending mask** in its cache.
- `mark(market, bits)`:
  1. `prev = pending.fetch_or(bits)`.
  2. If `prev == 0`, the market wasn't queued, so push its `MarketId` onto its partition's queue (the urgent lane if `bits ∩ URGENT`, else the normal lane) and wake that partition's worker.
  3. If it was already queued, do nothing more. It's queued at most once, however many updates arrive.
- If an urgent bit arrives for a market already sitting in the normal lane, it's also pushed to the urgent lane. The worker skips the duplicate entry when it finds the mask already cleared.
- Each partition has two lock-free MPSC queues (urgent, normal) and a doorbell (as in 06). The worker always drains urgent first.

This gives conflation for free: ten book updates and three trades for market A before the worker gets to it become **one** queue entry with mask `BOOK | TRADES`.

### Worker loop

```rust
loop {
    let market = partition.pop_urgent().or_else(|| partition.pop_normal()).or_else(|| wait(doorbell, next_timer()));
    if !self.owns(market) { forward_to_current_owner(market); continue; }   // see Rebalancing
    let changed = cache[market].pending.swap(0);       // clear BEFORE reading
    if changed.is_empty() { continue; }                 // duplicate entry
    let inputs = cache[market].snapshot();              // book, new trades, game, params, orders_view, health + versions
    let derived = self.state[market].update(&inputs, changed);   // provisional book, sweeps, leg change, phase…
    let targets = self.strategy.decide(&self.state[market].view(&inputs), now);
    let targets = self.market_risk.gate(market, targets, &inputs);   // per-market limits & pre-trade checks
    coordinator.send(Proposal { market, targets, derived, versions: inputs.versions(), worker: self.id });
}
```

**Clear before reading** is what keeps updates from being lost. An update that lands after the `swap(0)` sets the mask again and re-queues the market, so the worker will see it on its next pass. The worst case is one extra pass; an update is never skipped.

Timers (blend decay, game-stale, phase change) for a market live in its owning worker's timer heap; when one fires, the worker marks the market `TIMER`. Account-wide timers (reconcile, GTD refresh batching, watchdog) live in the coordinator.

## Partitioning by load

### Load estimate per market

```
load(m) = rate(m) × cost(m)
rate(m) = EWMA over 60 s of (book frames + trade frames + game changes) per second    // measured live
        before enough data: prior(phase, league)
cost(m) = EWMA of worker CPU time per pass for m                                      // measured; prior = league average
```

Priors, until live rates exist:
- Pre-match ≈ 0.03 frames/s per market. The 2026-10-21 baseline was 4,577 updates/hour over 41 tickers.
- In-play: use the FIX history's per-minute peak for that league from [`load_estimate.json`](../../data-analysis/load_estimate.json), scaled down for WS L2 conflation.
- A market about to go live (within 10 min of `gameStartTime`) is given its in-play prior *before* it goes live, so rebalancing happens ahead of the burst rather than during it.

### Assignment

- **Greedy longest-processing-time bin packing:** sort markets by load, descending, and assign each to the worker with the lowest total so far. It's simple, deterministic (ties broken by `MarketId`) and within 4/3 of optimal.
- The result is an `assignment: ArcSwap<Vec<WorkerId>>` indexed by `MarketId`, plus a generation number. `mark()` reads it to find the partition.
- **New markets** from discovery are placed on the currently least-loaded worker at their prior load.
- **Worker count:** config, default `min(available_cores − 3, 4)`. The other cores go to the tokio I/O runtime, the coordinator and the OS. `workers = 1` is valid and is the default for backtests and debugging.

### Rebalancing

| When | What |
| --- | --- |
| Every 60 s | Recompute loads and the ideal assignment. Apply it only if the max/mean worker load imbalance is > 1.5 **and** moving markets would cut it by ≥ 20% (hysteresis, so markets don't bounce) |
| A market enters the start window or goes live | Re-place that market immediately at its in-play prior |
| A market ends | Drop its load; no move needed |
| A worker's queue delay p99 > 50 ms for 10 s | Force a rebalance |

At most `max_moves_per_rebalance` (e.g. 3) markets move at once, preferring the cheapest moves: markets with no working orders and no in-flight actions.

### Moving a market between workers (ownership handoff)

Each market has exactly one owner at a time, which is why its strategy state needs no locks. A move goes:

1. The coordinator publishes the new assignment with `generation + 1` and the market marked `migrating{from, to}`.
2. New `mark()` calls go to the new partition. The new worker sees `migrating` and parks those entries without processing them.
3. The old worker finishes its current pass (if any), then sees on its next pop that it no longer owns the market. It packs the per-market state (strategy state, provisional overlay, trade cursor, timers) into a `Handoff` message to the new worker, and forwards any queue entries it still holds for that market.
4. The new worker installs the state, clears `migrating`, sets the pending mask to `all` and processes the market once from fresh caches.
5. While a market is migrating, the coordinator ignores proposals for it from the old worker, using the `worker` and generation in the proposal. Resting orders stay working; risk-driven cancels from the coordinator still apply, because they don't need a worker.

Handoff time is a metric. Target: under 5 ms.

## The coordinator

Single thread. It owns everything shared across markets:

- **Order feedback:** executions, REST results, snapshot, positions and balance go straight to the coordinator's FIFO, not through the worker queues. It updates the order manager and portfolio, writes each affected market's `orders_view`, and calls `mark(market, ORDERS)`. That's how a fill or reject reaches the owning worker's strategy.
- **Kill switches and urgent safety:** feed down, private down, game stale, `MatchEnded`/`CAN`, admin cancel-all. The coordinator acts on these itself, without waiting for a worker, and then sets `health` and marks the affected markets `HEALTH`.
- **Leg changes:** the game poller writes the game cache, marks the market `GAME`, **and** sends `LegChanged(market, game_version)` to the coordinator. The coordinator immediately blocks quoting at the old fair value for that market: it cancels the working quotes unless a proposal built on `game_version ≥` the new one arrives within `leg_reprice_grace` (e.g. 5 ms). The safety reaction doesn't depend on worker scheduling, and a fast worker can still reprice instead of the market going dark.
- **Proposal handling:**
  1. **Staleness check:** a proposal carries the versions of the inputs it was computed from. The coordinator drops a proposal whose `orders_view` version is older than the current one (a fill arrived since), or whose game version is older than the latest leg change, then re-marks the market so the worker recomputes. Book version is not checked strictly, because a slightly older book is normal and harmless.
  2. **Account gate** in priority order with reservations, exactly as in [pm-core](../architecture/pm-core.md#the-tick). Proposals from all workers that arrived since the last pass are sorted by priority before gating, so priority holds across workers.
  3. **Order manager:** diff, actions, gateway. As in [pm-core](../architecture/pm-core.md#order-manager).
- **Load:** the coordinator does little per proposal (a few checks and a diff), so one thread is ample. If it ever becomes the bottleneck, its queue delay metric will show it first.

## Ordering and determinism

| Property | Guaranteed? |
| --- | --- |
| Per market: updates are processed in order, and strategy state has one writer | Yes (single owner, versions are monotonic) |
| Per market: a fill or reject is seen by the strategy before its next proposal is accepted | Yes (staleness check on `orders_view` version) |
| Across markets: the order in which workers finish | **No** (depends on scheduling) |
| Across markets: the order of account gating | Yes within a coordinator pass (priority sort); passes depend on arrival timing |
| Replay reproduces live commands exactly | Only the coordinator, given the journaled proposals. A full replay with `workers = 1` is deterministic but may differ in timing-dependent ways from a multi-worker live run |

What follows from that:
- The coordinator journals every proposal it receives (with versions) and the order it handled them in. Incidents are replayed from the coordinator journal.
- Backtests run with `workers = 1`, or with simulated partitions that process in a deterministic order. Results must not depend on worker count. A CI test runs one day of replay with 1 and 4 workers and checks that the per-market targets match.

## Metrics

- Per partition: urgent and normal queue depth, **mark → pick-up delay** (p50/p99), passes per second, worker CPU utilisation.
- Per market: estimated vs measured load, conflation ratio (marks per pass), stale proposals dropped.
- Rebalances: count, markets moved, handoff duration, imbalance before and after.
- Coordinator: feedback queue depth, proposals per second, time from proposal to command.

## Why it's deferred

At today's measured rates (≈ 1.3 book frames/s across all markets pre-match; a few microseconds of strategy work per market) one core thread is idle almost all the time. The partitioned design pays off only in-play with many concurrent matches, or if the strategy becomes expensive (e.g. a richer model), and it costs exact cross-market replay. Decide from measurements: the 06 metrics (queue delay, tick time per step, books skipped by conflation) are what would justify it.

## Tests

- **Lost-update test:** hammer `mark()` from several threads while a worker clears and reads; assert the last update for every market is always processed.
- **Handoff test:** migrate markets mid-burst; assert no pass runs on two workers at once, no state is lost and no stale proposal is accepted.
- **Staleness test:** a fill lands between snapshot and proposal; assert the proposal is dropped and recomputed.
- **Leg-change race:** worker slow or fast relative to `leg_reprice_grace`; assert quotes at the old fair value never stay up past the grace period.
- **Equivalence:** 1 worker vs N workers on the same journal produce the same per-market targets.
- **Load:** synthetic in-play burst at 3× the FIX peak (2,292 rows/s) across 20 live markets; measure pick-up delay and rebalancing behaviour.
