# Operator-run latency probe

The `latency_probe` strategy exercises ordinary strategy, risk, order-manager, gateway, and private-feedback paths with small, distant post-only quotes. It does not guarantee non-execution. This implementation has been tested locally; no live orders were submitted as part of its preparation.

## Behavior

Use [the ready-to-run configuration](../config/latency-probe.toml), also provided as [an example](../config/latency-probe.example.toml). Normal darts match-winner discovery and subscription updates run unchanged. `latency_probe` is the default strategy for all discovered markets, with no slug allowlist or routes. The normal risk gate caps worst-case exposure, including resting orders, at **$2 per market and $20 across the account**. Up to 40 active orders are allowed so multiple markets can quote. Minimum-size quoting and the 0.1-contract order/position caps remain in place; the dollar limits are ceilings, not sizing targets. The separate $0.05 daily/market loss stops remain unchanged.

The strategy requires a known pre-match game at least five minutes before scheduled start, both feeds ready through the normal core gate, and an open, seeded book. Wide books are allowed. Each quote must remain strictly behind the same-side best price and at least $0.20 from the opposite best price. Each side is checked independently; if the 2-cent/98-cent variant would join the touch, it falls back to 1 cent/99 cents, or skips that side if no candidate passes. It skips markets whose minimum order size exceeds `max_size` (0.1 contracts by default), or whose tick cannot express the distant quotes.

It quotes the venue minimum quantity, alternating YES-price bid/ask pairs of $0.01/$0.99 and $0.02/$0.98 on received input, at least `reprice_s` (2 seconds) apart per market. These are post-only maker targets, never IOC takes. New placements and reprices require a valid received book/trade or game frame in the current tick; timer-only and feedback-only ticks cannot create that traffic. Safety cancellations can still run without a new input. Existing order-state and risk checks still apply. The dedicated configuration explicitly widens the fair-distance check to $0.75 to permit these diagnostic quotes while retaining the real book microprice as fair value; other strategies should not inherit this configuration casually.

There is no quoting-session timeout. The probe continues evaluating received inputs until the process is stopped or the market becomes ineligible. A fill or any observed nonzero position permanently stops that strategy instance. The normal order manager requests cancellation when targets disappear; the strategy never automatically liquidates a resulting position. Orders have a 15-second GTD lifetime rounded up to whole seconds (at most 16 seconds from decision time). Housekeeping does not passively renew probe orders; strategy repricing can replace them with a new short expiry. Network delay or rejection can still prevent an immediate cancellation.

When a probe profile is configured, successful responses from the existing game-state poll are forwarded even when scores/status are unchanged. This adds no polling requests or synthetic events: the real response receipt provides a REST input timestamp. Quiet pre-match polling normally runs every 30 seconds, so quotes may expire between polls. WebSocket inputs also trigger evaluation, including valid snapshots with an unchanged touch.

Stop the process with Ctrl-C, or use the admin pause/cancel-all command to stop quoting. Restarting the process creates new strategy instances; it also clears any fill/position stop latched by an earlier instance, though an existing position will immediately stop that market again.

## Launch and collect

The probe configuration sets `[execution].rate_limit_enabled = false` for the unthrottled comparison. Ordinary configurations default to true (15 tokens/second, charged per order action). Actual HTTP 429 backoff stays enabled in either case. Restart to apply this flag; do not run both variants simultaneously. The status response and metrics journals include `rate_limit_enabled` and cumulative `http_429_total`. Compare `gw_wait`, `input_to_send`, and `input_to_ack` along with sample counts and 429s. See [the rate limiter](architecture/pm-venue.md#rate-limiter) and [venue rate limits](polymarket-docs/polymarket-connectivity.md#8-rate-limits-and-request-budget).

Build the executable; the probe configuration is already present:

```sh
cargo build --release -p pm-live --offline
```

Run the live process yourself; discovery selects the markets automatically:

```sh
./target/release/pmbot --mode live \
  --config config/latency-probe.toml \
  --journal-dir captures/latency-probe \
  --run-dir run/latency-probe
```

This command submits real orders. Use an otherwise idle account/session: existing reconciliation can cancel unknown orders, and shutdown/cancel-all can cancel account-wide orders, not just this probe's quotes.

In another terminal, inspect eligibility/status and the since-start latency table:

```sh
echo status | nc -U run/latency-probe/pmbot.sock
echo metrics | nc -U run/latency-probe/pmbot.sock
```

The log explains normal gate failures. Strategy eligibility filters can also produce no quotes. An unready or ineligible market is not evidence of zero latency; other discovered eligible markets can still quote within the account and order limits.

Metrics windows are saved every 60 seconds to `captures/latency-probe/metrics-<UTC date>.jsonl`. Core decisions and execution feedback are in the associated core/private journals. Read the since-start table before exiting, or retain the window journals. Ctrl-C invokes the existing best-effort cancellation path. `cancel-all` over the admin socket cancels and leaves the bot paused.

Paper mode can exercise decisions and synthetic acknowledgements, but intentionally emits neither new live end-to-end metric. Record mode measures feeds without order execution. Neither provides real order round-trip latency.

## Measurements

| Metric | Boundary |
| --- | --- |
| `input_to_send` | Same-market input receipt through decoding, core processing, gateway wait, request serialization/signing, to HTTP-client dispatch |
| `input_to_ack` | Same input receipt through a matching private New/Replace acknowledgement processed by the core |
| `gw_wait` | Gateway queue entry to dispatch scheduling |
| `rest_create`, `rest_modify`, `rest_cancel` | REST round trips, including a retry where applicable |
| `book_lag`, `trade_lag` | Venue timestamp to local receive; includes clock offset |
| `feed`, `input_to_drain`, `queue`, `wake`, `core_pre`, `tick`, `core_post`, `core_busy`, `decide` | Internal processing stages; see [metrics](metrics.md) for how they add up to `input_to_send` |

The new stages report sample count, mean, p50, p90, p99, and maximum alongside existing stages. Their origin is the oldest surviving same-market input in the current tick, not an unrelated global input or an old event retained across ticks. Game input begins at REST poll completion; book/trade input begins at complete WebSocket frame receipt. This is batch attribution, since several events may influence a decision.

Timer-only and feedback-only ticks cannot place/reprice probe orders. Passive GTD refreshes, outbound reads, account-wide cancellations, and commands with no corresponding input in their tick do not contribute to these metrics. An inbound game-poll response can trigger a measured order even if its payload is unchanged. `input_to_ack` covers successful, correlated private acknowledgements only; it excludes REST-only acceptance, snapshots, fills without a New/Replace acknowledgement, and cancel acknowledgements. A command that never reaches HTTP execution contributes no send sample. One send sample is recorded per attributed command, not per retry; acknowledgement latency includes retry time if the action eventually acknowledges.

Inspect sample counts before interpreting percentiles. In quiet markets most samples may originate from real REST poll responses rather than WebSocket updates; these are both included in the aggregate end-to-end histograms. Sample collection can pause if feeds, market eligibility, or risk checks block orders. Do not average window percentiles into a session percentile; use the since-start histogram for that. Neither metric establishes when the order becomes visible on the public book. See [metrics.md](metrics.md) for exact attribution, expiry, and aggregation rules.
