//! FLV container primitives shared by the HTTP-FLV/WS-FLV muxer
//! (`http_flv::mux`) and the RTMP FLV codec (`rtmp::flv`).

use std::time::Duration;

use bytes::{BufMut, BytesMut};

/// FLV tag types.
pub const TAG_TYPE_AUDIO: u8 = 0x08;
pub const TAG_TYPE_VIDEO: u8 = 0x09;
pub const TAG_TYPE_METADATA: u8 = 0x12;

/// Size of the FLV tag header: type(1) + size(3) + timestamp(3+1) +
/// stream id(3).
pub const FLV_TAG_HEADER_SIZE: usize = 11;

/// FLV sound formats (4 bits of the audio tag body header byte).
pub const SOUND_FORMAT_ALAW: u8 = 7;
pub const SOUND_FORMAT_ULAW: u8 = 8;
pub const SOUND_FORMAT_AAC: u8 = 10;

/// AAC audio tag header: sound format 10, 44 kHz (3), 16-bit (1),
/// stereo (1). Zero rate bits would claim 5.5 kHz and break ffmpeg's FLV
/// demuxer, so the rate bits are never left at 0.
pub const AAC_SOUND_HEADER: u8 = (SOUND_FORMAT_AAC << 4) | 0x0f;

/// Write an 11-byte FLV tag header: type, 24-bit body size, 24-bit
/// timestamp plus 8-bit extended timestamp, 24-bit stream id (0).
pub fn write_tag_header(buf: &mut BytesMut, tag_type: u8, body_size: usize, timestamp_ms: u32) {
    let timestamp_ext = (timestamp_ms >> 24) as u8;
    let timestamp_base = timestamp_ms & 0x00ff_ffff;
    buf.put_u8(tag_type);
    buf.put_u8((body_size >> 16) as u8);
    buf.put_u8((body_size >> 8) as u8);
    buf.put_u8(body_size as u8);
    buf.put_u8((timestamp_base >> 16) as u8);
    buf.put_u8((timestamp_base >> 8) as u8);
    buf.put_u8(timestamp_base as u8);
    buf.put_u8(timestamp_ext);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
}

/// Composition-time (PTS − DTS) offset in milliseconds, clamped to the
/// FLV signed 24-bit range. `None` DTS (no B-frames) → 0.
pub fn composition_time_ms(pts: Duration, dts: Option<Duration>) -> i32 {
    match dts {
        Some(dts) if pts >= dts => (pts - dts).as_millis().min(0x7f_ffff) as i32,
        Some(dts) => -((dts - pts).as_millis().min(0x80_0000) as i32),
        None => 0,
    }
}

/// Write the 24-bit composition-time field (big-endian; the top bit of
/// the 24-bit value is the sign).
pub fn write_composition_time(buf: &mut BytesMut, cts: i32) {
    let encoded = (cts as u32) & 0x00ff_ffff;
    buf.put_u8((encoded >> 16) as u8);
    buf.put_u8((encoded >> 8) as u8);
    buf.put_u8(encoded as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_header_layout() {
        let mut buf = BytesMut::new();
        write_tag_header(&mut buf, TAG_TYPE_VIDEO, 0x010203, 0x0123_4567);
        assert_eq!(
            buf.as_ref(),
            &[0x09, 0x01, 0x02, 0x03, 0x23, 0x45, 0x67, 0x01, 0, 0, 0]
        );
        // Small timestamps keep the extension byte at zero.
        let mut buf = BytesMut::new();
        write_tag_header(&mut buf, TAG_TYPE_AUDIO, 5, 20);
        assert_eq!(buf.as_ref(), &[0x08, 0, 0, 5, 0, 0, 20, 0, 0, 0, 0]);
    }

    #[test]
    fn composition_time_clamps_to_signed_24bit() {
        assert_eq!(
            composition_time_ms(Duration::from_millis(40), Some(Duration::from_millis(0))),
            40
        );
        assert_eq!(
            composition_time_ms(Duration::ZERO, Some(Duration::from_millis(40))),
            -40
        );
        assert_eq!(composition_time_ms(Duration::ZERO, None), 0);
        // > 24.8 days of B-frame offset clamps (FLV signed 24-bit limit).
        assert_eq!(
            composition_time_ms(Duration::from_secs(100 * 24 * 3600), Some(Duration::ZERO)),
            0x7f_ffff
        );
        assert_eq!(
            composition_time_ms(Duration::ZERO, Some(Duration::from_secs(100 * 24 * 3600))),
            -0x80_0000
        );
    }

    #[test]
    fn composition_time_encoding_matches_mask() {
        let mut buf = BytesMut::new();
        write_composition_time(&mut buf, -1);
        assert_eq!(buf.as_ref(), &[0xff, 0xff, 0xff]);
        let mut buf = BytesMut::new();
        write_composition_time(&mut buf, -0x80_0000);
        assert_eq!(buf.as_ref(), &[0x80, 0x00, 0x00]);
        let mut buf = BytesMut::new();
        write_composition_time(&mut buf, 0x7f_ffff);
        assert_eq!(buf.as_ref(), &[0x7f, 0xff, 0xff]);
    }

    #[test]
    fn aac_sound_header_has_nonzero_rate_bits() {
        // 0xAF: AAC (10) << 4 | 44kHz(3) 16-bit(1) stereo(1).
        assert_eq!(AAC_SOUND_HEADER, 0xAF);
    }
}
