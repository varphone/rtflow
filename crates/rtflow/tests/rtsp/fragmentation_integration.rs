use bytes::Bytes;
use rtflow::{
    AccessUnit, Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
};
use rtflow_testkit::{description, wait_until, within};
use std::{sync::Arc, time::Duration};

async fn round_trip(codec: Codec, transport: UpstreamTransport) {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/fragmented"),
            MountConfig::Local {
                description: description("fragmented", &[codec]),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let mut client = RtspClient::connect(
        url::Url::parse(&format!("rtsp://{}/fragmented", server.local_addr())).unwrap(),
        ClientOptions::builder()
            .transport(transport)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    wait_until(Duration::from_secs(1), || {
        mount.snapshot().subscriber_count > 0
    })
    .await;

    let mut payload = vec![0x5a; 8 * 1024];
    match codec {
        Codec::H264 => payload[0] = 0x65,
        Codec::H265 => {
            payload[0] = 0x26;
            payload[1] = 0x01;
        }
        _ => unreachable!(),
    }
    // Prepend Annex-B start code so the pushed frame matches the format the
    // depacketizer now consistently produces.
    let mut data = vec![0u8, 0, 0, 1];
    data.extend_from_slice(&payload);
    let expected = AccessUnit {
        nal_offsets: None,
        track_id: 0,
        codec,
        pts: Duration::ZERO,
        dts: None,
        rtp_timestamp: 90_000,
        is_keyframe: true,
        data: Bytes::from(data),
    };
    publisher.try_push(expected.clone()).unwrap();
    let received = within(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.data, expected.data);
    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn large_h264_and_h265_units_fragment_and_reassemble() {
    for transport in [UpstreamTransport::Udp, UpstreamTransport::TcpInterleaved] {
        for codec in [Codec::H264, Codec::H265] {
            round_trip(codec, transport).await;
        }
    }
}
