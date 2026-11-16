//! AES-256-GCM envelope for recoverable secrets (proxy passwords, SRT
//! passphrases). Stream keys are hashes and never go through this module.
use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use zeroize::Zeroizing;

pub(crate) struct SecretBox {
    key: Zeroizing<[u8; 32]>,
}

impl SecretBox {
    pub(crate) fn new(key: [u8; 32]) -> Self {
        Self {
            key: Zeroizing::new(key),
        }
    }

    pub(crate) fn seal(&self, aad: &str, plaintext: &[u8]) -> Vec<u8> {
        let cipher = Aes256Gcm::new_from_slice(self.key.as_ref()).expect("32-byte AES key");
        let mut nonce = [0u8; 12];
        let _ = getrandom::getrandom(&mut nonce);
        let ciphertext = cipher
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: plaintext,
                    aad: aad.as_bytes(),
                },
            )
            .expect("AES-GCM encryption");
        let mut output = Vec::with_capacity(1 + nonce.len() + ciphertext.len());
        output.push(1);
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&ciphertext);
        output
    }

    pub(crate) fn open(&self, aad: &str, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.len() < 1 + 12 || payload[0] != 1 {
            return None;
        }
        let nonce = &payload[1..13];
        let ciphertext = &payload[13..];
        let cipher = Aes256Gcm::new_from_slice(self.key.as_ref()).ok()?;
        cipher
            .decrypt(
                nonce.into(),
                Payload {
                    msg: ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_aad_binding() {
        let secret = SecretBox::new([9u8; 32]);
        let sealed = secret.seal("proxy:/cam:upstreams", b"password");
        assert_eq!(
            secret.open("proxy:/cam:upstreams", &sealed).as_deref(),
            Some(b"password".as_slice())
        );
        assert!(secret.open("proxy:/other:upstreams", &sealed).is_none());
    }
}
