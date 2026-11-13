//! Compatibility facade for the RTSP control plane.
//!
//! The canonical home is [`crate::rtsp`]; this module keeps the pre-0.3 paths
//! (`rtflow::protocol::sdp`, `rtflow::protocol::rtsp`, …) working. Deprecated
//! in 0.3 and removed in 0.4.
pub use crate::rtsp::message as rtsp;
pub use crate::rtsp::{auth, range, sdp, token_auth, transport};
