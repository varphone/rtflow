//! GStreamer pipeline builder for media capture and encoding.

use crate::media::{Codec, MediaKind};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Video source configuration.
#[derive(Clone, Debug)]
pub struct VideoSourceConfig {
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
}

/// Audio source configuration.
#[derive(Clone, Debug)]
pub struct AudioSourceConfig {
    pub device: String,
    pub sample_rate: u32,
    pub channels: u32,
}

/// Encoder configuration.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub bitrate: Option<u32>,
    pub hardware: bool,
}

impl EncoderConfig {
    pub fn h264_hardware() -> Self {
        Self {
            codec: Codec::H264,
            bitrate: None,
            hardware: true,
        }
    }
    pub fn h264_software(bitrate: u32) -> Self {
        Self {
            codec: Codec::H264,
            bitrate: Some(bitrate),
            hardware: false,
        }
    }
    pub fn h265_hardware() -> Self {
        Self {
            codec: Codec::H265,
            bitrate: None,
            hardware: true,
        }
    }
    pub fn aac() -> Self {
        Self {
            codec: Codec::Aac,
            bitrate: None,
            hardware: false,
        }
    }
}

/// Track information derived from GStreamer caps.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub kind: MediaKind,
    pub codec: Codec,
    pub clock_rate: u32,
    pub channels: Option<u8>,
    pub payload_type: u8,
    pub caps: gst::Caps,
}

/// Chain elements returned by pipeline construction.
pub(super) struct ChainElements {
    pub caps: gst::Caps,
}

/// Try to create an element, returning None if the factory is not available.
fn try_element(name: &str) -> Option<gst::Element> {
    gst::ElementFactory::make(name).build().ok()
}

/// Build a video capture + encode chain.
pub(super) fn build_video_chain(
    pipeline: &gst::Pipeline,
    config: &VideoSourceConfig,
    enc: &EncoderConfig,
) -> Result<ChainElements, PipelineError> {
    // Only H.264 encoding is implemented; claiming a different codec in
    // the SDP while emitting H.264 would produce an undecodable stream.
    if enc.codec != crate::media::Codec::H264 {
        return Err(PipelineError(
            "GStreamer video encoder supports H.264 only".into(),
        ));
    }
    let src = gst::ElementFactory::make("v4l2src")
        .property("device", &config.device)
        .build()?;

    let convert = gst::ElementFactory::make("videoconvert").build()?;

    let encoder = if enc.hardware {
        try_element("vpuenc_h264")
            .or_else(|| try_element("v4l2h264enc"))
            .or_else(|| try_element("nvh264enc"))
            .or_else(|| try_element("vaapih264enc"))
            .unwrap_or(
                gst::ElementFactory::make("x264enc")
                    .property("tune", "zerolatency")
                    .build()?,
            )
    } else {
        gst::ElementFactory::make("x264enc")
            .property("bitrate", enc.bitrate.unwrap_or(2000))
            .build()?
    };

    let sink = gst::ElementFactory::make("appsink")
        .property("name", "video-sink")
        .property("emit-signals", true)
        .property("sync", true)
        .build()?;

    pipeline.add_many([&src, &convert, &encoder, &sink])?;

    let caps = gst::Caps::builder("video/x-raw")
        .field("width", config.width as i32)
        .field("height", config.height as i32)
        .field("framerate", gst::Fraction::new(config.framerate as i32, 1))
        .build();

    src.link_filtered(&convert, &caps)?;
    convert.link(&encoder)?;
    encoder.link(&sink)?;

    let output_caps = encoder
        .static_pad("src")
        .and_then(|pad| pad.current_caps())
        .unwrap_or_else(|| gst::Caps::builder("video/x-h264").build());

    Ok(ChainElements { caps: output_caps })
}

/// Build an audio capture + encode chain.
pub(super) fn build_audio_chain(
    pipeline: &gst::Pipeline,
    config: &AudioSourceConfig,
    enc: &EncoderConfig,
) -> Result<ChainElements, PipelineError> {
    let src = try_element("pulsesrc")
        .or_else(|| try_element("alsasrc"))
        .ok_or_else(|| {
            gstreamer::glib::Error::new(
                gst::CoreError::MissingPlugin,
                "no audio source element available (need pulsesrc or alsasrc)",
            )
        })?;

    let convert = gst::ElementFactory::make("audioconvert").build()?;

    let encoder = match enc.codec {
        Codec::Aac => try_element("avenc_aac")
            .or_else(|| try_element("faac"))
            .ok_or_else(|| {
                gstreamer::glib::Error::new(
                    gst::CoreError::MissingPlugin,
                    "no AAC encoder available",
                )
            })?,
        Codec::Pcma => gst::ElementFactory::make("alawenc").build()?,
        Codec::Pcmu => gst::ElementFactory::make("mulawenc").build()?,
        _ => {
            return Err(gstreamer::glib::Error::new(
                gst::CoreError::Negotiation,
                &format!("unsupported audio codec: {:?}", enc.codec),
            )
            .into());
        }
    };

    let sink = gst::ElementFactory::make("appsink")
        .property("name", "audio-sink")
        .property("emit-signals", true)
        .property("sync", true)
        .build()?;

    pipeline.add_many([&src, &convert, &encoder, &sink])?;

    let caps = gst::Caps::builder("audio/x-raw")
        .field("rate", config.sample_rate as i32)
        .field("channels", config.channels as i32)
        .build();

    src.link_filtered(&convert, &caps)?;
    convert.link(&encoder)?;
    encoder.link(&sink)?;

    let output_caps = encoder
        .static_pad("src")
        .and_then(|pad| pad.current_caps())
        .unwrap_or_else(|| gst::Caps::builder("audio/mpeg").build());

    Ok(ChainElements { caps: output_caps })
}

/// Error type for pipeline operations.
#[derive(Debug)]
pub struct PipelineError(pub String);

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PipelineError {}

impl From<gstreamer::glib::Error> for PipelineError {
    fn from(e: gstreamer::glib::Error) -> Self {
        Self(e.to_string())
    }
}

impl From<gstreamer::glib::BoolError> for PipelineError {
    fn from(e: gstreamer::glib::BoolError) -> Self {
        Self(e.to_string())
    }
}

impl From<gst::StateChangeError> for PipelineError {
    fn from(e: gst::StateChangeError) -> Self {
        Self(format!("state change failed: {e:?}"))
    }
}

impl From<PipelineError> for crate::Error {
    fn from(e: PipelineError) -> Self {
        crate::Error::Media(e.0)
    }
}
