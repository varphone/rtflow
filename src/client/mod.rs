//! RTSP client control-plane primitives.
mod setup;

#[cfg(feature = "tls")]
use crate::security::TlsClientConfig;
/// Transport selection for RTSP client connections (re-exported here so
/// callers do not need to reach into `rtflow::proxy`/`rtflow::transport`).
pub use crate::transport::UpstreamTransport;
use crate::{
    Error, Result, RetryClass,
    auth::{Credentials, authorization_from_challenge, basic_authorization},
    identity::SessionId,
    media::{AccessUnit, MediaDescription},
    observe::{ClientEvent, EventSink, FrameworkEvent},
    protocol::range::NptRange,
    protocol::sdp,
    rtcp::{ReceptionSnapshot, RtcpSession},
    rtp::{Depacketizer, JitterBuffer, JitterBufferConfig, RtpPacket},
    rtsp::{self, Message, Method, Response},
    runtime::io::{ControlConnection, ControlWriteHalf},
};
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;

static NEXT_CLIENT_SESSION: AtomicU64 = AtomicU64::new(1);

fn next_client_session_id() -> SessionId {
    SessionId::new(NEXT_CLIENT_SESSION.fetch_add(1, Ordering::Relaxed))
}

fn emit_client_event(sink: &Option<Arc<dyn EventSink>>, event: ClientEvent) {
    if let Some(sink) = sink {
        sink.record(&FrameworkEvent::Client(event));
    }
}

#[derive(Clone)]
#[non_exhaustive]
pub struct ClientOptions {
    pub transport: UpstreamTransport,
    pub credentials: Option<Credentials>,
    pub request_timeout: Duration,
    pub jitter: JitterBufferConfig,
    pub output_queue_capacity: usize,
    pub keep_alive_interval: Option<Duration>,
    #[cfg(feature = "tls")]
    pub tls: Option<TlsClientConfig>,
    pub play_range: Option<NptRange>,
    /// Optional sink for [`ClientEvent`]s (lifecycle, fallback, backoff).
    pub event_sink: Option<Arc<dyn EventSink>>,
    /// UDP socket receive-buffer size per RTP/RTCP socket. Defaults to 4MiB;
    /// memory-constrained embedded clients can lower it at the cost of burst
    /// tolerance. Applies to UDP transport only.
    pub udp_recv_buffer: Option<usize>,
}
impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            transport: UpstreamTransport::Auto,
            credentials: None,
            request_timeout: Duration::from_secs(5),
            jitter: JitterBufferConfig::default(),
            output_queue_capacity: 64,
            keep_alive_interval: Some(Duration::from_secs(20)),
            #[cfg(feature = "tls")]
            tls: None,
            play_range: None,
            event_sink: None,
            udp_recv_buffer: None,
        }
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ClientReconnectPolicy {
    /// Delay before the first reconnect attempt.
    pub initial_delay: Duration,
    /// Upper bound for exponential reconnect backoff.
    pub max_delay: Duration,
    /// Number of failed reconnect attempts before reporting an error; zero retries forever.
    pub max_attempts: u32,
}

impl Default for ClientReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            max_attempts: 0,
        }
    }
}

impl ClientReconnectPolicy {
    pub fn builder() -> ClientReconnectPolicyBuilder {
        ClientReconnectPolicyBuilder {
            policy: Self::default(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.initial_delay == Duration::ZERO || self.max_delay < self.initial_delay {
            return Err(Error::Limit("client reconnect delay"));
        }
        Ok(())
    }
}

pub struct ClientReconnectPolicyBuilder {
    policy: ClientReconnectPolicy,
}

impl ClientReconnectPolicyBuilder {
    pub fn initial_delay(mut self, value: Duration) -> Self {
        self.policy.initial_delay = value;
        self
    }

    pub fn max_delay(mut self, value: Duration) -> Self {
        self.policy.max_delay = value;
        self
    }

    pub fn max_attempts(mut self, value: u32) -> Self {
        self.policy.max_attempts = value;
        self
    }

    pub fn build(self) -> Result<ClientReconnectPolicy> {
        self.policy.validate()?;
        Ok(self.policy)
    }
}

/// Exponential backoff with deterministic jitter (±25%) so that many
/// clients failing at once do not reconnect in a synchronized herd.
fn jittered_backoff(delay: Duration, attempt: u32) -> Duration {
    let sample = (attempt.wrapping_mul(1_103_515_245).wrapping_add(12_345) % 101) as f32 / 100.0;
    delay.mul_f32(0.75 + 0.5 * sample)
}

/// Backoff for failed keep-alive attempts (audit R23): the interval doubles
/// per consecutive failure (capped at 16x) with the standard jitter applied.
fn keep_alive_backoff(interval: Duration, failures: u32) -> Duration {
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

/// Parse the `timeout=` parameter from a SETUP response Session header.
fn response_session_timeout(response: &Response) -> Option<Duration> {
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

pub struct ReconnectingClientSession {
    pub description: MediaDescription,
    id: SessionId,
    receiver: mpsc::Receiver<Result<AccessUnit>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl ReconnectingClientSession {
    /// Stable identifier correlating this session's [`ClientEvent`]s.
    pub fn session_id(&self) -> SessionId {
        self.id
    }

    pub async fn next(&mut self) -> Option<Result<AccessUnit>> {
        self.receiver.recv().await
    }

    pub async fn teardown(self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

impl Drop for ReconnectingClientSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

impl ClientOptions {
    pub fn builder() -> ClientOptionsBuilder {
        ClientOptionsBuilder {
            options: Self::default(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_options(self)
    }
}

pub struct ClientOptionsBuilder {
    options: ClientOptions,
}

impl ClientOptionsBuilder {
    pub fn transport(mut self, value: UpstreamTransport) -> Self {
        self.options.transport = value;
        self
    }

    pub fn credentials(mut self, value: Credentials) -> Self {
        self.options.credentials = Some(value);
        self
    }

    pub fn request_timeout(mut self, value: Duration) -> Self {
        self.options.request_timeout = value;
        self
    }

    pub fn jitter(mut self, value: JitterBufferConfig) -> Self {
        self.options.jitter = value;
        self
    }

    pub fn output_queue_capacity(mut self, value: usize) -> Self {
        self.options.output_queue_capacity = value;
        self
    }

    pub fn keep_alive_interval(mut self, value: Option<Duration>) -> Self {
        self.options.keep_alive_interval = value;
        self
    }

    #[cfg(feature = "tls")]
    pub fn tls(mut self, value: TlsClientConfig) -> Self {
        self.options.tls = Some(value);
        self
    }

    pub fn play_range(mut self, value: NptRange) -> Self {
        self.options.play_range = Some(value);
        self
    }

    pub fn event_sink(mut self, value: Arc<dyn EventSink>) -> Self {
        self.options.event_sink = Some(value);
        self
    }

    pub fn udp_recv_buffer(mut self, value: usize) -> Self {
        self.options.udp_recv_buffer = Some(value);
        self
    }

    pub fn build(self) -> Result<ClientOptions> {
        self.options.validate()?;
        Ok(self.options)
    }
}

pub struct ClientSession {
    pub description: MediaDescription,
    id: SessionId,
    receiver: mpsc::Receiver<Result<AccessUnit>>,
    control_tx: Option<mpsc::Sender<ControlCommand>>,
    request_timeout: Duration,
    playing: bool,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    quality: Arc<Mutex<HashMap<u8, RtcpSession>>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientQualitySnapshot {
    pub tracks: BTreeMap<u8, ReceptionSnapshot>,
}

/// Internal construction bundle so `ClientSession::new` stays readable.
pub(super) struct SessionParts {
    pub(super) description: MediaDescription,
    pub(super) id: SessionId,
    pub(super) receiver: mpsc::Receiver<Result<AccessUnit>>,
    pub(super) control_tx: Option<mpsc::Sender<ControlCommand>>,
    pub(super) request_timeout: Duration,
    pub(super) cancel: CancellationToken,
    pub(super) tasks: Vec<JoinHandle<()>>,
    pub(super) quality: Arc<Mutex<HashMap<u8, RtcpSession>>>,
}

impl ClientSession {
    fn new(parts: SessionParts) -> Self {
        Self {
            description: parts.description,
            id: parts.id,
            receiver: parts.receiver,
            control_tx: parts.control_tx,
            request_timeout: parts.request_timeout,
            playing: true,
            cancel: parts.cancel,
            tasks: parts.tasks,
            quality: parts.quality,
        }
    }

    /// Stable identifier for correlating [`ClientEvent`]s and logs.
    pub fn session_id(&self) -> SessionId {
        self.id
    }
    pub async fn play(&mut self) -> Result<()> {
        self.send_control("PLAY").await?;
        self.playing = true;
        Ok(())
    }
    pub async fn pause(&mut self) -> Result<()> {
        self.send_control("PAUSE").await?;
        self.playing = false;
        Ok(())
    }
    /// Receive the next access unit. Media consumption never blocks on the
    /// control plane: keep-alives run in the dedicated control task
    /// (audit RT-7/RT-8) and failures surface as error frames.
    pub async fn next(&mut self) -> Option<Result<AccessUnit>> {
        self.receiver.recv().await
    }

    pub async fn keep_alive(&mut self) -> Result<()> {
        self.send_control("GET_PARAMETER").await
    }
    async fn send_control(&mut self, method: &'static str) -> Result<()> {
        let Some(commands) = self.control_tx.as_ref() else {
            return Err(Error::Closed);
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        commands
            .send(ControlCommand::Method {
                method,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Closed)?;
        match timeout(self.request_timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => Err(Error::Rtsp("control request timed out".into())),
        }
    }
    pub async fn teardown(mut self) -> Result<()> {
        self.send_control("TEARDOWN").await
    }
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub async fn quality_snapshot(&self) -> ClientQualitySnapshot {
        ClientQualitySnapshot {
            tracks: self
                .quality
                .lock()
                .await
                .iter()
                .map(|(track, session)| (*track, session.snapshot()))
                .collect(),
        }
    }
}
impl Drop for ClientSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

/// The wire the control task drives: either an owned control connection
/// (UDP transport) or the shared interleaved writer/response stream.
enum ControlTransport {
    Owned(ControlConnection),
    Interleaved {
        writer: Arc<Mutex<ControlWriteHalf>>,
        responses: mpsc::Receiver<Response>,
    },
}

pub(super) enum ControlCommand {
    Method {
        method: &'static str,
        reply: oneshot::Sender<Result<()>>,
    },
}

async fn wait_for_instant(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// One RTSP control round trip on the session's transport.
async fn perform_control(
    transport: &mut ControlTransport,
    method: &'static str,
    uri: &str,
    cseq: &mut u32,
    session_id: &str,
    request_timeout: Duration,
    credentials: Option<&Credentials>,
) -> Result<()> {
    let extra = format!("Session: {session_id}\r\n");
    let sequence = *cseq;
    *cseq = cseq.wrapping_add(1);
    match transport {
        ControlTransport::Owned(control) => {
            let response = request(
                control,
                method,
                uri,
                sequence,
                Some(&extra),
                request_timeout,
                credentials,
            )
            .await?;
            if response.status != 200 {
                return Err(status_error(method, response.status));
            }
        }
        ControlTransport::Interleaved { writer, responses } => {
            let mut authorization = credentials.map(basic_authorization);
            let wire = encode_request(
                method,
                uri,
                sequence,
                Some(&extra),
                authorization.as_deref(),
            );
            tokio::time::timeout(request_timeout, async {
                writer.lock().await.write_all(wire.as_bytes()).await
            })
            .await
            .map_err(|_| Error::Limit("interleaved control write timeout"))??;
            // Pair responses by CSeq: a stale response left over from a
            // previous exchange (e.g. a timed-out reconnect) must not be
            // mistaken for the reply to this request.
            let mut response = loop {
                let candidate = timeout(request_timeout, responses.recv())
                    .await
                    .map_err(|_| Error::Rtsp("RTSP response timeout".into()))?
                    .ok_or(Error::Closed)?;
                if candidate_headers_cseq(&candidate) == Some(sequence) {
                    break candidate;
                }
            };
            if response.status == 401
                && let (Some(credentials), Some(challenge)) =
                    (credentials, response.headers.get("www-authenticate"))
            {
                authorization = Some(authorization_from_challenge(
                    challenge,
                    Method::from_str(method)?,
                    uri,
                    credentials,
                )?);
                let wire = encode_request(
                    method,
                    uri,
                    sequence,
                    Some(&extra),
                    authorization.as_deref(),
                );
                tokio::time::timeout(request_timeout, async {
                    writer.lock().await.write_all(wire.as_bytes()).await
                })
                .await
                .map_err(|_| Error::Limit("interleaved control write timeout"))??;
                response = loop {
                    let candidate = timeout(request_timeout, responses.recv())
                        .await
                        .map_err(|_| Error::Rtsp("RTSP response timeout".into()))?
                        .ok_or(Error::Closed)?;
                    if candidate_headers_cseq(&candidate) == Some(sequence) {
                        break candidate;
                    }
                };
            }
            if response.status != 200 {
                return Err(status_error(method, response.status));
            }
        }
    }
    Ok(())
}

/// Dedicated control-plane task: serializes PLAY/PAUSE/TEARDOWN commands and
/// schedules keep-alives, keeping RTSP round trips off the media path
/// (audits RT-7/RT-8). Keep-alive failures back off (R23) and downgrade
/// GET_PARAMETER to OPTIONS when the server does not implement it.
#[allow(clippy::too_many_arguments)]
async fn control_task(
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
                let Some(ControlCommand::Method { method: requested, reply }) = command else {
                    return;
                };
                let result = perform_control(
                    &mut transport,
                    requested,
                    &uri,
                    &mut cseq,
                    &session_id,
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
                            request_timeout,
                            credentials.as_ref(),
                        )
                        .await
                    }
                    other => other,
                };
                match &result {
                    Ok(()) => {
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
                            request_timeout,
                            credentials.as_ref(),
                        )
                        .await
                    }
                    other => other,
                };
                match result {
                    Ok(()) => {
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

pub struct RtspClient;
impl RtspClient {
    pub async fn describe(url: url::Url, options: ClientOptions) -> Result<MediaDescription> {
        validate_options(&options)?;
        let (_, _, _, description) = open_and_describe(&url, &options).await?;
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
            let (stream, uri, setup_base, description) = open_and_describe(&url, &options).await?;
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
        let (stream, uri, setup_base, description) = match udp_open {
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
                let (stream, uri, setup_base, description) =
                    open_and_describe(&url, &tcp_options).await?;
                return setup::connect(stream, uri, setup_base, description, tcp_options, id)
                    .await
                    .map_err(|failure| failure.error);
            }
            Err(error) => return Err(error),
        };
        let id = next_client_session_id();
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
                        let (stream, uri, setup_base, description) =
                            open_and_describe(&url, &tcp_options).await?;
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

fn validate_options(options: &ClientOptions) -> Result<()> {
    if options.output_queue_capacity == 0 {
        return Err(Error::Limit("client output queue capacity"));
    }
    if options.keep_alive_interval == Some(Duration::ZERO) {
        return Err(Error::Limit("client keep-alive interval"));
    }
    if options.request_timeout == Duration::ZERO {
        return Err(Error::Limit("client request timeout"));
    }
    Ok(())
}

async fn open_and_describe(
    url: &url::Url,
    options: &ClientOptions,
) -> Result<(ControlConnection, String, String, MediaDescription)> {
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
    let description = sdp::parse(&response.body)?;
    Ok((stream, uri, setup_base, description))
}

fn status_error(method: &'static str, status: u16) -> Error {
    if status == 401 || status == 403 {
        Error::Unauthorized
    } else {
        Error::RtspStatus { method, status }
    }
}

fn receive_rtp_packet(
    packet: RtpPacket,
    jitter: &mut JitterBuffer,
    depacketizer: &mut Depacketizer,
) -> Vec<Result<AccessUnit>> {
    depacketize_packets(jitter.push(packet), depacketizer)
}

fn drain_jitter(
    jitter: &mut JitterBuffer,
    depacketizer: &mut Depacketizer,
) -> Vec<Result<AccessUnit>> {
    depacketize_packets(jitter.drain_expired(), depacketizer)
}

fn depacketize_packets(
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

fn response_session_id(response: &Response) -> Result<String> {
    response
        .headers
        .get("session")
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::Rtsp("SETUP response has no Session".into()))
}

fn encode_request(
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

async fn request(
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

fn should_fallback_to_tcp(error: &Error) -> bool {
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
fn candidate_headers_cseq(response: &Response) -> Option<u32> {
    response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("cseq"))
        .and_then(|(_, value)| value.parse().ok())
}

fn validate_response_cseq(response: &Response, cseq: u32) -> Result<()> {
    if response.headers.get("cseq").map(String::as_str) != Some(cseq.to_string().as_str()) {
        return Err(Error::Rtsp(
            "RTSP response CSeq does not match request".into(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_request(
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

#[cfg(test)]
mod tests {
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
    fn jittered_backoff_is_deterministic_per_attempt() {
        let delay = Duration::from_millis(800);
        for attempt in 0..32u32 {
            assert_eq!(
                jittered_backoff(delay, attempt),
                jittered_backoff(delay, attempt)
            );
        }
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
}
