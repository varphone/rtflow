//! Unit tests for `auth::AppAuth`.
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

fn auth_with_key(max_connections: usize) -> (AppAuth, String) {
    let mut config = Config::default();
    config.server.state_dir = std::env::temp_dir().join(format!(
        "rtflow-keylimit-{}-{max_connections}",
        std::process::id()
    ));
    config.auth.mode = AuthMode::Token;
    config.auth.enforce_path_rules = true;
    let keys = Arc::new(KeyStore::new(&config).unwrap());
    let issued = keys
        .issue(crate::keys::KeySpec {
            roles: vec!["publish".into()],
            max_connections,
            ..crate::keys::KeySpec::default()
        })
        .unwrap();
    let token = format!("{}.{}", issued.status.kid, issued.secret);
    let paths = Arc::new(PathStore::new(&config, false).unwrap());
    let mounts = Arc::new(MountRegistry::new(16));
    let auth = AppAuth::new(&config, keys, paths, mounts).unwrap();
    (auth, token)
}

#[test]
fn max_connections_is_enforced_per_peer() {
    let (auth, token) = auth_with_key(1);
    let header = format!("Bearer {token}");
    let request = |peer: &'static str| AuthRequest {
        method: Method::Announce,
        path: "/live/cam",
        peer_addr: peer.parse().unwrap(),
        role: AccessRole::Publish,
        authorization: Some(&header),
    };
    assert!(matches!(
        auth.authorize_sync(&request("127.0.0.1:1000")),
        AuthDecision::Allow
    ));
    assert!(matches!(
        auth.authorize_sync(&request("127.0.0.1:2000")),
        AuthDecision::Deny
    ));
    // The same peer may reconnect.
    assert!(matches!(
        auth.authorize_sync(&request("127.0.0.1:1000")),
        AuthDecision::Allow
    ));
}

#[test]
fn srt_signed_token_is_verified() {
    let mut config = Config::default();
    config.server.state_dir =
        std::env::temp_dir().join(format!("rtflow-srt-auth-{}", std::process::id()));
    config.auth.mode = AuthMode::Token;
    config.auth.token_secrets = vec!["srt-secret-0123456789abcdef0123456789".into()];
    let keys = Arc::new(KeyStore::new(&config).unwrap());
    let paths = Arc::new(PathStore::new(&config, false).unwrap());
    let mounts = Arc::new(MountRegistry::new(16));
    let auth = AppAuth::new(&config, keys, paths, mounts).unwrap();
    let payload = TokenPayload {
        sub: "srt".into(),
        role: "publish".into(),
        path: "live/**".into(),
        exp: unix_now() + 60,
        nonce: "srt-1".into(),
        tenant: None,
    };
    let token = auth.issue_signed_token(&payload).unwrap();
    let peer: std::net::SocketAddr = "127.0.0.1:9000".parse().unwrap();
    assert!(auth.verify_srt(&format!("live/cam?token={token}"), peer));
    assert!(!auth.verify_srt("live/cam", peer));
    // Replay of a nonce is rejected by the signed-token verifier.
    assert!(!auth.verify_srt(&format!("live/cam?token={token}"), peer));
}
