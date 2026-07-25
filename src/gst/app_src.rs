//! Pull mode: AppSrc with need-data/enough-data flow control.

use gstreamer as gst;
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Pull-mode flow controller.
pub struct PullController {
    app_src: gst_app::AppSrc,
    needs_data: Arc<AtomicBool>,
}

impl PullController {
    /// Create a new pull controller from an AppSrc element.
    pub fn new(app_src: gst_app::AppSrc) -> Self {
        let needs_data = Arc::new(AtomicBool::new(false));
        let flag = needs_data.clone();

        app_src.set_callbacks(
            gst_app::AppSrcCallbacks::builder()
                .need_data(move |_element, _size| {
                    flag.store(true, Ordering::Release);
                })
                .enough_data({
                    let flag = needs_data.clone();
                    move |_element| {
                        flag.store(false, Ordering::Release);
                    }
                })
                .build(),
        );

        Self { app_src, needs_data }
    }

    /// Returns true if the AppSrc is requesting data.
    pub fn is_ready(&self) -> bool {
        self.needs_data.load(Ordering::Acquire)
    }

    /// Push an encoded frame into the AppSrc.
    pub fn push_frame(
        &self,
        data: &[u8],
        pts: Duration,
        is_keyframe: bool,
    ) -> Result<(), gst::FlowError> {
        let mut buffer = gst::Buffer::with_size(data.len())
            .map_err(|_| gst::FlowError::Error)?;

        {
            let buf_ref = buffer.get_mut().ok_or(gst::FlowError::Error)?;
            buf_ref.set_pts(gst::ClockTime::from_nseconds(pts.as_nanos() as u64));
            if !is_keyframe {
                buf_ref.set_flags(gst::BufferFlags::DELTA_UNIT);
            }

            let mut mapped = buf_ref.map_writable().map_err(|_| gst::FlowError::Error)?;
            mapped.copy_from_slice(data);
        }

        self.app_src
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|_| gst::FlowError::Error)
    }

    /// Push a pre-built GStreamer Buffer directly (for zero-copy input).
    pub fn push_buffer(&self, buffer: gst::Buffer) -> Result<(), gst::FlowError> {
        self.app_src
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|_| gst::FlowError::Error)
    }

    /// Send EOS to the AppSrc.
    pub fn end_of_stream(&self) -> Result<(), gst::FlowError> {
        self.app_src
            .end_of_stream()
            .map(|_| ())
            .map_err(|_| gst::FlowError::Error)
    }
}
