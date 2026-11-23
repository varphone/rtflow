use bytes::{Bytes, BytesMut};
use rtflow::{
    AccessUnit, Codec,
    rtp::{Packetizer, RtpPacket},
};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn measure(name: &str, iterations: u64, mut operation: impl FnMut()) {
    for _ in 0..1_000 {
        operation();
    }
    let started = Instant::now();
    for _ in 0..iterations {
        operation();
    }
    let elapsed = started.elapsed();
    println!(
        "{name}: {:.0} ops/s ({elapsed:?})",
        iterations as f64 / elapsed.as_secs_f64()
    );
}

fn main() {
    let unit = AccessUnit {
        nal_offsets: None,
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::ZERO,
        dts: None,
        rtp_timestamp: 90_000,
        is_keyframe: true,
        data: Bytes::from_static(&[0x65; 1_200]),
    };
    let mut packetizer = Packetizer::new(Codec::H264, 96, 1, 1_200).unwrap();
    measure("h264-packetize-1200", 100_000, || {
        black_box(packetizer.packetize(black_box(&unit)).unwrap());
    });

    let packet = packetizer.packetize(&unit).unwrap().remove(0).encode();
    measure("rtp-parse", 500_000, || {
        black_box(RtpPacket::parse(black_box(packet.clone())).unwrap());
    });

    let response = b"RTSP/1.0 200 OK\r\nCSeq: 42\r\nContent-Length: 0\r\n\r\n";
    measure("rtsp-parse", 200_000, || {
        let mut bytes = BytesMut::from(&response[..]);
        black_box(rtflow::rtsp::parse(&mut bytes, 16 * 1024, 256 * 1024).unwrap());
    });
}
