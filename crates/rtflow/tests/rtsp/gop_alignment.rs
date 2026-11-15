//! GOP-alignment contract for low-latency startup (surveillance-camera
//! behavior): a player joining mid-GOP must not replay from an old keyframe
//! (that keeps it a full GOP behind the live edge); it waits for the next
//! IDR instead. When the startup window does contain a keyframe, playback
//! starts from it immediately.
#![allow(dead_code)]
use rtflow::client::{ClientOptions, RtspClient};
use rtflow::media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};
use rtflow::transport::UpstreamTransport;
use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

fn description() -> MediaDescription {
    MediaDescription {
        session_name: "gop-alignment".into(),
        #[cfg(feature = "multicast")]
        connection: None,
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: Some(Arc::from("packetization-mode=1")),
            control: "track0".into(),
            width: Some(320),
            height: Some(240),
            frame_rate: Some((30, 1)),
            #[cfg(feature = "srtp")]
            crypto: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            rtx: None,
        }],
    }
}

fn frame(index: u64, is_keyframe: bool) -> AccessUnit {
    AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::from_micros(index * 1_000_000 / 30),
        dts: None,
        rtp_timestamp: (index * 3_000) as u32,
        is_keyframe,
        data: if is_keyframe {
            vec![0, 0, 0, 1, 0x65, 0x88, 0x84].into()
        } else {
            vec![0, 0, 0, 1, 0x41, 0x9a].into()
        },
    }
}

/// Joining mid-GOP: the first delivered unit must be a keyframe that is
/// *newer* than the GOP start — i.e. the player waited for the next IDR
/// instead of replaying an old GOP from the startup window.
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
}

#[tokio::test]
async fn joining_mid_gop_waits_for_next_idr() {
    init_tracing();
    let server = RtspServer::bind(
        ServerConfig::builder("127.0.0.1:0".parse().unwrap())
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let mount = server
        .register_mount(
            "/gop",
            MountConfig::Publisher {
                description: description(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();

    // GOP = 30 frames (1s), STARTUP_WINDOW = 500ms: after 3s of pushing
    // without subscribers the startup cache stays empty (the last keyframe
    // at index 60 is older than the window) — a joining player must wait
    // for the next IDR (index 120) instead of replaying the old GOP.
    for index in 0..90 {
        let is_keyframe = index % 30 == 0;
        let _ = publisher.try_push(frame(index, is_keyframe));
    }
    // One more P frame so the newest frame is mid-GOP (next IDR at 120).
    let _ = publisher.try_push(frame(90, false));

    // Keep pushing while the client waits for the next IDR (index 120).
    // LocalPublisher is not Clone: create a second publisher handle.
    let pusher_publisher = mount.publisher().unwrap();
    let pusher = tokio::spawn(async move {
        for index in 91..180u64 {
            tokio::time::sleep(Duration::from_millis(33)).await;
            let _ = pusher_publisher.try_push(frame(index, index % 30 == 0));
        }
    });

    let mut client = RtspClient::connect(
        Url::parse(&format!("rtsp://{}/gop", server.local_addr())).unwrap(),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .keep_alive_interval(None)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();

    let first = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("frame within timeout")
        .unwrap()
        .unwrap();
    let _ = pusher.await;
    assert!(
        first.is_keyframe,
        "first delivered unit must be a keyframe, got keyframe={}",
        first.is_keyframe
    );
    // It must be the *next* IDR (index 120), not the old GOP start (index 90
    // is a P frame; index 60 was the last keyframe before the join):
    // index 120 → pts ≈ 4.0s.
    let pts_secs = first.pts.as_secs_f64();
    assert!(
        (pts_secs - 4.0).abs() < 0.1,
        "expected the next IDR at ~4s, got pts={pts_secs:.2}s"
    );

    drop(client);
    server.shutdown().await.unwrap();
}

/// Joining with a keyframe inside the startup window: playback starts from
/// that keyframe immediately (no waiting for the next GOP).
#[tokio::test]
async fn joining_near_keyframe_starts_immediately() {
    init_tracing();
    let server = RtspServer::bind(
        ServerConfig::builder("127.0.0.1:0".parse().unwrap())
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let mount = server
        .register_mount(
            "/gop2",
            MountConfig::Publisher {
                description: description(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();

    // A subscriber must be attached while pushing for the startup cache
    // to fill: keep a warm client connected.
    let _warm = RtspClient::connect(
        Url::parse(&format!("rtsp://{}/gop2", server.local_addr())).unwrap(),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .keep_alive_interval(None)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();

    // Push a keyframe, then only 5 P frames (all within the 500ms window).
    for index in 0..=5 {
        assert!(
            publisher.try_push(frame(index, index == 0)).unwrap()
                == rtflow::mount::PushResult::Accepted
        );
    }

    // A second client joining now starts from the cached window keyframe.
    let mut client = RtspClient::connect(
        Url::parse(&format!("rtsp://{}/gop2", server.local_addr())).unwrap(),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .keep_alive_interval(None)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();

    let first = tokio::time::timeout(Duration::from_secs(3), client.next())
        .await
        .expect("frame within timeout")
        .unwrap()
        .unwrap();
    assert!(first.is_keyframe, "first unit must be the cached keyframe");
    assert!(
        first.pts.as_millis() < 500,
        "startup should begin at the window keyframe (pts ~0), got {:?}",
        first.pts
    );

    drop(client);
    server.shutdown().await.unwrap();
}
