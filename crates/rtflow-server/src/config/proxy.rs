//! RTSP pull-proxy configuration.
use super::ConfigError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Top-level `[proxy]` section.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProxySection {
    pub enabled: bool,
    #[serde(with = "humantime_serde")]
    pub idle_grace: Duration,
    pub max_proxies: usize,
    pub allow_private_networks: bool,
    pub allowed_hosts: Vec<String>,
    pub redirect_policy: RedirectPolicy,
    #[serde(with = "humantime_serde")]
    pub description_refresh: Duration,
    pub mounts: Vec<ProxyMount>,
}

impl Default for ProxySection {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_grace: Duration::from_secs(5),
            max_proxies: 128,
            allow_private_networks: true,
            allowed_hosts: Vec::new(),
            redirect_policy: RedirectPolicy::SameOrigin,
            description_refresh: Duration::from_secs(300),
            mounts: Vec::new(),
        }
    }
}

/// One configured proxy mount.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProxyMount {
    pub path: String,
    pub enabled: bool,
    pub mode: ProxyMode,
    #[serde(default, with = "humantime_serde")]
    pub idle_grace: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub description_refresh: Option<Duration>,
    pub upstreams: Vec<ProxyUpstream>,
}

/// Proxy connection mode.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RedirectPolicy {
    None,
    #[default]
    SameOrigin,
    Any,
}

impl RedirectPolicy {
    pub fn follow(self) -> bool {
        !matches!(self, Self::None)
    }
    pub fn cross_origin(self) -> bool {
        matches!(self, Self::Any)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyMode {
    #[default]
    OnDemand,
    Always,
}

impl ProxyMode {
    pub fn is_always(self) -> bool {
        matches!(self, Self::Always)
    }
}

impl Default for ProxyMount {
    fn default() -> Self {
        Self {
            path: String::new(),
            enabled: true,
            mode: ProxyMode::default(),
            idle_grace: None,
            description_refresh: None,
            upstreams: Vec::new(),
        }
    }
}

/// Upstream transport selection.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProxyTransport {
    #[default]
    Auto,
    Udp,
    Tcp,
}

/// One upstream RTSP endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProxyUpstream {
    pub url: String,
    pub transport: ProxyTransport,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub media_idle_timeout: Duration,
}

impl Default for ProxyUpstream {
    fn default() -> Self {
        Self {
            url: String::new(),
            transport: ProxyTransport::Auto,
            username: None,
            password: None,
            request_timeout: Duration::from_secs(5),
            media_idle_timeout: Duration::from_secs(15),
        }
    }
}

impl ProxyTransport {
    pub fn to_upstream(self) -> rtflow::mount::upstream::UpstreamTransport {
        match self {
            Self::Auto => rtflow::mount::upstream::UpstreamTransport::Auto,
            Self::Udp => rtflow::mount::upstream::UpstreamTransport::Udp,
            Self::Tcp => rtflow::mount::upstream::UpstreamTransport::TcpInterleaved,
        }
    }
}

impl ProxyMount {
    /// Validate this mount independently (used by the runtime admin API).
    pub fn validate(&self) -> Result<(), String> {
        if !self.path.starts_with('/') || self.path == "/" {
            return Err("proxy path must start with '/' and not be root".into());
        }
        if self.upstreams.is_empty() {
            return Err("proxy mount has no upstreams".into());
        }
        if self.idle_grace.is_some_and(|grace| grace.is_zero()) {
            return Err("proxy idle_grace is zero".into());
        }
        if self
            .description_refresh
            .is_some_and(|refresh| refresh.is_zero())
        {
            return Err("proxy description_refresh is zero".into());
        }
        for upstream in &self.upstreams {
            upstream.endpoint()?;
        }
        Ok(())
    }

    /// Build the runtime upstream configuration.
    pub fn upstream_config(&self) -> Result<rtflow::mount::upstream::UpstreamConfig, String> {
        self.upstream_config_with(RedirectPolicy::SameOrigin)
    }

    pub fn upstream_config_with(
        &self,
        redirect_policy: RedirectPolicy,
    ) -> Result<rtflow::mount::upstream::UpstreamConfig, String> {
        let first = self
            .upstreams
            .first()
            .ok_or_else(|| format!("proxy mount {} has no upstreams", self.path))?;
        let mut config = first.endpoint()?;
        let mut upstream =
            rtflow::mount::upstream::UpstreamConfig::new(config.url.clone(), config.transport);
        upstream.credentials = config.credentials.take();
        upstream.request_timeout = config.request_timeout;
        upstream.media_idle_timeout = config.media_idle_timeout;
        upstream.follow_redirects = redirect_policy.follow();
        upstream.allow_cross_origin_redirect = redirect_policy.cross_origin();
        for endpoint in self.upstreams.iter().skip(1) {
            upstream.add_fallback(endpoint.endpoint()?);
        }
        Ok(upstream)
    }
}

impl ProxyUpstream {
    pub fn endpoint(&self) -> Result<rtflow::mount::upstream::UpstreamEndpoint, String> {
        let url = url::Url::parse(&self.url)
            .map_err(|error| format!("invalid proxy upstream {:?}: {error}", self.url))?;
        if !matches!(url.scheme(), "rtsp" | "rtsps") {
            return Err(format!(
                "proxy upstream {:?} must use rtsp:// or rtsps://",
                self.url
            ));
        }
        if self.request_timeout.is_zero() {
            return Err("proxy upstream request_timeout is zero".into());
        }
        if self.media_idle_timeout.is_zero() {
            return Err("proxy upstream media_idle_timeout is zero".into());
        }
        let mut endpoint =
            rtflow::mount::upstream::UpstreamEndpoint::new(url, self.transport.to_upstream());
        endpoint.request_timeout = self.request_timeout;
        endpoint.media_idle_timeout = self.media_idle_timeout;
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            endpoint.credentials = Some(rtflow::auth::Credentials {
                username: username.clone(),
                password: password.clone(),
            });
        }
        Ok(endpoint)
    }
}

impl ProxySection {
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }
        if self.idle_grace.is_zero() {
            return Err(ConfigError::Invalid("proxy.idle_grace is zero".into()));
        }
        if self.description_refresh.is_zero() {
            return Err(ConfigError::Invalid(
                "proxy.description_refresh is zero".into(),
            ));
        }
        if self.max_proxies == 0 {
            return Err(ConfigError::Invalid("proxy.max_proxies is zero".into()));
        }
        for pattern in &self.allowed_hosts {
            validate_host_pattern(pattern)?;
        }
        let mut paths = std::collections::HashSet::new();
        for (index, mount) in self.mounts.iter().enumerate() {
            if !paths.insert(mount.path.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate proxy mount {}",
                    mount.path
                )));
            }
            mount
                .validate()
                .map_err(|error| ConfigError::Invalid(format!("proxy.mounts[{index}]: {error}")))?;
        }
        Ok(())
    }

    pub(super) fn redact(&mut self) {
        for mount in &mut self.mounts {
            for upstream in &mut mount.upstreams {
                if upstream.password.is_some() {
                    upstream.password = Some("<redacted>".into());
                }
                if let Ok(mut url) = url::Url::parse(&upstream.url)
                    && url.password().is_some()
                {
                    let _ = url.set_password(Some("redacted"));
                    upstream.url = url.into();
                }
            }
        }
    }
}

fn validate_host_pattern(pattern: &str) -> Result<(), ConfigError> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err(ConfigError::Invalid(
            "proxy.allowed_hosts contains an empty entry".into(),
        ));
    }
    if pattern.contains('/') {
        pattern
            .parse::<rtflow::security::IpCidr>()
            .map_err(|error| ConfigError::Invalid(format!("proxy.allowed_hosts: {error}")))?;
    }
    Ok(())
}
