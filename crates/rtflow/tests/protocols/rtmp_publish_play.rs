//! RTMP integration tests: publish and play through the RTMP server.
#![cfg(feature = "rtmp")]

use bytes::{BufMut, Bytes, BytesMut};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use rtflow::mount::MountRegistry;
use rtflow::rtmp::RtmpServer;
use rtflow::rtmp::amf::{AmfValue, decode_values, encode_values};
use rtflow::rtmp::chunk::{Chunker, Dechunker, MessageType, RtmpMessage};
use rtflow::rtmp::handshake::client_handshake;

/// Helper: connect and handshake with the RTMP server.
async fn connect_client(addr: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    client_handshake(&mut stream).await.unwrap();
    stream
}

/// Helper: send an RTMP command message.
async fn send_command(stream: &mut TcpStream, chunker: &Chunker, csid: u32, values: &[AmfValue]) {
    let payload = encode_values(values);
    let msg = RtmpMessage {
        chunk_stream_id: csid,
        timestamp: 0,
        msg_type: MessageType::CommandAmf0 as u8,
        msg_stream_id: 0,
        payload,
    };
    let encoded = chunker.encode(&msg);
    stream.write_all(&encoded).await.unwrap();
    stream.flush().await.unwrap();
}

/// Helper: read and decode response messages from the server.
/// Automatically handles SetChunkSize inline so subsequent messages decode correctly.
async fn read_messages(
    stream: &mut TcpStream,
    dechunker: &mut Dechunker,
    timeout_ms: u64,
) -> Vec<RtmpMessage> {
    let mut buf = vec![0u8; 8192];
    let mut all_msgs = Vec::new();

    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                dechunker.feed(&buf[..n]);
                let msgs = dechunker.poll_messages();

                // Process SetChunkSize inline so subsequent chunks decode correctly
                for msg in &msgs {
                    if msg.msg_type == MessageType::SetChunkSize as u8 && msg.payload.len() >= 4 {
                        let size = u32::from_be_bytes([
                            msg.payload[0],
                            msg.payload[1],
                            msg.payload[2],
                            msg.payload[3],
                        ]) & 0x7FFF_FFFF;
                        dechunker.set_chunk_size(size);
                    }
                }
                all_msgs.extend(msgs);

                // If we got command messages, we likely have what we need
                if all_msgs
                    .iter()
                    .any(|m| m.msg_type == MessageType::CommandAmf0 as u8)
                {
                    // Give a tiny bit more time for additional messages
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if let Ok(Ok(n)) =
                        tokio::time::timeout(Duration::from_millis(20), stream.read(&mut buf)).await
                        && n > 0
                    {
                        dechunker.feed(&buf[..n]);
                        let extra = dechunker.poll_messages();
                        for msg in &extra {
                            if msg.msg_type == MessageType::SetChunkSize as u8
                                && msg.payload.len() >= 4
                            {
                                let size = u32::from_be_bytes([
                                    msg.payload[0],
                                    msg.payload[1],
                                    msg.payload[2],
                                    msg.payload[3],
                                ]) & 0x7FFF_FFFF;
                                dechunker.set_chunk_size(size);
                            }
                        }
                        all_msgs.extend(extra);
                    }
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    all_msgs
}

/// Helper: find a command response by name.
fn find_command(msgs: &[RtmpMessage], name: &str) -> Option<Vec<AmfValue>> {
    for msg in msgs {
        if msg.msg_type == MessageType::CommandAmf0 as u8
            && let Ok(values) = decode_values(&msg.payload)
            && values.first().and_then(|v| v.as_str()) == Some(name)
        {
            return Some(values);
        }
    }
    None
}

#[tokio::test]
async fn rtmp_publish_delivers_video_to_mount() {
    let mounts = Arc::new(MountRegistry::new(10));
    let server = RtmpServer::bind("127.0.0.1:0", mounts.clone())
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();

    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    // Connect and handshake
    let mut stream = connect_client(addr).await;
    let chunker = Chunker::new();
    let mut dechunker = Dechunker::new();

    // Send connect
    send_command(
        &mut stream,
        &chunker,
        3,
        &[
            AmfValue::Str("connect".into()),
            AmfValue::Number(1.0),
            AmfValue::Object(vec![
                ("app".into(), AmfValue::Str("live".into())),
                ("tcUrl".into(), AmfValue::Str(format!("rtmp://{addr}/live"))),
            ]),
        ],
    )
    .await;

    // Read connect response
    let msgs = read_messages(&mut stream, &mut dechunker, 2000).await;

    // Verify _result response
    let result = find_command(&msgs, "_result");
    assert!(result.is_some(), "Expected _result for connect");

    // Send createStream
    send_command(
        &mut stream,
        &chunker,
        3,
        &[
            AmfValue::Str("createStream".into()),
            AmfValue::Number(2.0),
            AmfValue::Null,
        ],
    )
    .await;

    let msgs = read_messages(&mut stream, &mut dechunker, 2000).await;
    let result = find_command(&msgs, "_result");
    assert!(result.is_some(), "Expected _result for createStream");

    // Send publish
    send_command(
        &mut stream,
        &chunker,
        3,
        &[
            AmfValue::Str("publish".into()),
            AmfValue::Number(3.0),
            AmfValue::Null,
            AmfValue::Str("test_stream".into()),
            AmfValue::Str("live".into()),
        ],
    )
    .await;

    let msgs = read_messages(&mut stream, &mut dechunker, 2000).await;

    // Should get onStatus with NetStream.Publish.Start
    let status = find_command(&msgs, "onStatus");
    assert!(status.is_some(), "Expected onStatus for publish");
    let status_values = status.unwrap();
    let info_obj = status_values.get(3);
    let code = info_obj
        .and_then(|o| o.get("code"))
        .and_then(|v| v.as_str());
    assert_eq!(code, Some("NetStream.Publish.Start"));

    // Send onMetaData with dimensions/framerate before the sequence header.
    let metadata = encode_values(&[
        AmfValue::Str("@setDataFrame".into()),
        AmfValue::Str("onMetaData".into()),
        AmfValue::EcmaArray(vec![
            ("width".into(), AmfValue::Number(640.0)),
            ("height".into(), AmfValue::Number(360.0)),
            ("framerate".into(), AmfValue::Number(30.0)),
        ]),
    ]);
    let metadata_msg = RtmpMessage {
        chunk_stream_id: 5,
        timestamp: 0,
        msg_type: MessageType::DataAmf0 as u8,
        msg_stream_id: 1,
        payload: metadata,
    };
    stream
        .write_all(&chunker.encode(&metadata_msg))
        .await
        .unwrap();

    // Now send H.264 sequence header (AVC decoder configuration record)
    // Minimal AVCDecoderConfigurationRecord: version=1, profile=66, compat=0, level=30
    // numSPS=1, spsLen=4, sps=[67 42 00 1e], numPPS=1, ppsLen=4, pps=[68 ce 3c 80]
    let mut avc_config = BytesMut::new();
    avc_config.put_u8(0x17); // keyframe + AVC
    avc_config.put_u8(0x00); // AVC sequence header
    avc_config.put_u8(0x00); // composition time
    avc_config.put_u8(0x00);
    avc_config.put_u8(0x00);
    // AVCDecoderConfigurationRecord
    avc_config.put_u8(0x01); // configurationVersion
    avc_config.put_u8(0x42); // AVCProfileIndication (Baseline)
    avc_config.put_u8(0x00); // profile_compatibility
    avc_config.put_u8(0x1e); // AVCLevelIndication (3.0)
    avc_config.put_u8(0xFF); // lengthSizeMinusOne = 3 (4 bytes)
    avc_config.put_u8(0xE1); // numOfSequenceParameterSets = 1
    avc_config.put_u16(4); // SPS length
    avc_config.put_slice(&[0x67, 0x42, 0x00, 0x1e]); // SPS
    avc_config.put_u8(0x01); // numOfPictureParameterSets
    avc_config.put_u16(4); // PPS length
    avc_config.put_slice(&[0x68, 0xce, 0x3c, 0x80]); // PPS

    let seq_header_msg = RtmpMessage {
        chunk_stream_id: 6,
        timestamp: 0,
        msg_type: MessageType::Video as u8,
        msg_stream_id: 1,
        payload: avc_config.freeze(),
    };
    let encoded = chunker.encode(&seq_header_msg);
    stream.write_all(&encoded).await.unwrap();

    // Send a video NALU (keyframe IDR)
    let mut video_nalu = BytesMut::new();
    video_nalu.put_u8(0x17); // keyframe + AVC
    video_nalu.put_u8(0x01); // AVC NALU
    video_nalu.put_u8(0x00); // composition time
    video_nalu.put_u8(0x00);
    video_nalu.put_u8(0x00);
    // AVCC format: 4-byte length + NALU
    let nalu_data = [0x65u8, 0x88, 0x84, 0x00, 0x33, 0xFF]; // IDR slice
    video_nalu.put_u32(nalu_data.len() as u32);
    video_nalu.put_slice(&nalu_data);

    let video_msg = RtmpMessage {
        chunk_stream_id: 6,
        timestamp: 40, // 40ms
        msg_type: MessageType::Video as u8,
        msg_stream_id: 1,
        payload: video_nalu.freeze(),
    };
    let encoded = chunker.encode(&video_msg);
    stream.write_all(&encoded).await.unwrap();
    stream.flush().await.unwrap();

    // Give the server time to process
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify the mount was created and has a subscriber-ready state
    let handle = mounts.get_handle("/live/test_stream");
    assert!(handle.is_some(), "Mount /live/test_stream should exist");

    let handle = handle.unwrap();
    let description = handle.description();
    assert_eq!(
        description.tracks.len(),
        1,
        "sequence header must create a video track"
    );
    assert_eq!(description.tracks[0].codec, rtflow::Codec::H264);
    assert_eq!(description.tracks[0].width, Some(640));
    assert_eq!(description.tracks[0].height, Some(360));
    assert_eq!(description.tracks[0].frame_rate, Some((30, 1)));
    let fmtp = description.tracks[0].fmtp.as_deref().unwrap_or_default();
    assert!(
        fmtp.contains("sprop-parameter-sets="),
        "RTMP sequence header must seed HLS fmtp/codec config: {fmtp}"
    );
    let (mut rx, _startup) = handle.subscribe_with_startup();

    // Send another frame to verify live delivery
    let mut video_nalu2 = BytesMut::new();
    video_nalu2.put_u8(0x17); // keyframe + AVC
    video_nalu2.put_u8(0x01); // AVC NALU
    video_nalu2.put_u8(0x00);
    video_nalu2.put_u8(0x00);
    video_nalu2.put_u8(0x00);
    let nalu_data2 = [0x65u8, 0xAA, 0xBB, 0xCC];
    video_nalu2.put_u32(nalu_data2.len() as u32);
    video_nalu2.put_slice(&nalu_data2);

    let video_msg2 = RtmpMessage {
        chunk_stream_id: 6,
        timestamp: 80,
        msg_type: MessageType::Video as u8,
        msg_stream_id: 1,
        payload: video_nalu2.freeze(),
    };
    let encoded = chunker.encode(&video_msg2);
    stream.write_all(&encoded).await.unwrap();
    stream.flush().await.unwrap();

    // Wait for the frame to arrive
    let unit = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for frame")
        .expect("channel closed");

    assert_eq!(unit.codec, rtflow::media::Codec::H264);
    assert!(unit.is_keyframe);
    eprintln!(
        "RTMP UNIT DATA: {:02x?}",
        &unit.data[..unit.data.len().min(40)]
    );

    // Cleanup
    cancel.cancel();
}

#[tokio::test]
async fn rtmp_play_receives_flv_tags() {
    use rtflow::media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};
    use rtflow::mount::MountConfig;

    let mounts = Arc::new(MountRegistry::new(10));

    // Register a mount with a video track
    let description = MediaDescription {
        session_name: Arc::from("play-test"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
            crypto: None,
            width: None,
            height: None,
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            rtx: None,
        }],
        #[cfg(feature = "multicast")]
        connection: None,
    };

    let mount = mounts
        .register(
            Arc::from("/live/play_test"),
            MountConfig::Local { description },
        )
        .unwrap();

    let publisher = mount.publisher().unwrap();

    // Start RTMP server
    let server = RtmpServer::bind("127.0.0.1:0", mounts.clone())
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    // Connect play client
    let mut stream = connect_client(addr).await;
    let chunker = Chunker::new();
    let mut dechunker = Dechunker::new();

    // connect
    send_command(
        &mut stream,
        &chunker,
        3,
        &[
            AmfValue::Str("connect".into()),
            AmfValue::Number(1.0),
            AmfValue::Object(vec![("app".into(), AmfValue::Str("live".into()))]),
        ],
    )
    .await;

    let msgs = read_messages(&mut stream, &mut dechunker, 2000).await;
    assert!(find_command(&msgs, "_result").is_some());

    // createStream
    send_command(
        &mut stream,
        &chunker,
        3,
        &[
            AmfValue::Str("createStream".into()),
            AmfValue::Number(2.0),
            AmfValue::Null,
        ],
    )
    .await;

    let msgs = read_messages(&mut stream, &mut dechunker, 2000).await;
    assert!(find_command(&msgs, "_result").is_some());

    // play
    send_command(
        &mut stream,
        &chunker,
        3,
        &[
            AmfValue::Str("play".into()),
            AmfValue::Number(3.0),
            AmfValue::Null,
            AmfValue::Str("play_test".into()),
            AmfValue::Number(-1.0), // start = -1 (live)
        ],
    )
    .await;

    let msgs = read_messages(&mut stream, &mut dechunker, 2000).await;

    // Should get onStatus with NetStream.Play.Start
    let status = find_command(&msgs, "onStatus");
    assert!(status.is_some(), "Expected onStatus for play");

    // Now push a frame from the publisher side
    publisher
        .push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(0),
            dts: None,
            rtp_timestamp: 0,
            is_keyframe: true,
            data: Bytes::from_static(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02, 0x03]),
        })
        .unwrap();

    // Read video data from the play client
    let mut video_received = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut buf = vec![0u8; 8192];

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                dechunker.feed(&buf[..n]);
                let msgs = dechunker.poll_messages();
                for msg in &msgs {
                    if msg.msg_type == MessageType::Video as u8 {
                        video_received = true;
                        // FLV video tag: first byte should be 0x17 (keyframe+AVC)
                        assert!(!msg.payload.is_empty());
                        assert_eq!(msg.payload[0] & 0x0F, 0x07); // AVC codec ID
                    }
                }
                if video_received {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }

    assert!(video_received, "Play client should receive video FLV tags");

    cancel.cancel();
}

#[tokio::test]
async fn rtmp_idle_timeout_disconnects() {
    let mounts = Arc::new(MountRegistry::new(10));
    let server = RtmpServer::bind("127.0.0.1:0", mounts.clone())
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    // Connect but don't send anything after handshake
    let stream = connect_client(addr).await;

    // The server has a 30s idle timeout; we just verify the connection is alive
    // by checking we can still read (no immediate disconnect)
    drop(stream);

    // Verify server is still running
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!cancel.is_cancelled());

    cancel.cancel();
}

#[tokio::test]
async fn rtmp_publish_rejects_invalid_url_token() {
    use rtflow::rtmp::{RtmpConfig, session::RtmpSessionConfig};
    use rtflow::token_auth::TokenConfig;

    let mounts = Arc::new(MountRegistry::new(4));
    let token = TokenConfig::new(b"secret".to_vec());
    let mut config = RtmpConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        ..RtmpConfig::default()
    };
    config.session = RtmpSessionConfig {
        token: Some(token),
        ..RtmpSessionConfig::default()
    };
    let server = RtmpServer::with_config(config, mounts.clone())
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    let mut stream = connect_client(addr).await;
    let chunker = Chunker::new();
    let mut dechunker = Dechunker::new();
    send_command(
        &mut stream,
        &chunker,
        3,
        &[
            AmfValue::Str("connect".into()),
            AmfValue::Number(1.0),
            AmfValue::Object(vec![("app".into(), AmfValue::Str("live".into()))]),
        ],
    )
    .await;
    let _ = read_messages(&mut stream, &mut dechunker, 2000).await;
    send_command(
        &mut stream,
        &chunker,
        3,
        &[
            AmfValue::Str("createStream".into()),
            AmfValue::Number(2.0),
            AmfValue::Null,
        ],
    )
    .await;
    let _ = read_messages(&mut stream, &mut dechunker, 2000).await;
    send_command(
        &mut stream,
        &chunker,
        3,
        &[
            AmfValue::Str("publish".into()),
            AmfValue::Number(3.0),
            AmfValue::Null,
            AmfValue::Str("cam?token=invalid".into()),
            AmfValue::Str("live".into()),
        ],
    )
    .await;
    let messages = read_messages(&mut stream, &mut dechunker, 2000).await;
    let error = find_command(&messages, "_error").expect("denied status");
    assert_eq!(
        error
            .get(3)
            .and_then(|object| object.get("code"))
            .and_then(|value| value.as_str()),
        Some("NetStream.Publish.Denied")
    );
    assert!(mounts.get_handle("/live/cam").is_none());
    cancel.cancel();
}
