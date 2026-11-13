#![no_main]
use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use rtflow::rtsp::{self, Message};

fuzz_target!(|data: &[u8]| {
    let mut input = BytesMut::from(data);
    for _ in 0..8 {
        match rtsp::parse(&mut input, 16 * 1024, 64 * 1024) {
            Ok(Some(Message::Response(response))) => {
                let _ = rtsp::encode_response(&response);
            }
            Ok(Some(Message::Request(_))) => {}
            _ => break,
        }
    }
});
