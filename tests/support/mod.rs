#![allow(dead_code)]

use bytes::{Bytes, BytesMut};
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription,
    rtsp::{self as wire_rtsp, Message, Response},
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

pub fn description(name: &str, codecs: &[Codec]) -> MediaDescription {
    MediaDescription {
        session_name: Arc::from(name),
        tracks: codecs
            .iter()
            .copied()
            .enumerate()
            .map(|(index, codec)| track(index as u8, codec))
            .collect(),
        connection: None,
    }
}

pub fn track(id: u8, codec: Codec) -> TrackDescription {
    let (kind, clock_rate, channels, payload_type, fmtp) = match codec {
        Codec::H264 => (
            MediaKind::Video,
            90_000,
            None,
            96,
            Some("packetization-mode=1"),
        ),
        Codec::H265 => (MediaKind::Video, 90_000, None, 98, None),
        Codec::Aac => (
            MediaKind::Audio,
            48_000,
            Some(2),
            97,
            Some(
                "profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1190",
            ),
        ),
        Codec::Pcma => (MediaKind::Audio, 8_000, Some(1), 8, None),
        Codec::Pcmu => (MediaKind::Audio, 8_000, Some(1), 0, None),
        Codec::Opus => (MediaKind::Audio, 48_000, Some(2), 111, None),
        Codec::Vp8 => (MediaKind::Video, 90_000, None, 99, None),
        Codec::Vp9 => (MediaKind::Video, 90_000, None, 100, None),
    };
    TrackDescription {
        id,
        kind,
        codec,
        clock_rate,
        channels,
        payload_type,
        fmtp: fmtp.map(Arc::from),
        control: Arc::from(format!("trackID={id}")),
        crypto: None,
        width: None,
        height: None,
        frame_rate: None,
    }
}

pub fn unit(track_id: u8, codec: Codec, sequence: u32) -> AccessUnit {
    let data = match codec {
        // H.264/H.265 access units are now consistently Annex-B (4-byte start
        // code prefix).  The RTSP sender packetizes them and the remote
        // depacketizer reconstructs Annex-B, so the round-trip preserves the
        // start code.
        Codec::H264 => {
            let mut b = BytesMut::with_capacity(8);
            b.extend_from_slice(&[0, 0, 0, 1]);
            b.extend_from_slice(&[0x65, 0x88, 0x84, 0x21]);
            b.freeze()
        }
        Codec::H265 => {
            let mut b = BytesMut::with_capacity(8);
            b.extend_from_slice(&[0, 0, 0, 1]);
            b.extend_from_slice(&[0x26, 0x01, 0xaf, 0x09]);
            b.freeze()
        }
        Codec::Aac => Bytes::from_static(&[0x21, 0x10, 0x04, 0x60]),
        Codec::Pcma => Bytes::from_static(&[0xd5; 160]),
        Codec::Pcmu => Bytes::from_static(&[0xff; 160]),
        Codec::Opus => Bytes::from_static(&[0xfc; 80]),
        Codec::Vp8 | Codec::Vp9 => Bytes::from_static(&[0x10, 0x01, 0x02, 0x03]),
    };
    AccessUnit {
        track_id,
        codec,
        pts: Duration::from_millis(u64::from(sequence) * 20),
        dts: None,
        rtp_timestamp: sequence.saturating_mul(1_800),
        is_keyframe: true,
        data,
    }
}

pub async fn wait_until<F>(timeout: Duration, mut predicate: F)
where
    F: FnMut() -> bool,
{
    tokio::time::timeout(timeout, async {
        while !predicate() {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("condition was not reached before timeout");
}

pub async fn within<T>(duration: Duration, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(duration, future)
        .await
        .expect("operation timed out")
}

pub struct RawRtsp {
    stream: TcpStream,
    input: BytesMut,
    cseq: u32,
}

impl RawRtsp {
    pub async fn connect(address: std::net::SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(address).await.unwrap(),
            input: BytesMut::new(),
            cseq: 1,
        }
    }

    pub async fn request(&mut self, method: &str, uri: &str, headers: &str) -> Response {
        let cseq = self.cseq;
        self.cseq += 1;
        let wire = format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n{headers}\r\n");
        self.stream.write_all(wire.as_bytes()).await.unwrap();
        let mut buffer = [0u8; 4096];
        loop {
            if let Some(Message::Response(response)) =
                wire_rtsp::parse(&mut self.input, 16 * 1024, 256 * 1024).unwrap()
            {
                return response;
            }
            let count = self.stream.read(&mut buffer).await.unwrap();
            assert!(count > 0, "RTSP connection closed before a response");
            self.input.extend_from_slice(&buffer[..count]);
        }
    }
}

/// Scripted RTSP server counterpart to [`RawRtsp`]: accepts connections on
/// 127.0.0.1, feeds each parsed request to a shared handler, and writes back
/// whatever the handler decides. Used to exercise client behavior against
/// misbehaving servers (wrong statuses, stale CSeq, abrupt closes).
pub enum ServerReply {
    /// Write the response and keep serving on this connection.
    Response(Response),
    /// Write the response, then close the connection.
    ResponseThenClose(Response),
    /// Write arbitrary bytes (e.g. a stale response or interleaved frame).
    Raw(Vec<u8>),
    /// Close the connection without replying.
    Close,
}

pub type RtspServerHandler = Box<dyn FnMut(&wire_rtsp::Request) -> ServerReply + Send>;

pub struct RawRtspServer {
    address: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<wire_rtsp::Request>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl RawRtspServer {
    pub async fn start(handler: RtspServerHandler) -> Self {
        Self::start_on("127.0.0.1:0", handler).await
    }

    pub async fn start_on(bind: &str, handler: RtspServerHandler) -> Self {
        let listener = TcpListener::bind(bind).await.unwrap();
        let address = listener.local_addr().unwrap();
        let handler = Arc::new(Mutex::new(handler));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let accept_handler = handler.clone();
        let accept_requests = requests.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => return,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { return };
                        let handler = accept_handler.clone();
                        let requests = accept_requests.clone();
                        tokio::spawn(async move {
                            let _ = Self::serve_connection(stream, handler, requests).await;
                        });
                    }
                }
            }
        });
        Self {
            address,
            requests,
            shutdown: Some(shutdown_tx),
        }
    }

    async fn serve_connection(
        mut stream: TcpStream,
        handler: Arc<Mutex<RtspServerHandler>>,
        requests: Arc<Mutex<Vec<wire_rtsp::Request>>>,
    ) -> std::io::Result<()> {
        let mut input = bytes::BytesMut::new();
        let mut buffer = [0u8; 4096];
        loop {
            while let Some(Message::Request(request)) =
                wire_rtsp::parse(&mut input, 64 * 1024, 1024 * 1024).unwrap()
            {
                requests.lock().await.push(request.clone());
                let reply = handler.lock().await(&request);
                match reply {
                    ServerReply::Response(response) => {
                        stream
                            .write_all(&wire_rtsp::encode_response(&response))
                            .await?;
                    }
                    ServerReply::ResponseThenClose(response) => {
                        stream
                            .write_all(&wire_rtsp::encode_response(&response))
                            .await?;
                        return Ok(());
                    }
                    ServerReply::Raw(bytes) => {
                        stream.write_all(&bytes).await?;
                    }
                    ServerReply::Close => return Ok(()),
                }
            }
            let count = stream.read(&mut buffer).await?;
            if count == 0 {
                return Ok(());
            }
            input.extend_from_slice(&buffer[..count]);
        }
    }

    pub fn address(&self) -> std::net::SocketAddr {
        self.address
    }

    pub fn url(&self, path: &str) -> url::Url {
        url::Url::parse(&format!("rtsp://{}{path}", self.address)).unwrap()
    }

    pub async fn requests(&self) -> Vec<wire_rtsp::Request> {
        self.requests.lock().await.clone()
    }

    pub async fn method_counts(&self) -> std::collections::BTreeMap<String, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for request in self.requests.lock().await.iter() {
            *counts
                .entry(request.method.as_str().to_owned())
                .or_insert(0) += 1;
        }
        counts
    }
}

impl Drop for RawRtspServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// A response that echoes the request CSeq (required for client pairing).
pub fn reply(request: &wire_rtsp::Request, status: u16, reason: &str) -> Response {
    let mut headers = std::collections::BTreeMap::new();
    if let Some(cseq) = request.headers.get("cseq") {
        headers.insert("cseq".to_owned(), cseq.clone());
    }
    Response {
        status,
        reason: reason.to_owned(),
        headers,
        body: Vec::new(),
    }
}

pub fn with_header(mut response: Response, name: &str, value: &str) -> Response {
    response.headers.insert(name.to_owned(), value.to_owned());
    response
}

/// A DESCRIBE response whose body is the SDP encoding of `description`.
pub fn describe_reply(request: &wire_rtsp::Request, description: &MediaDescription) -> Response {
    let body = rtflow::protocol::sdp::encode(description).into_bytes();
    let response = with_header(reply(request, 200, "OK"), "content-type", "application/sdp");
    Response { body, ..response }
}
