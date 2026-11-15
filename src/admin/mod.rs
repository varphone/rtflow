//! Commercial-style, feature-gated admin console.
//!
//! The module owns its HTTP API, authentication, session store and static
//! single-page application. It only depends on `http`; VOD management is
//! compiled in when the `hls` feature is enabled and a
//! [`crate::hls::vod::HlsVodManager`] is supplied.
//!
//! Enable with `features = ["admin"]`; the default build does not contain
//! any of this module.
mod api;
mod auth;

use crate::MountRegistry;
use auth::{PasswordDigest, Sessions};
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
    #[cfg(feature = "hls")]
    pub(crate) vod: Option<Arc<crate::hls::vod::HlsVodManager>>,
}

impl AdminConfig {
    /// Create a config with a salted password digest. The plaintext password
    /// is not retained in the config.
    pub fn new(username: impl Into<String>, password: &str) -> crate::Result<Self> {
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
            #[cfg(feature = "hls")]
            vod: None,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn with_base_path(mut self, base_path: impl Into<String>) -> crate::Result<Self> {
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

    pub fn with_rtmp_base(mut self, base: impl Into<String>) -> Self {
        self.rtmp_base = Some(base.into());
        self
    }

    #[cfg(feature = "hls")]
    pub fn with_vod(mut self, vod: Arc<crate::hls::vod::HlsVodManager>) -> Self {
        self.vod = Some(vod);
        self
    }

    fn validate(&self) -> crate::Result<()> {
        if !self.base_path.starts_with('/') || self.base_path.ends_with('/') {
            return Err(crate::Error::InvalidConfig(
                "admin base_path must start with '/' and have no trailing '/'".into(),
            ));
        }
        if !self
            .base_path
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.'))
        {
            return Err(crate::Error::InvalidConfig(
                "admin base_path contains unsupported characters".into(),
            ));
        }
        if self.username.is_empty() {
            return Err(crate::Error::InvalidConfig(
                "admin username is empty".into(),
            ));
        }
        if self.session_ttl.is_zero() {
            return Err(crate::Error::InvalidConfig(
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

/// Admin console handle. Register it on an [`crate::http::HttpServer`].
pub struct Admin {
    state: Arc<AdminState>,
}

impl Admin {
    pub fn new(config: AdminConfig, mounts: Arc<MountRegistry>) -> crate::Result<Self> {
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
    pub async fn register(&self, http: &crate::http::HttpServer) {
        api::register(http, self.state.clone()).await;
    }

    pub fn config(&self) -> &AdminConfig {
        &self.state.config
    }
}
