//! WHEP 探针：向真实服务端（如板子上的 hi-ipc）发起 WebRTC 订阅，统计收到的 RTP 包。
//!
//! 用法：`cargo run --release --features webrtc --example whep_probe -- <url> [秒数]`
//! 例：  `<http://192.168.5.136:3000/whep/cam/main> 15`

use std::sync::Arc;
use std::time::{Duration, Instant};

use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use webrtc::media_stream::track_remote::TrackRemoteEvent;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCPeerConnectionIceEvent, RTCSessionDescription,
};

#[derive(Clone)]
struct Handler {
    candidates: Arc<tokio::sync::Mutex<Vec<String>>>,
    track: Arc<tokio::sync::Mutex<Option<Arc<dyn webrtc::media_stream::track_remote::TrackRemote>>>>,
    gather_done: Arc<tokio::sync::Notify>,
    connected: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let mut line = format!("a={}", init.candidate);
            if let Some(mid) = init.sdp_mid {
                line.push_str(&format!("\r\na=mid:{mid}"));
            }
            self.candidates.lock().await.push(line);
        }
    }
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gather_done.notify_waiters();
        }
    }
    async fn on_track(&self, track: Arc<dyn webrtc::media_stream::track_remote::TrackRemote>) {
        *self.track.lock().await = Some(track);
    }
    async fn on_connection_state_change(
        &self,
        state: webrtc::peer_connection::RTCPeerConnectionState,
    ) {
        if state == webrtc::peer_connection::RTCPeerConnectionState::Connected {
            self.connected.notify_waiters();
        }
    }
}

async fn http_post(addr: (&str, u16), path: &str, body: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
         Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
        addr.0,
        body.len()
    );
    stream.write_all(req.as_bytes()).await.unwrap();
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
    (status, body.to_string())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://192.168.5.136:3000/whep/cam/main".to_string());
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(15);

    let rest = url.strip_prefix("http://").expect("只支持 http://");
    let (hostport, path) = rest.split_once('/').expect("URL 缺少路径");
    let (host, port) = hostport.split_once(':').unwrap_or((hostport, "80"));
    let addr = (host, port.parse::<u16>().unwrap());
    let whep_path = format!("/{path}");

    let mut media_engine = webrtc::peer_connection::MediaEngine::default();
    media_engine.register_default_codecs().unwrap();
    let handler = Handler {
        candidates: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        track: Arc::new(tokio::sync::Mutex::new(None)),
        gather_done: Arc::new(tokio::sync::Notify::new()),
        connected: Arc::new(tokio::sync::Notify::new()),
    };
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::default().build())
        .with_media_engine(media_engine)
        .with_handler(Arc::new(handler.clone()))
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await
        .expect("build peer connection");

    pc.add_transceiver_from_kind(
        RtpCodecKind::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            streams: vec![],
            send_encodings: vec![],
        }),
    )
    .await
    .expect("add transceiver");

    let t0 = Instant::now();
    let offer = pc.create_offer(None).await.expect("create offer");
    pc.set_local_description(offer.clone()).await.expect("set local");
    tokio::time::timeout(Duration::from_secs(5), handler.gather_done.notified())
        .await
        .expect("ICE gathering 超时");

    let mut offer_sdp = offer.sdp.clone();
    if !offer_sdp.contains("a=candidate") {
        let cands = handler.candidates.lock().await;
        for line in cands.iter() {
            offer_sdp.push_str("\r\n");
            offer_sdp.push_str(line);
            offer_sdp.push_str("\r\n");
        }
    }

    let (status, answer) = http_post(addr, &whep_path, &offer_sdp).await;
    println!("WHEP POST -> HTTP {status}");
    if status != 201 {
        println!("answer: {answer}");
        std::process::exit(1);
    }
    let answer_sdp = answer.clone();
    pc.set_remote_description(RTCSessionDescription::answer(answer).unwrap())
        .await
        .expect("set remote");

    // 把 answer 里的 candidate 塞给 ICE agent
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
            let _ = pc.add_ice_candidate(init).await;
        }
    }

    tokio::time::timeout(Duration::from_secs(8), handler.connected.notified())
        .await
        .expect("ICE/DTLS 未连接");
    println!("已连接（ICE+DTLS+SRTP），耗时 {:?}", t0.elapsed());

    let track = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(t) = handler.track.lock().await.clone() {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("远端轨道未建立");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut packets = 0usize;
    let mut bytes = 0usize;
    let mut first_packet: Option<Instant> = None;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), track.poll()).await {
            Ok(Some(TrackRemoteEvent::OnRtpPacket(p))) => {
                if first_packet.is_none() {
                    first_packet = Some(Instant::now());
                    println!("首包到达，自发起起 {:?}", t0.elapsed());
                }
                packets += 1;
                bytes += p.payload.len();
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
    }
    let dur = secs as f64;
    println!(
        "收到 {packets} 个 RTP 包，载荷 {:.1} KB，平均 {:.1} kbps",
        bytes as f64 / 1024.0,
        bytes as f64 * 8.0 / dur / 1000.0
    );
}
