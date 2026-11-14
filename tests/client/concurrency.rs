//! Concurrent RTSP clients: each connection must own its control/media state
//! and make progress independently.
use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    transport::UpstreamTransport,
};
use rtflow_testkit::description;
use std::{sync::Arc, time::Duration};
use tokio::task::JoinSet;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thirty_two_clients_pull_independently() {
    const CLIENTS: usize = 32;
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let media = description("concurrent", &[Codec::H264]);
    let mount = server
        .register_mount(
            Arc::from("/live"),
            MountConfig::Local { description: media },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let feed = tokio::spawn(async move {
        let mut sequence = 0u32;
        loop {
            if publisher
                .push(rtflow_testkit::unit(0, Codec::H264, sequence))
                .is_err()
            {
                return;
            }
            sequence = sequence.wrapping_add(1);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let url = format!("rtsp://{}/live", server.local_addr());
    let mut tasks = JoinSet::new();
    for index in 0..CLIENTS {
        let url = url.clone();
        tasks.spawn(async move {
            let mut options = ClientOptions::builder()
                .transport(UpstreamTransport::TcpInterleaved)
                .output_queue_capacity(8)
                .build()
                .unwrap();
            if index % 2 == 0 {
                options = ClientOptions::low_memory();
                options.transport = UpstreamTransport::TcpInterleaved;
            }
            let mut client = RtspClient::connect(&url, options)
                .await
                .unwrap_or_else(|error| panic!("client {index} connect: {error}"));
            let unit = tokio::time::timeout(Duration::from_secs(5), client.next())
                .await
                .unwrap_or_else(|_| panic!("client {index} timed out"))
                .unwrap_or_else(|| panic!("client {index} ended"))
                .unwrap_or_else(|error| panic!("client {index} media: {error}"));
            assert_eq!(unit.codec, Codec::H264);
            client.teardown().await.unwrap();
            index
        });
    }
    let mut finished = 0;
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
        finished += 1;
    }
    assert_eq!(finished, CLIENTS);
    feed.abort();
    let _ = feed.await;
    server.shutdown().await.unwrap();
}
