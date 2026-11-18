//! PeerConnection transport factory for WHIP/WHEP sessions.
//!
//! Wraps the `webrtc` crate (0.20) peer-connection API: builds a
//! [`PeerConnection`] with the default media engine (H.264/VP8/VP9 + Opus),
//! collects local ICE candidates through the event handler, and signals
//! connection/gathering state transitions to the session logic.

use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

use rtc::rtp_transceiver::rtp_sender::RTCRtpSendParameters;
use tokio::sync::{Notify, RwLock};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionEventHandler, RTCIceConnectionState, RTCIceGatheringState,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};

use rtflow_core::error::Result;

/// Timeout for ICE gathering before the answer is assembled. Host
/// candidates on a LAN gather in milliseconds; the timeout only guards
/// against a stuck agent.
pub(crate) const GATHER_TIMEOUT: Duration = Duration::from_secs(3);

/// State snapshot shared between the PC event handler and the session.
#[derive(Default)]
pub struct PcState {
    /// Local ICE candidates in SDP attribute form (`candidate:...`),
    /// with their `a=mid` from the candidate event.
    pub candidates: RwLock<Vec<String>>,
    /// Signalled when ICE gathering completes (or the timeout elapses).
    pub gather_done: Notify,
    /// Signalled when ICE connection reaches the connected state.
    pub connected: Notify,
    /// Signalled when the peer connection closes (or fails).
    pub closed: Notify,
    /// Latest ICE connection state.
    pub ice_state: RwLock<RTCIceConnectionState>,
    /// Maximum candidates kept for the SDP answer (memory bound).
    pub max_candidates: usize,
}

impl PcState {
    pub fn new() -> Arc<Self> {
        Self::new_with_max(64)
    }

    pub fn new_with_max(max_candidates: usize) -> Arc<Self> {
        Arc::new(Self {
            max_candidates,
            ..Self::default()
        })
    }
}

/// Event handler wired into every WebRTC peer connection we create.
///
/// Candidates are collected in SDP form so the answer can be assembled
/// without trickle ICE; state transitions notify the waiting session.
pub(crate) struct PcHandler {
    pub state: Arc<PcState>,
}

impl PcHandler {
    pub fn new(state: Arc<PcState>) -> Self {
        Self { state }
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for PcHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        // Convert the candidate to its SDP attribute form and remember
        // which media section it belongs to, so the answer can carry it.
        if let Ok(init) = event.candidate.to_json() {
            let line = format!("a={}", init.candidate);
            let mut candidates = self.state.candidates.write().await;
            if candidates.len() < self.state.max_candidates {
                tracing::debug!(candidate = %init.candidate, "WebRTC: local ICE candidate");
                candidates.push(line);
            }
        }
        // The last candidate is signalled with an empty candidate string.
        if event.candidate.address.is_empty() {
            self.state.gather_done.notify_waiters();
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        tracing::debug!(%state, "WebRTC: ICE gathering state");
        if state == RTCIceGatheringState::Complete {
            self.state.gather_done.notify_waiters();
        }
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        tracing::debug!(%state, "WebRTC: ICE connection state");
        *self.state.ice_state.write().await = state;
        match state {
            RTCIceConnectionState::Connected | RTCIceConnectionState::Completed => {
                self.state.connected.notify_waiters();
            }
            RTCIceConnectionState::Failed
            | RTCIceConnectionState::Disconnected
            | RTCIceConnectionState::Closed => {
                self.state.closed.notify_waiters();
            }
            _ => {}
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        tracing::debug!(%state, "WebRTC: connection state");
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            self.state.closed.notify_waiters();
        }
    }
}

/// Build a peer connection listening on the given UDP addresses.
///
/// `udp_addrs` follows the `PeerConnectionBuilder` convention (e.g.
/// `vec!["0.0.0.0:0"]` for an ephemeral port on all interfaces — the
/// usual choice for LAN/embedded deployments).
pub(crate) async fn build_peer_connection<A>(
    udp_addrs: Vec<A>,
    handler: Arc<PcHandler>,
    ice_servers: Vec<webrtc::peer_connection::RTCIceServer>,
    ice_transport_policy: webrtc::peer_connection::RTCIceTransportPolicy,
) -> Result<Arc<dyn PeerConnection>>
where
    A: ToSocketAddrs + Send + Sync + 'static,
{
    use webrtc::peer_connection::{MediaEngine, PeerConnectionBuilder, RTCConfigurationBuilder};

    // The default media engine registers no codecs; the WHIP/WHEP sessions
    // negotiate H.264/VP8/VP9 video and Opus audio, so register the
    // standard set (profile-level-id 42e01f, packetization-mode 1, etc.).
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs().map_err(|e| {
        rtflow_core::error::Error::Media(format!("WebRTC media engine init failed: {e}"))
    })?;
    // AAC-LC (MPEG4-GENERIC) is not part of the default set but is
    // supported by Chrome/Safari WebRTC (not Firefox) — the common audio
    // codec of IP cameras alongside Opus and G.711 (PCMA/PCMU, which the
    // default set already registers).
    media_engine
        .register_codec(
            rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters {
                payload_type: 97,
                rtp_codec: rtc::rtp_transceiver::rtp_sender::RTCRtpCodec {
                    mime_type: "audio/mp4a.40.2".to_string(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "profile-level-id=1;mode=AAC-hbr".to_string(),
                    rtcp_feedback: vec![],
                },
            },
            rtc::rtp_transceiver::rtp_sender::RtpCodecKind::Audio,
        )
        .map_err(|e| {
            rtflow_core::error::Error::Media(format!("WebRTC AAC registration failed: {e}"))
        })?;

    let config = RTCConfigurationBuilder::default()
        .with_ice_servers(ice_servers)
        .with_ice_transport_policy(ice_transport_policy)
        .build();
    let builder = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_handler(handler)
        .with_udp_addrs(udp_addrs);

    let pc: Arc<dyn PeerConnection> = Arc::new(builder.build().await.map_err(|e| {
        rtflow_core::error::Error::Media(format!("WebRTC peer connection build failed: {e}"))
    })?);
    Ok(pc)
}

/// The negotiated payload type for a sender, read after the answer has
/// been applied. WebRTC may remap payload types during negotiation, so
/// packets must be written with the negotiated value.
///
/// `mime` is the primary codec MIME type (e.g. `video/H264`); the sender's
/// codec list also carries rtx/fec entries, so the match is by MIME type
/// rather than list position.
pub(crate) async fn sender_payload_type(
    sender: &Arc<dyn webrtc::rtp_transceiver::RtpSender>,
    mime: &str,
) -> Option<u8> {
    let params: RTCRtpSendParameters = sender.get_parameters().await.ok()?;
    let needle = mime.to_ascii_uppercase();
    params
        .rtp_parameters
        .codecs
        .iter()
        .find(|c| {
            let hay = c.rtp_codec.mime_type.to_ascii_uppercase();
            hay == needle || hay.ends_with(needle.trim_start_matches("VIDEO/"))
        })
        .map(|c| c.payload_type)
}

/// Append gathered candidates to an SDP answer without introducing a blank
/// line (which terminates the SDP and makes browsers reject the answer).
pub(crate) fn append_candidates(sdp: &str, candidates: &[String]) -> String {
    if candidates.is_empty() {
        return sdp.to_owned();
    }
    // Candidates on the last m-line are not used for a BUNDLE transport by
    // some browsers. Put them in the first media section instead.
    let mut out = sdp.to_owned();
    let insert_at = out
        .match_indices("\r\nm=")
        .nth(1)
        .map(|(index, _)| index + 2);
    let mut block = String::new();
    for line in candidates {
        block.push_str(line.trim_end_matches('\r'));
        block.push_str("\r\n");
    }
    match insert_at {
        Some(index) => out.insert_str(index, &block),
        None => {
            if !out.ends_with("\r\n") {
                out.push_str("\r\n");
            }
            out.push_str(&block);
        }
    }
    out
}

/// Extract ICE candidates from an SDP document, associating each with its
/// media section (`a=mid`) and section index.
///
/// `webrtc` 0.20 does not feed remote candidates found in the SDP into the
/// ICE agent automatically — the application must call
/// [`PeerConnection::add_ice_candidate`] for each one.
pub(crate) fn candidates_from_sdp(sdp: &str) -> Vec<webrtc::peer_connection::RTCIceCandidateInit> {
    let mut out = Vec::new();
    let mut section = 0u16;
    let mut mid: Option<String> = None;
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("m=") {
            section += 1;
            mid = None;
            let _ = rest;
        } else if let Some(m) = line.strip_prefix("a=mid:") {
            mid = Some(m.trim().to_string());
        } else if let Some(c) = line.strip_prefix("a=candidate:") {
            out.push(webrtc::peer_connection::RTCIceCandidateInit {
                candidate: format!("candidate:{c}"),
                sdp_mid: mid.clone(),
                sdp_mline_index: Some(section),
                username_fragment: None,
                url: None,
            });
        }
    }
    out
}
