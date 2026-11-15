use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncWrite, WriteHalf},
    time::{Sleep, sleep},
};

use rtflow_runtime::io::ControlConnection;

/// Per-connection RTSP write timeout. A peer that stops reading must not
/// pin the session, its peer slot and the shutdown grace indefinitely.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct TimedWriter {
    inner: WriteHalf<ControlConnection>,
    timer: Option<Pin<Box<Sleep>>>,
}

impl TimedWriter {
    pub(crate) fn new(inner: WriteHalf<ControlConnection>) -> Self {
        Self { inner, timer: None }
    }

    fn timer(&mut self) -> &mut Pin<Box<Sleep>> {
        self.timer
            .get_or_insert_with(|| Box::pin(sleep(WRITE_TIMEOUT)))
    }
}

impl AsyncWrite for TimedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.timer().as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "RTSP write timeout",
            )));
        }
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(written)) => {
                this.timer = None;
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.timer().as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "RTSP flush timeout",
            )));
        }
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                this.timer = None;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
