//! Crash-safe segmented recording and seekable VOD playback.
use crate::{
    Error, Result,
    file::{DemuxedSample, Demuxer},
    media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription},
};
use bytes::Bytes;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;

const MAGIC: &[u8; 8] = b"RTSPSEG1";
const RECORD_MAGIC: &[u8; 4] = b"AU01";
const MAX_METADATA: usize = 64 * 1024;
static SEGMENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct RecordingConfig {
    pub directory: PathBuf,
    pub segment_duration: Duration,
    pub max_segment_bytes: u64,
    pub max_disk_bytes: u64,
    pub max_segments: usize,
    pub queue_capacity: usize,
    pub sync_on_finalize: bool,
}

impl RecordingConfig {
    pub fn validate(&self) -> Result<()> {
        if self.segment_duration == Duration::ZERO
            || self.max_segment_bytes == 0
            || self.max_disk_bytes < self.max_segment_bytes
            || self.max_segments == 0
            || self.queue_capacity == 0
        {
            return Err(Error::Limit("recording configuration"));
        }
        Ok(())
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("recordings"),
            segment_duration: Duration::from_secs(10),
            max_segment_bytes: 128 * 1024 * 1024,
            max_disk_bytes: 4 * 1024 * 1024 * 1024,
            max_segments: 10_000,
            queue_capacity: 256,
            sync_on_finalize: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordOutcome {
    Accepted,
    QueueFull,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecordingReport {
    pub accepted_units: u64,
    pub dropped_units: u64,
    pub written_bytes: u64,
    pub finalized_segments: u64,
}

enum Command {
    Unit(AccessUnit),
    Shutdown(oneshot::Sender<Result<RecordingReport>>),
}

struct Counters {
    accepted: AtomicU64,
    dropped: AtomicU64,
}

#[derive(Clone)]
pub struct RecordingSink {
    sender: mpsc::SyncSender<Command>,
    closed: Arc<AtomicBool>,
    counters: Arc<Counters>,
}

impl RecordingSink {
    pub fn try_record(&self, unit: AccessUnit) -> RecordOutcome {
        if self.closed.load(Ordering::Acquire) {
            return RecordOutcome::Closed;
        }
        match self.sender.try_send(Command::Unit(unit)) {
            Ok(()) => {
                self.counters.accepted.fetch_add(1, Ordering::Relaxed);
                RecordOutcome::Accepted
            }
            Err(mpsc::TrySendError::Full(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                RecordOutcome::QueueFull
            }
            Err(mpsc::TrySendError::Disconnected(_)) => RecordOutcome::Closed,
        }
    }
}

pub struct SegmentRecorder {
    sink: RecordingSink,
    abandoned: Arc<AtomicBool>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl SegmentRecorder {
    pub async fn start(config: RecordingConfig, description: MediaDescription) -> Result<Self> {
        config.validate()?;
        validate_description(&description)?;
        tokio::fs::create_dir_all(&config.directory).await?;
        let recovery_directory = config.directory.clone();
        tokio::task::spawn_blocking(move || RecordingCatalog::recover(recovery_directory))
            .await
            .map_err(|error| Error::Media(format!("recording recovery failed: {error}")))??;
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let closed = Arc::new(AtomicBool::new(false));
        let abandoned = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters {
            accepted: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let worker_counters = counters.clone();
        let worker_closed = closed.clone();
        let worker_abandoned = abandoned.clone();
        let worker = tokio::task::spawn_blocking(move || {
            run_writer(
                config,
                description,
                receiver,
                &worker_counters,
                &worker_abandoned,
            );
            worker_closed.store(true, Ordering::Release);
        });
        Ok(Self {
            sink: RecordingSink {
                sender,
                closed,
                counters,
            },
            abandoned,
            worker: Some(worker),
        })
    }

    pub fn sink(&self) -> RecordingSink {
        self.sink.clone()
    }

    pub async fn shutdown(mut self) -> Result<RecordingReport> {
        self.sink.closed.store(true, Ordering::Release);
        let (sender, receiver) = oneshot::channel();
        let command_sender = self.sink.sender.clone();
        tokio::task::spawn_blocking(move || command_sender.send(Command::Shutdown(sender)))
            .await
            .map_err(|error| Error::Media(format!("recording shutdown worker failed: {error}")))?
            .map_err(|_| Error::Closed)?;
        let report = receiver.await.map_err(|_| Error::Closed)??;
        if let Some(worker) = self.worker.take() {
            worker
                .await
                .map_err(|error| Error::Media(format!("recording worker failed: {error}")))?;
        }
        Ok(report)
    }
}

impl Drop for SegmentRecorder {
    fn drop(&mut self) {
        self.sink.closed.store(true, Ordering::Release);
        self.abandoned.store(true, Ordering::Release);
    }
}

fn run_writer(
    config: RecordingConfig,
    description: MediaDescription,
    receiver: mpsc::Receiver<Command>,
    counters: &Counters,
    abandoned: &AtomicBool,
) {
    let mut segment: Option<SegmentWriter> = None;
    let mut report = RecordingReport::default();
    loop {
        let command = match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) if !abandoned.load(Ordering::Acquire) => continue,
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(writer) = segment.take() {
                    let _ = writer.finalize(config.sync_on_finalize);
                }
                return;
            }
        };
        match command {
            Command::Unit(unit) => {
                let rotate = segment.as_ref().is_some_and(|writer| {
                    // A timestamp wrap (pts < start) must also force a cut,
                    // otherwise time-based rotation stops forever.
                    unit.pts < writer.start_pts
                        || unit.pts.saturating_sub(writer.start_pts) >= config.segment_duration
                        || writer.bytes >= config.max_segment_bytes
                });
                if rotate && let Some(writer) = segment.take() {
                    match writer.finalize(config.sync_on_finalize) {
                        Ok(bytes) => {
                            report.written_bytes = report.written_bytes.saturating_add(bytes);
                            report.finalized_segments += 1;
                            let _ = prune(&config);
                        }
                        Err(_) => report.dropped_units += 1,
                    }
                }
                if segment.is_none() {
                    match SegmentWriter::create(&config.directory, &description, unit.pts) {
                        Ok(writer) => segment = Some(writer),
                        Err(_) => {
                            report.dropped_units += 1;
                            continue;
                        }
                    }
                }
                if let Some(writer) = segment.as_mut()
                    && writer.write_unit(&unit).is_err()
                {
                    report.dropped_units += 1;
                    // A failed write leaves a truncated record inside the
                    // segment; continuing to append would poison the whole
                    // recording directory. Discard the segment and let the
                    // next unit start a fresh one.
                    if let Some(corrupt) = segment.take() {
                        corrupt.abort();
                    }
                }
            }
            Command::Shutdown(reply) => {
                if let Some(writer) = segment.take() {
                    match writer.finalize(config.sync_on_finalize) {
                        Ok(bytes) => {
                            report.written_bytes = report.written_bytes.saturating_add(bytes);
                            report.finalized_segments += 1;
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                            return;
                        }
                    }
                }
                let _ = prune(&config);
                report.accepted_units = counters.accepted.load(Ordering::Relaxed);
                report.dropped_units = report
                    .dropped_units
                    .saturating_add(counters.dropped.load(Ordering::Relaxed));
                let _ = reply.send(Ok(report));
                return;
            }
        }
    }
}

struct SegmentWriter {
    writer: BufWriter<File>,
    partial: PathBuf,
    final_path: PathBuf,
    start_pts: Duration,
    bytes: u64,
}

impl SegmentWriter {
    fn create(
        directory: &Path,
        description: &MediaDescription,
        start_pts: Duration,
    ) -> Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = SEGMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "segment-{stamp:020}-{sequence:016x}-{:016}.rtspseg",
            duration_us(start_pts)
        );
        let final_path = directory.join(name);
        let partial = final_path.with_extension("partial");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        let mut writer = BufWriter::new(file);
        if let Err(e) = write_description(&mut writer, description) {
            // Don't leave a partial file behind when the header write fails.
            drop(writer);
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
        let bytes = writer.stream_position()?;
        Ok(Self {
            writer,
            partial,
            final_path,
            start_pts,
            bytes,
        })
    }

    fn write_unit(&mut self, unit: &AccessUnit) -> Result<()> {
        let length =
            u32::try_from(unit.data.len()).map_err(|_| Error::Limit("recorded access unit"))?;
        self.writer.write_all(RECORD_MAGIC)?;
        self.writer.write_all(&[
            unit.track_id,
            codec_id(unit.codec),
            u8::from(unit.is_keyframe),
            0,
        ])?;
        self.writer
            .write_all(&duration_us(unit.pts).to_be_bytes())?;
        let dts = unit.dts.map(duration_us).unwrap_or(u64::MAX);
        self.writer.write_all(&dts.to_be_bytes())?;
        self.writer.write_all(&unit.rtp_timestamp.to_be_bytes())?;
        self.writer.write_all(&length.to_be_bytes())?;
        self.writer.write_all(&unit.data)?;
        self.bytes = self.bytes.saturating_add(32 + u64::from(length));
        Ok(())
    }

    fn finalize(mut self, sync: bool) -> Result<u64> {
        // Every failure point cleans up the partial file: a leftover
        // .partial would otherwise accumulate and confuse recovery.
        if let Err(e) = self.writer.flush() {
            let _ = std::fs::remove_file(&self.partial);
            return Err(e.into());
        }
        if sync && let Err(e) = self.writer.get_ref().sync_all() {
            let _ = std::fs::remove_file(&self.partial);
            return Err(e.into());
        }
        drop(self.writer);
        if let Err(e) = fs::rename(&self.partial, &self.final_path) {
            let _ = std::fs::remove_file(&self.partial);
            return Err(e.into());
        }
        if sync && let Some(parent) = self.final_path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(self.bytes)
    }

    /// Discard the in-progress segment and remove its partial file.
    fn abort(self) {
        drop(self.writer);
        let _ = std::fs::remove_file(&self.partial);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentInfo {
    pub path: PathBuf,
    pub start: Duration,
    pub end: Duration,
    pub bytes: u64,
    pub units: u64,
}

#[derive(Clone, Debug)]
pub struct RecordingCatalog {
    pub description: MediaDescription,
    pub segments: Vec<SegmentInfo>,
}

impl RecordingCatalog {
    pub fn recover(directory: impl AsRef<Path>) -> Result<usize> {
        let mut removed = 0;
        if !directory.as_ref().exists() {
            return Ok(0);
        }
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "partial")
            {
                fs::remove_file(path)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn scan(directory: impl AsRef<Path>, max_sample_size: usize) -> Result<Self> {
        let mut paths = fs::read_dir(directory)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "rtspseg")
            })
            .collect::<Vec<_>>();
        paths.sort();
        let mut description = None;
        let mut segments = Vec::new();
        for path in paths {
            // A corrupt segment must not make the whole directory VOD
            // unavailable; skip it and keep the rest.
            let (candidate, info) = match inspect_segment(&path, max_sample_size) {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        "recording scan: skipping corrupt segment {}: {e}",
                        path.display()
                    );
                    continue;
                }
            };
            if description
                .as_ref()
                .is_some_and(|current| current != &candidate)
            {
                return Err(Error::Media(
                    "recording segments have incompatible descriptions".into(),
                ));
            }
            description = Some(candidate);
            segments.push(info);
        }
        let description =
            description.ok_or_else(|| Error::NotFound("recording segments".into()))?;
        Ok(Self {
            description,
            segments,
        })
    }

    pub fn duration(&self) -> Duration {
        match (self.segments.first(), self.segments.last()) {
            (Some(first), Some(last)) => last.end.saturating_sub(first.start),
            _ => Duration::ZERO,
        }
    }
}

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

fn normalize_unit(mut unit: AccessUnit, origin: Duration) -> AccessUnit {
    unit.pts = unit.pts.saturating_sub(origin);
    unit.dts = unit.dts.map(|value| value.saturating_sub(origin));
    unit
}

struct SegmentReader {
    reader: BufReader<File>,
    max_sample_size: usize,
}

impl SegmentReader {
    fn open(path: &Path, max_sample_size: usize) -> Result<(MediaDescription, Self)> {
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

    fn next_unit(&mut self) -> Result<Option<AccessUnit>> {
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

fn inspect_segment(path: &Path, max_sample_size: usize) -> Result<(MediaDescription, SegmentInfo)> {
    let (description, mut reader) = SegmentReader::open(path, max_sample_size)?;
    let mut start = None;
    let mut end = Duration::ZERO;
    let mut units = 0;
    while let Some(unit) = reader.next_unit()? {
        start.get_or_insert(unit.pts);
        end = end.max(unit.pts);
        units += 1;
    }
    Ok((
        description,
        SegmentInfo {
            path: path.to_owned(),
            start: start.unwrap_or_default(),
            end,
            bytes: fs::metadata(path)?.len(),
            units,
        },
    ))
}

fn prune(config: &RecordingConfig) -> Result<()> {
    let catalog = match RecordingCatalog::scan(&config.directory, usize::MAX) {
        Ok(catalog) => catalog,
        Err(e) => {
            // A failed scan must not silently disable disk-limit enforcement.
            tracing::warn!("recording prune: catalog scan failed: {e}");
            return Ok(());
        }
    };
    let mut bytes = catalog
        .segments
        .iter()
        .map(|segment| segment.bytes)
        .sum::<u64>();
    let mut count = catalog.segments.len();
    for segment in catalog.segments {
        if bytes <= config.max_disk_bytes && count <= config.max_segments {
            break;
        }
        if let Err(e) = fs::remove_file(&segment.path) {
            tracing::warn!(
                "recording prune: failed to remove {}: {e}",
                segment.path.display()
            );
        }
        bytes = bytes.saturating_sub(segment.bytes);
        count -= 1;
    }
    Ok(())
}

fn write_description(writer: &mut impl Write, description: &MediaDescription) -> Result<()> {
    writer.write_all(MAGIC)?;
    let name = description.session_name.as_bytes();
    write_len(writer, name.len())?;
    writer.write_all(name)?;
    let tracks =
        u16::try_from(description.tracks.len()).map_err(|_| Error::Limit("recording tracks"))?;
    writer.write_all(&tracks.to_be_bytes())?;
    for track in &description.tracks {
        writer.write_all(&[
            track.id,
            kind_id(track.kind),
            codec_id(track.codec),
            track.channels.unwrap_or(u8::MAX),
            track.payload_type,
        ])?;
        writer.write_all(&track.clock_rate.to_be_bytes())?;
        write_bytes(writer, track.fmtp.as_deref().unwrap_or("").as_bytes())?;
        write_bytes(writer, track.control.as_bytes())?;
    }
    Ok(())
}

fn read_description(reader: &mut impl Read) -> Result<MediaDescription> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(Error::Media("invalid recording segment magic".into()));
    }
    let session_name = read_string(reader)?;
    let tracks = read_u16(reader)?;
    let mut descriptions = Vec::with_capacity(usize::from(tracks));
    for _ in 0..tracks {
        let mut fixed = [0u8; 9];
        reader.read_exact(&mut fixed)?;
        let fmtp = read_string(reader)?;
        let control = read_string(reader)?;
        descriptions.push(TrackDescription {
            id: fixed[0],
            kind: kind_from_id(fixed[1])?,
            codec: codec_from_id(fixed[2])?,
            channels: (fixed[3] != u8::MAX).then_some(fixed[3]),
            payload_type: fixed[4],
            clock_rate: u32::from_be_bytes([fixed[5], fixed[6], fixed[7], fixed[8]]),
            fmtp: (!fmtp.is_empty()).then(|| Arc::from(fmtp)),
            control: Arc::from(control),
            width: None,
            height: None,
            frame_rate: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        });
    }
    Ok(MediaDescription {
        session_name: Arc::from(session_name),
        tracks: descriptions,
        #[cfg(feature = "multicast")]
        connection: None,
    })
}

fn write_bytes(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    write_len(writer, bytes.len())?;
    writer.write_all(bytes)?;
    Ok(())
}
fn write_len(writer: &mut impl Write, length: usize) -> Result<()> {
    let length = u16::try_from(length).map_err(|_| Error::Limit("recording metadata"))?;
    writer.write_all(&length.to_be_bytes())?;
    Ok(())
}
fn read_u16(reader: &mut impl Read) -> Result<u16> {
    let mut value = [0; 2];
    reader.read_exact(&mut value)?;
    Ok(u16::from_be_bytes(value))
}
fn read_string(reader: &mut impl Read) -> Result<String> {
    let length = usize::from(read_u16(reader)?);
    if length > MAX_METADATA {
        return Err(Error::Limit("recording metadata"));
    }
    let mut value = vec![0; length];
    reader.read_exact(&mut value)?;
    String::from_utf8(value).map_err(|_| Error::Media("recording metadata is not UTF-8".into()))
}
fn duration_us(value: Duration) -> u64 {
    u64::try_from(value.as_micros()).unwrap_or(u64::MAX)
}
fn duration_from_us(value: u64) -> Duration {
    Duration::from_micros(value)
}
fn codec_id(codec: Codec) -> u8 {
    match codec {
        Codec::H264 => 1,
        Codec::H265 => 2,
        Codec::Aac => 3,
        Codec::Pcma => 4,
        Codec::Pcmu => 5,
        Codec::Opus => 6,
        Codec::Vp8 => 7,
        Codec::Vp9 => 8,
    }
}
fn codec_from_id(id: u8) -> Result<Codec> {
    match id {
        1 => Ok(Codec::H264),
        2 => Ok(Codec::H265),
        3 => Ok(Codec::Aac),
        4 => Ok(Codec::Pcma),
        5 => Ok(Codec::Pcmu),
        6 => Ok(Codec::Opus),
        7 => Ok(Codec::Vp8),
        8 => Ok(Codec::Vp9),
        _ => Err(Error::Media("unknown recorded codec".into())),
    }
}
fn kind_id(kind: MediaKind) -> u8 {
    match kind {
        MediaKind::Video => 1,
        MediaKind::Audio => 2,
    }
}
fn kind_from_id(id: u8) -> Result<MediaKind> {
    match id {
        1 => Ok(MediaKind::Video),
        2 => Ok(MediaKind::Audio),
        _ => Err(Error::Media("unknown recorded media kind".into())),
    }
}
fn validate_description(description: &MediaDescription) -> Result<()> {
    if description.tracks.is_empty() {
        Err(Error::Media("recording requires at least one track".into()))
    } else {
        Ok(())
    }
}
