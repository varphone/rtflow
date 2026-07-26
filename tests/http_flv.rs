#![cfg(feature = "http-flv")]

use bytes::Bytes;
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow::http_flv::{create_flv_handler, mux::FlvMuxer};
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig,
    TrackDescription,
};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn h264_description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("test"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
            #[cfg(feature = "srtp")]
            crypto: None,
        }],
        #[cfg(feature = "multicast")]
        connection: None,
    }
}

fn h264_keyframe() -> Bytes {
    // Minimal Annex-B H.264 keyframe: SPS + PPS + IDR
    let mut data = Vec::new();
    // SPS
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x67, 0x42, 0x00, 0x0a, 0xe9, 0x40, 0x20]);
    // PPS
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x68, 0xce, 0x38, 0x80]);
    // IDR
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x65, 0x88, 0x80, 0x40, 0x00]);
    Bytes::from(data)
}

fn h264_interframe() -> Bytes {
    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x41, 0x9a, 0x00, 0x10]);
    Bytes::from(data)
}

#[test]
fn flv_muxer_video_tag_roundtrip() {
    let desc = h264_description();
    let mut muxer = FlvMuxer::new(&desc);

    let header = muxer.flv_header();
    assert_eq!(&header[..3], b"FLV");
    assert_eq!(header[4] & 0x01, 1); // video flag

    let keyframe = AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::from_millis(0),
        dts: None,
        rtp_timestamp: 0,
        is_keyframe: true,
        data: h264_keyframe(),
    };

    // Set sequence header from keyframe
    if let Some(seq) = FlvMuxer::build_avc_sequence_header(&keyframe.data) {
        muxer.set_avc_sequence_header(seq);
    }

    let tags = muxer.encode(&keyframe);
    assert!(!tags.is_empty());
    // First byte should be video tag type (0x09)
    assert_eq!(tags[0], 0x09);
}

#[test]
fn flv_muxer_avc_sequence_header() {
    let keyframe_data = h264_keyframe();
    let seq = FlvMuxer::build_avc_sequence_header(&keyframe_data).unwrap();
    assert_eq!(seq[0], 1); // configurationVersion
    assert_eq!(seq[1], 0x42); // AVCProfileIndication
}

#[test]
fn flv_muxer_annex_b_to_avcc() {
    let desc = h264_description();
    let mut muxer = FlvMuxer::new(&desc);

    let unit = AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::from_millis(100),
        dts: None,
        rtp_timestamp: 9000,
        is_keyframe: true,
        data: h264_keyframe(),
    };

    if let Some(seq) = FlvMuxer::build_avc_sequence_header(&unit.data) {
        muxer.set_avc_sequence_header(seq);
    }

    let tags = muxer.encode(&unit);
    // Should produce sequence header tag + data tag
    assert!(tags.len() > 11); // At least one FLV tag
}

#[tokio::test]
async fn http_flv_e2e_stream() {
    // Setup RTSP server
    let rtsp_server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let description = h264_description();
    let mount = rtsp_server
        .register_mount(
            Arc::from("/test"),
            MountConfig::Local {
                description: description.clone(),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();

    // Setup HTTP server
    let http_config = HttpConfig::new("127.0.0.1:0".parse().unwrap());
    let mut http_server = HttpServer::bind(http_config).await.unwrap();
    let http_addr = http_server.local_addr();

    // Create FLV handler
    let receiver = mount.subscribe();
    let handler = create_flv_handler(description.clone(), receiver);
    http_server.add_route(Method::Get, "/live/test.flv", handler).await;
    http_server.run().await.unwrap();

    // Push a keyframe
    publisher
        .try_push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(0),
            dts: None,
            rtp_timestamp: 0,
            is_keyframe: true,
            data: h264_keyframe(),
        })
        .unwrap();

    // Connect to HTTP-FLV endpoint
    let mut stream = tokio::net::TcpStream::connect(http_addr).await.unwrap();
    stream
        .write_all(b"GET /live/test.flv HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    // Read response
    let mut buf = vec![0u8; 4096];
    let mut total = Vec::new();
    // Give some time for the response
    tokio::time::sleep(Duration::from_millis(100)).await;
    loop {
        match tokio::time::timeout(Duration::from_millis(200), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                total.extend_from_slice(&buf[..n]);
                if total.len() >= 13 {
                    break; // Got enough data
                }
            }
            _ => break,
        }
    }

    // Verify we got HTTP response with FLV data
    let response = String::from_utf8_lossy(&total);
    assert!(response.contains("200 OK") || response.starts_with("HTTP/1.1"));
    assert!(response.contains("Transfer-Encoding: chunked"));
    assert!(response.contains("video/x-flv"));
    // The response body should contain FLV data (chunked encoded)
    // Find the FLV signature in the response body (after chunk headers)
    let has_flv = total.windows(3).any(|w| w == b"FLV");
    assert!(has_flv, "response should contain FLV data");

    http_server.shutdown();
}

#[tokio::test]
async fn http_flv_concurrent_subscribers() {
    let rtsp_server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let description = h264_description();
    let mount = rtsp_server
        .register_mount(
            Arc::from("/test"),
            MountConfig::Local {
                description: description.clone(),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();

    let http_config = HttpConfig::new("127.0.0.1:0".parse().unwrap());
    let mut http_server = HttpServer::bind(http_config).await.unwrap();
    let http_addr = http_server.local_addr();

    // Register two FLV handlers for different paths
    let receiver1 = mount.subscribe();
    let handler1 = create_flv_handler(description.clone(), receiver1);
    http_server.add_route(Method::Get, "/live/stream1.flv", handler1).await;

    let receiver2 = mount.subscribe();
    let handler2 = create_flv_handler(description.clone(), receiver2);
    http_server.add_route(Method::Get, "/live/stream2.flv", handler2).await;

    http_server.run().await.unwrap();

    // Push a frame
    publisher
        .try_push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(0),
            dts: None,
            rtp_timestamp: 0,
            is_keyframe: true,
            data: h264_keyframe(),
        })
        .unwrap();

    // Connect two clients
    let mut stream1 = tokio::net::TcpStream::connect(http_addr).await.unwrap();
    stream1
        .write_all(b"GET /live/stream1.flv HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    let mut stream2 = tokio::net::TcpStream::connect(http_addr).await.unwrap();
    stream2
        .write_all(b"GET /live/stream2.flv HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    // Both should receive data
    let mut buf1 = vec![0u8; 4096];
    let mut buf2 = vec![0u8; 4096];
    tokio::time::sleep(Duration::from_millis(100)).await;

    let n1 = tokio::time::timeout(Duration::from_millis(200), stream1.read(&mut buf1))
        .await
        .unwrap_or(Ok(0))
        .unwrap_or(0);
    let n2 = tokio::time::timeout(Duration::from_millis(200), stream2.read(&mut buf2))
        .await
        .unwrap_or(Ok(0))
        .unwrap_or(0);

    assert!(n1 > 0, "stream1 should receive HTTP response");
    assert!(n2 > 0, "stream2 should receive HTTP response");

    http_server.shutdown();
}
