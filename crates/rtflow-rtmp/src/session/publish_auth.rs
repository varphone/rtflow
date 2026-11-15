//! RTMP publish/play authentication hooks.
use std::net::SocketAddr;

/// Trait for RTMP publish authentication.
///
/// Implementations decide whether a given publish request (app + stream name)
/// is allowed. This enables stream-key validation, IP whitelisting, etc.
pub trait PublishAuthenticator: Send + Sync + std::fmt::Debug {
    /// Check if publishing `stream_name` on `app` is allowed.
    ///
    /// Returns `true` if the publish is authorized.
    fn authorize_publish(&self, app: &str, stream_name: &str) -> bool;

    /// Check if playing `stream_name` on `app` is allowed. Defaults to the
    /// publish policy so a configured key list protects both directions.
    fn authorize_play(&self, app: &str, stream_name: &str) -> bool {
        self.authorize_publish(app, stream_name)
    }

    /// Peer-aware publish check. Defaults to the peer-less method; policy
    /// implementations that enforce per-key connection limits override it.
    fn authorize_publish_peer(
        &self,
        app: &str,
        stream_name: &str,
        _peer: Option<SocketAddr>,
    ) -> bool {
        self.authorize_publish(app, stream_name)
    }

    /// Peer-aware play check.
    fn authorize_play_peer(&self, app: &str, stream_name: &str, _peer: Option<SocketAddr>) -> bool {
        self.authorize_play(app, stream_name)
    }
}

/// Asynchronous publish authenticator for policy checks that need I/O
/// (HTTP hooks, token introspection, database lookups).
///
/// Consulted after [`PublishAuthenticator`], so a synchronous allow-list can
/// still short-circuit the expensive path.
pub trait AsyncPublishAuthenticator: Send + Sync + std::fmt::Debug {
    fn authorize_publish<'a>(
        &'a self,
        app: &'a str,
        stream_name: &'a str,
    ) -> futures::future::BoxFuture<'a, bool>;

    fn authorize_play<'a>(
        &'a self,
        app: &'a str,
        stream_name: &'a str,
    ) -> futures::future::BoxFuture<'a, bool>;

    fn authorize_publish_peer<'a>(
        &'a self,
        app: &'a str,
        stream_name: &'a str,
        peer: Option<SocketAddr>,
    ) -> futures::future::BoxFuture<'a, bool> {
        let _ = peer;
        self.authorize_publish(app, stream_name)
    }

    fn authorize_play_peer<'a>(
        &'a self,
        app: &'a str,
        stream_name: &'a str,
        peer: Option<SocketAddr>,
    ) -> futures::future::BoxFuture<'a, bool> {
        let _ = peer;
        self.authorize_play(app, stream_name)
    }
}

/// A simple authenticator that validates stream keys against a static list.
///
/// The key format is `app/stream_name`. If the list is empty, all publishes
/// are allowed (open mode).
#[derive(Clone, Debug, Default)]
pub struct StreamKeyAuthenticator {
    /// Allowed stream keys in the form "app/stream_name".
    /// If empty, all publishes are allowed.
    allowed_keys: Vec<String>,
}

impl StreamKeyAuthenticator {
    /// Create a new authenticator with the given allowed stream keys.
    pub fn new(allowed_keys: Vec<String>) -> Self {
        Self { allowed_keys }
    }

    /// Add an allowed stream key.
    pub fn allow(&mut self, key: impl Into<String>) {
        self.allowed_keys.push(key.into());
    }
}

impl PublishAuthenticator for StreamKeyAuthenticator {
    fn authorize_publish(&self, app: &str, stream_name: &str) -> bool {
        if self.allowed_keys.is_empty() {
            return true; // Open mode
        }
        let key = format!("{app}/{stream_name}");
        self.allowed_keys.contains(&key)
    }
}
