//! Client observability tests: ClientEvent sequences and metrics wiring.
use rtflow::client::{ClientOptions, ClientReconnectPolicy, RtspClient};
use rtflow::observe::ClientEvent;
use rtflow::transport::UpstreamTransport;
use rtflow::{Codec, EventSink, FrameworkEvent, rtsp::Method};
use rtflow_testkit::{RawRtspServer, ServerReply, describe_reply, reply, with_header};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;

#[derive(Default)]
struct Collector {
    events: Mutex<Vec<FrameworkEvent>>,
}

impl Collector {
    fn client_events(&self) -> Vec<ClientEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|event| match event {
                FrameworkEvent::Client(client) => Some(client.clone()),
                _ => None,
            })
            .collect()
    }
}

impl EventSink for Collector {
    fn record(&self, event: &FrameworkEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn interleaved_ok(request: &rtflow::rtsp::Request) -> rtflow::rtsp::Response {
    with_header(
        with_header(reply(request, 200, "OK"), "session", "sess-1"),
        "transport",
        "RTP/AVP/TCP;unicast;interleaved=0-1",
    )
}

fn interleaved_options(sink: Arc<dyn EventSink>) -> ClientOptions {
    ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .event_sink(sink)
        .build()
        .unwrap()
}

#[tokio::test]
async fn connect_emits_connected_event() {
    let collector = Arc::new(Collector::default());
    let description = rtflow_testkit::description("events", &[Codec::H264]);
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(interleaved_ok(request)),
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let session = RtspClient::connect(
        server.url("/stream"),
        interleaved_options(collector.clone()),
    )
    .await
    .unwrap();
    let events = collector.client_events();
    let connected = events.iter().find_map(|event| match event {
        ClientEvent::Connected {
            transport, peer, ..
        } => Some((*transport, *peer)),
        _ => None,
    });
    assert_eq!(
        connected,
        Some(("tcp-interleaved", server.address())),
        "expected a Connected event for the negotiated transport, got {events:?}"
    );
    drop(session);
}

#[tokio::test]
async fn fallback_emits_fallback_then_connected() {
    let collector = Arc::new(Collector::default());
    let setups = Arc::new(AtomicU32::new(0));
    let counter = setups.clone();
    let description = rtflow_testkit::description("events", &[Codec::H264]);
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => {
            let attempt = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                ServerReply::Response(reply(request, 461, "Unsupported transport"))
            } else {
                ServerReply::Response(interleaved_ok(request))
            }
        }
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let session = RtspClient::connect(
        server.url("/stream"),
        ClientOptions::builder()
            .event_sink(collector.clone())
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let events = collector.client_events();
    let fallback_index = events
        .iter()
        .position(|event| matches!(event, ClientEvent::Fallback { .. }));
    let connected_index = events
        .iter()
        .position(|event| matches!(event, ClientEvent::Connected { .. }));
    assert!(
        fallback_index.is_some() && connected_index.is_some(),
        "expected Fallback and Connected events, got {events:?}"
    );
    assert!(
        fallback_index.unwrap() < connected_index.unwrap(),
        "Fallback must precede Connected, got {events:?}"
    );
    drop(session);
}

#[tokio::test]
async fn keep_alive_failure_emits_event() {
    let collector = Arc::new(Collector::default());
    let description = rtflow_testkit::description("events", &[Codec::H264]);
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(interleaved_ok(request)),
        Method::Play => ServerReply::Response(reply(request, 200, "OK")),
        Method::GetParameter => ServerReply::Response(reply(request, 500, "Server Error")),
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut options = interleaved_options(collector.clone());
    options.keep_alive_interval = Some(Duration::from_millis(30));
    let mut session = RtspClient::connect(server.url("/stream"), options)
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), session.next())
        .await
        .unwrap()
        .unwrap();
    assert!(frame.is_err());
    let failed = collector
        .client_events()
        .iter()
        .find_map(|event| match event {
            ClientEvent::KeepAliveFailed { consecutive, .. } => Some(*consecutive),
            _ => None,
        });
    assert!(
        failed.is_some_and(|consecutive| consecutive >= 1),
        "expected a KeepAliveFailed event"
    );
}

#[tokio::test]
async fn reconnect_lifecycle_emits_backoff_failed_closed_and_feeds_metrics() {
    use rtflow::metrics::{MetricsEventSink, MetricsRegistry};

    let registry = Arc::new(MetricsRegistry::new());
    let sink: Arc<dyn EventSink> = Arc::new(MetricsEventSink::new(registry.clone()));
    let requests = Arc::new(AtomicU32::new(0));
    let counter = requests.clone();
    let description = rtflow_testkit::description("events", &[Codec::H264]);
    let server = RawRtspServer::start(Box::new(move |request| {
        let request_no = counter.fetch_add(1, Ordering::SeqCst) + 1;
        match request.method {
            Method::Options if request_no == 1 => ServerReply::Response(reply(request, 200, "OK")),
            Method::Describe => ServerReply::Response(describe_reply(request, &description)),
            Method::Setup => ServerReply::Response(interleaved_ok(request)),
            Method::Play => ServerReply::ResponseThenClose(reply(request, 200, "OK")),
            _ => ServerReply::Response(reply(request, 500, "Server Error")),
        }
    }))
    .await;
    let policy = ClientReconnectPolicy::builder()
        .initial_delay(Duration::from_millis(1))
        .max_delay(Duration::from_millis(2))
        .max_attempts(2)
        .build()
        .unwrap();
    let mut session =
        RtspClient::connect_reconnecting(server.url("/stream"), interleaved_options(sink), policy)
            .await
            .expect("initial connection should succeed");
    let outcome = tokio::time::timeout(Duration::from_secs(5), session.next())
        .await
        .unwrap()
        .unwrap();
    assert!(outcome.is_err());
    assert!(
        tokio::time::timeout(Duration::from_secs(5), session.next())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        registry
            .client_sessions_total
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "initial connection must register one client session"
    );
    assert!(
        registry
            .client_reconnect_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1,
        "failed reconnects must count as backoff attempts"
    );
    assert!(
        registry.errors_by_code[8].load(std::sync::atomic::Ordering::Relaxed) >= 1,
        "terminal 500 must land in errors_by_code (UpstreamUnavailable)"
    );
}
