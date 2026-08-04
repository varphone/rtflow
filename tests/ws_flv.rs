//! WS-FLV end-to-end tests: RFC 6455 handshake, FLV stream delivery over
//! WebSocket binary frames, ping/pong handling, and mount path mapping.
#![cfg(feature = "ws-flv")]

use bytes::Bytes;
use rtflow::http_flv::ws::{WsFlvConfig, WsFlvServer};
use rtflow::media::{AccessUnit, Codec, MediaDescription, TrackDescription};
use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn h264_keyframe() -> Bytes {
    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x67, 0x42, 0x00, 0x0a, 0xe9, 0x40, 0x20]);
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x68, 0xce, 0x38, 0x80]);
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x65, 0x88, 0x80, 0x40, 0x00]);
    Bytes::from(data)
}

fn unit(key: bool, seq: u64) -> AccessUnit {
    AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::from_micros(seq * 33_333),
        dts: None,
        rtp_timestamp: (seq * 3_000) as u32,
        is_keyframe: key,
        data: if key {
            h264_keyframe()
        } else {
            Bytes::from(vec![0, 0, 0, 1, 0x41, 0x9a])
        },
    }
}

/// Open a WebSocket connection with a manual RFC 6455 handshake.
/// Returns a BufReader so any bytes coalesced with the 101 response
/// (e.g. the first FLV frame) stay buffered for `read_ws_frame`.
async fn ws_connect(addr: std::net::SocketAddr, path: &str) -> BufReader<TcpStream> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let req = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut reader = BufReader::new(stream);
    // Read response headers line by line (exactly up to the blank line).
    let mut saw_status = false;
    let mut saw_accept = false;
    loop {
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await.unwrap();
        let line = String::from_utf8_lossy(&line);
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.starts_with("HTTP/1.1 101") {
            saw_status = true;
        }
        if trimmed
            .to_ascii_lowercase()
            .starts_with("sec-websocket-accept:")
            && trimmed.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        {
            saw_accept = true;
        }
    }
    assert!(saw_status, "expected HTTP/1.1 101");
    assert!(saw_accept, "accept key mismatch");
    reader
}

/// Read one server frame (unmasked); returns (opcode, payload).
async fn read_ws_frame(stream: &mut BufReader<TcpStream>) -> (u8, Vec<u8>) {
    let mut hdr = [0u8; 2];
    stream.read_exact(&mut hdr).await.unwrap();
    let opcode = hdr[0] & 0x0f;
    assert_eq!(hdr[0] & 0x80, 0x80, "FIN must be set");
    assert_eq!(hdr[1] & 0x80, 0, "server frames are unmasked");
    let mut len = (hdr[1] & 0x7f) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        stream.read_exact(&mut b).await.unwrap();
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        stream.read_exact(&mut b).await.unwrap();
        len = u64::from_be_bytes(b);
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await.unwrap();
    (opcode, payload)
}

/// Send a masked client frame (ping/close).
async fn send_masked_frame(stream: &mut BufReader<TcpStream>, opcode: u8, payload: &[u8]) {
    let stream = stream.get_mut();
    let mask = [0x11, 0x22, 0x33, 0x44];
    let mut hdr = vec![0x80 | opcode, 0x80 | payload.len() as u8];
    hdr.extend_from_slice(&mask);
    let masked: Vec<u8> = payload
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ mask[i % 4])
        .collect();
    hdr.extend_from_slice(&masked);
    stream.write_all(&hdr).await.unwrap();
}

async fn setup() -> (
    RtspServer,
    std::net::SocketAddr,
    rtflow::mount::LocalPublisher,
) {
    setup_with_config(WsFlvConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    })
    .await
}

async fn setup_with_config(
    ws_config: WsFlvConfig,
) -> (
    RtspServer,
    std::net::SocketAddr,
    rtflow::mount::LocalPublisher,
) {
    let server = RtspServer::bind(
        ServerConfig::builder("127.0.0.1:0".parse().unwrap())
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let mount = server
        .register_mount(
            "/live/stream",
            MountConfig::Publisher {
                description: MediaDescription::from_tracks(
                    "ws-flv-test",
                    vec![TrackDescription::h264_video(0, &[], &[], 640, 360, 30)],
                ),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();

    let ws = WsFlvServer::bind(ws_config, server.registry())
        .await
        .unwrap();
    let ws_addr = ws.local_addr();
    tokio::spawn(async move { ws.run().await });
    // Give the accept loop a moment to start.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (server, ws_addr, publisher)
}

#[tokio::test]
async fn handshake_and_flv_stream_delivery() {
    let (server, ws_addr, publisher) = setup().await;

    // Publish a keyframe before connecting (startup cache), then connect.
    let _ = publisher.try_push(unit(true, 0));
    let _ = publisher.try_push(unit(false, 1));
    let mut stream = ws_connect(ws_addr, "/live/stream.flv").await;

    // Frame 1: FLV header ("FLV" magic).
    let (op, payload) = read_ws_frame(&mut stream).await;
    assert_eq!(op, 0x2, "binary frame");
    assert_eq!(&payload[..3], b"FLV");
    assert!(payload.len() >= 13, "FLV header is 13 bytes");

    // Frame 2: onMetaData.
    let (op, payload) = read_ws_frame(&mut stream).await;
    assert_eq!(op, 0x2);
    // Script tag: type 18.
    assert_eq!(payload[0], 18, "metadata tag type");

    // Frames 3+: video tags (type 9) — keyframe then delta.
    let (op, payload) = read_ws_frame(&mut stream).await;
    assert_eq!(op, 0x2);
    assert_eq!(payload[0], 9, "video tag");
    let (op, _payload) = read_ws_frame(&mut stream).await;
    assert_eq!(op, 0x2);

    // Live frame after the startup snapshot.
    let _ = publisher.try_push(unit(false, 2));
    let (op, payload) = read_ws_frame(&mut stream).await;
    assert_eq!(op, 0x2);
    assert_eq!(payload[0], 9, "live video tag");

    drop(stream);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ping_gets_pong_and_missing_mount_404s() {
    let (server, ws_addr, publisher) = setup().await;
    let _ = publisher.try_push(unit(true, 0));

    let mut stream = ws_connect(ws_addr, "/live/stream.flv").await;
    // Drain the FLV header/metadata frames.
    let _ = read_ws_frame(&mut stream).await;
    let _ = read_ws_frame(&mut stream).await;

    // Ping -> pong with the same payload (buffered video frames may
    // arrive ahead of the pong; skip binary frames).
    send_masked_frame(&mut stream, 0x9, b"ping-payload").await;
    let (op, payload) = loop {
        let (op, payload) = read_ws_frame(&mut stream).await;
        if op == 0xA {
            break (op, payload);
        }
    };
    assert_eq!(op, 0xA, "pong");
    assert_eq!(payload, b"ping-payload");

    // Close -> server echoes the close frame (RFC 6455 §5.5.1), then EOF.
    send_masked_frame(&mut stream, 0x8, b"bye").await;
    let (op, payload) = loop {
        let (op, payload) = read_ws_frame(&mut stream).await;
        if op == 0x8 {
            break (op, payload);
        }
    };
    assert_eq!(op, 0x8, "close echo");
    assert_eq!(payload, b"bye", "close payload echoed");
    // Drain until EOF (buffered FLV frames may arrive before the close).
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_secs(2), stream.get_mut().read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break,
        }
    }

    drop(stream);

    // Unknown mount: server closes the TCP connection after handshake.
    let mut stream = ws_connect(ws_addr, "/nope.flv").await;
    let mut buf = [0u8; 16];
    let n = stream.get_mut().read(&mut buf).await.unwrap();
    assert_eq!(n, 0, "no media for missing mount");
    drop(stream);

    server.shutdown().await.unwrap();
}

#[test]
fn rfc6455_accept_key_vector() {
    // Covered by the unit test inside ws.rs; keep an integration-level
    // assertion of the same vector here for documentation.
    use base64::Engine as _;
    use sha1::{Digest as _, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(b"dGhlIHNhbXBsZSBub25jZQ==");
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
    assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}

#[tokio::test]
async fn handshake_requires_version_13() {
    let (server, ws_addr, _publisher) = setup().await;
    let mut stream = TcpStream::connect(ws_addr).await.unwrap();
    let req = "GET /live/stream.flv HTTP/1.1\r\n\
               Host: t\r\n\
               Upgrade: websocket\r\n\
               Connection: Upgrade\r\n\
               Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
               Sec-WebSocket-Version: 8\r\n\
               \r\n";
    stream.write_all(req.as_bytes()).await.unwrap();
    // Missing/unsupported version: the server must NOT send 101; the
    // connection is closed instead.
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    assert!(
        !head.starts_with("HTTP/1.1 101"),
        "no 101 for bad version: {head}"
    );
    drop(stream);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn heartbeat_pings_silent_client_and_reaps_it() {
    // Server heartbeat: a silent client gets a ping after one interval and
    // is disconnected after a second silent interval (dead-peer reaping).
    let (server, ws_addr, _publisher) = setup_with_config(WsFlvConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        ping_interval: Some(Duration::from_millis(200)),
        ..Default::default()
    })
    .await;

    let mut stream = ws_connect(ws_addr, "/live/stream.flv").await;
    // Drain buffered FLV frames until the heartbeat ping arrives.
    let mut saw_ping = false;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_secs(2), read_ws_frame(&mut stream)).await {
            Ok((0x9, payload)) => {
                assert_eq!(payload, b"rtflow-ping");
                saw_ping = true;
                break;
            }
            Ok((_op, _payload)) => continue, // FLV data frames
            Err(_) => break,
        }
    }
    assert!(saw_ping, "server must ping after an idle interval");

    // Stay silent (no pong): the next interval must close the connection.
    let mut buf = [0u8; 4096];
    let mut eof = false;
    for _ in 0..20 {
        match tokio::time::timeout(Duration::from_secs(2), stream.get_mut().read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => {
                eof = true;
                break;
            }
            Ok(Ok(_)) => continue, // buffered data before the close
            Ok(Err(_)) => {
                eof = true;
                break;
            }
        }
    }
    assert!(
        eof,
        "silent client must be disconnected after unanswered ping"
    );
    drop(stream);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn client_pong_keeps_connection_alive() {
    // A client that answers the heartbeat stays connected past the second
    // interval.
    let (server, ws_addr, _publisher) = setup_with_config(WsFlvConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        ping_interval: Some(Duration::from_millis(200)),
        ..Default::default()
    })
    .await;

    let mut stream = ws_connect(ws_addr, "/live/stream.flv").await;
    // Consume frames; when a server ping arrives, answer with a pong.
    for _ in 0..12 {
        match tokio::time::timeout(Duration::from_secs(2), read_ws_frame(&mut stream)).await {
            Ok((0x9, payload)) => {
                // Pong with the same payload (masked client frame).
                let stream = stream.get_mut();
                let mask = [0x11, 0x22, 0x33, 0x44];
                let mut hdr = vec![0x80 | 0xA, 0x80 | payload.len() as u8];
                hdr.extend_from_slice(&mask);
                hdr.extend_from_slice(
                    &payload
                        .iter()
                        .enumerate()
                        .map(|(i, b)| b ^ mask[i % 4])
                        .collect::<Vec<_>>(),
                );
                stream.write_all(&hdr).await.unwrap();
            }
            Ok((_op, _payload)) => continue,
            Err(_) => break,
        }
    }
    // The connection must still be alive (not EOF).
    let mut buf = [0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.get_mut().read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_ne!(n, 0, "ponging client must not be disconnected");
    drop(stream);
    server.shutdown().await.unwrap();
}
