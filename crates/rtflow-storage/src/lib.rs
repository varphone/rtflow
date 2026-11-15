//! Persistent media storage, recording, indexing and VOD sources.
mod recording;

use rtflow_core::error::Result;
use rtflow_media::{MediaDescription, demux::Demuxer};

pub use rtflow_media::vod::VodSource;

pub use recording::{
    RecordOutcome, RecordingCatalog, RecordingConfig, RecordingDemuxer, RecordingReport,
    RecordingSink, SegmentInfo, SegmentRecorder,
};

#[derive(Clone)]
pub struct RecordingVodSource {
    directory: std::path::PathBuf,
    description: MediaDescription,
    max_sample_size: usize,
}

impl RecordingVodSource {
    pub fn open(directory: impl AsRef<std::path::Path>, max_sample_size: usize) -> Result<Self> {
        let directory = directory.as_ref().to_owned();
        let catalog = RecordingCatalog::scan(&directory, max_sample_size)?;
        Ok(Self {
            directory,
            description: catalog.description,
            max_sample_size,
        })
    }
}

impl VodSource for RecordingVodSource {
    fn description(&self) -> &MediaDescription {
        &self.description
    }
    fn open(&self) -> Result<Box<dyn Demuxer>> {
        Ok(Box::new(RecordingDemuxer::open(
            &self.directory,
            self.max_sample_size,
        )?))
    }

    fn fingerprint(&self) -> Option<String> {
        let mut entries: Vec<(String, u64, u128)> = std::fs::read_dir(&self.directory)
            .ok()?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let metadata = entry.metadata().ok()?;
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .unwrap_or_default();
                Some((
                    entry.file_name().to_string_lossy().into_owned(),
                    metadata.len(),
                    modified,
                ))
            })
            .collect();
        entries.sort();
        Some(format!("{}:{entries:?}", self.directory.display()))
    }
}


