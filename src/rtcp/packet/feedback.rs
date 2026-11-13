//! RFC 4585 feedback, RFC 3611 extended reports and their constructors.

use super::{Packet, read_u32};
use crate::{Error, Result};
use bytes::{Bytes, BytesMut};

/// One RFC 3611 XR block; `data` excludes the 4-byte block header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XrBlock {
    pub block_type: u8,
    pub data: Bytes,
}

/// One RFC 4585 NACK entry: a lost packet id plus a 16-bit bitmask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NackEntry {
    pub packet_id: u16,
    pub lost: u16,
}

impl XrBlock {
    /// RFC 3611 Receiver Reference Time (block type 4).
    pub fn rttr(ntp: u64) -> Self {
        Self {
            block_type: 4,
            data: Bytes::copy_from_slice(&ntp.to_be_bytes()),
        }
    }

    /// RFC 3611 DLRR (block type 5): (last RR, delay since last RR) pairs.
    pub fn dlrr(entries: &[(u32, u32)]) -> Self {
        let mut data = BytesMut::with_capacity(entries.len() * 12);
        for (last_rr, delay) in entries {
            data.extend_from_slice(&last_rr.to_be_bytes());
            data.extend_from_slice(&delay.to_be_bytes());
            data.extend_from_slice(&[0, 0, 0, 0]);
        }
        Self {
            block_type: 5,
            data: data.freeze(),
        }
    }

    pub fn rttr_timestamp(&self) -> Option<u64> {
        (self.block_type == 4 && self.data.len() == 8).then(|| {
            u64::from_be_bytes([
                self.data[0],
                self.data[1],
                self.data[2],
                self.data[3],
                self.data[4],
                self.data[5],
                self.data[6],
                self.data[7],
            ])
        })
    }
}

impl Packet {
    /// Build an RFC 4585 NACK (RTPFB FMT 1).
    pub fn nack(sender_ssrc: u32, media_ssrc: u32, entries: &[NackEntry]) -> Result<Self> {
        let mut fci = BytesMut::with_capacity(entries.len() * 4);
        for entry in entries {
            fci.extend_from_slice(&entry.packet_id.to_be_bytes());
            fci.extend_from_slice(&entry.lost.to_be_bytes());
        }
        Ok(Self::RtpFeedback {
            fmt: 1,
            sender_ssrc,
            media_ssrc,
            fci: fci.freeze(),
        })
    }

    /// Build a Picture Loss Indication (PSFB FMT 1).
    pub fn pli(sender_ssrc: u32, media_ssrc: u32) -> Self {
        Self::PayloadFeedback {
            fmt: 1,
            sender_ssrc,
            media_ssrc,
            fci: Bytes::new(),
        }
    }

    /// Build a Full Intra Request (PSFB FMT 4) for `target_ssrc`.
    pub fn fir(sender_ssrc: u32, media_ssrc: u32, target_ssrc: u32, sequence: u8) -> Self {
        let mut fci = BytesMut::with_capacity(8);
        fci.extend_from_slice(&target_ssrc.to_be_bytes());
        fci.extend_from_slice(&[sequence, 0, 0, 0]);
        Self::PayloadFeedback {
            fmt: 4,
            sender_ssrc,
            media_ssrc,
            fci: fci.freeze(),
        }
    }

    /// Build a Receiver Estimated Maximum Bitrate (PSFB FMT 15 / AFB).
    pub fn remb(sender_ssrc: u32, media_ssrc: u32, bitrate: u64, ssrcs: &[u32]) -> Result<Self> {
        if ssrcs.len() > 255 {
            return Err(Error::Limit("RTCP REMB SSRC list"));
        }
        let mut exponent = 0u8;
        let mut mantissa = bitrate;
        while mantissa > 0x3ffff && exponent < 63 {
            mantissa >>= 1;
            exponent += 1;
        }
        let mut fci = BytesMut::with_capacity(4 + ssrcs.len() * 4);
        fci.extend_from_slice(&[ssrcs.len() as u8]);
        fci.extend_from_slice(&[exponent << 2 | ((mantissa >> 16) & 0x03) as u8]);
        fci.extend_from_slice(&[(mantissa >> 8) as u8, mantissa as u8]);
        for ssrc in ssrcs {
            fci.extend_from_slice(&ssrc.to_be_bytes());
        }
        Ok(Self::PayloadFeedback {
            fmt: 15,
            sender_ssrc,
            media_ssrc,
            fci: fci.freeze(),
        })
    }

    /// Decode NACK entries from an RTPFB FMT 1 packet.
    pub fn nack_entries(&self) -> Option<Vec<NackEntry>> {
        let Self::RtpFeedback { fmt: 1, fci, .. } = self else {
            return None;
        };
        if !fci.len().is_multiple_of(4) {
            return None;
        }
        Some(
            fci.as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| NackEntry {
                    packet_id: u16::from_be_bytes([chunk[0], chunk[1]]),
                    lost: u16::from_be_bytes([chunk[2], chunk[3]]),
                })
                .collect(),
        )
    }

    /// Decode the bitrate from a REMB (PSFB FMT 15) packet.
    pub fn remb_bitrate(&self) -> Option<u64> {
        let Self::PayloadFeedback { fmt: 15, fci, .. } = self else {
            return None;
        };
        if fci.len() < 4 {
            return None;
        }
        let exponent = fci[1] >> 2;
        let mantissa =
            (u64::from(fci[1] & 0x03) << 16) | (u64::from(fci[2]) << 8) | u64::from(fci[3]);
        Some(mantissa << exponent)
    }
}

pub(super) fn feedback_body(sender_ssrc: u32, media_ssrc: u32, fci: &[u8]) -> Result<BytesMut> {
    if !fci.len().is_multiple_of(4) {
        return Err(Error::Media(
            "RTCP feedback FCI is not 32-bit aligned".into(),
        ));
    }
    let mut body = BytesMut::with_capacity(8 + fci.len());
    body.extend_from_slice(&sender_ssrc.to_be_bytes());
    body.extend_from_slice(&media_ssrc.to_be_bytes());
    body.extend_from_slice(fci);
    Ok(body)
}

pub(super) fn parse_feedback(body: Bytes, count: u8, rtp: bool) -> Result<Packet> {
    if body.len() < 8 || !(body.len() - 8).is_multiple_of(4) {
        return Err(Error::Media("invalid RTCP feedback length".into()));
    }
    let sender_ssrc = read_u32(&body, 0)?;
    let media_ssrc = read_u32(&body, 4)?;
    let fci = body.slice(8..);
    if rtp {
        Ok(Packet::RtpFeedback {
            fmt: count,
            sender_ssrc,
            media_ssrc,
            fci,
        })
    } else {
        Ok(Packet::PayloadFeedback {
            fmt: count,
            sender_ssrc,
            media_ssrc,
            fci,
        })
    }
}

pub(super) fn parse_extended_report(body: Bytes) -> Result<Packet> {
    let ssrc = read_u32(&body, 0)?;
    let mut position = 4usize;
    let mut blocks = Vec::new();
    while position < body.len() {
        if position + 4 > body.len() {
            return Err(Error::Media("truncated RTCP XR block header".into()));
        }
        let block_type = body[position];
        let words = u16::from_be_bytes([body[position + 2], body[position + 3]]);
        let length = (usize::from(words) + 1)
            .checked_mul(4)
            .ok_or(Error::Limit("RTCP XR block"))?;
        let end = position
            .checked_add(length)
            .ok_or(Error::Limit("RTCP XR block"))?;
        if end > body.len() {
            return Err(Error::Media("truncated RTCP XR block".into()));
        }
        blocks.push(XrBlock {
            block_type,
            data: body.slice(position + 4..end),
        });
        position = end;
    }
    Ok(Packet::ExtendedReport { ssrc, blocks })
}
