use super::{
    bitstream::{START_CODE, aac_sample_rate},
    source::{
        DemuxedSample, Demuxer, FileSourceOptions, RawFormat, duration_from_ticks,
        file_description, rtp_timestamp, track_description,
    },
};
use bytes::{Bytes, BytesMut};
use rtflow_core::error::{Error, Result};
use rtflow_media::{AccessUnit, Codec, MediaDescription};
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) fn open_raw(
    path: &Path,
    format: RawFormat,
    options: &FileSourceOptions,
) -> Result<Box<dyn Demuxer>> {
    match format {
        RawFormat::H264 => Ok(Box::new(VideoDemuxer::open(
            path,
            Codec::H264,
            options.raw_video_frame_rate,
            options.max_sample_size,
        )?)),
        RawFormat::H265 => Ok(Box::new(VideoDemuxer::open(
            path,
            Codec::H265,
            options.raw_video_frame_rate,
            options.max_sample_size,
        )?)),
        RawFormat::AacAdts => Ok(Box::new(AdtsDemuxer::open(path, options.max_sample_size)?)),
        RawFormat::Pcma | RawFormat::Pcmu => Ok(Box::new(G711Demuxer::open(
            path,
            if format == RawFormat::Pcma {
                Codec::Pcma
            } else {
                Codec::Pcmu
            },
            options.raw_g711_sample_rate,
            options.raw_g711_channels,
            options.raw_g711_frame_duration,
            options.max_sample_size,
        )?)),
    }
}

mod adts;
mod annexb;
mod g711;
#[cfg(test)]
mod tests;
mod wav;

use self::adts::AdtsDemuxer;
use self::annexb::VideoDemuxer;
use self::g711::G711Demuxer;
pub(crate) use self::wav::WavDemuxer;
