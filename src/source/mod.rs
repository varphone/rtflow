//! Built-in synthetic live source for demos and protocol tests.
//!
//! Enabled with the `test-source` feature (not part of `full`). The source
//! produces codec-shaped access units at a fixed frame rate; the payloads are
//! intentionally tiny and are meant for RTSP/RTP integration tests, not for
//! decoder conformance.

use crate::{
    Error, Result,
    media::{AccessUnit, Codec, MediaDescription, MediaKind},
    mount::LocalPublisher,
};
use bytes::{Bytes, BytesMut};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Synthetic source pacing and GOP configuration.
#[derive(Clone, Copy, Debug)]
pub struct SyntheticSourceConfig {
    /// Frames per second (1..=1000).
    pub fps: u32,
    /// Keyframe interval in frames (>= 1).
    pub keyframe_interval: u32,
}

impl Default for SyntheticSourceConfig {
    fn default() -> Self {
        Self {
            fps: 25,
            keyframe_interval: 25,
        }
    }
}

/// Generate synthetic access units into a mount publisher.
pub struct SyntheticSource;

impl SyntheticSource {
    /// Spawn the source. The task stops when the publisher closes or when the
    /// returned handle is aborted.
    pub fn spawn(
        publisher: LocalPublisher,
        description: MediaDescription,
        config: SyntheticSourceConfig,
    ) -> Result<JoinHandle<Result<()>>> {
        Self::spawn_with_cancel(publisher, description, config, CancellationToken::new())
    }

    /// Spawn the source with an explicit cancellation token.
    pub fn spawn_with_cancel(
        publisher: LocalPublisher,
        description: MediaDescription,
        config: SyntheticSourceConfig,
        cancel: CancellationToken,
    ) -> Result<JoinHandle<Result<()>>> {
        if config.fps == 0 || config.fps > 1_000 {
            return Err(Error::Limit("synthetic source fps"));
        }
        if config.keyframe_interval == 0 {
            return Err(Error::Limit("synthetic source keyframe interval"));
        }
        let tracks: Arc<[crate::media::TrackDescription]> = description.tracks.into();
        if tracks.is_empty() {
            return Err(Error::Media("synthetic source has no tracks".into()));
        }
        Ok(tokio::spawn(async move {
            let frame_duration = Duration::from_secs_f64(1.0 / f64::from(config.fps));
            let mut ticker = tokio::time::interval(frame_duration);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut frame = 0u64;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = ticker.tick() => {}
                }
                if publisher.is_closed() {
                    return Ok(());
                }
                for track in tracks.iter() {
                    let keyframe = track.kind == MediaKind::Audio
                        || frame.is_multiple_of(u64::from(config.keyframe_interval));
                    let unit = make_unit(track, frame, config.fps, keyframe);
                    if publisher.push(unit).is_err() {
                        return Ok(());
                    }
                }
                frame = frame.wrapping_add(1);
            }
        }))
    }
}

fn make_unit(
    track: &crate::media::TrackDescription,
    frame: u64,
    fps: u32,
    keyframe: bool,
) -> AccessUnit {
    let data = payload(track.codec, frame, keyframe);
    let timestamp = (u128::from(frame) * u128::from(track.clock_rate) / u128::from(fps)) as u32;
    let pts = Duration::from_secs_f64(frame as f64 / f64::from(fps));
    if track.kind == MediaKind::Audio {
        AccessUnit::audio(track.id, track.codec, pts, timestamp, data)
    } else {
        AccessUnit::video(track.id, track.codec, pts, timestamp, keyframe, data)
    }
}

fn payload(codec: Codec, frame: u64, keyframe: bool) -> Bytes {
    let seed = (frame & 0xff) as u8;
    match codec {
        Codec::H264 => {
            let mut out = BytesMut::new();
            if keyframe {
                // Start code + plausible SPS/PPS + IDR.
                out.extend_from_slice(&[
                    0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, 0x95, 0xa8, 0x28, 0x0f, 0x00, 0x44, 0xfc,
                    0xb8, 0x08,
                ]);
                out.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
                out.extend_from_slice(&[0, 0, 0, 1, 0x65, seed, 0x88, 0x84]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 1, 0x41, seed, 0x9a, 0x20]);
            }
            out.freeze()
        }
        Codec::H265 => {
            let mut out = BytesMut::new();
            if keyframe {
                out.extend_from_slice(&[0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01]);
                out.extend_from_slice(&[0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01]);
                out.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01, 0xaf, seed]);
            } else {
                out.extend_from_slice(&[0, 0, 0, 1, 0x02, 0x01, 0xd0, seed]);
            }
            out.freeze()
        }
        Codec::Aac => Bytes::from(vec![0x21, 0x10, 0x04, seed]),
        Codec::Pcma => {
            let mut out = vec![0xd5; 160];
            out[0] = seed;
            Bytes::from(out)
        }
        Codec::Pcmu => {
            let mut out = vec![0xff; 160];
            out[0] = seed;
            Bytes::from(out)
        }
        Codec::Opus => Bytes::from(vec![0xfc; 80]),
        Codec::Vp8 => Bytes::from(vec![0x10, seed, 0x00, 0x00]),
        Codec::Vp9 => Bytes::from(vec![0x82, 0x00, seed, 0x00]),
        Codec::Mp3 => Bytes::from(vec![0xff, 0xfb, 0x90, seed, 0x00, 0x00, 0x00]),
        Codec::G726_16 => Bytes::from(vec![0x55; 40]),
        Codec::G726_24 => Bytes::from(vec![0x55; 60]),
        Codec::G726_32 => Bytes::from(vec![0x55; 80]),
        Codec::G726_40 => Bytes::from(vec![0x55; 100]),
        Codec::Av1 => {
            let mut out = vec![0x0a, 0x12];
            out.push(seed);
            out.extend_from_slice(&[0x55; 17]);
            Bytes::from(out)
        }
        Codec::Jpeg => {
            let mut out = vec![0xff, 0xd8, 0xff, 0xda, seed];
            out.extend_from_slice(&[0x33; 16]);
            out.push(0xff);
            out.push(0xd9);
            Bytes::from(out)
        }
        Codec::H263 => {
            let mut out = vec![0x80, seed, 0x00];
            out.extend_from_slice(&[0x44; 16]);
            Bytes::from(out)
        }
        Codec::Mpeg4 => {
            let mut out = vec![0x00, 0x00, 0x01, 0xb6, 0x00, seed];
            out.extend_from_slice(&[0x66; 16]);
            Bytes::from(out)
        }
    }
}
