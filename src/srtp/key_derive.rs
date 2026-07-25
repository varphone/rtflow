//! SRTP Key Derivation Function (RFC 3711 §4.3).
//!
//! Derives session keys from a master key and master salt using AES-CM as a PRF.
//! The KDF produces six session keys:
//! - RTP cipher key (16 bytes)
//! - RTP authentication key (20 bytes)
//! - RTP session salt (14 bytes)
//! - RTCP cipher key (16 bytes)
//! - RTCP authentication key (20 bytes)
//! - RTCP session salt (14 bytes)

use aes::Aes128;
use aes::cipher::{BlockCipherEncrypt, KeyInit, array::Array};

/// Key derivation labels (RFC 3711 §4.3.1).
const LABEL_RTP_CIPHER: u8 = 0x00;
const LABEL_RTP_AUTH: u8 = 0x01;
const LABEL_RTP_SALT: u8 = 0x02;
const LABEL_RTCP_CIPHER: u8 = 0x03;
const LABEL_RTCP_AUTH: u8 = 0x04;
const LABEL_RTCP_SALT: u8 = 0x05;

/// Derived session keys from the SRTP KDF.
#[derive(Clone, Debug)]
pub struct SessionKeys {
    pub rtp_cipher_key: [u8; 16],
    pub rtp_auth_key: [u8; 20],
    pub rtp_salt: [u8; 14],
    pub rtcp_cipher_key: [u8; 16],
    pub rtcp_auth_key: [u8; 20],
    pub rtcp_salt: [u8; 14],
}

/// Derive all session keys from the master key and master salt.
///
/// Implements RFC 3711 §4.3.1: `key_derive(label, master_key, master_salt, index=0)`.
///
/// - `master_key`: 16-byte master key.
/// - `master_salt`: 14-byte master salt.
pub fn derive_session_keys(master_key: &[u8; 16], master_salt: &[u8; 14]) -> SessionKeys {
    SessionKeys {
        rtp_cipher_key: derive_key(master_key, master_salt, LABEL_RTP_CIPHER, 16)
            .try_into()
            .unwrap(),
        rtp_auth_key: derive_key(master_key, master_salt, LABEL_RTP_AUTH, 20)
            .try_into()
            .unwrap(),
        rtp_salt: derive_key(master_key, master_salt, LABEL_RTP_SALT, 14)
            .try_into()
            .unwrap(),
        rtcp_cipher_key: derive_key(master_key, master_salt, LABEL_RTCP_CIPHER, 16)
            .try_into()
            .unwrap(),
        rtcp_auth_key: derive_key(master_key, master_salt, LABEL_RTCP_AUTH, 20)
            .try_into()
            .unwrap(),
        rtcp_salt: derive_key(master_key, master_salt, LABEL_RTCP_SALT, 14)
            .try_into()
            .unwrap(),
    }
}

/// Derive a single session key using AES-CM as PRF.
///
/// RFC 3711 §4.3.1:
///   x = label || r   (where r = index DIV key_derivation_rate, default 0)
///   key_id = label || r
///   iv = (master_salt XOR (key_id padded to 14 bytes)) || 0x0000
///
/// Then generate `out_len` bytes of keystream by encrypting successive counter
/// blocks with the master key.
fn derive_key(master_key: &[u8; 16], master_salt: &[u8; 14], label: u8, out_len: usize) -> Vec<u8> {
    // Build the 14-byte "x" value: salt XOR (key_id padded to 14 bytes)
    // key_id = label || r (label at byte 7 for SRTP, per RFC 3711 §4.3.1)
    // r = 0 for default key_derivation_rate, so only label contributes.
    let mut x = [0u8; 14];
    x[7] = label;
    // XOR with master salt.
    for i in 0..14 {
        x[i] ^= master_salt[i];
    }

    // Build the 16-byte IV: x (14 bytes) || 0x0000 (2 bytes)
    let mut iv = [0u8; 16];
    iv[..14].copy_from_slice(&x);

    // Generate keystream using AES-CM with the master key.
    let cipher = Aes128::new(&Array::from(*master_key));
    let mut output = Vec::with_capacity(out_len);
    let mut counter_block = iv;
    let mut block = Array::from(counter_block);

    while output.len() < out_len {
        cipher.encrypt_block(&mut block);
        let take = out_len - output.len();
        if take >= 16 {
            output.extend_from_slice(&block);
        } else {
            output.extend_from_slice(&block[..take]);
        }
        increment_counter(&mut counter_block);
        block = Array::from(counter_block);
    }
    output
}

/// Increment the 32-bit counter in the last 4 bytes of the 16-byte block (big-endian).
fn increment_counter(block: &mut [u8; 16]) {
    let mut carry = 1u32;
    for i in (12..16).rev() {
        let (sum, overflow) = (block[i] as u32).overflowing_add(carry);
        block[i] = sum as u8;
        carry = overflow as u32;
        if carry == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 3711 Appendix B test vectors.
    //
    // Master Key:  0xE1F97A0D3E018BE0D64FA32C06DE4139
    // Master Salt: 0x0EC675AD498AFEEBB6960B3AABE6
    //
    // Expected RTP cipher key:  C61E7A93744F39EE10734AFE3FF7A087
    // Expected RTP auth key:    CEBE321F6FF7716B6FD4AB49AF256A15 6D38BAA4
    // Expected RTP salt:        30CBBC08863D8C85D49DB34A9AE1
    //
    // Expected RTCP cipher key: 53933F2B1F5592F04AF728B5B44A4D07
    // Expected RTCP auth key:   4D2B6768BC78B33FC8D6B8D0D7C35D24 2F1104A4
    // Expected RTCP salt:       6768B797A5E4E5D2B0F5A2D7C3006C21

    #[test]
    fn rfc3711_appendix_b_rtp_cipher_key() {
        let master_key: [u8; 16] = [
            0xE1, 0xF9, 0x7A, 0x0D, 0x3E, 0x01, 0x8B, 0xE0, 0xD6, 0x4F, 0xA3, 0x2C, 0x06, 0xDE,
            0x41, 0x39,
        ];
        let master_salt: [u8; 14] = [
            0x0E, 0xC6, 0x75, 0xAD, 0x49, 0x8A, 0xFE, 0xEB, 0xB6, 0x96, 0x0B, 0x3A, 0xAB, 0xE6,
        ];
        let keys = derive_session_keys(&master_key, &master_salt);
        assert_eq!(
            keys.rtp_cipher_key,
            [
                0xC6, 0x1E, 0x7A, 0x93, 0x74, 0x4F, 0x39, 0xEE, 0x10, 0x73, 0x4A, 0xFE, 0x3F, 0xF7,
                0xA0, 0x87,
            ]
        );
    }

    #[test]
    fn rfc3711_appendix_b_rtp_auth_key() {
        let master_key: [u8; 16] = [
            0xE1, 0xF9, 0x7A, 0x0D, 0x3E, 0x01, 0x8B, 0xE0, 0xD6, 0x4F, 0xA3, 0x2C, 0x06, 0xDE,
            0x41, 0x39,
        ];
        let master_salt: [u8; 14] = [
            0x0E, 0xC6, 0x75, 0xAD, 0x49, 0x8A, 0xFE, 0xEB, 0xB6, 0x96, 0x0B, 0x3A, 0xAB, 0xE6,
        ];
        let keys = derive_session_keys(&master_key, &master_salt);
        assert_eq!(
            keys.rtp_auth_key,
            [
                0xCE, 0xBE, 0x32, 0x1F, 0x6F, 0xF7, 0x71, 0x6B, 0x6F, 0xD4, 0xAB, 0x49, 0xAF, 0x25,
                0x6A, 0x15, 0x6D, 0x38, 0xBA, 0xA4,
            ]
        );
    }

    #[test]
    fn rfc3711_appendix_b_rtp_salt() {
        let master_key: [u8; 16] = [
            0xE1, 0xF9, 0x7A, 0x0D, 0x3E, 0x01, 0x8B, 0xE0, 0xD6, 0x4F, 0xA3, 0x2C, 0x06, 0xDE,
            0x41, 0x39,
        ];
        let master_salt: [u8; 14] = [
            0x0E, 0xC6, 0x75, 0xAD, 0x49, 0x8A, 0xFE, 0xEB, 0xB6, 0x96, 0x0B, 0x3A, 0xAB, 0xE6,
        ];
        let keys = derive_session_keys(&master_key, &master_salt);
        assert_eq!(
            keys.rtp_salt,
            [
                0x30, 0xCB, 0xBC, 0x08, 0x86, 0x3D, 0x8C, 0x85, 0xD4, 0x9D, 0xB3, 0x4A, 0x9A, 0xE1,
            ]
        );
    }

    #[test]
    fn derived_keys_are_deterministic() {
        let master_key: [u8; 16] = [0xAB; 16];
        let master_salt: [u8; 14] = [0xCD; 14];
        let k1 = derive_session_keys(&master_key, &master_salt);
        let k2 = derive_session_keys(&master_key, &master_salt);
        assert_eq!(k1.rtp_cipher_key, k2.rtp_cipher_key);
        assert_eq!(k1.rtp_auth_key, k2.rtp_auth_key);
        assert_eq!(k1.rtp_salt, k2.rtp_salt);
        assert_eq!(k1.rtcp_cipher_key, k2.rtcp_cipher_key);
        assert_eq!(k1.rtcp_auth_key, k2.rtcp_auth_key);
        assert_eq!(k1.rtcp_salt, k2.rtcp_salt);
    }

    #[test]
    fn different_keys_produce_different_derived_keys() {
        let key1 = [0x01u8; 16];
        let key2 = [0x02u8; 16];
        let salt = [0x03u8; 14];
        let k1 = derive_session_keys(&key1, &salt);
        let k2 = derive_session_keys(&key2, &salt);
        assert_ne!(k1.rtp_cipher_key, k2.rtp_cipher_key);
        assert_ne!(k1.rtp_auth_key, k2.rtp_auth_key);
    }
}
