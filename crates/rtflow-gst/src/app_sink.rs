//! Push mode: AppSink callback delivers frames to LocalPublisher.
//!
//! The GStreamer pipeline runs continuously. When a new sample arrives
//! at the AppSink, it is zero-copy converted to Bytes and pushed into
//! the RTSP mount's broadcast channel.

use super::buffer::buffer_to_bytes;
use rtflow_media::{AccessUnit, Codec};
use rtflow_mount::LocalPublisher;
use gstreamer as gst;
use gstreamer_app as gst_app;
use std::sync::Arc;
use std::time::Duration;

/// Set up an AppSink callback for push mode.
///
/// Each new sample is zero-copy converted and pushed to the publisher.
pub fn setup_push_callback(
    appsink: &gst_app::AppSink,
    publisher: Arc<LocalPublisher>,
    track_id: u8,
    codec: Codec,
    clock_rate: u32,
) {
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;

                let (data, meta) = buffer_to_bytes(buffer)?;

                let pts = meta
                    .pts
                    .map(|t| Duration::from_nanos(t.nseconds()))
                    .unwrap_or_default();

                let rtp_timestamp = rtflow_media::clock::duration_to_ticks(pts, clock_rate) as u32;

                let unit = AccessUnit {
                    track_id,
                    codec,
                    pts,
                    dts: meta.dts.map(|t| Duration::from_nanos(t.nseconds())),
                    rtp_timestamp,
                    is_keyframe: meta.is_keyframe,
                    data,
                };

                match publisher.try_push(unit) {
                    Ok(_) => Ok(gst::FlowSuccess::Ok),
                    Err(_) => Err(gst::FlowError::Error),
                }
            })
            .build(),
    );
}
