use super::*;

#[cfg(feature = "multicast")]
pub(super) async fn connect_multicast(
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
