//! On-demand upstream proxy supervision.
use crate::{auth::Credentials, media::MediaDescription, rtp::JitterBufferConfig};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamTransport {
    Udp,
    TcpInterleaved,
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
#[derive(Clone)]
pub struct UpstreamConfig {
    pub url: url::Url,
    pub transport: UpstreamTransport,
    pub credentials: Option<Credentials>,
    pub reconnect: ReconnectPolicy,
    pub request_timeout: Duration,
    pub jitter: JitterBufferConfig,
    pub cached_description: Option<MediaDescription>,
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
            cached_description: None,
            state: Arc::new(Mutex::new(ProxyState::Idle)),
        }
    }
    pub fn state(&self) -> ProxyState {
        self.state.lock().unwrap().clone()
    }
    pub fn set_state(&self, state: ProxyState) {
        *self.state.lock().unwrap() = state;
    }
    pub fn next_retry(&self, attempts: u32) -> Duration {
        let ms = (self.reconnect.initial_delay.as_millis() as f32
            * self
                .reconnect
                .multiplier
                .powi(attempts.saturating_sub(1).min(30) as i32)) as u128;
        let capped = ms.min(self.reconnect.max_delay.as_millis()) as f32;
        let ratio = self.reconnect.jitter_ratio.clamp(0.0, 1.0);
        let sample = attempts.wrapping_mul(1_103_515_245).wrapping_add(12_345) % 2_001;
        let centered = sample as f32 / 1_000.0 - 1.0;
        Duration::from_millis(
            (capped * (1.0 + centered * ratio))
                .max(0.0)
                .min(self.reconnect.max_delay.as_millis() as f32) as u64,
        )
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
