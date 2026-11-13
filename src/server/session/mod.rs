//! Per-connection RTSP state machine and media task ownership.
#[cfg(feature = "file-storage")]
use super::vod_runtime::{VodPlayback, spawn_vod_playback};
use super::{
    ConnectionPhase, State,
    proxy_runtime::start_proxy_if_idle,
    tasks::{
        SenderInput, spawn_interleaved_sender_from, spawn_udp_receiver, spawn_udp_sender_from,
    },
};
use crate::mount::{Mount, MountConfig, description_for};
use crate::{
    Error, Result,
    auth::{AccessRole, AuthDecision, AuthRequest},
    identity::{MountId, SessionId},
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
    sync::Mutex,
};

mod announce;
mod frames;
mod gate;
mod helpers;

use self::announce::handle_announce;
use self::frames::handle_incoming_interleaved;
use self::gate::gate_request;
use self::helpers::*;

pub(super) async fn serve_connection(
    stream: ControlConnection,
    peer: SocketAddr,
    session_id: SessionId,
    state: Arc<State>,
) -> Result<()> {
    let (mut reader, write_half) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(write_half));
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
    let session_id = session_id.to_string();
    let mut phase = ConnectionPhase::Init;
    let mut has_announced = false;
    let mut selected_mount: Option<String> = None;
    let mut interleaved_depacketizers: HashMap<u8, Depacketizer> = HashMap::new();
    let mut interleaved_ssrcs: HashMap<u8, u32> = HashMap::new();
    let mut vod_cursor = std::time::Duration::ZERO;
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
                if setups.len() + interleaved.len()
                    >= state.config.resource_limits.max_tracks_per_session
                {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            453,
                            "Not Enough Bandwidth",
                            cseq,
                        )))
                        .await?;
                    continue;
                }
                let Some((path, mount)) = registered_mount_for_uri(&state, &req.uri).await else {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            404,
                            "Not Found",
                            cseq,
                        )))
                        .await?;
                    continue;
                };
                if selected_mount
                    .as_ref()
                    .is_some_and(|selected| selected != &path)
                {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            459,
                            "Aggregate Operation Not Allowed",
                            cseq,
                        )))
                        .await?;
                    continue;
                }
                let Some(raw) = req.headers.get("transport") else {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            461,
                            "Unsupported Transport",
                            cseq,
                        )))
                        .await?;
                    continue;
                };
                let transport = TransportSpec::parse_all(raw).ok().and_then(|alternatives| {
                    alternatives.into_iter().find(|candidate| match candidate {
                        TransportSpec::TcpInterleaved {
                            rtp_channel,
                            rtcp_channel,
                        } => {
                            rtp_channel != rtcp_channel
                                && !interleaved.iter().any(|(_, current_rtp, current_rtcp, _)| {
                                    [*current_rtp, *current_rtcp].into_iter().any(|current| {
                                        current == *rtp_channel || current == *rtcp_channel
                                    })
                                })
                        }
                        TransportSpec::Udp { .. } => true,
                    })
                });
                let Some(transport) = transport else {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            461,
                            "Unsupported Transport",
                            cseq,
                        )))
                        .await?;
                    continue;
                };
                if let TransportSpec::TcpInterleaved {
                    rtp_channel,
                    rtcp_channel,
                } = transport
                {
                    let Ok(track) = track_for_uri(&description_for(&mount.config)?, &req.uri)
                    else {
                        // Unknown control URI: reply 404 instead of dropping
                        // the connection (strict clients treat a dropped
                        // connection as a network failure).
                        writer
                            .lock()
                            .await
                            .write_all(&rtsp::encode_response(&simple_response(
                                404,
                                "Not Found",
                                cseq,
                            )))
                            .await?;
                        continue;
                    };
                    let proxy_mount = mount.clone();
                    interleaved.push((mount, rtp_channel, rtcp_channel, track));
                    selected_mount.get_or_insert(path.clone());
                    phase = ConnectionPhase::Ready;
                    let mut response = simple_response(200, "OK", cseq);
                    response.headers.insert(
                        "session".into(),
                        session_header(&session_id, state.config.session_timeout),
                    );
                    response.headers.insert(
                        "transport".into(),
                        TransportSpec::TcpInterleaved {
                            rtp_channel,
                            rtcp_channel,
                        }
                        .render(),
                    );
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&response))
                        .await?;
                    // Warm the proxy during SETUP so PLAY's first frame does
                    // not wait for a full control round trip.
                    start_proxy_if_idle(
                        proxy_mount,
                        state.cancel.clone(),
                        state.supervisor.clone(),
                        state.config.no_subscriber_grace,
                    );
                    continue;
                }
                let TransportSpec::Udp {
                    client_rtp,
                    client_rtcp,
                    ..
                } = transport
                else {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            461,
                            "Unsupported Transport",
                            cseq,
                        )))
                        .await?;
                    continue;
                };
                let bind_ip = match peer.ip() {
                    std::net::IpAddr::V4(_) => std::net::Ipv4Addr::UNSPECIFIED.into(),
                    std::net::IpAddr::V6(_) => std::net::Ipv6Addr::UNSPECIFIED.into(),
                };
                let pair = UdpPair::bind(bind_ip, state.config.udp_port_range.clone()).await?;
                let server_rtp = pair.rtp_port()?;
                let server_rtcp = pair.rtcp_port()?;
                let rtp_target = SocketAddr::new(peer.ip(), client_rtp);
                let rtcp_target = SocketAddr::new(peer.ip(), client_rtcp);
                let Ok(track) = track_for_uri(&description_for(&mount.config)?, &req.uri) else {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            404,
                            "Not Found",
                            cseq,
                        )))
                        .await?;
                    continue;
                };
                let mut response = simple_response(200, "OK", cseq);
                response.headers.insert(
                    "session".into(),
                    session_header(&session_id, state.config.session_timeout),
                );
                response.headers.insert(
                    "transport".into(),
                    TransportSpec::Udp {
                        client_rtp,
                        client_rtcp,
                        server_rtp: Some(server_rtp),
                        server_rtcp: Some(server_rtcp),
                    }
                    .render(),
                );
                let proxy_mount = mount.clone();
                setups.push((mount, pair, rtp_target, rtcp_target, track));
                selected_mount.get_or_insert(path);
                phase = ConnectionPhase::Ready;
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&response))
                    .await?;
                // Warm the proxy during SETUP so PLAY's first frame does not
                // wait for a full OPTIONS+DESCRIBE+SETUP+PLAY round trip.
                start_proxy_if_idle(
                    proxy_mount,
                    state.cancel.clone(),
                    state.supervisor.clone(),
                    state.config.no_subscriber_grace,
                );
            } else if req.method == Method::Play {
                #[cfg(feature = "file-storage")]
                let vod_source = interleaved
                    .iter()
                    .map(|(mount, ..)| mount)
                    .chain(setups.iter().map(|(mount, ..)| mount))
                    .find_map(|mount| match &mount.config {
                        MountConfig::Vod { source } => Some(source.clone()),
                        _ => None,
                    });
                let requested_range = match req.headers.get("range") {
                    Some(value) => match NptRange::parse(value) {
                        Ok(range) => Some(range),
                        Err(_) => {
                            writer
                                .lock()
                                .await
                                .write_all(&rtsp::encode_response(&simple_response(
                                    457,
                                    "Invalid Range",
                                    cseq,
                                )))
                                .await?;
                            continue;
                        }
                    },
                    None => None,
                };
                let live_range_supported = requested_range.is_none_or(|range| {
                    matches!(range.start, NptPosition::Now)
                        || matches!(range.start, NptPosition::Time(position) if position == std::time::Duration::ZERO)
                }) && requested_range.is_none_or(|range| range.end.is_none());
                if vod_source.is_none() && !live_range_supported {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            457,
                            "Invalid Range",
                            cseq,
                        )))
                        .await?;
                    continue;
                }
                for task in resources.senders.drain(..) {
                    task.abort();
                    // Wait for the old sender to actually stop before a new
                    // one starts, so the two never interleave on the wire.
                    let _ = task.await;
                }
                if let Some(position) = resources.stop_vod() {
                    vod_cursor = position;
                }
                let start = requested_range
                    .map(|range| match range.start {
                        NptPosition::Time(position) => position,
                        NptPosition::Now => vod_cursor,
                    })
                    .unwrap_or(vod_cursor);
                let end = requested_range.and_then(|range| range.end);
                let vod_output = vod_source.as_ref().map(|_| {
                    tokio::sync::broadcast::channel(state.config.consumer_queue_capacity).0
                });
                let mut live_interleaved_inputs = Vec::new();
                let mut live_udp_inputs = Vec::new();
                let mut rtp_info = Vec::new();
                if vod_output.is_none() {
                    for (mount, _, _, track) in &interleaved {
                        let sequence = rtp_sequences
                            .entry(track.id)
                            .or_insert_with(|| Arc::new(AtomicU16::new(0)))
                            .clone();
                        let input = live_sender_input(mount, track.id, sequence);
                        if let Some(entry) = rtp_info_entry(mount, &req.uri, track, &input) {
                            rtp_info.push(entry);
                        }
                        live_interleaved_inputs.push(input);
                    }
                    for (mount, _, _, _, track) in &setups {
                        let sequence = rtp_sequences
                            .entry(track.id)
                            .or_insert_with(|| Arc::new(AtomicU16::new(0)))
                            .clone();
                        let input = live_sender_input(mount, track.id, sequence);
                        if let Some(entry) = rtp_info_entry(mount, &req.uri, track, &input) {
                            rtp_info.push(entry);
                        }
                        live_udp_inputs.push(input);
                    }
                }
                if interleaved.is_empty() && setups.is_empty() {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            455,
                            "Method Not Valid in This State",
                            cseq,
                        )))
                        .await?;
                } else {
                    phase = ConnectionPhase::Playing;
                    resources.register_play(
                        interleaved
                            .iter()
                            .map(|(mount, _, _, _)| mount)
                            .chain(setups.iter().map(|(mount, _, _, _, _)| mount)),
                    );
                    let mut response = simple_response(200, "OK", cseq);
                    response.headers.insert(
                        "session".into(),
                        session_header(&session_id, state.config.session_timeout),
                    );
                    if vod_source.is_some() {
                        response.headers.insert(
                            "range".into(),
                            NptRange {
                                start: NptPosition::Time(start),
                                end,
                            }
                            .render(),
                        );
                    } else {
                        // A live presentation has no finite endpoint, but PLAY still
                        // confirms the effective Normal Play Time range. This lets
                        // RTSP clients distinguish a live stream from a response
                        // that omitted range semantics altogether.
                        response.headers.insert(
                            "range".into(),
                            requested_range
                                .unwrap_or(NptRange {
                                    start: NptPosition::Now,
                                    end: None,
                                })
                                .render(),
                        );
                    }
                    if !rtp_info.is_empty() {
                        response
                            .headers
                            .insert("rtp-info".into(), rtp_info.join(","));
                    }
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&response))
                        .await?;
                    for (mount, rtp_channel, rtcp_channel, track) in interleaved.iter().cloned() {
                        let sequence = rtp_sequences
                            .entry(track.id)
                            .or_insert_with(|| Arc::new(AtomicU16::new(0)))
                            .clone();
                        resources.senders.push(if let Some(output) = &vod_output {
                            spawn_interleaved_sender_from(
                                mount.clone(),
                                SenderInput {
                                    receiver: output.subscribe(),
                                    startup: Vec::new(),
                                    sequence,
                                },
                                rtp_channel,
                                rtcp_channel,
                                track,
                                writer.clone(),
                            )
                        } else {
                            spawn_interleaved_sender_from(
                                mount.clone(),
                                live_interleaved_inputs.remove(0),
                                rtp_channel,
                                rtcp_channel,
                                track,
                                writer.clone(),
                            )
                        });
                        start_proxy_if_idle(
                            mount,
                            state.cancel.clone(),
                            state.supervisor.clone(),
                            state.config.no_subscriber_grace,
                        );
                    }
                    for (mount, socket, rtp_target, rtcp_target, track) in setups.iter().cloned() {
                        let sequence = rtp_sequences
                            .entry(track.id)
                            .or_insert_with(|| Arc::new(AtomicU16::new(0)))
                            .clone();
                        resources.senders.push(if let Some(output) = &vod_output {
                            spawn_udp_sender_from(
                                mount.clone(),
                                SenderInput {
                                    receiver: output.subscribe(),
                                    startup: Vec::new(),
                                    sequence,
                                },
                                socket,
                                rtp_target,
                                rtcp_target,
                                track,
                            )
                        } else {
                            spawn_udp_sender_from(
                                mount.clone(),
                                live_udp_inputs.remove(0),
                                socket,
                                rtp_target,
                                rtcp_target,
                                track,
                            )
                        });
                        start_proxy_if_idle(
                            mount,
                            state.cancel.clone(),
                            state.supervisor.clone(),
                            state.config.no_subscriber_grace,
                        );
                    }
                    if let (Some(source), Some(output)) = (vod_source, vod_output) {
                        resources.vod = Some(spawn_vod_playback(
                            &state.supervisor,
                            source,
                            output,
                            start,
                            end,
                            state.config.consumer_queue_capacity,
                        )?);
                    }
                }
            } else if req.method == Method::Record {
                let candidates = setups
                    .iter()
                    .map(|(mount, ..)| mount.clone())
                    .chain(interleaved.iter().map(|(mount, ..)| mount.clone()))
                    .collect::<Vec<_>>();
                match claim_remote_publishers(candidates) {
                    Ok(claims) => resources.claims = claims,
                    Err(_) => {
                        writer
                            .lock()
                            .await
                            .write_all(&rtsp::encode_response(&simple_response(
                                409, "Conflict", cseq,
                            )))
                            .await?;
                        continue;
                    }
                }
                for task in resources.senders.drain(..) {
                    task.abort();
                    // Wait for the old sender to actually stop before a new
                    // one starts, so the two never interleave on the wire.
                    let _ = task.await;
                }
                if let Some(position) = resources.stop_vod() {
                    vod_cursor = position;
                }
                for (mount, socket, _rtp_target, _rtcp_target, track) in setups.iter().cloned() {
                    if matches!(mount.config, MountConfig::Publisher { .. }) {
                        resources.senders.push(spawn_udp_receiver(
                            mount,
                            socket,
                            _rtp_target,
                            track,
                        ));
                    }
                }
                let mut has_interleaved_receiver = false;
                interleaved_ssrcs.clear();
                for (mount, rtp_channel, _rtcp_channel, track) in interleaved.iter().cloned() {
                    if matches!(mount.config, MountConfig::Publisher { .. }) {
                        has_interleaved_receiver = true;
                        interleaved_depacketizers.insert(
                            rtp_channel,
                            Depacketizer::new(track.codec, track.id, track.clock_rate),
                        );
                        if !resources
                            .recording_mounts
                            .iter()
                            .any(|current| Arc::ptr_eq(current, &mount))
                        {
                            mount.publishers.fetch_add(1, Ordering::AcqRel);
                            resources.recording_mounts.push(mount);
                        }
                    }
                }
                if resources.senders.is_empty() && !has_interleaved_receiver {
                    release_remote_publishers(&mut resources.claims, true);
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            455,
                            "Method Not Valid in This State",
                            cseq,
                        )))
                        .await?;
                } else {
                    phase = ConnectionPhase::Recording;
                    let mut response = simple_response(200, "OK", cseq);
                    response.headers.insert(
                        "session".into(),
                        session_header(&session_id, state.config.session_timeout),
                    );
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&response))
                        .await?;
                }
            } else if req.method == Method::Pause {
                phase = ConnectionPhase::Paused;
                release_recording_mounts(&mut resources.recording_mounts);
                // Release publisher claims too: a paused session is not
                // publishing, and keeping the claim would make a later
                // RECORD on the same session (or another) fail with 409.
                release_remote_publishers(&mut resources.claims, true);
                interleaved_depacketizers.clear();
                for task in resources.senders.drain(..) {
                    task.abort();
                    // Wait for the old sender to actually stop before a new
                    // one starts, so the two never interleave on the wire.
                    let _ = task.await;
                }
                if let Some(position) = resources.stop_vod() {
                    vod_cursor = position;
                }
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(200, "OK", cseq)))
                    .await?;
            } else if req.method == Method::Teardown {
                release_recording_mounts(&mut resources.recording_mounts);
                release_remote_publishers(&mut resources.claims, true);
                interleaved_depacketizers.clear();
                for task in resources.senders.drain(..) {
                    task.abort();
                    // Wait for the old sender to actually stop before a new
                    // one starts, so the two never interleave on the wire.
                    let _ = task.await;
                }
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(200, "OK", cseq)))
                    .await?;
                return Ok(());
            } else {
                let response = handle_request(req, &state).await;
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
