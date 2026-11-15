//! Crash-safe segmented recording and seekable VOD playback.
use rtflow_core::error::{Error, Result};
use rtflow_file::{DemuxedSample, Demuxer};
use rtflow_media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};
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

pub(super) const MAGIC: &[u8; 8] = b"RTSPSEG1";
pub(super) const RECORD_MAGIC: &[u8; 4] = b"AU01";
pub(super) const MAX_METADATA: usize = 64 * 1024;
pub(super) static SEGMENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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

mod catalog;
mod format;
mod reader;
mod sink;

pub use catalog::{RecordingCatalog, SegmentInfo};
pub use reader::RecordingDemuxer;
pub use sink::{RecordingSink, SegmentRecorder};
