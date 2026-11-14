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
    // Media segment should start with styp box, then moof box
    assert_eq!(
        &segment.data[4..8],
        b"styp",
        "segment should start with styp"
    );
    let styp_size = u32::from_be_bytes([
        segment.data[0],
        segment.data[1],
        segment.data[2],
        segment.data[3],
    ]) as usize;
    // moof follows styp
    let moof_offset = styp_size;
    let moof_size = u32::from_be_bytes([
        segment.data[moof_offset],
        segment.data[moof_offset + 1],
        segment.data[moof_offset + 2],
        segment.data[moof_offset + 3],
    ]);
    assert!(moof_size > 8, "moof size should be > 8");
    assert_eq!(&segment.data[moof_offset + 4..moof_offset + 8], b"moof");
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
        discontinuity: false,
        discontinuity_sequence: None,
        timestamp: Duration::ZERO,
    });
    ring.push(Segment {
        sequence: 1,
        duration: Duration::from_secs(2),
        init: Bytes::from_static(b"init"),
        data: Bytes::from_static(b"data1"),
        starts_with_keyframe: true,
        discontinuity: false,
        discontinuity_sequence: None,
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
            discontinuity: false,
            discontinuity_sequence: None,
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
            discontinuity: false,
            discontinuity_sequence: None,
            timestamp: Duration::ZERO,
        },
        Segment {
            sequence: 1,
            duration: Duration::from_millis(2500),
            init: Bytes::new(),
            data: Bytes::new(),
            starts_with_keyframe: true,
            discontinuity: false,
            discontinuity_sequence: None,
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
        discontinuity: false,
        discontinuity_sequence: None,
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

#[test]
fn segmenter_rotates_audio_only_stream_by_time() {
    // Audio-only streams never carry a video keyframe; segments must still
    // be produced by the time-based hard cut instead of buffering forever.
    let desc = MediaDescription {
        session_name: Arc::from("audio"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Audio,
            codec: Codec::Aac,
            clock_rate: 48_000,
            channels: Some(2),
            payload_type: 97,
            fmtp: Some(Arc::from("config=1210")),
            control: Arc::from("trackID=0"),
            #[cfg(feature = "srtp")]
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
    let config = SegmenterConfig {
        segment_duration: Duration::from_millis(100),
        lookback_count: 5,
    };
    let mut segmenter = Segmenter::new(config, &desc);
    let mut rx = segmenter.subscribe();

    for i in 0..12u32 {
        segmenter.push(AccessUnit {
            track_id: 0,
            codec: Codec::Aac,
            pts: Duration::from_millis(i as u64 * 40),
            dts: None,
            rtp_timestamp: i * 480,
            is_keyframe: false,
            data: Bytes::from_static(&[0x21, 0x10, 0x00, 0x01]),
        });
    }
    segmenter.flush();

    let first = rx
        .try_recv()
        .expect("audio-only stream must produce segments");
    assert!(!first.data.is_empty());
    let second = rx
        .try_recv()
        .expect("audio-only stream must rotate segments");
    assert!(!second.data.is_empty());
}

/// A long-running stream whose audio and video clocks advance at their own
/// rates (video 33.33 ms/frame, audio 20 ms/block — independent baselines,
/// as on real encoders). After ~30 s of wall time the baselines drift more
/// than 30 s apart; the segmenter must NOT mistake this for a stream reset
/// (regression: discontinuity storm + zero-length segments).
#[test]
fn segmenter_survives_av_clock_drift() {
    let desc = MediaDescription::from_tracks(
        "drift",
        vec![
            TrackDescription::h264_video(0, &[], &[], 640, 360, 30),
            TrackDescription::aac_audio(1, 48_000, 2),
        ],
    );
    let config = SegmenterConfig {
        segment_duration: Duration::from_secs(2),
        lookback_count: 5,
    };
    let mut segmenter = Segmenter::new(config, &desc);
    let mut rx = segmenter.subscribe();
    let mut segments = Vec::new();

    // 4000 frames: video reaches ~133 s, audio ~80 s — the baselines drift
    // ~53 s apart, far beyond the 30 s reset threshold.
    for index in 0..4000u64 {
        let video_pts = Duration::from_micros(index * 1_000_000 / 30);
        let is_key = index % 60 == 0;
        let data = if is_key {
            h264_keyframe()
        } else {
            h264_interframe()
        };
        segmenter.push(AccessUnit::video(
            0,
            Codec::H264,
            video_pts,
            (index * 3000) as u32,
            is_key,
            data,
        ));
        let audio_pts = Duration::from_micros(index * 20_000);
        segmenter.push(AccessUnit::audio(
            1,
            Codec::Aac,
            audio_pts,
            (index * 960) as u32,
            Bytes::from_static(&[0xff, 0xf1, 0x50, 0x80, 0x00, 0x1f, 0xfc, 0x00]),
        ));
        while let Ok(segment) = rx.try_recv() {
            segments.push(segment);
        }
    }
    segmenter.flush();
    while let Ok(segment) = rx.try_recv() {
        segments.push(segment);
    }

    assert!(!segments.is_empty(), "stream must produce segments");
    assert!(
        segments.iter().all(|s| !s.duration.is_zero()),
        "no zero-duration segments"
    );
    assert_eq!(
        segments.iter().filter(|s| s.discontinuity).count(),
        0,
        "clock drift must not look like a stream reset"
    );
    // Segment durations stay in the 1–5 s band (2 s target, GOP-aligned).
    for segment in &segments {
        assert!(
            segment.duration >= Duration::from_secs(1)
                && segment.duration <= Duration::from_secs(5),
            "segment duration {:?} out of band",
            segment.duration
        );
    }
}

/// A real stream restart (timestamp wrap / source reboot) must still be
/// detected per-track and marked as a discontinuity — the track-level
/// reset check added for clock-drift robustness must not lose this.
#[test]
fn segmenter_detects_real_track_reset() {
    let desc = MediaDescription::from_tracks(
        "reset",
        vec![TrackDescription::h264_video(0, &[], &[], 640, 360, 30)],
    );
    let config = SegmenterConfig {
        segment_duration: Duration::from_secs(2),
        lookback_count: 5,
    };
    let mut segmenter = Segmenter::new(config, &desc);
    let mut rx = segmenter.subscribe();
    let mut segments = Vec::new();

    // 3000 frames ≈ 100 s of video.
    for index in 0..3000u64 {
        let pts = Duration::from_micros(index * 1_000_000 / 30);
        let is_key = index % 60 == 0;
        let data = if is_key {
            h264_keyframe()
        } else {
            h264_interframe()
        };
        segmenter.push(AccessUnit::video(
            0,
            Codec::H264,
            pts,
            (index * 3000) as u32,
            is_key,
            data,
        ));
        while let Ok(segment) = rx.try_recv() {
            segments.push(segment);
        }
    }

    // Source restarts: PTS jumps back to 0 (a >30 s backward jump).
    segmenter.push(AccessUnit::video(
        0,
        Codec::H264,
        Duration::ZERO,
        0,
        true,
        h264_keyframe(),
    ));
    segmenter.flush();
    while let Ok(segment) = rx.try_recv() {
        segments.push(segment);
    }

    let resets = segments.iter().filter(|s| s.discontinuity).count();
    assert_eq!(resets, 1, "restart must be marked once: {segments:#?}");
    let last = segments.last().expect("segment after restart");
    assert!(last.discontinuity, "segment after restart is flagged");
}

/// G.711 μ-law audio must be labeled `ulaw` (not `alaw`) in the fMP4
/// sample entry — a regression guard for the codec-label mapping.
#[test]
fn pcmu_segment_uses_ulaw_sample_entry() {
    let desc =
        MediaDescription::from_tracks("pcmu", vec![TrackDescription::pcmu_audio(1, 8_000, 1)]);
    let config = SegmenterConfig {
        segment_duration: Duration::from_millis(100),
        lookback_count: 5,
    };
    let mut segmenter = Segmenter::new(config, &desc);
    let mut rx = segmenter.subscribe();

    for i in 0..4u32 {
        segmenter.push(AccessUnit {
            track_id: 1,
            codec: Codec::Pcmu,
            pts: Duration::from_millis(i as u64 * 20),
            dts: None,
            rtp_timestamp: i * 160,
            is_keyframe: false,
            data: Bytes::from(vec![0xff; 40]),
        });
    }
    segmenter.flush();

    let segment = rx.try_recv().expect("segment produced");
    assert!(
        segment.init.as_ref().windows(4).any(|w| w == b"ulaw"),
        "init segment must carry a ulaw sample entry: {:?}",
        &segment.init[..segment.init.len().min(256)]
    );
}

#[cfg(feature = "server")]
#[tokio::test]
async fn registry_live_handler_serves_dynamic_mount() {
    use rtflow::{
        MountConfig, MountRegistry, RtspServer, ServerConfig,
        hls::{HlsConfig, live::HlsLiveManager},
        http::{HttpConfig, HttpServer, Method},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await.unwrap();
        String::from_utf8_lossy(&body).into_owned()
    }

    let _server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let registry = Arc::new(MountRegistry::new(4));
    let empty = MediaDescription {
        session_name: Arc::from("cam"),
        tracks: Vec::new(),
        #[cfg(feature = "multicast")]
        connection: None,
    };
    let mount = registry
        .register(
            Arc::from("/live/cam"),
            MountConfig::Publisher {
                description: empty,
                allow_replace_after_disconnect: true,
            },
        )
        .unwrap();
    mount.update_description(h264_description()).unwrap();
    let publisher = mount.publisher().unwrap();

    let manager = Arc::new(HlsLiveManager::new(
        registry.clone(),
        HlsConfig {
            segment_duration: Duration::from_millis(100),
            segment_count: 5,
            live: true,
        },
    ));
    let mut http = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    http.add_route(
        Method::Get,
        "/hls/*",
        rtflow::hls::live::create_hls_live_handler(manager),
    )
    .await;
    let addr = http.local_addr();
    tokio::spawn(async move { http.run().await });

    // First request starts the segmenter and waits for its first segment;
    // publish concurrently, as a real encoder/hls.js pair would.
    let first = tokio::spawn(async move { get(addr, "/hls/live/cam/index.m3u8").await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    for i in 0..12 {
        let key = i % 5 == 0;
        publisher
            .push(AccessUnit {
                track_id: 0,
                codec: Codec::H264,
                pts: Duration::from_millis(i * 40),
                dts: None,
                rtp_timestamp: i as u32 * 3600,
                is_keyframe: key,
                data: if key {
                    h264_keyframe()
                } else {
                    h264_interframe()
                },
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let body = first.await.unwrap();
    assert!(body.contains("200 OK"), "{body}");
    assert!(body.contains("#EXTM3U"), "{body}");

    let mut playlist = String::new();
    for _ in 0..20 {
        playlist = get(addr, "/hls/live/cam/index.m3u8").await;
        if playlist.contains(".m4s") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        playlist.contains(".m4s"),
        "playlist has no segment: {playlist}"
    );
    let init = get(addr, "/hls/live/cam/init.m4s").await;
    assert!(init.contains("200 OK"), "{init}");
}

#[cfg(all(feature = "file-storage", feature = "mp4", feature = "server"))]
#[tokio::test]
async fn vod_handler_generates_playlist_and_segments_from_file() {
    use rtflow::{
        FileSourceOptions, FileVodSource,
        hls::vod::{HlsVodConfig, HlsVodEntry, HlsVodManager, create_hls_vod_handler},
        http::{HttpConfig, HttpServer, Method},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await.unwrap();
        String::from_utf8_lossy(&body).into_owned()
    }

    let source =
        FileVodSource::open("tests/fixtures/h264-aac.mp4", FileSourceOptions::default()).unwrap();
    let cache = std::env::temp_dir().join(format!("rtflow-hls-vod-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);
    let manager = Arc::new(HlsVodManager::new(
        vec![HlsVodEntry {
            id: "fixture".into(),
            title: "fixture".into(),
            duration: None,
            source: Arc::new(source),
        }],
        HlsVodConfig {
            segment_duration: Duration::from_millis(500),
            cache_dir: cache.clone(),
        },
    ));
    let mut http = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    http.add_route(Method::Get, "/vod/*", create_hls_vod_handler(manager))
        .await;
    let addr = http.local_addr();
    tokio::spawn(async move { http.run().await });

    let playlist = get(addr, "/vod/fixture/index.m3u8").await;
    assert!(playlist.contains("200 OK"), "{playlist}");
    assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"), "{playlist}");
    assert!(playlist.contains("#EXT-X-ENDLIST"), "{playlist}");
    assert!(playlist.contains("init.m4s"), "{playlist}");
    let init = get(addr, "/vod/fixture/init.m4s").await;
    assert!(init.contains("200 OK"), "{init}");
    let segment = playlist
        .lines()
        .find(|line| line.ends_with(".m4s") && !line.contains("init"))
        .expect("segment URI");
    let segment = get(addr, &format!("/vod/fixture/{segment}")).await;
    assert!(segment.contains("200 OK"), "{segment}");
    let _ = std::fs::remove_dir_all(cache);
}

#[test]
fn video_segments_never_start_mid_gop() {
    let mut segmenter = Segmenter::new(
        SegmenterConfig {
            segment_duration: Duration::from_millis(100),
            lookback_count: 5,
        },
        &h264_description(),
    );
    let mut segments = segmenter.subscribe();
    // One IDR, then P frames well past the old 2×target hard cut. A
    // mid-GOP segment starts without a reference picture and hls.js shows
    // a black screen even though ffprobe accepts the container.
    segmenter.push(AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::ZERO,
        dts: None,
        rtp_timestamp: 0,
        is_keyframe: true,
        data: h264_keyframe(),
    });
    for i in 1..10 {
        segmenter.push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(i * 40),
            dts: None,
            rtp_timestamp: (i * 3600) as u32,
            is_keyframe: false,
            data: h264_interframe(),
        });
    }
    assert!(
        segments.try_recv().is_err(),
        "video must wait for the next IDR instead of cutting mid-GOP"
    );
    segmenter.push(AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::from_millis(400),
        dts: None,
        rtp_timestamp: 36_000,
        is_keyframe: true,
        data: h264_keyframe(),
    });
    let segment = segments.try_recv().expect("segment at IDR");
    assert!(segment.starts_with_keyframe);
}
