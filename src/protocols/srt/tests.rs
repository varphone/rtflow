use super::*;

use super::*;
use std::time::Duration;

#[tokio::test]
async fn server_binds_and_shuts_down() {
    let mounts = Arc::new(MountRegistry::new(10));
    let config = SrtConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    };
    let server = SrtServer::with_config(config, mounts).await.unwrap();
    let addr = server.local_addr().unwrap();
    assert_ne!(addr.port(), 0);

    let cancel = server.cancellation_token();
    let handle = tokio::spawn(server.run());

    tokio::time::sleep(Duration::from_millis(10)).await;
    cancel.cancel();

    handle.await.unwrap().unwrap();
}

#[test]
fn parse_stream_id_with_resource() {
    // Build a fake handshake with SID extension
    let mut hs = vec![0u8; 48]; // header
    // SID: "#!::u=publisher,r=live/cam1" = 28 bytes, pad to 28 (7 words)
    let sid = b"#!::u=publisher,r=live/cam1";
    let padded_len = (sid.len() + 3) & !3; // round up to 4
    let words = (padded_len / 4) as u16;
    hs.extend_from_slice(&5u16.to_be_bytes()); // type = SRT_CMD_SID
    hs.extend_from_slice(&words.to_be_bytes()); // length in words
    hs.extend_from_slice(sid);
    hs.extend_from_slice(&vec![0u8; padded_len - sid.len()]); // null padding

    let result = parse_stream_id(&hs);
    assert_eq!(result, Some("live/cam1".to_string()));
}

#[test]
fn parse_stream_id_plain() {
    let mut hs = vec![0u8; 48];
    hs.extend_from_slice(&5u16.to_be_bytes());
    hs.extend_from_slice(&2u16.to_be_bytes()); // 2 words = 8 bytes
    let sid = b"stream1\0"; // 8 bytes
    hs.extend_from_slice(sid);

    let result = parse_stream_id(&hs);
    assert_eq!(result, Some("stream1".to_string()));
}

#[test]
fn parse_stream_id_none() {
    let hs = vec![0u8; 48]; // no extensions
    assert_eq!(parse_stream_id(&hs), None);
}
