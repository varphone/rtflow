#![cfg(feature = "hls")]

use bytes::Bytes;
use rtflow::hls::{
    playlist::{self},
    segment_ring::SegmentRing,
    segmenter::{Segment, Segmenter, SegmenterConfig},
};
use rtflow::media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};
use std::{sync::Arc, time::Duration};

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
    let mut data = Vec::new();
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x67, 0x42, 0x00, 0x0a, 0xe9, 0x40, 0x20]);
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(&[0x68, 0xce, 0x38, 0x80]);
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
fn segmenter_produces_init_segment() {
    let desc = h264_description();
    let config = SegmenterConfig {
        segment_duration: Duration::from_millis(100),
        lookback_count: 5,
    };
    let mut segmenter = Segmenter::new(config, &desc);
    let mut rx = segmenter.subscribe();

    // Push enough frames to trigger segment creation
    for i in 0..10 {
        let is_key = i % 5 == 0;
        let data = if is_key {
            h264_keyframe()
        } else {
            h264_interframe()
        };
        segmenter.push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(i * 40),
            dts: None,
            rtp_timestamp: (i * 3600) as u32,
            is_keyframe: is_key,
            data,
        });
    }

    segmenter.flush();

    // Should have received at least one segment
    let segment = rx.try_recv().unwrap();
    assert!(!segment.init.is_empty(), "init segment should not be empty");
    // Init segment should start with ftyp box
    assert_eq!(&segment.init[4..8], b"ftyp");
    // Should also contain moov box
    let init_str = std::str::from_utf8(&segment.init).unwrap_or("");
    assert!(init_str.contains("moov") || segment.init.windows(4).any(|w| w == b"moov"));
    let avcc = segment
        .init
        .windows(4)
        .position(|window| window == b"avcC")
        .expect("init contains avcC");
    assert_eq!(&segment.init[avcc + 4..avcc + 8], &[1, 0x42, 0x00, 0x0a]);
}

#[test]
fn segmenter_produces_media_segment() {
    let desc = h264_description();
    let config = SegmenterConfig {
        segment_duration: Duration::from_millis(100),
        lookback_count: 5,
    };
    let mut segmenter = Segmenter::new(config, &desc);
    let mut rx = segmenter.subscribe();

    // Push frames to trigger segment
    for i in 0..10 {
        let is_key = i % 5 == 0;
        let data = if is_key {
            h264_keyframe()
        } else {
            h264_interframe()
        };
        segmenter.push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(i * 40),
            dts: None,
            rtp_timestamp: (i * 3600) as u32,
            is_keyframe: is_key,
            data,
        });
    }

    segmenter.flush();

    let segment = rx.try_recv().unwrap();
    assert!(
        !segment.data.is_empty(),
        "media segment should not be empty"
    );
    // Media segment should start with moof box
    let moof_size = u32::from_be_bytes([
        segment.data[0],
        segment.data[1],
        segment.data[2],
        segment.data[3],
    ]);
    assert!(moof_size > 8, "moof size should be > 8");
    assert_eq!(&segment.data[4..8], b"moof");
    let mdat = segment
        .data
        .windows(4)
        .position(|window| window == b"mdat")
        .expect("media segment contains mdat");
    assert_eq!(&segment.data[mdat + 4..mdat + 9], &[0, 0, 0, 5, 0x65]);
}

#[test]
fn segment_ring_push_and_get() {
    let ring = SegmentRing::new(3);

    ring.push(Segment {
        sequence: 0,
        duration: Duration::from_secs(2),
        init: Bytes::from_static(b"init"),
        data: Bytes::from_static(b"data0"),
        starts_with_keyframe: true,
        timestamp: Duration::ZERO,
    });
    ring.push(Segment {
        sequence: 1,
        duration: Duration::from_secs(2),
        init: Bytes::from_static(b"init"),
        data: Bytes::from_static(b"data1"),
        starts_with_keyframe: true,
        timestamp: Duration::from_secs(2),
    });

    assert_eq!(ring.len(), 2);
    assert_eq!(ring.get(0).unwrap().data, Bytes::from_static(b"data0"));
    assert_eq!(ring.get(1).unwrap().data, Bytes::from_static(b"data1"));
    assert!(ring.get(2).is_none());
}

#[test]
fn segment_ring_eviction() {
    let ring = SegmentRing::new(2);

    for i in 0..5 {
        ring.push(Segment {
            sequence: i,
            duration: Duration::from_secs(2),
            init: Bytes::new(),
            data: Bytes::from(format!("data{i}")),
            starts_with_keyframe: true,
            timestamp: Duration::from_secs(i * 2),
        });
    }

    assert_eq!(ring.len(), 2);
    // Should have evicted segments 0, 1, 2
    assert!(ring.get(0).is_none());
    assert!(ring.get(1).is_none());
    assert!(ring.get(2).is_none());
    assert!(ring.get(3).is_some());
    assert!(ring.get(4).is_some());
}

#[test]
fn live_playlist_format() {
    let segments = vec![
        Segment {
            sequence: 0,
            duration: Duration::from_millis(2000),
            init: Bytes::new(),
            data: Bytes::new(),
            starts_with_keyframe: true,
            timestamp: Duration::ZERO,
        },
        Segment {
            sequence: 1,
            duration: Duration::from_millis(2500),
            init: Bytes::new(),
            data: Bytes::new(),
            starts_with_keyframe: true,
            timestamp: Duration::from_millis(2000),
        },
    ];
    let playlist = playlist::write_live_playlist(&segments, Duration::from_secs(3));

    assert!(playlist.starts_with("#EXTM3U"));
    assert!(playlist.contains("#EXT-X-VERSION:7"));
    assert!(playlist.contains("#EXT-X-TARGETDURATION:3"));
    assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    assert!(playlist.contains("#EXT-X-MAP:URI=\"init.m4s\""));
    assert!(playlist.contains("#EXTINF:2.000,"));
    assert!(playlist.contains("#EXTINF:2.500,"));
    assert!(playlist.contains("0.m4s"));
    assert!(playlist.contains("1.m4s"));
    assert!(!playlist.contains("#EXT-X-ENDLIST"));
}

#[test]
fn vod_playlist_format() {
    let segments = vec![Segment {
        sequence: 0,
        duration: Duration::from_millis(2000),
        init: Bytes::new(),
        data: Bytes::new(),
        starts_with_keyframe: true,
        timestamp: Duration::ZERO,
    }];
    let playlist = playlist::write_vod_playlist(&segments);

    assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    assert!(playlist.contains("#EXT-X-ENDLIST"));
}

#[test]
fn segmenter_accepts_non_contiguous_track_ids() {
    let mut desc = h264_description();
    desc.tracks[0].id = 7;
    let mut segmenter = Segmenter::new(
        SegmenterConfig {
            segment_duration: Duration::from_millis(100),
            lookback_count: 2,
        },
        &desc,
    );
    let mut rx = segmenter.subscribe();
    for i in 0..4 {
        segmenter.push(AccessUnit {
            track_id: 7,
            codec: Codec::H264,
            pts: Duration::from_millis(i * 40),
            dts: None,
            rtp_timestamp: (i * 3600) as u32,
            is_keyframe: i == 0,
            data: if i == 0 {
                h264_keyframe()
            } else {
                h264_interframe()
            },
        });
    }
    segmenter.flush();
    assert!(
        rx.try_recv().is_ok(),
        "non-contiguous track IDs must be routed"
    );
}

#[test]
fn segmenter_keyframe_alignment() {
    let desc = h264_description();
    let config = SegmenterConfig {
        segment_duration: Duration::from_millis(200),
        lookback_count: 5,
    };
    let mut segmenter = Segmenter::new(config, &desc);
    let mut rx = segmenter.subscribe();

    // Push frames: keyframe, inter, inter, keyframe (at segment boundary)
    for i in 0..12 {
        let is_key = i % 6 == 0;
        let data = if is_key {
            h264_keyframe()
        } else {
            h264_interframe()
        };
        segmenter.push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(i * 40),
            dts: None,
            rtp_timestamp: (i * 3600) as u32,
            is_keyframe: is_key,
            data,
        });
    }

    segmenter.flush();

    let first = rx.try_recv().unwrap();
    let second = rx.try_recv().unwrap();
    assert!(
        first.starts_with_keyframe,
        "first segment should start with keyframe"
    );
    assert!(
        second.starts_with_keyframe,
        "next segment should start with keyframe"
    );
}
