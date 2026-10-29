# `pm-venue`

`pm-venue` is the Rust adapter for the Polymarket US Retail API: request signing, JSON decoding into `pm-types`, a one-shot REST client with a shared rate limiter, and signed WebSocket handshakes and subscription messages. It knows nothing about strategies, risk, or order state: callers in [`pm-live`](pm-live.md) own reconnects, retries, and priorities. See [the repository overview](architecture.md) for where it sits.

Dependencies are `pm-types`, `serde_json` (with `arbitrary_precision`), `reqwest` (rustls, HTTP/2 by ALPN), `tokio-tungstenite`, `ed25519-dalek`, and `base64`. `pm-core` uses it only as a dev-dependency, for decoding real fixtures in tests and benches.

## Modules

| Module | Responsibility |
| --- | --- |
| [`auth`](../../crates/pm-venue/src/auth.rs) | Credential loading and Ed25519 request signing |
| [`decode`](../../crates/pm-venue/src/decode.rs) | Venue JSON → `pm-types` frames; reject classification |
| [`rest`](../../crates/pm-venue/src/rest.rs) | `RestClient`, `RateLimiter`, `Budget`, order request bodies |
| [`ws`](../../crates/pm-venue/src/ws.rs) | Socket URLs, signed connect, subscription messages |

## Authentication

`load_credentials` reads `POLYMARKET_KEY_ID` and `POLYMARKET_SECRET_KEY`, preferring the process environment over a `.env` file (a minimal parser that accepts `export` prefixes and quoted values). Neither `Credentials` nor `Signer` prints the secret in its `Debug` output, and `pmbot` drops `Credentials` once the signer is built.

`Signer` takes the first 32 bytes of the base64-decoded secret as the Ed25519 seed. `headers(method, path)` signs `timestamp_ms + METHOD + path` (path only, no host or query string) and returns the three `X-PM-*` headers. A unit test pins the signature against a known reference value.

## Decoding

`decode` turns raw text into typed frames and is the only place venue field names appear.

- **Market socket:** `decode_market_frame` → `MarketMsg::{Book, Trade, SubscriptionError, Other}`. Books, the hottest path, first try a borrowed fast path that deserializes straight into `&str`-holding wire structs with no `Value` tree and no per-level allocation; each side's levels are parsed element by element into the final `Vec<Level>`. Anything it doesn't decode exactly (another message type, a number where text was expected, escapes, duplicate keys) falls back to `decode_market_frame_slow`, which produces the same result or the same error.
- **Private socket:** `decode_private_frame` → `PrivateMsg::{Snapshot { orders, eof }, Execution, Position, Balance, SubscriptionError, Other}`. Snapshot parts accumulate in the caller until `eof`. A frame carrying `error` is a `SubscriptionError` whatever else it holds: a failed snapshot subscription arrives with an empty `eof` snapshot attached (seen as `Unauthenticated`), which must not read as "no open orders".
- **REST payloads:** balances, paged positions (`decode_positions_page` returns the cursor and `eof`), open orders, single orders, and `decode_events_page`, which yields both tradable markets (`active && !closed`) and game states from one discovery page. The venue sends empty results as explicit empty containers (`"orders": []`, `"positions": {}`, `"events": []`), so a missing or wrongly typed container is an error, never an empty account or market list.
- **Rejects:** `classify_reject(reason, text)` maps venue reason codes and free text onto `RejectClass`. The text `Global Rate Limit Exceeded` is classified `Transient` (the venue's latency stopgap), not `RateLimited`, which is reserved for HTTP 429.

Decoding conventions:

- Numbers go through their text, so ticks and quantities stay exact even when sent as JSON numbers.
- Unknown enum strings become `Unknown`; a frame that can't be decoded returns `Err`. The markets socket logs and drops it; the private socket reconnects, so a dropped execution can't silently lose a fill.
- A numeric field that is present but unreadable is an error, never a zero; an absent one is a protobuf default (zero). A `FILL`/`PARTIAL_FILL` execution needs a positive `lastShares`; a zero or absent `lastPx` falls back to the order price.
- Positions prefer the `*Decimal` fields. The venue's positive held-outcome purchase cost is normalized to signed YES cash paid as `cost + min(net, 0) - realized`, so NO positions and earlier realized PnL enter the core with the same basis as fills. Execution reject reasons are read only on `Rejected` executions, because protobuf defaults put a reason on every execution.
- A tick or minimum quantity that is present but unusable (unparseable, more than 4 decimals, not positive) skips the market with a warning. An absent one defaults to 0.01, also with a warning. League is taken from the event's league or series fields, falling back to the event-slug prefix.
- Before a match, `eventState` is null, and `decode_game` reads the summary fields from the event itself.

## REST client

`RestClient::request` sends exactly one request and reports what happened: a decoded `Value` or a `RestError` (`Status`, `Transport`, `Decode`). Its `Display` truncates a long error body on a character boundary. `Transport` explicitly means the request may have taken effect. There are no retries here. Signed calls go to `api.polymarket.us`, public reads to `gateway.polymarket.us`. The `reqwest` client has a 10-second timeout, a 90-second idle pool, and `TCP_NODELAY`. `RestClient::new` returns an error rather than panicking if the client can't be built.

Typed helpers wrap the endpoints the bot uses:

| Group | Calls |
| --- | --- |
| Public reads | `darts_events` (pages of 100 until a short page), `market` |
| Account reads | `balance`, `positions` (cursor-paged; a cursor that doesn't advance is an error), `open_orders`, `order` (`None` on 404; a 200 without an order is an error) |
| Orders | `create_orders` (single or batched), `modify_orders`, `cancel_orders`, `cancel_all` |

Order calls return only what the venue echoes: created IDs for creates, nothing for modify/cancel. Outcomes arrive on the private socket. `BATCH_LIMIT` is 20; an empty or oversized batch is refused with an error before sending. `events_page` and `created_ids` pull the events array and created IDs out of a response without copying it. `order_body` and `modify_body` build the wire shape: the YES price as an `Amount`, the quantity as an exact JSON number, `participateDontInitiate` for post-only, and good-till-date with a whole-second `goodTillTime`.

`with_dispatch_observer` returns a request-scoped view of the client sharing its pool and limiter; the observer runs after signing, immediately before the HTTP call, so `pm-live` can time the build stage of a single request.

## Rate limiter

One `RateLimiter` token bucket is shared by every REST caller in the process, because the venue counts limits per source IP. Capacity equals the rate (`pmbot` uses 15/s).

- `Budget` says who pays, through `RateLimiter::charge(budget, cost)`: `Normal` takes one token per order action (one per batch entry, taken one at a time so a 20-entry batch fits a 15-token bucket); `Low` (game polls, reconciliation reads) does the same but waits until three tokens of headroom would remain for order traffic; `Prepaid` means the caller already paid and only waits out a 429 back-off. The order gateway prepays one token per action as it builds a batch; its single retry goes through `Normal`.
- `try_acquire` takes a token only if one is available now; the gateway uses it to grow batches.
- A real HTTP 429 calls `drain`: a shared exponential cooldown (1, 2, 4, …, 32 s) that every caller waits out, plus an emptied bucket. A success only clears the escalation once the cooldown has passed, so an in-flight success can't cancel it.
- Proactive throttling can be disabled (`with_enabled(rate, false)`, driven by `[execution].rate_limit_enabled`) for latency measurement. The 429 cooldown stays active regardless. `new`/`with_enabled` return an error for a non-finite rate or one not above the reserve of 3.

`utilisation()` and `http_429_total()` feed status and metrics. See [venue rate limits](../polymarket-docs/polymarket-connectivity.md#8-rate-limits-and-request-budget) for the conflicting published limits.

## WebSockets

`connect_signed` performs the handshake with the signed `GET` headers and reports HTTP 401 as `ConnectError::Unauthorized`, so callers can wait longer instead of retrying immediately. `market_subscriptions` emits book and trade subscriptions in groups of at most 100 slugs, with debouncing turned off for books. `private_subscriptions` emits order, position, and balance update streams first and the order snapshot last, the order observed to work live. Pinging, liveness timeouts, reconnection, and resubscription belong to the caller.

## Tests, benches, and examples

Inline tests cover signing against a reference signature, the order body shape, the limiter's disabled mode, 429 behavior and per-action batch charging, decoding of every frame in `tests/fixtures/market_frame.jsonl` (including fast-path/slow-path agreement), and the rejection rules above: auth-error snapshots, malformed fills and order sizes, implicit empty REST snapshots, unusable increments, and long non-ASCII error bodies. [`benches/venue.rs`](../../crates/pm-venue/benches/venue.rs) measures per-frame decode (book, trade, execution, fill, position), discovery-page handling (using the public page in `tests/fixtures/events_page.json`), created-ID parsing, signing, and request bodies (`cargo bench -p pm-venue`). [`examples/http_ab.rs`](../../crates/pm-venue/examples/http_ab.rs) compares HTTP/1.1 and HTTP/2 latency against public reads, timing only successful responses and reporting the negotiated versions; it found no clear difference, and the client stays on HTTP/2.
