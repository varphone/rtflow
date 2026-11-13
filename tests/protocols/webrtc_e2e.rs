#![cfg(all(feature = "webrtc", feature = "http", feature = "server"))]

//! End-to-end WebRTC (WHEP) integration test: a real peer connection
//! inside the test process receives H.264 RTP from the server over a
//! genuine ICE/DTLS/SRTP session on loopback UDP.

use std::sync::Arc;
use std::time::Duration;

use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow::media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};
use rtflow::mount::{MountConfig, MountRegistry};
use rtflow::webrtc::{WebRtcConfig, create_webrtc_handlers};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::media_stream::{MediaStreamId, MediaStreamTrack, MediaStreamTrackId};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCPeerConnectionIceEvent, RTCSessionDescription,
};

/// Browser-side handler: collects local candidates, records the incoming
/// track, and signals ICE connection state.
#[derive(Clone)]
struct BrowserHandler {
    candidates: Arc<tokio::sync::Mutex<Vec<String>>>,
    track: Arc<tokio::sync::Mutex<Option<Arc<dyn TrackRemote>>>>,
    connected: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for BrowserHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let mut line = format!("a={}", init.candidate);
            if let Some(mid) = init.sdp_mid {
                line.push_str(&format!("\r\na=mid:{mid}"));
            }
            self.candidates.lock().await.push(line);
        }
        if event.candidate.address.is_empty() {
            // Last candidate: nothing to signal, gathering is implicit.
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.connected.notify_waiters();
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        *self.track.lock().await = Some(track);
    }

    async fn on_ice_connection_state_change(
        &self,
        _state: webrtc::peer_connection::RTCIceConnectionState,
    ) {
    }

    async fn on_connection_state_change(
        &self,
        state: webrtc::peer_connection::RTCPeerConnectionState,
    ) {
        // RTCPeerConnectionState::Connected means ICE *and* DTLS are done
        // and SRTP contexts are installed — only then is it safe to push
        // media packets.
        if state == webrtc::peer_connection::RTCPeerConnectionState::Connected {
            self.connected.notify_waiters();
        }
    }
}

/// Simple HTTP POST helper returning (status, headers, body).
async fn http_post(addr: std::net::SocketAddr, path: &str, body: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

#[tokio::test]
async fn whep_streams_h264_rtp_over_real_ice_dtls_srtp() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rtflow::webrtc=debug,webrtc_e2e=debug,rtc::ice=trace,rtc::srtp=trace,rtc::dtls=trace,rtc::peer_connection=trace")
        .with_test_writer()
        .try_init();
    let _ = tracing_log::LogTracer::init();

    // Server: mount + WHEP handler.
    let mounts = Arc::new(MountRegistry::new(10));
    mounts
        .register(
            Arc::from("/live/stream"),
            MountConfig::Publisher {
                description: MediaDescription {
                    session_name: Arc::from("e2e"),
                    tracks: vec![TrackDescription {
                        id: 0,
                        kind: MediaKind::Video,
                        codec: Codec::H264,
                        clock_rate: 90_000,
                        channels: None,
                        payload_type: 96,
                        fmtp: None,
                        control: Arc::from("trackID=0"),
                        crypto: None,
                        width: Some(640),
                        height: Some(360),
                        frame_rate: Some((30, 1)),
                        direction: None,
                        ptime: None,
                        rtcp: None,
                        bandwidth_as: None,
                    }],
                    connection: None,
                },
                allow_replace_after_disconnect: true,
            },
        )
        .unwrap();
    let publisher = mounts
        .get_handle("/live/stream")
        .unwrap()
        .publisher()
        .unwrap();

    let (_whip_handler, whep_handler) =
        create_webrtc_handlers(WebRtcConfig::default(), mounts.clone());
    let mut http = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let addr = http.local_addr();
    http.add_route(Method::Post, "/whep/live/stream", whep_handler)
        .await;
    http.run().await.unwrap();

    // Browser: recvonly H.264 peer connection.
    let mut media_engine = webrtc::peer_connection::MediaEngine::default();
    media_engine.register_default_codecs().unwrap();
    let handler = BrowserHandler {
        candidates: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        track: Arc::new(tokio::sync::Mutex::new(None)),
        connected: Arc::new(tokio::sync::Notify::new()),
    };
    let browser = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::default().build())
        .with_media_engine(media_engine)
        .with_handler(Arc::new(handler.clone()))
        .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
        .build()
        .await
        .unwrap();

    browser
        .add_transceiver_from_kind(
            RtpCodecKind::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                streams: vec![],
                send_encodings: vec![],
            }),
        )
        .await
        .unwrap();

    let offer = browser.create_offer(None).await.unwrap();
    browser.set_local_description(offer.clone()).await.unwrap();
    // Wait for local ICE gathering so the offer carries candidates.
    tokio::time::timeout(Duration::from_secs(5), handler.connected.notified())
        .await
        .expect("browser ICE gathering timeout");
    // The local description may already carry candidates after gathering;
    // only append what is missing (webrtc-rs 0.20 does not always inline
    // them into the SDP until the next set_local_description).
    let mut offer_sdp = offer.sdp.clone();
    if !offer_sdp.contains("a=candidate") {
        let candidates = handler.candidates.lock().await;
        for line in candidates.iter() {
            offer_sdp.push_str("\r\n");
            offer_sdp.push_str(line);
            offer_sdp.push_str("\r\n");
        }
    }

    let (status, _headers, answer) = http_post(addr, "/whep/live/stream", &offer_sdp).await;
    assert_eq!(status, 201, "WHEP POST failed: {answer}");
    assert!(
        answer.contains("a=mid"),
        "answer should carry media sections"
    );

    // Feed the answer's ICE candidates into the browser agent (the
    // server's handler does the same for offer candidates).
    let answer_sdp = answer.clone();
    let answer_desc = RTCSessionDescription::answer(answer).unwrap();
    browser.set_remote_description(answer_desc).await.unwrap();
    // Parse candidates with their media section (same logic as the
    // server-side handler): ICE needs sdp_mid/sdp_mline_index to associate
    // a candidate with its transport.
    let mut section = 0u16;
    let mut mid: Option<String> = None;
    for line in answer_sdp.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("m=") {
            section += 1;
            mid = None;
        } else if let Some(m) = line.strip_prefix("a=mid:") {
            mid = Some(m.trim().to_string());
        } else if let Some(c) = line.strip_prefix("a=candidate:") {
            let init = webrtc::peer_connection::RTCIceCandidateInit {
                candidate: format!("candidate:{c}"),
                sdp_mid: mid.clone(),
                sdp_mline_index: Some(section),
                username_fragment: None,
                url: None,
            };
            if let Err(e) = browser.add_ice_candidate(init).await {
                tracing::debug!("browser add_ice_candidate failed: {e}");
            }
        }
    }

    // Wait for the browser-side peer connection (ICE + DTLS complete) so
    // SRTP contexts are installed before the first media packet is sent.
    tokio::time::timeout(Duration::from_secs(5), handler.connected.notified())
        .await
        .expect("browser ICE never connected");

    // The remote track opens when the first RTP packet arrives, so start
    // publishing before waiting for it.

    // Publish a few H.264 access units from the mount side; the WHEP play
    // task packetizes and sends them over the real transport.
    let frame: Vec<u8> = {
        // Annex-B: SPS + PPS + IDR slice (minimal but structurally valid).
        let mut v = Vec::new();
        v.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x0a, 0xe9, 0x40]);
        v.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x38, 0x80]);
        v.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x00, 0x00]);
        v
    };
    for i in 0..30u32 {
        publisher
            .push(AccessUnit {
                track_id: 0,
                codec: Codec::H264,
                pts: Duration::from_millis(u64::from(i) * 33),
                dts: None,
                rtp_timestamp: i * 3000,
                is_keyframe: i == 0,
                data: frame.clone().into(),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Wait for the remote track (opens on first RTP packet), then collect
    // at least three packets.
    let track = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(track) = handler.track.lock().await.clone() {
                return track;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("remote track never arrived");
    let received = tokio::time::timeout(Duration::from_secs(5), async {
        let mut received = 0usize;
        loop {
            match track.poll().await {
                Some(TrackRemoteEvent::OnRtpPacket(_packet)) => {
                    received += 1;
                    if received >= 3 {
                        return received;
                    }
                }
                Some(_) => {}
                None => return received,
            }
        }
    })
    .await
    .unwrap_or(0);
    assert!(
        received >= 3,
        "browser received {received} RTP packets (expected >= 3)"
    );

    browser.close().await.unwrap();
}

#[tokio::test]
async fn whip_receives_h264_rtp_over_real_ice_dtls_srtp() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rtflow::webrtc=debug,webrtc_e2e=debug,rtc::peer_connection::handler::srtp=debug,rtc::peer_connection::handler::endpoint=debug,rtc::peer_connection::handler::ice=debug")
        .with_test_writer()
        .try_init();
    let _ = tracing_log::LogTracer::init();

    // Server: publisher mount + WHIP handler.
    let mounts = Arc::new(MountRegistry::new(10));
    mounts
        .register(
            Arc::from("/live/cam"),
            MountConfig::Publisher {
                description: MediaDescription {
                    session_name: Arc::from("e2e-whip"),
                    tracks: vec![TrackDescription {
                        id: 0,
                        kind: MediaKind::Video,
                        codec: Codec::H264,
                        clock_rate: 90_000,
                        channels: None,
                        payload_type: 96,
                        fmtp: None,
                        control: Arc::from("trackID=0"),
                        crypto: None,
                        width: Some(640),
                        height: Some(360),
                        frame_rate: Some((30, 1)),
                        direction: None,
                        ptime: None,
                        rtcp: None,
                        bandwidth_as: None,
                    }],
                    connection: None,
                },
                allow_replace_after_disconnect: true,
            },
        )
        .unwrap();
    let mount = mounts.get_handle("/live/cam").unwrap();
    let mut frames_rx = mount.subscribe();

    let (whip_handler, _whep_handler) =
        create_webrtc_handlers(WebRtcConfig::default(), mounts.clone());
    let mut http = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let addr = http.local_addr();
    http.add_route(Method::Post, "/whip/live/cam", whip_handler)
        .await;
    http.run().await.unwrap();

    // Browser: sendonly H.264 peer connection.
    let mut media_engine = webrtc::peer_connection::MediaEngine::default();
    media_engine.register_default_codecs().unwrap();
    let handler = BrowserHandler {
        candidates: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        track: Arc::new(tokio::sync::Mutex::new(None)),
        connected: Arc::new(tokio::sync::Notify::new()),
    };
    let browser = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::default().build())
        .with_media_engine(media_engine)
        .with_handler(Arc::new(handler.clone()))
        .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
        .build()
        .await
        .unwrap();

    // Send-only track (explicit SSRC so the server can match it).
    let send_ssrc: u32 = 0x5e11d5e;
    let track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
        MediaStreamId::from("browser-cam".to_string()),
        MediaStreamTrackId::from("video-0".to_string()),
        "browser video".to_string(),
        RtpCodecKind::Video,
        vec![rtc::rtp_transceiver::rtp_sender::RTCRtpEncodingParameters {
            rtp_coding_parameters: rtc::rtp_transceiver::rtp_sender::RTCRtpCodingParameters {
                ssrc: Some(send_ssrc),
                ..Default::default()
            },
            active: true,
            codec: rtc::rtp_transceiver::rtp_sender::RTCRtpCodec {
                mime_type: "video/H264".to_string(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: String::new(),
                rtcp_feedback: vec![],
            },
            ..Default::default()
        }],
    )));
    let sender = browser.add_track(track.clone()).await.unwrap();

    let offer = browser.create_offer(None).await.unwrap();
    browser.set_local_description(offer.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), handler.connected.notified())
        .await
        .expect("browser ICE gathering timeout");
    let mut offer_sdp = offer.sdp.clone();
    if !offer_sdp.contains("a=candidate") {
        let candidates = handler.candidates.lock().await;
        for line in candidates.iter() {
            offer_sdp.push_str("\r\n");
            offer_sdp.push_str(line);
            offer_sdp.push_str("\r\n");
        }
    }

    let (status, _headers, answer) = http_post(addr, "/whip/live/cam", &offer_sdp).await;
    assert_eq!(status, 201, "WHIP POST failed: {answer}");

    let answer_sdp = answer.clone();
    let answer_desc = RTCSessionDescription::answer(answer).unwrap();
    browser.set_remote_description(answer_desc).await.unwrap();
    let mut section = 0u16;
    let mut mid: Option<String> = None;
    for line in answer_sdp.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("m=") {
            section += 1;
            mid = None;
        } else if let Some(m) = line.strip_prefix("a=mid:") {
            mid = Some(m.trim().to_string());
        } else if let Some(c) = line.strip_prefix("a=candidate:") {
            let init = webrtc::peer_connection::RTCIceCandidateInit {
                candidate: format!("candidate:{c}"),
                sdp_mid: mid.clone(),
                sdp_mline_index: Some(section),
                username_fragment: None,
                url: None,
            };
            if let Err(e) = browser.add_ice_candidate(init).await {
                tracing::debug!("browser add_ice_candidate failed: {e}");
            }
        }
    }

    // Wait for ICE+DTLS, then send H.264 RTP packets.
    tokio::time::timeout(Duration::from_secs(5), handler.connected.notified())
        .await
        .expect("browser PC never connected");

    let frame: Vec<u8> = {
        let mut v = Vec::new();
        v.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x0a, 0xe9, 0x40]);
        v.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x38, 0x80]);
        v.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x00, 0x00]);
        v
    };
    // The negotiated payload type comes from the sender parameters (the
    // media engine remaps PTs during negotiation).
    let send_params = sender.get_parameters().await.unwrap();
    let send_pt = send_params
        .rtp_parameters
        .codecs
        .iter()
        .find(|c| c.rtp_codec.mime_type.to_ascii_uppercase().contains("H264"))
        .map(|c| c.payload_type)
        .unwrap_or(96);
    let mut packetizer =
        rtflow::rtp::Packetizer::new(Codec::H264, send_pt, send_ssrc, 1200).unwrap();
    for i in 0..20u32 {
        let units = packetizer
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::H264,
                pts: Duration::from_millis(u64::from(i) * 33),
                dts: None,
                rtp_timestamp: i * 3000,
                is_keyframe: i == 0,
                data: frame.clone().into(),
            })
            .unwrap();
        for p in units {
            let rtp = rtc::rtp::packet::Packet {
                header: rtc::rtp::Header {
                    version: 2,
                    marker: p.header.marker,
                    payload_type: p.header.payload_type,
                    sequence_number: p.header.sequence,
                    timestamp: p.header.timestamp,
                    ssrc: p.header.ssrc,
                    ..Default::default()
                },
                payload: p.payload,
            };
            if track.write_rtp(rtp).await.is_err() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The server must depacketize and push at least one frame to the mount.
    let received = tokio::time::timeout(Duration::from_secs(5), async {
        let mut frames = 0usize;
        loop {
            match frames_rx.recv().await {
                Ok(unit) => {
                    frames += 1;
                    if unit.codec == Codec::H264 && frames >= 3 {
                        return frames;
                    }
                }
                Err(_) => return frames,
            }
        }
    })
    .await
    .unwrap_or(0);
    assert!(
        received >= 3,
        "server received {received} H.264 frames (expected >= 3)"
    );

    let _ = sender;
    browser.close().await.unwrap();
}

#[tokio::test]
async fn whep_streams_pcma_audio_over_real_ice_dtls_srtp() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rtflow::webrtc=debug,webrtc_e2e=debug,rtc=warn")
        .with_test_writer()
        .try_init();
    let _ = tracing_log::LogTracer::init();

    // Server: mount with a G.711 A-law audio track only.
    let mounts = Arc::new(MountRegistry::new(10));
    mounts
        .register(
            Arc::from("/live/audio"),
            MountConfig::Publisher {
                description: MediaDescription {
                    session_name: Arc::from("e2e-pcma"),
                    tracks: vec![TrackDescription {
                        id: 0,
                        kind: MediaKind::Audio,
                        codec: Codec::Pcma,
                        clock_rate: 8_000,
                        channels: Some(1),
                        payload_type: 8,
                        fmtp: None,
                        control: Arc::from("trackID=0"),
                        crypto: None,
                        width: None,
                        height: None,
                        frame_rate: None,
                        direction: None,
                        ptime: None,
                        rtcp: None,
                        bandwidth_as: None,
                    }],
                    connection: None,
                },
                allow_replace_after_disconnect: true,
            },
        )
        .unwrap();
    let publisher = mounts
        .get_handle("/live/audio")
        .unwrap()
        .publisher()
        .unwrap();

    let (_whip_handler, whep_handler) =
        create_webrtc_handlers(WebRtcConfig::default(), mounts.clone());
    let mut http = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let addr = http.local_addr();
    http.add_route(Method::Post, "/whep/live/audio", whep_handler)
        .await;
    http.run().await.unwrap();

    // Browser: audio-only recvonly PC (PCMA is registered by the default
    // media engine).
    let mut media_engine = webrtc::peer_connection::MediaEngine::default();
    media_engine.register_default_codecs().unwrap();
    let handler = BrowserHandler {
        candidates: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        track: Arc::new(tokio::sync::Mutex::new(None)),
        connected: Arc::new(tokio::sync::Notify::new()),
    };
    let browser = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::default().build())
        .with_media_engine(media_engine)
        .with_handler(Arc::new(handler.clone()))
        .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
        .build()
        .await
        .unwrap();
    browser
        .add_transceiver_from_kind(
            RtpCodecKind::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                streams: vec![],
                send_encodings: vec![],
            }),
        )
        .await
        .unwrap();

    let offer = browser.create_offer(None).await.unwrap();
    browser.set_local_description(offer.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), handler.connected.notified())
        .await
        .expect("browser ICE gathering timeout");
    let mut offer_sdp = offer.sdp.clone();
    if !offer_sdp.contains("a=candidate") {
        let candidates = handler.candidates.lock().await;
        for line in candidates.iter() {
            offer_sdp.push_str("\r\n");
            offer_sdp.push_str(line);
            offer_sdp.push_str("\r\n");
        }
    }

    let (status, _headers, answer) = http_post(addr, "/whep/live/audio", &offer_sdp).await;
    assert_eq!(status, 201, "WHEP POST failed: {answer}");
    assert!(
        answer.to_ascii_lowercase().contains("pcma"),
        "answer must negotiate PCMA audio: {answer}"
    );

    let answer_sdp = answer.clone();
    let answer_desc = RTCSessionDescription::answer(answer).unwrap();
    browser.set_remote_description(answer_desc).await.unwrap();
    let mut section = 0u16;
    let mut mid: Option<String> = None;
    for line in answer_sdp.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("m=") {
            section += 1;
            mid = None;
        } else if let Some(m) = line.strip_prefix("a=mid:") {
            mid = Some(m.trim().to_string());
        } else if let Some(c) = line.strip_prefix("a=candidate:") {
            let init = webrtc::peer_connection::RTCIceCandidateInit {
                candidate: format!("candidate:{c}"),
                sdp_mid: mid.clone(),
                sdp_mline_index: Some(section),
                username_fragment: None,
                url: None,
            };
            if let Err(e) = browser.add_ice_candidate(init).await {
                tracing::debug!("browser add_ice_candidate failed: {e}");
            }
        }
    }

    // Wait for ICE+DTLS, then push G.711 frames (160 bytes = 20 ms @ 8 kHz).
    tokio::time::timeout(Duration::from_secs(5), handler.connected.notified())
        .await
        .expect("browser PC never connected");

    let alaw_frame = vec![0xd5u8; 160]; // ~silence in A-law
    for i in 0..40u32 {
        publisher
            .push(AccessUnit {
                track_id: 0,
                codec: Codec::Pcma,
                pts: Duration::from_millis(u64::from(i) * 20),
                dts: None,
                rtp_timestamp: i * 160,
                is_keyframe: true,
                data: alaw_frame.clone().into(),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The browser must receive G.711 RTP packets on the audio track.
    let track = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(track) = handler.track.lock().await.clone() {
                return track;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("remote audio track never arrived");

    let received = tokio::time::timeout(Duration::from_secs(5), async {
        let mut received = 0usize;
        loop {
            match track.poll().await {
                Some(TrackRemoteEvent::OnRtpPacket(_)) => {
                    received += 1;
                    if received >= 3 {
                        return received;
                    }
                }
                Some(_) => {}
                None => return received,
            }
        }
    })
    .await
    .unwrap_or(0);
    assert!(
        received >= 3,
        "browser received {received} PCMA RTP packets (expected >= 3)"
    );

    browser.close().await.unwrap();
}
