# Polymarket US: connecting and reading data

What we know about getting data from **Polymarket US**: hosts, authentication, discovery, the markets WebSocket (books and trades), the private WebSocket, live match state, and the latency and behaviour we measured. Status: as of 2026-10-23.

- **Venue API reference:** [`README.md`](README.md) and its siblings, a distilled copy of [docs.polymarket.us](https://docs.polymarket.us/getting-started/welcome). It includes a table of [where the docs are wrong](README.md#where-the-docs-are-wrong-or-inconsistent).
- **This page:** what we observed live. When this page and the venue docs disagree, this page records the live behaviour.

## Contents

1. [Scope: which API](#1-scope-which-api)
2. [Hosts and authentication](#2-hosts-and-authentication)
3. [Discovery](#3-discovery)
4. [Markets WebSocket: books](#4-markets-websocket-books)
5. [Markets WebSocket: trades](#5-markets-websocket-trades)
6. [Private WebSocket and REST account reads](#6-private-websocket-and-rest-account-reads)
7. [Live game state](#7-live-game-state)
8. [Rate limits and request budget](#8-rate-limits-and-request-budget)
9. [Latency](#9-latency)
10. [Volume and update rates](#10-volume-and-update-rates)
11. [Historical data (FIX vs WebSocket)](#11-historical-data-fix-vs-websocket)
12. [Open questions](#12-open-questions)
13. [Sources](#13-sources)

## 1. Scope: which API

Polymarket US has two API families, and **we can use only the Retail API**.

| | Retail API (ours) | Institutional / Exchange API |
| --- | --- | --- |
| Hosts | `api.polymarket.us`, `gateway.polymarket.us` | `api.prod.polymarketexchange.com`, gRPC, FIX |
| Auth | Ed25519-signed headers | Auth0 OAuth |
| Access | API key after KYC ([polymarket.us/developer](https://polymarket.us/developer)) | Signed Entity Participant and Clearing Member Agreement, Institutional registration, Polymarket review, AWS PrivateLink |

- **FIX is not an option.** We don't have the agreement. FIX rows in the historical Parquet export are backtest data only ([§11](#11-historical-data-fix-vs-websocket)).
- **The Trader Guide describes the Institutional API**, even though its pages look general. Don't copy endpoints, limits or auth from it ([details](README.md#scope)).
- **The international Polymarket** (CLOB, `docs.polymarket.com`) is a different venue. Its sports WebSocket was checked for darts and has none ([§7](#7-live-game-state)).

## 2. Hosts and authentication

| Purpose | URL | Auth |
| --- | --- | --- |
| Public REST: events, markets, books | `https://gateway.polymarket.us` | none |
| Trading REST: orders, portfolio, account | `https://api.polymarket.us` | signed |
| Markets WebSocket | `wss://api.polymarket.us/v1/ws/markets` | signed handshake |
| Private WebSocket | `wss://api.polymarket.us/v1/ws/private` | signed handshake |

**Signing** ([venue docs](README.md#authentication)):
- Each request carries three headers:
  - `X-PM-Access-Key`: the key ID.
  - `X-PM-Timestamp`: Unix time in milliseconds, within 30 s of server time.
  - `X-PM-Signature`: `base64(Ed25519.sign(timestamp + METHOD + path))`.
- The private key is the first 32 bytes of the base64-decoded secret.
- For a socket, the signed string is `GET` plus the socket path.
- Credentials come only from `.env` (`POLYMARKET_KEY_ID`, `POLYMARKET_SECRET_KEY`).
- A bad signature returns HTTP 401 `Invalid API key signature`. Retrying won't help; treat it as fatal.

**Message format on both sockets:** camelCase fields and string enums (e.g. `"SUBSCRIPTION_TYPE_MARKET_DATA"`). The WebSocket overview page shows snake_case fields and numeric types. That format is stale and does not work.

**Connection handling:**
- **Liveness is ping/pong.** The server sends its own pings about every 10 s and answers client pings. No `{"heartbeat":{}}` message has been observed.
- **Reconnect** with exponential backoff and jitter, and resubscribe. The first message after resubscribing is again a full book.

## 3. Discovery

**Request:** `GET https://gateway.polymarket.us/v2/sports/darts/events?limit=100&offset=N&type=sport` (and `type=futures` for futures). Page until a short page comes back, keep markets that are `active && !closed`, and deduplicate by slug ([venue docs](market-data.md#discovery-get-v2sportsslugevents-public)).

**Per-market fields worth reading:**
- `slug`
- `sportsMarketType` (`darts_match_winner` for match markets)
- `orderPriceMinTickSize`: 0.01 on every darts market seen
- `minimumTradeQty`: 0.01 contracts
- `feeCoefficient`: 0.0695
- `gameStartTime`
- `marketSides[]`: `long: true` marks the YES side, and `team.ordering` is `home` or `away`

**Number types:** tick size, minimum quantity and fee arrive as JSON numbers. Parse them through their text into a decimal type so they stay exact; don't use floats.

**Event fields used for game state:**
- `id`: a string in the list response; may also appear as a number
- `startTime`
- `live`, `ended`, `score`, `period`
- `eventState` ([§7](#7-live-game-state))

**Coverage check (2026-10-22, against the official SDK):**
- The retail list call returns **67 events and 84 markets** (66 match-winner, 18 futures).
- `GET /v1/events/{id}` and search find nothing extra.
- Since 2026-10-15, list endpoints return only each event's primary markets. Match-winner is primary, so darts discovery is unaffected ([changelog note](market-data.md#discovery-get-v2sportsslugevents-public)).

**Orientation:** the docs say the first team in a two-sided slug is YES. Don't parse slugs; use `marketSides[].long`. In every darts market sampled, the long side's team had `ordering: "home"` and appeared first in the slug.

**New listings:** markets appear over time, so discovery has to be re-run periodically rather than once at startup.

## 4. Markets WebSocket: books

**Subscribe** with one frame per type, at most 100 slugs per subscription (more subscriptions on the same socket beyond that):

```json
{ "subscribe": { "requestId": "md-0", "subscriptionType": "SUBSCRIPTION_TYPE_MARKET_DATA",
                 "marketSlugs": ["aec-…"], "responsesDebounced": false } }
```

[Venue docs](market-data.md#markets-websocket-wssapipolymarketusv1wsmarkets).

**Behaviour observed:**

| Fact | Observed | Implication |
| --- | --- | --- |
| Message content | Every `marketData` frame is a **full book** with `state`, `stats` and `transactTime`, not a delta | Validate, then replace atomically. Ignore a frame with an older `transactTime`. |
| Seeding | The first frame per ticker after subscribing is the full book | No REST seed needed. REST books are for debugging. |
| Depth | No cap seen: up to 7 levels per side, matching REST exactly | Depth is whatever the socket sends |
| Updates | Sent only on change | A quiet ticker is not stale. Judge staleness from connection state, not per-ticker silence. |
| Readiness | 15 tickers: all books within 0.15 s. 67 tickers (2026-10-22): all seeded 67 ms after subscribe. | A seed timeout of a few seconds is generous |
| Heartbeat JSON | `{"heartbeat":{}}` never seen | Use ping/pong liveness |
| `responsesDebounced` | Not yet measured | Measure before enabling ([§12](#12-open-questions)) |
| Precision | Prices and quantities are decimal strings with up to 4 decimals; `qty` can be fractional | Use fixed-point 10⁻⁴ and validate against the tick |
| Market states | Only `MARKET_STATE_OPEN` seen in captures | Treat anything else as "don't quote" |

## 5. Markets WebSocket: trades

The trade subscription is `SUBSCRIPTION_TYPE_TRADE`. Findings from captures on 2026-10-21 and 2026-10-22: 18 frames from 9 aggressive orders, all pre-match or futures.

**Frame shape (live):**

```json
{"trade":{"marketSlug":"…","price":{"value":"0.1500","currency":"USD"},"quantity":{"value":"0.0100","currency":"USD"},
 "tradeTime":"2026-10-21T03:46:16.483269046Z",
 "maker":{"side":"ORDER_SIDE_SELL","intent":"ORDER_INTENT_UNDEFINED","outcomeSide":"OUTCOME_SIDE_UNSPECIFIED","action":"ORDER_ACTION_UNSPECIFIED"},
 "taker":{"side":"ORDER_SIDE_BUY","intent":"ORDER_INTENT_SELL_SHORT","outcomeSide":"OUTCOME_SIDE_NO","action":"ORDER_ACTION_SELL"},
 "id":"D7NKT28G6ZRM","state":"TRADE_STATE_NEW"}}
```

The documented shape has only `side` and `intent` per party. The live frame also has `outcomeSide`, `action`, `id` and `state`.

| Fact | Value |
| --- | --- |
| Direction | **Only `taker.side`** gives direction in YES terms. It matched the hit side in 18/18 frames. `intent`, `outcomeSide` and `action` are the user's view (selling NO is a YES buy). Maker fields can be `UNDEFINED`. |
| Frames per aggressive order | One per resting order filled, all with the same `tradeTime`. 6 of 9 orders crossed 2–3 price levels. |
| `id` | Unique per fill; use it to deduplicate |
| `state` | Only `TRADE_STATE_NEW` seen |
| Which book reflects the trade | The first book with `transactTime ≥ tradeTime`: equal in 5/9 cases, 14–57 ms later in 4/9. Quantities matched exactly. |
| Arrival order | Trade first in 8 of 9 events. The book followed after a median of **+37 ms** (max +90 ms). Once the book came first (−3.6 ms). |

**Trades as an early book signal:** because trades usually arrive before the book that reflects them, a trade can be applied provisionally to the local book until the venue book confirms it. In a live check (2026-10-22, 19:17–19:22 UTC):
- 9 trades were applied provisionally; 8 were confirmed exactly by the next venue book.
- Confirmation lag was up to 86 ms.
- The 1 mismatch was a maker cancelling the rest of a level in the same update, not a misapplied trade.

## 6. Private WebSocket and REST account reads

Venue reference: [private-stream.md](private-stream.md).

| Subscription | Docs say | Observed (2026-10-21, official Python SDK 2.4.1) |
| --- | --- | --- |
| `SUBSCRIPTION_TYPE_ORDER_SNAPSHOT` | | `orderSubscriptionSnapshot{orders, eof}` within 0.3 s. **The only source of the open-order snapshot.** |
| `SUBSCRIPTION_TYPE_ORDER` | Starts with a snapshot | Updates only. `EXECUTION_TYPE_NEW` acks arrive on it. |
| `SUBSCRIPTION_TYPE_POSITION` | | Nothing for placing or cancelling orders. Not yet seen on a fill. |
| `SUBSCRIPTION_TYPE_ACCOUNT_BALANCE` | Starts with `accountBalancesSnapshot` | Nothing in 60 s on an idle account, nor for placing or cancelling orders |

**Startup sequence that works:**
- Subscribe `ORDER`, `POSITION` and `ACCOUNT_BALANCE`, then `ORDER_SNAPSHOT`.
- Take the starting balance and positions from REST: `GET /v1/account/balances`, and `GET /v1/portfolio/positions`, whose `positions` is a **map keyed by slug**.
- Send no orders until the order snapshot, balance and positions are all in, on every connection.

**Live order test (2026-10-21, 0.01 contracts, far from the touch):**
- One batched create was **acked on the private socket 40 ms later**.
- After cancel-all, both `EXECUTION_TYPE_CANCELED` frames came **within 15 ms**.
- `GET /v1/orders/open` confirmed nothing was left open.
- Buying power did not change while the orders rested, as the docs say.

**Protobuf defaults:** every execution carries `unsolicitedCancelReason: UNSOLICITED_CXL_REASON_UNDEFINED` and `orderRejectReason: ORD_REJECT_REASON_EXCHANGE_OPTION`, even on acks and our own cancels.
- Treat `_UNDEFINED` as absent.
- Read the reject reason only on `EXECUTION_TYPE_REJECTED`.

**No replay after a disconnect:** resync from the snapshot subscription plus REST.

## 7. Live game state

Investigated 2026-10-22 during the live match `pdcdarts-gerpri-robcro`.

| Source | Darts? | Verdict |
| --- | --- | --- |
| International sports WS `wss://sports-api.polymarket.com/ws` ([docs](https://docs.polymarket.com/market-data/realtime-data)) | **No**: 122 updates in 2 min across other sports, none for darts while a darts match was live | Not usable, and it is a different venue |
| US markets and private sockets ([overview](https://docs.polymarket.us/api-reference/websocket/overview.md)) | No game subscription type | Nothing to push |
| US sports data guide ([page](https://docs.polymarket.us/data-guide/sports-data.md)) | Reference data only | Not live |
| **US events REST** `GET gateway.polymarket.us/v2/sports/darts/events` | **Yes**, through `eventState` | Usable, by polling |

**Fields in `eventState`:**
- `score`: legs as `"home-away"`, e.g. `"5-1"`.
- `period`: `""` (scheduled), `NS`, `LIVE`, or `CAN` (cancelled).
- `live`, `ended`.
- `updatedAt`.
- `periodScores` and `marketGroupStats`: always empty so far.
- `sportradarGameId`, `eventId`.

There is no thrower, turn or per-set detail. Markets have `sportradarGameId: null`; the event has it.

**Timeliness (one sample):**
- **Poll delay:** the leg 5-1 → 5-2 had `updatedAt` 19:19:43.260, and a 2 s poll saw it 0.2 s later.
- **Market reaction:** the market first moved at 19:19:57, about **14 s after** the game state. The best bid of 0.81 was pulled, briefly 0.73, then 0.77.
- One sample is not evidence. The lead/lag needs measuring over a full session.

**Cancellation:** `CAN` matters because a match that does not start settles at **$0.50**. Quotes should be withdrawn on cancelled or ended matches.

**Polling cost:** one request per second while a match is live, which may share the IP rate limit with order traffic ([§8](#8-rate-limits-and-request-budget)).

## 8. Rate limits and request budget

[Venue docs](README.md#rate-limits).

- **The number is inconsistent across pages:** 25 req/s per IP on the rate-limits page; 20 req/s per key on the orders overview and in the changelog. Limits are counted per source IP and Cloudflare location.
- **Budget:** assume 20 req/s and run order output at about **15 req/s**, cancels first.
- **Batches:** up to 20 orders per request. No page says whether each entry is charged separately; `pmbot` charges one token per entry to be safe.
- **On 429:** wait at least 1 s, then back off exponentially.
- **`Global Rate Limit Exceeded` is not a rate limit.** It is the 5 s latency stopgap for new orders and replaces. Treat it as a transient reject and don't throttle. Pure cancels are never stopgapped.
- **Streams instead of polling:** books and trades come from the markets socket, and orders, positions and balances from the private socket. REST is for startup snapshots and resyncs, plus game state, which has no stream.

## 9. Latency

| Measure | Value | Source |
| --- | --- | --- |
| WebSocket ping RTT | Mostly **24–33 ms**, with outliers to 42 ms (2026-10-22). It was 58–135 ms on 2026-10-19 from another network. | Ping/pong timing |
| Exchange stamp → our receive, books | **p50 63 ms** | `transactTime` vs receive time, captures 2026-10-21/22 |
| Exchange stamp → our receive, trades | **p50 54 ms** | `tradeTime` vs receive time |
| Trade frame → book that reflects it | median **+37 ms**, max +90 ms; once the book came first (−3.6 ms) | Same local clock, so no clock skew |
| Order create → ack on the private socket | **40 ms** (n=1) | Live order test, 2026-10-21 |
| Cancel-all → cancel confirmations | **≤ 15 ms** (n=1) | Live order test |
| Game `updatedAt` → seen by poll | 0.2 s at 2 s polling (n=1) | 2026-10-22 |
| REST read (events endpoint) | p50 235 ms, max 645 ms (n=12) | `pmbot` paper runs, 2026-10-23 04:00 UTC |
| Exchange stamp → our receive, books (excluding subscribe snapshots) | p50 about +100 to +125 ms once corrected for a 314 ms local clock offset (n≈50, quiet hour) | `pmbot` `book_lag`, 2026-10-23 |

**Interpretation:** the exchange → receive delay (~55–65 ms) is about twice the RTT (~29 ms), so most of it is on the exchange side. We can't remove it, only use it.

**Clock caveats:** on 2026-10-23 the dev Mac's clock was **314 ms behind NTP** (`sntp time.apple.com`), which made every book look like it arrived before it was stamped. Check the offset before reading any cross-clock number. Earlier, our clock was within about 1 ms of NTP, yet a trade was received 11 ms before its `tradeTime`. Treat cross-clock numbers as ±20 ms, and never compare local and exchange times in trading logic. Order and join by exchange stamps only.

**Historical comparison:** in the Parquet export the receive − event latency was:
- FIX: p50 16 ms, p99 110 ms.
- WebSocket: p50 86 ms, p99 347 ms.

These come from different weeks and markets, so they are indicative only ([§11](#11-historical-data-fix-vs-websocket)).

## 10. Volume and update rates

**1-hour pre-match baseline**, 2026-10-21 02:57–03:57 UTC: 41 tickers (23 match + 18 futures), one connection, 0 reconnects.

| | Value |
| --- | --- |
| Book updates after the seed | **4,577** (≈ 1.3/s across all markets) |
| Changed the best price / best size only / deeper levels only | 197 (4%) / 700 (15%) / 3,680 (80%) |
| Peak burst | 44 updates in 1 s, 63 in 10 s |
| Gap between updates (all markets) | p50 0.30 s, p90 2.2 s, max 12.0 s |
| Trades | 6 trades, 5.02 shares, **$0.55**, all on one futures market |
| Cumulative notional reported by the venue | $34,515 (futures $33,289, match markets $1,226) |
| Raw capture size | 10.1 MB/hour |

- **Most updates are below the touch.** A few market makers move orders deeper in the book; only about 900 updates in the hour changed the best bid or ask.
- **Liquidity is thin.** PDC match books were often 44–88 cents wide before the match, and futures sat at 0.01/0.02.
- **The venue's `stats.notionalTraded` matches the trade sums.** For `w-lukhum` it went up by $0.5524, which equals the sum of the six trade frames exactly.
- **In-play rates are still to be measured.**

## 11. Historical data (FIX vs WebSocket)

- **The export holds two eras that never overlap:**
  - WebSocket L2 full books: futures only, 2026-07-29 → 2026-08-05.
  - FIX L3 order-by-order deltas: match and futures markets, 2026-08-05 → 2026-10-15.
- **All historical match-winner books are FIX.** We can backtest on them, but a live Retail bot only ever sees the WebSocket L2 feed with its extra lag.
- **Backtests should simulate that lag:** delay L2 publication relative to trades using the measured distribution (median 37 ms, max 90 ms).

See also [`../../data-analysis/README.md`](../../data-analysis/README.md).

## 12. Open questions

- In-play message rates, trade/book lag in play, and provisional-trade accuracy over a full session.
- What `responsesDebounced` changes (interval, payload, lag).
- Trade states other than `TRADE_STATE_NEW` (busts, corrections).
- Trades for hidden or iceberg quantity not on the visible book.
- The `period` code for a finished match, a suspension or a walkover; the score format for set play.
- **Score orientation** (`"home-away"` with home = first slug player = YES) is assumed, not verified.
- Game state lead/lag vs the market over ≥ 50 legs.
- Whether the events endpoint supports ETag or `If-Modified-Since`, to cut poll cost.
- The real rate limit (20 or 25 req/s, per key or per IP), whether a batch counts once, and whether the game poll shares the order budget.
- Whether `POSITION` and `ACCOUNT_BALANCE` push on a fill.
- Whether `wss://sports-api.polymarket.us/ws` (named in the task brief) carries darts game state.

## 13. Sources

- **Venue docs:**
  - [Welcome](https://docs.polymarket.us/getting-started/welcome)
  - [`llms.txt`](https://docs.polymarket.us/llms.txt)
  - [Markets WebSocket](https://docs.polymarket.us/api-reference/websocket/markets)
  - [Private WebSocket](https://docs.polymarket.us/api-reference/websocket/private)
  - [WebSocket overview](https://docs.polymarket.us/api-reference/websocket/overview.md)
  - [Sports API](https://docs.polymarket.us/api-reference/sports/overview)
  - [Get Market Book](https://docs.polymarket.us/api-reference/markets/get-market-book)
  - [Rate Limits](https://docs.polymarket.us/api-reference/rate-limits)
  - [Orders API](https://docs.polymarket.us/api-reference/orders/overview)
  - [Changelog](https://docs.polymarket.us/changelog)
  - [Status page](https://status.polymarketexchange.com)
- **Distilled copies:** [`README.md`](README.md) and siblings in this folder.
- **Data (gitignored):** `data-analysis/polymarket_us_darts_history.parquet`
