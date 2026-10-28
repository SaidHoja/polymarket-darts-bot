//! Credentials and Ed25519 request signing: base64(sign(timestamp_ms + METHOD + path)).
//!
//! Credentials come only from the environment or `.env` at the repo root, and
//! never appear in `Debug` output.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signer as _, SigningKey};

#[derive(Clone)]
pub struct Credentials {
    pub key_id: String,
    pub secret_key: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Credentials(key_id={:?}, secret_key=<hidden>)",
            self.key_id
        )
    }
}

fn read_env_file(path: &Path) -> HashMap<String, String> {
    let mut values = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return values;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_start_matches("export ").trim();
        let mut value = value.trim();
        if value.len() >= 2
            && (value.starts_with('"') && value.ends_with('"')
                || value.starts_with('\'') && value.ends_with('\''))
        {
            value = &value[1..value.len() - 1];
        }
        values.insert(key.to_string(), value.to_string());
    }
    values
}

/// Process environment wins over the `.env` file.
pub fn load_credentials(env_file: &Path) -> Result<Credentials, String> {
    let file = read_env_file(env_file);
    let get = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| file.get(name).cloned())
    };
    match (get("POLYMARKET_KEY_ID"), get("POLYMARKET_SECRET_KEY")) {
        (Some(key_id), Some(secret_key)) => Ok(Credentials { key_id, secret_key }),
        _ => Err("POLYMARKET_KEY_ID and POLYMARKET_SECRET_KEY must be set (env or .env)".into()),
    }
}

pub struct Signer {
    key_id: String,
    key: SigningKey,
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signer(key_id={:?})", self.key_id)
    }
}

impl Signer {
    pub fn new(credentials: &Credentials) -> Result<Self, String> {
        let decoded = STANDARD
            .decode(credentials.secret_key.trim())
            .map_err(|e| format!("secret key is not base64: {e}"))?;
        // The private key is the first 32 bytes of the decoded secret.
        let seed: [u8; 32] = decoded
            .get(..32)
            .and_then(|s| s.try_into().ok())
            .ok_or("secret key decodes to fewer than 32 bytes")?;
        Ok(Signer {
            key_id: credentials.key_id.clone(),
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Headers for `method path`. `path` has no host and no query string (the query isn't signed).
    pub fn headers_at(
        &self,
        method: &str,
        path: &str,
        timestamp_ms: i64,
    ) -> [(&'static str, String); 3] {
        let message = format!("{timestamp_ms}{}{path}", method.to_ascii_uppercase());
        let signature = self.key.sign(message.as_bytes());
        [
            ("X-PM-Access-Key", self.key_id.clone()),
            ("X-PM-Timestamp", timestamp_ms.to_string()),
            ("X-PM-Signature", STANDARD.encode(signature.to_bytes())),
        ]
    }

    pub fn headers(&self, method: &str, path: &str) -> [(&'static str, String); 3] {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.headers_at(method, path, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_python_signer() {
        // Reference from the former Python signer with the same dummy secret (bytes 0..64).
        let secret = STANDARD.encode((0u8..64).collect::<Vec<_>>());
        let signer = Signer::new(&Credentials {
            key_id: "key-1".into(),
            secret_key: secret,
        })
        .unwrap();
        let headers = signer.headers_at("get", "/v1/ws/private", 1_760_000_000_000);
        assert_eq!(headers[1].1, "1760000000000");
        assert_eq!(headers[2].1, "ndrWVxQp+WoHqFoazWmrlLCHNdJCsT3g9MDGWLqYxwv5JR99kOYPYB7dPoMdfHpRTtH0aAFAfgRudbPewPi2DA==");
        assert!(!format!(
            "{:?}",
            Credentials {
                key_id: "k".into(),
                secret_key: "s3cret".into()
            }
        )
        .contains("s3cret"));
    }
}
