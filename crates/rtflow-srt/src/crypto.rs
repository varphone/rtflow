//! HaiCrypt AES-CTR encryption for SRT (libsrt 1.4 compatible).
//!
//! Scope: listener (responder) side of the HSv5 caller-listener handshake.
//! The caller generates one session key (SEK), wraps it with a KEK derived
//! from the passphrase, and sends it as a KMREQ handshake extension. The
//! listener unwraps it and echoes the exact bytes as KMRSP; data packets
//! carrying the KK flag are then decrypted with AES-CTR.
//!
//! Reference: libsrt 1.4.4 `haicrypt/` (see docs/spec/encryption.md).
use aes::{
    Aes128, Aes192, Aes256,
    cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit, array::Array},
};
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// PBKDF2 iterations used by HaiCrypt.
const PBKDF2_ITERATIONS: u32 = 2048;
/// Number of salt bytes fed to PBKDF2 (the last eight of the 16-byte salt).
const PBKDF2_SALT_LEN: usize = 8;
/// KM fixed header length.
const KM_HEADER_LEN: usize = 16;
/// Salt length on the wire.
const KM_SALT_LEN: usize = 16;
/// RFC 3394 integrity block length.
const WRAP_ICV_LEN: usize = 8;

/// KM state codes (`SRT_KM_STATE`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KmState {
    Unsecured = 0,
    Securing = 1,
    Secured = 2,
    NoSecret = 3,
    BadSecret = 4,
}

/// Errors from KM processing or data decryption.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("malformed KM message: {0}")]
    BadKm(&'static str),
    #[error("unsupported KM option: {0}")]
    Unsupported(&'static str),
    #[error("key unwrap failed (wrong passphrase)")]
    BadSecret,
    #[error("invalid passphrase length")]
    InvalidPassphrase,
}

/// AES key sizes supported by HaiCrypt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyLength {
    Aes128,
    Aes192,
    Aes256,
}

impl KeyLength {
    pub fn from_bytes(bytes: usize) -> Option<Self> {
        match bytes {
            16 => Some(Self::Aes128),
            24 => Some(Self::Aes192),
            32 => Some(Self::Aes256),
            _ => None,
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            Self::Aes128 => 16,
            Self::Aes192 => 24,
            Self::Aes256 => 32,
        }
    }

    /// Handshake Encryption Field value (PBKEYLEN bits): 2/3/4.
    pub fn encryption_field(self) -> u16 {
        match self {
            Self::Aes128 => 2,
            Self::Aes192 => 3,
            Self::Aes256 => 4,
        }
    }

    /// Decode the handshake Encryption Field.
    pub fn from_encryption_field(value: u16) -> Option<Self> {
        match value {
            2 => Some(Self::Aes128),
            3 => Some(Self::Aes192),
            4 => Some(Self::Aes256),
            _ => None,
        }
    }
}

/// Which key slots a KM message carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KmKeys {
    Even,
    Odd,
    Both,
}

impl KmKeys {
    fn bits(self) -> u8 {
        match self {
            Self::Even => 0b01,
            Self::Odd => 0b10,
            Self::Both => 0b11,
        }
    }

    fn count(self) -> usize {
        match self {
            Self::Both => 2,
            _ => 1,
        }
    }
}

/// Parsed KM message (KMREQ or successful KMRSP echo).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KmMessage {
    pub keys: KmKeys,
    pub key_len: KeyLength,
    pub salt: [u8; KM_SALT_LEN],
    /// RFC 3394 blobs: one per key (even first for `Both`).
    pub wrapped: Vec<Vec<u8>>,
}

impl KmMessage {
    /// Parse and validate a KM message.
    pub fn parse(buf: &[u8]) -> Result<Self, CryptoError> {
        if buf.len() <= KM_HEADER_LEN {
            return Err(CryptoError::BadKm("not longer than its header"));
        }
        if buf[0] >> 4 != 0x1 {
            return Err(CryptoError::Unsupported("KM version is not 1"));
        }
        if buf[0] & 0x0F != 0x2 {
            return Err(CryptoError::BadKm("payload type is not KM"));
        }
        if buf[1..3] != [0x20, 0x29] {
            return Err(CryptoError::BadKm("bad HaiCrypt signature"));
        }
        if buf[10] != 2 {
            return Err(CryptoError::Unsupported(
                "stream encapsulation is not TSSRT",
            ));
        }
        let keys = match buf[3] & 0x03 {
            0b01 => KmKeys::Even,
            0b10 => KmKeys::Odd,
            0b11 => KmKeys::Both,
            _ => return Err(CryptoError::BadKm("KM carries no key")),
        };
        let salt_len = usize::from(buf[14]) * 4;
        if salt_len != KM_SALT_LEN {
            return Err(CryptoError::Unsupported("salt is not 16 bytes"));
        }
        let sek_len = usize::from(buf[15]) * 4;
        let key_len =
            KeyLength::from_bytes(sek_len).ok_or(CryptoError::BadKm("SEK length not 16/24/32"))?;
        let expected =
            KM_HEADER_LEN + salt_len + WRAP_ICV_LEN * keys.count() + sek_len * keys.count();
        if buf.len() != expected {
            return Err(CryptoError::BadKm("KM length does not match its fields"));
        }
        if buf[8] != 2 {
            return Err(CryptoError::Unsupported("cipher is not AES-CTR"));
        }
        if buf[9] != 0 {
            return Err(CryptoError::Unsupported("authentication is not supported"));
        }
        let mut salt = [0u8; KM_SALT_LEN];
        salt.copy_from_slice(&buf[KM_HEADER_LEN..KM_HEADER_LEN + KM_SALT_LEN]);
        let mut wrapped = Vec::with_capacity(keys.count());
        let mut offset = KM_HEADER_LEN + KM_SALT_LEN;
        for _ in 0..keys.count() {
            let end = offset + WRAP_ICV_LEN + sek_len;
            wrapped.push(buf[offset..end].to_vec());
            offset = end;
        }
        Ok(Self {
            keys,
            key_len,
            salt,
            wrapped,
        })
    }

    /// Assemble a KM message carrying `keys` (used by tests and future
    /// sender-side support).
    pub fn build(
        keys: KmKeys,
        key_len: KeyLength,
        salt: [u8; KM_SALT_LEN],
        sek_even: &[u8],
        sek_odd: &[u8],
        kek: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let plaintexts: Vec<&[u8]> = match keys {
            KmKeys::Even => vec![sek_even],
            KmKeys::Odd => vec![sek_odd],
            KmKeys::Both => vec![sek_even, sek_odd],
        };
        let mut buf = Vec::with_capacity(64);
        buf.push(0x12);
        buf.extend_from_slice(&[0x20, 0x29]);
        buf.push(keys.bits());
        buf.extend_from_slice(&[0u8; 4]); // KEKI
        buf.extend_from_slice(&[2, 0, 2, 0]); // cipher, auth, SE, reserved
        buf.extend_from_slice(&[0, 0]); // reserved
        buf.push((KM_SALT_LEN / 4) as u8);
        buf.push((key_len.bytes() / 4) as u8);
        buf.extend_from_slice(&salt);
        for plaintext in plaintexts {
            buf.extend_from_slice(&wrap_key(kek, plaintext)?);
        }
        Ok(buf)
    }
}

/// Derive the KEK from a passphrase and the last eight salt bytes.
pub fn derive_kek(
    passphrase: &[u8],
    salt: &[u8; KM_SALT_LEN],
    key_len: KeyLength,
) -> Result<Vec<u8>, CryptoError> {
    if !(10..=80).contains(&passphrase.len()) {
        return Err(CryptoError::InvalidPassphrase);
    }
    let mut derived = vec![0u8; key_len.bytes()];
    pbkdf2_hmac_sha1(
        passphrase,
        &salt[KM_SALT_LEN - PBKDF2_SALT_LEN..],
        PBKDF2_ITERATIONS,
        &mut derived,
    );
    Ok(derived)
}

fn pbkdf2_hmac_sha1(password: &[u8], salt: &[u8], iterations: u32, output: &mut [u8]) {
    let mut block_index: u32 = 1;
    let mut offset = 0;
    while offset < output.len() {
        let mut mac = HmacSha1::new_from_slice(password).expect("HMAC accepts any key");
        mac.update(salt);
        mac.update(&block_index.to_be_bytes());
        let mut u = mac.finalize().into_bytes();
        let mut t = u;
        for _ in 1..iterations {
            let mut mac = HmacSha1::new_from_slice(password).expect("HMAC key");
            mac.update(&u);
            u = mac.finalize().into_bytes();
            for (a, b) in t.iter_mut().zip(u.iter()) {
                *a ^= *b;
            }
        }
        let take = (output.len() - offset).min(t.len());
        output[offset..offset + take].copy_from_slice(&t[..take]);
        offset += take;
        block_index += 1;
    }
}

enum AesBlock {
    Aes128(Aes128),
    Aes192(Aes192),
    Aes256(Aes256),
}

impl AesBlock {
    fn new(key: &[u8]) -> Result<Self, CryptoError> {
        match key.len() {
            16 => {
                let bytes: [u8; 16] = key.try_into().map_err(|_| CryptoError::BadKm("key"))?;
                Ok(Self::Aes128(Aes128::new(&Array::from(bytes))))
            }
            24 => {
                let bytes: [u8; 24] = key.try_into().map_err(|_| CryptoError::BadKm("key"))?;
                Ok(Self::Aes192(Aes192::new(&Array::from(bytes))))
            }
            32 => {
                let bytes: [u8; 32] = key.try_into().map_err(|_| CryptoError::BadKm("key"))?;
                Ok(Self::Aes256(Aes256::new(&Array::from(bytes))))
            }
            _ => Err(CryptoError::BadKm("AES key length")),
        }
    }

    fn encrypt(&self, block: &mut [u8; 16]) {
        let mut value = Array::from(*block);
        match self {
            Self::Aes128(cipher) => cipher.encrypt_block(&mut value),
            Self::Aes192(cipher) => cipher.encrypt_block(&mut value),
            Self::Aes256(cipher) => cipher.encrypt_block(&mut value),
        }
        block.copy_from_slice(&value);
    }

    fn decrypt(&self, block: &mut [u8; 16]) {
        let mut value = Array::from(*block);
        match self {
            Self::Aes128(cipher) => cipher.decrypt_block(&mut value),
            Self::Aes192(cipher) => cipher.decrypt_block(&mut value),
            Self::Aes256(cipher) => cipher.decrypt_block(&mut value),
        }
        block.copy_from_slice(&value);
    }
}

const WRAP_IV: [u8; 8] = [0xA6; 8];

/// RFC 3394 AES Key Wrap.
pub fn wrap_key(kek: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if !plaintext.len().is_multiple_of(8) {
        return Err(CryptoError::BadKm("key is not a multiple of 8 bytes"));
    }
    let cipher = AesBlock::new(kek)?;
    let n = plaintext.len() / 8;
    let mut a = WRAP_IV;
    let mut r: Vec<[u8; 8]> = plaintext
        .chunks(8)
        .map(|chunk| chunk.try_into().expect("chunk"))
        .collect();
    for j in 0..6u64 {
        for (i, block) in r.iter_mut().enumerate() {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&a);
            b[8..].copy_from_slice(block);
            cipher.encrypt(&mut b);
            let t = n as u64 * j + i as u64 + 1;
            let t_bytes = t.to_be_bytes();
            for index in 0..8 {
                a[index] = b[index] ^ t_bytes[index];
            }
            block.copy_from_slice(&b[8..]);
        }
    }
    let mut out = Vec::with_capacity(8 + plaintext.len());
    out.extend_from_slice(&a);
    for block in r {
        out.extend_from_slice(&block);
    }
    Ok(out)
}

/// RFC 3394 AES Key Unwrap.
pub fn unwrap_key(kek: &[u8], wrapped: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if wrapped.len() < 16 || !wrapped.len().is_multiple_of(8) {
        return Err(CryptoError::BadKm("wrapped key length"));
    }
    let cipher = AesBlock::new(kek)?;
    let n = wrapped.len() / 8 - 1;
    let mut a: [u8; 8] = wrapped[..8].try_into().expect("prefix");
    let mut r: Vec<[u8; 8]> = wrapped[8..]
        .chunks(8)
        .map(|chunk| chunk.try_into().expect("chunk"))
        .collect();
    for j in (0..6u64).rev() {
        for i in (0..n).rev() {
            let t = n as u64 * j + i as u64 + 1;
            let t_bytes = t.to_be_bytes();
            let mut b = [0u8; 16];
            for index in 0..8 {
                b[index] = a[index] ^ t_bytes[index];
            }
            b[8..].copy_from_slice(&r[i]);
            cipher.decrypt(&mut b);
            a.copy_from_slice(&b[..8]);
            r[i].copy_from_slice(&b[8..]);
        }
    }
    if a != WRAP_IV {
        return Err(CryptoError::BadSecret);
    }
    let mut out = Vec::with_capacity(n * 8);
    for block in r {
        out.extend_from_slice(&block);
    }
    Ok(out)
}

/// AES-CTR keystream per HaiCrypt's SRT counter block layout.
struct AesCtr {
    cipher: AesBlock,
}

impl AesCtr {
    fn new(key: &[u8]) -> Result<Self, CryptoError> {
        Ok(Self {
            cipher: AesBlock::new(key)?,
        })
    }

    fn apply(&self, salt: &[u8; KM_SALT_LEN], sequence: u32, data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        // IV: salt[0..10] | salt[10..14] XOR seq | 0x0000 (block counter).
        let mut counter = [0u8; 16];
        counter[..10].copy_from_slice(&salt[..10]);
        let sequence_bytes = sequence.to_be_bytes();
        for index in 0..4 {
            counter[10 + index] = salt[10 + index] ^ sequence_bytes[index];
        }
        let mut offset = 0;
        while offset < data.len() {
            let mut block = counter;
            self.cipher.encrypt(&mut block);
            let take = (data.len() - offset).min(16);
            for index in 0..take {
                data[offset + index] ^= block[index];
            }
            offset += take;
            // Increment the 128-bit counter (big-endian).
            for byte in counter.iter_mut().rev() {
                *byte = byte.wrapping_add(1);
                if *byte != 0 {
                    break;
                }
            }
        }
    }
}

struct KeySlot {
    key: Vec<u8>,
    salt: [u8; KM_SALT_LEN],
    ctr: AesCtr,
}

/// Receiver-side crypto state for one SRT connection.
pub struct SessionCrypto {
    passphrase: Vec<u8>,
    state: KmState,
    even: Option<KeySlot>,
    odd: Option<KeySlot>,
}

impl SessionCrypto {
    /// Create a receiver context for the given passphrase.
    pub fn new(passphrase: &[u8]) -> Result<Self, CryptoError> {
        if !(10..=80).contains(&passphrase.len()) {
            return Err(CryptoError::InvalidPassphrase);
        }
        Ok(Self {
            passphrase: passphrase.to_vec(),
            state: KmState::Securing,
            even: None,
            odd: None,
        })
    }

    pub fn state(&self) -> KmState {
        self.state
    }

    /// Install the keys from a KMREQ. Returns the KMRSP payload: the exact
    /// received bytes on success, or the 1-word little-endian state on
    /// failure (used when encryption is not enforced).
    pub fn process_km(&mut self, message: &[u8]) -> (KmState, Vec<u8>) {
        let parsed = match KmMessage::parse(message) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::warn!(%error, "SRT: invalid KM message");
                let state = match error {
                    CryptoError::BadSecret => KmState::BadSecret,
                    _ => KmState::NoSecret,
                };
                self.state = state;
                return (state, km_state_payload(state));
            }
        };
        let kek = match derive_kek(&self.passphrase, &parsed.salt, parsed.key_len) {
            Ok(kek) => kek,
            Err(error) => {
                tracing::warn!(%error, "SRT: KM key derivation failed");
                self.state = KmState::BadSecret;
                return (self.state, km_state_payload(self.state));
            }
        };
        let mut keys = Vec::with_capacity(parsed.wrapped.len());
        for blob in &parsed.wrapped {
            match unwrap_key(&kek, blob) {
                Ok(key) => keys.push(key),
                Err(error) => {
                    tracing::warn!(%error, "SRT: KM unwrap failed");
                    self.state = KmState::BadSecret;
                    return (self.state, km_state_payload(self.state));
                }
            }
        }
        let install = |key: Vec<u8>| -> Option<KeySlot> {
            let ctr = AesCtr::new(&key).ok()?;
            Some(KeySlot {
                key,
                salt: parsed.salt,
                ctr,
            })
        };
        match parsed.keys {
            KmKeys::Even => {
                self.even = keys.pop().and_then(install);
            }
            KmKeys::Odd => {
                self.odd = keys.pop().and_then(install);
            }
            KmKeys::Both => {
                // Even key first in wire order.
                let mut iter = keys.into_iter();
                self.even = iter.next().and_then(install);
                self.odd = iter.next().and_then(install);
            }
        }
        if self.even.is_none() && self.odd.is_none() {
            self.state = KmState::NoSecret;
            return (self.state, km_state_payload(self.state));
        }
        self.state = KmState::Secured;
        (self.state, message.to_vec())
    }

    /// Decrypt a data payload. Returns `None` when no usable key exists.
    pub fn decrypt(&self, sequence: u32, key_flags: u8, payload: &mut [u8]) -> bool {
        let slot = match key_flags & 0x03 {
            1 => self.even.as_ref(),
            2 | 3 => self.odd.as_ref(),
            _ => None,
        };
        let Some(slot) = slot else {
            return false;
        };
        let _ = &slot.key;
        slot.ctr.apply(&slot.salt, sequence, payload);
        true
    }
}

fn km_state_payload(state: KmState) -> Vec<u8> {
    // libsrt writes this 1-word error as a host-order integer; on the wire
    // that is little-endian for mainstream builds.
    (state as u32).to_le_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSPHRASE: &[u8] = b"correct horse battery staple";

    #[test]
    fn km_roundtrip_and_decrypt() {
        let salt = [7u8; 16];
        let sek = [9u8; 16];
        let mut crypto = SessionCrypto::new(PASSPHRASE).unwrap();
        let kek = derive_kek(PASSPHRASE, &salt, KeyLength::Aes128).unwrap();
        let km = KmMessage::build(KmKeys::Even, KeyLength::Aes128, salt, &sek, &[], &kek).unwrap();
        let (state, response) = crypto.process_km(&km);
        assert_eq!(state, KmState::Secured);
        assert_eq!(response, km);

        // The sender encrypts with the same SEK/salt; the receiver decrypts.
        let sender = AesCtr::new(&sek).unwrap();
        let mut plaintext = b"hello srt encryption".to_vec();
        let mut ciphertext = plaintext.clone();
        sender.apply(&salt, 42, &mut ciphertext);
        assert_ne!(ciphertext, plaintext);
        assert!(crypto.decrypt(42, 1, &mut ciphertext));
        assert_eq!(ciphertext, plaintext);
        let _ = &mut plaintext;
    }

    #[test]
    fn wrong_passphrase_is_rejected() {
        let salt = [1u8; 16];
        let sek = [2u8; 16];
        let kek = derive_kek(PASSPHRASE, &salt, KeyLength::Aes128).unwrap();
        let km = KmMessage::build(KmKeys::Even, KeyLength::Aes128, salt, &sek, &[], &kek).unwrap();
        let mut crypto = SessionCrypto::new(b"wrong passphrase!!").unwrap();
        let (state, response) = crypto.process_km(&km);
        assert_eq!(state, KmState::BadSecret);
        assert_eq!(response, vec![4, 0, 0, 0]);
    }

    #[test]
    fn pbkdf2_matches_known_vectors() {
        let mut out = [0u8; 20];
        pbkdf2_hmac_sha1(b"password", b"salt", 1, &mut out);
        assert_eq!(
            out,
            [
                0x0c, 0x60, 0xc8, 0x0f, 0x96, 0x1f, 0x0e, 0x71, 0xf3, 0xa9, 0xb5, 0x24, 0xaf, 0x60,
                0x12, 0x06, 0x2f, 0xe0, 0x37, 0xa6
            ]
        );
        let mut out = [0u8; 20];
        pbkdf2_hmac_sha1(b"password", b"salt", 4096, &mut out);
        assert_eq!(
            out,
            [
                0x4b, 0x00, 0x79, 0x01, 0xb7, 0x65, 0x48, 0x9a, 0xbe, 0xad, 0x49, 0xd9, 0x26, 0xf7,
                0x21, 0xd0, 0x65, 0xa4, 0x29, 0xc1
            ]
        );
    }

    #[test]
    fn wrap_roundtrip_for_all_key_lengths() {
        for key_len in [KeyLength::Aes128, KeyLength::Aes192, KeyLength::Aes256] {
            let kek = vec![0x33u8; key_len.bytes()];
            let key = vec![0x44u8; key_len.bytes()];
            let wrapped = wrap_key(&kek, &key).unwrap();
            assert_eq!(wrapped.len(), 8 + key_len.bytes());
            assert_eq!(unwrap_key(&kek, &wrapped).unwrap(), key);
        }
    }

    #[test]
    fn wrap_matches_rfc3394_vector() {
        // RFC 3394 §4.1 test vector (128-bit KEK, 128-bit key).
        let kek = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
            0x0E, 0x0F,
        ];
        let key = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ];
        let wrapped = wrap_key(&kek, &key).unwrap();
        assert_eq!(
            wrapped,
            vec![
                0x1F, 0xA6, 0x8B, 0x0A, 0x81, 0x12, 0xB4, 0x47, 0xAE, 0xF3, 0x4B, 0xD8, 0xFB, 0x5A,
                0x7B, 0x82, 0x9D, 0x3E, 0x86, 0x23, 0x71, 0xD2, 0xCF, 0xE5
            ]
        );
        assert_eq!(unwrap_key(&kek, &wrapped).unwrap(), key);
    }
}
