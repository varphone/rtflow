use super::*;

#[derive(Clone, Copy)]
pub(super) struct ProgramTrack {
    pub(super) pid: u16,
    pub(super) codec: Codec,
}

pub(super) fn probe_program(reader: &mut TsReader) -> Result<Vec<ProgramTrack>> {
    reader.rewind()?;
    let mut pmt_pid = None;
    let mut sections: HashMap<u16, Vec<u8>> = HashMap::new();
    for _ in 0..32_768 {
        let Some(packet) = reader.next_packet()? else {
            break;
        };
        if !packet.has_payload {
            continue;
        }
        if packet.pid != 0 && Some(packet.pid) != pmt_pid {
            continue;
        }
        if let Some(section) = push_section(
            &mut sections,
            packet.pid,
            packet.payload_unit_start,
            &packet.payload,
        )? {
            if packet.pid == 0 {
                pmt_pid = parse_pat(&section)?;
            } else if Some(packet.pid) == pmt_pid {
                let tracks = parse_pmt(&section)?;
                if !tracks.is_empty() {
                    return Ok(tracks);
                }
            }
        }
    }
    Err(Error::Media(
        "MPEG-TS PAT/PMT not found within probe limit".into(),
    ))
}

pub(super) fn push_section(
    buffers: &mut HashMap<u16, Vec<u8>>,
    pid: u16,
    unit_start: bool,
    payload: &[u8],
) -> Result<Option<Vec<u8>>> {
    let buffer = buffers.entry(pid).or_default();
    if unit_start {
        let pointer = usize::from(
            *payload
                .first()
                .ok_or_else(|| Error::Media("empty PSI payload".into()))?,
        );
        let start = 1usize
            .checked_add(pointer)
            .filter(|start| *start <= payload.len())
            .ok_or_else(|| Error::Media("invalid PSI pointer field".into()))?;
        buffer.clear();
        buffer.extend_from_slice(&payload[start..]);
    } else if !buffer.is_empty() {
        buffer.extend_from_slice(payload);
    }
    if buffer.len() < 3 {
        return Ok(None);
    }
    let section_length = (usize::from(buffer[1] & 0x0f) << 8) | usize::from(buffer[2]);
    let total = section_length + 3;
    if total > 4096 {
        return Err(Error::Limit("MPEG-TS PSI section"));
    }
    if buffer.len() < total {
        return Ok(None);
    }
    Ok(Some(buffer.drain(..total).collect()))
}

pub(super) fn parse_pat(section: &[u8]) -> Result<Option<u16>> {
    if section.len() < 12 || section[0] != 0 {
        return Err(Error::Media("invalid MPEG-TS PAT".into()));
    }
    let mut position = 8usize;
    let end = section.len() - 4;
    while position + 4 <= end {
        let program = u16::from_be_bytes([section[position], section[position + 1]]);
        let pid = (u16::from(section[position + 2] & 0x1f) << 8) | u16::from(section[position + 3]);
        if program != 0 {
            return Ok(Some(pid));
        }
        position += 4;
    }
    Ok(None)
}

pub(super) fn parse_pmt(section: &[u8]) -> Result<Vec<ProgramTrack>> {
    if section.len() < 16 || section[0] != 2 {
        return Err(Error::Media("invalid MPEG-TS PMT".into()));
    }
    let program_info_length = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
    let mut position = 12usize
        .checked_add(program_info_length)
        .ok_or(Error::Limit("MPEG-TS PMT"))?;
    let end = section.len() - 4;
    let mut tracks = Vec::new();
    while position + 5 <= end {
        let stream_type = section[position];
        let pid = (u16::from(section[position + 1] & 0x1f) << 8) | u16::from(section[position + 2]);
        let info_length =
            (usize::from(section[position + 3] & 0x0f) << 8) | usize::from(section[position + 4]);
        let info_end = position + 5 + info_length;
        if info_end > end {
            return Err(Error::Media("truncated MPEG-TS PMT entry".into()));
        }
        let codec = match stream_type {
            0x1b => Some(Codec::H264),
            0x24 => Some(Codec::H265),
            0x0f => Some(Codec::Aac),
            0x90 => Some(Codec::Pcma),
            0x91 => Some(Codec::Pcmu),
            0x06 => private_stream_codec(&section[position + 5..info_end]),
            _ => None,
        };
        if let Some(codec) = codec {
            tracks.push(ProgramTrack { pid, codec });
        }
        position = info_end;
    }
    Ok(tracks)
}

pub(super) fn private_stream_codec(descriptors: &[u8]) -> Option<Codec> {
    let mut position = 0usize;
    while position + 2 <= descriptors.len() {
        let tag = descriptors[position];
        let length = usize::from(descriptors[position + 1]);
        let end = position + 2 + length;
        if end > descriptors.len() {
            break;
        }
        if tag == 5 && length >= 4 {
            return match &descriptors[position + 2..position + 6] {
                b"PCMA" | b"ALAW" => Some(Codec::Pcma),
                b"PCMU" | b"ULAW" => Some(Codec::Pcmu),
                _ => None,
            };
        }
        position = end;
    }
    None
}
