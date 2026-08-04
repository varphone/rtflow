//! WHEP (WebRTC-HTTP Egress Protocol) playback.
//!
//! The browser POSTs an SDP offer; the server creates a peer connection,
//! answers with the negotiated SDP (including gathered ICE candidates),
//! and streams the mount's media as RTP (H.264 video; Opus audio when the
//! source provides it — WebRTC has no AAC/G.711, so other audio codecs
//! are skipped until transcoding exists).

use std::sync::Arc;

use tokio::sync::RwLock;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::{MediaStreamId, MediaStreamTrack, MediaStreamTrackId};
use webrtc::peer_connection::{PeerConnection, RTCSessionDescription};
use rtc::rtp::packet::Packet as RtpPacket;
use rtc::rtp::header::Header as RtpHeader;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};

use super::session::{SessionRegistry, SessionState, SessionType, WebrtcSession};
use super::transport::{
    GATHER_TIMEOUT, PcHandler, PcState, build_peer_connection, sender_payload_type,
};
use crate::http::ResponseWriter;
use crate::media::Codec;
use crate::mount::MountRegistry;
use crate::rtp::Packetizer;
use crate::{Error, Result};

/// WHEP endpoint configuration.
#[derive(Clone, Debug)]
pub struct WhepConfig {
    /// ICE server URLs (STUN/TURN). Empty for LAN host-candidate only.
    pub ice_servers: Vec<String>,
    /// Local UDP addresses to bind (e.g. `["0.0.0.0:0"]`).
    pub udp_addrs: Vec<String>,
    /// RTP packetization MTU (bytes, including headers).
    pub mtu: usize,
}

impl Default for WhepConfig {
    fn default() -> Self {
        Self {
            ice_servers: vec![],
            udp_addrs: vec!["0.0.0.0:0".to_string()],
            mtu: 1200,
        }
    }
}

/// Handle a WHEP POST: browser offer → server answer (201 + SDP).
pub async fn handle_whep_post(
    req: &crate::http::HttpRequest,
    writer: ResponseWriter<crate::http::ConnWriter>,
    body: &[u8],
    mounts: Arc<MountRegistry>,
    config: &WhepConfig,
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<crate::http::ConnWriter>> {
    let path = req
        .path
        .trim_end_matches('/')
        .strip_prefix("/whep")
        .unwrap_or(req.path.trim_end_matches('/'))
        .to_string();
    if path.is_empty() || path == "/whep" {
        return respond_plain(writer, 400, "missing stream path").await;
    }

    let offer = std::str::from_utf8(body)
        .map_err(|_| Error::Http("WHEP: offer must be UTF-8 SDP".into()))?;
    if !offer.contains("m=") {
        return respond_plain(writer, 400, "invalid SDP offer").await;
    }

    // The mount must exist and offer at least one playable track.
    let mount = mounts
        .get_handle(&path)
        .ok_or_else(|| Error::Http("WHEP: mount not found".into()))?;
    let has_video = mount
        .description
        .tracks
        .iter()
        .any(|t| t.kind == crate::media::MediaKind::Video);
    if !has_video {
        return respond_plain(writer, 404, "no video track on mount").await;
    }

    let session = sessions
        .create_session(SessionType::Play, path.clone())
        .await?;

    // Build the peer connection with the shared handler.
    let pc_state = PcState::new();
    let handler = Arc::new(PcHandler::new(pc_state.clone()));
    let pc = build_peer_connection(config.udp_addrs.clone(), handler).await?;
    {
        let mut s = session.write().await;
        s.attach(pc.clone(), pc_state.clone());
    }

    // Accept the browser's offer.
    let remote = RTCSessionDescription::offer(offer.to_string())
        .map_err(|e| Error::Http(format!("WHEP: invalid offer: {e}")))?;
    if let Err(e) = pc.set_remote_description(remote).await {
        let id = session.read().await.id.clone();
        sessions.remove_session(&id).await;
        return Err(Error::Http(format!("WHEP: set_remote_description: {e}")));
    }
    // webrtc 0.20 does not auto-feed remote candidates found in the SDP
    // into ICE; add them explicitly (non-trickle offers carry them inline).
    for candidate in super::transport::candidates_from_sdp(offer) {
        if let Err(e) = pc.add_ice_candidate(candidate).await {
            tracing::debug!("WHEP: add_ice_candidate failed: {e}");
        }
    }

    // Add a sender for each video track (H.264) and Opus audio if present.
    let mut senders = Vec::new();
    for track in &mount.description.tracks {
        let (kind, codec) = match track.kind {
            crate::media::MediaKind::Video => (RtpCodecKind::Video, track.codec),
            crate::media::MediaKind::Audio if track.codec == Codec::Opus => {
                (RtpCodecKind::Audio, track.codec)
            }
            _ => continue,
        };
        if !matches!(codec, Codec::H264 | Codec::H265 | Codec::Vp8 | Codec::Vp9 | Codec::Opus) {
            tracing::warn!(?codec, "WHEP: codec not supported by WebRTC, skipping track");
            continue;
        }
        let track_id = format!("rtflow-{}-{}", kind_str(kind), track.id);
        // Explicit SSRC so the answer carries it (a=ssrc) and the browser
        // can match inbound packets to this track. Reliable assignment is
        // preferable to reading it back from the sender mid-negotiation.
        let track_ssrc = rand_ssrc();
        let media_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            MediaStreamId::from("rtflow-whep".to_string()),
            MediaStreamTrackId::from(track_id.clone()),
            track_id.clone(),
            kind,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(track_ssrc),
                    ..Default::default()
                },
                active: true,
                codec: RTCRtpCodec {
                    mime_type: mime_for(codec).to_string(),
                    clock_rate: track.clock_rate.max(1),
                    channels: if kind == RtpCodecKind::Audio { 2 } else { 0 },
                    sdp_fmtp_line: String::new(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        )));
        match pc.add_track(media_track.clone()).await {
            Ok(sender) => senders.push((media_track, sender, kind, codec, track_ssrc)),
            Err(e) => {
                tracing::warn!("WHEP: add_track failed: {e}");
            }
        }
    }
    if senders.is_empty() {
        let _ = session.write().await.close().await;
        return respond_plain(writer, 400, "no negotiated media").await;
    }

    // Answer and wait for ICE gathering so the SDP carries candidates.
    let answer = match pc.create_answer(None).await {
        Ok(a) => a,
        Err(e) => {
            let id = session.read().await.id.clone();
            sessions.remove_session(&id).await;
            return Err(Error::Http(format!("WHEP: create_answer: {e}")));
        }
    };
    if let Err(e) = pc.set_local_description(answer.clone()).await {
        let id = session.read().await.id.clone();
        sessions.remove_session(&id).await;
        return Err(Error::Http(format!("WHEP: set_local_description: {e}")));
    }

    tokio::time::timeout(GATHER_TIMEOUT, pc_state.gather_done.notified())
        .await
        .ok();
    let answer_sdp = assemble_answer_sdp(pc_state.clone(), &answer).await;

    // Start the play pipeline: mount → packetizer → RTP sender.
    for (track, sender, kind, codec, ssrc) in &senders {
        let Some(pt) = sender_payload_type(sender, mime_for(*codec)).await else {
            tracing::warn!("WHEP: sender has no negotiated payload type");
            continue;
        };

        spawn_play_task(
            &session,
            pc.clone(),
            mounts.clone(),
            path.clone(),
            *kind,
            *codec,
            pt,
            Some(*ssrc),
            track.clone(),
            config.mtu,
        )
        .await;
    }

    // Touch the session and answer.
    {
        let mut s = session.write().await;
        s.touch();
    }
    let mut writer = writer;
    writer
        .write_headers(
            201,
            &[
                ("Location", &format!("/whep/{}", session.read().await.id)),
                ("Access-Control-Expose-Headers", "Location"),
                ("Cache-Control", "no-store"),
                ("Content-Type", "application/sdp"),
            ],
        )
        .await?;
    writer.write_raw(answer_sdp.as_bytes()).await?;
    writer.flush().await?;
    Ok(writer)
}

/// Random 32-bit SSRC for a sent track (RFC 3550: chosen uniformly so
/// collisions are unlikely in a single session).
fn rand_ssrc() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // SplitMix64-style mixing for a decent spread without a rand dep.
    let mut z = u64::from(seed).wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    (z ^ (z >> 31)) as u32 & 0x7fff_ffff
}

/// MIME type for a WebRTC-capable codec (H.264/VP8/VP9/Opus).
fn mime_for(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "video/H264",
        Codec::H265 => "video/H265",
        Codec::Vp8 => "video/VP8",
        Codec::Vp9 => "video/VP9",
        Codec::Opus => "audio/opus",
        _ => "application/unknown",
    }
}

fn kind_str(kind: RtpCodecKind) -> &'static str {
    match kind {
        RtpCodecKind::Video => "video",
        RtpCodecKind::Audio => "audio",
        _ => "unknown",
    }
}

/// Append the gathered ICE candidates to the answer SDP.
async fn assemble_answer_sdp(pc_state: Arc<PcState>, answer: &RTCSessionDescription) -> String {
    let mut sdp = answer.sdp.clone();
    let candidates = pc_state.candidates.read().await;
    if !candidates.is_empty() {
        sdp.push_str("\r\n");
        for line in candidates.iter() {
            sdp.push_str(line);
            sdp.push_str("\r\n");
        }
    }
    sdp
}

/// The play loop: subscribe to the mount, packetize access units and
/// write them to the RTP sender. H.264/H.265/VP8/VP9 Annex-B frames are
/// converted by the shared packetizer; RTP timestamps come from the
/// access unit's RTP clock (90 kHz video / 48 kHz Opus).
#[allow(clippy::too_many_arguments)]
async fn spawn_play_task(
    session: &Arc<RwLock<WebrtcSession>>,
    pc: Arc<dyn PeerConnection>,
    mounts: Arc<MountRegistry>,
    mount_path: String,
    _kind: RtpCodecKind,
    codec: Codec,
    payload_type: u8,
    ssrc: Option<u32>,
    track: Arc<TrackLocalStaticRTP>,
    mtu: usize,
) {
    let cancel = session.read().await.cancel.clone();
    let ssrc_value = ssrc.unwrap_or(0);

    let task_session = session.clone();
    let task = tokio::spawn(async move {
        let (mut rx, startup) = match mounts.get_handle(&mount_path) {
            Some(mount) => {
                let (rx, startup) = mount.subscribe_with_startup();
                (rx, startup)
            }
            None => {
                tracing::error!("WHEP: mount vanished: {mount_path}");
                return;
            }
        };
        let mut packetizer = match Packetizer::new(codec, payload_type, ssrc_value, mtu) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("WHEP: packetizer init failed: {e}");
                return;
            }
        };
        let mut startup_iter = startup.into_iter();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                frame = async {
                    if let Some(unit) = startup_iter.next() {
                        Some(unit)
                    } else {
                        rx.recv().await.ok()
                    }
                } => {
                    let Some(unit) = frame else { break };

                    if unit.codec != codec {
                        continue;
                    }
                    let packets = match packetizer.packetize(&unit) {
                        Ok(pkts) => pkts,
                        Err(e) => {
                            tracing::debug!("WHEP: packetize error: {e}");
                            continue;
                        }
                    };
                    for p in packets {
                        let rtp = RtpPacket {
                            header: RtpHeader {
                                version: 2,
                                marker: p.header.marker,
                                payload_type: p.header.payload_type,
                                sequence_number: p.header.sequence,
                                timestamp: p.header.timestamp,
                                ssrc: p.header.ssrc,
                                ..Default::default()
                            },
                            payload: p.payload,
                        };
                        if track.write_rtp(rtp).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        tracing::debug!("WHEP: play task ended ({mount_path})");
        // The media pipeline ended: mark the session closed so the idle
        // sweep reclaims its peer connection and UDP socket.
        if let Ok(mut s) = task_session.try_write() {
            s.state = SessionState::Closed;
        }
        let _ = pc;
    });
    session.write().await.spawn_media_task(task);
}

/// Handle WHEP DELETE: release the session.
pub async fn handle_whep_delete(
    req: &crate::http::HttpRequest,
    writer: ResponseWriter<crate::http::ConnWriter>,
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<crate::http::ConnWriter>> {
    if let Some(id) = session_id_from_path(&req.path) {
        sessions.remove_session(&id).await;
    }
    let mut writer = writer;
    writer.write_body(200, "text/plain", b"ok").await?;
    Ok(writer)
}

/// Handle WHEP OPTIONS (CORS preflight).
pub async fn handle_whep_options(
    _req: &crate::http::HttpRequest,
    writer: ResponseWriter<crate::http::ConnWriter>,
) -> Result<ResponseWriter<crate::http::ConnWriter>> {
    let mut writer = writer;
    writer.write_body(204, "text/plain", b"").await?;
    Ok(writer)
}

fn session_id_from_path(path: &str) -> Option<String> {
    path.rsplit('/').next().map(|s| s.to_string()).filter(|s| !s.is_empty())
}

async fn respond_plain(
    mut writer: ResponseWriter<crate::http::ConnWriter>,
    status: u16,
    msg: &str,
) -> Result<ResponseWriter<crate::http::ConnWriter>> {
    writer.write_body(status, "text/plain", msg.as_bytes()).await?;
    Ok(writer)
}
