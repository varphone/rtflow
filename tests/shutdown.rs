use rtsp::{RtspServer, ServerConfig};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

#[tokio::test]
async fn shutdown_waits_for_listener_and_active_sessions_to_close() {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.shutdown_grace = Duration::from_secs(1);
    let server = RtspServer::bind(config).await.unwrap();
    let address = server.local_addr();
    let connection = TcpStream::connect(address).await.unwrap();

    server.shutdown().await.unwrap();
    let listener = TcpListener::bind(address).await.unwrap();
    drop(listener);

    let mut byte = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(1), connection.readable())
        .await
        .unwrap();
    closed.unwrap();
    match connection.try_read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() != std::io::ErrorKind::WouldBlock => {}
        other => panic!("session socket remained open: {other:?}"),
    }
}
