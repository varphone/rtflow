//! Metrics collection for rtflow server instances.
//!
//! Provides a zero-allocation, lock-free metrics registry using atomic counters
//! and bounded histograms. Metrics are collected via the [`EventSink`] trait
//! and can be exported in Prometheus text format via [`MetricsExporter`].

mod histogram;
pub use histogram::Histogram;

use crate::error::ErrorCode;
use crate::observe::{
    ClientEvent, EventSink, FrameworkEvent, MountEvent, PipelineEvent, ProxyEvent, RejectReason,
    RuntimeEvent, SecurityEvent, ServerEvent, SessionEvent,
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
    write_counter(
        &mut out,
        "rtflow_client_sessions_total",
        "Total outbound RTSP client sessions",
        registry.client_sessions_total.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_client_sessions_active",
        "Active outbound RTSP client sessions",
        registry.client_sessions_active.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_client_reconnect_attempts_total",
        "Total client reconnect backoff attempts",
        registry.client_reconnect_attempts.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_client_keep_alive_failures_total",
        "Total client keep-alive failures",
        registry.client_keep_alive_failures.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_client_fallbacks_total",
        "Total client UDP-to-TCP transport fallbacks",
        registry.client_fallbacks.load(Ordering::Relaxed),
    );

    // Protocol-level counters
    write_counter(
        &mut out,
        "rtflow_rtmp_connections_total",
        "Total RTMP connections",
        registry.rtmp_connections_total.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_rtmp_publishers_active",
        "Active RTMP publishers",
        registry.rtmp_publishers_active.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_rtmp_players_active",
        "Active RTMP players",
        registry.rtmp_players_active.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_srt_connections_total",
        "Total SRT connections",
        registry.srt_connections_total.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_srt_streams_active",
        "Active SRT streams",
        registry.srt_streams_active.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_webrtc_sessions_total",
        "Total WebRTC sessions",
        registry.webrtc_sessions_total.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_webrtc_sessions_active",
        "Active WebRTC sessions",
        registry.webrtc_sessions_active.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_http_requests_total",
        "Total HTTP requests served",
        registry.http_requests_total.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_hls_segments_served_total",
        "Total HLS segments served",
        registry.hls_segments_served.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_dash_segments_served_total",
        "Total DASH segments served",
        registry.dash_segments_served.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_flv_streams_active",
        "Active HTTP-FLV streams",
        registry.flv_streams_active.load(Ordering::Relaxed),
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

// ---------------------------------------------------------------------------
// Mount-level metrics and /stats HTTP handler
// ---------------------------------------------------------------------------

/// Per-mount metrics snapshot for JSON reporting.
#[derive(Clone, Debug)]
pub struct MountMetrics {
    pub path: String,
    pub publisher_connected: bool,
    pub subscriber_count: usize,
    pub accepted_frames: u64,
    pub dropped_frames: u64,
    pub rtp_packets: u64,
    pub rtp_octets: u64,
}

/// Collect per-mount metrics from a registry.
pub fn collect_mount_metrics(mounts: &crate::mount::MountRegistry) -> Vec<MountMetrics> {
    mounts
        .snapshots()
        .into_iter()
        .map(|(path, snap)| MountMetrics {
            path: path.to_string(),
            publisher_connected: snap.publisher_connected,
            subscriber_count: snap.subscriber_count,
            accepted_frames: snap.stats.accepted_frames,
            dropped_frames: snap.stats.dropped_frames,
            rtp_packets: snap.stats.rtp_packets,
            rtp_octets: snap.stats.rtp_octets,
        })
        .collect()
}

/// Create an HTTP handler that serves /stats as JSON.
///
/// Returns global protocol metrics plus per-mount snapshots.
#[cfg(feature = "http")]
pub fn create_stats_handler(
    registry: Arc<MetricsRegistry>,
    mounts: Arc<crate::mount::MountRegistry>,
) -> crate::http::Handler {
    let handler: crate::http::Handler = Arc::new(move |_req, mut writer| {
        let registry = registry.clone();
        let mounts = mounts.clone();
        Box::pin(async move {
            let json = build_stats_json(&registry, &mounts);
            writer
                .write_body(200, "application/json", json.as_bytes())
                .await?;
            Ok(writer)
        })
    });
    handler
}

/// Create an HTTP handler that serves Prometheus metrics text.
#[cfg(feature = "http")]
pub fn create_metrics_handler(registry: Arc<MetricsRegistry>) -> crate::http::Handler {
    let handler: crate::http::Handler = Arc::new(move |_req, mut writer| {
        let registry = registry.clone();
        Box::pin(async move {
            let mut body = String::with_capacity(6144);
            // Reuse the same format as the standalone exporter
            write_counter(
                &mut body,
                "rtflow_connections_total",
                "Total connections accepted",
                registry.connections_total.load(Ordering::Relaxed),
            );
            write_gauge(
                &mut body,
                "rtflow_connections_active",
                "Currently active connections",
                registry.connections_active.load(Ordering::Relaxed),
            );
            write_counter(
                &mut body,
                "rtflow_sessions_total",
                "Total sessions",
                registry.sessions_total.load(Ordering::Relaxed),
            );
            write_gauge(
                &mut body,
                "rtflow_sessions_active",
                "Active sessions",
                registry.sessions_active.load(Ordering::Relaxed),
            );
            write_gauge(
                &mut body,
                "rtflow_mounts_active",
                "Active mounts",
                registry.mounts_active.load(Ordering::Relaxed),
            );
            write_counter(
                &mut body,
                "rtflow_frames_accepted_total",
                "Frames accepted",
                registry.frames_accepted.load(Ordering::Relaxed),
            );
            write_counter(
                &mut body,
                "rtflow_frames_dropped_total",
                "Frames dropped",
                registry.frames_dropped.load(Ordering::Relaxed),
            );
            write_counter(
                &mut body,
                "rtflow_rtmp_connections_total",
                "RTMP connections",
                registry.rtmp_connections_total.load(Ordering::Relaxed),
            );
            write_counter(
                &mut body,
                "rtflow_srt_connections_total",
                "SRT connections",
                registry.srt_connections_total.load(Ordering::Relaxed),
            );
            write_counter(
                &mut body,
                "rtflow_webrtc_sessions_total",
                "WebRTC sessions",
                registry.webrtc_sessions_total.load(Ordering::Relaxed),
            );
            write_counter(
                &mut body,
                "rtflow_hls_segments_served_total",
                "HLS segments served",
                registry.hls_segments_served.load(Ordering::Relaxed),
            );
            write_counter(
                &mut body,
                "rtflow_dash_segments_served_total",
                "DASH segments served",
                registry.dash_segments_served.load(Ordering::Relaxed),
            );
            writer
                .write_body(
                    200,
                    "text/plain; version=0.0.4; charset=utf-8",
                    body.as_bytes(),
                )
                .await?;
            Ok(writer)
        })
    });
    handler
}

#[cfg(feature = "http")]
fn build_stats_json(registry: &MetricsRegistry, mounts: &crate::mount::MountRegistry) -> String {
    let mount_metrics = collect_mount_metrics(mounts);
    let mut out = String::with_capacity(2048);
    out.push_str("{\"global\":{");
    out.push_str(&format!(
        "\"connections_total\":{},",
        registry.connections_total.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"connections_active\":{},",
        registry.connections_active.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"sessions_active\":{},",
        registry.sessions_active.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"mounts_active\":{},",
        registry.mounts_active.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"frames_accepted\":{},",
        registry.frames_accepted.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"frames_dropped\":{},",
        registry.frames_dropped.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"rtmp_connections_total\":{},",
        registry.rtmp_connections_total.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"rtmp_publishers_active\":{},",
        registry.rtmp_publishers_active.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"srt_connections_total\":{},",
        registry.srt_connections_total.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"srt_streams_active\":{},",
        registry.srt_streams_active.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"webrtc_sessions_total\":{},",
        registry.webrtc_sessions_total.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"webrtc_sessions_active\":{},",
        registry.webrtc_sessions_active.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"hls_segments_served\":{},",
        registry.hls_segments_served.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"dash_segments_served\":{},",
        registry.dash_segments_served.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"flv_streams_active\":{}",
        registry.flv_streams_active.load(Ordering::Relaxed)
    ));
    out.push_str("},\"mounts\":[");
    for (i, m) in mount_metrics.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let escaped_path = m.path.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!(
            "{{\"path\":\"{}\",\"publisher_connected\":{},\"subscriber_count\":{},\"accepted_frames\":{},\"dropped_frames\":{},\"rtp_packets\":{},\"rtp_octets\":{}}}",
            escaped_path, m.publisher_connected, m.subscriber_count,
            m.accepted_frames, m.dropped_frames, m.rtp_packets, m.rtp_octets
        ));
    }
    out.push_str("]}");
    out
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
