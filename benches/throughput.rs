use bytes::{Bytes, BytesMut};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rtflow::{
    AccessUnit, Codec,
    rtp::{Packetizer, RtpPacket},
    srtp::{SrtpContext, SrtpProfile},
};

fn bench_rtp_packetize_h264(c: &mut Criterion) {
    let unit = AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: std::time::Duration::ZERO,
        dts: None,
        rtp_timestamp: 90_000,
        is_keyframe: true,
        data: Bytes::from_static(&[0x65; 1_200]),
    };
    let mut packetizer = Packetizer::new(Codec::H264, 96, 1, 1_200).unwrap();

    c.bench_function("rtp_packetize_h264_1200", |b| {
        b.iter(|| {
            black_box(packetizer.packetize(black_box(&unit)).unwrap());
        })
    });
}

fn bench_rtp_packetize_h264_large(c: &mut Criterion) {
    let unit = AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: std::time::Duration::ZERO,
        dts: None,
        rtp_timestamp: 90_000,
        is_keyframe: true,
        data: Bytes::from_static(&[0x65; 65_536]),
    };
    let mut packetizer = Packetizer::new(Codec::H264, 96, 1, 1_200).unwrap();

    c.bench_function("rtp_packetize_h264_64k", |b| {
        b.iter(|| {
            black_box(packetizer.packetize(black_box(&unit)).unwrap());
        })
    });
}

fn bench_rtp_parse(c: &mut Criterion) {
    let unit = AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: std::time::Duration::ZERO,
        dts: None,
        rtp_timestamp: 90_000,
        is_keyframe: true,
        data: Bytes::from_static(&[0x65; 1_200]),
    };
    let mut packetizer = Packetizer::new(Codec::H264, 96, 1, 1_200).unwrap();
    let packet = packetizer.packetize(&unit).unwrap().remove(0).encode();

    c.bench_function("rtp_parse", |b| {
        b.iter(|| {
            black_box(RtpPacket::parse(black_box(packet.clone())).unwrap());
        })
    });
}

fn bench_rtsp_parse(c: &mut Criterion) {
    let response = b"RTSP/1.0 200 OK\r\nCSeq: 42\r\nContent-Length: 0\r\n\r\n";

    c.bench_function("rtsp_parse", |b| {
        b.iter(|| {
            let mut bytes = BytesMut::from(&response[..]);
            black_box(rtflow::rtsp::parse(&mut bytes, 16 * 1024, 256 * 1024).unwrap());
        })
    });
}

fn bench_srtp_protect(c: &mut Criterion) {
    let master_key = [0xABu8; 16];
    let master_salt = [0xCDu8; 14];
    let mut ctx = SrtpContext::new(
        SrtpProfile::AES_128_CM_HMAC_SHA1_80,
        &master_key,
        &master_salt,
    );

    // Build a minimal RTP packet.
    let mut rtp = Vec::new();
    rtp.extend_from_slice(&[0x80, 0x60]);
    rtp.extend_from_slice(&1u16.to_be_bytes());
    rtp.extend_from_slice(&1000u32.to_be_bytes());
    rtp.extend_from_slice(&0x12345678u32.to_be_bytes());
    rtp.extend_from_slice(&[0x65; 200]);

    c.bench_function("srtp_protect", |b| {
        b.iter(|| {
            black_box(ctx.protect_rtp(black_box(&rtp)).unwrap());
        })
    });
}

fn bench_srtp_unprotect(c: &mut Criterion) {
    let master_key = [0xABu8; 16];
    let master_salt = [0xCDu8; 14];
    let mut sender = SrtpContext::new(
        SrtpProfile::AES_128_CM_HMAC_SHA1_80,
        &master_key,
        &master_salt,
    );
    let mut receiver = SrtpContext::new(
        SrtpProfile::AES_128_CM_HMAC_SHA1_80,
        &master_key,
        &master_salt,
    );

    let mut rtp = Vec::new();
    rtp.extend_from_slice(&[0x80, 0x60]);
    rtp.extend_from_slice(&1u16.to_be_bytes());
    rtp.extend_from_slice(&1000u32.to_be_bytes());
    rtp.extend_from_slice(&0x12345678u32.to_be_bytes());
    rtp.extend_from_slice(&[0x65; 200]);

    let protected = sender.protect_rtp(&rtp).unwrap();

    c.bench_function("srtp_unprotect", |b| {
        b.iter(|| {
            // Need to clone since unprotect mutates the replay window.
            let p = protected.clone();
            black_box(receiver.unprotect_rtp(black_box(&p)).unwrap());
        })
    });
}

fn bench_sdp_parse(c: &mut Criterion) {
    let sdp = b"v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=test\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 profile-level-id=42e01f\r\na=control:trackID=0\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000/1\r\na=control:trackID=1\r\n";

    c.bench_function("sdp_parse", |b| {
        b.iter(|| {
            black_box(rtflow::protocol::sdp::parse(black_box(sdp)).unwrap());
        })
    });
}

criterion_group!(
    benches,
    bench_rtp_packetize_h264,
    bench_rtp_packetize_h264_large,
    bench_rtp_parse,
    bench_rtsp_parse,
    bench_srtp_protect,
    bench_srtp_unprotect,
    bench_sdp_parse,
);
criterion_main!(benches);
