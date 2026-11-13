use super::*;

pub(super) struct AnnexBReader {
    reader: BufReader<File>,
    buffer: Vec<u8>,
    eof: bool,
    max_nal_size: usize,
}

impl AnnexBReader {
    pub(super) fn open(path: &Path, max_nal_size: usize) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            buffer: Vec::with_capacity(64 * 1024),
            eof: false,
            max_nal_size,
        })
    }

    pub(super) fn next_nal(&mut self) -> Result<Option<Bytes>> {
        loop {
            if let Some((first, first_size)) = find_start_code(&self.buffer, 0) {
                if first > 0 {
                    self.buffer.drain(..first);
                    continue;
                }
                if let Some((second, _)) = find_start_code(&self.buffer, first_size) {
                    if second > self.max_nal_size + first_size {
                        return Err(Error::Limit("raw video NAL size"));
                    }
                    let nal = Bytes::copy_from_slice(&self.buffer[first_size..second]);
                    self.buffer.drain(..second);
                    if !nal.is_empty() {
                        return Ok(Some(nal));
                    }
                    continue;
                }
                if self.eof {
                    let nal = Bytes::copy_from_slice(&self.buffer[first_size..]);
                    self.buffer.clear();
                    return Ok((!nal.is_empty()).then_some(nal));
                }
            } else if self.eof {
                if self.buffer.iter().all(|byte| *byte == 0) {
                    self.buffer.clear();
                    return Ok(None);
                }
                return Err(Error::Media("raw video is not Annex-B formatted".into()));
            } else if self.buffer.len() > self.max_nal_size + 4 {
                return Err(Error::Limit("raw video NAL size"));
            }

            let mut chunk = [0u8; 64 * 1024];
            let read = self.reader.read(&mut chunk)?;
            if read == 0 {
                self.eof = true;
            } else {
                self.buffer.extend_from_slice(&chunk[..read]);
            }
        }
    }
}

fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if index + 4 <= data.len() && data[index..].starts_with(START_CODE) {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

pub(super) struct VideoDemuxer {
    path: PathBuf,
    reader: AnnexBReader,
    description: MediaDescription,
    codec: Codec,
    frame_rate: u32,
    max_sample_size: usize,
    frame_index: u64,
    pending_nal: Option<Bytes>,
    pending_sample: Option<DemuxedSample>,
    /// Parameter sets (SPS/PPS/VPS) collected from the stream, prepended
    /// to keyframes so decoders can initialize from mid-stream seeks.
    parameter_sets: Vec<Bytes>,
}

impl VideoDemuxer {
    pub(super) fn open(
        path: &Path,
        codec: Codec,
        frame_rate: u32,
        max_sample_size: usize,
    ) -> Result<Self> {
        let reader = AnnexBReader::open(path, max_sample_size)?;
        Ok(Self {
            path: path.to_owned(),
            reader,
            description: file_description(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("raw video"),
                vec![track_description(0, codec, 90_000, None, None)],
            ),
            codec,
            frame_rate,
            max_sample_size,
            frame_index: 0,
            pending_nal: None,
            pending_sample: None,
            parameter_sets: Vec::new(),
        })
    }

    pub(super) fn read_access_unit(&mut self) -> Result<Option<(Bytes, bool)>> {
        let mut output = BytesMut::new();
        let mut has_vcl = false;
        let mut is_keyframe = false;
        loop {
            let nal = match self.pending_nal.take() {
                Some(nal) => Some(nal),
                None => self.reader.next_nal()?,
            };
            let Some(nal) = nal else {
                let data = if is_keyframe && !self.parameter_sets.is_empty() {
                    // Prepend parameter sets so a decoder that joins at
                    // this keyframe (mid-stream seek) can initialize.
                    let mut merged = BytesMut::with_capacity(
                        output.len()
                            + self
                                .parameter_sets
                                .iter()
                                .map(|p| p.len() + 4)
                                .sum::<usize>(),
                    );
                    for parameter_set in &self.parameter_sets {
                        merged.extend_from_slice(START_CODE);
                        merged.extend_from_slice(parameter_set);
                    }
                    merged.extend_from_slice(&output);
                    merged.freeze()
                } else {
                    output.freeze()
                };
                let has_data = !data.is_empty();
                return Ok(has_data.then_some((data, is_keyframe)));
            };
            // Collect parameter sets (H.264 SPS/PPS; H.265 VPS/SPS/PPS).
            let nal_type = match self.codec {
                Codec::H264 => crate::media::bitstream::nal_type_h264(&nal),
                Codec::H265 => crate::media::bitstream::nal_type_h265(&nal),
                _ => None,
            };
            let is_parameter_set =
                matches!(nal_type, Some(7 | 8)) || matches!(nal_type, Some(32..=34));
            if is_parameter_set && self.parameter_sets.len() < 8 {
                self.parameter_sets.push(nal.clone());
            }
            let (is_vcl, starts_picture, starts_prefix, keyframe) =
                nal_properties(self.codec, &nal);
            let boundary = !output.is_empty()
                && ((is_vcl && has_vcl && starts_picture) || (has_vcl && starts_prefix));
            if boundary {
                self.pending_nal = Some(nal);
                return Ok(Some((output.freeze(), is_keyframe)));
            }
            if output.len() + START_CODE.len() + nal.len() > self.max_sample_size {
                return Err(Error::Limit("raw video access unit size"));
            }
            output.extend_from_slice(START_CODE);
            output.extend_from_slice(&nal);
            has_vcl |= is_vcl;
            is_keyframe |= keyframe;
        }
    }

    pub(super) fn next_inner(&mut self) -> Result<Option<DemuxedSample>> {
        let Some((data, is_keyframe)) = self.read_access_unit()? else {
            return Ok(None);
        };
        let pts = duration_from_ticks(self.frame_index, u64::from(self.frame_rate));
        let duration = duration_from_ticks(1, u64::from(self.frame_rate));
        self.frame_index += 1;
        Ok(Some(DemuxedSample {
            unit: AccessUnit {
                track_id: 0,
                codec: self.codec,
                pts,
                dts: Some(pts),
                rtp_timestamp: rtp_timestamp(pts, 90_000),
                is_keyframe,
                data,
            },
            duration: Some(duration),
        }))
    }

    pub(super) fn reset(&mut self) -> Result<()> {
        self.reader = AnnexBReader::open(&self.path, self.max_sample_size)?;
        self.frame_index = 0;
        self.pending_nal = None;
        self.pending_sample = None;
        Ok(())
    }
}

impl Demuxer for VideoDemuxer {
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

pub(super) fn nal_properties(codec: Codec, nal: &[u8]) -> (bool, bool, bool, bool) {
    match codec {
        Codec::H264 => {
            let kind = nal.first().map_or(0, |byte| byte & 0x1f);
            let is_vcl = matches!(kind, 1..=5);
            let starts_picture =
                is_vcl && first_mb_in_slice_is_zero(nal.get(1..).unwrap_or_default());
            let starts_prefix = matches!(kind, 6..=9);
            (is_vcl, starts_picture, starts_prefix, kind == 5)
        }
        Codec::H265 => {
            let kind = nal.first().map_or(0, |byte| (byte >> 1) & 0x3f);
            let is_vcl = kind <= 31;
            let starts_picture = is_vcl && nal.get(2).is_some_and(|byte| byte & 0x80 != 0);
            let starts_prefix = matches!(kind, 32..=35 | 39);
            (
                is_vcl,
                starts_picture,
                starts_prefix,
                matches!(kind, 19..=21),
            )
        }
        Codec::Aac
        | Codec::Pcma
        | Codec::Pcmu
        | Codec::Opus
        | Codec::Vp8
        | Codec::Vp9
        | Codec::Mp3
        | Codec::G726_16
        | Codec::G726_24
        | Codec::G726_32
        | Codec::G726_40 => (true, true, false, true),
    }
}

pub(super) fn first_mb_in_slice_is_zero(ebsp: &[u8]) -> bool {
    let mut rbsp = Vec::with_capacity(ebsp.len());
    let mut zero_count = 0;
    for byte in ebsp {
        if zero_count >= 2 && *byte == 3 {
            zero_count = 0;
            continue;
        }
        rbsp.push(*byte);
        zero_count = if *byte == 0 { zero_count + 1 } else { 0 };
    }
    for (leading_zeros, bit_index) in (0..rbsp.len() * 8).enumerate() {
        let bit = (rbsp[bit_index / 8] >> (7 - bit_index % 8)) & 1;
        if bit == 1 {
            return leading_zeros == 0;
        }
    }
    false
}
