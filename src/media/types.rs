use bytes::Bytes;
use std::{sync::Arc, time::Duration};
pub type TrackId = u8;
pub type RtpTimestamp = u32;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Codec {
    H264,
    H265,
    Aac,
    Pcma,
    Pcmu,
    Opus,
    Vp8,
    Vp9,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Video,
    Audio,
}
#[derive(Clone, Debug)]
pub struct AccessUnit {
    pub track_id: TrackId,
    pub codec: Codec,
    pub pts: Duration,
    pub dts: Option<Duration>,
    pub rtp_timestamp: RtpTimestamp,
    pub is_keyframe: bool,
    pub data: Bytes,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackDescription {
    pub id: TrackId,
    pub kind: MediaKind,
    pub codec: Codec,
    pub clock_rate: u32,
    pub channels: Option<u8>,
    pub payload_type: u8,
    pub fmtp: Option<Arc<str>>,
    pub control: Arc<str>,
    /// SRTP crypto parameters from SDP `a=crypto:` attribute.
    #[cfg(feature = "srtp")]
    pub crypto: Option<crate::protocol::sdp::SdpCrypto>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaDescription {
    pub session_name: Arc<str>,
    pub tracks: Vec<TrackDescription>,
    /// SDP connection information (`c=` line). None if not present.
    #[cfg(feature = "multicast")]
    pub connection: Option<crate::protocol::sdp::SdpConnection>,
}
impl MediaDescription {
    pub fn track(&self, id: TrackId) -> Option<&TrackDescription> {
        self.tracks.iter().find(|t| t.id == id)
    }
}
