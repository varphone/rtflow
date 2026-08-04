use super::{
    bitstream::{
        VideoConfig, aac_audio_specific_config, avcc_config, hvcc_config,
        length_prefixed_to_annex_b,
    },
    source::{
        DemuxedSample, Demuxer, duration_from_ticks, file_description, rtp_timestamp,
        track_description,
    },
};
use crate::{
    Error, Result,
    media::{AccessUnit, Codec, MediaDescription},
};
use bytes::Bytes;
use mp4box::{Box as IsoBox, SampleInfo, StructuredData};
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) struct Mp4Demuxer {
    reader: BufReader<File>,
    file_size: u64,
    description: MediaDescription,
    duration: Duration,
    tracks: Vec<TrackState>,
    max_sample_size: usize,
    pending_output: Option<DemuxedSample>,
    _path: PathBuf,
}

struct TrackState {
    track_id: u8,
    codec: Codec,
    clock_rate: u32,
    timescale: u32,
    samples: Vec<SampleInfo>,
    next_sample: usize,
    video_config: Option<VideoConfig>,
    /// Edit-list media_time offset in timescale ticks (elst) applied to
    /// every sample timestamp.
    edit_offset: i64,
}

impl Mp4Demuxer {
    pub fn open(path: &Path, max_sample_size: usize) -> Result<Self> {
        let mut reader = BufReader::new(File::open(path)?);
        let file_size = reader.get_ref().metadata()?.len();
        let boxes = mp4box::get_boxes(&mut reader, file_size, true)
            .map_err(|error| Error::Media(format!("invalid MP4/MOV container: {error}")))?;
        let mut sample_sets = mp4box::track_samples_from_reader(File::open(path)?)
            .map_err(|error| Error::Media(format!("invalid MP4 sample index: {error}")))?
            .into_iter()
            .map(|samples| (samples.track_id, samples))
            .collect::<std::collections::HashMap<_, _>>();
        let mut parsed = Vec::new();
        for moov in boxes.iter().filter(|node| node.typ == "moov") {
            let Some(children) = &moov.children else {
                continue;
            };
            for trak in children.iter().filter(|node| node.typ == "trak") {
                let Some(track_id) = track_id(trak) else {
                    continue;
                };
                let Some(samples) = sample_sets.remove(&track_id) else {
                    continue;
                };
                parsed.push((track_id, trak, samples));
            }
        }
        parsed.sort_by_key(|(track_id, _, _)| *track_id);
        let mut tracks = Vec::new();
        let mut descriptions = Vec::new();
        let mut duration = Duration::ZERO;
        for (_, trak, samples) in parsed {
            let Some(entry) = sample_entry(trak) else {
                continue;
            };
            let (codec, clock_rate, channels, fmtp, video_config) = match entry.codec.as_str() {
                "avc1" | "avc2" | "avc3" | "avc4" => {
                    let config = read_video_config(&mut reader, trak, "avcC", avcc_config)?;
                    (Codec::H264, 90_000, None, config.fmtp.clone(), Some(config))
                }
                "hev1" | "hvc1" => {
                    let config = read_video_config(&mut reader, trak, "hvcC", hvcc_config)?;
                    (Codec::H265, 90_000, None, config.fmtp.clone(), Some(config))
                }
                "mp4a" => {
                    let asc = read_aac_config(&mut reader, trak)?;
                    let (sample_rate, channels, fmtp) = aac_audio_specific_config(&asc)?;
                    (Codec::Aac, sample_rate, Some(channels), Some(fmtp), None)
                }
                "alaw" => (
                    Codec::Pcma,
                    samples.timescale,
                    Some(entry.channel_count.unwrap_or(1) as u8),
                    None,
                    None,
                ),
                "ulaw" => (
                    Codec::Pcmu,
                    samples.timescale,
                    Some(entry.channel_count.unwrap_or(1) as u8),
                    None,
                    None,
                ),
                _ => continue,
            };
            if samples.timescale == 0 {
                return Err(Error::Media("MP4 track timescale is zero".into()));
            }
            if samples.samples.iter().any(|sample| {
                sample.size as usize > max_sample_size
                    || sample
                        .file_offset
                        .checked_add(u64::from(sample.size))
                        .is_none_or(|end| end > file_size)
            }) {
                return Err(Error::Limit("MP4 media sample size or offset"));
            }
            let track_id =
                u8::try_from(tracks.len()).map_err(|_| Error::Limit("file media tracks"))?;
            if track_id > 31 {
                return Err(Error::Limit("dynamic RTP payload types"));
            }
            descriptions.push(track_description(
                track_id, codec, clock_rate, channels, fmtp,
            ));
            duration = duration.max(duration_from_ticks(
                samples.duration,
                u64::from(samples.timescale),
            ));
            let edit_offset = edit_media_time(trak);
            if edit_offset > 0 {
                tracing::debug!(
                    "mp4: track {track_id} edit list media_time={edit_offset} ticks (timescale {})",
                    samples.timescale
                );
            }
            tracks.push(TrackState {
                track_id,
                codec,
                clock_rate,
                timescale: samples.timescale,
                samples: samples.samples,
                next_sample: 0,
                video_config,
                edit_offset,
            });
        }
        if tracks.is_empty() {
            return Err(Error::Media(
                "MP4/MOV has no supported H264, H265, AAC, or G.711 tracks".into(),
            ));
        }
        Ok(Self {
            reader,
            file_size,
            description: file_description(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("MP4 file"),
                descriptions,
            ),
            duration,
            tracks,
            max_sample_size,
            pending_output: None,
            _path: path.to_owned(),
        })
    }

    fn next_inner(&mut self) -> Result<Option<DemuxedSample>> {
        let selected = self
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(index, track)| {
                track.samples.get(track.next_sample).map(|sample| {
                    (
                        index,
                        duration_from_ticks(sample.dts, u64::from(track.timescale)),
                    )
                })
            })
            .min_by_key(|(_, time)| *time)
            .map(|(index, _)| index);
        let Some(index) = selected else {
            return Ok(None);
        };
        let track = &mut self.tracks[index];
        let sample = track.samples[track.next_sample].clone();
        track.next_sample += 1;
        let size = sample.size as usize;
        if size > self.max_sample_size
            || sample
                .file_offset
                .checked_add(u64::from(sample.size))
                .is_none_or(|end| end > self.file_size)
        {
            return Err(Error::Limit("MP4 media sample size or offset"));
        }
        self.reader.seek(SeekFrom::Start(sample.file_offset))?;
        let mut bytes = vec![0u8; size];
        self.reader.read_exact(&mut bytes)?;
        // Apply the edit list: timestamps start after the leading edit
        // (media_time), e.g. an initial audio delay in ffmpeg output.
        let dts = duration_from_ticks(
            sample.dts.saturating_sub(track.edit_offset.max(0) as u64),
            u64::from(track.timescale),
        );
        let pts = duration_from_ticks(
            sample.pts.saturating_sub(track.edit_offset.max(0) as u64),
            u64::from(track.timescale),
        );
        let duration = duration_from_ticks(u64::from(sample.duration), u64::from(track.timescale));
        let data = Bytes::from(bytes);
        let data = match &track.video_config {
            Some(config) => length_prefixed_to_annex_b(
                data,
                config.length_size,
                &config.parameter_sets,
                sample.is_sync,
            )?,
            None => data,
        };
        Ok(Some(DemuxedSample {
            unit: AccessUnit {
                track_id: track.track_id,
                codec: track.codec,
                pts,
                dts: Some(dts),
                rtp_timestamp: rtp_timestamp(pts, track.clock_rate),
                is_keyframe: sample.is_sync,
                data,
            },
            duration: Some(duration),
        }))
    }

    fn reset(&mut self) {
        for track in &mut self.tracks {
            track.next_sample = 0;
        }
        self.pending_output = None;
    }
}

impl Demuxer for Mp4Demuxer {
    fn description(&self) -> &MediaDescription {
        &self.description
    }
    fn duration(&self) -> Option<Duration> {
        Some(self.duration)
    }
    fn next_sample(&mut self) -> Result<Option<DemuxedSample>> {
        if self.pending_output.is_some() {
            return Ok(self.pending_output.take());
        }
        self.next_inner()
    }
    fn seek(&mut self, position: Duration) -> Result<()> {
        self.reset();
        for track in &mut self.tracks {
            track.next_sample = track
                .samples
                .iter()
                .position(|sample| {
                    duration_from_ticks(sample.pts, u64::from(track.timescale)) >= position
                })
                .unwrap_or(track.samples.len());
            if matches!(track.codec, Codec::H264 | Codec::H265) {
                while track.next_sample > 0
                    && track
                        .samples
                        .get(track.next_sample)
                        .is_none_or(|sample| !sample.is_sync)
                {
                    track.next_sample -= 1;
                }
            }
        }
        while let Some(sample) = self.next_inner()? {
            if sample.unit.pts >= position || sample.unit.is_keyframe {
                self.pending_output = Some(sample);
                break;
            }
        }
        Ok(())
    }
}

fn track_id(trak: &IsoBox) -> Option<u32> {
    let tkhd = find_descendant(trak, "tkhd")?;
    let StructuredData::TrackHeader(data) = tkhd.structured_data.as_ref()? else {
        return None;
    };
    Some(data.track_id)
}

/// First non-empty edit list media_time (in media timescale ticks) for a
/// track, or 0 when there is no edit list / no usable entry. Negative
/// media_time marks an empty edit (leading gap) and is skipped.
fn edit_media_time(trak: &IsoBox) -> i64 {
    let Some(edts) = find_descendant(trak, "elst") else {
        return 0;
    };
    let Some(StructuredData::EditList(data)) = edts.structured_data.as_ref() else {
        return 0;
    };
    data.entries
        .iter()
        .find_map(|entry| (entry.media_time >= 0).then_some(entry.media_time))
        .unwrap_or(0)
}

fn sample_entry(trak: &IsoBox) -> Option<&mp4box::SampleEntry> {
    let stsd = find_descendant(trak, "stsd")?;
    let StructuredData::SampleDescription(data) = stsd.structured_data.as_ref()? else {
        return None;
    };
    data.entries.first()
}

fn find_descendant<'a>(node: &'a IsoBox, name: &str) -> Option<&'a IsoBox> {
    if node.typ == name {
        return Some(node);
    }
    node.children
        .as_ref()?
        .iter()
        .find_map(|child| find_descendant(child, name))
}

fn read_payload(reader: &mut BufReader<File>, node: &IsoBox, limit: usize) -> Result<Vec<u8>> {
    let offset = node
        .payload_offset
        .ok_or_else(|| Error::Media(format!("{} has no payload", node.typ)))?;
    let size = usize::try_from(
        node.payload_size
            .ok_or_else(|| Error::Media(format!("{} has no payload size", node.typ)))?,
    )
    .map_err(|_| Error::Limit("MP4 codec configuration"))?;
    if size > limit {
        return Err(Error::Limit("MP4 codec configuration"));
    }
    reader.seek(SeekFrom::Start(offset))?;
    let mut data = vec![0u8; size];
    reader.read_exact(&mut data)?;
    Ok(data)
}

fn read_video_config(
    reader: &mut BufReader<File>,
    trak: &IsoBox,
    name: &str,
    parse: fn(&[u8]) -> Result<VideoConfig>,
) -> Result<VideoConfig> {
    let node = find_descendant(trak, name)
        .ok_or_else(|| Error::Media(format!("MP4 video track has no {name} box")))?;
    parse(&read_payload(reader, node, 1024 * 1024)?)
}

fn read_aac_config(reader: &mut BufReader<File>, trak: &IsoBox) -> Result<Vec<u8>> {
    let esds = find_descendant(trak, "esds")
        .ok_or_else(|| Error::Media("MP4 AAC track has no esds box".into()))?;
    let payload = read_payload(reader, esds, 64 * 1024)?;
    find_decoder_specific_info(&payload)
        .ok_or_else(|| Error::Media("MP4 esds has no AAC DecoderSpecificInfo".into()))
}

fn find_decoder_specific_info(data: &[u8]) -> Option<Vec<u8>> {
    for position in 0..data.len() {
        if data[position] != 5 {
            continue;
        }
        let (length, header) = descriptor_length(data.get(position + 1..)?)?;
        let start = position + 1 + header;
        let end = start.checked_add(length)?;
        let value = data.get(start..end)?;
        if value.len() >= 2 && aac_audio_specific_config(value).is_ok() {
            return Some(value.to_vec());
        }
    }
    None
}

fn descriptor_length(data: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for (index, byte) in data.iter().take(4).enumerate() {
        value = value.checked_shl(7)? | usize::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_aac_decoder_specific_info() {
        let esds = [
            3, 0x80, 0x80, 0x80, 4, 0, 0, 4, 0x11, 0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 2,
            0x11, 0x90,
        ];
        assert_eq!(find_decoder_specific_info(&esds).unwrap(), vec![0x11, 0x90]);
    }
}
