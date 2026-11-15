//! Container-agnostic demuxing abstractions.
//!
//! [`Demuxer`] is implemented by the concrete container readers in
//! `rtflow_file` and consumed by file playback, recording and VOD sources, so
//! the trait lives with the media value types instead of the file layer.

use super::MediaDescription;
use crate::AccessUnit;
use rtflow_core::error::Result;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct DemuxedSample {
    pub unit: AccessUnit,
    pub duration: Option<Duration>,
}

/// Sequential reader over the encoded samples of one container.
pub trait Demuxer: Send {
    fn description(&self) -> &MediaDescription;
    fn duration(&self) -> Option<Duration>;
    fn next_sample(&mut self) -> Result<Option<DemuxedSample>>;
    fn seek(&mut self, position: Duration) -> Result<()>;
}
