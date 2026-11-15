use super::Depacketizer;
use crate::RtpPacket;
use rtflow_core::error::{Error, Result};
use rtflow_media::{AccessUnit, Codec};

impl Depacketizer {
    pub(super) fn audio(&mut self, p: RtpPacket) -> Result<Vec<AccessUnit>> {
        let timestamp = p.header.timestamp;
        match self.codec {
            Codec::Aac => {
                if p.payload.len() < 4 {
                    return Err(Error::Media("short AAC AU header".into()));
                }
                let header_bits = u16::from_be_bytes([p.payload[0], p.payload[1]]) as usize;
                if !header_bits.is_multiple_of(16) || header_bits == 0 {
                    return Err(Error::Media("invalid AAC AU-headers-length".into()));
                }
                let au_count = header_bits / 16;
                let header_end = 2 + au_count * 2;
                if p.payload.len() < header_end {
                    return Err(Error::Media("AAC AU headers exceed payload".into()));
                }
                let mut units = Vec::with_capacity(au_count);
                let mut data_offset = header_end;
                for i in 0..au_count {
                    let ah = u16::from_be_bytes([p.payload[2 + i * 2], p.payload[2 + i * 2 + 1]]);
                    let size = (ah >> 3) as usize;
                    if data_offset + size > p.payload.len() {
                        return Err(Error::Media("AAC AU-size exceeds payload".into()));
                    }
                    let pts = self.pts_from_timestamp(timestamp);
                    units.push(AccessUnit {
                        track_id: self.track_id,
                        codec: self.codec,
                        pts,
                        dts: None,
                        rtp_timestamp: timestamp,
                        is_keyframe: true,
                        data: p.payload.slice(data_offset..data_offset + size),
                    });
                    data_offset += size;
                }
                Ok(units)
            }
            Codec::G726_16 | Codec::G726_24 | Codec::G726_32 | Codec::G726_40 => {
                let frame_bytes = match self.codec {
                    Codec::G726_16 => 40,
                    Codec::G726_24 => 60,
                    Codec::G726_32 => 80,
                    Codec::G726_40 => 100,
                    _ => unreachable!(),
                };
                if p.payload.is_empty() || !p.payload.len().is_multiple_of(frame_bytes) {
                    return Err(Error::Media("invalid G.726 payload length".into()));
                }
                let pts = self.pts_from_timestamp(timestamp);
                Ok(vec![AccessUnit {
                    track_id: self.track_id,
                    codec: self.codec,
                    pts,
                    dts: None,
                    rtp_timestamp: timestamp,
                    is_keyframe: true,
                    data: p.payload,
                }])
            }
            Codec::Mp3 => {
                if p.payload.len() < 4 {
                    return Err(Error::Media("short MPEG audio payload".into()));
                }
                let pts = self.pts_from_timestamp(timestamp);
                Ok(vec![AccessUnit {
                    track_id: self.track_id,
                    codec: self.codec,
                    pts,
                    dts: None,
                    rtp_timestamp: timestamp,
                    is_keyframe: true,
                    data: p.payload.slice(4..),
                }])
            }
            Codec::Opus | Codec::Pcma | Codec::Pcmu => {
                let pts = self.pts_from_timestamp(timestamp);
                Ok(vec![AccessUnit {
                    track_id: self.track_id,
                    codec: self.codec,
                    pts,
                    dts: None,
                    rtp_timestamp: timestamp,
                    is_keyframe: true,
                    data: p.payload,
                }])
            }
            // Never panic on an unhandled codec: a future SDP/feature
            // extension must fail this session cleanly instead of killing
            // the tokio task (and with it the whole stream).
            other => Err(Error::Media(format!(
                "depacketization not implemented for {other:?}"
            ))),
        }
    }
}
