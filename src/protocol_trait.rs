//! Protocol-agnostic ingestion and output traits.
//!
//! Each streaming protocol (RTMP, SRT, WebRTC, etc.) implements one or both of
//! these traits to plug into the shared mount infrastructure.
use crate::Result;
use crate::mount::MountHandle;
use tokio_util::sync::CancellationToken;

/// Ingest (publish) protocol adapter.
///
/// Implementations decode protocol-specific framing into [`AccessUnit`] values
/// and push them into a mount via [`LocalPublisher`].
///
/// [`AccessUnit`]: crate::media::AccessUnit
/// [`LocalPublisher`]: crate::mount::LocalPublisher
pub trait IngestProtocol: Send + Sync {
    /// Human-readable protocol name (e.g. "rtmp", "srt", "whip").
    fn name(&self) -> &str;

    /// Run the ingest loop until cancellation or fatal error.
    fn ingest(
        &self,
        mount: &MountHandle,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// Output (playback) protocol adapter.
///
/// Implementations subscribe to a mount and encode [`AccessUnit`] values into
/// protocol-specific framing for downstream consumers.
///
/// [`AccessUnit`]: crate::media::AccessUnit
pub trait OutputProtocol: Send + Sync {
    /// Human-readable protocol name (e.g. "rtmp", "hls", "dash", "whep").
    fn name(&self) -> &str;

    /// Serve one output session until cancellation or fatal error.
    fn serve(
        &self,
        mount: &MountHandle,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}
