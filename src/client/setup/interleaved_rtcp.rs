//! RTCP handling for the client's shared interleaved control connection.
use std::{collections::HashMap, sync::Arc};

use tokio::{io::AsyncWriteExt, sync::Mutex};

use crate::{
    rtcp::{Packet as RtcpPacket, RtcpSession},
    runtime::io::ControlWriteHalf,
    transport::InterleavedFrame,
};

/// Handle one RTCP frame (plain or SRTCP), feed quality statistics and send
/// the receiver report back on the same channel. Returns false when the
/// connection writer failed.
pub(super) async fn handle_rtcp(
    frame: &InterleavedFrame,
    srtcp: Option<&mut crate::srtp::SrtcpContext>,
    quality: &Arc<Mutex<HashMap<u8, RtcpSession>>>,
    track_id: u8,
    clock_rate: u32,
    local_ssrc: u32,
    writer: &Arc<Mutex<ControlWriteHalf>>,
) -> bool {
    let mut srtcp = srtcp;
    let payload = match &mut srtcp {
        Some(context) => match context.unprotect(&frame.payload) {
            Ok(payload) => payload,
            Err(_) => return true,
        },
        None => frame.payload.clone(),
    };
    let Ok(packets) = RtcpPacket::parse(payload) else {
        return true;
    };
    let Ok(Some(report)) =
        super::quality_report(quality, track_id, clock_rate, local_ssrc, &packets).await
    else {
        return true;
    };
    let Ok(payload) = report.try_encode() else {
        return true;
    };
    let payload = match &mut srtcp {
        Some(context) => match context.protect(&payload) {
            Ok(payload) => payload,
            Err(_) => return true,
        },
        None => payload,
    };
    let Ok(wire) = (InterleavedFrame {
        channel: frame.channel,
        payload,
    })
    .try_encode() else {
        return false;
    };
    let write = async { writer.lock().await.write_all(&wire).await };
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(10), write).await,
        Ok(Ok(()))
    )
}
