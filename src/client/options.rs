use super::*;

#[derive(Clone)]
#[non_exhaustive]
pub struct ClientOptions {
    pub transport: UpstreamTransport,
    pub credentials: Option<Credentials>,
    pub request_timeout: Duration,
    pub jitter: JitterBufferConfig,
    pub output_queue_capacity: usize,
    /// Byte cap for frames buffered in the output queue. When exceeded the
    /// new frame is dropped (the consumer is not stalled). Defaults to 8 MiB.
    pub max_output_bytes: usize,
    pub keep_alive_interval: Option<Duration>,
    #[cfg(feature = "tls")]
    pub tls: Option<TlsClientConfig>,
    pub play_range: Option<NptRange>,
    /// Optional sink for [`ClientEvent`]s (lifecycle, fallback, backoff).
    pub event_sink: Option<Arc<dyn EventSink>>,
    /// UDP socket receive-buffer size per RTP/RTCP socket. Defaults to 4MiB;
    /// memory-constrained embedded clients can lower it at the cost of burst
    /// tolerance. Applies to UDP transport only.
    pub udp_recv_buffer: Option<usize>,
    /// Request RTCP multiplexing (`rtcp-mux`). `None` (default) follows the
    /// server's SDP advertisement, `Some(true)` requests it unconditionally
    /// and `Some(false)` disables it.
    pub rtcp_mux: Option<bool>,
}
impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            transport: UpstreamTransport::Auto,
            credentials: None,
            request_timeout: Duration::from_secs(5),
            jitter: JitterBufferConfig::default(),
            output_queue_capacity: 64,
            max_output_bytes: 8 * 1024 * 1024,
            keep_alive_interval: Some(Duration::from_secs(20)),
            #[cfg(feature = "tls")]
            tls: None,
            play_range: None,
            event_sink: None,
            udp_recv_buffer: None,
            rtcp_mux: None,
        }
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ClientReconnectPolicy {
    /// Delay before the first reconnect attempt.
    pub initial_delay: Duration,
    /// Upper bound for exponential reconnect backoff.
    pub max_delay: Duration,
    /// Number of failed reconnect attempts before reporting an error; zero retries forever.
    pub max_attempts: u32,
}

impl Default for ClientReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            max_attempts: 0,
        }
    }
}

impl ClientReconnectPolicy {
    pub fn builder() -> ClientReconnectPolicyBuilder {
        ClientReconnectPolicyBuilder {
            policy: Self::default(),
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.initial_delay == Duration::ZERO || self.max_delay < self.initial_delay {
            return Err(Error::Limit("client reconnect delay"));
        }
        Ok(())
    }
}

pub struct ClientReconnectPolicyBuilder {
    policy: ClientReconnectPolicy,
}

impl ClientReconnectPolicyBuilder {
    pub fn initial_delay(mut self, value: Duration) -> Self {
        self.policy.initial_delay = value;
        self
    }

    pub fn max_delay(mut self, value: Duration) -> Self {
        self.policy.max_delay = value;
        self
    }

    pub fn max_attempts(mut self, value: u32) -> Self {
        self.policy.max_attempts = value;
        self
    }

    pub fn build(self) -> Result<ClientReconnectPolicy> {
        self.policy.validate()?;
        Ok(self.policy)
    }
}

pub(super) fn validate_options(options: &ClientOptions) -> Result<()> {
    if options.output_queue_capacity == 0 {
        return Err(Error::Limit("client output queue capacity"));
    }
    if options.max_output_bytes == 0 {
        return Err(Error::Limit("client output byte budget"));
    }
    if options.keep_alive_interval == Some(Duration::ZERO) {
        return Err(Error::Limit("client keep-alive interval"));
    }
    if options.request_timeout == Duration::ZERO {
        return Err(Error::Limit("client request timeout"));
    }
    Ok(())
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

    pub fn max_output_bytes(mut self, value: usize) -> Self {
        self.options.max_output_bytes = value;
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

    pub fn event_sink(mut self, value: Arc<dyn EventSink>) -> Self {
        self.options.event_sink = Some(value);
        self
    }

    pub fn udp_recv_buffer(mut self, value: usize) -> Self {
        self.options.udp_recv_buffer = Some(value);
        self
    }

    /// Force RTCP multiplexing on (`true`) or off (`false`); omit for auto.
    pub fn rtcp_mux(mut self, value: bool) -> Self {
        self.options.rtcp_mux = Some(value);
        self
    }

    pub fn build(self) -> Result<ClientOptions> {
        self.options.validate()?;
        Ok(self.options)
    }
}

/// Methods and extensions advertised by the server in the OPTIONS response
/// (`Public`, `Supported`, `Server`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServerCapabilities {
    /// Methods listed in `Public`.
    pub public: Vec<String>,
    /// Package/feature tokens listed in `Supported`.
    pub supported: Vec<String>,
    /// `Server` product string, when present.
    pub server: Option<String>,
}

impl ServerCapabilities {
    pub(super) fn from_response(response: &Response) -> Self {
        let split = |value: Option<&String>| {
            value
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            public: split(response.headers.get("public")),
            supported: split(response.headers.get("supported")),
            server: response.headers.get("server").cloned(),
        }
    }

    /// Whether the server advertised a method (case-insensitive).
    pub fn supports(&self, method: &str) -> bool {
        self.public
            .iter()
            .any(|item| item.eq_ignore_ascii_case(method))
    }
}

impl ClientOptions {
    /// Memory- and latency-oriented defaults for embedded clients.
    ///
    /// Caps per-track jitter to 32 packets / 30 ms, the output queue to 16
    /// frames and UDP receive buffers to 256 KiB. Bandwidth and resilience
    /// are traded for a small, predictable footprint.
    pub fn low_memory() -> Self {
        Self {
            jitter: JitterBufferConfig {
                max_packets: 32,
                max_reorder_window: 32,
                max_delay: Duration::from_millis(30),
            },
            output_queue_capacity: 16,
            max_output_bytes: 2 * 1024 * 1024,
            udp_recv_buffer: Some(256 * 1024),
            ..Self::default()
        }
    }
}
