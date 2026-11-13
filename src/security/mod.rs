//! Network admission controls independent of a particular management plane.
#[cfg(feature = "tls")]
mod tls;

use crate::{Error, Result};
#[cfg(any(feature = "server", feature = "http"))]
use std::{collections::HashMap, sync::Mutex, time::Instant};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
    time::Duration,
};
#[cfg(feature = "tls")]
pub use tls::{TlsClientConfig, TlsServerConfig};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IpCidr {
    V4 { network: u32, prefix: u8 },
    V6 { network: u128, prefix: u8 },
}

impl IpCidr {
    pub fn contains(self, address: IpAddr) -> bool {
        match (self, address) {
            (Self::V4 { network, prefix }, IpAddr::V4(address)) => {
                masked_v4(u32::from(address), prefix) == network
            }
            (Self::V6 { network, prefix }, IpAddr::V6(address)) => {
                masked_v6(u128::from(address), prefix) == network
            }
            _ => false,
        }
    }
}

impl FromStr for IpCidr {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let (address, prefix) = value
            .split_once('/')
            .ok_or_else(|| Error::InvalidConfig("CIDR prefix is missing".into()))?;
        let address = IpAddr::from_str(address)
            .map_err(|_| Error::InvalidConfig("invalid CIDR address".into()))?;
        let prefix = prefix
            .parse::<u8>()
            .map_err(|_| Error::InvalidConfig("invalid CIDR prefix".into()))?;
        match address {
            IpAddr::V4(address) if prefix <= 32 => Ok(Self::V4 {
                network: masked_v4(u32::from(address), prefix),
                prefix,
            }),
            IpAddr::V6(address) if prefix <= 128 => Ok(Self::V6 {
                network: masked_v6(u128::from(address), prefix),
                prefix,
            }),
            _ => Err(Error::InvalidConfig("CIDR prefix is out of range".into())),
        }
    }
}

impl From<Ipv4Addr> for IpCidr {
    fn from(address: Ipv4Addr) -> Self {
        Self::V4 {
            network: u32::from(address),
            prefix: 32,
        }
    }
}

impl From<Ipv6Addr> for IpCidr {
    fn from(address: Ipv6Addr) -> Self {
        Self::V6 {
            network: u128::from(address),
            prefix: 128,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct IpAcl {
    pub allow: Vec<IpCidr>,
    pub deny: Vec<IpCidr>,
}

impl IpAcl {
    pub fn permits(&self, address: IpAddr) -> bool {
        if self.deny.iter().any(|network| network.contains(address)) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|network| network.contains(address))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RateLimitConfig {
    pub burst: u32,
    pub refill_per_second: f64,
    pub idle_retention: Duration,
    pub max_tracked_peers: usize,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            burst: 32,
            refill_per_second: 8.0,
            idle_retention: Duration::from_secs(300),
            max_tracked_peers: 65_536,
        }
    }
}

impl RateLimitConfig {
    pub fn validate(self) -> Result<()> {
        if self.burst == 0
            || !self.refill_per_second.is_finite()
            || self.refill_per_second <= 0.0
            || self.idle_retention == Duration::ZERO
            || self.max_tracked_peers == 0
        {
            return Err(Error::Limit("connection rate limit"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SecurityConfig {
    pub ip_acl: IpAcl,
    pub connection_rate_limit: Option<RateLimitConfig>,
    pub authentication_failure_delay: Duration,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            ip_acl: IpAcl::default(),
            connection_rate_limit: None,
            authentication_failure_delay: Duration::from_millis(25),
        }
    }
}

#[cfg(any(feature = "server", feature = "http"))]
struct Bucket {
    tokens: f64,
    updated_at: Instant,
}

#[cfg(any(feature = "server", feature = "http"))]
pub(crate) struct ConnectionRateLimiter {
    config: Option<RateLimitConfig>,
    state: Mutex<LimiterState>,
}

#[cfg(any(feature = "server", feature = "http"))]
struct LimiterState {
    buckets: HashMap<IpAddr, Bucket>,
    last_cleanup: Instant,
}

#[cfg(any(feature = "server", feature = "http"))]
impl ConnectionRateLimiter {
    pub(crate) fn new(config: Option<RateLimitConfig>) -> Result<Self> {
        if let Some(config) = config {
            config.validate()?;
        }
        Ok(Self {
            config,
            state: Mutex::new(LimiterState {
                buckets: HashMap::new(),
                last_cleanup: Instant::now(),
            }),
        })
    }

    pub(crate) fn admit(&self, address: IpAddr, now: Instant) -> bool {
        let Some(config) = self.config else {
            return true;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.saturating_duration_since(state.last_cleanup) >= config.idle_retention / 2 {
            state.buckets.retain(|_, bucket| {
                now.saturating_duration_since(bucket.updated_at) <= config.idle_retention
            });
            state.last_cleanup = now;
        }
        if !state.buckets.contains_key(&address) && state.buckets.len() >= config.max_tracked_peers
        {
            return false;
        }
        let bucket = state.buckets.entry(address).or_insert(Bucket {
            tokens: f64::from(config.burst),
            updated_at: now,
        });
        let elapsed = now
            .saturating_duration_since(bucket.updated_at)
            .as_secs_f64();
        bucket.tokens =
            (bucket.tokens + elapsed * config.refill_per_second).min(f64::from(config.burst));
        bucket.updated_at = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }
}

fn masked_v4(address: u32, prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        address & (u32::MAX << (32 - prefix))
    }
}

fn masked_v6(address: u128, prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        address & (u128::MAX << (128 - prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_acl_handles_v4_v6_and_deny_precedence() {
        let acl = IpAcl {
            allow: vec![
                "10.0.0.0/8".parse().unwrap(),
                "2001:db8::/32".parse().unwrap(),
            ],
            deny: vec!["10.1.0.0/16".parse().unwrap()],
        };
        assert!(acl.permits("10.2.3.4".parse().unwrap()));
        assert!(!acl.permits("10.1.3.4".parse().unwrap()));
        assert!(acl.permits("2001:db8::1".parse().unwrap()));
        assert!(!acl.permits("192.168.1.1".parse().unwrap()));
    }

    #[cfg(any(feature = "server", feature = "http"))]
    #[test]
    fn token_bucket_refills_deterministically() {
        use std::time::Instant;
        let limiter = ConnectionRateLimiter::new(Some(RateLimitConfig {
            burst: 2,
            refill_per_second: 10.0,
            idle_retention: Duration::from_secs(1),
            max_tracked_peers: 16,
        }))
        .unwrap();
        let address = "127.0.0.1".parse().unwrap();
        let start = Instant::now();
        assert!(limiter.admit(address, start));
        assert!(limiter.admit(address, start));
        assert!(!limiter.admit(address, start));
        assert!(limiter.admit(address, start + Duration::from_millis(100)));
    }
}
