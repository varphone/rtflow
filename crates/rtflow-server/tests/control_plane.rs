use rtflow_server::{Config, config::PathRule, server::ServerApp};
use serde_json::json;

fn config(dir: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.server.name = "test".into();
    config.server.state_dir = dir.join("state");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.recording.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = "testpass".into();
    config.discovery.public = true;
    config.paths.insert("live/*".into(), PathRule::default());
    config
}

#[tokio::test]
async fn control_plane_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let handle = ServerApp::start(config(dir.path())).await.unwrap();
    let base = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let health = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);

    // Control API: Basic, browser session and bearer token all authorize.
    let wrong = client
        .get(format!("{base}/admin/api/v1/paths"))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let basic = client
        .get(format!("{base}/admin/api/v1/paths"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(basic.status(), 200);
    let login = client
        .post(format!("{base}/admin/api/login"))
        .json(&json!({"username": "admin", "password": "testpass"}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 200);
    let cookie = login
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::to_owned)
        .expect("login sets a session cookie");

    let discovery = client
        .get(format!("{base}/api/v1/discovery"))
        .send()
        .await
        .unwrap();
    assert_eq!(discovery.status(), 200);

    // The session cookie from the console login also authorizes the v1 API.
    let session = client
        .get(format!("{base}/admin/api/v1/paths"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(session.status(), 200);

    let create = client
        .post(format!("{base}/admin/api/v1/paths"))
        .basic_auth("admin", Some("testpass"))
        .json(&json!({"name": "live/cam", "record": true, "read": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 200, "{}", create.text().await.unwrap());

    let paths = client
        .get(format!("{base}/admin/api/v1/paths"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert!(paths.to_string().contains("live/cam"));

    let detail = client
        .get(format!("{base}/admin/api/v1/paths/live/cam"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(detail.status(), 200);

    let stale = client
        .patch(format!("{base}/admin/api/v1/paths/live/cam"))
        .basic_auth("admin", Some("testpass"))
        .header("if-match", "999999")
        .json(&json!({"read": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 412, "stale If-Match must be rejected");

    let update = client
        .patch(format!("{base}/admin/api/v1/paths/live/cam"))
        .basic_auth("admin", Some("testpass"))
        .json(&json!({"read": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(update.status(), 200);
    let updated: serde_json::Value = update.json().await.unwrap();
    let version = updated["updated_at"].to_string();
    let fresh = client
        .patch(format!("{base}/admin/api/v1/paths/live/cam"))
        .basic_auth("admin", Some("testpass"))
        .header("if-match", version.trim_matches('"'))
        .json(&json!({"record": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(fresh.status(), 200, "matching If-Match must succeed");

    // A retried POST with the same Idempotency-Key creates one resource only.
    let first = client
        .post(format!("{base}/admin/api/v1/paths"))
        .basic_auth("admin", Some("testpass"))
        .header("idempotency-key", "create-live-idem")
        .json(&json!({"name": "live/idem"}))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let second = client
        .post(format!("{base}/admin/api/v1/paths"))
        .basic_auth("admin", Some("testpass"))
        .header("idempotency-key", "create-live-idem")
        .json(&json!({"name": "live/idem"}))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);

    let key = client
        .post(format!("{base}/admin/api/v1/keys"))
        .basic_auth("admin", Some("testpass"))
        .json(&json!({"name": "obs", "roles": ["publish"], "paths": ["live/*"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(key.status(), 200);
    let key: serde_json::Value = key.json().await.unwrap();
    assert!(key["secret"].as_str().is_some());

    handle.shutdown().await;

    // API paths and keys survive a restart.
    let handle = ServerApp::start(config(dir.path())).await.unwrap();
    let base = format!("http://{}", handle.http_addr);
    let paths = client
        .get(format!("{base}/admin/api/v1/paths"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(paths.contains("live/cam"), "{paths}");
    let paths = client
        .get(format!("{base}/admin/api/v1/paths/live/cam"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(paths.contains("\"read\":false"), "{paths}");
    assert!(
        paths.contains("\"record\":true"),
        "PATCH must preserve omitted fields: {paths}"
    );

    let keys = client
        .get(format!("{base}/admin/api/v1/keys"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(keys.contains("rk_"), "{keys}");
    handle.shutdown().await;
}

#[tokio::test]
async fn discovery_token_gates_public_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.http.admin = false;
    config.discovery.public = false;
    config.discovery.token = Some("discovery-secret".into());
    let handle = ServerApp::start(config).await.unwrap();
    let base = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/api/v1/discovery"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{base}/api/v1/discovery?token=discovery-secret"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn api_token_authorizes_control_plane() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    config.http.api_tokens = vec!["api-secret".into()];
    let handle = ServerApp::start(config).await.unwrap();
    let base = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/admin/api/v1/paths"))
            .bearer_auth("api-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{base}/admin/api/v1/paths"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    handle.shutdown().await;
}
