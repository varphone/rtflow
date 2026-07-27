pub mod auth_middleware;
pub mod cors;
mod request;
pub mod response;
mod router;

pub use request::{HttpRequest, Method};
pub use response::ResponseWriter;
pub use router::{Handler, Router};

use crate::{Error, Result, security::SecurityConfig};
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
    pub security: SecurityConfig,
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
            security: SecurityConfig::default(),
        }
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
        let listener = self.listener.take().expect("run() can only be called once");
        let router = self.router.clone();
        let cancel = self.cancel.clone();
        let security = self.config.security.clone();
        let read_timeout = self.config.read_timeout;
        let shutdown_timeout = self.config.shutdown_timeout;
        let max_header_bytes = self.config.max_header_bytes;
        let max_body_bytes = self.config.max_body_bytes;
        let connections = Arc::new(tokio::sync::Semaphore::new(self.config.max_connections));
        let rate_limiter = Arc::new(crate::security::ConnectionRateLimiter::new(
            self.config.security.connection_rate_limit,
        )?);

        tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let Ok((stream, addr)) = result else { continue; };
                        if !security.ip_acl.permits(addr.ip())
                            || !rate_limiter.admit(addr.ip(), std::time::Instant::now())
                        {
                            continue;
                        }
                        let Ok(permit) = connections.clone().try_acquire_owned() else {
                            continue;
                        };
                        let router = router.clone();
                        let security = security.clone();
                        tasks.spawn(async move {
                            let _permit = permit;
                            if let Err(e) = handle_connection(
                                stream, router, security, read_timeout, max_header_bytes, max_body_bytes,
                            ).await {
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
}

async fn handle_connection(
    stream: TcpStream,
    router: Arc<RwLock<Router>>,
    security: SecurityConfig,
    read_timeout: Duration,
    max_header_bytes: usize,
    max_body_bytes: usize,
) -> Result<()> {
    let peer = stream.peer_addr().ok();

    // ACL check
    if let Some(peer_addr) = peer
        && !security.ip_acl.permits(peer_addr.ip())
    {
        return Err(Error::Unauthorized);
    }

    let (mut read_half, write_half) = stream.into_split();
    let mut writer = Some(ResponseWriter::new(write_half));
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
                return Err(Error::Rtsp("invalid HTTP request".into()));
            }
        };

        let keep_alive = !req
            .header("connection")
            .is_some_and(|value| value.eq_ignore_ascii_case("close"));

        let w = writer.take().expect("writer should be available");
        let guard = router.read().await;
        let (handler, params) = guard.route(&req.method, &req.path.clone());
        let handler = handler.clone();
        let mut req = req;
        req.path_params = params;
        drop(guard);

        match handler(req, w).await {
            Ok(returned_writer) => {
                writer = Some(returned_writer);
            }
            Err(e) => {
                tracing::debug!("http handler error: {e}");
                break;
            }
        }

        if !keep_alive {
            break;
        }
    }

    Ok(())
}
