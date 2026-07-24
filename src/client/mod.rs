//! RTSP client control-plane primitives.
mod setup;

use crate::{
    Error, Result,
    auth::{Credentials, authorization_from_challenge, basic_authorization},
    media::{AccessUnit, MediaDescription},
    protocol::sdp,
    proxy::UpstreamTransport,
    rtp::{Depacketizer, JitterBuffer, JitterBufferConfig, RtpPacket},
    rtsp::{self, Message, Method, Response},
};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
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
        }
    }
}

pub struct ClientSession {
    pub description: MediaDescription,
    receiver: mpsc::Receiver<Result<AccessUnit>>,
    control: Option<TcpStream>,
    cseq: u32,
    session_id: String,
    uri: String,
    playing: bool,
    interleaved_writer: Option<Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>>,
    interleaved_responses: Option<mpsc::Receiver<Response>>,
    credentials: Option<Credentials>,
    request_timeout: Duration,
    keep_alive_interval: Option<Duration>,
    last_control: Instant,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}
impl ClientSession {
    #[allow(clippy::too_many_arguments)]
    fn new(
        description: MediaDescription,
        receiver: mpsc::Receiver<Result<AccessUnit>>,
        control: Option<TcpStream>,
        interleaved_writer: Option<Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>>,
        interleaved_responses: Option<mpsc::Receiver<Response>>,
        cseq: u32,
        session_id: String,
        uri: String,
        credentials: Option<Credentials>,
        request_timeout: Duration,
        keep_alive_interval: Option<Duration>,
        cancel: CancellationToken,
        tasks: Vec<JoinHandle<()>>,
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
    Ok(())
}

async fn open_and_describe(
    url: &url::Url,
    options: &ClientOptions,
) -> Result<(TcpStream, String, String, MediaDescription)> {
    let host = url
        .host_str()
        .ok_or_else(|| Error::Rtsp("RTSP URL has no host".into()))?;
    let port = url.port().unwrap_or(554);
    let mut stream = timeout(options.request_timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::Rtsp("RTSP connect timeout".into()))??;
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

fn receive_rtp(
    data: bytes::Bytes,
    jitter: &mut JitterBuffer,
    depacketizer: &mut Depacketizer,
) -> Vec<Result<AccessUnit>> {
    match RtpPacket::parse(data) {
        Ok(packet) => depacketize_packets(jitter.push(packet), depacketizer),
        Err(error) => vec![Err(error)],
    }
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
        .filter_map(|packet| match depacketizer.push(packet) {
            Ok(Some(unit)) => Some(Ok(unit)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
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
    let mut wire =
        format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\nUser-Agent: rtsp-rust/0.1\r\n");
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
    stream: &mut TcpStream,
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
    stream: &mut TcpStream,
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
    let mut buffer = bytes::BytesMut::with_capacity(4096);
    let mut temporary = [0u8; 4096];
    loop {
        if let Some(message) = rtsp::parse(&mut buffer, 16 * 1024, 256 * 1024)? {
            return match message {
                Message::Response(response) => Ok(response),
                _ => Err(Error::Rtsp("server sent request".into())),
            };
        }
        let size = timeout(wait, stream.read(&mut temporary))
            .await
            .map_err(|_| Error::Rtsp("RTSP response timeout".into()))??;
        if size == 0 {
            return Err(Error::Closed);
        }
        buffer.extend_from_slice(&temporary[..size]);
    }
}
