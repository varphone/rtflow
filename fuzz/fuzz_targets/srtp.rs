#![no_main]
use libfuzzer_sys::fuzz_target;
use rtflow::srtp::{SrtcpContext, SrtpContext, SrtpProfile};

fuzz_target!(|data: &[u8]| {
    let mut srtp = SrtpContext::new(
        SrtpProfile::AES_128_CM_HMAC_SHA1_80,
        &[0x11; 16],
        &[0x22; 14],
    );
    let _ = srtp.protect_rtp(data);
    let _ = srtp.unprotect_rtp(data);

    let mut srtcp = SrtcpContext::new(&[0x33; 16], &[0x44; 20], &[0x55; 14]);
    let _ = srtcp.protect(data);
    let _ = srtcp.unprotect(data);
});
