//! Helpers shared by the examples (CLI args, tracing, mock access units).

#![allow(dead_code)]

pub mod live_tools;

use bytes::Bytes;
use clap::ValueEnum;
use rtflow::{
    AccessUnit, Codec, Credentials, MediaDescription, MediaKind, TrackDescription,
    transport::UpstreamTransport,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum TransportArg {
    Udp,
    Tcp,
}

impl TransportArg {
    pub fn as_rtsp_name(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }
}

impl From<TransportArg> for UpstreamTransport {
    fn from(value: TransportArg) -> Self {
        match value {
            TransportArg::Udp => Self::Udp,
            TransportArg::Tcp => Self::TcpInterleaved,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ValueEnum)]
pub enum CodecArg {
    H264,
    H265,
    Aac,
    Pcma,
    Pcmu,
}

impl From<CodecArg> for Codec {
    fn from(value: CodecArg) -> Self {
        match value {
            CodecArg::H264 => Self::H264,
            CodecArg::H265 => Self::H265,
            CodecArg::Aac => Self::Aac,
            CodecArg::Pcma => Self::Pcma,
            CodecArg::Pcmu => Self::Pcmu,
        }
    }
}

pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

pub fn mount_path(value: &str) -> Result<Arc<str>, String> {
    if !value.starts_with('/') || value.len() == 1 {
        return Err("mount must start with '/' and contain a name".into());
    }
    Ok(Arc::from(value))
}

pub fn credentials(
    username: Option<String>,
    password: Option<String>,
) -> Result<Option<Credentials>, String> {
    match (username, password) {
        (None, None) => Ok(None),
        (Some(username), Some(password)) => Ok(Some(Credentials { username, password })),
        _ => Err("--username and --password must be supplied together".into()),
    }
}

pub fn description(name: &str, codecs: &[CodecArg]) -> MediaDescription {
    MediaDescription {
        session_name: Arc::from(name),
        tracks: codecs
            .iter()
            .copied()
            .enumerate()
            .map(|(index, value)| track(index as u8, value.into()))
            .collect(),
        connection: None,
    }
}

pub fn publisher_description(name: &str, codecs: &[CodecArg]) -> MediaDescription {
    let mut description = description(name, codecs);
    for track in &mut description.tracks {
        track.control = Arc::from(format!("streamid={}", track.id));
    }
    description
}

pub fn track(id: u8, codec: Codec) -> TrackDescription {
    let (kind, clock_rate, channels, payload_type, fmtp) = match codec {
        Codec::H264 => (
            MediaKind::Video,
            90_000,
            None,
            96,
            Some("packetization-mode=1"),
        ),
        Codec::H265 => (MediaKind::Video, 90_000, None, 98, None),
        Codec::Aac => (
            MediaKind::Audio,
            48_000,
            Some(2),
            97,
            Some(
                "profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1190",
            ),
        ),
        Codec::Pcma => (MediaKind::Audio, 8_000, Some(1), 8, None),
        Codec::Pcmu => (MediaKind::Audio, 8_000, Some(1), 0, None),
        Codec::Opus => (MediaKind::Audio, 48_000, Some(2), 111, None),
        Codec::Vp8 => (MediaKind::Video, 90_000, None, 99, None),
        Codec::Vp9 => (MediaKind::Video, 90_000, None, 100, None),
        Codec::Mp3 => (MediaKind::Audio, 90_000, None, 14, None),
        Codec::G726_16 => (MediaKind::Audio, 8_000, Some(1), 101, None),
        Codec::G726_24 => (MediaKind::Audio, 8_000, Some(1), 102, None),
        Codec::G726_32 => (MediaKind::Audio, 8_000, Some(1), 103, None),
        Codec::G726_40 => (MediaKind::Audio, 8_000, Some(1), 104, None),
        Codec::Av1 => (MediaKind::Video, 90_000, None, 105, None),
        Codec::Jpeg => (MediaKind::Video, 90_000, None, 106, None),
        Codec::H263 => (MediaKind::Video, 90_000, None, 107, None),
        Codec::Mpeg4 => (MediaKind::Video, 90_000, None, 108, None),
        _ => (MediaKind::Audio, 8_000, Some(1), 120, None),
    };
    TrackDescription {
        id,
        kind,
        codec,
        clock_rate,
        channels,
        payload_type,
        fmtp: fmtp.map(Arc::from),
        control: Arc::from(format!("trackID={id}")),
        crypto: None,
        width: None,
        height: None,
        frame_rate: None,
        direction: None,
        ptime: None,
        rtcp: None,
        bandwidth_as: None,
    }
}

pub fn sample_unit(track: &TrackDescription, frame: u64, fps: u32) -> AccessUnit {
    let data = match track.codec {
        Codec::H264 => Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21]),
        Codec::H265 => Bytes::from_static(&[0, 0, 0, 1, 0x26, 0x01, 0xaf, 0x09]),
        Codec::Aac => Bytes::from_static(&[0x21, 0x10, 0x04, 0x60]),
        Codec::Pcma => Bytes::from_static(&[0xd5; 160]),
        Codec::Pcmu => Bytes::from_static(&[0xff; 160]),
        Codec::Opus => Bytes::from_static(&[0xfc; 80]),
        Codec::Vp8 | Codec::Vp9 => Bytes::from_static(&[0x10, 0x01, 0x02, 0x03]),
        Codec::Mp3 => Bytes::from_static(&[0xff, 0xfb, 0x90, 0x00, 0x00, 0x00, 0x00]),
        Codec::G726_16 => Bytes::from_static(&[0x55; 40]),
        Codec::G726_24 => Bytes::from_static(&[0x55; 60]),
        Codec::G726_32 => Bytes::from_static(&[0x55; 80]),
        Codec::G726_40 => Bytes::from_static(&[0x55; 100]),
        Codec::Av1 => Bytes::from_static(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05]),
        Codec::Jpeg => Bytes::from_static(&[0xff, 0xd8, 0xff, 0xda, 0x01, 0x02, 0xff, 0xd9]),
        Codec::H263 => Bytes::from_static(&[0x00, 0x00, 0x80, 0x01, 0x02]),
        Codec::Mpeg4 => Bytes::from_static(&[0x00, 0x00, 0x01, 0xb6, 0x00, 0x01]),
        _ => Bytes::from_static(&[0; 4]),
    };
    let frame_duration = Duration::from_secs_f64(1.0 / f64::from(fps));
    AccessUnit {
        track_id: track.id,
        codec: track.codec,
        pts: frame_duration.mul_f64(frame as f64),
        dts: None,
        rtp_timestamp: frame.wrapping_mul(u64::from(track.clock_rate) / u64::from(fps)) as u32,
        is_keyframe: matches!(track.kind, MediaKind::Audio) || frame.is_multiple_of(u64::from(fps)),
        data,
    }
}

pub fn advertised_url(listen: SocketAddr, mount: &str) -> String {
    let host = if listen.ip().is_unspecified() {
        "127.0.0.1".to_owned()
    } else {
        listen.ip().to_string()
    };
    format!("rtsp://{host}:{}{mount}", listen.port())
}
