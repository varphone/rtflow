use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::{BufMut, Bytes, BytesMut};
use std::{collections::HashMap, time::Duration};
use tokio::sync::broadcast;

use crate::media::{AccessUnit, Codec, MediaDescription, MediaKind};

/// Configuration for the fMP4 segmenter.
#[derive(Clone, Debug)]
pub struct SegmenterConfig {
    /// Target segment duration.
    pub segment_duration: Duration,
    /// Maximum number of segments to keep in the ring buffer.
    pub lookback_count: usize,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            segment_duration: Duration::from_secs(2),
            lookback_count: 5,
        }
    }
}

/// A completed fMP4 segment.
#[derive(Clone, Debug)]
pub struct Segment {
    /// Sequence number (monotonically increasing).
    pub sequence: u64,
    /// Actual duration of this segment.
    pub duration: Duration,
    /// Init segment data (ftyp + moov).
    pub init: Bytes,
    /// Media segment data (moof + mdat).
    pub data: Bytes,
    /// Whether this segment starts with a keyframe.
    pub starts_with_keyframe: bool,
    /// Timestamp of the first sample in the segment.
    pub timestamp: Duration,
}

struct TrackState {
    kind: MediaKind,
    timescale: u32,
    samples: Vec<SampleEntry>,
    last_dts: Option<Duration>,
    codec_config: Option<Bytes>,
}

struct SampleEntry {
    data: Bytes,
    pts: Duration,
    dts: Duration,
    is_keyframe: bool,
    duration: Duration,
}

pub struct Segmenter {
    config: SegmenterConfig,
    description: MediaDescription,
    tracks: Vec<TrackState>,
    track_indices: HashMap<u8, usize>,
    current_segment_pts: Option<Duration>,
    sequence: u64,
    tx: broadcast::Sender<Segment>,
    init_segment: Option<Bytes>,
}

impl Segmenter {
    pub fn new(config: SegmenterConfig, description: &MediaDescription) -> Self {
        let (tx, _) = broadcast::channel(config.lookback_count.max(2));
        let track_indices = description
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id, index))
            .collect();
        let tracks = description
            .tracks
            .iter()
            .map(|t| TrackState {
                kind: t.kind,
                timescale: t.clock_rate,
                samples: Vec::new(),
                last_dts: None,
                codec_config: None,
            })
            .collect();

        Self {
            config,
            description: description.clone(),
            tracks,
            track_indices,
            current_segment_pts: None,
            sequence: 0,
            tx,
            init_segment: None,
        }
    }

    /// Subscribe to completed segments.
    pub fn subscribe(&self) -> broadcast::Receiver<Segment> {
        self.tx.subscribe()
    }

    /// Push an AccessUnit into the segmenter. Segments are cut immediately before
    /// the first keyframe at or after the target duration, so every new video
    /// segment begins at a random access point.
    pub fn push(&mut self, unit: AccessUnit) {
        let Some(&track_index) = self.track_indices.get(&unit.track_id) else {
            return;
        };
        let pts = unit.pts;
        let starts_new_segment = self.current_segment_pts.is_some_and(|start| {
            pts >= start
                && pts.saturating_sub(start) >= self.config.segment_duration
                && unit.is_keyframe
                && matches!(unit.codec, Codec::H264 | Codec::H265)
        });
        if unit.is_keyframe && matches!(unit.codec, Codec::H264 | Codec::H265) {
            tracing::debug!(?pts, ?self.current_segment_pts, "HLS video random-access point");
        }
        if starts_new_segment {
            self.finalize_segment(pts);
        }
        if self.current_segment_pts.is_none() {
            self.current_segment_pts = Some(pts);
        }

        let track = &mut self.tracks[track_index];
        if track.codec_config.is_none() {
            track.codec_config = codec_config_from_annex_b(unit.codec, &unit.data);
        }
        let dts = unit.dts.unwrap_or(pts);
        let sample_duration = track
            .last_dts
            .map(|last| dts.saturating_sub(last))
            .filter(|duration| !duration.is_zero())
            .unwrap_or_else(|| Duration::from_millis(40));
        track.last_dts = Some(dts);
        let data = match unit.codec {
            Codec::H264 | Codec::H265 => annex_b_to_length_prefixed(unit.codec, &unit.data),
            _ => unit.data,
        };
        track.samples.push(SampleEntry {
            data,
            pts,
            dts,
            is_keyframe: unit.is_keyframe,
            duration: sample_duration,
        });
    }

    fn finalize_segment(&mut self, end_pts: Duration) {
        // Check if we have any samples
        let has_samples = self.tracks.iter().any(|t| !t.samples.is_empty());
        if !has_samples {
            return;
        }

        // Build init segment if not cached
        if self.init_segment.is_none() {
            self.init_segment = Some(build_init_segment(&self.description, &self.tracks));
        }

        // Build media segment
        let segment_data = build_media_segment(&mut self.tracks, self.sequence);
        let segment_pts = self.current_segment_pts.unwrap_or(end_pts);
        let segment_duration = end_pts.saturating_sub(segment_pts);

        let starts_with_keyframe = self.tracks.iter().any(|track| {
            track.kind == MediaKind::Video
                && track
                    .samples
                    .first()
                    .is_some_and(|sample| sample.is_keyframe)
        });

        let segment = Segment {
            sequence: self.sequence,
            duration: segment_duration,
            init: self.init_segment.clone().unwrap_or_default(),
            data: segment_data,
            starts_with_keyframe,
            timestamp: segment_pts,
        };

        tracing::debug!(sequence = segment.sequence, ?segment.duration, "HLS segment finalized");
        let _ = self.tx.send(segment);
        self.sequence += 1;
        self.current_segment_pts = Some(end_pts);

        // Clear samples for next segment
        for track in &mut self.tracks {
            track.samples.clear();
        }
    }

    /// Flush any remaining samples as a final segment.
    pub fn flush(&mut self) {
        let has_samples = self.tracks.iter().any(|t| !t.samples.is_empty());
        if !has_samples {
            return;
        }

        if self.init_segment.is_none() {
            self.init_segment = Some(build_init_segment(&self.description, &self.tracks));
        }

        let segment_data = build_media_segment(&mut self.tracks, self.sequence);
        let segment_pts = self.current_segment_pts.unwrap_or_default();

        let end_pts = self
            .tracks
            .iter()
            .flat_map(|track| track.samples.iter())
            .map(|sample| sample.pts.saturating_add(sample.duration))
            .max()
            .unwrap_or(segment_pts);
        let segment = Segment {
            sequence: self.sequence,
            duration: end_pts.saturating_sub(segment_pts),
            init: self.init_segment.clone().unwrap_or_default(),
            data: segment_data,
            starts_with_keyframe: self.tracks.iter().any(|track| {
                track.kind == MediaKind::Video
                    && track
                        .samples
                        .first()
                        .is_some_and(|sample| sample.is_keyframe)
            }),
            timestamp: segment_pts,
        };

        let _ = self.tx.send(segment);
        self.sequence += 1;

        for track in &mut self.tracks {
            track.samples.clear();
        }
    }
}

/// Build fMP4 init segment (ftyp + moov).
fn build_init_segment(description: &MediaDescription, tracks: &[TrackState]) -> Bytes {
    let mut buf = BytesMut::with_capacity(1024);

    // ftyp box
    write_ftyp(&mut buf);

    // moov box
    write_moov(&mut buf, description, tracks);

    buf.freeze()
}

/// Build fMP4 media segment (moof + mdat).
fn build_media_segment(tracks: &mut [TrackState], sequence: u64) -> Bytes {
    let mut buf = BytesMut::with_capacity(64 * 1024);

    // Calculate total mdat data size
    write_moof(&mut buf, tracks, sequence);

    // mdat box
    write_mdat(&mut buf, tracks);

    buf.freeze()
}

fn duration_to_ticks(duration: Duration, timescale: u32) -> u64 {
    duration.as_secs().saturating_mul(u64::from(timescale))
        + u64::from(duration.subsec_nanos()).saturating_mul(u64::from(timescale)) / 1_000_000_000
}

fn composition_offset_ticks(pts: Duration, dts: Duration, timescale: u32) -> i32 {
    let ticks = if pts >= dts {
        duration_to_ticks(pts - dts, timescale) as i64
    } else {
        -(duration_to_ticks(dts - pts, timescale) as i64)
    };
    ticks.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn annex_b_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut marks = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let size = if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else if index + 4 <= data.len() && data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else {
            index += 1;
            continue;
        };
        marks.push((index, size));
        index += size;
    }
    marks
        .iter()
        .enumerate()
        .filter_map(|(index, (offset, size))| {
            let start = offset + size;
            let end = marks.get(index + 1).map_or(data.len(), |next| next.0);
            (start < end).then_some(&data[start..end])
        })
        .collect()
}

fn annex_b_to_length_prefixed(codec: Codec, data: &Bytes) -> Bytes {
    let nals = annex_b_nals(data);
    if nals.is_empty() {
        return data.clone();
    }
    let mut output = BytesMut::with_capacity(data.len());
    for nal in nals {
        let parameter_set = match codec {
            Codec::H264 => nal.first().is_some_and(|byte| matches!(byte & 0x1f, 7 | 8)),
            Codec::H265 => nal
                .first()
                .is_some_and(|byte| matches!((byte >> 1) & 0x3f, 32..=34)),
            _ => false,
        };
        if parameter_set {
            continue;
        }
        output.put_u32(nal.len() as u32);
        output.put_slice(nal);
    }
    output.freeze()
}

fn codec_config_from_annex_b(codec: Codec, data: &Bytes) -> Option<Bytes> {
    let nals = annex_b_nals(data);
    match codec {
        Codec::H264 => {
            let sps = nals
                .iter()
                .find(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 7))?;
            let pps = nals
                .iter()
                .find(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 8))?;
            build_avcc(sps, pps)
        }
        Codec::H265 => {
            let find = |kind| {
                nals.iter()
                    .find(|nal| nal.first().is_some_and(|byte| (byte >> 1) & 0x3f == kind))
            };
            build_hvcc(find(32)?, find(33)?, find(34)?)
        }
        _ => None,
    }
}

fn codec_config_from_fmtp(track: &crate::media::TrackDescription) -> Option<Bytes> {
    let fmtp = track.fmtp.as_deref()?;
    match track.codec {
        Codec::H264 => {
            let values = fmtp
                .split(';')
                .find(|value| value.trim().starts_with("sprop-parameter-sets"))?
                .split_once('=')?
                .1;
            let mut nals = values
                .split(',')
                .filter_map(|value| STANDARD.decode(value.trim()).ok());
            build_avcc(&nals.next()?, &nals.next()?)
        }
        Codec::H265 => {
            let value = |key: &str| {
                fmtp.split(';')
                    .find(|part| part.trim().starts_with(key))
                    .and_then(|part| {
                        part.split_once('=')
                            .map(|(_, value)| STANDARD.decode(value.trim()).ok())
                    })
                    .flatten()
            };
            build_hvcc(
                &value("sprop-vps")?,
                &value("sprop-sps")?,
                &value("sprop-pps")?,
            )
        }
        _ => None,
    }
}

fn build_avcc(sps: &[u8], pps: &[u8]) -> Option<Bytes> {
    if sps.len() < 4 || pps.is_empty() {
        return None;
    }
    let mut config = BytesMut::with_capacity(11 + sps.len() + pps.len());
    config.put_u8(1);
    config.put_u8(sps[1]);
    config.put_u8(sps[2]);
    config.put_u8(sps[3]);
    config.put_u8(0xff);
    config.put_u8(0xe1);
    config.put_u16(sps.len() as u16);
    config.put_slice(sps);
    config.put_u8(1);
    config.put_u16(pps.len() as u16);
    config.put_slice(pps);
    Some(config.freeze())
}

fn build_hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Option<Bytes> {
    if vps.len() < 2 || sps.len() <= 14 || pps.len() < 2 {
        return None;
    }
    let profile = &sps[2..];
    let mut config = BytesMut::with_capacity(128);
    config.put_u8(1);
    config.put_u8(profile[1]);
    config.put_slice(&profile[2..6]);
    config.put_slice(&profile[6..12]);
    config.put_u8(profile[12]);
    config.put_u16(0xf000);
    config.put_u8(0xfc);
    config.put_u8(0xfd);
    config.put_u8(0xf8);
    config.put_u8(0xf8);
    config.put_u16(0);
    config.put_u8(0x0f);
    config.put_u8(3);
    for (kind, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        config.put_u8(0x80 | kind);
        config.put_u16(1);
        config.put_u16(nal.len() as u16);
        config.put_slice(nal);
    }
    Some(config.freeze())
}

fn write_ftyp(buf: &mut BytesMut) {
    let compatible = [b"isom".as_ref(), b"iso6".as_ref(), b"mp41".as_ref()];
    let compatible_len: usize = compatible.iter().map(|b| b.len()).sum();
    let box_size = 8 + 4 + 4 + compatible_len; // header + major + minor + compatible

    buf.put_u32(box_size as u32);
    buf.put_slice(b"ftyp");
    buf.put_slice(b"isom"); // major brand
    buf.put_u32(0x200); // minor version
    for brand in &compatible {
        buf.put_slice(brand);
    }
}

fn write_moov(buf: &mut BytesMut, description: &MediaDescription, tracks: &[TrackState]) {
    // Calculate moov size first
    let moov_start = buf.len();
    buf.put_u32(0); // placeholder for size
    buf.put_slice(b"moov");

    // mvhd box
    write_mvhd(buf, description);

    // trak boxes
    for (i, track) in description.tracks.iter().enumerate() {
        write_trak(
            buf,
            track,
            i as u32,
            tracks.get(i).and_then(|state| state.codec_config.as_ref()),
        );
    }

    write_mvex(buf, description.tracks.len());

    // Fill in moov size
    let moov_size = (buf.len() - moov_start) as u32;
    buf[moov_start..moov_start + 4].copy_from_slice(&moov_size.to_be_bytes());
}

fn write_mvex(buf: &mut BytesMut, track_count: usize) {
    let mvex_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"mvex");
    for track_id in 1..=track_count {
        buf.put_u32(32);
        buf.put_slice(b"trex");
        buf.put_u32(0);
        buf.put_u32(track_id as u32);
        buf.put_u32(1);
        buf.put_u32(0);
        buf.put_u32(0);
        buf.put_u32(0);
    }
    let mvex_size = (buf.len() - mvex_start) as u32;
    buf[mvex_start..mvex_start + 4].copy_from_slice(&mvex_size.to_be_bytes());
}

fn write_mvhd(buf: &mut BytesMut, _description: &MediaDescription) {
    let box_size = 108; // mvhd v0
    buf.put_u32(box_size);
    buf.put_slice(b"mvhd");
    buf.put_u32(0); // version + flags
    buf.put_u32(0); // creation time
    buf.put_u32(0); // modification time
    buf.put_u32(1000); // timescale
    buf.put_u32(0); // duration (unknown for live)
    buf.put_u32(0x00010000); // rate (1.0)
    buf.put_u16(0x0100); // volume (1.0)
    buf.put_u16(0); // reserved
    buf.put_u32(0); // reserved
    buf.put_u32(0); // reserved
    // identity matrix
    for &v in &[0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
        buf.put_u32(v);
    }
    // pre-defined
    for _ in 0..6 {
        buf.put_u32(0);
    }
    buf.put_u32(0xFFFFFFFF); // next_track_ID
}

fn write_trak(
    buf: &mut BytesMut,
    track: &crate::media::TrackDescription,
    track_id: u32,
    codec_config: Option<&Bytes>,
) {
    let trak_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"trak");

    // tkhd box
    write_tkhd(buf, track, track_id);

    // mdia box
    write_mdia(buf, track, track_id, codec_config);

    let trak_size = (buf.len() - trak_start) as u32;
    buf[trak_start..trak_start + 4].copy_from_slice(&trak_size.to_be_bytes());
}

fn write_tkhd(buf: &mut BytesMut, _track: &crate::media::TrackDescription, track_id: u32) {
    let box_size = 92; // tkhd v0
    buf.put_u32(box_size);
    buf.put_slice(b"tkhd");
    buf.put_u32(0x00000003); // version=0, flags=track_enabled|track_in_movie
    buf.put_u32(0); // creation time
    buf.put_u32(0); // modification time
    buf.put_u32(track_id);
    buf.put_u32(0); // reserved
    buf.put_u32(0); // duration
    buf.put_u32(0); // reserved
    buf.put_u32(0); // reserved
    buf.put_u16(0); // layer
    buf.put_u16(0); // alternate_group
    buf.put_u16(0); // volume
    buf.put_u16(0); // reserved
    // identity matrix
    for &v in &[0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
        buf.put_u32(v);
    }
    buf.put_u32(0); // width (0 for audio)
    buf.put_u32(0); // height
}

fn write_mdia(
    buf: &mut BytesMut,
    track: &crate::media::TrackDescription,
    track_id: u32,
    codec_config: Option<&Bytes>,
) {
    let mdia_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"mdia");

    // mdhd box
    write_mdhd(buf, track);

    // hdlr box
    write_hdlr(buf, track);

    // minf box
    write_minf(buf, track, track_id, codec_config);

    let mdia_size = (buf.len() - mdia_start) as u32;
    buf[mdia_start..mdia_start + 4].copy_from_slice(&mdia_size.to_be_bytes());
}

fn write_mdhd(buf: &mut BytesMut, track: &crate::media::TrackDescription) {
    let box_size = 32; // mdhd v0
    buf.put_u32(box_size);
    buf.put_slice(b"mdhd");
    buf.put_u32(0); // version + flags
    buf.put_u32(0); // creation time
    buf.put_u32(0); // modification time
    buf.put_u32(track.clock_rate); // timescale
    buf.put_u32(0); // duration
    buf.put_u16(0x55C4); // language (und)
    buf.put_u16(0); // pre-defined
}

fn write_hdlr(buf: &mut BytesMut, track: &crate::media::TrackDescription) {
    let handler = match track.kind {
        MediaKind::Video => b"vide".as_ref(),
        MediaKind::Audio => b"soun".as_ref(),
    };
    let name = b"rtflow\0";
    let box_size = 32 + name.len(); // hdlr v0

    buf.put_u32(box_size as u32);
    buf.put_slice(b"hdlr");
    buf.put_u32(0); // version + flags
    buf.put_u32(0); // pre-defined
    buf.put_slice(handler);
    buf.put_u32(0); // reserved
    buf.put_u32(0); // reserved
    buf.put_u32(0); // reserved
    buf.put_slice(name);
}

fn write_minf(
    buf: &mut BytesMut,
    track: &crate::media::TrackDescription,
    track_id: u32,
    codec_config: Option<&Bytes>,
) {
    let minf_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"minf");

    // vmhd or smhd
    match track.kind {
        MediaKind::Video => {
            buf.put_u32(20); // size
            buf.put_slice(b"vmhd");
            buf.put_u32(0x00000001); // version + flags
            buf.put_u64(0); // graphicsmode + opcolor
        }
        MediaKind::Audio => {
            buf.put_u32(16); // size
            buf.put_slice(b"smhd");
            buf.put_u32(0); // version + flags
            buf.put_u32(0); // balance + reserved
        }
    }

    // dinf box (required)
    write_dinf(buf);

    // stbl box
    write_stbl(buf, track, track_id, codec_config);

    let minf_size = (buf.len() - minf_start) as u32;
    buf[minf_start..minf_start + 4].copy_from_slice(&minf_size.to_be_bytes());
}

fn write_dinf(buf: &mut BytesMut) {
    // dref box inside dinf
    let dref_data_len = 12; // one entry
    let dref_size = 8 + 8 + dref_data_len;
    let dinf_size = 8 + dref_size;

    buf.put_u32(dinf_size);
    buf.put_slice(b"dinf");
    buf.put_u32(dref_size as u32);
    buf.put_slice(b"dref");
    buf.put_u32(0); // version + flags
    buf.put_u32(1); // entry count
    // url entry
    buf.put_u32(12); // size
    buf.put_slice(b"url ");
    buf.put_u32(0x00000001); // version + flags (self-contained)
}

fn write_stbl(
    buf: &mut BytesMut,
    track: &crate::media::TrackDescription,
    track_id: u32,
    codec_config: Option<&Bytes>,
) {
    let stbl_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"stbl");

    // stsd box
    write_stsd(buf, track, track_id, codec_config);

    // stts (time-to-sample) - empty for init segment
    write_empty_full_box(buf, b"stts");

    // stsc (sample-to-chunk) - empty
    write_empty_full_box(buf, b"stsc");

    // stsz (sample size) - empty
    write_empty_stsz(buf);

    // stco (chunk offset) - empty
    write_empty_full_box(buf, b"stco");

    let stbl_size = (buf.len() - stbl_start) as u32;
    buf[stbl_start..stbl_start + 4].copy_from_slice(&stbl_size.to_be_bytes());
}

fn write_empty_stsz(buf: &mut BytesMut) {
    buf.put_u32(20);
    buf.put_slice(b"stsz");
    buf.put_u32(0);
    buf.put_u32(0);
    buf.put_u32(0);
}

fn write_empty_full_box(buf: &mut BytesMut, box_type: &[u8; 4]) {
    buf.put_u32(16); // size
    buf.put_slice(box_type);
    buf.put_u32(0); // version + flags
    buf.put_u32(0); // entry count
}

fn write_stsd(
    buf: &mut BytesMut,
    track: &crate::media::TrackDescription,
    _track_id: u32,
    codec_config: Option<&Bytes>,
) {
    let stsd_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"stsd");
    buf.put_u32(0); // version + flags
    buf.put_u32(1); // entry count

    match track.kind {
        MediaKind::Video => write_video_entry(buf, track, codec_config),
        MediaKind::Audio => write_audio_entry(buf, track),
    }

    let stsd_size = (buf.len() - stsd_start) as u32;
    buf[stsd_start..stsd_start + 4].copy_from_slice(&stsd_size.to_be_bytes());
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn read_bits(&mut self, count: usize) -> Option<u64> {
        if self.bit.checked_add(count)? > self.data.len().checked_mul(8)? {
            return None;
        }
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 1) | u64::from((self.data[self.bit / 8] >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        Some(value)
    }

    fn read_ue(&mut self) -> Option<u64> {
        let mut zeros = 0usize;
        while self.read_bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let suffix = self.read_bits(zeros)?;
        Some((1u64 << zeros) - 1 + suffix)
    }
}

fn h265_dimensions_from_hvcc(config: &[u8]) -> Option<(u16, u16)> {
    if config.len() < 23 || config[0] != 1 {
        return None;
    }
    let mut position = 23;
    for _ in 0..config[22] {
        let nal_type = *config.get(position)? & 0x3f;
        position += 1;
        let count =
            u16::from_be_bytes([*config.get(position)?, *config.get(position + 1)?]) as usize;
        position += 2;
        for _ in 0..count {
            let length =
                u16::from_be_bytes([*config.get(position)?, *config.get(position + 1)?]) as usize;
            position += 2;
            let end = position.checked_add(length)?;
            let nal = config.get(position..end)?;
            position = end;
            if nal_type == 33 {
                return h265_dimensions_from_sps(nal);
            }
        }
    }
    None
}

fn h265_dimensions_from_sps(sps: &[u8]) -> Option<(u16, u16)> {
    if sps.len() < 3 {
        return None;
    }
    let mut rbsp = Vec::with_capacity(sps.len());
    let mut zeros = 0usize;
    for &byte in &sps[2..] {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let mut bits = BitReader {
        data: &rbsp,
        bit: 0,
    };
    bits.read_bits(4)?;
    let max_sub_layers = bits.read_bits(3)? as usize;
    bits.read_bits(1)?;
    bits.read_bits(96)?;
    let mut profile_present = [false; 7];
    let mut level_present = [false; 7];
    for index in 0..max_sub_layers {
        profile_present[index] = bits.read_bits(1)? != 0;
        level_present[index] = bits.read_bits(1)? != 0;
    }
    if max_sub_layers > 0 {
        bits.read_bits((8 - max_sub_layers) * 2)?;
    }
    for index in 0..max_sub_layers {
        if profile_present[index] {
            bits.read_bits(96)?;
        }
        if level_present[index] {
            bits.read_bits(8)?;
        }
    }
    bits.read_ue()?;
    let chroma_format = bits.read_ue()? as u8;
    if chroma_format == 3 {
        bits.read_bits(1)?;
    }
    let width = bits.read_ue()?;
    let height = bits.read_ue()?;
    let conformance_window = bits.read_bits(1)? != 0;
    let (left, right, top, bottom) = if conformance_window {
        (
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };
    let (sub_width, sub_height) = match chroma_format {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = width.saturating_sub((left + right) * sub_width);
    let height = height.saturating_sub((top + bottom) * sub_height);
    Some((u16::try_from(width).ok()?, u16::try_from(height).ok()?))
}

fn write_video_entry(
    buf: &mut BytesMut,
    track: &crate::media::TrackDescription,
    codec_config: Option<&Bytes>,
) {
    let config_from_fmtp = codec_config_from_fmtp(track);
    let codec_config = codec_config.or(config_from_fmtp.as_ref());
    let entry_start = buf.len();
    buf.put_u32(0);
    match track.codec {
        Codec::H265 => buf.put_slice(b"hvc1"),
        _ => buf.put_slice(b"avc1"),
    }
    buf.put_u32(0);
    buf.put_u16(0);
    buf.put_u16(1);
    buf.put_u16(0);
    buf.put_u16(0);
    buf.put_u32(0);
    buf.put_u32(0);
    buf.put_u32(0);
    let dimensions = match track.codec {
        Codec::H265 => codec_config.and_then(|config| h265_dimensions_from_hvcc(config)),
        _ => None,
    };
    buf.put_u16(dimensions.map_or(0, |(width, _)| width));
    buf.put_u16(dimensions.map_or(0, |(_, height)| height));
    buf.put_u32(0x00480000);
    buf.put_u32(0x00480000);
    buf.put_u32(0);
    buf.put_u16(1);
    let name = b"rtflow";
    buf.put_u8(name.len() as u8);
    buf.put_slice(name);
    for _ in 0..(31 - name.len()) {
        buf.put_u8(0);
    }
    buf.put_u16(0x0018);
    buf.put_i16(-1);

    let config_start = buf.len();
    buf.put_u32(0);
    match track.codec {
        Codec::H265 => buf.put_slice(b"hvcC"),
        _ => buf.put_slice(b"avcC"),
    }
    if let Some(config) = codec_config {
        buf.put_slice(config);
    }
    let config_size = (buf.len() - config_start) as u32;
    buf[config_start..config_start + 4].copy_from_slice(&config_size.to_be_bytes());

    let entry_size = (buf.len() - entry_start) as u32;
    buf[entry_start..entry_start + 4].copy_from_slice(&entry_size.to_be_bytes());
}

fn write_audio_entry(buf: &mut BytesMut, track: &crate::media::TrackDescription) {
    let entry_start = buf.len();
    buf.put_u32(0); // placeholder
    match track.codec {
        Codec::Aac => buf.put_slice(b"mp4a"),
        Codec::Pcma | Codec::Pcmu => buf.put_slice(b"alaw"),
        _ => buf.put_slice(b"mp4a"),
    }
    buf.put_u32(0); // reserved
    buf.put_u16(0); // reserved
    buf.put_u16(1); // data_reference_index
    buf.put_u32(0); // reserved
    buf.put_u32(0); // reserved
    buf.put_u16(track.channels.unwrap_or(1) as u16); // channelcount
    buf.put_u16(16); // samplesize
    buf.put_u16(0); // pre-defined
    buf.put_u16(0); // reserved
    buf.put_u32(track.clock_rate << 16); // samplerate (fixed point)

    // esds box for AAC
    if track.codec == Codec::Aac {
        let config = aac_config_from_fmtp(track)
            .unwrap_or_else(|| default_aac_config(track.clock_rate, track.channels.unwrap_or(1)));
        write_esds(buf, &config);
    }

    let entry_size = (buf.len() - entry_start) as u32;
    buf[entry_start..entry_start + 4].copy_from_slice(&entry_size.to_be_bytes());
}

fn aac_config_from_fmtp(track: &crate::media::TrackDescription) -> Option<Bytes> {
    let value = track
        .fmtp
        .as_deref()?
        .split(';')
        .find(|part| part.trim().starts_with("config="))?
        .split_once('=')?
        .1
        .trim();
    if value.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect();
    bytes.filter(|bytes| !bytes.is_empty()).map(Bytes::from)
}

fn default_aac_config(clock_rate: u32, channels: u8) -> Bytes {
    let sample_rates = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let index = sample_rates
        .iter()
        .position(|rate| *rate == clock_rate)
        .unwrap_or(4) as u8;
    Bytes::from(vec![
        (2 << 3) | (index >> 1),
        ((index & 1) << 7) | ((channels.min(7)) << 3),
    ])
}

fn write_esds(buf: &mut BytesMut, config: &Bytes) {
    let esds_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"esds");
    buf.put_u32(0);

    let decoder_config_size = 13 + 2 + config.len();
    let es_descriptor_size = 3 + 2 + decoder_config_size + 3;
    buf.put_u8(0x03);
    buf.put_u8(es_descriptor_size as u8);
    buf.put_u16(0);
    buf.put_u8(0);
    buf.put_u8(0x04);
    buf.put_u8(decoder_config_size as u8);
    buf.put_u8(0x40);
    buf.put_u8(0x15);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u32(128_000);
    buf.put_u32(128_000);
    buf.put_u8(0x05);
    buf.put_u8(config.len() as u8);
    buf.put_slice(config);
    buf.put_u8(0x06);
    buf.put_u8(1);
    buf.put_u8(2);

    let esds_size = (buf.len() - esds_start) as u32;
    buf[esds_start..esds_start + 4].copy_from_slice(&esds_size.to_be_bytes());
}

fn write_moof(buf: &mut BytesMut, tracks: &[TrackState], sequence: u64) {
    let moof_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"moof");

    buf.put_u32(16);
    buf.put_slice(b"mfhd");
    buf.put_u32(0);
    buf.put_u32(sequence as u32 + 1);

    let mut data_offset_positions = Vec::new();
    for (track_id, track) in tracks.iter().enumerate() {
        if !track.samples.is_empty() {
            data_offset_positions.push(write_traf(buf, track, track_id as u32));
        }
    }

    let moof_size = (buf.len() - moof_start) as u32;
    buf[moof_start..moof_start + 4].copy_from_slice(&moof_size.to_be_bytes());

    let mut media_offset = moof_size + 8;
    for (data_offset_position, track) in data_offset_positions
        .into_iter()
        .zip(tracks.iter().filter(|track| !track.samples.is_empty()))
    {
        buf[data_offset_position..data_offset_position + 4]
            .copy_from_slice(&media_offset.to_be_bytes());
        media_offset += track
            .samples
            .iter()
            .map(|sample| sample.data.len() as u32)
            .sum::<u32>();
    }
}

fn write_traf(buf: &mut BytesMut, track: &TrackState, track_id: u32) -> usize {
    let traf_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"traf");

    buf.put_u32(16);
    buf.put_slice(b"tfhd");
    buf.put_u32(0x020000);
    buf.put_u32(track_id + 1);

    let base_dts = track
        .samples
        .first()
        .map(|sample| sample.dts)
        .unwrap_or_default();
    let base_dts_ticks = duration_to_ticks(base_dts, track.timescale);
    buf.put_u32(20);
    buf.put_slice(b"tfdt");
    buf.put_u32(0x01000000);
    buf.put_u64(base_dts_ticks);

    let sample_count = track.samples.len() as u32;
    let trun_size = 20 + sample_count * 16;
    buf.put_u32(trun_size);
    buf.put_slice(b"trun");
    buf.put_u32(0x010f01);
    buf.put_u32(sample_count);
    let data_offset_position = buf.len();
    buf.put_u32(0);
    for sample in &track.samples {
        buf.put_u32(duration_to_ticks(sample.duration, track.timescale).max(1) as u32);
        buf.put_u32(sample.data.len() as u32);
        let flags = if track.kind == MediaKind::Video && !sample.is_keyframe {
            0x0001_0000
        } else {
            0
        };
        buf.put_u32(flags);
        let composition_offset = composition_offset_ticks(sample.pts, sample.dts, track.timescale);
        buf.put_i32(composition_offset);
    }

    let traf_size = (buf.len() - traf_start) as u32;
    buf[traf_start..traf_start + 4].copy_from_slice(&traf_size.to_be_bytes());
    data_offset_position
}

fn write_mdat(buf: &mut BytesMut, tracks: &[TrackState]) {
    let mdat_payload_size: usize = tracks
        .iter()
        .flat_map(|t| t.samples.iter())
        .map(|s| s.data.len())
        .sum();

    let mdat_size = 8 + mdat_payload_size;
    buf.put_u32(mdat_size as u32);
    buf.put_slice(b"mdat");

    for track in tracks {
        for sample in &track.samples {
            buf.put_slice(&sample.data);
        }
    }
}
