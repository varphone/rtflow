//! SRT caller (client) for pulling MPEG-TS from a remote listener.
//!
//! Implements the HSv5 initiator role: INDUCTION → CONCLUSION with HSREQ
//! (and optional KMREQ), then receives data while sending ACK/NAK/keepalive
//! exactly like the listener-side receiver.
use crate::crypto::{KeyLength, SessionCrypto};
use crate::handshake::{
    Handshake, HandshakeType, HsExtFields, HsExtension, HsFlags, SRT_MAGIC, SRT_VERSION,
    SRT_VERSION_4, SRT_VERSION_5, encode_handshake, parse_handshake,
};
use crate::packet::{
    AckFields, ControlPacket, ControlType, SrtPacket, ack_packet, encode_control_packet,
    keepalive_packet, nak_packet, parse_packet_borrowed, shutdown_packet,
};
use crate::tsbpd::TsbpdBuffer;
use rtflow_core::error::{Error, Result};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

/// Caller configuration.
#[derive(Clone, Debug)]
pub struct SrtCallerConfig {
    /// Remote listener address.
    pub remote: SocketAddr,
    /// Optional Stream ID (`live/cam` or `#!::r=...`).
    pub stream_id: Option<String>,
    /// Receiver TSBPD latency; the effective value is `max(this, peer send)`.
    pub latency_ms: u32,
    /// Optional HaiCrypt passphrase.
    pub passphrase: Option<String>,
    /// AES key length (0 → 16).
    pub pbkeylen: u32,
    /// Handshake deadline.
    pub connect_timeout: Duration,
    /// Disconnect after this long without any packet.
    pub peer_idle_timeout: Duration,
    pub mtu: u32,
    pub flow_window: u32,
    pub tlpktdrop: bool,
    pub tsbpd_max_buffered: usize,
    /// SRT payload per data packet for the sender role (default 1316 = 7×188).
    pub payload_size: usize,
    /// Sender key refresh interval in data packets (0 disables rekeying).
    pub key_refresh_packets: u32,
}

impl Default for SrtCallerConfig {
    fn default() -> Self {
        Self {
            remote: "127.0.0.1:9000".parse().expect("valid default"),
            stream_id: None,
            latency_ms: 120,
            passphrase: None,
            pbkeylen: 0,
            connect_timeout: Duration::from_secs(3),
            peer_idle_timeout: Duration::from_secs(30),
            mtu: 1500,
            flow_window: 8192,
            tlpktdrop: true,
            tsbpd_max_buffered: crate::tsbpd::DEFAULT_MAX_BUFFERED,
            payload_size: 1316,
            key_refresh_packets: 0,
        }
    }
}

/// Connected SRT caller.
pub struct SrtCaller {
    socket: UdpSocket,
    remote: SocketAddr,
    local_id: u32,
    peer_id: u32,
    tsbpd: TsbpdBuffer,
    crypto: Option<SessionCrypto>,
    ack_seq: u32,
    last_ack_sent: Option<(u32, Instant)>,
    last_ack_time: Instant,
    last_nak_time: Instant,
    last_nak_losses: Vec<(u32, u32)>,
    pkts_since_ack: u32,
    packets_received: u64,
    bytes_received: u64,
    rtt_us: u32,
    rtt_var_us: u32,
    last_recv: Instant,
    last_sent: Instant,
    started: Instant,
    closed: bool,
    latency_ms: u32,
    peer_idle_timeout: Duration,
    last_skipped: u64,
    stream_id: Option<String>,
}

/// Result of the HSv5 caller handshake, shared by the receiver (caller) and
/// the sender.
pub(crate) struct HandshakeOutcome {
    pub(crate) socket: UdpSocket,
    pub(crate) remote: SocketAddr,
    pub(crate) local_id: u32,
    pub(crate) peer_id: u32,
    pub(crate) crypto: Option<SessionCrypto>,
    pub(crate) latency_ms: u32,
    pub(crate) timestamp: u32,
    pub(crate) initial_sequence: u32,
    pub(crate) stream_id: Option<String>,
}

/// Perform the HSv5 caller handshake (induction + conclusion).
pub(crate) async fn connect_handshake(config: &SrtCallerConfig) -> Result<HandshakeOutcome> {
    let bind: SocketAddr = if config.remote.is_ipv4() {
        "0.0.0.0:0".parse().expect("valid")
    } else {
        "[::]:0".parse().expect("valid")
    };
    let socket = UdpSocket::bind(bind)
        .await
        .map_err(|error| Error::Srt(format!("caller bind: {error}")))?;
    let local_id = random_id();
    let deadline = Instant::now() + config.connect_timeout;

    // --- INDUCTION -> response -------------------------------------
    let induction = Handshake {
        version: SRT_VERSION_4,
        encryption: 0,
        extension_field: 2,
        initial_sequence: 0,
        mtu: config.mtu,
        flow_window: config.flow_window,
        handshake_type: HandshakeType::Induction,
        socket_id: local_id,
        cookie: 0,
        peer_ip: [0u8; 16],
        extensions: Vec::new(),
    };
    let packet = control(&induction, 0, 0);
    let mut buffer = vec![0u8; 2048];
    let response = 'induction: loop {
        send_control(&socket, &packet, config.remote).await?;
        let retry_deadline = (Instant::now() + Duration::from_millis(250)).min(deadline);
        loop {
            if Instant::now() >= deadline {
                return Err(Error::Srt("SRT caller: handshake timeout".into()));
            }
            match recv_control(&socket, &mut buffer, retry_deadline).await? {
                Some((ControlType::Handshake, data, _)) => {
                    if let Some(handshake) = parse_handshake(&data)
                        && handshake.handshake_type == HandshakeType::Induction
                        && handshake.version == SRT_VERSION_5
                        && handshake.extension_field == SRT_MAGIC
                    {
                        break 'induction handshake;
                    }
                }
                Some(_) => continue,
                None => break, // retransmit the induction
            }
        }
    };
    let cookie = response.cookie;
    debug!(remote = %config.remote, cookie, "SRT caller: induction complete");

    // --- CONCLUSION request ----------------------------------------
    let mut extensions = vec![HsExtension::HsReq(HsExtFields {
        srt_version: SRT_VERSION,
        flags: HsFlags::live_defaults(),
        recv_latency_ms: config.latency_ms.min(u32::from(u16::MAX)) as u16,
        send_latency_ms: 0,
    })];
    if let Some(stream_id) = &config.stream_id {
        extensions.push(HsExtension::StreamId(stream_id.clone()));
    }
    let mut crypto = None;
    let mut kmreq = None;
    if let Some(passphrase) = &config.passphrase {
        let key_len = match config.pbkeylen {
            24 => KeyLength::Aes192,
            32 => KeyLength::Aes256,
            _ => KeyLength::Aes128,
        };
        let (context, message) = SessionCrypto::generate_initiator(passphrase.as_bytes(), key_len)
            .map_err(|error| Error::Srt(format!("SRT caller crypto: {error}")))?;
        crypto = Some(context);
        kmreq = Some(message.clone());
        extensions.push(HsExtension::KmReq(message));
    }
    let conclusion = Handshake {
        version: SRT_VERSION_5,
        encryption: match config.pbkeylen {
            24 => 3,
            32 => 4,
            _ if config.passphrase.is_some() => 2,
            _ => 0,
        },
        extension_field: 0,
        initial_sequence: random_id() & 0x7FFF_FFFF,
        mtu: config.mtu,
        flow_window: config.flow_window,
        handshake_type: HandshakeType::Conclusion,
        socket_id: local_id,
        cookie,
        peer_ip: [0u8; 16],
        extensions,
    };
    let packet = control(&conclusion, 0, 1000);
    let (peer_id, latency_ms, timestamp) = 'conclusion: loop {
        send_control(&socket, &packet, config.remote).await?;
        let retry_deadline = (Instant::now() + Duration::from_millis(250)).min(deadline);
        loop {
            if Instant::now() >= deadline {
                return Err(Error::Srt("SRT caller: conclusion timeout".into()));
            }
            match recv_control(&socket, &mut buffer, retry_deadline).await? {
                Some((ControlType::Handshake, data, timestamp)) => {
                    let Some(handshake) = parse_handshake(&data) else {
                        continue;
                    };
                    match handshake.handshake_type {
                        HandshakeType::Conclusion => {
                            if let Some(message) = &kmreq {
                                let response =
                                    handshake.extensions.iter().find_map(|ext| match ext {
                                        HsExtension::KmRsp(payload) => Some(payload.clone()),
                                        _ => None,
                                    });
                                match response {
                                    Some(payload) if &payload == message => {}
                                    Some(payload) if payload.len() == 4 => {
                                        let state = u32::from_le_bytes([
                                            payload[0], payload[1], payload[2], payload[3],
                                        ]);
                                        return Err(Error::Srt(format!(
                                            "SRT caller: encryption rejected (state {state})"
                                        )));
                                    }
                                    _ => {
                                        return Err(Error::Srt(
                                            "SRT caller: KMRSP missing or mismatched".into(),
                                        ));
                                    }
                                }
                            }
                            let peer = handshake.socket_id;
                            let mut latency = config.latency_ms;
                            if let Some(caps) = handshake.hs_ext() {
                                latency = latency.max(u32::from(caps.send_latency_ms));
                            }
                            break 'conclusion (peer, latency, timestamp);
                        }
                        HandshakeType::Rejection(code) => {
                            return Err(Error::Srt(format!(
                                "SRT caller: connection rejected (code {code})"
                            )));
                        }
                        _ => continue,
                    }
                }
                Some(_) => continue,
                None => break, // retransmit the conclusion
            }
        }
    };

    Ok(HandshakeOutcome {
        socket,
        remote: config.remote,
        local_id,
        peer_id,
        crypto,
        latency_ms,
        timestamp,
        initial_sequence: conclusion.initial_sequence,
        stream_id: config.stream_id.clone(),
    })
}

impl SrtCaller {
    /// Connect to an SRT listener and complete the HSv5 handshake.
    pub async fn connect(config: SrtCallerConfig) -> Result<Self> {
        let outcome = connect_handshake(&config).await?;
        let HandshakeOutcome {
            socket,
            remote,
            local_id,
            peer_id,
            crypto,
            latency_ms,
            timestamp,
            initial_sequence: _,
            stream_id,
        } = outcome;
        let _ = timestamp;
        let mut tsbpd = TsbpdBuffer::new(latency_ms, config.tsbpd_max_buffered);
        tsbpd.set_tlpktdrop(config.tlpktdrop);
        tsbpd.set_time_base(Instant::now(), timestamp);
        let now = Instant::now();
        info!(
            remote = %config.remote,
            stream_id = ?config.stream_id,
            latency_ms,
            encrypted = crypto.is_some(),
            "SRT caller: connected"
        );
        Ok(Self {
            socket,
            remote,
            local_id,
            peer_id,
            tsbpd,
            crypto,
            ack_seq: 0,
            last_ack_sent: None,
            last_ack_time: now,
            last_nak_time: now,
            last_nak_losses: Vec::new(),
            pkts_since_ack: 0,
            packets_received: 0,
            bytes_received: 0,
            rtt_us: 0,
            rtt_var_us: 0,
            last_recv: now,
            last_sent: now,
            started: now,
            closed: false,
            latency_ms,
            peer_idle_timeout: config.peer_idle_timeout,
            last_skipped: 0,
            stream_id,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn latency_ms(&self) -> u32 {
        self.latency_ms
    }

    pub fn encrypted(&self) -> bool {
        self.crypto.is_some()
    }

    /// Current connection statistics.
    pub fn stats(&self) -> crate::SrtConnectionStats {
        let stats = self.tsbpd.stats();
        crate::SrtConnectionStats {
            peer: self.remote,
            stream_id: self.stream_id.clone(),
            mount: None,
            encrypted: self.crypto.is_some(),
            latency_ms: self.latency_ms,
            rtt_us: self.rtt_us,
            packets_received: self.packets_received,
            bytes_received: self.bytes_received,
            bytes_sent: 0,
            delivered: stats.delivered,
            skipped: stats.skipped,
            loss_events: stats.loss_events,
            undecryptable: 0,
            duration_secs: self.started.elapsed().as_secs(),
            srt_version: 0,
        }
    }

    /// Receive the next decrypted MPEG-TS payload chunk.
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; 2048];
        loop {
            if self.closed {
                return Err(Error::Closed);
            }
            match tokio::time::timeout(
                Duration::from_millis(10),
                self.socket.recv_from(&mut buffer),
            )
            .await
            {
                Ok(Ok((size, _peer))) => {
                    let data = bytes::Bytes::copy_from_slice(&buffer[..size]);
                    match parse_packet_borrowed(data) {
                        Some(SrtPacket::Control(ctrl)) => self.handle_control(&ctrl).await?,
                        Some(SrtPacket::Data(packet)) => self.handle_data(&packet).await?,
                        None => {}
                    }
                    if let Some(payload) = self.take_ready() {
                        return Ok(payload);
                    }
                }
                Ok(Err(error)) => {
                    return Err(Error::Srt(format!("SRT caller recv: {error}")));
                }
                Err(_) => {}
            }

            self.tick().await?;
            if let Some(payload) = self.take_ready() {
                return Ok(payload);
            }
            if self.last_recv.elapsed() > self.peer_idle_timeout {
                return Err(Error::Closed);
            }
        }
    }

    async fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        let timestamp = self.started.elapsed().as_micros() as u32;

        // Keepalive when idle.
        if now.duration_since(self.last_sent) >= Duration::from_secs(1) {
            let packet = keepalive_packet(self.peer_id, timestamp);
            self.send(&packet).await;
        }

        // Loss reports.
        let losses = self.tsbpd.detect_losses();
        if !losses.is_empty()
            && now.duration_since(self.last_nak_time) >= Duration::from_millis(20)
            && losses != self.last_nak_losses
        {
            let nak = nak_packet(&losses, self.peer_id, timestamp);
            self.send(&nak).await;
            self.last_nak_losses = losses;
            self.last_nak_time = now;
        }

        if self.tsbpd.skipped_count() > self.last_skipped {
            self.last_skipped = self.tsbpd.skipped_count();
            self.send_ack(timestamp).await;
        } else if self.pkts_since_ack > 0
            && now.duration_since(self.last_ack_time) >= Duration::from_millis(10)
        {
            self.send_ack(timestamp).await;
        }
        Ok(())
    }

    fn take_ready(&mut self) -> Option<Vec<u8>> {
        let ready = self.tsbpd.pop_ready();
        if ready.is_empty() {
            return None;
        }
        // Concatenate ready payloads: MPEG-TS framing does not care about
        // packet boundaries, and this keeps the API allocation-light.
        let mut out = Vec::new();
        for packet in ready {
            out.extend_from_slice(&packet.payload);
        }
        Some(out)
    }

    async fn handle_data(&mut self, packet: &crate::packet::DataPacket) -> Result<()> {
        if packet.socket_id != self.local_id {
            debug!(
                socket_id = packet.socket_id,
                "SRT caller: data for another socket"
            );
            return Ok(());
        }
        self.last_recv = Instant::now();
        let mut payload = packet.payload.to_vec();
        if let Some(crypto) = &self.crypto {
            if packet.key_flags & 0x03 == 0 {
                debug!("SRT caller: cleartext packet on an encrypted link");
                return Ok(());
            }
            if !crypto.decrypt(packet.sequence, packet.key_flags, &mut payload) {
                debug!(
                    sequence = packet.sequence,
                    "SRT caller: undecryptable packet"
                );
                return Ok(());
            }
        } else if packet.key_flags & 0x03 != 0 {
            debug!("SRT caller: encrypted packet without key");
            return Ok(());
        }
        self.packets_received += 1;
        self.bytes_received += payload.len() as u64;
        if self.tsbpd.push(
            packet.sequence,
            bytes::Bytes::from(payload),
            packet.timestamp,
            packet.retransmitted,
        ) {
            self.pkts_since_ack += 1;
        }
        Ok(())
    }

    async fn handle_control(&mut self, ctrl: &ControlPacket) -> Result<()> {
        self.last_recv = Instant::now();
        let timestamp = self.started.elapsed().as_micros() as u32;
        match ctrl.control_type {
            ControlType::Keepalive => {
                let packet = keepalive_packet(self.peer_id, timestamp);
                self.send(&packet).await;
            }
            ControlType::AckAck => {
                if let Some((ack_seq, sent_at)) = self.last_ack_sent
                    && ctrl.additional_info == ack_seq
                {
                    let sample = sent_at.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
                    if self.rtt_us == 0 {
                        self.rtt_us = sample;
                    } else {
                        let delta = sample.abs_diff(self.rtt_us);
                        self.rtt_var_us = (self.rtt_var_us * 3 + delta) / 4;
                        self.rtt_us = (self.rtt_us * 7 + sample) / 8;
                    }
                }
            }
            ControlType::Shutdown => {
                info!(remote = %self.remote, "SRT caller: peer shutdown");
                self.closed = true;
                return Err(Error::Closed);
            }
            ControlType::PeerError => {
                warn!(remote = %self.remote, "SRT caller: peer error");
                self.closed = true;
                return Err(Error::Closed);
            }
            ControlType::Nak => {
                debug!("SRT caller: ignoring NAK (receive-only)");
            }
            ControlType::DropReq => {
                if ctrl.data.len() >= 8 {
                    let first = u32::from_be_bytes([
                        ctrl.data[0],
                        ctrl.data[1],
                        ctrl.data[2],
                        ctrl.data[3],
                    ]) & 0x7FFF_FFFF;
                    let last = u32::from_be_bytes([
                        ctrl.data[4],
                        ctrl.data[5],
                        ctrl.data[6],
                        ctrl.data[7],
                    ]) & 0x7FFF_FFFF;
                    self.tsbpd.drop_range(first, last);
                }
            }
            ControlType::Handshake => {
                debug!("SRT caller: ignoring handshake after connect");
            }
            _ => {}
        }
        Ok(())
    }

    async fn send_ack(&mut self, timestamp: u32) {
        self.ack_seq = self.ack_seq.wrapping_add(1).max(1);
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let fields = AckFields {
            ack_sequence: self.ack_seq,
            last_ack_sequence: self.tsbpd.ack_number(),
            rtt_us: self.rtt_us,
            rtt_var_us: self.rtt_var_us,
            available_buffer: self
                .tsbpd
                .capacity()
                .saturating_sub(self.tsbpd.buffered_count()) as u32,
            packet_rate: (self.packets_received as f64 / elapsed) as u32,
            link_capacity: (self.packets_received as f64 / elapsed) as u32,
            byte_rate: (self.bytes_received as f64 / elapsed) as u32,
        };
        let ack = ack_packet(&fields, self.peer_id, timestamp);
        self.send(&ack).await;
        self.pkts_since_ack = 0;
        self.last_ack_time = Instant::now();
        self.last_ack_sent = Some((self.ack_seq, Instant::now()));
    }

    async fn send(&mut self, packet: &ControlPacket) {
        let encoded = encode_control_packet(packet);
        if self.socket.send_to(&encoded, self.remote).await.is_ok() {
            self.last_sent = Instant::now();
        }
    }

    /// Send SHUTDOWN and release the socket.
    pub async fn close(mut self) -> Result<()> {
        let timestamp = self.started.elapsed().as_micros() as u32;
        let packet = shutdown_packet(self.peer_id, timestamp);
        self.send(&packet).await;
        self.closed = true;
        Ok(())
    }
}

fn random_id() -> u32 {
    let mut bytes = [0u8; 4];
    if getrandom::getrandom(&mut bytes).is_err() {
        return 1;
    }
    u32::from_be_bytes(bytes).max(1)
}

fn control(handshake: &Handshake, socket_id: u32, timestamp: u32) -> ControlPacket {
    ControlPacket {
        control_type: ControlType::Handshake,
        type_specific: 0,
        additional_info: 0,
        timestamp,
        socket_id,
        data: encode_handshake(handshake),
    }
}

async fn send_control(
    socket: &UdpSocket,
    packet: &ControlPacket,
    remote: SocketAddr,
) -> Result<()> {
    let encoded = encode_control_packet(packet);
    socket
        .send_to(&encoded, remote)
        .await
        .map(|_| ())
        .map_err(|error| Error::Srt(format!("SRT caller send: {error}")))
}

async fn recv_control(
    socket: &UdpSocket,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<Option<(ControlType, bytes::Bytes, u32)>> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        let timeout = deadline - now;
        match tokio::time::timeout(timeout, socket.recv_from(buffer)).await {
            Ok(Ok((size, _peer))) => {
                let data = bytes::Bytes::copy_from_slice(&buffer[..size]);
                if let Some(SrtPacket::Control(ctrl)) = parse_packet_borrowed(data.clone()) {
                    return Ok(Some((ctrl.control_type, ctrl.data, ctrl.timestamp)));
                }
            }
            Ok(Err(error)) => {
                return Err(Error::Srt(format!("SRT caller recv: {error}")));
            }
            Err(_) => return Ok(None),
        }
    }
}
