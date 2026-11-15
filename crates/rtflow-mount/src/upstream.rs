//! Upstream proxy configuration and supervision state.
//!
//! These are plain value types shared with the mount registry (which owns the
//! proxy state of a mount); the runtime that drives them lives in
//! `rtflow_rtsp_server::proxy_runtime`.
pub use super::continuity::{
    ContinuityPolicy, KeyframeCache, StartupCacheConfig, TimelineNormalizer, TimelineSnapshot,
    TrackCompatibility,
};
use rtflow_media::MediaDescription;
use rtflow_rtp::JitterBufferConfig;
use rtflow_rtsp::auth::Credentials;
pub use rtflow_rtsp::transport::UpstreamTransport;
#[cfg(feature = "tls")]
use rtflow_security::TlsClientConfig;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct UpstreamEndpoint {
    pub url: url::Url,
    pub transport: UpstreamTransport,
    pub credentials: Option<Credentials>,
    pub request_timeout: Duration,
    pub jitter: JitterBufferConfig,
    #[cfg(feature = "tls")]
    pub tls: Option<TlsClientConfig>,
}

impl UpstreamEndpoint {
    pub fn new(url: url::Url, transport: UpstreamTransport) -> Self {
        Self {
            url,
            transport,
            credentials: None,
            request_timeout: Duration::from_secs(5),
            jitter: JitterBufferConfig::default(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailoverStrategy {
    Ordered,
    RoundRobin,
}

#[derive(Clone, Debug)]
pub struct FailoverPolicy {
    pub strategy: FailoverStrategy,
    pub failure_threshold: u32,
    pub circuit_open_duration: Duration,
}

impl Default for FailoverPolicy {
    fn default() -> Self {
        Self {
            strategy: FailoverStrategy::Ordered,
            failure_threshold: 1,
            circuit_open_duration: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct UpstreamHealth {
    pub consecutive_failures: u32,
    pub circuit_open_until: Option<Instant>,
    pub last_error: Option<rtflow_core::error::ErrorCode>,
}

impl UpstreamHealth {
    pub fn is_available(&self, now: Instant) -> bool {
        self.circuit_open_until.is_none_or(|until| now >= until)
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.circuit_open_until = None;
        self.last_error = None;
    }

    pub fn record_failure(
        &mut self,
        code: rtflow_core::error::ErrorCode,
        policy: &FailoverPolicy,
        now: Instant,
    ) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error = Some(code);
        if self.consecutive_failures >= policy.failure_threshold.max(1) {
            self.circuit_open_until = Some(now + policy.circuit_open_duration);
        }
    }
}

pub trait UpstreamSelector: Send + Sync {
    fn select(
        &self,
        endpoints: &[UpstreamEndpoint],
        health: &[UpstreamHealth],
        attempt: u32,
        now: Instant,
    ) -> Option<usize>;
}

#[derive(Clone, Debug)]
pub struct PolicySelector {
    pub strategy: FailoverStrategy,
}

impl UpstreamSelector for PolicySelector {
    fn select(
        &self,
        endpoints: &[UpstreamEndpoint],
        health: &[UpstreamHealth],
        attempt: u32,
        now: Instant,
    ) -> Option<usize> {
        if endpoints.is_empty() || endpoints.len() != health.len() {
            return None;
        }
        let start = match self.strategy {
            FailoverStrategy::Ordered => 0,
            FailoverStrategy::RoundRobin => attempt as usize % endpoints.len(),
        };
        (0..endpoints.len())
            .map(|offset| (start + offset) % endpoints.len())
            .find(|index| health[*index].is_available(now))
    }
}

#[derive(Clone, Debug)]
pub struct ProxyRoute {
    pub upstreams: Vec<UpstreamEndpoint>,
    pub reconnect: ReconnectPolicy,
    pub failover: FailoverPolicy,
    pub continuity: ContinuityPolicy,
    pub startup_cache: StartupCacheConfig,
    pub cached_description: Option<MediaDescription>,
}

impl ProxyRoute {
    pub fn new(primary: UpstreamEndpoint) -> Self {
        Self {
            upstreams: vec![primary],
            reconnect: ReconnectPolicy::default(),
            failover: FailoverPolicy::default(),
            continuity: ContinuityPolicy::default(),
            startup_cache: StartupCacheConfig::default(),
            cached_description: None,
        }
    }

    pub fn add_fallback(mut self, endpoint: UpstreamEndpoint) -> Self {
        self.upstreams.push(endpoint);
        self
    }

    pub fn validate(&self) -> rtflow_core::error::Result<()> {
        if self.upstreams.is_empty() {
            return Err(rtflow_core::error::Error::Limit("proxy upstreams"));
        }
        if self.failover.failure_threshold == 0
            || self.failover.circuit_open_duration == Duration::ZERO
        {
            return Err(rtflow_core::error::Error::Limit("proxy circuit breaker"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub multiplier: f32,
    pub jitter_ratio: f32,
}
impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
            jitter_ratio: 0.2,
        }
    }
}
#[derive(Clone, Debug)]
pub enum ProxyState {
    Idle,
    Connecting,
    Streaming,
    BackingOff { retry_at: Instant, attempts: u32 },
    Stopped,
}
fn random_jitter_sample() -> u16 {
    let mut bytes = [0u8; 2];
    if getrandom::getrandom(&mut bytes).is_ok() {
        u16::from_le_bytes(bytes) % 2_001
    } else {
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_millis() as u16
            * 2)
            % 2_001
    }
}

#[derive(Clone)]
pub struct UpstreamConfig {
    pub url: url::Url,
    pub transport: UpstreamTransport,
    pub credentials: Option<Credentials>,
    pub reconnect: ReconnectPolicy,
    pub request_timeout: Duration,
    pub jitter: JitterBufferConfig,
    #[cfg(feature = "tls")]
    pub tls: Option<TlsClientConfig>,
    pub cached_description: Option<MediaDescription>,
    pub continuity: ContinuityPolicy,
    pub startup_cache: StartupCacheConfig,
    pub fallbacks: Vec<UpstreamEndpoint>,
    pub failover: FailoverPolicy,
    state: Arc<Mutex<ProxyState>>,
}
impl UpstreamConfig {
    pub fn new(url: url::Url, transport: UpstreamTransport) -> Self {
        Self {
            url,
            transport,
            credentials: None,
            reconnect: ReconnectPolicy::default(),
            request_timeout: Duration::from_secs(5),
            jitter: JitterBufferConfig::default(),
            #[cfg(feature = "tls")]
            tls: None,
            cached_description: None,
            continuity: ContinuityPolicy::default(),
            startup_cache: StartupCacheConfig::default(),
            fallbacks: Vec::new(),
            failover: FailoverPolicy::default(),
            state: Arc::new(Mutex::new(ProxyState::Idle)),
        }
    }
    pub fn add_fallback(&mut self, endpoint: UpstreamEndpoint) {
        self.fallbacks.push(endpoint);
    }
    pub fn endpoints(&self) -> Vec<UpstreamEndpoint> {
        let mut endpoints = Vec::with_capacity(self.fallbacks.len() + 1);
        endpoints.push(UpstreamEndpoint {
            url: self.url.clone(),
            transport: self.transport,
            credentials: self.credentials.clone(),
            request_timeout: self.request_timeout,
            jitter: self.jitter.clone(),
            #[cfg(feature = "tls")]
            tls: self.tls.clone(),
        });
        endpoints.extend(self.fallbacks.iter().cloned());
        endpoints
    }
    pub fn state(&self) -> ProxyState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn set_state(&self, state: ProxyState) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    }
    pub fn next_retry(&self, attempts: u32) -> Duration {
        let ms = (self.reconnect.initial_delay.as_millis() as f32
            * self
                .reconnect
                .multiplier
                .powi(attempts.saturating_sub(1).min(30) as i32)) as u128;
        let capped = ms.min(self.reconnect.max_delay.as_millis()) as f32;
        let ratio = self.reconnect.jitter_ratio.clamp(0.0, 1.0);
        let sample = random_jitter_sample();
        let centered = f32::from(sample) / 1_000.0 - 1.0;
        Duration::from_millis(
            (capped * (1.0 + centered * ratio))
                .max(0.0)
                .min(self.reconnect.max_delay.as_millis() as f32) as u64,
        )
    }
}

impl TryFrom<ProxyRoute> for UpstreamConfig {
    type Error = rtflow_core::error::Error;

    fn try_from(mut route: ProxyRoute) -> Result<Self, Self::Error> {
        route.validate()?;
        let primary = route.upstreams.remove(0);
        let mut config = Self::new(primary.url, primary.transport);
        config.credentials = primary.credentials;
        config.request_timeout = primary.request_timeout;
        config.jitter = primary.jitter;
        #[cfg(feature = "tls")]
        {
            config.tls = primary.tls;
        }
        config.fallbacks = route.upstreams;
        config.reconnect = route.reconnect;
        config.failover = route.failover;
        config.continuity = route.continuity;
        config.startup_cache = route.startup_cache;
        config.cached_description = route.cached_description;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_is_jittered_and_capped() {
        let mut upstream = UpstreamConfig::new(
            url::Url::parse("rtsp://127.0.0.1/source").unwrap(),
            UpstreamTransport::Udp,
        );
        upstream.reconnect.initial_delay = Duration::from_millis(100);
        upstream.reconnect.max_delay = Duration::from_millis(500);
        upstream.reconnect.jitter_ratio = 0.2;
        let first = upstream.next_retry(1);
        assert!((80..=120).contains(&first.as_millis()));
        assert!(upstream.next_retry(20) <= Duration::from_millis(500));
    }
}
