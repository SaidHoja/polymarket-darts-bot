# Working with the Polymarket US darts history

This folder contains `polymarket_us_darts_history.parquet`, a 390 MB Parquet export of public Polymarket US darts history and market metadata. The export metadata says timestamps are UTC, prices are USD probability units, sizes are contracts, and source fields are preserved in `source_record_json`. It also warns that different feed types are not deduplicated against one another.

## What is in the file

I inspected the Parquet footer locally. It reports 5,120,735 rows in 107 row groups and 22 columns. Most row groups have 50,000 rows; row groups 0–4 and 9 are smaller. The first row group includes market metadata. A sampled metadata record is a futures market, so do not assume each market is a single-match winner market. Row groups are storage chunks, not chronological partitions or match boundaries.

| Column | Meaning / use |
| --- | --- |
| `record_type`, `feed` | Record and source-feed classification. Inspect before interpreting `price`, `size`, or `value`; records can represent different event types. |
| `market_slug`, `event_slug` | Market and event identifiers. Join/group by stable IDs where present in source JSON; a slug alone may change. |
| `event_time` | Event timestamp from the source. |
| `sending_time` | Timestamp when the provider sent the message. |
| `wire_received_at` | Local capture receive timestamp; useful for observed delivery delay and replay ordering. |
| `dt` | Date partition column. It is not a substitute for event time. |
| `price`, `size` | Numeric market values when applicable. Price is in USD probability units; size is in contracts. Nulls are expected for record types that do not use them. |
| `side`, `action` | Feed-specific side/action. Confirm their values by `record_type` before assigning trading semantics. |
| `entry_id`, `order_id`, `trade_id` | Source identifiers where supplied; use for idempotency only within the source's documented semantics. |
| `msg_seq`, `entry_index` | Sequence/index fields where supplied. Preserve these for stable ordering and diagnose ties. |
| `is_snapshot_marker` | Snapshot boundary marker where supplied. Keep it when reconstructing books. |
| `stat_type`, `value`, `state` | Statistics/state payload fields; interpretation depends on feed and record type. |
| `source_record_json` | Original source record as JSON text. Retain it when normalizing so derived rows can be audited. |

The plan describes this as market-only history: it does not include paired darts score/turn game state. That means it can support market/book replay and baseline execution studies, but it cannot establish the predictive value of a game-aware model. `settlement`-like fields may appear inside metadata JSON, but do not assume outcomes are present or complete until the row-level report confirms them.

The report covers timestamp bounds, event/feed counts, market/event counts, and null counts. It does not calculate duplicate rates, within-range gaps, book completeness, or settlement coverage. Do not treat a min/max range as proof of continuous coverage.

## Inspection results

The generated report at [`history_inspection.json`](history_inspection.json) was produced with the inspector and PyArrow. It reports:

- **5,120,735 rows**. The inspector's `market_count` (1,405) and `event_count` (1,570) count distinct slugs across all rows, including metadata, so they overstate coverage:
  - 1,405 markets have a `market_metadata` record (1,259 `darts_match_winner`, 146 `futures`), but only **435 markets have any market data** (308 match-winner, 127 futures). That matches the brief's 435.
  - The 435 markets with data belong to 310 events by metadata `event_slug`. The `event_slug` format differs between metadata rows (e.g. `modus-...`) and FIX rows (e.g. `aec-modus-...`), which inflates the distinct-event count. Join on `market_slug`, not `event_slug`.
- `event_time` spans **2026-07-29 03:33:40.504967 UTC** to **2026-10-15 07:02:43.091940 UTC**. `wire_received_at` spans **2026-07-29 03:38:38.422370 UTC** to **2026-10-15 07:02:43.104687 UTC**. These are only bounds; the report does not establish internal continuity or verify the brief's August–September gap.
- By record type: 4,844,466 `book_change`; 240,687 `book_snapshot`; 19,930 `market_stat`; 14,247 `trade`; 1,405 `market_metadata`.
- By feed: 4,917,423 `fix`; 201,907 `websocket`; 1,405 `metadata`. The report shows both FIX and WebSocket snapshots/trades, so compare their schemas, overlap, ordering, and semantics before combining them.
- The `dt` and `wire_received_at` fields are null on 1,405 rows, matching the metadata count. `event_time` is null on 40,354 rows (about 0.8%). The snapshot marker is null on 5,081,786 rows; only 1,583 rows have it true. Nulls must be interpreted by record type rather than filled indiscriminately.

The inspector's top markets are predominantly 2027 PDC World Championship futures, followed by event-specific markets. The dataset therefore contains more than match-winner markets. Select market types and actual event dates explicitly before designing replay windows. The report does not measure duplicate rates, within-range gaps, book completeness, or settlement coverage.

## Setup and inspect

From the repository root, install PyArrow into your Python environment (or use an environment that already has it):

```sh
python3 -m pip install pyarrow
python3 data-analysis/inspect_history.py > data-analysis/history_inspection.json
```

The inspector reads selected columns in batches rather than loading all 390 MB and all JSON payloads into memory. It prints schema, row count, null counts, time bounds, counts by `record_type`/`feed`, snapshot-marker counts, and the most frequent markets. It intentionally does not deduplicate records or claim a canonical ordering.

For a reproducible evaluation, record the file checksum alongside the inspector output:

```sh
shasum -a 256 data-analysis/polymarket_us_darts_history.parquet
```

To point it at another export:

```sh
python3 data-analysis/inspect_history.py path/to/history.parquet --batch-size 100000 --top-markets 30
```

For a quick interactive query, DuckDB is convenient:

```sh
python3 -m pip install duckdb
duckdb -c "DESCRIBE SELECT * FROM read_parquet('data-analysis/polymarket_us_darts_history.parquet')"
duckdb -c "SELECT record_type, feed, count(*) FROM read_parquet('data-analysis/polymarket_us_darts_history.parquet') GROUP BY 1,2 ORDER BY 3 DESC"
```

Use projection and filters when querying this large file. For example, select only identifiers and timestamps for a time-range audit rather than selecting `source_record_json` on every query.

## Prepare replay data safely

1. Save the inspector output, source file checksum, code revision, and analysis configuration with each report.
2. Keep raw rows immutable. Normalize into typed events while carrying source row identity, `record_type`, `feed`, source timestamps, receive timestamp, `msg_seq`, `entry_index`, and the original JSON.
3. First segment by market/event and feed. Market metadata, book snapshots/updates, trades, and statistics are different record classes; do not treat all rows as price changes.
4. For replay, use the receive timestamp as the local observation clock and retain source event/send times for lag analysis. Prefer sequence/index fields when valid. When timestamps tie and no sequence establishes order, preserve the ambiguity and evaluate conservative orderings.
5. Rebuild a book only from confirmed snapshot boundaries and update semantics. Do not infer a full book from trades, top-of-book observations, or metadata.
6. Deduplicate within a feed using the source's event identity and update semantics. The export explicitly says feed types are not deduplicated against one another, so identical-looking rows across feeds may be intentional.
7. Keep markets and missing periods isolated. Do not carry quotes, inventory, or a strategy state from one unrelated event into another, and do not interpolate through a data gap.
8. Simulated fills require explicit latency, queue-ahead, partial-fill, cancellation-race, fee, and size assumptions. A quote touching a historical price is not evidence it would have filled.

When backtesting, split development and holdout chronologically by match, freeze parameters before holdout, compare no-trade and market-baseline strategies, and report fill sensitivity, costs, drawdown, exposure, and latency separately. Only paired sports/market sessions can validate score-conditioned prediction quality.

`export_backtest.py` applies these rules for the backtester: it writes one CSV per market (`backtest/<slug>.csv`, sorted by receive time, `msg_seq`, `entry_index`, deduplicated) plus `backtest/markets.json` with the source checksum, git revision and per-market metadata. See [architecture](../docs/architecture/architecture.md#backtesting).

## Book VWAP depth study

[`vwap_depth.py`](vwap_depth.py) compares 1, 3, 4, 5, 10 and 20 price levels per side using the existing exported FIX replays and root public WebSocket captures. It aggregates FIX orders by price after complete messages and samples observed books in 10-second buckets, without filling capture gaps. Its output, `vwap-depth-results.json` (gitignored, about 325 KB; regenerate with the command below), records input bounds/hashes, market-weighted and pooled statistics, and 30-second future-midpoint checks.

```sh
python3 -m unittest discover -s data-analysis -p test_vwap_depth.py
python3 data-analysis/vwap_depth.py --out data-analysis/vwap-depth-results.json
```

These results set the default of three levels. [`vwap-strategy-results.json`](vwap-strategy-results.json) preserves the exact comparison config and simulator summaries. A better depth-limited book statistic does not establish better trading performance.

## Suggested next analyses

- Verify actual time coverage and missing intervals using both `event_time` and `wire_received_at`.
- Count each `record_type`/`feed` pair and inspect representative `source_record_json` samples for every pair.
- Identify complete event/market histories, snapshot markers, sequence resets, timestamp ties, duplicate source IDs, and book gaps.
- Audit `price` ranges/ticks and `size` units by record type; audit metadata and settlement fields in the raw JSON separately.
- Select one liquid match for a first replay, then several matches with different liquidity and chronology. Make the market/event mapping explicit.


## Estimate peak volume and service load

[`estimate_load.py`](estimate_load.py) scans the history in batches and writes a
capacity-planning report, split by feed and metadata market type. This export has
**no WebSocket match-market rows**: `websocket/all` contains futures only.
`fix/darts_match_winner` supplies match-market activity, but FIX entry counts
describe a different workload and must not be interpreted as WebSocket messages.

```sh
venv/bin/python data-analysis/estimate_load.py --output data-analysis/load_estimate.json
# Optional sizing assumptions; supply a representative measured CPU cost:
venv/bin/python data-analysis/estimate_load.py --headroom 5 --cpu-us-per-row 100 --requote-fraction 0.1
venv/bin/python -m unittest discover -s data-analysis -p 'test_*.py'
```

The report includes:

- Maximum rows, book rows, source JSON bytes, trade count, contracts and
  trade `price * size` in UTC-aligned 1-second, 10-second, minute, hour and day
  buckets, with the timestamp of each peak.
- Calendar and nonempty-second averages, nonempty-second p99, activity gaps,
  missing timestamps, busiest markets and peak markets observed in a minute.
- A default **3× observed one-second peak** sizing scenario for input throughput,
  exported JSON byte rate and hypothetical order requests. The default order
  scenario assumes every book row causes a two-sided cancel-and-place (four
  requests). Adjust `--requote-fraction` and `--requests-per-requote` to model
  strategy behavior. CPU cores are calculated only if `--cpu-us-per-row` is set;
  `--cpu-utilization` defaults to 0.7.

The default clock is `wire_received_at`; use `--clock event_time` to compare
source activity with received bursts. Missing timestamps are counted and excluded
without substituting a different clock. Metadata is joined by `market_slug` in a
first pass so file order does not affect classification. Batch reading limits raw
payload memory; accumulated statistics grow with nonempty seconds and observed
market/minute pairs.

These are observed peaks, not a proven future maximum or a throughput benchmark.
Aligned windows can split bursts: a rolling window of the same length can contain
up to twice the aligned peak. Empty periods can reflect inactivity or missing
capture. Trade sums are raw recorded volume, **not deduplicated**; do not combine
feeds or treat these figures as unique venue volume, bot fills or revenue.
Source JSON bytes are a serialization-size proxy, not measured network bandwidth
or RAM. The sustained-peak daily storage scenario is deliberately hypothetical.
Retries, private order traffic and subscription/reconnect overhead need separate
measurement. Coalescing and unchanged-intent suppression in the service usually
reduce strategy and order traffic relative to the hypothetical scenario.

### Results from this export

The generated [`load_estimate.json`](load_estimate.json) accounts for all
5,119,330 non-metadata rows. With the default receive clock:

| Feed / scope | Peak rows in 1 s | Peak book rows in 1 s | Peak rows in 60 s | 3× input target / s |
| --- | ---: | ---: | ---: | ---: |
| WebSocket / futures (all available WS data) | 123 | 123 | 418 | 369 |
| FIX / match winners | 764 | 713 | 7,050 | 2,292 |
| FIX / futures | 209 | 190 | 1,366 | 627 |
| FIX / all | 764 | 713 | 7,050 | 2,292 |

Peaks in separate columns need not occur at the same time. The 3× exported-JSON
byte-rate proxy is 2.72 Mbit/s for WebSocket futures and 9.12 Mbit/s for FIX all.
The largest recorded daily trade `price * size` sums are $749.39 for WebSocket
futures and $14,351.91 for FIX match winners, without deduplication.
These observations provide starting points for replay/load tests; the service's
WebSocket match-market peak remains unmeasured by this export.

## In-play leg study

[`in-play-leg-study-2026-10-28/`](in-play-leg-study-2026-10-28) holds the scripts behind the [leg study](../docs/potential-improvements/strat-improvements/in-play-leg-study-2026-10-28/in-play-leg-study-2026-10-28.md). They read `captures/` rather than the Parquet history; the commands are in the study's Reproducing section.
