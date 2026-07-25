//! GStreamer Buffer to Bytes zero-copy mapping.
//!
//! Uses `Buffer::into_mapped_buffer_readable()` + `Bytes::from_owner()`
//! for 0-memcpy data transfer from GStreamer to rtflow.

use bytes::Bytes;
use gstreamer::{Buffer, BufferFlags, ClockTime};

/// Metadata extracted from a GStreamer Buffer before consuming it.
#[derive(Clone, Debug)]
pub struct BufferMeta {
    pub pts: Option<ClockTime>,
    pub dts: Option<ClockTime>,
    pub size: usize,
    pub is_keyframe: bool,
}

/// Convert a GStreamer Buffer into zero-copy Bytes.
///
/// Consumes the Buffer, extracts metadata, and returns a `Bytes` that
/// references the same memory via `MappedBuffer<Readable>`.
/// No memcpy occurs — fan-out to N subscribers only increments ref counts.
pub fn buffer_to_bytes(buffer: Buffer) -> Result<(Bytes, BufferMeta), gstreamer::FlowError> {
    let meta = BufferMeta {
        pts: buffer.pts(),
        dts: buffer.dts(),
        size: buffer.size(),
        is_keyframe: !buffer.flags().contains(BufferFlags::DELTA_UNIT),
    };

    let mapped = buffer
        .into_mapped_buffer_readable()
        .map_err(|_| gstreamer::FlowError::Error)?;
    let bytes = Bytes::from_owner(mapped);

    Ok((bytes, meta))
}
