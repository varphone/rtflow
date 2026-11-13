use super::{
    control::{ControlTransport, control_task, effective_keep_alive},
    emit_client_event,
    options::ClientOptions,
    session::{ClientSession, SessionParts},
    wire::{
        drain_jitter, receive_rtp_packet, request, response_session_id, response_session_timeout,
    },
};
use crate::{
    Error, Result,
    identity::SessionId,
    media::{AccessUnit, MediaDescription},
    observe::ClientEvent,
    rtcp::{Packet as RtcpPacket, RtcpSession},
    rtp::{Depacketizer, JitterBuffer, RtpPacket},
    rtsp::Response,
    runtime::framing::{ConnectionFramer, InboundFrame},
    runtime::io::ControlConnection,
    transport::{InterleavedFrame, TransportSpec, UdpPair},
};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, mpsc},
};
use tokio_util::sync::CancellationToken;

/// Parsed per-track `RTP-Info` entry from a PLAY response.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RtpInfo {
    pub sequence: Option<u16>,
    pub rtptime: Option<u32>,
}

/// Parse `RTP-Info` and pair entries with tracks by control URI, falling back
/// to declaration order when the server omits `url`.
pub(super) fn parse_rtp_info(
    response: &Response,
    description: &MediaDescription,
    setup_base: &str,
) -> HashMap<u8, RtpInfo> {
    let Some(value) = response.headers.get("rtp-info") else {
        return HashMap::new();
    };
    let mut entries = Vec::new();
    for entry in value.split(',') {
        let mut url = None;
        let mut sequence = None;
        let mut rtptime = None;
        for part in entry.split(';') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key.trim().to_ascii_lowercase().as_str() {
                "url" => url = Some(value.trim().to_owned()),
                "seq" => sequence = value.trim().parse().ok(),
                "rtptime" => rtptime = value.trim().parse().ok(),
                _ => {}
            }
        }
        if sequence.is_some() || rtptime.is_some() {
            entries.push((url, RtpInfo { sequence, rtptime }));
        }
    }
    let mut out = HashMap::new();
    for (index, (url, info)) in entries.into_iter().enumerate() {
        let track = url
            .as_deref()
            .and_then(|url| {
                description.tracks.iter().find(|track| {
                    crate::rtsp::sdp::control_uri(setup_base, &track.control)
                        .is_ok_and(|control| control == url)
                })
            })
            .or_else(|| description.tracks.get(index));
        if let Some(track) = track {
            out.insert(track.id, info);
        }
    }
    out
}

/// NPT base from the PLAY `Range` response (live `now` and absent ranges are 0).
pub(super) fn play_pts_base(response: &Response) -> std::time::Duration {
    response
        .headers
        .get("range")
        .and_then(|value| crate::rtsp::range::NptRange::parse(value).ok())
        .map(|range| match range.start {
            crate::rtsp::range::NptPosition::Time(value) => value,
            crate::rtsp::range::NptPosition::Now => std::time::Duration::ZERO,
        })
        .unwrap_or(std::time::Duration::ZERO)
}

/// A SETUP/PLAY-phase rejection: the negotiation failed at the RTSP status
/// level, but the control connection and the parsed SDP are still healthy,
/// so the caller can retry with a different transport without redoing
/// OPTIONS+DESCRIBE (audit RT-9).
pub(super) struct SetupFailure {
    pub(super) error: Error,
    pub(super) stream: ControlConnection,
    pub(super) setup_base: String,
    pub(super) description: MediaDescription,
}

pub(super) async fn connect(
    stream: ControlConnection,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    capabilities: crate::client::ServerCapabilities,
    options: ClientOptions,
    id: SessionId,
) -> std::result::Result<ClientSession, Box<SetupFailure>> {
    match options.transport {
        crate::transport::UpstreamTransport::Udp => {
            connect_udp(
                stream,
                uri,
                setup_base,
                description,
                capabilities,
                options,
                id,
            )
            .await
        }
        crate::transport::UpstreamTransport::TcpInterleaved => {
            connect_interleaved(
                stream,
                uri,
                setup_base,
                description,
                capabilities,
                options,
                id,
            )
            .await
        }
        crate::transport::UpstreamTransport::Auto => Err(Box::new(SetupFailure {
            error: Error::Rtsp(
                "automatic transport selection must be resolved before SETUP".into(),
            ),
            stream,
            setup_base,
            description,
        })),
    }
}

mod interleaved;
mod interleaved_rtcp;
#[cfg(feature = "multicast")]
mod multicast;
mod udp;

use super::queue::FrameSender;
use interleaved::connect_interleaved;
use udp::connect_udp;

/// Sleep until the jitter buffer's next release deadline. Never fires while
/// nothing is buffered, replacing the old 1–10ms poll tick (audit RT-17).
async fn wait_for_deadline(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

/// Non-blocking delivery into the frame channel: a full queue drops the
/// frame instead of stalling the receive loop (audit RT-17). Returns false
/// when the consumer is gone.
fn try_deliver(tx: &FrameSender, result: Result<AccessUnit>, dropped: &mut u64) -> bool {
    match tx.try_send(result) {
        Ok(outcome) => {
            *dropped = dropped.saturating_add(outcome.dropped as u64);
            if outcome.dropped > 0 && *dropped % 64 <= outcome.dropped as u64 {
                tracing::debug!(
                    dropped = *dropped,
                    evicted = outcome.dropped,
                    "client output queue evicted oldest frames"
                );
            }
            true
        }
        Err(()) => false,
    }
}

fn ssrc_process_seed() -> u32 {
    use std::sync::OnceLock;
    static SEED: OnceLock<u32> = OnceLock::new();
    *SEED.get_or_init(|| {
        let mut bytes = [0u8; 4];
        if getrandom::getrandom(&mut bytes).is_ok() {
            u32::from_le_bytes(bytes)
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u32
        }
    })
}

/// Per-session SSRC: unique enough for RTCP collision detection and never
/// shared between two concurrently connected clients.
pub(super) fn client_ssrc(session: crate::identity::SessionId, track_id: u8) -> u32 {
    let session = session.get() as u32;
    ssrc_process_seed()
        .wrapping_mul(0x9e37_79b9)
        .wrapping_add(session.rotate_left(13))
        ^ u32::from(track_id).wrapping_mul(0x85eb_ca6b)
}

fn client_cname(local_ssrc: u32) -> bytes::Bytes {
    bytes::Bytes::from(format!("rtflow@{local_ssrc:08x}"))
}

async fn observe_quality(
    quality: &Mutex<HashMap<u8, RtcpSession>>,
    track_id: u8,
    clock_rate: u32,
    local_ssrc: u32,
    packet: &RtpPacket,
) -> Result<()> {
    let now = tokio::time::Instant::now();
    let mut quality = quality.lock().await;
    let session = match quality.entry(track_id) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => entry.insert(RtcpSession::new(
            local_ssrc,
            packet.header.ssrc,
            clock_rate,
            client_cname(local_ssrc),
            std::time::Duration::from_secs(5),
            now,
        )?),
    };
    if session.on_rtp(packet, now).is_err() {
        *session = RtcpSession::new(
            local_ssrc,
            packet.header.ssrc,
            clock_rate,
            client_cname(local_ssrc),
            std::time::Duration::from_secs(5),
            now,
        )?;
        session.on_rtp(packet, now)?;
    }
    Ok(())
}

async fn quality_report(
    quality: &Mutex<HashMap<u8, RtcpSession>>,
    track_id: u8,
    clock_rate: u32,
    local_ssrc: u32,
    packets: &[RtcpPacket],
) -> Result<Option<crate::rtcp::CompoundPacket>> {
    let now = tokio::time::Instant::now();
    let mut quality = quality.lock().await;
    if !quality.contains_key(&track_id)
        && let Some(remote_ssrc) = packets.iter().find_map(|packet| match packet {
            RtcpPacket::SenderReport { ssrc, .. }
            | RtcpPacket::SenderReportWithReports { ssrc, .. } => Some(*ssrc),
            _ => None,
        })
    {
        quality.insert(
            track_id,
            RtcpSession::new(
                local_ssrc,
                remote_ssrc,
                clock_rate,
                client_cname(local_ssrc),
                std::time::Duration::from_secs(5),
                now,
            )?,
        );
    }
    let Some(session) = quality.get_mut(&track_id) else {
        return Ok(None);
    };
    for packet in packets {
        session.on_rtcp(packet, now);
    }
    session.report_now(now).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ssrc_is_deterministic_and_distinct_per_track() {
        let session = crate::identity::SessionId::new(7);
        for track_id in 0..=255u8 {
            assert_eq!(
                client_ssrc(session, track_id),
                client_ssrc(session, track_id)
            );
        }
        let first = client_ssrc(session, 0);
        assert!((1..=255u8).all(|track_id| client_ssrc(session, track_id) != first));
        assert_ne!(
            client_ssrc(session, 0),
            client_ssrc(crate::identity::SessionId::new(8), 0)
        );
    }
}
