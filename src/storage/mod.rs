//! Persistent media storage, recording, indexing and VOD sources.
mod recording;

use crate::{
    Result,
    media::{MediaDescription, demux::Demuxer},
};

pub use crate::media::vod::VodSource;

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
}

impl std::fmt::Debug for dyn VodSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("VodSource")
    }
}
