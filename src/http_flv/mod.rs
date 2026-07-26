pub mod handler;
pub mod mux;

use crate::http::Handler;
use crate::media::{AccessUnit, MediaDescription};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Create an HTTP-FLV handler for a given mount.
///
/// Uses `frame_sender.subscribe()` to create a fresh receiver per request,
/// ensuring each client receives frames published after it connects.
pub fn create_flv_handler(
    description: MediaDescription,
    frame_sender: broadcast::Sender<AccessUnit>,
) -> Handler {
    Arc::new(move |_req, writer| {
        let desc = description.clone();
        let rx = frame_sender.subscribe();
        Box::pin(async move {
            handler::serve_flv_stream(_req, writer, desc, rx).await
        })
    })
}
