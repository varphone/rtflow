use super::reconnect::jittered_backoff;
use super::wire::*;
use super::*;

/// The wire the control task drives: either an owned control connection
/// (UDP transport) or the shared interleaved writer/response stream.
pub(super) enum ControlTransport {
    Owned(ControlConnection),
    Interleaved {
        writer: Arc<Mutex<ControlWriteHalf>>,
        responses: mpsc::Receiver<Response>,
    },
}

pub(super) enum ControlCommand {
    Request {
        method: &'static str,
        extra: Option<String>,
        body: Option<Vec<u8>>,
        reply: oneshot::Sender<Result<Response>>,
    },
}

async fn wait_for_instant(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// One RTSP control round trip on the session's transport.
#[allow(clippy::too_many_arguments)]
async fn perform_control(
    transport: &mut ControlTransport,
    method: &'static str,
    uri: &str,
    cseq: &mut u32,
    session_id: &str,
    extra: Option<&str>,
    body: Option<&[u8]>,
    request_timeout: Duration,
    credentials: Option<&Credentials>,
) -> Result<Response> {
    let mut headers = format!("Session: {session_id}\r\n");
    if let Some(extra) = extra {
        headers.push_str(extra);
    }
    let sequence = *cseq;
    *cseq = cseq.wrapping_add(1);
    match transport {
        ControlTransport::Owned(control) => {
            let response = request_with_body(
                control,
                method,
                uri,
                sequence,
                Some(&headers),
                body,
                request_timeout,
                credentials,
            )
            .await?;
            if response.status == 551 {
                return Err(unsupported_error(method, &response));
            }
            if response.status == 551 {
                return Err(unsupported_error(method, &response));
            }
            if response.status != 200 {
                return Err(status_error(method, response.status));
            }
            Ok(response)
        }
        ControlTransport::Interleaved { writer, responses } => {
            let mut authorization = credentials.map(basic_authorization);
            let response = loop {
                let wire = encode_request_body(
                    method,
                    uri,
                    sequence,
                    Some(&headers),
                    authorization.as_deref(),
                    body,
                );
                tokio::time::timeout(request_timeout, async {
                    writer.lock().await.write_all(&wire).await
                })
                .await
                .map_err(|_| Error::Limit("interleaved control write timeout"))??;
                let candidate = loop {
                    let candidate = timeout(request_timeout, responses.recv())
                        .await
                        .map_err(|_| Error::Rtsp("RTSP response timeout".into()))?
                        .ok_or(Error::Closed)?;
                    if candidate_headers_cseq(&candidate) == Some(sequence) {
                        break candidate;
                    }
                };
                if candidate.status != 401 {
                    break candidate;
                }
                let (Some(credentials), Some(challenge)) =
                    (credentials, candidate.headers.get("www-authenticate"))
                else {
                    break candidate;
                };
                let challenged = authorization_from_challenge(
                    challenge,
                    Method::from_str(method)?,
                    uri,
                    credentials,
                )?;
                if authorization.as_deref() == Some(challenged.as_str()) {
                    break candidate;
                }
                authorization = Some(challenged);
            };
            if response.status == 551 {
                return Err(unsupported_error(method, &response));
            }
            if response.status == 551 {
                return Err(unsupported_error(method, &response));
            }
            if response.status != 200 {
                return Err(status_error(method, response.status));
            }
            Ok(response)
        }
    }
}

/// Dedicated control-plane task: serializes PLAY/PAUSE/TEARDOWN commands and
/// schedules keep-alives, keeping RTSP round trips off the media path
/// (audits RT-7/RT-8). Keep-alive failures back off (R23) and downgrade
/// GET_PARAMETER to OPTIONS when the server does not implement it.
#[allow(clippy::too_many_arguments)]
pub(super) async fn control_task(
    mut transport: ControlTransport,
    mut commands: mpsc::Receiver<ControlCommand>,
    media_tx: mpsc::Sender<Result<AccessUnit>>,
    uri: String,
    session_id: String,
    mut cseq: u32,
    credentials: Option<Credentials>,
    request_timeout: Duration,
    keep_alive_interval: Option<Duration>,
    id: SessionId,
    sink: Option<Arc<dyn EventSink>>,
    cancel: CancellationToken,
) {
    let mut method: &'static str = "GET_PARAMETER";
    let mut failures = 0u32;
    let mut last_failure: Option<Instant> = None;
    let mut last_control = Instant::now();
    loop {
        let due = keep_alive_interval.map(|interval| {
            let mut due = last_control + interval;
            if let Some(failed_at) = last_failure {
                due = due.max(failed_at + keep_alive_backoff(interval, failures));
            }
            due
        });
        tokio::select! {
            _ = cancel.cancelled() => return,
            command = commands.recv() => {
                let Some(ControlCommand::Request {
                    method: requested,
                    extra,
                    body,
                    reply,
                }) = command
                else {
                    return;
                };
                let result = perform_control(
                    &mut transport,
                    requested,
                    &uri,
                    &mut cseq,
                    &session_id,
                    extra.as_deref(),
                    body.as_deref(),
                    request_timeout,
                    credentials.as_ref(),
                )
                .await;
                let result = match result {
                    Err(Error::RtspStatus {
                        status: 405 | 501, ..
                    }) if requested == "GET_PARAMETER" && method == "GET_PARAMETER" => {
                        method = "OPTIONS";
                        perform_control(
                            &mut transport,
                            method,
                            &uri,
                            &mut cseq,
                            &session_id,
                            None,
                            None,
                            request_timeout,
                            credentials.as_ref(),
                        )
                        .await
                    }
                    other => other,
                };
                match &result {
                    Ok(_) => {
                        failures = 0;
                        last_failure = None;
                        last_control = Instant::now();
                    }
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        last_failure = Some(Instant::now());
                        tracing::warn!(
                            session_id = %id,
                            method = requested,
                            consecutive = failures,
                            error = %error,
                            "RTSP client control request failed"
                        );
                        emit_client_event(
                            &sink,
                            ClientEvent::KeepAliveFailed {
                                id,
                                consecutive: failures,
                            },
                        );
                    }
                }
                let _ = reply.send(result);
            }
            _ = wait_for_instant(due) => {
                let result = perform_control(
                    &mut transport,
                    method,
                    &uri,
                    &mut cseq,
                    &session_id,
                    None,
                    None,
                    request_timeout,
                    credentials.as_ref(),
                )
                .await;
                let result = match result {
                    Err(Error::RtspStatus {
                        status: 405 | 501, ..
                    }) if method == "GET_PARAMETER" => {
                        // GET_PARAMETER is optional in RFC 2326; remember the
                        // capability and keep the session alive via OPTIONS.
                        method = "OPTIONS";
                        perform_control(
                            &mut transport,
                            method,
                            &uri,
                            &mut cseq,
                            &session_id,
                            None,
                            None,
                            request_timeout,
                            credentials.as_ref(),
                        )
                        .await
                    }
                    other => other,
                };
                match result {
                    Ok(_) => {
                        failures = 0;
                        last_failure = None;
                        last_control = Instant::now();
                    }
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        last_failure = Some(Instant::now());
                        tracing::warn!(
                            session_id = %id,
                            consecutive = failures,
                            error = %error,
                            "RTSP client keep-alive failed"
                        );
                        emit_client_event(
                            &sink,
                            ClientEvent::KeepAliveFailed {
                                id,
                                consecutive: failures,
                            },
                        );
                        // Surface the failure to the consumer; drop it if the
                        // queue is full rather than blocking the control plane.
                        let _ = media_tx.try_send(Err(error));
                    }
                }
            }
        }
    }
}

/// Backoff for failed keep-alive attempts (audit R23): the interval doubles
/// per consecutive failure (capped at 16x) with the standard jitter applied.
pub(super) fn keep_alive_backoff(interval: Duration, failures: u32) -> Duration {
    let growth = 1u32 << failures.saturating_sub(1).min(4);
    jittered_backoff(interval.saturating_mul(growth), failures)
}

/// Effective keep-alive interval given the client configuration and the
/// server's Session `timeout=` parameter (RFC 2326 §12.37): stay at most
/// half the server timeout so a single dropped request cannot expire the
/// session. When the client did not configure an interval, derive one from
/// the server timeout instead of disabling keep-alives.
pub(super) fn effective_keep_alive(
    configured: Option<Duration>,
    server_timeout: Option<Duration>,
) -> Option<Duration> {
    match (configured, server_timeout) {
        (Some(configured), Some(server_timeout)) => Some(configured.min(server_timeout / 2)),
        (None, Some(server_timeout)) => Some((server_timeout / 2).max(Duration::from_secs(1))),
        (configured, None) => configured,
    }
}
