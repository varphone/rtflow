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
    pub security: SecurityConfig,
}

impl HttpConfig {
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            read_timeout: Duration::from_secs(30),
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

        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    result = listener.accept() => match result {
                        Ok((stream, _addr)) => stream,
                        Err(_) => continue,
                    },
                    _ = cancel.cancelled() => break,
                };

                let router = router.clone();
                let security = security.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, router, security, read_timeout).await {
                        tracing::debug!("http connection error: {e}");
                    }
                });
            }
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
) -> Result<()> {
    let peer = stream.peer_addr().ok();

    // ACL check
    if let Some(peer_addr) = peer {
        if !security.ip_acl.permits(peer_addr.ip()) {
            return Err(Error::Unauthorized);
        }
    }

    let (mut read_half, write_half) = stream.into_split();
    let mut writer = Some(ResponseWriter::new(tokio::io::BufWriter::new(write_half)));
    let mut buf = Vec::with_capacity(READ_BUF_SIZE);

    loop {
        // Read request
        let mut temp = [0u8; READ_BUF_SIZE];
        let n = tokio::time::timeout(read_timeout, tokio::io::AsyncReadExt::read(&mut read_half, &mut temp))
            .await
            .map_err(|_| Error::Limit("read timeout"))?
            .map_err(Error::Io)?;

        if n == 0 {
            break;
        }
        buf.extend_from_slice(&temp[..n]);

        // Try to parse request
        let req = match request::parse_request(&buf) {
            Ok(Some((req, consumed))) => {
                buf.drain(..consumed);
                req
            }
            Ok(None) => {
                if buf.len() > 8192 {
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

        let keep_alive = req
            .header("connection")
            .map(|v| v.to_ascii_lowercase() == "keep-alive")
            .unwrap_or(false);

        let w = writer.take().expect("writer should be available");
        let guard = router.read().await;
        let (handler, _params) = guard.route(&req.method, &req.path.clone());
        let handler = handler.clone();
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
