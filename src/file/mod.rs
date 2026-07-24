//! Encoded file sources and container demuxers.
//!
//! The file layer only demuxes and normalizes encoded access units. It never
//! decodes or transcodes, so the live-source RTP path remains unchanged.

mod bitstream;
mod matroska;
mod mp4;
mod mpeg_ts;
mod raw;
mod source;

pub use source::{
    ContainerFormat, DemuxedSample, Demuxer, FilePlayback, FileSource, FileSourceOptions, LoopMode,
    PlaybackStats, RawFormat,
};
