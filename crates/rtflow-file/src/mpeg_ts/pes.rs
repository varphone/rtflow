use super::*;

pub(super) struct ParsedPes<'a> {
    pub(super) pts: Option<u64>,
    pub(super) dts: Option<u64>,
    pub(super) payload: &'a [u8],
}

pub(super) fn parse_pes(data: &[u8]) -> Result<Option<ParsedPes<'_>>> {
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

pub(super) fn parse_pts(data: &[u8]) -> Result<u64> {
    if data.len() < 5 || data[0] & 1 == 0 || data[2] & 1 == 0 || data[4] & 1 == 0 {
        return Err(Error::Media("invalid PES timestamp marker bits".into()));
    }
    Ok((u64::from((data[0] >> 1) & 7) << 30)
        | (u64::from(data[1]) << 22)
        | (u64::from(data[2] >> 1) << 15)
        | (u64::from(data[3]) << 7)
        | u64::from(data[4] >> 1))
}

pub(super) fn unwrap_timestamp(value: u64, previous: Option<u64>) -> u64 {
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

pub(super) fn pes_total_size(data: &[u8]) -> Option<usize> {
    if data.len() < 6 || !data.starts_with(&[0, 0, 1]) {
        return None;
    }
    let declared = u16::from_be_bytes([data[4], data[5]]) as usize;
    (declared != 0).then_some(declared + 6)
}
