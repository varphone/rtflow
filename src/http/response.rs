use crate::{Error, Result};
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub struct ResponseWriter<W: AsyncWrite + Unpin> {
    inner: W,
    headers_written: bool,
    connection_close: bool,
}

impl<W: AsyncWrite + Unpin> ResponseWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: writer,
            headers_written: false,
            connection_close: false,
        }
    }

    /// Whether the response indicated the connection should be closed.
    pub fn should_close(&self) -> bool {
        self.connection_close
    }

    pub async fn write_headers(&mut self, status: u16, headers: &[(&str, &str)]) -> Result<()> {
        if self.headers_written {
            return Err(Error::Rtsp("HTTP response headers already written".into()));
        }
        let reason = status_reason(status);
        let mut buf = format!("HTTP/1.1 {} {}\r\n", status, reason);
        for (key, value) in headers {
            if key.eq_ignore_ascii_case("connection") && value.eq_ignore_ascii_case("close") {
                self.connection_close = true;
            }
            buf.push_str(key);
            buf.push_str(": ");
            buf.push_str(value);
            buf.push_str("\r\n");
        }
        buf.push_str("\r\n");
        self.inner.write_all(buf.as_bytes()).await?;
        self.headers_written = true;
        Ok(())
    }

    pub async fn write_chunked_headers(
        &mut self,
        content_type: &str,
        extra_headers: &[(&str, &str)],
    ) -> Result<()> {
        let mut headers = vec![
            ("Transfer-Encoding", "chunked"),
            ("Content-Type", content_type),
            ("Cache-Control", "no-cache, no-store"),
            ("Connection", "keep-alive"),
        ];
        for (k, v) in extra_headers {
            headers.push((k, v));
        }
        self.write_headers(200, &headers).await
    }

    pub async fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
        if !self.headers_written {
            return Err(Error::Rtsp("HTTP chunk written before headers".into()));
        }
        if data.is_empty() {
            return Ok(());
        }
        let header = format!("{:x}\r\n", data.len());
        self.inner.write_all(header.as_bytes()).await?;
        self.inner.write_all(data).await?;
        self.inner.write_all(b"\r\n").await?;
        Ok(())
    }

    /// Write raw bytes directly to the stream (no chunked framing).
    /// Use with `Transfer-Encoding: none` / `Connection: close` for streaming.
    pub async fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.inner.write_all(data).await?;
        Ok(())
    }

    pub async fn write_body(&mut self, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
        let content_length = body.len().to_string();
        let headers = vec![
            ("Content-Type", content_type),
            ("Content-Length", content_length.as_str()),
            ("Cache-Control", "no-cache"),
            ("Connection", "close"),
            // HLS playlists and other bodies are consumed cross-origin by
            // browser players (hls.js etc).
            ("Access-Control-Allow-Origin", "*"),
        ];
        self.write_headers(status, &headers).await?;
        self.inner.write_all(body).await?;
        Ok(())
    }

    pub async fn write_json(&mut self, status: u16, json: &str) -> Result<()> {
        self.write_body(status, "application/json", json.as_bytes())
            .await
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await.map_err(Error::from)
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}
