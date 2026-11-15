//! Protocol-agnostic mount registry and stream publication primitives.
//!
//! A *mount* is a named stream endpoint that can be published to and subscribed
//! from. All protocol servers (RTSP, RTMP, SRT, WebRTC) share the same mount
//! registry, enabling cross-protocol interop.
mod config;
pub mod continuity;
mod frame_bus;
pub use frame_bus::{FrameBus, FrameReceiver, FrameRecvError};
mod handle;
mod publisher;
mod registry;
mod stats;
pub mod upstream;

#[doc(hidden)]
pub use config::description_for;
pub use config::{MountConfig, PushResult};
#[doc(hidden)]
pub use handle::{Mount, NackRequest, codec_lookup_table};
pub use handle::{MountHandle, STARTUP_WINDOW, startup_dedup_keys};
pub use publisher::LocalPublisher;
pub use registry::{
    DEFAULT_MAX_QUEUED_BYTES_PER_SESSION, MAX_CONSUMER_QUEUE_CAPACITY, MountOptions, MountRegistry,
};
pub use stats::{MountSnapshot, StreamStats};

#[doc(hidden)]
pub use stats::StreamCounters;

use self::{
    continuity::KeyframeCache,
    upstream::{ProxyState, UpstreamConfig},
};
use rtflow_core::{
    error::{Error, Result},
    identity::MountId,
    observe::EventSink,
};
use rtflow_media::{AccessUnit, Codec, MediaDescription};
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
