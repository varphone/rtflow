use rtsp::{
    EventSink, FrameworkEvent, IpAcl, RateLimitConfig, RtspServer, SecurityConfig, ServerConfig,
    observe::{RejectReason, SessionEvent},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncReadExt, net::TcpStream};

#[derive(Default)]
struct Events(Mutex<Vec<FrameworkEvent>>);
impl EventSink for Events {
    fn record(&self, event: &FrameworkEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

async fn connection_is_closed(address: std::net::SocketAddr) -> bool {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let mut byte = [0];
    matches!(
        tokio::time::timeout(Duration::from_secs(1), stream.read(&mut byte)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}

#[tokio::test]
async fn server_enforces_ip_acl_before_allocating_a_session() {
    let events = Arc::new(Events::default());
    let security = SecurityConfig {
        ip_acl: IpAcl {
            allow: Vec::new(),
            deny: vec!["127.0.0.1/32".parse().unwrap()],
        },
        ..Default::default()
    };
    let config = ServerConfig::builder("127.0.0.1:0".parse().unwrap())
        .security(security)
        .event_sink(events.clone())
        .build()
        .unwrap();
    let server = RtspServer::bind(config).await.unwrap();
    assert!(connection_is_closed(server.local_addr()).await);
    assert!(events.0.lock().unwrap().iter().any(|event| matches!(
        event,
        FrameworkEvent::Session(SessionEvent::Rejected {
            reason: RejectReason::AccessDenied,
            ..
        })
    )));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn connection_token_bucket_rejects_burst_excess() {
    let events = Arc::new(Events::default());
    let security = SecurityConfig {
        connection_rate_limit: Some(RateLimitConfig {
            burst: 1,
            refill_per_second: 0.01,
            idle_retention: Duration::from_secs(60),
            max_tracked_peers: 16,
        }),
        ..Default::default()
    };
    let config = ServerConfig::builder("127.0.0.1:0".parse().unwrap())
        .security(security)
        .event_sink(events.clone())
        .build()
        .unwrap();
    let server = RtspServer::bind(config).await.unwrap();
    let first = TcpStream::connect(server.local_addr()).await.unwrap();
    assert!(connection_is_closed(server.local_addr()).await);
    assert!(events.0.lock().unwrap().iter().any(|event| matches!(
        event,
        FrameworkEvent::Session(SessionEvent::Rejected {
            reason: RejectReason::RateLimited,
            ..
        })
    )));
    drop(first);
    server.shutdown().await.unwrap();
}
