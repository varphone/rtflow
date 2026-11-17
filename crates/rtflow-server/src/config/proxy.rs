//! RTSP pull-proxy configuration.
use super::ConfigError;
use chrono::{DateTime, Datelike, Local, NaiveTime};
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
    pub max_upstreams_per_proxy: usize,
    pub describe_concurrency: usize,
    pub allow_private_networks: bool,
    pub allowed_hosts: Vec<String>,
    pub redirect_policy: RedirectPolicy,
    #[serde(with = "humantime_serde")]
    pub description_refresh: Duration,
    /// Persist runtime API changes to `state_dir/proxies.toml` (0600).
    pub persist_runtime: bool,
    pub mounts: Vec<ProxyMount>,
}

impl Default for ProxySection {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_grace: Duration::from_secs(5),
            max_proxies: 128,
            max_upstreams_per_proxy: 4,
            describe_concurrency: 8,
            allow_private_networks: true,
            allowed_hosts: Vec::new(),
            redirect_policy: RedirectPolicy::SameOrigin,
            description_refresh: Duration::from_secs(300),
            persist_runtime: false,
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
    /// When present, the pull source owns a managed path policy at `path`
    /// (live outputs, recording and reader limit). `None` keeps the legacy
    /// behaviour where path policy is configured separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<ProxyOutputs>,
    /// Optional weekly pull window (server-local time). `None` pulls whenever
    /// the source is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ProxySchedule>,
    pub upstreams: Vec<ProxyUpstream>,
}

/// Live/recording outputs provisioned for a pull-source mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProxyOutputs {
    /// Allow RTSP/read access to the pulled mount.
    pub read: bool,
    /// HLS live output.
    pub hls: bool,
    /// DASH live output.
    pub dash: bool,
    /// HTTP-FLV / WS-FLV live output.
    pub flv: bool,
    /// WebRTC live output.
    pub webrtc: bool,
    /// Segment recording; requires `[recording] enabled` and `auto_start`.
    pub record: bool,
    /// 0 means unlimited.
    pub max_readers: usize,
}

impl Default for ProxyOutputs {
    fn default() -> Self {
        Self {
            read: true,
            hls: true,
            dash: false,
            flv: true,
            webrtc: false,
            record: false,
            max_readers: 0,
        }
    }
}

impl ProxyOutputs {
    /// Any protocol view enabled for the pulled stream.
    pub fn live_enabled(self) -> bool {
        self.hls || self.dash || self.flv || self.webrtc
    }
}

/// Weekly pull window in the server's local time zone.
///
/// `start == end` is rejected. A window whose end is before its start spans
/// midnight (for example Mon 22:00–06:00 runs into Tuesday).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct ProxySchedule {
    pub days: Vec<ScheduleDay>,
    /// `HH:MM` in 24-hour local time.
    pub start: String,
    /// `HH:MM` in 24-hour local time; exclusive.
    pub end: String,
}

#[derive(Clone, Copy, Debug, Hash, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleDay {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl ScheduleDay {
    fn from_weekday(day: chrono::Weekday) -> Self {
        match day {
            chrono::Weekday::Mon => Self::Mon,
            chrono::Weekday::Tue => Self::Tue,
            chrono::Weekday::Wed => Self::Wed,
            chrono::Weekday::Thu => Self::Thu,
            chrono::Weekday::Fri => Self::Fri,
            chrono::Weekday::Sat => Self::Sat,
            chrono::Weekday::Sun => Self::Sun,
        }
    }
}

fn parse_clock(value: &str) -> Result<NaiveTime, String> {
    let (hour, minute) = value
        .split_once(':')
        .ok_or_else(|| "expected HH:MM".to_string())?;
    if hour.is_empty() || hour.len() > 2 || minute.len() != 2 {
        return Err("expected HH:MM".into());
    }
    let hour: u32 = hour.parse().map_err(|_| "invalid hour".to_string())?;
    let minute: u32 = minute.parse().map_err(|_| "invalid minute".to_string())?;
    NaiveTime::from_hms_opt(hour, minute, 0).ok_or_else(|| "invalid time".into())
}

impl ProxySchedule {
    pub fn validate(&self) -> Result<(), String> {
        if self.days.is_empty() {
            return Err("schedule.days is empty".into());
        }
        let mut seen = std::collections::HashSet::new();
        for day in &self.days {
            if !seen.insert(*day) {
                return Err(format!("schedule.days contains duplicate {day:?}"));
            }
        }
        let start = parse_clock(&self.start).map_err(|error| format!("schedule.start: {error}"))?;
        let end = parse_clock(&self.end).map_err(|error| format!("schedule.end: {error}"))?;
        if start == end {
            return Err("schedule.start equals schedule.end".into());
        }
        Ok(())
    }

    /// Whether `now` falls inside the weekly window (start inclusive, end
    /// exclusive). Overnight windows continue into the following day.
    pub fn allows(&self, now: DateTime<Local>) -> bool {
        let (Ok(start), Ok(end)) = (parse_clock(&self.start), parse_clock(&self.end)) else {
            return false;
        };
        let today = ScheduleDay::from_weekday(now.weekday());
        let yesterday = ScheduleDay::from_weekday(now.weekday().pred());
        let time = now.time();
        if start < end {
            self.days.contains(&today) && time >= start && time < end
        } else {
            (self.days.contains(&today) && time >= start)
                || (self.days.contains(&yesterday) && time < end)
        }
    }
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
            outputs: None,
            schedule: None,
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
        if self.outputs.is_some_and(|outputs| {
            !outputs.read
                && !outputs.hls
                && !outputs.dash
                && !outputs.flv
                && !outputs.webrtc
                && !outputs.record
        }) {
            return Err("proxy outputs enable neither live view nor recording".into());
        }
        if let Some(schedule) = &self.schedule {
            schedule
                .validate()
                .map_err(|error| format!("proxy schedule: {error}"))?;
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
        if self.describe_concurrency == 0 {
            return Err(ConfigError::Invalid(
                "proxy.describe_concurrency is zero".into(),
            ));
        }
        if self.max_upstreams_per_proxy == 0 {
            return Err(ConfigError::Invalid(
                "proxy.max_upstreams_per_proxy is zero".into(),
            ));
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
