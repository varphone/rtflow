//! Media types and hardware-encoder integration helpers.
//!
//! [`AccessUnit`] carries encoded frames (Annex-B for video, raw AAC/PCM
//! for audio) into the data plane; the `*_video`/`*_audio` constructors
//! below remove the SDP/clock boilerplate when wiring a hardware encoder
//! (HiSilicon HiMPP VENC, V4L2, ...) into the framework.

pub mod bitstream;
pub mod clock;
#[cfg(feature = "multicast")]
pub mod connection;
#[cfg(feature = "srtp")]
pub mod crypto;
pub mod demux;
pub mod flv;
pub mod vod;

#[cfg(feature = "multicast")]
pub use connection::SdpConnection;
#[cfg(feature = "srtp")]
pub use crypto::SdpCrypto;
pub use demux::{DemuxedSample, Demuxer};

/// MPEG-TS packet size in bytes.
pub const TS_PACKET_SIZE: usize = 188;

use bytes::Bytes;
use std::{sync::Arc, time::Duration};
pub type TrackId = u8;
pub type RtpTimestamp = u32;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum Codec {
    H264,
    H265,
    Aac,
    Pcma,
    Pcmu,
    Opus,
    Vp8,
    Vp9,
    /// MPEG-1/2 Audio (MP3), RFC 2250/5219.
    Mp3,
    /// G.726 at 16 kbit/s (RFC 3551).
    G726_16,
    /// G.726 at 24 kbit/s.
    G726_24,
    /// G.726 at 32 kbit/s.
    G726_32,
    /// G.726 at 40 kbit/s.
    G726_40,
    /// AV1 video (draft-ietf-payload-av1).
    Av1,
    /// Motion JPEG (RFC 2435).
    Jpeg,
    /// H.263 (RFC 4629, `H263-1998`).
    H263,
    /// MPEG-4 Part 2 Visual (RFC 3016, `MP4V-ES`).
    Mpeg4,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Video,
    Audio,
}

/// SDP media direction (`a=sendonly`/`a=recvonly`/`a=sendrecv`/`a=inactive`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SdpDirection {
    SendOnly,
    RecvOnly,
    SendRecv,
    Inactive,
}

impl SdpDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SendOnly => "sendonly",
            Self::RecvOnly => "recvonly",
            Self::SendRecv => "sendrecv",
            Self::Inactive => "inactive",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sendonly" => Some(Self::SendOnly),
            "recvonly" => Some(Self::RecvOnly),
            "sendrecv" => Some(Self::SendRecv),
            "inactive" => Some(Self::Inactive),
            _ => None,
        }
    }
}

/// SDP `a=rtcp:<port> [address]` override.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcpAddress {
    pub port: u16,
    pub address: Option<std::net::IpAddr>,
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

impl AccessUnit {
    /// Determine the media kind from the codec.
    pub fn kind(&self) -> MediaKind {
        match self.codec {
            Codec::H264
            | Codec::H265
            | Codec::Vp8
            | Codec::Vp9
            | Codec::Av1
            | Codec::Jpeg
            | Codec::H263
            | Codec::Mpeg4 => MediaKind::Video,
            Codec::Aac
            | Codec::Pcma
            | Codec::Pcmu
            | Codec::Opus
            | Codec::Mp3
            | Codec::G726_16
            | Codec::G726_24
            | Codec::G726_32
            | Codec::G726_40 => MediaKind::Audio,
        }
    }

    /// Construct a video access unit. `data` is an Annex-B H.264/H.265
    /// access unit (start codes included, as produced by HiMPP VENC).
    pub fn video(
        track_id: TrackId,
        codec: Codec,
        pts: Duration,
        rtp_timestamp: RtpTimestamp,
        is_keyframe: bool,
        data: Bytes,
    ) -> Self {
        Self {
            track_id,
            codec,
            pts,
            dts: None,
            rtp_timestamp,
            is_keyframe,
            data,
        }
    }

    /// Construct an audio access unit (raw AAC frame or 8 kHz G.711
    /// sample block; the caller supplies the RTP timestamp).
    pub fn audio(
        track_id: TrackId,
        codec: Codec,
        pts: Duration,
        rtp_timestamp: RtpTimestamp,
        data: Bytes,
    ) -> Self {
        Self {
            track_id,
            codec,
            pts,
            dts: None,
            rtp_timestamp,
            is_keyframe: true,
            data,
        }
    }
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
    /// Video width in pixels (if known).
    pub width: Option<u32>,
    /// Video height in pixels (if known).
    pub height: Option<u32>,
    /// Video frame rate as (numerator, denominator) (if known).
    pub frame_rate: Option<(u32, u32)>,
    /// SDP media direction; `None` means unspecified.
    pub direction: Option<SdpDirection>,
    /// SDP `a=ptime` packetization interval in milliseconds.
    pub ptime: Option<u32>,
    /// SDP `a=rtcp` RTCP port/address override.
    pub rtcp: Option<RtcpAddress>,
    /// SDP `b=AS` bandwidth in kbit/s.
    pub bandwidth_as: Option<u32>,
    /// SRTP crypto parameters from SDP `a=crypto:` attribute.
    #[cfg(feature = "srtp")]
    pub crypto: Option<crate::media::crypto::SdpCrypto>,
}
impl TrackDescription {
    /// H.264 video track from encoder parameter sets.
    ///
    /// `sps`/`pps` are the NAL bodies *without* start codes (extract them
    /// with [`bitstream::h264_parameter_sets`] from an Annex-B IDR).
    /// `sps`/`pps` may be empty for raw single-NAL setups, in which case
    /// the fmtp is a plain `packetization-mode=1` (the mount auto-injects
    /// `sprop-parameter-sets` once a keyframe with parameter sets arrives).
    pub fn h264_video(
        id: TrackId,
        sps: &[u8],
        pps: &[u8],
        width: u32,
        height: u32,
        frame_rate: u32,
    ) -> Self {
        // Empty parameter sets → no fmtp yet: the mount auto-injects
        // sprop-parameter-sets from the first keyframe that carries them
        // (hardware encoders with separated SPS/PPS), which DESCRIBE then
        // merges in. A stream without any parameter sets still works when
        // every IDR carries them inline (single-NAL packaging is valid
        // under the default packetization-mode=0).
        let fmtp = if sps.is_empty() || pps.is_empty() {
            None
        } else {
            Some(Arc::from(bitstream::h264_fmtp(sps, pps)))
        };
        Self {
            id,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
            channels: None,
            payload_type: 96,
            fmtp,
            control: Arc::from(format!("track{id}")),
            width: Some(width),
            height: Some(height),
            frame_rate: Some((frame_rate, 1)),
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }
    }

    /// H.265 video track from encoder parameter sets (VPS/SPS/PPS NAL
    /// bodies without start codes).
    pub fn h265_video(
        id: TrackId,
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
        width: u32,
        height: u32,
        frame_rate: u32,
    ) -> Self {
        let fmtp = if vps.is_empty() || sps.is_empty() || pps.is_empty() {
            None
        } else {
            Some(Arc::from(bitstream::h265_fmtp(vps, sps, pps)))
        };
        Self {
            id,
            kind: MediaKind::Video,
            codec: Codec::H265,
            clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
            channels: None,
            payload_type: 96,
            fmtp,
            control: Arc::from(format!("track{id}")),
            width: Some(width),
            height: Some(height),
            frame_rate: Some((frame_rate, 1)),
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }
    }

    /// AAC-LC audio track (MPEG4-GENERIC, RFC 3640 fmtp with an
    /// auto-generated `config=` AudioSpecificConfig).
    pub fn aac_audio(id: TrackId, sample_rate: u32, channels: u8) -> Self {
        let fmtp = bitstream::aac_audio_specific_config(sample_rate, channels)
            .map(|config| Arc::from(bitstream::aac_fmtp(&config)));
        Self {
            id,
            kind: MediaKind::Audio,
            codec: Codec::Aac,
            clock_rate: sample_rate,
            channels: Some(channels),
            payload_type: 97,
            fmtp,
            control: Arc::from(format!("track{id}")),
            width: None,
            height: None,
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }
    }

    /// G.711 A-law audio track (8 kHz, 1 channel; no fmtp needed).
    pub fn pcma_audio(id: TrackId, sample_rate: u32, channels: u8) -> Self {
        Self {
            id,
            kind: MediaKind::Audio,
            codec: Codec::Pcma,
            clock_rate: sample_rate,
            channels: Some(channels),
            payload_type: 8,
            fmtp: None,
            control: Arc::from(format!("track{id}")),
            width: None,
            height: None,
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }
    }

    /// MPEG-1/2 Audio (MP3) track: 90 kHz clock, static payload type 14.
    pub fn mp3_audio(id: TrackId) -> Self {
        Self {
            id,
            kind: MediaKind::Audio,
            codec: Codec::Mp3,
            clock_rate: 90_000,
            channels: Some(2),
            payload_type: 14,
            fmtp: None,
            control: Arc::from(format!("track{id}")),
            width: None,
            height: None,
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }
    }

    /// G.726 audio track (8 kHz, 1 channel); `codec` selects the bit rate.
    fn g726_audio(id: TrackId, codec: Codec) -> Self {
        Self {
            id,
            kind: MediaKind::Audio,
            codec,
            clock_rate: 8_000,
            channels: Some(1),
            payload_type: 96u8.saturating_add(id),
            fmtp: None,
            control: Arc::from(format!("track{id}")),
            width: None,
            height: None,
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }
    }

    pub fn g726_16_audio(id: TrackId) -> Self {
        Self::g726_audio(id, Codec::G726_16)
    }

    pub fn g726_24_audio(id: TrackId) -> Self {
        Self::g726_audio(id, Codec::G726_24)
    }

    pub fn g726_32_audio(id: TrackId) -> Self {
        Self::g726_audio(id, Codec::G726_32)
    }

    pub fn g726_40_audio(id: TrackId) -> Self {
        Self::g726_audio(id, Codec::G726_40)
    }

    /// AV1 video track (90 kHz clock, dynamic payload type).
    pub fn av1_video(id: TrackId, width: u32, height: u32) -> Self {
        Self::video_track(id, Codec::Av1, width, height)
    }

    /// Motion JPEG video track (90 kHz clock, dynamic payload type).
    pub fn jpeg_video(id: TrackId, width: u32, height: u32) -> Self {
        Self::video_track(id, Codec::Jpeg, width, height)
    }

    /// H.263 video track (`H263-1998`, 90 kHz clock).
    pub fn h263_video(id: TrackId, width: u32, height: u32) -> Self {
        Self::video_track(id, Codec::H263, width, height)
    }

    /// MPEG-4 Part 2 Visual track (`MP4V-ES`, 90 kHz clock).
    pub fn mpeg4_video(id: TrackId, width: u32, height: u32) -> Self {
        Self::video_track(id, Codec::Mpeg4, width, height)
    }

    fn video_track(id: TrackId, codec: Codec, width: u32, height: u32) -> Self {
        Self {
            id,
            kind: MediaKind::Video,
            codec,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96u8.saturating_add(id),
            fmtp: None,
            control: Arc::from(format!("track{id}")),
            width: Some(width),
            height: Some(height),
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }
    }

    /// G.711 μ-law audio track (8 kHz, 1 channel; no fmtp needed).
    pub fn pcmu_audio(id: TrackId, sample_rate: u32, channels: u8) -> Self {
        Self {
            id,
            kind: MediaKind::Audio,
            codec: Codec::Pcmu,
            clock_rate: sample_rate,
            channels: Some(channels),
            payload_type: 0,
            fmtp: None,
            control: Arc::from(format!("track{id}")),
            width: None,
            height: None,
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
        }
    }
}

impl MediaDescription {
    /// Build a description from tracks (ids must be contiguous from 0).
    pub fn from_tracks(session_name: impl Into<Arc<str>>, tracks: Vec<TrackDescription>) -> Self {
        Self {
            session_name: session_name.into(),
            tracks,
            #[cfg(feature = "multicast")]
            connection: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaDescription {
    pub session_name: Arc<str>,
    pub tracks: Vec<TrackDescription>,
    /// SDP connection information (`c=` line). None if not present.
    #[cfg(feature = "multicast")]
    pub connection: Option<crate::media::connection::SdpConnection>,
}
impl MediaDescription {
    /// Build a description with no SDP session connection. Protocol servers
    /// add the negotiated `c=` information when encoding.
    pub fn new(session_name: impl Into<Arc<str>>, tracks: Vec<TrackDescription>) -> Self {
        Self {
            session_name: session_name.into(),
            tracks,
            #[cfg(feature = "multicast")]
            connection: None,
        }
    }

    pub fn track(&self, id: TrackId) -> Option<&TrackDescription> {
        self.tracks.iter().find(|t| t.id == id)
    }
}

/// RTX (RFC 4588) parameters derived for one video track. RTX is advertised
/// only when the server enables it; the payload type is allocated from the
/// dynamic range so it never collides with a media PT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtxConfig {
    pub payload_type: u8,
    pub ssrc: u32,
    pub apt: u8,
}

/// Allocate one RTX PT/SSRC per video track without colliding with any media
/// payload type in the description.
pub fn rtx_configs(description: &MediaDescription) -> std::collections::HashMap<u8, RtxConfig> {
    let used = description
        .tracks
        .iter()
        .map(|track| track.payload_type)
        .collect::<std::collections::HashSet<_>>();
    let mut assigned = std::collections::HashSet::new();
    let mut configs = std::collections::HashMap::new();
    for track in &description.tracks {
        if track.kind != MediaKind::Video || track.payload_type < 96 {
            continue;
        }
        let mut candidates = (96u8..=127).rev();
        let payload_type = candidates
            .find(|pt| !used.contains(pt) && !assigned.contains(pt) && *pt != track.payload_type);
        let Some(payload_type) = payload_type else {
            continue;
        };
        assigned.insert(payload_type);
        configs.insert(
            track.id,
            RtxConfig {
                payload_type,
                ssrc: 0x5254_0000
                    ^ (u32::from(track.id).wrapping_mul(0x9e37_79b9))
                    ^ u32::from(track.payload_type),
                apt: track.payload_type,
            },
        );
    }
    configs
}

#[cfg(test)]
mod rtx_tests {
    use super::*;

    #[test]
    fn rtx_payload_types_are_distinct_and_do_not_collide() {
        let description = MediaDescription::new(
            "rtx",
            vec![
                TrackDescription::h264_video(0, &[], &[], 640, 360, 30),
                TrackDescription::h264_video(1, &[], &[], 320, 180, 30),
            ],
        );
        let configs = rtx_configs(&description);
        assert_eq!(configs.len(), 2);
        let first = configs.get(&0).unwrap();
        let second = configs.get(&1).unwrap();
        assert_ne!(first.payload_type, second.payload_type);
        assert_eq!(first.apt, 96);
        assert!(first.payload_type >= 96 && first.payload_type <= 127);
        assert_ne!(first.ssrc, second.ssrc);
    }
}
