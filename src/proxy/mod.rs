//! Compatibility facade for upstream proxy configuration, state and timing.
//!
//! The canonical home moved to [`crate::mount::upstream`] and
//! [`crate::mount::continuity`] so the mount registry can own proxy config and
//! startup-cache state without depending on the proxy runtime. This module
//! keeps the pre-0.3 `rtflow::proxy::{…}` paths working; deprecated in 0.3.

pub use crate::mount::continuity::*;
pub use crate::mount::upstream::*;
pub use crate::transport::UpstreamTransport;
