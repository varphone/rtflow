//! HMAC-SHA256 token authentication for HTTP streaming endpoints.
//!
//! Tokens are URL-safe and time-limited, suitable for protecting HLS/DASH/FLV
//! playback URLs. Format: `base64url(HMAC-SHA256(secret, path + "." + expires)) + "." + expires`
//!
//! # Example
//!
//! ```
//! use rtflow::token_auth::{TokenConfig, generate_token, verify_token};
//! use std::time::Duration;
//!
//! let config = TokenConfig::new(b"my-secret-key".to_vec());
//! let token = generate_token(&config, "/live/stream");
//! assert!(verify_token(&config, "/live/stream", &token));
//! assert!(!verify_token(&config, "/other/path", &token));
//! ```
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac, KeyInit};
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Configuration for HMAC token generation and verification.
#[derive(Clone, Debug)]
pub struct TokenConfig {
    /// HMAC-SHA256 secret key.
    pub secret: Vec<u8>,
    /// Token time-to-live.
    pub ttl: Duration,
    /// URL query parameter name for the token (default: "token").
    pub param_name: String,
}

impl TokenConfig {
    /// Create a new token config with the given secret and default TTL (1 hour).
    pub fn new(secret: Vec<u8>) -> Self {
        Self {
            secret,
            ttl: Duration::from_secs(3600),
            param_name: "token".to_string(),
        }
    }

    /// Set the token TTL.
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Set the URL parameter name.
    pub fn with_param_name(mut self, name: impl Into<String>) -> Self {
        self.param_name = name.into();
        self
    }
}

/// Generate a time-limited HMAC token for the given path.
///
/// The token format is: `signature.expires` where:
/// - `signature` = base64url(HMAC-SHA256(secret, path + "." + expires))
/// - `expires` = unix timestamp (seconds) when the token expires
pub fn generate_token(config: &TokenConfig, path: &str) -> String {
    let expires = current_timestamp() + config.ttl.as_secs();
    let signature = compute_signature(&config.secret, path, expires);
    format!("{signature}.{expires}")
}

/// Generate a token with a specific expiry timestamp (for testing).
pub fn generate_token_with_expiry(config: &TokenConfig, path: &str, expires: u64) -> String {
    let signature = compute_signature(&config.secret, path, expires);
    format!("{signature}.{expires}")
}

/// Verify a token for the given path.
///
/// Returns `true` if the token is valid (correct signature and not expired).
/// Uses constant-time comparison to prevent timing attacks.
pub fn verify_token(config: &TokenConfig, path: &str, token: &str) -> bool {
    let Some((signature, expires_str)) = token.rsplit_once('.') else {
        return false;
    };
    let Ok(expires) = expires_str.parse::<u64>() else {
        return false;
    };
    // Check expiry
    if current_timestamp() > expires {
        return false;
    }
    // Verify signature (constant-time)
    let expected = compute_signature(&config.secret, path, expires);
    signature.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Extract token from a URL query string.
///
/// Looks for the configured parameter name in the query string.
pub fn extract_token_from_query<'a>(config: &TokenConfig, query: &'a str) -> Option<&'a str> {
    let param = &config.param_name;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == param {
                return Some(value);
            }
        }
    }
    None
}

fn compute_signature(secret: &[u8], path: &str, expires: u64) -> String {
    let message = format!("{path}.{expires}");
    let mut mac =
        HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    let result = mac.finalize();
    URL_SAFE_NO_PAD.encode(result.into_bytes())
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_verify_roundtrip() {
        let config = TokenConfig::new(b"test-secret".to_vec());
        let token = generate_token(&config, "/live/stream");
        assert!(verify_token(&config, "/live/stream", &token));
    }

    #[test]
    fn wrong_path_fails() {
        let config = TokenConfig::new(b"test-secret".to_vec());
        let token = generate_token(&config, "/live/stream");
        assert!(!verify_token(&config, "/other/path", &token));
    }

    #[test]
    fn wrong_secret_fails() {
        let config_a = TokenConfig::new(b"secret-a".to_vec());
        let config_b = TokenConfig::new(b"secret-b".to_vec());
        let token = generate_token(&config_a, "/live/stream");
        assert!(!verify_token(&config_b, "/live/stream", &token));
    }

    #[test]
    fn expired_token_fails() {
        let config = TokenConfig::new(b"test-secret".to_vec());
        // Generate a token that expired in the past
        let past = current_timestamp() - 100;
        let token = generate_token_with_expiry(&config, "/live/stream", past);
        assert!(!verify_token(&config, "/live/stream", &token));
    }

    #[test]
    fn malformed_token_fails() {
        let config = TokenConfig::new(b"test-secret".to_vec());
        assert!(!verify_token(&config, "/live/stream", ""));
        assert!(!verify_token(&config, "/live/stream", "no-dot-separator"));
        assert!(!verify_token(&config, "/live/stream", "sig.not_a_number"));
        assert!(!verify_token(&config, "/live/stream", "sig."));
    }

    #[test]
    fn extract_token_from_query_works() {
        let config = TokenConfig::new(b"secret".to_vec());
        assert_eq!(
            extract_token_from_query(&config, "token=abc123&other=x"),
            Some("abc123")
        );
        assert_eq!(
            extract_token_from_query(&config, "other=x&token=def456"),
            Some("def456")
        );
        assert_eq!(extract_token_from_query(&config, "other=x"), None);
        assert_eq!(extract_token_from_query(&config, ""), None);
    }

    #[test]
    fn custom_param_name() {
        let config = TokenConfig::new(b"secret".to_vec()).with_param_name("key");
        assert_eq!(
            extract_token_from_query(&config, "key=abc123&token=wrong"),
            Some("abc123")
        );
    }

    #[test]
    fn custom_ttl() {
        let config = TokenConfig::new(b"secret".to_vec()).with_ttl(Duration::from_secs(60));
        assert_eq!(config.ttl, Duration::from_secs(60));
        let token = generate_token(&config, "/path");
        assert!(verify_token(&config, "/path", &token));
    }
}
