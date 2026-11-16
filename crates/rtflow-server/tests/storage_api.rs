//! Business data persists in SQLite across restarts and legacy JSON imports.
use rtflow_server::{Config, server::ServerApp};
use serde_json::json;

fn config(state: &std::path::Path) -> Config {
    let mut config = Config::default();
    config.server.name = "storage-test".into();
    config.server.state_dir = state.to_path_buf();
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = "pass".into();
    config.discovery.public = true;
    config
}

#[tokio::test]
async fn legacy_paths_import_and_proxy_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    std::fs::write(
        dir.path().join("state/paths.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "paths": [{
                "name": "live/legacy",
                "enabled": true,
                "publish": true,
                "read": true,
                "record": false,
                "hls": true,
                "dash": false,
                "flv": false,
                "webrtc": false,
                "max_readers": 3,
                "tenant": null,
                "labels": {},
                "metadata": null,
                "source": "api",
                "created_at": 1,
                "updated_at": 2,
                "expires_at": null
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let handle = ServerApp::start(config(&dir.path().join("state")))
        .await
        .unwrap();
    let base = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let paths = client
        .get(format!("{base}/admin/api/v1/paths"))
        .basic_auth("admin", Some("pass"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(paths.contains("live/legacy"), "{paths}");
    assert!(
        !dir.path().join("state/paths.json").exists(),
        "legacy file must be archived"
    );
    assert!(
        std::fs::read_dir(dir.path().join("state"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("paths.json.migrated")),
        "legacy path file was not archived"
    );

    let created = client
        .post(format!("{base}/admin/api/v1/proxies"))
        .basic_auth("admin", Some("pass"))
        .json(&json!({
            "path": "/proxy/one",
            "upstreams": [{"url": "rtsp://127.0.0.1:1/live", "transport": "tcp"}]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        created.status().is_success(),
        "{}",
        created.text().await.unwrap()
    );
    handle.shutdown().await;

    // Restart on the same state directory: the SQLite row must survive.
    let handle = ServerApp::start(config(&dir.path().join("state")))
        .await
        .unwrap();
    let base = format!("http://{}", handle.http_addr);
    let configs = client
        .get(format!("{base}/admin/api/v1/proxies/config"))
        .basic_auth("admin", Some("pass"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(configs.contains("/proxy/one"), "{configs}");
    assert!(dir.path().join("state/rtflow.db").exists());
    handle.shutdown().await;
}

#[tokio::test]
async fn srt_endpoint_crud_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let handle = ServerApp::start(config(&dir.path().join("state")))
        .await
        .unwrap();
    let base = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();
    let created: serde_json::Value = client
        .post(format!("{base}/admin/api/v1/srt/pulls"))
        .basic_auth("admin", Some("pass"))
        .json(&json!({"remote": "127.0.0.1:9", "mount": "/live/srt"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_owned();
    let list = client
        .get(format!("{base}/admin/api/v1/srt/pulls"))
        .basic_auth("admin", Some("pass"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(list.contains(&id), "{list}");
    let deleted = client
        .delete(format!("{base}/admin/api/v1/srt/pulls/{id}"))
        .basic_auth("admin", Some("pass"))
        .send()
        .await
        .unwrap();
    assert!(deleted.status().is_success());
    handle.shutdown().await;
}
