use super::FlvMuxer;
use super::avcc::{annex_b_avcc_size, annex_b_to_avcc, write_annex_b_avcc};
use super::tag::FLV_TAG_HEADER_SIZE;
use crate::media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};
use bytes::{Bytes, BytesMut};
use std::sync::Arc;
use std::time::Duration;

fn make_h264_description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("test"),
        tracks: vec![
            TrackDescription {
                id: 0,
                kind: MediaKind::Video,
                codec: Codec::H264,
                clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
                channels: None,
                payload_type: 96,
                fmtp: None,
                control: Arc::from("trackID=0"),
                width: None,
                height: None,
                frame_rate: None,
                #[cfg(feature = "srtp")]
                crypto: None,
            },
            TrackDescription {
                id: 1,
                kind: MediaKind::Audio,
                codec: Codec::Aac,
                clock_rate: 44100,
                channels: Some(2),
                payload_type: 97,
                fmtp: None,
                control: Arc::from("trackID=1"),
                width: None,
                height: None,
                frame_rate: None,
                #[cfg(feature = "srtp")]
                crypto: None,
            },
        ],
        #[cfg(feature = "multicast")]
        connection: None,
    }
}

fn make_h265_description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("test"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H265,
            clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
            width: None,
            height: None,
            frame_rate: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }],
        #[cfg(feature = "multicast")]
        connection: None,
    }
}

#[test]
fn flv_header_video_and_audio() {
    let muxer = FlvMuxer::new(&make_h264_description());
    let header = muxer.flv_header();
    assert_eq!(&header[..3], b"FLV");
    assert_eq!(header[4], 5); // video + audio flags
}

#[test]
fn flv_header_audio_only() {
    let desc = MediaDescription {
        session_name: Arc::from("test"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Audio,
            codec: Codec::Aac,
            clock_rate: 44100,
            channels: Some(2),
            payload_type: 97,
            fmtp: None,
            control: Arc::from("trackID=0"),
            width: None,
            height: None,
            frame_rate: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }],
        #[cfg(feature = "multicast")]
        connection: None,
    };
    let muxer = FlvMuxer::new(&desc);
    let header = muxer.flv_header();
    assert_eq!(header[4], 4); // audio flag
}

#[test]
fn avc_sequence_header_from_sps_pps() {
    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x67, 0x42, 0x00, 0x0a, 0xe9, 0x40, 0x20]);
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x68, 0xce, 0x38, 0x80]);

    let bytes = Bytes::from(data);
    let seq = FlvMuxer::build_avc_sequence_header(&bytes).unwrap();
    assert_eq!(seq[0], 1);
    assert_eq!(seq[4], 0xff);
    assert_eq!(seq[5], 0xe1);
}

#[test]
fn h265_enhanced_rtmp_tag() {
    let desc = make_h265_description();
    let mut muxer = FlvMuxer::new(&desc);

    // Minimal H.265 data with VPS/SPS/PPS.
    // SPS must be long enough for build_h265_sequence_header to parse
    // profile_tier_level (needs >=13 bytes after the 2-byte NAL header + 1-byte SPS header).
    let mut data = Vec::new();
    // VPS (nal_type=32, first byte = 0x40)
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x40, 0x01]);
    // SPS (nal_type=33, first byte = 0x42)
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    // NAL header: [0x42, 0x01], then SPS RBSP:
    //   [0]: sps_video_parameter_set_id(4)+sps_max_sub_layers_minus1(3)+sps_temporal_id_nesting_flag(1)
    //   [1..12]: profile_tier_level (12 bytes minimum)
    data.extend_from_slice(&[
        0x42, 0x01, // NAL header
        0x00, // sps params
        0x01, // general_profile_idc = 1 (Main)
        0x00, 0x00, 0x00, 0x00, // general_profile_compatibility_flags
        0x00, 0x00, 0x00, 0x00, 0x00, 0x60, // general_constraint_indicator_flags
        0x78, // general_level_idc = 120 (level 3.0)
    ]);
    // PPS (nal_type=34, first byte = 0x44)
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x44, 0x01]);

    let bytes = Bytes::from(data);
    let seq = FlvMuxer::build_h265_sequence_header(&bytes);
    assert!(
        seq.is_some(),
        "should build H.265 sequence header from VPS/SPS/PPS"
    );

    let seq = seq.unwrap();
    muxer.set_video_sequence_header(seq);

    let unit = AccessUnit {
        track_id: 0,
        codec: Codec::H265,
        pts: Duration::from_millis(0),
        dts: None,
        rtp_timestamp: 0,
        is_keyframe: true,
        data: bytes,
    };

    let tags = muxer.encode(&unit);
    assert!(!tags.is_empty());
    // Find the first video tag and verify Enhanced RTMP format (Veovera spec)
    // Byte 0: IsExVideoHeader(1) | VideoFrameType(3) | VideoPacketType(4)
    let mut pos = 0;
    while pos + FLV_TAG_HEADER_SIZE + 5 < tags.len() {
        if tags[pos] == 0x09 {
            let body_start = pos + FLV_TAG_HEADER_SIZE;
            // Sequence header: IsEx=1, FrameType=1(keyframe), PacketType=0
            // = 0x80 | 0x10 | 0x00 = 0x90
            assert_eq!(
                tags[body_start], 0x90,
                "seq hdr: IsEx=1, FrameType=1, PacketType=0"
            );
            assert_eq!(&tags[body_start + 1..body_start + 5], b"hvc1");
            // No separate PacketType byte — it's embedded in byte 0
            break;
        }
        pos += 1;
    }
}

#[test]
fn annex_b_to_avcc_conversion() {
    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x65, 0x88, 0x00]);
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x41, 0x9a, 0x00]);

    let bytes = Bytes::from(data);
    let avcc = annex_b_to_avcc(&bytes);
    assert_eq!(avcc.len(), 14);
}

/// The in-place conversion used by `encode` must be byte-identical to
/// the reference `annex_b_to_avcc`, including mixed 3/4-byte start
/// codes and the already-length-prefixed fast path.
#[test]
fn inplace_conversion_matches_reference() {
    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42]); // 4-byte start code
    data.extend_from_slice(&[0x00, 0x00, 0x01, 0x68]); // 3-byte start code
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80, 0x40]);
    let bytes = Bytes::from(data);

    let expected = annex_b_to_avcc(&bytes);
    let mut buf = BytesMut::new();
    write_annex_b_avcc(&mut buf, &bytes);
    assert_eq!(buf.freeze(), expected, "Annex-B in-place conversion");

    // Fast path: length-prefixed payload (single- or multi-NAL) must
    // pass through byte-identical.
    let mut buf = BytesMut::new();
    write_annex_b_avcc(&mut buf, &expected);
    assert_eq!(buf.freeze(), expected, "length-prefixed passthrough");
    assert!(annex_b_avcc_size(&expected).is_none());
    // Multi-NAL AVCC (RTMP ingest shape) is not mistaken for Annex-B.
    let mut multi = Vec::new();
    multi.extend_from_slice(&(3u32).to_be_bytes());
    multi.extend_from_slice(&[0x67, 0x42, 0x00]);
    multi.extend_from_slice(&(2u32).to_be_bytes());
    multi.extend_from_slice(&[0x65, 0x88]);
    let multi = Bytes::from(multi);
    assert!(
        annex_b_avcc_size(&multi).is_none(),
        "multi-NAL AVCC is AVCC"
    );
    let mut buf = BytesMut::new();
    write_annex_b_avcc(&mut buf, &multi);
    assert_eq!(buf.freeze(), multi, "multi-NAL AVCC passthrough");

    // An AVCC NAL whose length field is `00 00 01 XX` (256–511-byte
    // NAL, e.g. an AAC frame) looks like a 3-byte start code — it must
    // still be recognized as length-prefixed and pass through.
    let mut audio = Vec::new();
    audio.extend_from_slice(&0x0101u32.to_be_bytes()); // 257-byte NAL
    audio.extend_from_slice(&[0x21, 0x10, 0x00, 0x01]);
    audio.resize(4 + 257, 0xab);
    let audio = Bytes::from(audio);
    assert!(
        annex_b_avcc_size(&audio).is_none(),
        "00 00 01 XX AVCC NAL is not Annex-B"
    );
    let mut buf = BytesMut::new();
    write_annex_b_avcc(&mut buf, &audio);
    assert_eq!(buf.freeze(), audio, "00 00 01 XX AVCC passthrough");
}

/// `encode` of a multi-NAL Annex-B keyframe must produce a video tag
/// whose body is exactly frame-type + packet-type + CTS + AVCC NALs.
#[test]
fn encode_h264_tag_payload_is_avcc() {
    let desc = MediaDescription {
        session_name: Arc::from("t"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
            #[cfg(feature = "srtp")]
            crypto: None,
            width: None,
            height: None,
            frame_rate: None,
        }],
        #[cfg(feature = "multicast")]
        connection: None,
    };
    let mut muxer = FlvMuxer::new(&desc);
    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x0a]);
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x38, 0x80]);
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80]);
    let unit = AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::from_millis(40),
        dts: None,
        rtp_timestamp: 3600,
        is_keyframe: true,
        data: Bytes::from(data),
    };
    if let Some(seq) = FlvMuxer::build_avc_sequence_header(&unit.data) {
        muxer.set_video_sequence_header(seq);
    }
    let tags = muxer.encode(&unit);
    // Walk the FLV tags and locate the coded-frames video tag.
    let mut pos = 0;
    let mut frame_body: Option<&[u8]> = None;
    while pos + 15 <= tags.len() {
        let tag_type = tags[pos];
        let body_size =
            u32::from_be_bytes([0, tags[pos + 1], tags[pos + 2], tags[pos + 3]]) as usize;
        let body = &tags[pos + 11..pos + 11 + body_size];
        if tag_type == 9 && body.get(1) == Some(&1) {
            frame_body = Some(body);
        }
        pos += 11 + body_size + 4; // header + body + previous-tag-size
    }
    let frame_body = frame_body.expect("coded-frames video tag");
    assert_eq!(frame_body[0] & 0x0f, 7, "codec id AVC");
    assert_eq!(frame_body[1], 1, "AVC packet type NALU");
    // Body after 5 bytes of video-tag header: length-prefixed NALs.
    assert_eq!(
        &frame_body[5..],
        annex_b_to_avcc(&unit.data).as_ref(),
        "frame payload is AVCC"
    );
}
/// G.711 audio tags must use the normalized PTS (0-based), not the raw
/// unit PTS — a regression guard for the FLV timestamp base.
#[test]
fn g711_tag_pts_normalized_to_zero() {
    let desc = MediaDescription {
        session_name: Arc::from("t"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Audio,
            codec: Codec::Pcma,
            clock_rate: 8000,
            channels: Some(1),
            payload_type: 8,
            fmtp: None,
            control: Arc::from("trackID=0"),
            #[cfg(feature = "srtp")]
            crypto: None,
            width: None,
            height: None,
            frame_rate: None,
        }],
        #[cfg(feature = "multicast")]
        connection: None,
    };
    let mut muxer = FlvMuxer::new(&desc);
    let first = AccessUnit {
        track_id: 0,
        codec: Codec::Pcma,
        pts: Duration::from_millis(500),
        dts: None,
        rtp_timestamp: 4000,
        is_keyframe: true,
        data: Bytes::from_static(&[0x55; 40]),
    };
    let second = AccessUnit {
        pts: Duration::from_millis(520),
        rtp_timestamp: 4160,
        ..first.clone()
    };
    let tag1 = muxer.encode(&first);
    let tag2 = muxer.encode(&second);
    // FLV tag timestamp: 3 bytes + 1 extension byte at offset 4..8.
    let ts = |tag: &Bytes| (u32::from(tag[4]) << 16) | (u32::from(tag[5]) << 8) | u32::from(tag[6]);
    assert_eq!(ts(&tag1), 0, "first tag timestamp normalized to 0");
    assert_eq!(ts(&tag2), 20, "second tag timestamp is base-relative (ms)");
    // Sound format bits: A-law (7) << 4 | 0x0F.
    assert_eq!(tag1[11] >> 4, 7, "sound format A-law");
}
