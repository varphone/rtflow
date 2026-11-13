use super::format::*;
use super::*;

pub struct RecordingDemuxer {
    catalog: RecordingCatalog,
    origin: Duration,
    index: usize,
    reader: Option<SegmentReader>,
    pending: Option<AccessUnit>,
    max_sample_size: usize,
}

impl RecordingDemuxer {
    pub fn open(directory: impl AsRef<Path>, max_sample_size: usize) -> Result<Self> {
        let catalog = RecordingCatalog::scan(directory, max_sample_size)?;
        let origin = catalog
            .segments
            .first()
            .map(|segment| segment.start)
            .unwrap_or_default();
        Ok(Self {
            catalog,
            origin,
            index: 0,
            reader: None,
            pending: None,
            max_sample_size,
        })
    }
}

impl Demuxer for RecordingDemuxer {
    fn description(&self) -> &MediaDescription {
        &self.catalog.description
    }

    fn duration(&self) -> Option<Duration> {
        Some(self.catalog.duration())
    }

    fn next_sample(&mut self) -> Result<Option<DemuxedSample>> {
        if let Some(unit) = self.pending.take() {
            return Ok(Some(DemuxedSample {
                unit,
                duration: None,
            }));
        }
        loop {
            if self.reader.is_none() {
                let Some(info) = self.catalog.segments.get(self.index) else {
                    return Ok(None);
                };
                self.reader = Some(SegmentReader::open(&info.path, self.max_sample_size)?.1);
                self.index += 1;
            }
            let Some(reader) = self.reader.as_mut() else {
                return Err(Error::Media(
                    "recording reader state is inconsistent".into(),
                ));
            };
            if let Some(unit) = reader.next_unit()? {
                return Ok(Some(DemuxedSample {
                    unit: normalize_unit(unit, self.origin),
                    duration: None,
                }));
            }
            self.reader = None;
        }
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        let target = self.origin.saturating_add(position);
        self.index = self
            .catalog
            .segments
            .iter()
            .position(|segment| segment.end >= target)
            .unwrap_or(self.catalog.segments.len());
        self.reader = None;
        self.pending = None;
        while let Some(sample) = self.next_sample()? {
            // Align to the next keyframe once past the requested position:
            // starting on a non-keyframe would black-screen until the next
            // IDR (and the sender drops those frames anyway).
            if sample.unit.pts >= position
                && (sample.unit.codec != crate::media::Codec::H264
                    && sample.unit.codec != crate::media::Codec::H265
                    || sample.unit.is_keyframe)
            {
                self.pending = Some(sample.unit);
                break;
            }
        }
        Ok(())
    }
}

pub(super) fn normalize_unit(mut unit: AccessUnit, origin: Duration) -> AccessUnit {
    unit.pts = unit.pts.saturating_sub(origin);
    unit.dts = unit.dts.map(|value| value.saturating_sub(origin));
    unit
}

pub(super) struct SegmentReader {
    pub(super) reader: BufReader<File>,
    pub(super) max_sample_size: usize,
}

impl SegmentReader {
    pub(super) fn open(path: &Path, max_sample_size: usize) -> Result<(MediaDescription, Self)> {
        let mut reader = BufReader::new(File::open(path)?);
        let description = read_description(&mut reader)?;
        Ok((
            description,
            Self {
                reader,
                max_sample_size,
            },
        ))
    }

    pub(super) fn next_unit(&mut self) -> Result<Option<AccessUnit>> {
        let mut magic = [0u8; 4];
        match self.reader.read_exact(&mut magic) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        if &magic != RECORD_MAGIC {
            return Err(Error::Media("invalid recording record".into()));
        }
        let mut fixed = [0u8; 28];
        match self.reader.read_exact(&mut fixed) {
            Ok(()) => {}
            // A truncated trailing record (e.g. after a disk-full aborted
            // segment) ends the segment gracefully instead of failing the
            // whole directory scan.
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let length = u32::from_be_bytes([fixed[24], fixed[25], fixed[26], fixed[27]]) as usize;
        if length > self.max_sample_size {
            return Err(Error::Limit("recorded access unit"));
        }
        let mut data = vec![0; length];
        match self.reader.read_exact(&mut data) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let dts = u64::from_be_bytes([
            fixed[12], fixed[13], fixed[14], fixed[15], fixed[16], fixed[17], fixed[18], fixed[19],
        ]);
        Ok(Some(AccessUnit {
            track_id: fixed[0],
            codec: codec_from_id(fixed[1])?,
            is_keyframe: fixed[2] != 0,
            pts: duration_from_us(u64::from_be_bytes([
                fixed[4], fixed[5], fixed[6], fixed[7], fixed[8], fixed[9], fixed[10], fixed[11],
            ])),
            dts: (dts != u64::MAX).then(|| duration_from_us(dts)),
            rtp_timestamp: u32::from_be_bytes([fixed[20], fixed[21], fixed[22], fixed[23]]),
            data: Bytes::from(data),
        }))
    }
}
