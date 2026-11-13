use super::control::{effective_keep_alive, keep_alive_backoff};
use super::options::validate_options;
use super::reconnect::jittered_backoff;
use super::wire::{
    candidate_headers_cseq, encode_request, response_session_id, response_session_timeout,
    should_fallback_to_tcp, status_error, validate_response_cseq,
};
use super::*;

fn response_with_headers(status: u16, headers: Vec<(&str, &str)>) -> Response {
    Response {
        status,
        reason: "OK".into(),
        headers: headers
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        body: Vec::new(),
    }
}

#[test]
fn jittered_backoff_stays_within_25_percent_bounds() {
    let delay = Duration::from_secs(1);
    for attempt in 0..256u32 {
        let jittered = jittered_backoff(delay, attempt);
        assert!(
            jittered >= Duration::from_millis(750) && jittered <= Duration::from_millis(1250),
            "attempt {attempt} produced {jittered:?}"
        );
    }
}

#[test]
fn jittered_backoff_uses_independent_samples() {
    let delay = Duration::from_millis(800);
    let samples = (0..32)
        .map(|attempt| jittered_backoff(delay, attempt))
        .collect::<std::collections::HashSet<_>>();
    assert!(samples.len() > 1, "jitter must not be deterministic");
}

#[test]
fn jittered_backoff_does_not_panic_on_large_attempts() {
    let _ = jittered_backoff(Duration::from_secs(10), u32::MAX);
}

#[test]
fn fallback_classification_covers_transport_failures() {
    assert!(should_fallback_to_tcp(&Error::Closed));
    for kind in [
        std::io::ErrorKind::TimedOut,
        std::io::ErrorKind::ConnectionRefused,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::ConnectionAborted,
    ] {
        assert!(
            should_fallback_to_tcp(&Error::Io(std::io::Error::new(kind, "x"))),
            "{kind:?} should fall back"
        );
    }
    assert!(!should_fallback_to_tcp(&Error::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "x"
    ))));
    assert!(!should_fallback_to_tcp(&Error::Unauthorized));
}

// M1 switched fallback classification from error-text sniffing to the
// typed RtspStatus variant; free-text Rtsp errors no longer trigger it.
#[test]
fn fallback_classification_uses_typed_status() {
    assert!(should_fallback_to_tcp(&Error::RtspStatus {
        method: "SETUP",
        status: 461
    }));
    assert!(!should_fallback_to_tcp(&Error::RtspStatus {
        method: "PLAY",
        status: 453
    }));
    assert!(!should_fallback_to_tcp(&Error::Rtsp(
        "SETUP returned 461".into()
    )));
}

#[test]
fn encode_request_has_expected_wire_shape() {
    let wire = encode_request(
        "OPTIONS",
        "rtsp://host/stream",
        5,
        Some("Session: abc\r\n"),
        Some("Basic dXNlcg=="),
    );
    assert_eq!(
        wire,
        format!(
            concat!(
                "OPTIONS rtsp://host/stream RTSP/1.0\r\n",
                "CSeq: 5\r\n",
                "User-Agent: rtflow/{}\r\n",
                "Authorization: Basic dXNlcg==\r\n",
                "Session: abc\r\n",
                "\r\n",
            ),
            env!("CARGO_PKG_VERSION")
        )
    );
    let bare = encode_request("TEARDOWN", "rtsp://host/stream", 9, None, None);
    assert_eq!(
        bare,
        format!(
            concat!(
                "TEARDOWN rtsp://host/stream RTSP/1.0\r\n",
                "CSeq: 9\r\n",
                "User-Agent: rtflow/{}\r\n",
                "\r\n",
            ),
            env!("CARGO_PKG_VERSION")
        )
    );
}

#[test]
fn session_id_is_extracted_and_timeout_suffix_stripped() {
    let with_timeout = response_with_headers(200, vec![("session", "abc123;timeout=60")]);
    assert_eq!(response_session_id(&with_timeout).unwrap(), "abc123");
    let padded = response_with_headers(200, vec![("session", "  xyz  ")]);
    assert_eq!(response_session_id(&padded).unwrap(), "xyz");
    let empty = response_with_headers(200, vec![("session", ";timeout=60")]);
    assert!(response_session_id(&empty).is_err());
    let missing = response_with_headers(200, vec![]);
    assert!(response_session_id(&missing).is_err());
}

#[test]
fn session_timeout_parameter_is_parsed() {
    let with_timeout = response_with_headers(200, vec![("session", "abc;timeout=60")]);
    assert_eq!(
        response_session_timeout(&with_timeout),
        Some(Duration::from_secs(60))
    );
    let spaced = response_with_headers(200, vec![("session", "abc ; Timeout = 30")]);
    assert_eq!(
        response_session_timeout(&spaced),
        Some(Duration::from_secs(30))
    );
    let no_param = response_with_headers(200, vec![("session", "abc")]);
    assert_eq!(response_session_timeout(&no_param), None);
    let zero = response_with_headers(200, vec![("session", "abc;timeout=0")]);
    assert_eq!(response_session_timeout(&zero), None);
    let invalid = response_with_headers(200, vec![("session", "abc;timeout=lots")]);
    assert_eq!(response_session_timeout(&invalid), None);
    let missing = response_with_headers(200, vec![]);
    assert_eq!(response_session_timeout(&missing), None);
}

#[test]
fn effective_keep_alive_adapts_to_server_timeout() {
    // Configured interval wins when it is already below half the timeout.
    assert_eq!(
        effective_keep_alive(Some(Duration::from_secs(20)), Some(Duration::from_secs(60))),
        Some(Duration::from_secs(20))
    );
    // A long configured interval is capped at half the server timeout.
    assert_eq!(
        effective_keep_alive(Some(Duration::from_secs(20)), Some(Duration::from_secs(10))),
        Some(Duration::from_secs(5))
    );
    // Without configuration the server timeout drives the interval.
    assert_eq!(
        effective_keep_alive(None, Some(Duration::from_secs(10))),
        Some(Duration::from_secs(5))
    );
    // Degenerate server timeouts never produce a zero interval.
    assert_eq!(
        effective_keep_alive(None, Some(Duration::from_secs(1))),
        Some(Duration::from_secs(1))
    );
    // No server timeout: configuration passes through unchanged.
    assert_eq!(
        effective_keep_alive(Some(Duration::from_secs(20)), None),
        Some(Duration::from_secs(20))
    );
    assert_eq!(effective_keep_alive(None, None), None);
}

#[test]
fn keep_alive_backoff_grows_and_stays_bounded() {
    let interval = Duration::from_millis(100);
    let first = keep_alive_backoff(interval, 1);
    assert!(first >= Duration::from_millis(75) && first <= Duration::from_millis(125));
    let third = keep_alive_backoff(interval, 3);
    assert!(third >= Duration::from_millis(300) && third <= Duration::from_millis(500));
    // Growth caps at 16x regardless of how many failures accumulate.
    let saturated = keep_alive_backoff(interval, 5);
    let huge = keep_alive_backoff(interval, 1_000);
    for value in [saturated, huge] {
        assert!(
            value >= Duration::from_millis(1_200) && value <= Duration::from_secs(2),
            "expected the 16x cap around 1.6s, got {value:?}"
        );
    }
}

#[test]
fn status_error_maps_auth_statuses() {
    assert!(matches!(status_error("OPTIONS", 401), Error::Unauthorized));
    assert!(matches!(status_error("SETUP", 403), Error::Unauthorized));
    let Error::RtspStatus { method, status } = status_error("PLAY", 453) else {
        panic!("expected Error::RtspStatus");
    };
    assert_eq!(method, "PLAY");
    assert_eq!(status, 453);
}

#[test]
fn candidate_cseq_is_case_insensitive_and_lenient() {
    assert_eq!(
        candidate_headers_cseq(&response_with_headers(200, vec![("cseq", "7")])),
        Some(7)
    );
    assert_eq!(
        candidate_headers_cseq(&response_with_headers(200, vec![("CSeq", "8")])),
        Some(8)
    );
    assert_eq!(
        candidate_headers_cseq(&response_with_headers(200, vec![("cseq", "not-a-number")])),
        None
    );
    assert_eq!(
        candidate_headers_cseq(&response_with_headers(200, vec![])),
        None
    );
}

#[test]
fn validate_response_cseq_enforces_exact_match() {
    let matching = response_with_headers(200, vec![("cseq", "3")]);
    assert!(validate_response_cseq(&matching, 3).is_ok());
    let stale = response_with_headers(200, vec![("cseq", "4")]);
    assert!(validate_response_cseq(&stale, 3).is_err());
    let missing = response_with_headers(200, vec![]);
    assert!(validate_response_cseq(&missing, 3).is_err());
}

#[test]
fn validate_options_rejects_degenerate_values() {
    assert!(validate_options(&ClientOptions::default()).is_ok());
    let options = ClientOptions {
        output_queue_capacity: 0,
        ..ClientOptions::default()
    };
    assert!(matches!(
        validate_options(&options),
        Err(Error::Limit("client output queue capacity"))
    ));
    let options = ClientOptions {
        keep_alive_interval: Some(Duration::ZERO),
        ..ClientOptions::default()
    };
    assert!(matches!(
        validate_options(&options),
        Err(Error::Limit("client keep-alive interval"))
    ));
    let options = ClientOptions {
        request_timeout: Duration::ZERO,
        ..ClientOptions::default()
    };
    assert!(matches!(
        validate_options(&options),
        Err(Error::Limit("client request timeout"))
    ));
}

#[test]
fn builder_round_trips_and_validates() {
    let options = ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .output_queue_capacity(8)
        .keep_alive_interval(None)
        .build()
        .unwrap();
    assert_eq!(options.transport, UpstreamTransport::TcpInterleaved);
    assert_eq!(options.output_queue_capacity, 8);
    assert_eq!(options.keep_alive_interval, None);
    assert!(
        ClientOptions::builder()
            .output_queue_capacity(0)
            .build()
            .is_err()
    );
}

#[test]
fn reconnect_policy_validate_rejects_degenerate_delays() {
    assert!(ClientReconnectPolicy::default().validate().is_ok());
    let zero_initial = ClientReconnectPolicy {
        initial_delay: Duration::ZERO,
        ..ClientReconnectPolicy::default()
    };
    assert!(zero_initial.validate().is_err());
    let inverted = ClientReconnectPolicy {
        initial_delay: Duration::from_secs(10),
        max_delay: Duration::from_secs(1),
        ..ClientReconnectPolicy::default()
    };
    assert!(inverted.validate().is_err());
}

#[test]
fn low_memory_preset_is_bounded() {
    let options = ClientOptions::low_memory();
    assert!(options.output_queue_capacity <= 16);
    assert!(options.jitter.max_packets <= 32);
    assert!(options.jitter.max_delay <= Duration::from_millis(30));
    assert_eq!(options.udp_recv_buffer, Some(256 * 1024));
    assert_eq!(options.max_output_bytes, 2 * 1024 * 1024);
    options.validate().unwrap();
    let mut invalid = options.clone();
    invalid.max_output_bytes = 0;
    assert!(invalid.validate().is_err());
}

#[test]
fn rtp_info_pairs_tracks_by_control_url_and_order() {
    let description = crate::media::MediaDescription {
        session_name: "info".into(),
        tracks: vec![
            crate::media::TrackDescription::h264_video(0, &[], &[], 176, 144, 25),
            crate::media::TrackDescription::aac_audio(1, 48_000, 2),
        ],
        #[cfg(feature = "multicast")]
        connection: None,
    };
    let response = response_with_headers(
        200,
        vec![(
            "rtp-info",
            "url=rtsp://host/stream/trackID=0;seq=1000;rtptime=90000,\
             url=rtsp://host/stream/trackID=1;seq=2000;rtptime=48000",
        )],
    );
    let info = super::setup::parse_rtp_info(&response, &description, "rtsp://host/stream/");
    assert_eq!(info.get(&0).unwrap().sequence, Some(1000));
    assert_eq!(info.get(&0).unwrap().rtptime, Some(90_000));
    assert_eq!(info.get(&1).unwrap().sequence, Some(2000));

    let response = response_with_headers(200, vec![("rtp-info", "seq=7;rtptime=8")]);
    let info = super::setup::parse_rtp_info(&response, &description, "rtsp://host/stream/");
    assert_eq!(info.get(&0).unwrap().sequence, Some(7), "order fallback");
}

#[test]
fn play_range_sets_npt_pts_base() {
    let response = response_with_headers(200, vec![("range", "npt=12.5-20")]);
    assert_eq!(
        super::setup::play_pts_base(&response),
        Duration::from_millis(12_500)
    );
    let live = response_with_headers(200, vec![("range", "npt=now-")]);
    assert_eq!(super::setup::play_pts_base(&live), Duration::ZERO);
}

#[test]
fn jitter_prime_accepts_the_first_hinted_sequence() {
    let mut jitter = crate::rtp::JitterBuffer::new(Default::default());
    jitter.prime(500);
    let packet = crate::rtp::RtpPacket {
        header: crate::rtp::RtpHeader {
            marker: true,
            payload_type: 96,
            sequence: 500,
            timestamp: 0,
            ssrc: 1,
        },
        payload: bytes::Bytes::from_static(b"x"),
    };
    assert_eq!(jitter.push(packet).len(), 1);
}
