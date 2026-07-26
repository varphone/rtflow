use bytes::Bytes;
use std::collections::HashMap;

const MAX_HEADER_BYTES: usize = 8 * 1024;
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

pub(crate) enum ParseError {
    Incomplete,
    TooLarge,
    Invalid,
}

pub(crate) fn parse_request(buf: &[u8]) -> Result<Option<(HttpRequest, usize)>, ParseError> {
    // Find end of headers
    let header_end = find_header_end(buf).ok_or_else(|| {
        if buf.len() > MAX_HEADER_BYTES {
            ParseError::TooLarge
        } else {
            ParseError::Incomplete
        }
    })?;

    let header_str =
        std::str::from_utf8(&buf[..header_end]).map_err(|_| ParseError::Invalid)?;
    let mut lines = header_str.split("\r\n");

    // Request line
    let request_line = lines.next().ok_or(ParseError::Invalid)?;
    let mut parts = request_line.splitn(3, ' ');
    let method_str = parts.next().ok_or(ParseError::Invalid)?;
    let path_str = parts.next().ok_or(ParseError::Invalid)?;
    let _version = parts.next().ok_or(ParseError::Invalid)?;

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
        if let Some((key, value)) = line.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }

    // Content-Length for body
    let content_length = header_value(&headers, "content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

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

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let lower = name.to_ascii_lowercase();
    headers
        .iter()
        .find(|(k, _)| k.to_ascii_lowercase() == lower)
        .map(|(_, v)| v.as_str())
}
