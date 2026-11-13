use super::*;

#[cfg(feature = "multicast")]
pub(super) async fn connect_multicast(
    mut stream: ControlConnection,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    capabilities: crate::client::ServerCapabilities,
    options: ClientOptions,
    id: SessionId,
) -> std::result::Result<ClientSession, Box<SetupFailure>> {
    let Some(connection) = description
        .connection
        .clone()
        .filter(|connection| connection.address.is_multicast())
    else {
        return Err(Box::new(SetupFailure {
            error: Error::Rtsp("multicast session without c= group".into()),
            stream,
            setup_base,
            description,
        }));
    };
    let group = connection.address;
    let port = connection.port.unwrap_or(5004);
    let ttl = connection.ttl.unwrap_or(1);
    let mut cseq = 3u32;
    let mut session_id = None;
    let mut session_timeout = None;
    for track in description.tracks.iter().cloned() {
        if matches!(
            track.direction,
            Some(crate::media::SdpDirection::RecvOnly | crate::media::SdpDirection::Inactive)
        ) {
            continue;
        }
        let setup_uri = match crate::rtsp::sdp::control_uri(&setup_base, &track.control) {
            Ok(uri) => uri,
            Err(error) => {
                return Err(Box::new(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                }));
            }
        };
        #[cfg(feature = "srtp")]
        let secure = track.crypto.is_some();
        #[cfg(not(feature = "srtp"))]
        let secure = false;
        let profile = if secure { "RTP/SAVP" } else { "RTP/AVP" };
        let require = options.require_headers();
        let mut extra = format!("Transport: {profile};multicast\r\n");
        if let Some(require) = require.as_deref() {
            extra.push_str(require);
        }
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
                return Err(Box::new(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                }));
            }
        };
        cseq = cseq.wrapping_add(1);
        if response.status != 200 {
            return Err(Box::new(SetupFailure {
                error: Error::RtspStatus {
                    method: "SETUP",
                    status: response.status,
                },
                stream,
                setup_base,
                description,
            }));
        }
        let received = match response_session_id(&response) {
            Ok(value) => value,
            Err(error) => {
                return Err(Box::new(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                }));
            }
        };
        if session_id.as_ref().is_some_and(|value| value != &received) {
            return Err(Box::new(SetupFailure {
                error: Error::Rtsp("server changed Session during SETUP".into()),
                stream,
                setup_base,
                description,
            }));
        }
        session_id = Some(received);
        if session_timeout.is_none() {
            session_timeout = response_session_timeout(&response);
        }
    }
    let Some(session_id) = session_id else {
        return Err(Box::new(SetupFailure {
            error: Error::Rtsp("SDP has no tracks".into()),
            stream,
            setup_base,
            description,
        }));
    };
    let mut extra = format!("Session: {session_id}\r\n");
    if let Some(require) = options.require_headers() {
        extra.push_str(&require);
    }
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
            return Err(Box::new(SetupFailure {
                error,
                stream,
                setup_base,
                description,
            }));
        }
    };
    if response.status != 200 {
        return Err(Box::new(SetupFailure {
            error: Error::RtspStatus {
                method: "PLAY",
                status: response.status,
            },
            stream,
            setup_base,
            description,
        }));
    }
    let rtp_socket = match crate::rtsp::multicast::MulticastConfig::new(group, port)
        .with_ttl(ttl)
        .bind()
        .await
    {
        Ok(socket) => socket,
        Err(error) => {
            return Err(Box::new(SetupFailure {
                error,
                stream,
                setup_base,
                description,
            }));
        }
    };
    let rtcp_socket =
        match crate::rtsp::multicast::MulticastConfig::new(group, port.saturating_add(1))
            .with_ttl(ttl)
            .bind()
            .await
        {
            Ok(socket) => socket,
            Err(error) => {
                return Err(Box::new(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                }));
            }
        };
    let pair = UdpPair::from_sockets(rtp_socket, rtcp_socket);
    #[cfg(feature = "srtp")]
    let mut srtp_contexts: std::collections::HashMap<u8, crate::srtp::SrtpContext> =
        std::collections::HashMap::new();
    #[cfg(feature = "srtp")]
    let mut srtcp_contexts: std::collections::HashMap<u8, crate::srtp::SrtcpContext> =
        std::collections::HashMap::new();
    #[cfg(feature = "srtp")]
    for track in &description.tracks {
        if let Some(attribute) = &track.crypto {
            match crate::srtp::from_sdp_crypto(attribute) {
                Ok((srtp, srtcp)) => {
                    srtp_contexts.insert(track.payload_type, srtp);
                    srtcp_contexts.insert(track.id, srtcp);
                }
                Err(error) => {
                    return Err(Box::new(SetupFailure {
                        error,
                        stream,
                        setup_base,
                        description,
                    }));
                }
            }
        }
    }
    let rtp_info = super::parse_rtp_info(&response, &description, &setup_base);
    let pts_base = super::play_pts_base(&response);
    let (tx, rx) = crate::client::queue::FrameReceiver::new(
        options.output_queue_capacity,
        options.max_output_bytes,
    );
    let cancel = CancellationToken::new();
    let quality = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut tasks = Vec::new();
    let track_list: Vec<_> = description.tracks.to_vec();
    let rr_meta: Vec<(u8, u32)> = track_list
        .iter()
        .map(|track| (track.id, client_ssrc(id, track.id)))
        .collect();
    let media_for_control = tx.clone();
    let receiver_cancel = cancel.clone();
    let receiver_quality = quality.clone();
    tasks.push(tokio::spawn(async move {
        let mut buffer = bytes::BytesMut::zeroed(65_536);
        let mut rr_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        rr_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut active: HashMap<u8, (JitterBuffer, Depacketizer, u8, u32, u32)> = HashMap::new();
        for track in track_list {
            let mut jitter = JitterBuffer::new(crate::rtp::JitterBufferConfig::default());
            let mut depacketizer = Depacketizer::new(track.codec, track.id, track.clock_rate);
            if let Some(info) = rtp_info.get(&track.id) {
                if let Some(sequence) = info.sequence {
                    jitter.prime(sequence);
                }
                if pts_base > std::time::Duration::ZERO
                    && let Some(rtptime) = info.rtptime
                {
                    depacketizer.set_time_base(rtptime, pts_base);
                }
            }
            active.insert(
                track.payload_type,
                (
                    jitter,
                    depacketizer,
                    track.id,
                    track.clock_rate,
                    client_ssrc(id, track.id),
                ),
            );
        }
        loop {
            let received = tokio::select! {
                _ = receiver_cancel.cancelled() => return,
                _ = rr_tick.tick() => {
                    // Multicast receivers report over the RTCP group port;
                    // there is no unicast back-channel.
                    for (track_id, ssrc) in &rr_meta {
                        let Ok(report) = crate::rtcp::CompoundPacket::new(vec![
                            RtcpPacket::ReceiverReport { ssrc: *ssrc },
                        ])
                        .and_then(|compound| compound.try_encode()) else {
                            continue;
                        };
                        #[cfg(feature = "srtp")]
                        let report = match srtcp_contexts.get_mut(track_id) {
                            Some(context) => match context.protect(&report) {
                                Ok(report) => report,
                                Err(_) => continue,
                            },
                            None => report,
                        };
                        let target = std::net::SocketAddr::new(group, port.saturating_add(1));
                        let _ = pair.rtcp.send_to(&report, target).await;
                    }
                    continue;
                }
                result = pair.rtp.recv_from(&mut buffer) => result,
            };
            let Ok((size, _source)) = received else {
                return;
            };
            let mut payload = bytes::Bytes::copy_from_slice(&buffer[..size]);
            #[cfg(feature = "srtp")]
            if let Some(payload_type) = payload.get(1).map(|byte| byte & 0x7f)
                && let Some(context) = srtp_contexts.get_mut(&payload_type)
            {
                payload = match context.unprotect_rtp(&payload) {
                    Ok(payload) => payload,
                    Err(_) => continue,
                };
            }
            let Ok(packet) = RtpPacket::parse(payload) else {
                continue;
            };
            let Some((jitter, depacketizer, track_id, clock_rate, local_ssrc)) =
                active.get_mut(&packet.header.payload_type)
            else {
                continue;
            };
            let _ = observe_quality(
                &receiver_quality,
                *track_id,
                *clock_rate,
                *local_ssrc,
                &packet,
            );
            let mut dropped = 0u64;
            for result in receive_rtp_packet(packet, jitter, depacketizer) {
                if !try_deliver(&tx, result, &mut dropped) {
                    return;
                }
            }
        }
    }));
    let peer = stream.peer_addr();
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
    emit_client_event(
        &options.event_sink,
        ClientEvent::Connected {
            id,
            transport: "multicast",
            peer,
        },
    );
    Ok(ClientSession::new(SessionParts {
        description,
        capabilities,
        id,
        receiver: rx,
        control_tx: Some(control_tx),
        request_timeout: options.request_timeout,
        cancel,
        tasks,
        quality,
    }))
}
