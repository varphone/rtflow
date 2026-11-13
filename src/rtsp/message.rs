//! Bounded RTSP/1.0 framing primitives.
use crate::{Error, Result};
use bytes::{Buf, BytesMut};
use std::{collections::BTreeMap, str::FromStr};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Options,
    Describe,
    Announce,
    Setup,
    Play,
    Pause,
    Record,
    Teardown,
    GetParameter,
    SetParameter,
    Redirect,
    Unsupported,
}
impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Options => "OPTIONS",
            Self::Describe => "DESCRIBE",
            Self::Announce => "ANNOUNCE",
            Self::Setup => "SETUP",
            Self::Play => "PLAY",
            Self::Pause => "PAUSE",
            Self::Record => "RECORD",
            Self::Teardown => "TEARDOWN",
            Self::GetParameter => "GET_PARAMETER",
            Self::SetParameter => "SET_PARAMETER",
            Self::Redirect => "REDIRECT",
            Self::Unsupported => "UNSUPPORTED",
        }
    }
}
impl FromStr for Method {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "OPTIONS" => Ok(Self::Options),
            "DESCRIBE" => Ok(Self::Describe),
            "ANNOUNCE" => Ok(Self::Announce),
            "SETUP" => Ok(Self::Setup),
            "PLAY" => Ok(Self::Play),
            "PAUSE" => Ok(Self::Pause),
            "RECORD" => Ok(Self::Record),
            "TEARDOWN" => Ok(Self::Teardown),
            "GET_PARAMETER" => Ok(Self::GetParameter),
            "SET_PARAMETER" => Ok(Self::SetParameter),
            "REDIRECT" => Ok(Self::Redirect),
            _ => Ok(Self::Unsupported),
        }
    }
}
#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    pub uri: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    /// Case-insensitive header lookup (parsed keys are lower-case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl Response {
    /// Case-insensitive header lookup (parsed keys are lower-case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}
#[derive(Clone, Debug)]
pub enum Message {
    Request(Request),
    Response(Response),
}
pub fn parse(input: &mut BytesMut, max_header: usize, max_body: usize) -> Result<Option<Message>> {
    let Some(pos) = input.windows(4).position(|w| w == b"\r\n\r\n") else {
        if input.len() > max_header {
            return Err(Error::Limit("RTSP header"));
        }
        return Ok(None);
    };
    let end = pos + 4;
    if end > max_header {
        return Err(Error::Limit("RTSP header"));
    }
    let text = std::str::from_utf8(&input[..pos])
        .map_err(|_| Error::Rtsp("header is not UTF-8".into()))?;
    let mut lines = text.split("\r\n");
    let start = lines
        .next()
        .ok_or_else(|| Error::Rtsp("empty message".into()))?
        .to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| Error::Rtsp("malformed header".into()))?;
        let key = k.trim();
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || headers
                .insert(key.to_ascii_lowercase(), v.trim().to_owned())
                .is_some()
        {
            return Err(Error::Rtsp("duplicate or malformed header".into()));
        }
    }
    let len = headers
        .get("content-length")
        .map(|v| {
            v.parse::<usize>()
                .map_err(|_| Error::Rtsp("bad Content-Length".into()))
        })
        .transpose()?
        .unwrap_or(0);
    if len > max_body {
        return Err(Error::Limit("RTSP body"));
    }
    if input.len() < end + len {
        return Ok(None);
    }
    let body = input[end..end + len].to_vec();
    input.advance(end + len);
    let p: Vec<_> = start.split_whitespace().collect();
    if p.first() == Some(&"RTSP/1.0") {
        if p.len() < 3 {
            return Err(Error::Rtsp("bad status line".into()));
        }
        let status = p[1].parse().map_err(|_| Error::Rtsp("bad status".into()))?;
        Ok(Some(Message::Response(Response {
            status,
            reason: p[2..].join(" "),
            headers,
            body,
        })))
    } else {
        if p.len() != 3 || p[2] != "RTSP/1.0" {
            return Err(Error::Rtsp("bad request line".into()));
        }
        Ok(Some(Message::Request(Request {
            method: p[0].parse()?,
            uri: p[1].into(),
            headers,
            body,
        })))
    }
}
pub fn encode_response(r: &Response) -> Vec<u8> {
    let mut out = format!("RTSP/1.0 {} {}\r\n", r.status, r.reason);
    for (k, v) in &r.headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n")
    }
    if !r.body.is_empty() && !r.headers.contains_key("content-length") {
        out.push_str(&format!("content-length: {}\r\n", r.body.len()))
    }
    out.push_str("\r\n");
    let mut b = out.into_bytes();
    b.extend_from_slice(&r.body);
    b
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn enforces_header_and_body_limits() {
        let mut header = BytesMut::from(&b"OPTIONS rtsp://x/a RTSP/1.0\r\nLong: 123456789\r\n"[..]);
        assert!(matches!(
            parse(&mut header, 16, 16),
            Err(Error::Limit("RTSP header"))
        ));

        let mut body =
            BytesMut::from(&b"ANNOUNCE rtsp://x/a RTSP/1.0\r\nContent-Length: 5\r\n\r\n12345"[..]);
        assert!(matches!(
            parse(&mut body, 1024, 4),
            Err(Error::Limit("RTSP body"))
        ));
    }

    #[test]
    fn rejects_malformed_content_length_without_consuming_partial_data() {
        let wire = b"ANNOUNCE rtsp://x/a RTSP/1.0\r\nContent-Length: nope\r\n\r\n";
        let mut input = BytesMut::from(&wire[..]);
        assert!(parse(&mut input, 1024, 1024).is_err());
        assert_eq!(&input[..], wire);
    }

    #[test]
    fn rejects_duplicate_headers_and_preserves_input() {
        let wire = b"OPTIONS rtsp://x/a RTSP/1.0\r\nCSeq: 1\r\nCSeq: 2\r\n\r\n";
        let mut input = BytesMut::from(&wire[..]);
        assert!(parse(&mut input, 1024, 1024).is_err());
        assert_eq!(&input[..], wire);
    }

    #[test]
    fn preserves_unknown_methods_for_a_501_response() {
        let mut input = BytesMut::from("FROBNICATE rtsp://x/a RTSP/1.0\r\nCSeq: 1\r\n\r\n");
        let Some(Message::Request(request)) = parse(&mut input, 1024, 1024).unwrap() else {
            panic!("expected request");
        };
        assert_eq!(request.method, Method::Unsupported);
    }

    #[test]
    fn incremental() {
        let mut b = BytesMut::from("OPTIONS rtsp://x/a RTSP/1.0\r\nCSeq: 1\r\n\r\n");
        assert!(matches!(
            parse(&mut b, 1000, 1).unwrap(),
            Some(Message::Request(_))
        ));
        assert!(b.is_empty())
    }
}
