//! Admin credentials, sessions and cookie helpers.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

const SESSION_BYTES: usize = 32;
const CSRF_BYTES: usize = 24;

/// Argon2id password hash (PHC string). The plaintext password is never
/// retained and verification uses the standard Argon2 parameters.
#[derive(Clone)]
pub(crate) struct PasswordDigest {
    phc: String,
}

impl PasswordDigest {
    pub(crate) fn new(password: &str) -> rtflow_core::error::Result<Self> {
        use argon2::{
            Argon2,
            password_hash::{PasswordHasher, SaltString},
        };
        let mut salt_bytes = [0u8; 16];
        getrandom::getrandom(&mut salt_bytes).map_err(|error| {
            rtflow_core::error::Error::InvalidConfig(format!("admin salt: {error}"))
        })?;
        let salt = SaltString::encode_b64(&salt_bytes).map_err(|error| {
            rtflow_core::error::Error::InvalidConfig(format!("admin salt: {error}"))
        })?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|error| {
                rtflow_core::error::Error::InvalidConfig(format!("admin password hash: {error}"))
            })?;
        Ok(Self {
            phc: hash.to_string(),
        })
    }

    pub(crate) fn verify(&self, password: &str) -> bool {
        use argon2::{
            Argon2,
            password_hash::{PasswordHash, PasswordVerifier},
        };
        let Ok(parsed) = PasswordHash::new(&self.phc) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    }
}

pub(crate) fn constant_time_eq(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

pub(crate) fn random_token(bytes: usize) -> rtflow_core::error::Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::getrandom(&mut buffer).map_err(|error| {
        rtflow_core::error::Error::InvalidConfig(format!("admin token: {error}"))
    })?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

pub(crate) struct Session {
    pub(crate) csrf: String,
    pub(crate) expires: Instant,
}

pub(crate) struct Sessions {
    ttl: Duration,
    entries: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn create(&self) -> rtflow_core::error::Result<(String, String)> {
        let token = random_token(SESSION_BYTES)?;
        let csrf = random_token(CSRF_BYTES)?;
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, session| session.expires > now);
        entries.insert(
            token.clone(),
            Session {
                csrf: csrf.clone(),
                expires: now + self.ttl,
            },
        );
        Ok((token, csrf))
    }

    pub(crate) fn get(&self, token: &str) -> Option<String> {
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, session| session.expires > now);
        entries.get(token).map(|session| session.csrf.clone())
    }

    pub(crate) fn remove(&self, token: &str) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(token);
    }
}

pub(crate) fn session_cookie(req: &rtflow_http::HttpRequest, name: &str) -> Option<String> {
    let header = req.header("cookie")?;
    header.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

pub(crate) fn cookie_header(
    name: &str,
    value: &str,
    ttl: Duration,
    secure: bool,
    path: &str,
) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{name}={value}; Path={path}; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        ttl.as_secs()
    )
}

pub(crate) fn clear_cookie(name: &str, path: &str) -> String {
    format!("{name}=; Path={path}; HttpOnly; SameSite=Strict; Max-Age=0")
}
