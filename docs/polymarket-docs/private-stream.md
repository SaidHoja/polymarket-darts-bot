# Private stream, positions, balances

Sources: [Private WebSocket](https://docs.polymarket.us/api-reference/websocket/private), [Get User Positions](https://docs.polymarket.us/api-reference/portfolio/get-user-positions), [Get Account Balances](https://docs.polymarket.us/api-reference/account/get-account-balances), [Rate Limits](https://docs.polymarket.us/api-reference/rate-limits).

## Private WebSocket: `wss://api.polymarket.us/v1/ws/private`

Needs a signed handshake (`GET` + `/v1/ws/private`). Messages are camelCase, and enums are strings. Subscribe with the same envelope as the markets socket:

```json
{ "subscribe": { "requestId": "orders-1", "subscriptionType": "SUBSCRIPTION_TYPE_ORDER", "marketSlugs": [] } }
```

Empty or omitted `marketSlugs` means all markets. The limit is 100 slugs per subscription.

| `subscriptionType` | Payload key(s) | What actually happens |
| --- | --- | --- |
| `SUBSCRIPTION_TYPE_ORDER_SNAPSHOT` | `orderSubscriptionSnapshot { orders[], eof }` | **The only reliable source of the open-order snapshot.** It comes at once; accumulate until `eof: true`. |
| `SUBSCRIPTION_TYPE_ORDER` | `orderSubscriptionUpdate { execution }` | Updates only. The docs claim it starts with a snapshot, but a live probe saw none. |
| `SUBSCRIPTION_TYPE_POSITION` | `positionSubscription { beforePosition, afterPosition, updateTime, entryType, tradeId }` | Updates only. Take the starting positions from REST. |
| `SUBSCRIPTION_TYPE_ACCOUNT_BALANCE` | `accountBalancesSnapshot { balances[] }`, `accountBalancesUpdate { balanceChange{ beforeBalance, afterBalance, updateTime, entryType } }` | The docs promise a snapshot, but a live probe on an idle account got nothing for 60 s, and nothing came for placing or cancelling orders either (open orders do not change the balance). Take the starting balance from REST. |

### Order updates

`execution` has the shape of `Execution` in [trading.md](trading.md#order-lifecycle): `type`, `order` (full `Order`, including `state`, `leavesQuantity`, `cumQuantity`), `lastShares` (**string**, can be fractional), `lastPx`, `tradeId`, `aggressor`, `orderRejectReason`, `unsolicitedCancelReason`.

- Batched REST responses don't confirm anything, so this stream is the source of truth for every create, modify, and cancel.
- Messages are delivered in order.
- There is **no replay** after a disconnect. Resync with the snapshot subscription plus `GET /v1/orders/open` and `GET /v1/portfolio/positions`, because fills during the gap are otherwise lost.

### Position updates

`beforePosition` / `afterPosition` carry `netPosition` (rounded, deprecated), **`netPositionDecimal`**, `cost`, and possibly `qtyBoughtDecimal`, `qtySoldDecimal`, `bodPositionDecimal`, `qtyAvailableDecimal`. Always use the `*Decimal` fields when present.

### Ledger entry types (`entryType`)

`LEDGER_ENTRY_TYPE_ORDER_EXECUTION`, `_COMMISSION`, `_RESOLUTION`, `_CONTRACT_EXPIRATION`, `_NETTING`, `_CORRECTION`, `_MANUAL_ADJUSTMENT`, `_DEPOSIT`, `_WITHDRAWAL`.

## `GET /v1/portfolio/positions`

Query: `market` (one slug), `limit` (default 100), `cursor`. The response is

```json
{ "positions": { "<marketSlug>": UserPosition, … }, "nextCursor": "…", "eof": true }
```

`positions` is a **map keyed by slug**, not an array. Page with `cursor = nextCursor` until `eof`.

`UserPosition`:

| Field | Meaning |
| --- | --- |
| `netPositionDecimal` | Signed net YES position (negative = short). **Use this**, not `netPosition`. |
| `qtyBoughtDecimal`, `qtySoldDecimal`, `bodPositionDecimal` | Totals and beginning-of-day position |
| `qtyAvailableDecimal` | Tradable quantity after open orders (nullable) |
| `cost` | Purchase cost of the held outcome (`Amount`), positive for both YES and NO positions |
| `realized` | Realized PnL |
| `cashValue` | Current value of the held outcome, not unrealized PnL (observed live on 2026-10-29) |
| `expired`, `updateTime`, `marketMetadata` | |

The core accounts in signed YES cash flows. For signed net YES quantity `q`, the adapter converts
the venue basis to `cost + min(q, 0) - realized`. A short of one YES with venue NO cost $0.64
becomes signed YES cost −$0.36, giving settlement PnL −$0.64 if YES and +$0.36 if NO. Reported
realized PnL is included so a restart retains gains and losses from earlier closes in a held market.

## `GET /v1/account/balances`

The response is `{ balances: [UserBalance] }`, one entry per currency (USD). The numbers here are JSON **numbers**, not strings.

| Field | Meaning |
| --- | --- |
| `currentBalance` | Cash, excluding position values |
| `buyingPower` | Unencumbered capital, after positions and open orders. This is the number for pre-trade checks (but see the per-instrument rule in [trading.md](trading.md#collateral-and-buying-power)). |
| `openOrders` | Notional of open orders |
| `marginRequirement` | Margin locked for positions |
| `assetNotional`, `assetAvailable` | Position value and its collateral value |
| `unsettledFunds`, `pendingCredit`, `pendingWithdrawals[]`, `balanceReservation` | Funds not yet usable |
| `lastUpdated` | Time of the last change |
