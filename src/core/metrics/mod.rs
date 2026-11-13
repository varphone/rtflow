//! Metrics primitives shared by the whole crate.
//!
//! [`MetricsRegistry`] is a passive, lock-free counter/histogram container and
//! [`MetricsEventSink`] fills it from [`crate::observe::FrameworkEvent`]s. Both
//! live at the bottom of the dependency graph so low layers (mounts, HTTP) can
//! update or expose counters without depending on the exporter built on top of
//! them ([`crate::metrics`]).

mod histogram;
pub use histogram::Histogram;

use crate::error::ErrorCode;
use crate::observe::{
    ClientEvent, EventSink, FrameworkEvent, MountEvent, PipelineEvent, ProxyEvent, RejectReason,
    RuntimeEvent, SecurityEvent, ServerEvent, SessionEvent,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Number of error code variants for the error counter array.
const ERROR_CODE_COUNT: usize = 12;

/// Metrics registry containing all server-level counters and histograms.
///
/// All fields use atomic types for lock-free access. Recording is designed
/// to have minimal impact on the hot path.
pub struct MetricsRegistry {
    // Connection layer
    pub connections_total: AtomicU64,
    pub connections_active: AtomicI64,
    pub connections_rejected: AtomicU64,

    // Session layer
    pub sessions_total: AtomicU64,
    pub sessions_active: AtomicI64,
    pub session_duration: Histogram,

    // Mount layer
    pub mounts_active: AtomicI64,
    pub frames_accepted: AtomicU64,
    pub frames_dropped: AtomicU64,

    // RTP layer
    pub rtp_packets_sent: AtomicU64,
    pub rtp_octets_sent: AtomicU64,
    pub rtp_packets_received: AtomicU64,

    // RTCP quality
    pub rtcp_loss_ratio: Histogram,
    pub rtcp_jitter_ms: Histogram,
    pub rtcp_rtt_ms: Histogram,

    // Errors by code
    pub errors_by_code: [AtomicU64; ERROR_CODE_COUNT],

    // Proxy
    pub proxy_reconnects: AtomicU64,
    pub proxy_failovers: AtomicU64,

    // Client (outbound RTSP play sessions)
    pub client_sessions_total: AtomicU64,
    pub client_sessions_active: AtomicI64,
    pub client_reconnect_attempts: AtomicU64,
    pub client_keep_alive_failures: AtomicU64,
    pub client_fallbacks: AtomicU64,

    // Protocol-level: RTMP
    pub rtmp_connections_total: AtomicU64,
    pub rtmp_publishers_active: AtomicI64,
    pub rtmp_players_active: AtomicI64,

    // Protocol-level: SRT
    pub srt_connections_total: AtomicU64,
    pub srt_streams_active: AtomicI64,

    // Protocol-level: WebRTC
    pub webrtc_sessions_total: AtomicU64,
    pub webrtc_sessions_active: AtomicI64,

    // Protocol-level: HTTP
    pub http_requests_total: AtomicU64,
    pub hls_segments_served: AtomicU64,
    pub dash_segments_served: AtomicU64,
    pub flv_streams_active: AtomicI64,
}

impl MetricsRegistry {
    /// Create a new metrics registry with all counters at zero.
    pub fn new() -> Self {
        Self {
            connections_total: AtomicU64::new(0),
            connections_active: AtomicI64::new(0),
            connections_rejected: AtomicU64::new(0),
            sessions_total: AtomicU64::new(0),
            sessions_active: AtomicI64::new(0),
            session_duration: Histogram::latency_ms(),
            mounts_active: AtomicI64::new(0),
            frames_accepted: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            rtp_packets_sent: AtomicU64::new(0),
            rtp_octets_sent: AtomicU64::new(0),
            rtp_packets_received: AtomicU64::new(0),
            rtcp_loss_ratio: Histogram::new(&[
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
            ]),
            rtcp_jitter_ms: Histogram::latency_ms(),
            rtcp_rtt_ms: Histogram::latency_ms(),
            errors_by_code: std::array::from_fn(|_| AtomicU64::new(0)),
            proxy_reconnects: AtomicU64::new(0),
            proxy_failovers: AtomicU64::new(0),
            client_sessions_total: AtomicU64::new(0),
            client_sessions_active: AtomicI64::new(0),
            client_reconnect_attempts: AtomicU64::new(0),
            client_keep_alive_failures: AtomicU64::new(0),
            client_fallbacks: AtomicU64::new(0),
            rtmp_connections_total: AtomicU64::new(0),
            rtmp_publishers_active: AtomicI64::new(0),
            rtmp_players_active: AtomicI64::new(0),
            srt_connections_total: AtomicU64::new(0),
            srt_streams_active: AtomicI64::new(0),
            webrtc_sessions_total: AtomicU64::new(0),
            webrtc_sessions_active: AtomicI64::new(0),
            http_requests_total: AtomicU64::new(0),
            hls_segments_served: AtomicU64::new(0),
            dash_segments_served: AtomicU64::new(0),
            flv_streams_active: AtomicI64::new(0),
        }
    }

    /// Record an error code into the errors_by_code counter.
    pub fn record_error(&self, code: ErrorCode) {
        let index = error_code_index(code);
        self.errors_by_code[index].fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MetricsRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsRegistry")
            .field(
                "connections_total",
                &self.connections_total.load(Ordering::Relaxed),
            )
            .field(
                "sessions_active",
                &self.sessions_active.load(Ordering::Relaxed),
            )
            .field("mounts_active", &self.mounts_active.load(Ordering::Relaxed))
            .finish()
    }
}

/// Map an ErrorCode to an array index (0..ERROR_CODE_COUNT).
fn error_code_index(code: ErrorCode) -> usize {
    match code {
        ErrorCode::InvalidProtocol => 0,
        ErrorCode::InvalidMedia => 1,
        ErrorCode::InvalidState => 2,
        ErrorCode::Timeout => 3,
        ErrorCode::ResourceExhausted => 4,
        ErrorCode::Unauthorized => 5,
        ErrorCode::Forbidden => 6,
        ErrorCode::Unsupported => 7,
        ErrorCode::UpstreamUnavailable => 8,
        ErrorCode::Io => 9,
        ErrorCode::Closed => 10,
        ErrorCode::Internal => 11,
        #[cfg(feature = "srtp")]
        ErrorCode::SrtpAuthenticationFailed | ErrorCode::SrtpReplayDetected => 11,
    }
}

/// EventSink implementation that records events into a MetricsRegistry.
pub struct MetricsEventSink {
    registry: Arc<MetricsRegistry>,
}

impl MetricsEventSink {
    pub fn new(registry: Arc<MetricsRegistry>) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &Arc<MetricsRegistry> {
        &self.registry
    }
}

/// Extract the metrics registry from an event sink, if the sink is a
/// [`MetricsEventSink`]. Used to wire per-mount frame counters into the
/// global registry.
pub fn registry_from_sink(
    sink: &Arc<dyn crate::observe::EventSink>,
) -> Option<Arc<MetricsRegistry>> {
    sink.as_any()
        .downcast_ref::<MetricsEventSink>()
        .map(|sink| sink.registry.clone())
}

impl EventSink for MetricsEventSink {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn record(&self, event: &FrameworkEvent) {
        match event {
            FrameworkEvent::Server(server_event) => match server_event {
                ServerEvent::Started { .. } => {}
                ServerEvent::Stopping | ServerEvent::Stopped => {}
                ServerEvent::AcceptFailed { .. } => {
                    self.registry
                        .connections_rejected
                        .fetch_add(1, Ordering::Relaxed);
                }
            },
            FrameworkEvent::Session(session_event) => match session_event {
                SessionEvent::Accepted { .. } => {
                    self.registry
                        .connections_total
                        .fetch_add(1, Ordering::Relaxed);
                    self.registry
                        .connections_active
                        .fetch_add(1, Ordering::Relaxed);
                    self.registry.sessions_total.fetch_add(1, Ordering::Relaxed);
                    self.registry
                        .sessions_active
                        .fetch_add(1, Ordering::Relaxed);
                }
                SessionEvent::Rejected { reason, .. } => {
                    self.registry
                        .connections_rejected
                        .fetch_add(1, Ordering::Relaxed);
                    match reason {
                        RejectReason::GlobalLimit | RejectReason::PeerLimit => {
                            self.registry.record_error(ErrorCode::ResourceExhausted);
                        }
                        RejectReason::AccessDenied => {
                            self.registry.record_error(ErrorCode::Unauthorized);
                        }
                        RejectReason::RateLimited => {
                            self.registry.record_error(ErrorCode::ResourceExhausted);
                        }
                        RejectReason::ShuttingDown => {
                            self.registry.record_error(ErrorCode::Closed);
                        }
                    }
                }
                SessionEvent::Closed { elapsed, error, .. } => {
                    self.registry
                        .connections_active
                        .fetch_sub(1, Ordering::Relaxed);
                    self.registry
                        .sessions_active
                        .fetch_sub(1, Ordering::Relaxed);
                    self.registry
                        .session_duration
                        .record(elapsed.as_secs_f64() * 1000.0);
                    if let Some(code) = error {
                        self.registry.record_error(*code);
                    }
                }
            },
            FrameworkEvent::Mount(mount_event) => match mount_event {
                MountEvent::Registered { .. } => {
                    self.registry.mounts_active.fetch_add(1, Ordering::Relaxed);
                }
                MountEvent::Unregistered { .. } => {
                    self.registry.mounts_active.fetch_sub(1, Ordering::Relaxed);
                }
            },
            FrameworkEvent::Proxy(proxy_event) => match proxy_event {
                ProxyEvent::Connecting { .. } => {
                    self.registry
                        .proxy_reconnects
                        .fetch_add(1, Ordering::Relaxed);
                }
                ProxyEvent::Failed { .. } => {
                    self.registry
                        .proxy_failovers
                        .fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            },
            FrameworkEvent::Client(client_event) => match client_event {
                ClientEvent::Connected { .. } => {
                    self.registry
                        .client_sessions_total
                        .fetch_add(1, Ordering::Relaxed);
                    self.registry
                        .client_sessions_active
                        .fetch_add(1, Ordering::Relaxed);
                }
                ClientEvent::Closed { error, .. } => {
                    self.registry
                        .client_sessions_active
                        .fetch_sub(1, Ordering::Relaxed);
                    if let Some(code) = error {
                        self.registry.record_error(*code);
                    }
                }
                ClientEvent::ReconnectBackoff { .. } => {
                    self.registry
                        .client_reconnect_attempts
                        .fetch_add(1, Ordering::Relaxed);
                }
                ClientEvent::KeepAliveFailed { .. } => {
                    self.registry
                        .client_keep_alive_failures
                        .fetch_add(1, Ordering::Relaxed);
                }
                ClientEvent::Fallback { .. } => {
                    self.registry
                        .client_fallbacks
                        .fetch_add(1, Ordering::Relaxed);
                }
                ClientEvent::Failed { code, .. } => {
                    self.registry.record_error(*code);
                }
            },
            FrameworkEvent::Runtime(RuntimeEvent::TaskFailed { code, .. }) => {
                self.registry.record_error(*code);
            }
            FrameworkEvent::Runtime(_) => {}
            FrameworkEvent::Pipeline(PipelineEvent::ElementFailed { code, .. }) => {
                self.registry.record_error(*code);
            }
            FrameworkEvent::Security(security_event) => match security_event {
                SecurityEvent::RateLimited { .. } => {
                    self.registry
                        .connections_rejected
                        .fetch_add(1, Ordering::Relaxed);
                }
                SecurityEvent::AuthenticationFailed { .. } => {
                    self.registry.record_error(ErrorCode::Unauthorized);
                }
                SecurityEvent::TlsHandshakeFailed { .. } => {
                    self.registry.record_error(ErrorCode::Io);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::SessionEvent;
    use std::sync::atomic::Ordering;

    #[test]
    fn metrics_registry_new() {
        let reg = MetricsRegistry::new();
        assert_eq!(reg.connections_total.load(Ordering::Relaxed), 0);
        assert_eq!(reg.sessions_active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn error_code_index_mapping() {
        assert_eq!(error_code_index(ErrorCode::InvalidProtocol), 0);
        assert_eq!(error_code_index(ErrorCode::Internal), 11);
    }

    #[test]
    fn metrics_event_sink_records_session() {
        let registry = Arc::new(MetricsRegistry::new());
        let sink = MetricsEventSink::new(registry.clone());

        use crate::identity::SessionId;
        use std::net::SocketAddr;

        sink.record(&FrameworkEvent::Session(SessionEvent::Accepted {
            id: SessionId::new(1),
            peer: "127.0.0.1:1234".parse::<SocketAddr>().unwrap(),
        }));

        assert_eq!(registry.sessions_total.load(Ordering::Relaxed), 1);
        assert_eq!(registry.sessions_active.load(Ordering::Relaxed), 1);
    }
}
