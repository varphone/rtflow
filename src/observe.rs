//! Allocation-conscious control-plane events.
use crate::{
    error::{ErrorCode, Operation},
    identity::{MountId, SessionId, TaskId},
};
use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum FrameworkEvent {
    Server(ServerEvent),
    Session(SessionEvent),
    Mount(MountEvent),
    Proxy(ProxyEvent),
    Pipeline(PipelineEvent),
    Runtime(RuntimeEvent),
    Security(SecurityEvent),
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ServerEvent {
    Started { local_addr: SocketAddr },
    Stopping,
    Stopped,
    AcceptFailed { error: Arc<str> },
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SessionEvent {
    Accepted {
        id: SessionId,
        peer: SocketAddr,
    },
    Rejected {
        peer: SocketAddr,
        reason: RejectReason,
    },
    Closed {
        id: SessionId,
        peer: SocketAddr,
        elapsed: Duration,
        error: Option<ErrorCode>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    GlobalLimit,
    PeerLimit,
    AccessDenied,
    RateLimited,
    ShuttingDown,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum MountEvent {
    Registered { id: MountId },
    Unregistered { id: MountId },
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ProxyEvent {
    Connecting {
        mount: MountId,
        attempt: u32,
    },
    Streaming {
        mount: MountId,
    },
    Backoff {
        mount: MountId,
        attempt: u32,
        delay: Duration,
    },
    Failed {
        mount: MountId,
        code: ErrorCode,
    },
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum PipelineEvent {
    ElementFailed { element: Arc<str>, code: ErrorCode },
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum RuntimeEvent {
    TaskStarted {
        id: TaskId,
        name: Arc<str>,
    },
    TaskFinished {
        id: TaskId,
        name: Arc<str>,
    },
    TaskFailed {
        id: TaskId,
        name: Arc<str>,
        operation: Operation,
        code: ErrorCode,
    },
    TaskPanicked {
        id: TaskId,
        name: Arc<str>,
    },
    TaskCancelled {
        id: TaskId,
        name: Arc<str>,
    },
    TaskAborted {
        id: TaskId,
        name: Arc<str>,
    },
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SecurityEvent {
    AuthenticationFailed {
        peer: SocketAddr,
        mount: Option<MountId>,
    },
    RateLimited {
        peer: SocketAddr,
    },
    TlsHandshakeFailed {
        peer: SocketAddr,
        error: Arc<str>,
    },
}

pub trait EventSink: Send + Sync + 'static {
    fn record(&self, event: &FrameworkEvent);
}

#[derive(Default)]
pub struct NoopEventSink;
impl EventSink for NoopEventSink {
    fn record(&self, _: &FrameworkEvent) {}
}

#[derive(Default)]
pub struct TracingEventSink;
impl EventSink for TracingEventSink {
    fn record(&self, event: &FrameworkEvent) {
        tracing::info!(target: "rtsp::event", event = ?event, "framework event");
    }
}

impl fmt::Debug for dyn EventSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EventSink")
    }
}

pub(crate) fn emit(sink: &Arc<dyn EventSink>, event: FrameworkEvent) {
    sink.record(&event);
}
