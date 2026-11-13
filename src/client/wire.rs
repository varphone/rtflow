use super::*;

pub(super) fn status_error(method: &'static str, status: u16) -> Error {
    if status == 401 || status == 403 {
        Error::Unauthorized
    } else {
        Error::RtspStatus { method, status }
    }
}

pub(super) fn receive_rtp_packet(
    packet: RtpPacket,
    jitter: &mut JitterBuffer,
    depacketizer: &mut Depacketizer,
) -> Vec<Result<AccessUnit>> {
    depacketize_packets(jitter.push(packet), depacketizer)
}

pub(super) fn drain_jitter(
    jitter: &mut JitterBuffer,
    depacketizer: &mut Depacketizer,
) -> Vec<Result<AccessUnit>> {
    depacketize_packets(jitter.drain_expired(), depacketizer)
}

pub(super) fn depacketize_packets(
    packets: Vec<RtpPacket>,
    depacketizer: &mut Depacketizer,
) -> Vec<Result<AccessUnit>> {
    packets
        .into_iter()
        .flat_map(|packet| match depacketizer.push(packet) {
            Ok(units) => units.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(error) => vec![Err(error)],
        })
        .collect()
}

pub(super) fn response_session_id(response: &Response) -> Result<String> {
    response
        .headers
        .get("session")
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::Rtsp("SETUP response has no Session".into()))
}

pub(super) fn encode_request(
    method: &str,
    uri: &str,
    cseq: u32,
    extra: Option<&str>,
    authorization: Option<&str>,
) -> String {
    let mut wire = format!(
        "{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\nUser-Agent: rtflow/{}\r\n",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(value) = authorization {
        wire.push_str("Authorization: ");
        wire.push_str(value);
        wire.push_str("\r\n");
    }
    if let Some(value) = extra {
        wire.push_str(value);
    }
    wire.push_str("\r\n");
    wire
}

pub(super) async fn request(
    stream: &mut ControlConnection,
    method: &str,
    uri: &str,
    cseq: u32,
    extra: Option<&str>,
    wait: Duration,
    credentials: Option<&Credentials>,
) -> Result<Response> {
    let mut authorization = credentials.map(basic_authorization);
    let mut response = send_request(
        stream,
        method,
        uri,
        cseq,
        extra,
        wait,
        authorization.as_deref(),
    )
    .await?;
    if response.status == 401
        && let (Some(credentials), Some(challenge)) =
            (credentials, response.headers.get("www-authenticate"))
    {
        let method = Method::from_str(method)?;
        let challenged = authorization_from_challenge(challenge, method, uri, credentials)?;
        if authorization.as_deref() != Some(challenged.as_str()) {
            authorization = Some(challenged);
            response = send_request(
                stream,
                method.as_str(),
                uri,
                cseq,
                extra,
                wait,
                authorization.as_deref(),
            )
            .await?;
        }
    }
    Ok(response)
}

pub(super) fn should_fallback_to_tcp(error: &Error) -> bool {
    match error {
        Error::Closed => true,
        Error::Io(error) => matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
        ),
        // The server refused the negotiated transport (RFC 2326 §461);
        // interleaved TCP is the standards-blessed fallback.
        Error::RtspStatus { status: 461, .. } => true,
        _ => false,
    }
}

/// Read the CSeq header of a response (if present) for pairing.
pub(super) fn candidate_headers_cseq(response: &Response) -> Option<u32> {
    response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("cseq"))
        .and_then(|(_, value)| value.parse().ok())
}

pub(super) fn validate_response_cseq(response: &Response, cseq: u32) -> Result<()> {
    if response.headers.get("cseq").map(String::as_str) != Some(cseq.to_string().as_str()) {
        return Err(Error::Rtsp(
            "RTSP response CSeq does not match request".into(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn send_request(
    stream: &mut ControlConnection,
    method: &str,
    uri: &str,
    cseq: u32,
    extra: Option<&str>,
    wait: Duration,
    authorization: Option<&str>,
) -> Result<Response> {
    let wire = encode_request(method, uri, cseq, extra, authorization);
    timeout(wait, stream.write_all(wire.as_bytes()))
        .await
        .map_err(|_| Error::Rtsp("RTSP write timeout".into()))??;
    let mut temporary = [0u8; 4096];
    loop {
        if let Some(message) = rtsp::parse(stream.read_ahead_mut(), 16 * 1024, 256 * 1024)? {
            return match message {
                Message::Response(response) => {
                    validate_response_cseq(&response, cseq)?;
                    Ok(response)
                }
                _ => Err(Error::Rtsp("server sent request".into())),
            };
        }
        let size = timeout(wait, stream.read_transport(&mut temporary))
            .await
            .map_err(|_| Error::Rtsp("RTSP response timeout".into()))??;
        if size == 0 {
            return Err(Error::Closed);
        }
        stream
            .read_ahead_mut()
            .extend_from_slice(&temporary[..size]);
    }
}

/// Parse the `timeout=` parameter from a SETUP response Session header.
pub(super) fn response_session_timeout(response: &Response) -> Option<Duration> {
    response
        .headers
        .get("session")?
        .split(';')
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            if key.trim().eq_ignore_ascii_case("timeout") {
                value.trim().parse::<u64>().ok()
            } else {
                None
            }
        })
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}
