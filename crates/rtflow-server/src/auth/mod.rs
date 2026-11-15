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
    /// Active RTSP leases per API key, for `max_connections`.
    key_leases: Mutex<HashMap<String, HashMap<String, std::time::Instant>>>,
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
            key_leases: Mutex::new(HashMap::new()),
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

    /// Media endpoints are hit per segment/request: audit denials only, so the
    /// log stays about security events rather than playlist polling.
    fn audit_http_decision(&self, allowed: bool, path: &str, role: AccessRole, protocol: &str) {
        if !allowed && let Some(counters) = &self.counters {
            counters.auth_failure();
        }
        if !allowed && let Some(audit) = &self.audit {
            audit.record(
                &AuditEvent::new("auth.deny", "denied")
                    .path(path.trim_start_matches('/'))
                    .protocol(protocol)
                    .role(role_name(role)),
            );
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

    fn key_allowed(
        &self,
        token: &str,
        path: &str,
        role: AccessRole,
        protocol: &str,
        principal: Option<&str>,
    ) -> bool {
        let Some(record) = self.keys.verify(token, path, role_name(role), protocol) else {
            return false;
        };
        if record.max_connections == 0 {
            return true;
        }
        // Enforced per distinct principal (RTSP peer address). Leases expire
        // when a client stops sending requests, which for RTSP means the
        // session is gone or already closed by the idle timeout.
        const LEASE_TTL: std::time::Duration = std::time::Duration::from_secs(120);
        let Some(principal) = principal else {
            return true;
        };
        let now = std::time::Instant::now();
        let mut leases = self
            .key_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        leases.retain(|_, map| {
            map.retain(|_, seen| now.duration_since(*seen) < LEASE_TTL);
            !map.is_empty()
        });
        let entry = leases.entry(record.kid.clone()).or_default();
        if let Some(seen) = entry.get_mut(principal) {
            *seen = now;
            return true;
        }
        if entry.len() >= record.max_connections {
            tracing::warn!(
                kid = %record.kid,
                limit = record.max_connections,
                "stream key connection limit reached"
            );
            return false;
        }
        entry.insert(principal.to_owned(), now);
        true
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
                && self.key_allowed(
                    token,
                    path,
                    request.role,
                    "rtsp",
                    Some(&request.peer_addr.to_string()),
                ))
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

mod media;
mod rtmp;

#[cfg(test)]
mod tests;
