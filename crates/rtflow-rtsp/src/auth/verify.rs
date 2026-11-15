use super::*;

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
    if !algorithm.eq_ignore_ascii_case("MD5") && !algorithm.eq_ignore_ascii_case("SHA-256") {
        return Err(Error::Rtsp(format!(
            "unsupported Digest algorithm {algorithm}"
        )));
    }
    let digest: fn(&str) -> String = if algorithm.eq_ignore_ascii_case("SHA-256") {
        sha256_hex
    } else {
        md5_hex
    };
    let ha1 = digest(&format!(
        "{}:{realm}:{}",
        credentials.username, credentials.password
    ));
    let ha2 = digest(&format!("{}:{uri}", method.as_str()));
    let mut response = digest(&format!("{ha1}:{nonce}:{ha2}"));
    let mut suffix = String::new();
    if fields
        .get("qop")
        .is_some_and(|value| value.split(',').any(|item| item.trim() == "auth"))
    {
        let nc = "00000001";
        let cnonce = digest(&format!("{}:{nonce}:{uri}", credentials.username));
        response = digest(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
        suffix = format!(", qop=auth, nc={nc}, cnonce=\"{cnonce}\"");
    }
    Ok(format!(
        "Digest username=\"{}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\", algorithm={algorithm}{suffix}",
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
    let algorithm = fields.get("algorithm").map(String::as_str).unwrap_or("MD5");
    let digest: fn(&str) -> String = if algorithm.eq_ignore_ascii_case("SHA-256") {
        sha256_hex
    } else {
        md5_hex
    };
    let ha1 = digest(&format!(
        "{}:{realm}:{}",
        credentials.username, credentials.password
    ));
    let ha2 = digest(&format!("{}:{uri}", method.as_str()));
    let expected = if let (Some(qop), Some(nc), Some(cnonce)) =
        (fields.get("qop"), fields.get("nc"), fields.get("cnonce"))
    {
        digest(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"))
    } else {
        digest(&format!("{ha1}:{nonce}:{ha2}"))
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

pub(super) fn required<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> Result<&'a String> {
    fields
        .get(key)
        .ok_or_else(|| Error::Rtsp(format!("Digest missing {key}")))
}

pub(super) fn parse_fields(value: &str) -> Result<BTreeMap<String, String>> {
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

pub(super) fn insert_field(out: &mut BTreeMap<String, String>, item: &str) -> Result<()> {
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

pub(super) fn sha256_hex(value: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn md5_hex(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Md5::digest(value.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for &byte in digest.iter() {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

pub(super) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

pub(super) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

pub(super) fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
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
