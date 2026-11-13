//! RTSP control plane: message codec, SDP, ranges, authentication, transport.
//!
//! The public entry points stay available at the crate root (`rtflow::auth`,
//! `rtflow::transport`, `rtflow::token_auth`, `rtflow::multicast`) and the
//! message types remain at `rtflow::rtsp::{Message, Request, Response}`.
pub mod auth;
pub mod message;
pub mod range;
pub mod sdp;
pub mod token_auth;
pub mod transport;

#[cfg(feature = "multicast")]
pub mod multicast;

#[doc(inline)]
pub use message::*;
pub use range::{NptPosition, NptRange, TimelineMode};
