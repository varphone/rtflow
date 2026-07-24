use crate::{Error, Result, rtsp::Method};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::future::BoxFuture;
use md5::{Digest, Md5};
use std::{collections::BTreeMap, net::SocketAddr};

#[derive(Clone, Debug)]
pub struct Credentials {
    pub username: String,
    pub password: String,
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
    let Ok(pair) = std::str::from_utf8(&raw) else {
        return false;
    };
    pair.len() == credentials.username.len() + credentials.password.len() + 1
        && pair == format!("{}:{}", credentials.username, credentials.password)
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
    if username != &credentials.username || fields.get("realm").map(String::as_str) != Some(realm) {
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
    Ok(expected.eq_ignore_ascii_case(response))
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
}
