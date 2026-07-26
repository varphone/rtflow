pub mod handler;
pub mod mux;

use crate::http::Handler;
use crate::media::AccessUnit;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Create an HTTP-FLV handler for a given mount.
///
/// Leaks a receiver so the broadcast channel always has at least one
/// subscriber, preventing writes from being rejected.  Each client gets
/// a fresh receiver via `Sender::subscribe()`.
pub fn create_flv_handler(
    mount: &crate::server::MountHandle,
) -> Handler {
    let frame_sender = mount.frame_sender().clone();
    let description = mount.description.clone();
    let mount_clone = mount.clone();

    // Keep one receiver alive for the server's lifetime.
    let _ = Box::leak(Box::new(frame_sender.subscribe()));

    Arc::new(move |_req, writer| {
        let desc = description.clone();
        let rx = frame_sender.subscribe();
        let fmtp = mount_clone.announce_fmtp();
        Box::pin(async move {
            handler::serve_flv_stream(_req, writer, desc, rx, fmtp).await
        })
    })
}
