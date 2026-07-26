pub mod handler;
pub mod mux;

use crate::http::Handler;
use std::sync::Arc;

/// Create an HTTP-FLV handler for a given mount.
///
/// Each client receives the cached decodable stream start, followed by a
/// fresh subscription to live frames.
pub fn create_flv_handler(mount: &crate::server::MountHandle) -> Handler {
    let description = mount.description.clone();
    let mount_clone = mount.clone();

    Arc::new(move |_req, writer| {
        let desc = description.clone();
        let (rx, startup) = mount_clone.subscribe_with_startup();
        let fmtp_source = mount_clone.clone();
        Box::pin(async move {
            handler::serve_flv_stream(_req, writer, desc, rx, startup, fmtp_source).await
        })
    })
}
