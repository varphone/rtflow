#![cfg_attr(not(feature = "gstreamer"), forbid(unsafe_code))]
//! Async RTSP/RTP primitives for embedded media gateways.
//!
//! Encoded [`AccessUnit`] values travel through the pipeline; the crate never
//! decodes or transcodes media, so hardware codecs remain outside the hot path.
#[path = "protocol/auth.rs"]
pub mod auth;
#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "dash")]
pub mod dash;
mod error;
#[cfg(feature = "file-storage")]
pub mod file;
pub mod gateway;
#[cfg(feature = "gstreamer")]
pub mod gst;
#[cfg(feature = "hls")]
pub mod hls;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "http-flv")]
pub mod http_flv;
pub mod identity;
#[path = "media/types.rs"]
pub mod media;
#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(feature = "multicast")]
pub mod multicast;
pub mod mount;
pub mod observe;
#[path = "media/pipeline.rs"]
pub mod pipeline;
#[cfg(feature = "server")]
pub mod proxy;
pub mod protocol_trait;
pub mod rtcp;
#[cfg(feature = "rtmp")]
pub mod rtmp;
pub mod rtp;
#[path = "protocol/rtsp.rs"]
pub mod rtsp;
pub mod runtime;
pub mod security;
#[cfg(feature = "srt")]
pub mod srt;
#[cfg(feature = "srtp")]
pub mod srtp;
#[cfg(feature = "file-storage")]
pub mod storage;
#[cfg(feature = "webrtc")]
pub mod webrtc;
/// Backward-compatible recording facade. New code may use [`storage`].
#[cfg(feature = "file-storage")]
#[deprecated(since = "0.2.0", note = "use `storage` module instead")]
pub mod recording {
    pub use crate::storage::*;
}
/// Backward-compatible TLS facade. New code may use [`security`].
#[cfg(feature = "tls")]
#[deprecated(since = "0.2.0", note = "use `security` module instead")]
pub mod tls {
    pub use crate::security::{TlsClientConfig, TlsServerConfig};
}
#[path = "protocol/token_auth.rs"]
pub mod token_auth;
#[path = "protocol/transport.rs"]
pub mod transport;
// Stable expansion-oriented facades.
pub mod protocol;
pub mod media_pipeline {
    pub use crate::pipeline::*;
    pub use crate::rtp::*;
}
#[cfg(feature = "server")]
pub mod server;
pub use auth::{
    AccessRole, AuthDecision, Authenticator, Credentials, DigestNonceManager,
    ManagedDigestAuthenticator,
};
pub use error::{Error, ErrorCode, Operation, Result, RetryClass};
#[cfg(feature = "file-storage")]
pub use file::{
    ContainerFormat, DemuxedSample, Demuxer, FilePlayback, FileSource, FileSourceOptions, LoopMode,
    PlaybackStats, RawFormat,
};
pub use identity::{MountId, SessionId, StreamEpoch, TaskId, TrackKey};
pub use media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription, TrackId};
pub use mount::{MountRegistry, PushResult};
pub use observe::{EventSink, FrameworkEvent, NoopEventSink, TracingEventSink};
pub use protocol_trait::{IngestProtocol, OutputProtocol};
pub use security::{IpAcl, IpCidr, RateLimitConfig, SecurityConfig};
#[cfg(feature = "tls")]
pub use security::{TlsClientConfig, TlsServerConfig};
#[cfg(feature = "server")]
pub use server::{
    ConfigError, LocalPublisher, MountConfig, MountHandle, ResourceLimits, RtspServer,
    ServerConfig, ServerConfigBuilder,
};
#[cfg(feature = "srtp")]
pub use srtp::{SessionKeys, SrtcpContext, SrtpContext, SrtpProfile};
#[cfg(feature = "file-storage")]
pub use storage::{
    RecordOutcome, RecordingCatalog, RecordingConfig, RecordingDemuxer, RecordingReport,
    RecordingSink, RecordingVodSource, SegmentInfo, SegmentRecorder, VodSource,
};
