use super::*;

#[derive(Clone, Debug, Default)]
pub struct StreamStats {
    pub accepted_frames: u64,
    pub dropped_frames: u64,
    pub subscribers: usize,
    pub rtp_packets: u64,
    pub rtp_octets: u64,
    pub rtcp_sender_reports: u64,
    pub rtcp_receiver_reports: u64,
}

#[derive(Clone, Debug)]
pub struct MountSnapshot {
    /// Unique RTSP play sessions (clients) on this mount.
    pub subscriber_count: usize,
    /// Current broadcast receivers (RTSP track subscriptions plus
    /// HTTP-FLV / WS-FLV / HLS subscribers). Publishers use the sum of
    /// both counters as their power gate so browser playback keeps the
    /// encoder running.
    pub broadcast_receivers: usize,
    pub publisher_connected: bool,
    #[cfg(feature = "server")]
    pub proxy_state: Option<ProxyState>,
    pub stats: StreamStats,
    pub startup_cache_frames: usize,
    pub startup_cache_bytes: usize,
}

// ---------------------------------------------------------------------------
// StreamCounters (internal)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct StreamCounters {
    pub accepted_frames: AtomicU64,
    pub dropped_frames: AtomicU64,
    pub rtp_packets: AtomicU64,
    pub rtp_octets: AtomicU64,
    pub rtcp_sender_reports: AtomicU64,
    pub rtcp_receiver_reports: AtomicU64,
    /// Optional global registry to mirror frame counters into
    /// (`rtflow_frames_accepted_total` / `rtflow_frames_dropped_total`).
    #[cfg(feature = "metrics")]
    global: Option<Arc<crate::core::metrics::MetricsRegistry>>,
}

impl StreamCounters {
    /// Mirror frame counters into a global metrics registry (once per mount).
    #[cfg(feature = "metrics")]
    pub fn attach_global(&mut self, registry: Arc<crate::core::metrics::MetricsRegistry>) {
        self.global = Some(registry);
    }

    pub fn add_accepted(&self, value: u64) {
        self.accepted_frames.fetch_add(value, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        if let Some(global) = &self.global {
            global.frames_accepted.fetch_add(value, Ordering::Relaxed);
        }
    }
    pub fn add_dropped(&self, value: u64) {
        self.dropped_frames.fetch_add(value, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        if let Some(global) = &self.global {
            global.frames_dropped.fetch_add(value, Ordering::Relaxed);
        }
    }
    #[allow(dead_code)] // called by feature-gated server/protocol code
    pub fn add_rtp(&self, packets: u64, octets: u64) {
        self.rtp_packets.fetch_add(packets, Ordering::Relaxed);
        self.rtp_octets.fetch_add(octets, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        if let Some(global) = &self.global {
            global
                .rtp_packets_sent
                .fetch_add(packets, Ordering::Relaxed);
            global.rtp_octets_sent.fetch_add(octets, Ordering::Relaxed);
        }
    }
    #[allow(dead_code)] // called by feature-gated server/protocol code
    pub fn add_sender_reports(&self, value: u64) {
        self.rtcp_sender_reports.fetch_add(value, Ordering::Relaxed);
    }
    #[allow(dead_code)] // called by feature-gated server/protocol code
    pub fn add_receiver_reports(&self, value: u64) {
        self.rtcp_receiver_reports
            .fetch_add(value, Ordering::Relaxed);
    }
    pub fn snapshot(&self, subscribers: usize) -> StreamStats {
        StreamStats {
            accepted_frames: self.accepted_frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            subscribers,
            rtp_packets: self.rtp_packets.load(Ordering::Relaxed),
            rtp_octets: self.rtp_octets.load(Ordering::Relaxed),
            rtcp_sender_reports: self.rtcp_sender_reports.load(Ordering::Relaxed),
            rtcp_receiver_reports: self.rtcp_receiver_reports.load(Ordering::Relaxed),
        }
    }
}
