//! HTTPS listener: a generated self-signed certificate must serve the API.
use rtflow_server::{Config, server::ServerApp};
use std::{fs, path::Path};

fn write_certificate(directory: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let certificate = directory.join("cert.pem");
    let key = directory.join("key.pem");
    fs::write(&certificate, certified.cert.pem()).unwrap();
    fs::write(&key, certified.signing_key.serialize_pem()).unwrap();
    (certificate, key)
}

#[tokio::test]
async fn https_listener_serves_health_and_api() {
    let dir = tempfile::tempdir().unwrap();
    let (certificate, key) = write_certificate(dir.path());
    let mut config = Config::default();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.recording.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = "https-pass".into();
    config.http.tls = Some(rtflow_server::config::TlsFiles {
        certificate,
        private_key: key,
    });
    let handle = ServerApp::start(config).await.unwrap();
    let base = format!("https://{}", handle.http_addr);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let health = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);
    let paths = client
        .get(format!("{base}/admin/api/v1/paths"))
        .basic_auth("admin", Some("https-pass"))
        .send()
        .await
        .unwrap();
    assert_eq!(paths.status(), 200);
    handle.shutdown().await;
}
