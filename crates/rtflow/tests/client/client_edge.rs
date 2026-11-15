//! Client edge-case tests against scripted (misbehaving) RTSP servers.
//!
//! These pin down the client's robustness behavior: transport fallback,
//! stale-response pairing, auth retries, non-2xx control statuses, and the
//! reconnecting session's termination contract.
use rtflow::client::{ClientOptions, ClientReconnectPolicy, RtspClient};
use rtflow::transport::UpstreamTransport;
use rtflow::{Codec, Error, rtsp::Method};
use rtflow_testkit::{RawRtspServer, ServerReply, describe_reply, reply, with_header};
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;

fn single_h264() -> rtflow::MediaDescription {
    rtflow_testkit::description("edge", &[Codec::H264])
}

fn interleaved_ok(request: &rtflow::rtsp::Request) -> rtflow::rtsp::Response {
    with_header(
        with_header(reply(request, 200, "OK"), "session", "sess-1"),
        "transport",
        "RTP/AVP/TCP;unicast;interleaved=0-1",
    )
}

fn interleaved_options(mut options: ClientOptions) -> ClientOptions {
    options.transport = UpstreamTransport::TcpInterleaved;
    options
}

#[tokio::test]
async fn udp_setup_461_falls_back_to_tcp_interleaved() {
    let setup_attempts = Arc::new(AtomicU32::new(0));
    let attempts = setup_attempts.clone();
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                // First SETUP advertises UDP client ports: refuse it so the
                // client must fall back to TCP interleaving.
                assert!(
                    request
                        .headers
                        .get("transport")
                        .unwrap()
                        .contains("client_port")
                );
                ServerReply::Response(reply(request, 461, "Unsupported transport"))
            } else {
                assert!(
                    request
                        .headers
                        .get("transport")
                        .unwrap()
                        .contains("interleaved")
                );
                ServerReply::Response(interleaved_ok(request))
            }
        }
        Method::Play => ServerReply::Response(reply(request, 200, "OK")),
        Method::Teardown => ServerReply::Response(reply(request, 200, "OK")),
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let session = RtspClient::connect(server.url("/stream"), ClientOptions::default())
        .await
        .expect("auto transport should fall back to TCP interleaving");
    assert_eq!(session.description.tracks.len(), 1);
    assert_eq!(setup_attempts.load(Ordering::SeqCst), 2);
    // RT-9: the fallback reuses the control connection and the parsed SDP,
    // so OPTIONS/DESCRIBE must run exactly once across both transports.
    let counts = server.method_counts().await;
    assert_eq!(counts.get("OPTIONS"), Some(&1));
    assert_eq!(counts.get("DESCRIBE"), Some(&1));
    session.teardown().await.unwrap();
}

#[tokio::test]
async fn stale_cseq_responses_are_skipped_during_pairing() {
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(interleaved_ok(request)),
        Method::Play => ServerReply::Response(reply(request, 200, "OK")),
        Method::Pause => {
            // A leftover response from an earlier exchange must be skipped;
            // only the one matching this request's CSeq counts.
            let cseq = request.headers.get("cseq").unwrap().clone();
            let wire = format!(
                "RTSP/1.0 200 OK\r\ncseq: 1\r\n\r\nRTSP/1.0 200 OK\r\ncseq: {cseq}\r\n\r\n"
            );
            ServerReply::Raw(wire.into_bytes())
        }
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut options = interleaved_options(ClientOptions::default());
    options.keep_alive_interval = None;
    let mut session = RtspClient::connect(server.url("/stream"), options)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.pause())
        .await
        .expect("pause should complete despite a stale response")
        .unwrap();
    assert!(!session.is_playing());
}

#[tokio::test]
async fn unauthorized_options_surfaces_without_credentials() {
    let server = RawRtspServer::start(Box::new(|request| {
        ServerReply::Response(with_header(
            reply(request, 401, "Unauthorized"),
            "www-authenticate",
            "Basic realm=\"edge\"",
        ))
    }))
    .await;
    let result = RtspClient::connect(server.url("/stream"), ClientOptions::default()).await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("401 without credentials must fail"),
    };
    assert!(matches!(error, Error::Unauthorized), "got {error:?}");
}

#[tokio::test]
async fn digest_challenge_is_retried_with_credentials() {
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => {
            if request
                .headers
                .get("authorization")
                .is_some_and(|value| value.contains("Digest"))
            {
                ServerReply::Response(reply(request, 200, "OK"))
            } else {
                ServerReply::Response(with_header(
                    reply(request, 401, "Unauthorized"),
                    "www-authenticate",
                    "Digest realm=\"edge\", nonce=\"0123456789abcdef\"",
                ))
            }
        }
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(interleaved_ok(request)),
        Method::Play => ServerReply::Response(reply(request, 200, "OK")),
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut options = interleaved_options(ClientOptions::default());
    options.credentials = Some(rtflow::Credentials {
        username: "user".into(),
        password: "pass".into(),
    });
    let session = RtspClient::connect(server.url("/stream"), options)
        .await
        .expect("client should retry with digest authorization");
    assert_eq!(session.description.tracks.len(), 1);
    let counts = server.method_counts().await;
    assert_eq!(
        counts.get("OPTIONS"),
        Some(&2),
        "401 challenge must be retried"
    );
}

#[tokio::test]
async fn setup_error_status_surfaces() {
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(reply(request, 500, "Internal Server Error")),
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let result = RtspClient::connect(
        server.url("/stream"),
        interleaved_options(ClientOptions::default()),
    )
    .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("SETUP 500 must fail the connection"),
    };
    assert!(
        matches!(
            error,
            Error::RtspStatus {
                method: "SETUP",
                status: 500
            }
        ),
        "got {error:?}"
    );
}

#[tokio::test]
async fn play_error_status_surfaces() {
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(interleaved_ok(request)),
        Method::Play => ServerReply::Response(reply(request, 453, "Not Enough Bandwidth")),
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let result = RtspClient::connect(
        server.url("/stream"),
        interleaved_options(ClientOptions::default()),
    )
    .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("PLAY 453 must fail the connection"),
    };
    assert!(
        matches!(
            error,
            Error::RtspStatus {
                method: "PLAY",
                status: 453
            }
        ),
        "got {error:?}"
    );
}

#[cfg(feature = "tls")]
#[tokio::test]
async fn rtsps_without_tls_configuration_errors() {
    // The client must reject the scheme before attempting a TLS handshake.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut [0u8; 1]).await;
            });
        }
    });
    let url = url::Url::parse(&format!("rtsps://{address}/stream")).unwrap();
    let result = RtspClient::connect(url, ClientOptions::default()).await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("rtsps without a TLS configuration must fail"),
    };
    assert!(matches!(error, Error::Tls(_)), "got {error:?}");
}

#[tokio::test]
async fn describe_returns_parsed_media_description() {
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let received = RtspClient::describe(server.url("/stream"), ClientOptions::default())
        .await
        .expect("describe should succeed against a compliant server");
    assert_eq!(received.session_name.as_ref(), "edge");
    assert_eq!(received.tracks.len(), 1);
    assert_eq!(received.tracks[0].codec, Codec::H264);
}

#[tokio::test]
async fn reconnecting_session_reports_error_after_max_attempts() {
    let connections = Arc::new(AtomicU32::new(0));
    let counter = connections.clone();
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| {
        let connection = counter.fetch_add(1, Ordering::SeqCst) + 1;
        match request.method {
            Method::Options if connection == 1 => ServerReply::Response(reply(request, 200, "OK")),
            Method::Describe => ServerReply::Response(describe_reply(request, &description)),
            Method::Setup => ServerReply::Response(interleaved_ok(request)),
            // Close right after PLAY so the media session ends and the
            // client starts reconnecting; every later OPTIONS is refused.
            Method::Play => ServerReply::ResponseThenClose(reply(request, 200, "OK")),
            _ => ServerReply::Response(reply(request, 500, "Internal Server Error")),
        }
    }))
    .await;
    // Millisecond backoffs keep the test fast in real time while still
    // exercising the exponential-delay and exhaustion paths.
    let policy = ClientReconnectPolicy::builder()
        .initial_delay(Duration::from_millis(1))
        .max_delay(Duration::from_millis(2))
        .max_attempts(2)
        .build()
        .unwrap();
    let mut session = RtspClient::connect_reconnecting(
        server.url("/stream"),
        interleaved_options(ClientOptions::default()),
        policy,
    )
    .await
    .expect("initial connection should succeed");
    // Keep-alive error frames surface first (the control task notices the
    // dead connection); drain them until the terminal reconnect-exhaustion
    // error arrives.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let terminal = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(remaining, session.next())
            .await
            .expect("reconnect exhaustion should surface within the timeout")
            .expect("channel should deliver a terminal error frame");
        match frame {
            Err(Error::RtspStatus {
                method: "GET_PARAMETER",
                ..
            }) => continue,
            Err(other) => break other,
            Ok(_) => panic!("unexpected media frame while reconnecting"),
        }
    };
    assert!(
        matches!(
            terminal,
            Error::RtspStatus {
                method: "OPTIONS",
                status: 500
            }
        ),
        "expected the exhausted reconnect attempt's error, got {terminal:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), session.next())
            .await
            .unwrap()
            .is_none(),
        "channel must close after the terminal error"
    );
}

#[tokio::test]
async fn reconnecting_session_stops_on_permanent_error() {
    // Regression: with max_attempts=0 (retry forever) a permanent failure
    // such as 401 Unauthorized must still terminate instead of looping.
    let requests = Arc::new(AtomicU32::new(0));
    let counter = requests.clone();
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| {
        let request_no = counter.fetch_add(1, Ordering::SeqCst) + 1;
        match request.method {
            Method::Options if request_no == 1 => ServerReply::Response(reply(request, 200, "OK")),
            Method::Describe => ServerReply::Response(describe_reply(request, &description)),
            Method::Setup => ServerReply::Response(interleaved_ok(request)),
            Method::Play => ServerReply::ResponseThenClose(reply(request, 200, "OK")),
            // Every reconnect attempt is rejected with 401.
            _ => ServerReply::Response(with_header(
                reply(request, 401, "Unauthorized"),
                "www-authenticate",
                "Basic realm=\"edge\"",
            )),
        }
    }))
    .await;
    let policy = ClientReconnectPolicy::builder()
        .initial_delay(Duration::from_millis(1))
        .max_delay(Duration::from_millis(2))
        .max_attempts(0)
        .build()
        .unwrap();
    let mut session = RtspClient::connect_reconnecting(
        server.url("/stream"),
        interleaved_options(ClientOptions::default()),
        policy,
    )
    .await
    .expect("initial connection should succeed");
    // Drain any transient keep-alive errors until the terminal permanent
    // error arrives.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let terminal = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(remaining, session.next())
            .await
            .expect("permanent error must terminate the retry-forever loop")
            .expect("channel should deliver a terminal error frame");
        match frame {
            Err(Error::RtspStatus {
                method: "GET_PARAMETER",
                ..
            }) => continue,
            Err(other) => break other,
            Ok(_) => panic!("unexpected media frame while reconnecting"),
        }
    };
    assert!(matches!(terminal, Error::Unauthorized), "got {terminal:?}");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), session.next())
            .await
            .unwrap()
            .is_none(),
        "channel must close after the terminal error"
    );
}

#[tokio::test]
async fn keep_alive_failure_surfaces_from_next() {
    // Baseline for audit item R23: today a failed keep-alive surfaces as an
    // error frame immediately and without backoff. M2 changes this behavior;
    // update the assertion together with the fix.
    let keep_alives = Arc::new(AtomicU32::new(0));
    let counter = keep_alives.clone();
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(interleaved_ok(request)),
        Method::Play => ServerReply::Response(reply(request, 200, "OK")),
        Method::GetParameter => {
            let attempt = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                ServerReply::Response(reply(request, 200, "OK"))
            } else {
                ServerReply::Response(reply(request, 500, "Internal Server Error"))
            }
        }
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut options = interleaved_options(ClientOptions::default());
    options.keep_alive_interval = Some(Duration::from_millis(50));
    let mut session = RtspClient::connect(server.url("/stream"), options)
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), session.next())
        .await
        .expect("keep-alive failure should surface quickly")
        .expect("session channel should still be open");
    let error = frame.expect_err("second keep-alive fails");
    assert!(
        matches!(
            error,
            Error::RtspStatus {
                method: "GET_PARAMETER",
                status: 500
            }
        ),
        "got {error:?}"
    );
    assert!(keep_alives.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn keep_alive_downgrades_to_options_when_get_parameter_unsupported() {
    let get_parameters = Arc::new(AtomicU32::new(0));
    let options_keep_alives = Arc::new(AtomicU32::new(0));
    let gp = get_parameters.clone();
    let ok = options_keep_alives.clone();
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => {
            // The handshake OPTIONS happens first; later ones are keep-alives.
            if request.headers.contains_key("session") {
                ok.fetch_add(1, Ordering::SeqCst);
            }
            ServerReply::Response(reply(request, 200, "OK"))
        }
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(interleaved_ok(request)),
        Method::Play => ServerReply::Response(reply(request, 200, "OK")),
        Method::GetParameter => {
            gp.fetch_add(1, Ordering::SeqCst);
            ServerReply::Response(reply(request, 501, "Option not supported"))
        }
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut options = interleaved_options(ClientOptions::default());
    options.keep_alive_interval = Some(Duration::from_millis(30));
    let mut session = RtspClient::connect(server.url("/stream"), options)
        .await
        .unwrap();
    // Successful (downgraded) keep-alives do not surface frames; the session
    // should simply stay healthy for several intervals.
    let frame = tokio::time::timeout(Duration::from_millis(300), session.next()).await;
    assert!(
        frame.is_err(),
        "no error frame should surface, got {frame:?}"
    );
    assert_eq!(
        get_parameters.load(Ordering::SeqCst),
        1,
        "GET_PARAMETER must not be retried after 501"
    );
    assert!(
        options_keep_alives.load(Ordering::SeqCst) >= 2,
        "keep-alive must continue via OPTIONS"
    );
}

#[tokio::test]
async fn keep_alive_failures_back_off() {
    let attempts = Arc::new(AtomicU32::new(0));
    let counter = attempts.clone();
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(interleaved_ok(request)),
        Method::Play => ServerReply::Response(reply(request, 200, "OK")),
        Method::GetParameter => {
            counter.fetch_add(1, Ordering::SeqCst);
            ServerReply::Response(reply(request, 500, "Internal Server Error"))
        }
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut options = interleaved_options(ClientOptions::default());
    options.keep_alive_interval = Some(Duration::from_millis(20));
    let mut session = RtspClient::connect(server.url("/stream"), options)
        .await
        .unwrap();
    // Drain error frames for a fixed window. Without backoff the 20ms
    // interval would produce ~25 attempts in 500ms; exponential backoff
    // (20, 40, 80, 160, 320ms...) keeps it to a handful.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, session.next()).await {
            Ok(Some(Err(_))) => continue,
            Ok(Some(Ok(_))) | Ok(None) => panic!("unexpected frame while keep-alive fails"),
            Err(_) => break,
        }
    }
    let count = attempts.load(Ordering::SeqCst);
    assert!(count >= 2, "failures must still surface, saw {count}");
    assert!(
        count <= 8,
        "expected backoff, saw {count} attempts in 500ms"
    );
}

#[tokio::test]
async fn session_timeout_parameter_drives_keep_alive_interval() {
    let keep_alives = Arc::new(AtomicU32::new(0));
    let counter = keep_alives.clone();
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        Method::Setup => ServerReply::Response(with_header(
            interleaved_ok(request),
            "session",
            "sess-1;timeout=1",
        )),
        Method::Play => ServerReply::Response(reply(request, 200, "OK")),
        Method::GetParameter => {
            counter.fetch_add(1, Ordering::SeqCst);
            ServerReply::Response(reply(request, 200, "OK"))
        }
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    // No keep-alive interval configured: the client must derive one from
    // timeout=1 (half a second) instead of disabling keep-alives.
    let mut session = RtspClient::connect(
        server.url("/stream"),
        interleaved_options(
            ClientOptions::builder()
                .keep_alive_interval(None)
                .build()
                .unwrap(),
        ),
    )
    .await
    .unwrap();
    let frame = tokio::time::timeout(Duration::from_millis(1_500), session.next()).await;
    assert!(frame.is_err(), "session should stay healthy, got {frame:?}");
    assert!(
        keep_alives.load(Ordering::SeqCst) >= 1,
        "timeout=1 must trigger keep-alives well before the 20s default"
    );
}

#[tokio::test]
async fn describe_succeeds_over_ipv6_loopback() {
    if tokio::net::TcpListener::bind("[::1]:0").await.is_err() {
        eprintln!("skipping: IPv6 loopback unavailable on this host");
        return;
    }
    let description = single_h264();
    let server = RawRtspServer::start_on(
        "[::1]:0",
        Box::new(move |request| match request.method {
            Method::Options => ServerReply::Response(reply(request, 200, "OK")),
            Method::Describe => ServerReply::Response(describe_reply(request, &description)),
            _ => ServerReply::Response(reply(request, 200, "OK")),
        }),
    )
    .await;
    // SocketAddr renders IPv6 with brackets, so this is a valid rtsp:// URL.
    let url = url::Url::parse(&format!("rtsp://{}/stream", server.address())).unwrap();
    let received = RtspClient::describe(url, ClientOptions::default())
        .await
        .expect("describe should work over IPv6 loopback");
    assert_eq!(received.tracks.len(), 1);
}
