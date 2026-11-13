use bytes::Bytes;
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig,
    TrackDescription,
    client::{ClientOptions, ClientSession, RtspClient},
    proxy::UpstreamTransport,
};
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_transport_fanout_stays_bounded_under_burst() {
    const CLIENTS: usize = 16;
    const FRAMES: usize = 1_000;

    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.max_sessions = 64;
    config.consumer_queue_capacity = 128;
    let server = RtspServer::bind(config).await.unwrap();
    let mount = server
        .register_mount(
            Arc::from("/stress"),
            MountConfig::Local {
                description: MediaDescription {
                    session_name: Arc::from("stress"),
                    tracks: vec![TrackDescription {
                        id: 0,
                        kind: MediaKind::Video,
                        codec: Codec::H264,
                        clock_rate: 90_000,
                        channels: None,
                        payload_type: 96,
                        fmtp: Some(Arc::from("packetization-mode=1")),
                        control: Arc::from("video"),
                        crypto: None,
                        width: None,
                        height: None,
                        frame_rate: None,
                    }],
                    connection: None,
                },
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = url::Url::parse(&format!("rtsp://{}/stress", server.local_addr())).unwrap();
    let mut clients: Vec<ClientSession> = Vec::with_capacity(CLIENTS);
    for index in 0..CLIENTS {
        clients.push(
            RtspClient::connect(
                url.clone(),
                ClientOptions::builder()
                    .transport(if index % 2 == 0 {
                        UpstreamTransport::Udp
                    } else {
                        UpstreamTransport::TcpInterleaved
                    })
                    .output_queue_capacity(128)
                    .keep_alive_interval(None)
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap(),
        );
    }
    assert_eq!(mount.snapshot().subscriber_count, CLIENTS);

    let payload = Bytes::from(vec![0x65; 900]);
    for index in 0..FRAMES {
        publisher
            .try_push(AccessUnit {
                track_id: 0,
                codec: Codec::H264,
                pts: Duration::from_micros(index as u64 * 33_333),
                dts: None,
                rtp_timestamp: index as u32 * 3_000,
                is_keyframe: true,
                data: payload.clone(),
            })
            .unwrap();
    }

    for client in &mut clients {
        let frame = tokio::time::timeout(Duration::from_secs(3), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(frame.data.len(), payload.len() + 4); // +4 for Annex-B start code
    }
    assert_eq!(mount.snapshot().stats.accepted_frames, FRAMES as u64);

    drop(clients);
    tokio::time::timeout(Duration::from_secs(3), async {
        while mount.snapshot().subscriber_count != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.shutdown().await.unwrap();
}
