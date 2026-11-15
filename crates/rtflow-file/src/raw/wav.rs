use super::*;

pub(crate) struct WavDemuxer {
    inner: G711Demuxer,
}

impl WavDemuxer {
    pub fn open(path: &Path, frame_duration: Duration, max_sample_size: usize) -> Result<Self> {
        let mut file = BufReader::new(File::open(path)?);
        let mut riff = [0u8; 12];
        file.read_exact(&mut riff)?;
        if &riff[..4] != b"RIFF" || &riff[8..] != b"WAVE" {
            return Err(Error::Media("file is not a RIFF/WAVE container".into()));
        }
        let mut format = None;
        let mut data = None;
        loop {
            let mut header = [0u8; 8];
            match file.read_exact(&mut header) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error.into()),
            }
            let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
            let offset = file.stream_position()?;
            match &header[..4] {
                b"fmt " => {
                    if size < 16 {
                        return Err(Error::Media("short WAVE fmt chunk".into()));
                    }
                    let mut bytes = [0u8; 16];
                    file.read_exact(&mut bytes)?;
                    format = Some((
                        u16::from_le_bytes(bytes[0..2].try_into().unwrap()),
                        u16::from_le_bytes(bytes[2..4].try_into().unwrap()),
                        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                        u16::from_le_bytes(bytes[14..16].try_into().unwrap()),
                    ));
                }
                b"data" => data = Some((offset, size)),
                _ => {}
            }
            if size == 0 {
                // A zero-length chunk before fmt/data were both found is
                // malformed: the seek below would reposition onto the same
                // header and loop forever.
                return Err(Error::Media("WAVE file has a zero-length chunk".into()));
            }
            file.seek(SeekFrom::Start(offset + size + (size & 1)))?;
            if format.is_some() && data.is_some() {
                break;
            }
        }
        let (tag, channels, sample_rate, bits) =
            format.ok_or_else(|| Error::Media("WAVE file has no fmt chunk".into()))?;
        let codec = match tag {
            6 => Codec::Pcma,
            7 => Codec::Pcmu,
            _ => {
                return Err(Error::Media(format!(
                    "unsupported WAVE format tag {tag}; expected A-law (6) or mu-law (7)"
                )));
            }
        };
        if bits != 8 || channels == 0 || channels > u16::from(u8::MAX) {
            return Err(Error::Media("unsupported G.711 WAVE sample layout".into()));
        }
        let (data_offset, data_length) =
            data.ok_or_else(|| Error::Media("WAVE file has no data chunk".into()))?;
        Ok(Self {
            inner: G711Demuxer::with_region(
                path,
                codec,
                sample_rate,
                channels as u8,
                frame_duration,
                max_sample_size,
                data_offset,
                data_length,
            )?,
        })
    }
}

impl Demuxer for WavDemuxer {
    fn description(&self) -> &MediaDescription {
        self.inner.description()
    }
    fn duration(&self) -> Option<Duration> {
        self.inner.duration()
    }
    fn next_sample(&mut self) -> Result<Option<DemuxedSample>> {
        self.inner.next_sample()
    }
    fn seek(&mut self, position: Duration) -> Result<()> {
        self.inner.seek(position)
    }
}
