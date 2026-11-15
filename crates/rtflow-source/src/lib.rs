//! Built-in live source for demos and protocol tests.
//!
//! Enabled with the `test-source` feature (not part of `full`). Video tracks
//! loop a small, decoder-ready GOP generated from a 176x144/10 fps test
//! pattern; audio tracks loop real encoded packets (or a generated G.711
//! tone). The source is paced per track, so audio and video timestamps are
//! independent and decoders see a normal stream instead of codec-shaped
//! placeholder bytes.

mod assets;

use rtflow_core::error::{Error, Result};
use rtflow_media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};
use rtflow_mount::LocalPublisher;
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Synthetic source pacing and GOP configuration.
#[derive(Clone, Copy, Debug)]
pub struct SyntheticSourceConfig {
    /// Video frames per second (1..=1000).
    pub fps: u32,
    /// Keyframe interval in video frames (>= 1).
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

/// Generate live access units into a mount publisher.
pub struct SyntheticSource;

struct Emitter {
    track: TrackDescription,
    samples: Option<Arc<[assets::Sample]>>,
    index: usize,
    next: Duration,
    units: u64,
    duration: Duration,
    samples_per_packet: u64,
    sample_rate: u64,
    fps: u64,
    force_keyframe: bool,
}

impl Emitter {
    fn new(track: TrackDescription, config: SyntheticSourceConfig) -> Result<Self> {
        let samples = assets::load(track.codec)?;
        if samples.is_none() && !matches!(track.codec, Codec::Pcma | Codec::Pcmu) {
            return Err(Error::Media(format!(
                "no built-in samples for {:?}",
                track.codec
            )));
        }
        let (samples_per_packet, sample_rate) = packet_duration(&track)?;
        let duration = if track.kind == MediaKind::Video {
            if config.fps == 0 {
                return Err(Error::Limit("synthetic source fps"));
            }
            Duration::from_secs_f64(1.0 / f64::from(config.fps))
        } else {
            Duration::from_secs_f64(samples_per_packet as f64 / sample_rate as f64)
        };
        if duration.is_zero() {
            return Err(Error::Limit("synthetic source frame duration"));
        }
        Ok(Self {
            track,
            samples: samples.map(Arc::from),
            index: 0,
            next: Duration::ZERO,
            units: 0,
            duration,
            samples_per_packet,
            sample_rate,
            fps: u64::from(config.fps),
            force_keyframe: false,
        })
    }

    fn unit(&mut self, keyframe_interval: u32) -> AccessUnit {
        let forced = std::mem::take(&mut self.force_keyframe);
        let (data, keyframe) = if let Some(samples) = &self.samples {
            let index = if forced && self.track.kind == MediaKind::Video {
                0
            } else if self.track.kind == MediaKind::Video {
                let gop = (self.units % u64::from(keyframe_interval)) as usize;
                gop.min(samples.len() - 1)
            } else {
                self.index % samples.len()
            };
            self.index = self.index.wrapping_add(1);
            let sample = &samples[index];
            (sample.data.clone(), sample.keyframe || forced)
        } else {
            (assets::g711_tone(self.track.codec, self.units), true)
        };
        let timestamp = if self.track.kind == MediaKind::Video {
            (u128::from(self.units) * u128::from(self.track.clock_rate)
                / u128::from(self.fps.max(1)))
            .min(u128::from(u32::MAX)) as u32
        } else {
            (u128::from(self.units)
                * u128::from(self.samples_per_packet)
                * u128::from(self.track.clock_rate)
                / u128::from(self.sample_rate))
            .min(u128::from(u32::MAX)) as u32
        };
        let pts = Duration::from_secs_f64(f64::from(timestamp) / f64::from(self.track.clock_rate));
        if self.track.kind == MediaKind::Audio {
            AccessUnit::audio(self.track.id, self.track.codec, pts, timestamp, data)
        } else {
            AccessUnit::video(
                self.track.id,
                self.track.codec,
                pts,
                timestamp,
                keyframe,
                data,
            )
        }
    }
}

/// Packet duration in samples at the codec sample rate. Video is paced by fps
/// and returns `(1, clock_rate)`; audio returns the real packet duration.
fn packet_duration(track: &TrackDescription) -> Result<(u64, u64)> {
    Ok(match track.codec {
        Codec::Pcma
        | Codec::Pcmu
        | Codec::G726_16
        | Codec::G726_24
        | Codec::G726_32
        | Codec::G726_40 => (160, 8_000),
        Codec::Aac => (1024, 48_000),
        Codec::Opus => (960, 48_000),
        Codec::Mp3 => (1152, 44_100),
        _ if track.kind == MediaKind::Audio => (1024, u64::from(track.clock_rate)),
        _ => (1, u64::from(track.clock_rate)),
    })
}

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
        let tracks: Vec<TrackDescription> = description.tracks;
        if tracks.is_empty() {
            return Err(Error::Media("synthetic source has no tracks".into()));
        }

        let mut keyframe_requests = publisher.keyframe_requests();
        let mut emitters = Vec::with_capacity(tracks.len());
        for track in tracks {
            emitters.push(Emitter::new(track, config)?);
        }

        // Wake at 5 ms or at the fastest track rate, whichever is smaller.
        let mut tick = Duration::from_millis(5);
        for emitter in &emitters {
            tick = tick.min(emitter.duration);
        }
        if tick.is_zero() {
            return Err(Error::Limit("synthetic source tick"));
        }

        Ok(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tick);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut clock = Duration::ZERO;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    request = keyframe_requests.recv() => {
                        if let Ok(track_id) = request {
                            for emitter in &mut emitters {
                                if emitter.track.id == track_id {
                                    emitter.force_keyframe = true;
                                }
                            }
                        }
                    }
                    _ = ticker.tick() => {}
                }
                clock = clock.saturating_add(tick);
                if publisher.is_closed() {
                    return Ok(());
                }
                for emitter in &mut emitters {
                    while emitter.next <= clock {
                        let unit = emitter.unit(config.keyframe_interval);
                        if publisher.push(unit).is_err() {
                            return Ok(());
                        }
                        emitter.next = emitter.next.saturating_add(emitter.duration);
                        emitter.units = emitter.units.wrapping_add(1);
                    }
                }
            }
        }))
    }
}
