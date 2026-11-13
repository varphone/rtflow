#![no_main]
use libfuzzer_sys::fuzz_target;
use rtflow::rtsp::sdp::{SdpLimits, parse, parse_with_limits};

fuzz_target!(|data: &[u8]| {
    let _ = parse(data);
    let _ = parse_with_limits(data, SdpLimits { max_tracks: 64 });
});
