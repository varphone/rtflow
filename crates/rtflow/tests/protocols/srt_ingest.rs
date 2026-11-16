#![cfg(all(feature = "srt", feature = "server"))]

//! SRT ingest integration tests.
//!
//! Tests the full SRT caller → listener flow:
//! - HSv5 handshake (induction + conclusion)
//! - MPEG-TS data delivery through TSBPD + demuxer → mount
//! - Stream ID routing
//! - Keepalive timeout disconnect

use bytes::{Bytes, BytesMut};
use rtflow::mount::MountRegistry;
use rtflow::srt::handshake::{
    Handshake, HandshakeType, HsExtFields, HsExtension, HsFlags, SRT_MAGIC, SRT_VERSION,
    SRT_VERSION_4, SRT_VERSION_5, encode_handshake, parse_handshake,
};
use rtflow::srt::packet::{
    ControlPacket, ControlType, DataPacket, SrtPacket, encode_control_packet, encode_data_packet,
    parse_packet,
};
use rtflow::srt::{SrtConfig, SrtServer};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Wrap handshake bytes in a control packet and encode to wire format.
fn handshake_to_wire(hs: &Handshake, socket_id: u32, timestamp: u32) -> BytesMut {
    let pkt = ControlPacket {
        control_type: ControlType::Handshake,
        type_specific: 0,
        additional_info: 0,
        timestamp,
        socket_id,
        data: encode_handshake(hs),
    };
    encode_control_packet(&pkt)
}

/// Build an induction request (caller → listener, phase 1).
fn build_induction_request(socket_id: u32) -> Handshake {
    Handshake {
        version: SRT_VERSION_4,
        encryption: 0,
        extension_field: 2,
        initial_sequence: 0,
        mtu: 1500,
        flow_window: 8192,
        handshake_type: HandshakeType::Induction,
        socket_id,
        cookie: 0,
        peer_ip: [0u8; 16],
        extensions: Vec::new(),
    }
}

/// Build a conclusion request with optional Stream ID (caller → listener, phase 2).
fn build_conclusion_request(
    socket_id: u32,
    cookie: u32,
    initial_seq: u32,
    stream_id: Option<&str>,
) -> Bytes {
    let handshake = Handshake {
        version: SRT_VERSION_5,
        encryption: 0,
        extension_field: 0,
        initial_sequence: initial_seq,
        mtu: 1500,
        flow_window: 8192,
        handshake_type: HandshakeType::Conclusion,
        socket_id,
        cookie,
        peer_ip: [0u8; 16],
        extensions: {
            let mut extensions = vec![HsExtension::HsReq(HsExtFields {
                srt_version: SRT_VERSION,
                flags: HsFlags::live_defaults(),
                recv_latency_ms: 120,
                send_latency_ms: 0,
            })];
            if let Some(stream_id) = stream_id {
                extensions.push(HsExtension::StreamId(stream_id.to_owned()));
            }
            extensions
        },
    };
    encode_handshake(&handshake)
}

/// Send a conclusion request with raw bytes (for custom extensions).
fn conclusion_to_wire(hs_data: Bytes, socket_id: u32, timestamp: u32) -> BytesMut {
    let pkt = ControlPacket {
        control_type: ControlType::Handshake,
        type_specific: 0,
        additional_info: 0,
        timestamp,
        socket_id,
        data: hs_data,
    };
    encode_control_packet(&pkt)
}

/// Build a PAT (Program Association Table) TS packet.
/// Points to PMT at PID 0x1000.
fn build_pat_packet() -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47; // sync
    pkt[1] = 0x40; // payload_unit_start=1, PID high=0
    pkt[2] = 0x00; // PID low=0 (PAT)
    pkt[3] = 0x10; // adaptation_field_control=01 (payload only)

    // Pointer field
    pkt[4] = 0x00;

    // PAT section
    pkt[5] = 0x00; // table_id
    pkt[6] = 0xB0; // section_syntax_indicator=1, section_length high
    pkt[7] = 0x0D; // section_length low = 13
    pkt[8] = 0x00; // transport_stream_id high
    pkt[9] = 0x01; // transport_stream_id low
    pkt[10] = 0xC1; // version=0, current_next=1
    pkt[11] = 0x00; // section_number
    pkt[12] = 0x00; // last_section_number
    // Program entry: program_number=1, PMT PID=0x1000
    pkt[13] = 0x00; // program_number high
    pkt[14] = 0x01; // program_number low
    pkt[15] = 0xF0 | 0x10; // reserved + PID high (0x10)
    pkt[16] = 0x00; // PID low (0x00) → PMT PID = 0x1000
    // CRC32 (dummy - demuxer doesn't verify)
    pkt[17] = 0x00;
    pkt[18] = 0x00;
    pkt[19] = 0x00;
    pkt[20] = 0x00;

    pkt
}

/// Build a PMT (Program Map Table) TS packet.
/// Declares H.264 video on PID 0x0100.
fn build_pmt_packet() -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47; // sync
    pkt[1] = 0x40 | 0x10; // payload_unit_start=1, PID high = 0x10
    pkt[2] = 0x00; // PID low = 0x00 → PID = 0x1000
    pkt[3] = 0x10; // payload only

    // Pointer field
    pkt[4] = 0x00;

    // PMT section
    pkt[5] = 0x02; // table_id
    pkt[6] = 0xB0; // section_syntax_indicator=1, section_length high
    pkt[7] = 0x12; // section_length low = 18
    pkt[8] = 0x00; // program_number high
    pkt[9] = 0x01; // program_number low
    pkt[10] = 0xC1; // version=0, current_next=1
    pkt[11] = 0x00; // section_number
    pkt[12] = 0x00; // last_section_number
    pkt[13] = 0xE0 | 0x01; // reserved + PCR_PID high (0x01)
    pkt[14] = 0x00; // PCR_PID low (0x00) → PCR PID = 0x0100
    pkt[15] = 0xF0; // reserved + program_info_length high
    pkt[16] = 0x00; // program_info_length low = 0

    // ES entry: H.264 video
    pkt[17] = 0x1B; // stream_type = H.264
    pkt[18] = 0xE0 | 0x01; // reserved + elementary_PID high (0x01)
    pkt[19] = 0x00; // elementary_PID low (0x00) → PID = 0x0100
    pkt[20] = 0xF0; // reserved + ES_info_length high
    pkt[21] = 0x00; // ES_info_length low = 0

    // CRC32 (dummy)
    pkt[22] = 0x00;
    pkt[23] = 0x00;
    pkt[24] = 0x00;
    pkt[25] = 0x00;

    pkt
}

/// Build a PES packet containing H.264 data (SPS + IDR) as a TS packet.
/// This is the first PES (payload_unit_start=1).
fn build_pes_ts_packet_start(pts: u64) -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47; // sync
    pkt[1] = 0x40 | 0x01; // payload_unit_start=1, PID high = 0x01
    pkt[2] = 0x00; // PID low = 0x00 → PID = 0x0100
    pkt[3] = 0x10; // payload only

    // PES header
    let mut pos = 4;
    pkt[pos] = 0x00;
    pos += 1; // PES start code prefix
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x01;
    pos += 1;
    pkt[pos] = 0xE0;
    pos += 1; // stream_id = video
    pkt[pos] = 0x00;
    pos += 1; // PES_packet_length high (0 = unbounded)
    pkt[pos] = 0x00;
    pos += 1; // PES_packet_length low
    pkt[pos] = 0x80;
    pos += 1; // marker bits
    pkt[pos] = 0x80;
    pos += 1; // PTS_DTS_flags = 10 (PTS only)
    pkt[pos] = 0x05;
    pos += 1; // PES_header_data_length = 5

    // Encode PTS (5 bytes)
    let pts_bytes = encode_pts(pts);
    pkt[pos..pos + 5].copy_from_slice(&pts_bytes);
    pos += 5;

    // H.264 payload: SPS NAL + IDR NAL
    // SPS: 00 00 00 01 67 ...
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x01;
    pos += 1;
    pkt[pos] = 0x67;
    pos += 1; // NAL type 7 (SPS)
    pkt[pos] = 0x42;
    pos += 1; // profile_idc
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x1F;
    pos += 1;
    pkt[pos] = 0xDA;
    pos += 1;
    pkt[pos] = 0x01;
    pos += 1;
    pkt[pos] = 0x40;
    pos += 1;

    // IDR: 00 00 00 01 65 ...
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x01;
    pos += 1;
    pkt[pos] = 0x65;
    pos += 1; // NAL type 5 (IDR)
    // Fill rest with dummy slice data
    while pos < 188 {
        pkt[pos] = 0xAA;
        pos += 1;
    }

    pkt
}

/// Build a second PES packet (to flush the first one from the demuxer).
fn build_pes_ts_packet_second(pts: u64) -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40 | 0x01; // payload_unit_start=1, PID=0x0100
    pkt[2] = 0x00;
    pkt[3] = 0x10;

    let mut pos = 4;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x01;
    pos += 1;
    pkt[pos] = 0xE0;
    pos += 1;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x80;
    pos += 1;
    pkt[pos] = 0x80;
    pos += 1;
    pkt[pos] = 0x05;
    pos += 1;

    let pts_bytes = encode_pts(pts);
    pkt[pos..pos + 5].copy_from_slice(&pts_bytes);
    pos += 5;

    // Non-IDR frame: 00 00 00 01 41 ...
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x00;
    pos += 1;
    pkt[pos] = 0x01;
    pos += 1;
    pkt[pos] = 0x41;
    pos += 1; // NAL type 1 (non-IDR slice)
    while pos < 188 {
        pkt[pos] = 0xBB;
        pos += 1;
    }

    pkt
}

/// Encode a PTS value into the 5-byte PES timestamp format.
fn encode_pts(pts: u64) -> [u8; 5] {
    let mut out = [0u8; 5];
    out[0] = 0x21 | (((pts >> 29) & 0x0E) as u8); // '0010' marker + PTS[32..30] + marker
    out[1] = ((pts >> 22) & 0xFF) as u8;
    out[2] = (((pts >> 14) & 0xFE) as u8) | 0x01; // PTS[29..15] + marker
    out[3] = ((pts >> 7) & 0xFF) as u8;
    out[4] = (((pts << 1) & 0xFE) as u8) | 0x01; // PTS[14..0] + marker
    out
}

/// Perform the full HSv5 handshake as a caller.
/// Returns (server_socket_id, cookie) on success.
async fn perform_handshake(
    sock: &UdpSocket,
    server_addr: std::net::SocketAddr,
    caller_socket_id: u32,
    stream_id: Option<&str>,
) -> (u32, u32) {
    // Phase 1: Induction
    let induction = build_induction_request(caller_socket_id);
    let wire = handshake_to_wire(&induction, 0, 0);
    sock.send_to(&wire, server_addr).await.unwrap();

    // Receive induction response
    let mut buf = vec![0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("induction response timeout")
        .unwrap();

    let resp = parse_packet(&buf[..n]).expect("failed to parse induction response");
    let (cookie, _server_socket_id) = match resp {
        SrtPacket::Control(ctrl) => {
            assert_eq!(ctrl.control_type, ControlType::Handshake);
            let hs = parse_handshake(&ctrl.data).expect("failed to parse handshake");
            assert_eq!(hs.handshake_type, HandshakeType::Induction);
            assert_eq!(hs.version, SRT_VERSION_5);
            assert_eq!(hs.extension_field, SRT_MAGIC);
            // libsrt keeps the caller's socket id in the induction response.
            assert_eq!(hs.socket_id, caller_socket_id);
            (hs.cookie, hs.socket_id)
        }
        _ => panic!("expected control packet for induction response"),
    };

    // Phase 2: Conclusion
    let initial_seq = 1000u32;
    let conclusion_data =
        build_conclusion_request(caller_socket_id, cookie, initial_seq, stream_id);
    // The caller sends the conclusion to socket id 0 (connection request).
    let wire = conclusion_to_wire(conclusion_data, 0, 1000);
    sock.send_to(&wire, server_addr).await.unwrap();

    // Receive conclusion response
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("conclusion response timeout")
        .unwrap();

    let resp = parse_packet(&buf[..n]).expect("failed to parse conclusion response");
    match resp {
        SrtPacket::Control(ctrl) => {
            assert_eq!(ctrl.control_type, ControlType::Handshake);
            assert_eq!(ctrl.socket_id, caller_socket_id);
            let hs = parse_handshake(&ctrl.data).expect("failed to parse conclusion handshake");
            assert_eq!(hs.handshake_type, HandshakeType::Conclusion);
            assert_ne!(hs.socket_id, 0);
            (hs.socket_id, initial_seq)
        }
        _ => panic!("expected control packet for conclusion response"),
    }
}

/// Send TS data as SRT data packets.
async fn send_ts_data(
    sock: &UdpSocket,
    server_addr: std::net::SocketAddr,
    dest_socket_id: u32,
    start_seq: u32,
    ts_packets: &[&[u8]],
) {
    for (i, ts_pkt) in ts_packets.iter().enumerate() {
        let data_pkt = DataPacket {
            sequence: start_seq + i as u32,
            position: 3, // single-packet message
            message_number: (i + 1) as u32,
            timestamp: (i as u32) * 1000, // 1ms apart
            socket_id: dest_socket_id,
            ordered: true,
            retransmitted: false,
            payload: Bytes::copy_from_slice(ts_pkt),
        };
        let wire = encode_data_packet(&data_pkt);
        sock.send_to(&wire, server_addr).await.unwrap();
        // Small delay to avoid overwhelming the receiver
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn srt_handshake_completes() {
    let mounts = Arc::new(MountRegistry::new(10));
    let config = SrtConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        latency_ms: 0,
        ..Default::default()
    };
    let server = SrtServer::with_config(config, mounts).await.unwrap();
    let server_addr = server.local_addr().unwrap();
    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (server_sid, _initial_seq) = perform_handshake(&sock, server_addr, 0xABCD, None).await;

    // Server should assign a non-zero socket ID
    assert_ne!(server_sid, 0);

    cancel.cancel();
}

#[tokio::test]
async fn srt_ts_ingest_creates_mount() {
    let mounts = Arc::new(MountRegistry::new(10));
    let config = SrtConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        latency_ms: 0,
        ..Default::default()
    };
    let server = SrtServer::with_config(config, mounts.clone())
        .await
        .unwrap();
    let server_addr = server.local_addr().unwrap();
    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (server_sid, initial_seq) = perform_handshake(&sock, server_addr, 0xABCD, None).await;

    // Build TS packets: PAT + PMT + PES1 + PES2 (PES2 flushes PES1)
    let pat = build_pat_packet();
    let pmt = build_pmt_packet();
    let pes1 = build_pes_ts_packet_start(90000); // PTS = 1s
    let pes2 = build_pes_ts_packet_second(93600); // PTS = 1.04s

    let ts_packets: Vec<&[u8]> = vec![&pat, &pmt, &pes1, &pes2];
    send_ts_data(&sock, server_addr, server_sid, initial_seq, &ts_packets).await;

    // Wait for the server to process and deliver
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The mount should be registered (path based on port since no stream_id)
    let snapshots = mounts.snapshots();
    assert!(
        !snapshots.is_empty(),
        "Expected at least one mount after TS ingest"
    );

    cancel.cancel();
}

#[tokio::test]
async fn srt_stream_id_routing() {
    let mounts = Arc::new(MountRegistry::new(10));
    let config = SrtConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        latency_ms: 0,
        ..Default::default()
    };
    let server = SrtServer::with_config(config, mounts.clone())
        .await
        .unwrap();
    let server_addr = server.local_addr().unwrap();
    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // Use stream ID "#!::r=live/cam1" which should route to mount "/live/cam1"
    let (server_sid, initial_seq) =
        perform_handshake(&sock, server_addr, 0x1234, Some("#!::r=live/cam1")).await;

    // Send TS data
    let pat = build_pat_packet();
    let pmt = build_pmt_packet();
    let pes1 = build_pes_ts_packet_start(90000);
    let pes2 = build_pes_ts_packet_second(93600);

    let ts_packets: Vec<&[u8]> = vec![&pat, &pmt, &pes1, &pes2];
    send_ts_data(&sock, server_addr, server_sid, initial_seq, &ts_packets).await;

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The mount should be at /live/cam1
    let handle = mounts.get_handle("/live/cam1");
    assert!(
        handle.is_some(),
        "Expected mount at /live/cam1 from stream ID routing"
    );

    cancel.cancel();
}

#[tokio::test]
async fn srt_keepalive_timeout_disconnects() {
    let mounts = Arc::new(MountRegistry::new(10));
    let config = SrtConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        latency_ms: 0,
        peer_idle_timeout: Duration::from_secs(1), // Short timeout
        ..Default::default()
    };
    let server = SrtServer::with_config(config, mounts.clone())
        .await
        .unwrap();
    let server_addr = server.local_addr().unwrap();
    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (server_sid, initial_seq) = perform_handshake(&sock, server_addr, 0x5678, None).await;

    // Send some data to establish the stream
    let pat = build_pat_packet();
    let pmt = build_pmt_packet();
    let pes1 = build_pes_ts_packet_start(90000);
    let pes2 = build_pes_ts_packet_second(93600);

    let ts_packets: Vec<&[u8]> = vec![&pat, &pmt, &pes1, &pes2];
    send_ts_data(&sock, server_addr, server_sid, initial_seq, &ts_packets).await;

    // Wait for mount to appear (the negotiated latency is 120ms).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!mounts.snapshots().is_empty(), "Mount should exist");

    // Now stop sending anything (no keepalives) and wait for timeout.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // The connection timed out, so its publisher mount must be unregistered
    // (otherwise distinct stream paths would leak registry slots forever).
    assert!(
        mounts.snapshots().is_empty(),
        "Mount should be unregistered after keepalive timeout"
    );

    // After timeout, sending data should not crash the server
    // (connection is removed, data from unknown peer is ignored)
    let data_pkt = DataPacket {
        sequence: initial_seq + 100,
        position: 3,
        message_number: 99,
        timestamp: 50000,
        socket_id: server_sid,
        ordered: true,
        retransmitted: false,
        payload: Bytes::from_static(&[0x47; 188]),
    };
    let wire = encode_data_packet(&data_pkt);
    sock.send_to(&wire, server_addr).await.unwrap();

    // Server should still be running (not panicked)
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!cancel.is_cancelled(), "Server should still be running");

    cancel.cancel();
}
