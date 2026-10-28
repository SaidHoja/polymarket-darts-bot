# Polymarket US API: what this bot needs

A distilled copy of [docs.polymarket.us](https://docs.polymarket.us/getting-started/welcome), cut down to the parts a darts trading bot needs. Pulled 2026-10-20 from the raw pages listed in [`llms.txt`](https://docs.polymarket.us/llms.txt). When this summary and the venue's live behaviour disagree, the live behaviour wins; known disagreements are listed [below](#where-the-docs-are-wrong-or-inconsistent).

| File | Covers |
| --- | --- |
| [market-data.md](market-data.md) | Discovery (events, markets, tick/min size), REST book, markets WebSocket |
| [trading.md](trading.md) | Order model (YES price, intents), order endpoints, batches, states, rejects, collateral, fees |
| [private-stream.md](private-stream.md) | Private WebSocket, positions, balances |

## Scope

Polymarket US has two API families. **This bot uses only the Retail API.**

| | Retail API (ours) | Institutional / Exchange API (ignore) |
| --- | --- | --- |
| Hosts | `https://api.polymarket.us` (authenticated), `https://gateway.polymarket.us` (public) | `api.prod.polymarketexchange.com`, gRPC, FIX |
| Auth | Ed25519 signed headers | Auth0 OAuth tokens |
| Docs | `/api-reference/*`, `/concepts/*`, `/fees`, `/market-structure/*` | `/trader-guide/*`, `/institutional/*`, `/streaming-endpoints/*`, `/grpc-api/*` |

The Trader Guide pages look general but describe the Institutional API. Don't copy endpoints, limits, or auth from them. Also out of scope: RFQs, combos, crypto markets, deposits/withdrawals, partners, the SDKs.

## Hosts

| Purpose | URL | Auth |
| --- | --- | --- |
| Public REST: events, markets, books, sports | `https://gateway.polymarket.us` | none |
| Trading REST: orders, portfolio, account | `https://api.polymarket.us` | signed |
| Markets WebSocket | `wss://api.polymarket.us/v1/ws/markets` | signed handshake |
| Private WebSocket | `wss://api.polymarket.us/v1/ws/private` | signed handshake |
| Status page | [status.polymarketexchange.com](https://status.polymarketexchange.com) | |
| Changelog RSS | `https://docs.polymarket.us/changelog/rss.xml` | |

There is no order-entry socket. Orders go over REST, and their outcomes arrive on the private socket.

## Authentication

Keys come from [polymarket.us/developer](https://polymarket.us/developer) after KYC in the app. You get a Key ID and a Secret Key, and the secret is shown once. Always sign in with the same method (Apple/Google/email), because switching can break key access.

Every authenticated REST request and both WebSocket handshakes carry:

| Header | Value |
| --- | --- |
| `X-PM-Access-Key` | Key ID (UUID) |
| `X-PM-Timestamp` | Unix time in **milliseconds**, within **30 s** of server time |
| `X-PM-Signature` | base64(Ed25519.sign(`timestamp + METHOD + path`)) |

- The private key is the first 32 bytes of the base64-decoded secret.
- `path` is the request path without host (e.g. `/v1/portfolio/positions`). For sockets it is `GET` plus `/v1/ws/markets` or `/v1/ws/private`.
- The query string is **not** part of the signed path: signing only `/v1/portfolio/positions` was accepted for `?limit=100` (verified live 2026-10-22).
- Bad signature: HTTP 401 `Invalid API key signature` (seen live).

## Rate limits

- Exceeding a limit gives `429 Too Many Requests` (`{"status":429,"message":"Too Many Requests"}`). Stop, wait **≥ 1 s**, then back off exponentially.
- The rate-limits page says limits are counted **per source IP and Cloudflare location**, so keys sharing an IP share counters.
- The limit number is inconsistent across pages (see below). Budget for **20 req/s**. Run at about 15 req/s with cancels first.
- Higher limits for automated systems: email support@polymarket.us with the use case, volume, and endpoints.
- Use streams instead of polling: private WS replaces polling open orders, positions, and balances, and markets WS replaces polling book and BBO. Cache market/event metadata.

### Latency stopgap (not a rate limit)

When the exchange is slow, any **new order or cancel/replace** that isn't processed within **5 s** is rejected with the message **`Global Rate Limit Exceeded`**. That message is misleading: **don't throttle** in response, treat it as a transient reject. **Pure cancels are never stopgapped**, and you can cancel an order before its ack arrives.

## Trading hours

- Nearly 24/7. Recurring maintenance is **Thursday 06:00–08:00 ET**, and extra windows are announced in the changelog (several in Sept 2026, both APIs). Emergency pauses can come without notice.
- The trading day rolls at **17:00 ET** (weekends too). `DAY` orders currently **do not** auto-cancel at the roll, so use `GOOD_TILL_DATE` instead.
- Execution history has been archived (wiped from queries) during maintenance before. Don't rely on REST history for past fills.

## Where the docs are wrong or inconsistent

| Topic | Docs say | Reality / what we do |
| --- | --- | --- |
| WebSocket message format | Overview page: snake_case fields, numeric `subscription_type` | **camelCase fields and string enums** (`"SUBSCRIPTION_TYPE_MARKET_DATA"`), as on the markets/private pages. Only these work. |
| Private order snapshot | `SUBSCRIPTION_TYPE_ORDER` starts with `orderSubscriptionSnapshot` | Live probe: the snapshot comes **only** from `SUBSCRIPTION_TYPE_ORDER_SNAPSHOT`. `ORDER` is updates-only. |
| Private balance snapshot | `ACCOUNT_BALANCE` sends `accountBalancesSnapshot` first | Live probe: nothing on an idle account. Get starting balance and positions from REST. |
| Heartbeats | Server sends `{"heartbeat":{}}` | Never seen. Server sends WebSocket pings about every 10 s and answers ours, so liveness is ping/pong ([observed](polymarket-connectivity.md#2-hosts-and-authentication)). |
| Retail rate limit | 25 req/s per IP (rate-limits page); 20 req/s per key (orders overview; changelog 2026-04-10) | Assume 20, run at 15. |
| Order states list | Private WS page omits `ORDER_STATE_NEW` | `ORDER_STATE_NEW` exists (orders overview: accepted and resting). |
| Execution types | Private WS page omits `EXECUTION_TYPE_NEW` | It exists (orders overview). |
