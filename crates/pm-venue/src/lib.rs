//! Polymarket US Retail API: signing, JSON decoding, REST and WebSocket plumbing.

pub mod auth;
pub mod decode;
pub mod rest;
pub mod ws;

pub use auth::{load_credentials, Credentials, Signer};
pub use rest::{Budget, RateLimiter, RestClient, RestError};
