pub mod auth_middleware;
pub mod cors;
mod request;
pub mod response;
mod router;

pub use request::{HttpRequest, Method};
#[doc(hidden)]
pub use request::{ParseError, parse_request};
pub use response::ResponseWriter;
pub use router::ConnWriter;
pub use router::{Handler, Router};

use rtflow_core::error::{Error, Result};
use rtflow_security::SecurityConfig;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::RwLock,
};
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_CONNECTIONS: usize = 1024;
const READ_BUF_SIZE: usize = 8192;

#[derive(Clone)]
pub struct HttpConfig {
    pub listen: SocketAddr,
    pub max_connections: usize,
    pub read_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    /// Per-write deadline for response bodies. A peer that stops reading
    /// (slow/frozen client) is disconnected after this, instead of
    /// pinning a connection slot forever (R22).
    pub response_timeout: Duration,
    pub security: SecurityConfig,
    /// Optional global metrics registry for request counting.
    pub metrics: Option<Arc<rtflow_core::metrics::MetricsRegistry>>,
}

impl HttpConfig {
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            read_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(5),
            max_header_bytes: 8 * 1024,
            max_body_bytes: 256 * 1024,
            response_timeout: Duration::from_secs(60),
            security: SecurityConfig::default(),
            metrics: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.max_connections == 0 {
            return Err(Error::Limit("HTTP max connections"));
        }
        if self.read_timeout == Duration::ZERO
            || self.response_timeout == Duration::ZERO
            || self.shutdown_timeout == Duration::ZERO
        {
            return Err(Error::Limit("HTTP timeout"));
        }
        if self.max_header_bytes == 0 || self.max_body_bytes == 0 {
            return Err(Error::Limit("HTTP message size"));
        }
        Ok(())
    }

    /// Attach a global metrics registry (request/segment counters).
    pub fn with_metrics(mut self, metrics: Arc<rtflow_core::metrics::MetricsRegistry>) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

pub struct HttpServer {
    listener: Option<TcpListener>,
    router: Arc<RwLock<Router>>,
    cancel: CancellationToken,
    config: HttpConfig,
    local_addr: SocketAddr,
}

impl HttpServer {
    pub async fn bind(config: HttpConfig) -> Result<Self> {
        config.validate()?;
        let listener = TcpListener::bind(config.listen).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener: Some(listener),
            router: Arc::new(RwLock::new(Router::new())),
            cancel: CancellationToken::new(),
            config,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn add_route(&self, method: Method, pattern: &str, handler: Handler) {
        let mut router = self.router.write().await;
        router.add(method, pattern, handler);
    }

    pub async fn run(&mut self) -> Result<()> {
        let Some(listener) = self.listener.take() else {
            return Err(Error::InvalidConfig(
                "HTTP server run() can only be called once".into(),
            ));
        };
        let router = self.router.clone();
        let cancel = self.cancel.clone();
        let security = self.config.security.clone();
        let metrics = self.config.metrics.clone();
        let read_timeout = self.config.read_timeout;
        let shutdown_timeout = self.config.shutdown_timeout;
        let max_header_bytes = self.config.max_header_bytes;
        let max_body_bytes = self.config.max_body_bytes;
        let response_timeout = self.config.response_timeout;
        let connections = Arc::new(tokio::sync::Semaphore::new(self.config.max_connections));
        let rate_limiter = Arc::new(rtflow_security::ConnectionRateLimiter::new(
            self.config.security.connection_rate_limit,
        )?);

        tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let (stream, addr) = match result {
                            Ok(value) => value,
                            Err(_) => {
                                // EMFILE/ENFILE must not turn into a tight
                                // accept-error spin that burns a core.
                                tokio::time::sleep(Duration::from_millis(10)).await;
                                continue;
                            }
                        };
                        // Low-latency streaming: disable Nagle so small FLV
                        // tags are not delayed behind unrelated segments
                        // (tokio defaults to true, but be explicit).
                        let _ = stream.set_nodelay(true);
                        if !security.ip_acl.permits(addr.ip()) {
                            tracing::warn!(peer = %addr, "HTTP connection rejected by IP ACL");
                            continue;
                        }
                        if !rate_limiter.admit(addr.ip(), std::time::Instant::now()) {
                            tracing::warn!(peer = %addr, "HTTP connection rejected by rate limit");
                            // Reply 429 so clients can distinguish rate
                            // limiting from a network failure.
                            tokio::spawn(async move {
                                let mut writer = ResponseWriter::new(stream);
                                let _ = writer
                                    .write_body(429, "text/plain", b"Too Many Requests")
                                    .await;
                            });
                            continue;
                        }
                        let Ok(permit) = connections.clone().try_acquire_owned() else {
                            // Connection capacity exhausted: same 429
                            // semantics as rate limiting.
                            tokio::spawn(async move {
                                let mut writer = ResponseWriter::new(stream);
                                let _ = writer
                                    .write_body(429, "text/plain", b"Too Many Requests")
                                    .await;
                            });
                            continue;
                        };
                        let router = router.clone();
                        let security = security.clone();
                        let metrics = metrics.clone();
                        tasks.spawn(async move {
                            let _permit = permit;
                            if let Some(metrics) = &metrics {
                                use std::sync::atomic::Ordering;
                                metrics.http_requests_total.fetch_add(1, Ordering::Relaxed);
                            }
                            if let Err(e) = handle_connection(
                                stream,
                                router,
                                security,
                                read_timeout,
                                max_header_bytes,
                                max_body_bytes,
                                response_timeout,
                            )
                            .await
                            {
                                tracing::debug!("http connection error: {e}");
                            }
                        });
                    }
                    _ = cancel.cancelled() => break,
                }
            }
            let _ = tokio::time::timeout(shutdown_timeout, async {
                while tasks.join_next().await.is_some() {}
            })
            .await;
            tasks.abort_all();
        });

        tracing::info!("http server listening on {}", self.local_addr);
        Ok(())
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    /// Cancellation token shared with the accept loop, for supervisors that
    /// need to stop HTTP together with other protocol servers.
    pub fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.clone()
    }
}

async fn handle_connection(
    stream: TcpStream,
    router: Arc<RwLock<Router>>,
    security: SecurityConfig,
    read_timeout: Duration,
    max_header_bytes: usize,
    max_body_bytes: usize,
    response_timeout: Duration,
) -> Result<()> {
    let peer = stream.peer_addr().ok();

    // ACL check
    if let Some(peer_addr) = peer
        && !security.ip_acl.permits(peer_addr.ip())
    {
        return Err(Error::Unauthorized);
    }

    let (mut read_half, write_half) = stream.into_split();
    let mut writer =
        Some(ResponseWriter::new(write_half).with_write_timeout(Some(response_timeout)));
    let mut buf = Vec::with_capacity(READ_BUF_SIZE);

    loop {
        // Read request
        let mut temp = [0u8; READ_BUF_SIZE];
        let n = tokio::time::timeout(
            read_timeout,
            tokio::io::AsyncReadExt::read(&mut read_half, &mut temp),
        )
        .await
        .map_err(|_| Error::Limit("read timeout"))?
        .map_err(Error::Io)?;

        if n == 0 {
            break;
        }
        buf.extend_from_slice(&temp[..n]);

        // Try to parse request
        let req = match request::parse_request(&buf, max_header_bytes, max_body_bytes) {
            Ok(Some((req, consumed))) => {
                buf.drain(..consumed);
                req
            }
            Ok(None) => {
                if buf.len() > max_header_bytes {
                    return Err(Error::Limit("request headers too large"));
                }
                continue;
            }
            Err(request::ParseError::Incomplete) => continue,
            Err(request::ParseError::TooLarge) => {
                return Err(Error::Limit("request headers too large"));
            }
            Err(request::ParseError::Invalid) => {
                // Reply 400 instead of dropping the connection so the
                // client can distinguish a protocol error from a network
                // failure.
                if let Some(writer) = writer.as_mut() {
                    let _ = writer.write_body(400, "text/plain", b"Bad Request").await;
                }
                return Err(Error::Http("invalid HTTP request".into()));
            }
        };

        tracing::trace!(peer = ?peer, method = ?req.method, path = %req.path, "HTTP request");
        let keep_alive = !req
            .header("connection")
            .is_some_and(|value| value.eq_ignore_ascii_case("close"));

        let req_method = req.method.clone();
        let req_path = req.path.clone();
        let mut w = writer.take().expect("writer should be available");
        // Keep-alive: the same writer serves multiple requests; clear the
        // previous response's state (headers_written/connection_close)
        // before handing it to the next handler.
        w.reset();
        let guard = router.read().await;
        let (handler, params) = guard.route(&req.method, &req.path.clone());
        let handler = handler.clone();
        let mut req = req;
        req.path_params = params;
        drop(guard);

        match handler(req, w).await {
            Ok(returned_writer) => {
                let close = returned_writer.should_close();
                writer = Some(returned_writer);
                if close {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(peer = ?peer, method = ?req_method, path = %req_path, %e, "HTTP handler error");
                break;
            }
        }

        if !keep_alive {
            break;
        }
    }

    Ok(())
}
