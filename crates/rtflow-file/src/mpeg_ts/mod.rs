use super::{
    bitstream::{aac_audio_specific_config, aac_sample_rate, is_keyframe},
    source::{DemuxedSample, Demuxer, duration_from_ticks, file_description, track_description},
};
use bytes::Bytes;
use rtflow_core::error::{Error, Result};
use rtflow_media::{AccessUnit, Codec, MediaDescription};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

pub(super) const TS_SIZE: usize = rtflow_media::TS_PACKET_SIZE;
pub(super) const PTS_MODULUS: u64 = 1 << 33;

pub(crate) struct MpegTsDemuxer {
    pub(super) path: PathBuf,
    pub(super) reader: TsReader,
    pub(super) description: MediaDescription,
    pub(super) tracks: Vec<TrackState>,
    pub(super) pes: HashMap<u16, Vec<u8>>,
    pub(super) continuity: HashMap<u16, u8>,
    pub(super) pending: VecDeque<DemuxedSample>,
    pub(super) pending_output: Option<DemuxedSample>,
    pub(super) origin_pts: Option<u64>,
    pub(super) max_sample_size: usize,
    pub(super) eof_flushed: bool,
}

pub(super) struct TrackState {
    pub(super) pid: u16,
    pub(super) track_id: u8,
    pub(super) codec: Codec,
    pub(super) clock_rate: u32,
    pub(super) channels: Option<u8>,
    pub(super) last_pts: Option<u64>,
    pub(super) last_dts: Option<u64>,
    pub(super) aac_buffer: Vec<u8>,
    pub(super) aac_next_tick: Option<u64>,
}

impl MpegTsDemuxer {
    pub fn open(path: &Path, max_sample_size: usize) -> Result<Self> {
        let mut reader = TsReader::open(path)?;
        let program_tracks = probe_program(&mut reader)?;
        let aac_configs = probe_aac(&mut reader, &program_tracks)?;
        let mut tracks = Vec::new();
        let mut descriptions = Vec::new();
        for program in program_tracks {
            let (clock_rate, channels, fmtp) = match program.codec {
                Codec::H264 | Codec::H265 => (90_000, None, None),
                Codec::Aac => {
                    let config = aac_configs.get(&program.pid).ok_or_else(|| {
                        Error::Media(format!(
                            "MPEG-TS AAC PID {} has no readable ADTS configuration",
                            program.pid
                        ))
                    })?;
                    let asc = [
                        (config.profile << 3) | (config.frequency_index >> 1),
                        ((config.frequency_index & 1) << 7) | (config.channels << 3),
                    ];
                    let (sample_rate, channels, fmtp) = aac_audio_specific_config(&asc)?;
                    (sample_rate, Some(channels), Some(fmtp))
                }
                Codec::Pcma | Codec::Pcmu => (8_000, Some(1), None),
                Codec::Opus => (48_000, Some(2), None),
                Codec::Vp8 | Codec::Vp9 => (90_000, None, None),
                Codec::Mp3
                | Codec::G726_16
                | Codec::G726_24
                | Codec::G726_32
                | Codec::G726_40
                | Codec::Av1
                | Codec::Jpeg
                | Codec::H263
                | Codec::Mpeg4 => {
                    return Err(Error::Media(
                        "MPEG-TS track uses a codec the demuxer does not support".into(),
                    ));
                }
                _ => {
                    return Err(Error::Media(
                        "MPEG-TS track uses a codec the demuxer does not support".into(),
                    ));
                }
            };
            let track_id =
                u8::try_from(tracks.len()).map_err(|_| Error::Limit("file media tracks"))?;
            if track_id > 31 {
                return Err(Error::Limit("dynamic RTP payload types"));
            }
            descriptions.push(track_description(
                track_id,
                program.codec,
                clock_rate,
                channels,
                fmtp,
            ));
            tracks.push(TrackState {
                pid: program.pid,
                track_id,
                codec: program.codec,
                clock_rate,
                channels,
                last_pts: None,
                last_dts: None,
                aac_buffer: Vec::new(),
                aac_next_tick: None,
            });
        }
        if tracks.is_empty() {
            return Err(Error::Media(
                "MPEG-TS PMT has no supported elementary streams".into(),
            ));
        }
        reader.rewind()?;
        Ok(Self {
            path: path.to_owned(),
            reader,
            description: file_description(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("MPEG-TS file"),
                descriptions,
            ),
            tracks,
            pes: HashMap::new(),
            continuity: HashMap::new(),
            pending: VecDeque::new(),
            pending_output: None,
            origin_pts: None,
            max_sample_size,
            eof_flushed: false,
        })
    }

    pub(super) fn next_inner(&mut self) -> Result<Option<DemuxedSample>> {
        loop {
            if let Some(sample) = self.pending.pop_front() {
                return Ok(Some(sample));
            }
            let packet = self.reader.next_packet()?;
            let Some(packet) = packet else {
                if self.eof_flushed {
                    // A trailing partial AAC frame (truncated recording) is
                    // common and must not fail the whole playback.
                    for track in self.tracks.iter_mut() {
                        if !track.aac_buffer.is_empty() {
                            tracing::warn!(
                                "mpeg-ts: discarding {} trailing bytes of an incomplete AAC frame at EOF",
                                track.aac_buffer.len()
                            );
                            track.aac_buffer.clear();
                        }
                    }
                    return Ok(None);
                }
                self.eof_flushed = true;
                let mut pids = self.pes.keys().copied().collect::<Vec<_>>();
                pids.sort_unstable();
                for pid in pids {
                    if let Some(raw) = self.pes.remove(&pid) {
                        self.finalize_pes(pid, raw)?;
                    }
                }
                continue;
            };
            if !packet.has_payload {
                continue;
            }
            let Some(track_index) = self.tracks.iter().position(|track| track.pid == packet.pid)
            else {
                continue;
            };
            if packet.discontinuity {
                self.pes.remove(&packet.pid);
                self.continuity.remove(&packet.pid);
                self.tracks[track_index].aac_buffer.clear();
                self.tracks[track_index].aac_next_tick = None;
            }
            if let Some(previous) = self
                .continuity
                .insert(packet.pid, packet.continuity_counter)
            {
                let expected = previous.wrapping_add(1) & 0x0f;
                if expected != packet.continuity_counter {
                    self.pes.remove(&packet.pid);
                    self.tracks[track_index].aac_buffer.clear();
                    self.tracks[track_index].aac_next_tick = None;
                    continue;
                }
            }
            if packet.payload_unit_start {
                if let Some(raw) = self.pes.remove(&packet.pid) {
                    self.finalize_pes(packet.pid, raw)?;
                }
                self.pes.insert(packet.pid, Vec::new());
            }
            let buffer = self.pes.entry(packet.pid).or_default();
            if buffer.len() + packet.payload.len() > self.max_sample_size.saturating_mul(16) {
                return Err(Error::Limit("MPEG-TS PES size"));
            }
            buffer.extend_from_slice(&packet.payload);
            let complete = pes_total_size(buffer).is_some_and(|size| buffer.len() >= size);
            if complete && let Some(raw) = self.pes.remove(&packet.pid) {
                self.finalize_pes_by_index(track_index, raw)?;
            }
        }
    }

    pub(super) fn finalize_pes(&mut self, pid: u16, raw: Vec<u8>) -> Result<()> {
        let Some(index) = self.tracks.iter().position(|track| track.pid == pid) else {
            return Ok(());
        };
        self.finalize_pes_by_index(index, raw)
    }

    pub(super) fn finalize_pes_by_index(&mut self, track_index: usize, raw: Vec<u8>) -> Result<()> {
        let Some(parsed) = parse_pes(&raw)? else {
            return Ok(());
        };
        if parsed.payload.is_empty() {
            return Ok(());
        }
        let raw_pts = parsed
            .pts
            .or(parsed.dts)
            .ok_or_else(|| Error::Media("MPEG-TS PES has no PTS".into()))?;
        let track = &mut self.tracks[track_index];
        let pts_unwrapped = unwrap_timestamp(raw_pts, track.last_pts);
        track.last_pts = Some(pts_unwrapped);
        let dts_unwrapped = parsed
            .dts
            .map(|value| unwrap_timestamp(value, track.last_dts));
        if let Some(value) = dts_unwrapped {
            track.last_dts = Some(value);
        }
        let origin = *self.origin_pts.get_or_insert(pts_unwrapped);
        let pts_90k = pts_unwrapped.saturating_sub(origin);
        let dts_90k = dts_unwrapped.map(|value| value.saturating_sub(origin));
        match track.codec {
            Codec::Aac => split_adts(
                parsed.payload,
                pts_90k,
                track,
                self.max_sample_size,
                &mut self.pending,
            ),
            Codec::Mp3
            | Codec::G726_16
            | Codec::G726_24
            | Codec::G726_32
            | Codec::G726_40
            | Codec::Av1
            | Codec::Jpeg
            | Codec::H263
            | Codec::Mpeg4 => Err(Error::Media(
                "MPEG-TS demuxer does not support this codec".into(),
            )),
            Codec::H264 | Codec::H265 => {
                if parsed.payload.len() > self.max_sample_size {
                    return Err(Error::Limit("MPEG-TS video access unit size"));
                }
                let pts = duration_from_ticks(pts_90k, 90_000);
                let dts = dts_90k.map(|value| duration_from_ticks(value, 90_000));
                self.pending.push_back(DemuxedSample {
                    unit: AccessUnit {
                        track_id: track.track_id,
                        codec: track.codec,
                        pts,
                        dts,
                        rtp_timestamp: pts_90k as u32,
                        is_keyframe: is_keyframe(track.codec, parsed.payload),
                        data: Bytes::copy_from_slice(parsed.payload),
                    },
                    duration: None,
                });
                Ok(())
            }
            Codec::Pcma | Codec::Pcmu => {
                if parsed.payload.len() > self.max_sample_size {
                    return Err(Error::Limit("MPEG-TS G.711 access unit size"));
                }
                let pts = duration_from_ticks(pts_90k, 90_000);
                self.pending.push_back(DemuxedSample {
                    unit: AccessUnit {
                        track_id: track.track_id,
                        codec: track.codec,
                        pts,
                        dts: dts_90k.map(|value| duration_from_ticks(value, 90_000)),
                        rtp_timestamp: (u128::from(pts_90k) * u128::from(track.clock_rate) / 90_000)
                            as u32,
                        is_keyframe: true,
                        data: Bytes::copy_from_slice(parsed.payload),
                    },
                    duration: track.channels.map(|channels| {
                        duration_from_ticks(
                            parsed.payload.len() as u64 / u64::from(channels),
                            u64::from(track.clock_rate),
                        )
                    }),
                });
                Ok(())
            }
            Codec::Opus | Codec::Vp8 | Codec::Vp9 => {
                Err(Error::Media("MPEG-TS does not support Opus/VP8/VP9".into()))
            }
            _ => Err(Error::Media("MPEG-TS does not support this codec".into())),
        }
    }

    pub(super) fn reset(&mut self) -> Result<()> {
        self.reader = TsReader::open(&self.path)?;
        self.pes.clear();
        self.continuity.clear();
        self.pending.clear();
        self.pending_output = None;
        self.origin_pts = None;
        self.eof_flushed = false;
        for track in &mut self.tracks {
            track.last_pts = None;
            track.last_dts = None;
            track.aac_buffer.clear();
            track.aac_next_tick = None;
        }
        Ok(())
    }
}

impl Demuxer for MpegTsDemuxer {
    fn description(&self) -> &MediaDescription {
        &self.description
    }
    fn duration(&self) -> Option<Duration> {
        None
    }
    fn next_sample(&mut self) -> Result<Option<DemuxedSample>> {
        if self.pending_output.is_some() {
            return Ok(self.pending_output.take());
        }
        self.next_inner()
    }
    fn seek(&mut self, position: Duration) -> Result<()> {
        self.reset()?;
        while let Some(sample) = self.next_inner()? {
            if sample.unit.pts >= position {
                self.pending_output = Some(sample);
                break;
            }
        }
        Ok(())
    }
}

mod adts;
mod pes;
mod psi;
mod reader;
#[cfg(test)]
mod tests;

use self::adts::{probe_aac, split_adts};
use self::pes::{parse_pes, pes_total_size, unwrap_timestamp};
use self::psi::probe_program;
use self::reader::TsReader;
