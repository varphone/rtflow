//! SDP crypto attribute value type (RFC 4568, `a=crypto`).

use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SdpCrypto {
    pub tag: u32,
    pub crypto_suite: String,
    pub key_params: String,
    pub session_params: Option<String>,
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
