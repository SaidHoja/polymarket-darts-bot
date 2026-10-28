# Trading

Sources: [Orders & Trading](https://docs.polymarket.us/concepts/orders), [Orders API](https://docs.polymarket.us/api-reference/orders/overview) and its endpoint pages, [Collateral and Margin](https://docs.polymarket.us/market-structure/collateral-and-margin), [Fees](https://docs.polymarket.us/fees), [Liquidity Incentives](https://docs.polymarket.us/incentives/liquidity), [Rate Limits](https://docs.polymarket.us/api-reference/rate-limits).

## The YES-only model

Each market has **one instrument, the YES (long) side**. NO is synthetic: buying NO at X is selling YES at 1 − X.

- **`price.value` is always the YES price**, whatever the intent. To trade NO at X, send `1 − X`.
- **Self-match:** a buy YES at 0.60 and a buy NO at 0.40 are two orders at the same level of the same book. That's an error. Keep both-side exposure on different levels.
- Valid prices are **0.01–0.99**. An out-of-range price still returns an order id, then is rejected by the exchange. Validate client-side.

| Intent | Equivalent `outcomeSide` + `action` | Effect |
| --- | --- | --- |
| `ORDER_INTENT_BUY_LONG` | `OUTCOME_SIDE_YES` + `ORDER_ACTION_BUY` | Buy YES |
| `ORDER_INTENT_SELL_LONG` | `OUTCOME_SIDE_YES` + `ORDER_ACTION_SELL` | Sell YES you hold |
| `ORDER_INTENT_BUY_SHORT` | `OUTCOME_SIDE_NO` + `ORDER_ACTION_BUY` | Buy NO, i.e. sell YES short |
| `ORDER_INTENT_SELL_SHORT` | `OUTCOME_SIDE_NO` + `ORDER_ACTION_SELL` | Sell NO you hold |

Send either `intent` or `outcomeSide`+`action`. If both are present, `outcomeSide`+`action` wins. Responses carry both. `side` (`ORDER_SIDE_BUY`/`SELL`) is the YES-book side.

An ask can be sent as `SELL_LONG` when a long position covers the size, and otherwise as `BUY_SHORT` at the same YES price.

## Order request

`POST /v1/orders` on `https://api.polymarket.us`. All enums are **strings**.

```json
{
  "marketSlug": "…",
  "type": "ORDER_TYPE_LIMIT",
  "price": { "value": "0.555", "currency": "USD" },
  "quantity": 0.5,
  "tif": "TIME_IN_FORCE_GOOD_TILL_DATE",
  "goodTillTime": "2026-10-20T14:40:00Z",
  "intent": "ORDER_INTENT_BUY_LONG",
  "manualOrderIndicator": "MANUAL_ORDER_INDICATOR_AUTOMATIC",
  "participateDontInitiate": true
}
```

| Field | Notes |
| --- | --- |
| `marketSlug` | Required |
| `type` | `ORDER_TYPE_LIMIT` (needs `price`) or `ORDER_TYPE_MARKET` |
| `price` | `Amount` = `{ value: decimal string, currency: "USD" }`. Must sit on `orderPriceMinTickSize`. |
| `quantity` | **Number**, in contracts, can be fractional. Must be a multiple of `minimumTradeQty`. Extra precision may be silently truncated (e.g. `0.015` → `0.01`, `"0.515"` → `"0.51"`), so round yourself. |
| `tif` | `GOOD_TILL_CANCEL`, `GOOD_TILL_DATE` (+ `goodTillTime`), `DAY` (no auto-cancel at the 17:00 ET roll right now), `IMMEDIATE_OR_CANCEL`, `FILL_OR_KILL`. All prefixed `TIME_IN_FORCE_`. |
| `participateDontInitiate` | **Post-only.** Rejected if it would match on arrival. |
| `manualOrderIndicator` | **Required** (regulatory). Bots send `MANUAL_ORDER_INDICATOR_AUTOMATIC`. |
| `synchronousExecution`, `maxBlockTime` | Blocks up to 10 s for a final state. **Don't use for resting/market-making orders.** |
| `slippageTolerance` | `{ currentPrice, ticks \| bips }`, for market orders only. Unlimited by default. |
| `cashOrderQty` | Dollar size for market orders |

The response is `{ id, executions[] }`. `executions` is filled only with synchronous execution.

There is no client order id field. Orders are identified only by the exchange-assigned `id`.

## Endpoints

All need signing. The batch limit is **20 entries**.

| Method & path | Body → response | Notes |
| --- | --- | --- |
| `POST /v1/orders` | order → `{id}` | |
| `POST /v1/orders/batched` | `{orders:[CreateOrderRequest…]}` → `{createdOrderIds}` | Ids are real and in request order. Accept/reject per entry still comes on the private stream. |
| `POST /v1/order/{id}/modify` | `{marketSlug, price, quantity, tif, participateDontInitiate, goodTillTime}` → `{}` | Cancel-replace on the exchange |
| `POST /v1/orders/batched/modify` | `{orders:[{orderId, marketSlug, price?, quantity?, tif?, participateDontInitiate?, goodTillTime?}]}` → `{modifiedOrderIds}` | **Ids are an echo, not a confirmation** |
| `POST /v1/order/{id}/cancel` | `{marketSlug}` → `{}` | |
| `POST /v1/orders/batched/cancel` | `{orders:[{orderId, marketSlug}]}` → `{canceledOrderIds}` | **Echo, not a confirmation** |
| `POST /v1/orders/open/cancel` | `{slugs:[…]}` (empty = all) → `{canceledOrderIds}` | Cancel-all |
| `GET /v1/orders/open?slugs=a&slugs=b` | → `{orders:[Order…]}` | For resyncs, not polling |
| `GET /v1/order/{id}` | → `{order}` | 404 if unknown |
| `POST /v1/order/preview` | order → preview | Validation without submitting |
| `POST /v1/order/close-position` | | Market-style close, with slippage tolerance |

### Batch semantics

- Gateway validation is **atomic**: one malformed entry (missing `orderId`, more than 20 entries, …) rejects the whole batch with `400`.
- The exchange then handles entries **independently**. **Unknown `orderId`s in cancel/modify are silently ignored.**
- Treat the private stream as the only source of truth: `EXECUTION_TYPE_NEW` / `REPLACE` / `CANCELED` / `REJECTED`.

### `Order` object (REST and stream)

`id`, `marketSlug`, `side`, `type`, `price`, `quantity`, `cumQuantity`, `leavesQuantity`, `avgPx`, `tif`, `goodTillTime`, `intent`, `outcomeSide`, `action`, `state`, `insertTime`, `createTime`, `commissionsBasisPoints`, `makerCommissionsBasisPoints`, `commissionNotionalTotalCollected`, `marketMetadata{slug,title,outcome,eventSlug,team}`.

## Order lifecycle

| State | Meaning |
| --- | --- |
| `ORDER_STATE_PENDING_NEW` | Received, not yet processed |
| `ORDER_STATE_NEW` | Accepted and resting |
| `ORDER_STATE_PARTIALLY_FILLED` | Partly filled |
| `ORDER_STATE_FILLED` | Fully filled |
| `ORDER_STATE_PENDING_REPLACE` / `ORDER_STATE_PENDING_CANCEL` | Modify / cancel in flight |
| `ORDER_STATE_PENDING_RISK` | Awaiting risk approval |
| `ORDER_STATE_REPLACED` | Replaced by a modify |
| `ORDER_STATE_CANCELED` / `ORDER_STATE_REJECTED` / `ORDER_STATE_EXPIRED` | Terminal (expired = GTD) |

Execution types: `EXECUTION_TYPE_NEW`, `_PARTIAL_FILL`, `_FILL`, `_CANCELED`, `_REPLACE`, `_REJECTED`, `_EXPIRED`, `_DONE_FOR_DAY`.

An `Execution` carries `id`, `order`, `type`, `lastShares` (string), `lastPx`, `tradeId`, `aggressor` (true = taker), `commissionNotionalCollected`, `orderRejectReason`, `unsolicitedCancelReason`, `text`, `transactTime`.

### Reject reasons (`orderRejectReason`)

| Reason | Meaning |
| --- | --- |
| `ORD_REJECT_REASON_UNKNOWN_MARKET` | Bad slug |
| `ORD_REJECT_REASON_EXCHANGE_CLOSED` | Market/exchange closed (maintenance, pre-open, halt) |
| `ORD_REJECT_REASON_INCORRECT_QUANTITY` | Off `minimumTradeQty` |
| `ORD_REJECT_REASON_INVALID_PRICE_INCREMENT` | Off tick |
| `ORD_REJECT_REASON_PRICE_OUT_OF_BOUNDS` | Outside 0.01–0.99 |
| `ORD_REJECT_REASON_INCORRECT_ORDER_TYPE` | Type not allowed |
| `ORD_REJECT_REASON_NO_LIQUIDITY` | Market order with nothing to hit |
| `ORD_REJECT_REASON_EXCHANGE_OPTION` | Generic, used when no specific code applies |

The 5 s latency stopgap's reject is identified by its message, `Global Rate Limit Exceeded`; the docs don't give its reason code. Retry it and don't throttle (see [README](README.md#latency-stopgap-not-a-rate-limit)).

### Unsolicited cancels (`unsolicitedCancelReason`)

`UNSOLICITED_CXL_REASON_CONNECTION_LOSS`, `_LOGOUT`, `_EXCHANGE_OPTION`, `_OTHER`. These are exchange-initiated cancels and must be handled in order tracking. One known cause is the collateral re-check below.

**Live behaviour (2026-10-21):** the field is on every execution, set to `UNSOLICITED_CXL_REASON_UNDEFINED` when there is no unsolicited cancel, including on acks and on cancels we asked for. Treat `_UNDEFINED` as absent. Likewise `orderRejectReason` is `ORD_REJECT_REASON_EXCHANGE_OPTION` on every execution, so read it only on `EXECUTION_TYPE_REJECTED`.

## Collateral and buying power

Contracts are fully collateralized at the trade.

| | Cash | Margin locked | Buying-power change |
| --- | --- | --- | --- |
| Buy YES at p | −p | 0 | −p |
| Sell YES short at p | +p | $1.00 | −(1 − p) |

Worst cases: a long loses p, and a short loses 1 − p.

The pre-trade check for **open orders is per instrument**. A new order's worst-case loss is checked against buying power plus your open orders **in that same market only**.

- Within one market, resting interest counts at the **worse of all-bids-fill or all-asks-fill**, with offsets for position it would close. Quoting both sides only costs the riskier side.
- Across markets, open orders don't reduce each other's buying power, so total resting exposure can exceed the balance.
- **When a fill eats buying power, the exchange re-checks open orders per instrument and auto-cancels unfunded ones.** These arrive as unsolicited cancels on the order stream.
- A trade that would take buying power below zero fails.

## Fees

Fees are effective from 2026-10-19 13:00 ET. Exact formula, per fill:

```
fee = Θ × C × p × (1 − p)        C = contracts, p = trade price (YES, decimal)
```

| | Θ |
| --- | --- |
| Taker | **0.0695** (Table Tennis 0.10). Read `feeCoefficient` per market. |
| Maker rebate | Tier 1 (< $10M prior-month maker volume): **none**. Tiers 2–4: −0.0035 / −0.0070 / −0.0104. |

- Rounded to the cent with **banker's rounding** (half to even).
- When one aggressive order fills against several makers, the per-fill fees are adjusted so their sum never exceeds the rounded total.
- Fees are charged only on executions. They're deducted at the trade, and rebates are credited at the fill.
- Taker rebates (3–30% by prior-month taker volume, paid weekly) start at $250K/month.
- Volume per fill is C × p for a buy and C × (1 − p) for a sell.
- Maximum taker fee per 100 contracts is $1.74, at p = 0.50.

## Liquidity rewards (optional, per market)

Some markets pay a pool for resting orders, listed at [polymarket.us/rewards](https://polymarket.us/rewards) and `GET /v1/incentives`.

- Once per second, at a random time, each resting order scores `DiscountFactor^(ticks from best) × size`. Each side is normalized and split pro rata.
- Only orders inside the first **Target Size** of the book (walking out from best, all participants) score.
- An optional **Max Spread** (half-width from the size-adjusted mid, in dollars) means both sides must qualify for a second to pay.
- Periods: pre-game (listing until 6 h before start), day-of (6 h before until start), live (start until settlement).
- Payouts under $1 are dropped, and canceled or postponed games pay nothing.

Orders far from the touch earn close to nothing.
