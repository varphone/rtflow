//! SDP crypto attribute value type (RFC 4568, `a=crypto`).

use crate::{Error, Result};

#[derive(Clone, Eq, PartialEq)]
pub struct SdpCrypto {
    pub tag: u32,
    pub crypto_suite: String,
    pub key_params: String,
    pub session_params: Option<String>,
}

impl std::fmt::Debug for SdpCrypto {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SdpCrypto")
            .field("tag", &self.tag)
            .field("crypto_suite", &self.crypto_suite)
            .field("key_params", &"[REDACTED]")
            .field("session_params", &self.session_params)
            .finish()
    }
}

impl SdpCrypto {
    pub fn decode_key_material(&self) -> Result<Vec<u8>> {
        let inline = self
            .key_params
            .split(';')
            .find_map(|p| p.strip_prefix("inline:"))
            .ok_or_else(|| Error::Rtsp("SRTP crypto key-params missing inline".into()))?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(inline)
            .map_err(|_| Error::Rtsp("SRTP crypto inline is not valid base64".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_inline_key() {
        let crypto = SdpCrypto {
            tag: 1,
            crypto_suite: "AES_128_CM_HMAC_SHA1_80".into(),
            key_params: "inline:SECRET".into(),
            session_params: None,
        };
        let rendered = format!("{crypto:?}");
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("SECRET"));
    }
}
