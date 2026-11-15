//! RTMP publish/play authorization (sync allow-list, API keys, async hooks).
use super::*;

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
        let allowed = self.authorize_publish_inner(app, stream, token, None);
        self.rtmp_audit(allowed, &format!("{app}/{stream}"), "publish");
        allowed
    }

    fn authorize_play(&self, app: &str, stream_name: &str) -> bool {
        let (stream, token) = split_rtmp_stream(stream_name);
        let allowed = self.authorize_play_inner(app, stream, token, None);
        self.rtmp_audit(allowed, &format!("{app}/{stream}"), "read");
        allowed
    }

    fn authorize_publish_peer(
        &self,
        app: &str,
        stream_name: &str,
        peer: Option<std::net::SocketAddr>,
    ) -> bool {
        let (stream, token) = split_rtmp_stream(stream_name);
        let principal = peer.map(|peer| peer.to_string());
        let allowed = self.authorize_publish_inner(app, stream, token, principal.as_deref());
        self.rtmp_audit(allowed, &format!("{app}/{stream}"), "publish");
        allowed
    }

    fn authorize_play_peer(
        &self,
        app: &str,
        stream_name: &str,
        peer: Option<std::net::SocketAddr>,
    ) -> bool {
        let (stream, token) = split_rtmp_stream(stream_name);
        let principal = peer.map(|peer| peer.to_string());
        let allowed = self.authorize_play_inner(app, stream, token, principal.as_deref());
        self.rtmp_audit(allowed, &format!("{app}/{stream}"), "read");
        allowed
    }
}

impl AppAuth {
    fn authorize_publish_inner(
        &self,
        app: &str,
        stream: &str,
        token: Option<&str>,
        principal: Option<&str>,
    ) -> bool {
        let path = format!("{app}/{stream}");
        if let Some(token) = token {
            if (token.starts_with("rk_")
                && token.contains('.')
                && self.key_allowed(token, &path, AccessRole::Publish, "rtmp", principal))
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

    fn authorize_play_inner(
        &self,
        app: &str,
        stream: &str,
        token: Option<&str>,
        principal: Option<&str>,
    ) -> bool {
        let path = format!("{app}/{stream}");
        if let Some(token) = token {
            if (token.starts_with("rk_")
                && token.contains('.')
                && self.key_allowed(token, &path, AccessRole::Read, "rtmp", principal))
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
    fn authorize_publish_peer<'a>(
        &'a self,
        app: &'a str,
        stream_name: &'a str,
        peer: Option<std::net::SocketAddr>,
    ) -> futures::future::BoxFuture<'a, bool> {
        let self_ref = self;
        Box::pin(async move {
            let (stream, _) = split_rtmp_stream(stream_name);
            let path = format!("{app}/{stream}");
            let allowed = self_ref
                .run_hook(
                    HookKind::PublishAuth,
                    &path,
                    "publish",
                    "rtmp",
                    peer.map(|peer| peer.to_string()),
                )
                .await;
            self_ref.rtmp_audit(allowed, &path, "publish");
            allowed
        })
    }

    fn authorize_play_peer<'a>(
        &'a self,
        app: &'a str,
        stream_name: &'a str,
        peer: Option<std::net::SocketAddr>,
    ) -> futures::future::BoxFuture<'a, bool> {
        let self_ref = self;
        Box::pin(async move {
            let (stream, _) = split_rtmp_stream(stream_name);
            let path = format!("{app}/{stream}");
            let allowed = self_ref
                .run_hook(
                    HookKind::PlayAuth,
                    &path,
                    "read",
                    "rtmp",
                    peer.map(|peer| peer.to_string()),
                )
                .await;
            self_ref.rtmp_audit(allowed, &path, "read");
            allowed
        })
    }

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
