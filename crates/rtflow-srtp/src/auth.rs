//! HMAC-SHA1-80 authentication tag for SRTP/SRTCP (RFC 3711 §4.2).
//!
//! The authentication tag covers the authenticated portion of the packet
//! (header + encrypted payload for SRTP, or header + payload + E flag + SRTCP index
//! for SRTCP) plus a 4-byte ROC (rollover counter) appended for SRTP.

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// HMAC-SHA1-80 authenticator.
///
/// Produces and verifies 10-byte (80-bit) authentication tags.
pub struct HmacSha1_80 {
    mac: HmacSha1,
}

impl HmacSha1_80 {
    /// Create a new authenticator with the given 20-byte auth key from the KDF.
    pub fn new(key: &[u8; 20]) -> Self {
        let mac = HmacSha1::new_from_slice(key).expect("HMAC-SHA1 accepts any key length");
        Self { mac }
    }

    /// Compute the 10-byte authentication tag for the given data.
    ///
    /// For SRTP, `data` should be: RTP header + encrypted payload + ROC (4 bytes).
    /// For SRTCP, `data` should be: RTCP header + payload + E flag + SRTCP index.
    pub fn tag(&self, data: &[u8]) -> [u8; 10] {
        let mut mac = self.mac.clone();
        mac.update(data);
        let result = mac.finalize().into_bytes();
        let mut tag = [0u8; 10];
        tag.copy_from_slice(&result[..10]);
        tag
    }

    /// Verify a 10-byte authentication tag against the given data.
    ///
    /// Returns `true` if the tag is valid, `false` otherwise.
    /// Uses constant-time comparison internally.
    pub fn verify(&self, data: &[u8], tag: &[u8; 10]) -> bool {
        let mut mac = self.mac.clone();
        mac.update(data);
        let result = mac.finalize().into_bytes();
        // Constant-time comparison via subtle.
        use subtle::ConstantTimeEq;
        result[..10].ct_eq(tag).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_and_verify_roundtrip() {
        let key = [0x42u8; 20];
        let auth = HmacSha1_80::new(&key);
        let data = b"RTP packet payload for testing";
        let tag = auth.tag(data);
        assert_eq!(tag.len(), 10);
        assert!(auth.verify(data, &tag));
    }

    #[test]
    fn reject_tampered_data() {
        let key = [0x42u8; 20];
        let auth = HmacSha1_80::new(&key);
        let data = b"RTP packet payload for testing";
        let tag = auth.tag(data);
        let mut tampered = data.to_vec();
        tampered[0] ^= 0xFF;
        assert!(!auth.verify(&tampered, &tag));
    }

    #[test]
    fn reject_wrong_tag() {
        let key = [0x42u8; 20];
        let auth = HmacSha1_80::new(&key);
        let data = b"RTP packet payload for testing";
        let mut tag = auth.tag(data);
        tag[0] ^= 0xFF;
        assert!(!auth.verify(data, &tag));
    }

    #[test]
    fn different_keys_produce_different_tags() {
        let key1 = [0x01u8; 20];
        let key2 = [0x02u8; 20];
        let auth1 = HmacSha1_80::new(&key1);
        let auth2 = HmacSha1_80::new(&key2);
        let data = b"same data";
        assert_ne!(auth1.tag(data), auth2.tag(data));
    }
}
