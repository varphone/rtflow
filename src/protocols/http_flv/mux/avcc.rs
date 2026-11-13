use bytes::{BufMut, Bytes, BytesMut};

pub(super) fn is_length_prefixed_chain(data: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if len == 0 {
            return false; // empty NAL: not a valid AVCC chain
        }
        pos += 4 + len;
        if pos == data.len() {
            return true;
        }
        if pos > data.len() {
            return false;
        }
    }
    false
}

/// Converted AVCC/HVCC (length-prefixed) size of `data`, or `None` when
/// `data` is already length-prefixed (AVCC/HVCC, e.g. from RTMP ingest).
///
/// Detection: length-prefixed chains pass through untouched; payloads
/// starting with an Annex-B start code are converted (single scan, no
/// allocation); anything else (raw single NAL without framing) also
/// passes through, matching the pre-optimization behaviour.
pub(super) fn annex_b_avcc_size(data: &Bytes) -> Option<usize> {
    let bytes = data.as_ref();
    if is_length_prefixed_chain(bytes) {
        return None;
    }
    let starts_with_start_code = bytes.len() >= 4
        && ((bytes[0] == 0 && bytes[1] == 0 && bytes[2] == 1)
            || (bytes[0] == 0 && bytes[1] == 0 && bytes[2] == 0 && bytes[3] == 1));
    if !starts_with_start_code {
        return None;
    }
    let mut total = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let Some(start) = find_start_code(bytes, i) else {
            break;
        };
        let nal_end = find_start_code(bytes, start + 3).unwrap_or(bytes.len());
        let nal_data_start = if start + 3 < bytes.len() && bytes[start + 2] == 0x01 {
            start + 3
        } else if start + 4 < bytes.len() && bytes[start + 3] == 0x01 {
            start + 4
        } else {
            start + 3
        };
        if nal_data_start < nal_end {
            total += 4 + (nal_end - nal_data_start);
        }
        i = nal_end;
    }
    Some(total)
}

/// Append `data` to `buf` as length-prefixed NALs (Annex-B → AVCC).
/// Already length-prefixed payloads are copied verbatim. No allocation.
pub(super) fn write_annex_b_avcc(buf: &mut BytesMut, data: &Bytes) {
    if annex_b_avcc_size(data).is_none() {
        buf.put_slice(data);
        return;
    }
    let bytes = data.as_ref();
    let mut i = 0;
    while i < bytes.len() {
        let Some(start) = find_start_code(bytes, i) else {
            break;
        };
        let nal_end = find_start_code(bytes, start + 3).unwrap_or(bytes.len());
        let nal_data_start = if start + 3 < bytes.len() && bytes[start + 2] == 0x01 {
            start + 3
        } else if start + 4 < bytes.len() && bytes[start + 3] == 0x01 {
            start + 4
        } else {
            start + 3
        };
        if nal_data_start < nal_end {
            buf.put_u32((nal_end - nal_data_start) as u32);
            buf.put_slice(&bytes[nal_data_start..nal_end]);
        }
        i = nal_end;
    }
}

/// H.264 video tag with Annex-B → AVCC conversion performed directly into
/// `buf`: the tag header carries the converted size, then the
/// length-prefixed NALs are written in place. Single copy, no intermediate
/// allocation (a ~2× reduction of the per-frame copy/alloc cost that
/// matters on low-end IPC CPUs). Length-prefixed payloads (RTMP ingest)
/// are copied as-is.
#[cfg(test)]
pub(super) fn annex_b_to_avcc(data: &Bytes) -> Bytes {
    // Fast path: the payload is already AVCC/HVCC (length-prefixed), which
    // is what RTMP ingest delivers. Detecting it avoids a full-frame scan
    // and rebuild; the Bytes clone is a reference-count bump only.
    if data.len() >= 4
        && u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize == data.len() - 4
    {
        return data.clone();
    }
    let ranges = annex_b_nal_ranges(data);
    if ranges.is_empty() {
        // No start code — treat whole buffer as a single NAL.
        let mut buf = BytesMut::with_capacity(4 + data.len());
        buf.put_u32(data.len() as u32);
        buf.put_slice(data);
        return buf.freeze();
    }

    let total_size: usize = ranges.iter().map(|(start, end)| 4 + (end - start)).sum();
    let mut buf = BytesMut::with_capacity(total_size);
    for (start, end) in &ranges {
        buf.put_u32((end - start) as u32);
        buf.put_slice(&data[*start..*end]);
    }
    buf.freeze()
}

/// Locate Annex-B NAL boundaries as offset ranges into the buffer, without
/// copying the NAL data (test helper for the reference conversion).
#[cfg(test)]
pub(super) fn annex_b_nal_ranges(data: &Bytes) -> Vec<(usize, usize)> {
    let bytes = data.as_ref();
    let mut nals = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let Some(start) = find_start_code(bytes, i) else {
            break;
        };
        let nal_end = find_start_code(bytes, start + 3).unwrap_or(bytes.len());

        let nal_data_start = if start + 3 < bytes.len() && bytes[start + 2] == 0x01 {
            start + 3
        } else if start + 4 < bytes.len() && bytes[start + 3] == 0x01 {
            start + 4
        } else {
            start + 3
        };

        if nal_data_start < nal_end {
            nals.push((nal_data_start, nal_end));
        }

        i = nal_end;
    }

    nals
}

/// Locate the next Annex-B start code (00 00 01 or 00 00 00 01) at or
/// after `start`.
pub(super) fn find_start_code(data: &[u8], start: usize) -> Option<usize> {
    let len = data.len();
    if start + 2 >= len {
        return None;
    }
    for i in start..len - 2 {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some(i);
            }
            if i + 3 < len && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some(i);
            }
        }
    }
    None
}
