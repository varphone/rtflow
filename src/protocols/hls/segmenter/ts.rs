//! Live MPEG-TS HLS via the `transmux` crate's streaming TS-HLS segmenter.
//!
//! This intentionally does not hand-roll PAT/PMT/PES or AVCC→Annex-B:
//! `transmux::StreamingTsHlsSegmenter` already implements keyframe-aligned
//! TS segments, PAT/PMT repetition, continuity counters, rolling playlists and
//! the Apple HLS IDR cut policy. We only adapt this project's `AccessUnit`s to
//! its IR (`TrackSpec` + `Sample`) and expose the result as an HLS `Segment`.

use super::{Segment, SegmentFormat, SegmenterConfig};
use crate::media::{Codec, MediaDescription, MediaKind};
use bytes::Bytes;
use std::time::Duration;
use transmux::avc_config::AVCConfigurationBox;
use transmux::mp4esds::{
    DecoderConfigDescriptor, DecoderSpecificInfo, ESDescriptor, EsdsBox, SLConfigDescriptor,
};
use transmux::ts_hls::StreamingTsHlsSegmenter;
use transmux::{CodecConfig, Sample, TrackId, TrackSpec};

pub(super) struct TsPipeline {
    segmenter: StreamingTsHlsSegmenter,
    sequence_offset: u64,
    timestamp: Duration,
    track_ids: std::collections::HashMap<u8, TrackId>,
    timescales: std::collections::HashMap<u8, u32>,
    /// Nominal per-sample duration in track timescale ticks. RTMP ingest
    /// timestamps are not reliable enough to drive HLS segment duration
    /// (ffmpeg publishers can arrive with 1.5–1.8× inflated deltas); the
    /// codec frame rate / AAC frame size is the stable clock.
    durations: std::collections::HashMap<u8, u32>,
}

fn aac_esds(sample_rate: u32, channels: u16) -> EsdsBox {
    // AAC-LC AudioSpecificConfig: object type 2, 4-bit frequency index,
    // 4-bit channel configuration. HLS AAC ingest is LC here.
    let index = match sample_rate {
        96_000 => 0u8,
        88_200 => 1,
        64_000 => 2,
        48_000 => 3,
        44_100 => 4,
        32_000 => 5,
        24_000 => 6,
        22_050 => 7,
        16_000 => 8,
        12_000 => 9,
        11_025 => 10,
        8_000 => 11,
        _ => 4,
    };
    let asc = vec![
        (2u8 << 3) | (index >> 1),
        ((index & 1) << 7) | ((channels as u8 & 0x0f) << 3),
    ];
    EsdsBox::new(ESDescriptor::new(
        1,
        0,
        Some(DecoderConfigDescriptor::new(
            0x40,
            0x05,
            false,
            0,
            0,
            0,
            Some(DecoderSpecificInfo::new(asc)),
        )),
        Some(SLConfigDescriptor::predefined_two()),
    ))
}

fn track_spec(track: &crate::media::TrackDescription) -> Option<TrackSpec> {
    match track.codec {
        Codec::H264 => {
            let config = track.fmtp.as_deref().and_then(|fmtp| {
                let value = fmtp
                    .split(';')
                    .find(|part| part.trim().starts_with("sprop-parameter-sets"))?
                    .split_once('=')?
                    .1;
                let mut nals = value.split(',').filter_map(|v| {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .decode(v.trim())
                        .ok()
                });
                let sps = nals.next()?;
                let pps = nals.next()?;
                let mut config = Vec::with_capacity(sps.len() + pps.len() + 16);
                config.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xff, 0xe1]);
                config.extend_from_slice(&(sps.len() as u16).to_be_bytes());
                config.extend_from_slice(&sps);
                config.push(1);
                config.extend_from_slice(&(pps.len() as u16).to_be_bytes());
                config.extend_from_slice(&pps);
                Some(config)
            })?;
            let avcc = AVCConfigurationBox::parse_body(&config).ok()?;
            Some(TrackSpec::new(
                TrackId::new(u32::from(track.id) + 1),
                track.clock_rate,
                CodecConfig::Avc {
                    config: avcc,
                    width: track.width.unwrap_or(0) as u16,
                    height: track.height.unwrap_or(0) as u16,
                },
            ))
        }
        Codec::Aac => Some(TrackSpec::new(
            TrackId::new(u32::from(track.id) + 1),
            track.clock_rate,
            CodecConfig::Aac {
                esds: aac_esds(track.clock_rate, track.channels.unwrap_or(2) as u16),
                channel_count: track.channels.unwrap_or(2) as u16,
                sample_rate: track.clock_rate,
                sample_size: 16,
            },
        )),
        _ => None,
    }
}

impl TsPipeline {
    pub(super) fn new(description: &MediaDescription, config: &SegmenterConfig) -> Option<Self> {
        let mut specs = Vec::new();
        let mut track_ids = std::collections::HashMap::new();
        let mut timescales = std::collections::HashMap::new();
        let mut durations = std::collections::HashMap::new();
        for track in &description.tracks {
            if let Some(spec) = track_spec(track) {
                track_ids.insert(track.id, spec.track_id);
                timescales.insert(track.id, track.clock_rate);
                let duration = match track.kind {
                    MediaKind::Video => track
                        .frame_rate
                        .map(|(num, den)| {
                            (u64::from(track.clock_rate) * u64::from(den) / u64::from(num).max(1))
                                as u32
                        })
                        .unwrap_or(3_000),
                    // AAC frame = 1024 samples; the track timescale is the
                    // sample rate, so its duration is exactly 1024 ticks.
                    MediaKind::Audio => 1024,
                };
                durations.insert(track.id, duration.max(1));
                specs.push(spec);
            }
        }
        if specs.is_empty()
            || !specs
                .iter()
                .any(|spec| matches!(spec.config, CodecConfig::Avc { .. }))
        {
            return None;
        }
        let target_secs = config.segment_duration.as_secs().max(1) as u32;
        let segmenter =
            StreamingTsHlsSegmenter::new(specs, target_secs, config.lookback_count.max(3)).ok()?;
        Some(Self {
            segmenter,
            sequence_offset: 0,
            timestamp: Duration::ZERO,
            track_ids,
            timescales,
            durations,
        })
    }

    pub(super) fn set_sequence(&mut self, sequence: u64) {
        self.sequence_offset = sequence;
    }

    pub(super) fn push(&mut self, unit: crate::media::AccessUnit) -> Vec<Segment> {
        let Some(&track_id) = self.track_ids.get(&unit.track_id) else {
            return Vec::new();
        };
        let timescale = u64::from(
            self.timescales
                .get(&unit.track_id)
                .copied()
                .unwrap_or(90_000),
        );
        let ticks = |d: Duration| -> i64 {
            (d.as_secs() * timescale + u64::from(d.subsec_nanos()) * timescale / 1_000_000_000)
                as i64
        };
        let sample = if unit.codec == Codec::Aac {
            Sample::from_raw(
                unit.data,
                Some(ticks(unit.dts.unwrap_or(unit.pts))),
                Some(ticks(unit.pts)),
                None,
            )
        } else {
            Sample::from_annexb(
                &unit.data,
                Some(ticks(unit.dts.unwrap_or(unit.pts))),
                Some(ticks(unit.pts)),
                None,
                unit.is_keyframe,
            )
        };

        let duration = self.durations.get(&unit.track_id).copied().unwrap_or(3_000);
        let mut sample = sample;
        sample.duration = Some(duration);
        if let Err(error) = self.segmenter.push(track_id, sample) {
            tracing::warn!(?error, "transmux TS-HLS push failed");
        }
        self.drain()
    }

    pub(super) fn flush(&mut self) -> Vec<Segment> {
        if self.segmenter.finish().is_err() {
            return Vec::new();
        }
        self.drain()
    }

    pub(super) fn mark_discontinuity(&mut self) {
        self.segmenter.mark_discontinuity();
    }

    fn drain(&mut self) -> Vec<Segment> {
        self.segmenter
            .take_ready()
            .into_iter()
            .map(|segment| {
                let duration = Duration::from_secs_f64(segment.duration.max(0.001));
                self.timestamp += duration;
                Segment {
                    sequence: segment.sequence + self.sequence_offset,
                    duration,
                    init: Bytes::new(),
                    data: Bytes::from(segment.bytes),
                    starts_with_keyframe: true,
                    discontinuity: segment.discontinuous,
                    discontinuity_sequence: None,
                    timestamp: self.timestamp,
                    format: SegmentFormat::MpegTs,
                }
            })
            .collect()
    }
}
