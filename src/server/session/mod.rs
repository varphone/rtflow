//! Per-connection RTSP state machine and media task ownership.
#[cfg(feature = "file-storage")]
use super::vod_runtime::{VodPlayback, spawn_vod_playback};
use super::{
    ConnectionPhase, State,
    proxy_runtime::start_proxy_if_idle,
    tasks::{
        MediaCrypto, SenderInput, spawn_interleaved_sender_from, spawn_udp_receiver,
        spawn_udp_sender_from,
    },
};
use crate::mount::{Mount, MountConfig, NackRequest, description_for};
use crate::{
    Error, Result,
    auth::{AccessRole, AuthDecision, AuthRequest},
    identity::MountId,
    media::MediaDescription,
    observe::{FrameworkEvent, SecurityEvent, emit},
    rtcp::Packet as RtcpPacket,
    rtp::{Depacketizer, RtpPacket},
    rtsp::range::{NptPosition, NptRange},
    rtsp::sdp,
    rtsp::{self, Message, Method, Response},
    runtime::{
        framing::{ConnectionFramer, InboundFrame},
        io::ControlConnection,
    },
    transport::{TransportSpec, UdpPair},
};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, Notify},
};

mod announce;
mod frames;
mod gate;
mod helpers;
mod pause;
mod play;
mod record;
mod setup;
mod teardown;

use self::announce::handle_announce;
use self::frames::handle_incoming_interleaved;
use self::gate::gate_request;
use self::helpers::*;
use self::pause::handle_pause;
use self::play::handle_play;
use self::record::handle_record;
use self::setup::handle_setup;
use self::teardown::handle_teardown;
use crate::server::timed_writer::TimedWriter;

/// Generate an unguessable wire-level session identifier (RFC 2326 §3.4).
fn new_session_token() -> String {
    static FALLBACK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // RNG failure must not panic a live RTSP connection: mix the clock
        // and a process-local counter as a best-effort fallback.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes();
        bytes[..16].copy_from_slice(&now[..16]);
        let counter = FALLBACK.fetch_add(1, Ordering::Relaxed);
        for (slot, byte) in bytes[..8].iter_mut().zip(counter.to_le_bytes()) {
            *slot ^= byte;
        }
    }
    let mut out = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub(super) async fn serve_connection(
    stream: ControlConnection,
    peer: SocketAddr,
    state: Arc<State>,
) -> Result<()> {
    let local_addr = stream.local_addr();
    #[cfg(feature = "tls")]
    let scheme = if state.config.tls.is_some() {
        "rtsps"
    } else {
        "rtsp"
    };
    #[cfg(not(feature = "tls"))]
    let scheme = "rtsp";
    let (mut reader, write_half) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(TimedWriter::new(write_half)));
    let mut framer = ConnectionFramer::new(
        state.config.max_rtsp_header_bytes,
        state.config.max_rtsp_body_bytes,
        state.config.resource_limits.max_interleaved_frame_bytes,
    );
    let mut scratch = [0u8; 4096];
    let mut setups: Vec<(
        Arc<Mount>,
        UdpPair,
        SocketAddr,
        SocketAddr,
        crate::media::TrackDescription,
    )> = Vec::new();
    let mut resources = ConnectionResources::default();
    let mut interleaved: Vec<(Arc<Mount>, u8, u8, crate::media::TrackDescription)> = Vec::new();
    let mut multicast_tracks: Vec<(Arc<Mount>, crate::media::TrackDescription)> = Vec::new();
    let mut interleaved_crypto: HashMap<u8, MediaCrypto> = HashMap::new();
    let session_id = new_session_token();
    let mut phase = ConnectionPhase::Init;
    let mut has_announced = false;
    let mut selected_mount: Option<String> = None;
    let mut interleaved_depacketizers: HashMap<u8, Depacketizer> = HashMap::new();
    let mut interleaved_ssrcs: HashMap<u8, u32> = HashMap::new();
    let mut vod_cursor = std::time::Duration::ZERO;
    let vod_end = Arc::new(Notify::new());
    let mut rtp_sequences: HashMap<u8, Arc<AtomicU16>> = HashMap::new();
    loop {
        let n = tokio::select! { _ = state.cancel.cancelled() => break, result = tokio::time::timeout(state.config.session_timeout, reader.read(&mut scratch)) => result.map_err(|_| Error::Closed)?? };
        if n == 0 {
            break;
        }
        framer.push(&scratch[..n]);
        while let Some(frame) = framer.next_frame()? {
            let msg = match frame {
                InboundFrame::Rtsp(message) => message,
                InboundFrame::Interleaved(frame) => {
                    handle_incoming_interleaved(
                        frame,
                        phase,
                        &interleaved,
                        &mut interleaved_depacketizers,
                        &mut interleaved_ssrcs,
                        &mut interleaved_crypto,
                    )
                    .await?;
                    continue;
                }
            };
            let Message::Request(req) = msg else {
                return Err(Error::Rtsp("client sent response".into()));
            };
            let Some(cseq) = req
                .headers
                .get("cseq")
                .filter(|value| value.parse::<u32>().is_ok())
                .cloned()
            else {
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(
                        400,
                        "Bad Request",
                        String::new(),
                    )))
                    .await?;
                continue;
            };
            if !gate_request(
                &req,
                peer,
                &state,
                &writer,
                &session_id,
                phase,
                has_announced,
                cseq.clone(),
            )
            .await?
            {
                continue;
            }
            if req.method == Method::Announce {
                handle_announce(&req, &state, &writer, &mut has_announced, cseq.clone()).await?;
                continue;
            }
            if req.method == Method::Setup {
                handle_setup(
                    &req,
                    &state,
                    &writer,
                    &session_id,
                    peer,
                    cseq.clone(),
                    &mut setups,
                    &mut interleaved,
                    &mut selected_mount,
                    &mut multicast_tracks,
                    &mut interleaved_crypto,
                    &mut phase,
                )
                .await?;
            } else if req.method == Method::Play {
                handle_play(
                    &req,
                    &state,
                    &writer,
                    &session_id,
                    cseq.clone(),
                    &setups,
                    &interleaved,
                    &multicast_tracks,
                    &mut resources,
                    &mut rtp_sequences,
                    &mut vod_cursor,
                    &mut phase,
                    &vod_end,
                )
                .await?;
            } else if req.method == Method::Record {
                handle_record(
                    &req,
                    &state,
                    &writer,
                    &session_id,
                    cseq.clone(),
                    &setups,
                    &interleaved,
                    &mut resources,
                    &mut interleaved_depacketizers,
                    &mut interleaved_ssrcs,
                    &mut vod_cursor,
                    &mut phase,
                )
                .await?;
            } else if req.method == Method::Pause {
                handle_pause(
                    &writer,
                    cseq.clone(),
                    &mut resources,
                    &mut interleaved_depacketizers,
                    &mut vod_cursor,
                    &mut phase,
                )
                .await?;
            } else if req.method == Method::Teardown {
                handle_teardown(
                    &writer,
                    cseq,
                    &mut resources,
                    &mut interleaved_depacketizers,
                )
                .await?;
                return Ok(());
            } else {
                let response = handle_request(req, &state, local_addr, scheme).await;
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&response))
                    .await?;
            }
        }
    }
    resources.cleanup();
    Ok(())
}
