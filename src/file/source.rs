#[cfg(feature = "mkv")]
use super::matroska::MatroskaDemuxer;
#[cfg(feature = "mp4")]
use super::mp4::Mp4Demuxer;
use super::{mpeg_ts::MpegTsDemuxer, raw};
#[cfg(feature = "server")]
use crate::mount::{LocalPublisher, PushResult};
use crate::{
    Error, Result,
    media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription},
    pipeline::{ElementId, IngressOutcome, PipelineIngress},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawFormat {
    H264,
    H265,
    AacAdts,
    Pcma,
    Pcmu,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerFormat {
    Auto,
    #[cfg(feature = "mp4")]
    Mp4,
    #[cfg(feature = "mkv")]
    Matroska,
    MpegTs,
    Wave,
    Raw(RawFormat),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopMode {
    Once,
    /// Play once, then repeat this many additional times.
    Repeat(u32),
    Forever,
}

#[derive(Clone, Debug)]
pub struct FileSourceOptions {
    pub format: ContainerFormat,
    pub loop_mode: LoopMode,
    pub playback_rate: f64,
    pub start_position: Duration,
    pub pause_when_idle: bool,
    pub prefetch: usize,
    pub max_sample_size: usize,
    pub raw_video_frame_rate: u32,
    pub raw_g711_sample_rate: u32,
    pub raw_g711_channels: u8,
    pub raw_g711_frame_duration: Duration,
    /// Which audio track to expose (0-based index among audio tracks).
    /// `None` (default) exposes all audio tracks.
    pub audio_track: Option<usize>,
}

impl Default for FileSourceOptions {
    fn default() -> Self {
        Self {
            format: ContainerFormat::Auto,
            loop_mode: LoopMode::Once,
            playback_rate: 1.0,
            start_position: Duration::ZERO,
            pause_when_idle: true,
            prefetch: 8,
            max_sample_size: 8 * 1024 * 1024,
            raw_video_frame_rate: 25,
            raw_g711_sample_rate: 8_000,
            raw_g711_channels: 1,
            raw_g711_frame_duration: Duration::from_millis(20),
            audio_track: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DemuxedSample {
    pub unit: AccessUnit,
    pub duration: Option<Duration>,
}

pub trait Demuxer: Send {
    fn description(&self) -> &MediaDescription;
    fn duration(&self) -> Option<Duration>;
    fn next_sample(&mut self) -> Result<Option<DemuxedSample>>;
    fn seek(&mut self, position: Duration) -> Result<()>;
}

pub struct FileSource {
    path: PathBuf,
    options: FileSourceOptions,
    demuxer: Box<dyn Demuxer>,
    /// Description with the configured audio-track selection applied.
    filtered: std::sync::OnceLock<MediaDescription>,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>, options: FileSourceOptions) -> Result<Self> {
        validate_options(&options)?;
        let path = path.as_ref().to_owned();
        let format = detect_format(&path, options.format)?;
        let demuxer: Box<dyn Demuxer> = match format {
            ContainerFormat::Auto => unreachable!(),
            #[cfg(feature = "mp4")]
            ContainerFormat::Mp4 => Box::new(Mp4Demuxer::open(&path, options.max_sample_size)?),
            #[cfg(feature = "mkv")]
            ContainerFormat::Matroska => {
                Box::new(MatroskaDemuxer::open(&path, options.max_sample_size)?)
            }
            ContainerFormat::MpegTs => {
                Box::new(MpegTsDemuxer::open(&path, options.max_sample_size)?)
            }
            ContainerFormat::Wave => Box::new(raw::WavDemuxer::open(
                &path,
                options.raw_g711_frame_duration,
                options.max_sample_size,
            )?),
            ContainerFormat::Raw(format) => raw::open_raw(&path, format, &options)?,
        };
        Ok(Self {
            path,
            options,
            demuxer,
            filtered: std::sync::OnceLock::new(),
        })
    }

    /// Open and parse file metadata without blocking an async runtime worker.
    pub async fn open_async(path: impl AsRef<Path>, options: FileSourceOptions) -> Result<Self> {
        let path = path.as_ref().to_owned();
        tokio::task::spawn_blocking(move || Self::open(path, options))
            .await
            .map_err(|error| Error::Media(format!("file open worker failed: {error}")))?
    }

    pub fn from_demuxer(
        name: impl Into<PathBuf>,
        demuxer: Box<dyn Demuxer>,
        options: FileSourceOptions,
    ) -> Result<Self> {
        validate_options(&options)?;
        Ok(Self {
            path: name.into(),
            options,
            demuxer,
            filtered: std::sync::OnceLock::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Media description with the configured audio-track selection applied
    /// (all video tracks are kept; audio keeps only `options.audio_track`
    /// when set).
    pub fn description(&self) -> &MediaDescription {
        self.filtered.get_or_init(|| {
            let mut description = self.demuxer.description().clone();
            if let Some(selected) = self.options.audio_track {
                let mut audio_seen = 0usize;
                description.tracks.retain(|track| {
                    if track.kind == crate::media::MediaKind::Audio {
                        let keep = audio_seen == selected;
                        audio_seen += 1;
                        keep
                    } else {
                        true
                    }
                });
                // Rebuild track ids to stay contiguous.
                for (index, track) in description.tracks.iter_mut().enumerate() {
                    track.id = index as u8;
                }
            }
            description
        })
    }

    pub fn duration(&self) -> Option<Duration> {
        self.demuxer.duration()
    }

    /// Consume the source and expose its demuxer for custom pipeline wiring.
    pub fn into_demuxer(self) -> Box<dyn Demuxer> {
        self.demuxer
    }

    #[cfg(feature = "server")]
    pub fn play(self, publisher: LocalPublisher) -> FilePlayback {
        self.play_target(PlaybackTarget::Publisher(publisher))
    }

    /// Pace and inject the file into an existing Element DAG ingress.
    pub fn play_into_pipeline(self, ingress: PipelineIngress, target: ElementId) -> FilePlayback {
        self.play_target(PlaybackTarget::Pipeline { ingress, target })
    }

    fn play_target(self, target: PlaybackTarget) -> FilePlayback {
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move { self.run(target, task_cancel).await });
        FilePlayback {
            cancel,
            task: Some(task),
        }
    }

    async fn run(self, target: PlaybackTarget, cancel: CancellationToken) -> Result<PlaybackStats> {
        let Self {
            options, demuxer, ..
        } = self;
        let (tx, mut rx) = mpsc::channel(options.prefetch);
        let worker_cancel = cancel.clone();
        let worker_options = options.clone();
        let worker = tokio::task::spawn_blocking(move || {
            demux_worker(demuxer, worker_options, worker_cancel, tx)
        });
        let mut stats = PlaybackStats::default();
        let mut generation = None;
        let mut wall_origin = Instant::now();
        let mut media_origin = Duration::ZERO;
        let mut was_idle = false;
        while let Some(message) = tokio::select! {
            _ = cancel.cancelled() => None,
            message = rx.recv() => message,
        } {
            let WorkerMessage::Sample {
                generation: sample_generation,
                sample,
            } = message
            else {
                stats.loops_completed += 1;
                continue;
            };
            if generation != Some(sample_generation) {
                generation = Some(sample_generation);
                wall_origin = Instant::now();
                media_origin = schedule_time(&sample.unit);
            }
            if options.pause_when_idle {
                while target.consumer_count() == 0 {
                    if target.is_closed() {
                        cancel.cancel();
                        break;
                    }
                    was_idle = true;
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                    }
                }
                if cancel.is_cancelled() {
                    break;
                }
                if was_idle {
                    wall_origin = Instant::now();
                    media_origin = schedule_time(&sample.unit);
                    was_idle = false;
                }
            }
            let media_elapsed = schedule_time(&sample.unit).saturating_sub(media_origin);
            let delay_seconds = media_elapsed.as_secs_f64() / options.playback_rate;
            if !delay_seconds.is_finite() || delay_seconds > u64::MAX as f64 {
                return Err(Error::Limit("file playback deadline"));
            }
            let delay = Duration::from_secs_f64(delay_seconds);
            let deadline = wall_origin
                .checked_add(delay)
                .ok_or(Error::Limit("file playback deadline"))?;
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep_until(deadline) => {}
            }
            if target.is_closed() {
                cancel.cancel();
                break;
            }
            if target.try_push(sample.unit)? {
                stats.accepted_samples += 1;
            } else {
                stats.dropped_samples += 1;
            }
            stats.bytes += sample.sample_size as u64;
        }
        cancel.cancel();
        drop(rx);
        match worker.await {
            Ok(result) => result?,
            Err(error) if error.is_cancelled() => {}
            Err(error) => {
                return Err(Error::Media(format!("file demux worker failed: {error}")));
            }
        }
        Ok(stats)
    }
}

enum PlaybackTarget {
    #[cfg(feature = "server")]
    Publisher(LocalPublisher),
    Pipeline {
        ingress: PipelineIngress,
        target: ElementId,
    },
}

impl PlaybackTarget {
    fn consumer_count(&self) -> usize {
        match self {
            #[cfg(feature = "server")]
            Self::Publisher(publisher) => publisher.subscriber_count(),
            Self::Pipeline { .. } => 1,
        }
    }

    fn is_closed(&self) -> bool {
        match self {
            #[cfg(feature = "server")]
            Self::Publisher(publisher) => publisher.is_closed(),
            Self::Pipeline { .. } => false,
        }
    }

    fn try_push(&self, unit: AccessUnit) -> Result<bool> {
        match self {
            #[cfg(feature = "server")]
            Self::Publisher(publisher) => {
                Ok(matches!(publisher.try_push(unit)?, PushResult::Accepted))
            }
            Self::Pipeline { ingress, target } => Ok(matches!(
                ingress.try_push(*target, unit)?,
                IngressOutcome::Accepted
            )),
        }
    }
}

enum WorkerMessage {
    Sample {
        generation: u32,
        sample: SizedSample,
    },
    LoopComplete,
}

struct SizedSample {
    sample_size: usize,
    unit: AccessUnit,
}

fn demux_worker(
    mut demuxer: Box<dyn Demuxer>,
    options: FileSourceOptions,
    cancel: CancellationToken,
    tx: mpsc::Sender<WorkerMessage>,
) -> Result<()> {
    let mut generation = 0u32;
    loop {
        demuxer.seek(options.start_position)?;
        let mut samples_this_round = 0u64;
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let Some(sample) = demuxer.next_sample()? else {
                break;
            };
            samples_this_round = samples_this_round.saturating_add(1);
            let sample_size = sample.unit.data.len();
            if sample_size > options.max_sample_size {
                return Err(Error::Limit("file media sample size"));
            }
            if tx
                .blocking_send(WorkerMessage::Sample {
                    generation,
                    sample: SizedSample {
                        sample_size,
                        unit: sample.unit,
                    },
                })
                .is_err()
            {
                return Ok(());
            }
        }
        if tx.blocking_send(WorkerMessage::LoopComplete).is_err() {
            return Ok(());
        }
        let repeat = match options.loop_mode {
            LoopMode::Once => false,
            LoopMode::Repeat(count) => generation < count,
            LoopMode::Forever => true,
        };
        if !repeat || cancel.is_cancelled() {
            return Ok(());
        }
        if samples_this_round == 0 {
            // Empty media (or a start position past the end) would otherwise
            // busy-spin the worker at 100% CPU forever.
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        generation = generation
            .checked_add(1)
            .ok_or(Error::Limit("file playback loop counter"))?;
    }
}

fn schedule_time(unit: &AccessUnit) -> Duration {
    unit.dts.unwrap_or(unit.pts)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaybackStats {
    pub accepted_samples: u64,
    pub dropped_samples: u64,
    pub bytes: u64,
    pub loops_completed: u64,
}

pub struct FilePlayback {
    cancel: CancellationToken,
    task: Option<JoinHandle<Result<PlaybackStats>>>,
}

impl FilePlayback {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub async fn wait(mut self) -> Result<PlaybackStats> {
        let task = self.task.take().ok_or(Error::Closed)?;
        task.await
            .map_err(|error| Error::Media(format!("file playback task failed: {error}")))?
    }

    pub async fn stop(mut self) -> Result<PlaybackStats> {
        self.cancel.cancel();
        let task = self.task.take().ok_or(Error::Closed)?;
        task.await
            .map_err(|error| Error::Media(format!("file playback task failed: {error}")))?
    }
}

impl Drop for FilePlayback {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn validate_options(options: &FileSourceOptions) -> Result<()> {
    if !options.playback_rate.is_finite() || !(0.01..=100.0).contains(&options.playback_rate) {
        return Err(Error::Media(
            "file playback rate must be between 0.01 and 100".into(),
        ));
    }
    if options.prefetch == 0 {
        return Err(Error::Limit("file prefetch queue"));
    }
    if options.max_sample_size == 0 {
        return Err(Error::Limit("file media sample size"));
    }
    if options.raw_video_frame_rate == 0 {
        return Err(Error::Media("raw video frame rate must be positive".into()));
    }
    if options.raw_g711_sample_rate == 0
        || options.raw_g711_channels == 0
        || options.raw_g711_frame_duration.is_zero()
    {
        return Err(Error::Media("invalid raw G.711 audio parameters".into()));
    }
    Ok(())
}

fn detect_format(path: &Path, requested: ContainerFormat) -> Result<ContainerFormat> {
    if requested != ContainerFormat::Auto {
        return Ok(requested);
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let by_extension = match extension.as_str() {
        #[cfg(feature = "mp4")]
        "mp4" | "m4v" | "m4a" | "mov" => Some(ContainerFormat::Mp4),
        #[cfg(feature = "mkv")]
        "mkv" | "mka" | "webm" => Some(ContainerFormat::Matroska),
        "ts" | "m2ts" | "mts" => Some(ContainerFormat::MpegTs),
        "wav" | "wave" => Some(ContainerFormat::Wave),
        "h264" | "264" | "avc" => Some(ContainerFormat::Raw(RawFormat::H264)),
        "h265" | "265" | "hevc" => Some(ContainerFormat::Raw(RawFormat::H265)),
        "aac" | "adts" => Some(ContainerFormat::Raw(RawFormat::AacAdts)),
        "pcma" | "alaw" | "g711a" => Some(ContainerFormat::Raw(RawFormat::Pcma)),
        "pcmu" | "ulaw" | "mulaw" | "g711u" => Some(ContainerFormat::Raw(RawFormat::Pcmu)),
        _ => None,
    };
    by_extension.ok_or_else(|| {
        Error::Media(format!(
            "cannot infer file format from {}; set FileSourceOptions::format",
            path.display()
        ))
    })
}

pub(crate) fn duration_from_ticks(value: u64, rate: u64) -> Duration {
    if rate == 0 {
        return Duration::ZERO;
    }
    let seconds = value / rate;
    let nanos = ((value % rate) as u128 * 1_000_000_000u128 / rate as u128) as u32;
    Duration::new(seconds, nanos)
}

pub(crate) fn rtp_timestamp(pts: Duration, clock_rate: u32) -> u32 {
    let ticks = pts
        .as_secs()
        .wrapping_mul(u64::from(clock_rate))
        .wrapping_add(u64::from(pts.subsec_nanos()) * u64::from(clock_rate) / 1_000_000_000);
    ticks as u32
}

pub(crate) fn track_description(
    id: u8,
    codec: Codec,
    clock_rate: u32,
    channels: Option<u8>,
    fmtp: Option<Arc<str>>,
) -> TrackDescription {
    let payload_type = match codec {
        Codec::Pcmu => 0,
        Codec::Pcma => 8,
        Codec::Opus => 111,
        Codec::H264 | Codec::H265 | Codec::Aac | Codec::Vp8 | Codec::Vp9 => 96u8.saturating_add(id),
    };
    TrackDescription {
        id,
        kind: if matches!(codec, Codec::H264 | Codec::H265) {
            MediaKind::Video
        } else {
            MediaKind::Audio
        },
        codec,
        clock_rate,
        channels,
        payload_type,
        fmtp,
        control: Arc::from(format!("trackID={id}")),
        width: None,
        height: None,
        frame_rate: None,
        #[cfg(feature = "srtp")]
        crypto: None,
    }
}

pub(crate) fn file_description(name: &str, tracks: Vec<TrackDescription>) -> MediaDescription {
    MediaDescription {
        session_name: Arc::from(name),
        tracks,
        #[cfg(feature = "multicast")]
        connection: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_detection_and_timestamps() {
        assert_eq!(
            detect_format(Path::new("movie.MOV"), ContainerFormat::Auto).unwrap(),
            ContainerFormat::Mp4
        );
        assert_eq!(rtp_timestamp(Duration::from_millis(500), 90_000), 45_000);
        assert_eq!(
            duration_from_ticks(45_000, 90_000),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn rejects_invalid_options() {
        let options = FileSourceOptions {
            playback_rate: 0.0,
            ..Default::default()
        };
        assert!(validate_options(&options).is_err());
    }
}
