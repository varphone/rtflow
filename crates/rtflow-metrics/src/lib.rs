//! Metrics collection and export for rtflow server instances.
//!
//! The counter/histogram registry and the event sink live in
//! `rtflow_core::metrics`; this module adds the Prometheus exporter and the
//! HTTP handlers (`/metrics`, `/stats`, `/health`).

pub use rtflow_core::metrics::{Histogram, MetricsEventSink, MetricsRegistry, registry_from_sink};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

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
    pub async fn start(self) -> rtflow_core::error::Result<tokio::task::JoinHandle<()>> {
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
    write_gauge(
        &mut out,
        "rtflow_proxy_configured",
        "Configured RTSP proxy mounts",
        registry.proxy_configured.load(Ordering::Relaxed),
    );
    write_gauge(
        &mut out,
        "rtflow_proxy_streaming",
        "RTSP proxy mounts currently streaming",
        registry.proxy_streaming.load(Ordering::Relaxed),
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
    write_gauge(
        &mut out,
        "rtflow_srt_rtt_microseconds",
        "Last SRT round-trip time estimate",
        registry.srt_rtt_us.load(Ordering::Relaxed) as i64,
    );
    write_counter(
        &mut out,
        "rtflow_srt_loss_packets_total",
        "SRT packets detected as lost",
        registry.srt_loss_packets.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_srt_skipped_packets_total",
        "SRT packets skipped by TLPKTDROP",
        registry.srt_skipped_packets.load(Ordering::Relaxed),
    );
    write_counter(
        &mut out,
        "rtflow_srt_handshake_failures_total",
        "Rejected SRT handshakes",
        registry.srt_handshake_failures.load(Ordering::Relaxed),
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
pub fn collect_mount_metrics(mounts: &rtflow_mount::MountRegistry) -> Vec<MountMetrics> {
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
pub fn create_stats_handler(
    registry: Arc<MetricsRegistry>,
    mounts: Arc<rtflow_mount::MountRegistry>,
) -> rtflow_http::Handler {
    let handler: rtflow_http::Handler = Arc::new(move |_req, mut writer| {
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

/// Render Prometheus text for a registry.
///
/// Applications that expose additional metrics (path quotas, audit counters,
/// recording gauges) can append their own families to the returned text.
pub fn render_prometheus(registry: &MetricsRegistry) -> String {
    metrics_response(registry)
}

/// Create an HTTP handler that serves Prometheus metrics text.
pub fn create_metrics_handler(registry: Arc<MetricsRegistry>) -> rtflow_http::Handler {
    let handler: rtflow_http::Handler = Arc::new(move |_req, mut writer| {
        let registry = registry.clone();
        Box::pin(async move {
            let body = render_prometheus(&registry);
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
fn build_stats_json(registry: &MetricsRegistry, mounts: &rtflow_mount::MountRegistry) -> String {
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
        "\"srt_rtt_us\":{},",
        registry.srt_rtt_us.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"srt_loss_packets\":{},",
        registry.srt_loss_packets.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "\"srt_skipped_packets\":{},",
        registry.srt_skipped_packets.load(Ordering::Relaxed)
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
