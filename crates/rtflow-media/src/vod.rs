//! Video-on-demand source abstraction.
//!
//! A [`VodSource`] opens an independent [`Demuxer`] per playback session, which
//! is what `MountConfig::Vod` needs. The trait lives with the media types so
//! the mount registry does not have to depend on the storage implementation.

use super::{MediaDescription, demux::Demuxer};
use rtflow_core::error::Result;

/// Factory for per-session VOD playback cursors.
pub trait VodSource: Send + Sync + 'static {
    fn description(&self) -> &MediaDescription;
    fn open(&self) -> Result<Box<dyn Demuxer>>;

    /// Stable fingerprint of the source contents (mtime/size, catalog
    /// generation). `None` keeps the existing cache forever; sources that can
    /// change should return a value so the HLS cache is regenerated.
    fn fingerprint(&self) -> Option<String> {
        None
    }
}

impl std::fmt::Debug for dyn VodSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("VodSource")
    }
}
