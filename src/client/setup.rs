use super::{
    ClientOptions, ClientSession, ControlTransport, SessionParts, control_task, drain_jitter,
    effective_keep_alive, emit_client_event, receive_rtp_packet, request, response_session_id,
    response_session_timeout,
};
use crate::{
    Error, Result,
    identity::SessionId,
    media::{AccessUnit, MediaDescription},
    observe::ClientEvent,
    rtcp::{Packet as RtcpPacket, RtcpSession},
    rtp::{Depacketizer, JitterBuffer, RtpPacket},
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
    options: ClientOptions,
    id: SessionId,
) -> std::result::Result<ClientSession, SetupFailure> {
    match options.transport {
        crate::transport::UpstreamTransport::Udp => {
            connect_udp(stream, uri, setup_base, description, options, id).await
        }
        crate::transport::UpstreamTransport::TcpInterleaved => {
            connect_interleaved(stream, uri, setup_base, description, options, id).await
        }
        crate::transport::UpstreamTransport::Auto => Err(SetupFailure {
            error: Error::Rtsp(
                "automatic transport selection must be resolved before SETUP".into(),
            ),
            stream,
            setup_base,
            description,
        }),
    }
}

async fn connect_interleaved(
    mut stream: ControlConnection,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    options: ClientOptions,
    id: SessionId,
) -> std::result::Result<ClientSession, SetupFailure> {
    let mut cseq = 3u32;
    let mut session_id = None;
    let mut session_timeout = None;
    let mut tracks = HashMap::new();
    let mut rtcp_channels = HashMap::new();
    let track_list: Vec<_> = description.tracks.to_vec();
    for (index, track) in track_list.iter().cloned().enumerate() {
        // Two interleaved channels per track; more than 127 tracks would
        // overflow the 8-bit channel space — refuse instead of wrapping.
        let channel = match u8::try_from(index * 2) {
            Ok(channel) => channel,
            Err(_) => {
                return Err(SetupFailure {
                    error: Error::Limit("too many tracks for interleaved transport"),
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        let setup_uri = match crate::protocol::sdp::control_uri(&setup_base, &track.control) {
            Ok(uri) => uri,
            Err(error) => {
                return Err(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        let mut extra = format!(
            "Transport: RTP/AVP/TCP;unicast;interleaved={}-{}\r\n",
            channel,
            channel.saturating_add(1)
        );
        if let Some(session) = &session_id {
            extra.push_str(&format!("Session: {session}\r\n"));
        }
        let response = match request(
            &mut stream,
            "SETUP",
            &setup_uri,
            cseq,
            Some(&extra),
            options.request_timeout,
            options.credentials.as_ref(),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                return Err(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        cseq = cseq.wrapping_add(1);
        if response.status != 200 {
            return Err(SetupFailure {
                error: Error::RtspStatus {
                    method: "SETUP",
                    status: response.status,
                },
                stream,
                setup_base,
                description,
            });
        }
        let received = match response_session_id(&response) {
            Ok(value) => value,
            Err(error) => {
                return Err(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        if session_id.as_ref().is_some_and(|value| value != &received) {
            return Err(SetupFailure {
                error: Error::Rtsp("server changed Session during SETUP".into()),
                stream,
                setup_base,
                description,
            });
        }
        session_id = Some(received);
        if session_timeout.is_none() {
            session_timeout = response_session_timeout(&response);
        }
        tracks.insert(
            channel,
            (
                JitterBuffer::new(options.jitter.clone()),
                Depacketizer::new(track.codec, track.id, track.clock_rate),
                track.id,
                track.clock_rate,
                client_ssrc(track.id),
            ),
        );
        rtcp_channels.insert(channel.saturating_add(1), channel);
    }
    let Some(session_id) = session_id else {
        return Err(SetupFailure {
            error: Error::Rtsp("SDP has no tracks".into()),
            stream,
            setup_base,
            description,
        });
    };
    let mut extra = format!("Session: {session_id}\r\n");
    if let Some(range) = options.play_range {
        extra.push_str(&format!("Range: {}\r\n", range.render()));
    }
    let response = match request(
        &mut stream,
        "PLAY",
        &uri,
        cseq,
        Some(&extra),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            return Err(SetupFailure {
                error,
                stream,
                setup_base,
                description,
            });
        }
    };
    cseq = cseq.wrapping_add(1);
    if response.status != 200 {
        return Err(SetupFailure {
            error: Error::RtspStatus {
                method: "PLAY",
                status: response.status,
            },
            stream,
            setup_base,
            description,
        });
    }
    let peer = stream.peer_addr();
    let (mut reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(writer));
    let rtcp_writer = writer.clone();
    let (tx, rx) = mpsc::channel(options.output_queue_capacity);
    let (response_tx, response_rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let reader_cancel = cancel.clone();
    let quality = Arc::new(Mutex::new(HashMap::new()));
    let reader_quality = quality.clone();
    let media_for_control = tx.clone();
    let reader_task = tokio::spawn(async move {
        let mut framer = ConnectionFramer::new(16 * 1024, 256 * 1024, 65_536);
        let mut scratch = [0u8; 4096];
        let mut dropped = 0u64;
        loop {
            // Event-driven jitter release (audit RT-17): arm a timer for the
            // earliest buffered deadline instead of polling every 1-10ms.
            let deadline = tracks
                .values()
                .filter_map(|(jitter, ..)| jitter.next_deadline())
                .min();
            let read = tokio::select! {
                _ = reader_cancel.cancelled() => return,
                _ = wait_for_deadline(deadline) => {
                    for (jitter, depacketizer, ..) in tracks.values_mut() {
                        for result in drain_jitter(jitter, depacketizer) {
                            if !try_deliver(&tx, result, &mut dropped) {
                                return;
                            }
                        }
                    }
                    continue;
                }
                result = reader.read(&mut scratch) => result,
            };
            match read {
                Ok(0) => break,
                Ok(n) => {
                    framer.push(&scratch[..n]);
                    loop {
                        match framer.next_frame() {
                            Ok(Some(InboundFrame::Interleaved(frame))) => {
                                if let Some((
                                    jitter,
                                    depacketizer,
                                    track_id,
                                    clock_rate,
                                    local_ssrc,
                                )) = tracks.get_mut(&frame.channel)
                                {
                                    let packet = match RtpPacket::parse(frame.payload) {
                                        Ok(packet) => packet,
                                        Err(error) => {
                                            if !try_deliver(&tx, Err(error), &mut dropped) {
                                                return;
                                            }
                                            continue;
                                        }
                                    };
                                    if let Err(error) = observe_quality(
                                        &reader_quality,
                                        *track_id,
                                        *clock_rate,
                                        *local_ssrc,
                                        &packet,
                                    )
                                    .await
                                    {
                                        if !try_deliver(&tx, Err(error), &mut dropped) {
                                            return;
                                        }
                                        continue;
                                    }
                                    for result in receive_rtp_packet(packet, jitter, depacketizer) {
                                        if !try_deliver(&tx, result, &mut dropped) {
                                            return;
                                        }
                                    }
                                } else if let Some(rtp_channel) = rtcp_channels.get(&frame.channel)
                                    && let Ok(packets) = RtcpPacket::parse(frame.payload)
                                    && let Some((_, _, track_id, clock_rate, local_ssrc)) =
                                        tracks.get(rtp_channel)
                                    && let Ok(Some(report)) = quality_report(
                                        &reader_quality,
                                        *track_id,
                                        *clock_rate,
                                        *local_ssrc,
                                        &packets,
                                    )
                                    .await
                                    && let Ok(payload) = report.try_encode()
                                {
                                    let Ok(wire) = (InterleavedFrame {
                                        channel: frame.channel,
                                        payload,
                                    })
                                    .try_encode() else {
                                        return;
                                    };
                                    if rtcp_writer.lock().await.write_all(&wire).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Ok(Some(InboundFrame::Rtsp(crate::rtsp::Message::Response(
                                response,
                            )))) => {
                                if response_tx.send(response).await.is_err() {
                                    return;
                                }
                            }
                            Ok(Some(InboundFrame::Rtsp(_))) => continue,
                            Ok(None) => break,
                            Err(error) => {
                                let _ = tx.send(Err(error)).await;
                                return;
                            }
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(error.into())).await;
                    break;
                }
            }
        }
        // The interleaved connection carries media and control on one socket:
        // EOF or an IO error ends the session. Cancel the shared token so the
        // control task stops and drops its frame sender; otherwise the output
        // channel stays open and `next()` (and the reconnect loop) would block
        // until a keep-alive failure minutes later.
        reader_cancel.cancel();
    });
    let (control_tx, control_rx) = mpsc::channel(16);
    let control_task_handle = tokio::spawn(control_task(
        ControlTransport::Interleaved {
            writer,
            responses: response_rx,
        },
        control_rx,
        media_for_control,
        uri,
        session_id,
        cseq,
        options.credentials.clone(),
        options.request_timeout,
        effective_keep_alive(options.keep_alive_interval, session_timeout),
        id,
        options.event_sink.clone(),
        cancel.clone(),
    ));
    let event_sink = options.event_sink.clone();
    emit_client_event(
        &event_sink,
        ClientEvent::Connected {
            id,
            transport: "tcp-interleaved",
            peer,
        },
    );
    tracing::debug!(session_id = %id, %peer, transport = "tcp-interleaved", "RTSP client session established");
    Ok(ClientSession::new(SessionParts {
        description,
        id,
        receiver: rx,
        control_tx: Some(control_tx),
        request_timeout: options.request_timeout,
        cancel,
        tasks: vec![reader_task, control_task_handle],
        quality,
    }))
}

async fn connect_udp(
    mut stream: ControlConnection,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    options: ClientOptions,
    id: SessionId,
) -> std::result::Result<ClientSession, SetupFailure> {
    let mut cseq = 3u32;
    let mut session_id = None;
    let mut session_timeout = None;
    let mut receivers = Vec::new();
    let track_list: Vec<_> = description.tracks.to_vec();
    for track in track_list {
        let bind_ip = match stream.local_addr().ip() {
            std::net::IpAddr::V4(_) => std::net::Ipv4Addr::UNSPECIFIED.into(),
            std::net::IpAddr::V6(_) => std::net::Ipv6Addr::UNSPECIFIED.into(),
        };
        let bind_result = match options.udp_recv_buffer {
            Some(recv_buffer) => UdpPair::bind_with_recv_buffer(bind_ip, None, recv_buffer).await,
            None => UdpPair::bind(bind_ip, None).await,
        };
        let pair = match bind_result {
            Ok(pair) => pair,
            Err(error) => {
                return Err(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        let port = match pair.rtp_port() {
            Ok(port) => port,
            Err(error) => {
                return Err(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        let setup_uri = match crate::protocol::sdp::control_uri(&setup_base, &track.control) {
            Ok(uri) => uri,
            Err(error) => {
                return Err(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        let mut extra = format!(
            "Transport: RTP/AVP;unicast;client_port={}-{}\r\n",
            port,
            port.saturating_add(1)
        );
        if let Some(session) = &session_id {
            extra.push_str(&format!("Session: {session}\r\n"));
        }
        let response = match request(
            &mut stream,
            "SETUP",
            &setup_uri,
            cseq,
            Some(&extra),
            options.request_timeout,
            options.credentials.as_ref(),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                return Err(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        cseq = cseq.wrapping_add(1);
        if response.status != 200 {
            return Err(SetupFailure {
                error: Error::RtspStatus {
                    method: "SETUP",
                    status: response.status,
                },
                stream,
                setup_base,
                description,
            });
        }
        let received = match response_session_id(&response) {
            Ok(value) => value,
            Err(error) => {
                return Err(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        if session_id.as_ref().is_some_and(|value| value != &received) {
            return Err(SetupFailure {
                error: Error::Rtsp("server changed Session during SETUP".into()),
                stream,
                setup_base,
                description,
            });
        }
        session_id = Some(received);
        if session_timeout.is_none() {
            session_timeout = response_session_timeout(&response);
        }
        let transport_spec = match response
            .headers
            .get("transport")
            .map(String::as_str)
            .map(TransportSpec::parse)
            .transpose()
        {
            Ok(spec) => spec,
            Err(error) => {
                return Err(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        let server_rtp = match &transport_spec {
            Some(TransportSpec::Udp {
                server_rtp: Some(port),
                ..
            }) => *port,
            _ => {
                return Err(SetupFailure {
                    error: Error::Rtsp("SETUP response has no server RTP port".into()),
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        let server_rtcp = match &transport_spec {
            Some(TransportSpec::Udp {
                server_rtcp: Some(port),
                ..
            }) => *port,
            _ => {
                return Err(SetupFailure {
                    error: Error::Rtsp("SETUP response has no server RTCP port".into()),
                    stream,
                    setup_base,
                    description,
                });
            }
        };
        let peer_ip = stream.peer_addr().ip();
        let rtp_target = std::net::SocketAddr::new(peer_ip, server_rtp);
        let rtcp_target = std::net::SocketAddr::new(peer_ip, server_rtcp);
        receivers.push((pair, track, rtp_target, rtcp_target));
    }
    let Some(session_id) = session_id else {
        return Err(SetupFailure {
            error: Error::Rtsp("SDP has no tracks".into()),
            stream,
            setup_base,
            description,
        });
    };
    let mut extra = format!("Session: {session_id}\r\n");
    if let Some(range) = options.play_range {
        extra.push_str(&format!("Range: {}\r\n", range.render()));
    }
    let response = match request(
        &mut stream,
        "PLAY",
        &uri,
        cseq,
        Some(&extra),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            return Err(SetupFailure {
                error,
                stream,
                setup_base,
                description,
            });
        }
    };
    cseq = cseq.wrapping_add(1);
    if response.status != 200 {
        return Err(SetupFailure {
            error: Error::RtspStatus {
                method: "PLAY",
                status: response.status,
            },
            stream,
            setup_base,
            description,
        });
    }
    let (tx, rx) = mpsc::channel(options.output_queue_capacity);
    let cancel = CancellationToken::new();
    let quality = Arc::new(Mutex::new(HashMap::new()));
    let mut tasks = Vec::new();
    for (pair, track, rtp_target, rtcp_target) in receivers {
        let tx = tx.clone();
        let jitter_config = options.jitter.clone();
        let rtcp_pair = pair.clone();
        let rtp_cancel = cancel.clone();
        let rtp_quality = quality.clone();
        let track_id = track.id;
        let clock_rate = track.clock_rate;
        let local_ssrc = client_ssrc(track_id);
        tasks.push(tokio::spawn(async move {
            let mut jitter = JitterBuffer::new(jitter_config);
            // Server SSRC is not signaled in SETUP; anchor on the first
            // packet and reject any later SSRC change (RFC 3550 conflict
            // handling — a restarted server reconnects instead of mixing).
            let mut expected_ssrc: Option<u32> = None;
            let mut depacketizer = Depacketizer::new(track.codec, track.id, track.clock_rate);
            let mut buffer = bytes::BytesMut::zeroed(65_536);
            let mut dropped = 0u64;
            loop {
                // Event-driven jitter release (audit RT-17): sleep until the
                // buffered deadline instead of polling every 1-10ms.
                let deadline = jitter.next_deadline();
                let received = tokio::select! {
                    _ = rtp_cancel.cancelled() => return,
                    _ = wait_for_deadline(deadline) => {
                        for result in drain_jitter(&mut jitter, &mut depacketizer) {
                            if !try_deliver(&tx, result, &mut dropped) {
                                return;
                            }
                        }
                        continue;
                    }
                    result = pair.rtp.recv_from(&mut buffer) => result,
                };
                match received {
                    Ok((size, source)) => {
                        // Reject packets from any address other than the
                        // negotiated server RTP port: RTSP/RTCP has no
                        // built-in authenticity, so an on-path attacker
                        // must not be able to inject media.
                        if source != rtp_target {
                            continue;
                        }
                        let payload = buffer.split_to(size).freeze();
                        buffer.resize(65_536, 0);
                        let packet = match RtpPacket::parse(payload) {
                            Ok(packet) => packet,
                            Err(error) => {
                                if !try_deliver(&tx, Err(error), &mut dropped) {
                                    return;
                                }
                                continue;
                            }
                        };
                        // Reject mismatched payload type or SSRC: they must
                        // match the negotiated track (a server restart with
                        // a new SSRC triggers a reconnect, not silent mix).
                        if packet.header.payload_type != track.payload_type {
                            continue;
                        }
                        match expected_ssrc {
                            Some(expected) if packet.header.ssrc != expected => continue,
                            None => expected_ssrc = Some(packet.header.ssrc),
                            _ => {}
                        }
                        if let Err(error) =
                            observe_quality(&rtp_quality, track_id, clock_rate, local_ssrc, &packet)
                                .await
                        {
                            if !try_deliver(&tx, Err(error), &mut dropped) {
                                return;
                            }
                            continue;
                        }
                        for result in receive_rtp_packet(packet, &mut jitter, &mut depacketizer) {
                            if !try_deliver(&tx, result, &mut dropped) {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error.into())).await;
                        return;
                    }
                }
            }
        }));
        let rtcp_cancel = cancel.clone();
        let rtcp_quality = quality.clone();
        tasks.push(tokio::spawn(async move {
            let mut buffer = vec![0u8; 2048];
            loop {
                let received = tokio::select! {
                    _ = rtcp_cancel.cancelled() => return,
                    result = rtcp_pair.rtcp.recv_from(&mut buffer) => result,
                };
                let Ok((size, _)) = received else {
                    return;
                };
                if let Ok(packets) =
                    RtcpPacket::parse(bytes::Bytes::copy_from_slice(&buffer[..size]))
                    && let Ok(Some(report)) =
                        quality_report(&rtcp_quality, track_id, clock_rate, local_ssrc, &packets)
                            .await
                    && let Ok(report) = report.try_encode()
                {
                    let _ = rtcp_pair.rtcp.send_to(&report, rtcp_target).await;
                }
            }
        }));
    }
    let peer = stream.peer_addr();
    let media_for_control = tx.clone();
    let (control_tx, control_rx) = mpsc::channel(16);
    let control_task_handle = tokio::spawn(control_task(
        ControlTransport::Owned(stream),
        control_rx,
        media_for_control,
        uri,
        session_id,
        cseq,
        options.credentials.clone(),
        options.request_timeout,
        effective_keep_alive(options.keep_alive_interval, session_timeout),
        id,
        options.event_sink.clone(),
        cancel.clone(),
    ));
    tasks.push(control_task_handle);
    drop(tx);
    let event_sink = options.event_sink.clone();
    emit_client_event(
        &event_sink,
        ClientEvent::Connected {
            id,
            transport: "udp",
            peer,
        },
    );
    tracing::debug!(session_id = %id, %peer, transport = "udp", "RTSP client session established");
    Ok(ClientSession::new(SessionParts {
        description,
        id,
        receiver: rx,
        control_tx: Some(control_tx),
        request_timeout: options.request_timeout,
        cancel,
        tasks,
        quality,
    }))
}

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
fn try_deliver(
    tx: &mpsc::Sender<Result<AccessUnit>>,
    result: Result<AccessUnit>,
    dropped: &mut u64,
) -> bool {
    match tx.try_send(result) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Closed(_)) => false,
        Err(mpsc::error::TrySendError::Full(_)) => {
            *dropped += 1;
            if *dropped % 64 == 1 {
                tracing::debug!(
                    dropped = *dropped,
                    "client output queue full; dropping frame"
                );
            }
            true
        }
    }
}

fn client_ssrc(track_id: u8) -> u32 {
    0x434c_4e54 ^ u32::from(track_id)
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
            bytes::Bytes::from_static(b"rtsp-client"),
            std::time::Duration::from_secs(5),
            now,
        )?),
    };
    if session.on_rtp(packet, now).is_err() {
        *session = RtcpSession::new(
            local_ssrc,
            packet.header.ssrc,
            clock_rate,
            bytes::Bytes::from_static(b"rtsp-client"),
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
                bytes::Bytes::from_static(b"rtsp-client"),
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
        for track_id in 0..=255u8 {
            assert_eq!(client_ssrc(track_id), client_ssrc(track_id));
        }
        let first = client_ssrc(0);
        assert!((1..=255u8).all(|track_id| client_ssrc(track_id) != first));
    }
}
