use super::catalog::prune;
use super::format::*;
use super::*;

pub(super) enum Command {
    Unit(AccessUnit),
    Shutdown(oneshot::Sender<Result<RecordingReport>>),
}

pub(super) struct Counters {
    accepted: AtomicU64,
    dropped: AtomicU64,
    segments: AtomicU64,
    finalized_bytes: AtomicU64,
    current_bytes: AtomicU64,
}

impl Counters {
    fn finish_segment(&self, bytes: u64) {
        self.finalized_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.segments.fetch_add(1, Ordering::Relaxed);
        self.current_bytes.store(0, Ordering::Relaxed);
    }
}

/// Live progress of an active recording.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecordingProgress {
    pub segments: u64,
    pub bytes: u64,
    pub accepted_units: u64,
    pub dropped_units: u64,
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
            segments: AtomicU64::new(0),
            finalized_bytes: AtomicU64::new(0),
            current_bytes: AtomicU64::new(0),
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

    pub fn progress(&self) -> RecordingProgress {
        let counters = &self.sink.counters;
        RecordingProgress {
            segments: counters.segments.load(Ordering::Relaxed),
            bytes: counters.finalized_bytes.load(Ordering::Relaxed)
                + counters.current_bytes.load(Ordering::Relaxed),
            accepted_units: counters.accepted.load(Ordering::Relaxed),
            dropped_units: counters.dropped.load(Ordering::Relaxed),
        }
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

pub(super) fn run_writer(
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
                            counters.finish_segment(bytes);
                            let _ = prune(&config);
                        }
                        Err(_) => report.dropped_units += 1,
                    }
                }
                if segment.is_none() {
                    match SegmentWriter::create(&config.directory, &description, unit.pts) {
                        Ok(writer) => {
                            counters
                                .current_bytes
                                .store(writer.bytes, Ordering::Relaxed);
                            segment = Some(writer);
                        }
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
                        counters.current_bytes.store(0, Ordering::Relaxed);
                    }
                } else if let Some(writer) = segment.as_ref() {
                    counters
                        .current_bytes
                        .store(writer.bytes, Ordering::Relaxed);
                }
            }
            Command::Shutdown(reply) => {
                if let Some(writer) = segment.take() {
                    match writer.finalize(config.sync_on_finalize) {
                        Ok(bytes) => {
                            report.written_bytes = report.written_bytes.saturating_add(bytes);
                            report.finalized_segments += 1;
                            counters.finish_segment(bytes);
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

pub(super) struct SegmentWriter {
    pub(super) writer: BufWriter<File>,
    pub(super) partial: PathBuf,
    pub(super) final_path: PathBuf,
    pub(super) start_pts: Duration,
    pub(super) bytes: u64,
}

impl SegmentWriter {
    pub(super) fn create(
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

    pub(super) fn write_unit(&mut self, unit: &AccessUnit) -> Result<()> {
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

    pub(super) fn finalize(mut self, sync: bool) -> Result<u64> {
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
    pub(super) fn abort(self) {
        drop(self.writer);
        let _ = std::fs::remove_file(&self.partial);
    }
}
