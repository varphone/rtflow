//! Protocol-agnostic mount registry and stream publication primitives.
//!
//! A *mount* is a named stream endpoint that can be published to and subscribed
//! from. All protocol servers (RTSP, RTMP, SRT, WebRTC) share the same mount
//! registry, enabling cross-protocol interop.
mod config;
#[cfg(feature = "server")]
pub mod continuity;
mod frame_bus;
pub(crate) use frame_bus::FrameBus;
pub use frame_bus::{FrameReceiver, FrameRecvError};
mod handle;
mod publisher;
mod registry;
mod stats;
#[cfg(feature = "server")]
pub mod upstream;

pub(crate) use config::description_for;
pub use config::{MountConfig, PushResult};
#[allow(unused_imports)] // NackRequest is only read by the RTSP sender feature
pub(crate) use handle::{Mount, NackRequest, codec_lookup_table};
pub use handle::{MountHandle, STARTUP_WINDOW, startup_dedup_keys};
pub use publisher::LocalPublisher;
pub use registry::{
    DEFAULT_MAX_QUEUED_BYTES_PER_SESSION, MAX_CONSUMER_QUEUE_CAPACITY, MountOptions, MountRegistry,
};
pub use stats::{MountSnapshot, StreamStats};

pub(crate) use stats::StreamCounters;

#[cfg(feature = "server")]
use self::{
    continuity::KeyframeCache,
    upstream::{ProxyState, UpstreamConfig},
};
use crate::{
    Codec, Error, Result,
    identity::MountId,
    media::{AccessUnit, MediaDescription},
    observe::EventSink,
};
use bytes::Bytes;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
