use super::packetizer::annex_b_nals;
use super::{Depacketizer, Packetizer};
use crate::{RtpHeader, RtpPacket};
use bytes::Bytes;
use rtflow_core::error::Error;
use rtflow_media::Codec;

use crate::{JitterBuffer, JitterBufferConfig};
use rtflow_media::AccessUnit;
use std::time::Duration;
fn frame(c: Codec, data: &[u8]) -> AccessUnit {
    AccessUnit {
        track_id: 0,
        codec: c,
        pts: Duration::ZERO,
        dts: None,
        rtp_timestamp: 7,
        is_keyframe: true,
        data: Bytes::copy_from_slice(data),
    }
}

mod basic;
mod keyframes;
mod vp8;
mod vp9;
