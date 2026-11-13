//! FLV muxing: file header, metadata tag and per-codec video/audio tags.
mod avcc;
mod h264;
mod h265;
mod tag;
#[cfg(test)]
mod tests;

use crate::media::{AccessUnit, Codec, MediaDescription};
use base64::Engine;
use bytes::{BufMut, Bytes, BytesMut};
use std::time::Duration;

use self::tag::FLV_HEADER_SIZE;
use self::{
    h264::{write_h264_video_tag, write_h264_video_tag_inplace},
    h265::{write_h265_video_tag, write_h265_video_tag_inplace},
    tag::{write_audio_tag, write_g711_audio_tag},
};

type H265Sprop = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

pub struct FlvMuxer {
    has_video: bool,
    has_audio: bool,
    video_sequence_header_sent: bool,
    audio_sequence_header_sent: bool,
    tag_buffer: BytesMut,
    /// Cached video decoder configuration record (sequence header).
    video_sequence_header: Option<Bytes>,
    /// Cached AAC audio specific config (sequence header).
    aac_sequence_header: Option<Bytes>,
    /// PTS of the first emitted access unit, used to normalize the shared FLV timeline.
    first_pts: Option<Duration>,
    /// Next decode timestamp for video. RTP delivers B-frames in decode order.
    next_video_dts: Option<Duration>,
    // Presentation timestamps are reordered by B-frames; their smallest observed
    // non-zero delta is the encoded frame duration.
    last_video_pts: Option<Duration>,
    video_frame_duration: Option<Duration>,
}

impl FlvMuxer {
    pub fn new(description: &MediaDescription) -> Self {
        let video_track = description
            .tracks
            .iter()
            .find(|t| t.kind == crate::media::MediaKind::Video);
        let has_video = video_track.is_some();
        let has_audio = description
            .tracks
            .iter()
            .any(|t| t.kind == crate::media::MediaKind::Audio);
        Self {
            has_video,
            has_audio,
            video_sequence_header_sent: false,
            audio_sequence_header_sent: false,
            tag_buffer: BytesMut::with_capacity(4096),
            video_sequence_header: None,
            aac_sequence_header: None,
            first_pts: None,
            next_video_dts: None,
            last_video_pts: None,
            video_frame_duration: None,
        }
    }

    /// Write FLV file header (9 bytes + 4 bytes previous tag size 0).
    pub fn flv_header(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(FLV_HEADER_SIZE + 4);
        buf.put_slice(b"FLV"); // signature
        buf.put_u8(1); // version
        let flags = (if self.has_video { 1u8 } else { 0 }) | (if self.has_audio { 4u8 } else { 0 });
        buf.put_u8(flags); // type flags
        buf.put_u32(FLV_HEADER_SIZE as u32); // data offset
        buf.put_u32(0); // previous tag size 0
        buf.freeze()
    }

    /// onMetaData script tag (width/height/framerate/duration). Some legacy
    /// Flash-era and Android players refuse to start without it. AMF0 is
    /// encoded inline so http-flv does not depend on the rtmp feature.
    pub fn metadata_tag(&self) -> Option<Bytes> {
        let mut payload = BytesMut::with_capacity(128);
        // "onMetaData" string
        payload.put_u8(0x02);
        payload.put_u16(10);
        payload.put_slice(b"onMetaData");
        // ECMA array
        let mut props: Vec<(&str, f64)> = vec![("server", 0.0), ("duration", 0.0)];
        let mut bool_props: Vec<(&str, bool)> = Vec::new();
        if self.has_video {
            props.push(("videocodecid", 7.0));
            props.push(("framerate", 30.0));
            props.push(("width", 0.0));
            props.push(("height", 0.0));
        }
        if self.has_audio {
            props.push(("audiocodecid", 10.0));
            props.push(("audiosamplerate", 44100.0));
            props.push(("audiosamplesize", 16.0));
            bool_props.push(("stereo", true));
        }
        let count = (props.len() + bool_props.len()) as u32;
        payload.put_u8(0x08);
        payload.put_u32(count);
        for (key, value) in props {
            payload.put_u16(key.len() as u16);
            payload.put_slice(key.as_bytes());
            payload.put_u8(0x00); // number
            payload.put_f64(value);
        }
        for (key, value) in bool_props {
            payload.put_u16(key.len() as u16);
            payload.put_slice(key.as_bytes());
            payload.put_u8(0x01); // boolean
            payload.put_u8(value as u8);
        }
        payload.put_u32(0x0000_0009); // ECMA array end marker
        let mut buf = BytesMut::with_capacity(payload.len() + 16);
        buf.put_u8(0x12); // AMF0 script tag
        let len = payload.len() as u32;
        buf.put_slice(&len.to_be_bytes()[1..]); // 24-bit
        buf.put_slice(&0u32.to_be_bytes()[1..]); // timestamp (3 bytes)
        buf.put_u8(0); // timestamp extension
        // FLV tag header is 11 bytes: type(1) + size(3) + ts(3) + ext(1)
        // + stream id(3). One extra byte here shifts every following tag.
        buf.put_slice(&0u32.to_be_bytes()[1..]); // stream id (3 bytes)
        buf.put_slice(&payload);
        buf.put_u32((payload.len() + 11) as u32); // previous tag size
        Some(buf.freeze())
    }

    /// Set the video sequence header (AVCDecoderConfigurationRecord or HEVCDecoderConfigurationRecord).
    pub fn set_video_sequence_header(&mut self, data: Bytes) {
        self.video_sequence_header = Some(data);
    }

    /// Set the AAC sequence header (AudioSpecificConfig).
    pub fn set_aac_sequence_header(&mut self, data: Bytes) {
        self.aac_sequence_header = Some(data);
    }

    /// Reset timestamp and codec-header emission state after a subscriber gap.
    pub fn reset_after_discontinuity(&mut self) {
        self.video_sequence_header_sent = false;
        self.audio_sequence_header_sent = false;
        self.first_pts = None;
        self.next_video_dts = None;
        self.last_video_pts = None;
        self.video_frame_duration = None;
    }

    /// Encode an AccessUnit into FLV tags.
    /// Returns one or more FLV tags (may include sequence headers).
    pub fn encode(&mut self, unit: &AccessUnit) -> Bytes {
        self.tag_buffer.clear();

        // Normalize the shared FLV timeline to start at 0.  Audio and video
        // must use the same base or a cached video keyframe can move DTS backward.
        let base = *self.first_pts.get_or_insert(unit.pts);
        let normalized_pts = unit.pts.saturating_sub(base);
        if unit.codec == Codec::Aac && tracing::enabled!(tracing::Level::DEBUG) {
            tracing::debug!(
                "mux: aac pts={:?} rtp_ts={} base={:?} norm={:?}",
                unit.pts,
                unit.rtp_timestamp,
                base,
                normalized_pts
            );
        }
        let mut normalized = unit.clone();
        normalized.pts = normalized_pts;
        if matches!(normalized.codec, Codec::H264 | Codec::H265) {
            // RTP access units arrive in decode order while their PTS can be
            // reordered by B-frames.  FLV timestamps carry DTS; retain PTS in
            // the signed CompositionTime field written by the video tag.
            let frame_duration = self.observe_video_frame_duration(normalized.pts);
            let dts = self.next_video_dts.unwrap_or(normalized.pts);
            self.next_video_dts = Some(dts.saturating_add(frame_duration));
            normalized.dts = Some(dts);
        }

        match normalized.codec {
            Codec::H264 => {
                if !self.video_sequence_header_sent
                    && let Some(ref seq) = self.video_sequence_header
                {
                    write_h264_video_tag(&mut self.tag_buffer, &normalized, seq, true);
                    self.video_sequence_header_sent = true;
                }
                write_h264_video_tag_inplace(&mut self.tag_buffer, &normalized, &normalized.data);
            }
            Codec::H265 => {
                if !self.video_sequence_header_sent
                    && let Some(ref seq) = self.video_sequence_header
                {
                    write_h265_video_tag(&mut self.tag_buffer, &normalized, seq, true);
                    self.video_sequence_header_sent = true;
                }
                write_h265_video_tag_inplace(&mut self.tag_buffer, &normalized, &normalized.data);
            }
            Codec::Aac => {
                if !self.audio_sequence_header_sent
                    && let Some(ref seq) = self.aac_sequence_header
                {
                    write_audio_tag(&mut self.tag_buffer, &normalized, seq, true);
                    self.audio_sequence_header_sent = true;
                }
                write_audio_tag(&mut self.tag_buffer, &normalized, &normalized.data, false);
            }
            Codec::Pcma | Codec::Pcmu => {
                write_g711_audio_tag(&mut self.tag_buffer, &normalized);
            }
            Codec::Opus
            | Codec::Vp8
            | Codec::Vp9
            | Codec::Mp3
            | Codec::G726_16
            | Codec::G726_24
            | Codec::G726_32
            | Codec::G726_40
            | Codec::Av1
            | Codec::Jpeg
            | Codec::H263
            | Codec::Mpeg4 => {
                // FLV does not support these codecs; skip silently.
            }
        }

        self.tag_buffer.split_to(self.tag_buffer.len()).freeze()
    }

    fn observe_video_frame_duration(&mut self, pts: Duration) -> Duration {
        if let Some(previous) = self.last_video_pts {
            let delta = pts.abs_diff(previous);
            if !delta.is_zero() && delta <= Duration::from_secs(1) {
                self.video_frame_duration = Some(
                    self.video_frame_duration
                        .map_or(delta, |current| current.min(delta)),
                );
            }
        }
        self.last_video_pts = Some(pts);
        self.video_frame_duration
            .unwrap_or(Duration::from_millis(40))
    }

    /// Build an AVCDecoderConfigurationRecord from Annex-B H.264 data
    /// (SPS + PPS extracted via [`crate::media::bitstream::h264_parameter_sets`]).
    pub fn build_avc_sequence_header(annex_b_data: &Bytes) -> Option<Bytes> {
        let sets = crate::media::bitstream::h264_parameter_sets(annex_b_data);
        crate::media::bitstream::build_avc_config_record(sets.sps.as_deref()?, sets.pps.as_deref()?)
    }

    /// Build a HEVCDecoderConfigurationRecord from Annex-B H.265 data
    /// (VPS/SPS/PPS extracted via [`crate::media::bitstream::h265_parameter_sets`]).
    pub fn build_h265_sequence_header(annex_b_data: &Bytes) -> Option<Bytes> {
        let sets = crate::media::bitstream::h265_parameter_sets(annex_b_data);
        crate::media::bitstream::build_hevc_config_record(
            sets.vps.as_deref()?,
            sets.sps.as_deref()?,
            sets.pps.as_deref()?,
        )
    }

    /// Build an AVCDecoderConfigurationRecord from pre-extracted SPS/PPS NALs.
    pub fn build_avc_sequence_header_from_nals(sps: &[u8], pps: &[u8]) -> Option<Bytes> {
        crate::media::bitstream::build_avc_config_record(sps, pps)
    }

    /// Build a HEVCDecoderConfigurationRecord from pre-extracted VPS/SPS/PPS NALs.
    pub fn build_h265_sequence_header_from_nals(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
    ) -> Option<Bytes> {
        crate::media::bitstream::build_hevc_config_record(vps, sps, pps)
    }

    /// Parse AAC AudioSpecificConfig from an fmtp string.
    pub fn parse_aac_config_from_fmtp(fmtp: &str) -> Option<Bytes> {
        let config_hex = fmtp
            .split(';')
            .find(|kv| kv.trim().starts_with("config="))
            .and_then(|kv| kv.split_once('=').map(|(_, value)| value))?
            .trim();
        let bytes: Option<Vec<u8>> = (0..config_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&config_hex[i..i + 2], 16).ok())
            .collect();
        let bytes = bytes?;
        if bytes.is_empty() {
            return None;
        }
        Some(Bytes::from(bytes))
    }

    /// Parse H.265 sprop-vps/sprop-sps/sprop-pps from an fmtp string.
    pub fn parse_h265_sprop_from_fmtp(fmtp: &str) -> H265Sprop {
        let b64_decode = |key: &str| -> Option<Vec<u8>> {
            let val = fmtp
                .split(';')
                .find(|kv| kv.trim().starts_with(key))
                .and_then(|kv| kv.split_once('=').map(|(_, value)| value))?
                .trim();
            base64::engine::general_purpose::STANDARD.decode(val).ok()
        };
        (
            b64_decode("sprop-vps"),
            b64_decode("sprop-sps"),
            b64_decode("sprop-pps"),
        )
    }

    /// Parse H.264 sprop-parameter-sets from an fmtp string.
    pub fn parse_h264_sprop_from_fmtp(fmtp: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let val = match fmtp
            .split(';')
            .find(|kv| kv.trim().starts_with("sprop-parameter-sets"))
            .and_then(|kv| kv.split_once('=').map(|(_, value)| value))
        {
            Some(v) => v.trim(),
            None => return (None, None),
        };
        let parts: Vec<&str> = val.split(',').collect();
        if parts.len() < 2 {
            return (None, None);
        }
        let decode = |b64: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .ok()
        };
        (decode(parts[0]), decode(parts[1]))
    }
}
