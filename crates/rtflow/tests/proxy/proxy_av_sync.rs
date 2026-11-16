//! Proxy uses RTCP sender reports to align an audio track that started later
//! than video onto the same wall clock.
use rtflow::{
    MountConfig,
    client::{ClientOptions, RtspClient, UpstreamTransport},
    media::Codec,
    proxy::UpstreamConfig,
    server::{RtspServer, ServerConfig},
};
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread")]
async fn proxy_aligns_late_audio_using_sender_reports() {
    let upstream = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let source = upstream
        .register_mount(
            Arc::from("/av"),
            MountConfig::Local {
                description: rtflow_testkit::description("av", &[Codec::H264, Codec::Pcma]),
            },
        )
        .await
        .unwrap();
    let publisher = source.publisher().unwrap();
    let feeder = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut video = 0u32;
        let mut audio = 0u32;
        loop {
            let elapsed = started.elapsed();
            while elapsed >= Duration::from_millis(u64::from(video) * 33) {
                let mut unit = rtflow_testkit::unit(0, Codec::H264, video);
                unit.pts = Duration::from_millis(u64::from(video) * 33);
                unit.dts = Some(unit.pts);
                unit.rtp_timestamp = 90_000u32.wrapping_add(video.wrapping_mul(2_970));
                if publisher.try_push(unit).is_err() {
                    return;
                }
                video += 1;
            }
            if elapsed >= Duration::from_secs(2)
                && elapsed >= Duration::from_secs(2) + Duration::from_millis(u64::from(audio) * 20)
            {
                let mut unit = rtflow_testkit::unit(1, Codec::Pcma, audio);
                unit.pts = Duration::from_secs(2) + Duration::from_millis(u64::from(audio) * 20);
                unit.dts = Some(unit.pts);
                unit.rtp_timestamp = 8_000u32.wrapping_add(audio.wrapping_mul(160));
                if publisher.try_push(unit).is_err() {
                    return;
                }
                audio += 1;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    proxy
        .register_mount(
            Arc::from("/proxy"),
            MountConfig::Proxy {
                upstream: Box::new(UpstreamConfig::new(
                    url::Url::parse(&format!("rtsp://{}/av", upstream.local_addr())).unwrap(),
                    UpstreamTransport::TcpInterleaved,
                )),
            },
        )
        .await
        .unwrap();
    let mut downstream = RtspClient::connect(
        format!("rtsp://{}/proxy", proxy.local_addr()),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .expect("proxy connect");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(9);
    let mut last_video = None;
    let mut last_audio = None;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(unit))) =
            tokio::time::timeout(Duration::from_millis(500), downstream.next()).await
        else {
            continue;
        };
        match unit.track_id {
            0 => last_video = Some(unit.pts),
            1 => last_audio = Some(unit.pts),
            _ => {}
        }
    }
    let video = last_video.expect("video frames");
    let audio = last_audio.expect("audio frames");
    let difference = video.abs_diff(audio);
    assert!(
        difference < Duration::from_millis(500),
        "audio/video PTS diverged: video={video:?} audio={audio:?}"
    );

    feeder.abort();
    let _ = downstream.teardown().await;
    proxy.shutdown().await.unwrap();
    upstream.shutdown().await.unwrap();
}
