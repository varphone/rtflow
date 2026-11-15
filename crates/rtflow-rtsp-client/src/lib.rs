//! RTSP client control-plane primitives.

use rtflow_core::{
    error::{Error, Result, RetryClass},
    identity::SessionId,
    observe::{ClientEvent, EventSink, FrameworkEvent},
};
use rtflow_media::{AccessUnit, MediaDescription};
use rtflow_rtcp::{ReceptionSnapshot, RtcpSession};
use rtflow_rtp::{Depacketizer, JitterBuffer, JitterBufferConfig, RtpPacket};
use rtflow_rtsp::auth::{Credentials, authorization_from_challenge, basic_authorization};
use rtflow_rtsp::range::NptRange;
/// Transport selection for RTSP client connections (re-exported here so
/// callers do not need to reach into `rtflow::proxy`/`rtflow::transport`).
pub use rtflow_rtsp::transport::UpstreamTransport;
use rtflow_rtsp::{self as rtsp, Message, Method, Response, sdp};
use rtflow_runtime::io::{ControlConnection, ControlWriteHalf};
#[cfg(feature = "tls")]
use rtflow_security::TlsClientConfig;
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;

mod control;
mod options;
mod publish;
mod queue;
mod reconnect;
mod rtsp_client;
mod session;
mod setup;
#[cfg(test)]
mod tests;
mod wire;

pub use options::{
    ClientOptions, ClientOptionsBuilder, ClientReconnectPolicy, ClientReconnectPolicyBuilder,
    ServerCapabilities,
};
pub use publish::RecordSession;
pub use reconnect::ReconnectingPlaySession;
pub use rtsp_client::{IntoRtspUrl, RtspClient};
pub use session::{ClientQualitySnapshot, PlayRequest, PlaySession};

/// Backwards-compatible name for [`PlaySession`].
pub type ClientSession = PlaySession;
/// Backwards-compatible name for [`RecordSession`].
pub type PublisherSession = RecordSession;
/// Backwards-compatible name for [`ReconnectingPlaySession`].
pub type ReconnectingClientSession = ReconnectingPlaySession;

static NEXT_CLIENT_SESSION: AtomicU64 = AtomicU64::new(1);

fn next_client_session_id() -> SessionId {
    SessionId::new(NEXT_CLIENT_SESSION.fetch_add(1, Ordering::Relaxed))
}

fn emit_client_event(sink: &Option<Arc<dyn EventSink>>, event: ClientEvent) {
    if let Some(sink) = sink {
        sink.record(&FrameworkEvent::Client(event));
    }
}
