//! RTP fixed-header parsing with zero-copy payload slicing.
use rtflow_core::error::{Error, Result};
use bytes::Bytes;

#[derive(Clone, Debug)]
pub struct RtpHeader {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}
#[derive(Clone, Debug)]
pub struct RtpPacket {
    pub header: RtpHeader,
    pub payload: Bytes,
}
impl RtpPacket {
    pub fn parse(data: Bytes) -> Result<Self> {
        if data.len() < 12 {
            return Err(Error::Media("short RTP packet".into()));
        }
        let b0 = data[0];
        if b0 >> 6 != 2 {
            return Err(Error::Media("unsupported RTP version".into()));
        }
        let cc = (b0 & 15) as usize;
        let mut offset = 12 + cc * 4;
        if data.len() < offset {
            return Err(Error::Media("truncated CSRC list".into()));
        }
        if b0 & 0x10 != 0 {
            if data.len() < offset + 4 {
                return Err(Error::Media("truncated RTP extension".into()));
            }
            let n = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            offset += 4 + n * 4;
            if data.len() < offset {
                return Err(Error::Media("truncated RTP extension data".into()));
            }
        }
        let end = if b0 & 0x20 != 0 {
            let n = *data.last().unwrap() as usize;
            if n == 0 || n > data.len() - offset {
                return Err(Error::Media("invalid RTP padding".into()));
            }
            data.len() - n
        } else {
            data.len()
        };
        Ok(Self {
            header: RtpHeader {
                marker: data[1] & 0x80 != 0,
                payload_type: data[1] & 0x7f,
                sequence: u16::from_be_bytes([data[2], data[3]]),
                timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
                ssrc: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
            },
            payload: data.slice(offset..end),
        })
    }
    pub fn encode_header(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[0] = 0x80;
        b[1] = ((self.header.marker as u8) << 7) | self.header.payload_type;
        b[2..4].copy_from_slice(&self.header.sequence.to_be_bytes());
        b[4..8].copy_from_slice(&self.header.timestamp.to_be_bytes());
        b[8..12].copy_from_slice(&self.header.ssrc.to_be_bytes());
        b
    }
    /// Encode header+payload into an existing buffer, reusing its capacity.
    pub fn encode_into(&self, out: &mut bytes::BytesMut) {
        out.clear();
        out.reserve(12 + self.payload.len());
        out.extend_from_slice(&self.encode_header());
        out.extend_from_slice(&self.payload);
    }

    pub fn encode(&self) -> Bytes {
        let mut b = Vec::with_capacity(12 + self.payload.len());
        b.extend_from_slice(&self.encode_header());
        b.extend_from_slice(&self.payload);
        Bytes::from(b)
    }
}
