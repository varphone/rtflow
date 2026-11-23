use super::psi::ProgramTrack;
use super::*;

pub(super) fn split_adts(
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
                nal_offsets: None,
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

#[derive(Clone, Copy)]
pub(super) struct AdtsProbe {
    pub(super) profile: u8,
    pub(super) frequency_index: u8,
    pub(super) sample_rate: u32,
    pub(super) channels: u8,
    pub(super) frame_length: usize,
    pub(super) header_size: usize,
}

pub(super) fn probe_aac(
    reader: &mut TsReader,
    tracks: &[ProgramTrack],
) -> Result<HashMap<u16, AdtsProbe>> {
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

pub(super) fn parse_adts_header(data: &[u8]) -> Result<AdtsProbe> {
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
