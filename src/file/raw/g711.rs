use super::*;

pub(super) struct G711Demuxer {
    path: PathBuf,
    reader: BufReader<File>,
    description: MediaDescription,
    codec: Codec,
    sample_rate: u32,
    channels: u8,
    frame_bytes: usize,
    data_offset: u64,
    data_length: u64,
    bytes_read: u64,
    max_sample_size: usize,
}

impl G711Demuxer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn open(
        path: &Path,
        codec: Codec,
        sample_rate: u32,
        channels: u8,
        frame_duration: Duration,
        max_sample_size: usize,
    ) -> Result<Self> {
        let data_length = File::open(path)?.metadata()?.len();
        Self::with_region(
            path,
            codec,
            sample_rate,
            channels,
            frame_duration,
            max_sample_size,
            0,
            data_length,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn with_region(
        path: &Path,
        codec: Codec,
        sample_rate: u32,
        channels: u8,
        frame_duration: Duration,
        max_sample_size: usize,
        data_offset: u64,
        data_length: u64,
    ) -> Result<Self> {
        let samples =
            (frame_duration.as_nanos() * u128::from(sample_rate) / 1_000_000_000) as usize;
        let frame_bytes = samples.saturating_mul(usize::from(channels));
        if frame_bytes == 0 || frame_bytes > max_sample_size {
            return Err(Error::Limit("G.711 frame size"));
        }
        let mut reader = BufReader::new(File::open(path)?);
        reader.seek(SeekFrom::Start(data_offset))?;
        Ok(Self {
            path: path.to_owned(),
            reader,
            description: file_description(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("G.711 file"),
                vec![track_description(
                    0,
                    codec,
                    sample_rate,
                    Some(channels),
                    None,
                )],
            ),
            codec,
            sample_rate,
            channels,
            frame_bytes,
            data_offset,
            data_length,
            bytes_read: 0,
            max_sample_size,
        })
    }
}

impl Demuxer for G711Demuxer {
    fn description(&self) -> &MediaDescription {
        &self.description
    }
    fn duration(&self) -> Option<Duration> {
        Some(duration_from_ticks(
            self.data_length / u64::from(self.channels),
            u64::from(self.sample_rate),
        ))
    }
    fn next_sample(&mut self) -> Result<Option<DemuxedSample>> {
        if self.bytes_read >= self.data_length {
            return Ok(None);
        }
        let remaining = usize::try_from(self.data_length - self.bytes_read).unwrap_or(usize::MAX);
        let length = self.frame_bytes.min(remaining);
        if length > self.max_sample_size {
            return Err(Error::Limit("G.711 frame size"));
        }
        let mut data = vec![0u8; length];
        self.reader.read_exact(&mut data)?;
        let pts = duration_from_ticks(
            self.bytes_read / u64::from(self.channels),
            u64::from(self.sample_rate),
        );
        self.bytes_read += length as u64;
        let duration = duration_from_ticks(
            length as u64 / u64::from(self.channels),
            u64::from(self.sample_rate),
        );
        Ok(Some(DemuxedSample {
            unit: AccessUnit {
                track_id: 0,
                codec: self.codec,
                pts,
                dts: Some(pts),
                rtp_timestamp: rtp_timestamp(pts, self.sample_rate),
                is_keyframe: true,
                data: Bytes::from(data),
            },
            duration: Some(duration),
        }))
    }
    fn seek(&mut self, position: Duration) -> Result<()> {
        let requested_samples = position
            .as_secs()
            .saturating_mul(u64::from(self.sample_rate))
            + u64::from(position.subsec_nanos()) * u64::from(self.sample_rate) / 1_000_000_000;
        let requested_bytes = requested_samples.saturating_mul(u64::from(self.channels));
        let aligned = (requested_bytes / self.frame_bytes as u64) * self.frame_bytes as u64;
        self.bytes_read = aligned.min(self.data_length);
        self.reader = BufReader::new(File::open(&self.path)?);
        self.reader
            .seek(SeekFrom::Start(self.data_offset + self.bytes_read))?;
        Ok(())
    }
}
