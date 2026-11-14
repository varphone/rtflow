//! UDP NACK retransmission end-to-end: raw RTSP client + real RTP/RTCP sockets.
#![cfg(feature = "server")]

use bytes::{Bytes, BytesMut};
use rtflow::{
    AccessUnit, Codec, MountConfig, RtspServer, ServerConfig,
    rtcp::{CompoundPacket, NackEntry, Packet as RtcpPacket},
    rtp::RtpPacket,
    rtsp::{Message, Response, transport::TransportSpec},
};
use rtflow_testkit::description;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};

async fn read_response(stream: &mut TcpStream, buffer: &mut BytesMut) -> Response {
    loop {
        if let Some(Message::Response(response)) =
            rtflow::rtsp::parse(buffer, 64 * 1024, 1024 * 1024).unwrap()
        {
            return response;
        }
        let mut chunk = [0u8; 4096];
        let size = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk))
            .await
            .expect("RTSP response timeout")
            .unwrap();
        assert!(size > 0, "RTSP connection closed");
        buffer.extend_from_slice(&chunk[..size]);
    }
}

async fn exchange(
    stream: &mut TcpStream,
    buffer: &mut BytesMut,
    cseq: u32,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
) -> Response {
    let mut request = format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let response = read_response(stream, buffer).await;
    assert_eq!(response.status, 200, "{method} failed: {response:?}");
    response
}

#[tokio::test]
async fn udp_nack_retransmits_cached_rtp_packet() {
    nack_case(false).await;
}

#[tokio::test]
async fn udp_rtx_retransmits_with_separate_payload_type() {
    nack_case(true).await;
}

async fn nack_case(rtx_enabled: bool) {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.rtx = rtx_enabled;
    let server = RtspServer::bind(config).await.unwrap();
    let description = description("nack", &[Codec::H264]);
    let rtx = rtx_enabled
        .then(|| rtflow::media::rtx_configs(&description).get(&0).copied())
        .flatten();
    let mount = server
        .register_mount(
            Arc::from("/nack"),
            MountConfig::Local {
                description: description.clone(),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = format!("rtsp://{}/nack", server.local_addr());

    let (client_rtp, client_rtcp) = {
        let mut pair = None;
        for _ in 0..100 {
            let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let port = rtp.local_addr().unwrap().port();
            if port == u16::MAX {
                continue;
            }
            if let Ok(rtcp) = UdpSocket::bind(("127.0.0.1", port + 1)).await {
                pair = Some((rtp, rtcp));
                break;
            }
        }
        pair.expect("adjacent UDP ports")
    };
    let rtp_port = client_rtp.local_addr().unwrap().port();
    let rtcp_port = client_rtcp.local_addr().unwrap().port();

    let mut tcp = TcpStream::connect(server.local_addr()).await.unwrap();
    let mut buffer = BytesMut::new();
    let mut cseq = 1u32;
    exchange(&mut tcp, &mut buffer, cseq, "OPTIONS", &url, &[]).await;
    cseq += 1;
    let describe = exchange(
        &mut tcp,
        &mut buffer,
        cseq,
        "DESCRIBE",
        &url,
        &[("Accept", "application/sdp".into())],
    )
    .await;
    let sdp = String::from_utf8_lossy(&describe.body);
    assert!(sdp.contains("m=video"));
    cseq += 1;
    let setup = exchange(
        &mut tcp,
        &mut buffer,
        cseq,
        "SETUP",
        &format!("{url}/trackID=0"),
        &[(
            "Transport",
            format!("RTP/AVP;unicast;client_port={rtp_port}-{rtcp_port}"),
        )],
    )
    .await;
    let session = setup.header("session").unwrap().to_string();
    let transport = TransportSpec::parse(setup.header("transport").unwrap()).unwrap();
    let TransportSpec::Udp {
        server_rtcp: Some(server_rtcp),
        ..
    } = transport
    else {
        panic!("expected UDP transport with server RTCP port");
    };
    cseq += 1;
    exchange(
        &mut tcp,
        &mut buffer,
        cseq,
        "PLAY",
        &url,
        &[("Session", session.clone())],
    )
    .await;

    // A keyframe with a NAL large enough to span several RTP packets.
    let mut data = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    data.extend(std::iter::repeat_n(0x55, 4000));
    for (index, keyframe) in [true, false].into_iter().enumerate() {
        publisher
            .try_push(AccessUnit {
                track_id: 0,
                codec: Codec::H264,
                pts: Duration::from_millis(index as u64 * 40),
                dts: None,
                rtp_timestamp: index as u32 * 3600,
                is_keyframe: keyframe,
                data: Bytes::copy_from_slice(&data),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut packet = [0u8; 2048];
    let mut first_sequence = None;
    let mut rtp_source = None;
    for _ in 0..20 {
        let Ok(Ok((size, source))) =
            tokio::time::timeout(Duration::from_secs(1), client_rtp.recv_from(&mut packet)).await
        else {
            break;
        };
        if let Ok(parsed) = RtpPacket::parse(Bytes::copy_from_slice(&packet[..size])) {
            first_sequence = Some(parsed.header.sequence);
            rtp_source = Some(source);
            break;
        }
    }
    let sequence = first_sequence.expect("RTP packet");
    assert!(rtp_source.is_some());

    let nack = RtcpPacket::nack(
        0x1122_3344,
        0x5566_7788,
        &[NackEntry {
            packet_id: sequence,
            lost: 0,
        }],
    )
    .unwrap();
    let compound =
        CompoundPacket::new(vec![RtcpPacket::ReceiverReport { ssrc: 0x1122_3344 }, nack])
            .unwrap()
            .try_encode()
            .unwrap();
    client_rtcp
        .send_to(
            &compound,
            std::net::SocketAddr::new(server.local_addr().ip(), server_rtcp),
        )
        .await
        .unwrap();

    let mut retransmitted = false;
    for _ in 0..20 {
        let Ok(Ok((size, _))) =
            tokio::time::timeout(Duration::from_secs(1), client_rtp.recv_from(&mut packet)).await
        else {
            break;
        };
        if let Ok(parsed) = RtpPacket::parse(Bytes::copy_from_slice(&packet[..size])) {
            if let Some(rtx) = rtx {
                if parsed.header.payload_type == rtx.payload_type
                    && parsed.header.ssrc == rtx.ssrc
                    && parsed.payload.len() >= 2
                    && u16::from_be_bytes([parsed.payload[0], parsed.payload[1]]) == sequence
                {
                    retransmitted = true;
                    break;
                }
            } else if parsed.header.sequence == sequence {
                retransmitted = true;
                break;
            }
        }
    }
    assert!(retransmitted, "NACK did not produce a retransmission");
    let _ = tcp.shutdown().await;
    drop(publisher);
    let _ = tokio::time::timeout(Duration::from_secs(2), server.shutdown()).await;
}
