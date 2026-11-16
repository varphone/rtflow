//! Commercial-style, feature-gated admin console.
//!
//! The module owns its HTTP API, authentication, session store and static
//! single-page application. It only depends on `http`; VOD management is
//! compiled in when the `hls` feature is enabled and a
//! [`rtflow_hls::vod::HlsVodManager`] is supplied.
//!
//! Enable with `features = ["admin"]`; the default build does not contain
//! any of this module.
mod api;
mod auth;

use auth::{PasswordDigest, Sessions};
use futures::future::BoxFuture;
use rtflow_mount::MountRegistry;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// Admin console configuration.
#[derive(Clone)]
pub struct AdminConfig {
    /// URL prefix, e.g. `/admin`.
    pub base_path: String,
    /// Shown in the browser title and login page.
    pub title: String,
    /// Operator account name.
    pub username: String,
    pub(crate) password: PasswordDigest,
    /// Session lifetime (also cookie Max-Age).
    pub session_ttl: Duration,
    /// Mark the session cookie `Secure` when served behind TLS.
    pub secure_cookies: bool,
    /// Hide/deny destructive actions (kick, VOD cache refresh).
    pub readonly: bool,
    /// Prefix where HLS live playlists are served, e.g. `/hls`.
    pub hls_prefix: String,
    /// Prefix where LL-HLS live playlists are served, e.g. `/llhls`.
    pub ll_hls_prefix: String,
    /// Public RTMP base used for ingest hints, e.g. `rtmp://host:1935`.
    pub rtmp_base: Option<String>,
    /// Public RTSP base used for playback hints, e.g. `rtsp://host:8554`.
    pub rtsp_base: Option<String>,
    pub(crate) vod: Option<Arc<rtflow_hls::vod::HlsVodManager>>,
    pub(crate) control: Option<Arc<dyn ControlPlane>>,
}

/// Optional control plane exposed by the admin console: path registration,
/// stream keys and recording control.
///
/// The console renders the data and forwards operator intent; the application
/// (`rtflow-server`) owns persistence, policy and audit. Values are JSON so
/// this crate does not depend on the application's types.
pub trait ControlPlane: Send + Sync + 'static {
    fn paths(&self) -> serde_json::Value;
    fn create_path(&self, body: serde_json::Value) -> Result<serde_json::Value, String>;
    fn update_path(&self, name: &str, body: serde_json::Value)
    -> Result<serde_json::Value, String>;
    fn delete_path(&self, name: &str) -> Result<(), String>;
    fn kick(&self, name: &str) -> Result<(), String>;
    fn keys(&self) -> serde_json::Value;
    fn issue_key(&self, body: serde_json::Value) -> Result<serde_json::Value, String>;
    fn revoke_key(&self, kid: &str) -> Result<(), String>;
    fn recordings(&self) -> serde_json::Value;
    /// Recent control-plane/auth audit events, newest first.
    fn audit_events(&self, _limit: usize) -> serde_json::Value {
        serde_json::json!({ "events": [] })
    }
    /// RTSP proxy mounts (read-only snapshot for the console).
    fn proxies(&self) -> serde_json::Value {
        serde_json::json!({ "proxies": [] })
    }
    fn start_recording<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), String>>;
    fn stop_recording<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<(), String>>;
}

impl AdminConfig {
    /// Create a config with a salted password digest. The plaintext password
    /// is not retained in the config.
    pub fn new(username: impl Into<String>, password: &str) -> rtflow_core::error::Result<Self> {
        let base_path = "/admin".to_owned();
        let config = Self {
            base_path,
            title: "rtflow 管理控制台".to_owned(),
            username: username.into(),
            password: PasswordDigest::new(password)?,
            session_ttl: Duration::from_secs(8 * 60 * 60),
            secure_cookies: false,
            readonly: false,
            hls_prefix: "/hls".to_owned(),
            ll_hls_prefix: "/llhls".to_owned(),
            rtmp_base: None,
            rtsp_base: None,
            vod: None,
            control: None,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn with_base_path(
        mut self,
        base_path: impl Into<String>,
    ) -> rtflow_core::error::Result<Self> {
        self.base_path = base_path.into();
        self.validate()?;
        Ok(self)
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn with_readonly(mut self, readonly: bool) -> Self {
        self.readonly = readonly;
        self
    }

    pub fn with_secure_cookies(mut self, secure: bool) -> Self {
        self.secure_cookies = secure;
        self
    }

    pub fn with_hls_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.hls_prefix = prefix.into();
        self
    }

    pub fn with_ll_hls_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.ll_hls_prefix = prefix.into();
        self
    }

    pub fn with_rtsp_base(mut self, base: impl Into<String>) -> Self {
        self.rtsp_base = Some(base.into());
        self
    }

    pub fn with_rtmp_base(mut self, base: impl Into<String>) -> Self {
        self.rtmp_base = Some(base.into());
        self
    }
    pub fn with_vod(mut self, vod: Arc<rtflow_hls::vod::HlsVodManager>) -> Self {
        self.vod = Some(vod);
        self
    }

    /// Enable the path/key/recording control pages and API.
    pub fn with_control(mut self, control: Arc<dyn ControlPlane>) -> Self {
        self.control = Some(control);
        self
    }

    fn validate(&self) -> rtflow_core::error::Result<()> {
        if !self.base_path.starts_with('/') || self.base_path.ends_with('/') {
            return Err(rtflow_core::error::Error::InvalidConfig(
                "admin base_path must start with '/' and have no trailing '/'".into(),
            ));
        }
        if !self
            .base_path
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.'))
        {
            return Err(rtflow_core::error::Error::InvalidConfig(
                "admin base_path contains unsupported characters".into(),
            ));
        }
        if self.username.is_empty() {
            return Err(rtflow_core::error::Error::InvalidConfig(
                "admin username is empty".into(),
            ));
        }
        if self.session_ttl.is_zero() {
            return Err(rtflow_core::error::Error::InvalidConfig(
                "admin session_ttl is zero".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) struct AdminState {
    pub(crate) config: AdminConfig,
    pub(crate) mounts: Arc<MountRegistry>,
    pub(crate) sessions: Sessions,
    pub(crate) started: Instant,
}

/// Admin console handle. Register it on an [`rtflow_http::HttpServer`].
pub struct Admin {
    state: Arc<AdminState>,
}

impl Admin {
    pub fn new(
        config: AdminConfig,
        mounts: Arc<MountRegistry>,
    ) -> rtflow_core::error::Result<Self> {
        config.validate()?;
        Ok(Self {
            state: Arc::new(AdminState {
                sessions: Sessions::new(config.session_ttl),
                config,
                mounts,
                started: Instant::now(),
            }),
        })
    }

    /// Register the login page, SPA assets and JSON API.
    pub async fn register(&self, http: &rtflow_http::HttpServer) {
        api::register(http, self.state.clone()).await;
    }

    pub fn config(&self) -> &AdminConfig {
        &self.state.config
    }

    /// True when the request carries a valid console session cookie.
    ///
    /// Applications that mount their own API on the same HTTP server can use
    /// this to accept the browser session alongside other credentials.
    pub fn is_authenticated(&self, request: &rtflow_http::HttpRequest) -> bool {
        api::session_valid(&self.state, request)
    }
}
