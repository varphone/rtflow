#![allow(dead_code)]

use bytes::{Bytes, BytesMut};
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription,
    rtsp::{self as wire_rtsp, Message, Response},
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub fn description(name: &str, codecs: &[Codec]) -> MediaDescription {
    MediaDescription {
        session_name: Arc::from(name),
        tracks: codecs
            .iter()
            .copied()
            .enumerate()
            .map(|(index, codec)| track(index as u8, codec))
            .collect(),
        connection: None,
    }
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
    }
}

pub fn unit(track_id: u8, codec: Codec, sequence: u32) -> AccessUnit {
    let data = match codec {
        // H.264/H.265 access units are now consistently Annex-B (4-byte start
        // code prefix).  The RTSP sender packetizes them and the remote
        // depacketizer reconstructs Annex-B, so the round-trip preserves the
        // start code.
        Codec::H264 => {
            let mut b = BytesMut::with_capacity(8);
            b.extend_from_slice(&[0, 0, 0, 1]);
            b.extend_from_slice(&[0x65, 0x88, 0x84, 0x21]);
            b.freeze()
        }
        Codec::H265 => {
            let mut b = BytesMut::with_capacity(8);
            b.extend_from_slice(&[0, 0, 0, 1]);
            b.extend_from_slice(&[0x26, 0x01, 0xaf, 0x09]);
            b.freeze()
        }
        Codec::Aac => Bytes::from_static(&[0x21, 0x10, 0x04, 0x60]),
        Codec::Pcma => Bytes::from_static(&[0xd5; 160]),
        Codec::Pcmu => Bytes::from_static(&[0xff; 160]),
        Codec::Opus => Bytes::from_static(&[0xfc; 80]),
        Codec::Vp8 | Codec::Vp9 => Bytes::from_static(&[0x10, 0x01, 0x02, 0x03]),
    };
    AccessUnit {
        track_id,
        codec,
        pts: Duration::from_millis(u64::from(sequence) * 20),
        dts: None,
        rtp_timestamp: sequence.saturating_mul(1_800),
        is_keyframe: true,
        data,
    }
}

pub async fn wait_until<F>(timeout: Duration, mut predicate: F)
where
    F: FnMut() -> bool,
{
    tokio::time::timeout(timeout, async {
        while !predicate() {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("condition was not reached before timeout");
}

pub async fn within<T>(duration: Duration, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(duration, future)
        .await
        .expect("operation timed out")
}

pub struct RawRtsp {
    stream: TcpStream,
    input: BytesMut,
    cseq: u32,
}

impl RawRtsp {
    pub async fn connect(address: std::net::SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(address).await.unwrap(),
            input: BytesMut::new(),
            cseq: 1,
        }
    }

    pub async fn request(&mut self, method: &str, uri: &str, headers: &str) -> Response {
        let cseq = self.cseq;
        self.cseq += 1;
        let wire = format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n{headers}\r\n");
        self.stream.write_all(wire.as_bytes()).await.unwrap();
        let mut buffer = [0u8; 4096];
        loop {
            if let Some(Message::Response(response)) =
                wire_rtsp::parse(&mut self.input, 16 * 1024, 256 * 1024).unwrap()
            {
                return response;
            }
            let count = self.stream.read(&mut buffer).await.unwrap();
            assert!(count > 0, "RTSP connection closed before a response");
            self.input.extend_from_slice(&buffer[..count]);
        }
    }
}
