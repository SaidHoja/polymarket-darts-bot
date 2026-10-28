//! REST client for the Retail API: public gateway reads and signed trading calls.
//!
//! The client sends one request and reports what happened. All requests share
//! one token bucket ([`RateLimiter`], owned here) because limits are counted per
//! source IP, and every response feeds its 429 back-off. Each order action costs
//! one token, also inside a batch (a conservative local choice; the venue docs don't say whether batch entries are charged separately).
//!
//! Who pays is the caller's choice ([`Budget`]): the order gateway (`pm-live`)
//! owns queue priority and retries, so it takes its tokens itself (`Prepaid`, one
//! per action as it builds a batch). Any other caller (`Normal`, `Low`, and the
//! gateway's single retry) is charged here, one token per batch entry.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pm_types::time::format_rfc3339_secs;
use pm_types::{BalanceFrame, NewOrder, OrderId, PositionFrame, Px, Qty, Tif, VenueOrder};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::auth::Signer;
use crate::decode;

pub const GATEWAY_URL: &str = "https://gateway.polymarket.us";
pub const API_URL: &str = "https://api.polymarket.us";
pub const BATCH_LIMIT: usize = 20;

#[derive(Debug, Clone)]
pub enum RestError {
    /// The server answered with an error status.
    Status {
        status: u16,
        body: String,
    },
    /// No usable answer: timeout, reset, TLS. The request may have taken effect.
    Transport(String),
    Decode(String),
}

impl std::fmt::Display for RestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestError::Status { status, body } => {
                write!(f, "HTTP {status}: {}", decode::truncate(body, 300))
            }
            RestError::Transport(e) => write!(f, "transport: {e}"),
            RestError::Decode(e) => write!(f, "decode: {e}"),
        }
    }
}

impl std::error::Error for RestError {}

/// Token bucket shared by every REST caller. Low-priority callers (game polls,
/// reconciliation reads) leave `reserve` tokens for order traffic.
pub struct RateLimiter {
    enabled: bool,
    rate: f64,
    capacity: f64,
    state: Mutex<(f64, Instant)>,
    /// Venue-imposed cooldown is independent of proactive local throttling.
    backoff: Mutex<(Instant, u32)>,
    http_429: AtomicU64,
}

impl RateLimiter {
    pub fn new(rate: f64) -> Result<Arc<Self>, String> {
        Self::with_enabled(rate, true)
    }

    /// `rate` is tokens per second and also the bucket size; it must exceed the low-priority reserve.
    pub fn with_enabled(rate: f64, enabled: bool) -> Result<Arc<Self>, String> {
        if !(rate.is_finite() && rate > 3.0) {
            return Err(format!(
                "rate limit {rate}/s must be finite and above the low-priority reserve of 3"
            ));
        }
        Ok(Arc::new(RateLimiter {
            enabled,
            rate,
            capacity: rate,
            state: Mutex::new((rate, Instant::now())),
            backoff: Mutex::new((Instant::now(), 0)),
            http_429: AtomicU64::new(0),
        }))
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn http_429_total(&self) -> u64 {
        self.http_429.load(Ordering::Relaxed)
    }

    pub async fn wait_backoff(&self) {
        loop {
            let until = self.backoff.lock().await.0;
            let remaining = until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            tokio::time::sleep(remaining).await;
        }
    }

    async fn succeeded(&self) {
        let mut backoff = self.backoff.lock().await;
        // A success already in flight before a 429 must not end its cooldown.
        if Instant::now() >= backoff.0 {
            backoff.1 = 0;
        }
    }

    async fn take(&self, reserve: f64) {
        loop {
            self.wait_backoff().await;
            if !self.enabled {
                return;
            }
            let wait = {
                let mut state = self.state.lock().await;
                let now = Instant::now();
                state.0 = (state.0 + now.duration_since(state.1).as_secs_f64() * self.rate)
                    .min(self.capacity);
                state.1 = now;
                if state.0 >= 1.0 + reserve {
                    state.0 -= 1.0;
                    return;
                }
                (1.0 + reserve - state.0) / self.rate
            };
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }

    pub async fn acquire(&self) {
        self.take(0.0).await
    }

    pub async fn acquire_low(&self) {
        self.take(3.0).await
    }

    /// Pay for a request of `cost` actions under `budget`. One token at a time, so a batch
    /// larger than the bucket still gets through; prepaid requests only wait out a 429 back-off.
    pub async fn charge(&self, budget: Budget, cost: usize) {
        match budget {
            Budget::Normal => {
                for _ in 0..cost.max(1) {
                    self.acquire().await
                }
            }
            Budget::Low => {
                for _ in 0..cost.max(1) {
                    self.acquire_low().await
                }
            }
            Budget::Prepaid => self.wait_backoff().await,
        }
    }

    /// Take a token only if one is available now.
    pub async fn try_acquire(&self) -> bool {
        if self.backoff.lock().await.0 > Instant::now() {
            return false;
        }
        if !self.enabled {
            return true;
        }
        let mut state = self.state.lock().await;
        let now = Instant::now();
        state.0 =
            (state.0 + now.duration_since(state.1).as_secs_f64() * self.rate).min(self.capacity);
        state.1 = now;
        if state.0 >= 1.0 {
            state.0 -= 1.0;
            true
        } else {
            false
        }
    }

    /// Actual HTTP 429: shared exponential cooldown (1, 2, 4, …, 32 seconds),
    /// even when proactive throttling is disabled.
    pub async fn drain(&self) {
        self.http_429.fetch_add(1, Ordering::Relaxed);
        let mut backoff = self.backoff.lock().await;
        let seconds = 1u64 << backoff.1.min(5);
        backoff.1 = backoff.1.saturating_add(1);
        backoff.0 = backoff.0.max(Instant::now() + Duration::from_secs(seconds));
        drop(backoff);
        let mut state = self.state.lock().await;
        state.0 = -self.rate;
        state.1 = Instant::now();
    }

    /// Fraction of capacity currently used (0 = idle, 1 = exhausted).
    pub async fn utilisation(&self) -> f64 {
        if !self.enabled {
            return 0.0;
        }
        let state = self.state.lock().await;
        1.0 - (state.0 / self.capacity).clamp(0.0, 1.0)
    }
}

/// Who pays for a request against the shared rate budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Normal,
    /// Leaves headroom for order traffic (game polls, reconciliation reads).
    Low,
    /// The caller already took the token (the order gateway does, so it can prioritise).
    Prepaid,
}

pub struct RestClient {
    http: reqwest::Client,
    signer: Option<Arc<Signer>>,
    pub limiter: Arc<RateLimiter>,
    dispatch_observer: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn amount(px: Px) -> Value {
    json!({"value": px.to_string(), "currency": "USD"})
}

/// Quantity is a JSON number on the wire; build it from the exact decimal text.
fn quantity(q: Qty) -> Value {
    Value::Number(
        q.to_string()
            .parse()
            .expect("fixed-point text is a valid JSON number"),
    )
}

fn tif_fields(body: &mut serde_json::Map<String, Value>, tif: Tif) {
    let Tif::Gtd(ns) = tif;
    body.insert("tif".into(), json!("TIME_IN_FORCE_GOOD_TILL_DATE"));
    body.insert("goodTillTime".into(), json!(format_rfc3339_secs(ns)));
}

/// `CreateOrderRequest` body. The price is always the YES price.
pub fn order_body(order: &NewOrder) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("marketSlug".into(), json!(&*order.slug));
    body.insert("type".into(), json!("ORDER_TYPE_LIMIT"));
    body.insert("price".into(), amount(order.px));
    body.insert("quantity".into(), quantity(order.qty));
    body.insert("intent".into(), json!(order.intent.wire()));
    body.insert(
        "manualOrderIndicator".into(),
        json!("MANUAL_ORDER_INDICATOR_AUTOMATIC"),
    );
    body.insert("participateDontInitiate".into(), json!(order.post_only));
    tif_fields(&mut body, order.tif);
    Value::Object(body)
}

pub fn modify_body(
    order_id: &str,
    slug: &str,
    px: Px,
    qty: Qty,
    tif: Tif,
    post_only: bool,
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("orderId".into(), json!(order_id));
    body.insert("marketSlug".into(), json!(slug));
    body.insert("price".into(), amount(px));
    body.insert("quantity".into(), quantity(qty));
    body.insert("participateDontInitiate".into(), json!(post_only));
    tif_fields(&mut body, tif);
    Value::Object(body)
}

/// The events of one discovery page. An empty result is `"events": []`; anything else would
/// silently drop every market.
pub fn events_page(mut body: Value) -> Result<Vec<Value>, RestError> {
    if let Some(Value::Array(events)) = body.get_mut("events") {
        return Ok(std::mem::take(events));
    }
    Err(RestError::Decode(format!(
        "events page without an events array: {}",
        decode::snippet(&body)
    )))
}

/// Per entry of a batched create, in request order: the venue id, if one was echoed.
pub fn created_ids(response: &Value, n: usize) -> Vec<Option<OrderId>> {
    let ids = response
        .get("createdOrderIds")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    (0..n)
        .map(|i| {
            ids.get(i)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(Arc::from)
        })
        .collect()
}

/// The gateway never builds a batch outside 1..=BATCH_LIMIT; if it did, refuse before sending.
fn check_batch(n: usize) -> Result<(), RestError> {
    if n == 0 || n > BATCH_LIMIT {
        return Err(RestError::Decode(format!(
            "batch of {n} actions, limit {BATCH_LIMIT}; not sent"
        )));
    }
    Ok(())
}

impl RestClient {
    pub fn new(signer: Option<Arc<Signer>>, limiter: Arc<RateLimiter>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_nodelay(true)
            .build()
            .map_err(|e| format!("HTTP client: {e}"))?;
        Ok(RestClient {
            http,
            signer,
            limiter,
            dispatch_observer: None,
        })
    }

    /// A request-scoped view sharing the connection pool and limiter. Observers
    /// run after serialization/signing, immediately before HTTP execution.
    /// The original shared client and other concurrent requests are unaffected.
    pub fn with_dispatch_observer(&self, observer: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            http: self.http.clone(),
            signer: self.signer.clone(),
            limiter: self.limiter.clone(),
            dispatch_observer: Some(observer),
        }
    }

    /// One request, no retries, costing one token.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        signed: bool,
        query: &[(&str, String)],
        body: Option<&Value>,
        budget: Budget,
    ) -> Result<Value, RestError> {
        self.request_weighted(method, path, signed, query, body, budget, 1)
            .await
    }

    /// One request costing `cost` tokens (one per order action in a batch) unless prepaid.
    #[allow(clippy::too_many_arguments)]
    async fn request_weighted(
        &self,
        method: &str,
        path: &str,
        signed: bool,
        query: &[(&str, String)],
        body: Option<&Value>,
        budget: Budget,
        cost: usize,
    ) -> Result<Value, RestError> {
        self.limiter.charge(budget, cost).await;
        let base = if signed { API_URL } else { GATEWAY_URL };
        let method_v = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| RestError::Decode(e.to_string()))?;
        let mut request = self
            .http
            .request(method_v, format!("{base}{path}"))
            .header("Accept", "application/json");
        if !query.is_empty() {
            request = request.query(query);
        }
        if signed {
            let signer = self
                .signer
                .as_ref()
                .ok_or_else(|| RestError::Decode(format!("{path} needs credentials")))?;
            for (name, value) in signer.headers(method, path) {
                request = request.header(name, value);
            }
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let request = request
            .build()
            .map_err(|e| RestError::Transport(e.to_string()))?;
        if let Some(observer) = &self.dispatch_observer {
            observer();
        }
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| RestError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        if status == 429 {
            self.limiter.drain().await;
        } else if status < 400 {
            self.limiter.succeeded().await;
        }
        let text = response
            .text()
            .await
            .map_err(|e| RestError::Transport(e.to_string()))?;
        if status >= 400 {
            return Err(RestError::Status { status, body: text });
        }
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        decode::parse_json(&text).map_err(RestError::Decode)
    }

    // ------------------------------------------------------------ public reads

    /// All darts events of one kind (`sport` or `futures`), paging until a short page.
    pub async fn darts_events(&self, kind: &str, budget: Budget) -> Result<Vec<Value>, RestError> {
        let mut events = Vec::new();
        let mut offset = 0;
        loop {
            let query = [
                ("limit", "100".to_string()),
                ("offset", offset.to_string()),
                ("type", kind.to_string()),
            ];
            let body = self
                .request(
                    "GET",
                    "/v2/sports/darts/events",
                    false,
                    &query,
                    None,
                    budget,
                )
                .await?;
            let page = events_page(body)?;
            let n = page.len();
            events.extend(page);
            if n < 100 {
                return Ok(events);
            }
            offset += 100;
        }
    }

    pub async fn market(&self, slug: &str) -> Result<Value, RestError> {
        self.request(
            "GET",
            &format!("/v1/market/slug/{slug}"),
            false,
            &[],
            None,
            Budget::Low,
        )
        .await
    }

    /// The YES settlement price (public): `None` while the market is unresolved (404).
    pub async fn settlement(&self, slug: &str, budget: Budget) -> Result<Option<Px>, RestError> {
        match self
            .request(
                "GET",
                &format!("/v1/markets/{slug}/settlement"),
                false,
                &[],
                None,
                budget,
            )
            .await
        {
            Ok(body) => decode::decode_settlement(&body)
                .map(Some)
                .map_err(RestError::Decode),
            Err(RestError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // ------------------------------------------------------------ account reads

    pub async fn balance(&self, budget: Budget) -> Result<Option<BalanceFrame>, RestError> {
        let body = self
            .request("GET", "/v1/account/balances", true, &[], None, budget)
            .await?;
        Ok(decode::decode_balances(&body))
    }

    pub async fn positions(&self, budget: Budget) -> Result<Vec<PositionFrame>, RestError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query = vec![("limit", "100".to_string())];
            if let Some(c) = &cursor {
                query.push(("cursor", c.clone()));
            }
            let body = self
                .request("GET", "/v1/portfolio/positions", true, &query, None, budget)
                .await?;
            let (page, next, eof) =
                decode::decode_positions_page(&body).map_err(RestError::Decode)?;
            out.extend(page);
            match next {
                Some(next) if cursor.as_ref() == Some(&next) => {
                    return Err(RestError::Decode(format!(
                        "positions cursor did not advance: {next}"
                    )));
                }
                Some(next) if !eof => cursor = Some(next),
                _ => return Ok(out),
            }
        }
    }

    pub async fn open_orders(
        &self,
        slugs: &[Arc<str>],
        budget: Budget,
    ) -> Result<Vec<VenueOrder>, RestError> {
        let query: Vec<(&str, String)> = slugs.iter().map(|s| ("slugs", s.to_string())).collect();
        let body = self
            .request("GET", "/v1/orders/open", true, &query, None, budget)
            .await?;
        decode::decode_orders(&body).map_err(RestError::Decode)
    }

    /// `None` for a 404 (`Order not found`). A 200 without an order is a decode error.
    pub async fn order(&self, id: &str, budget: Budget) -> Result<Option<VenueOrder>, RestError> {
        match self
            .request("GET", &format!("/v1/order/{id}"), true, &[], None, budget)
            .await
        {
            Ok(body) => match body.get("order") {
                Some(order) => decode::decode_order(order)
                    .map(Some)
                    .map_err(RestError::Decode),
                None => Err(RestError::Decode(format!(
                    "order {id}: response without an order: {}",
                    decode::snippet(&body)
                ))),
            },
            Err(RestError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // ------------------------------------------------------------ orders
    // Responses only echo ids: outcomes arrive on the private socket.

    pub async fn create_orders(
        &self,
        orders: &[NewOrder],
        budget: Budget,
    ) -> Result<Vec<Option<OrderId>>, RestError> {
        if orders.len() == 1 {
            let body = self
                .request(
                    "POST",
                    "/v1/orders",
                    true,
                    &[],
                    Some(&order_body(&orders[0])),
                    budget,
                )
                .await?;
            return Ok(vec![body.get("id").and_then(Value::as_str).map(Arc::from)]);
        }
        check_batch(orders.len())?;
        let body = json!({"orders": orders.iter().map(order_body).collect::<Vec<_>>()});
        let response = self
            .request_weighted(
                "POST",
                "/v1/orders/batched",
                true,
                &[],
                Some(&body),
                budget,
                orders.len(),
            )
            .await?;
        Ok(created_ids(&response, orders.len()))
    }

    /// Entries are `modify_body` values. The response ids are an echo, not a confirmation.
    pub async fn modify_orders(
        &self,
        entries: Vec<Value>,
        budget: Budget,
    ) -> Result<(), RestError> {
        check_batch(entries.len())?;
        let n = entries.len();
        self.request_weighted(
            "POST",
            "/v1/orders/batched/modify",
            true,
            &[],
            Some(&json!({"orders": entries})),
            budget,
            n,
        )
        .await?;
        Ok(())
    }

    /// `(order_id, slug)` pairs. Unknown ids are silently ignored by the venue.
    pub async fn cancel_orders(
        &self,
        orders: &[(OrderId, Arc<str>)],
        budget: Budget,
    ) -> Result<(), RestError> {
        check_batch(orders.len())?;
        let entries: Vec<Value> = orders
            .iter()
            .map(|(id, slug)| json!({"orderId": &**id, "marketSlug": &**slug}))
            .collect();
        self.request_weighted(
            "POST",
            "/v1/orders/batched/cancel",
            true,
            &[],
            Some(&json!({"orders": entries})),
            budget,
            orders.len(),
        )
        .await?;
        Ok(())
    }

    pub async fn cancel_all(&self, slugs: &[Arc<str>], budget: Budget) -> Result<(), RestError> {
        let slugs: Vec<&str> = slugs.iter().map(|s| &**s).collect();
        self.request(
            "POST",
            "/v1/orders/open/cancel",
            true,
            &[],
            Some(&json!({"slugs": slugs})),
            budget,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_types::Intent;

    #[tokio::test]
    async fn disabled_limiter_bypasses_tokens_for_normal_low_and_batch_callers() {
        let enabled = RateLimiter::new(15.0).unwrap();
        for _ in 0..15 {
            assert!(enabled.try_acquire().await);
        }
        assert!(!enabled.try_acquire().await);
        let disabled = RateLimiter::with_enabled(15.0, false).unwrap();
        assert!(!disabled.enabled());
        for _ in 0..100 {
            assert!(disabled.try_acquire().await);
        }
        tokio::time::timeout(Duration::from_millis(100), async {
            for _ in 0..100 {
                disabled.acquire().await;
                disabled.acquire_low().await;
            }
        })
        .await
        .expect("disabled limiter must not wait for replenishment");
        assert_eq!(disabled.utilisation().await, 0.0);
    }

    #[tokio::test]
    async fn actual_429_backoff_remains_active_when_local_limiter_is_disabled() {
        let limiter = RateLimiter::with_enabled(15.0, false).unwrap();
        limiter.drain().await;
        assert_eq!(limiter.http_429_total(), 1);
        assert!(!limiter.try_acquire().await);
        assert!(
            limiter
                .backoff
                .lock()
                .await
                .0
                .duration_since(Instant::now())
                > Duration::from_millis(900)
        );
        limiter.succeeded().await; // response already in flight must not clear cooldown
        assert!(
            tokio::time::timeout(Duration::from_millis(20), limiter.acquire())
                .await
                .is_err()
        );
        limiter.drain().await;
        assert_eq!(limiter.http_429_total(), 2);
        assert!(
            limiter
                .backoff
                .lock()
                .await
                .0
                .duration_since(Instant::now())
                > Duration::from_millis(1900)
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), limiter.wait_backoff())
                .await
                .is_err()
        );
        // A later success resets escalation, without needing a multi-second test sleep.
        limiter.backoff.lock().await.0 = Instant::now();
        limiter.succeeded().await;
        assert_eq!(limiter.backoff.lock().await.1, 0);
        assert!(limiter.try_acquire().await);
    }

    #[tokio::test]
    async fn batches_are_charged_per_action_unless_prepaid() {
        let available = |l: Arc<RateLimiter>| async move {
            let mut n = 0;
            while l.try_acquire().await {
                n += 1;
            }
            n
        };
        let limiter = RateLimiter::new(15.0).unwrap();
        limiter.charge(Budget::Normal, 10).await;
        assert_eq!(available(limiter).await, 5);
        let limiter = RateLimiter::new(15.0).unwrap();
        limiter.charge(Budget::Prepaid, 10).await;
        assert_eq!(available(limiter).await, 15);
        // Larger than the bucket: waits for refills instead of never fitting.
        let limiter = RateLimiter::new(15.0).unwrap();
        tokio::time::timeout(Duration::from_secs(3), limiter.charge(Budget::Normal, 20))
            .await
            .expect("20 actions at 15/s");
    }

    #[test]
    fn long_non_ascii_error_body_formats_without_panicking() {
        let body = format!("{}é", "a".repeat(299));
        let text = RestError::Status { status: 500, body }.to_string();
        assert_eq!(text, format!("HTTP 500: {}", "a".repeat(299)));
        assert!(
            check_batch(0).is_err()
                && check_batch(BATCH_LIMIT + 1).is_err()
                && check_batch(BATCH_LIMIT).is_ok()
        );
    }

    #[test]
    fn order_body_matches_venue_shape() {
        let order = NewOrder {
            slug: Arc::from("m"),
            intent: Intent::BuyShort,
            px: Px(5550),
            qty: Qty(5_000),
            tif: Tif::Gtd(pm_types::time::parse_rfc3339_ns("2026-10-20T14:40:00.7Z").unwrap()),
            post_only: true,
        };
        let body = order_body(&order);
        assert_eq!(body["price"]["value"], "0.5550");
        assert_eq!(body["quantity"].to_string(), "0.5000");
        assert_eq!(body["intent"], "ORDER_INTENT_BUY_SHORT");
        assert_eq!(body["tif"], "TIME_IN_FORCE_GOOD_TILL_DATE");
        assert_eq!(body["goodTillTime"], "2026-10-20T14:40:00Z");
        assert_eq!(body["participateDontInitiate"], true);
        assert_eq!(
            body["manualOrderIndicator"],
            "MANUAL_ORDER_INDICATOR_AUTOMATIC"
        );
    }
}
