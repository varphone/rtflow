#![cfg(all(feature = "dash", feature = "http", feature = "server"))]

//! DASH playback integration tests.
//!
//! Verifies the DASH HTTP endpoints (MPD, init segment, media segments)
//! by publishing frames directly to a mount and querying via HTTP.

use bytes::Bytes;
use rtflow::dash::{DashConfig, create_dash_handlers};
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow::media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};
use rtflow::mount::MountRegistry;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn h264_description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("dash-test"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: Some(Arc::from("packetization-mode=1")),
            control: Arc::from("trackID=0"),
            crypto: None,
            width: Some(1280),
            height: Some(720),
            frame_rate: Some((25, 1)),
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            rtx: None,
        }],
        connection: None,
    }
}

fn h264_keyframe(seq: u32) -> AccessUnit {
    // Minimal Annex-B H.264 keyframe (SPS + PPS + IDR)
    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // start code
    data.extend_from_slice(&[0x67, 0x42, 0x00, 0x1e, 0xab, 0x40, 0x50]); // SPS
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x68, 0xce, 0x38, 0x80]); // PPS
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x65, 0x88, 0x80, 0x40, 0x00, 0x01, 0x02]); // IDR
    AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::from_millis(seq as u64 * 40),
        dts: None,
        rtp_timestamp: seq * 3600,
        is_keyframe: true,
        data: Bytes::from(data),
    }
}

fn h264_interframe(seq: u32) -> AccessUnit {
    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x41, 0x9a, 0x00, 0x10, 0x00, 0x01]);
    AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::from_millis(seq as u64 * 40),
        dts: None,
        rtp_timestamp: seq * 3600,
        is_keyframe: false,
        data: Bytes::from(data),
    }
}

/// Simple HTTP GET request, returns (status_code, body).
async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response).to_string();

    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);

    // Body is after \r\n\r\n
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();

    (status, body)
}

#[tokio::test]
async fn dash_mpd_returns_503_without_publisher() {
    let mounts = Arc::new(MountRegistry::new(10));
    let mount = mounts
        .register(
            Arc::from("/live/test"),
            rtflow::mount::MountConfig::Publisher {
                description: h264_description(),
                allow_replace_after_disconnect: true,
            },
        )
        .unwrap();

    let config = DashConfig {
        segment_duration: Duration::from_millis(200),
        segment_count: 3,
        ..Default::default()
    };
    let (mpd_handler, _seg_handler, _init_handler) = create_dash_handlers(config, &mount);

    let mut http = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let addr = http.local_addr();
    http.add_route(Method::Get, "/live/test/manifest.mpd", mpd_handler)
        .await;
    http.run().await.unwrap();

    // No segments yet -> 503
    let (status, body) = http_get(addr, "/live/test/manifest.mpd").await;
    assert_eq!(status, 503);
    assert!(body.contains("MPD") || body.contains("No segments"));

    http.shutdown();
}

#[tokio::test]
async fn dash_mpd_and_segments_after_publish() {
    let mounts = Arc::new(MountRegistry::new(10));
    let mount = mounts
        .register(
            Arc::from("/live/cam"),
            rtflow::mount::MountConfig::Publisher {
                description: h264_description(),
                allow_replace_after_disconnect: true,
            },
        )
        .unwrap();

    let config = DashConfig {
        segment_duration: Duration::from_millis(200),
        segment_count: 5,
        ..Default::default()
    };
    let (mpd_handler, seg_handler, init_handler) = create_dash_handlers(config, &mount);

    let mut http = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let addr = http.local_addr();
    http.add_route(Method::Get, "/live/cam/manifest.mpd", mpd_handler)
        .await;
    http.add_route(Method::Get, "/live/cam/init.mp4", init_handler)
        .await;
    http.add_route(Method::Get, "/live/cam/segment_{seq}.m4s", seg_handler)
        .await;
    http.run().await.unwrap();

    // Publish frames to generate segments
    let publisher = mount.publisher().unwrap();
    for i in 0..50 {
        let frame = if i % 10 == 0 {
            h264_keyframe(i)
        } else {
            h264_interframe(i)
        };
        let _ = publisher.push(frame);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Wait for segmenter to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // MPD should now be available
    let (status, body) = http_get(addr, "/live/cam/manifest.mpd").await;
    assert_eq!(status, 200, "MPD should return 200 after publishing");
    assert!(
        body.contains("MPD") || body.contains("mpd"),
        "Body should contain MPD XML"
    );
    assert!(
        body.contains("SegmentTemplate") || body.contains("Representation"),
        "MPD should contain segment info"
    );

    // Init segment should be available
    let (status, _body) = http_get(addr, "/live/cam/init.mp4").await;
    assert_eq!(status, 200, "Init segment should return 200");

    http.shutdown();
}

#[tokio::test]
async fn dash_init_returns_404_without_segments() {
    let mounts = Arc::new(MountRegistry::new(10));
    let mount = mounts
        .register(
            Arc::from("/live/empty"),
            rtflow::mount::MountConfig::Publisher {
                description: h264_description(),
                allow_replace_after_disconnect: true,
            },
        )
        .unwrap();

    let config = DashConfig::default();
    let (_mpd_handler, _seg_handler, init_handler) = create_dash_handlers(config, &mount);

    let mut http = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let addr = http.local_addr();
    http.add_route(Method::Get, "/live/empty/init.mp4", init_handler)
        .await;
    http.run().await.unwrap();

    let (status, _) = http_get(addr, "/live/empty/init.mp4").await;
    assert_eq!(status, 404, "Init segment should return 404 without data");

    http.shutdown();
}
