//! GStreamer integration for zero-copy media streaming.

pub mod app_sink;
pub mod app_src;
pub mod buffer;
pub mod pipeline;

use crate::media::{Codec, MediaDescription, MediaKind, TrackDescription};
use crate::mount::{LocalPublisher, MountConfig, MountRegistry};
#[cfg(feature = "server")]
use crate::server::RtspServer;
use crate::{Error, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use pipeline::{AudioSourceConfig, EncoderConfig, TrackInfo, VideoSourceConfig};
use std::mem::ManuallyDrop;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// GStreamer source (push-mode appsink capture).
pub struct GStreamerSource {
    pipeline: gst::Pipeline,
    tracks: Vec<TrackInfo>,
}

/// Builder for constructing a GStreamerSource.
pub struct GstPipelineBuilder {
    video: Option<VideoSourceConfig>,
    audio: Option<AudioSourceConfig>,
    video_encoder: Option<EncoderConfig>,
    audio_encoder: Option<EncoderConfig>,
}

impl GstPipelineBuilder {
    pub fn new() -> Self {
        Self {
            video: None,
            audio: None,
            video_encoder: None,
            audio_encoder: None,
        }
    }

    pub fn video(mut self, config: VideoSourceConfig) -> Self {
        self.video = Some(config);
        self
    }

    pub fn audio(mut self, config: AudioSourceConfig) -> Self {
        self.audio = Some(config);
        self
    }

    pub fn encoder(mut self, config: EncoderConfig) -> Self {
        match config.codec {
            Codec::H264 | Codec::H265 | Codec::Vp8 | Codec::Vp9 => {
                self.video_encoder = Some(config)
            }
            Codec::Aac | Codec::Pcma | Codec::Pcmu | Codec::Opus => {
                self.audio_encoder = Some(config)
            }
            Codec::Mp3 | Codec::G726_16 | Codec::G726_24 | Codec::G726_32 | Codec::G726_40 => {}
            Codec::Av1 | Codec::Jpeg | Codec::H263 | Codec::Mpeg4 => {}
        }
        self
    }

    pub fn video_encoder(mut self, config: EncoderConfig) -> Self {
        self.video_encoder = Some(config);
        self
    }

    pub fn audio_encoder(mut self, config: EncoderConfig) -> Self {
        self.audio_encoder = Some(config);
        self
    }

    pub fn build(self) -> Result<GStreamerSource> {
        gst::init().map_err(|e| Error::Media(format!("GStreamer init failed: {e}")))?;

        let pipeline = gst::Pipeline::new();
        let mut tracks = Vec::new();

        if let (Some(video), Some(enc)) = (&self.video, &self.video_encoder) {
            let chain = pipeline::build_video_chain(&pipeline, video, enc)?;
            tracks.push(TrackInfo {
                kind: MediaKind::Video,
                codec: enc.codec,
                clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
                channels: None,
                payload_type: 96,
                caps: chain.caps,
            });
        }

        if let (Some(audio), Some(enc)) = (&self.audio, &self.audio_encoder) {
            let chain = pipeline::build_audio_chain(&pipeline, audio, enc)?;
            let (clock_rate, channels) = match enc.codec {
                Codec::Aac => (48_000, Some(2)),
                Codec::Pcma | Codec::Pcmu => (8_000, Some(1)),
                _ => (48_000, None),
            };
            tracks.push(TrackInfo {
                kind: MediaKind::Audio,
                codec: enc.codec,
                clock_rate,
                channels,
                payload_type: 97,
                caps: chain.caps,
            });
        }

        if tracks.is_empty() {
            return Err(Error::Media("no tracks configured".into()));
        }

        Ok(GStreamerSource { pipeline, tracks })
    }
}

impl Default for GstPipelineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the AudioSpecificConfig from GStreamer AAC caps
/// (`codec_data`) and build the RFC 3640 fmtp line; without it downstream
/// players cannot initialize an AAC decoder.
fn aac_fmtp_from_caps(caps: &gstreamer::Caps) -> Option<String> {
    let structure = caps.structure(0)?;
    let value = structure.value("codec_data").ok()?;
    let codec_data: gstreamer::glib::Bytes = value.get().ok()?;
    let bytes = codec_data.as_ref();
    if bytes.is_empty() {
        return None;
    }
    let mut config = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(config, "{byte:02X}");
    }
    Some(format!(
        "profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config={config}"
    ))
}

impl GStreamerSource {
    /// Build a MediaDescription from the configured tracks.
    pub fn build_media_description(&self) -> MediaDescription {
        let tracks = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| TrackDescription {
                id: i as u8,
                kind: t.kind,
                codec: t.codec,
                clock_rate: t.clock_rate,
                channels: t.channels,
                payload_type: t.payload_type,
                fmtp: if t.codec == Codec::Aac {
                    aac_fmtp_from_caps(&t.caps).map(Arc::from)
                } else {
                    None
                },
                control: Arc::from(format!("trackID={i}")),
                width: None,
                height: None,
                frame_rate: None,
                #[cfg(feature = "srtp")]
                crypto: None,
                direction: None,
                ptime: None,
                rtcp: None,
                bandwidth_as: None,
                rtx: None,
            })
            .collect();

        MediaDescription {
            session_name: Arc::from("gstreamer-source"),
            tracks,
            #[cfg(feature = "multicast")]
            connection: None,
        }
    }

    /// Register with an RTSP server and start push-mode playback.
    #[cfg(feature = "server")]
    pub async fn register_and_play(
        self,
        server: &RtspServer,
        path: impl Into<Arc<str>>,
    ) -> Result<GstPlayback> {
        let description = self.build_media_description();
        let handle = server
            .register_mount(path, MountConfig::Local { description })
            .await?;
        let publisher = handle.publisher()?;
        self.play(publisher)
    }

    /// Register with a mount registry and start push-mode playback.
    ///
    /// Protocol-agnostic variant of [`Self::register_and_play`]: it only needs
    /// the shared [`MountRegistry`], so it is available without the `server`
    /// feature (the mount can then be served by RTMP, SRT, HTTP or WebRTC).
    pub fn register_and_play_with(
        self,
        mounts: &MountRegistry,
        path: impl Into<Arc<str>>,
    ) -> Result<GstPlayback> {
        let description = self.build_media_description();
        let handle = mounts.register(path.into(), MountConfig::Local { description })?;
        let publisher = handle.publisher()?;
        self.play(publisher)
    }

    /// Start push-mode playback to a LocalPublisher.
    pub fn play(self, publisher: LocalPublisher) -> Result<GstPlayback> {
        let bus = self
            .pipeline
            .bus()
            .ok_or_else(|| Error::Media("pipeline has no bus".into()))?;

        let publisher = Arc::new(publisher);

        for (i, track) in self.tracks.iter().enumerate() {
            let sink_name = match track.kind {
                MediaKind::Video => "video-sink",
                MediaKind::Audio => "audio-sink",
            };
            let appsink = self
                .pipeline
                .by_name(sink_name)
                .ok_or_else(|| Error::Media(format!("appsink '{sink_name}' not found")))?
                .downcast::<gst_app::AppSink>()
                .map_err(|_| Error::Media(format!("'{sink_name}' is not AppSink")))?;

            app_sink::setup_push_callback(
                &appsink,
                publisher.clone(),
                i as u8,
                track.codec,
                track.clock_rate,
            );
        }

        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| Error::Media(format!("pipeline play failed: {e}")))?;

        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let pipeline_ref = self.pipeline.clone();

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_cancel.cancelled() => break,
                    msg = async { bus.timed_pop(gst::ClockTime::from_seconds(1)) } => {
                        match msg {
                            Some(msg) => match msg.view() {
                                gst::MessageView::Error(err) => {
                                    return Err(Error::Media(format!(
                                        "GStreamer error: {} ({:?})",
                                        err.error().message(),
                                        err.debug()
                                    )));
                                }
                                gst::MessageView::Eos(_) => break,
                                _ => {}
                            },
                            None => continue,
                        }
                    }
                }
            }

            let _ = pipeline_ref.set_state(gst::State::Null);
            Ok(())
        });

        Ok(GstPlayback {
            pipeline: self.pipeline,
            cancel,
            task: ManuallyDrop::new(task),
        })
    }

    /// Get the GStreamer pipeline for pull-mode setup.
    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    /// Get the track info.
    pub fn tracks(&self) -> &[TrackInfo] {
        &self.tracks
    }
}

/// Handle to a running GStreamer playback.
pub struct GstPlayback {
    pipeline: gst::Pipeline,
    cancel: CancellationToken,
    task: ManuallyDrop<JoinHandle<Result<()>>>,
}

impl GstPlayback {
    /// Wait for the playback to finish.
    pub async fn wait(mut self) -> Result<()> {
        let task = unsafe { ManuallyDrop::take(&mut self.task) };
        // Prevent Drop from running since we took the task
        std::mem::forget(self);
        task.await.map_err(|e| Error::Media(e.to_string()))?
    }

    /// Stop the playback gracefully (send EOS).
    pub fn stop(&self) {
        self.cancel.cancel();
        let _ = self.pipeline.send_event(gst::event::Eos::new());
    }

    /// Abort the playback immediately.
    pub fn abort(&self) {
        self.cancel.cancel();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for GstPlayback {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        // Safety: task is always initialized and we're in drop
        unsafe { ManuallyDrop::take(&mut self.task) }.abort();
    }
}
