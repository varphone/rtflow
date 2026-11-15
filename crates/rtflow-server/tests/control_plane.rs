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

    let discovery = client
        .get(format!("{base}/api/v1/discovery"))
        .send()
        .await
        .unwrap();
    assert_eq!(discovery.status(), 200);

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

    let update = client
        .patch(format!("{base}/admin/api/v1/paths/live/cam"))
        .basic_auth("admin", Some("testpass"))
        .json(&json!({"read": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(update.status(), 200);

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
