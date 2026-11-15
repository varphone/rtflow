//! Errors, identifiers, control-plane events and lock-free metric counters.
//!
//! This is the bottom layer of the workspace: it depends on nothing else in
//! `rtflow` and every other crate depends on it.
pub mod error;
pub mod identity;
pub mod metrics;
pub mod observe;

pub use error::{Error, ErrorCode, Operation, Result, RetryClass};
