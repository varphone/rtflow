use super::*;

pub(super) fn write_description(
    writer: &mut impl Write,
    description: &MediaDescription,
) -> Result<()> {
    writer.write_all(MAGIC)?;
    let name = description.session_name.as_bytes();
    write_len(writer, name.len())?;
    writer.write_all(name)?;
    let tracks =
        u16::try_from(description.tracks.len()).map_err(|_| Error::Limit("recording tracks"))?;
    writer.write_all(&tracks.to_be_bytes())?;
    for track in &description.tracks {
        writer.write_all(&[
            track.id,
            kind_id(track.kind),
            codec_id(track.codec),
            track.channels.unwrap_or(u8::MAX),
            track.payload_type,
        ])?;
        writer.write_all(&track.clock_rate.to_be_bytes())?;
        write_bytes(writer, track.fmtp.as_deref().unwrap_or("").as_bytes())?;
        write_bytes(writer, track.control.as_bytes())?;
    }
    Ok(())
}

pub(super) fn read_description(reader: &mut impl Read) -> Result<MediaDescription> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(Error::Media("invalid recording segment magic".into()));
    }
    let session_name = read_string(reader)?;
    let tracks = read_u16(reader)?;
    let mut descriptions = Vec::with_capacity(usize::from(tracks));
    for _ in 0..tracks {
        let mut fixed = [0u8; 9];
        reader.read_exact(&mut fixed)?;
        let fmtp = read_string(reader)?;
        let control = read_string(reader)?;
        descriptions.push(TrackDescription {
            id: fixed[0],
            kind: kind_from_id(fixed[1])?,
            codec: codec_from_id(fixed[2])?,
            channels: (fixed[3] != u8::MAX).then_some(fixed[3]),
            payload_type: fixed[4],
            clock_rate: u32::from_be_bytes([fixed[5], fixed[6], fixed[7], fixed[8]]),
            fmtp: (!fmtp.is_empty()).then(|| Arc::from(fmtp)),
            control: Arc::from(control),
            width: None,
            height: None,
            frame_rate: None,
            crypto: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            rtx: None,
        });
    }
    Ok(MediaDescription {
        session_name: Arc::from(session_name),
        tracks: descriptions,
        connection: None,
    })
}

pub(super) fn write_bytes(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    write_len(writer, bytes.len())?;
    writer.write_all(bytes)?;
    Ok(())
}
pub(super) fn write_len(writer: &mut impl Write, length: usize) -> Result<()> {
    let length = u16::try_from(length).map_err(|_| Error::Limit("recording metadata"))?;
    writer.write_all(&length.to_be_bytes())?;
    Ok(())
}
pub(super) fn read_u16(reader: &mut impl Read) -> Result<u16> {
    let mut value = [0; 2];
    reader.read_exact(&mut value)?;
    Ok(u16::from_be_bytes(value))
}
pub(super) fn read_string(reader: &mut impl Read) -> Result<String> {
    let length = usize::from(read_u16(reader)?);
    if length > MAX_METADATA {
        return Err(Error::Limit("recording metadata"));
    }
    let mut value = vec![0; length];
    reader.read_exact(&mut value)?;
    String::from_utf8(value).map_err(|_| Error::Media("recording metadata is not UTF-8".into()))
}
pub(super) fn duration_us(value: Duration) -> u64 {
    u64::try_from(value.as_micros()).unwrap_or(u64::MAX)
}
pub(super) fn duration_from_us(value: u64) -> Duration {
    Duration::from_micros(value)
}
pub(super) fn codec_id(codec: Codec) -> u8 {
    match codec {
        Codec::H264 => 1,
        Codec::H265 => 2,
        Codec::Aac => 3,
        Codec::Pcma => 4,
        Codec::Pcmu => 5,
        Codec::Opus => 6,
        Codec::Vp8 => 7,
        Codec::Vp9 => 8,
        Codec::Mp3 => 9,
        Codec::G726_16 => 10,
        Codec::G726_24 => 11,
        Codec::G726_32 => 12,
        Codec::G726_40 => 13,
        Codec::Av1 => 14,
        Codec::Jpeg => 15,
        Codec::H263 => 16,
        Codec::Mpeg4 => 17,
        _ => 255,
    }
}
pub(super) fn codec_from_id(id: u8) -> Result<Codec> {
    match id {
        1 => Ok(Codec::H264),
        2 => Ok(Codec::H265),
        3 => Ok(Codec::Aac),
        4 => Ok(Codec::Pcma),
        5 => Ok(Codec::Pcmu),
        6 => Ok(Codec::Opus),
        7 => Ok(Codec::Vp8),
        8 => Ok(Codec::Vp9),
        9 => Ok(Codec::Mp3),
        10 => Ok(Codec::G726_16),
        11 => Ok(Codec::G726_24),
        12 => Ok(Codec::G726_32),
        13 => Ok(Codec::G726_40),
        14 => Ok(Codec::Av1),
        15 => Ok(Codec::Jpeg),
        16 => Ok(Codec::H263),
        17 => Ok(Codec::Mpeg4),
        _ => Err(Error::Media("unknown recorded codec".into())),
    }
}
pub(super) fn kind_id(kind: MediaKind) -> u8 {
    match kind {
        MediaKind::Video => 1,
        MediaKind::Audio => 2,
    }
}
pub(super) fn kind_from_id(id: u8) -> Result<MediaKind> {
    match id {
        1 => Ok(MediaKind::Video),
        2 => Ok(MediaKind::Audio),
        _ => Err(Error::Media("unknown recorded media kind".into())),
    }
}
pub(super) fn validate_description(description: &MediaDescription) -> Result<()> {
    if description.tracks.is_empty() {
        Err(Error::Media("recording requires at least one track".into()))
    } else {
        Ok(())
    }
}
