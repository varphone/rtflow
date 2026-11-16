#![no_main]
use libfuzzer_sys::fuzz_target;
use rtflow::framing::ConnectionFramer;

fuzz_target!(|data: &[u8]| {
    let mut framer = ConnectionFramer::new(16 * 1024, 256 * 1024, 65_536);
    for chunk in data.chunks(7) {
        framer.push(chunk);
        while let Ok(Some(_)) = framer.next_frame() {}
    }
});
