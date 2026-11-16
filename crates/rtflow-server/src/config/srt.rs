//! SRT listener, pull and push configuration.
use super::*;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SrtSection {
    pub enabled: bool,
    pub listen: SocketAddr,
    /// Receiver TSBPD latency; the connection uses max(this, peer value).
    pub latency_ms: u32,
    #[serde(with = "humantime_serde")]
    pub peer_idle_timeout: Duration,
    /// Drop packets that missed their play time.
    pub tlpktdrop: bool,
    pub mtu: u32,
    pub max_connections: usize,
    /// HaiCrypt passphrase (10..80 bytes); empty disables SRT encryption.
    pub passphrase: Option<String>,
    /// Pull sources: connect as SRT caller and publish into mounts.
    pub pull: Vec<SrtPull>,
    /// Push destinations: mux a mount to MPEG-TS and send as SRT caller.
    pub push: Vec<SrtPush>,
    /// AES key length in bytes: 0 (auto → 16), 16, 24 or 32.
    pub pbkeylen: u32,
    /// Reject peers whose encryption settings differ (libsrt default).
    pub enforced_encryption: bool,
    /// Optional SRT stream-id allowlist. Empty keeps SRT open; use signed
    /// keys/tokens at the application layer where possible.
    pub allowed_stream_ids: Vec<String>,
}

impl Default for SrtSection {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "0.0.0.0:9000".parse().expect("valid default"),
            latency_ms: 120,
            peer_idle_timeout: Duration::from_secs(30),
            tlpktdrop: true,
            mtu: 1500,
            max_connections: 10,
            passphrase: None,
            pull: Vec::new(),
            push: Vec::new(),
            pbkeylen: 0,
            enforced_encryption: true,
            allowed_stream_ids: Vec::new(),
        }
    }
}

/// One SRT pull (caller) source.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SrtPull {
    /// Remote listener, `host:port`.
    pub remote: String,
    /// Stream ID sent in the handshake.
    pub stream_id: Option<String>,
    /// Mount path override; defaults to the Stream ID resource.
    pub mount: Option<String>,
    /// Passphrase override; falls back to `[srt].passphrase`.
    pub passphrase: Option<String>,
    /// AES key length (0 → `[srt].pbkeylen`).
    pub pbkeylen: u32,
    /// Receiver latency override.
    pub latency_ms: Option<u32>,
}

/// One SRT push destination (server as caller, sending MPEG-TS).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SrtPush {
    /// Source mount, e.g. `/live/cam`.
    pub mount: String,
    /// Remote listener, `host:port`.
    pub remote: String,
    /// Stream ID sent in the handshake; defaults to the mount path without `/`.
    pub stream_id: Option<String>,
    /// Passphrase override; falls back to `[srt].passphrase`.
    pub passphrase: Option<String>,
    /// AES key length (0 → `[srt].pbkeylen`).
    pub pbkeylen: u32,
    /// Receiver latency hint.
    pub latency_ms: Option<u32>,
}
