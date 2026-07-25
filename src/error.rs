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
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid RTSP message: {0}")]
    Rtsp(String),
    #[error("invalid media payload: {0}")]
    Media(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
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
}

impl Error {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Rtsp(_) => ErrorCode::InvalidProtocol,
            Self::Media(_) => ErrorCode::InvalidMedia,
            Self::Io(error) if error.kind() == io::ErrorKind::TimedOut => ErrorCode::Timeout,
            Self::Io(_) | Self::Tls(_) => ErrorCode::Io,
            Self::Closed => ErrorCode::Closed,
            Self::Limit(_) => ErrorCode::ResourceExhausted,
            Self::NotFound(_) | Self::Conflict(_) => ErrorCode::InvalidState,
            Self::Unauthorized => ErrorCode::Unauthorized,
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
            _ => RetryClass::Never,
        }
    }

    pub fn rtsp_status(&self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 455,
            Self::Limit(_) => 453,
            Self::Rtsp(_) | Self::Media(_) => 400,
            Self::Closed | Self::Io(_) | Self::Tls(_) => 500,
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
    }
}
