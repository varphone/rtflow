use super::options::validate_options;
use super::reconnect::{ReconnectingPlaySession, jittered_backoff};
use super::wire::{request, should_fallback_to_tcp, status_error};
use super::*;

/// URL conversion accepted by [`RtspClient`] entry points. `Url`, `&str`
/// and `String` are supported, so `connect`/`describe` are gone.
pub trait IntoRtspUrl {
    fn into_rtsp_url(self) -> Result<url::Url>;
}

impl IntoRtspUrl for url::Url {
    fn into_rtsp_url(self) -> Result<url::Url> {
        Ok(self)
    }
}

impl IntoRtspUrl for &url::Url {
    fn into_rtsp_url(self) -> Result<url::Url> {
        Ok(self.clone())
    }
}

impl IntoRtspUrl for &str {
    fn into_rtsp_url(self) -> Result<url::Url> {
        url::Url::parse(self).map_err(|error| Error::Rtsp(format!("invalid RTSP URL: {error}")))
    }
}

impl IntoRtspUrl for &String {
    fn into_rtsp_url(self) -> Result<url::Url> {
        self.as_str().into_rtsp_url()
    }
}

impl IntoRtspUrl for String {
    fn into_rtsp_url(self) -> Result<url::Url> {
        self.as_str().into_rtsp_url()
    }
}

pub struct RtspClient;
impl RtspClient {
    pub async fn describe(
        url: impl IntoRtspUrl,
        options: ClientOptions,
    ) -> Result<MediaDescription> {
        let url = url.into_rtsp_url()?;
        tracing::debug!(host = url.host_str().unwrap_or(""), path = %url.path(), "RTSP client DESCRIBE");
        validate_options(&options)?;
        let (_, _, _, description, _, _) = open_and_describe(&url, &options).await?;
        Ok(description)
    }

    /// Fetch the server's OPTIONS capabilities (`Public`, `Supported`,
    /// `Server`). The control connection is closed afterwards.
    pub async fn options(
        url: impl IntoRtspUrl,
        options: ClientOptions,
    ) -> Result<ServerCapabilities> {
        let url = url.into_rtsp_url()?;
        validate_options(&options)?;
        let (mut stream, uri) = open_control(&url, &options).await?;
        let require = options.require_headers();
        let response = request(
            &mut stream,
            "OPTIONS",
            &uri,
            1,
            require.as_deref(),
            options.request_timeout,
            options.credentials.as_ref(),
        )
        .await?;
        if response.status != 200 {
            return Err(status_error("OPTIONS", response.status));
        }
        Ok(ServerCapabilities::from_response(&response))
    }

    pub async fn connect(url: impl IntoRtspUrl, options: ClientOptions) -> Result<PlaySession> {
        validate_options(&options)?;
        let mut current = url.into_rtsp_url()?;
        for _ in 0..5 {
            match Self::connect_once(current.clone(), options.clone()).await {
                Ok(session) => return Ok(session),
                Err(Error::Redirect(location)) if options.follow_redirects => {
                    let next = url::Url::parse(&location).map_err(|error| {
                        Error::Rtsp(format!("invalid REDIRECT location: {error}"))
                    })?;
                    if !redirect_allowed(&current, &next, &options) {
                        return Err(Error::Redirect(location));
                    }
                    current = next;
                }
                Err(error) => {
                    tracing::warn!(host = current.host_str().unwrap_or(""), path = %current.path(), %error, "RTSP client connect failed");
                    return Err(error);
                }
            }
        }
        Err(Error::Rtsp("too many RTSP REDIRECTs".into()))
    }

    async fn connect_once(url: url::Url, options: ClientOptions) -> Result<PlaySession> {
        validate_options(&options)?;
        tracing::debug!(
            host = url.host_str().unwrap_or(""),
            port = url.port(),
            path = %url.path(),
            transport = ?options.transport,
            "RTSP client connecting"
        );
        let sink = options.event_sink.clone();
        if options.transport != UpstreamTransport::Auto {
            let (stream, uri, setup_base, description, sdp_mux, capabilities) =
                open_and_describe(&url, &options).await?;
            let mut options = options;
            if options.rtcp_mux.is_none() {
                options.rtcp_mux = Some(sdp_mux);
            }
            return setup::connect(
                stream,
                uri,
                setup_base,
                description,
                capabilities,
                options,
                next_client_session_id(),
            )
            .await
            .map_err(|failure| failure.error);
        }

        let mut udp_options = options.clone();
        udp_options.transport = UpstreamTransport::Udp;
        let udp_open = open_and_describe(&url, &udp_options).await;
        let (stream, uri, setup_base, description, sdp_mux, capabilities) = match udp_open {
            Ok(value) => value,
            Err(error) if should_fallback_to_tcp(&error) => {
                let id = next_client_session_id();
                tracing::debug!(session_id = %id, error = %error, "UDP RTSP connection failed; retrying with TCP interleaving");
                emit_client_event(
                    &sink,
                    ClientEvent::Fallback {
                        id,
                        from: "udp",
                        to: "tcp-interleaved",
                    },
                );
                let mut tcp_options = options.clone();
                tcp_options.transport = UpstreamTransport::TcpInterleaved;
                let (stream, uri, setup_base, description, fallback_mux, capabilities) =
                    open_and_describe(&url, &tcp_options).await?;
                let mut tcp_options = tcp_options;
                if tcp_options.rtcp_mux.is_none() {
                    tcp_options.rtcp_mux = Some(fallback_mux);
                }
                return setup::connect(
                    stream,
                    uri,
                    setup_base,
                    description,
                    capabilities,
                    tcp_options,
                    id,
                )
                .await
                .map_err(|failure| failure.error);
            }
            Err(error) => return Err(error),
        };
        let id = next_client_session_id();
        let mut udp_options = udp_options;
        if udp_options.rtcp_mux.is_none() {
            udp_options.rtcp_mux = Some(sdp_mux);
        }
        match setup::connect(
            stream,
            uri.clone(),
            setup_base,
            description,
            capabilities.clone(),
            udp_options,
            id,
        )
        .await
        {
            Ok(session) => Ok(session),
            Err(failure) if should_fallback_to_tcp(&failure.error) => {
                tracing::debug!(session_id = %id, error = %failure.error, "UDP RTSP SETUP failed; retrying with TCP interleaving");
                emit_client_event(
                    &sink,
                    ClientEvent::Fallback {
                        id,
                        from: "udp",
                        to: "tcp-interleaved",
                    },
                );
                let mut tcp_options = options;
                tcp_options.transport = UpstreamTransport::TcpInterleaved;
                // The SETUP rejection left the control connection and the
                // parsed SDP intact: retry interleaved SETUP in place
                // instead of redoing OPTIONS+DESCRIBE (audit RT-9).
                let reused = setup::connect(
                    failure.stream,
                    uri.clone(),
                    failure.setup_base,
                    failure.description,
                    capabilities.clone(),
                    tcp_options.clone(),
                    id,
                )
                .await;
                match reused {
                    Ok(session) => Ok(session),
                    Err(_) => {
                        // The server may have closed the connection after the
                        // rejection; recover with a fresh one.
                        let (stream, uri, setup_base, description, fallback_mux, capabilities) =
                            open_and_describe(&url, &tcp_options).await?;
                        let mut tcp_options = tcp_options;
                        if tcp_options.rtcp_mux.is_none() {
                            tcp_options.rtcp_mux = Some(fallback_mux);
                        }
                        setup::connect(
                            stream,
                            uri,
                            setup_base,
                            description,
                            capabilities,
                            tcp_options,
                            id,
                        )
                        .await
                        .map_err(|failure| failure.error)
                    }
                }
            }
            Err(failure) => Err(failure.error),
        }
    }

    /// Publish a stream: OPTIONS + ANNOUNCE + SETUP + RECORD.
    ///
    /// The returned [`RecordSession`] owns the UDP RTP sockets and a
    /// keep-alive task; call [`RecordSession::push`] for every access unit.
    pub async fn publish(
        url: impl IntoRtspUrl,
        description: MediaDescription,
        options: ClientOptions,
    ) -> Result<RecordSession> {
        let url = url.into_rtsp_url()?;
        validate_options(&options)?;
        super::publish::connect(&url, description, &options).await
    }

    /// Connect and keep retrying after a media-session disconnect.
    ///
    /// Initial connection failures are still returned to the caller.
    pub async fn connect_reconnecting(
        url: impl IntoRtspUrl,
        options: ClientOptions,
        policy: ClientReconnectPolicy,
    ) -> Result<ReconnectingPlaySession> {
        let url = url.into_rtsp_url()?;
        policy.validate()?;
        let initial = Self::connect(url.clone(), options.clone()).await?;
        let description = initial.description.clone();
        let (tx, receiver) = super::queue::FrameReceiver::new(
            options.output_queue_capacity,
            options.max_output_bytes,
        );
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let id = next_client_session_id();
        let sink = options.event_sink.clone();
        let started = Instant::now();
        let mut current_url = url;
        let task = tokio::spawn(async move {
            let mut active = Some(initial);
            let mut failures = 0u32;
            let mut delay = policy.initial_delay;
            loop {
                let mut session = match active.take() {
                    Some(session) => session,
                    None => match Self::connect(current_url.clone(), options.clone()).await {
                        Ok(session) => {
                            failures = 0;
                            delay = policy.initial_delay;
                            session
                        }
                        Err(error) => {
                            // Permanent failures (bad credentials, invalid
                            // configuration) never recover by retrying; with
                            // max_attempts=0 they would otherwise loop forever.
                            if matches!(error.retry_class(), RetryClass::Never) {
                                let code = error.code();
                                tracing::warn!(session_id = %id, error = %error, "RTSP client reconnect aborted by permanent error");
                                emit_client_event(&sink, ClientEvent::Failed { id, code });
                                emit_client_event(
                                    &sink,
                                    ClientEvent::Closed {
                                        id,
                                        elapsed: started.elapsed(),
                                        error: Some(code),
                                    },
                                );
                                let _ = tx.try_send(Err(error));
                                return;
                            }
                            failures = failures.saturating_add(1);
                            if policy.max_attempts != 0 && failures >= policy.max_attempts {
                                let code = error.code();
                                tracing::warn!(session_id = %id, error = %error, attempts = failures, "RTSP client reconnect attempts exhausted");
                                emit_client_event(&sink, ClientEvent::Failed { id, code });
                                emit_client_event(
                                    &sink,
                                    ClientEvent::Closed {
                                        id,
                                        elapsed: started.elapsed(),
                                        error: Some(code),
                                    },
                                );
                                let _ = tx.try_send(Err(error));
                                return;
                            }
                            let jittered = jittered_backoff(delay, failures);
                            tracing::warn!(session_id = %id, error = %error, attempt = failures, delay_ms = jittered.as_millis(), "RTSP client reconnect failed");
                            emit_client_event(
                                &sink,
                                ClientEvent::ReconnectBackoff {
                                    id,
                                    attempt: failures,
                                    delay: jittered,
                                },
                            );
                            tokio::select! {
                                _ = task_cancel.cancelled() => return,
                                _ = sleep(jittered) => {}
                            }
                            delay = delay.saturating_mul(2).min(policy.max_delay);
                            continue;
                        }
                    },
                };

                loop {
                    tokio::select! {
                        _ = task_cancel.cancelled() => return,
                        frame = session.next() => match frame {
                            Some(Ok(unit)) => {
                                if tx.try_send(Ok(unit)).is_err() {
                                    return;
                                }
                            }
                            Some(Err(error)) => {
                                // A server REDIRECT is followed for the next
                                // reconnect attempt when enabled; it is not a
                                // media failure.
                                if let Error::Redirect(location) = &error
                                    && options.follow_redirects
                                    && let Ok(next) = url::Url::parse(location)
                                    && redirect_allowed(&current_url, &next, &options)
                                {
                                    emit_client_event(
                                        &sink,
                                        ClientEvent::Redirect {
                                            id,
                                            location: location.clone(),
                                        },
                                    );
                                    current_url = next;
                                    break;
                                }
                                // Transient errors (keep-alive failures, IO
                                // hiccups) surface to the consumer and then
                                // trigger a reconnect; permanent ones end the
                                // session for good.
                                let permanent =
                                    matches!(error.retry_class(), RetryClass::Never);
                                if tx.try_send(Err(error)).is_err() {
                                    return;
                                }
                                if permanent {
                                    return;
                                }
                                break;
                            }
                            None => break,
                        }
                    }
                }
                tracing::warn!(session_id = %id, "RTSP client session ended; reconnecting");
                let jittered = jittered_backoff(delay, failures);
                emit_client_event(
                    &sink,
                    ClientEvent::ReconnectBackoff {
                        id,
                        attempt: failures,
                        delay: jittered,
                    },
                );
                tokio::select! {
                    _ = task_cancel.cancelled() => return,
                    _ = sleep(jittered) => {}
                }
                delay = delay.saturating_mul(2).min(policy.max_delay);
            }
        });
        Ok(ReconnectingPlaySession {
            description,
            id,
            receiver,
            cancel,
            task,
        })
    }
}

#[allow(clippy::large_enum_variant)]
fn redirect_allowed(current: &url::Url, next: &url::Url, options: &ClientOptions) -> bool {
    options.allow_cross_origin_redirect
        || (next.scheme() == current.scheme()
            && next.host_str() == current.host_str()
            && next.port_or_known_default() == current.port_or_known_default())
}

#[allow(clippy::large_enum_variant)]
enum DescribeOutcome {
    Ready(
        ControlConnection,
        String,
        String,
        MediaDescription,
        bool,
        ServerCapabilities,
    ),
    Redirect(url::Url),
}

fn redirect_outcome(
    base: &url::Url,
    response: &Response,
    options: &ClientOptions,
) -> Result<DescribeOutcome> {
    let location = response
        .headers
        .get("location")
        .ok_or_else(|| Error::Rtsp("RTSP redirect has no Location".into()))?;
    let target = base
        .join(location)
        .map_err(|error| Error::Rtsp(format!("invalid RTSP redirect Location: {error}")))?;
    // Do not forward credentials to another origin implicitly; compare the
    // effective port as well (same host on another port is another service).
    if !redirect_allowed(base, &target, options) {
        return Err(Error::Rtsp(
            "RTSP redirect to a different origin is not followed".into(),
        ));
    }
    Ok(DescribeOutcome::Redirect(target))
}

async fn open_and_describe(
    url: &url::Url,
    options: &ClientOptions,
) -> Result<(
    ControlConnection,
    String,
    String,
    MediaDescription,
    bool,
    ServerCapabilities,
)> {
    let mut current = url.clone();
    for _ in 0..5 {
        match open_and_describe_once(&current, options).await? {
            DescribeOutcome::Ready(stream, uri, base, description, mux, capabilities) => {
                return Ok((stream, uri, base, description, mux, capabilities));
            }
            DescribeOutcome::Redirect(target) => current = target,
        }
    }
    Err(Error::Rtsp("too many RTSP redirects".into()))
}

pub(super) async fn open_control(
    url: &url::Url,
    options: &ClientOptions,
) -> Result<(ControlConnection, String)> {
    let host = url
        .host_str()
        .ok_or_else(|| Error::Rtsp("RTSP URL has no host".into()))?;
    // `url` reports IPv6 hosts bracketed ([::1]); the resolver wants the
    // bare address.
    let host = host.trim_matches(['[', ']']);
    let port = url
        .port()
        .unwrap_or_else(|| if url.scheme() == "rtsps" { 322 } else { 554 });
    let tcp = timeout(options.request_timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::Rtsp("RTSP connect timeout".into()))??;
    tcp.set_nodelay(true)?;
    let local_addr = tcp.local_addr()?;
    let peer_addr = tcp.peer_addr()?;
    let stream = match url.scheme() {
        "rtsp" => ControlConnection::new(tcp, local_addr, peer_addr),
        #[cfg(feature = "tls")]
        "rtsps" => {
            let tls = options
                .tls
                .as_ref()
                .ok_or_else(|| Error::Tls("RTSPS requires a client TLS configuration".into()))?;
            let server_name = tls.server_name.as_deref().unwrap_or(host).to_owned();
            let server_name = rustls::pki_types::ServerName::try_from(server_name)
                .map_err(|error| Error::Tls(error.to_string()))?;
            let connector = tokio_rustls::TlsConnector::from(tls.config.clone());
            let secured = timeout(tls.handshake_timeout, connector.connect(server_name, tcp))
                .await
                .map_err(|_| Error::Tls("TLS handshake timeout".into()))?
                .map_err(|error| Error::Tls(error.to_string()))?;
            ControlConnection::new(secured, local_addr, peer_addr)
        }
        #[cfg(not(feature = "tls"))]
        "rtsps" => return Err(Error::Rtsp("RTSPS requires the tls feature".into())),
        scheme => return Err(Error::Rtsp(format!("unsupported RTSP URL scheme {scheme}"))),
    };
    let uri = url.as_str().to_owned();
    Ok((stream, uri))
}

async fn open_and_describe_once(
    url: &url::Url,
    options: &ClientOptions,
) -> Result<DescribeOutcome> {
    let (mut stream, uri) = open_control(url, options).await?;
    let require = options.require_headers();
    let response = request(
        &mut stream,
        "OPTIONS",
        &uri,
        1,
        require.as_deref(),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    if (300..=399).contains(&response.status) {
        return redirect_outcome(url, &response, options);
    }
    if response.status != 200 {
        return Err(status_error("OPTIONS", response.status));
    }
    let capabilities = ServerCapabilities::from_response(&response);
    let describe_headers = format!(
        "Accept: application/sdp\r\n{}",
        require.as_deref().unwrap_or_default()
    );
    let response = request(
        &mut stream,
        "DESCRIBE",
        &uri,
        2,
        Some(&describe_headers),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    if (300..=399).contains(&response.status) {
        return redirect_outcome(url, &response, options);
    }
    if response.status != 200 {
        return Err(status_error("DESCRIBE", response.status));
    }
    let setup_base = response
        .headers
        .get("content-base")
        .or_else(|| response.headers.get("content-location"))
        .map(|value| sdp::control_uri(&uri, value))
        .transpose()?
        .unwrap_or_else(|| uri.clone());
    let rtcp_mux = std::str::from_utf8(&response.body)
        .map(|sdp| {
            sdp.lines()
                .any(|line| line.trim().eq_ignore_ascii_case("a=rtcp-mux"))
        })
        .unwrap_or(false);
    let description = sdp::parse(&response.body)?;
    Ok(DescribeOutcome::Ready(
        stream,
        uri,
        setup_base,
        description,
        rtcp_mux,
        capabilities,
    ))
}
