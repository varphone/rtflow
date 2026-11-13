//! RTSP 1.0 method coverage for the client: OPTIONS capabilities,
//! PLAY Range/Scale/Speed, PAUSE Range, SET/GET_PARAMETER and Digest retry
//! on the interleaved control channel.
use rtflow::client::{ClientOptions, PlayRequest, RtspClient};
use rtflow::rtsp::range::NptRange;
use rtflow::transport::UpstreamTransport;
use rtflow::{Codec, Error};
use rtflow_testkit::{RawRtspServer, ServerReply, describe_reply, reply, with_header};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;

fn single_h264() -> rtflow::MediaDescription {
    rtflow_testkit::description("methods", &[Codec::H264])
}

fn interleaved(request: &rtflow::rtsp::Request) -> rtflow::rtsp::Response {
    with_header(
        with_header(reply(request, 200, "OK"), "session", "sess-1"),
        "transport",
        "RTP/AVP/TCP;unicast;interleaved=0-1",
    )
}

fn tcp_options() -> ClientOptions {
    ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .keep_alive_interval(None)
        .build()
        .unwrap()
}

#[tokio::test]
async fn capabilities_play_range_scale_speed_and_parameters() {
    let description = single_h264();
    let captured = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let sink = captured.clone();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        rtflow::rtsp::Method::Options => ServerReply::Response(with_header(
            reply(request, 200, "OK"),
            "public",
            "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER",
        )),
        rtflow::rtsp::Method::Describe => {
            ServerReply::Response(describe_reply(request, &description))
        }
        rtflow::rtsp::Method::Setup => ServerReply::Response(interleaved(request)),
        rtflow::rtsp::Method::Play => {
            let mut headers = sink.lock().unwrap();
            headers.push((
                "PLAY".into(),
                request.headers.get("range").cloned().unwrap_or_default(),
            ));
            headers.push((
                "SCALE".into(),
                request.headers.get("scale").cloned().unwrap_or_default(),
            ));
            headers.push((
                "SPEED".into(),
                request.headers.get("speed").cloned().unwrap_or_default(),
            ));
            drop(headers);
            ServerReply::Response(reply(request, 200, "OK"))
        }
        rtflow::rtsp::Method::Pause => {
            sink.lock().unwrap().push((
                "PAUSE".into(),
                request.headers.get("range").cloned().unwrap_or_default(),
            ));
            ServerReply::Response(reply(request, 200, "OK"))
        }
        rtflow::rtsp::Method::SetParameter => {
            sink.lock().unwrap().push((
                "SET".into(),
                String::from_utf8_lossy(&request.body).into_owned(),
            ));
            ServerReply::Response(reply(request, 200, "OK"))
        }
        rtflow::rtsp::Method::GetParameter => {
            let mut response = reply(request, 200, "OK");
            response
                .headers
                .insert("content-type".into(), "text/parameters".into());
            response.body = b"video_speed=2\r\n".to_vec();
            ServerReply::Response(response)
        }
        rtflow::rtsp::Method::Teardown => ServerReply::Response(reply(request, 200, "OK")),
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut session = RtspClient::connect(server.url("/stream"), tcp_options())
        .await
        .unwrap();
    assert!(session.capabilities().supports("PLAY"));
    assert!(session.capabilities().supports("set_parameter"));
    assert!(!session.capabilities().supports("ANNOUNCE"));

    let range = NptRange::parse("npt=1-2").unwrap();
    session
        .play_with(PlayRequest {
            range: Some(range),
            scale: Some(2.0),
            speed: Some(1.5),
        })
        .await
        .unwrap();
    session
        .pause_with(Some(NptRange::parse("npt=3-").unwrap()))
        .await
        .unwrap();
    session.set_parameter("video_speed=2\r\n").await.unwrap();
    let value = session.get_parameter("video_speed").await.unwrap();
    assert_eq!(value, "video_speed=2\r\n");

    let captured = captured.lock().unwrap().clone();
    assert!(
        captured
            .iter()
            .any(|(name, value)| name == "PLAY" && value.starts_with("npt=1"))
    );
    assert!(
        captured
            .iter()
            .any(|(name, value)| name == "SCALE" && value == "2")
    );
    assert!(
        captured
            .iter()
            .any(|(name, value)| name == "SPEED" && value == "1.5")
    );
    assert!(
        captured
            .iter()
            .any(|(name, value)| name == "PAUSE" && value.starts_with("npt=3"))
    );
    assert!(
        captured
            .iter()
            .any(|(name, value)| name == "SET" && value == "video_speed=2\r\n")
    );
    session.teardown().await.unwrap();
}

#[tokio::test]
async fn invalid_play_scale_is_rejected_before_wire() {
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        rtflow::rtsp::Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        rtflow::rtsp::Method::Describe => {
            ServerReply::Response(describe_reply(request, &description))
        }
        rtflow::rtsp::Method::Setup => ServerReply::Response(interleaved(request)),
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut session = RtspClient::connect(server.url("/stream"), tcp_options())
        .await
        .unwrap();
    let result = session
        .play_with(PlayRequest {
            scale: Some(f64::NAN),
            ..PlayRequest::default()
        })
        .await;
    assert!(matches!(result, Err(Error::Limit("PLAY Scale"))));
    session.teardown().await.unwrap();
}

#[tokio::test]
async fn interleaved_control_retries_play_with_digest() {
    let description = single_h264();
    let plays = Arc::new(AtomicU32::new(0));
    let attempts = plays.clone();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        rtflow::rtsp::Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        rtflow::rtsp::Method::Describe => {
            ServerReply::Response(describe_reply(request, &description))
        }
        rtflow::rtsp::Method::Setup => ServerReply::Response(interleaved(request)),
        rtflow::rtsp::Method::Pause => {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                ServerReply::Response(with_header(
                    reply(request, 401, "Unauthorized"),
                    "www-authenticate",
                    "Digest realm=\"methods\", nonce=\"0123456789abcdef\"",
                ))
            } else {
                assert!(
                    request
                        .headers
                        .get("authorization")
                        .is_some_and(|value| value.contains("Digest")),
                    "second PAUSE must carry Digest authorization"
                );
                ServerReply::Response(reply(request, 200, "OK"))
            }
        }
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut options = tcp_options();
    options.credentials = Some(rtflow::Credentials {
        username: "user".into(),
        password: "pass".into(),
    });
    let mut session = RtspClient::connect(server.url("/stream"), options)
        .await
        .unwrap();
    session.pause().await.unwrap();
    assert_eq!(plays.load(Ordering::SeqCst), 2);
    session.teardown().await.unwrap();
}

#[tokio::test]
async fn options_reports_public_methods() {
    let server = RawRtspServer::start(Box::new(|request| {
        if request.method == rtflow::rtsp::Method::Options {
            ServerReply::Response(with_header(
                reply(request, 200, "OK"),
                "public",
                "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN",
            ))
        } else {
            ServerReply::Response(reply(request, 200, "OK"))
        }
    }))
    .await;
    let capabilities = tokio::time::timeout(
        Duration::from_secs(2),
        RtspClient::options_str(server.url("/stream").as_str(), tcp_options()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(capabilities.supports("OPTIONS"));
    assert!(capabilities.supports("PLAY"));
    assert!(!capabilities.supports("RECORD"));
}

#[tokio::test]
async fn require_tokens_are_sent_and_551_surfaces() {
    let seen = Arc::new(Mutex::new(None::<String>));
    let sink = seen.clone();
    let server = RawRtspServer::start(Box::new(move |request| {
        if request.method == rtflow::rtsp::Method::Options {
            *sink.lock().unwrap() = request.headers.get("require").cloned();
            ServerReply::Response(with_header(
                reply(request, 551, "Option Not Supported"),
                "unsupported",
                "com.example.feature",
            ))
        } else {
            ServerReply::Response(reply(request, 200, "OK"))
        }
    }))
    .await;
    let options = ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .require(["com.example.feature"])
        .build()
        .unwrap();
    let error = RtspClient::options(server.url("/stream"), options)
        .await
        .expect_err("551 must surface");
    match error {
        Error::RtspUnsupported {
            ref method,
            ref unsupported,
        } => {
            assert_eq!(method, "OPTIONS");
            assert_eq!(unsupported, &["com.example.feature"]);
        }
        other => panic!("expected RtspUnsupported, got {other:?}"),
    }
    assert_eq!(
        seen.lock().unwrap().as_deref(),
        Some("com.example.feature"),
        "Require header missing from OPTIONS"
    );
}

#[test]
fn require_tokens_reject_header_injection() {
    let error = match ClientOptions::builder()
        .require(["bad\r\nX-Injected: 1"])
        .build()
    {
        Ok(_) => panic!("CRLF in Require token must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::InvalidConfig(_)));
}

#[tokio::test]
async fn server_redirect_request_surfaces_as_error() {
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        rtflow::rtsp::Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        rtflow::rtsp::Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        rtflow::rtsp::Method::Setup => ServerReply::Response(interleaved(request)),
        rtflow::rtsp::Method::Play => {
            let cseq = request.headers.get("cseq").cloned().unwrap_or_default();
            ServerReply::Raw(
                format!(
                    "REDIRECT rtsp://127.0.0.1:554/moved RTSP/1.0\r\nCSeq: {cseq}\r\nLocation: rtsp://127.0.0.1:554/moved\r\n\r\n"
                )
                .into_bytes(),
            )
        }
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let error = match RtspClient::connect(server.url("/stream"), tcp_options()).await {
        Ok(_) => panic!("REDIRECT request must fail the session"),
        Err(error) => error,
    };
    assert!(matches!(error, Error::Redirect(ref url) if url.ends_with("/moved")));
}

#[tokio::test]
async fn interleaved_redirect_during_control_surfaces_on_media() {
    let description = single_h264();
    let server = RawRtspServer::start(Box::new(move |request| match request.method {
        rtflow::rtsp::Method::Options => ServerReply::Response(reply(request, 200, "OK")),
        rtflow::rtsp::Method::Describe => ServerReply::Response(describe_reply(request, &description)),
        rtflow::rtsp::Method::Setup => ServerReply::Response(interleaved(request)),
        rtflow::rtsp::Method::Play => ServerReply::Response(reply(request, 200, "OK")),
        rtflow::rtsp::Method::Pause => {
            let cseq = request.headers.get("cseq").cloned().unwrap_or_default();
            ServerReply::Raw(
                format!(
                    "REDIRECT rtsp://127.0.0.1:554/moved RTSP/1.0\r\nCSeq: {cseq}\r\nLocation: rtsp://127.0.0.1:554/moved\r\n\r\n"
                )
                .into_bytes(),
            )
        }
        _ => ServerReply::Response(reply(request, 200, "OK")),
    }))
    .await;
    let mut session = RtspClient::connect(server.url("/stream"), tcp_options())
        .await
        .unwrap();
    let _ = session.pause().await;
    let error = tokio::time::timeout(Duration::from_secs(2), session.next())
        .await
        .expect("redirect should surface")
        .expect("media channel closed")
        .expect_err("media must carry a redirect error");
    assert!(matches!(error, Error::Redirect(ref url) if url.ends_with("/moved")));
}

#[tokio::test]
async fn connect_follows_server_redirect() {
    let description = single_h264();
    let redirected = Arc::new(Mutex::new(false));
    let state = redirected.clone();
    let server = RawRtspServer::start(Box::new(move |request| {
        let new_path = request.uri.contains("/new");
        match request.method {
            rtflow::rtsp::Method::Options => ServerReply::Response(reply(request, 200, "OK")),
            rtflow::rtsp::Method::Describe => {
                ServerReply::Response(describe_reply(request, &description))
            }
            rtflow::rtsp::Method::Setup => ServerReply::Response(interleaved(request)),
            rtflow::rtsp::Method::Play if !new_path => {
                *state.lock().unwrap() = true;
                let cseq = request.headers.get("cseq").cloned().unwrap_or_default();
                ServerReply::Raw(
                    format!(
                        "REDIRECT rtsp://{}/new RTSP/1.0\r\nCSeq: {cseq}\r\nLocation: rtsp://{}/new\r\n\r\n",
                        request.uri.split('/').nth(2).unwrap_or("127.0.0.1"),
                        request.uri.split('/').nth(2).unwrap_or("127.0.0.1"),
                    )
                    .into_bytes(),
                )
            }
            _ => ServerReply::Response(reply(request, 200, "OK")),
        }
    }))
    .await;
    let session = RtspClient::connect(
        server.url("/old"),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .keep_alive_interval(None)
            .build()
            .unwrap(),
    )
    .await
    .expect("client should follow REDIRECT");
    assert_eq!(session.description.tracks.len(), 1);
    assert!(*redirected.lock().unwrap());
    session.teardown().await.unwrap();
}

#[tokio::test]
async fn reconnecting_session_follows_redirect_on_keepalive() {
    let description = single_h264();
    let new_seen = Arc::new(AtomicU32::new(0));
    let state = new_seen.clone();
    let server = RawRtspServer::start(Box::new(move |request| {
        let is_new = request.uri.contains("/new");
        if is_new {
            state.fetch_add(1, Ordering::SeqCst);
        }
        match request.method {
            rtflow::rtsp::Method::Options => ServerReply::Response(reply(request, 200, "OK")),
            rtflow::rtsp::Method::Describe => {
                ServerReply::Response(describe_reply(request, &description))
            }
            rtflow::rtsp::Method::Setup => ServerReply::Response(interleaved(request)),
            rtflow::rtsp::Method::Play => ServerReply::Response(reply(request, 200, "OK")),
            rtflow::rtsp::Method::GetParameter if !is_new => {
                let cseq = request.headers.get("cseq").cloned().unwrap_or_default();
                ServerReply::Raw(
                    format!(
                        "REDIRECT rtsp://127.0.0.1:554/new RTSP/1.0\r\nCSeq: {cseq}\r\nLocation: rtsp://{}/new\r\n\r\n",
                        request.uri.split('/').nth(2).unwrap_or("127.0.0.1")
                    )
                    .into_bytes(),
                )
            }
            _ => ServerReply::Response(reply(request, 200, "OK")),
        }
    }))
    .await;
    let options = ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .keep_alive_interval(Some(Duration::from_millis(100)))
        .build()
        .unwrap();
    let mut session = RtspClient::connect_reconnecting(
        server.url("/old"),
        options,
        rtflow::client::ClientReconnectPolicy::default(),
    )
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while new_seen.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "redirect reconnect timeout"
        );
        tokio::select! {
            _ = session.next() => {}
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
    assert!(new_seen.load(Ordering::SeqCst) > 0);
}
