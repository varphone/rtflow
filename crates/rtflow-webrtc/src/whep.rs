//! WHEP (WebRTC-HTTP Egress Protocol) playback.
//!
//! The browser POSTs an SDP offer; the server creates a peer connection,
//! answers with the negotiated SDP (including gathered ICE candidates),
//! and streams the mount's media as RTP (H.264 video; Opus audio when the
//! source provides it — WebRTC has no AAC/G.711, so other audio codecs
//! are skipped until transcoding exists).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rtc::rtp::header::Header as RtpHeader;
use rtc::rtp::packet::Packet as RtpPacket;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::RwLock;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::{MediaStreamId, MediaStreamTrack, MediaStreamTrackId};
use webrtc::peer_connection::{
    PeerConnection, RTCIceServer, RTCIceTransportPolicy, RTCSessionDescription,
};

use super::session::{SessionRegistry, SessionState, SessionType, WebrtcSession};
use super::transport::{
    GATHER_TIMEOUT, PcHandler, PcState, build_peer_connection, sender_payload_type,
};
use rtflow_core::error::{Error, Result};
use rtflow_http::ResponseWriter;
use rtflow_media::Codec;
use rtflow_mount::MountRegistry;
use rtflow_rtp::Packetizer;

/// WHEP endpoint configuration.
#[derive(Clone, Debug)]
pub struct WhepConfig {
    /// ICE server URLs (STUN/TURN). Empty for LAN host-candidate only.
    pub ice_servers: Vec<String>,
    /// TURN username/credential.
    pub ice_username: String,
    pub ice_credential: String,
    /// `all` or `relay`.
    pub ice_transport_policy: String,
    /// Local UDP addresses to bind (e.g. `["0.0.0.0:0"]`).
    pub udp_addrs: Vec<String>,
    /// RTP packetization MTU (bytes, including headers).
    pub mtu: usize,
}

impl Default for WhepConfig {
    fn default() -> Self {
        Self {
            ice_servers: vec![],
            ice_username: String::new(),
            ice_credential: String::new(),
            ice_transport_policy: "all".into(),
            udp_addrs: vec!["0.0.0.0:0".to_string()],
            mtu: 1200,
        }
    }
}

/// Handle a WHEP POST: browser offer → server answer (201 + SDP).
pub async fn handle_whep_post(
    req: &rtflow_http::HttpRequest,
    writer: ResponseWriter<rtflow_http::ConnWriter>,
    body: &[u8],
    mounts: Arc<MountRegistry>,
    config: &WhepConfig,
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<rtflow_http::ConnWriter>> {
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
    // At least one WebRTC-playable track (video, or Opus/G.711/AAC audio)
    // must exist; audio-only mounts are valid (e.g. voice intercom).
    let playable = mount.description().tracks.iter().any(|t| match t.kind {
        rtflow_media::MediaKind::Video => true,
        rtflow_media::MediaKind::Audio => {
            matches!(
                t.codec,
                Codec::Opus | Codec::Pcma | Codec::Pcmu | Codec::Aac
            )
        }
    });
    if !playable {
        return respond_plain(writer, 404, "no playable track on mount").await;
    }

    let session = sessions
        .create_session(SessionType::Play, path.clone())
        .await?;

    // Build the peer connection with the shared handler.
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

    // Only add tracks whose codec the browser actually offered. This avoids
    // `add_track failed` for source-only codecs such as AAC/HEVC.
    let offered = offered_encodings(offer);
    let mut senders = Vec::new();
    for track in &mount.description().tracks {
        let (kind, codec) = match track.kind {
            rtflow_media::MediaKind::Video => (RtpCodecKind::Video, track.codec),
            rtflow_media::MediaKind::Audio
                if matches!(
                    track.codec,
                    Codec::Opus | Codec::Pcma | Codec::Pcmu | Codec::Aac
                ) =>
            {
                (RtpCodecKind::Audio, track.codec)
            }
            _ => continue,
        };
        if !matches!(
            codec,
            Codec::H264
                | Codec::H265
                | Codec::Vp8
                | Codec::Vp9
                | Codec::Opus
                | Codec::Pcma
                | Codec::Pcmu
                | Codec::Aac
        ) {
            tracing::warn!(
                ?codec,
                "WHEP: codec not supported by WebRTC, skipping track"
            );
            continue;
        }
        if !offered.contains(codec_encoding(codec).as_str()) {
            tracing::debug!(?codec, "WHEP: browser did not offer codec, skipping");
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
                    channels: if kind == RtpCodecKind::Audio {
                        if codec == Codec::Pcma || codec == Codec::Pcmu {
                            1
                        } else {
                            2
                        }
                    } else {
                        0
                    },
                    sdp_fmtp_line: codec_fmtp(codec, track.fmtp.as_deref()),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        )));
        match pc.add_track(media_track.clone()).await {
            Ok(sender) => senders.push((media_track, sender, kind, codec, track_ssrc, track.id)),
            Err(e) => {
                tracing::warn!("WHEP: add_track failed: {e}");
            }
        }
    }
    if senders.is_empty() {
        let _ = session.write().await.close().await;
        return respond_plain(writer, 406, "no offered WebRTC codec").await;
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
    for (track, sender, kind, codec, ssrc, track_id) in &senders {
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
            *track_id,
        )
        .await;
    }

    // Touch the session and answer.
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
                ("Location", &format!("/whep/{}", session.read().await.id)),
                ("Access-Control-Expose-Headers", "Location, ETag"),
                ("Access-Control-Allow-Origin", "*"),
                ("ETag", &format!("\"{}\"", session.read().await.id)),
                ("Cache-Control", "no-store"),
                ("Content-Type", "application/sdp"),
                ("Content-Length", &answer_len),
            ],
        )
        .await?;
    tracing::info!(mount = %path, "WHEP session established");
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

/// MIME type for a WebRTC-capable codec (H.264/H.265/VP8/VP9, Opus,
/// G.711 PCMA/PCMU, AAC-LC).
fn offered_encodings(sdp: &str) -> HashSet<String> {
    let mut offered = HashSet::new();
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("a=rtpmap:")
            && let Some((_pt, encoding)) = rest.split_once(' ')
            && let Some(name) = encoding.split('/').next()
        {
            offered.insert(name.to_ascii_uppercase());
        }
    }
    offered
}

fn codec_encoding(codec: Codec) -> String {
    match codec {
        Codec::H264 => "H264".into(),
        Codec::H265 => "H265".into(),
        Codec::Vp8 => "VP8".into(),
        Codec::Vp9 => "VP9".into(),
        Codec::Opus => "OPUS".into(),
        Codec::Pcma => "PCMA".into(),
        Codec::Pcmu => "PCMU".into(),
        Codec::Aac => "MP4A.40.2".into(),
        Codec::Av1 => "AV1".into(),
        _ => String::new(),
    }
}

fn codec_fmtp(codec: Codec, source_fmtp: Option<&str>) -> String {
    match codec {
        Codec::H264 => source_fmtp
            .filter(|fmtp| fmtp.contains("profile-level-id"))
            .map(str::to_owned)
            .unwrap_or_else(|| {
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_string()
            }),
        Codec::Opus => "minptime=10;useinbandfec=1".to_string(),
        _ => String::new(),
    }
}

fn mime_for(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "video/H264",
        Codec::H265 => "video/H265",
        Codec::Vp8 => "video/VP8",
        Codec::Vp9 => "video/VP9",
        Codec::Opus => "audio/opus",
        Codec::Pcma => "audio/PCMA",
        Codec::Pcmu => "audio/PCMU",
        Codec::Aac => "audio/mp4a.40.2",
        Codec::Mp3 | Codec::G726_16 | Codec::G726_24 | Codec::G726_32 | Codec::G726_40 => {
            "application/octet-stream"
        }
        Codec::Av1 => "video/AV1",
        Codec::Jpeg => "image/JPEG",
        Codec::H263 => "video/H263",
        Codec::Mpeg4 => "video/MP4V-ES",
        _ => "application/octet-stream",
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
    let candidates = pc_state.candidates.read().await;
    super::transport::append_candidates(&answer.sdp, &candidates)
}

/// Keep only a short, keyframe-aligned startup window. Replaying a full
/// cached GOP would add its whole duration to the first displayed frame.
fn trim_startup(frames: &mut Vec<Arc<rtflow_media::AccessUnit>>, codec: Codec, max: Duration) {
    let Some(last) = frames.iter().rfind(|frame| frame.codec == codec) else {
        return;
    };
    let cutoff = last.pts.saturating_sub(max);
    let Some(position) = frames
        .iter()
        .position(|frame| frame.codec == codec && frame.is_keyframe && frame.pts >= cutoff)
    else {
        frames.clear();
        return;
    };
    frames.drain(..position);
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
    track_id: u8,
) {
    let cancel = session.read().await.cancel.clone();
    let ssrc_value = ssrc.unwrap_or(0);

    let task_session = session.clone();
    let task = tokio::spawn(async move {
        let (mut rx, mut startup) = match mounts.get_handle(&mount_path) {
            Some(mount) => {
                let (rx, startup) = mount.subscribe_with_startup();
                if let Ok(publisher) = mount.publisher() {
                    publisher.request_keyframe(track_id);
                }
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
        if matches!(
            codec,
            rtflow_media::Codec::H264
                | rtflow_media::Codec::H265
                | rtflow_media::Codec::Vp8
                | rtflow_media::Codec::Vp9
                | rtflow_media::Codec::Av1
        ) {
            trim_startup(&mut startup, codec, Duration::from_millis(150));
        }
        let mut startup_iter = startup.into_iter();
        let mut awaiting_key = codec == rtflow_media::Codec::H264
            || codec == rtflow_media::Codec::H265
            || codec == rtflow_media::Codec::Vp8
            || codec == rtflow_media::Codec::Vp9
            || codec == rtflow_media::Codec::Av1;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                frame = async {
                    if let Some(unit) = startup_iter.next() {
                        Ok(unit)
                    } else {
                        rx.recv().await
                    }
                } => {
                    let unit = match frame {
                        Ok(unit) => unit,
                        Err(rtflow_mount::FrameRecvError::Lagged(count)) => {
                            tracing::debug!(count, "WHEP lagged; waiting for a keyframe");
                            awaiting_key = true;
                            if let Some(mount) = mounts.get_handle(&mount_path)
                                && let Ok(publisher) = mount.publisher()
                            {
                                publisher.request_keyframe(track_id);
                            }
                            continue;
                        }
                        Err(rtflow_mount::FrameRecvError::Closed) => break,
                    };

                    if unit.codec != codec {
                        continue;
                    }
                    if awaiting_key && unit.kind() == rtflow_media::MediaKind::Video {
                        if !unit.is_keyframe {
                            continue;
                        }
                        awaiting_key = false;
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

/// Handle WHEP PATCH: add trickled ICE candidates to a session.
pub async fn handle_whep_patch(
    req: &rtflow_http::HttpRequest,
    writer: ResponseWriter<rtflow_http::ConnWriter>,
    body: &[u8],
    sessions: Arc<SessionRegistry>,
) -> Result<ResponseWriter<rtflow_http::ConnWriter>> {
    let Some(id) = session_id_from_path(&req.path) else {
        return respond_plain(writer, 400, "missing session id").await;
    };
    if let Some(expected) = req.header("if-match")
        && expected.trim_matches('"') != id
    {
        return respond_plain(writer, 412, "etag mismatch").await;
    }
    let Some(session) = sessions.get_session(&id).await else {
        return respond_plain(writer, 404, "session not found").await;
    };
    let s = session.read().await;
    let Some(pc) = s.pc.clone() else {
        return respond_plain(writer, 409, "session has no peer connection").await;
    };
    drop(s);
    let candidates = super::transport::candidates_from_sdp(&String::from_utf8_lossy(body));
    for candidate in candidates {
        if let Err(e) = pc.add_ice_candidate(candidate).await {
            tracing::debug!("WHEP: PATCH add_ice_candidate failed: {e}");
        }
    }
    session.write().await.touch();
    let mut writer = writer;
    writer
        .write_headers(
            204,
            &[
                ("Access-Control-Allow-Origin", "*"),
                ("Content-Length", "0"),
                ("Cache-Control", "no-store"),
            ],
        )
        .await?;
    Ok(writer)
}

/// Handle WHEP DELETE: release the session.
pub async fn handle_whep_delete(
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

/// Handle WHEP OPTIONS (CORS preflight).
pub async fn handle_whep_options(
    _req: &rtflow_http::HttpRequest,
    writer: ResponseWriter<rtflow_http::ConnWriter>,
) -> Result<ResponseWriter<rtflow_http::ConnWriter>> {
    let mut writer = writer;
    writer
        .write_headers(
            204,
            &[
                ("Access-Control-Allow-Origin", "*"),
                (
                    "Access-Control-Allow-Methods",
                    "POST, PATCH, DELETE, OPTIONS",
                ),
                (
                    "Access-Control-Allow-Headers",
                    "Content-Type, If-Match, Authorization",
                ),
                ("Access-Control-Expose-Headers", "Location, ETag"),
                ("Access-Control-Max-Age", "86400"),
                ("Content-Length", "0"),
            ],
        )
        .await?;
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
