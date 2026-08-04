//! WebRTC SDP extensions for ICE, DTLS, and BUNDLE.
//!
//! Handles parsing and generating the SDP attributes required for WebRTC:
//! - ice-ufrag / ice-pwd
//! - fingerprint (DTLS certificate hash)
//! - setup (DTLS role)
//! - rtcp-mux
//! - BUNDLE group
//! - candidate lines
use std::fmt::Write;

/// WebRTC-specific SDP session attributes.
#[derive(Clone, Debug, Default)]
pub struct WebRtcSdpAttributes {
    /// ICE username fragment.
    pub ice_ufrag: Option<String>,
    /// ICE password.
    pub ice_pwd: Option<String>,
    /// DTLS certificate fingerprint (e.g., "sha-256 AA:BB:CC:...").
    pub fingerprint: Option<String>,
    /// DTLS setup role: "active", "passive", or "actpass".
    pub setup: Option<String>,
    /// Whether rtcp-mux is enabled.
    pub rtcp_mux: bool,
    /// ICE candidates.
    pub candidates: Vec<IceCandidate>,
    /// BUNDLE group mids.
    pub bundle_mids: Vec<String>,
}

/// An ICE candidate.
#[derive(Clone, Debug)]
pub struct IceCandidate {
    /// Candidate foundation.
    pub foundation: String,
    /// Component ID (1 for RTP, 2 for RTCP).
    pub component: u32,
    /// Transport protocol (usually "udp").
    pub transport: String,
    /// Priority.
    pub priority: u32,
    /// IP address.
    pub address: String,
    /// Port.
    pub port: u16,
    /// Candidate type (host, srflx, relay).
    pub candidate_type: String,
}

impl IceCandidate {
    /// Format as SDP candidate attribute line.
    pub fn to_sdp(&self) -> String {
        format!(
            "candidate:{} {} {} {} {} {} typ {}",
            self.foundation,
            self.component,
            self.transport,
            self.priority,
            self.address,
            self.port,
            self.candidate_type,
        )
    }

    /// Parse from SDP candidate attribute value.
    pub fn from_sdp(value: &str) -> Option<Self> {
        let parts: Vec<&str> = value.split_whitespace().collect();
        if parts.len() < 8 {
            return None;
        }

        Some(Self {
            foundation: parts[0].trim_start_matches("candidate:").to_string(),
            component: parts[1].parse().ok()?,
            transport: parts[2].to_string(),
            priority: parts[3].parse().ok()?,
            address: parts[4].to_string(),
            port: parts[5].parse().ok()?,
            candidate_type: parts[7].to_string(),
        })
    }
}

impl WebRtcSdpAttributes {
    /// Generate SDP attribute lines for a media section.
    pub fn to_sdp_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();

        if let Some(ufrag) = &self.ice_ufrag {
            lines.push(format!("a=ice-ufrag:{ufrag}"));
        }
        if let Some(pwd) = &self.ice_pwd {
            lines.push(format!("a=ice-pwd:{pwd}"));
        }
        if let Some(fp) = &self.fingerprint {
            lines.push(format!("a=fingerprint:{fp}"));
        }
        if let Some(setup) = &self.setup {
            lines.push(format!("a=setup:{setup}"));
        }
        if self.rtcp_mux {
            lines.push("a=rtcp-mux".to_string());
        }
        for candidate in &self.candidates {
            lines.push(format!("a={}", candidate.to_sdp()));
        }

        lines
    }

    /// Parse WebRTC attributes from SDP lines.
    pub fn from_sdp_lines(lines: &[&str]) -> Self {
        let mut attrs = Self::default();

        for line in lines {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("a=ice-ufrag:") {
                attrs.ice_ufrag = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("a=ice-pwd:") {
                attrs.ice_pwd = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("a=fingerprint:") {
                attrs.fingerprint = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("a=setup:") {
                attrs.setup = Some(value.to_string());
            } else if line == "a=rtcp-mux" {
                attrs.rtcp_mux = true;
            } else if let Some(value) = line.strip_prefix("a=candidate:")
                && let Some(candidate) = IceCandidate::from_sdp(value)
            {
                attrs.candidates.push(candidate);
            }
        }

        attrs
    }
}

/// Generate a minimal WebRTC SDP answer for WHEP (playback).
pub fn generate_whep_answer(
    session_name: &str,
    ice_ufrag: &str,
    ice_pwd: &str,
    fingerprint: &str,
    host_candidate: &IceCandidate,
    video_payload_type: u8,
    audio_payload_type: u8,
) -> String {
    let mut sdp = String::with_capacity(2048);

    // Session level
    let _ = writeln!(sdp, "v=0");
    let _ = writeln!(sdp, "o=- 0 0 IN IP4 0.0.0.0");
    let _ = writeln!(sdp, "s={session_name}");
    let _ = writeln!(sdp, "t=0 0");
    let _ = writeln!(sdp, "a=group:BUNDLE 0 1");
    let _ = writeln!(sdp, "a=msid-semantic: WMS *");

    // Video media
    let _ = writeln!(sdp, "m=video 9 UDP/TLS/RTP/SAVPF {video_payload_type}");
    let _ = writeln!(sdp, "c=IN IP4 0.0.0.0");
    let _ = writeln!(sdp, "a=mid:0");
    let _ = writeln!(sdp, "a=ice-ufrag:{ice_ufrag}");
    let _ = writeln!(sdp, "a=ice-pwd:{ice_pwd}");
    let _ = writeln!(sdp, "a=fingerprint:{fingerprint}");
    let _ = writeln!(sdp, "a=setup:passive");
    let _ = writeln!(sdp, "a=rtcp-mux");
    let _ = writeln!(sdp, "a=recvonly");
    let _ = writeln!(sdp, "a=rtpmap:{video_payload_type} H264/90000");
    let _ = writeln!(sdp, "a={}", host_candidate.to_sdp());

    // Audio media
    let _ = writeln!(sdp, "m=audio 9 UDP/TLS/RTP/SAVPF {audio_payload_type}");
    let _ = writeln!(sdp, "c=IN IP4 0.0.0.0");
    let _ = writeln!(sdp, "a=mid:1");
    let _ = writeln!(sdp, "a=ice-ufrag:{ice_ufrag}");
    let _ = writeln!(sdp, "a=ice-pwd:{ice_pwd}");
    let _ = writeln!(sdp, "a=fingerprint:{fingerprint}");
    let _ = writeln!(sdp, "a=setup:passive");
    let _ = writeln!(sdp, "a=rtcp-mux");
    let _ = writeln!(sdp, "a=recvonly");
    let _ = writeln!(sdp, "a=rtpmap:{audio_payload_type} opus/48000/2");
    let _ = writeln!(sdp, "a={}", host_candidate.to_sdp());

    sdp
}

/// Generate a minimal WebRTC SDP answer for WHIP (ingest).
pub fn generate_whip_answer(
    session_name: &str,
    ice_ufrag: &str,
    ice_pwd: &str,
    fingerprint: &str,
    host_candidate: &IceCandidate,
) -> String {
    let mut sdp = String::with_capacity(2048);

    // Session level
    let _ = writeln!(sdp, "v=0");
    let _ = writeln!(sdp, "o=- 0 0 IN IP4 0.0.0.0");
    let _ = writeln!(sdp, "s={session_name}");
    let _ = writeln!(sdp, "t=0 0");
    let _ = writeln!(sdp, "a=group:BUNDLE 0");
    let _ = writeln!(sdp, "a=msid-semantic: WMS *");

    // Single media section for receiving
    let _ = writeln!(sdp, "m=application 9 UDP/DTLS/SCTP webrtc-datachannel");
    let _ = writeln!(sdp, "c=IN IP4 0.0.0.0");
    let _ = writeln!(sdp, "a=mid:0");
    let _ = writeln!(sdp, "a=ice-ufrag:{ice_ufrag}");
    let _ = writeln!(sdp, "a=ice-pwd:{ice_pwd}");
    let _ = writeln!(sdp, "a=fingerprint:{fingerprint}");
    let _ = writeln!(sdp, "a=setup:passive");
    let _ = writeln!(sdp, "a={}", host_candidate.to_sdp());

    sdp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_roundtrip() {
        let candidate = IceCandidate {
            foundation: "1".to_string(),
            component: 1,
            transport: "udp".to_string(),
            priority: 2130706431,
            address: "192.168.1.100".to_string(),
            port: 50000,
            candidate_type: "host".to_string(),
        };

        let sdp = candidate.to_sdp();
        assert!(sdp.contains("candidate:1"));
        assert!(sdp.contains("udp"));
        assert!(sdp.contains("192.168.1.100"));

        let parsed = IceCandidate::from_sdp(&sdp).unwrap();
        assert_eq!(parsed.foundation, "1");
        assert_eq!(parsed.port, 50000);
    }

    #[test]
    fn sdp_attributes_roundtrip() {
        let attrs = WebRtcSdpAttributes {
            ice_ufrag: Some("testufrag".to_string()),
            ice_pwd: Some("testpwd123456789012".to_string()),
            fingerprint: Some("sha-256 AA:BB:CC:DD".to_string()),
            setup: Some("actpass".to_string()),
            rtcp_mux: true,
            candidates: vec![],
            bundle_mids: vec![],
        };

        let lines = attrs.to_sdp_lines();
        assert!(lines.iter().any(|l| l.contains("ice-ufrag:testufrag")));
        assert!(lines.iter().any(|l| l.contains("rtcp-mux")));

        let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let parsed = WebRtcSdpAttributes::from_sdp_lines(&line_refs);
        assert_eq!(parsed.ice_ufrag, Some("testufrag".to_string()));
        assert!(parsed.rtcp_mux);
    }
}
