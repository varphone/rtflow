#![no_main]
use libfuzzer_sys::fuzz_target;
use rtflow::srt::handshake::parse_handshake;
use rtflow::srt::packet::{parse_nak_loss_list, parse_packet};

fuzz_target!(|data: &[u8]| {
    let _ = parse_packet(data);
    let _ = parse_handshake(data);
    let _ = parse_nak_loss_list(data);
});
