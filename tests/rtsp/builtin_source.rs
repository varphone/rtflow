//! Built-in synthetic source coverage (requires `--features test-source`).
#![cfg(feature = "test-source")]

use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    source::{SyntheticSource, SyntheticSourceConfig},
    transport::UpstreamTransport,
};
use rtflow_testkit::description;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn synthetic_source_feeds_every_codec_over_rtsp() {
    for codec in [
        Codec::H264,
        Codec::H265,
        Codec::Aac,
        Codec::Pcma,
        Codec::Pcmu,
        Codec::Opus,
        Codec::Vp8,
        Codec::Vp9,
    ] {
        let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let mount = server
            .register_mount(
                Arc::from("/source"),
                MountConfig::Local {
                    description: description("source", &[codec]),
                },
            )
            .await
            .unwrap();
        let source = SyntheticSource::spawn(
            mount.publisher().unwrap(),
            description("source", &[codec]),
            SyntheticSourceConfig {
                fps: 50,
                keyframe_interval: 10,
            },
        )
        .unwrap();
        let url = format!("rtsp://{}/source", server.local_addr());
        let mut client = RtspClient::connect_str(
            &url,
            ClientOptions::builder()
                .transport(UpstreamTransport::Udp)
                .build()
                .unwrap(),
        )
        .await
        .unwrap_or_else(|error| panic!("connect {codec:?}: {error}"));
        let unit = tokio::time::timeout(Duration::from_secs(2), async {
            match client.next().await {
                Some(Ok(unit)) => unit,
                Some(Err(error)) => panic!("media error for {codec:?}: {error}"),
                None => panic!("session ended before a frame for {codec:?}"),
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {codec:?}"));
        assert_eq!(unit.codec, codec);
        client.teardown().await.unwrap();
        source.abort();
        let _ = source.await;
        server.shutdown().await.unwrap();
    }
}
