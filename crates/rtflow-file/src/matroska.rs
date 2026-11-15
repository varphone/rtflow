use super::{
    bitstream::{
        VideoConfig, aac_audio_specific_config, avcc_config, hvcc_config, is_keyframe,
        length_prefixed_to_annex_b, prepend_parameter_sets,
    },
    source::{DemuxedSample, Demuxer, file_description, rtp_timestamp, track_description},
};
use rtflow_core::error::{Error, Result};
use rtflow_media::{AccessUnit, Codec, MediaDescription};
use bytes::Bytes;
use matroska_demuxer::{Frame, MatroskaFile, TrackType};
use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) struct MatroskaDemuxer {
    path: PathBuf,
    reader: MatroskaFile<BufReader<File>>,
    description: MediaDescription,
    duration: Option<Duration>,
    timestamp_scale: u64,
    tracks: Vec<TrackState>,
    frame: Frame,
    max_sample_size: usize,
    pending_output: Option<DemuxedSample>,
}

struct TrackState {
    source_id: u64,
    track_id: u8,
    codec: Codec,
    clock_rate: u32,
    codec_delay_ns: u64,
    default_duration_ns: Option<u64>,
    video_config: Option<VideoConfig>,
}

impl MatroskaDemuxer {
    pub fn open(path: &Path, max_sample_size: usize) -> Result<Self> {
        let reader = MatroskaFile::open(BufReader::new(File::open(path)?))
            .map_err(|error| Error::Media(format!("invalid Matroska/WebM container: {error}")))?;
        let timestamp_scale = reader.info().timestamp_scale().get();
        let duration = reader.info().duration().and_then(|value| {
            let nanos = value * timestamp_scale as f64;
            nanos
                .is_finite()
                .then(|| Duration::from_secs_f64(nanos / 1_000_000_000.0))
        });
        let mut tracks = Vec::new();
        let mut descriptions = Vec::new();
        for source in reader.tracks() {
            if !source.flag_enabled()
                || !matches!(source.track_type(), TrackType::Video | TrackType::Audio)
            {
                continue;
            }
            let parsed = match source.codec_id() {
                "V_MPEG4/ISO/AVC" => {
                    let private = source.codec_private().ok_or_else(|| {
                        Error::Media("Matroska AVC track has no CodecPrivate".into())
                    })?;
                    let config = avcc_config(private)?;
                    Some((Codec::H264, 90_000, None, config.fmtp.clone(), Some(config)))
                }
                "V_MPEGH/ISO/HEVC" => {
                    let private = source.codec_private().ok_or_else(|| {
                        Error::Media("Matroska HEVC track has no CodecPrivate".into())
                    })?;
                    let config = hvcc_config(private)?;
                    Some((Codec::H265, 90_000, None, config.fmtp.clone(), Some(config)))
                }
                "A_AAC" => {
                    let private = source.codec_private().ok_or_else(|| {
                        Error::Media("Matroska AAC track has no AudioSpecificConfig".into())
                    })?;
                    let (sample_rate, channels, fmtp) = aac_audio_specific_config(private)?;
                    Some((Codec::Aac, sample_rate, Some(channels), Some(fmtp), None))
                }
                "A_MS/ACM" => parse_wave_format(source.codec_private())?,
                _ => None,
            };
            let Some((codec, clock_rate, channels, fmtp, video_config)) = parsed else {
                continue;
            };
            if source
                .content_encodings()
                .is_some_and(|values| !values.is_empty())
            {
                return Err(Error::Media(format!(
                    "Matroska content encoding is unsupported for track {}",
                    source.track_number()
                )));
            }
            let track_id =
                u8::try_from(tracks.len()).map_err(|_| Error::Limit("file media tracks"))?;
            if track_id > 31 {
                return Err(Error::Limit("dynamic RTP payload types"));
            }
            descriptions.push(track_description(
                track_id, codec, clock_rate, channels, fmtp,
            ));
            tracks.push(TrackState {
                source_id: source.track_number().get(),
                track_id,
                codec,
                clock_rate,
                codec_delay_ns: source.codec_delay().unwrap_or(0),
                default_duration_ns: source.default_duration().map(|value| value.get()),
                video_config,
            });
        }
        if tracks.is_empty() {
            return Err(Error::Media(
                "Matroska/WebM has no supported H264, H265, AAC, or G.711 tracks".into(),
            ));
        }
        Ok(Self {
            path: path.to_owned(),
            reader,
            description: file_description(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Matroska file"),
                descriptions,
            ),
            duration,
            timestamp_scale,
            tracks,
            frame: Frame::default(),
            max_sample_size,
            pending_output: None,
        })
    }

    fn next_inner(&mut self) -> Result<Option<DemuxedSample>> {
        loop {
            let available = self
                .reader
                .next_frame(&mut self.frame)
                .map_err(|error| Error::Media(format!("failed to read Matroska frame: {error}")))?;
            if !available {
                return Ok(None);
            }
            let Some(track) = self
                .tracks
                .iter()
                .find(|track| track.source_id == self.frame.track)
            else {
                continue;
            };
            if self.frame.data.len() > self.max_sample_size {
                return Err(Error::Limit("Matroska media sample size"));
            }
            let timestamp_ns = self
                .frame
                .timestamp
                .saturating_mul(self.timestamp_scale)
                .saturating_sub(track.codec_delay_ns);
            let pts = Duration::from_nanos(timestamp_ns);
            let duration_ns = self
                .frame
                .duration
                .map(|value| value.saturating_mul(self.timestamp_scale))
                .or(track.default_duration_ns);
            let source_data = Bytes::from(std::mem::take(&mut self.frame.data));
            let marked_keyframe = self.frame.is_keyframe.unwrap_or(false);
            let data = match &track.video_config {
                Some(config) => length_prefixed_to_annex_b(
                    source_data,
                    config.length_size,
                    &config.parameter_sets,
                    marked_keyframe,
                )?,
                None => source_data,
            };
            let keyframe = self
                .frame
                .is_keyframe
                .unwrap_or_else(|| is_keyframe(track.codec, &data));
            let data = match &track.video_config {
                Some(config) if keyframe && !marked_keyframe => {
                    prepend_parameter_sets(data, &config.parameter_sets)
                }
                _ => data,
            };
            return Ok(Some(DemuxedSample {
                unit: AccessUnit {
                    track_id: track.track_id,
                    codec: track.codec,
                    pts,
                    dts: None,
                    rtp_timestamp: rtp_timestamp(pts, track.clock_rate),
                    is_keyframe: keyframe,
                    data,
                },
                duration: duration_ns.map(Duration::from_nanos),
            }));
        }
    }

    fn reopen(&mut self) -> Result<()> {
        self.reader = MatroskaFile::open(BufReader::new(File::open(&self.path)?))
            .map_err(|error| Error::Media(format!("failed to reopen Matroska file: {error}")))?;
        self.frame = Frame::default();
        self.pending_output = None;
        Ok(())
    }
}

impl Demuxer for MatroskaDemuxer {
    fn description(&self) -> &MediaDescription {
        &self.description
    }
    fn duration(&self) -> Option<Duration> {
        self.duration
    }
    fn next_sample(&mut self) -> Result<Option<DemuxedSample>> {
        if self.pending_output.is_some() {
            return Ok(self.pending_output.take());
        }
        self.next_inner()
    }
    fn seek(&mut self, position: Duration) -> Result<()> {
        self.reopen()?;
        if position.is_zero() {
            return Ok(());
        }
        let target = (position.as_nanos() / u128::from(self.timestamp_scale))
            .min(u128::from(u64::MAX)) as u64;
        self.reader
            .seek(target.saturating_sub(1))
            .map_err(|error| Error::Media(format!("failed to seek Matroska file: {error}")))?;
        while let Some(sample) = self.next_inner()? {
            if sample.unit.pts >= position {
                self.pending_output = Some(sample);
                break;
            }
        }
        Ok(())
    }
}

type ParsedTrack = Option<(
    Codec,
    u32,
    Option<u8>,
    Option<std::sync::Arc<str>>,
    Option<VideoConfig>,
)>;

fn parse_wave_format(private: Option<&[u8]>) -> Result<ParsedTrack> {
    let Some(private) = private else {
        return Ok(None);
    };
    if private.len() < 16 {
        return Ok(None);
    }
    let tag = u16::from_le_bytes([private[0], private[1]]);
    let channels = u16::from_le_bytes([private[2], private[3]]);
    let sample_rate = u32::from_le_bytes(private[4..8].try_into().unwrap());
    if channels == 0 || channels > u16::from(u8::MAX) || sample_rate == 0 {
        return Ok(None);
    }
    let codec = match tag {
        6 => Codec::Pcma,
        7 => Codec::Pcmu,
        _ => return Ok(None),
    };
    Ok(Some((codec, sample_rate, Some(channels as u8), None, None)))
}
