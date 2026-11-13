use super::*;

struct ReceiverSpec {
    pair: UdpPair,
    track: crate::media::TrackDescription,
    rtp_target: std::net::SocketAddr,
    rtcp_target: std::net::SocketAddr,
    rtcp_mux: bool,
    rtp_info: super::RtpInfo,
    pts_base: std::time::Duration,
    #[cfg(feature = "srtp")]
    srtp: Option<crate::srtp::SrtpContext>,
    #[cfg(feature = "srtp")]
    srtcp: Option<crate::srtp::SrtcpContext>,
    #[cfg(feature = "srtp")]
    srtcp_rr: Option<crate::srtp::SrtcpContext>,
}

pub(super) async fn connect_udp(
    mut stream: ControlConnection,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    capabilities: crate::client::ServerCapabilities,
    options: ClientOptions,
    id: SessionId,
) -> std::result::Result<ClientSession, Box<SetupFailure>> {
    #[cfg(feature = "multicast")]
    if let Some(connection) = &description.connection
        && connection.address.is_multicast()
    {
        return super::multicast::connect_multicast(
            stream,
            uri,
            setup_base,
            description,
            capabilities,
            options,
            id,
        )
        .await;
    }
    let mut cseq = 3u32;
    let mut session_id = None;
    let mut session_timeout = None;
    let mut receivers = Vec::new();
    let track_list: Vec<_> = description.tracks.to_vec();
    for track in track_list {
        if matches!(
            track.direction,
            Some(crate::media::SdpDirection::RecvOnly | crate::media::SdpDirection::Inactive)
        ) {
            continue;
        }
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
        #[cfg(feature = "srtp")]
        let secure = track.crypto.is_some();
        #[cfg(not(feature = "srtp"))]
        let secure = false;
        let profile = if secure { "RTP/SAVP" } else { "RTP/AVP" };
        let mut extra = if request_mux {
            format!("Transport: {profile};unicast;client_port={port};rtcp-mux\r\n")
        } else {
            format!(
                "Transport: {profile};unicast;client_port={}-{}\r\n",
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
        let rtcp_target = track.rtcp.map_or_else(
            || std::net::SocketAddr::new(peer_ip, server_rtcp),
            |rtcp| std::net::SocketAddr::new(rtcp.address.unwrap_or(peer_ip), rtcp.port),
        );
        #[cfg(feature = "srtp")]
        let (srtp, srtcp, srtcp_rr) = match &track.crypto {
            Some(crypto) => {
                let first = crate::srtp::from_sdp_crypto(crypto);
                let second = crate::srtp::from_sdp_crypto(crypto);
                match (first, second) {
                    (Ok((srtp, srtcp)), Ok((_, srtcp_rr))) => {
                        (Some(srtp), Some(srtcp), Some(srtcp_rr))
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        return Err(Box::new(SetupFailure {
                            error,
                            stream,
                            setup_base,
                            description,
                        }));
                    }
                }
            }
            None => (None, None, None),
        };
        receivers.push(ReceiverSpec {
            pair,
            track,
            rtp_target,
            rtcp_target,
            rtcp_mux: negotiated_mux,
            rtp_info: super::RtpInfo::default(),
            pts_base: std::time::Duration::ZERO,
            #[cfg(feature = "srtp")]
            srtp,
            #[cfg(feature = "srtp")]
            srtcp,
            #[cfg(feature = "srtp")]
            srtcp_rr,
        });
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
    let rtp_info = super::parse_rtp_info(&response, &description, &setup_base);
    let pts_base = super::play_pts_base(&response);
    for spec in &mut receivers {
        if let Some(info) = rtp_info.get(&spec.track.id) {
            spec.rtp_info = *info;
        }
        spec.pts_base = pts_base;
    }
    let (tx, rx) = mpsc::channel(options.output_queue_capacity);
    let cancel = CancellationToken::new();
    let quality = Arc::new(Mutex::new(HashMap::new()));
    let queued_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for spec in receivers {
        let ReceiverSpec {
            pair,
            track,
            rtp_target,
            rtcp_target,
            rtcp_mux,
            rtp_info,
            pts_base,
            #[cfg(feature = "srtp")]
            srtp,
            #[cfg(feature = "srtp")]
            srtcp,
            #[cfg(feature = "srtp")]
            srtcp_rr,
        } = spec;
        #[cfg(feature = "srtp")]
        let mut srtp = srtp;
        #[cfg(feature = "srtp")]
        let mut srtcp = srtcp;
        #[cfg(feature = "srtp")]
        let mut srtcp_rr = srtcp_rr;
        let tx = tx.clone();
        let jitter_config = options.jitter.clone();
        let rtcp_pair = pair.clone();
        let rtp_cancel = cancel.clone();
        let rtp_quality = quality.clone();
        let rtp_queued = queued_bytes.clone();
        let byte_limit = options.max_output_bytes;
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
            if let Some(sequence) = rtp_info.sequence {
                jitter.prime(sequence);
            }
            if pts_base > std::time::Duration::ZERO
                && let Some(rtptime) = rtp_info.rtptime
            {
                depacketizer.set_time_base(rtptime, pts_base);
            }
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
                            if !try_deliver(&tx, result, &mut dropped, &rtp_queued, byte_limit) {
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
                            #[cfg(feature = "srtp")]
                            let payload = match srtcp.as_mut() {
                                Some(context) => match context.unprotect(&payload) {
                                    Ok(payload) => payload,
                                    Err(_) => continue,
                                },
                                None => payload,
                            };
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
                                #[cfg(feature = "srtp")]
                                let wire = match srtcp.as_mut() {
                                    Some(context) => match context.protect(&wire) {
                                        Ok(wire) => wire,
                                        Err(_) => continue,
                                    },
                                    None => wire,
                                };
                                let _ = pair.rtp.send_to(&wire, mux_rtcp_target).await;
                            }
                            continue;
                        }
                        let payload = buffer.split_to(size).freeze();
                        buffer.resize(65_536, 0);
                        #[cfg(feature = "srtp")]
                        let payload = match srtp.as_mut() {
                            Some(context) => match context.unprotect_rtp(&payload) {
                                Ok(payload) => payload,
                                Err(_) => continue,
                            },
                            None => payload,
                        };
                        let packet = match RtpPacket::parse(payload) {
                            Ok(packet) => packet,
                            Err(error) => {
                                if !try_deliver(
                                    &tx,
                                    Err(error),
                                    &mut dropped,
                                    &rtp_queued,
                                    byte_limit,
                                ) {
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
                            if !try_deliver(&tx, Err(error), &mut dropped, &rtp_queued, byte_limit)
                            {
                                return;
                            }
                            continue;
                        }
                        for result in receive_rtp_packet(packet, &mut jitter, &mut depacketizer) {
                            if !try_deliver(&tx, result, &mut dropped, &rtp_queued, byte_limit) {
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
                        #[cfg(feature = "srtp")]
                        let report = match srtcp_rr.as_mut() {
                            Some(context) => match context.protect(&report) {
                                Ok(report) => report,
                                Err(_) => continue,
                            },
                            None => report,
                        };
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
        capabilities,
        id,
        receiver: rx,
        control_tx: Some(control_tx),
        request_timeout: options.request_timeout,
        cancel,
        tasks,
        quality,
        queued_bytes,
        max_output_bytes: options.max_output_bytes,
    }))
}
