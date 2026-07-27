//! WHEP (WebRTC-HTTP Egress Protocol) endpoint.
//!
//! WHEP allows browsers to subscribe to media from a server using a simple
//! HTTP POST with SDP offer/answer exchange.
//!
//! See: https://datatracker.ietf.org/doc/html/draft-murillo-whep
use super::sdp_ext::{IceCandidate, WebRtcSdpAttributes, generate_whep_answer};
use super::session::{SessionRegistry, SessionType};
use crate::http::{HttpRequest, ResponseWriter};
use crate::mount::MountRegistry;
use crate::Result;
use std::sync::Arc;
use tokio::io::AsyncWrite;
use tracing::{debug, info};

/// WHEP endpoint configuration.
#[derive(Clone, Debug)]
pub struct WhepConfig {
    /// Server's public IP address for ICE candidates.
    pub public_ip: String,
    /// UDP port range start for ICE.
    pub udp_port_min: u16,
    /// UDP port range end for ICE.
    pub udp_port_max: u16,
    /// DTLS certificate fingerprint (placeholder until webrtc-dtls integration).
    pub fingerprint: String,
    /// Video payload type.
    pub video_payload_type: u8,
    /// Audio payload type.
    pub audio_payload_type: u8,
}

impl Default for WhepConfig {
    fn default() -> Self {
        Self {
            public_ip: "127.0.0.1".to_string(),
            udp_port_min: 50000,
            udp_port_max: 50100,
            fingerprint: "sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00".to_string(),
            video_payload_type: 96,
            audio_payload_type: 111,
        }
    }
}

/// Handle a WHEP POST request (SDP offer -> SDP answer).
pub async fn handle_whep_post<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    body: &[u8],
    mounts: Arc<MountRegistry>,
    config: &WhepConfig,
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<W>> {
    let offer_sdp = String::from_utf8_lossy(body);
    debug!("WHEP: received SDP offer ({} bytes)", offer_sdp.len());

    // Extract stream path from URL (e.g., /whep/live/stream -> /live/stream)
    let mount_path = req
        .path
        .strip_prefix("/whep")
        .unwrap_or(&req.path)
        .to_string();

    info!("WHEP: play request for {mount_path}");

    // Check if mount exists
    let mount = match mounts.get_handle(&mount_path) {
        Some(m) => m,
        None => {
            writer
                .write_body(404, "text/plain", b"Stream not found")
                .await?;
            return Ok(writer);
        }
    };

    // Parse ICE credentials from offer
    let offer_lines: Vec<&str> = offer_sdp.lines().collect();
    let _offer_attrs = WebRtcSdpAttributes::from_sdp_lines(&offer_lines);

    // Create session
    let session = sessions.create_session(SessionType::Play, mount_path.clone()).await?;
    let session_id = session.read().await.id.clone();

    // Generate our ICE credentials
    let ice_ufrag = generate_ufrag();
    let ice_pwd = generate_pwd();

    // Get DTLS fingerprint from session registry
    let dtls_config = sessions.dtls_config();
    let fingerprint = dtls_config.fingerprint().to_string();

    // Create host candidate
    let host_candidate = IceCandidate {
        foundation: "1".to_string(),
        component: 1,
        transport: "udp".to_string(),
        priority: 2130706431,
        address: config.public_ip.clone(),
        port: config.udp_port_min,
        candidate_type: "host".to_string(),
    };

    // Generate SDP answer
    let session_name = mount.description.session_name.to_string();
    let answer_sdp = generate_whep_answer(
        &session_name,
        &ice_ufrag,
        &ice_pwd,
        &fingerprint,
        &host_candidate,
        config.video_payload_type,
        config.audio_payload_type,
    );

    // Return 201 Created with SDP answer
    let content_length = answer_sdp.len().to_string();
    let location = format!("/whep{mount_path}/{session_id}");
    let headers = [
        ("Content-Type", "application/sdp"),
        ("Content-Length", content_length.as_str()),
        ("Location", location.as_str()),
        ("Access-Control-Allow-Origin", "*"),
    ];
    writer.write_headers(201, &headers).await?;
    writer.write_raw(answer_sdp.as_bytes()).await?;
    writer.flush().await?;

    info!("WHEP: session {session_id} created for {mount_path}");
    Ok(writer)
}

/// Handle WHEP DELETE request (session teardown).
pub async fn handle_whep_delete<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<W>> {
    // Extract session ID from path
    let session_id = req.path.rsplit('/').next().unwrap_or("");
    info!("WHEP: session teardown for {session_id}");

    if sessions.remove_session(session_id).await.is_some() {
        writer.write_body(200, "text/plain", b"OK").await?;
    } else {
        writer.write_body(404, "text/plain", b"Session not found").await?;
    }
    Ok(writer)
}

/// Handle WHEP OPTIONS request (CORS preflight).
pub async fn handle_whep_options<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
) -> Result<ResponseWriter<W>> {
    let headers = [
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "POST, DELETE, OPTIONS"),
        ("Access-Control-Allow-Headers", "Content-Type, Authorization"),
        ("Access-Control-Max-Age", "86400"),
    ];
    writer.write_headers(204, &headers).await?;
    writer.flush().await?;
    Ok(writer)
}

fn generate_ufrag() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:08x}", (ts & 0xFFFF_FFFF) as u32)
}

fn generate_pwd() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:016x}{:08x}", ts, (ts >> 64) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let config = WhepConfig::default();
        assert_eq!(config.video_payload_type, 96);
        assert_eq!(config.audio_payload_type, 111);
        assert_eq!(config.public_ip, "127.0.0.1");
    }
}
