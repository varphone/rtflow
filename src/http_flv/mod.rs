pub mod handler;
pub mod mux;

use crate::http::Handler;
use crate::media::MediaDescription;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Create an HTTP-FLV handler for a given mount path.
pub fn create_flv_handler(
    description: MediaDescription,
    receiver: broadcast::Receiver<crate::media::AccessUnit>,
) -> Handler {
    Arc::new(move |_req, writer| {
        let desc = description.clone();
        let rx = receiver.resubscribe();
        Box::pin(async move {
            handler::serve_flv_stream(_req, writer, desc, rx).await
        })
    })
}
