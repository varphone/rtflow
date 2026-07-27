//! Metrics collection for rtflow server instances.
//!
//! Provides a zero-allocation, lock-free metrics registry using atomic counters
//! and bounded histograms. Metrics are collected via the [`EventSink`] trait
//! and can be exported in Prometheus text format via [`MetricsExporter`].

mod histogram;
pub use histogram::Histogram;

use crate::error::ErrorCode;
use crate::observe::{
    EventSink, FrameworkEvent, MountEvent, PipelineEvent, ProxyEvent, RejectReason, RuntimeEvent,
    SecurityEvent, ServerEvent, SessionEvent,
};
use std::net::SocketAddr;
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

impl EventSink for MetricsEventSink {
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

/// Prometheus text format exporter.
///
/// Serves `/metrics` and `/health` HTTP endpoints.
pub struct MetricsExporter {
    registry: Arc<MetricsRegistry>,
    listen: SocketAddr,
}

impl MetricsExporter {
    pub fn new(registry: Arc<MetricsRegistry>, listen: SocketAddr) -> Self {
        Self { registry, listen }
    }

    /// Start the HTTP metrics server in the background.
    ///
    /// Returns a handle that can be used to stop the server.
    pub async fn start(self) -> crate::Result<tokio::task::JoinHandle<()>> {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(self.listen).await?;
        let registry = self.registry;
        let connections = Arc::new(tokio::sync::Semaphore::new(16));

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                let Ok(permit) = connections.clone().try_acquire_owned() else {
                    let _ = stream.shutdown().await;
                    continue;
                };
                let registry = registry.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let mut buf = [0u8; 1024];
                    let Ok(Ok(n)) = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
                    )
                    .await
                    else {
                        return;
                    };
                    let request = std::str::from_utf8(&buf[..n]).unwrap_or("");
                    let response = if request.starts_with("GET /health") {
                        health_response(&registry)
                    } else if request.starts_with("GET /metrics") {
                        metrics_response(&registry)
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        Ok(handle)
    }
}

/// Generate Prometheus text format metrics.
fn metrics_response(registry: &MetricsRegistry) -> String {
    let mut out = String::with_capacity(4096);

    // Counters
    write_counter(
        &mut out,
        "rtflow_connections_total",
        "Total connections accepted",
        registry.connections_total.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_connections_active",
        "Currently active connections",
        registry.connections_active.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_connections_rejected_total",
        "Total connections rejected",
        registry.connections_rejected.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_sessions_total",
        "Total RTSP sessions",
        registry.sessions_total.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_sessions_active",
        "Currently active RTSP sessions",
        registry.sessions_active.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_mounts_active",
        "Currently registered mounts",
        registry.mounts_active.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_frames_accepted_total",
        "Total frames accepted",
        registry.frames_accepted.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_frames_dropped_total",
        "Total frames dropped",
        registry.frames_dropped.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_rtp_packets_sent_total",
        "Total RTP packets sent",
        registry.rtp_packets_sent.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_rtp_octets_sent_total",
        "Total RTP octets sent",
        registry.rtp_octets_sent.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_rtp_packets_received_total",
        "Total RTP packets received",
        registry.rtp_packets_received.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_proxy_reconnects_total",
        "Total proxy reconnect attempts",
        registry.proxy_reconnects.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_proxy_failovers_total",
        "Total proxy failovers",
        registry.proxy_failovers.load(Ordering::Relaxed),
    );

    // Error counters by code
    let code_names = [
        "InvalidProtocol",
        "InvalidMedia",
        "InvalidState",
        "Timeout",
        "ResourceExhausted",
        "Unauthorized",
        "Forbidden",
        "Unsupported",
        "UpstreamUnavailable",
        "Io",
        "Closed",
        "Internal",
    ];
    out.push_str("# HELP rtflow_errors_total Total errors by error code\n");
    out.push_str("# TYPE rtflow_errors_total counter\n");
    for (i, name) in code_names.iter().enumerate() {
        let value = registry.errors_by_code[i].load(Ordering::Relaxed);
        if value > 0 {
            out.push_str(&format!("rtflow_errors_total{{code=\"{name}\"}} {value}\n"));
        }
    }

    // Histograms
    write_histogram(
        &mut out,
        "rtflow_session_duration_ms",
        "Session duration in milliseconds",
        &registry.session_duration,
    );
    write_histogram(
        &mut out,
        "rtflow_rtcp_loss_ratio",
        "RTCP packet loss ratio",
        &registry.rtcp_loss_ratio,
    );
    write_histogram(
        &mut out,
        "rtflow_rtcp_jitter_ms",
        "RTCP jitter in milliseconds",
        &registry.rtcp_jitter_ms,
    );
    write_histogram(
        &mut out,
        "rtflow_rtcp_rtt_ms",
        "RTCP round-trip time in milliseconds",
        &registry.rtcp_rtt_ms,
    );

    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
        out.len(),
        out
    )
}

/// Generate health check response.
fn health_response(registry: &MetricsRegistry) -> String {
    let body = format!(
        "{{\"status\":\"healthy\",\"active_sessions\":{},\"active_mounts\":{}}}",
        registry.sessions_active.load(Ordering::Relaxed),
        registry.mounts_active.load(Ordering::Relaxed),
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

fn write_counter(out: &mut String, name: &str, help: &str, value: u64) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} counter\n"));
    out.push_str(&format!("{name} {value}\n"));
}

fn write_gauge(out: &mut String, name: &str, help: &str, value: i64) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} gauge\n"));
    out.push_str(&format!("{name} {value}\n"));
}

fn write_histogram(out: &mut String, name: &str, help: &str, histogram: &Histogram) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} histogram\n"));
    for (boundary, count) in histogram.cumulative_counts() {
        let le = if boundary.is_infinite() {
            "+Inf".to_string()
        } else {
            format!("{boundary}")
        };
        out.push_str(&format!("{name}_bucket{{le=\"{le}\"}} {count}\n"));
    }
    out.push_str(&format!("{name}_sum {}\n", histogram.sum()));
    out.push_str(&format!("{name}_count {}\n", histogram.count()));
}

#[cfg(test)]
mod tests {
    use super::*;

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
