use super::options::validate_options;
use super::reconnect::{ReconnectingClientSession, jittered_backoff};
use super::wire::{request, should_fallback_to_tcp, status_error};
use super::*;

pub struct RtspClient;
impl RtspClient {
    pub async fn describe(url: url::Url, options: ClientOptions) -> Result<MediaDescription> {
        validate_options(&options)?;
        let (_, _, _, description, _) = open_and_describe(&url, &options).await?;
        Ok(description)
    }

    /// Convenience entry accepting a URL string; see [`Self::describe`].
    pub async fn describe_str(url: &str, options: ClientOptions) -> Result<MediaDescription> {
        let url = url::Url::parse(url)
            .map_err(|error| Error::Rtsp(format!("invalid RTSP URL: {error}")))?;
        Self::describe(url, options).await
    }

    pub async fn connect(url: url::Url, options: ClientOptions) -> Result<ClientSession> {
        validate_options(&options)?;
        let sink = options.event_sink.clone();
        if options.transport != UpstreamTransport::Auto {
            let (stream, uri, setup_base, description, sdp_mux) =
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
                options,
                next_client_session_id(),
            )
            .await
            .map_err(|failure| failure.error);
        }

        let mut udp_options = options.clone();
        udp_options.transport = UpstreamTransport::Udp;
        let udp_open = open_and_describe(&url, &udp_options).await;
        let (stream, uri, setup_base, description, sdp_mux) = match udp_open {
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
                let (stream, uri, setup_base, description, fallback_mux) =
                    open_and_describe(&url, &tcp_options).await?;
                let mut tcp_options = tcp_options;
                if tcp_options.rtcp_mux.is_none() {
                    tcp_options.rtcp_mux = Some(fallback_mux);
                }
                return setup::connect(stream, uri, setup_base, description, tcp_options, id)
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
                    tcp_options.clone(),
                    id,
                )
                .await;
                match reused {
                    Ok(session) => Ok(session),
                    Err(_) => {
                        // The server may have closed the connection after the
                        // rejection; recover with a fresh one.
                        let (stream, uri, setup_base, description, fallback_mux) =
                            open_and_describe(&url, &tcp_options).await?;
                        let mut tcp_options = tcp_options;
                        if tcp_options.rtcp_mux.is_none() {
                            tcp_options.rtcp_mux = Some(fallback_mux);
                        }
                        setup::connect(stream, uri, setup_base, description, tcp_options, id)
                            .await
                            .map_err(|failure| failure.error)
                    }
                }
            }
            Err(failure) => Err(failure.error),
        }
    }

    /// Convenience entry accepting a URL string; see [`Self::connect`].
    pub async fn connect_str(url: &str, options: ClientOptions) -> Result<ClientSession> {
        let url = url::Url::parse(url)
            .map_err(|error| Error::Rtsp(format!("invalid RTSP URL: {error}")))?;
        Self::connect(url, options).await
    }

    /// Connect and keep retrying after a media-session disconnect.
    ///
    /// Initial connection failures are still returned to the caller.
    pub async fn connect_reconnecting(
        url: url::Url,
        options: ClientOptions,
        policy: ClientReconnectPolicy,
    ) -> Result<ReconnectingClientSession> {
        policy.validate()?;
        let initial = Self::connect(url.clone(), options.clone()).await?;
        let description = initial.description.clone();
        let (tx, receiver) = mpsc::channel(options.output_queue_capacity);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let id = next_client_session_id();
        let sink = options.event_sink.clone();
        let started = Instant::now();
        let task = tokio::spawn(async move {
            let mut active = Some(initial);
            let mut failures = 0u32;
            let mut delay = policy.initial_delay;
            loop {
                let mut session = match active.take() {
                    Some(session) => session,
                    None => match Self::connect(url.clone(), options.clone()).await {
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
                                let _ = tx.send(Err(error)).await;
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
                                let _ = tx.send(Err(error)).await;
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
                                if tx.send(Ok(unit)).await.is_err() {
                                    return;
                                }
                            }
                            Some(Err(error)) => {
                                // Transient errors (keep-alive failures, IO
                                // hiccups) surface to the consumer and then
                                // trigger a reconnect; permanent ones end the
                                // session for good.
                                let permanent =
                                    matches!(error.retry_class(), RetryClass::Never);
                                if tx.send(Err(error)).await.is_err() {
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
        Ok(ReconnectingClientSession {
            description,
            id,
            receiver,
            cancel,
            task,
        })
    }
}

async fn open_and_describe(
    url: &url::Url,
    options: &ClientOptions,
) -> Result<(ControlConnection, String, String, MediaDescription, bool)> {
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
    let mut stream = match url.scheme() {
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
    let response = request(
        &mut stream,
        "OPTIONS",
        &uri,
        1,
        None,
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    if response.status != 200 {
        return Err(status_error("OPTIONS", response.status));
    }
    let response = request(
        &mut stream,
        "DESCRIBE",
        &uri,
        2,
        Some("Accept: application/sdp\r\n"),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
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
    Ok((stream, uri, setup_base, description, rtcp_mux))
}
