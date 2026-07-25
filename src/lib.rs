#![forbid(unsafe_code)]
//! Async RTSP/RTP primitives for embedded media gateways.
//!
//! Encoded [`AccessUnit`] values travel through the pipeline; the crate never
//! decodes or transcodes media, so hardware codecs remain outside the hot path.
#[path = "protocol/auth.rs"]
pub mod auth;
pub mod client;
mod error;
pub mod file;
pub mod identity;
#[path = "media/types.rs"]
pub mod media;
pub mod observe;
#[path = "media/pipeline.rs"]
pub mod pipeline;
pub mod proxy;
pub mod rtcp;
pub mod rtp;
#[path = "protocol/rtsp.rs"]
pub mod rtsp;
pub mod runtime;
pub mod security;
pub mod storage;
/// Backward-compatible recording facade. New code may use [`storage`].
pub mod recording {
    pub use crate::storage::*;
}
/// Backward-compatible TLS facade. New code may use [`security`].
pub mod tls {
    pub use crate::security::{TlsClientConfig, TlsServerConfig};
}
#[path = "protocol/transport.rs"]
pub mod transport;
// Stable expansion-oriented facades.
pub mod protocol;
pub mod media_pipeline {
    pub use crate::pipeline::*;
    pub use crate::rtp::*;
}
pub mod server;
pub use auth::{
    AccessRole, AuthDecision, Authenticator, Credentials, DigestNonceManager,
    ManagedDigestAuthenticator,
};
pub use error::{Error, ErrorCode, Operation, Result, RetryClass};
pub use file::{
    ContainerFormat, DemuxedSample, Demuxer, FilePlayback, FileSource, FileSourceOptions, LoopMode,
    PlaybackStats, RawFormat,
};
pub use identity::{MountId, SessionId, StreamEpoch, TaskId, TrackKey};
pub use media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription, TrackId};
pub use observe::{EventSink, FrameworkEvent, NoopEventSink, TracingEventSink};
pub use security::{
    IpAcl, IpCidr, RateLimitConfig, SecurityConfig, TlsClientConfig, TlsServerConfig,
};
pub use server::{
    ConfigError, LocalPublisher, MountConfig, MountHandle, ResourceLimits, RtspServer,
    ServerConfig, ServerConfigBuilder,
};
pub use storage::{
    RecordOutcome, RecordingCatalog, RecordingConfig, RecordingDemuxer, RecordingReport,
    RecordingSink, RecordingVodSource, SegmentInfo, SegmentRecorder, VodSource,
};
