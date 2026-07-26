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
    time::{Instant, sleep_until, timeout},
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
            transport: UpstreamTransport::Udp,
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
                    if let Err(error) = self.keep_alive().await {
                        return Some(Err(error));
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
        let (stream, uri, setup_base, description) = open_and_describe(&url, &options).await?;
        setup::connect(stream, uri, setup_base, description, options).await
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
                Message::Response(response) => Ok(response),
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
