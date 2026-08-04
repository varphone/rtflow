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

#[derive(Clone)]
pub struct DigestNonceManager {
    inner: Arc<NonceInner>,
}

struct NonceInner {
    secret: Vec<u8>,
    ttl: Duration,
    maximum_entries: usize,
    counter: AtomicU64,
    replay: Mutex<HashMap<ReplayKey, ReplayState>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ReplayKey {
    nonce: String,
    username: String,
    cnonce: String,
}

struct ReplayState {
    highest_count: u32,
    updated_at: Instant,
}

impl DigestNonceManager {
    pub fn new(secret: impl Into<Vec<u8>>, ttl: Duration, maximum_entries: usize) -> Result<Self> {
        let secret = secret.into();
        if secret.len() < 32 {
            return Err(Error::Limit("Digest nonce secret"));
        }
        if ttl == Duration::ZERO || maximum_entries == 0 {
            return Err(Error::Limit("Digest nonce policy"));
        }
        Ok(Self {
            inner: Arc::new(NonceInner {
                secret,
                ttl,
                maximum_entries,
                counter: AtomicU64::new(1),
                replay: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn issue(&self) -> String {
        self.issue_at(unix_seconds())
    }

    pub fn validate(&self, nonce: &str) -> bool {
        self.validate_at(nonce, unix_seconds())
    }

    fn issue_at(&self, issued_at: u64) -> String {
        let counter = self.inner.counter.fetch_add(1, Ordering::Relaxed);
        let mut authenticated = [0u8; 16];
        authenticated[..8].copy_from_slice(&issued_at.to_be_bytes());
        authenticated[8..].copy_from_slice(&counter.to_be_bytes());
        let signature = hmac_sha256(&self.inner.secret, &authenticated);
        let mut raw = [0u8; 48];
        raw[..16].copy_from_slice(&authenticated);
        raw[16..].copy_from_slice(&signature);
        URL_SAFE_NO_PAD.encode(raw)
    }

    fn validate_at(&self, nonce: &str, now: u64) -> bool {
        let Ok(raw) = URL_SAFE_NO_PAD.decode(nonce) else {
            return false;
        };
        if raw.len() != 48 {
            return false;
        }
        let mut authenticated = [0u8; 16];
        authenticated.copy_from_slice(&raw[..16]);
        let issued_at = u64::from_be_bytes([
            authenticated[0],
            authenticated[1],
            authenticated[2],
            authenticated[3],
            authenticated[4],
            authenticated[5],
            authenticated[6],
            authenticated[7],
        ]);
        if issued_at > now.saturating_add(5)
            || now.saturating_sub(issued_at) > self.inner.ttl.as_secs().max(1)
        {
            return false;
        }
        let expected = hmac_sha256(&self.inner.secret, &authenticated);
        bool::from(expected.ct_eq(&raw[16..]))
    }

    fn record_nonce_count(
        &self,
        nonce: &str,
        username: &str,
        cnonce: &str,
        nonce_count: u32,
    ) -> bool {
        let mut replay = self
            .inner
            .replay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expiration = self.inner.ttl;
        // Retain is O(n) under the lock; only run it when the table has
        // grown enough. Stale entries merely cause conservative rejections
        // until the next sweep, which is the safe direction.
        if replay.len() >= 1024 {
            replay.retain(|_, state| state.updated_at.elapsed() <= expiration);
        }
        let key = ReplayKey {
            nonce: nonce.to_owned(),
            username: username.to_owned(),
            cnonce: cnonce.to_owned(),
        };
        if let Some(state) = replay.get_mut(&key) {
            if nonce_count <= state.highest_count {
                return false;
            }
            state.highest_count = nonce_count;
            state.updated_at = Instant::now();
            return true;
        }
        if replay.len() >= self.inner.maximum_entries
            && let Some(oldest) = replay
                .iter()
                .min_by_key(|(_, state)| state.updated_at)
                .map(|(key, _)| key.clone())
        {
            replay.remove(&oldest);
        }
        replay.insert(
            key,
            ReplayState {
                highest_count: nonce_count,
                updated_at: Instant::now(),
            },
        );
        true
    }
}

pub struct ManagedDigestAuthenticator {
    credentials: Credentials,
    realm: String,
    nonces: DigestNonceManager,
}

impl ManagedDigestAuthenticator {
    pub fn new(
        credentials: Credentials,
        realm: impl Into<String>,
        secret: impl Into<Vec<u8>>,
        nonce_ttl: Duration,
    ) -> Result<Self> {
        Ok(Self {
            credentials,
            realm: realm.into(),
            nonces: DigestNonceManager::new(secret, nonce_ttl, 16_384)?,
        })
    }

    pub fn nonce_manager(&self) -> DigestNonceManager {
        self.nonces.clone()
    }
}

impl Authenticator for ManagedDigestAuthenticator {
    fn authorize<'a>(&'a self, request: AuthRequest<'a>) -> BoxFuture<'a, AuthDecision> {
        Box::pin(async move {
            let allowed = request.authorization.is_some_and(|header| {
                verify_digest_managed(
                    header,
                    request.method,
                    &self.credentials,
                    &self.realm,
                    &self.nonces,
                )
                .unwrap_or(false)
            });
            if allowed {
                AuthDecision::Allow
            } else {
                AuthDecision::DigestChallenge {
                    realm: self.realm.clone(),
                    nonce: self.nonces.issue(),
                }
            }
        })
    }
}

pub fn basic_authorization(credentials: &Credentials) -> String {
    let value = STANDARD.encode(format!("{}:{}", credentials.username, credentials.password));
    format!("Basic {value}")
}

pub fn authorization_from_challenge(
    challenge: &str,
    method: Method,
    uri: &str,
    credentials: &Credentials,
) -> Result<String> {
    if challenge
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Basic"))
    {
        return Ok(basic_authorization(credentials));
    }
    let value = challenge
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("Digest "))
        .map(|_| &challenge[7..])
        .ok_or_else(|| Error::Rtsp("unsupported authentication challenge".into()))?;
    let fields = parse_fields(value)?;
    let realm = required(&fields, "realm")?;
    let nonce = required(&fields, "nonce")?;
    let algorithm = fields.get("algorithm").map(String::as_str).unwrap_or("MD5");
    if !algorithm.eq_ignore_ascii_case("MD5") {
        return Err(Error::Rtsp(format!(
            "unsupported Digest algorithm {algorithm}"
        )));
    }
    let ha1 = md5_hex(&format!(
        "{}:{realm}:{}",
        credentials.username, credentials.password
    ));
    let ha2 = md5_hex(&format!("{}:{uri}", method.as_str()));
    let mut response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
    let mut suffix = String::new();
    if fields
        .get("qop")
        .is_some_and(|value| value.split(',').any(|item| item.trim() == "auth"))
    {
        let nc = "00000001";
        let cnonce = md5_hex(&format!("{}:{nonce}:{uri}", credentials.username));
        response = md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
        suffix = format!(", qop=auth, nc={nc}, cnonce=\"{cnonce}\"");
    }
    Ok(format!(
        "Digest username=\"{}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\", algorithm=MD5{suffix}",
        credentials.username
    ))
}

pub fn verify_basic(header: &str, credentials: &Credentials) -> bool {
    let Some(value) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(raw) = STANDARD.decode(value.trim()) else {
        return false;
    };
    let expected = format!("{}:{}", credentials.username, credentials.password);
    constant_time_eq(&raw, expected.as_bytes())
}

pub fn verify_digest(
    header: &str,
    method: Method,
    credentials: &Credentials,
    realm: &str,
) -> Result<bool> {
    let Some(value) = header.strip_prefix("Digest ") else {
        return Ok(false);
    };
    let fields = parse_fields(value)?;
    let username = required(&fields, "username")?;
    let nonce = required(&fields, "nonce")?;
    let uri = required(&fields, "uri")?;
    let response = required(&fields, "response")?;
    if !constant_time_eq(username.as_bytes(), credentials.username.as_bytes())
        || !fields
            .get("realm")
            .is_some_and(|value| constant_time_eq(value.as_bytes(), realm.as_bytes()))
    {
        return Ok(false);
    }
    let ha1 = md5_hex(&format!(
        "{}:{realm}:{}",
        credentials.username, credentials.password
    ));
    let ha2 = md5_hex(&format!("{}:{uri}", method.as_str()));
    let expected = if let (Some(qop), Some(nc), Some(cnonce)) =
        (fields.get("qop"), fields.get("nc"), fields.get("cnonce"))
    {
        md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"))
    } else {
        md5_hex(&format!("{ha1}:{nonce}:{ha2}"))
    };
    Ok(constant_time_eq(
        expected.as_bytes(),
        response.to_ascii_lowercase().as_bytes(),
    ))
}

pub fn verify_digest_managed(
    header: &str,
    method: Method,
    credentials: &Credentials,
    realm: &str,
    nonces: &DigestNonceManager,
) -> Result<bool> {
    if !verify_digest(header, method, credentials, realm)? {
        return Ok(false);
    }
    let Some(value) = header.strip_prefix("Digest ") else {
        return Ok(false);
    };
    let fields = parse_fields(value)?;
    let nonce = required(&fields, "nonce")?;
    let username = required(&fields, "username")?;
    let qop = required(&fields, "qop")?;
    let nonce_count = required(&fields, "nc")?;
    let cnonce = required(&fields, "cnonce")?;
    if qop != "auth" || !nonces.validate(nonce) {
        return Ok(false);
    }
    let nonce_count = u32::from_str_radix(nonce_count, 16)
        .map_err(|_| Error::Rtsp("invalid Digest nonce count".into()))?;
    if nonce_count == 0 {
        return Ok(false);
    }
    Ok(nonces.record_nonce_count(nonce, username, cnonce, nonce_count))
}

fn required<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> Result<&'a String> {
    fields
        .get(key)
        .ok_or_else(|| Error::Rtsp(format!("Digest missing {key}")))
}

fn parse_fields(value: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut start = 0;
    let mut quoted = false;
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'"' {
            quoted = !quoted;
        } else if *byte == b',' && !quoted {
            insert_field(&mut out, &value[start..index])?;
            start = index + 1;
        }
    }
    if quoted {
        return Err(Error::Rtsp("unterminated Digest quote".into()));
    }
    insert_field(&mut out, &value[start..])?;
    Ok(out)
}

fn insert_field(out: &mut BTreeMap<String, String>, item: &str) -> Result<()> {
    let (key, value) = item
        .trim()
        .split_once('=')
        .ok_or_else(|| Error::Rtsp("malformed authentication field".into()))?;
    out.insert(
        key.trim().to_ascii_lowercase(),
        value.trim().trim_matches('"').to_owned(),
    );
    Ok(())
}

fn md5_hex(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Md5::digest(value.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for &byte in digest.iter() {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut normalized = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let digest = Sha256::digest(key);
        normalized[..32].copy_from_slice(&digest);
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36; BLOCK_SIZE];
    let mut outer_pad = [0x5c; BLOCK_SIZE];
    for index in 0..BLOCK_SIZE {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    ShaDigest::update(&mut inner, inner_pad);
    ShaDigest::update(&mut inner, data);
    let inner = ShaDigest::finalize(inner);
    let mut outer = Sha256::new();
    ShaDigest::update(&mut outer, outer_pad);
    ShaDigest::update(&mut outer, inner);
    let digest = ShaDigest::finalize(outer);
    let mut output = [0u8; 32];
    output.copy_from_slice(&digest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            username: "u".into(),
            password: "p".into(),
        }
    }

    #[test]
    fn basic() {
        let credentials = credentials();
        assert_eq!(basic_authorization(&credentials), "Basic dTpw");
        assert!(verify_basic("Basic dTpw", &credentials));
    }

    #[test]
    fn digest() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        let credentials = credentials();
        let header = authorization_from_challenge(
            "Digest realm=\"r\", nonce=\"n\"",
            Method::Options,
            "/x",
            &credentials,
        )
        .unwrap();
        assert!(verify_digest(&header, Method::Options, &credentials, "r").unwrap());
        let bad = Credentials {
            username: "u".into(),
            password: "bad".into(),
        };
        assert!(!verify_digest(&header, Method::Options, &bad, "r").unwrap());
    }

    #[test]
    fn digest_qop_auth() {
        let credentials = credentials();
        let header = authorization_from_challenge(
            "Digest realm=\"r\", nonce=\"n\", qop=\"auth,auth-int\"",
            Method::Describe,
            "rtsp://localhost/x",
            &credentials,
        )
        .unwrap();
        assert!(verify_digest(&header, Method::Describe, &credentials, "r").unwrap());
    }

    #[test]
    fn signed_nonce_expires_and_detects_tampering() {
        let manager = DigestNonceManager::new(
            b"0123456789abcdef0123456789abcdef".to_vec(),
            Duration::from_secs(30),
            8,
        )
        .unwrap();
        let nonce = manager.issue_at(100);
        assert!(manager.validate_at(&nonce, 100));
        assert!(manager.validate_at(&nonce, 130));
        assert!(!manager.validate_at(&nonce, 131));
        let mut tampered = nonce.into_bytes();
        tampered[20] = if tampered[20] == b'a' { b'b' } else { b'a' };
        assert!(!manager.validate_at(std::str::from_utf8(&tampered).unwrap(), 100));
    }

    #[test]
    fn managed_digest_rejects_replayed_nonce_count() {
        let credentials = credentials();
        let manager = DigestNonceManager::new(
            b"0123456789abcdef0123456789abcdef".to_vec(),
            Duration::from_secs(30),
            8,
        )
        .unwrap();
        let nonce = manager.issue();
        let challenge =
            format!("Digest realm=\"r\", nonce=\"{nonce}\", algorithm=MD5, qop=\"auth\"");
        let header = authorization_from_challenge(
            &challenge,
            Method::Describe,
            "rtsp://example/live",
            &credentials,
        )
        .unwrap();
        assert!(
            verify_digest_managed(&header, Method::Describe, &credentials, "r", &manager).unwrap()
        );
        assert!(
            !verify_digest_managed(&header, Method::Describe, &credentials, "r", &manager).unwrap()
        );
    }

    #[test]
    fn credentials_debug_redacts_password() {
        let rendered = format!("{:?}", credentials());
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("password: \"p\""));
    }
}
