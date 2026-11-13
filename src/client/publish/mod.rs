//! RTSP 1.0 publishing client: ANNOUNCE, SETUP (UDP or interleaved), RECORD.

use super::{
    options::ClientOptions,
    wire::{request, request_with_body},
};
use crate::{
    Error, Result,
    identity::SessionId,
    media::{AccessUnit, MediaDescription, TrackDescription},
    rtp::Packetizer,
    rtsp::sdp,
    runtime::framing::{ConnectionFramer, InboundFrame},
    runtime::io::ControlConnection,
    transport::{TransportSpec, UdpPair},
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, atomic::AtomicU32},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, mpsc},
};
use tokio_util::sync::CancellationToken;

/// Per-track media transport chosen at SETUP.
enum PublishTransport {
    Udp { pair: UdpPair, target: SocketAddr },
    Interleaved { channel: u8 },
}

struct PublishTrack {
    packetizer: Packetizer,
    transport: PublishTransport,
    #[cfg(feature = "srtp")]
    srtp: Option<crate::srtp::SrtpContext>,
}

mod control;
use control::{ControlInner, PublisherControl};

/// A live RTSP publisher. `push` sends one access unit as RTP packets; the
/// control connection is kept alive in a dedicated task.
pub struct PublisherSession {
    pub description: MediaDescription,
    id: SessionId,
    control: Arc<PublisherControl>,
    tracks: HashMap<u8, PublishTrack>,
    cancel: CancellationToken,
    keepalive: Option<tokio::task::JoinHandle<()>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl PublisherSession {
    /// Correlating id for logs/events.
    pub fn session_id(&self) -> SessionId {
        self.id
    }

    /// Publish one access unit. Track id selects the negotiated track; the
    /// caller owns inter-track ordering.
    pub async fn push(&mut self, unit: AccessUnit) -> Result<()> {
        let control = &self.control;
        let Some(track) = self.tracks.get_mut(&unit.track_id) else {
            return Err(Error::Rtsp(format!(
                "no publisher track for id {}",
                unit.track_id
            )));
        };
        let packets = track.packetizer.packetize(&unit)?;
        for packet in packets {
            let encoded = packet.encode();
            #[cfg(feature = "srtp")]
            let encoded = match track.srtp.as_mut() {
                Some(context) => context.protect_rtp(&encoded)?,
                None => encoded,
            };
            match &track.transport {
                PublishTransport::Udp { pair, target } => {
                    pair.rtp.send_to(&encoded, *target).await?;
                }
                PublishTransport::Interleaved { channel } => {
                    control.write_interleaved(*channel, encoded).await?;
                }
            }
        }
        Ok(())
    }

    /// Send TEARDOWN and release sockets/tasks.
    pub async fn teardown(mut self) -> Result<()> {
        if let Some(task) = self.keepalive.take() {
            task.abort();
            let _ = task.await;
        }
        let result = self.control.request("TEARDOWN", None, None).await;
        self.cancel.cancel();
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        result.map(|_| ())
    }
}

impl Drop for PublisherSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.keepalive.take() {
            task.abort();
        }
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

fn publisher_ssrc(track_id: u8) -> u32 {
    0x5055_4221 ^ u32::from(track_id)
}

fn packetizer(track: &TrackDescription) -> Result<Packetizer> {
    Ok(Packetizer::new(
        track.codec,
        track.payload_type,
        publisher_ssrc(track.id),
        1200,
    )?
    .with_dimensions(
        u16::try_from(track.width.unwrap_or(0)).unwrap_or(0),
        u16::try_from(track.height.unwrap_or(0)).unwrap_or(0),
    ))
}

async fn announce(
    stream: &mut ControlConnection,
    uri: &str,
    description: &MediaDescription,
    options: &ClientOptions,
    cseq: &mut u32,
) -> Result<()> {
    let body = sdp::encode(description).into_bytes();
    let headers = format!(
        "Content-Type: application/sdp\r\nContent-Length: {}\r\n{}",
        body.len(),
        options.require_headers().unwrap_or_default()
    );
    let sequence = *cseq;
    *cseq = cseq.wrapping_add(1);
    let response = request_with_body(
        stream,
        "ANNOUNCE",
        uri,
        sequence,
        Some(&headers),
        Some(&body),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    if response.status != 200 {
        return Err(Error::RtspStatus {
            method: "ANNOUNCE",
            status: response.status,
        });
    }
    Ok(())
}

async fn record(
    stream: &mut ControlConnection,
    uri: &str,
    session_id: &str,
    options: &ClientOptions,
    cseq: &mut u32,
) -> Result<()> {
    let mut headers = format!("Session: {session_id}\r\n");
    if let Some(require) = options.require_headers() {
        headers.push_str(&require);
    }
    let sequence = *cseq;
    *cseq = cseq.wrapping_add(1);
    let response = request(
        stream,
        "RECORD",
        uri,
        sequence,
        Some(&headers),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    if response.status != 200 {
        return Err(Error::RtspStatus {
            method: "RECORD",
            status: response.status,
        });
    }
    Ok(())
}

pub(super) async fn connect(
    url: &url::Url,
    description: MediaDescription,
    options: &ClientOptions,
) -> Result<PublisherSession> {
    if options.play_range.is_some() {
        return Err(Error::Rtsp(
            "play_range is not valid for a publisher".into(),
        ));
    }
    let interleaved = match options.transport {
        crate::transport::UpstreamTransport::TcpInterleaved => true,
        crate::transport::UpstreamTransport::Udp | crate::transport::UpstreamTransport::Auto => {
            false
        }
    };
    let (stream, uri) = super::rtsp_client::open_control(url, options).await?;
    let mut stream = stream;
    let mut cseq = 1u32;
    let require = options.require_headers();
    let response = request(
        &mut stream,
        "OPTIONS",
        &uri,
        cseq,
        require.as_deref(),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    cseq = cseq.wrapping_add(1);
    if response.status != 200 {
        return Err(Error::RtspStatus {
            method: "OPTIONS",
            status: response.status,
        });
    }
    announce(&mut stream, &uri, &description, options, &mut cseq).await?;

    let peer_ip = stream.peer_addr().ip();
    let bind_ip: IpAddr = match peer_ip {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let mut session_id: Option<String> = None;
    let mut udp_tracks = Vec::new();
    let mut interleaved_tracks = Vec::new();
    for (index, track) in description.tracks.iter().cloned().enumerate() {
        if matches!(
            track.direction,
            Some(crate::media::SdpDirection::RecvOnly | crate::media::SdpDirection::Inactive)
        ) {
            continue;
        }
        #[cfg(feature = "srtp")]
        let secure = track.crypto.is_some();
        #[cfg(not(feature = "srtp"))]
        let secure = false;
        let profile = if interleaved {
            if secure {
                "RTP/SAVP/TCP"
            } else {
                "RTP/AVP/TCP"
            }
        } else if secure {
            "RTP/SAVP"
        } else {
            "RTP/AVP"
        };
        let setup_uri = sdp::control_uri(&uri, &track.control)?;
        let mut headers = if interleaved {
            let channel = u8::try_from(index * 2)
                .map_err(|_| Error::Limit("too many tracks for interleaved publishing"))?;
            interleaved_tracks.push((track.clone(), channel));
            format!(
                "Transport: {profile};unicast;interleaved={channel}-{};mode=record\r\n",
                channel.saturating_add(1)
            )
        } else {
            let pair = UdpPair::bind(bind_ip, None).await?;
            let port = pair.rtp_port()?;
            udp_tracks.push((track.clone(), pair, port));
            format!(
                "Transport: {profile};unicast;client_port={}-{};mode=record\r\n",
                port,
                port.saturating_add(1)
            )
        };
        if let Some(require) = options.require_headers() {
            headers.push_str(&require);
        }
        if let Some(session) = &session_id {
            headers.push_str(&format!("Session: {session}\r\n"));
        }
        let sequence = cseq;
        cseq = cseq.wrapping_add(1);
        let response = request(
            &mut stream,
            "SETUP",
            &setup_uri,
            sequence,
            Some(&headers),
            options.request_timeout,
            options.credentials.as_ref(),
        )
        .await?;
        if response.status != 200 {
            return Err(Error::RtspStatus {
                method: "SETUP",
                status: response.status,
            });
        }
        let received = response
            .headers
            .get("session")
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Rtsp("SETUP response has no Session".into()))?
            .to_owned();
        if let Some(existing) = &session_id {
            if existing != &received {
                return Err(Error::Rtsp("server changed Session during SETUP".into()));
            }
        } else {
            session_id = Some(received);
        }
        if !interleaved {
            let response_transport = response
                .headers
                .get("transport")
                .ok_or_else(|| Error::Rtsp("SETUP response has no Transport".into()))
                .and_then(|value| TransportSpec::parse(value))?;
            let TransportSpec::Udp {
                server_rtp: Some(server_rtp),
                ..
            } = response_transport
            else {
                return Err(Error::Rtsp(
                    "publisher SETUP did not return a server RTP port".into(),
                ));
            };
            if let Some(last) = udp_tracks.last_mut() {
                last.2 = server_rtp;
            }
        }
    }
    let Some(session_id) = session_id else {
        return Err(Error::Rtsp("SDP has no publishable tracks".into()));
    };
    record(&mut stream, &uri, &session_id, options, &mut cseq).await?;

    let mut tracks = HashMap::new();
    if interleaved {
        let (reader, writer) = tokio::io::split(stream);
        let writer = Mutex::new(writer);
        let (response_tx, response_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let reader_cancel = cancel.clone();
        let reader_task = tokio::spawn(async move {
            let mut framer = ConnectionFramer::new(16 * 1024, 256 * 1024, 65_536);
            let mut scratch = [0u8; 4096];
            let mut reader = reader;
            loop {
                tokio::select! {
                    _ = reader_cancel.cancelled() => return,
                    result = reader.read(&mut scratch) => {
                        match result {
                            Ok(0) | Err(_) => return,
                            Ok(size) => {
                                framer.push(&scratch[..size]);
                                loop {
                                    match framer.next_frame() {
                                        Ok(Some(InboundFrame::Rtsp(
                                            crate::rtsp::Message::Response(response),
                                        ))) => {
                                            if response_tx.send(response).await.is_err() {
                                                return;
                                            }
                                        }
                                        Ok(Some(_)) => continue,
                                        Ok(None) => break,
                                        Err(_) => return,
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        for (track, channel) in interleaved_tracks {
            tracks.insert(
                track.id,
                PublishTrack {
                    packetizer: packetizer(&track)?,
                    transport: PublishTransport::Interleaved { channel },
                    #[cfg(feature = "srtp")]
                    srtp: track
                        .crypto
                        .as_ref()
                        .map(|crypto| crate::srtp::from_sdp_crypto(crypto).map(|(srtp, _)| srtp))
                        .transpose()?,
                },
            );
        }
        let control = Arc::new(PublisherControl {
            inner: ControlInner::Interleaved {
                writer,
                responses: Mutex::new(response_rx),
            },
            cseq: AtomicU32::new(cseq),
            request_timeout: options.request_timeout,
            uri: uri.clone(),
            session_id: session_id.clone(),
            credentials: options.credentials.clone(),
        });
        return finish(
            description,
            control,
            tracks,
            cancel,
            Some(reader_task),
            options,
        );
    }

    // UDP: the server_port values were stored in udp_tracks during SETUP.
    for (track, pair, server_rtp) in udp_tracks {
        tracks.insert(
            track.id,
            PublishTrack {
                packetizer: packetizer(&track)?,
                transport: PublishTransport::Udp {
                    pair,
                    target: SocketAddr::new(peer_ip, server_rtp),
                },
                #[cfg(feature = "srtp")]
                srtp: track
                    .crypto
                    .as_ref()
                    .map(|crypto| crate::srtp::from_sdp_crypto(crypto).map(|(srtp, _)| srtp))
                    .transpose()?,
            },
        );
    }
    let control = Arc::new(PublisherControl {
        inner: ControlInner::Udp(Mutex::new(stream)),
        cseq: AtomicU32::new(cseq),
        request_timeout: options.request_timeout,
        uri: uri.clone(),
        session_id: session_id.clone(),
        credentials: options.credentials.clone(),
    });
    finish(
        description,
        control,
        tracks,
        CancellationToken::new(),
        None,
        options,
    )
}

fn finish(
    description: MediaDescription,
    control: Arc<PublisherControl>,
    tracks: HashMap<u8, PublishTrack>,
    cancel: CancellationToken,
    reader: Option<tokio::task::JoinHandle<()>>,
    options: &ClientOptions,
) -> Result<PublisherSession> {
    if tracks.is_empty() {
        return Err(Error::Rtsp("SDP has no publishable tracks".into()));
    }
    let keepalive = spawn_keepalive(control.clone(), cancel.clone(), options);
    Ok(PublisherSession {
        description,
        id: super::next_client_session_id(),
        control,
        tracks,
        cancel,
        keepalive: Some(keepalive),
        reader,
    })
}

fn spawn_keepalive(
    control: Arc<PublisherControl>,
    cancel: CancellationToken,
    options: &ClientOptions,
) -> tokio::task::JoinHandle<()> {
    let interval = options
        .keep_alive_interval
        .unwrap_or(Duration::from_secs(20));
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    let _ = control.request("OPTIONS", None, None).await;
                }
            }
        }
    })
}
