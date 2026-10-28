//! Venue JSON → typed events.
//!
//! Numbers are read through their text (`serde_json` is built with
//! `arbitrary_precision`), so tick sizes and quantities that arrive as JSON
//! numbers stay exact. Unknown enum values decode to `Unknown`; a frame that
//! can't be decoded is reported as an error and dropped by the caller.

use std::borrow::Cow;
use std::sync::Arc;

use pm_types::time::parse_rfc3339_ns;
use pm_types::{
    BalanceFrame, BookFrame, ExecKind, ExecutionFrame, FillInfo, GameFrame, Intent, Level,
    MarketParams, MarketState, OrderState, PositionFrame, Px, Qty, Reject, RejectClass, Side,
    TeamOrdering, TradeFrame, VenueOrder,
};
use serde_json::Value;

pub type DecodeResult<T> = Result<T, String>;

/// The text of a decimal: a string, a JSON number, or an `Amount` (`{"value": "0.55", "currency": "USD"}`).
fn decimal_text(v: &Value) -> Option<Cow<'_, str>> {
    match v {
        Value::String(s) if !s.is_empty() => Some(Cow::Borrowed(s)),
        Value::Number(n) => Some(Cow::Owned(n.to_string())),
        Value::Object(map) => map.get("value").and_then(decimal_text),
        _ => None,
    }
}

fn px(v: &Value) -> DecodeResult<Px> {
    let text = decimal_text(v).ok_or_else(|| format!("missing price: {v}"))?;
    Px::parse(&text).map_err(|e| e.to_string())
}

fn qty(v: &Value) -> DecodeResult<Qty> {
    let text = decimal_text(v).ok_or_else(|| format!("missing quantity: {v}"))?;
    Qty::parse(&text).map_err(|e| e.to_string())
}

/// A field that carries a value. Absent, null and `""` are protobuf defaults.
fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null() && v.as_str() != Some(""))
}

fn opt_qty(v: Option<&Value>) -> Option<Qty> {
    v.and_then(decimal_text).and_then(|t| Qty::parse(&t).ok())
}

/// Dollar amounts (balances, fees, cost) are reporting values, so f64 is fine here.
fn dollars(v: Option<&Value>) -> Option<f64> {
    v.and_then(decimal_text).and_then(|t| t.parse().ok())
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn ts(v: &Value, key: &str) -> Option<i64> {
    v.get(key)
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ns)
}

/// Event ids arrive as strings in list responses and sometimes as numbers.
fn id_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

pub fn parse_json(raw: &str) -> DecodeResult<Value> {
    serde_json::from_str(raw).map_err(|e| format!("bad JSON: {e}"))
}

// ---------------------------------------------------------------- markets socket

#[derive(Debug, Clone, PartialEq)]
pub enum MarketMsg {
    Book(BookFrame),
    Trade(TradeFrame),
    SubscriptionError { request_id: String, error: String },
    Other,
}

fn levels(v: Option<&Value>) -> DecodeResult<Vec<Level>> {
    let Some(Value::Array(items)) = v else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|level| Ok((px(&level["px"])?, qty(&level["qty"])?)))
        .collect()
}

/// `stats.settlementPx`, only on a market that has ended: open books never carry it (none of 231,607
/// captured), and a default `0` on an open market must not read as a resolution.
fn settlement(state: MarketState, text: Option<&str>) -> Option<Px> {
    matches!(state, MarketState::Expired | MarketState::Terminated).then_some(())?;
    Px::parse(text?).ok()
}

pub fn decode_book(data: &Value) -> DecodeResult<BookFrame> {
    let slug = data
        .get("marketSlug")
        .and_then(Value::as_str)
        .ok_or("book without marketSlug")?;
    let stats = data.get("stats");
    let state = MarketState::from_wire(str_of(data, "state"));
    let settlement_px = stats
        .and_then(|s| s.get("settlementPx"))
        .and_then(decimal_text);
    Ok(BookFrame {
        slug: Arc::from(slug),
        bids: levels(data.get("bids"))?,
        offers: levels(data.get("offers"))?,
        state,
        transact_ns: ts(data, "transactTime"),
        shares_traded: opt_qty(stats.and_then(|s| s.get("sharesTraded"))),
        settlement: settlement(state, settlement_px.as_deref()),
    })
}

pub fn decode_trade(trade: &Value) -> DecodeResult<TradeFrame> {
    let slug = trade
        .get("marketSlug")
        .and_then(Value::as_str)
        .ok_or("trade without marketSlug")?;
    Ok(TradeFrame {
        id: str_of(trade, "id").to_string(),
        slug: Arc::from(slug),
        px: px(&trade["price"])?,
        qty: qty(&trade["quantity"])?,
        // Only taker.side gives the direction in YES terms; intent/outcomeSide/action are the user's view.
        taker_side: Side::from_wire(trade.get("taker").map(|t| str_of(t, "side")).unwrap_or("")),
        trade_ns: ts(trade, "tradeTime"),
    })
}

// Fast path for books: deserialize straight into borrowed wire structs, with no `Value` tree and
// no per-level allocations. Anything it doesn't handle exactly (another message type, a number
// where a string was expected, escaped text, a missing or malformed field) returns `None`, and the
// frame goes through the `Value` path, which gives the same result or the same error as before.

/// A decimal's text, borrowed: a string or an `Amount` object. JSON numbers aren't accepted.
struct WireDecimal<'a>(&'a str);

impl<'de: 'a, 'a> serde::Deserialize<'de> for WireDecimal<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = WireDecimal<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a decimal string or {\"value\": string}")
            }
            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                s: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(WireDecimal(s))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut value = None;
                while let Some(key) = map.next_key::<&'de str>()? {
                    if key == "value" {
                        // A repeated key resolves differently in a `Value` tree (the last one wins): decline.
                        if value.is_some() {
                            return Err(serde::de::Error::duplicate_field("value"));
                        }
                        value = Some(map.next_value::<&'de str>()?);
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                value
                    .map(WireDecimal)
                    .ok_or_else(|| serde::de::Error::missing_field("value"))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(serde::Deserialize)]
struct WireLevel<'a> {
    #[serde(borrow)]
    px: WireDecimal<'a>,
    #[serde(borrow)]
    qty: WireDecimal<'a>,
}

/// One book side parsed element by element into its final vector, with no intermediate
/// `Vec<WireLevel>`. A level that doesn't parse fails the frame, so the fast path declines it.
struct WireLevels(Vec<Level>);

impl<'de> serde::Deserialize<'de> for WireLevels {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = WireLevels;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of price levels")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<WireLevels, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(level) = seq.next_element::<WireLevel<'de>>()? {
                    let px = Px::parse(level.px.0).map_err(serde::de::Error::custom)?;
                    let qty = Qty::parse(level.qty.0).map_err(serde::de::Error::custom)?;
                    out.push((px, qty));
                }
                Ok(WireLevels(out))
            }
        }
        d.deserialize_seq(V)
    }
}

#[derive(serde::Deserialize)]
struct WireStats<'a> {
    #[serde(borrow, rename = "sharesTraded")]
    shares_traded: Option<WireDecimal<'a>>,
    #[serde(borrow, rename = "settlementPx")]
    settlement_px: Option<WireDecimal<'a>>,
}

#[derive(serde::Deserialize)]
struct WireBook<'a> {
    #[serde(rename = "marketSlug")]
    slug: &'a str,
    bids: Option<WireLevels>,
    offers: Option<WireLevels>,
    state: Option<&'a str>,
    #[serde(rename = "transactTime")]
    transact_time: Option<&'a str>,
    #[serde(borrow)]
    stats: Option<WireStats<'a>>,
}

#[derive(serde::Deserialize)]
struct WireBookFrame<'a> {
    #[serde(borrow, rename = "marketData")]
    market_data: Option<WireBook<'a>>,
}

/// The book in `raw`, if it is a book frame the fast path decodes exactly; `None` otherwise.
fn fast_book(raw: &str) -> Option<BookFrame> {
    let book = serde_json::from_str::<WireBookFrame>(raw)
        .ok()?
        .market_data?;
    let (shares_traded, settlement_px) = match book.stats {
        // The `Value` path ignores an empty or unparseable count; so does this one.
        Some(stats) => (
            stats.shares_traded.and_then(|t| Qty::parse(t.0).ok()),
            stats.settlement_px.map(|t| t.0),
        ),
        None => (None, None),
    };
    let state = MarketState::from_wire(book.state.unwrap_or(""));
    Some(BookFrame {
        slug: Arc::from(book.slug),
        bids: book.bids.map_or_else(Vec::new, |l| l.0),
        offers: book.offers.map_or_else(Vec::new, |l| l.0),
        state,
        transact_ns: book.transact_time.and_then(parse_rfc3339_ns),
        shares_traded,
        settlement: settlement(state, settlement_px),
    })
}

pub fn decode_market_frame(raw: &str) -> DecodeResult<MarketMsg> {
    // The key check keeps trades and other frames from paying for a failed typed parse.
    if raw.contains("\"marketData\"") {
        if let Some(book) = fast_book(raw) {
            return Ok(MarketMsg::Book(book));
        }
    }
    decode_market_frame_slow(raw)
}

/// The generic decoder, through a `Value` tree: every message type, and books the fast path declines.
pub fn decode_market_frame_slow(raw: &str) -> DecodeResult<MarketMsg> {
    let msg = parse_json(raw)?;
    if let Some(data) = msg.get("marketData") {
        return decode_book(data).map(MarketMsg::Book);
    }
    if let Some(trade) = msg.get("trade") {
        return decode_trade(trade).map(MarketMsg::Trade);
    }
    if let Some(error) = msg.get("error") {
        return Ok(MarketMsg::SubscriptionError {
            request_id: str_of(&msg, "requestId").to_string(),
            error: error.to_string(),
        });
    }
    Ok(MarketMsg::Other)
}

// ---------------------------------------------------------------- private socket

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // executions are the common message; boxing adds an allocation each
pub enum PrivateMsg {
    /// One part of the order snapshot; accumulate until `eof`.
    Snapshot {
        orders: Vec<VenueOrder>,
        eof: bool,
    },
    Execution(ExecutionFrame),
    Position(PositionFrame),
    Balance(BalanceFrame),
    SubscriptionError {
        request_id: String,
        error: String,
    },
    Other,
}

/// Absent numeric fields are protobuf defaults (zero); a field that is present but unreadable
/// is an error, so a malformed order never understates resting size.
pub fn decode_order(order: &Value) -> DecodeResult<VenueOrder> {
    let id = order
        .get("id")
        .and_then(Value::as_str)
        .ok_or("order without id")?;
    let slug = order
        .get("marketSlug")
        .and_then(Value::as_str)
        .unwrap_or("");
    let field = |key: &str| present(order.get(key));
    let bad = |e: String| format!("order {id}: {e}");
    let quantity = field("quantity")
        .map(qty)
        .transpose()
        .map_err(bad)?
        .unwrap_or(Qty::ZERO);
    let cum = field("cumQuantity")
        .map(qty)
        .transpose()
        .map_err(bad)?
        .unwrap_or(Qty::ZERO);
    let leaves = field("leavesQuantity").map(qty).transpose().map_err(bad)?;
    let price = field("price")
        .map(px)
        .transpose()
        .map_err(bad)?
        .unwrap_or(Px::ZERO);
    Ok(VenueOrder {
        id: Arc::from(id),
        slug: Arc::from(slug),
        side: Side::from_wire(str_of(order, "side")),
        intent: Intent::from_wire(str_of(order, "intent")),
        px: price,
        qty: quantity,
        cum_qty: cum,
        leaves_qty: leaves.unwrap_or(quantity - cum),
        state: OrderState::from_wire(str_of(order, "state")),
    })
}

/// Reject classes from the venue's reason code and free text.
pub fn classify_reject(reason: &str, text: &str) -> RejectClass {
    let lower = text.to_ascii_lowercase();
    if lower.contains("global rate limit exceeded") {
        return RejectClass::Transient;
    }
    match reason {
        "ORD_REJECT_REASON_INVALID_PRICE_INCREMENT"
        | "ORD_REJECT_REASON_INCORRECT_QUANTITY"
        | "ORD_REJECT_REASON_PRICE_OUT_OF_BOUNDS"
        | "ORD_REJECT_REASON_INCORRECT_ORDER_TYPE" => RejectClass::OurBug,
        "ORD_REJECT_REASON_EXCHANGE_CLOSED" | "ORD_REJECT_REASON_UNKNOWN_MARKET" => {
            RejectClass::MarketUnavailable
        }
        _ if lower.contains("post only")
            || lower.contains("post-only")
            || lower.contains("participate")
            || lower.contains("would cross")
            || lower.contains("would match") =>
        {
            RejectClass::BookMoved
        }
        _ if lower.contains("buying power")
            || lower.contains("insufficient")
            || lower.contains("balance") =>
        {
            RejectClass::Funding
        }
        "ORD_REJECT_REASON_NO_LIQUIDITY" => RejectClass::BookMoved,
        _ => RejectClass::Unknown,
    }
}

pub fn decode_execution(exec: &Value) -> DecodeResult<ExecutionFrame> {
    let order = decode_order(exec.get("order").ok_or("execution without order")?)?;
    let kind = ExecKind::from_wire(str_of(exec, "type"));
    // A fill must say how much traded; one that doesn't is an error, never a fill-less terminal execution.
    let fill = if matches!(kind, ExecKind::Fill | ExecKind::PartialFill) {
        let bad = |e: String| format!("{kind:?} execution {}: {e}", str_of(exec, "id"));
        let last_shares = present(exec.get("lastShares"))
            .map(qty)
            .transpose()
            .map_err(bad)?
            .filter(|q| *q > Qty::ZERO)
            .ok_or_else(|| bad("no positive lastShares".into()))?;
        let last_px = present(exec.get("lastPx"))
            .map(px)
            .transpose()
            .map_err(bad)?;
        Some(FillInfo {
            last_shares,
            last_px: last_px.filter(|p| *p > Px::ZERO).unwrap_or(order.px),
            trade_id: str_of(exec, "tradeId").to_string(),
            aggressor: exec
                .get("aggressor")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            commission: dollars(exec.get("commissionNotionalCollected")),
        })
    } else {
        None
    };
    // Protobuf defaults: the reject reason is on every execution, so read it only on rejects.
    let reject = (kind == ExecKind::Rejected).then(|| {
        let reason = str_of(exec, "orderRejectReason").to_string();
        let text = str_of(exec, "text").to_string();
        Reject {
            class: classify_reject(&reason, &text),
            reason,
            text,
        }
    });
    let unsolicited = exec
        .get("unsolicitedCancelReason")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty() && !r.ends_with("_UNDEFINED"))
        .map(str::to_string);
    Ok(ExecutionFrame {
        exec_id: str_of(exec, "id").to_string(),
        order,
        kind,
        fill,
        reject,
        unsolicited,
        transact_ns: ts(exec, "transactTime"),
    })
}

fn position_slug(sub: &Value) -> Option<String> {
    for key in ["marketSlug", "slug"] {
        if let Some(s) = sub.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    for side in ["afterPosition", "beforePosition"] {
        if let Some(meta) = sub.get(side).and_then(|p| p.get("marketMetadata")) {
            if let Some(s) = meta.get("slug").and_then(Value::as_str) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// A `UserPosition`: always prefer the `*Decimal` fields.
pub fn decode_user_position(slug: &str, pos: &Value) -> PositionFrame {
    PositionFrame {
        slug: Arc::from(slug),
        net: opt_qty(pos.get("netPositionDecimal"))
            .or_else(|| opt_qty(pos.get("netPosition")))
            .unwrap_or(Qty::ZERO),
        cost: dollars(pos.get("cost")),
        resolution: false,
    }
}

pub fn decode_private_frame(raw: &str) -> DecodeResult<PrivateMsg> {
    let msg = parse_json(raw)?;
    // Checked first: a failed subscription can still carry an empty `orderSubscriptionSnapshot`
    // with `eof: true` (seen with "Unauthenticated"), which must not read as "no open orders".
    if let Some(error) = present(msg.get("error")) {
        return Ok(PrivateMsg::SubscriptionError {
            request_id: str_of(&msg, "requestId").to_string(),
            error: error.to_string(),
        });
    }
    if let Some(snapshot) = msg.get("orderSubscriptionSnapshot") {
        let orders = match snapshot.get("orders") {
            Some(Value::Array(items)) => items
                .iter()
                .map(decode_order)
                .collect::<DecodeResult<Vec<_>>>()?,
            _ => Vec::new(),
        };
        let eof = snapshot
            .get("eof")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return Ok(PrivateMsg::Snapshot { orders, eof });
    }
    if let Some(update) = msg.get("orderSubscriptionUpdate") {
        let exec = update
            .get("execution")
            .ok_or("order update without execution")?;
        return decode_execution(exec).map(PrivateMsg::Execution);
    }
    if let Some(sub) = msg.get("positionSubscription") {
        let slug = position_slug(sub).ok_or("position update without a market slug")?;
        // No `afterPosition` decodes as flat, the protobuf default.
        let after = sub.get("afterPosition").unwrap_or(&Value::Null);
        let mut position = decode_user_position(&slug, after);
        position.resolution = str_of(sub, "entryType") == "LEDGER_ENTRY_TYPE_RESOLUTION";
        return Ok(PrivateMsg::Position(position));
    }
    if let Some(update) = msg.get("accountBalancesUpdate") {
        if let Some(after) = update
            .get("balanceChange")
            .and_then(|c| c.get("afterBalance"))
        {
            return Ok(PrivateMsg::Balance(decode_balance(after)));
        }
        return Ok(PrivateMsg::Other);
    }
    if let Some(snapshot) = msg.get("accountBalancesSnapshot") {
        if let Some(first) = snapshot
            .get("balances")
            .and_then(Value::as_array)
            .and_then(|b| b.first())
        {
            return Ok(PrivateMsg::Balance(decode_balance(first)));
        }
        return Ok(PrivateMsg::Other);
    }
    Ok(PrivateMsg::Other)
}

// ---------------------------------------------------------------- REST payloads

pub fn decode_balance(b: &Value) -> BalanceFrame {
    BalanceFrame {
        current_balance: dollars(b.get("currentBalance")).unwrap_or(0.0),
        buying_power: dollars(b.get("buyingPower")).unwrap_or(0.0),
        open_orders: dollars(b.get("openOrders")).unwrap_or(0.0),
    }
}

/// `GET /v1/account/balances` → the USD balance.
/// `GET /v1/markets/{slug}/settlement`: `{"slug": "…", "settlement": 1}`, a YES price from 0 to 1.
pub fn decode_settlement(body: &Value) -> DecodeResult<Px> {
    let value = present(body.get("settlement"))
        .ok_or_else(|| format!("settlement response without a price: {}", snippet(body)))?;
    let p = px(value)?;
    if p < Px::ZERO || p > Px(pm_types::num::SCALE) {
        return Err(format!("settlement price out of range: {p}"));
    }
    Ok(p)
}

pub fn decode_balances(body: &Value) -> Option<BalanceFrame> {
    body.get("balances")?
        .as_array()?
        .first()
        .map(decode_balance)
}

/// One page of `GET /v1/portfolio/positions`: positions, next cursor, eof. The venue sends
/// `"positions": {}` when there are none, so a missing or non-map field is an error, not "flat".
pub fn decode_positions_page(
    body: &Value,
) -> DecodeResult<(Vec<PositionFrame>, Option<String>, bool)> {
    let positions = match body.get("positions") {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(slug, pos)| decode_user_position(slug, pos))
            .collect(),
        _ => {
            return Err(format!(
                "positions page without a positions map: {}",
                snippet(body)
            ))
        }
    };
    let cursor = body
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string);
    let eof = body.get("eof").and_then(Value::as_bool).unwrap_or(true);
    Ok((positions, cursor, eof))
}

/// `GET /v1/orders/open`. The venue sends `"orders": []` when there are none.
pub fn decode_orders(body: &Value) -> DecodeResult<Vec<VenueOrder>> {
    match body.get("orders") {
        Some(Value::Array(items)) => items.iter().map(decode_order).collect(),
        _ => Err(format!(
            "open orders without an orders array: {}",
            snippet(body)
        )),
    }
}

/// The start of a body for an error message, cut on a character boundary.
pub fn snippet(body: &Value) -> String {
    truncate(&body.to_string(), 200).to_string()
}

/// At most `max` bytes of `s`, never splitting a character.
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn decode_market(
    market: &Value,
    event: &Value,
    league: &str,
    is_futures: bool,
) -> DecodeResult<MarketParams> {
    let slug = market
        .get("slug")
        .and_then(Value::as_str)
        .ok_or("market without slug")?;
    let (mut yes_name, mut no_name, mut yes_ordering) = (None, None, None);
    if let Some(Value::Array(sides)) = market.get("marketSides") {
        for side in sides {
            let description = side
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            if side.get("long").and_then(Value::as_bool).unwrap_or(false) {
                yes_name = description;
                yes_ordering = match side.get("team").map(|t| str_of(t, "ordering")) {
                    Some("home") => Some(TeamOrdering::Home),
                    Some("away") => Some(TeamOrdering::Away),
                    _ => None,
                };
            } else {
                no_name = description;
            }
        }
    }
    // An increment the venue sends but we can't use (unparseable, more than 4 decimals, not
    // positive) skips the market: quoting it with a guessed increment would get rejected or worse.
    // Absent increments fall back to 0.01, the value on every darts market so far.
    let unusable = |key: &str, v: &Value| format!("market {slug}: unusable {key} {v}");
    let tick = present(market.get("orderPriceMinTickSize"))
        .map(|v| {
            px(v)
                .ok()
                .filter(|t| *t > Px::ZERO)
                .ok_or_else(|| unusable("orderPriceMinTickSize", v))
        })
        .transpose()?;
    let min_qty = present(market.get("minimumTradeQty"))
        .map(|v| {
            qty(v)
                .ok()
                .filter(|q| *q > Qty::ZERO)
                .ok_or_else(|| unusable("minimumTradeQty", v))
        })
        .transpose()?;
    if tick.is_none() {
        tracing::warn!("market {slug}: no orderPriceMinTickSize; assuming 0.01");
    }
    if min_qty.is_none() {
        tracing::warn!("market {slug}: no minimumTradeQty; assuming 0.01");
    }
    Ok(MarketParams {
        slug: Arc::from(slug),
        title: market
            .get("title")
            .or_else(|| market.get("question"))
            .and_then(Value::as_str)
            .unwrap_or(slug)
            .to_string(),
        event_id: id_text(event.get("id")),
        event_slug: str_of(event, "slug").to_string(),
        league: league.to_string(),
        sports_market_type: str_of(market, "sportsMarketType").to_string(),
        tick: tick.unwrap_or(Px(100)),
        min_qty: min_qty.unwrap_or(Qty(100)),
        fee_coefficient: dollars(market.get("feeCoefficient")).unwrap_or(0.0),
        game_start_ns: ts(market, "gameStartTime").or_else(|| ts(event, "startTime")),
        yes_name,
        no_name,
        yes_ordering,
        is_futures,
    })
}

fn event_league(event: &Value) -> String {
    for key in ["league", "leagueSlug", "seriesSlug"] {
        match event.get(key) {
            Some(Value::String(s)) if !s.is_empty() => return s.clone(),
            Some(Value::Object(obj)) => {
                if let Some(s) = obj.get("slug").and_then(Value::as_str) {
                    return s.to_string();
                }
            }
            _ => {}
        }
    }
    // Event slugs look like `pdcdarts-gerpri-robcro-2026-10-22`.
    str_of(event, "slug")
        .split('-')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Live state; before a match `eventState` is null and the summary fields sit on the event.
pub fn decode_game(event: &Value) -> GameFrame {
    let state = event.get("eventState").filter(|s| s.is_object());
    let pick = |key: &str| {
        state
            .and_then(|s| s.get(key))
            .filter(|v| !v.is_null())
            .or_else(|| event.get(key))
    };
    let score = pick("score").and_then(Value::as_str).and_then(|s| {
        let (home, away) = s.split_once('-')?;
        Some((home.trim().parse().ok()?, away.trim().parse().ok()?))
    });
    GameFrame {
        event_id: id_text(event.get("id")),
        event_slug: str_of(event, "slug").to_string(),
        period: pick("period")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        live: pick("live").and_then(Value::as_bool).unwrap_or(false),
        ended: pick("ended").and_then(Value::as_bool).unwrap_or(false),
        score,
        updated_ns: state.and_then(|s| ts(s, "updatedAt")),
    }
}

/// One page of `GET /v2/sports/darts/events`: tradable markets (`active && !closed`) and game states.
pub fn decode_events_page(
    body: &Value,
    is_futures: bool,
) -> (Vec<MarketParams>, Vec<GameFrame>, usize) {
    let Some(events) = body.get("events").and_then(Value::as_array) else {
        return (Vec::new(), Vec::new(), 0);
    };
    let mut markets = Vec::new();
    let mut games = Vec::new();
    for event in events {
        if !is_futures {
            games.push(decode_game(event));
        }
        let league = event_league(event);
        for market in event
            .get("markets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let active = market
                .get("active")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let closed = market
                .get("closed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !active || closed {
                continue;
            }
            match decode_market(market, event, &league, is_futures) {
                Ok(params) => markets.push(params),
                Err(e) => tracing::warn!("skipping market: {e}"),
            }
        }
    }
    (markets, games, events.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../tests/fixtures/market_frame.jsonl");

    #[test]
    fn decodes_every_fixture_frame() {
        let mut books = 0;
        for line in FIXTURE.lines().filter(|l| !l.trim().is_empty()) {
            let envelope = parse_json(line).unwrap();
            let raw = envelope["raw"].as_str().unwrap();
            match decode_market_frame(raw).unwrap() {
                MarketMsg::Book(book) => {
                    books += 1;
                    assert!(
                        book.problems(Some(Px(100))).is_empty(),
                        "{:?}",
                        book.problems(None)
                    );
                    assert!(book.transact_ns.is_some());
                }
                MarketMsg::Trade(_) | MarketMsg::Other => {}
                MarketMsg::SubscriptionError { .. } => panic!("unexpected error frame"),
            }
        }
        assert!(books > 0);
    }

    /// The fast path either declines a frame or decodes exactly what the `Value` path does.
    fn assert_paths_agree(raw: &str) -> bool {
        match fast_book(raw) {
            Some(fast) => {
                assert_eq!(
                    decode_market_frame_slow(raw),
                    Ok(MarketMsg::Book(fast)),
                    "{raw}"
                );
                true
            }
            None => false,
        }
    }

    #[test]
    fn fast_book_path_matches_the_value_path() {
        for line in FIXTURE.lines().filter(|l| !l.trim().is_empty()) {
            let raw = parse_json(line).unwrap()["raw"]
                .as_str()
                .unwrap()
                .to_string();
            let is_book = matches!(decode_market_frame_slow(&raw), Ok(MarketMsg::Book(_)));
            assert_eq!(
                assert_paths_agree(&raw),
                is_book,
                "fast path declined a normal book: {raw}"
            );
        }
        let ok = r#"{"marketData":{"marketSlug":"m","bids":[{"px":{"value":"0.42","currency":"USD"},"qty":"3"}],
            "offers":[{"px":"0.45","qty":"1.5","extra":[1,{"a":null}]}],"state":"MARKET_STATE_OPEN",
            "stats":{"sharesTraded":"12.5","currentPx":{"value":"0.43"}},"transactTime":"2026-10-20T22:33:00.347786144Z"}}"#;
        assert!(assert_paths_agree(ok));
        // No sides, no state, no stats, an empty count: all optional on both paths.
        assert!(assert_paths_agree(
            r#"{"marketData":{"marketSlug":"m","bids":null,"stats":{"sharesTraded":""}}}"#
        ));
        // Declined, so the `Value` path decides (and still decodes or rejects them as before).
        for declined in [
            r#"{"marketData":{"marketSlug":"m","bids":[{"px":0.42,"qty":"3"}]}}"#, // number
            r#"{"marketData":{"marketSlug":"m","bids":[{"px":{"value":"0.42","value":"0.43"},"qty":"3"}]}}"#, // repeated key
            r#"{"marketData":{"marketSlug":"m\u0041","bids":[]}}"#, // escaped text
            r#"{"marketData":{"marketSlug":"m","bids":[{"px":"","qty":"3"}]}}"#, // empty price
            r#"{"marketData":{"marketSlug":"m","bids":[{"qty":"3"}]}}"#, // missing price
            r#"{"marketData":{"bids":[]}}"#,                        // no slug
            r#"{"marketData":null}"#,
            r#"{"marketData":{"marketSlug":"m","stats":{"sharesTraded":7}}}"#,
            r#"{"trade":{"marketSlug":"m","price":"0.1","quantity":"1"}}"#,
            r#"{"heartbeat":{}}"#,
            "not json",
        ] {
            assert!(fast_book(declined).is_none(), "{declined}");
        }
        let MarketMsg::Book(b) = decode_market_frame(
            r#"{"marketData":{"marketSlug":"m","bids":[{"px":0.42,"qty":"3"}]}}"#,
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(b.bids, vec![(Px(4200), Qty(30_000))]);
        assert!(
            decode_market_frame(r#"{"marketData":{"marketSlug":"m","bids":[{"qty":"3"}]}}"#)
                .is_err()
        );
    }

    /// Every market frame in `captures/*.jsonl`: run with
    /// `cargo test --release -p pm-venue fast_book_path_matches_captures -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn fast_book_path_matches_captures() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../captures");
        let (mut books, mut fast, mut lines) = (0u64, 0u64, 0u64);
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                let Ok(envelope) = parse_json(line) else {
                    continue;
                };
                let Some(raw) = envelope.get("raw").and_then(Value::as_str) else {
                    continue;
                };
                lines += 1;
                if matches!(decode_market_frame_slow(raw), Ok(MarketMsg::Book(_))) {
                    books += 1;
                }
                if assert_paths_agree(raw) {
                    fast += 1;
                }
            }
        }
        println!("{lines} raw frames, {books} books, {fast} on the fast path");
        assert!(books > 0);
        assert_eq!(fast, books, "the fast path declined some normal books");
    }

    #[test]
    fn decodes_trade_direction_from_taker_only() {
        let raw = r#"{"trade":{"marketSlug":"m","price":{"value":"0.1500","currency":"USD"},"quantity":{"value":"0.0100","currency":"USD"},
            "tradeTime":"2026-10-21T03:46:16.483269046Z",
            "maker":{"side":"ORDER_SIDE_SELL","intent":"ORDER_INTENT_UNDEFINED"},
            "taker":{"side":"ORDER_SIDE_BUY","intent":"ORDER_INTENT_SELL_SHORT","outcomeSide":"OUTCOME_SIDE_NO","action":"ORDER_ACTION_SELL"},
            "id":"D7NKT28G6ZRM","state":"TRADE_STATE_NEW"}}"#;
        let MarketMsg::Trade(t) = decode_market_frame(raw).unwrap() else {
            panic!()
        };
        assert_eq!(t.taker_side, Side::Buy);
        assert_eq!(t.px, Px(1500));
        assert_eq!(t.qty, Qty(100));
        assert_eq!(t.id, "D7NKT28G6ZRM");
    }

    #[test]
    fn decodes_execution_with_protobuf_defaults() {
        let raw = r#"{"requestId":"orders","orderSubscriptionUpdate":{"execution":{"id":"E1",
            "order":{"id":"O1","marketSlug":"m","side":"ORDER_SIDE_SELL","type":"ORDER_TYPE_LIMIT",
              "price":{"value":"0.81","currency":"USD"},"quantity":0.01,"cumQuantity":0,"leavesQuantity":0.01,
              "intent":"ORDER_INTENT_BUY_SHORT","state":"ORDER_STATE_NEW"},
            "lastShares":"0.0000","lastPx":{"value":"0.0000","currency":"USD"},"type":"EXECUTION_TYPE_NEW","text":"",
            "orderRejectReason":"ORD_REJECT_REASON_EXCHANGE_OPTION","transactTime":"2026-10-21T22:07:56.283266966Z",
            "tradeId":"","aggressor":false,"unsolicitedCancelReason":"UNSOLICITED_CXL_REASON_UNDEFINED"}}}"#;
        let PrivateMsg::Execution(e) = decode_private_frame(raw).unwrap() else {
            panic!()
        };
        assert_eq!(e.kind, ExecKind::New);
        assert_eq!(e.order.qty, Qty(100));
        assert_eq!(e.order.px, Px(8100));
        assert_eq!(e.order.intent, Intent::BuyShort);
        assert!(e.reject.is_none());
        assert!(e.unsolicited.is_none());
        assert!(e.fill.is_none());
    }

    #[test]
    fn decodes_fill_and_reject() {
        let fill = r#"{"orderSubscriptionUpdate":{"execution":{"id":"E2","type":"EXECUTION_TYPE_PARTIAL_FILL",
            "order":{"id":"O1","marketSlug":"m","side":"ORDER_SIDE_BUY","price":{"value":"0.40"},"quantity":2,"cumQuantity":0.5,"leavesQuantity":1.5,"state":"ORDER_STATE_PARTIALLY_FILLED","intent":"ORDER_INTENT_BUY_LONG"},
            "lastShares":"0.5","lastPx":{"value":"0.40"},"tradeId":"T1","aggressor":false,
            "commissionNotionalCollected":{"value":"0.0000"}}}}"#;
        let PrivateMsg::Execution(e) = decode_private_frame(fill).unwrap() else {
            panic!()
        };
        let f = e.fill.unwrap();
        assert_eq!(
            (f.last_shares, f.last_px, f.trade_id.as_str()),
            (Qty(5_000), Px(4_000), "T1")
        );
        assert_eq!(e.order.leaves_qty, Qty(15_000));

        let reject = r#"{"orderSubscriptionUpdate":{"execution":{"id":"E3","type":"EXECUTION_TYPE_REJECTED",
            "order":{"id":"O2","marketSlug":"m","side":"ORDER_SIDE_BUY","price":{"value":"0.40"},"quantity":1,"state":"ORDER_STATE_REJECTED"},
            "orderRejectReason":"ORD_REJECT_REASON_INVALID_PRICE_INCREMENT","text":"bad tick"}}}"#;
        let PrivateMsg::Execution(e) = decode_private_frame(reject).unwrap() else {
            panic!()
        };
        assert_eq!(e.reject.unwrap().class, RejectClass::OurBug);
        assert_eq!(
            classify_reject(
                "ORD_REJECT_REASON_EXCHANGE_OPTION",
                "Global Rate Limit Exceeded"
            ),
            RejectClass::Transient
        );
    }

    #[test]
    fn decodes_events_and_game_state() {
        let body = parse_json(r#"{"events":[{"id":174985,"slug":"pdcdarts-gerpri-robcro-2026-10-22","startTime":"2026-10-22T19:00:00Z",
            "eventState":{"score":"5-1","period":"LIVE","live":true,"ended":false,"updatedAt":"2026-10-22T19:19:43.260Z"},
            "markets":[{"slug":"aec-pdcdarts-gerpri-robcro-2026-10-22","active":true,"closed":false,
               "orderPriceMinTickSize":0.01,"minimumTradeQty":0.01,"feeCoefficient":0.0695,"sportsMarketType":"darts_match_winner",
               "gameStartTime":"2026-10-22T19:00:00Z",
               "marketSides":[{"long":true,"description":"Gerwyn Price","team":{"ordering":"home"}},{"long":false,"description":"Rob Cross","team":{"ordering":"away"}}]},
              {"slug":"closed-one","active":true,"closed":true}]}]}"#).unwrap();
        let (markets, games, n) = decode_events_page(&body, false);
        assert_eq!(n, 1);
        assert_eq!(markets.len(), 1);
        let m = &markets[0];
        assert_eq!((m.tick, m.min_qty), (Px(100), Qty(100)));
        assert_eq!(m.yes_ordering, Some(TeamOrdering::Home));
        assert_eq!(m.league, "pdcdarts");
        assert_eq!(m.event_id, "174985");
        assert_eq!(games[0].score, Some((5, 1)));
        assert!(games[0].live);
    }

    #[test]
    fn decodes_positions_map() {
        let body = parse_json(r#"{"positions":{"m1":{"netPositionDecimal":"-2.5","netPosition":-2,"cost":{"value":"1.20"}}},"eof":true}"#).unwrap();
        let (positions, cursor, eof) = decode_positions_page(&body).unwrap();
        assert_eq!(positions[0].net, Qty(-25_000));
        assert_eq!(positions[0].cost, Some(1.2));
        assert!(cursor.is_none() && eof);
    }

    /// Empty-account bodies as the venue sends them (2026-10-28) versus malformed ones.
    #[test]
    fn empty_rest_snapshots_must_be_explicit() {
        let empty =
            parse_json(r#"{"positions":{},"nextCursor":"","eof":true,"availablePositions":[]}"#)
                .unwrap();
        assert_eq!(decode_positions_page(&empty).unwrap(), (vec![], None, true));
        assert_eq!(
            decode_orders(&parse_json(r#"{"orders":[]}"#).unwrap()).unwrap(),
            vec![]
        );
        for body in [
            r#"{}"#,
            r#"{"positions":[]}"#,
            r#"{"positions":null,"eof":true}"#,
        ] {
            assert!(
                decode_positions_page(&parse_json(body).unwrap()).is_err(),
                "{body}"
            );
        }
        for body in [r#"{}"#, r#"{"orders":{}}"#, r#"{"orders":null}"#] {
            assert!(decode_orders(&parse_json(body).unwrap()).is_err(), "{body}");
        }
    }

    #[test]
    fn markets_with_unusable_increments_are_skipped() {
        let page = |tick: &str| {
            parse_json(&format!(r#"{{"events":[{{"id":1,"slug":"e","markets":[{{"slug":"m","active":true,"closed":false,
            "minimumTradeQty":0.01{tick}}}]}}]}}"#)).unwrap()
        };
        for bad in [
            r#","orderPriceMinTickSize":0"#,
            r#","orderPriceMinTickSize":"0.00005""#,
            r#","orderPriceMinTickSize":"x""#,
            r#","orderPriceMinTickSize":-0.01"#,
        ] {
            assert!(decode_events_page(&page(bad), false).0.is_empty(), "{bad}");
        }
        assert_eq!(
            decode_events_page(&page(r#","orderPriceMinTickSize":0.005"#), false).0[0].tick,
            Px(50)
        );
        assert_eq!(decode_events_page(&page(""), false).0[0].tick, Px(100));
    }

    /// Captured 49 times on 2026-10-27 17:04–17:10 UTC.
    #[test]
    fn auth_error_with_empty_snapshot_is_an_error() {
        let raw = r#"{"requestId":"order-snapshot","error":"rpc error: code = Unauthenticated desc = valid token required.",
            "subscriptionType":"SUBSCRIPTION_TYPE_ORDER_SNAPSHOT","orderSubscriptionSnapshot":{"orders":[],"eof":true}}"#;
        assert!(matches!(decode_private_frame(raw).unwrap(),
            PrivateMsg::SubscriptionError { request_id, error } if request_id == "order-snapshot" && error.contains("Unauthenticated")));
        let ok = r#"{"requestId":"order-snapshot","orderSubscriptionSnapshot":{"orders":[],"eof":true}}"#;
        assert_eq!(
            decode_private_frame(ok).unwrap(),
            PrivateMsg::Snapshot {
                orders: vec![],
                eof: true
            }
        );
    }

    #[test]
    fn malformed_fills_and_order_sizes_are_errors() {
        let order = r#""order":{"id":"O1","marketSlug":"m","side":"ORDER_SIDE_BUY","price":{"value":"0.40"},"quantity":2,"cumQuantity":2,"leavesQuantity":0,"state":"ORDER_STATE_FILLED"}"#;
        let frame = |extra: &str| {
            format!(
                r#"{{"orderSubscriptionUpdate":{{"execution":{{"id":"E1","type":"EXECUTION_TYPE_FILL",{order}{extra}}}}}}}"#
            )
        };
        for extra in [
            r#","lastShares":"bad""#,
            r#","lastShares":"0.0000""#,
            "",
            r#","lastShares":"2","lastPx":{"value":"x"}"#,
        ] {
            assert!(decode_private_frame(&frame(extra)).is_err(), "{extra}");
        }
        let PrivateMsg::Execution(e) =
            decode_private_frame(&frame(r#","lastShares":"2","lastPx":{"value":"0.40"}"#)).unwrap()
        else {
            panic!()
        };
        assert_eq!(e.fill.unwrap().last_shares, Qty(20_000));

        let bad_order = |field: &str| {
            let raw = format!(
                r#"{{"id":"O1","marketSlug":"m","price":{{"value":"0.40"}},"quantity":2,{field}}}"#
            );
            decode_order(&parse_json(&raw).unwrap())
        };
        assert!(bad_order(r#""leavesQuantity":"lots""#).is_err());
        assert!(bad_order(r#""cumQuantity":{"value":"?"}"#).is_err());
        assert!(
            decode_order(&parse_json(r#"{"id":"O1","price":"0.4x","quantity":2}"#).unwrap())
                .is_err()
        );
        assert_eq!(
            bad_order(r#""cumQuantity":0"#).unwrap().leaves_qty,
            Qty(20_000)
        );
    }

    /// Trimmed from the 2026-10-28 captures: the second `EXPIRED` book carries the settlement, at the
    /// same moment as the `LEDGER_ENTRY_TYPE_RESOLUTION` position update.
    #[test]
    fn settlement_is_read_from_expired_books_resolution_entries_and_rest() {
        let book = |state: &str, stats: &str| {
            format!(
                r#"{{"marketData":{{"marketSlug":"m","bids":[],"offers":[],"state":"{state}","stats":{{"sharesTraded":"19.29"{stats}}},"transactTime":"2026-10-28T11:28:48.5Z"}}}}"#
            )
        };
        let settled = |raw: &str| match decode_market_frame(raw).unwrap() {
            MarketMsg::Book(b) => b.settlement,
            other => panic!("{other:?}"),
        };
        let expired = book(
            "MARKET_STATE_EXPIRED",
            r#","settlementPx":{"value":"1.0000","currency":"USD"}"#,
        );
        assert_eq!(settled(&expired), Some(Px(10_000)));
        assert!(assert_paths_agree(&expired));
        assert_eq!(settled(&book("MARKET_STATE_EXPIRED", "")), None);
        // A default zero on a market that is still open is not a resolution.
        let open = book(
            "MARKET_STATE_OPEN",
            r#","settlementPx":{"value":"0.0000","currency":"USD"}"#,
        );
        assert_eq!(settled(&open), None);
        assert!(assert_paths_agree(&open));

        let position = |entry: &str| {
            format!(
                r#"{{"positionSubscription":{{"beforePosition":{{"netPositionDecimal":"1.9000","marketMetadata":{{"slug":"m"}}}},"afterPosition":{{"netPositionDecimal":"0.0000","realized":{{"value":"-0.4020"}},"marketMetadata":{{"slug":"m"}}}},"entryType":"{entry}"}}}}"#
            )
        };
        let PrivateMsg::Position(p) =
            decode_private_frame(&position("LEDGER_ENTRY_TYPE_RESOLUTION")).unwrap()
        else {
            panic!()
        };
        assert!(p.resolution && p.net.is_zero());
        let PrivateMsg::Position(p) =
            decode_private_frame(&position("LEDGER_ENTRY_TYPE_ORDER_EXECUTION")).unwrap()
        else {
            panic!()
        };
        assert!(!p.resolution);

        assert_eq!(
            decode_settlement(&parse_json(r#"{"slug":"m","settlement":0}"#).unwrap()),
            Ok(Px(0))
        );
        assert_eq!(
            decode_settlement(&parse_json(r#"{"slug":"m","settlement":0.5}"#).unwrap()),
            Ok(Px(5_000))
        );
        assert!(decode_settlement(&parse_json(r#"{"slug":"m"}"#).unwrap()).is_err());
        assert!(decode_settlement(&parse_json(r#"{"slug":"m","settlement":2}"#).unwrap()).is_err());
    }
}
