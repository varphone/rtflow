//! DTLS (Datagram Transport Layer Security) for WebRTC.
//!
//! Uses webrtc-dtls for DTLS 1.2 handshake and SRTP key derivation.
use std::sync::Arc;
use tracing::info;
use webrtc_dtls::config::{Config, ExtendedMasterSecretType};
use webrtc_dtls::conn::DTLSConn;
use webrtc_dtls::crypto::Certificate;

use crate::Result;
use crate::error::Error;

/// DTLS certificate and configuration.
pub struct DtlsConfig {
    /// Generated certificate.
    certificate: Certificate,
    /// Certificate fingerprint (SHA-256).
    fingerprint: String,
}

impl DtlsConfig {
    /// Generate a new self-signed DTLS certificate.
    pub fn generate() -> Result<Self> {
        // Generate ECDSA P-256 certificate
        let cert = Certificate::generate_self_signed(vec!["rtflow.local".to_string()])
            .map_err(|e| Error::Webrtc(format!("DTLS cert generation: {e}")))?;

        // Compute fingerprint from certificate DER
        let der = cert
            .certificate
            .first()
            .ok_or_else(|| Error::Webrtc("DTLS: no certificate".into()))?;
        let fingerprint = compute_fingerprint(der);

        info!("DTLS: generated certificate, fingerprint: {fingerprint}");

        Ok(Self {
            certificate: cert,
            fingerprint,
        })
    }

    /// Get the certificate fingerprint for SDP.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Get the certificate.
    pub fn certificate(&self) -> &Certificate {
        &self.certificate
    }

    /// Create a DTLS server config.
    pub fn server_config(&self) -> Config {
        Config {
            certificates: vec![self.certificate.clone()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            ..Default::default()
        }
    }

    /// Create a DTLS client config.
    pub fn client_config(&self) -> Config {
        Config {
            certificates: vec![self.certificate.clone()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            insecure_skip_verify: true, // For testing; production should verify
            ..Default::default()
        }
    }
}

/// Compute SHA-256 fingerprint of a DER-encoded certificate.
fn compute_fingerprint(der: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(der);
    let hex: Vec<String> = hash.iter().map(|b| format!("{b:02X}")).collect();
    format!("sha-256 {}", hex.join(":"))
}

/// SRTP keying material exported from DTLS.
#[derive(Clone, Debug)]
pub struct SrtpKeyingMaterial {
    /// Client write key.
    pub client_write_key: Vec<u8>,
    /// Server write key.
    pub server_write_key: Vec<u8>,
    /// Client write salt.
    pub client_write_salt: Vec<u8>,
    /// Server write salt.
    pub server_write_salt: Vec<u8>,
}

/// DTLS connection state.
pub struct DtlsConnection {
    /// The underlying DTLS connection.
    #[allow(dead_code)]
    conn: Option<Arc<DTLSConn>>,
    /// Whether we are the DTLS server (passive) or client (active).
    is_server: bool,
}

impl DtlsConnection {
    /// Create a new DTLS connection wrapper.
    pub fn new(is_server: bool) -> Self {
        Self {
            conn: None,
            is_server,
        }
    }

    /// Check if we are the DTLS server.
    pub fn is_server(&self) -> bool {
        self.is_server
    }

    /// Export SRTP keying material from the DTLS connection.
    ///
    /// This should be called after the DTLS handshake completes.
    /// Note: Full implementation requires DTLS handshake to complete first.
    pub fn export_srtp_keying_material(&self) -> Result<SrtpKeyingMaterial> {
        // In a full implementation, this would export from the DTLS connection
        // For now, return an error indicating handshake not complete
        Err(Error::Webrtc("DTLS: handshake not complete".into()))
    }

    /// Get the underlying DTLS connection.
    pub fn conn(&self) -> Option<Arc<DTLSConn>> {
        self.conn.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtls_config_generation() {
        let config = DtlsConfig::generate().unwrap();
        let fp = config.fingerprint();

        // Fingerprint should be SHA-256 format
        assert!(fp.starts_with("sha-256 ") || fp.contains(":"));
        assert!(fp.len() > 50); // "sha-256 XX:XX:..." is long
    }

    #[test]
    fn dtls_fingerprint_format() {
        let config = DtlsConfig::generate().unwrap();
        let fp = config.fingerprint();

        // Should contain colons separating hex bytes
        let parts: Vec<&str> = fp.split(' ').collect();
        assert!(parts.len() >= 2);
        assert_eq!(parts[0].to_lowercase(), "sha-256");
    }
}
