use super::*;

#[test]
fn stream_key_auth_open_mode() {
    let auth = StreamKeyAuthenticator::default();
    // Empty list = allow all
    assert!(auth.authorize_publish("live", "stream1"));
    assert!(auth.authorize_publish("any", "thing"));
}

#[test]
fn stream_key_auth_restricts() {
    let auth =
        StreamKeyAuthenticator::new(vec!["live/stream1".to_string(), "live/cam2".to_string()]);
    assert!(auth.authorize_publish("live", "stream1"));
    assert!(auth.authorize_publish("live", "cam2"));
    assert!(!auth.authorize_publish("live", "unknown"));
    assert!(!auth.authorize_publish("other", "stream1"));
}

#[test]
fn stream_key_auth_allow_method() {
    let mut auth = StreamKeyAuthenticator::default();
    auth.allow("live/test");
    assert!(auth.authorize_publish("live", "test"));
    assert!(!auth.authorize_publish("live", "other"));
}

#[test]
fn rtmp_url_token_is_verified_against_app_and_stream() {
    let token = rtflow_rtsp::token_auth::TokenConfig::new(b"secret".to_vec());
    let value = rtflow_rtsp::token_auth::generate_token(&token, "/live/cam");
    let stream = format!("cam?token={value}");
    let (name, parsed) = super::commands::split_stream_token(&stream, &token);
    assert_eq!(name, "cam");
    assert_eq!(parsed, Some(value.as_str()));
    assert!(super::commands::token_allows(
        Some(&token),
        "live",
        name,
        parsed
    ));
    assert!(!super::commands::token_allows(
        Some(&token),
        "live",
        name,
        Some("invalid")
    ));
    assert!(!super::commands::token_allows(
        Some(&token),
        "live",
        name,
        None
    ));
}
