//! Wire-level check that the publisher emits RTCP SR on the negotiated
//! server RTCP port.
use rtflow::{
    Codec,
    client::{ClientOptions, RtspClient},
    rtcp::Packet as RtcpPacket,
    transport::UpstreamTransport,
};
use rtflow_testkit::{RawRtspServer, ServerReply, describe_reply, reply, with_header};
use std::{sync::Arc, time::Duration};
use tokio::net::UdpSocket;

#[tokio::test]
async fn publisher_sends_sender_report_to_server_rtcp_port() {
    let (server_rtp, server_rtcp) = {
        let mut pair = None;
        for _ in 0..32 {
            let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let port = rtp.local_addr().unwrap().port();
            if let Ok(rtcp) = UdpSocket::bind(("127.0.0.1", port + 1)).await {
                pair = Some((rtp, rtcp));
                break;
            }
        }
        let (rtp, rtcp) = pair.expect("adjacent RTP/RTCP ports");
        (Arc::new(rtp), Arc::new(rtcp))
    };
    let rtp_port = server_rtp.local_addr().unwrap().port();
    let rtcp_port = server_rtcp.local_addr().unwrap().port();
    let description = rtflow_testkit::description("publish-sr", &[Codec::H264]);
    let response_description = description.clone();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        rtflow::rtsp::Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        rtflow::rtsp::Method::Announce => ServerReply::Response(reply(request, 200, "OK")),
        rtflow::rtsp::Method::Setup => {
            let transport = request.headers.get("transport").cloned().unwrap_or_default();
            let client_port = transport
                .split(';')
                .find_map(|part| part.trim().strip_prefix("client_port="))
                .unwrap_or("0-0");
            ServerReply::Response(with_header(
                with_header(
                    reply(request, 200, "OK"),
                    "session",
                    "sr-session;timeout=60",
                ),
                "transport",
                &format!(
                    "RTP/AVP;unicast;client_port={client_port};server_port={rtp_port}-{rtcp_port};mode=record"
                ),
            ))
        }
        rtflow::rtsp::Method::Record => ServerReply::Response(reply(request, 200, "OK")),
        rtflow::rtsp::Method::Teardown => ServerReply::Response(reply(request, 200, "OK")),
        _ => ServerReply::Response(describe_reply(request, &response_description)),
    }))
    .await;
    let mut publisher = RtspClient::publish_str(
        server.url("/sr").as_str(),
        description,
        ClientOptions::builder()
            .transport(UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    publisher
        .push(rtflow_testkit::unit(0, Codec::H264, 0))
        .await
        .unwrap();

    let mut rtp = [0u8; 2048];
    let (size, _) = tokio::time::timeout(Duration::from_secs(3), server_rtp.recv_from(&mut rtp))
        .await
        .expect("server RTP timeout")
        .unwrap();
    assert!(size >= 12, "short RTP packet");
    assert_eq!(rtp[1] & 0x7f, 96);

    let mut rtcp = [0u8; 2048];
    let (size, _) = tokio::time::timeout(Duration::from_secs(3), server_rtcp.recv_from(&mut rtcp))
        .await
        .expect("server RTCP timeout")
        .unwrap();
    let packets =
        RtcpPacket::parse(bytes::Bytes::copy_from_slice(&rtcp[..size])).expect("parse SR compound");
    assert!(
        packets.iter().any(|packet| matches!(
            packet,
            RtcpPacket::SenderReport { .. } | RtcpPacket::SenderReportWithReports { .. }
        )),
        "no SenderReport in {packets:?}"
    );
    publisher.teardown().await.unwrap();
}
