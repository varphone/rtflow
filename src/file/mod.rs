//! Encoded file sources and container demuxers.
//!
//! The file layer only demuxes and normalizes encoded access units. It never
//! decodes or transcodes, so the live-source RTP path remains unchanged.

mod bitstream;
#[cfg(feature = "mkv")]
mod matroska;
#[cfg(feature = "mp4")]
mod mp4;
mod mpeg_ts;
mod raw;
mod source;

pub use crate::media::demux::{DemuxedSample, Demuxer};
pub use source::{
    ContainerFormat, FilePlayback, FileSource, FileSourceOptions, LoopMode, PlaybackStats,
    RawFormat,
};
