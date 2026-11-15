use super::TrackState;
use super::entries::{write_audio_entry, write_video_entry};
use super::sps::{h264_dimensions_from_avcc, h265_dimensions_from_hvcc};
use crate::media::{Codec, MediaDescription, MediaKind};
use bytes::{BufMut, Bytes, BytesMut};

/// Build fMP4 init segment (ftyp + moov).
pub(super) fn build_init_segment(description: &MediaDescription, tracks: &[TrackState]) -> Bytes {
    let mut buf = BytesMut::with_capacity(1024);

    // ftyp box
    write_ftyp(&mut buf);

    // moov box
    write_moov(&mut buf, description, tracks);

    buf.freeze()
}

pub(super) fn write_ftyp(buf: &mut BytesMut) {
    let compatible = [b"iso5".as_ref(), b"iso6".as_ref(), b"mp41".as_ref()];
    let compatible_len: usize = compatible.iter().map(|b| b.len()).sum();
    let box_size = 8 + 4 + 4 + compatible_len; // header + major + minor + compatible

    buf.put_u32(box_size as u32);
    buf.put_slice(b"ftyp");
    buf.put_slice(b"iso5"); // major brand (ISO BMFF v5, required for fMP4/DASH)
    buf.put_u32(0x200); // minor version
    for brand in &compatible {
        buf.put_slice(brand);
    }
}

/// Write styp (segment type) box for media segments.
/// Required by DASH-IF IOP and Chrome MSE for identifying fMP4 segments.
pub(super) fn write_styp(buf: &mut BytesMut) {
    // DASH-IF IOP requires msdh or msix brand for media segments
    let compatible = [
        b"msdh".as_ref(),
        b"msix".as_ref(),
        b"iso5".as_ref(),
        b"iso6".as_ref(),
    ];
    let compatible_len: usize = compatible.iter().map(|b| b.len()).sum();
    let box_size = 8 + 4 + 4 + compatible_len;

    buf.put_u32(box_size as u32);
    buf.put_slice(b"styp");
    buf.put_slice(b"msdh"); // major brand (DASH-IF media segment)
    buf.put_u32(0); // minor version
    for brand in &compatible {
        buf.put_slice(brand);
    }
}
pub(super) fn write_moov(
    buf: &mut BytesMut,
    description: &MediaDescription,
    tracks: &[TrackState],
) {
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

pub(super) fn write_mvex(buf: &mut BytesMut, track_count: usize) {
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

pub(super) fn write_mvhd(buf: &mut BytesMut, _description: &MediaDescription) {
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

pub(super) fn write_trak(
    buf: &mut BytesMut,
    track: &crate::media::TrackDescription,
    track_id: u32,
    codec_config: Option<&Bytes>,
) {
    let trak_start = buf.len();
    buf.put_u32(0); // placeholder
    buf.put_slice(b"trak");

    // Extract dimensions for video tracks
    let dimensions = if track.kind == MediaKind::Video {
        match track.codec {
            Codec::H265 => codec_config.and_then(|c| h265_dimensions_from_hvcc(c)),
            Codec::H264 => codec_config.and_then(|c| h264_dimensions_from_avcc(c)),
            Codec::Vp9 | Codec::Av1 => track
                .width
                .zip(track.height)
                .map(|(width, height)| (width as u16, height as u16)),
            _ => None,
        }
    } else {
        None
    };

    // tkhd box
    write_tkhd(buf, track, track_id, dimensions);

    // edts box (edit list) - required by some players (Firefox)
    write_edts(buf);

    // mdia box
    write_mdia(buf, track, track_id, codec_config);

    let trak_size = (buf.len() - trak_start) as u32;
    buf[trak_start..trak_start + 4].copy_from_slice(&trak_size.to_be_bytes());
}

pub(super) fn write_tkhd(
    buf: &mut BytesMut,
    _track: &crate::media::TrackDescription,
    track_id: u32,
    dimensions: Option<(u16, u16)>,
) {
    let box_size = 92; // tkhd v0
    buf.put_u32(box_size);
    buf.put_slice(b"tkhd");
    buf.put_u32(0x00000003); // version=0, flags=track_enabled|track_in_movie
    buf.put_u32(0); // creation time
    buf.put_u32(0); // modification time
    buf.put_u32(track_id + 1); // 1-based track_ID (ISOBMFF requires non-zero)
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
    // width and height as 16.16 fixed-point
    buf.put_u32(dimensions.map_or(0, |(w, _)| (w as u32) << 16));
    buf.put_u32(dimensions.map_or(0, |(_, h)| (h as u32) << 16));
}

/// Write edts (edit) box with an empty edit list.
/// Some players (notably Firefox) require this box for proper fMP4 playback.
pub(super) fn write_edts(buf: &mut BytesMut) {
    // edts box: 8 (header) + 28 (elst) = 36
    buf.put_u32(36);
    buf.put_slice(b"edts");
    // elst box: 8 (header) + 4 (version/flags) + 4 (entry_count) + 12 (one entry) = 28
    buf.put_u32(28);
    buf.put_slice(b"elst");
    buf.put_u32(0); // version=0, flags=0
    buf.put_u32(1); // entry_count
    // Edit list entry: segment_duration=0 (entire track), media_time=0, media_rate=1.0
    buf.put_u32(0); // segment_duration
    buf.put_u32(0); // media_time
    buf.put_u32(0x00010000); // media_rate (1.0 in 16.16 fixed-point)
}

pub(super) fn write_mdia(
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

pub(super) fn write_mdhd(buf: &mut BytesMut, track: &crate::media::TrackDescription) {
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

pub(super) fn write_hdlr(buf: &mut BytesMut, track: &crate::media::TrackDescription) {
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

pub(super) fn write_minf(
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

pub(super) fn write_dinf(buf: &mut BytesMut) {
    // dref box inside dinf
    // dref = header(8) + version_flags(4) + entry_count(4) + url_entry(12) = 28
    let dref_size: u32 = 28;
    // dinf = header(8) + dref(28) = 36
    let dinf_size: u32 = 36;

    buf.put_u32(dinf_size);
    buf.put_slice(b"dinf");
    buf.put_u32(dref_size);
    buf.put_slice(b"dref");
    buf.put_u32(0); // version + flags
    buf.put_u32(1); // entry count
    // url entry
    buf.put_u32(12); // size
    buf.put_slice(b"url ");
    buf.put_u32(0x00000001); // version + flags (self-contained)
}

pub(super) fn write_stbl(
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

pub(super) fn write_empty_stsz(buf: &mut BytesMut) {
    buf.put_u32(20);
    buf.put_slice(b"stsz");
    buf.put_u32(0);
    buf.put_u32(0);
    buf.put_u32(0);
}
pub(super) fn write_empty_full_box(buf: &mut BytesMut, box_type: &[u8; 4]) {
    buf.put_u32(16); // size
    buf.put_slice(box_type);
    buf.put_u32(0); // version + flags
    buf.put_u32(0); // entry count
}
pub(super) fn write_stsd(
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
