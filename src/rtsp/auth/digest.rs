use super::verify::*;
use super::*;

#[derive(Clone)]
pub struct DigestNonceManager {
    inner: Arc<NonceInner>,
}

pub(super) struct NonceInner {
    secret: Vec<u8>,
    ttl: Duration,
    maximum_entries: usize,
    counter: AtomicU64,
    replay: Mutex<HashMap<ReplayKey, ReplayState>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct ReplayKey {
    nonce: String,
    username: String,
    cnonce: String,
}

pub(super) struct ReplayState {
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

    pub(super) fn issue_at(&self, issued_at: u64) -> String {
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

    pub(super) fn validate_at(&self, nonce: &str, now: u64) -> bool {
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

    pub(super) fn record_nonce_count(
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
