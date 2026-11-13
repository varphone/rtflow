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
    runtime::io::ControlConnection,
    transport::{TransportSpec, UdpPair},
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct PublishTrack {
    pair: UdpPair,
    packetizer: Packetizer,
    target: SocketAddr,
}

/// A live RTSP publisher. `push` sends one access unit as RTP packets; the
/// control connection is kept alive in a dedicated task.
pub struct PublisherSession {
    pub description: MediaDescription,
    id: SessionId,
    stream: Arc<Mutex<ControlConnection>>,
    uri: String,
    session_id: String,
    cseq: Arc<AtomicU32>,
    request_timeout: Duration,
    credentials: Option<crate::auth::Credentials>,
    tracks: HashMap<u8, PublishTrack>,
    cancel: CancellationToken,
    keepalive: Option<tokio::task::JoinHandle<()>>,
}

impl PublisherSession {
    /// Correlating id for logs/events.
    pub fn session_id(&self) -> SessionId {
        self.id
    }

    /// Publish one access unit. Track id selects the negotiated track; the
    /// caller owns inter-track ordering.
    pub async fn push(&mut self, unit: AccessUnit) -> Result<()> {
        let Some(track) = self.tracks.get_mut(&unit.track_id) else {
            return Err(Error::Rtsp(format!(
                "no publisher track for id {}",
                unit.track_id
            )));
        };
        let packets = track.packetizer.packetize(&unit)?;
        for packet in packets {
            track
                .pair
                .rtp
                .send_to(&packet.encode(), track.target)
                .await?;
        }
        Ok(())
    }

    /// Send TEARDOWN and release sockets.
    pub async fn teardown(mut self) -> Result<()> {
        self.cancel.cancel();
        if let Some(task) = self.keepalive.take() {
            let _ = task.await;
        }
        let extra = format!("Session: {}\r\n", self.session_id);
        let sequence = self.next_cseq();
        let mut stream = self.stream.lock().await;
        let response = request(
            &mut stream,
            "TEARDOWN",
            &self.uri,
            sequence,
            Some(&extra),
            self.request_timeout,
            self.credentials.as_ref(),
        )
        .await?;
        if response.status != 200 {
            return Err(crate::Error::RtspStatus {
                method: "TEARDOWN",
                status: response.status,
            });
        }
        Ok(())
    }

    fn next_cseq(&self) -> u32 {
        self.cseq.fetch_add(1, Ordering::Relaxed)
    }
}

impl Drop for PublisherSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.keepalive.take() {
            task.abort();
        }
    }
}

struct SetupTrack {
    track: TrackDescription,
    pair: UdpPair,
    target: SocketAddr,
    packetizer: Packetizer,
}

pub(super) async fn connect(
    url: &url::Url,
    description: MediaDescription,
    options: &ClientOptions,
) -> Result<PublisherSession> {
    if !matches!(
        options.transport,
        crate::transport::UpstreamTransport::Udp | crate::transport::UpstreamTransport::Auto
    ) {
        return Err(Error::Rtsp(
            "publishing currently requires UDP transport".into(),
        ));
    }
    if options.play_range.is_some() {
        return Err(Error::Rtsp(
            "play_range is not valid for a publisher".into(),
        ));
    }
    let (stream, uri) = super::rtsp_client::open_control(url, options).await?;
    let mut stream = stream;
    let mut cseq = 1u32;
    let mut next = || {
        let value = cseq;
        cseq = cseq.wrapping_add(1);
        value
    };

    let response = request(
        &mut stream,
        "OPTIONS",
        &uri,
        next(),
        None,
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    if response.status != 200 {
        return Err(crate::Error::RtspStatus {
            method: "OPTIONS",
            status: response.status,
        });
    }

    let body = sdp::encode(&description).into_bytes();
    let announce_headers = format!(
        "Content-Type: application/sdp\r\nContent-Length: {}\r\n",
        body.len()
    );
    let response = request_with_body(
        &mut stream,
        "ANNOUNCE",
        &uri,
        next(),
        Some(&announce_headers),
        Some(&body),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    if response.status != 200 {
        return Err(crate::Error::RtspStatus {
            method: "ANNOUNCE",
            status: response.status,
        });
    }

    let peer_ip = stream.peer_addr().ip();
    let bind_ip: IpAddr = match peer_ip {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    let mut setup_tracks = Vec::new();
    let mut session_id: Option<String> = None;
    let mut selected = None;
    for track in description.tracks.iter().cloned() {
        if matches!(
            track.direction,
            Some(crate::media::SdpDirection::RecvOnly | crate::media::SdpDirection::Inactive)
        ) {
            continue;
        }
        #[cfg(feature = "srtp")]
        if track.crypto.is_some() {
            return Err(Error::Rtsp("publishing SRTP is not implemented yet".into()));
        }
        let pair = UdpPair::bind(bind_ip, None).await?;
        let port = pair.rtp_port()?;
        let setup_uri = sdp::control_uri(&uri, &track.control)?;
        let mut headers = format!(
            "Transport: RTP/AVP;unicast;client_port={}-{};mode=record\r\n",
            port,
            port.saturating_add(1)
        );
        if let Some(session) = &session_id {
            headers.push_str(&format!("Session: {session}\r\n"));
        }
        let response = request(
            &mut stream,
            "SETUP",
            &setup_uri,
            next(),
            Some(&headers),
            options.request_timeout,
            options.credentials.as_ref(),
        )
        .await?;
        if response.status != 200 {
            return Err(crate::Error::RtspStatus {
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
        let spec = response
            .headers
            .get("transport")
            .ok_or_else(|| Error::Rtsp("SETUP response has no Transport".into()))
            .and_then(|value| TransportSpec::parse(value))?;
        let TransportSpec::Udp {
            server_rtp: Some(server_rtp),
            ..
        } = spec
        else {
            return Err(Error::Rtsp(
                "publisher SETUP did not return a server RTP port".into(),
            ));
        };
        let target = SocketAddr::new(peer_ip, server_rtp);
        let packetizer = Packetizer::new(
            track.codec,
            track.payload_type,
            publisher_ssrc(track.id),
            1200,
        )?
        .with_dimensions(
            u16::try_from(track.width.unwrap_or(0)).unwrap_or(0),
            u16::try_from(track.height.unwrap_or(0)).unwrap_or(0),
        );
        selected = Some(track.id);
        setup_tracks.push(SetupTrack {
            track,
            pair,
            target,
            packetizer,
        });
    }
    let Some(session_id) = session_id else {
        return Err(Error::Rtsp("SDP has no publishable tracks".into()));
    };
    if selected.is_none() {
        return Err(Error::Rtsp("SDP has no publishable tracks".into()));
    }
    let record_headers = format!("Session: {session_id}\r\n");
    let response = request(
        &mut stream,
        "RECORD",
        &uri,
        next(),
        Some(&record_headers),
        options.request_timeout,
        options.credentials.as_ref(),
    )
    .await?;
    if response.status != 200 {
        return Err(crate::Error::RtspStatus {
            method: "RECORD",
            status: response.status,
        });
    }

    let track_count = setup_tracks.len();
    let mut tracks = HashMap::with_capacity(track_count);
    for setup in setup_tracks {
        let _ = setup.track;
        tracks.insert(
            setup.track.id,
            PublishTrack {
                pair: setup.pair,
                packetizer: setup.packetizer,
                target: setup.target,
            },
        );
    }
    let stream = Arc::new(Mutex::new(stream));
    let cseq = Arc::new(AtomicU32::new(cseq));
    let cancel = CancellationToken::new();
    let id = super::next_client_session_id();
    let keepalive = spawn_keepalive(
        stream.clone(),
        uri.clone(),
        session_id.clone(),
        cseq.clone(),
        options,
        cancel.clone(),
    );
    Ok(PublisherSession {
        description,
        id,
        stream,
        uri,
        session_id,
        cseq,
        request_timeout: options.request_timeout,
        credentials: options.credentials.clone(),
        tracks,
        cancel,
        keepalive: Some(keepalive),
    })
}

fn publisher_ssrc(track_id: u8) -> u32 {
    0x5055_4221 ^ u32::from(track_id)
}

fn spawn_keepalive(
    stream: Arc<Mutex<ControlConnection>>,
    uri: String,
    session_id: String,
    cseq: Arc<AtomicU32>,
    options: &ClientOptions,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let interval = options
        .keep_alive_interval
        .unwrap_or(Duration::from_secs(20));
    let timeout = options.request_timeout;
    let credentials = options.credentials.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    let sequence = cseq.fetch_add(1, Ordering::Relaxed);
                    let headers = format!("Session: {session_id}\r\n");
                    let mut stream = stream.lock().await;
                    let _ = request(
                        &mut stream,
                        "OPTIONS",
                        &uri,
                        sequence,
                        Some(&headers),
                        timeout,
                        credentials.as_ref(),
                    )
                    .await;
                }
            }
        }
    })
}
