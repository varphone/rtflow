struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn read_bits(&mut self, count: usize) -> Option<u64> {
        if self.bit.checked_add(count)? > self.data.len().checked_mul(8)? {
            return None;
        }
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 1) | u64::from((self.data[self.bit / 8] >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        Some(value)
    }

    fn read_ue(&mut self) -> Option<u64> {
        let mut zeros = 0usize;
        while self.read_bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let suffix = self.read_bits(zeros)?;
        Some((1u64 << zeros) - 1 + suffix)
    }
}

/// Extract H.264 dimensions from an avcC configuration record.
pub(super) fn h264_dimensions_from_avcc(config: &[u8]) -> Option<(u16, u16)> {
    // avcC: [version, profile, compat, level, 0xFF, numSPS|0xE0, spsLen(2), sps...]
    if config.len() < 8 || config[0] != 1 {
        return None;
    }
    let num_sps = (config[5] & 0x1f) as usize;
    if num_sps == 0 {
        return None;
    }
    let sps_len = u16::from_be_bytes([config[6], config[7]]) as usize;
    let sps = config.get(8..8 + sps_len)?;
    h264_dimensions_from_sps(sps)
}

/// Parse an H.264 SPS NAL to extract coded picture dimensions.
pub(super) fn h264_dimensions_from_sps(sps: &[u8]) -> Option<(u16, u16)> {
    if sps.len() < 4 {
        return None;
    }
    // Remove emulation prevention bytes (skip NAL header byte[0])
    let mut rbsp = Vec::with_capacity(sps.len());
    let mut zeros = 0usize;
    for &byte in &sps[1..] {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let mut bits = BitReader {
        data: &rbsp,
        bit: 0,
    };
    let profile_idc = bits.read_bits(8)? as u8;
    bits.read_bits(8)?; // constraint flags + reserved
    bits.read_bits(8)?; // level_idc
    bits.read_ue()?; // seq_parameter_set_id

    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134
    ) {
        let chroma_format_idc = bits.read_ue()?;
        if chroma_format_idc == 3 {
            bits.read_bits(1)?; // separate_colour_plane_flag
        }
        bits.read_ue()?; // bit_depth_luma_minus8
        bits.read_ue()?; // bit_depth_chroma_minus8
        bits.read_bits(1)?; // qpprime_y_zero_transform_bypass_flag
        let seq_scaling_matrix_present = bits.read_bits(1)? != 0;
        if seq_scaling_matrix_present {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                let present = bits.read_bits(1)? != 0;
                if present {
                    let size = if i < 6 { 16 } else { 64 };
                    skip_scaling_list(&mut bits, size)?;
                }
            }
        }
    }

    bits.read_ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = bits.read_ue()?;
    if pic_order_cnt_type == 0 {
        bits.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        bits.read_bits(1)?; // delta_pic_order_always_zero_flag
        bits.read_ue()?; // offset_for_non_ref_pic
        bits.read_ue()?; // offset_for_top_to_bottom_field
        let num_ref = bits.read_ue()?;
        for _ in 0..num_ref {
            bits.read_ue()?;
        }
    }
    bits.read_ue()?; // max_num_ref_frames
    bits.read_bits(1)?; // gaps_in_frame_num_value_allowed_flag
    let pic_width_in_mbs = bits.read_ue()? + 1;
    let pic_height_in_map_units = bits.read_ue()? + 1;
    let frame_mbs_only = bits.read_bits(1)? != 0;
    if !frame_mbs_only {
        bits.read_bits(1)?; // mb_adaptive_frame_field_flag
    }
    bits.read_bits(1)?; // direct_8x8_inference_flag
    let frame_cropping = bits.read_bits(1)? != 0;
    let (crop_left, crop_right, crop_top, crop_bottom) = if frame_cropping {
        (
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };

    let width = pic_width_in_mbs * 16 - (crop_left + crop_right) * 2;
    let height =
        (2 - frame_mbs_only as u64) * pic_height_in_map_units * 16 - (crop_top + crop_bottom) * 2;
    Some((u16::try_from(width).ok()?, u16::try_from(height).ok()?))
}

fn skip_scaling_list(bits: &mut BitReader, size: usize) -> Option<()> {
    let mut last_scale: i64 = 8;
    let mut next_scale: i64 = 8;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = bits.read_ue()? as i64;
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
    }
    Some(())
}

pub(super) fn h265_dimensions_from_hvcc(config: &[u8]) -> Option<(u16, u16)> {
    if config.len() < 23 || config[0] != 1 {
        return None;
    }
    let mut position = 23;
    for _ in 0..config[22] {
        let nal_type = *config.get(position)? & 0x3f;
        position += 1;
        let count =
            u16::from_be_bytes([*config.get(position)?, *config.get(position + 1)?]) as usize;
        position += 2;
        for _ in 0..count {
            let length =
                u16::from_be_bytes([*config.get(position)?, *config.get(position + 1)?]) as usize;
            position += 2;
            let end = position.checked_add(length)?;
            let nal = config.get(position..end)?;
            position = end;
            if nal_type == 33 {
                return h265_dimensions_from_sps(nal);
            }
        }
    }
    None
}

pub(super) fn h265_dimensions_from_sps(sps: &[u8]) -> Option<(u16, u16)> {
    if sps.len() < 3 {
        return None;
    }
    let mut rbsp = Vec::with_capacity(sps.len());
    let mut zeros = 0usize;
    for &byte in &sps[2..] {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        rbsp.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let mut bits = BitReader {
        data: &rbsp,
        bit: 0,
    };
    bits.read_bits(4)?;
    let max_sub_layers = bits.read_bits(3)? as usize;
    bits.read_bits(1)?;
    bits.read_bits(96)?;
    let mut profile_present = [false; 7];
    let mut level_present = [false; 7];
    for index in 0..max_sub_layers {
        profile_present[index] = bits.read_bits(1)? != 0;
        level_present[index] = bits.read_bits(1)? != 0;
    }
    if max_sub_layers > 0 {
        bits.read_bits((8 - max_sub_layers) * 2)?;
    }
    for index in 0..max_sub_layers {
        if profile_present[index] {
            bits.read_bits(96)?;
        }
        if level_present[index] {
            bits.read_bits(8)?;
        }
    }
    bits.read_ue()?;
    let chroma_format = bits.read_ue()? as u8;
    if chroma_format == 3 {
        bits.read_bits(1)?;
    }
    let width = bits.read_ue()?;
    let height = bits.read_ue()?;
    let conformance_window = bits.read_bits(1)? != 0;
    let (left, right, top, bottom) = if conformance_window {
        (
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };
    let (sub_width, sub_height) = match chroma_format {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = width.saturating_sub((left + right) * sub_width);
    let height = height.saturating_sub((top + bottom) * sub_height);
    Some((u16::try_from(width).ok()?, u16::try_from(height).ok()?))
}
