//! AV1 RTP payload (aomediacodec.github.io/av1-rtp-spec, v1.0).
//!
//! The aggregation header is `Z Y W W N - - -`; each OBU element carries the
//! OBU header with the size bit cleared and no LEB size field. The
//! depacketizer restores the size field and emits a complete temporal unit.

use bytes::Bytes;
use rtflow_core::error::{Error, Result};

pub(crate) struct Obu {
    pub header: Vec<u8>,
    pub payload: Bytes,
}

fn leb(data: &[u8]) -> Result<(u32, usize)> {
    let mut value = 0u32;
    for (index, byte) in data.iter().take(8).enumerate() {
        value |= u32::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
    }
    Err(Error::Media("invalid AV1 LEB128 size".into()))
}

fn put_leb(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Split an AV1 temporal unit into OBU elements; temporal delimiters and
/// padding OBUs are dropped as required by the payload format.
pub(crate) fn split_obus(data: &[u8]) -> Result<Vec<Obu>> {
    let mut offset = 0usize;
    let mut out = Vec::new();
    while offset < data.len() {
        let header = data[offset];
        offset += 1;
        if header & 0x80 != 0 {
            return Err(Error::Media("AV1 forbidden OBU bit".into()));
        }
        let obu_type = (header >> 3) & 0x0f;
        let extension = header & 0x04 != 0;
        let extension_byte = if extension {
            let byte = *data
                .get(offset)
                .ok_or_else(|| Error::Media("short AV1 OBU extension".into()))?;
            offset += 1;
            Some(byte)
        } else {
            None
        };
        let has_size = header & 0x02 != 0;
        let (size, used) = if has_size {
            let (size, used) = leb(&data[offset..])?;
            (size as usize, used)
        } else {
            (data.len() - offset, 0)
        };
        offset += used;
        if offset + size > data.len() {
            return Err(Error::Media("short AV1 OBU payload".into()));
        }
        let payload = data[offset..offset + size].to_vec();
        offset += size;
        if obu_type == 15 {
            continue;
        }
        let mut element_header = vec![header & !0x02];
        if let Some(byte) = extension_byte {
            element_header.push(byte);
        }
        out.push(Obu {
            header: element_header,
            payload: Bytes::from(payload),
        });
    }
    Ok(out)
}

fn element_bytes(obu: &Obu) -> Vec<u8> {
    let mut out = Vec::with_capacity(obu.header.len() + obu.payload.len());
    out.extend_from_slice(&obu.header);
    out.extend_from_slice(&obu.payload);
    out
}

/// Packetize one AV1 temporal unit into RTP payloads (without RTP headers).
/// One OBU element per packet, fragmented when it exceeds the MTU. `sequence`
/// is true for the first packet of the coded video sequence.
pub(crate) fn packetize(
    data: &[u8],
    max_payload: usize,
    keyframe: bool,
    sequence_sent: &mut bool,
) -> Result<Vec<(Bytes, bool)>> {
    if max_payload < 3 {
        return Err(Error::Media("MTU too small for AV1".into()));
    }
    let obus = split_obus(data)?;
    if obus.is_empty() {
        return Err(Error::Media("AV1 access unit has no OBU".into()));
    }
    let elements: Vec<Vec<u8>> = obus.iter().map(element_bytes).collect();
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < elements.len() {
        let mut group: Vec<Vec<u8>> = Vec::new();
        let mut used = 1usize;
        while index < elements.len() && group.len() < 3 {
            let element = &elements[index];
            let prefix = if group.is_empty() {
                0
            } else {
                leb_size(element.len())
            };
            if used + prefix + element.len() > max_payload {
                break;
            }
            used += prefix + element.len();
            group.push(element.clone());
            index += 1;
        }
        if group.is_empty() {
            // Fragment one OBU element across packets.
            let element = &elements[index];
            let chunk = max_payload - 1;
            let total = element.len().div_ceil(chunk).max(1);
            for (part_index, part) in element.chunks(chunk).enumerate() {
                let mut header = 0x10u8; // W = 1
                if part_index > 0 {
                    header |= 0x80; // Z
                }
                if part_index + 1 < total {
                    header |= 0x40; // Y
                }
                if out.is_empty() && !*sequence_sent && keyframe {
                    header |= 0x08; // N
                    *sequence_sent = true;
                }
                let mut payload = Vec::with_capacity(1 + part.len());
                payload.push(header);
                payload.extend_from_slice(part);
                let marker = index + 1 == elements.len() && part_index + 1 == total;
                out.push((Bytes::from(payload), marker));
            }
            index += 1;
            continue;
        }
        let count = group.len() as u8;
        let mut body = Vec::new();
        for (position, element) in group.iter().enumerate() {
            if position + 1 < group.len() {
                put_leb(&mut body, element.len());
            }
            body.extend_from_slice(element);
        }
        let mut header = count << 4;
        if out.is_empty() && !*sequence_sent && keyframe {
            header |= 0x08;
            *sequence_sent = true;
        }
        let marker = index == elements.len();
        let mut payload = Vec::with_capacity(1 + body.len());
        payload.push(header);
        payload.extend_from_slice(&body);
        out.push((Bytes::from(payload), marker));
    }
    Ok(out)
}

fn leb_size(mut value: usize) -> usize {
    let mut size = 1;
    while value >= 0x80 {
        value >>= 7;
        size += 1;
    }
    size
}

/// Reassemble AV1 OBU elements from RTP payloads.
#[derive(Debug, Default)]
pub(crate) struct Av1State {
    fragment: Option<Vec<u8>>,
    frame: Vec<u8>,
    keyframe: bool,
}

fn restore_element(element: &[u8]) -> Result<Vec<u8>> {
    let header = *element
        .first()
        .ok_or_else(|| Error::Media("empty AV1 OBU element".into()))?;
    let extension = header & 0x04 != 0;
    let payload_offset = if extension { 2 } else { 1 };
    if element.len() < payload_offset {
        return Err(Error::Media("short AV1 OBU element".into()));
    }
    let payload = &element[payload_offset..];
    let mut out = Vec::with_capacity(element.len() + 5);
    out.push(header | 0x02); // restore the OBU size field
    if extension {
        out.push(element[1]);
    }
    put_leb(&mut out, payload.len());
    out.extend_from_slice(payload);
    Ok(out)
}

impl Av1State {
    fn add_element(&mut self, element: &[u8]) -> Result<()> {
        let restored = restore_element(element)?;
        if restored.len() >= 2 {
            let obu_type = (restored[0] >> 3) & 0x0f;
            if obu_type == 1 {
                self.keyframe = true;
            }
        }
        self.frame.extend_from_slice(&restored);
        Ok(())
    }

    fn finish_fragment(&mut self) -> Result<()> {
        if let Some(element) = self.fragment.take() {
            self.add_element(&element)?;
        }
        Ok(())
    }

    pub(crate) fn depacketize(
        &mut self,
        payload: &[u8],
        marker: bool,
    ) -> Result<Option<(Bytes, bool)>> {
        if payload.len() < 2 {
            return Err(Error::Media("short AV1 RTP payload".into()));
        }
        let aggregation = payload[0];
        let body = &payload[1..];
        let continuation = aggregation & 0x80 != 0;
        let last_fragment = aggregation & 0x40 != 0;
        let count = (aggregation >> 4) & 0x03;
        if aggregation & 0x08 != 0 {
            self.keyframe = true;
        }
        if continuation {
            let Some(fragment) = self.fragment.as_mut() else {
                return Ok(None);
            };
            fragment.extend_from_slice(body);
            if !last_fragment {
                self.finish_fragment()?;
            }
        } else {
            self.fragment = None;
            let mut offset = 0usize;
            let elements = count.max(1) as usize;
            for index in 0..elements {
                let length = if count == 0 {
                    let (length, used) = leb(&body[offset..])?;
                    offset += used;
                    length as usize
                } else if index + 1 == elements {
                    body.len() - offset
                } else {
                    let (length, used) = leb(&body[offset..])?;
                    offset += used;
                    length as usize
                };
                if offset + length > body.len() {
                    return Err(Error::Media("short AV1 OBU element".into()));
                }
                let element = &body[offset..offset + length];
                offset += length;
                if index + 1 == elements && last_fragment {
                    self.fragment = Some(element.to_vec());
                } else {
                    self.add_element(element)?;
                }
            }
        }
        if !marker {
            return Ok(None);
        }
        if self.frame.is_empty() && self.fragment.is_none() {
            return Ok(None);
        }
        if let Some(fragment) = self.fragment.take() {
            self.add_element(&fragment)?;
        }
        let data = std::mem::take(&mut self.frame);
        let keyframe = std::mem::take(&mut self.keyframe);
        Ok(Some((Bytes::from(data), keyframe)))
    }
}
