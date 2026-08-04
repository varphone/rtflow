//! Variable frame rate (VFR) end-to-end support: frames with non-uniform
//! PTS spacing (e.g. 30fps bursts, 5fps scenes, a single 2s-gap frame)
//! must be delivered losslessly with RTP timestamps that stay linear in
//! PTS — no frame-rate assumptions anywhere in the publish path.
use rtflow::client::{ClientOptions, RtspClient};
use rtflow::media::{AccessUnit, Codec, MediaDescription, TrackDescription};
use rtflow::protocol::transport::UpstreamTransport;
use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::time::Duration;
use url::Url;

/// VFR frame plan: 30fps burst, then 5fps scene, then a single 2s gap.
fn frame_pts(index: u64) -> Duration {
    match index {
        // 0..30: 30fps (33.33ms)
        0..=29 => Duration::from_micros(index * 33_333),
        // 30..60: 5fps (200ms)
        30..=59 => Duration::from_micros(1_000_000 + (index - 30) * 200_000),
        // 60: single frame after a 2s gap
        60 => Duration::from_micros(7_000_000),
        // afterwards: 15fps (66.66ms)
        _ => Duration::from_micros(7_000_000 + (index - 60) * 66_666),
    }
}

fn frame(index: u64) -> AccessUnit {
    let pts = frame_pts(index);
    AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts,
        dts: None,
        // Published with a VFR-aware clock: delta-based, monotonic.
        rtp_timestamp: (pts.as_micros() as u64 * 90_000 / 1_000_000) as u32,
        is_keyframe: index.is_multiple_of(25),
        data: if index.is_multiple_of(25) {
            vec![0, 0, 0, 1, 0x65, 0x88, 0x84].into()
        } else {
            vec![0, 0, 0, 1, 0x41, 0x9a].into()
        },
    }
}

#[tokio::test]
async fn vfr_frames_are_delivered_losslessly_with_linear_timestamps() {
    let server = RtspServer::bind(
        ServerConfig::builder("127.0.0.1:0".parse().unwrap())
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let mount = server
        .register_mount(
            "/vfr",
            MountConfig::Publisher {
                description: MediaDescription::from_tracks(
                    "vfr",
                    vec![TrackDescription::h264_video(0, &[], &[], 640, 360, 30)],
                ),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();

    let mut client = RtspClient::connect(
        Url::parse(&format!("rtsp://{}/vfr", server.local_addr())).unwrap(),
        ClientOptions {
            transport: UpstreamTransport::TcpInterleaved,
            keep_alive_interval: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Push all 200 VFR frames while the client drains (subscriber attached,
    // so every frame is delivered and the startup cache stays fresh).
    let pusher = tokio::spawn(async move {
        for index in 0..200u64 {
            tokio::time::sleep(Duration::from_millis(2)).await;
            let _ = publisher.try_push(frame(index));
        }
    });

    // Expect all 200 frames, in order, with rtp_timestamp linear in pts.
    let mut last_ts: Option<u32> = None;
    for expected_index in 0..200u64 {
        let unit = tokio::time::timeout(Duration::from_secs(10), client.next())
            .await
            .expect("frame within timeout")
            .unwrap()
            .unwrap();
        assert_eq!(unit.track_id, 0);
        // Keyframe alignment: first delivered unit is an IDR.
        if expected_index == 0 {
            assert!(unit.is_keyframe, "first unit must be a keyframe");
        }
        // Timestamps must be strictly monotonic and match the published
        // linear mapping (pts*90kHz), including across the VFR gaps.
        let expected_ts =
            (frame_pts(expected_index).as_micros() as u64 * 90_000 / 1_000_000) as u32;
        assert_eq!(
            unit.rtp_timestamp, expected_ts,
            "index {expected_index}: timestamp mismatch"
        );
        if let Some(last) = last_ts {
            assert!(
                unit.rtp_timestamp.wrapping_sub(last) > 0,
                "index {expected_index}: timestamp regression"
            );
        }
        last_ts = Some(unit.rtp_timestamp);
    }
    let _ = pusher.await;
    drop(client);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn vfr_startup_snapshot_replays_at_pts_spacing() {
    // A snapshot containing VFR frames (30fps burst + 2s gap) must replay
    // pacing by PTS deltas, not at a fixed rate.
    let server = RtspServer::bind(
        ServerConfig::builder("127.0.0.1:0".parse().unwrap())
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let mount = server
        .register_mount(
            "/vfr2",
            MountConfig::Publisher {
                description: MediaDescription::from_tracks(
                    "vfr2",
                    vec![TrackDescription::h264_video(0, &[], &[], 640, 360, 30)],
                ),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();

    // Warm subscriber so the startup cache fills; feed it one keyframe.
    let mut warm = RtspClient::connect(
        Url::parse(&format!("rtsp://{}/vfr2", server.local_addr())).unwrap(),
        ClientOptions {
            transport: UpstreamTransport::TcpInterleaved,
            keep_alive_interval: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _ = publisher.try_push(frame(0));
    let _ = tokio::time::timeout(Duration::from_secs(3), warm.next())
        .await
        .expect("warm frame within timeout")
        .unwrap()
        .unwrap();

    // Push a VFR window whose last 500ms contains NO keyframe (IDR every
    // 25 frames; frames 61..63 are 15fps and index 75 is far away): a
    // joining player must wait for the next IDR, not replay a stale one.
    for index in 1..64 {
        assert!(publisher.try_push(frame(index)).unwrap() == rtflow::mount::PushResult::Accepted);
    }

    // Second client joins mid-GOP; the next IDR (index 75) arrives only
    // while we keep pushing.
    let mut client = RtspClient::connect(
        Url::parse(&format!("rtsp://{}/vfr2", server.local_addr())).unwrap(),
        ClientOptions {
            transport: UpstreamTransport::TcpInterleaved,
            keep_alive_interval: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let pusher = tokio::spawn(async move {
        for index in 64..90u64 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = publisher.try_push(frame(index));
        }
    });
    let first = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("next IDR within timeout")
        .unwrap()
        .unwrap();
    assert!(first.is_keyframe, "first delivered unit must be a keyframe");
    // The waited-for IDR is index 75: pts = 7s + 15×66666µs = 7999990µs
    // → floor(7999990 × 90000 / 1e6) = 719999 ticks.
    assert_eq!(
        first.rtp_timestamp, 719_999,
        "expected the next IDR after the gap"
    );
    let _ = pusher.await;

    drop(client);
    drop(warm);
    server.shutdown().await.unwrap();
}
