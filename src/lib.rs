#![cfg_attr(not(feature = "gstreamer"), forbid(unsafe_code))]
//! Async RTSP/RTP primitives for embedded media gateways.
//!
//! Encoded [`AccessUnit`] values travel through the pipeline; the crate never
//! decodes or transcodes media, so hardware codecs remain outside the hot path.
#[cfg(feature = "client")]
pub mod client;
pub mod core;
mod error;
#[cfg(feature = "file-storage")]
pub mod file;
pub mod gateway;
#[cfg(feature = "http")]
pub mod http;
pub mod identity;
pub mod media;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod mount;
pub mod observe;
pub mod pipeline;
pub mod protocols;
#[cfg(feature = "server")]
pub mod proxy;
pub mod rtcp;
pub mod rtp;
/// RTSP control plane: message codec, SDP, ranges, authentication, transport.
pub mod rtsp;
pub mod runtime;
pub mod security;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "srtp")]
pub mod srtp;
#[cfg(feature = "file-storage")]
pub mod storage;
// Stable expansion-oriented facades. `protocol` is the pre-0.3 home of the RTSP
// control plane; new code should use `rtsp`.
pub mod protocol;
pub mod protocol_trait;
pub mod media_pipeline {
    pub use crate::pipeline::*;
    pub use crate::rtp::*;
}
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
#[cfg(feature = "dash")]
pub use protocols::dash;
#[cfg(feature = "gstreamer")]
pub use protocols::gst;
#[cfg(feature = "hls")]
pub use protocols::hls;
#[cfg(feature = "http-flv")]
pub use protocols::http_flv;
#[cfg(feature = "rtmp")]
pub use protocols::rtmp;
#[cfg(feature = "srt")]
pub use protocols::srt;
#[cfg(feature = "webrtc")]
pub use protocols::webrtc;
pub use rtsp::auth::{
    AccessRole, AuthDecision, Authenticator, Credentials, DigestNonceManager,
    ManagedDigestAuthenticator,
};
#[cfg(feature = "multicast")]
pub use rtsp::multicast;
pub use rtsp::{auth, token_auth, transport};
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
