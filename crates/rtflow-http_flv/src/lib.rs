pub mod handler;
pub mod mux;
pub mod web;
#[cfg(feature = "ws-flv")]
pub mod ws;

use rtflow_http::Handler;
use std::sync::Arc;

/// Create an HTTP-FLV handler for a given mount.
///
/// Each client receives the cached decodable stream start, followed by a
/// fresh subscription to live frames.
pub fn create_flv_handler(mount: &rtflow_mount::MountHandle) -> Handler {
    let description = mount.description();
    let mount_clone = mount.clone();
    // Track active FLV viewer count in the global registry (if mounted).
    #[cfg(feature = "metrics")]
    let metrics = rtflow_core::metrics::registry_from_sink(&mount.mount().event_sink);

    Arc::new(move |_req, writer| {
        let desc = description.clone();
        let (rx, startup) = mount_clone.subscribe_with_startup();
        let fmtp_source = mount_clone.clone();
        #[cfg(feature = "metrics")]
        let metrics = metrics.clone();
        Box::pin(async move {
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &metrics {
                use std::sync::atomic::Ordering;
                metrics.flv_streams_active.fetch_add(1, Ordering::Relaxed);
            }
            let result =
                handler::serve_flv_stream(_req, writer, desc, rx, startup, fmtp_source).await;
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &metrics {
                use std::sync::atomic::Ordering;
                metrics.flv_streams_active.fetch_sub(1, Ordering::Relaxed);
            }
            result
        })
    })
}
