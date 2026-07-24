use super::{
    ClientOptions, ClientSession, drain_jitter, receive_rtp, request, response_session_id,
};
use crate::{
    Error, Result,
    media::MediaDescription,
    rtcp::Packet as RtcpPacket,
    rtp::{Depacketizer, JitterBuffer},
    runtime::framing::{ConnectionFramer, InboundFrame},
    transport::{InterleavedFrame, TransportSpec, UdpPair},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Mutex, mpsc},
};
use tokio_util::sync::CancellationToken;

pub(super) async fn connect(
    stream: TcpStream,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    options: ClientOptions,
) -> Result<ClientSession> {
    match options.transport {
        crate::proxy::UpstreamTransport::Udp => {
            connect_udp(stream, uri, setup_base, description, options).await
        }
        crate::proxy::UpstreamTransport::TcpInterleaved => {
            connect_interleaved(stream, uri, setup_base, description, options).await
        }
    }
}

async fn connect_interleaved(
    mut stream: TcpStream,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    options: ClientOptions,
) -> Result<ClientSession> {
    let mut cseq = 3u32;
    let mut session_id = None;
    let mut tracks = HashMap::new();
    let mut rtcp_channels = HashSet::new();
    for (index, track) in description.tracks.iter().cloned().enumerate() {
        let channel = (index * 2) as u8;
        let setup_uri = crate::protocol::sdp::control_uri(&setup_base, &track.control)?;
        let mut extra = format!(
            "Transport: RTP/AVP/TCP;unicast;interleaved={}-{}\r\n",
            channel,
            channel.saturating_add(1)
        );
        if let Some(session) = &session_id {
            extra.push_str(&format!("Session: {session}\r\n"));
        }
        let response = request(
            &mut stream,
            "SETUP",
            &setup_uri,
            cseq,
            Some(&extra),
            options.request_timeout,
            options.credentials.as_ref(),
        )
        .await?;
        cseq = cseq.wrapping_add(1);
        if response.status != 200 {
            return Err(Error::Rtsp(format!("SETUP returned {}", response.status)));
        }
        let received = response_session_id(&response)?;
        if session_id.as_ref().is_some_and(|value| value != &received) {
            return Err(Error::Rtsp("server changed Session during SETUP".into()));
        }
        session_id = Some(received);
        tracks.insert(
            channel,
            (
                JitterBuffer::new(options.jitter.clone()),
                Depacketizer::new(track.codec, track.id),
            ),
        );
        rtcp_channels.insert(channel.saturating_add(1));
    }
    let session_id = session_id.ok_or_else(|| Error::Rtsp("SDP has no tracks".into()))?;
    let extra = format!("Session: {session_id}\r\n");
    let response = request(
        &mut stream,
        "PLAY",
        &uri,
        cseq,
        Some(&extra),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    cseq = cseq.wrapping_add(1);
    if response.status != 200 {
        return Err(Error::Rtsp(format!("PLAY returned {}", response.status)));
    }
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let rtcp_writer = writer.clone();
    let (tx, rx) = mpsc::channel(options.output_queue_capacity);
    let (response_tx, response_rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let reader_cancel = cancel.clone();
    let jitter_poll = options
        .jitter
        .max_delay
        .min(std::time::Duration::from_millis(10))
        .max(std::time::Duration::from_millis(1));
    let reader_task = tokio::spawn(async move {
        let mut framer = ConnectionFramer::new(16 * 1024, 256 * 1024, 65_536);
        let mut scratch = [0u8; 4096];
        let mut jitter_tick = tokio::time::interval(jitter_poll);
        jitter_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let read = tokio::select! {
                _ = reader_cancel.cancelled() => return,
                _ = jitter_tick.tick() => {
                    for (jitter, depacketizer) in tracks.values_mut() {
                        for result in drain_jitter(jitter, depacketizer) {
                            if tx.send(result).await.is_err() {
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
                                if let Some((jitter, depacketizer)) = tracks.get_mut(&frame.channel)
                                {
                                    for result in receive_rtp(frame.payload, jitter, depacketizer) {
                                        if tx.send(result).await.is_err() {
                                            return;
                                        }
                                    }
                                } else if rtcp_channels.contains(&frame.channel)
                                    && RtcpPacket::parse(frame.payload).is_ok()
                                {
                                    let report = RtcpPacket::ReceiverReport { ssrc: 0x434c_4e54 };
                                    let Ok(wire) = (InterleavedFrame {
                                        channel: frame.channel,
                                        payload: report.encode(),
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
    });
    Ok(ClientSession::new(
        description,
        rx,
        None,
        Some(writer),
        Some(response_rx),
        cseq,
        session_id,
        uri,
        options.credentials.clone(),
        options.request_timeout,
        options.keep_alive_interval,
        cancel,
        vec![reader_task],
    ))
}

async fn connect_udp(
    mut stream: TcpStream,
    uri: String,
    setup_base: String,
    description: MediaDescription,
    options: ClientOptions,
) -> Result<ClientSession> {
    let mut cseq = 3u32;
    let mut session_id = None;
    let mut receivers = Vec::new();
    for track in description.tracks.iter().cloned() {
        let bind_ip = match stream.local_addr()?.ip() {
            std::net::IpAddr::V4(_) => std::net::Ipv4Addr::UNSPECIFIED.into(),
            std::net::IpAddr::V6(_) => std::net::Ipv6Addr::UNSPECIFIED.into(),
        };
        let pair = UdpPair::bind(bind_ip, None).await?;
        let port = pair.rtp_port()?;
        let setup_uri = crate::protocol::sdp::control_uri(&setup_base, &track.control)?;
        let mut extra = format!(
            "Transport: RTP/AVP;unicast;client_port={}-{}\r\n",
            port,
            port.saturating_add(1)
        );
        if let Some(session) = &session_id {
            extra.push_str(&format!("Session: {session}\r\n"));
        }
        let response = request(
            &mut stream,
            "SETUP",
            &setup_uri,
            cseq,
            Some(&extra),
            options.request_timeout,
            options.credentials.as_ref(),
        )
        .await?;
        cseq = cseq.wrapping_add(1);
        if response.status != 200 {
            return Err(Error::Rtsp(format!("SETUP returned {}", response.status)));
        }
        let received = response_session_id(&response)?;
        if session_id.as_ref().is_some_and(|value| value != &received) {
            return Err(Error::Rtsp("server changed Session during SETUP".into()));
        }
        session_id = Some(received);
        let server_rtcp = match response
            .headers
            .get("transport")
            .map(String::as_str)
            .map(TransportSpec::parse)
            .transpose()?
        {
            Some(TransportSpec::Udp {
                server_rtcp: Some(port),
                ..
            }) => port,
            _ => return Err(Error::Rtsp("SETUP response has no server RTCP port".into())),
        };
        let rtcp_target = std::net::SocketAddr::new(stream.peer_addr()?.ip(), server_rtcp);
        receivers.push((pair, track, rtcp_target));
    }
    let session_id = session_id.ok_or_else(|| Error::Rtsp("SDP has no tracks".into()))?;
    let extra = format!("Session: {session_id}\r\n");
    let response = request(
        &mut stream,
        "PLAY",
        &uri,
        cseq,
        Some(&extra),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    cseq = cseq.wrapping_add(1);
    if response.status != 200 {
        return Err(Error::Rtsp(format!("PLAY returned {}", response.status)));
    }
    let (tx, rx) = mpsc::channel(options.output_queue_capacity);
    let cancel = CancellationToken::new();
    let mut tasks = Vec::new();
    for (pair, track, rtcp_target) in receivers {
        let tx = tx.clone();
        let jitter_config = options.jitter.clone();
        let jitter_poll = jitter_config
            .max_delay
            .min(std::time::Duration::from_millis(10))
            .max(std::time::Duration::from_millis(1));
        let rtcp_pair = pair.clone();
        let rtp_cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut jitter = JitterBuffer::new(jitter_config);
            let mut depacketizer = Depacketizer::new(track.codec, track.id);
            let mut buffer = vec![0u8; 65_536];
            let mut jitter_tick = tokio::time::interval(jitter_poll);
            jitter_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let received = tokio::select! {
                    _ = rtp_cancel.cancelled() => return,
                    _ = jitter_tick.tick() => {
                        for result in drain_jitter(&mut jitter, &mut depacketizer) {
                            if tx.send(result).await.is_err() {
                                return;
                            }
                        }
                        continue;
                    }
                    result = pair.rtp.recv_from(&mut buffer) => result,
                };
                match received {
                    Ok((size, _)) => {
                        for result in receive_rtp(
                            bytes::Bytes::copy_from_slice(&buffer[..size]),
                            &mut jitter,
                            &mut depacketizer,
                        ) {
                            if tx.send(result).await.is_err() {
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
                if RtcpPacket::parse(bytes::Bytes::copy_from_slice(&buffer[..size])).is_ok() {
                    let report = RtcpPacket::ReceiverReport { ssrc: 0x434c_4e54 }.encode();
                    let _ = rtcp_pair.rtcp.send_to(&report, rtcp_target).await;
                }
            }
        }));
    }
    drop(tx);
    Ok(ClientSession::new(
        description,
        rx,
        Some(stream),
        None,
        None,
        cseq,
        session_id,
        uri,
        options.credentials.clone(),
        options.request_timeout,
        options.keep_alive_interval,
        cancel,
        tasks,
    ))
}
