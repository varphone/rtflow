#![cfg_attr(not(feature = "gstreamer"), forbid(unsafe_code))]
//! Async RTSP/RTP client, server and on-demand proxy framework for embedded media.
//!
//! Encoded [`AccessUnit`] values travel through the pipeline; the crate never
//! decodes or transcodes media, so hardware codecs remain outside the hot path.
//!
//! # Module map
//!
//! | Layer | Modules |
//! |---|---|
//! | Primitives | [`core`] (error/identity/observe/metrics registry), [`media`] |
//! | Wire formats | [`rtsp`], [`rtp`], [`rtcp`], [`srtp`], [`security`] |
//! | Streaming | [`mount`], [`pipeline`], [`runtime`] |
//! | Endpoints | [`server`], [`client`], [`http`], [`metrics`] |
//! | Adapters | [`protocols`] (HLS, DASH, HTTP-FLV, RTMP, SRT, WebRTC, GStreamer) |
//! | Composition | [`gateway`] |
//!
//! Dependencies only point downwards; `scripts/check-layering.py` enforces it and
//! `docs/architecture/overview.md` documents the layer diagram.
//!
//! # Feature profiles
//!
//! - default: `full`
//! - `embedded-gateway`: RTSP client/server + TLS + metrics + HTTP-FLV + HLS
//! - `experimental-protocols`: DASH, RTMP, SRT, WebRTC
//! - `gstreamer` is opt-in (system GStreamer libraries required).
//!
//! # Quick start
//!
//! [`prelude`] re-exports the common types; protocol-specific items live in
//! their module (`rtflow::server`, `rtflow::client`, `rtflow::protocols::*`).
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
/// Compatibility facade for the pre-0.3 `rtflow::protocol::*` module paths.
#[deprecated(since = "0.3.0", note = "use `rtflow::rtsp` instead")]
pub mod protocol;
pub mod protocols;
#[cfg(feature = "server")]
#[deprecated(
    since = "0.3.0",
    note = "use `rtflow::mount::{upstream, continuity}` instead"
)]
pub mod proxy;
pub mod rtcp;
pub mod rtp;
/// RTSP control plane: message codec, SDP, ranges, authentication, transport.
pub mod rtsp;
pub mod runtime;
pub mod security;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "test-source")]
pub mod source;
#[cfg(feature = "srtp")]
pub mod srtp;
#[cfg(feature = "file-storage")]
pub mod storage;
/// Compatibility alias for the pre-0.3 protocol-adapter traits.
#[deprecated(since = "0.3.0", note = "use `rtflow::gateway::protocol` instead")]
pub use gateway::protocol as protocol_trait;
pub use gateway::protocol::{IngestProtocol, OutputProtocol};
/// Compatibility facade combining [`pipeline`] and [`rtp`].
#[deprecated(since = "0.3.0", note = "use `rtflow::pipeline` and `rtflow::rtp`")]
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
pub use mount::{
    LocalPublisher, MountConfig, MountHandle, MountRegistry, MountSnapshot, PushResult, StreamStats,
};
pub use observe::{EventSink, FrameworkEvent, NoopEventSink, TracingEventSink};
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
    ConfigError, ResourceLimits, RtspServer, ServerConfig, ServerConfigBuilder, TimelineMode,
};
#[cfg(feature = "srtp")]
pub use srtp::{SessionKeys, SrtcpContext, SrtpContext, SrtpProfile};
#[cfg(feature = "file-storage")]
pub use storage::{
    RecordOutcome, RecordingCatalog, RecordingConfig, RecordingDemuxer, RecordingReport,
    RecordingSink, RecordingVodSource, SegmentInfo, SegmentRecorder, VodSource,
};

/// Common imports for applications built on rtflow.
///
/// ```
/// use rtflow::prelude::*;
/// ```
pub mod prelude {
    pub use crate::mount::{LocalPublisher, MountHandle, MountRegistry};
    pub use crate::observe::{EventSink, FrameworkEvent};
    pub use crate::{AccessUnit, Codec, Error, MediaDescription, Result, TrackDescription};

    #[cfg(feature = "client")]
    pub use crate::client::{ClientOptions, RtspClient};
    #[cfg(feature = "server")]
    pub use crate::server::{RtspServer, ServerConfig};
}
