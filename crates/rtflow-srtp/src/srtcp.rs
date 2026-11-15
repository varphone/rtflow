//! SRTCP encryption and authentication (RFC 3711 §3.4).
//!
//! SRTCP uses the same AES-128-CM cipher and HMAC-SHA1-80 authentication as SRTP,
//! but with separate keys and a different packet format:
//! - A 32-bit SRTCP index with an E (encryption) flag in the MSB.
//! - The authentication tag covers the entire SRTCP packet including the index.

use rtflow_core::error::{Error, Result};
use bytes::{BufMut, Bytes, BytesMut};

use super::auth::HmacSha1_80;
use super::cipher::Aes128Cm;
use super::replay::ReplayWindow;

/// Minimum SRTCP packet size: 8 bytes RTCP header.
const MIN_SRTCP_SIZE: usize = 8;

/// SRTCP encryptor/decryptor.
pub struct SrtcpContext {
    cipher: Aes128Cm,
    auth: HmacSha1_80,
    send_index: u32,
    replay: ReplayWindow,
    replay_initialized: bool,
}

impl SrtcpContext {
    /// Create a new SRTCP context with derived session keys.
    pub fn new(cipher_key: &[u8; 16], auth_key: &[u8; 20], salt: &[u8; 14]) -> Self {
        Self {
            cipher: Aes128Cm::new(cipher_key, salt),
            auth: HmacSha1_80::new(auth_key),
            send_index: 0,
            replay: ReplayWindow::new(),
            replay_initialized: false,
        }
    }

    /// Protect an RTCP compound packet: encrypt payload and append SRTCP index + auth tag.
    ///
    /// Input: raw RTCP compound packet (header + payload).
    /// Output: header + encrypted payload || SRTCP index (4 bytes) || auth tag (10 bytes).
    pub fn protect(&mut self, packet: &[u8]) -> Result<Bytes> {
        if packet.len() < MIN_SRTCP_SIZE {
            return Err(Error::Media("SRTCP packet too short".into()));
        }

        // RTCP header: first 8 bytes are not encrypted per RFC 3711 §3.4.
        let header_len = 8;
        let ssrc_start = 4;
        let ssrc = u32::from_be_bytes([
            packet[ssrc_start],
            packet[ssrc_start + 1],
            packet[ssrc_start + 2],
            packet[ssrc_start + 3],
        ]);

        let index = self.send_index;
        self.send_index = self.send_index.wrapping_add(1);

        // E flag (encryption) = 1, combined with 31-bit index.
        let srtcp_index = 0x8000_0000 | (index & 0x7FFF_FFFF);

        // Build output: header + encrypted payload + SRTCP index.
        let mut output = BytesMut::with_capacity(packet.len() + 4 + 10);
        output.extend_from_slice(&packet[..header_len]);

        // Encrypt payload portion (everything after the first 8 bytes).
        let payload = &packet[header_len..];
        let encrypted = self.cipher.apply_to_vec(ssrc, index as u64, payload);
        output.extend_from_slice(&encrypted);

        // Append SRTCP index.
        output.put_u32(srtcp_index);

        // Compute auth tag over: header + encrypted payload + SRTCP index.
        let tag = self.auth.tag(&output);
        output.extend_from_slice(&tag);

        Ok(output.freeze())
    }

    /// Unprotect an SRTCP packet: verify auth tag, decrypt payload.
    ///
    /// Input: SRTCP packet (header + encrypted payload + SRTCP index + auth tag).
    /// Output: original RTCP packet (header + payload).
    pub fn unprotect(&mut self, packet: &[u8]) -> Result<Bytes> {
        if packet.len() < MIN_SRTCP_SIZE + 4 + 10 {
            return Err(Error::Media("SRTCP packet too short".into()));
        }

        // Split off auth tag (last 10 bytes).
        let (body, tag_bytes) = packet.split_at(packet.len() - 10);
        let mut tag = [0u8; 10];
        tag.copy_from_slice(tag_bytes);

        // Verify auth tag.
        if !self.auth.verify(body, &tag) {
            return Err(Error::Media("SRTCP authentication failed".into()));
        }

        // Split off SRTCP index (4 bytes before the tag).
        let (payload_with_index, _) = body.split_at(body.len() - 4);
        let srtcp_index_bytes = &body[body.len() - 4..];
        let srtcp_index = u32::from_be_bytes([
            srtcp_index_bytes[0],
            srtcp_index_bytes[1],
            srtcp_index_bytes[2],
            srtcp_index_bytes[3],
        ]);

        let encrypted = (srtcp_index & 0x8000_0000) != 0;
        let index = srtcp_index & 0x7FFF_FFFF;
        if !self.replay_initialized {
            self.replay_initialized = true;
        }
        if !self.replay.check(u64::from(index)) {
            return Err(Error::Media("SRTCP replay detected".into()));
        }

        // RTCP header (first 8 bytes) + payload (rest minus index).
        let header_len = 8;
        let ssrc = u32::from_be_bytes([
            payload_with_index[4],
            payload_with_index[5],
            payload_with_index[6],
            payload_with_index[7],
        ]);

        let mut output = BytesMut::with_capacity(payload_with_index.len());
        output.extend_from_slice(&payload_with_index[..header_len]);

        if encrypted {
            let payload = &payload_with_index[header_len..];
            let decrypted = self.cipher.apply_to_vec(ssrc, index as u64, payload);
            output.extend_from_slice(&decrypted);
        } else {
            output.extend_from_slice(&payload_with_index[header_len..]);
        }

        Ok(output.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_unprotect_roundtrip() {
        let cipher_key = [0x01u8; 16];
        let auth_key = [0x02u8; 20];
        let salt = [0x03u8; 14];

        // Minimal RTCP SR: 8 bytes header + 20 bytes body = 28 bytes.
        let rtcp = vec![
            0x80u8, 0xC8, 0x00, 0x06, // V=2, P=0, RC=0, PT=200, length=6
            0x00, 0x00, 0x00, 0x01, // SSRC = 1
            0xD4, 0x35, 0x12, 0x84, 0x6A, 0x86, 0x98, 0x5E, // NTP timestamp
            0x00, 0x00, 0x00, 0x00, // RTP timestamp
            0x00, 0x00, 0x00, 0x00, // Sender packet count
            0x00, 0x00, 0x00, 0x00, // Sender octet count
        ];

        let mut sender = SrtcpContext::new(&cipher_key, &auth_key, &salt);
        let mut receiver = SrtcpContext::new(&cipher_key, &auth_key, &salt);

        let protected = sender.protect(&rtcp).unwrap();
        // Protected = 28 (original) + 4 (index) + 10 (tag) = 42 bytes.
        assert_eq!(protected.len(), 42);

        let unprotected = receiver.unprotect(&protected).unwrap();
        assert_eq!(&unprotected[..], &rtcp[..]);
    }

    #[test]
    fn rejects_tampered_packet() {
        let cipher_key = [0x01u8; 16];
        let auth_key = [0x02u8; 20];
        let salt = [0x03u8; 14];

        let rtcp = vec![
            0x80u8, 0xC8, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let mut sender = SrtcpContext::new(&cipher_key, &auth_key, &salt);
        let mut receiver = SrtcpContext::new(&cipher_key, &auth_key, &salt);

        let mut protected = sender.protect(&rtcp).unwrap().to_vec();
        // Tamper with a byte in the encrypted payload.
        protected[10] ^= 0xFF;

        assert!(receiver.unprotect(&protected).is_err());
    }

    #[test]
    fn rejects_truncated_packet() {
        let cipher_key = [0x01u8; 16];
        let auth_key = [0x02u8; 20];
        let salt = [0x03u8; 14];
        let mut receiver = SrtcpContext::new(&cipher_key, &auth_key, &salt);
        assert!(receiver.unprotect(&[0u8; 20]).is_err());
    }
    #[test]
    fn rejects_replayed_srtcp_packet() {
        let cipher_key = [0x01u8; 16];
        let auth_key = [0x02u8; 20];
        let salt = [0x03u8; 14];
        let rtcp = vec![
            0x80u8, 0xC8, 0x00, 0x06, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
        ];
        let mut sender = SrtcpContext::new(&cipher_key, &auth_key, &salt);
        let mut receiver = SrtcpContext::new(&cipher_key, &auth_key, &salt);
        let first = sender.protect(&rtcp).unwrap();
        let second = sender.protect(&rtcp).unwrap();
        receiver.unprotect(&first).unwrap();
        receiver.unprotect(&second).unwrap();
        assert!(receiver.unprotect(&first).is_err());
    }
}
