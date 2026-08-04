//! WHIP (WebRTC-HTTP Ingestion Protocol) endpoint.
//!
//! WHIP allows browsers to publish media to a server using a simple
//! HTTP POST with SDP offer/answer exchange.
//!
//! See: https://datatracker.ietf.org/doc/html/draft-ietf-wish-whip
use super::sdp_ext::{IceCandidate, WebRtcSdpAttributes, generate_whip_answer};
use super::session::{SessionRegistry, SessionType};
use crate::Result;
use crate::http::{HttpRequest, ResponseWriter};
use crate::mount::MountRegistry;
use std::sync::Arc;
use tokio::io::AsyncWrite;
use tracing::{debug, info};

/// WHIP endpoint configuration.
#[derive(Clone, Debug)]
pub struct WhipConfig {
    /// Server's public IP address for ICE candidates.
    pub public_ip: String,
    /// UDP port range start for ICE.
    pub udp_port_min: u16,
    /// UDP port range end for ICE.
    pub udp_port_max: u16,
    /// DTLS certificate fingerprint (placeholder until webrtc-dtls integration).
    pub fingerprint: String,
}

impl Default for WhipConfig {
    fn default() -> Self {
        Self {
            public_ip: "127.0.0.1".to_string(),
            udp_port_min: 50000,
            udp_port_max: 50100,
            fingerprint: "sha-256 00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00".to_string(),
        }
    }
}

/// Handle a WHIP POST request (SDP offer -> SDP answer).
pub async fn handle_whip_post<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    body: &[u8],
    _mounts: Arc<MountRegistry>,
    config: &WhipConfig,
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<W>> {
    let offer_sdp = String::from_utf8_lossy(body);
    debug!("WHIP: received SDP offer ({} bytes)", offer_sdp.len());

    // Extract stream path from URL (e.g., /whip/live/stream -> /live/stream)
    let mount_path = req
        .path
        .strip_prefix("/whip")
        .unwrap_or(&req.path)
        .to_string();

    info!("WHIP: publish request for {mount_path}");

    // Parse ICE credentials from offer
    let offer_lines: Vec<&str> = offer_sdp.lines().collect();
    let _offer_attrs = WebRtcSdpAttributes::from_sdp_lines(&offer_lines);

    // Create session
    let session = sessions
        .create_session(SessionType::Publish, mount_path.clone())
        .await?;
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
    let answer_sdp = generate_whip_answer(
        "whip-session",
        &ice_ufrag,
        &ice_pwd,
        &fingerprint,
        &host_candidate,
    );

    // Return 201 Created with SDP answer
    let content_length = answer_sdp.len().to_string();
    let location = format!("/whip{mount_path}/{session_id}");
    let headers = [
        ("Content-Type", "application/sdp"),
        ("Content-Length", content_length.as_str()),
        ("Location", location.as_str()),
        ("Access-Control-Allow-Origin", "*"),
    ];
    writer.write_headers(201, &headers).await?;
    writer.write_raw(answer_sdp.as_bytes()).await?;
    writer.flush().await?;

    info!("WHIP: session {session_id} created for {mount_path}");
    Ok(writer)
}

/// Handle WHIP DELETE request (session teardown).
pub async fn handle_whip_delete<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<W>> {
    // Extract session ID from path
    let session_id = req.path.rsplit('/').next().unwrap_or("");
    info!("WHIP: session teardown for {session_id}");

    if sessions.remove_session(session_id).await.is_some() {
        writer.write_body(200, "text/plain", b"OK").await?;
    } else {
        writer
            .write_body(404, "text/plain", b"Session not found")
            .await?;
    }
    Ok(writer)
}

/// Handle WHIP OPTIONS request (CORS preflight).
pub async fn handle_whip_options<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
) -> Result<ResponseWriter<W>> {
    let headers = [
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "POST, DELETE, OPTIONS"),
        (
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization",
        ),
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
    fn ufrag_generation() {
        let ufrag = generate_ufrag();
        assert_eq!(ufrag.len(), 8);
        assert!(ufrag.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn pwd_generation() {
        let pwd = generate_pwd();
        assert!(pwd.len() >= 24);
    }
}
