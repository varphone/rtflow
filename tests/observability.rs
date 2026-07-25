mod support;

use rtsp::{
    EventSink, FrameworkEvent, MountConfig, ResourceLimits, RtspServer, ServerConfig,
    observe::{MountEvent, RejectReason, ServerEvent, SessionEvent},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use support::{RawRtsp, description};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Default)]
struct Collector {
    events: Mutex<Vec<FrameworkEvent>>,
}

impl Collector {
    fn snapshot(&self) -> Vec<FrameworkEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl EventSink for Collector {
    fn record(&self, event: &FrameworkEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
    }
}

#[tokio::test]
async fn emits_lifecycle_events_and_releases_peer_quota() {
    let collector = Arc::new(Collector::default());
    let limits = ResourceLimits {
        max_sessions_per_ip: 1,
        ..Default::default()
    };
    let config = ServerConfig::builder("127.0.0.1:0".parse().unwrap())
        .max_sessions(4)
        .resource_limits(limits)
        .event_sink(collector.clone())
        .build()
        .unwrap();
    let mut server = RtspServer::bind(config).await.unwrap();
    let address = server.local_addr();
    server
        .register_mount(
            Arc::from("/observed"),
            MountConfig::Local {
                description: description("observed", &[rtsp::Codec::H264]),
            },
        )
        .await
        .unwrap();

    let mut first = RawRtsp::connect(address).await;
    assert_eq!(
        first
            .request("OPTIONS", &format!("rtsp://{address}/"), "")
            .await
            .status,
        200
    );

    let mut rejected = TcpStream::connect(address).await.unwrap();
    rejected
        .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n")
        .await
        .unwrap();
    let mut byte = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(1), rejected.read(&mut byte))
        .await
        .expect("peer-limited connection was not closed");
    match closed {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("unexpected peer-limit read result: {other:?}"),
    }

    drop(first);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if collector
                .snapshot()
                .iter()
                .any(|event| matches!(event, FrameworkEvent::Session(SessionEvent::Closed { .. })))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let mut admitted_again = RawRtsp::connect(address).await;
    assert_eq!(
        admitted_again
            .request("OPTIONS", &format!("rtsp://{address}/"), "")
            .await
            .status,
        200
    );
    drop(admitted_again);

    server.unregister_mount("/observed").await.unwrap();
    let report = server.shutdown_with_report().await.unwrap();
    assert_eq!(report.failed_tasks, 0);
    assert_eq!(report.panicked_tasks, 0);

    let events = collector.snapshot();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, FrameworkEvent::Server(ServerEvent::Started { .. })))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        FrameworkEvent::Mount(MountEvent::Registered { id }) if id.as_str() == "/observed"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        FrameworkEvent::Session(SessionEvent::Rejected {
            reason: RejectReason::PeerLimit,
            ..
        })
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        FrameworkEvent::Mount(MountEvent::Unregistered { id }) if id.as_str() == "/observed"
    )));
    assert!(matches!(
        events.last(),
        Some(FrameworkEvent::Server(ServerEvent::Stopped))
    ));
}

#[test]
fn builders_reject_zero_limits_before_runtime_start() {
    assert!(
        ServerConfig::builder("127.0.0.1:0".parse().unwrap())
            .max_sessions(0)
            .build()
            .is_err()
    );
    assert!(
        rtsp::client::ClientOptions::builder()
            .request_timeout(Duration::ZERO)
            .build()
            .is_err()
    );
}
