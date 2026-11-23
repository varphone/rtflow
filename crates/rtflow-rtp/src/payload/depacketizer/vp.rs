use super::Depacketizer;
use super::MAX_ASSEMBLY_BYTES;
use crate::RtpPacket;
use bytes::BytesMut;
use rtflow_core::error::{Error, Result};
use rtflow_media::{AccessUnit, Codec};

impl Depacketizer {
    /// VP8/VP9 depacketization: strips the payload descriptor and
    /// reassembles fragments (same timestamp, B/S bit starts a frame,
    /// E bit or marker ends it). Returns the frame directly — VP8/VP9
    /// frames are single access units, they never aggregate.
    ///
    /// - VP8 (RFC 7741): 1-byte descriptor `X R N S PID`; keyframe when
    ///   the VP8 frame-header P bit (byte 0 of the frame) is 0.
    /// - VP9 (draft-ietf-payload-vp9): octet 0 `I P L F B E V Z`, with
    ///   I=1 followed by a 16-bit picture ID; keyframe when the VP9
    ///   uncompressed-header frame_type bit (byte 0, bit 4) is 0.
    pub(super) fn vp9_depacketize(&mut self, p: &RtpPacket) -> Result<Vec<AccessUnit>> {
        let payload = &p.payload;
        if payload.is_empty() {
            return Err(Error::Media("empty VP8/VP9 packet".into()));
        }
        struct VpFragment<'a> {
            start_bit: bool,
            end_bit: bool,
            data: &'a [u8],
            keyframe: fn(&[u8]) -> bool,
        }
        let VpFragment {
            start_bit,
            end_bit,
            data,
            keyframe: keyframe_fn,
        } = if self.codec == Codec::Vp8 {
            let first = payload[0];
            VpFragment {
                start_bit: first & 0x10 != 0,
                end_bit: false,
                data: &payload[1..],
                keyframe: |data| data.first().is_some_and(|byte| byte & 0x01 == 0),
            }
        } else {
            let first = payload[0];
            // I=1: picture ID follows. The M bit (octet 1, bit 7)
            // selects a 7-bit (2-byte descriptor, ffmpeg style) or
            // 15-bit (3-byte) picture ID.
            let offset = if first & 0x80 != 0 {
                if payload.len() < 2 {
                    return Err(Error::Media("short VP9 payload descriptor".into()));
                }
                if payload[1] & 0x80 != 0 { 3 } else { 2 }
            } else {
                1
            };
            if payload.len() < offset {
                return Err(Error::Media("short VP9 payload descriptor".into()));
            }
            VpFragment {
                start_bit: first & 0x08 != 0,
                end_bit: first & 0x04 != 0,
                data: &payload[offset..],
                // frame_type bit of the VP9 uncompressed header:
                // bit 4 of the first byte, 0 = keyframe.
                keyframe: |data| data.first().is_some_and(|byte| byte & 0x10 == 0),
            }
        };
        if start_bit {
            // A new frame begins: drop any truncated previous frame.
            if let Some(prev) = self.vp_assembling.take() {
                tracing::debug!(bytes = prev.len(), "VP frame dropped (truncated)");
            }
            if data.len() > MAX_ASSEMBLY_BYTES {
                return Err(Error::Media(format!(
                    "VP frame exceeds {MAX_ASSEMBLY_BYTES} bytes"
                )));
            }
            self.vp_is_keyframe = keyframe_fn(data);
            self.vp_assembling = Some(BytesMut::from(data));
        } else if let Some(buffer) = &mut self.vp_assembling {
            if buffer.len().saturating_add(data.len()) > MAX_ASSEMBLY_BYTES {
                self.vp_assembling = None;
                self.vp_is_keyframe = false;
                return Err(Error::Media(format!(
                    "VP frame exceeds {MAX_ASSEMBLY_BYTES} bytes"
                )));
            }
            buffer.extend_from_slice(data);
        } else {
            // Continuation without a start (first fragment lost): drop
            // until the next start fragment.
            return Ok(vec![]);
        }
        let frame_complete = if self.codec == Codec::Vp8 {
            p.header.marker
        } else {
            // VP9: E bit or RTP marker ends the frame.
            p.header.marker || end_bit
        };
        if frame_complete && let Some(frame) = self.vp_assembling.take() {
            let is_keyframe = std::mem::take(&mut self.vp_is_keyframe);
            return Ok(vec![AccessUnit {
                nal_offsets: None,
                track_id: self.track_id,
                codec: self.codec,
                pts: self.pts_from_timestamp(p.header.timestamp),
                dts: None,
                rtp_timestamp: p.header.timestamp,
                is_keyframe,
                data: frame.freeze(),
            }]);
        }
        Ok(vec![])
    }
}
