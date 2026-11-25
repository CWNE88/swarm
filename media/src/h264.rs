//! H.264's parameter sets and slice headers, read and written: for a
//! decoder on the graphics chip that's given what's in them rather than
//! the stream (VA-API), and an encoder given the parameter sets to put
//! out. Only what calls use: whole frames (no fields), no slice groups,
//! scaling lists, weighted prediction, B-frames or long-term references.
//! Anything else is refused (`None`), and OpenH264 decodes it instead.
//!
//! Self-contained (std only): the stand-in VA-API driver used in tests
//! includes it too.

use std::collections::HashMap;

/// The NAL units of an Annex-B stream, each without its start code.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(stream.len(), |&next| next - 3);
        while end > start && stream[end - 1] == 0 && n + 1 < starts.len() {
            end -= 1;
        }
        units.push(&stream[start..end]);
    }
    units
}

/// A NAL unit without its emulation-prevention bytes (0 0 3 stands for
/// 0 0).
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// A NAL unit with emulation-prevention bytes put in, so no start code
/// appears inside it.
pub fn escape(unit: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(unit.len() + 8);
    let mut zeros = 0;
    for &byte in unit {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Reading bit by bit, from the most significant.
pub struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Bits<'a> {
        Bits { data, at: 0 }
    }

    /// The bits read so far.
    pub fn at(&self) -> usize {
        self.at
    }

    pub fn take(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            value = (value << 1) | ((byte >> (7 - self.at % 8)) & 1) as u32;
            self.at += 1;
        }
        Some(value)
    }

    pub fn flag(&mut self) -> Option<bool> {
        Some(self.take(1)? == 1)
    }

    /// Exp-Golomb, unsigned.
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.take(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.take(zeros)? as u64) as u32)
    }

    /// Exp-Golomb, signed.
    pub fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }

    /// Whether there's more than the closing stop bit (and its zeros).
    fn more_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|&b| b != 0) else {
            return false;
        };
        let stop = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        self.at < stop
    }
}

/// Writing bit by bit, from the most significant.
#[derive(Default)]
pub struct BitWriter {
    out: Vec<u8>,
    /// Bits used in the last byte (0 for none: a new byte next).
    used: u32,
}

impl BitWriter {
    pub fn put(&mut self, n: u32, value: u32) {
        for i in (0..n).rev() {
            if self.used == 0 {
                self.out.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.out.last_mut().unwrap() |= bit << (7 - self.used);
            self.used = (self.used + 1) % 8;
        }
    }

    pub fn flag(&mut self, value: bool) {
        self.put(1, value as u32);
    }

    pub fn ue(&mut self, value: u32) {
        let coded = value as u64 + 1;
        let bits = 64 - coded.leading_zeros();
        self.put(bits - 1, 0);
        for i in (0..bits).rev() {
            self.put(1, ((coded >> i) & 1) as u32);
        }
    }

    pub fn se(&mut self, value: i32) {
        self.ue(if value > 0 { value as u32 * 2 - 1 } else { (-value) as u32 * 2 });
    }

    /// Bits written so far.
    pub fn written(&self) -> usize {
        self.out.len() * 8 - if self.used == 0 { 0 } else { 8 - self.used as usize }
    }

    /// What's written, its last byte filled out with zeros (no stop bit).
    pub fn bytes(self) -> Vec<u8> {
        self.out
    }

    /// The stop bit, then zeros to the byte.
    pub fn finish(mut self) -> Vec<u8> {
        self.put(1, 1);
        self.out
    }
}

/// A NAL unit of `kind` (with `ref_idc`) round `rbsp`.
fn unit(ref_idc: u8, kind: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut nal = vec![(ref_idc << 5) | kind];
    nal.extend_from_slice(rbsp);
    escape(&nal)
}

const HIGH_PROFILES: [u32; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// The most macroblocks across or down a picture can be: the square root
/// of eight times the largest level's most in a frame (A.3.1), 16,880
/// pixels.
const MOST_MBS_ACROSS: u32 = 1055;

/// A sequence parameter set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sps {
    pub profile_idc: u8,
    /// constraint_set0_flag to 5, as the byte has them.
    pub constraints: u8,
    pub level_idc: u8,
    pub id: u32,
    pub chroma_format_idc: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub width_in_mbs: u32,
    /// In map units: macroblocks for frames, pairs of them for fields.
    pub height_in_map_units: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    /// Left, right, top and bottom, in crop units (2 pixels for 4:2:0).
    pub crop: [u32; 4],
    /// Written only: pictures a second, said in the VUI with no
    /// reordering and one frame of buffering (so a decoder shows each
    /// picture at once). Not read.
    pub vui_fps: Option<u32>,
}

impl Sps {
    /// From an SPS NAL unit (its header included). `None` for what this
    /// doesn't read (scaling lists), or what no stream could be.
    pub fn parse(nal: &[u8]) -> Option<Sps> {
        let rbsp = unescape(nal.get(1..)?);
        let mut bits = Bits::new(&rbsp);
        let mut sps = Sps { profile_idc: bits.take(8)? as u8, constraints: bits.take(8)? as u8, level_idc: bits.take(8)? as u8, id: bits.ue()?, chroma_format_idc: 1, ..Sps::default() };
        if HIGH_PROFILES.contains(&(sps.profile_idc as u32)) {
            sps.chroma_format_idc = bits.ue()?;
            if sps.chroma_format_idc == 3 && bits.flag()? {
                return None;
            }
            sps.bit_depth_luma_minus8 = bits.ue()?;
            sps.bit_depth_chroma_minus8 = bits.ue()?;
            bits.take(1)?;
            if bits.flag()? {
                return None;
            }
        }
        sps.log2_max_frame_num_minus4 = bits.ue()?;
        sps.pic_order_cnt_type = bits.ue()?;
        match sps.pic_order_cnt_type {
            0 => sps.log2_max_pic_order_cnt_lsb_minus4 = bits.ue()?,
            1 => {
                sps.delta_pic_order_always_zero_flag = bits.flag()?;
                sps.offset_for_non_ref_pic = bits.se()?;
                sps.offset_for_top_to_bottom_field = bits.se()?;
                for _ in 0..bits.ue()?.min(255) {
                    sps.offset_for_ref_frame.push(bits.se()?);
                }
            }
            _ => {}
        }
        sps.max_num_ref_frames = bits.ue()?;
        sps.gaps_in_frame_num_value_allowed_flag = bits.flag()?;
        sps.width_in_mbs = bits.ue()? + 1;
        sps.height_in_map_units = bits.ue()? + 1;
        sps.frame_mbs_only_flag = bits.flag()?;
        if !sps.frame_mbs_only_flag {
            sps.mb_adaptive_frame_field_flag = bits.flag()?;
        }
        sps.direct_8x8_inference_flag = bits.flag()?;
        if bits.flag()? {
            sps.crop = [bits.ue()?, bits.ue()?, bits.ue()?, bits.ue()?];
        }
        let within = sps.log2_max_frame_num_minus4 <= 12
            && sps.log2_max_pic_order_cnt_lsb_minus4 <= 12
            && sps.width_in_mbs <= MOST_MBS_ACROSS
            && sps.height_in_map_units <= MOST_MBS_ACROSS
            && sps.crop.iter().all(|&c| c <= 16 * MOST_MBS_ACROSS);
        (within && sps.id <= 31 && sps.shown().is_some()).then_some(sps)
    }

    /// Its height in macroblocks (a frame's).
    pub fn height_in_mbs(&self) -> u32 {
        self.height_in_map_units * (2 - self.frame_mbs_only_flag as u32)
    }

    pub fn max_frame_num(&self) -> u32 {
        1 << (self.log2_max_frame_num_minus4 + 4)
    }

    /// The picture to show: its size with the cropping taken off, and
    /// where that starts.
    pub fn shown(&self) -> Option<(usize, usize, usize, usize)> {
        let frames = self.frame_mbs_only_flag as u32;
        let (unit_x, unit_y) = match self.chroma_format_idc {
            0 | 3 => (1, 2 - frames),
            2 => (2, 2 - frames),
            _ => (2, 2 * (2 - frames)),
        };
        let [left, right, top, bottom] = self.crop;
        let width = (self.width_in_mbs * 16).checked_sub((left + right) * unit_x)?;
        let height = (self.height_in_mbs() * 16).checked_sub((top + bottom) * unit_y)?;
        Some((width as usize, height as usize, (left * unit_x) as usize, (top * unit_y) as usize))
    }

    /// As an SPS NAL unit, without a start code.
    pub fn write(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(8, self.profile_idc as u32);
        w.put(8, self.constraints as u32);
        w.put(8, self.level_idc as u32);
        w.ue(self.id);
        if HIGH_PROFILES.contains(&(self.profile_idc as u32)) {
            w.ue(self.chroma_format_idc);
            if self.chroma_format_idc == 3 {
                w.flag(false);
            }
            w.ue(self.bit_depth_luma_minus8);
            w.ue(self.bit_depth_chroma_minus8);
            w.flag(false);
            w.flag(false);
        }
        w.ue(self.log2_max_frame_num_minus4);
        w.ue(self.pic_order_cnt_type);
        match self.pic_order_cnt_type {
            0 => w.ue(self.log2_max_pic_order_cnt_lsb_minus4),
            1 => {
                w.flag(self.delta_pic_order_always_zero_flag);
                w.se(self.offset_for_non_ref_pic);
                w.se(self.offset_for_top_to_bottom_field);
                w.ue(self.offset_for_ref_frame.len() as u32);
                for &offset in &self.offset_for_ref_frame {
                    w.se(offset);
                }
            }
            _ => {}
        }
        w.ue(self.max_num_ref_frames);
        w.flag(self.gaps_in_frame_num_value_allowed_flag);
        w.ue(self.width_in_mbs - 1);
        w.ue(self.height_in_map_units - 1);
        w.flag(self.frame_mbs_only_flag);
        if !self.frame_mbs_only_flag {
            w.flag(self.mb_adaptive_frame_field_flag);
        }
        w.flag(self.direct_8x8_inference_flag);
        let cropped = self.crop != [0; 4];
        w.flag(cropped);
        if cropped {
            for c in self.crop {
                w.ue(c);
            }
        }
        w.flag(self.vui_fps.is_some());
        if let Some(fps) = self.vui_fps {
            w.flag(true);
            w.put(8, 1);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(true);
            w.put(32, 1);
            w.put(32, fps * 2);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(true);
            w.flag(true);
            w.ue(2);
            w.ue(1);
            w.ue(15);
            w.ue(15);
            w.ue(0);
            w.ue(self.max_num_ref_frames.max(1));
        }
        unit(3, 7, &w.finish())
    }
}

/// A picture parameter set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub transform_8x8_mode_flag: bool,
    pub second_chroma_qp_index_offset: i32,
}

impl Pps {
    /// From a PPS NAL unit (its header included). `None` for what this
    /// doesn't read (slice groups, scaling lists).
    pub fn parse(nal: &[u8]) -> Option<Pps> {
        let rbsp = unescape(nal.get(1..)?);
        let mut bits = Bits::new(&rbsp);
        let mut pps = Pps { id: bits.ue()?, sps_id: bits.ue()?, entropy_coding_mode_flag: bits.flag()?, bottom_field_pic_order_in_frame_present_flag: bits.flag()?, ..Pps::default() };
        if bits.ue()? != 0 {
            return None;
        }
        pps.num_ref_idx_l0_default_active_minus1 = bits.ue()?;
        pps.num_ref_idx_l1_default_active_minus1 = bits.ue()?;
        pps.weighted_pred_flag = bits.flag()?;
        pps.weighted_bipred_idc = bits.take(2)?;
        pps.pic_init_qp_minus26 = bits.se()?;
        pps.pic_init_qs_minus26 = bits.se()?;
        pps.chroma_qp_index_offset = bits.se()?;
        pps.deblocking_filter_control_present_flag = bits.flag()?;
        pps.constrained_intra_pred_flag = bits.flag()?;
        pps.redundant_pic_cnt_present_flag = bits.flag()?;
        pps.second_chroma_qp_index_offset = pps.chroma_qp_index_offset;
        if bits.more_data() {
            pps.transform_8x8_mode_flag = bits.flag()?;
            if bits.flag()? {
                return None;
            }
            pps.second_chroma_qp_index_offset = bits.se()?;
        }
        (pps.id <= 255 && pps.sps_id <= 31).then_some(pps)
    }

    /// As a PPS NAL unit, without a start code.
    pub fn write(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(self.id);
        w.ue(self.sps_id);
        w.flag(self.entropy_coding_mode_flag);
        w.flag(self.bottom_field_pic_order_in_frame_present_flag);
        w.ue(0);
        w.ue(self.num_ref_idx_l0_default_active_minus1);
        w.ue(self.num_ref_idx_l1_default_active_minus1);
        w.flag(self.weighted_pred_flag);
        w.put(2, self.weighted_bipred_idc);
        w.se(self.pic_init_qp_minus26);
        w.se(self.pic_init_qs_minus26);
        w.se(self.chroma_qp_index_offset);
        w.flag(self.deblocking_filter_control_present_flag);
        w.flag(self.constrained_intra_pred_flag);
        w.flag(self.redundant_pic_cnt_present_flag);
        if self.transform_8x8_mode_flag || self.second_chroma_qp_index_offset != self.chroma_qp_index_offset {
            w.flag(self.transform_8x8_mode_flag);
            w.flag(false);
            w.se(self.second_chroma_qp_index_offset);
        }
        unit(3, 8, &w.finish())
    }
}

pub const SLICE_P: u32 = 0;
pub const SLICE_I: u32 = 2;

/// A slice's header, as far as a decoder needs it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SliceHeader {
    pub nal_ref_idc: u32,
    pub idr: bool,
    pub first_mb_in_slice: u32,
    /// 0 for P, 2 for I (B, SP and SI are refused).
    pub slice_type: u32,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub num_ref_idx_l0_active_minus1: u32,
    /// The reordering of list 0: (modification_of_pic_nums_idc, its
    /// abs_diff_pic_num_minus1), short-term only.
    pub modifications: Vec<(u32, u32)>,
    pub adaptive_marking: bool,
    /// Memory management (memory_management_control_operation, its
    /// difference_of_pic_nums_minus1): 1 and 5 only.
    pub mmco: Vec<(u32, u32)>,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    /// Bits from the NAL unit's start (its header byte included) to the
    /// slice's data, as read without emulation-prevention bytes.
    pub header_bits: usize,
}

impl SliceHeader {
    /// From a slice's NAL unit (type 1 or 5, its header included), with
    /// the parameter sets so far. `None` for what this doesn't decode.
    pub fn parse(nal: &[u8], spss: &HashMap<u32, Sps>, ppss: &HashMap<u32, Pps>) -> Option<SliceHeader> {
        let header = *nal.first()?;
        let kind = header & 0x1f;
        if kind != 1 && kind != 5 {
            return None;
        }
        let rbsp = unescape(nal);
        let mut bits = Bits::new(&rbsp);
        bits.take(8)?;
        let mut slice = SliceHeader { nal_ref_idc: (header >> 5) as u32 & 3, idr: kind == 5, first_mb_in_slice: bits.ue()?, ..SliceHeader::default() };
        slice.slice_type = bits.ue()? % 5;
        if slice.slice_type != SLICE_P && slice.slice_type != SLICE_I {
            return None;
        }
        slice.pps_id = bits.ue()?;
        let pps = ppss.get(&slice.pps_id)?;
        let sps = spss.get(&pps.sps_id)?;
        if !sps.frame_mbs_only_flag || pps.weighted_pred_flag || sps.pic_order_cnt_type == 1 {
            return None;
        }
        slice.frame_num = bits.take(sps.log2_max_frame_num_minus4 + 4)?;
        if slice.idr {
            slice.idr_pic_id = bits.ue()?;
        }
        if sps.pic_order_cnt_type == 0 {
            slice.pic_order_cnt_lsb = bits.take(sps.log2_max_pic_order_cnt_lsb_minus4 + 4)?;
            if pps.bottom_field_pic_order_in_frame_present_flag {
                slice.delta_pic_order_cnt_bottom = bits.se()?;
            }
        }
        if pps.redundant_pic_cnt_present_flag && bits.ue()? != 0 {
            return None;
        }
        slice.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
        if slice.slice_type == SLICE_P {
            if bits.flag()? {
                slice.num_ref_idx_l0_active_minus1 = bits.ue()?;
            }
            if slice.num_ref_idx_l0_active_minus1 > 31 {
                return None;
            }
            if bits.flag()? {
                loop {
                    match bits.ue()? {
                        3 => break,
                        idc @ (0 | 1) => slice.modifications.push((idc, bits.ue()?)),
                        _ => return None,
                    }
                }
            }
        }
        if slice.nal_ref_idc != 0 {
            if slice.idr {
                bits.take(1)?;
                if bits.flag()? {
                    return None;
                }
            } else {
                slice.adaptive_marking = bits.flag()?;
                if slice.adaptive_marking {
                    loop {
                        match bits.ue()? {
                            0 => break,
                            1 => slice.mmco.push((1, bits.ue()?)),
                            5 => slice.mmco.push((5, 0)),
                            _ => return None,
                        }
                    }
                }
            }
        }
        if pps.entropy_coding_mode_flag && slice.slice_type != SLICE_I {
            slice.cabac_init_idc = bits.ue()?;
        }
        slice.slice_qp_delta = bits.se()?;
        if pps.deblocking_filter_control_present_flag {
            slice.disable_deblocking_filter_idc = bits.ue()?;
            if slice.disable_deblocking_filter_idc != 1 {
                slice.slice_alpha_c0_offset_div2 = bits.se()?;
                slice.slice_beta_offset_div2 = bits.se()?;
            }
        }
        slice.header_bits = bits.at();
        Some(slice)
    }

    /// As the start of a slice's NAL unit (without a start code), for a
    /// packed slice header: escaped, and how many bits of it are the
    /// header's (it ends mid-byte, where the slice's data would start).
    pub fn write(&self, sps: &Sps, pps: &Pps) -> (Vec<u8>, usize) {
        let mut w = BitWriter::default();
        w.put(8, (self.nal_ref_idc << 5) | if self.idr { 5 } else { 1 });
        w.ue(self.first_mb_in_slice);
        w.ue(self.slice_type);
        w.ue(self.pps_id);
        w.put(sps.log2_max_frame_num_minus4 + 4, self.frame_num);
        if self.idr {
            w.ue(self.idr_pic_id);
        }
        if sps.pic_order_cnt_type == 0 {
            w.put(sps.log2_max_pic_order_cnt_lsb_minus4 + 4, self.pic_order_cnt_lsb);
            if pps.bottom_field_pic_order_in_frame_present_flag {
                w.se(self.delta_pic_order_cnt_bottom);
            }
        }
        if pps.redundant_pic_cnt_present_flag {
            w.ue(0);
        }
        if self.slice_type == SLICE_P {
            let overridden = self.num_ref_idx_l0_active_minus1 != pps.num_ref_idx_l0_default_active_minus1;
            w.flag(overridden);
            if overridden {
                w.ue(self.num_ref_idx_l0_active_minus1);
            }
            w.flag(!self.modifications.is_empty());
            if !self.modifications.is_empty() {
                for &(idc, diff) in &self.modifications {
                    w.ue(idc);
                    w.ue(diff);
                }
                w.ue(3);
            }
        }
        if self.nal_ref_idc != 0 {
            if self.idr {
                w.put(2, 0);
            } else {
                w.flag(self.adaptive_marking);
                if self.adaptive_marking {
                    for &(operation, diff) in &self.mmco {
                        w.ue(operation);
                        if operation == 1 {
                            w.ue(diff);
                        }
                    }
                    w.ue(0);
                }
            }
        }
        if pps.entropy_coding_mode_flag && self.slice_type != SLICE_I {
            w.ue(self.cabac_init_idc);
        }
        w.se(self.slice_qp_delta);
        if pps.deblocking_filter_control_present_flag {
            w.ue(self.disable_deblocking_filter_idc);
            if self.disable_deblocking_filter_idc != 1 {
                w.se(self.slice_alpha_c0_offset_div2);
                w.se(self.slice_beta_offset_div2);
            }
        }
        let bits = w.written();
        let raw = w.bytes();
        let escaped = escape(&raw);
        let bits = bits + 8 * (escaped.len() - raw.len());
        (escaped, bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_goes_both_ways() {
        let mut w = BitWriter::default();
        let values = [0u32, 1, 2, 3, 7, 8, 255, 256, 65_535, 1 << 20];
        for &v in &values {
            w.ue(v);
        }
        for s in [0, 1, -1, 2, -2, 100, -100] {
            w.se(s);
        }
        w.put(5, 0b10110);
        let out = w.finish();
        let mut bits = Bits::new(&out);
        for &v in &values {
            assert_eq!(bits.ue(), Some(v));
        }
        for s in [0, 1, -1, 2, -2, 100, -100] {
            assert_eq!(bits.se(), Some(s));
        }
        assert_eq!(bits.take(5), Some(0b10110));
        assert!(!bits.more_data());
    }

    #[test]
    fn escaping_goes_both_ways() {
        let raw = [0x67, 0, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0];
        let escaped = escape(&raw);
        assert_eq!(escaped, vec![0x67, 0, 0, 3, 0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 3, 3, 0, 0, 4, 0, 0]);
        assert_eq!(unescape(&escaped), raw);
    }

    #[test]
    fn parameter_sets_go_both_ways() {
        let sps = Sps {
            profile_idc: 66,
            constraints: 0xc0,
            level_idc: 31,
            chroma_format_idc: 1,
            log2_max_frame_num_minus4: 4,
            log2_max_pic_order_cnt_lsb_minus4: 4,
            max_num_ref_frames: 1,
            width_in_mbs: 60,
            height_in_map_units: 34,
            frame_mbs_only_flag: true,
            direct_8x8_inference_flag: true,
            crop: [0, 0, 0, 2],
            vui_fps: Some(30),
            ..Sps::default()
        };
        let nal = sps.write();
        assert_eq!(nal[0], 0x67);
        assert_eq!(Sps::parse(&nal), Some(Sps { vui_fps: None, ..sps.clone() }));
        assert_eq!(sps.shown(), Some((960, 540, 0, 0)));
        let pps = Pps { pic_init_qp_minus26: -3, deblocking_filter_control_present_flag: true, chroma_qp_index_offset: 2, second_chroma_qp_index_offset: 2, ..Pps::default() };
        assert_eq!(Pps::parse(&pps.write()), Some(pps.clone()));
        let pps = Pps { transform_8x8_mode_flag: true, second_chroma_qp_index_offset: -1, entropy_coding_mode_flag: true, ..pps };
        assert_eq!(Pps::parse(&pps.write()), Some(pps));
        for wrong in [
            Sps { log2_max_frame_num_minus4: 13, ..sps.clone() },
            Sps { log2_max_pic_order_cnt_lsb_minus4: 13, ..sps.clone() },
            Sps { width_in_mbs: 1 << 30, ..sps.clone() },
            Sps { height_in_map_units: 1 << 30, ..sps.clone() },
            Sps { crop: [u32::MAX - 1, u32::MAX - 1, 0, 0], ..sps.clone() },
        ] {
            assert_eq!(Sps::parse(&wrong.write()), None, "{wrong:?}");
        }
    }

    /// Slice headers written read back the same, ending on the bit said.
    #[test]
    fn slice_headers_go_both_ways() {
        let sps = Sps { log2_max_frame_num_minus4: 4, pic_order_cnt_type: 2, frame_mbs_only_flag: true, ..Sps::default() };
        let mut poc0 = Sps { pic_order_cnt_type: 0, log2_max_pic_order_cnt_lsb_minus4: 12, ..sps.clone() };
        poc0.id = 1;
        let pps = Pps::default();
        let deblocking = Pps { id: 1, sps_id: 1, deblocking_filter_control_present_flag: true, ..Pps::default() };
        let spss = HashMap::from([(0, sps.clone()), (1, poc0.clone())]);
        let ppss = HashMap::from([(0, pps.clone()), (1, deblocking.clone())]);
        let headers = [
            (SliceHeader { nal_ref_idc: 3, idr: true, slice_type: SLICE_I, idr_pic_id: 7, ..SliceHeader::default() }, &sps, &pps),
            (SliceHeader { nal_ref_idc: 2, slice_type: SLICE_P, frame_num: 255, ..SliceHeader::default() }, &sps, &pps),
            (SliceHeader { nal_ref_idc: 0, slice_type: SLICE_P, pps_id: 1, ..SliceHeader::default() }, &poc0, &deblocking),
            (
                SliceHeader {
                    nal_ref_idc: 1,
                    slice_type: SLICE_P,
                    pps_id: 1,
                    frame_num: 3,
                    pic_order_cnt_lsb: 6,
                    num_ref_idx_l0_active_minus1: 1,
                    modifications: vec![(0, 1), (1, 0)],
                    adaptive_marking: true,
                    mmco: vec![(1, 2), (5, 0)],
                    slice_qp_delta: -4,
                    disable_deblocking_filter_idc: 2,
                    slice_alpha_c0_offset_div2: -1,
                    slice_beta_offset_div2: 3,
                    ..SliceHeader::default()
                },
                &poc0,
                &deblocking,
            ),
            (SliceHeader { nal_ref_idc: 3, idr: true, slice_type: SLICE_I, pps_id: 1, disable_deblocking_filter_idc: 1, ..SliceHeader::default() }, &poc0, &deblocking),
        ];
        let mut escaped = 0;
        for (header, sps, pps) in headers {
            let (nal, bits) = header.write(sps, pps);
            assert!(bits > (nal.len() - 1) * 8 && bits <= nal.len() * 8, "{bits} bits in {} bytes", nal.len());
            let parsed = SliceHeader::parse(&[&nal[..], &[0xff, 0xff]].concat(), &spss, &ppss).expect("reads");
            let escapes = nal.len() - unescape(&nal).len();
            escaped += escapes;
            assert_eq!(parsed.header_bits + 8 * escapes, bits);
            assert_eq!(parsed, SliceHeader { header_bits: parsed.header_bits, ..header });
        }
        assert!(escaped > 0);
    }
}
