//! TOML configuration for `rtflow-server`.
//!
//! Every section has a working default; a config file only overrides what an
//! operator needs. Unknown keys are rejected so a typo fails at startup
//! instead of silently disabling a feature.
use serde::{Deserialize, Serialize};

mod proxy;
mod srt;
pub use proxy::{ProxyMode, ProxyMount, ProxySection, ProxyTransport, ProxyUpstream};
pub use srt::{SrtPull, SrtPush, SrtSection};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

/// Top-level configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: ServerSection,
    pub log: LogSection,
    pub rtsp: RtspSection,
    pub rtmp: RtmpSection,
    pub srt: SrtSection,
    pub webrtc: WebrtcSection,
    pub http: HttpSection,
    pub hls: HlsSection,
    pub dash: DashSection,
    pub flv: FlvSection,
    pub recording: RecordingSection,
    pub auth: AuthSection,
    pub keys: KeysSection,
    pub hooks: HooksSection,
    pub audit: AuditSection,
    pub discovery: DiscoverySection,
    pub proxy: ProxySection,
    /// Static path rules. Keys are globs (`live/*`, `private/**`).
    pub paths: BTreeMap<String, PathRule>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSection {
    pub name: String,
    #[serde(with = "humantime_serde")]
    pub shutdown_grace: Duration,
    pub state_dir: PathBuf,
    pub public_rtsp: Option<String>,
    pub public_rtmp: Option<String>,
    pub public_srt: Option<String>,
    pub public_http: Option<String>,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            name: "rtflow".into(),
            shutdown_grace: Duration::from_secs(10),
            state_dir: PathBuf::from("./state"),
            public_rtsp: None,
            public_rtmp: None,
            public_srt: None,
            public_http: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogSection {
    pub level: String,
    pub format: LogFormat,
    pub access_log: bool,
}

impl Default for LogSection {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::Text,
            access_log: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RtspSection {
    pub enabled: bool,
    pub listen: SocketAddr,
    /// Optional TLS listener files. RTSP over TLS is used by browsers and
    /// some ONVIF clients; HTTP TLS is expected to terminate at a proxy.
    pub tls: Option<TlsFiles>,
    #[serde(with = "humantime_serde")]
    pub session_timeout: Duration,
    pub max_sessions: usize,
    pub dynamic_publishers: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFiles {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}

impl Default for RtspSection {
    fn default() -> Self {
        Self {
            enabled: true,
            tls: None,
            listen: "0.0.0.0:8554".parse().expect("valid default"),
            session_timeout: Duration::from_secs(60),
            max_sessions: 256,
            dynamic_publishers: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RtmpSection {
    pub enabled: bool,
    pub listen: SocketAddr,
    pub max_connections: usize,
}

impl Default for RtmpSection {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "0.0.0.0:1935".parse().expect("valid default"),
            max_connections: 1024,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebrtcSection {
    pub enabled: bool,
}

impl Default for WebrtcSection {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct HttpSection {
    pub listen: SocketAddr,
    /// Optional HTTPS listener files. Without them the server speaks plain
    /// HTTP and TLS is expected to terminate at a proxy.
    pub tls: Option<TlsFiles>,
    pub admin: bool,
    pub metrics: bool,
    pub health: bool,
    pub admin_username: String,
    /// Empty disables the console (no insecure default password).
    pub admin_password: String,
    /// Static bearer tokens accepted by the control API (`Authorization:
    /// Bearer ...` or `?token=`), in addition to Basic/session.
    pub api_tokens: Vec<String>,
}

impl Default for HttpSection {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".parse().expect("valid default"),
            tls: None,
            admin: true,
            metrics: true,
            health: true,
            admin_username: "admin".into(),
            admin_password: String::new(),
            api_tokens: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct HlsSection {
    pub enabled: bool,
    #[serde(with = "humantime_serde")]
    pub segment_duration: Duration,
    pub window: usize,
    pub low_latency: bool,
}

impl Default for HlsSection {
    fn default() -> Self {
        Self {
            enabled: true,
            segment_duration: Duration::from_secs(1),
            window: 7,
            low_latency: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DashSection {
    pub enabled: bool,
}

impl Default for DashSection {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct FlvSection {
    pub enabled: bool,
    pub ws_listen: SocketAddr,
}

impl Default for FlvSection {
    fn default() -> Self {
        Self {
            enabled: true,
            ws_listen: "0.0.0.0:8088".parse().expect("valid default"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RecordingSection {
    pub enabled: bool,
    pub root: PathBuf,
    #[serde(with = "humantime_serde")]
    pub segment_duration: Duration,
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
    pub max_disk_bytes: u64,
    pub auto_start: bool,
}

impl Default for RecordingSection {
    fn default() -> Self {
        Self {
            enabled: false,
            root: PathBuf::from("./recordings"),
            segment_duration: Duration::from_secs(10),
            retention: Duration::from_secs(7 * 24 * 3600),
            max_disk_bytes: 50 * 1024 * 1024 * 1024,
            auto_start: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthSection {
    pub mode: AuthMode,
    pub realm: String,
    pub token_secrets: Vec<String>,
    pub enforce_path_rules: bool,
    pub users: Vec<UserConfig>,
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            mode: AuthMode::None,
            realm: "rtflow".to_owned(),
            token_secrets: Vec::new(),
            enforce_path_rules: true,
            users: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    #[default]
    None,
    Basic,
    Digest,
    Token,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    pub username: String,
    /// Plaintext or `$argon2id$...` PHC string.
    pub password: String,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct KeysSection {
    #[serde(with = "humantime_serde")]
    pub default_ttl: Duration,
    pub max_per_path: usize,
}

impl Default for KeysSection {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(24 * 3600),
            max_per_path: 10,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct HooksSection {
    pub on_publish_auth: Option<String>,
    pub on_play_auth: Option<String>,
    pub on_publish_done: Option<String>,
    pub on_play_done: Option<String>,
    pub on_record_done: Option<String>,
    pub secret: Option<String>,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    pub fail_closed: bool,
}

impl Default for HooksSection {
    fn default() -> Self {
        Self {
            on_publish_auth: None,
            on_play_auth: None,
            on_publish_done: None,
            on_play_done: None,
            on_record_done: None,
            secret: None,
            timeout: Duration::from_secs(2),
            fail_closed: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuditSection {
    pub enabled: bool,
    /// Defaults to `<state_dir>/audit.log` when empty.
    pub path: Option<PathBuf>,
    pub max_bytes: u64,
    pub max_files: usize,
}

impl Default for AuditSection {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            max_bytes: 64 * 1024 * 1024,
            max_files: 5,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiscoverySection {
    pub public: bool,
    pub token: Option<String>,
    pub compat_mediamtx: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathRule {
    pub enabled: bool,
    pub publish: bool,
    pub read: bool,
    pub record: bool,
    pub hls: bool,
    pub dash: bool,
    pub flv: bool,
    pub webrtc: bool,
    pub max_readers: usize,
    pub tenant: Option<String>,
    pub labels: BTreeMap<String, String>,
}

impl Default for PathRule {
    fn default() -> Self {
        Self {
            enabled: true,
            publish: true,
            read: true,
            record: false,
            hls: true,
            dash: true,
            flv: true,
            webrtc: true,
            max_readers: 0,
            tenant: None,
            labels: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Parse and validate a TOML file. `${ENV}` references are expanded first.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|error| ConfigError::Read(path.display().to_string(), error.to_string()))?;
        let text = expand_env(&text)?;
        let mut config: Self = toml::from_str(&text)
            .map_err(|error| ConfigError::Parse(path.display().to_string(), error.to_string()))?;
        config.apply_defaults();
        config.validate()?;
        Ok(config)
    }

    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(text)
            .map_err(|error| ConfigError::Parse("<inline>".into(), error.to_string()))?;
        config.apply_defaults();
        config.validate()?;
        Ok(config)
    }

    /// Copy safe to print: secrets are replaced with a placeholder so
    /// `--print-config` never leaks credentials into logs or tickets.
    pub fn redacted(&self) -> Self {
        let mut copy = self.clone();
        if !copy.http.admin_password.is_empty() {
            copy.http.admin_password = "<redacted>".into();
        }
        for secret in &mut copy.auth.token_secrets {
            *secret = "<redacted>".into();
        }
        for user in &mut copy.auth.users {
            user.password = "<redacted>".into();
        }
        for token in &mut copy.http.api_tokens {
            *token = "<redacted>".into();
        }
        if copy.hooks.secret.is_some() {
            copy.hooks.secret = Some("<redacted>".into());
        }
        if copy.discovery.token.is_some() {
            copy.discovery.token = Some("<redacted>".into());
        }
        copy.proxy.redact();
        copy
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|error| ConfigError::Serialize(error.to_string()))
    }

    /// Fill runtime defaults that depend on other sections.
    fn apply_defaults(&mut self) {
        // `full` configuration defaults are applied by serde; normalize
        // env-derived admin credentials and path defaults here.
        if self.auth.realm.is_empty() {
            self.auth.realm = "rtflow".into();
        }
        if self.auth.mode == AuthMode::None {
            self.auth.enforce_path_rules = false;
        }
        if self.paths.is_empty() {
            self.paths.insert("**".into(), PathRule::default());
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.name.trim().is_empty() {
            return Err(ConfigError::Invalid("server.name is empty".into()));
        }
        if self.server.shutdown_grace.is_zero() {
            return Err(ConfigError::Invalid("server.shutdown_grace is zero".into()));
        }
        if self.rtsp.enabled && self.rtsp.max_sessions == 0 {
            return Err(ConfigError::Invalid("rtsp.max_sessions is zero".into()));
        }
        if self.hls.enabled && (self.hls.window < 3 || self.hls.segment_duration.is_zero()) {
            return Err(ConfigError::Invalid(
                "hls.window must be >= 3 and segment_duration non-zero".into(),
            ));
        }
        if self.recording.enabled {
            if self.recording.segment_duration.is_zero() {
                return Err(ConfigError::Invalid(
                    "recording.segment_duration is zero".into(),
                ));
            }
            if self.recording.max_disk_bytes == 0 {
                return Err(ConfigError::Invalid(
                    "recording.max_disk_bytes is zero".into(),
                ));
            }
        }
        if let Some(passphrase) = &self.srt.passphrase
            && !(10..=80).contains(&passphrase.len())
        {
            return Err(ConfigError::Invalid(
                "srt.passphrase must be 10..80 bytes".into(),
            ));
        }
        for pull in &self.srt.pull {
            if pull.remote.parse::<SocketAddr>().is_err() {
                return Err(ConfigError::Invalid(format!(
                    "srt.pull remote {:?} is not host:port",
                    pull.remote
                )));
            }
            if pull
                .mount
                .as_deref()
                .is_some_and(|mount| !mount.starts_with('/'))
            {
                return Err(ConfigError::Invalid(
                    "srt.pull mount must start with '/'".into(),
                ));
            }
            if let Some(passphrase) = &pull.passphrase
                && !(10..=80).contains(&passphrase.len())
            {
                return Err(ConfigError::Invalid(
                    "srt.pull passphrase must be 10..80 bytes".into(),
                ));
            }
            if !matches!(pull.pbkeylen, 0 | 16 | 24 | 32) {
                return Err(ConfigError::Invalid(
                    "srt.pull pbkeylen must be 0, 16, 24 or 32".into(),
                ));
            }
        }
        for push in &self.srt.push {
            if !push.mount.starts_with('/') {
                return Err(ConfigError::Invalid(
                    "srt.push mount must start with '/'".into(),
                ));
            }
            if push.remote.parse::<SocketAddr>().is_err() {
                return Err(ConfigError::Invalid(format!(
                    "srt.push remote {:?} is not host:port",
                    push.remote
                )));
            }
            if let Some(passphrase) = &push.passphrase
                && !(10..=80).contains(&passphrase.len())
            {
                return Err(ConfigError::Invalid(
                    "srt.push passphrase must be 10..80 bytes".into(),
                ));
            }
            if !matches!(push.pbkeylen, 0 | 16 | 24 | 32) {
                return Err(ConfigError::Invalid(
                    "srt.push pbkeylen must be 0, 16, 24 or 32".into(),
                ));
            }
        }
        if !matches!(self.srt.pbkeylen, 0 | 16 | 24 | 32) {
            return Err(ConfigError::Invalid(
                "srt.pbkeylen must be 0, 16, 24 or 32".into(),
            ));
        }
        self.proxy.validate()?;
        if self.hooks.timeout.is_zero() {
            return Err(ConfigError::Invalid("hooks.timeout is zero".into()));
        }
        if self.auth.mode == AuthMode::Token {
            if self.auth.token_secrets.is_empty() {
                return Err(ConfigError::Invalid(
                    "auth.token_secrets is required when auth.mode = \"token\"".into(),
                ));
            }
            if let Some(secret) = self.auth.token_secrets.iter().find(|s| s.len() < 32) {
                return Err(ConfigError::Invalid(format!(
                    "auth.token_secrets entries must be at least 32 bytes (got {})",
                    secret.len()
                )));
            }
        }
        if self.auth.mode != AuthMode::None
            && self.auth.users.is_empty()
            && self.auth.token_secrets.is_empty()
        {
            return Err(ConfigError::Invalid(
                "auth mode requires users or token_secrets".into(),
            ));
        }
        for (pattern, rule) in &self.paths {
            validate_path_pattern(pattern)?;
            if rule.max_readers > 0 && !rule.read {
                return Err(ConfigError::Invalid(format!(
                    "paths.{pattern}: max_readers set while read = false"
                )));
            }
        }
        if self.http.admin && self.http.admin_password.is_empty() {
            // Not fatal: the console is disabled with a warning. Keep the
            // server usable in a trusted LAN / behind an authenticating proxy.
        }
        Ok(())
    }

    /// Rule that applies to a mount path; the most specific pattern wins.
    pub fn path_rule<'a>(&'a self, path: &str) -> (&'a str, &'a PathRule) {
        let path = path.trim_start_matches('/');
        self.paths
            .iter()
            .filter(|(pattern, _)| glob_match(pattern, path))
            .max_by_key(|(pattern, _)| specificity(pattern))
            .map(|(pattern, rule)| (pattern.as_str(), rule))
            .unwrap_or_else(|| {
                self.paths
                    .get("**")
                    .map(|rule| ("**", rule))
                    .unwrap_or_else(|| ("", &EMPTY_RULE))
            })
    }
}

static EMPTY_RULE: PathRule = PathRule {
    enabled: false,
    publish: false,
    read: false,
    record: false,
    hls: false,
    dash: false,
    flv: false,
    webrtc: false,
    max_readers: 0,
    tenant: None,
    labels: BTreeMap::new(),
};

fn validate_path_pattern(pattern: &str) -> Result<(), ConfigError> {
    if pattern.trim().is_empty() {
        return Err(ConfigError::Invalid("paths: empty pattern".into()));
    }
    if pattern.starts_with('/') || pattern.contains("..") {
        return Err(ConfigError::Invalid(format!(
            "paths: invalid pattern {pattern:?}"
        )));
    }
    Ok(())
}

/// Glob match over `/`-separated path segments: `*` matches one segment,
/// `**` matches zero or more.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((segment, rest)) if *segment == "**" => {
                if segments_match(rest, path) {
                    return true;
                }
                if let Some((_, tail)) = path.split_first() {
                    return segments_match(pattern, tail);
                }
                false
            }
            Some((segment, rest)) => match path.split_first() {
                Some((candidate, tail)) => {
                    (segment == candidate || *segment == "*") && segments_match(rest, tail)
                }
                None => false,
            },
        }
    }
    let pattern: Vec<&str> = pattern.trim_matches('/').split('/').collect();
    let path: Vec<&str> = path.trim_matches('/').split('/').collect();
    segments_match(&pattern, &path)
}

fn specificity(pattern: &str) -> (usize, usize) {
    let wildcards = pattern.matches('*').count();
    (pattern.len().saturating_sub(wildcards), wildcards)
}

/// Expand `${NAME}` references. A missing variable is a configuration error:
/// silently substituting an empty secret would be worse than failing.
pub fn expand_env(text: &str) -> Result<String, ConfigError> {
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (code, comment) = split_toml_comment(line);
        output.push_str(&expand_env_fragment(code)?);
        output.push_str(comment);
    }
    Ok(output)
}

/// Split a TOML line at the first `#` outside a quoted string. This is not a
/// full TOML parser, but it keeps `${ENV}` in comments from being expanded
/// (the common way examples document optional secrets).
fn split_toml_comment(line: &str) -> (&str, &str) {
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        match ch {
            '\\' if in_basic && !escaped => {
                escaped = true;
                continue;
            }
            '"' if !in_literal && !escaped => in_basic = !in_basic,
            '\'' if !in_basic => in_literal = !in_literal,
            '#' if !in_basic && !in_literal => return (&line[..index], &line[index..]),
            _ => {}
        }
        escaped = false;
    }
    (line, "")
}

fn expand_env_fragment(text: &str) -> Result<String, ConfigError> {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(ConfigError::Invalid("unterminated ${...}".into()));
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err(ConfigError::Invalid("empty ${...}".into()));
        }
        let value = std::env::var(name)
            .map_err(|_| ConfigError::Invalid(format!("environment variable {name} is not set")))?;
        output.push_str(&value);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {0}: {1}")]
    Read(String, String),
    #[error("cannot parse config {0}: {1}")]
    Parse(String, String),
    #[error("cannot serialize config: {0}")]
    Serialize(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests;
