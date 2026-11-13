use super::Depacketizer;
use super::MAX_ASSEMBLY_BYTES;
use crate::{Error, Result};
use bytes::{Bytes, BytesMut};

impl Depacketizer {
    pub(super) fn h264(&mut self, p: Bytes) -> Result<Option<(Bytes, bool)>> {
        if p.is_empty() {
            return Err(Error::Media("empty H264 RTP payload".into()));
        }
        let typ = p[0] & 0x1f;
        if typ == 24 {
            let mut offset = 1;
            let mut output = BytesMut::new();
            let mut keyframe = false;
            while offset + 2 <= p.len() {
                let size = u16::from_be_bytes([p[offset], p[offset + 1]]) as usize;
                offset += 2;
                if size == 0 || offset + size > p.len() {
                    return Err(Error::Media("invalid H264 STAP-A".into()));
                }
                keyframe |= size > 0 && (p[offset] & 0x1F) == 5;
                output.extend_from_slice(&[0, 0, 0, 1]);
                output.extend_from_slice(&p[offset..offset + size]);
                offset += size;
            }
            if offset != p.len() {
                return Err(Error::Media("trailing H264 STAP-A bytes".into()));
            }
            return Ok(Some((output.freeze(), keyframe)));
        }
        if typ != 28 {
            // Single NAL: its header byte is the classification.
            let keyframe = (p[0] & 0x1F) == 5;
            return Ok(Some((p, keyframe)));
        }
        if p.len() < 2 {
            return Err(Error::Media("short H264 FU-A".into()));
        }
        let start = p[1] & 0x80 != 0;
        let end = p[1] & 0x40 != 0;
        if start {
            let mut b = BytesMut::with_capacity(p.len());
            b.extend_from_slice(&[(p[0] & 0xe0) | (p[1] & 0x1f)]);
            b.extend_from_slice(&p[2..]);
            let keyframe = (p[1] & 0x1f) == 5;
            if end {
                return Ok(Some((b.freeze(), keyframe)));
            }
            // Latch the keyframe flag: FU continuation packets do not
            // carry the NAL type, so the flag must survive until the
            // FU end packet completes the NAL.
            self.assembling_is_keyframe = keyframe;
            self.assembling = Some(b);
            return Ok(None);
        }
        let b = self
            .assembling
            .as_mut()
            .ok_or_else(|| Error::Media("H264 FU continuation without start".into()))?;
        if b.len().saturating_add(p.len() - 2) > MAX_ASSEMBLY_BYTES {
            self.assembling = None;
            self.assembling_is_keyframe = false;
            return Err(Error::Media(format!(
                "H264 FU assembly exceeds {MAX_ASSEMBLY_BYTES} bytes"
            )));
        }
        b.extend_from_slice(&p[2..]);
        if end {
            let keyframe = std::mem::take(&mut self.assembling_is_keyframe);
            Ok(self.assembling.take().map(|b| (b.freeze(), keyframe)))
        } else {
            Ok(None)
        }
    }
}
