//! Video-on-demand source abstraction.
//!
//! A [`VodSource`] opens an independent [`Demuxer`] per playback session, which
//! is what `MountConfig::Vod` needs. The trait lives with the media types so
//! the mount registry does not have to depend on the storage implementation.

use super::{MediaDescription, demux::Demuxer};
use crate::Result;

/// Factory for per-session VOD playback cursors.
pub trait VodSource: Send + Sync + 'static {
    fn description(&self) -> &MediaDescription;
    fn open(&self) -> Result<Box<dyn Demuxer>>;
}
