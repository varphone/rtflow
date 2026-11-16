//! Admin SPA assets are embedded and served with module-friendly MIME types.
use rtflow_server::{Config, server::ServerApp};

#[tokio::test]
async fn admin_spa_assets_are_served() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.name = "admin-ui".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = "pass".into();
    let handle = ServerApp::start(config).await.unwrap();
    let base = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let index = client.get(format!("{base}/admin")).send().await.unwrap();
    assert_eq!(index.status(), 200);
    let html = index.text().await.unwrap();
    assert!(html.contains("type=\"module\""), "{html}");

    let page = client
        .get(format!("{base}/admin/assets/js/pages/paths.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(
        page.headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/javascript; charset=utf-8")
    );
    assert!(
        page.text()
            .await
            .unwrap()
            .contains("export async function render")
    );

    assert_eq!(
        client
            .get(format!("{base}/admin/assets/js/does-not-exist.js"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    handle.shutdown().await;
}
