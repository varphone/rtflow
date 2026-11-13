use super::*;

pub(crate) async fn authorize_request(
    req: &crate::rtsp::Request,
    peer: SocketAddr,
    state: &State,
    cseq: String,
) -> Option<Response> {
    let registered = registered_mount_for_uri(state, &req.uri).await;
    let path = registered
        .as_ref()
        .map(|(path, _)| path.clone())
        .unwrap_or_else(|| uri_path(&req.uri));
    let publisher_mount = registered
        .as_ref()
        .is_some_and(|(_, mount)| matches!(mount.config, MountConfig::Publisher { .. }));
    let role = if matches!(req.method, Method::Announce | Method::Record)
        || (req.method == Method::Setup && publisher_mount)
    {
        AccessRole::Publish
    } else {
        AccessRole::Read
    };
    let decision = state
        .config
        .authenticator
        .authorize(AuthRequest {
            method: req.method,
            path: &path,
            peer_addr: peer,
            role,
            authorization: req.headers.get("authorization").map(String::as_str),
        })
        .await;
    if !matches!(decision, AuthDecision::Allow) {
        emit(
            &state.config.event_sink,
            FrameworkEvent::Security(SecurityEvent::AuthenticationFailed {
                peer,
                mount: Some(MountId::from(path.as_str())),
            }),
        );
        let delay = state.config.security.authentication_failure_delay;
        if delay != std::time::Duration::ZERO {
            // The delay must not outlive the session: server shutdown cancels
            // the session token, so wait on both.
            tokio::select! {
                _ = state.cancel.cancelled() => {}
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }
    match decision {
        AuthDecision::Allow => None,
        AuthDecision::Challenge { realm } => {
            let mut response = simple_response(401, "Unauthorized", cseq);
            response.headers.insert(
                "www-authenticate".into(),
                format!("Basic realm=\"{realm}\""),
            );
            Some(response)
        }
        AuthDecision::DigestChallenge { realm, nonce } => {
            let mut response = simple_response(401, "Unauthorized", cseq);
            response.headers.insert(
                "www-authenticate".into(),
                format!("Digest realm=\"{realm}\", nonce=\"{nonce}\", algorithm=MD5, qop=\"auth\""),
            );
            Some(response)
        }
        AuthDecision::Deny => Some(simple_response(403, "Forbidden", cseq)),
    }
}
