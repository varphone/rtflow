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
