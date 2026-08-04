use super::{
    bitstream::{START_CODE, aac_sample_rate},
    source::{
        DemuxedSample, Demuxer, FileSourceOptions, RawFormat, duration_from_ticks,
        file_description, rtp_timestamp, track_description,
    },
};
use crate::{
    Error, Result,
    media::{AccessUnit, Codec, MediaDescription},
};
use bytes::{Bytes, BytesMut};
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) fn open_raw(
    path: &Path,
    format: RawFormat,
    options: &FileSourceOptions,
) -> Result<Box<dyn Demuxer>> {
    match format {
        RawFormat::H264 => Ok(Box::new(VideoDemuxer::open(
            path,
            Codec::H264,
            options.raw_video_frame_rate,
            options.max_sample_size,
        )?)),
        RawFormat::H265 => Ok(Box::new(VideoDemuxer::open(
            path,
            Codec::H265,
            options.raw_video_frame_rate,
            options.max_sample_size,
        )?)),
        RawFormat::AacAdts => Ok(Box::new(AdtsDemuxer::open(path, options.max_sample_size)?)),
        RawFormat::Pcma | RawFormat::Pcmu => Ok(Box::new(G711Demuxer::open(
            path,
            if format == RawFormat::Pcma {
                Codec::Pcma
            } else {
                Codec::Pcmu
            },
            options.raw_g711_sample_rate,
            options.raw_g711_channels,
            options.raw_g711_frame_duration,
            options.max_sample_size,
        )?)),
    }
}

struct AnnexBReader {
    reader: BufReader<File>,
    buffer: Vec<u8>,
    eof: bool,
    max_nal_size: usize,
}

impl AnnexBReader {
    fn open(path: &Path, max_nal_size: usize) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            buffer: Vec::with_capacity(64 * 1024),
            eof: false,
            max_nal_size,
        })
    }

    fn next_nal(&mut self) -> Result<Option<Bytes>> {
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

struct VideoDemuxer {
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
    fn open(path: &Path, codec: Codec, frame_rate: u32, max_sample_size: usize) -> Result<Self> {
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

    fn read_access_unit(&mut self) -> Result<Option<(Bytes, bool)>> {
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
                Codec::H264 => nal.first().map(|byte| byte & 0x1F),
                Codec::H265 => nal.first().map(|byte| (byte >> 1) & 0x3F),
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

    fn next_inner(&mut self) -> Result<Option<DemuxedSample>> {
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

    fn reset(&mut self) -> Result<()> {
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

fn nal_properties(codec: Codec, nal: &[u8]) -> (bool, bool, bool, bool) {
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
        Codec::Aac | Codec::Pcma | Codec::Pcmu | Codec::Opus | Codec::Vp8 | Codec::Vp9 => {
            (true, true, false, true)
        }
    }
}

fn first_mb_in_slice_is_zero(ebsp: &[u8]) -> bool {
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

struct AdtsDemuxer {
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
    fn open(path: &Path, max_sample_size: usize) -> Result<Self> {
        let mut reader = BufReader::new(File::open(path)?);
        let header =
            read_adts_header(&mut reader)?.ok_or_else(|| Error::Media("empty ADTS file".into()))?;
        reader.seek(SeekFrom::Start(0))?;
        let profile = header.profile + 1;
        let asc = [
            (profile << 3) | (header.frequency_index >> 1),
            ((header.frequency_index & 1) << 7) | (header.channels << 3),
        ];
        let (_, sdp_channels, fmtp) = super::bitstream::aac_audio_specific_config(&asc)?;
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

    fn next_inner(&mut self) -> Result<Option<DemuxedSample>> {
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

    fn reset(&mut self) -> Result<()> {
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

struct AdtsHeader {
    profile: u8,
    frequency_index: u8,
    sample_rate: u32,
    channels: u8,
    frame_length: usize,
    header_size: usize,
}

fn read_adts_header(reader: &mut impl Read) -> Result<Option<AdtsHeader>> {
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

struct G711Demuxer {
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
    fn open(
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
    fn with_region(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_slice_boundary_detection() {
        assert_eq!(
            nal_properties(Codec::H264, &[0x65, 0x80]),
            (true, true, false, true)
        );
        assert!(!first_mb_in_slice_is_zero(&[0x40]));
    }

    #[test]
    fn adts_header_fields() {
        let bytes = [0xff, 0xf1, 0x4c, 0x80, 0x01, 0x3f, 0xfc];
        let header = read_adts_header(&mut &bytes[..]).unwrap().unwrap();
        assert_eq!(header.sample_rate, 48_000);
        assert_eq!(header.channels, 2);
        assert_eq!(header.frame_length, 9);
    }
}
