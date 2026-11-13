#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let bytes = Bytes::copy_from_slice(data);
    let _ = rtflow::rtp::RtpPacket::parse(bytes.clone());
    let _ = rtflow::rtcp::Packet::parse(bytes);
});
