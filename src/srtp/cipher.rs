//! AES-128 Counter Mode (AES-128-CM) cipher for SRTP (RFC 3711 §4.1).
//!
//! AES-CM generates a keystream by encrypting successive counter blocks.
//! The counter block is formed from the IV = (SSRC XOR salt) || (index << 16).

use aes::Aes128;
use aes::cipher::{BlockCipherEncrypt, KeyInit, array::Array};

/// AES-128-CM cipher state.
///
/// Holds the AES-128 block cipher initialized with the session cipher key.
/// The IV is constructed per-packet from the SSRC and packet index.
pub struct Aes128Cm {
    cipher: Aes128,
    /// Session salt (14 bytes), used to construct the IV.
    salt: [u8; 14],
}

impl Aes128Cm {
    /// Create a new AES-128-CM cipher with the given key and salt.
    ///
    /// - `key`: 16-byte cipher key from the KDF.
    /// - `salt`: 14-byte session salt from the KDF.
    pub fn new(key: &[u8; 16], salt: &[u8; 14]) -> Self {
        let cipher = Aes128::new(&Array::from(*key));
        Self {
            cipher,
            salt: *salt,
        }
    }

    /// Build the 16-byte IV for a given SSRC and packet index.
    ///
    /// IV layout (16 bytes):
    /// ```text
    ///  0                   1                   2                   3
    ///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
    /// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    /// |                     0x00 (4 bytes)                            |
    /// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    /// |                     SSRC XOR salt[2..6]                       |
    /// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    /// |                  (index XOR salt[6..12]) >> 16                |
    /// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    /// |              index XOR salt[8..14] (lower 48 bits)           |
    /// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    /// ```
    ///
    /// Simplified: IV = salt XOR (0 || SSRC || index), padded to 16 bytes.
    fn build_iv(&self, ssrc: u32, index: u64) -> [u8; 16] {
        let mut iv = [0u8; 16];
        // Place SSRC at bytes 4..8
        iv[4..8].copy_from_slice(&ssrc.to_be_bytes());
        // Place index at bytes 8..14 (48-bit, right-aligned in 8 bytes)
        let index_bytes = index.to_be_bytes();
        iv[8..16].copy_from_slice(&index_bytes);
        // XOR with salt (salt occupies bytes 2..16 of the IV)
        for i in 0..14 {
            iv[i + 2] ^= self.salt[i];
        }
        iv
    }

    /// Generate keystream bytes by encrypting successive counter blocks.
    ///
    /// Returns a `Vec<u8>` of `len` keystream bytes.
    pub fn keystream(&self, ssrc: u32, index: u64, len: usize) -> Vec<u8> {
        let iv = self.build_iv(ssrc, index);
        let mut output = Vec::with_capacity(len);
        let mut counter_block = iv;
        let mut block = Array::from(counter_block);

        while output.len() < len {
            self.cipher.encrypt_block(&mut block);
            let take = len - output.len();
            if take >= 16 {
                output.extend_from_slice(&block);
            } else {
                output.extend_from_slice(&block[..take]);
            }
            // Increment counter in the last 4 bytes (big-endian).
            increment_counter(&mut counter_block);
            block = Array::from(counter_block);
        }
        output
    }

    /// Encrypt or decrypt data in-place by XORing with the keystream.
    ///
    /// AES-CM is symmetric: encrypt and decrypt are the same operation.
    pub fn apply(&self, ssrc: u32, index: u64, data: &mut [u8]) {
        let keystream = self.keystream(ssrc, index, data.len());
        for (d, k) in data.iter_mut().zip(keystream.iter()) {
            *d ^= k;
        }
    }

    /// Encrypt or decrypt data and return a new `Vec<u8>`.
    pub fn apply_to_vec(&self, ssrc: u32, index: u64, data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        self.apply(ssrc, index, &mut out);
        out
    }
}

/// Increment the 32-bit counter in the last 4 bytes of the 16-byte block (big-endian).
fn increment_counter(block: &mut [u8; 16]) {
    for i in (12..16).rev() {
        block[i] = block[i].wrapping_add(1);
        if block[i] != 0 {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystream_length_matches() {
        let key = [0x01u8; 16];
        let salt = [0x02u8; 14];
        let cm = Aes128Cm::new(&key, &salt);
        let ks = cm.keystream(0x12345678, 0, 100);
        assert_eq!(ks.len(), 100);
    }

    #[test]
    fn apply_is_symmetric() {
        let key = [0xABu8; 16];
        let salt = [0xCDu8; 14];
        let cm = Aes128Cm::new(&key, &salt);
        let original = b"Hello, SRTP world! This is a test payload.";
        let mut data = original.to_vec();
        cm.apply(0xDEADBEEF, 42, &mut data);
        assert_ne!(&data[..], &original[..]);
        cm.apply(0xDEADBEEF, 42, &mut data);
        assert_eq!(&data[..], &original[..]);
    }

    #[test]
    fn different_indices_produce_different_keystream() {
        let key = [0x55u8; 16];
        let salt = [0x66u8; 14];
        let cm = Aes128Cm::new(&key, &salt);
        let ks0 = cm.keystream(1, 0, 32);
        let ks1 = cm.keystream(1, 1, 32);
        assert_ne!(ks0, ks1);
    }

    #[test]
    fn increment_counter_wraps() {
        let mut block = [0u8; 16];
        block[15] = 0xFF;
        increment_counter(&mut block);
        assert_eq!(block[15], 0);
        assert_eq!(block[14], 1);
    }

    #[test]
    fn build_iv_deterministic() {
        let key = [0u8; 16];
        let salt = [0u8; 14];
        let cm = Aes128Cm::new(&key, &salt);
        let iv1 = cm.build_iv(0x11223344, 0x5566778899AA);
        let iv2 = cm.build_iv(0x11223344, 0x5566778899AA);
        assert_eq!(iv1, iv2);
    }
}
