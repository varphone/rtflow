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
        Codec::Mp3,
        Codec::G726_16,
        Codec::G726_24,
        Codec::G726_32,
        Codec::G726_40,
        Codec::Av1,
        Codec::Jpeg,
        Codec::H263,
        Codec::Mpeg4,
    ] {
        let mut media = description("source", &[codec]);
        if matches!(
            codec,
            Codec::H264
                | Codec::H265
                | Codec::Vp8
                | Codec::Vp9
                | Codec::Av1
                | Codec::Jpeg
                | Codec::H263
                | Codec::Mpeg4
        ) {
            for track in &mut media.tracks {
                track.width = Some(176);
                track.height = Some(144);
            }
        }
        let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let mount = server
            .register_mount(
                Arc::from("/source"),
                MountConfig::Local {
                    description: media.clone(),
                },
            )
            .await
            .unwrap();
        let source = SyntheticSource::spawn(
            mount.publisher().unwrap(),
            media,
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
        assert!(!unit.data.is_empty(), "empty first unit for {codec:?}");
        if unit.kind() == rtflow::media::MediaKind::Video {
            assert!(
                unit.is_keyframe,
                "first video unit is not a keyframe for {codec:?}"
            );
        }
        client.teardown().await.unwrap();
        source.abort();
        let _ = source.await;
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn keyframe_request_forces_a_local_source_idr() {
    let media = description("source-keyframe", &[Codec::H264]);
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/source-keyframe"),
            MountConfig::Local {
                description: media.clone(),
            },
        )
        .await
        .unwrap();
    let mut subscriber = mount.subscribe();
    let source = SyntheticSource::spawn(
        mount.publisher().unwrap(),
        media,
        SyntheticSourceConfig {
            fps: 50,
            keyframe_interval: 50,
        },
    )
    .unwrap();
    let _first = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .unwrap()
        .unwrap();
    mount.request_keyframe(0);
    let mut forced = None;
    for _ in 0..10 {
        let unit = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
            .await
            .unwrap()
            .unwrap();
        if unit.is_keyframe {
            forced = Some(unit);
            break;
        }
    }
    assert!(
        forced.is_some(),
        "keyframe request did not reach the source"
    );
    source.abort();
    let _ = source.await;
    server.shutdown().await.unwrap();
}
