use super::*;

pub(super) struct TsPacket {
    pub(super) pid: u16,
    pub(super) payload_unit_start: bool,
    pub(super) continuity_counter: u8,
    pub(super) discontinuity: bool,
    pub(super) has_payload: bool,
    pub(super) payload: Vec<u8>,
}

pub(crate) struct TsReader {
    pub(super) reader: BufReader<File>,
    pub(super) packet_size: usize,
    pub(super) sync_offset: usize,
}

impl TsReader {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let mut reader = BufReader::new(File::open(path)?);
        let mut probe = vec![0u8; 192 * 5];
        let read = reader.read(&mut probe)?;
        probe.truncate(read);
        let (packet_size, sync_offset) = if has_sync_pattern(&probe, 188, 0) {
            (188, 0)
        } else if has_sync_pattern(&probe, 192, 4) {
            (192, 4)
        } else {
            return Err(Error::Media(
                "file is not 188-byte TS or 192-byte M2TS".into(),
            ));
        };
        reader.seek(SeekFrom::Start(0))?;
        Ok(Self {
            reader,
            packet_size,
            sync_offset,
        })
    }

    pub(super) fn rewind(&mut self) -> Result<()> {
        self.reader.seek(SeekFrom::Start(0))?;
        Ok(())
    }

    pub(super) fn next_packet(&mut self) -> Result<Option<TsPacket>> {
        let mut packet = vec![0u8; self.packet_size];
        let mut read = 0;
        while read < packet.len() {
            match self.reader.read(&mut packet[read..])? {
                0 if read == 0 => return Ok(None),
                0 => return Err(Error::Media("truncated MPEG-TS packet".into())),
                count => read += count,
            }
        }
        let data = &packet[self.sync_offset..self.sync_offset + TS_SIZE];
        if data[0] != 0x47 {
            return Err(Error::Media("MPEG-TS sync byte lost".into()));
        }
        if data[1] & 0x80 != 0 {
            return Ok(Some(TsPacket {
                pid: 0x1fff,
                payload_unit_start: false,
                continuity_counter: 0,
                discontinuity: true,
                has_payload: false,
                payload: Vec::new(),
            }));
        }
        let payload_unit_start = data[1] & 0x40 != 0;
        let pid = (u16::from(data[1] & 0x1f) << 8) | u16::from(data[2]);
        let adaptation_control = (data[3] >> 4) & 3;
        if adaptation_control == 0 {
            return Err(Error::Media("reserved MPEG-TS adaptation control".into()));
        }
        let has_payload = adaptation_control & 1 != 0;
        let mut offset = 4usize;
        let mut discontinuity = false;
        if adaptation_control & 2 != 0 {
            let length = usize::from(data[offset]);
            if offset + 1 + length > TS_SIZE {
                return Err(Error::Media("invalid MPEG-TS adaptation field".into()));
            }
            if length > 0 {
                discontinuity = data[offset + 1] & 0x80 != 0;
            }
            offset += 1 + length;
        }
        Ok(Some(TsPacket {
            pid,
            payload_unit_start,
            continuity_counter: data[3] & 0x0f,
            discontinuity,
            has_payload,
            payload: if has_payload {
                data[offset..].to_vec()
            } else {
                Vec::new()
            },
        }))
    }
}

pub(super) fn has_sync_pattern(data: &[u8], packet_size: usize, offset: usize) -> bool {
    (0..4).all(|index| data.get(offset + index * packet_size) == Some(&0x47))
}
