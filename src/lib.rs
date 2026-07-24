#![forbid(unsafe_code)]
//! Async RTSP/RTP primitives for embedded media gateways.
//!
//! Encoded [`AccessUnit`] values travel through the pipeline; the crate never
//! decodes or transcodes media, so hardware codecs remain outside the hot path.
#[path = "protocol/auth.rs"]
pub mod auth;
pub mod client;
pub mod file;
#[path = "media/types.rs"]
pub mod media;
#[path = "media/pipeline.rs"]
pub mod pipeline;
pub mod proxy;
pub mod rtcp;
pub mod rtp;
#[path = "protocol/rtsp.rs"]
pub mod rtsp;
pub mod runtime;
#[path = "protocol/transport.rs"]
pub mod transport;
// Stable expansion-oriented facades.
pub mod protocol;
pub mod media_pipeline {
    pub use crate::pipeline::*;
    pub use crate::rtp::*;
}
pub mod server;
pub use auth::{AccessRole, AuthDecision, Authenticator, Credentials};
pub use file::{
    ContainerFormat, DemuxedSample, Demuxer, FilePlayback, FileSource, FileSourceOptions, LoopMode,
    PlaybackStats, RawFormat,
};
pub use media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};
pub use server::{LocalPublisher, MountConfig, MountHandle, RtspServer, ServerConfig};
use thiserror::Error;
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid RTSP message: {0}")]
    Rtsp(String),
    #[error("invalid media payload: {0}")]
    Media(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session closed")]
    Closed,
    #[error("resource limit exceeded: {0}")]
    Limit(&'static str),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("authentication failed")]
    Unauthorized,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;
