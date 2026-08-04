use super::{
    bitstream::{aac_audio_specific_config, aac_sample_rate, is_keyframe},
    source::{DemuxedSample, Demuxer, duration_from_ticks, file_description, track_description},
};
use crate::{
    Error, Result,
    media::{AccessUnit, Codec, MediaDescription},
};
use bytes::Bytes;
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

const TS_SIZE: usize = 188;
const PTS_MODULUS: u64 = 1 << 33;

pub(crate) struct MpegTsDemuxer {
    path: PathBuf,
    reader: TsReader,
    description: MediaDescription,
    tracks: Vec<TrackState>,
    pes: HashMap<u16, Vec<u8>>,
    continuity: HashMap<u16, u8>,
    pending: VecDeque<DemuxedSample>,
    pending_output: Option<DemuxedSample>,
    origin_pts: Option<u64>,
    max_sample_size: usize,
    eof_flushed: bool,
}

struct TrackState {
    pid: u16,
    track_id: u8,
    codec: Codec,
    clock_rate: u32,
    channels: Option<u8>,
    last_pts: Option<u64>,
    last_dts: Option<u64>,
    aac_buffer: Vec<u8>,
    aac_next_tick: Option<u64>,
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

    fn next_inner(&mut self) -> Result<Option<DemuxedSample>> {
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

    fn finalize_pes(&mut self, pid: u16, raw: Vec<u8>) -> Result<()> {
        let Some(index) = self.tracks.iter().position(|track| track.pid == pid) else {
            return Ok(());
        };
        self.finalize_pes_by_index(index, raw)
    }

    fn finalize_pes_by_index(&mut self, track_index: usize, raw: Vec<u8>) -> Result<()> {
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
        }
    }

    fn reset(&mut self) -> Result<()> {
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

fn split_adts(
    payload: &[u8],
    pts_90k: u64,
    track: &mut TrackState,
    max_sample_size: usize,
    output: &mut VecDeque<DemuxedSample>,
) -> Result<()> {
    if track.aac_buffer.is_empty() {
        track.aac_next_tick =
            Some((u128::from(pts_90k) * u128::from(track.clock_rate) / 90_000) as u64);
    }
    if track.aac_buffer.len().saturating_add(payload.len()) > max_sample_size.saturating_add(9) {
        return Err(Error::Limit("AAC access unit size"));
    }
    track.aac_buffer.extend_from_slice(payload);
    loop {
        if track.aac_buffer.len() < 2 {
            return Ok(());
        }
        if !(track.aac_buffer[0] == 0xff && track.aac_buffer[1] & 0xf6 == 0xf0) {
            let Some(sync) = track
                .aac_buffer
                .windows(2)
                .position(|value| value[0] == 0xff && value[1] & 0xf6 == 0xf0)
            else {
                let keep = usize::from(track.aac_buffer.last() == Some(&0xff));
                let tail = track.aac_buffer.split_off(track.aac_buffer.len() - keep);
                track.aac_buffer = tail;
                return Ok(());
            };
            track.aac_buffer.drain(..sync);
        }
        if track.aac_buffer.len() < 7 {
            return Ok(());
        }
        let header = parse_adts_header(&track.aac_buffer)?;
        if header.sample_rate != track.clock_rate || Some(header.channels) != track.channels {
            return Err(Error::Media(
                "MPEG-TS AAC changes ADTS configuration".into(),
            ));
        }
        if header.frame_length > max_sample_size.saturating_add(header.header_size) {
            return Err(Error::Limit("AAC access unit size"));
        }
        if track.aac_buffer.len() < header.frame_length {
            return Ok(());
        }
        let audio_ticks = track
            .aac_next_tick
            .ok_or_else(|| Error::Media("AAC timestamp state is unavailable".into()))?;
        let pts = duration_from_ticks(audio_ticks, u64::from(track.clock_rate));
        output.push_back(DemuxedSample {
            unit: AccessUnit {
                track_id: track.track_id,
                codec: Codec::Aac,
                pts,
                dts: Some(pts),
                rtp_timestamp: audio_ticks as u32,
                is_keyframe: true,
                data: Bytes::copy_from_slice(
                    &track.aac_buffer[header.header_size..header.frame_length],
                ),
            },
            duration: Some(duration_from_ticks(1024, u64::from(track.clock_rate))),
        });
        track.aac_next_tick = Some(audio_ticks.saturating_add(1024));
        track.aac_buffer.drain(..header.frame_length);
        if track.aac_buffer.is_empty() {
            track.aac_next_tick = None;
            return Ok(());
        }
    }
}

struct ParsedPes<'a> {
    pts: Option<u64>,
    dts: Option<u64>,
    payload: &'a [u8],
}

fn parse_pes(data: &[u8]) -> Result<Option<ParsedPes<'_>>> {
    if data.len() < 9 || !data.starts_with(&[0, 0, 1]) {
        return Ok(None);
    }
    let declared = u16::from_be_bytes([data[4], data[5]]) as usize;
    let total = if declared == 0 {
        data.len()
    } else {
        (declared + 6).min(data.len())
    };
    let header_end = 9usize
        .checked_add(usize::from(data[8]))
        .filter(|end| *end <= total)
        .ok_or_else(|| Error::Media("truncated MPEG-TS PES header".into()))?;
    let flags = data[7];
    let pts = if flags & 0x80 != 0 {
        Some(parse_pts(
            data.get(9..14)
                .ok_or_else(|| Error::Media("truncated PES PTS".into()))?,
        )?)
    } else {
        None
    };
    let dts = if flags & 0x40 != 0 {
        Some(parse_pts(
            data.get(14..19)
                .ok_or_else(|| Error::Media("truncated PES DTS".into()))?,
        )?)
    } else {
        None
    };
    Ok(Some(ParsedPes {
        pts,
        dts,
        payload: &data[header_end..total],
    }))
}

fn parse_pts(data: &[u8]) -> Result<u64> {
    if data.len() < 5 || data[0] & 1 == 0 || data[2] & 1 == 0 || data[4] & 1 == 0 {
        return Err(Error::Media("invalid PES timestamp marker bits".into()));
    }
    Ok((u64::from((data[0] >> 1) & 7) << 30)
        | (u64::from(data[1]) << 22)
        | (u64::from(data[2] >> 1) << 15)
        | (u64::from(data[3]) << 7)
        | u64::from(data[4] >> 1))
}

fn unwrap_timestamp(value: u64, previous: Option<u64>) -> u64 {
    let value = value & (PTS_MODULUS - 1);
    let Some(previous) = previous else {
        return value;
    };
    let base = previous & !(PTS_MODULUS - 1);
    let candidates = [
        base.saturating_sub(PTS_MODULUS) | value,
        base | value,
        base.saturating_add(PTS_MODULUS) | value,
    ];
    candidates
        .into_iter()
        .min_by_key(|candidate| candidate.abs_diff(previous))
        .unwrap()
}

fn pes_total_size(data: &[u8]) -> Option<usize> {
    if data.len() < 6 || !data.starts_with(&[0, 0, 1]) {
        return None;
    }
    let declared = u16::from_be_bytes([data[4], data[5]]) as usize;
    (declared != 0).then_some(declared + 6)
}

#[derive(Clone, Copy)]
struct ProgramTrack {
    pid: u16,
    codec: Codec,
}

fn probe_program(reader: &mut TsReader) -> Result<Vec<ProgramTrack>> {
    reader.rewind()?;
    let mut pmt_pid = None;
    let mut sections: HashMap<u16, Vec<u8>> = HashMap::new();
    for _ in 0..32_768 {
        let Some(packet) = reader.next_packet()? else {
            break;
        };
        if !packet.has_payload {
            continue;
        }
        if packet.pid != 0 && Some(packet.pid) != pmt_pid {
            continue;
        }
        if let Some(section) = push_section(
            &mut sections,
            packet.pid,
            packet.payload_unit_start,
            &packet.payload,
        )? {
            if packet.pid == 0 {
                pmt_pid = parse_pat(&section)?;
            } else if Some(packet.pid) == pmt_pid {
                let tracks = parse_pmt(&section)?;
                if !tracks.is_empty() {
                    return Ok(tracks);
                }
            }
        }
    }
    Err(Error::Media(
        "MPEG-TS PAT/PMT not found within probe limit".into(),
    ))
}

fn push_section(
    buffers: &mut HashMap<u16, Vec<u8>>,
    pid: u16,
    unit_start: bool,
    payload: &[u8],
) -> Result<Option<Vec<u8>>> {
    let buffer = buffers.entry(pid).or_default();
    if unit_start {
        let pointer = usize::from(
            *payload
                .first()
                .ok_or_else(|| Error::Media("empty PSI payload".into()))?,
        );
        let start = 1usize
            .checked_add(pointer)
            .filter(|start| *start <= payload.len())
            .ok_or_else(|| Error::Media("invalid PSI pointer field".into()))?;
        buffer.clear();
        buffer.extend_from_slice(&payload[start..]);
    } else if !buffer.is_empty() {
        buffer.extend_from_slice(payload);
    }
    if buffer.len() < 3 {
        return Ok(None);
    }
    let section_length = (usize::from(buffer[1] & 0x0f) << 8) | usize::from(buffer[2]);
    let total = section_length + 3;
    if total > 4096 {
        return Err(Error::Limit("MPEG-TS PSI section"));
    }
    if buffer.len() < total {
        return Ok(None);
    }
    Ok(Some(buffer.drain(..total).collect()))
}

fn parse_pat(section: &[u8]) -> Result<Option<u16>> {
    if section.len() < 12 || section[0] != 0 {
        return Err(Error::Media("invalid MPEG-TS PAT".into()));
    }
    let mut position = 8usize;
    let end = section.len() - 4;
    while position + 4 <= end {
        let program = u16::from_be_bytes([section[position], section[position + 1]]);
        let pid = (u16::from(section[position + 2] & 0x1f) << 8) | u16::from(section[position + 3]);
        if program != 0 {
            return Ok(Some(pid));
        }
        position += 4;
    }
    Ok(None)
}

fn parse_pmt(section: &[u8]) -> Result<Vec<ProgramTrack>> {
    if section.len() < 16 || section[0] != 2 {
        return Err(Error::Media("invalid MPEG-TS PMT".into()));
    }
    let program_info_length = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
    let mut position = 12usize
        .checked_add(program_info_length)
        .ok_or(Error::Limit("MPEG-TS PMT"))?;
    let end = section.len() - 4;
    let mut tracks = Vec::new();
    while position + 5 <= end {
        let stream_type = section[position];
        let pid = (u16::from(section[position + 1] & 0x1f) << 8) | u16::from(section[position + 2]);
        let info_length =
            (usize::from(section[position + 3] & 0x0f) << 8) | usize::from(section[position + 4]);
        let info_end = position + 5 + info_length;
        if info_end > end {
            return Err(Error::Media("truncated MPEG-TS PMT entry".into()));
        }
        let codec = match stream_type {
            0x1b => Some(Codec::H264),
            0x24 => Some(Codec::H265),
            0x0f => Some(Codec::Aac),
            0x90 => Some(Codec::Pcma),
            0x91 => Some(Codec::Pcmu),
            0x06 => private_stream_codec(&section[position + 5..info_end]),
            _ => None,
        };
        if let Some(codec) = codec {
            tracks.push(ProgramTrack { pid, codec });
        }
        position = info_end;
    }
    Ok(tracks)
}

fn private_stream_codec(descriptors: &[u8]) -> Option<Codec> {
    let mut position = 0usize;
    while position + 2 <= descriptors.len() {
        let tag = descriptors[position];
        let length = usize::from(descriptors[position + 1]);
        let end = position + 2 + length;
        if end > descriptors.len() {
            break;
        }
        if tag == 5 && length >= 4 {
            return match &descriptors[position + 2..position + 6] {
                b"PCMA" | b"ALAW" => Some(Codec::Pcma),
                b"PCMU" | b"ULAW" => Some(Codec::Pcmu),
                _ => None,
            };
        }
        position = end;
    }
    None
}

#[derive(Clone, Copy)]
struct AdtsProbe {
    profile: u8,
    frequency_index: u8,
    sample_rate: u32,
    channels: u8,
    frame_length: usize,
    header_size: usize,
}

fn probe_aac(reader: &mut TsReader, tracks: &[ProgramTrack]) -> Result<HashMap<u16, AdtsProbe>> {
    let aac_pids = tracks
        .iter()
        .filter(|track| track.codec == Codec::Aac)
        .map(|track| track.pid)
        .collect::<Vec<_>>();
    let mut found = HashMap::new();
    if aac_pids.is_empty() {
        return Ok(found);
    }
    reader.rewind()?;
    for _ in 0..32_768 {
        let Some(packet) = reader.next_packet()? else {
            break;
        };
        if !aac_pids.contains(&packet.pid) {
            continue;
        }
        for position in 0..packet.payload.len().saturating_sub(6) {
            if packet.payload[position] == 0xff
                && packet.payload[position + 1] & 0xf6 == 0xf0
                && let Ok(header) = parse_adts_header(&packet.payload[position..])
            {
                found.entry(packet.pid).or_insert(header);
                break;
            }
        }
        if found.len() == aac_pids.len() {
            break;
        }
    }
    Ok(found)
}

fn parse_adts_header(data: &[u8]) -> Result<AdtsProbe> {
    if data.len() < 7 || data[0] != 0xff || data[1] & 0xf6 != 0xf0 {
        return Err(Error::Media("invalid ADTS header".into()));
    }
    let frequency_index = (data[2] >> 2) & 0x0f;
    let sample_rate = aac_sample_rate(frequency_index)
        .ok_or_else(|| Error::Media("unsupported ADTS sample rate".into()))?;
    let channels = ((data[2] & 1) << 2) | (data[3] >> 6);
    if channels == 0 {
        return Err(Error::Media("ADTS PCE channels are unsupported".into()));
    }
    let frame_length =
        (usize::from(data[3] & 3) << 11) | (usize::from(data[4]) << 3) | usize::from(data[5] >> 5);
    let header_size = if data[1] & 1 != 0 { 7 } else { 9 };
    if frame_length < header_size {
        return Err(Error::Media("invalid ADTS frame length".into()));
    }
    Ok(AdtsProbe {
        profile: (data[2] >> 6) + 1,
        frequency_index,
        sample_rate,
        channels,
        frame_length,
        header_size,
    })
}

struct TsPacket {
    pid: u16,
    payload_unit_start: bool,
    continuity_counter: u8,
    discontinuity: bool,
    has_payload: bool,
    payload: Vec<u8>,
}

struct TsReader {
    reader: BufReader<File>,
    packet_size: usize,
    sync_offset: usize,
}

impl TsReader {
    fn open(path: &Path) -> Result<Self> {
        let mut reader = BufReader::new(File::open(path)?);
        let mut probe = vec![0u8; 192 * 5];
        let read = reader.read(&mut probe)?;
        probe.truncate(read);
        let (packet_size, sync_offset) = if has_sync_pattern(&probe, 188, 0) {
            (188, 0)
        } else if has_sync_pattern(&probe, 192, 4) {
            (192, 4)
        } else {
            return Err(Error::Media(
                "file is not 188-byte TS or 192-byte M2TS".into(),
            ));
        };
        reader.seek(SeekFrom::Start(0))?;
        Ok(Self {
            reader,
            packet_size,
            sync_offset,
        })
    }

    fn rewind(&mut self) -> Result<()> {
        self.reader.seek(SeekFrom::Start(0))?;
        Ok(())
    }

    fn next_packet(&mut self) -> Result<Option<TsPacket>> {
        let mut packet = vec![0u8; self.packet_size];
        let mut read = 0;
        while read < packet.len() {
            match self.reader.read(&mut packet[read..])? {
                0 if read == 0 => return Ok(None),
                0 => return Err(Error::Media("truncated MPEG-TS packet".into())),
                count => read += count,
            }
        }
        let data = &packet[self.sync_offset..self.sync_offset + TS_SIZE];
        if data[0] != 0x47 {
            return Err(Error::Media("MPEG-TS sync byte lost".into()));
        }
        if data[1] & 0x80 != 0 {
            return Ok(Some(TsPacket {
                pid: 0x1fff,
                payload_unit_start: false,
                continuity_counter: 0,
                discontinuity: true,
                has_payload: false,
                payload: Vec::new(),
            }));
        }
        let payload_unit_start = data[1] & 0x40 != 0;
        let pid = (u16::from(data[1] & 0x1f) << 8) | u16::from(data[2]);
        let adaptation_control = (data[3] >> 4) & 3;
        if adaptation_control == 0 {
            return Err(Error::Media("reserved MPEG-TS adaptation control".into()));
        }
        let has_payload = adaptation_control & 1 != 0;
        let mut offset = 4usize;
        let mut discontinuity = false;
        if adaptation_control & 2 != 0 {
            let length = usize::from(data[offset]);
            if offset + 1 + length > TS_SIZE {
                return Err(Error::Media("invalid MPEG-TS adaptation field".into()));
            }
            if length > 0 {
                discontinuity = data[offset + 1] & 0x80 != 0;
            }
            offset += 1 + length;
        }
        Ok(Some(TsPacket {
            pid,
            payload_unit_start,
            continuity_counter: data[3] & 0x0f,
            discontinuity,
            has_payload,
            payload: if has_payload {
                data[offset..].to_vec()
            } else {
                Vec::new()
            },
        }))
    }
}

fn has_sync_pattern(data: &[u8], packet_size: usize, offset: usize) -> bool {
    (0..4).all(|index| data.get(offset + index * packet_size) == Some(&0x47))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_unwraps_across_33_bit_boundary() {
        let previous = PTS_MODULUS - 5;
        assert_eq!(unwrap_timestamp(3, Some(previous)), PTS_MODULUS + 3);
    }

    #[test]
    fn aac_frame_can_cross_pes_boundaries() {
        let frame = [0xff, 0xf1, 0x4c, 0x80, 0x01, 0x3f, 0xfc, 0xaa, 0xbb];
        let mut track = TrackState {
            pid: 256,
            track_id: 0,
            codec: Codec::Aac,
            clock_rate: 48_000,
            channels: Some(2),
            last_pts: None,
            last_dts: None,
            aac_buffer: Vec::new(),
            aac_next_tick: None,
        };
        let mut output = VecDeque::new();
        split_adts(&frame[..8], 0, &mut track, 1024, &mut output).unwrap();
        assert!(output.is_empty());
        split_adts(&frame[8..], 90_000, &mut track, 1024, &mut output).unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(&output[0].unit.data[..], &[0xaa, 0xbb]);
        assert!(track.aac_buffer.is_empty());
    }

    #[test]
    fn parses_pts() {
        let value = 90_000u64;
        let encoded = [
            0x20 | (((value >> 30) as u8 & 7) << 1) | 1,
            (value >> 22) as u8,
            (((value >> 15) as u8 & 0x7f) << 1) | 1,
            (value >> 7) as u8,
            ((value as u8 & 0x7f) << 1) | 1,
        ];
        assert_eq!(parse_pts(&encoded).unwrap(), value);
    }
}
