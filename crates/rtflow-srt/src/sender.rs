//! SRT sender (caller → listener): push MPEG-TS or arbitrary payloads.
//!
//! The caller handshake is shared with [`SrtCaller`](crate::SrtCaller); this
//! role additionally packetises the payload, keeps a retransmission window,
//! answers NAK/ACK, and advertises TSBPD sends via the standard HSREQ flags.
use crate::SrtConnectionStats;
use crate::caller::{SrtCallerConfig, connect_handshake};
use crate::crypto::SessionCrypto;
use crate::packet::{
    ControlPacket, ControlType, DataPacket, SrtPacket, ackack_packet, encode_control_packet,
    encode_data_packet, keepalive_packet, parse_ack, parse_nak_loss_list, parse_packet_borrowed,
    shutdown_packet,
};
use bytes::Bytes;
use rtflow_core::error::{Error, Result};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tracing::{debug, info};

struct SentPacket {
    sequence: u32,
    timestamp: u32,
    position: u8,
    message_number: u32,
    payload: Vec<u8>,
}

/// SRT sender connected to a listener.
pub struct SrtSender {
    socket: UdpSocket,
    remote: SocketAddr,
    local_id: u32,
    peer_id: u32,
    started: Instant,
    crypto: Option<SessionCrypto>,
    payload_size: usize,
    next_sequence: u32,
    next_message: u32,
    buffer: VecDeque<SentPacket>,
    last_ack: u32,
    have_ack: bool,
    rtt_us: u32,
    last_recv: Instant,
    last_sent: Instant,
    closed: bool,
    bytes_sent: u64,
    packets_sent: u64,
    retransmits: u64,
    stream_id: Option<String>,
    latency_ms: u32,
}

const MAX_SEND_BUFFER: usize = 8192;

impl SrtSender {
    /// Connect and complete the HSv5 handshake.
    pub async fn connect(config: SrtCallerConfig) -> Result<Self> {
        let outcome = connect_handshake(&config).await?;
        let now = Instant::now();
        info!(
            remote = %config.remote,
            stream_id = ?config.stream_id,
            encrypted = outcome.crypto.is_some(),
            "SRT sender: connected"
        );
        let mut sender = Self {
            socket: outcome.socket,
            remote: outcome.remote,
            local_id: outcome.local_id,
            peer_id: outcome.peer_id,
            started: now,
            crypto: outcome.crypto,
            payload_size: config
                .payload_size
                .clamp(188, (config.mtu as usize).saturating_sub(16)),
            next_sequence: outcome.initial_sequence,
            next_message: 1,
            buffer: VecDeque::new(),
            last_ack: 1,
            have_ack: false,
            rtt_us: 0,
            last_recv: now,
            last_sent: now,
            closed: false,
            bytes_sent: 0,
            packets_sent: 0,
            retransmits: 0,
            stream_id: config.stream_id.clone(),
            latency_ms: config.latency_ms,
        };
        // Consume any immediate handshake leftovers (e.g. duplicate conclusion).
        sender.poll_inbound().await?;
        Ok(sender)
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn encrypted(&self) -> bool {
        self.crypto.is_some()
    }

    /// Send one payload as an SRT message (split into MTU-sized packets).
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        if self.closed {
            return Err(Error::Closed);
        }
        self.poll_inbound().await?;
        if data.is_empty() {
            return Ok(());
        }
        let timestamp = self.started.elapsed().as_micros() as u32;
        let message_number = self.next_message;
        self.next_message = (self.next_message + 1) & 0x03FF_FFFF;
        let chunks: Vec<&[u8]> = data.chunks(self.payload_size).collect();
        let count = chunks.len();
        for (index, chunk) in chunks.into_iter().enumerate() {
            let position = match (index, count) {
                (0, 1) => 3,               // single
                (0, _) => 2,               // first
                (i, n) if i + 1 == n => 1, // last
                _ => 0,                    // middle
            };
            let sequence = self.next_sequence;
            self.next_sequence = self.next_sequence.wrapping_add(1) & 0x7FFF_FFFF;
            self.transmit(sequence, timestamp, position, message_number, chunk, false)
                .await?;
            self.buffer.push_back(SentPacket {
                sequence,
                timestamp,
                position,
                message_number,
                payload: chunk.to_vec(),
            });
            self.trim_buffer();
        }
        Ok(())
    }

    async fn transmit(
        &mut self,
        sequence: u32,
        timestamp: u32,
        position: u8,
        message_number: u32,
        payload: &[u8],
        retransmitted: bool,
    ) -> Result<()> {
        let mut data = payload.to_vec();
        let key_flags = match &self.crypto {
            Some(crypto) => {
                if !crypto.encrypt(sequence, &mut data) {
                    return Err(Error::Srt("SRT sender: encryption key missing".into()));
                }
                crypto.key_flags()
            }
            None => 0,
        };
        let packet = DataPacket {
            sequence,
            position,
            ordered: true,
            retransmitted,
            key_flags,
            message_number,
            timestamp,
            socket_id: self.peer_id,
            payload: Bytes::from(data),
        };
        let encoded = encode_data_packet(&packet);
        self.socket
            .send_to(&encoded, self.remote)
            .await
            .map_err(|error| Error::Srt(format!("SRT sender send: {error}")))?;
        self.last_sent = Instant::now();
        self.packets_sent += 1;
        self.bytes_sent = self.bytes_sent.saturating_add(payload.len() as u64);
        if retransmitted {
            self.retransmits += 1;
        }
        Ok(())
    }

    fn trim_buffer(&mut self) {
        if self.have_ack {
            while let Some(front) = self.buffer.front() {
                if seq_before(front.sequence, self.last_ack) {
                    self.buffer.pop_front();
                } else {
                    break;
                }
            }
            if self.buffer.len() > MAX_SEND_BUFFER {
                let excess = self.buffer.len() - MAX_SEND_BUFFER;
                self.buffer.drain(..excess);
            }
        }
    }

    /// Process ACK/NAK/keepalive and answer ACKs.
    pub async fn poll_inbound(&mut self) -> Result<()> {
        let mut buffer = vec![0u8; 2048];
        loop {
            match self.socket.try_recv_from(&mut buffer) {
                Ok((size, _peer)) => {
                    self.last_recv = Instant::now();
                    let data = Bytes::copy_from_slice(&buffer[..size]);
                    let Some(SrtPacket::Control(ctrl)) = parse_packet_borrowed(data) else {
                        continue;
                    };
                    self.handle_control(&ctrl).await?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    return Err(Error::Srt(format!("SRT sender recv: {error}")));
                }
            }
        }
        Ok(())
    }

    async fn handle_control(&mut self, ctrl: &ControlPacket) -> Result<()> {
        // Control packets must target the socket id we were assigned.
        if ctrl.socket_id != 0 && ctrl.socket_id != self.local_id {
            debug!(
                socket_id = ctrl.socket_id,
                "SRT sender: control for another socket"
            );
            return Ok(());
        }
        match ctrl.control_type {
            ControlType::Ack => {
                if let Some(fields) = parse_ack(ctrl) {
                    self.last_ack = fields.last_ack_sequence;
                    self.have_ack = true;
                    self.rtt_us = fields.rtt_us;
                    self.trim_buffer();
                    // A full ACK must be acknowledged for the receiver's RTT.
                    if ctrl.data.len() >= 28 {
                        let ackack =
                            ackack_packet(fields.ack_sequence, self.peer_id, ctrl.timestamp);
                        self.send_control(&ackack).await;
                    }
                }
            }
            ControlType::Nak => {
                let losses = parse_nak_loss_list(&ctrl.data);
                for (start, end) in losses {
                    let mut sequence = start;
                    loop {
                        if let Some(index) = self
                            .buffer
                            .iter()
                            .position(|packet| packet.sequence == sequence)
                        {
                            let packet = &self.buffer[index];
                            let (timestamp, position, message_number) =
                                (packet.timestamp, packet.position, packet.message_number);
                            let payload = packet.payload.clone();
                            self.transmit(
                                sequence,
                                timestamp,
                                position,
                                message_number,
                                &payload,
                                true,
                            )
                            .await?;
                        }
                        if sequence == end {
                            break;
                        }
                        sequence = sequence.wrapping_add(1) & 0x7FFF_FFFF;
                    }
                }
            }
            ControlType::Keepalive => {
                let timestamp = self.started.elapsed().as_micros() as u32;
                let packet = keepalive_packet(self.peer_id, timestamp);
                self.send_control(&packet).await;
            }
            ControlType::Shutdown => {
                info!(remote = %self.remote, "SRT sender: peer shutdown");
                self.closed = true;
                return Err(Error::Closed);
            }
            ControlType::PeerError => {
                self.closed = true;
                return Err(Error::Closed);
            }
            ControlType::AckAck | ControlType::DropReq | ControlType::Handshake => {}
            _ => {
                debug!(remote = %self.remote, kind = ?ctrl.control_type, "SRT sender: control");
            }
        }
        Ok(())
    }

    async fn send_control(&mut self, packet: &ControlPacket) {
        let encoded = encode_control_packet(packet);
        if self.socket.send_to(&encoded, self.remote).await.is_ok() {
            self.last_sent = Instant::now();
        }
    }

    /// Keepalive when idle, process inbound, and drop acknowledged packets.
    pub async fn tick(&mut self) -> Result<()> {
        self.poll_inbound().await?;
        if self.last_sent.elapsed() >= Duration::from_secs(1) {
            let timestamp = self.started.elapsed().as_micros() as u32;
            let packet = keepalive_packet(self.peer_id, timestamp);
            self.send_control(&packet).await;
        }
        self.trim_buffer();
        Ok(())
    }

    /// Current sender statistics.
    pub fn stats(&self) -> SrtConnectionStats {
        SrtConnectionStats {
            peer: self.remote,
            stream_id: self.stream_id.clone(),
            mount: None,
            encrypted: self.crypto.is_some(),
            latency_ms: self.latency_ms,
            rtt_us: self.rtt_us,
            packets_received: 0,
            bytes_received: 0,
            bytes_sent: self.bytes_sent,
            delivered: self.packets_sent,
            skipped: 0,
            loss_events: self.retransmits,
            undecryptable: 0,
            duration_secs: self.started.elapsed().as_secs(),
            srt_version: 0,
        }
    }

    /// Send SHUTDOWN and release the socket.
    pub async fn close(mut self) -> Result<()> {
        let timestamp = self.started.elapsed().as_micros() as u32;
        let packet = shutdown_packet(self.peer_id, timestamp);
        self.send_control(&packet).await;
        self.closed = true;
        Ok(())
    }
}

fn seq_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
