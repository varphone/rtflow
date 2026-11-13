use super::*;

pub(super) struct AdtsDemuxer {
    path: PathBuf,
    reader: BufReader<File>,
    description: MediaDescription,
    sample_rate: u32,
    channels: u8,
    frame_index: u64,
    max_sample_size: usize,
    pending_sample: Option<DemuxedSample>,
}

impl AdtsDemuxer {
    pub(super) fn open(path: &Path, max_sample_size: usize) -> Result<Self> {
        let mut reader = BufReader::new(File::open(path)?);
        let header =
            read_adts_header(&mut reader)?.ok_or_else(|| Error::Media("empty ADTS file".into()))?;
        reader.seek(SeekFrom::Start(0))?;
        let profile = header.profile + 1;
        let asc = [
            (profile << 3) | (header.frequency_index >> 1),
            ((header.frequency_index & 1) << 7) | (header.channels << 3),
        ];
        let (_, sdp_channels, fmtp) = crate::file::bitstream::aac_audio_specific_config(&asc)?;
        Ok(Self {
            path: path.to_owned(),
            reader,
            description: file_description(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("AAC file"),
                vec![track_description(
                    0,
                    Codec::Aac,
                    header.sample_rate,
                    Some(sdp_channels),
                    Some(fmtp),
                )],
            ),
            sample_rate: header.sample_rate,
            channels: header.channels,
            frame_index: 0,
            max_sample_size,
            pending_sample: None,
        })
    }

    pub(super) fn next_inner(&mut self) -> Result<Option<DemuxedSample>> {
        let Some(header) = read_adts_header(&mut self.reader)? else {
            return Ok(None);
        };
        if header.sample_rate != self.sample_rate || header.channels != self.channels {
            return Err(Error::Media(
                "ADTS stream changes audio configuration".into(),
            ));
        }
        let payload_size = header
            .frame_length
            .checked_sub(header.header_size)
            .ok_or_else(|| Error::Media("invalid ADTS frame length".into()))?;
        if payload_size > self.max_sample_size {
            return Err(Error::Limit("AAC access unit size"));
        }
        if header.header_size == 9 {
            let mut crc = [0u8; 2];
            self.reader.read_exact(&mut crc)?;
        }
        let mut payload = vec![0u8; payload_size];
        self.reader.read_exact(&mut payload)?;
        let pts = duration_from_ticks(self.frame_index * 1024, u64::from(self.sample_rate));
        let duration = duration_from_ticks(1024, u64::from(self.sample_rate));
        self.frame_index += 1;
        Ok(Some(DemuxedSample {
            unit: AccessUnit {
                track_id: 0,
                codec: Codec::Aac,
                pts,
                dts: Some(pts),
                rtp_timestamp: rtp_timestamp(pts, self.sample_rate),
                is_keyframe: true,
                data: Bytes::from(payload),
            },
            duration: Some(duration),
        }))
    }

    pub(super) fn reset(&mut self) -> Result<()> {
        self.reader = BufReader::new(File::open(&self.path)?);
        self.frame_index = 0;
        self.pending_sample = None;
        Ok(())
    }
}

impl Demuxer for AdtsDemuxer {
    fn description(&self) -> &MediaDescription {
        &self.description
    }
    fn duration(&self) -> Option<Duration> {
        None
    }
    fn next_sample(&mut self) -> Result<Option<DemuxedSample>> {
        if self.pending_sample.is_some() {
            return Ok(self.pending_sample.take());
        }
        self.next_inner()
    }
    fn seek(&mut self, position: Duration) -> Result<()> {
        self.reset()?;
        while let Some(sample) = self.next_inner()? {
            if sample.unit.pts >= position {
                self.pending_sample = Some(sample);
                break;
            }
        }
        Ok(())
    }
}

pub(super) struct AdtsHeader {
    pub(super) profile: u8,
    pub(super) frequency_index: u8,
    pub(super) sample_rate: u32,
    pub(super) channels: u8,
    pub(super) frame_length: usize,
    pub(super) header_size: usize,
}

pub(super) fn read_adts_header(reader: &mut impl Read) -> Result<Option<AdtsHeader>> {
    let mut bytes = [0u8; 7];
    let mut read = 0;
    while read < bytes.len() {
        match reader.read(&mut bytes[read..])? {
            0 if read == 0 => return Ok(None),
            0 => return Err(Error::Media("truncated ADTS header".into())),
            count => read += count,
        }
    }
    if bytes[0] != 0xff || bytes[1] & 0xf6 != 0xf0 {
        return Err(Error::Media("invalid ADTS sync word".into()));
    }
    let frequency_index = (bytes[2] >> 2) & 0x0f;
    let sample_rate = aac_sample_rate(frequency_index)
        .ok_or_else(|| Error::Media("unsupported ADTS sample rate".into()))?;
    let channels = ((bytes[2] & 1) << 2) | (bytes[3] >> 6);
    if channels == 0 {
        return Err(Error::Media(
            "ADTS program-config-element channels are unsupported".into(),
        ));
    }
    let frame_length = (usize::from(bytes[3] & 3) << 11)
        | (usize::from(bytes[4]) << 3)
        | usize::from(bytes[5] >> 5);
    let header_size = if bytes[1] & 1 != 0 { 7 } else { 9 };
    if frame_length < header_size {
        return Err(Error::Media("ADTS frame is shorter than its header".into()));
    }
    Ok(Some(AdtsHeader {
        profile: bytes[2] >> 6,
        frequency_index,
        sample_rate,
        channels,
        frame_length,
        header_size,
    }))
}
