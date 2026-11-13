use super::*;

pub(super) async fn connect_udp(
    mut stream: ControlConnection,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    options: ClientOptions,
    id: SessionId,
) -> std::result::Result<ClientSession, Box<SetupFailure>> {
    #[cfg(feature = "multicast")]
    if let Some(connection) = &description.connection
        && connection.address.is_multicast()
    {
        return connect_multicast(stream, uri, setup_base, description, options, id).await;
    }
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
                return Err(Box::new(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                }));
            }
        };
        let port = match pair.rtp_port() {
            Ok(port) => port,
            Err(error) => {
                return Err(Box::new(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                }));
            }
        };
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
        let request_mux = options.rtcp_mux == Some(true);
        let mut extra = if request_mux {
            format!("Transport: RTP/AVP;unicast;client_port={port};rtcp-mux\r\n")
        } else {
            format!(
                "Transport: RTP/AVP;unicast;client_port={}-{}\r\n",
                port,
                port.saturating_add(1)
            )
        };
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
        let transport_spec = match response
            .headers
            .get("transport")
            .map(String::as_str)
            .map(TransportSpec::parse)
            .transpose()
        {
            Ok(spec) => spec,
            Err(error) => {
                return Err(Box::new(SetupFailure {
                    error,
                    stream,
                    setup_base,
                    description,
                }));
            }
        };
        let server_rtp = match &transport_spec {
            Some(TransportSpec::Udp {
                server_rtp: Some(port),
                ..
            }) => *port,
            _ => {
                return Err(Box::new(SetupFailure {
                    error: Error::Rtsp("SETUP response has no server RTP port".into()),
                    stream,
                    setup_base,
                    description,
                }));
            }
        };
        let server_rtcp = match &transport_spec {
            Some(TransportSpec::Udp {
                server_rtcp: Some(port),
                ..
            }) => *port,
            _ => {
                return Err(Box::new(SetupFailure {
                    error: Error::Rtsp("SETUP response has no server RTCP port".into()),
                    stream,
                    setup_base,
                    description,
                }));
            }
        };
        let peer_ip = stream.peer_addr().ip();
        let negotiated_mux = matches!(
            &transport_spec,
            Some(TransportSpec::Udp { rtcp_mux: true, .. })
        );
        let rtp_target = std::net::SocketAddr::new(peer_ip, server_rtp);
        let rtcp_target = std::net::SocketAddr::new(peer_ip, server_rtcp);
        receivers.push((pair, track, rtp_target, rtcp_target, negotiated_mux));
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
    cseq = cseq.wrapping_add(1);
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
    let (tx, rx) = mpsc::channel(options.output_queue_capacity);
    let cancel = CancellationToken::new();
    let quality = Arc::new(Mutex::new(HashMap::new()));
    let mut tasks = Vec::new();
    for (pair, track, rtp_target, rtcp_target, rtcp_mux) in receivers {
        let tx = tx.clone();
        let jitter_config = options.jitter.clone();
        let rtcp_pair = pair.clone();
        let rtp_cancel = cancel.clone();
        let rtp_quality = quality.clone();
        let track_id = track.id;
        let mux_rtcp_target = rtcp_target;
        let mux = rtcp_mux;
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
                        // rtcp-mux: RTCP shares the RTP socket; classify by
                        // payload type (192..=223) before RTP parsing.
                        if mux && size >= 2 && (192..=223).contains(&buffer[1]) {
                            let payload = buffer.split_to(size).freeze();
                            buffer.resize(65_536, 0);
                            if let Ok(packets) = RtcpPacket::parse(payload)
                                && let Ok(Some(report)) = quality_report(
                                    &rtp_quality,
                                    track_id,
                                    clock_rate,
                                    local_ssrc,
                                    &packets,
                                )
                                .await
                                && let Ok(wire) = report.try_encode()
                            {
                                let _ = pair.rtp.send_to(&wire, mux_rtcp_target).await;
                            }
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
        if !rtcp_mux {
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
                        && let Ok(Some(report)) = quality_report(
                            &rtcp_quality,
                            track_id,
                            clock_rate,
                            local_ssrc,
                            &packets,
                        )
                        .await
                        && let Ok(report) = report.try_encode()
                    {
                        let _ = rtcp_pair.rtcp.send_to(&report, rtcp_target).await;
                    }
                }
            }));
        }
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

#[cfg(feature = "multicast")]
async fn connect_multicast(
    mut stream: ControlConnection,
    uri: String,
    setup_base: String,
    description: MediaDescription,
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
        let mut extra = String::from("Transport: RTP/AVP;multicast;\r\n");
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
    let (tx, rx) = mpsc::channel(options.output_queue_capacity);
    let cancel = CancellationToken::new();
    let quality = Arc::new(Mutex::new(HashMap::new()));
    let mut tasks = Vec::new();
    let track_list: Vec<_> = description.tracks.to_vec();
    let media_for_control = tx.clone();
    let receiver_cancel = cancel.clone();
    let receiver_quality = quality.clone();
    tasks.push(tokio::spawn(async move {
        let mut buffer = bytes::BytesMut::zeroed(65_536);
        let mut active: HashMap<u8, (JitterBuffer, Depacketizer, u8, u32, u32)> = HashMap::new();
        for track in track_list {
            active.insert(
                track.payload_type,
                (
                    JitterBuffer::new(crate::rtp::JitterBufferConfig::default()),
                    Depacketizer::new(track.codec, track.id, track.clock_rate),
                    track.id,
                    track.clock_rate,
                    client_ssrc(track.id),
                ),
            );
        }
        loop {
            let received = tokio::select! {
                _ = receiver_cancel.cancelled() => return,
                result = pair.rtp.recv_from(&mut buffer) => result,
            };
            let Ok((size, _source)) = received else {
                return;
            };
            let payload = buffer.split_to(size).freeze();
            buffer.resize(65_536, 0);
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
            )
            .await;
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
        id,
        receiver: rx,
        control_tx: Some(control_tx),
        request_timeout: options.request_timeout,
        cancel,
        tasks,
        quality,
    }))
}
