//! Rustls configuration for RTSPS control connections.
use rtflow_core::error::{Error, Result};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use rustls_pemfile::{certs, private_key};
use std::{io::Cursor, sync::Arc, time::Duration};

#[derive(Clone)]
pub struct TlsServerConfig {
    pub config: Arc<ServerConfig>,
    pub handshake_timeout: Duration,
}

impl TlsServerConfig {
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self {
            config,
            handshake_timeout: Duration::from_secs(5),
        }
    }

    pub fn from_pem(certificate_chain: &[u8], private_key_pem: &[u8]) -> Result<Self> {
        let certificates = certs(&mut Cursor::new(certificate_chain))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| Error::Tls(error.to_string()))?;
        let key = private_key(&mut Cursor::new(private_key_pem))
            .map_err(|error| Error::Tls(error.to_string()))?
            .ok_or_else(|| Error::Tls("PEM contains no private key".into()))?;
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .map_err(|error| Error::Tls(error.to_string()))?;
        Ok(Self::new(Arc::new(config)))
    }

    /// Build a server config that requires client certificates (mTLS):
    /// clients must present a certificate signed by one of `ca_certs_pem`.
    pub fn from_pem_with_client_auth(
        certificate_chain: &[u8],
        private_key_pem: &[u8],
        ca_certs_pem: &[u8],
    ) -> Result<Self> {
        use rustls::server::WebPkiClientVerifier;
        let certificates = certs(&mut Cursor::new(certificate_chain))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| Error::Tls(error.to_string()))?;
        let key = private_key(&mut Cursor::new(private_key_pem))
            .map_err(|error| Error::Tls(error.to_string()))?
            .ok_or_else(|| Error::Tls("PEM contains no private key".into()))?;
        let ca_certificates = certs(&mut Cursor::new(ca_certs_pem))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| Error::Tls(error.to_string()))?;
        let mut roots = RootCertStore::empty();
        let (accepted, rejected) = roots.add_parsable_certificates(ca_certificates);
        if accepted == 0 || rejected != 0 {
            return Err(Error::Tls("no valid client CA certificates in PEM".into()));
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|error| Error::Tls(error.to_string()))?;
        let config = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certificates, key)
            .map_err(|error| Error::Tls(error.to_string()))?;
        Ok(Self::new(Arc::new(config)))
    }
}

#[derive(Clone)]
pub struct TlsClientConfig {
    pub config: Arc<ClientConfig>,
    pub server_name: Option<String>,
    pub handshake_timeout: Duration,
}

impl TlsClientConfig {
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self {
            config,
            server_name: None,
            handshake_timeout: Duration::from_secs(5),
        }
    }

    pub fn with_server_name(mut self, server_name: impl Into<String>) -> Self {
        self.server_name = Some(server_name.into());
        self
    }

    pub fn with_root_certificates(certificates_pem: &[u8]) -> Result<Self> {
        let certificates = certs(&mut Cursor::new(certificates_pem))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| Error::Tls(error.to_string()))?;
        let mut roots = RootCertStore::empty();
        let (accepted, rejected) = roots.add_parsable_certificates(certificates);
        if accepted == 0 || rejected != 0 {
            return Err(Error::Tls("no valid root certificates in PEM".into()));
        }
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self::new(Arc::new(config)))
    }
}

impl std::fmt::Debug for TlsServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TlsServerConfig")
            .field("handshake_timeout", &self.handshake_timeout)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for TlsClientConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TlsClientConfig")
            .field("server_name", &self.server_name)
            .field("handshake_timeout", &self.handshake_timeout)
            .finish_non_exhaustive()
    }
}
