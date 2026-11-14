//! Adapter from a regular media file to the per-session `VodSource` trait.
use crate::{
    Result,
    file::{FileSource, FileSourceOptions},
    media::{MediaDescription, demux::Demuxer, vod::VodSource},
};
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
}
