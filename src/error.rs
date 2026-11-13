//! Stable error classification for retry, protocol mapping and telemetry.
use std::io;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ErrorCode {
    InvalidProtocol,
    InvalidMedia,
    InvalidState,
    Timeout,
    ResourceExhausted,
    Unauthorized,
    Forbidden,
    Unsupported,
    UpstreamUnavailable,
    Io,
    Closed,
    Internal,
    #[cfg(feature = "srtp")]
    SrtpAuthenticationFailed,
    #[cfg(feature = "srtp")]
    SrtpReplayDetected,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RetryClass {
    Never,
    Immediate,
    Backoff,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Operation {
    Unknown,
    Accept,
    Connect,
    ReadControl,
    WriteControl,
    ParseControl,
    SetupTransport,
    ReadMedia,
    WriteMedia,
    Authenticate,
    Authorize,
    RegisterMount,
    ProxyConnect,
    PipelineProcess,
    Shutdown,
    #[cfg(feature = "srtp")]
    SrtpProtect,
    #[cfg(feature = "srtp")]
    SrtpUnprotect,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid RTSP message: {0}")]
    Rtsp(String),
    #[error("invalid media payload: {0}")]
    Media(String),
    #[error("HTTP/WebSocket error: {0}")]
    Http(String),
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[cfg(feature = "tls")]
    #[error("TLS error: {0}")]
    Tls(String),
    #[error("session closed")]
    Closed,
    #[error("resource limit exceeded: {0}")]
    Limit(&'static str),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("authentication failed")]
    Unauthorized,
    /// Non-2xx response to an RTSP control request.
    #[error("{method} returned {status}")]
    RtspStatus { method: &'static str, status: u16 },
    #[cfg(feature = "srtp")]
    #[error("SRTP error: {0}")]
    Srtp(String),
    #[cfg(feature = "rtmp")]
    #[error("RTMP error: {0}")]
    Rtmp(String),
    #[cfg(feature = "srt")]
    #[error("SRT error: {0}")]
    Srt(String),
    #[cfg(feature = "webrtc")]
    #[error("WebRTC error: {0}")]
    Webrtc(String),
}

impl Error {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Rtsp(_) => ErrorCode::InvalidProtocol,
            Self::Media(_) => ErrorCode::InvalidMedia,
            Self::Http(_) => ErrorCode::InvalidProtocol,
            Self::InvalidConfig(_) => ErrorCode::InvalidState,
            Self::Unsupported(_) => ErrorCode::Unsupported,
            Self::Io(error) if error.kind() == io::ErrorKind::TimedOut => ErrorCode::Timeout,
            #[cfg(feature = "tls")]
            Self::Io(_) | Self::Tls(_) => ErrorCode::Io,
            #[cfg(not(feature = "tls"))]
            Self::Io(_) => ErrorCode::Io,
            Self::Closed => ErrorCode::Closed,
            Self::Limit(_) => ErrorCode::ResourceExhausted,
            Self::NotFound(_) | Self::Conflict(_) => ErrorCode::InvalidState,
            Self::Unauthorized => ErrorCode::Unauthorized,
            Self::RtspStatus { status: 401, .. } => ErrorCode::Unauthorized,
            Self::RtspStatus { status: 403, .. } => ErrorCode::Forbidden,
            Self::RtspStatus { status: 404, .. } => ErrorCode::InvalidState,
            Self::RtspStatus { status: 453, .. } => ErrorCode::ResourceExhausted,
            Self::RtspStatus { status: 461, .. } => ErrorCode::Unsupported,
            Self::RtspStatus {
                status: 500..=599, ..
            } => ErrorCode::UpstreamUnavailable,
            Self::RtspStatus { .. } => ErrorCode::InvalidProtocol,
            #[cfg(feature = "srtp")]
            Self::Srtp(_) => ErrorCode::InvalidMedia,
            #[cfg(feature = "rtmp")]
            Self::Rtmp(_) => ErrorCode::InvalidProtocol,
            #[cfg(feature = "srt")]
            Self::Srt(_) => ErrorCode::InvalidProtocol,
            #[cfg(feature = "webrtc")]
            Self::Webrtc(_) => ErrorCode::InvalidProtocol,
        }
    }

    pub fn retry_class(&self) -> RetryClass {
        match self {
            Self::Io(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::TimedOut
                ) =>
            {
                RetryClass::Backoff
            }
            Self::Closed => RetryClass::Immediate,
            Self::RtspStatus {
                status: 500..=599 | 408 | 453 | 461,
                ..
            } => RetryClass::Backoff,
            Self::RtspStatus { .. } => RetryClass::Never,
            #[cfg(feature = "srtp")]
            Self::Srtp(_) => RetryClass::Never,
            _ => RetryClass::Never,
        }
    }

    pub fn rtsp_status(&self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::RtspStatus { status, .. } => *status,
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 455,
            Self::Limit(_) => 453,
            Self::Rtsp(_) | Self::Media(_) => 400,
            Self::Http(_) | Self::InvalidConfig(_) => 500,
            Self::Unsupported(_) => 501,
            #[cfg(feature = "srtp")]
            Self::Srtp(_) => 400,
            #[cfg(feature = "rtmp")]
            Self::Rtmp(_) => 400,
            #[cfg(feature = "srt")]
            Self::Srt(_) => 400,
            #[cfg(feature = "webrtc")]
            Self::Webrtc(_) => 400,
            #[cfg(feature = "tls")]
            Self::Closed | Self::Io(_) | Self::Tls(_) => 500,
            #[cfg(not(feature = "tls"))]
            Self::Closed | Self::Io(_) => 500,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn errors_have_stable_operational_metadata() {
        let limit = Error::Limit("sessions");
        assert_eq!(limit.code(), ErrorCode::ResourceExhausted);
        assert_eq!(limit.retry_class(), RetryClass::Never);
        assert_eq!(limit.rtsp_status(), 453);
        let reset = Error::Io(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
        assert_eq!(reset.code(), ErrorCode::Io);
        assert_eq!(reset.retry_class(), RetryClass::Backoff);
        // Domain-specific variants map to their own codes/statuses.
        assert_eq!(
            Error::Http("bad request".into()).code(),
            ErrorCode::InvalidProtocol
        );
        assert_eq!(Error::Http("bad request".into()).rtsp_status(), 500);
        assert_eq!(
            Error::InvalidConfig("bad CIDR".into()).code(),
            ErrorCode::InvalidState
        );
        assert_eq!(
            Error::Unsupported("ipv6 multicast".into()).code(),
            ErrorCode::Unsupported
        );
        assert_eq!(
            Error::Unsupported("ipv6 multicast".into()).rtsp_status(),
            501
        );
        // Typed RTSP status errors carry stable operational metadata.
        let unauthorized = Error::RtspStatus {
            method: "OPTIONS",
            status: 401,
        };
        assert_eq!(unauthorized.code(), ErrorCode::Unauthorized);
        assert_eq!(unauthorized.retry_class(), RetryClass::Never);
        assert_eq!(unauthorized.rtsp_status(), 401);
        let unsupported_transport = Error::RtspStatus {
            method: "SETUP",
            status: 461,
        };
        assert_eq!(unsupported_transport.code(), ErrorCode::Unsupported);
        assert_eq!(unsupported_transport.retry_class(), RetryClass::Backoff);
        assert_eq!(unsupported_transport.rtsp_status(), 461);
        let server_error = Error::RtspStatus {
            method: "SETUP",
            status: 500,
        };
        assert_eq!(server_error.code(), ErrorCode::UpstreamUnavailable);
        assert_eq!(server_error.retry_class(), RetryClass::Backoff);
        assert_eq!(server_error.rtsp_status(), 500);
        let bandwidth = Error::RtspStatus {
            method: "PLAY",
            status: 453,
        };
        assert_eq!(bandwidth.code(), ErrorCode::ResourceExhausted);
        assert_eq!(bandwidth.retry_class(), RetryClass::Backoff);
        let permanent = Error::RtspStatus {
            method: "PLAY",
            status: 455,
        };
        assert_eq!(permanent.retry_class(), RetryClass::Never);
    }
}
