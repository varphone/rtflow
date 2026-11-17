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
fn srt_push_entries_are_validated() {
    let mut config = Config::default();
    config.srt.push.push(SrtPush {
        mount: "/live/cam".into(),
        remote: "127.0.0.1:9000".into(),
        ..SrtPush::default()
    });
    assert!(config.validate().is_ok());
    config.srt.push[0].mount = "live/cam".into();
    assert!(config.validate().is_err());
    config.srt.push[0].mount = "/live/cam".into();
    config.srt.push[0].remote = "nope".into();
    assert!(config.validate().is_err());
}

#[test]
fn token_secrets_need_32_bytes() {
    let mut config = Config::default();
    config.auth.mode = AuthMode::Token;
    config.auth.token_secrets = vec!["short".into()];
    assert!(config.validate().is_err());
    config.auth.token_secrets = vec!["0123456789abcdef0123456789abcdef".into()];
    assert!(config.validate().is_ok());
}

#[test]
fn proxy_section_validates_and_redacts() {
    let mut config = Config::default();
    config.proxy.mounts.push(ProxyMount {
        path: "/proxy/cam".into(),
        upstreams: vec![ProxyUpstream {
            url: "rtsp://user:secret@127.0.0.1:8554/live".into(),
            username: Some("user".into()),
            password: Some("secret".into()),
            ..ProxyUpstream::default()
        }],
        ..ProxyMount::default()
    });
    assert!(config.validate().is_ok());
    let text = config.redacted().to_toml().unwrap();
    assert!(!text.contains("user:secret"), "{text}");
    assert!(!text.contains("password = \"secret\""), "{text}");

    config.proxy.mounts.push(config.proxy.mounts[0].clone());
    assert!(config.validate().is_err());
    config.proxy.mounts.pop();
    config.proxy.mounts[0].path = "bad".into();
    assert!(config.validate().is_err());
    config.proxy.mounts[0].path = "/proxy/cam".into();
    config.proxy.mounts[0].upstreams[0].url = "http://127.0.0.1/live".into();
    assert!(config.validate().is_err());
    config.proxy.mounts[0].upstreams[0].url = "rtsp://127.0.0.1/live".into();
    config.proxy.allowed_hosts = vec!["10.0.0.0/99".into()];
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

#[test]
fn proxy_outputs_defaults_and_validation() {
    let outputs: ProxyOutputs = serde_json::from_str(r#"{"record":true}"#).unwrap();
    assert!(outputs.read && outputs.hls && outputs.flv);
    assert!(!outputs.dash && !outputs.webrtc && outputs.record);
    assert_eq!(outputs.max_readers, 0);

    let mut mount = ProxyMount {
        path: "/pull/cam".into(),
        outputs: Some(ProxyOutputs {
            read: false,
            hls: false,
            dash: false,
            flv: false,
            webrtc: false,
            record: false,
            max_readers: 0,
        }),
        ..ProxyMount::default()
    };
    mount.upstreams.push(ProxyUpstream {
        url: "rtsp://127.0.0.1/live".into(),
        ..ProxyUpstream::default()
    });
    let error = mount.validate().unwrap_err();
    assert!(error.contains("neither live view nor recording"), "{error}");

    mount.outputs = None;
    assert!(mount.validate().is_ok());
}

#[test]
fn proxy_schedule_windows_and_overnight() {
    use chrono::{Local, TimeZone};

    let mut schedule = ProxySchedule {
        days: vec![ScheduleDay::Wed],
        start: "08:00".into(),
        end: "18:00".into(),
    };
    schedule.validate().unwrap();
    let wed_10 = Local.with_ymd_and_hms(2026, 11, 18, 10, 30, 0).unwrap();
    let wed_18 = Local.with_ymd_and_hms(2026, 11, 18, 18, 0, 0).unwrap();
    let tue_10 = Local.with_ymd_and_hms(2026, 11, 17, 10, 30, 0).unwrap();
    assert!(schedule.allows(wed_10));
    assert!(!schedule.allows(wed_18), "end is exclusive");
    assert!(!schedule.allows(tue_10));

    schedule.start = "22:00".into();
    schedule.end = "06:00".into();
    schedule.validate().unwrap();
    let wed_23 = Local.with_ymd_and_hms(2026, 11, 18, 23, 0, 0).unwrap();
    let thu_03 = Local.with_ymd_and_hms(2026, 11, 19, 3, 0, 0).unwrap();
    let thu_07 = Local.with_ymd_and_hms(2026, 11, 19, 7, 0, 0).unwrap();
    let tue_23 = Local.with_ymd_and_hms(2026, 11, 17, 23, 0, 0).unwrap();
    assert!(schedule.allows(wed_23));
    assert!(schedule.allows(thu_03), "overnight continues into Thursday");
    assert!(!schedule.allows(thu_07));
    assert!(!schedule.allows(tue_23));

    schedule.days.clear();
    assert!(schedule.validate().is_err());
    schedule.days.push(ScheduleDay::Wed);
    schedule.end = "22:00".into();
    assert!(
        schedule.validate().is_err(),
        "zero-length windows are rejected"
    );
}

#[test]
fn dash_section_parses_and_validates() {
    let config = Config::from_toml(
        "[dash]\nsegment_duration = \"1s\"\nwindow = 6\nmin_buffer_time = \"3s\"\ntime_shift_buffer_depth = \"6s\"\nsuggested_presentation_delay = \"4s\"\n",
    )
    .unwrap();
    assert_eq!(config.dash.segment_duration, Some(Duration::from_secs(1)));
    assert_eq!(config.dash.window, Some(6));
    assert_eq!(config.dash.min_buffer_time, Duration::from_secs(3));
    assert_eq!(
        config.dash.time_shift_buffer_depth,
        Some(Duration::from_secs(6))
    );
    assert_eq!(
        config.dash.suggested_presentation_delay,
        Some(Duration::from_secs(4))
    );
    config.validate().unwrap();

    let mut invalid = Config::default();
    invalid.dash.window = Some(0);
    assert!(invalid.validate().is_err());
    invalid.dash.window = None;
    invalid.dash.segment_duration = Some(Duration::ZERO);
    assert!(invalid.validate().is_err());
    invalid.dash.segment_duration = None;
    invalid.dash.time_shift_buffer_depth = Some(Duration::ZERO);
    assert!(invalid.validate().is_err());
}
