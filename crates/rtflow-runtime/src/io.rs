//! Transport-independent RTSP control connection.
use bytes::{Buf, BytesMut};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

pub type ControlReadHalf = tokio::io::ReadHalf<ControlConnection>;
pub type ControlWriteHalf = tokio::io::WriteHalf<ControlConnection>;

pub trait AsyncControlIo: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T> AsyncControlIo for T where T: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

pub struct ControlConnection {
    io: Box<dyn AsyncControlIo>,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
    read_ahead: BytesMut,
}

impl ControlConnection {
    pub fn new(io: impl AsyncControlIo, local_addr: SocketAddr, peer_addr: SocketAddr) -> Self {
        Self {
            io: Box::new(io),
            local_addr,
            peer_addr,
            read_ahead: BytesMut::with_capacity(4096),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Buffered bytes received ahead of the current frame (framing helpers).
    #[doc(hidden)]
    pub fn read_ahead_mut(&mut self) -> &mut BytesMut {
        &mut self.read_ahead
    }

    /// Read raw transport bytes, bypassing TLS record framing where needed.
    #[doc(hidden)]
    pub async fn read_transport(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.io.read(buffer).await
    }
}

impl AsyncRead for ControlConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.read_ahead.is_empty() && buffer.remaining() > 0 {
            let length = self.read_ahead.len().min(buffer.remaining());
            buffer.put_slice(&self.read_ahead[..length]);
            self.read_ahead.advance(length);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut *self.io).poll_read(context, buffer)
    }
}

impl AsyncWrite for ControlConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.io).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.io).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.io).poll_shutdown(context)
    }
}
