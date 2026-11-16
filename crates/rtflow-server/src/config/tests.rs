//! Configuration tests.

use super::*;

#[test]
fn defaults_validate() {
    let config = Config::default();
    config.validate().unwrap();
}

#[test]
fn rejects_unknown_keys() {
    let error = Config::from_toml("[rtsp]\nlissten = \"0.0.0.0:8554\"\n").unwrap_err();
    assert!(matches!(error, ConfigError::Parse(..)));
}

#[test]
fn glob_matching() {
    assert!(glob_match("live/*", "live/cam"));
    assert!(!glob_match("live/*", "live/a/b"));
    assert!(glob_match("live/**", "live/a/b"));
    assert!(glob_match("**", "anything"));
    assert!(glob_match("**", ""));
}

#[test]
fn most_specific_rule_wins() {
    let mut config = Config::default();
    config.paths.insert(
        "**".into(),
        PathRule {
            hls: true,
            ..PathRule::default()
        },
    );
    config.paths.insert(
        "private/**".into(),
        PathRule {
            read: false,
            ..PathRule::default()
        },
    );
    let (pattern, rule) = config.path_rule("private/cam");
    assert_eq!(pattern, "private/**");
    assert!(!rule.read);
}

#[test]
fn printed_config_redacts_secrets() {
    let mut config = Config::default();
    config.http.admin_password = "secret".into();
    config.auth.token_secrets = vec!["token-secret".into()];
    config.hooks.secret = Some("hook-secret".into());
    config.http.api_tokens = vec!["api-token".into()];
    let text = config.redacted().to_toml().unwrap();
    assert!(!text.contains("token-secret"));
    assert!(!text.contains("hook-secret"));
    assert!(!text.contains("api-token"));
    assert!(text.contains("<redacted>"));
    // The original config keeps the real values.
    assert_eq!(config.http.admin_password, "secret");
}

#[test]
fn srt_passphrase_is_validated() {
    let mut config = Config::default();
    config.srt.passphrase = Some("short".into());
    assert!(config.validate().is_err());
    config.srt.passphrase = Some("a-long-enough-passphrase".into());
    config.srt.pbkeylen = 24;
    assert!(config.validate().is_ok());
    config.srt.pbkeylen = 17;
    assert!(config.validate().is_err());
}

#[test]
fn srt_pull_entries_are_validated() {
    let mut config = Config::default();
    config.srt.pull.push(SrtPull {
        remote: "127.0.0.1:9000".into(),
        stream_id: Some("live/cam".into()),
        ..SrtPull::default()
    });
    assert!(config.validate().is_ok());
    config.srt.pull[0].remote = "not-an-address".into();
    assert!(config.validate().is_err());
    config.srt.pull[0].remote = "127.0.0.1:9000".into();
    config.srt.pull[0].mount = Some("live/cam".into());
    assert!(config.validate().is_err());
    config.srt.pull[0].mount = Some("/live/cam".into());
    config.srt.pull[0].passphrase = Some("short".into());
    assert!(config.validate().is_err());
}

#[test]
fn comments_do_not_require_env_values() {
    assert_eq!(
        expand_env("# optional ${MISSING_FOR_TEST}\n").unwrap(),
        "# optional ${MISSING_FOR_TEST}\n"
    );
}

#[test]
fn env_expansion_requires_values() {
    unsafe { std::env::set_var("RTFLOW_TEST_TOKEN", "abc") };
    assert_eq!(expand_env("x=${RTFLOW_TEST_TOKEN}").unwrap(), "x=abc");
    assert!(expand_env("x=${RTFLOW_TEST_MISSING}").is_err());
}
