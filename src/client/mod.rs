//! RTSP client control-plane primitives.
mod setup;

#[cfg(feature = "tls")]
use crate::security::TlsClientConfig;
use crate::transport::UpstreamTransport;
use crate::{
    Error, Result,
    auth::{Credentials, authorization_from_challenge, basic_authorization},
    media::{AccessUnit, MediaDescription},
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
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{Mutex, mpsc},
    task::JoinHandle,
    time::{Instant, sleep, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
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
        }
    }
}

#[derive(Clone, Debug)]
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
    fn validate(&self) -> Result<()> {
        if self.initial_delay == Duration::ZERO || self.max_delay < self.initial_delay {
            return Err(Error::Limit("client reconnect delay"));
        }
        Ok(())
    }
}

/// Exponential backoff with deterministic jitter (±25%) so that many
/// clients failing at once do not reconnect in a synchronized herd.
fn jittered_backoff(delay: Duration, attempt: u32) -> Duration {
    let sample = (attempt.wrapping_mul(1_103_515_245).wrapping_add(12_345) % 101) as f32 / 100.0;
    delay.mul_f32(0.75 + 0.5 * sample)
}

pub struct ReconnectingClientSession {
    pub description: MediaDescription,
    receiver: mpsc::Receiver<Result<AccessUnit>>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl ReconnectingClientSession {
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

    pub fn build(self) -> Result<ClientOptions> {
        self.options.validate()?;
        Ok(self.options)
    }
}

pub struct ClientSession {
    pub description: MediaDescription,
    receiver: mpsc::Receiver<Result<AccessUnit>>,
    control: Option<ControlConnection>,
    cseq: u32,
    session_id: String,
    uri: String,
    playing: bool,
    interleaved_writer: Option<Arc<Mutex<ControlWriteHalf>>>,
    interleaved_responses: Option<mpsc::Receiver<Response>>,
    credentials: Option<Credentials>,
    request_timeout: Duration,
    keep_alive_interval: Option<Duration>,
    last_control: Instant,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    quality: Arc<Mutex<HashMap<u8, RtcpSession>>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientQualitySnapshot {
    pub tracks: BTreeMap<u8, ReceptionSnapshot>,
}
impl ClientSession {
    #[allow(clippy::too_many_arguments)]
    fn new(
        description: MediaDescription,
        receiver: mpsc::Receiver<Result<AccessUnit>>,
        control: Option<ControlConnection>,
        interleaved_writer: Option<Arc<Mutex<ControlWriteHalf>>>,
        interleaved_responses: Option<mpsc::Receiver<Response>>,
        cseq: u32,
        session_id: String,
        uri: String,
        credentials: Option<Credentials>,
        request_timeout: Duration,
        keep_alive_interval: Option<Duration>,
        cancel: CancellationToken,
        tasks: Vec<JoinHandle<()>>,
        quality: Arc<Mutex<HashMap<u8, RtcpSession>>>,
    ) -> Self {
        Self {
            description,
            receiver,
            control,
            interleaved_writer,
            interleaved_responses,
            cseq,
            session_id,
            uri,
            playing: true,
            credentials,
            request_timeout,
            keep_alive_interval,
            last_control: Instant::now(),
            cancel,
            tasks,
            quality,
        }
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
    pub async fn next(&mut self) -> Option<Result<AccessUnit>> {
        loop {
            let Some(interval) = self.keep_alive_interval else {
                return self.receiver.recv().await;
            };
            let deadline = self.last_control + interval;
            tokio::select! {
                frame = self.receiver.recv() => return frame,
                _ = sleep_until(deadline) => {
                    // The keep-alive round-trip runs inline (this task owns
                    // the control connection), so bound it: a slow control
                    // plane must not stall media consumption for the full
                    // request timeout on every interval.
                    match tokio::time::timeout(
                        self.request_timeout.min(std::time::Duration::from_secs(2)),
                        self.keep_alive(),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => return Some(Err(error)),
                        Err(_) => {
                            return Some(Err(Error::Rtsp(
                                "keep-alive timed out; control plane unreachable".into(),
                            )));
                        }
                    }
                }
            }
        }
    }

    pub async fn keep_alive(&mut self) -> Result<()> {
        self.send_control("GET_PARAMETER").await
    }
    async fn send_control(&mut self, method: &str) -> Result<()> {
        let extra = format!("Session: {}\r\n", self.session_id);
        let cseq = self.cseq;
        self.cseq = self.cseq.wrapping_add(1);
        if let Some(control) = self.control.as_mut() {
            let response = request(
                control,
                method,
                &self.uri,
                cseq,
                Some(&extra),
                self.request_timeout,
                self.credentials.as_ref(),
            )
            .await?;
            if response.status != 200 {
                return Err(Error::Rtsp(format!(
                    "{method} returned {}",
                    response.status
                )));
            }
        } else if let (Some(writer), Some(responses)) = (
            self.interleaved_writer.as_mut(),
            self.interleaved_responses.as_mut(),
        ) {
            let mut authorization = self.credentials.as_ref().map(basic_authorization);
            let wire = encode_request(
                method,
                &self.uri,
                cseq,
                Some(&extra),
                authorization.as_deref(),
            );
            writer.lock().await.write_all(wire.as_bytes()).await?;
            let mut response = timeout(self.request_timeout, responses.recv())
                .await
                .map_err(|_| Error::Rtsp("RTSP response timeout".into()))?
                .ok_or(Error::Closed)?;
            validate_response_cseq(&response, cseq)?;
            if response.status == 401
                && let (Some(credentials), Some(challenge)) = (
                    self.credentials.as_ref(),
                    response.headers.get("www-authenticate"),
                )
            {
                authorization = Some(authorization_from_challenge(
                    challenge,
                    Method::from_str(method)?,
                    &self.uri,
                    credentials,
                )?);
                let wire = encode_request(
                    method,
                    &self.uri,
                    cseq,
                    Some(&extra),
                    authorization.as_deref(),
                );
                writer.lock().await.write_all(wire.as_bytes()).await?;
                response = timeout(self.request_timeout, responses.recv())
                    .await
                    .map_err(|_| Error::Rtsp("RTSP response timeout".into()))?
                    .ok_or(Error::Closed)?;
                validate_response_cseq(&response, cseq)?;
            }
            if response.status != 200 {
                return Err(status_error(method, response.status));
            }
        } else {
            return Err(Error::Closed);
        }
        self.last_control = Instant::now();
        Ok(())
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

pub struct RtspClient;
impl RtspClient {
    pub async fn describe(url: url::Url, options: ClientOptions) -> Result<MediaDescription> {
        validate_options(&options)?;
        let (_, _, _, description) = open_and_describe(&url, &options).await?;
        Ok(description)
    }

    pub async fn connect(url: url::Url, options: ClientOptions) -> Result<ClientSession> {
        validate_options(&options)?;
        if options.transport != UpstreamTransport::Auto {
            let (stream, uri, setup_base, description) = open_and_describe(&url, &options).await?;
            return setup::connect(stream, uri, setup_base, description, options).await;
        }

        let mut udp_options = options.clone();
        udp_options.transport = UpstreamTransport::Udp;
        let udp_open = open_and_describe(&url, &udp_options).await;
        let (stream, uri, setup_base, description) = match udp_open {
            Ok(value) => value,
            Err(error) if should_fallback_to_tcp(&error) => {
                tracing::debug!(error = %error, "UDP RTSP connection failed; retrying with TCP interleaving");
                let mut tcp_options = options.clone();
                tcp_options.transport = UpstreamTransport::TcpInterleaved;
                let (stream, uri, setup_base, description) =
                    open_and_describe(&url, &tcp_options).await?;
                return setup::connect(stream, uri, setup_base, description, tcp_options).await;
            }
            Err(error) => return Err(error),
        };
        match setup::connect(stream, uri, setup_base, description, udp_options).await {
            Ok(session) => Ok(session),
            Err(udp_error) if should_fallback_to_tcp(&udp_error) => {
                tracing::debug!(error = %udp_error, "UDP RTSP SETUP failed; retrying with TCP interleaving");
                let mut tcp_options = options;
                tcp_options.transport = UpstreamTransport::TcpInterleaved;
                let (stream, uri, setup_base, description) =
                    open_and_describe(&url, &tcp_options).await?;
                setup::connect(stream, uri, setup_base, description, tcp_options).await
            }
            Err(error) => Err(error),
        }
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
                            failures = failures.saturating_add(1);
                            if policy.max_attempts != 0 && failures >= policy.max_attempts {
                                let _ = tx.send(Err(error)).await;
                                return;
                            }
                            tracing::warn!(error = %error, delay_ms = delay.as_millis(), "RTSP client reconnect failed");
                            let jittered = jittered_backoff(delay, failures);
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
                            Some(frame) => {
                                if tx.send(frame).await.is_err() {
                                    return;
                                }
                            }
                            None => break,
                        }
                    }
                }
                tracing::warn!("RTSP client session ended; reconnecting");
                let jittered = jittered_backoff(delay, failures);
                tokio::select! {
                    _ = task_cancel.cancelled() => return,
                    _ = sleep(jittered) => {}
                }
                delay = delay.saturating_mul(2).min(policy.max_delay);
            }
        });
        Ok(ReconnectingClientSession {
            description,
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

fn status_error(method: &str, status: u16) -> Error {
    if status == 401 || status == 403 {
        Error::Unauthorized
    } else {
        Error::Rtsp(format!("{method} returned {status}"))
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
    let mut wire = format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\nUser-Agent: rtflow/0.1\r\n");
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
        Error::Rtsp(message) => {
            let lower = message.to_ascii_lowercase();
            lower.contains("461")
                || lower.contains("unsupported transport")
                || lower.contains("udp") && lower.contains("timeout")
        }
        _ => false,
    }
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
