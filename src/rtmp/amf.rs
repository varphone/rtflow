//! AMF0 (Action Message Format) encoding and decoding.
//!
//! Supports the subset needed for RTMP command messages:
//! Number, Boolean, String, Object, Null, Undefined, EcmaArray.
use bytes::{Buf, BufMut, Bytes, BytesMut};

/// An AMF0 value.
#[derive(Clone, Debug, PartialEq)]
pub enum AmfValue {
    Number(f64),
    Boolean(bool),
    Str(String),
    Object(Vec<(String, AmfValue)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, AmfValue)>),
}

impl AmfValue {
    /// Get as f64 if Number.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            AmfValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Get as &str if String.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            AmfValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Get as bool if Boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            AmfValue::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// Get object property by key.
    pub fn get(&self, key: &str) -> Option<&AmfValue> {
        match self {
            AmfValue::Object(props) | AmfValue::EcmaArray(props) => {
                props.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }
}

// Type markers
const MARKER_NUMBER: u8 = 0x00;
const MARKER_BOOLEAN: u8 = 0x01;
const MARKER_STRING: u8 = 0x02;
const MARKER_OBJECT: u8 = 0x03;
const MARKER_NULL: u8 = 0x05;
const MARKER_UNDEFINED: u8 = 0x06;
const MARKER_ECMA_ARRAY: u8 = 0x08;
const MARKER_OBJECT_END: u8 = 0x09;
const MARKER_LONG_STRING: u8 = 0x0C;

/// AMF0 encoding error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmfError {
    UnexpectedEof,
    InvalidMarker(u8),
    InvalidUtf8,
    InvalidBoolean(u8),
}

impl std::fmt::Display for AmfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AmfError::UnexpectedEof => write!(f, "AMF0: unexpected end of data"),
            AmfError::InvalidMarker(m) => write!(f, "AMF0: invalid marker 0x{m:02x}"),
            AmfError::InvalidUtf8 => write!(f, "AMF0: invalid UTF-8 string"),
            AmfError::InvalidBoolean(b) => write!(f, "AMF0: invalid boolean value {b}"),
        }
    }
}

impl std::error::Error for AmfError {}

/// Encode a single AMF0 value into the buffer.
pub fn encode_value(buf: &mut BytesMut, value: &AmfValue) {
    match value {
        AmfValue::Number(n) => {
            buf.put_u8(MARKER_NUMBER);
            buf.put_f64(*n);
        }
        AmfValue::Boolean(b) => {
            buf.put_u8(MARKER_BOOLEAN);
            buf.put_u8(if *b { 1 } else { 0 });
        }
        AmfValue::Str(s) => {
            if s.len() <= 0xFFFF {
                buf.put_u8(MARKER_STRING);
                buf.put_u16(s.len() as u16);
                buf.put_slice(s.as_bytes());
            } else {
                buf.put_u8(MARKER_LONG_STRING);
                buf.put_u32(s.len() as u32);
                buf.put_slice(s.as_bytes());
            }
        }
        AmfValue::Object(props) => {
            buf.put_u8(MARKER_OBJECT);
            encode_properties(buf, props);
        }
        AmfValue::Null => {
            buf.put_u8(MARKER_NULL);
        }
        AmfValue::Undefined => {
            buf.put_u8(MARKER_UNDEFINED);
        }
        AmfValue::EcmaArray(props) => {
            buf.put_u8(MARKER_ECMA_ARRAY);
            buf.put_u32(props.len() as u32);
            encode_properties(buf, props);
        }
    }
}

fn encode_properties(buf: &mut BytesMut, props: &[(String, AmfValue)]) {
    for (key, value) in props {
        buf.put_u16(key.len() as u16);
        buf.put_slice(key.as_bytes());
        encode_value(buf, value);
    }
    // Object end marker: 0x00 0x00 0x09
    buf.put_u16(0);
    buf.put_u8(MARKER_OBJECT_END);
}

/// Encode multiple values (a complete command message).
pub fn encode_values(values: &[AmfValue]) -> Bytes {
    let mut buf = BytesMut::with_capacity(256);
    for value in values {
        encode_value(&mut buf, value);
    }
    buf.freeze()
}

/// Decode a single AMF0 value from the buffer.
pub fn decode_value(buf: &mut &[u8]) -> Result<AmfValue, AmfError> {
    if buf.is_empty() {
        return Err(AmfError::UnexpectedEof);
    }
    let marker = buf[0];
    buf.advance(1);

    match marker {
        MARKER_NUMBER => {
            if buf.remaining() < 8 {
                return Err(AmfError::UnexpectedEof);
            }
            Ok(AmfValue::Number(buf.get_f64()))
        }
        MARKER_BOOLEAN => {
            if buf.is_empty() {
                return Err(AmfError::UnexpectedEof);
            }
            let b = buf[0];
            buf.advance(1);
            match b {
                0 => Ok(AmfValue::Boolean(false)),
                1 => Ok(AmfValue::Boolean(true)),
                _ => Err(AmfError::InvalidBoolean(b)),
            }
        }
        MARKER_STRING => decode_short_string(buf).map(AmfValue::Str),
        MARKER_LONG_STRING => {
            if buf.remaining() < 4 {
                return Err(AmfError::UnexpectedEof);
            }
            let len = buf.get_u32() as usize;
            if buf.remaining() < len {
                return Err(AmfError::UnexpectedEof);
            }
            let s = std::str::from_utf8(&buf[..len])
                .map_err(|_| AmfError::InvalidUtf8)?
                .to_owned();
            buf.advance(len);
            Ok(AmfValue::Str(s))
        }
        MARKER_OBJECT => {
            let props = decode_properties(buf)?;
            Ok(AmfValue::Object(props))
        }
        MARKER_NULL => Ok(AmfValue::Null),
        MARKER_UNDEFINED => Ok(AmfValue::Undefined),
        MARKER_ECMA_ARRAY => {
            if buf.remaining() < 4 {
                return Err(AmfError::UnexpectedEof);
            }
            let _count = buf.get_u32(); // approximate count, not authoritative
            let props = decode_properties(buf)?;
            Ok(AmfValue::EcmaArray(props))
        }
        _ => Err(AmfError::InvalidMarker(marker)),
    }
}

fn decode_short_string(buf: &mut &[u8]) -> Result<String, AmfError> {
    if buf.remaining() < 2 {
        return Err(AmfError::UnexpectedEof);
    }
    let len = buf.get_u16() as usize;
    if buf.remaining() < len {
        return Err(AmfError::UnexpectedEof);
    }
    let s = std::str::from_utf8(&buf[..len])
        .map_err(|_| AmfError::InvalidUtf8)?
        .to_owned();
    buf.advance(len);
    Ok(s)
}

fn decode_properties(buf: &mut &[u8]) -> Result<Vec<(String, AmfValue)>, AmfError> {
    let mut props = Vec::new();
    loop {
        if buf.remaining() < 2 {
            return Err(AmfError::UnexpectedEof);
        }
        let key_len = buf.get_u16() as usize;
        if key_len == 0 {
            // Expect object end marker
            if buf.is_empty() {
                return Err(AmfError::UnexpectedEof);
            }
            let end = buf[0];
            buf.advance(1);
            if end == MARKER_OBJECT_END {
                break;
            }
            return Err(AmfError::InvalidMarker(end));
        }
        if buf.remaining() < key_len {
            return Err(AmfError::UnexpectedEof);
        }
        let key = std::str::from_utf8(&buf[..key_len])
            .map_err(|_| AmfError::InvalidUtf8)?
            .to_owned();
        buf.advance(key_len);
        let value = decode_value(buf)?;
        props.push((key, value));
    }
    Ok(props)
}

/// Decode all values from a buffer (command message payload).
pub fn decode_values(mut data: &[u8]) -> Result<Vec<AmfValue>, AmfError> {
    let mut values = Vec::new();
    while !data.is_empty() {
        values.push(decode_value(&mut data)?);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_number() {
        let val = AmfValue::Number(42.5);
        let encoded = encode_values(&[val.clone()]);
        let decoded = decode_values(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }

    #[test]
    fn roundtrip_string() {
        let val = AmfValue::Str("connect".to_string());
        let encoded = encode_values(&[val.clone()]);
        let decoded = decode_values(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }

    #[test]
    fn roundtrip_object() {
        let val = AmfValue::Object(vec![
            ("app".to_string(), AmfValue::Str("live".to_string())),
            (
                "flashVer".to_string(),
                AmfValue::Str("FMLE/3.0".to_string()),
            ),
            (
                "tcUrl".to_string(),
                AmfValue::Str("rtmp://localhost/live".to_string()),
            ),
        ]);
        let encoded = encode_values(&[val.clone()]);
        let decoded = decode_values(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }

    #[test]
    fn roundtrip_multiple() {
        let values = vec![
            AmfValue::Str("connect".to_string()),
            AmfValue::Number(1.0),
            AmfValue::Object(vec![("app".to_string(), AmfValue::Str("live".to_string()))]),
        ];
        let encoded = encode_values(&values);
        let decoded = decode_values(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn roundtrip_null_and_boolean() {
        let values = vec![
            AmfValue::Null,
            AmfValue::Boolean(true),
            AmfValue::Boolean(false),
            AmfValue::Undefined,
        ];
        let encoded = encode_values(&values);
        let decoded = decode_values(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn roundtrip_ecma_array() {
        let val = AmfValue::EcmaArray(vec![
            ("width".to_string(), AmfValue::Number(1920.0)),
            ("height".to_string(), AmfValue::Number(1080.0)),
            ("framerate".to_string(), AmfValue::Number(30.0)),
        ]);
        let encoded = encode_values(&[val.clone()]);
        let decoded = decode_values(&encoded).unwrap();
        assert_eq!(decoded, vec![val]);
    }
}
