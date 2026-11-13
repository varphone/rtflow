//! RTCP handling for the client's shared interleaved control connection.
use std::{collections::HashMap, sync::Arc};

use tokio::{io::AsyncWriteExt, sync::Mutex as TokioMutex};

use crate::{
    rtcp::{Packet as RtcpPacket, RtcpSession},
    runtime::io::ControlWriteHalf,
    transport::InterleavedFrame,
};

/// Handle one RTCP frame (plain or SRTCP), feed quality statistics and send
/// the receiver report back on the same channel. Returns false when the
/// connection writer failed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_rtcp(
    frame: &InterleavedFrame,
    #[cfg(feature = "srtp")] srtcp: Option<&mut crate::srtp::SrtcpContext>,
    quality: &Arc<std::sync::Mutex<HashMap<u8, RtcpSession>>>,
    track_id: u8,
    clock_rate: u32,
    local_ssrc: u32,
    report_interval: std::time::Duration,
    writer: &Arc<TokioMutex<ControlWriteHalf>>,
) -> bool {
    #[cfg(feature = "srtp")]
    let mut srtcp = srtcp;
    #[cfg(feature = "srtp")]
    let payload = match &mut srtcp {
        Some(context) => match context.unprotect(&frame.payload) {
            Ok(payload) => payload,
            Err(_) => return true,
        },
        None => frame.payload.clone(),
    };
    #[cfg(not(feature = "srtp"))]
    let payload = frame.payload.clone();
    let Ok(packets) = RtcpPacket::parse(payload) else {
        return true;
    };
    let Ok(Some(report)) = super::quality_report(
        quality,
        track_id,
        clock_rate,
        local_ssrc,
        &packets,
        report_interval,
    ) else {
        return true;
    };
    let Ok(payload) = report.try_encode() else {
        return true;
    };
    #[cfg(feature = "srtp")]
    let payload = match &mut srtcp {
        Some(context) => match context.protect(&payload) {
            Ok(payload) => payload,
            Err(_) => return true,
        },
        None => payload,
    };
    #[cfg(not(feature = "srtp"))]
    let payload = payload;
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
