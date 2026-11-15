//! SRTP secure transport (RFC 3711).
//!
//! Provides encryption and authentication for RTP packets over UDP, protecting
//! against eavesdropping and tampering. The SRTP context holds session keys
//! derived from a master key/salt pair and manages per-SSRC receiver state
//! including replay protection.
//!
//! # Design
//!
//! - SRTP operates as an independent layer: call [`SrtpContext::protect_rtp`] before
//!   sending and [`SrtpContext::unprotect_rtp`] after receiving.
//! - The normal [`rtflow_rtp::RtpPacket::parse`] path operates on plaintext;
//!   SRTP decryption happens first, producing the plaintext bytes.
//! - Session keys are derived once via `key_derive::derive_session_keys`.
//! - SRTCP is handled separately via [`SrtcpContext`].

mod auth;
mod cipher;
mod key_derive;
mod replay;
mod srtcp;

use rtflow_core::error::{Error, Result};
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

pub use key_derive::SessionKeys;
pub use srtcp::SrtcpContext;

/// SRTP profile identifier.
///
/// Currently only `AES_128_CM_HMAC_SHA1_80` is supported (RFC 3711 default).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SrtpProfile(u8);

impl SrtpProfile {
    /// AES-128-CM cipher with HMAC-SHA1-80 authentication tag.
    pub const AES_128_CM_HMAC_SHA1_80: Self = Self(1);
}

/// SRTP context for protecting and unprotecting RTP packets.
///
/// Holds the send-direction keys and per-SSRC receiver state.
pub struct SrtpContext {
    // Send direction
    send_cipher: cipher::Aes128Cm,
    send_auth: auth::HmacSha1_80,
    send_state: HashMap<u32, SrtpSendState>,
    // Receive direction (one receiver per SSRC)
    receive: HashMap<u32, SrtpReceiver>,
    // Session keys for creating new receivers on demand
    cipher_key: [u8; 16],
    auth_key: [u8; 20],
    salt: [u8; 14],
    // RTCP session keys, for SRTCP contexts derived from the same master keys.
    rtcp_cipher_key: [u8; 16],
    rtcp_auth_key: [u8; 20],
    rtcp_salt: [u8; 14],
}

/// Per-SSRC send index state.
struct SrtpSendState {
    roc: u32,
    last_seq: u16,
    highest_index: u64,
    initialized: bool,
}

/// Per-SSRC receiver state.
struct SrtpReceiver {
    cipher: cipher::Aes128Cm,
    auth: auth::HmacSha1_80,
    replay: replay::ReplayWindow,
    roc: u32,
    last_seq: u16,
    initialized: bool,
}

impl SrtpContext {
    /// Create a new SRTP context from a master key and master salt.
    ///
    /// Derives all six session keys (RTP cipher/auth/salt + RTCP cipher/auth/salt).
    ///
    /// - `profile`: SRTP profile (currently only `AES_128_CM_HMAC_SHA1_80`).
    /// - `master_key`: 16-byte master key.
    /// - `master_salt`: 14-byte master salt.
    /// - `send_ssrc`: SSRC of the send direction (for constructing the IV).
    pub fn new(_profile: SrtpProfile, master_key: &[u8; 16], master_salt: &[u8; 14]) -> Self {
        let keys = key_derive::derive_session_keys(master_key, master_salt);
        Self::from_session_keys(&keys)
    }

    /// Create an SRTP context from already-derived session keys.
    pub fn from_session_keys(keys: &SessionKeys) -> Self {
        Self {
            send_cipher: cipher::Aes128Cm::new(&keys.rtp_cipher_key, &keys.rtp_salt),
            send_auth: auth::HmacSha1_80::new(&keys.rtp_auth_key),
            send_state: HashMap::new(),
            receive: HashMap::new(),
            cipher_key: keys.rtp_cipher_key,
            auth_key: keys.rtp_auth_key,
            salt: keys.rtp_salt,
            rtcp_cipher_key: keys.rtcp_cipher_key,
            rtcp_auth_key: keys.rtcp_auth_key,
            rtcp_salt: keys.rtcp_salt,
        }
    }

    /// Protect an RTP packet: encrypt the payload and append the authentication tag.
    ///
    /// Input: raw RTP packet bytes.
    /// Output: SRTP packet = encrypted RTP + auth tag (10 bytes).
    ///
    /// The first 12 bytes (RTP fixed header + CSRC) are authenticated but not encrypted.
    /// The header extension (if present) and payload are encrypted.
    pub fn protect_rtp(&mut self, packet: &[u8]) -> Result<Bytes> {
        if packet.len() < 12 {
            return Err(Error::Media("RTP packet too short for SRTP".into()));
        }

        // Parse minimal header fields to find the encryption boundary.
        let cc = (packet[0] & 0x0F) as usize;
        let header_end = 12 + cc * 4;

        if packet.len() < header_end {
            return Err(Error::Media("truncated RTP CSRC list".into()));
        }

        // Check for header extension.
        let ext_end = if packet[0] & 0x10 != 0 {
            if packet.len() < header_end + 4 {
                return Err(Error::Media("truncated RTP extension header".into()));
            }
            let ext_len =
                u16::from_be_bytes([packet[header_end + 2], packet[header_end + 3]]) as usize;
            let end = header_end
                .checked_add(4)
                .and_then(|value| value.checked_add(ext_len.checked_mul(4)?))
                .ok_or_else(|| Error::Media("RTP extension length overflow".into()))?;
            if end > packet.len() {
                return Err(Error::Media("truncated RTP extension data".into()));
            }
            end
        } else {
            header_end
        };

        // Handle padding.
        let payload_end = if packet[0] & 0x20 != 0 {
            if ext_end >= packet.len() {
                return Err(Error::Media("invalid RTP padding".into()));
            }
            let pad_len = *packet.last().unwrap() as usize;
            if pad_len == 0 || pad_len > packet.len() - ext_end {
                return Err(Error::Media("invalid RTP padding".into()));
            }
            packet.len() - pad_len
        } else {
            packet.len()
        };
        if payload_end < ext_end {
            return Err(Error::Media("invalid RTP extension/padding".into()));
        }

        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let state = self.send_state.entry(ssrc).or_insert(SrtpSendState {
            roc: 0,
            last_seq: seq,
            highest_index: 0,
            initialized: false,
        });
        if state.initialized {
            if seq < state.last_seq && state.last_seq - seq > 0x8000 {
                state.roc = state.roc.wrapping_add(1);
            } else if seq > state.last_seq && seq - state.last_seq > 0x8000 {
                state.roc = state.roc.saturating_sub(1);
            }
        }
        let index = (u64::from(state.roc) << 16) | u64::from(seq);
        if state.initialized && index <= state.highest_index {
            return Err(Error::Media("SRTP send index reordered or replayed".into()));
        }
        state.last_seq = seq;
        state.highest_index = index;
        state.initialized = true;

        // Build output: header + encrypted extension/payload + ROC + auth tag.
        let mut output = BytesMut::with_capacity(packet.len() + 4 + 10);

        // Authenticated header (first `ext_end` bytes) — copied verbatim.
        output.extend_from_slice(&packet[..ext_end]);

        // Encrypt extension + payload.
        if ext_end < payload_end {
            let plain = &packet[ext_end..payload_end];
            let mut encrypted = plain.to_vec();
            self.send_cipher.apply(ssrc, index, &mut encrypted);
            output.extend_from_slice(&encrypted);
        }

        // Copy padding if present.
        if payload_end < packet.len() {
            output.extend_from_slice(&packet[payload_end..]);
        }

        // Append ROC (rollover counter) for auth computation.
        let roc = (index >> 16) as u32;
        let mut auth_input = output.to_vec();
        auth_input.extend_from_slice(&roc.to_be_bytes());

        // Compute and append auth tag.
        let tag = self.send_auth.tag(&auth_input);
        output.extend_from_slice(&tag);

        Ok(output.freeze())
    }

    /// Unprotect an SRTP packet: verify auth tag and decrypt the payload.
    ///
    /// Input: SRTP packet (encrypted RTP + auth tag).
    /// Output: original RTP packet bytes.
    pub fn unprotect_rtp(&mut self, packet: &[u8]) -> Result<Bytes> {
        if packet.len() < 12 + 10 {
            return Err(Error::Media("SRTP packet too short".into()));
        }

        // Split off the auth tag (last 10 bytes).
        let (body, tag_bytes) = packet.split_at(packet.len() - 10);
        let mut tag = [0u8; 10];
        tag.copy_from_slice(tag_bytes);

        // Parse SSRC from the header.
        let ssrc = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);

        // Get or create the receiver for this SSRC.
        let cipher_key = self.cipher_key;
        let auth_key = self.auth_key;
        let salt = self.salt;
        let receiver = self.receive.entry(ssrc).or_insert_with(|| SrtpReceiver {
            cipher: cipher::Aes128Cm::new(&cipher_key, &salt),
            auth: auth::HmacSha1_80::new(&auth_key),
            replay: replay::ReplayWindow::new(),
            roc: 0,
            last_seq: 0,
            initialized: false,
        });

        // Infer the rollover counter from the sequence gap, trying the
        // adjacent counters for reordered packets around a wrap boundary.
        let seq = u16::from_be_bytes([body[2], body[3]]);
        let mut roc = receiver.roc;
        if receiver.initialized {
            if seq < receiver.last_seq && receiver.last_seq - seq > 0x8000 {
                roc = roc.wrapping_add(1);
            } else if seq > receiver.last_seq && seq - receiver.last_seq > 0x8000 {
                roc = roc.saturating_sub(1);
            }
        }
        let mut selected = None;
        for candidate in [roc, roc.wrapping_add(1), roc.saturating_sub(1)] {
            let candidate_index = (u64::from(candidate) << 16) | u64::from(seq);
            let mut auth_input = body.to_vec();
            auth_input.extend_from_slice(&candidate.to_be_bytes());
            if receiver.auth.verify(&auth_input, &tag) {
                selected = Some((candidate, candidate_index));
                break;
            }
        }
        let Some((roc, index)) = selected else {
            return Err(Error::Media("SRTP authentication failed".into()));
        };

        // Check replay window.
        if !receiver.replay.check(index) {
            return Err(Error::Media("SRTP replay detected".into()));
        }
        if !receiver.initialized
            || index > ((u64::from(receiver.roc) << 16) | u64::from(receiver.last_seq))
        {
            receiver.roc = roc;
            receiver.last_seq = seq;
            receiver.initialized = true;
        }

        // Parse the encryption boundary (same as protect_rtp).
        let cc = (body[0] & 0x0F) as usize;
        let header_end = 12 + cc * 4;
        if body.len() < header_end {
            return Err(Error::Media("truncated RTP CSRC list".into()));
        }

        let ext_end = if body[0] & 0x10 != 0 {
            if body.len() < header_end + 4 {
                return Err(Error::Media("truncated RTP extension header".into()));
            }
            let ext_len = u16::from_be_bytes([body[header_end + 2], body[header_end + 3]]) as usize;
            let end = header_end
                .checked_add(4)
                .and_then(|value| value.checked_add(ext_len.checked_mul(4)?))
                .ok_or_else(|| Error::Media("RTP extension length overflow".into()))?;
            if end > body.len() {
                return Err(Error::Media("truncated RTP extension data".into()));
            }
            end
        } else {
            header_end
        };

        let payload_end = if body[0] & 0x20 != 0 {
            if ext_end >= body.len() {
                return Err(Error::Media("invalid RTP padding".into()));
            }
            let pad_len = *body.last().unwrap() as usize;
            if pad_len == 0 || pad_len > body.len() - ext_end {
                return Err(Error::Media("invalid RTP padding".into()));
            }
            body.len() - pad_len
        } else {
            body.len()
        };
        if payload_end < ext_end {
            return Err(Error::Media("invalid RTP extension/padding".into()));
        }

        // Decrypt extension + payload.
        let mut output = BytesMut::with_capacity(body.len());
        output.extend_from_slice(&body[..ext_end]);

        if ext_end < payload_end {
            let encrypted = &body[ext_end..payload_end];
            let mut decrypted = encrypted.to_vec();
            receiver.cipher.apply(ssrc, index, &mut decrypted);
            output.extend_from_slice(&decrypted);
        }

        if payload_end < body.len() {
            output.extend_from_slice(&body[payload_end..]);
        }

        Ok(output.freeze())
    }

    /// Create an SRTCP context using the derived RTCP session keys.
    ///
    /// Not implemented: the RTCP session keys are not stored on this
    /// context. Use [`SrtcpContext::new`] directly with the derived keys.
    pub fn srtcp_context(&self) -> crate::Result<SrtcpContext> {
        Ok(SrtcpContext::new(
            &self.rtcp_cipher_key,
            &self.rtcp_auth_key,
            &self.rtcp_salt,
        ))
    }
}

/// Build SRTP and SRTCP contexts from an SDP `a=crypto:` attribute.
///
/// Supports the RFC 3711 default suite `AES_128_CM_HMAC_SHA1_80` with an
/// `inline:` master key (16-byte key + 14-byte salt, base64).
pub fn from_sdp_crypto(
    crypto: &rtflow_media::crypto::SdpCrypto,
) -> crate::Result<(SrtpContext, SrtcpContext)> {
    use base64::Engine as _;
    if !crypto
        .crypto_suite
        .eq_ignore_ascii_case("AES_128_CM_HMAC_SHA1_80")
    {
        return Err(rtflow_core::error::Error::Unsupported(format!(
            "unsupported SRTP suite {}",
            crypto.crypto_suite
        )));
    }
    let encoded = crypto
        .key_params
        .strip_prefix("inline:")
        .ok_or_else(|| rtflow_core::error::Error::Unsupported("SRTP key is not inline".into()))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| rtflow_core::error::Error::Media("invalid SRTP inline key".into()))?;
    if decoded.len() < 30 {
        return Err(rtflow_core::error::Error::Media("SRTP master key is too short".into()));
    }
    let mut master_key = [0u8; 16];
    master_key.copy_from_slice(&decoded[..16]);
    let mut master_salt = [0u8; 14];
    master_salt.copy_from_slice(&decoded[16..30]);
    let keys = key_derive::derive_session_keys(&master_key, &master_salt);
    Ok((
        SrtpContext::from_session_keys(&keys),
        SrtcpContext::new(&keys.rtcp_cipher_key, &keys.rtcp_auth_key, &keys.rtcp_salt),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_crypto_builds_contexts() {
        use rtflow_media::crypto::SdpCrypto;
        use base64::Engine as _;
        let crypto = SdpCrypto {
            tag: 1,
            crypto_suite: "AES_128_CM_HMAC_SHA1_80".into(),
            key_params: format!(
                "inline:{}",
                base64::engine::general_purpose::STANDARD.encode([3u8; 30])
            ),
            session_params: None,
        };
        let (mut srtp, _srtcp) = from_sdp_crypto(&crypto).unwrap();
        let raw = Bytes::from_static(&[
            0x80, 0x60, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb,
        ]);
        let protected = srtp.protect_rtp(&raw).unwrap();
        assert!(protected.len() > raw.len());
        let plain = srtp.unprotect_rtp(&protected).unwrap();
        assert_eq!(plain, raw);
    }

    #[test]
    fn protect_unprotect_roundtrip() {
        let master_key: [u8; 16] = [0xAB; 16];
        let master_salt: [u8; 14] = [0xCD; 14];
        let ssrc: u32 = 0x12345678;

        let mut sender = SrtpContext::new(
            SrtpProfile::AES_128_CM_HMAC_SHA1_80,
            &master_key,
            &master_salt,
        );
        let mut receiver = SrtpContext::new(
            SrtpProfile::AES_128_CM_HMAC_SHA1_80,
            &master_key,
            &master_salt,
        );

        // Build a minimal RTP packet: V=2, PT=96, seq=1, ts=1000, ssrc.
        let mut rtp = Vec::new();
        rtp.extend_from_slice(&[0x80, 0x60]); // V=2, PT=96
        rtp.extend_from_slice(&1u16.to_be_bytes()); // sequence number
        rtp.extend_from_slice(&1000u32.to_be_bytes()); // timestamp
        rtp.extend_from_slice(&ssrc.to_be_bytes()); // SSRC
        rtp.extend_from_slice(&[0x65, 0x00, 0x01, 0x02, 0x03, 0x04]); // payload

        let protected = sender.protect_rtp(&rtp).unwrap();
        // Protected = 18 (RTP) + 10 (tag) = 28 bytes.
        assert_eq!(protected.len(), 28);

        let unprotected = receiver.unprotect_rtp(&protected).unwrap();
        assert_eq!(&unprotected[..], &rtp[..]);
    }

    #[test]
    fn rejects_tampered_packet() {
        let master_key: [u8; 16] = [0xAB; 16];
        let master_salt: [u8; 14] = [0xCD; 14];
        let ssrc: u32 = 0x12345678;

        let mut sender = SrtpContext::new(
            SrtpProfile::AES_128_CM_HMAC_SHA1_80,
            &master_key,
            &master_salt,
        );
        let mut receiver = SrtpContext::new(
            SrtpProfile::AES_128_CM_HMAC_SHA1_80,
            &master_key,
            &master_salt,
        );

        let mut rtp = Vec::new();
        rtp.extend_from_slice(&[0x80, 0x60]);
        rtp.extend_from_slice(&1u16.to_be_bytes());
        rtp.extend_from_slice(&1000u32.to_be_bytes());
        rtp.extend_from_slice(&ssrc.to_be_bytes());
        rtp.extend_from_slice(&[0x65, 0x00, 0x01, 0x02]);

        let mut protected = sender.protect_rtp(&rtp).unwrap().to_vec();
        // Tamper with encrypted payload.
        protected[14] ^= 0xFF;

        assert!(receiver.unprotect_rtp(&protected).is_err());
    }

    #[test]
    fn rejects_short_packet() {
        let master_key: [u8; 16] = [0xAB; 16];
        let master_salt: [u8; 14] = [0xCD; 14];
        let mut ctx = SrtpContext::new(
            SrtpProfile::AES_128_CM_HMAC_SHA1_80,
            &master_key,
            &master_salt,
        );
        assert!(ctx.protect_rtp(&[0u8; 5]).is_err());
        assert!(ctx.unprotect_rtp(&[0u8; 5]).is_err());
    }
    #[test]
    fn srtp_roc_rollover_keeps_sender_and_receiver_in_sync() {
        let mut sender = SrtpContext::new(
            SrtpProfile::AES_128_CM_HMAC_SHA1_80,
            &[0x11; 16],
            &[0x22; 14],
        );
        let mut receiver = SrtpContext::new(
            SrtpProfile::AES_128_CM_HMAC_SHA1_80,
            &[0x11; 16],
            &[0x22; 14],
        );
        for sequence in 0..=65_536u32 {
            let seq = sequence as u16;
            let mut packet = vec![0x80, 96, 0, 0, 0, 0, 0, 0, 0x44, 0x33, 0x22, 0x11, 0xAB];
            packet[2..4].copy_from_slice(&seq.to_be_bytes());
            packet[4..8].copy_from_slice(&sequence.to_be_bytes());
            let protected = sender.protect_rtp(&packet).unwrap();
            let plain = receiver.unprotect_rtp(&protected).unwrap();
            assert_eq!(plain.last(), Some(&0xAB), "packet {sequence}");
        }
    }
    #[test]
    fn malformed_extension_and_padding_do_not_panic() {
        let mut context = SrtpContext::new(
            SrtpProfile::AES_128_CM_HMAC_SHA1_80,
            &[0x11; 16],
            &[0x22; 14],
        );
        // Extension header claims 0xffff words beyond the packet.
        let packet = vec![
            0x90, 96, 0, 1, 0, 0, 0, 0, 0x44, 0x33, 0x22, 0x11, 0, 0, 0xff, 0xff,
        ];
        assert!(context.protect_rtp(&packet).is_err());
        // Padding length larger than the packet.
        let packet = vec![0xa0, 96, 0, 1, 0, 0, 0, 0, 0x44, 0x33, 0x22, 0x11, 0x7f];
        assert!(context.protect_rtp(&packet).is_err());
    }
}

#[cfg(test)]
mod rtx_stream_tests {
    use super::*;

    fn rtp(ssrc: u32, sequence: u16, payload: &[u8]) -> Bytes {
        let mut packet = BytesMut::with_capacity(12 + payload.len());
        packet.extend_from_slice(&[0x80, 0x60]); // V=2, PT=96
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&9_000u32.to_be_bytes());
        packet.extend_from_slice(&ssrc.to_be_bytes());
        packet.extend_from_slice(payload);
        packet.freeze()
    }

    #[test]
    fn one_context_protects_media_and_rtx_ssrc() {
        let key = [0x11; 16];
        let salt = [0x22; 14];
        let mut sender = SrtpContext::new(SrtpProfile::AES_128_CM_HMAC_SHA1_80, &key, &salt);
        let mut receiver = SrtpContext::new(SrtpProfile::AES_128_CM_HMAC_SHA1_80, &key, &salt);
        let media = rtp(0x1111_1111, 10, b"media");
        let rtx = rtp(0x2222_2222, 1, b"\x00\x0artx");
        let protected_media = sender.protect_rtp(&media).unwrap();
        let protected_rtx = sender.protect_rtp(&rtx).unwrap();
        assert_eq!(receiver.unprotect_rtp(&protected_media).unwrap(), media);
        assert_eq!(receiver.unprotect_rtp(&protected_rtx).unwrap(), rtx);
    }
}
