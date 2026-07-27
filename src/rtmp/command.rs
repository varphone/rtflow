//! RTMP command message parsing and construction.
//!
//! Handles connect, createStream, publish, play, and their responses,
//! as well as onMetaData and status notifications.
use super::amf::{AmfValue, decode_values, encode_values};
use bytes::Bytes;

/// A parsed RTMP command message.
#[derive(Clone, Debug)]
pub enum RtmpCommand {
    Connect {
        transaction_id: f64,
        app: String,
        tc_url: Option<String>,
        flash_ver: Option<String>,
    },
    CreateStream {
        transaction_id: f64,
    },
    Publish {
        transaction_id: f64,
        stream_name: String,
        publish_type: String,
    },
    Play {
        transaction_id: f64,
        stream_name: String,
        start: Option<f64>,
    },
    DeleteStream {
        transaction_id: f64,
        stream_id: f64,
    },
    CloseStream {
        transaction_id: f64,
    },
    ReleaseStream {
        transaction_id: f64,
        stream_name: String,
    },
    Fcpublish {
        transaction_id: f64,
        stream_name: String,
    },
    GetStreamLength {
        transaction_id: f64,
        stream_name: String,
    },
    /// onMetaData or other data message
    MetaData {
        name: String,
        data: Option<AmfValue>,
    },
    /// Unknown/generic command
    Generic {
        name: String,
        transaction_id: f64,
        args: Vec<AmfValue>,
    },
}

impl RtmpCommand {
    /// Parse a command from AMF0 payload.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let values = decode_values(payload).ok()?;
        if values.is_empty() {
            return None;
        }

        let name = values[0].as_str()?.to_string();
        let transaction_id = values.get(1).and_then(|v| v.as_number()).unwrap_or(0.0);

        match name.as_str() {
            "connect" => {
                let obj = values.get(2);
                let app = obj
                    .and_then(|o| o.get("app"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tc_url = obj
                    .and_then(|o| o.get("tcUrl"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let flash_ver = obj
                    .and_then(|o| o.get("flashVer"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Some(RtmpCommand::Connect {
                    transaction_id,
                    app,
                    tc_url,
                    flash_ver,
                })
            }
            "createStream" => Some(RtmpCommand::CreateStream { transaction_id }),
            "publish" => {
                let stream_name = values
                    .get(3)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let publish_type = values
                    .get(4)
                    .and_then(|v| v.as_str())
                    .unwrap_or("live")
                    .to_string();
                Some(RtmpCommand::Publish {
                    transaction_id,
                    stream_name,
                    publish_type,
                })
            }
            "play" => {
                let stream_name = values
                    .get(3)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let start = values.get(4).and_then(|v| v.as_number());
                Some(RtmpCommand::Play {
                    transaction_id,
                    stream_name,
                    start,
                })
            }
            "deleteStream" => {
                let stream_id = values.get(3).and_then(|v| v.as_number()).unwrap_or(0.0);
                Some(RtmpCommand::DeleteStream {
                    transaction_id,
                    stream_id,
                })
            }
            "closeStream" => Some(RtmpCommand::CloseStream { transaction_id }),
            "releaseStream" => {
                let stream_name = values
                    .get(3)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Some(RtmpCommand::ReleaseStream {
                    transaction_id,
                    stream_name,
                })
            }
            "FCPublish" | "FCUnpublish" => {
                let stream_name = values
                    .get(3)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Some(RtmpCommand::Fcpublish {
                    transaction_id,
                    stream_name,
                })
            }
            "getStreamLength" => {
                let stream_name = values
                    .get(3)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Some(RtmpCommand::GetStreamLength {
                    transaction_id,
                    stream_name,
                })
            }
            "@setDataFrame" | "onMetaData" => {
                // @setDataFrame: [0]="@setDataFrame", [1]="onMetaData", [2]=object
                // onMetaData: [0]="onMetaData", [1]=object
                let (meta_name, data) = if name == "@setDataFrame" {
                    let meta_name = values
                        .get(1)
                        .and_then(|v| v.as_str())
                        .unwrap_or("onMetaData")
                        .to_string();
                    let data = values.get(2).cloned();
                    (meta_name, data)
                } else {
                    (name.clone(), values.get(1).cloned())
                };
                Some(RtmpCommand::MetaData {
                    name: meta_name,
                    data,
                })
            }
            _ => Some(RtmpCommand::Generic {
                name,
                transaction_id,
                args: values.into_iter().skip(2).collect(),
            }),
        }
    }
}

/// Build a _result response for connect.
pub fn connect_result(transaction_id: f64) -> Bytes {
    let values = vec![
        AmfValue::Str("_result".to_string()),
        AmfValue::Number(transaction_id),
        AmfValue::Object(vec![
            ("fmsVer".to_string(), AmfValue::Str("FMS/3,0,1,123".to_string())),
            ("capabilities".to_string(), AmfValue::Number(31.0)),
        ]),
        AmfValue::Object(vec![
            ("level".to_string(), AmfValue::Str("status".to_string())),
            (
                "code".to_string(),
                AmfValue::Str("NetConnection.Connect.Success".to_string()),
            ),
            (
                "description".to_string(),
                AmfValue::Str("Connection succeeded.".to_string()),
            ),
            ("objectEncoding".to_string(), AmfValue::Number(0.0)),
        ]),
    ];
    encode_values(&values)
}

/// Build a _result response for createStream.
pub fn create_stream_result(transaction_id: f64, stream_id: f64) -> Bytes {
    let values = vec![
        AmfValue::Str("_result".to_string()),
        AmfValue::Number(transaction_id),
        AmfValue::Null,
        AmfValue::Number(stream_id),
    ];
    encode_values(&values)
}

/// Build an onStatus command.
pub fn on_status(level: &str, code: &str, description: &str) -> Bytes {
    let values = vec![
        AmfValue::Str("onStatus".to_string()),
        AmfValue::Number(0.0),
        AmfValue::Null,
        AmfValue::Object(vec![
            ("level".to_string(), AmfValue::Str(level.to_string())),
            ("code".to_string(), AmfValue::Str(code.to_string())),
            ("description".to_string(), AmfValue::Str(description.to_string())),
        ]),
    ];
    encode_values(&values)
}

/// Build a generic _error response.
pub fn error_response(transaction_id: f64, code: &str, description: &str) -> Bytes {
    let values = vec![
        AmfValue::Str("_error".to_string()),
        AmfValue::Number(transaction_id),
        AmfValue::Null,
        AmfValue::Object(vec![
            ("level".to_string(), AmfValue::Str("error".to_string())),
            ("code".to_string(), AmfValue::Str(code.to_string())),
            ("description".to_string(), AmfValue::Str(description.to_string())),
        ]),
    ];
    encode_values(&values)
}

/// Build a generic _result with null body.
pub fn null_result(transaction_id: f64) -> Bytes {
    let values = vec![
        AmfValue::Str("_result".to_string()),
        AmfValue::Number(transaction_id),
        AmfValue::Null,
    ];
    encode_values(&values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_connect() {
        let payload = encode_values(&[
            AmfValue::Str("connect".to_string()),
            AmfValue::Number(1.0),
            AmfValue::Object(vec![
                ("app".to_string(), AmfValue::Str("live".to_string())),
                ("tcUrl".to_string(), AmfValue::Str("rtmp://localhost/live".to_string())),
            ]),
        ]);
        let cmd = RtmpCommand::parse(&payload).unwrap();
        match cmd {
            RtmpCommand::Connect {
                transaction_id,
                app,
                tc_url,
                ..
            } => {
                assert_eq!(transaction_id, 1.0);
                assert_eq!(app, "live");
                assert_eq!(tc_url.as_deref(), Some("rtmp://localhost/live"));
            }
            _ => panic!("expected Connect"),
        }
    }

    #[test]
    fn parse_publish() {
        let payload = encode_values(&[
            AmfValue::Str("publish".to_string()),
            AmfValue::Number(4.0),
            AmfValue::Null,
            AmfValue::Str("mystream".to_string()),
            AmfValue::Str("live".to_string()),
        ]);
        let cmd = RtmpCommand::parse(&payload).unwrap();
        match cmd {
            RtmpCommand::Publish {
                stream_name,
                publish_type,
                ..
            } => {
                assert_eq!(stream_name, "mystream");
                assert_eq!(publish_type, "live");
            }
            _ => panic!("expected Publish"),
        }
    }

    #[test]
    fn parse_play() {
        let payload = encode_values(&[
            AmfValue::Str("play".to_string()),
            AmfValue::Number(5.0),
            AmfValue::Null,
            AmfValue::Str("mystream".to_string()),
            AmfValue::Number(-1.0),
        ]);
        let cmd = RtmpCommand::parse(&payload).unwrap();
        match cmd {
            RtmpCommand::Play {
                stream_name, start, ..
            } => {
                assert_eq!(stream_name, "mystream");
                assert_eq!(start, Some(-1.0));
            }
            _ => panic!("expected Play"),
        }
    }

    #[test]
    fn connect_result_encodes() {
        let result = connect_result(1.0);
        let values = decode_values(&result).unwrap();
        assert_eq!(values[0].as_str(), Some("_result"));
        assert_eq!(values[1].as_number(), Some(1.0));
    }
}
