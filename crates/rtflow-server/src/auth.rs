//! Shared authentication for RTSP, RTMP, SRT and HTTP playback.
//!
//! Three credentials can be combined:
//! - configured users (Basic/Digest; RTMP stream key);
//! - API-issued keys (`kid.secret`);
//! - HMAC-signed tokens (`base64url(payload).base64url(signature)`).
//!
//! All successful and failed decisions are returned to the caller so it can
//! audit them; secrets are never logged here.
use crate::{
    audit::{AuditEvent, AuditLog},
    config::{AuthMode, Config},
    hooks::{HookClient, HookKind, HookRequest},
    keys::KeyStore,
    paths::PathStore,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use rtflow::{
    MountRegistry,
    auth::{AccessRole, AuthDecision, AuthRequest, Authenticator, Credentials, DigestNonceManager},
    rtmp::session::PublishAuthenticator,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
pub struct AppAuthConfig {
    pub mode: AuthMode,
    pub realm: String,
    pub users: Vec<(String, String, Vec<String>)>,
    pub secrets: Vec<Vec<u8>>,
    pub enforce_path_rules: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenPayload {
    pub sub: String,
    pub role: String,
    pub path: String,
    pub exp: u64,
    pub nonce: String,
    #[serde(default)]
    pub tenant: Option<String>,
}

pub struct AppAuth {
    config: AppAuthConfig,
    keys: Arc<KeyStore>,
    paths: Arc<PathStore>,
    nonces: DigestNonceManager,
    replay: Mutex<HashMap<String, u64>>,
    mounts: Arc<MountRegistry>,
    hooks: Option<Arc<HookClient>>,
    audit: Option<Arc<AuditLog>>,
    counters: Option<Arc<crate::metrics::ServerCounters>>,
}

impl AppAuth {
    pub fn new(
        config: &Config,
        keys: Arc<KeyStore>,
        paths: Arc<PathStore>,
        mounts: Arc<MountRegistry>,
    ) -> Result<Self, rtflow::Error> {
        let auth = &config.auth;
        let mut secrets = Vec::new();
        for secret in &auth.token_secrets {
            secrets.push(secret.as_bytes().to_vec());
        }
        if auth.mode == AuthMode::None {
            secrets.clear();
        }
        let nonce_secret = secrets
            .first()
            .cloned()
            .unwrap_or_else(|| b"rtflow-nonce-secret-0123456789abcdef".to_vec());
        Ok(Self {
            config: AppAuthConfig {
                mode: auth.mode,
                realm: if auth.realm.is_empty() {
                    "rtflow".into()
                } else {
                    auth.realm.clone()
                },
                users: auth
                    .users
                    .iter()
                    .map(|user| {
                        (
                            user.username.clone(),
                            user.password.clone(),
                            user.roles.clone(),
                        )
                    })
                    .collect(),
                secrets,
                enforce_path_rules: auth.enforce_path_rules,
            },
            keys,
            paths,
            nonces: DigestNonceManager::new(nonce_secret, Duration::from_secs(300), 16_384)?,
            replay: Mutex::new(HashMap::new()),
            mounts,
            hooks: None,
            audit: None,
            counters: None,
        })
    }

    /// Ask the configured HTTP authorization hook; allow when no hook is set.
    async fn run_hook(
        &self,
        kind: HookKind,
        path: &str,
        role: &str,
        protocol: &str,
        peer: Option<String>,
    ) -> bool {
        let Some(hooks) = &self.hooks else {
            return true;
        };
        let request = HookRequest {
            event: kind,
            path: path.trim_start_matches('/').to_owned(),
            role: role.to_owned(),
            protocol: protocol.to_owned(),
            peer,
            key_id: None,
            stream_id: None,
            reason: None,
            timestamp: unix_now(),
        };
        match hooks.authorize(kind, request).await {
            Ok(decision) => decision.allow,
            Err(error) => {
                tracing::warn!(%error, path, "authorization hook failed");
                false
            }
        }
    }

    /// Attach application counters (auth failures).
    pub fn with_counters(mut self, counters: Arc<crate::metrics::ServerCounters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// Attach the audit log for authentication decisions.
    pub fn with_audit(mut self, audit: Option<Arc<AuditLog>>) -> Self {
        self.audit = audit;
        self
    }

    fn audit_decision(&self, request: &AuthRequest<'_>, allowed: bool, protocol: &str) {
        let Some(audit) = &self.audit else {
            return;
        };
        let path = request.path.split('?').next().unwrap_or(request.path);
        let event = AuditEvent::new(
            if allowed { "auth.allow" } else { "auth.deny" },
            if allowed { "ok" } else { "denied" },
        )
        .path(path.trim_start_matches('/'))
        .protocol(protocol)
        .role(role_name(request.role))
        .peer(request.peer_addr.to_string());
        audit.record(&event);
    }

    /// Attach HTTP authorization hooks. They are consulted after local
    /// credentials and path rules pass, so the hook cannot widen scope.
    pub fn with_hooks(mut self, hooks: Option<Arc<HookClient>>) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn mode(&self) -> AuthMode {
        self.config.mode
    }

    pub fn keys(&self) -> &Arc<KeyStore> {
        &self.keys
    }

    fn path_allowed(&self, path: &str, role: AccessRole) -> bool {
        let record = self.paths.resolve(path);
        if !record.enabled {
            return false;
        }
        if !self.config.enforce_path_rules && self.config.mode == AuthMode::None {
            return true;
        }
        let permitted = match role {
            AccessRole::Publish => record.publish,
            AccessRole::Read => record.read,
        };
        if !permitted {
            return false;
        }
        // Best-effort reader quota: the session count is sampled before the
        // new session registers, so a burst can overshoot by the number of
        // concurrent authorizations. The RTSP server's global and per-IP
        // limits remain the hard bound.
        if role == AccessRole::Read && record.max_readers > 0 {
            let current = self
                .mounts
                .get_handle(&format!("/{}", path.trim_start_matches('/')))
                .map(|handle| {
                    let snapshot = handle.snapshot();
                    snapshot.subscriber_count.max(snapshot.broadcast_receivers)
                })
                .unwrap_or(0);
            if current >= record.max_readers {
                tracing::warn!(
                    path,
                    readers = current,
                    limit = record.max_readers,
                    "reader quota reached"
                );
                return false;
            }
        }
        true
    }

    fn user_allowed(&self, username: &str, password: &str, role: AccessRole) -> bool {
        let role_name = role_name(role);
        self.config.users.iter().any(|(name, secret, roles)| {
            name == username
                && password_matches(password, secret)
                && (roles.is_empty() || roles.iter().any(|allowed| allowed == role_name))
        })
    }

    fn key_allowed(&self, token: &str, path: &str, role: AccessRole, protocol: &str) -> bool {
        self.keys
            .verify(token, path, role_name(role), protocol)
            .is_some()
    }

    /// HMAC token verification, usable by HTTP/WebRTC adapters.
    pub fn verify_signed_token(&self, token: &str, path: &str, role: AccessRole) -> bool {
        let Some((payload_b64, signature_b64)) = token.rsplit_once('.') else {
            return false;
        };
        let Ok(signature) = URL_SAFE_NO_PAD.decode(signature_b64) else {
            return false;
        };
        let valid_signature = self.config.secrets.iter().any(|secret| {
            let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC key");
            mac.update(payload_b64.as_bytes());
            mac.verify_slice(&signature).is_ok()
        });
        if !valid_signature {
            return false;
        }
        let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload_b64) else {
            return false;
        };
        let Ok(payload) = serde_json::from_slice::<TokenPayload>(&bytes) else {
            return false;
        };
        if payload.exp <= unix_now()
            || payload.role != role_name(role)
            || !crate::config::glob_match(&payload.path, path.trim_start_matches('/'))
        {
            return false;
        }
        self.replay_check(&payload.nonce, payload.exp)
    }

    fn replay_check(&self, nonce: &str, exp: u64) -> bool {
        let now = unix_now();
        let mut replay = self
            .replay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        replay.retain(|_, expiry| *expiry > now);
        if replay.contains_key(nonce) {
            return false;
        }
        replay.insert(nonce.to_owned(), exp);
        true
    }

    /// Issue a signed token (admin/tooling helper; the secret is config-only).
    pub fn issue_signed_token(&self, payload: &TokenPayload) -> Option<String> {
        let secret = self.config.secrets.first()?;
        let bytes = serde_json::to_vec(payload).ok()?;
        let payload_b64 = URL_SAFE_NO_PAD.encode(bytes);
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC key");
        mac.update(payload_b64.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Some(format!("{payload_b64}.{signature}"))
    }

    fn bearer_or_query_token<'a>(&self, request: &'a AuthRequest<'a>) -> Option<&'a str> {
        if let Some(header) = request.authorization
            && let Some(token) = header.strip_prefix("Bearer ")
        {
            return Some(token.trim());
        }
        // RTSP URLs sometimes carry `?token=...`; use only the query value.
        let (_, query) = request.path.split_once('?')?;
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            // RTSP control URLs may append `/streamid=0` after the query;
            // token/key alphabets never contain `/`.
            (key == "token" || key == "key").then_some(value.split('/').next().unwrap_or(value))
        })
    }

    fn authorize_sync(&self, request: &AuthRequest<'_>) -> AuthDecision {
        let decision = self.authorize_sync_inner(request);
        // OPTIONS is capability discovery and carries no media/path state;
        // it is intentionally not audited.
        if request.method != rtflow::rtsp::Method::Options {
            let allowed = matches!(decision, AuthDecision::Allow);
            self.audit_decision(request, allowed, "rtsp");
            if !allowed && let Some(counters) = &self.counters {
                counters.auth_failure();
            }
        }
        decision
    }

    fn authorize_sync_inner(&self, request: &AuthRequest<'_>) -> AuthDecision {
        if request.method == rtflow::rtsp::Method::Options {
            return AuthDecision::Allow;
        }
        let path = request.path.split('?').next().unwrap_or(request.path);
        if self.config.mode == AuthMode::None {
            return if self.path_allowed(path, request.role) {
                AuthDecision::Allow
            } else {
                AuthDecision::Deny
            };
        }
        if let Some(token) = self.bearer_or_query_token(request) {
            if (token.starts_with("rk_")
                && token.contains('.')
                && self.key_allowed(token, path, request.role, "rtsp"))
                || self.verify_signed_token(token, path, request.role)
            {
                return AuthDecision::Allow;
            }
            return AuthDecision::Deny;
        }
        let Some(header) = request.authorization else {
            return match self.config.mode {
                AuthMode::Digest => AuthDecision::DigestChallenge {
                    realm: self.config.realm.clone(),
                    nonce: self.nonces.issue(),
                },
                _ => AuthDecision::Challenge {
                    realm: self.config.realm.clone(),
                },
            };
        };
        if let Some((username, password)) = parse_basic(header) {
            if self.user_allowed(&username, &password, request.role)
                && self.path_allowed(path, request.role)
            {
                return AuthDecision::Allow;
            }
            return AuthDecision::Deny;
        }
        if self.config.mode == AuthMode::Digest {
            for (username, password, _) in &self.config.users {
                let credentials = Credentials {
                    username: username.clone(),
                    password: password.clone(),
                };
                if rtflow::auth::verify_digest(
                    header,
                    request.method,
                    &credentials,
                    &self.config.realm,
                )
                .unwrap_or(false)
                    && self.user_allowed(username, password, request.role)
                    && self.path_allowed(path, request.role)
                {
                    return AuthDecision::Allow;
                }
            }
            return AuthDecision::DigestChallenge {
                realm: self.config.realm.clone(),
                nonce: self.nonces.issue(),
            };
        }
        AuthDecision::Deny
    }
}

impl std::fmt::Debug for AppAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppAuth")
            .field("mode", &self.config.mode)
            .field("realm", &self.config.realm)
            .finish_non_exhaustive()
    }
}

impl Authenticator for AppAuth {
    fn authorize<'a>(
        &'a self,
        request: AuthRequest<'a>,
    ) -> futures::future::BoxFuture<'a, AuthDecision> {
        Box::pin(async move {
            let local = self.authorize_sync(&request);
            if !matches!(local, AuthDecision::Allow) {
                return local;
            }
            let Some(hooks) = &self.hooks else {
                return local;
            };
            let kind = match request.role {
                AccessRole::Publish => HookKind::PublishAuth,
                AccessRole::Read => HookKind::PlayAuth,
            };
            let path = request.path.split('?').next().unwrap_or(request.path);
            let hook_request = HookRequest {
                event: kind,
                path: path.trim_start_matches('/').to_owned(),
                role: role_name(request.role).to_owned(),
                protocol: "rtsp".to_owned(),
                peer: Some(request.peer_addr.to_string()),
                key_id: None,
                stream_id: None,
                reason: None,
                timestamp: unix_now(),
            };
            match hooks.authorize(kind, hook_request).await {
                Ok(decision) if decision.allow => AuthDecision::Allow,
                Ok(_) => AuthDecision::Deny,
                Err(error) => {
                    tracing::warn!(%error, path, "authorization hook failed");
                    AuthDecision::Deny
                }
            }
        })
    }
}

impl AppAuth {
    fn rtmp_audit(&self, allowed: bool, path: &str, role: &str) {
        if let Some(counters) = &self.counters
            && !allowed
        {
            counters.auth_failure();
        }
        if let Some(audit) = &self.audit {
            audit.record(
                &AuditEvent::new(
                    if allowed { "auth.allow" } else { "auth.deny" },
                    if allowed { "ok" } else { "denied" },
                )
                .path(path)
                .protocol("rtmp")
                .role(role),
            );
        }
    }
}

impl PublishAuthenticator for AppAuth {
    fn authorize_publish(&self, app: &str, stream_name: &str) -> bool {
        let (stream, token) = split_rtmp_stream(stream_name);
        let allowed = self.authorize_publish_inner(app, stream, token);
        self.rtmp_audit(allowed, &format!("{app}/{stream}"), "publish");
        allowed
    }

    fn authorize_play(&self, app: &str, stream_name: &str) -> bool {
        let (stream, token) = split_rtmp_stream(stream_name);
        let allowed = self.authorize_play_inner(app, stream, token);
        self.rtmp_audit(allowed, &format!("{app}/{stream}"), "read");
        allowed
    }
}

impl AppAuth {
    fn authorize_publish_inner(&self, app: &str, stream: &str, token: Option<&str>) -> bool {
        let path = format!("{app}/{stream}");
        if let Some(token) = token {
            if (token.starts_with("rk_")
                && token.contains('.')
                && self.key_allowed(token, &path, AccessRole::Publish, "rtmp"))
                || self.verify_signed_token(token, &path, AccessRole::Publish)
            {
                return true;
            }
            return false;
        }
        if self.config.mode == AuthMode::None {
            return self.path_allowed(&path, AccessRole::Publish);
        }
        if let Some((username, password, stream)) = parse_rtmp_credentials(stream) {
            let path = format!("{app}/{stream}");
            return self.user_allowed(&username, &password, AccessRole::Publish)
                && self.path_allowed(&path, AccessRole::Publish);
        }
        false
    }

    fn authorize_play_inner(&self, app: &str, stream: &str, token: Option<&str>) -> bool {
        let path = format!("{app}/{stream}");
        if let Some(token) = token {
            if (token.starts_with("rk_")
                && token.contains('.')
                && self.key_allowed(token, &path, AccessRole::Read, "rtmp"))
                || self.verify_signed_token(token, &path, AccessRole::Read)
            {
                return true;
            }
            return false;
        }
        if self.config.mode == AuthMode::None {
            return self.path_allowed(&path, AccessRole::Read);
        }
        if let Some((username, password, stream)) = parse_rtmp_credentials(stream) {
            let path = format!("{app}/{stream}");
            return self.user_allowed(&username, &password, AccessRole::Read)
                && self.path_allowed(&path, AccessRole::Read);
        }
        false
    }
}

impl rtflow::rtmp::session::AsyncPublishAuthenticator for AppAuth {
    fn authorize_publish<'a>(
        &'a self,
        app: &'a str,
        stream_name: &'a str,
    ) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(async move {
            let (stream, _) = split_rtmp_stream(stream_name);
            let path = format!("{app}/{stream}");
            let allowed = self
                .run_hook(HookKind::PublishAuth, &path, "publish", "rtmp", None)
                .await;
            if let Some(audit) = &self.audit {
                audit.record(
                    &AuditEvent::new(
                        if allowed { "auth.allow" } else { "auth.deny" },
                        if allowed { "ok" } else { "denied" },
                    )
                    .path(path)
                    .protocol("rtmp")
                    .role("publish"),
                );
            }
            allowed
        })
    }

    fn authorize_play<'a>(
        &'a self,
        app: &'a str,
        stream_name: &'a str,
    ) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(async move {
            let (stream, _) = split_rtmp_stream(stream_name);
            let path = format!("{app}/{stream}");
            let allowed = self
                .run_hook(HookKind::PlayAuth, &path, "read", "rtmp", None)
                .await;
            if let Some(audit) = &self.audit {
                audit.record(
                    &AuditEvent::new(
                        if allowed { "auth.allow" } else { "auth.deny" },
                        if allowed { "ok" } else { "denied" },
                    )
                    .path(path)
                    .protocol("rtmp")
                    .role("read"),
                );
            }
            allowed
        })
    }
}

fn split_rtmp_stream(stream_name: &str) -> (&str, Option<&str>) {
    let Some((stream, query)) = stream_name.split_once('?') else {
        return (stream_name, None);
    };
    let token = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token" || key == "key").then_some(value)
    });
    (stream, token)
}

fn parse_rtmp_credentials(stream: &str) -> Option<(String, String, String)> {
    let (credentials, stream) = stream.split_once('@')?;
    let (username, password) = credentials.split_once(':')?;
    (!username.is_empty() && !stream.is_empty())
        .then(|| (username.to_owned(), password.to_owned(), stream.to_owned()))
}

fn parse_basic(header: &str) -> Option<(String, String)> {
    let encoded = header.strip_prefix("Basic ")?;
    // Basic uses standard base64; accept URL-safe input as a convenience for
    // scripts that generate credentials without padding.
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .or_else(|_| URL_SAFE_NO_PAD.decode(encoded.trim()))
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (username, password) = text.split_once(':')?;
    Some((username.to_owned(), password.to_owned()))
}

fn password_matches(provided: &str, stored: &str) -> bool {
    if stored.starts_with("$argon2") {
        use argon2::PasswordVerifier;
        let Ok(hash) = argon2::PasswordHash::new(stored) else {
            return false;
        };
        return argon2::Argon2::default()
            .verify_password(provided.as_bytes(), &hash)
            .is_ok();
    }
    provided.as_bytes().ct_eq(stored.as_bytes()).into()
}

fn role_name(role: AccessRole) -> &'static str {
    match role {
        AccessRole::Publish => "publish",
        AccessRole::Read => "read",
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Verify an HTTP bearer token or API key for a playback path.
impl AppAuth {
    /// Verify an HTTP request using a Bearer token, `?token=` query, or
    /// Basic credentials. Used by HLS/DASH/FLV/VOD/WebRTC routes.
    pub fn verify_http_request(
        &self,
        path: &str,
        request: &rtflow::http::HttpRequest,
        role: AccessRole,
        protocol: &str,
    ) -> bool {
        let allowed = self.verify_http_request_inner(path, request, role, protocol);
        if !allowed && let Some(counters) = &self.counters {
            counters.auth_failure();
        }
        // Media endpoints are hit per segment/request: audit denials only, so
        // the log stays about security events rather than playlist polling.
        if !allowed && let Some(audit) = &self.audit {
            audit.record(
                &AuditEvent::new("auth.deny", "denied")
                    .path(path.trim_start_matches('/'))
                    .protocol(protocol)
                    .role(role_name(role)),
            );
        }
        allowed
    }

    fn verify_http_request_inner(
        &self,
        path: &str,
        request: &rtflow::http::HttpRequest,
        role: AccessRole,
        protocol: &str,
    ) -> bool {
        let path = path.trim_start_matches('/');
        let token = request
            .header("authorization")
            .and_then(|header| header.strip_prefix("Bearer ").map(str::trim))
            .or_else(|| {
                request
                    .query
                    .get("token")
                    .or_else(|| request.query.get("key"))
                    .map(String::as_str)
            });
        if let Some(token) = token {
            return self.verify_http(path, token, role, protocol);
        }
        if self.config.mode == AuthMode::None {
            return self.path_allowed(path, role);
        }
        if let Some(header) = request.header("authorization")
            && let Some((username, password)) = parse_basic(header)
            && self.user_allowed(&username, &password, role)
        {
            return self.path_allowed(path, role);
        }
        false
    }

    pub fn verify_http(&self, path: &str, token: &str, role: AccessRole, protocol: &str) -> bool {
        if self.config.mode == AuthMode::None {
            return self.path_allowed(path, role);
        }
        if token.starts_with("rk_") && token.contains('.') {
            return self.key_allowed(token, path, role, protocol);
        }
        self.verify_signed_token(token, path, role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtflow::rtsp::Method;
    use std::sync::Arc;

    fn auth() -> AppAuth {
        let mut config = Config::default();
        config.server.state_dir =
            std::env::temp_dir().join(format!("rtflow-auth-{}", std::process::id()));
        config.auth.mode = AuthMode::Basic;
        config.auth.users.push(crate::config::UserConfig {
            username: "u".into(),
            password: "p".into(),
            roles: vec!["publish".into()],
        });
        let keys = Arc::new(KeyStore::new(&config).unwrap());
        let paths = Arc::new(PathStore::new(&config, false).unwrap());
        let mounts = Arc::new(MountRegistry::new(16));
        AppAuth::new(&config, keys, paths, mounts).unwrap()
    }

    #[test]
    fn basic_role_is_enforced() {
        let auth = auth();
        let request = AuthRequest {
            method: Method::Record,
            path: "/live/cam",
            peer_addr: "127.0.0.1:1".parse().unwrap(),
            role: AccessRole::Publish,
            authorization: Some(&format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("u:p")
            )),
        };
        assert!(matches!(auth.authorize_sync(&request), AuthDecision::Allow));
    }

    #[test]
    fn signed_token_is_scoped() {
        let mut auth = auth();
        auth.config.mode = AuthMode::Token;
        auth.config.secrets = vec![b"secret".to_vec()];
        let payload = TokenPayload {
            sub: "u".into(),
            role: "read".into(),
            path: "live/**".into(),
            exp: unix_now() + 60,
            nonce: "n1".into(),
            tenant: None,
        };
        let token = auth.issue_signed_token(&payload).unwrap();
        assert!(auth.verify_signed_token(&token, "live/cam", AccessRole::Read));
        assert!(!auth.verify_signed_token(&token, "live/cam", AccessRole::Read));
        assert!(!auth.verify_signed_token(&token, "private/cam", AccessRole::Read));
    }
}
