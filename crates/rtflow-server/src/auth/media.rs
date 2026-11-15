//! HTTP/SRT media authorization.
use super::*;

impl AppAuth {
    /// Verify an SRT `streamid` (`live/cam` or `live/cam?token=...`).
    pub fn verify_srt(&self, stream_id: &str, peer: std::net::SocketAddr) -> bool {
        let (path, query) = match stream_id.split_once('?') {
            Some((path, query)) => (path, query),
            None => (stream_id, ""),
        };
        let path = path.trim_start_matches('/');
        let token = query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == "token" || key == "key").then_some(value)
        });
        let allowed = match token {
            Some(token) if token.starts_with("rk_") && token.contains('.') => self.key_allowed(
                token,
                path,
                AccessRole::Publish,
                "srt",
                Some(&peer.to_string()),
            ),
            Some(token) => {
                self.verify_signed_token(token, path, AccessRole::Publish)
                    && self.path_allowed(path, AccessRole::Publish)
            }
            None => {
                self.config.mode == AuthMode::None && self.path_allowed(path, AccessRole::Publish)
            }
        };
        self.audit_http_decision(allowed, path, AccessRole::Publish, "srt");
        allowed
    }

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
            return self.key_allowed(token, path, role, protocol, None);
        }
        self.verify_signed_token(token, path, role)
    }
}
