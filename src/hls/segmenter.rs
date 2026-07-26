use bytes::{BufMut, Bytes, BytesMut};
use std::time::Duration;
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
    codec: Codec,
    kind: MediaKind,
    clock_rate: u32,
    timescale: u32,
    samples: Vec<SampleEntry>,
    first_pts: Option<Duration>,
    last_pts: Option<Duration>,
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
    current_segment_pts: Option<Duration>,
    sequence: u64,
    tx: broadcast::Sender<Segment>,
    init_segment: Option<Bytes>,
}

impl Segmenter {
    pub fn new(
        config: SegmenterConfig,
        description: &MediaDescription,
    ) -> Self {
        let (tx, _) = broadcast::channel(config.lookback_count.max(2));
        let tracks = description
            .tracks
            .iter()
            .map(|t| TrackState {
                codec: t.codec,
                kind: t.kind,
                clock_rate: t.clock_rate,
                timescale: t.clock_rate,
                samples: Vec::new(),
                first_pts: None,
                last_pts: None,
            })
            .collect();

        Self {
            config,
            description: description.clone(),
            tracks,
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

    /// Push an AccessUnit into the segmenter.
    pub fn push(&mut self, unit: AccessUnit) {
        let track_idx = unit.track_id as usize;
        if track_idx >= self.tracks.len() {
            return;
        }

        let pts = unit.pts;
        let track = &mut self.tracks[track_idx];

        if track.first_pts.is_none() {
            track.first_pts = Some(pts);
        }

        let sample_duration = if let Some(last) = track.last_pts {
            pts.saturating_sub(last)
        } else {
            Duration::from_millis(40) // default ~25fps
        };
        track.last_pts = Some(pts);

        track.samples.push(SampleEntry {
            data: unit.data,
            pts,
            dts: unit.dts.unwrap_or(pts),
            is_keyframe: unit.is_keyframe,
            duration: sample_duration,
        });

        // Check if we should finalize the segment
        if self.current_segment_pts.is_none() {
            self.current_segment_pts = Some(pts);
        }

        let elapsed = pts.saturating_sub(self.current_segment_pts.unwrap());
        if elapsed >= self.config.segment_duration && unit.is_keyframe {
            self.finalize_segment(pts);
        }
    }

    fn finalize_segment(&mut self, end_pts: Duration) {
        // Check if we have any samples
        let has_samples = self.tracks.iter().any(|t| !t.samples.is_empty());
        if !has_samples {
            return;
        }

        // Build init segment if not cached
        if self.init_segment.is_none() {
            self.init_segment = Some(build_init_segment(&self.description));
        }

        // Build media segment
        let segment_data = build_media_segment(&mut self.tracks, self.sequence);
        let segment_pts = self.current_segment_pts.unwrap_or(end_pts);
        let segment_duration = end_pts.saturating_sub(segment_pts);

        let starts_with_keyframe = self.tracks.iter().any(|t| {
            t.samples.first().map(|s| s.is_keyframe).unwrap_or(false)
        });

        let segment = Segment {
            sequence: self.sequence,
            duration: segment_duration,
            init: self.init_segment.clone().unwrap_or_default(),
            data: segment_data,
            starts_with_keyframe,
            timestamp: segment_pts,
        };

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
            self.init_segment = Some(build_init_segment(&self.description));
        }

        let segment_data = build_media_segment(&mut self.tracks, self.sequence);
        let segment_pts = self.current_segment_pts.unwrap_or_default();

        let segment = Segment {
            sequence: self.sequence,
            duration: Duration::ZERO,
            init: self.init_segment.clone().unwrap_or_default(),
            data: segment_data,
            starts_with_keyframe: true,
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
fn build_init_segment(description: &MediaDescription) -> Bytes {
    let mut buf = BytesMut::with_capacity(1024);

    // ftyp box
    write_ftyp(&mut buf);

    // moov box
    write_moov(&mut buf, description);

    buf.freeze()
}

/// Build fMP4 media segment (moof + mdat).
fn build_media_segment(tracks: &mut [TrackState], sequence: u64) -> Bytes {
    let mut buf = BytesMut::with_capacity(64 * 1024);

    // Calculate total mdat data size
    let mdat_size: usize = tracks
        .iter()
        .flat_map(|t| t.samples.iter())
        .map(|s| s.data.len())
        .sum();

    // moof box
    write_moof(&mut buf, tracks, sequence, mdat_size);

    // mdat box
    write_mdat(&mut buf, tracks);

    buf.freeze()
}

fn write_ftyp(buf: &mut BytesMut) {
    let ftyp_data = b"isom"; // major brand
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

fn write_moov(buf: &mut BytesMut, description: &MediaDescription) {
    // Calculate moov size first
    let moov_start = buf.len();
    buf.put_u32(0); // placeholder for size
    buf.put_slice(b"moov");

    // mvhd box
    write_mvhd(buf, description);

    // trak boxes
    for (i, track) in description.tracks.iter().enumerate() {
        write_trak(buf, track, i as u32);
    }

    // Fill in moov size
    let moov_size = (buf.len() - moov_start) as u32;
    buf[moov_start..moov_start + 4].copy_from_slice(&moov_size.to_be_bytes());
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

fn write_trak(buf: &mut BytesMut, track: &crate::media::TrackDescription, track_id: u32) {
    let trak_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"trak");

    // tkhd box
    write_tkhd(buf, track, track_id);

    // mdia box
    write_mdia(buf, track, track_id);

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

fn write_mdia(buf: &mut BytesMut, track: &crate::media::TrackDescription, track_id: u32) {
    let mdia_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"mdia");

    // mdhd box
    write_mdhd(buf, track);

    // hdlr box
    write_hdlr(buf, track);

    // minf box
    write_minf(buf, track, track_id);

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
    let box_size = 33 + name.len(); // hdlr v0

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

fn write_minf(buf: &mut BytesMut, track: &crate::media::TrackDescription, track_id: u32) {
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
    write_stbl(buf, track, track_id);

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

fn write_stbl(buf: &mut BytesMut, track: &crate::media::TrackDescription, track_id: u32) {
    let stbl_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"stbl");

    // stsd box
    write_stsd(buf, track, track_id);

    // stts (time-to-sample) - empty for init segment
    write_empty_full_box(buf, b"stts");

    // stsc (sample-to-chunk) - empty
    write_empty_full_box(buf, b"stsc");

    // stsz (sample size) - empty
    write_empty_full_box(buf, b"stsz");

    // stco (chunk offset) - empty
    write_empty_full_box(buf, b"stco");

    let stbl_size = (buf.len() - stbl_start) as u32;
    buf[stbl_start..stbl_start + 4].copy_from_slice(&stbl_size.to_be_bytes());
}

fn write_empty_full_box(buf: &mut BytesMut, box_type: &[u8; 4]) {
    buf.put_u32(16); // size
    buf.put_slice(box_type);
    buf.put_u32(0); // version + flags
    buf.put_u32(0); // entry count
}

fn write_stsd(buf: &mut BytesMut, track: &crate::media::TrackDescription, _track_id: u32) {
    let stsd_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"stsd");
    buf.put_u32(0); // version + flags
    buf.put_u32(1); // entry count

    match track.kind {
        MediaKind::Video => write_avc_entry(buf, track),
        MediaKind::Audio => write_audio_entry(buf, track),
    }

    let stsd_size = (buf.len() - stsd_start) as u32;
    buf[stsd_start..stsd_start + 4].copy_from_slice(&stsd_size.to_be_bytes());
}

fn write_avc_entry(buf: &mut BytesMut, track: &crate::media::TrackDescription) {
    let entry_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"avc1"); // sample entry type
    buf.put_u32(0); // reserved
    buf.put_u16(0); // reserved
    buf.put_u16(1); // data_reference_index
    buf.put_u16(0); // pre-defined
    buf.put_u16(0); // reserved
    buf.put_u32(0); // pre-defined
    buf.put_u32(0); // pre-defined
    buf.put_u32(0); // pre-defined
    buf.put_u16(320); // width
    buf.put_u16(240); // height
    buf.put_u32(0x00480000); // horizresolution (72 dpi)
    buf.put_u32(0x00480000); // vertresolution
    buf.put_u32(0); // reserved
    buf.put_u16(1); // frame_count
    // compressorname (32 bytes)
    let name = b"rtflow\0";
    buf.put_u8(name.len() as u8);
    buf.put_slice(name);
    for _ in 0..(31 - name.len()) {
        buf.put_u8(0);
    }
    buf.put_u16(0x0018); // depth
    buf.put_i16(-1); // pre-defined

    // avcC box (empty for init - will be filled by segmenter)
    let avcc_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"avcC");
    buf.put_u8(1); // configurationVersion
    buf.put_u8(0x42); // AVCProfileIndication (Baseline)
    buf.put_u8(0xC0); // profile_compatibility
    buf.put_u8(0x1E); // AVCLevelIndication (3.0)
    buf.put_u8(0xFF); // lengthSizeMinus1 = 3
    buf.put_u8(0xE1); // numOfSequenceParameterSets = 1
    buf.put_u16(0); // SPS length (placeholder)
    buf.put_u8(1); // numOfPictureParameterSets = 1
    buf.put_u16(0); // PPS length (placeholder)

    let avcc_size = (buf.len() - avcc_start) as u32;
    buf[avcc_start..avcc_start + 4].copy_from_slice(&avcc_size.to_be_bytes());

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
        write_esds(buf);
    }

    let entry_size = (buf.len() - entry_start) as u32;
    buf[entry_start..entry_start + 4].copy_from_slice(&entry_size.to_be_bytes());
}

fn write_esds(buf: &mut BytesMut) {
    let esds_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"esds");
    buf.put_u32(0); // version + flags

    // ES_Descriptor
    buf.put_u8(0x03); // tag
    buf.put_u8(0x17); // length
    buf.put_u16(0); // ES_ID
    buf.put_u8(0x00); // streamDependenceFlag + URL_Flag + OCRstreamFlag

    // DecoderConfigDescriptor
    buf.put_u8(0x04); // tag
    buf.put_u8(0x0F); // length
    buf.put_u8(0x40); // objectTypeIndication (AAC)
    buf.put_u8(0x15); // streamType=5, upstream=0, reserved=1
    buf.put_u16(0x0001); // bufferSizeDB (high byte)
    buf.put_u8(0x00); // bufferSizeDB (low byte)
    buf.put_u32(128000); // maxBitrate
    buf.put_u32(128000); // avgBitrate

    // SLConfigDescriptor
    buf.put_u8(0x06); // tag
    buf.put_u8(0x01); // length
    buf.put_u8(0x02); // predefined (MP4)

    let esds_size = (buf.len() - esds_start) as u32;
    buf[esds_start..esds_start + 4].copy_from_slice(&esds_size.to_be_bytes());
}

fn write_moof(buf: &mut BytesMut, tracks: &[TrackState], sequence: u64, _mdat_size: usize) {
    let moof_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"moof");

    // mfhd box
    buf.put_u32(16); // size
    buf.put_slice(b"mfhd");
    buf.put_u32(0); // version + flags
    buf.put_u32(sequence as u32); // sequence_number

    // traf boxes
    for (track_id, track) in tracks.iter().enumerate() {
        if track.samples.is_empty() {
            continue;
        }
        write_traf(buf, track, track_id as u32);
    }

    let moof_size = (buf.len() - moof_start) as u32;
    buf[moof_start..moof_start + 4].copy_from_slice(&moof_size.to_be_bytes());
}

fn write_traf(buf: &mut BytesMut, track: &TrackState, track_id: u32) {
    let traf_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"traf");

    // tfhd box
    let tfhd_flags = 0x020000; // default-sample-duration-present
    buf.put_u32(16); // size
    buf.put_slice(b"tfhd");
    buf.put_u32(tfhd_flags); // version + flags
    buf.put_u32(track_id + 1); // track_ID

    // tfdt box (decode time)
    let base_pts = track.samples.first().map(|s| s.pts).unwrap_or_default();
    let base_pts_ticks = base_pts.as_secs() * track.timescale as u64
        + base_pts.subsec_nanos() as u64 * track.timescale as u64 / 1_000_000_000;

    buf.put_u32(16); // size
    buf.put_slice(b"tfdt");
    buf.put_u32(0); // version + flags
    buf.put_u32(base_pts_ticks as u32); // baseMediaDecodeTime

    // trun box
    let sample_count = track.samples.len() as u32;
    let trun_flags = 0x000301; // data-offset-present + sample-duration-present + sample-size-present
    let trun_size = 16 + 4 + sample_count * 8; // header + data-offset + per-sample

    let trun_start = buf.len();
    buf.put_u32(trun_size);
    buf.put_slice(b"trun");
    buf.put_u32(trun_flags); // version + flags
    buf.put_u32(sample_count);
    buf.put_u32(0); // data_offset placeholder

    let mut _data_offset = 0u32;
    for sample in &track.samples {
        let dur_ticks = sample.duration.as_secs() as u64 * track.timescale as u64
            + sample.duration.subsec_nanos() as u64 * track.timescale as u64 / 1_000_000_000;
        buf.put_u32((dur_ticks as u32).max(1)); // sample_duration
        buf.put_u32(sample.data.len() as u32); // sample_size
        _data_offset += sample.data.len() as u32;
    }

    // Fill in data_offset (offset from moof start to mdat payload)
    // moof size needs to be calculated - we'll fix it up after moof is complete
    // For now, store relative offset
    let moof_end_estimate = buf.len() + 8; // +8 for mdat header
    let _ = moof_end_estimate;
    // data_offset will be filled in write_mdat

    let traf_size = (buf.len() - traf_start) as u32;
    buf[traf_start..traf_start + 4].copy_from_slice(&traf_size.to_be_bytes());
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
