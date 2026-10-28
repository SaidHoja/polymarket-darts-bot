# Polymarket US darts

A Rust market-making bot for darts match-winner markets on Polymarket US (Retail API), a small Python operator tool, a backtester that replays the same strategy code against recorded history, and the research behind both.

## What is where

| Path | What it is | Start here |
| --- | --- | --- |
| [`crates/`](crates) | The Rust workspace (see below) | [Architecture](docs/architecture/architecture.md) |
| [`pmus/`](pmus) | Small Python operator tool on the official [`polymarket-us`](https://docs.polymarket.us/api-reference/sdks/python/quickstart) SDK: inspect the account, place or cancel orders by hand, record raw frames, and the dead-man cancel-all | `python -m pmus --help` |
| [`config/`](config) | TOML configs for the bot, latency probe and backtests. `*.example.toml` are the documented templates; the rest are named run configs | [`pmbot.example.toml`](config/pmbot.example.toml) |
| [`docs/`](docs) | How things work *now*: architecture per crate, venue notes, risk, backtesting, metrics, rate limits, operational guides. Also [`potential-improvements/`](docs/potential-improvements) for work designed but not built, including strategy studies | [Architecture](docs/architecture/architecture.md) |
| [`docs/polymarket-docs/`](docs/polymarket-docs) | Distilled venue API docs plus what we observed live. Live behaviour wins over the venue's docs | [README](docs/polymarket-docs/README.md) |
| [`investigations/`](investigations) | Dated test plans and one-off studies. Scripts behind studies written up in `docs/` live in [`data-analysis/`](data-analysis) | |
| [`data-analysis/`](data-analysis) | Python tooling for the Parquet history: inspection, load estimates, the backtest export, strategy research | [README](data-analysis/README.md) |
| [`tests/fixtures/`](tests/fixtures) | Captured venue frames used by Rust tests and benches | |

Gitignored, created locally: `captures/` (journals from `pmbot`), `backtests/` (backtest output), `data-analysis/backtest/` (exported replay data), `run/` (admin sockets), `logs/`, `target/`, `venv/`, `.env`.

### Crates

| Crate | Role |
| --- | --- |
| `pm-types` | Shared types: fixed-point prices and quantities, timestamps, venue enums, the `Event`s the core consumes and the commands it emits |
| `pm-venue` | Venue adapter: Ed25519 signing, JSON decoding, REST, rate limiting, WebSocket handshakes. Knows nothing about strategy |
| `pm-core` | The deterministic core: a pure `tick(clock, events) → commands` over market data, game state, strategies, risk and order lifecycle. No I/O |
| `pm-live` | The `pmbot` binary: feeds, gateway, journals, metrics, admin socket, operator UI, watchdog, shutdown |
| `pm-backtest` | The `pm-backtest` binary: replays history through `pm-core` against a simulated exchange |

Dependencies point inwards: `pm-live` → `pm-core` + `pm-venue` → `pm-types`, and `pm-backtest` → `pm-core`. Because the core does no I/O, the backtester runs exactly the code the live bot runs.

## Where changes go

- **Strategy logic:** behind the `Strategy` trait, with a `StrategyConfig` variant ([`pm-core/src/strategy/`](crates/pm-core/src/strategy)). Every parameter gets a backtested default.
- **Venue wire format:** in `pm-venue` decoding and the shared types in `pm-types`. The core never sees raw JSON.
- **New inputs:** feed typed events into the core from the `pm-live` shell. Don't mutate core state directly.
- **Docs:** update `docs/` when behaviour changes. Leave dated studies and `investigations/` as dated records.

## Conventions

- All prices are YES prices. A NO price X is sent as 1 − X.
- Venue numbers are parsed as fixed-point integers, never as floats.
- Orders are confirmed only by the private socket, never by REST responses.
- Credentials come from the environment or `.env` (`POLYMARKET_KEY_ID`, `POLYMARKET_SECRET_KEY`) and are never logged.
- Config keys you leave out use their defaults, and unknown keys are errors.

## Running

```sh
cargo build --release -p pm-live
./target/release/pmbot --mode record                              # feeds and journals only, no orders
./target/release/pmbot --mode paper                               # real data, local fake gateway that never fills
./target/release/pmbot --mode live --config config/pmbot.toml     # real orders
```

Add `--ui 127.0.0.1:8787` for the operator UI. The admin socket takes `status`, `metrics`, `pause [slug]`, `resume [slug]` and `cancel-all` (`echo status | nc -U run/pmbot.sock`). The daily kill, the day's PnL, market losses and indefinite pauses survive a restart in `run/risk-state-<mode>.json` (`--risk-state`). See [`pm-live`](docs/architecture/pm-live.md) for shutdown, the watchdog, the risk state file and journals, and [metrics](docs/metrics.md) for the latency stages.

Backtesting (details in [architecture](docs/architecture/architecture.md#backtesting) and [metrics](docs/metrics.md)):

```sh
python3 -m venv venv && venv/bin/pip install -r requirements.txt
venv/bin/python data-analysis/export_backtest.py       # once: Parquet → data-analysis/backtest/
cargo build --release -p pm-backtest
./target/release/pm-backtest audit
./target/release/pm-backtest run --config config/backtest.example.toml --strategy config --fill-model base,pessimistic
```

### `pmus`

```sh
python3 -m venv venv && venv/bin/pip install -r requirements.txt
venv/bin/python -m pmus markets [--include-futures]
venv/bin/python -m pmus book SLUG
venv/bin/python -m pmus balances | positions | orders [SLUG...]
venv/bin/python -m pmus order SLUG buy-yes 0.42 1 [--allow-take] [--gtd S] [-y]   # post-only, GTD 1 h, asks to confirm
venv/bin/python -m pmus cancel-all [SLUG...] [-y]                                  # then checks nothing is left open
venv/bin/python -m pmus record [--private] [--include-futures] [--duration S]     # captures/<date>.jsonl
```

Prices are for the side you name (`buy-no 0.95` is sent as YES 0.05). `pmus.deadman.lambda_handler` is the dead-man switch from [AWS deployment](docs/potential-improvements/aws-deployment.md).

## Tests

```sh
cargo test --workspace
cargo bench -p pm-core    # and -p pm-venue
```
