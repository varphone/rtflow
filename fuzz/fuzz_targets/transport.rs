#![no_main]
use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use rtflow::transport::{InterleavedFrame, TransportSpec};

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(alternatives) = TransportSpec::parse_all(text) {
            for alternative in &alternatives {
                let _ = alternative.render();
            }
        }
        let _ = TransportSpec::parse(text);
    }
    let mut buffer = BytesMut::from(data);
    for _ in 0..8 {
        match InterleavedFrame::parse(&mut buffer, 65_536) {
            Ok(Some(_)) => {}
            _ => break,
        }
    }
});
