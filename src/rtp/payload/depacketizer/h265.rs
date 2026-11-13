use super::Depacketizer;
use super::MAX_ASSEMBLY_BYTES;
use crate::{Error, Result};
use bytes::{Bytes, BytesMut};

impl Depacketizer {
    pub(super) fn h265(&mut self, p: Bytes) -> Result<Option<(Bytes, bool)>> {
        if p.len() < 2 {
            return Err(Error::Media("short H265 RTP payload".into()));
        }
        let typ = (p[0] >> 1) & 0x3f;
        if typ == 48 {
            let mut offset = 2;
            let mut output = BytesMut::new();
            let mut keyframe = false;
            while offset + 2 <= p.len() {
                let size = u16::from_be_bytes([p[offset], p[offset + 1]]) as usize;
                offset += 2;
                if size < 2 || offset + size > p.len() {
                    return Err(Error::Media("invalid H265 AP".into()));
                }
                keyframe |= size >= 2
                    && crate::media::bitstream::is_h265_random_access(&p[offset..offset + size]);
                output.extend_from_slice(&[0, 0, 0, 1]);
                output.extend_from_slice(&p[offset..offset + size]);
                offset += size;
            }
            if offset != p.len() {
                return Err(Error::Media("trailing H265 AP bytes".into()));
            }
            return Ok(Some((output.freeze(), keyframe)));
        }
        if typ != 49 {
            let keyframe = crate::media::bitstream::is_h265_random_access(&p[..2.min(p.len())]);
            return Ok(Some((p, keyframe)));
        }
        if p.len() < 3 {
            return Err(Error::Media("short H265 FU".into()));
        }
        let start = p[2] & 0x80 != 0;
        let end = p[2] & 0x40 != 0;
        if start {
            let mut b = BytesMut::with_capacity(p.len());
            b.extend_from_slice(&[(p[0] & 0x81) | ((p[2] & 0x3f) << 1), p[1]]);
            b.extend_from_slice(&p[3..]);
            let keyframe = matches!(p[2] & 0x3f, 16..=21);
            if end {
                return Ok(Some((b.freeze(), keyframe)));
            }
            // Latch the keyframe flag (see H.264 FU-A handling).
            self.assembling_is_keyframe = keyframe;
            self.assembling = Some(b);
            return Ok(None);
        }
        let b = self
            .assembling
            .as_mut()
            .ok_or_else(|| Error::Media("H265 FU continuation without start".into()))?;
        if b.len().saturating_add(p.len() - 3) > MAX_ASSEMBLY_BYTES {
            self.assembling = None;
            self.assembling_is_keyframe = false;
            return Err(Error::Media(format!(
                "H265 FU assembly exceeds {MAX_ASSEMBLY_BYTES} bytes"
            )));
        }
        b.extend_from_slice(&p[3..]);
        if end {
            let keyframe = std::mem::take(&mut self.assembling_is_keyframe);
            Ok(self.assembling.take().map(|b| (b.freeze(), keyframe)))
        } else {
            Ok(None)
        }
    }
}
