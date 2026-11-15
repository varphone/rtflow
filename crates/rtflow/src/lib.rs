#![cfg_attr(not(feature = "gstreamer"), forbid(unsafe_code))]
//! Async RTSP/RTP client, server and on-demand proxy framework for embedded media.
//!
//! Encoded [`AccessUnit`] values travel through the pipeline; the crate never
//! decodes or transcodes media, so hardware codecs remain outside the hot path.
//!
//! This crate is the **umbrella facade**: every module below is a re-export of
//! a sibling crate in the workspace (`rtflow-core`, `rtflow-media`,
//! `rtflow-rtsp`, `rtflow-hls`, …). Application code depends on `rtflow`
//! only; the underlying crates are an implementation detail.
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

#[cfg(feature = "admin")]
pub use rtflow_admin as admin;
pub use rtflow_core as core;
#[cfg(feature = "file-storage")]
pub use rtflow_file as file;
pub use rtflow_gateway as gateway;
pub use rtflow_http as http;
pub use rtflow_media as media;
#[cfg(feature = "metrics")]
pub use rtflow_metrics as metrics;
pub use rtflow_mount as mount;
pub use rtflow_pipeline as pipeline;
pub use rtflow_rtcp as rtcp;
pub use rtflow_rtp as rtp;
pub use rtflow_rtsp as rtsp;
#[cfg(feature = "client")]
pub use rtflow_rtsp_client as client;
pub use rtflow_rtsp_server as server;
pub use rtflow_runtime as runtime;
pub use rtflow_security as security;
#[cfg(feature = "test-source")]
pub use rtflow_source as source;
#[cfg(feature = "srtp")]
pub use rtflow_srtp as srtp;
#[cfg(feature = "file-storage")]
pub use rtflow_storage as storage;

/// Error, identity and event primitives (moved to `rtflow-core`).
pub use rtflow_core::error::{self};
#[doc(hidden)]
pub use rtflow_core::identity;
#[doc(hidden)]
pub use rtflow_core::observe;

/// Wire-protocol adapters (HLS, DASH, HTTP-FLV, RTMP, SRT, WebRTC, GStreamer).
pub mod protocols {
    #[cfg(feature = "dash")]
    pub use rtflow_dash as dash;
    #[cfg(feature = "gstreamer")]
    pub use rtflow_gst as gst;
    pub use rtflow_hls as hls;
    #[cfg(feature = "http-flv")]
    pub use rtflow_http_flv as http_flv;
    #[cfg(feature = "rtmp")]
    pub use rtflow_rtmp as rtmp;
    #[cfg(feature = "srt")]
    pub use rtflow_srt as srt;
    #[cfg(feature = "webrtc")]
    pub use rtflow_webrtc as webrtc;
}

#[cfg(feature = "dash")]
pub use protocols::dash;
#[cfg(feature = "gstreamer")]
pub use protocols::gst;
pub use protocols::hls;
#[cfg(feature = "http-flv")]
pub use protocols::http_flv;
#[cfg(feature = "rtmp")]
pub use protocols::rtmp;
#[cfg(feature = "srt")]
pub use protocols::srt;
#[cfg(feature = "webrtc")]
pub use protocols::webrtc;

/// Compatibility facade for the pre-0.3 `rtflow::protocol::*` module paths.
#[deprecated(since = "0.3.0", note = "use `rtflow::rtsp` instead")]
pub mod protocol {
    pub use crate::rtsp::message as rtsp;
    pub use crate::rtsp::{auth, range, sdp, token_auth, transport};
}

/// Compatibility facade for upstream proxy configuration.
#[deprecated(
    since = "0.3.0",
    note = "use `rtflow::mount::{upstream, continuity}` instead"
)]
pub mod proxy {
    pub use crate::mount::continuity::*;
    pub use crate::mount::upstream::*;
    pub use crate::rtsp::transport::UpstreamTransport;
}

/// Compatibility alias for the pre-0.3 protocol-adapter traits.
#[deprecated(since = "0.3.0", note = "use `rtflow::gateway::protocol` instead")]
pub use rtflow_gateway::protocol as protocol_trait;
pub use rtflow_gateway::protocol::{IngestProtocol, OutputProtocol};
/// Compatibility facade combining [`pipeline`] and [`rtp`].
#[deprecated(since = "0.3.0", note = "use `rtflow::pipeline` and `rtflow::rtp`")]
pub mod media_pipeline {
    pub use crate::pipeline::*;
    pub use crate::rtp::*;
}

pub use rtflow_core::error::{Error, ErrorCode, Operation, Result, RetryClass};
pub use rtflow_core::identity::{MountId, SessionId, StreamEpoch, TaskId};
pub use rtflow_core::observe::{EventSink, FrameworkEvent, NoopEventSink, TracingEventSink};
#[cfg(feature = "file-storage")]
pub use rtflow_file::{
    ContainerFormat, DemuxedSample, Demuxer, FilePlayback, FileSource, FileSourceOptions,
    FileVodSource, LoopMode, PlaybackStats, RawFormat,
};
pub use rtflow_media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription, TrackId};
pub use rtflow_mount::{
    LocalPublisher, MountConfig, MountHandle, MountOptions, MountRegistry, MountSnapshot,
    PushResult, StreamStats,
};
#[cfg(feature = "multicast")]
pub use rtflow_multicast as multicast;
pub use rtflow_rtsp::auth::{
    AccessRole, AuthDecision, Authenticator, Credentials, DigestNonceManager,
    ManagedDigestAuthenticator,
};
pub use rtflow_rtsp::{auth, token_auth, transport};
pub use rtflow_rtsp_server::{
    ConfigError, ResourceLimits, RtspServer, ServerConfig, ServerConfigBuilder, TimelineMode,
};
pub use rtflow_security::{IpAcl, IpCidr, RateLimitConfig, SecurityConfig};
#[cfg(feature = "tls")]
pub use rtflow_security::{TlsClientConfig, TlsServerConfig};
#[cfg(feature = "srtp")]
pub use rtflow_srtp::{SessionKeys, SrtcpContext, SrtpContext, SrtpProfile};
#[cfg(feature = "file-storage")]
pub use rtflow_storage::{
    RecordOutcome, RecordingCatalog, RecordingConfig, RecordingDemuxer, RecordingReport,
    RecordingSink, RecordingVodSource, SegmentInfo, SegmentRecorder, VodSource,
};

/// Common imports for applications built on rtflow.
///
/// ```
/// use rtflow::prelude::*;
/// ```
pub mod prelude {
    pub use crate::mount::{LocalPublisher, MountConfig, MountHandle, MountOptions, MountRegistry};
    pub use crate::observe::{EventSink, FrameworkEvent};
    pub use crate::{
        AccessUnit, Codec, Error, MediaDescription, MediaKind, Result, TrackDescription,
    };

    #[cfg(feature = "client")]
    pub use crate::client::{
        ClientOptions, ClientQualitySnapshot, PlaySession, ReconnectingPlaySession, RecordSession,
        RtspClient, UpstreamTransport,
    };
    pub use crate::server::{RtspServer, ServerConfig, TimelineMode};
}
