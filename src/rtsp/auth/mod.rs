use crate::{Error, Result, rtsp::Method};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use futures::future::BoxFuture;
use md5::{Digest, Md5};
use sha2::{Digest as ShaDigest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessRole {
    Read,
    Publish,
}

#[derive(Clone, Debug)]
pub struct AuthRequest<'a> {
    pub method: Method,
    pub path: &'a str,
    pub peer_addr: SocketAddr,
    pub role: AccessRole,
    pub authorization: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub enum AuthDecision {
    Allow,
    Challenge { realm: String },
    DigestChallenge { realm: String, nonce: String },
    Deny,
}

pub trait Authenticator: Send + Sync {
    fn authorize<'a>(&'a self, request: AuthRequest<'a>) -> BoxFuture<'a, AuthDecision>;
}

#[derive(Default)]
pub struct AllowAll;
impl Authenticator for AllowAll {
    fn authorize<'a>(&'a self, _: AuthRequest<'a>) -> BoxFuture<'a, AuthDecision> {
        Box::pin(async { AuthDecision::Allow })
    }
}

mod basic;
mod digest;
#[cfg(test)]
mod tests;
mod verify;

pub use basic::basic_authorization;
pub use digest::{DigestNonceManager, ManagedDigestAuthenticator};
pub use verify::{
    authorization_from_challenge, verify_basic, verify_digest, verify_digest_managed,
};
