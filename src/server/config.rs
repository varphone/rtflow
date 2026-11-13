use super::*;

#[derive(Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub session_timeout: Duration,
    pub max_rtsp_header_bytes: usize,
    pub max_rtsp_body_bytes: usize,
    pub max_sessions: usize,
    pub udp_port_range: Option<RangeInclusive<u16>>,
    pub consumer_queue_capacity: usize,
    pub no_subscriber_grace: Duration,
    pub shutdown_grace: Duration,
    pub authenticator: Arc<dyn Authenticator>,
    pub resource_limits: ResourceLimits,
    pub event_sink: Arc<dyn EventSink>,
    pub security: SecurityConfig,
    #[cfg(feature = "tls")]
    pub tls: Option<TlsServerConfig>,
}
impl ServerConfig {
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            session_timeout: Duration::from_secs(60),
            max_rtsp_header_bytes: 16 * 1024,
            max_rtsp_body_bytes: 256 * 1024,
            max_sessions: 256,
            udp_port_range: None,
            consumer_queue_capacity: 256,
            no_subscriber_grace: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(5),
            authenticator: Arc::new(AllowAll),
            resource_limits: ResourceLimits::default(),
            event_sink: Arc::new(NoopEventSink),
            security: SecurityConfig::default(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    pub fn builder(listen: SocketAddr) -> ServerConfigBuilder {
        ServerConfigBuilder {
            config: Self::new(listen),
        }
    }

    pub fn validate(&self) -> std::result::Result<(), ConfigError> {
        let positive = [
            (self.max_rtsp_header_bytes, "RTSP header bytes"),
            (self.max_rtsp_body_bytes, "RTSP body bytes"),
            (self.max_sessions, "RTSP sessions"),
            (self.consumer_queue_capacity, "consumer queue capacity"),
            (self.resource_limits.max_sessions_per_ip, "sessions per IP"),
            (self.resource_limits.max_mounts, "mounts"),
            (
                self.resource_limits.max_tracks_per_session,
                "tracks per session",
            ),
            (
                self.resource_limits.max_interleaved_frame_bytes,
                "interleaved frame bytes",
            ),
            (
                self.resource_limits.max_queued_bytes_per_session,
                "queued bytes per session",
            ),
        ];
        if let Some((_, name)) = positive.into_iter().find(|(value, _)| *value == 0) {
            return Err(ConfigError::Zero(name));
        }
        if self.session_timeout == Duration::ZERO {
            return Err(ConfigError::Zero("session timeout"));
        }
        if self.shutdown_grace == Duration::ZERO {
            return Err(ConfigError::Zero("shutdown grace"));
        }
        if self
            .udp_port_range
            .as_ref()
            .is_some_and(|range| range.start() >= range.end())
        {
            return Err(ConfigError::Invalid(
                "UDP port range must contain an RTP/RTCP pair",
            ));
        }
        if let Some(rate_limit) = self.security.connection_rate_limit {
            rate_limit
                .validate()
                .map_err(|_| ConfigError::Invalid("invalid connection rate limit"))?;
        }
        #[cfg(feature = "tls")]
        if self
            .tls
            .as_ref()
            .is_some_and(|tls| tls.handshake_timeout == Duration::ZERO)
        {
            return Err(ConfigError::Zero("TLS handshake timeout"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ResourceLimits {
    pub max_sessions_per_ip: usize,
    pub max_mounts: usize,
    pub max_tracks_per_session: usize,
    pub max_interleaved_frame_bytes: usize,
    pub max_queued_bytes_per_session: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_sessions_per_ip: 32,
            max_mounts: 1_024,
            max_tracks_per_session: 16,
            max_interleaved_frame_bytes: 65_536,
            max_queued_bytes_per_session: crate::mount::DEFAULT_MAX_QUEUED_BYTES_PER_SESSION,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    Zero(&'static str),
    Invalid(&'static str),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zero(name) => write!(formatter, "{name} must be greater than zero"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<ConfigError> for Error {
    fn from(error: ConfigError) -> Self {
        match error {
            ConfigError::Zero(name) => Self::Limit(name),
            ConfigError::Invalid(message) => Self::Rtsp(message.into()),
        }
    }
}

pub struct ServerConfigBuilder {
    config: ServerConfig,
}

impl ServerConfigBuilder {
    pub fn session_timeout(mut self, value: Duration) -> Self {
        self.config.session_timeout = value;
        self
    }

    pub fn max_sessions(mut self, value: usize) -> Self {
        self.config.max_sessions = value;
        self
    }

    pub fn consumer_queue_capacity(mut self, value: usize) -> Self {
        self.config.consumer_queue_capacity = value;
        self
    }

    pub fn udp_port_range(mut self, value: RangeInclusive<u16>) -> Self {
        self.config.udp_port_range = Some(value);
        self
    }

    /// Bound the maximum RTSP request header size (bytes).
    pub fn max_rtsp_header_bytes(mut self, value: usize) -> Self {
        self.config.max_rtsp_header_bytes = value;
        self
    }

    /// Bound the maximum RTSP request body size (bytes).
    pub fn max_rtsp_body_bytes(mut self, value: usize) -> Self {
        self.config.max_rtsp_body_bytes = value;
        self
    }

    pub fn no_subscriber_grace(mut self, value: Duration) -> Self {
        self.config.no_subscriber_grace = value;
        self
    }

    pub fn shutdown_grace(mut self, value: Duration) -> Self {
        self.config.shutdown_grace = value;
        self
    }

    pub fn authenticator(mut self, value: Arc<dyn Authenticator>) -> Self {
        self.config.authenticator = value;
        self
    }

    pub fn resource_limits(mut self, value: ResourceLimits) -> Self {
        self.config.resource_limits = value;
        self
    }

    pub fn event_sink(mut self, value: Arc<dyn EventSink>) -> Self {
        self.config.event_sink = value;
        self
    }

    pub fn security(mut self, value: SecurityConfig) -> Self {
        self.config.security = value;
        self
    }

    #[cfg(feature = "tls")]
    pub fn tls(mut self, value: TlsServerConfig) -> Self {
        self.config.tls = Some(value);
        self
    }

    pub fn build(self) -> std::result::Result<ServerConfig, ConfigError> {
        self.config.validate()?;
        Ok(self.config)
    }
}
