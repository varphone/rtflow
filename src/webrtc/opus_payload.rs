//! Opus RTP payload formatting (RFC 7587).
//!
//! Opus has a simple RTP payload format: each RTP packet contains
//! exactly one Opus packet. No fragmentation or aggregation is needed.
use bytes::Bytes;
use std::time::Duration;

use crate::media::AccessUnit;

/// Opus clock rate (always 48000 Hz per RFC 7587).
pub const OPUS_CLOCK_RATE: u32 = 48000;

/// Default Opus payload type.
pub const OPUS_PAYLOAD_TYPE: u8 = 111;

/// Opus frame duration (typically 20ms).
pub const OPUS_FRAME_DURATION: Duration = Duration::from_millis(20);

/// Samples per Opus frame at 48kHz with 20ms frames.
pub const OPUS_SAMPLES_PER_FRAME: u32 = 960;

/// Packetize an Opus AccessUnit into RTP payload.
///
/// For Opus, this is trivial: the AccessUnit data IS the RTP payload.
/// Each AccessUnit should contain exactly one Opus packet.
pub fn packetize(unit: &AccessUnit) -> Bytes {
    unit.data.clone()
}

/// Depacketize RTP payload into Opus frame data.
///
/// For Opus, this is also trivial: the RTP payload IS the Opus packet.
pub fn depacketize(payload: &[u8]) -> Bytes {
    Bytes::copy_from_slice(payload)
}

/// Calculate RTP timestamp increment for one Opus frame.
pub fn timestamp_increment() -> u32 {
    OPUS_SAMPLES_PER_FRAME
}

/// Calculate RTP timestamp from duration.
pub fn duration_to_timestamp(d: Duration) -> u32 {
    (d.as_secs_f64() * OPUS_CLOCK_RATE as f64) as u32
}

/// Calculate duration from RTP timestamp.
pub fn timestamp_to_duration(ts: u32) -> Duration {
    Duration::from_secs_f64(ts as f64 / OPUS_CLOCK_RATE as f64)
}

/// Build an Opus AccessUnit from raw Opus packet data.
pub fn build_access_unit(
    data: Bytes,
    timestamp: u32,
    track_id: u8,
) -> AccessUnit {
    let pts = timestamp_to_duration(timestamp);
    AccessUnit {
        track_id,
        codec: crate::media::Codec::Opus,
        pts,
        dts: None,
        rtp_timestamp: timestamp,
        is_keyframe: true, // Opus frames are always independently decodable
        data,
    }
}

/// Opus SDP fmtp parameters (minimal).
pub fn sdp_fmtp() -> &'static str {
    "minptime=10;useinbandfec=1"
}

/// Opus RTP map line.
pub fn sdp_rtpmap(payload_type: u8) -> String {
    format!("a=rtpmap:{payload_type} opus/48000/2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_conversion() {
        // 20ms at 48kHz = 960 samples
        let d = Duration::from_millis(20);
        let ts = duration_to_timestamp(d);
        assert_eq!(ts, 960);

        let back = timestamp_to_duration(ts);
        assert_eq!(back.as_millis(), 20);
    }

    #[test]
    fn packetize_depacketize_roundtrip() {
        let data = Bytes::from(vec![0xFC, 0x01, 0x02, 0x03]);
        let unit = build_access_unit(data.clone(), 960, 0);

        let payload = packetize(&unit);
        assert_eq!(payload, data);

        let depacketized = depacketize(&payload);
        assert_eq!(depacketized, data);
    }

    #[test]
    fn access_unit_properties() {
        let data = Bytes::from(vec![0xFC, 0x00]);
        let unit = build_access_unit(data, 1920, 1);

        assert_eq!(unit.track_id, 1);
        assert_eq!(unit.codec, crate::media::Codec::Opus);
        assert_eq!(unit.rtp_timestamp, 1920);
        assert!(unit.is_keyframe);
        assert_eq!(unit.pts.as_millis(), 40); // 1920/48000 = 40ms
    }
}
