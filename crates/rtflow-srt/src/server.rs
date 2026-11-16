use super::*;

impl SrtServer {
    /// Bind an SRT server to the given address.
    pub async fn bind(
        addr: impl tokio::net::ToSocketAddrs,
        mounts: Arc<MountRegistry>,
    ) -> Result<Self> {
        let socket = UdpSocket::bind(&addr)
            .await
            .map_err(|e| Error::Srt(format!("bind: {e}")))?;

        let local_addr = socket.local_addr().ok();
        info!("SRT server listening on {:?}", local_addr);

        Ok(Self {
            socket: Arc::new(socket),
            mounts,
            config: SrtConfig::default(),
            cancel: CancellationToken::new(),
            pending_cookies: std::sync::Mutex::new(HashMap::new()),
            stats: Arc::new(std::sync::RwLock::new(Vec::new())),
        })
    }

    /// Create an SRT server with full configuration.
    pub async fn with_config(config: SrtConfig, mounts: Arc<MountRegistry>) -> Result<Self> {
        let socket = UdpSocket::bind(config.bind_addr)
            .await
            .map_err(|e| Error::Srt(format!("bind {}: {e}", config.bind_addr)))?;

        info!("SRT server listening on {}", config.bind_addr);

        Ok(Self {
            socket: Arc::new(socket),
            mounts,
            config,
            cancel: CancellationToken::new(),
            pending_cookies: std::sync::Mutex::new(HashMap::new()),
            stats: Arc::new(std::sync::RwLock::new(Vec::new())),
        })
    }

    /// Snapshot handle for live connection statistics.
    pub fn stats_handle(&self) -> Arc<std::sync::RwLock<Vec<SrtConnectionStats>>> {
        self.stats.clone()
    }

    /// Current connection statistics snapshot.
    pub fn stats(&self) -> Vec<SrtConnectionStats> {
        self.stats
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Get the local address the server is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Get a cancellation token for graceful shutdown.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Run the server receive loop until cancelled.
    pub async fn run(self) -> Result<()> {
        let mut buf = bytes::BytesMut::with_capacity(2048);
        let mut connections: HashMap<SocketAddr, SrtConnection> = HashMap::new();
        let mut next_socket_id: u32 = 1;
        let mut ack_seq_counter: u32 = 1;
        let mut tick_interval = tokio::time::interval(Duration::from_millis(10));

        loop {
            // Keep room for one full datagram: after split_to() the buffer
            // is short by the previous packet length, so this re-grows only
            // that many zero bytes. The payload handed to SrtPacket is a
            // shared view (Bytes), so once a packet is still referenced the
            // buffer reallocates via CoW — but the datagram itself is never
            // memcpy'd a second time.
            buf.resize(2048, 0);
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("SRT server shutting down");
                    break;
                }
                _ = tick_interval.tick() => {
                    // Periodic maintenance: ACK, NAK, timeout
                    self.maintenance(&mut connections, &mut ack_seq_counter).await;
                }
                result = self.socket.recv_from(&mut buf) => {
                    let (n, peer) = match result {
                        Ok(r) => r,
                        Err(e) => {
                            warn!("SRT recv error: {e}");
                            continue;
                        }
                    };

                    // Take ownership of the received bytes without a copy;
                    // parse_packet_borrowed turns the payload into a shared
                    // view, so the datagram is memcpy'd exactly once (UDP
                    // kernel → user space).
                    let data = buf.split_to(n).freeze();
                    let Some(srt_packet) = parse_packet_borrowed(data) else {
                        debug!("SRT: failed to parse packet from {peer}");
                        continue;
                    };

                    match srt_packet {
                        SrtPacket::Control(ctrl) => {
                            self.handle_control(
                                &ctrl,
                                peer,
                                &mut connections,
                                &mut next_socket_id,
                            ).await;
                        }
                        SrtPacket::Data(data_pkt) => {
                            self.handle_data(
                                &data_pkt,
                                peer,
                                &mut connections,
                            ).await;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Shutdown the server.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}
