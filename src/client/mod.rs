//! RTSP client control-plane primitives.

#[cfg(feature = "tls")]
use crate::security::TlsClientConfig;
/// Transport selection for RTSP client connections (re-exported here so
/// callers do not need to reach into `rtflow::proxy`/`rtflow::transport`).
pub use crate::transport::UpstreamTransport;
use crate::{
    Error, Result, RetryClass,
    auth::{Credentials, authorization_from_challenge, basic_authorization},
    identity::SessionId,
    media::{AccessUnit, MediaDescription},
    observe::{ClientEvent, EventSink, FrameworkEvent},
    rtcp::{ReceptionSnapshot, RtcpSession},
    rtp::{Depacketizer, JitterBuffer, JitterBufferConfig, RtpPacket},
    rtsp::range::NptRange,
    rtsp::sdp,
    rtsp::{self, Message, Method, Response},
    runtime::io::{ControlConnection, ControlWriteHalf},
};
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
mod reconnect;
mod rtsp_client;
mod session;
mod setup;
#[cfg(test)]
mod tests;
mod wire;

pub use options::{
    ClientOptions, ClientOptionsBuilder, ClientReconnectPolicy, ClientReconnectPolicyBuilder,
};
pub use reconnect::ReconnectingClientSession;
pub use rtsp_client::RtspClient;
pub use session::{ClientQualitySnapshot, ClientSession};

static NEXT_CLIENT_SESSION: AtomicU64 = AtomicU64::new(1);

fn next_client_session_id() -> SessionId {
    SessionId::new(NEXT_CLIENT_SESSION.fetch_add(1, Ordering::Relaxed))
}

fn emit_client_event(sink: &Option<Arc<dyn EventSink>>, event: ClientEvent) {
    if let Some(sink) = sink {
        sink.record(&FrameworkEvent::Client(event));
    }
}
