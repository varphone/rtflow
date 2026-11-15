//! RTSP control plane: message codec, SDP, ranges, authentication, transport.
//!
//! The umbrella `rtflow` crate re-exports this module as `rtflow::rtsp`,
//! `rtflow::auth`, `rtflow::transport` and `rtflow::token_auth` so application
//! code keeps working unchanged.
pub mod auth;
pub mod message;
pub mod range;
pub mod sdp;
pub mod token_auth;
pub mod transport;

#[cfg(feature = "multicast")]
pub use rtflow_multicast as multicast;

#[doc(inline)]
pub use message::*;
pub use range::{NptPosition, NptRange, TimelineMode};
pub use transport::{InterleavedFrame, TransportMode, TransportSpec, UdpPair};
pub use transport::UpstreamTransport;
