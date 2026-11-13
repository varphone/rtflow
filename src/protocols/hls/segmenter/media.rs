use super::TrackState;
use super::init::write_styp;
use super::util::{composition_offset_ticks, duration_to_ticks};
use crate::media::MediaKind;
use bytes::{BufMut, Bytes, BytesMut};

/// Build fMP4 media segment (styp + moof + mdat).
pub(super) fn build_media_segment(tracks: &mut [TrackState], sequence: u64) -> Bytes {
    // Pre-size the buffer: the mdat payload total is known up front, and
    // high-bitrate segments far exceed the old fixed 64 KiB start (which
    // forced repeated geometric reallocations of the whole segment).
    let payload_total: usize = tracks
        .iter()
        .flat_map(|t| t.samples.iter())
        .map(|s| s.data.len())
        .sum();
    let mut buf = BytesMut::with_capacity(64 * 1024 + payload_total);

    // styp box (segment type, required by DASH-IF and Chrome MSE)
    write_styp(&mut buf);

    // moof box
    write_moof(&mut buf, tracks, sequence);

    // mdat box
    write_mdat(&mut buf, tracks);

    buf.freeze()
}
pub(super) fn write_moof(buf: &mut BytesMut, tracks: &[TrackState], sequence: u64) {
    let moof_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"moof");

    buf.put_u32(16);
    buf.put_slice(b"mfhd");
    buf.put_u32(0);
    buf.put_u32((sequence as u32).wrapping_add(1));

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

pub(super) fn write_traf(buf: &mut BytesMut, track: &TrackState, track_id: u32) -> usize {
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
    // trun flags: data-offset-present(0x01) | sample-duration(0x100) | sample-size(0x200)
    //           | sample-flags(0x400) | sample-cts-offset(0x800) = 0x0F01
    // NOTE: do NOT set first-sample-flags-present(0x10000) unless we write first_sample_flags
    let trun_size = 20 + sample_count * 16;
    buf.put_u32(trun_size);
    buf.put_slice(b"trun");
    buf.put_u32(0x000f01);
    buf.put_u32(sample_count);
    let data_offset_position = buf.len();
    buf.put_u32(0);
    for sample in &track.samples {
        buf.put_u32(duration_to_ticks(sample.duration, track.timescale).max(1) as u32);
        buf.put_u32(sample.data.len() as u32);
        // ISO 14496-12 sample_flags:
        //   bits 24-25: sample_depends_on (2=does not depend, 1=depends)
        //   bit 16: sample_is_non_sync_sample
        // Keyframes: 0x02000000 (does not depend on others)
        // Non-keyframes: 0x01010000 (depends on others + non-sync)
        let flags = if track.kind == MediaKind::Video && !sample.is_keyframe {
            0x0101_0000
        } else {
            0x0200_0000
        };
        buf.put_u32(flags);
        let composition_offset = composition_offset_ticks(sample.pts, sample.dts, track.timescale);
        buf.put_i32(composition_offset);
    }

    let traf_size = (buf.len() - traf_start) as u32;
    buf[traf_start..traf_start + 4].copy_from_slice(&traf_size.to_be_bytes());
    data_offset_position
}

pub(super) fn write_mdat(buf: &mut BytesMut, tracks: &[TrackState]) {
    let mdat_payload_size: usize = tracks
        .iter()
        .flat_map(|t| t.samples.iter())
        .map(|s| s.data.len())
        .sum();

    let mdat_size = 8 + mdat_payload_size;
    // `mdat_size as u32` would silently wrap for segments > 4 GiB; the
    // hard time-based cut keeps segments small, but clamp defensively.
    let mdat_size_field = u32::try_from(mdat_size).unwrap_or_else(|_| {
        tracing::warn!("hls: media segment exceeds 4 GiB, clamping mdat size");
        u32::MAX
    });
    buf.put_u32(mdat_size_field);
    buf.put_slice(b"mdat");

    for track in tracks {
        for sample in &track.samples {
            buf.put_slice(&sample.data);
        }
    }
}
