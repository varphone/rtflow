use super::verify::md5_hex;
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
    let challenge = format!("Digest realm=\"r\", nonce=\"{nonce}\", algorithm=MD5, qop=\"auth\"");
    let header = authorization_from_challenge(
        &challenge,
        Method::Describe,
        "rtsp://example/live",
        &credentials,
    )
    .unwrap();
    assert!(verify_digest_managed(&header, Method::Describe, &credentials, "r", &manager).unwrap());
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
