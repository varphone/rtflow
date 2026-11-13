//! Wire-protocol adapters on top of the mount registry.
//!
//! Every module here translates between a transport-specific framing and the
//! shared [`crate::media::AccessUnit`] stream: HTTP output (HLS/DASH/HTTP-FLV/
//! WS-FLV), RTMP, SRT, WebRTC (WHIP/WHEP) and GStreamer.
#[cfg(feature = "dash")]
pub mod dash;
#[cfg(feature = "gstreamer")]
pub mod gst;
#[cfg(feature = "hls")]
pub mod hls;
#[cfg(feature = "http-flv")]
pub mod http_flv;
#[cfg(feature = "rtmp")]
pub mod rtmp;
#[cfg(feature = "srt")]
pub mod srt;
#[cfg(feature = "webrtc")]
pub mod webrtc;
