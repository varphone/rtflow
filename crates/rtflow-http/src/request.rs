use bytes::Bytes;
use std::collections::HashMap;

const MAX_HEADERS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Head,
    Options,
    Patch,
}

impl Method {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "HEAD" => Some(Self::Head),
            "OPTIONS" => Some(Self::Options),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: Method,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// Values captured by `{name}` route segments, in declaration order.
    pub path_params: Vec<String>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == lower)
            .map(|(_, v)| v.as_str())
    }
}

pub enum ParseError {
    Incomplete,
    TooLarge,
    Invalid,
}

pub fn parse_request(
    buf: &[u8],
    max_header_bytes: usize,
    max_body_bytes: usize,
) -> Result<Option<(HttpRequest, usize)>, ParseError> {
    // Find end of headers
    let header_end = find_header_end(buf).ok_or({
        if buf.len() > max_header_bytes {
            ParseError::TooLarge
        } else {
            ParseError::Incomplete
        }
    })?;

    let header_str = std::str::from_utf8(&buf[..header_end]).map_err(|_| ParseError::Invalid)?;
    let mut lines = header_str.split("\r\n");

    // Request line
    let request_line = lines.next().ok_or(ParseError::Invalid)?;
    let mut parts = request_line.splitn(3, ' ');
    let method_str = parts.next().ok_or(ParseError::Invalid)?;
    let path_str = parts.next().ok_or(ParseError::Invalid)?;
    let version = parts.next().ok_or(ParseError::Invalid)?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(ParseError::Invalid);
    }

    let method = Method::parse(method_str).ok_or(ParseError::Invalid)?;

    // Split path and query
    let (path, query) = parse_path_query(path_str);

    // Headers
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(ParseError::TooLarge);
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(ParseError::Invalid);
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || key.bytes().any(|byte| byte <= 0x20 || byte >= 0x7f) {
            return Err(ParseError::Invalid);
        }
        headers.push((key.to_string(), value.to_string()));
    }

    let content_lengths: Vec<_> = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>().map_err(|_| ParseError::Invalid))
        .collect::<Result<_, _>>()?;
    if content_lengths
        .windows(2)
        .any(|values| values[0] != values[1])
    {
        return Err(ParseError::Invalid);
    }
    let content_length = content_lengths.first().copied().unwrap_or(0);
    if content_length > max_body_bytes {
        return Err(ParseError::TooLarge);
    }
    if headers.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("transfer-encoding") && !value.eq_ignore_ascii_case("identity")
    }) {
        return Err(ParseError::Invalid);
    }

    let total = header_end + 4 + content_length; // +4 for \r\n\r\n
    if buf.len() < total {
        return Err(ParseError::Incomplete);
    }

    let body = if content_length > 0 {
        Bytes::copy_from_slice(&buf[header_end + 4..total])
    } else {
        Bytes::new()
    };

    Ok(Some((
        HttpRequest {
            method,
            path,
            query,
            headers,
            body,
            path_params: Vec::new(),
        },
        total,
    )))
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

fn parse_path_query(raw: &str) -> (String, HashMap<String, String>) {
    if let Some((path, qs)) = raw.split_once('?') {
        let query = qs
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (path.to_string(), query)
    } else {
        (raw.to_string(), HashMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(wire: &[u8]) -> Result<Option<(HttpRequest, usize)>, ParseError> {
        parse_request(wire, 8 * 1024, 256)
    }

    #[test]
    fn parser_rejects_invalid_version_and_header_lines() {
        // HTTP/1.0 is accepted (answered with HTTP/1.1 framing).
        assert!(parse(b"GET / HTTP/1.0\r\n\r\n").is_ok());
        assert!(parse(b"GET / HTTP/1.1\r\nBroken\r\n\r\n").is_err());
    }

    #[test]
    fn parser_rejects_conflicting_lengths_and_unsupported_transfer_encoding() {
        assert!(
            parse(b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nx").is_err()
        );
        assert!(parse(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").is_err());
    }

    #[test]
    fn parser_enforces_body_limit() {
        assert!(parse(b"POST / HTTP/1.1\r\nContent-Length: 257\r\n\r\n").is_err());
    }
}
