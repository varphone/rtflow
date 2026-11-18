//! WHIP (WebRTC-HTTP Ingest Protocol) publishing.
//!
//! The browser POSTs an SDP offer for a stream it wants to publish; the
//! server creates a peer connection, answers with the negotiated SDP,
//! and feeds the received RTP (H.264 video, Opus audio) into the mount
//! via the shared depacketizer — the same path as RTSP publishing.

use std::sync::Arc;
use std::time::Duration;

use rtc::rtp::packet::Packet as RtpPacket;
use tokio::sync::RwLock;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, RTCIceServer, RTCIceTransportPolicy, RTCSessionDescription,
};

use super::session::{SessionRegistry, SessionState, SessionType};
use super::transport::{GATHER_TIMEOUT, PcHandler, PcState, build_peer_connection};
use rtflow_core::error::{Error, Result};
use rtflow_http::ResponseWriter;
use rtflow_media::Codec;
use rtflow_mount::MountRegistry;
use rtflow_rtp::Depacketizer;

/// WHIP endpoint configuration.
#[derive(Clone, Debug)]
pub struct WhipConfig {
    /// ICE server URLs (STUN/TURN). Empty for LAN host-candidate only.
    pub ice_servers: Vec<String>,
    /// TURN username/credential.
    pub ice_username: String,
    pub ice_credential: String,
    /// `all` or `relay`.
    pub ice_transport_policy: String,
    /// Local UDP addresses to bind (e.g. `["0.0.0.0:0"]`).
    pub udp_addrs: Vec<String>,
    /// RTP depacketization safety bound: frames larger than this are
    /// rejected instead of buffered (malicious/abnormal publisher).
    pub max_frame_bytes: usize,
}

impl Default for WhipConfig {
    fn default() -> Self {
        Self {
            ice_servers: vec![],
            ice_username: String::new(),
            ice_credential: String::new(),
            ice_transport_policy: "all".into(),
            udp_addrs: vec!["0.0.0.0:0".to_string()],
            max_frame_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Handle a WHIP POST: browser offer → server answer (201 + SDP), then
/// wire the received RTP into the mount.
pub async fn handle_whip_post(
    req: &rtflow_http::HttpRequest,
    writer: ResponseWriter<rtflow_http::ConnWriter>,
    body: &[u8],
    mounts: Arc<MountRegistry>,
    config: &WhipConfig,
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<rtflow_http::ConnWriter>> {
    let path = req
        .path
        .trim_end_matches('/')
        .strip_prefix("/whip")
        .unwrap_or(req.path.trim_end_matches('/'))
        .to_string();
    if path.is_empty() || path == "/whip" {
        return respond_plain(writer, 400, "missing stream path").await;
    }

    let offer = std::str::from_utf8(body)
        .map_err(|_| Error::Http("WHIP: offer must be UTF-8 SDP".into()))?;
    if !offer.contains("m=") {
        return respond_plain(writer, 400, "invalid SDP offer").await;
    }

    // The mount must be a publisher mount (created with the right config).
    let mount = mounts
        .get_handle(&path)
        .ok_or_else(|| Error::Http("WHIP: mount not found".into()))?;
    let _publisher = mount
        .publisher()
        .map_err(|_| Error::Http("WHIP: mount is not a publisher mount".into()))?;

    let session = sessions
        .create_session(SessionType::Publish, path.clone())
        .await?;

    let pc_state = PcState::new();
    let handler = Arc::new(PcHandler::new(pc_state.clone()));
    let ice_servers = config
        .ice_servers
        .iter()
        .map(|url| RTCIceServer {
            urls: vec![url.clone()],
            username: config.ice_username.clone(),
            credential: config.ice_credential.clone(),
        })
        .collect();
    let ice_policy = if config.ice_transport_policy.eq_ignore_ascii_case("relay") {
        RTCIceTransportPolicy::Relay
    } else {
        RTCIceTransportPolicy::All
    };
    let pc =
        build_peer_connection(config.udp_addrs.clone(), handler, ice_servers, ice_policy).await?;
    {
        let mut s = session.write().await;
        s.attach(pc.clone(), pc_state.clone());
    }

    let remote = RTCSessionDescription::offer(offer.to_string())
        .map_err(|e| Error::Http(format!("WHIP: invalid offer: {e}")))?;
    if let Err(e) = pc.set_remote_description(remote).await {
        let id = session.read().await.id.clone();
        sessions.remove_session(&id).await;
        return Err(Error::Http(format!("WHIP: set_remote_description: {e}")));
    }
    // webrtc 0.20 does not auto-feed remote candidates found in the SDP
    // into ICE; add them explicitly (non-trickle offers carry them inline).
    for candidate in super::transport::candidates_from_sdp(offer) {
        if let Err(e) = pc.add_ice_candidate(candidate).await {
            tracing::debug!("WHIP: add_ice_candidate failed: {e}");
        }
    }

    let answer = match pc.create_answer(None).await {
        Ok(a) => a,
        Err(e) => {
            let id = session.read().await.id.clone();
            sessions.remove_session(&id).await;
            return Err(Error::Http(format!("WHIP: create_answer: {e}")));
        }
    };
    if let Err(e) = pc.set_local_description(answer.clone()).await {
        let id = session.read().await.id.clone();
        sessions.remove_session(&id).await;
        return Err(Error::Http(format!("WHIP: set_local_description: {e}")));
    }

    tokio::time::timeout(GATHER_TIMEOUT, pc_state.gather_done.notified())
        .await
        .ok();
    let answer_sdp = {
        let candidates = pc_state.candidates.read().await;
        super::transport::append_candidates(&answer.sdp, &candidates)
    };

    // Start the ingest loop: remote track RTP → depacketizer → mount.
    spawn_ingest_task(
        &session,
        pc.clone(),
        mounts.clone(),
        path.clone(),
        config.max_frame_bytes,
    )
    .await;

    {
        let mut s = session.write().await;
        s.touch();
    }
    let mut writer = writer;
    let answer_len = answer_sdp.len().to_string();
    writer
        .write_headers(
            201,
            &[
                ("Location", &format!("/whip/{}", session.read().await.id)),
                ("Access-Control-Expose-Headers", "Location"),
                ("Access-Control-Allow-Origin", "*"),
                ("Cache-Control", "no-store"),
                ("Content-Type", "application/sdp"),
                ("Content-Length", &answer_len),
            ],
        )
        .await?;
    tracing::info!(mount = %path, "WHIP session established");
    writer.write_raw(answer_sdp.as_bytes()).await?;
    writer.flush().await?;
    Ok(writer)
}

/// The ingest loop: for every remote track (arriving via the PC event
/// handler), read RTP packets, depacketize into access units and push
/// them to the mount publisher.
async fn spawn_ingest_task(
    session: &Arc<RwLock<super::session::WebrtcSession>>,
    pc: Arc<dyn PeerConnection>,
    mounts: Arc<MountRegistry>,
    mount_path: String,
    max_frame_bytes: usize,
) {
    let cancel = session.read().await.cancel.clone();
    let task_session = session.clone();
    let task = tokio::spawn(async move {
        // Wait for the browser to connect, then register a depacketizer
        // per track from the negotiated codec parameters.
        let tracks = loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    let receivers = pc.get_receivers().await;
                    let mut found = Vec::new();
                    for receiver in &receivers {
                        found.push(receiver.track().clone());
                    }
                    if !found.is_empty() {
                        break found;
                    }
                }
            }
        };

        for (track_index, track) in tracks.into_iter().enumerate() {
            let cancel = cancel.clone();
            let mounts = mounts.clone();
            let mount_path = mount_path.clone();
            let negotiated = negotiated_codec(&track).await;
            tokio::spawn(async move {
                let Some(mount) = mounts.get_handle(&mount_path) else {
                    tracing::error!("WHIP: mount unavailable for {mount_path}");
                    return;
                };
                let Ok(publisher) = mount.publisher() else {
                    tracing::error!("WHIP: publisher unavailable for {mount_path}");
                    return;
                };
                let mut depacketizer: Option<Depacketizer> = None;
                // Media track index maps to the mount's TrackDescription id
                // (0 = first track, 1 = second, ...), matching the order
                // receivers were created from the negotiated media sections.
                let mut track_id: u32 = track_index as u32;
                let mut track_codec: Option<(Codec, u32)> = negotiated;
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        event = track.poll() => {
                            let Some(event) = event else { break };
                            match event {
                                TrackRemoteEvent::OnRtpPacket(packet) => {
                                    ingest_rtp(
                                        &mut depacketizer,
                                        &mut track_id,
                                        &mut track_codec,
                                        &publisher,
                                        packet,
                                        max_frame_bytes,
                                    );
                                }
                                TrackRemoteEvent::OnRtcpPacket(_) => {
                                    // SR/RR/PLI handled internally by the
                                    // peer connection; nothing to forward.
                                }
                                TrackRemoteEvent::OnEnded | TrackRemoteEvent::OnError => break,
                                _ => {}
                            }
                        }
                    }
                }
                tracing::debug!("WHIP: ingest task ended");
            });
        }
        if let Ok(mut s) = task_session.try_write() {
            s.state = SessionState::Closed;
        }
    });
    session.write().await.spawn_media_task(task);
}

fn ingest_rtp(
    depacketizer: &mut Option<Depacketizer>,
    track_id: &mut u32,
    track_codec: &mut Option<(Codec, u32)>,
    publisher: &rtflow_mount::LocalPublisher,
    packet: RtpPacket,
    max_frame_bytes: usize,
) {
    // Lazily configure the depacketizer from the negotiated track codec.
    if depacketizer.is_none() {
        let Some((codec, clock_rate)) = track_codec else {
            return;
        };
        *depacketizer = Some(Depacketizer::new(*codec, *track_id as u8, *clock_rate));
    }

    let Some(dep) = depacketizer.as_mut() else {
        return;
    };
    // Guard against a malicious publisher growing buffers without bound.
    if packet.payload.len() > max_frame_bytes {
        tracing::warn!(
            "WHIP: oversized RTP packet dropped ({} bytes)",
            packet.payload.len()
        );
        return;
    }
    let rtp_packet = rtflow_rtp::RtpPacket {
        header: rtflow_rtp::RtpHeader {
            marker: packet.header.marker,
            payload_type: packet.header.payload_type,
            sequence: packet.header.sequence_number,
            timestamp: packet.header.timestamp,
            ssrc: packet.header.ssrc,
        },
        payload: packet.payload.clone(),
    };
    match dep.push(rtp_packet) {
        Ok(units) => {
            for unit in units {
                if let Err(e) = publisher.push(unit) {
                    tracing::debug!("WHIP: push to mount failed: {e}");
                    return;
                }
            }
        }
        Err(e) => {
            tracing::debug!("WHIP: depacketize error: {e}");
        }
    }
}

/// Map the negotiated remote-track codec (from the SDP answer) to the
/// shared [`Codec`] enum.
async fn negotiated_codec(track: &Arc<dyn TrackRemote>) -> Option<(Codec, u32)> {
    let ssrc = track.ssrcs().await.into_iter().next()?;
    let codec = track.codec(ssrc).await?;
    let name = codec.mime_type.to_ascii_lowercase();
    let mapped = if name.contains("h264") {
        Some(Codec::H264)
    } else if name.contains("h265") || name.contains("hevc") {
        Some(Codec::H265)
    } else if name.contains("vp8") {
        Some(Codec::Vp8)
    } else if name.contains("vp9") {
        Some(Codec::Vp9)
    } else if name.contains("opus") {
        Some(Codec::Opus)
    } else if name.contains("pcma") || name.ends_with("pcm-a") || name.contains("alaw") {
        Some(Codec::Pcma)
    } else if name.contains("pcmu") || name.ends_with("pcm-u") || name.contains("ulaw") {
        Some(Codec::Pcmu)
    } else if name.contains("mp4a") || name.contains("aac") {
        Some(Codec::Aac)
    } else {
        tracing::warn!(mime = %codec.mime_type, "WHIP: unsupported codec, track skipped");
        None
    };
    mapped.map(|c| (c, codec.clock_rate.max(1)))
}

/// Handle WHIP DELETE: stop the session.
pub async fn handle_whip_delete(
    req: &rtflow_http::HttpRequest,
    writer: ResponseWriter<rtflow_http::ConnWriter>,
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<rtflow_http::ConnWriter>> {
    if let Some(id) = session_id_from_path(&req.path) {
        sessions.remove_session(&id).await;
    }
    let mut writer = writer;
    writer.write_body(200, "text/plain", b"ok").await?;
    Ok(writer)
}

/// Handle WHIP OPTIONS (CORS preflight).
pub async fn handle_whip_options(
    _req: &rtflow_http::HttpRequest,
    writer: ResponseWriter<rtflow_http::ConnWriter>,
) -> Result<ResponseWriter<rtflow_http::ConnWriter>> {
    let mut writer = writer;
    writer.write_body(204, "text/plain", b"").await?;
    Ok(writer)
}

fn session_id_from_path(path: &str) -> Option<String> {
    path.rsplit('/')
        .next()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

async fn respond_plain(
    mut writer: ResponseWriter<rtflow_http::ConnWriter>,
    status: u16,
    msg: &str,
) -> Result<ResponseWriter<rtflow_http::ConnWriter>> {
    writer
        .write_body(status, "text/plain", msg.as_bytes())
        .await?;
    Ok(writer)
}
