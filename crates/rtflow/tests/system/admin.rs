//! Admin console API integration.
#![cfg(feature = "admin")]

use rtflow::{
    MountRegistry,
    admin::{Admin, AdminConfig},
    http::{HttpConfig, HttpServer},
};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (key, value) in headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let header_end = text.find("\r\n\r\n").unwrap_or(0);
    let headers = text[..header_end].to_owned();
    let body = text[header_end + 4..].to_owned();
    (status, headers, body)
}

#[tokio::test]
async fn admin_requires_login_and_accepts_csrf_protected_actions() {
    let registry = Arc::new(MountRegistry::new(4));
    let config = AdminConfig::new("admin", "secret").unwrap();
    let admin = Admin::new(config, registry).unwrap();
    let mut http = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    admin.register(&http).await;
    let addr = http.local_addr();
    tokio::spawn(async move { http.run().await });

    let (status, _, body) = request(addr, "GET", "/admin/api/session", "", &[]).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"authenticated\":false"), "{body}");

    let (status, _, body) = request(addr, "GET", "/admin/api/streams", "", &[]).await;
    assert_eq!(status, 401, "{body}");

    let (status, headers, body) = request(
        addr,
        "POST",
        "/admin/api/login",
        r#"{"username":"admin","password":"secret"}"#,
        &[("Content-Type", "application/json")],
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        headers.to_ascii_lowercase().contains("set-cookie:"),
        "{headers}"
    );
    let cookie = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().split(';').next().unwrap_or("").to_owned())
        .expect("session cookie");
    let csrf: String = serde_json::from_str::<serde_json::Value>(&body)
        .unwrap()
        .get("csrf")
        .and_then(|value| value.as_str())
        .unwrap()
        .to_owned();

    let (status, _, body) = request(
        addr,
        "GET",
        "/admin/api/streams",
        "",
        &[("Cookie", &cookie)],
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"items\""), "{body}");

    let (status, _, _) = request(
        addr,
        "DELETE",
        "/admin/api/streams?path=%2Flive%2Fmissing",
        "",
        &[("Cookie", &cookie), ("X-CSRF-Token", "wrong")],
    )
    .await;
    assert_eq!(status, 403);
    let (status, _, _) = request(
        addr,
        "DELETE",
        "/admin/api/streams?path=%2Flive%2Fmissing",
        "",
        &[("Cookie", &cookie), ("X-CSRF-Token", &csrf)],
    )
    .await;
    assert_eq!(status, 404);
}
