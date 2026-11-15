//! Adapter from a regular media file to the per-session `VodSource` trait.
use crate::{FileSource, FileSourceOptions};
use rtflow_core::error::Result;
use rtflow_media::{MediaDescription, demux::Demuxer, vod::VodSource};
use std::path::{Path, PathBuf};

/// A `VodSource` over one MP4/MKV/TS/raw file. Each `open()` creates a fresh
/// demuxer so concurrent VOD sessions do not share a cursor.
#[derive(Clone)]
pub struct FileVodSource {
    path: PathBuf,
    options: FileSourceOptions,
    description: MediaDescription,
}

impl FileVodSource {
    pub fn open(path: impl AsRef<Path>, options: FileSourceOptions) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let source = FileSource::open(&path, options.clone())?;
        let description = source.description().clone();
        Ok(Self {
            path,
            options,
            description,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl VodSource for FileVodSource {
    fn description(&self) -> &MediaDescription {
        &self.description
    }

    fn open(&self) -> Result<Box<dyn Demuxer>> {
        Ok(FileSource::open(&self.path, self.options.clone())?.into_demuxer())
    }

    fn fingerprint(&self) -> Option<String> {
        let metadata = std::fs::metadata(&self.path).ok()?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Some(format!(
            "{}:{}:{}",
            self.path.display(),
            metadata.len(),
            modified
        ))
    }
}
