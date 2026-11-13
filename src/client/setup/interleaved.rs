use super::*;

pub(super) async fn connect_interleaved(
    mut stream: ControlConnection,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    options: ClientOptions,
    id: SessionId,
) -> std::result::Result<ClientSession, Box<SetupFailure>> {
    let mut cseq = 3u32;
    let mut session_id = None;
    let mut session_timeout = None;
    let mut tracks = HashMap::new();
    let mut rtcp_channels = HashMap::new();
    #[cfg(feature = "srtp")]
    let mut interleaved_crypto: HashMap<
        u8,
        (crate::srtp::SrtpContext, crate::srtp::SrtcpContext),
    > = HashMap::new();
    let track_list: Vec<_> = description.tracks.to_vec();
    for (index, track) in track_list.iter().cloned().enumerate() {
        if matches!(
            track.direction,
            Some(crate::media::SdpDirection::RecvOnly | crate::media::SdpDirection::Inactive)
        ) {
            continue;
        }
        // Two interleaved channels per track; more than 127 tracks would
        // overflow the 8-bit channel space — refuse instead of wrapping.
        let channel = match u8::try_from(index * 2) {
            Ok(channel) => channel,
            Err(_) => {
                return Err(Box::new(SetupFailure {
                    error: Error::Limit("too many tracks for interleaved transport"),
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
        let profile = if secure {
            "RTP/SAVP/TCP"
        } else {
            "RTP/AVP/TCP"
        };
        let mut extra = if request_mux {
            format!("Transport: {profile};unicast;interleaved={channel};rtcp-mux\r\n")
        } else {
            format!(
                "Transport: {profile};unicast;interleaved={}-{}\r\n",
                channel,
                channel.saturating_add(1)
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
        #[cfg(feature = "srtp")]
        if let Some(crypto) = &track.crypto {
            match crate::srtp::from_sdp_crypto(crypto) {
                Ok((srtp, srtcp)) => {
                    interleaved_crypto.insert(channel, (srtp, srtcp));
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
        let negotiated_mux = request_mux
            && response
                .headers
                .get("transport")
                .is_some_and(|value| value.to_ascii_lowercase().contains("rtcp-mux"));
        rtcp_channels.insert(
            if negotiated_mux {
                channel
            } else {
                channel.saturating_add(1)
            },
            channel,
        );
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
                                #[cfg(feature = "srtp")]
                                let secure_channel = interleaved_crypto
                                    .contains_key(&frame.channel)
                                    || rtcp_channels.get(&frame.channel).is_some_and(|channel| {
                                        interleaved_crypto.contains_key(channel)
                                    });
                                #[cfg(not(feature = "srtp"))]
                                let secure_channel = false;
                                // rtcp-mux: RTCP and RTP share a channel;
                                // classify by payload type before RTP parsing.
                                if !secure_channel
                                    && frame.payload.len() >= 2
                                    && (192..=223).contains(&frame.payload[1])
                                    && let Some(rtp_channel) = rtcp_channels.get(&frame.channel)
                                    && let Some((_, _, track_id, clock_rate, local_ssrc)) =
                                        tracks.get(rtp_channel)
                                    && let Ok(packets) = RtcpPacket::parse(frame.payload.clone())
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
                                    let write =
                                        async { rtcp_writer.lock().await.write_all(&wire).await };
                                    if !matches!(
                                        tokio::time::timeout(
                                            std::time::Duration::from_secs(10),
                                            write,
                                        )
                                        .await,
                                        Ok(Ok(()))
                                    ) {
                                        return;
                                    }
                                    continue;
                                }
                                if let Some((
                                    jitter,
                                    depacketizer,
                                    track_id,
                                    clock_rate,
                                    local_ssrc,
                                )) = tracks.get_mut(&frame.channel)
                                {
                                    #[cfg(feature = "srtp")]
                                    let payload = match interleaved_crypto.get_mut(&frame.channel) {
                                        Some((srtp, _)) => match srtp.unprotect_rtp(&frame.payload)
                                        {
                                            Ok(payload) => payload,
                                            Err(_) => continue,
                                        },
                                        None => frame.payload.clone(),
                                    };
                                    #[cfg(not(feature = "srtp"))]
                                    let payload = frame.payload.clone();
                                    let packet = match RtpPacket::parse(payload) {
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
                                } else if !secure_channel
                                    && let Some(rtp_channel) = rtcp_channels.get(&frame.channel)
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
                                    let write =
                                        async { rtcp_writer.lock().await.write_all(&wire).await };
                                    if !matches!(
                                        tokio::time::timeout(
                                            std::time::Duration::from_secs(10),
                                            write,
                                        )
                                        .await,
                                        Ok(Ok(()))
                                    ) {
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
