//! Signed WebSocket handshakes and subscription messages.
//!
//! Both sockets use camelCase JSON and string enums. Liveness is ping/pong;
//! the reconnect loop lives in the caller, which also owns resubscription.

use pm_types::Slug;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::auth::Signer;

pub const MARKETS_WS_URL: &str = "wss://api.polymarket.us/v1/ws/markets";
pub const PRIVATE_WS_URL: &str = "wss://api.polymarket.us/v1/ws/private";
pub const MAX_SLUGS: usize = 100;

pub const MARKET_DATA: &str = "SUBSCRIPTION_TYPE_MARKET_DATA";
pub const TRADE: &str = "SUBSCRIPTION_TYPE_TRADE";
pub const ORDER: &str = "SUBSCRIPTION_TYPE_ORDER";
pub const ORDER_SNAPSHOT: &str = "SUBSCRIPTION_TYPE_ORDER_SNAPSHOT";
pub const POSITION: &str = "SUBSCRIPTION_TYPE_POSITION";
pub const ACCOUNT_BALANCE: &str = "SUBSCRIPTION_TYPE_ACCOUNT_BALANCE";

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug)]
pub enum ConnectError {
    /// HTTP 401 on the handshake: retrying won't help.
    Unauthorized(String),
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Unauthorized(e) => write!(f, "unauthorized: {e}"),
            ConnectError::Other(e) => write!(f, "{e}"),
        }
    }
}

/// Connect with the signed handshake: the signed string is `GET` plus the socket path.
pub async fn connect_signed(url: &str, signer: &Signer) -> Result<Socket, ConnectError> {
    let path = url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
        .unwrap_or("/");
    let mut request = url
        .into_client_request()
        .map_err(|e| ConnectError::Other(e.to_string()))?;
    for (name, value) in signer.headers("GET", path) {
        request.headers_mut().insert(
            name,
            HeaderValue::from_str(&value).map_err(|e| ConnectError::Other(e.to_string()))?,
        );
    }
    match connect_async(request).await {
        Ok((socket, _)) => Ok(socket),
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if response.status().as_u16() == 401 =>
        {
            let body = response
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            Err(ConnectError::Unauthorized(body))
        }
        Err(e) => Err(ConnectError::Other(e.to_string())),
    }
}

pub fn subscribe(request_id: &str, subscription_type: &str, slugs: &[Slug]) -> String {
    let slugs: Vec<&str> = slugs.iter().map(|s| &**s).collect();
    let mut body = json!({"requestId": request_id, "subscriptionType": subscription_type, "marketSlugs": slugs});
    if subscription_type == MARKET_DATA {
        body["responsesDebounced"] = Value::Bool(false);
    }
    json!({"subscribe": body}).to_string()
}

/// Book and trade subscriptions for `slugs`, at most 100 slugs each.
pub fn market_subscriptions(slugs: &[Slug]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, group) in slugs.chunks(MAX_SLUGS).enumerate() {
        out.push(subscribe(&format!("md-{i}"), MARKET_DATA, group));
        out.push(subscribe(&format!("tr-{i}"), TRADE, group));
    }
    out
}

/// The order that worked live: update streams first, then the snapshot.
pub fn private_subscriptions() -> Vec<String> {
    [
        ("orders", ORDER),
        ("positions", POSITION),
        ("balances", ACCOUNT_BALANCE),
        ("order-snapshot", ORDER_SNAPSHOT),
    ]
    .iter()
    .map(|(id, kind)| subscribe(id, kind, &[]))
    .collect()
}
