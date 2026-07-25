mod support;

use rtsp::{
    Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
};
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "long-running stability gate; set RTSP_SOAK_SECONDS"]
async fn repeated_session_churn_and_fanout_remain_bounded() {
    let seconds = std::env::var("RTSP_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60);
    let mut server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/soak"),
            MountConfig::Local {
                description: support::description("soak", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = url::Url::parse(&format!("rtsp://{}/soak", server.local_addr())).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut sequence = 0u32;
    while tokio::time::Instant::now() < deadline {
        let mut clients = Vec::new();
        for index in 0..8 {
            let options = ClientOptions::builder()
                .transport(if index % 2 == 0 {
                    UpstreamTransport::Udp
                } else {
                    UpstreamTransport::TcpInterleaved
                })
                .build()
                .unwrap();
            clients.push(RtspClient::connect(url.clone(), options).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        publisher
            .try_push(support::unit(0, Codec::H264, sequence))
            .unwrap();
        for client in &mut clients {
            tokio::time::timeout(Duration::from_secs(1), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        sequence = sequence.wrapping_add(1);
        drop(clients);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while mount.snapshot().stats.subscribers != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let report = server.shutdown_with_report().await.unwrap();
    assert_eq!(report.panicked_tasks, 0);
    assert_eq!(report.forced_tasks, 0);
}
