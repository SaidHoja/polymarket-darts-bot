# Market data

Sources: [Events & Markets](https://docs.polymarket.us/concepts/events-and-markets), [Markets API](https://docs.polymarket.us/api-reference/market/overview), [Sports API](https://docs.polymarket.us/api-reference/sports/overview), [Get Market Book](https://docs.polymarket.us/api-reference/markets/get-market-book), [Markets WebSocket](https://docs.polymarket.us/api-reference/websocket/markets), [Changelog](https://docs.polymarket.us/changelog).

## Model

**Series** (league/season) → **Event** (one match, with a start time and participants) → **Market** (one yes/no question, settles at $1.00 or $0.00). A market's **slug** (e.g. `aec-nfl-kc-phi-2026-02-09`) identifies it everywhere: orders, books, subscriptions.

- In a two-sided game slug, the **first team is the long/YES side** and the second is the short/NO side. Don't parse slugs, though: use `marketSides[].long`.
- Darts match markets have `sportsMarketType = "darts_match_winner"` (added 2026-09-08). `sportsMarketTypeV2` is the coarse `MONEYLINE` / `SPREAD` / `TOTAL` / `PROP`.

## Discovery: `GET /v2/sports/{slug}/events` (public)

Darts discovery uses `GET https://gateway.polymarket.us/v2/sports/darts/events`.

| Query | Meaning |
| --- | --- |
| `limit`, `offset` | Pagination |
| `type` | `sport` (default, i.e. games) or `futures` |
| `section` | `general` (default) or `trending` |
| `excludeEventId` | Array of event ids to skip |

`GET /v2/leagues/{slug}/events` is the same, per league.

**Since 2026-10-15, list endpoints return only each event's primary markets** (main winner outcome). This applies to `/v1/events`, `/v2/leagues/{slug}/events`, and `/v2/sports/{slug}/events`. For an event's full market set, use `GET /v1/events/{id}` or `GET /v1/events/slug/{slug}`. Match-winner markets are primary, so darts discovery is unaffected.

### Event fields worth knowing

`slug`, `title`, `markets[]`, `startTime`, `eventDate`, `endDate`, and live state: `live`, `ended`, `score`, `period`, `elapsed` (all nullable). These are the only game-state feed in the Retail API docs.

### Market fields

| Field | Use |
| --- | --- |
| `slug`, `question`, `active`, `closed`, `archived` | Identity and status. Trade only `active && !closed`. |
| `orderPriceMinTickSize` | Price increment in dollars (`0.01`, or `0.005` = half-cent). **Read it per market and never assume 1¢.** |
| `minimumTradeQty` | Minimum order size in **contracts** (`0.01` = 1% of a contract). Sizes are multiples of it. |
| `feeCoefficient` | Taker fee Θ for this market (see [trading.md](trading.md#fees)) |
| `gameStartTime` | Game start (useful for deriving GTD expiry) |
| `marketSides[]` | `{ id, identifier, description, long, participantId, marketSideType }`. `long: true` is the YES side. |
| `sportsMarketType`, `sportsMarketTypeV2`, `gameId`, `line` | Sports classification |
| `bestBid`, `bestAsk`, `lastTradePrice`, `spread`, volume/liquidity fields | Snapshot stats only. Use the socket for live prices. |

Single-market lookups (public): `GET /v1/market/slug/{slug}`, `GET /v1/market/id/{id}`, and `GET /v1/markets` with filters (`active`, `closed`, `categories`, `sportsMarketTypes`, `gameId`, `slug[]`, date ranges, `limit`/`offset`).

## REST book: `GET /v1/markets/{slug}/book` (public)

Debug only. The live book comes from the socket. The response is `{ marketData: MarketData }`, the same shape as the socket's `marketData`:

```json
{
  "marketSlug": "…",
  "bids":   [{ "px": { "value": "0.555", "currency": "USD" }, "qty": "0.50" }],
  "offers": [{ "px": { "value": "0.560", "currency": "USD" }, "qty": "0.80" }],
  "state": "MARKET_STATE_OPEN",
  "stats": { "lastTradePx": {…}, "sharesTraded": "…", "openInterest": "…", "highPx": {…}, "lowPx": {…}, "settlementPx": {…}, "currentPx": {…}, … },
  "transactTime": "2024-01-15T10:30:00Z"
}
```

- Levels are best-first: highest bid first, lowest offer first.
- `qty` is a decimal string and can be fractional.
- Prices are YES prices.
- `stats.lastPriceSample` has been unsupported since 2026-07-03. Don't read it.

Also available: `GET /v1/markets/{slug}/bbo` (lightweight best bid/offer, uses `longQuote`/`shortQuote`) and `GET /v1/markets/{slug}/settlement`.

## Markets WebSocket: `wss://api.polymarket.us/v1/ws/markets`

Needs a signed handshake (see [README](README.md#authentication)). Messages are JSON with **camelCase** fields and **string** enums.

### Subscribe

```json
{ "subscribe": {
    "requestId": "md-sub-1",
    "subscriptionType": "SUBSCRIPTION_TYPE_MARKET_DATA",
    "marketSlugs": ["slug-1", "slug-2"],
    "responsesDebounced": false } }
```

- **At most 100 markets per subscription.** Use more subscriptions on the same socket beyond that.
- `responsesDebounced: true` batches updates at regular intervals instead of sending on every change (interval undocumented).
- Unsubscribe with `{ "unsubscribe": { "requestId": "<original id>" } }`.
- A failed subscribe returns `{ "requestId": "…", "error": "…" }`.

| `subscriptionType` | Payload key | Content |
| --- | --- | --- |
| `SUBSCRIPTION_TYPE_MARKET_DATA` | `marketData` | Full book + stats, same shape as REST above |
| `SUBSCRIPTION_TYPE_MARKET_DATA_LITE` | `marketDataLite` | `currentPx`, `lastTradePx`, `bestBid`, `bestAsk`, `bidDepth`, `askDepth`, `sharesTraded`, `openInterest` |
| `SUBSCRIPTION_TYPE_TRADE` | `trade` | `price`, `quantity`, `tradeTime`, `maker{side,intent}`, `taker{side,intent}` |

Every `marketData` message is a **full book, not a delta**. The first one per ticker is the seed. Messages arrive only on change, so a quiet market is not a stale one ([observed](polymarket-connectivity.md#4-markets-websocket-books)).

### Market states

| State | Meaning |
| --- | --- |
| `MARKET_STATE_OPEN` | Trading |
| `MARKET_STATE_PREOPEN` | Listed, trading not started |
| `MARKET_STATE_SUSPENDED` | Temporarily paused |
| `MARKET_STATE_HALTED` | Stopped |
| `MARKET_STATE_MATCH_AND_CLOSE_AUCTION` | Closing auction |
| `MARKET_STATE_EXPIRED` / `MARKET_STATE_TERMINATED` | Done |

### Liveness

The docs describe a `{"heartbeat":{}}` message, but none has been observed. The server sends WebSocket pings about every 10 s and answers client pings. Reconnect with exponential backoff and resubscribe; the first message after resubscribing is again a full book.
