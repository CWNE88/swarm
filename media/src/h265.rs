//! H.265's parameter sets and slice headers, read and written: for a
//! decoder on the graphics chip that's given what's in them as well as
//! the stream (VA-API), and an encoder given the parameter sets and slice
//! headers to put out. Only what calls use: 8-bit 4:2:0 frames and
//! short-term references; no scaling lists, weighted prediction,
//! long-term references, dependent slice segments or extensions. Anything
//! else is refused (`None`), and that call goes back to H.264.
//!
//! Self-contained (std and `h264`'s bit reading and writing): the
//! stand-in VA-API driver used in tests includes it too.

use crate::h264::{escape, unescape, BitWriter, Bits};
use std::collections::HashMap;

pub const TRAIL_N: u8 = 0;
pub const TRAIL_R: u8 = 1;
pub const RASL_R: u8 = 9;
pub const BLA_W_LP: u8 = 16;
pub const IDR_W_RADL: u8 = 19;
pub const IDR_N_LP: u8 = 20;
pub const CRA: u8 = 21;
pub const VPS: u8 = 32;
pub const SPS: u8 = 33;
pub const PPS: u8 = 34;

pub const SLICE_B: u32 = 0;
pub const SLICE_P: u32 = 1;
pub const SLICE_I: u32 = 2;

/// A NAL unit's type (its header included).
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3f)
}

/// Whether a unit of `kind` is a picture's slice (a VCL unit swarm reads).
pub fn is_slice(kind: u8) -> bool {
    kind <= RASL_R || (BLA_W_LP..=CRA).contains(&kind)
}

pub fn is_irap(kind: u8) -> bool {
    (BLA_W_LP..=23).contains(&kind)
}

pub fn is_idr(kind: u8) -> bool {
    kind == IDR_W_RADL || kind == IDR_N_LP
}

/// An Annex-B stream's VPS, SPS and PPS, each with its start code, if
/// it has them.
pub fn parameter_sets(stream: &[u8]) -> Option<Vec<u8>> {
    let mut sets = Vec::new();
    for nal in crate::h264::nal_units(stream) {
        if matches!(nal_type(nal), Some(VPS | SPS | PPS)) {
            sets.extend_from_slice(&[0, 0, 0, 1]);
            sets.extend_from_slice(nal);
        }
    }
    (!sets.is_empty()).then_some(sets)
}

/// A NAL unit of `kind` round `rbsp` (layer 0, temporal id 0).
fn unit(kind: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut nal = vec![kind << 1, 1];
    nal.extend_from_slice(rbsp);
    escape(&nal)
}

/// The bits a value below `n` takes (Ceil(Log2(n))).
fn bits_for(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

/// The general profile, tier and level (sub-layers' are skipped).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ptl {
    pub profile_idc: u8,
    pub tier: bool,
    pub compatibility: u32,
    pub level_idc: u8,
}

impl Ptl {
    /// Main, as an encoder of 8-bit progressive frames says it.
    pub fn main(level_idc: u8) -> Ptl {
        Ptl { profile_idc: 1, tier: false, compatibility: (1 << 30) | (1 << 29), level_idc }
    }

    fn read(bits: &mut Bits, max_sub_layers_minus1: u32) -> Option<Ptl> {
        if bits.take(2)? != 0 {
            return None;
        }
        let tier = bits.flag()?;
        let profile_idc = bits.take(5)? as u8;
        let compatibility = bits.take(32)?;
        bits.take(4)?;
        bits.take(32)?;
        bits.take(12)?;
        let level_idc = bits.take(8)? as u8;
        let mut present = Vec::new();
        for _ in 0..max_sub_layers_minus1 {
            present.push((bits.flag()?, bits.flag()?));
        }
        if max_sub_layers_minus1 > 0 {
            for _ in max_sub_layers_minus1..8 {
                bits.take(2)?;
            }
        }
        for (profile, level) in present {
            if profile {
                bits.take(32)?;
                bits.take(32)?;
                bits.take(24)?;
            }
            if level {
                bits.take(8)?;
            }
        }
        Some(Ptl { profile_idc, tier, compatibility, level_idc })
    }

    fn write(&self, w: &mut BitWriter) {
        w.put(2, 0);
        w.flag(self.tier);
        w.put(5, self.profile_idc as u32);
        w.put(32, self.compatibility);
        w.put(4, 0b1011);
        w.put(32, 0);
        w.put(12, 0);
        w.put(8, self.level_idc as u32);
    }
}

/// A short-term reference picture set: the pictures before the current
/// one and after it, as differences of picture order (nearest first),
/// and whether the current picture refers to each.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rps {
    pub negative: Vec<(i32, bool)>,
    pub positive: Vec<(i32, bool)>,
}

/// A difference of picture order in a reference picture set: 1 to
/// 2^15 (7.4.8), so no chain of them a stream can send overflows.
fn step(bits: &mut Bits) -> Option<i32> {
    let minus1 = bits.ue()?;
    (minus1 < 1 << 15).then(|| minus1 as i32 + 1)
}

impl Rps {
    /// st_ref_pic_set(`index`) (7.3.7, 7.4.8), with `sets` those before:
    /// one of the SPS's, or (`in_slice`) a slice header's own.
    fn read(bits: &mut Bits, index: usize, sets: &[Rps], in_slice: bool) -> Option<Rps> {
        let predicted = index != 0 && bits.flag()?;
        if !predicted {
            let (negatives, positives) = (bits.ue()?, bits.ue()?);
            if negatives.saturating_add(positives) > 16 {
                return None;
            }
            let mut rps = Rps::default();
            let mut poc = 0i32;
            for _ in 0..negatives {
                poc -= step(bits)?;
                rps.negative.push((poc, bits.flag()?));
            }
            poc = 0;
            for _ in 0..positives {
                poc += step(bits)?;
                rps.positive.push((poc, bits.flag()?));
            }
            return Some(rps);
        }
        let delta_idx = if in_slice { bits.ue()? as usize + 1 } else { 1 };
        let reference = sets.get(index.checked_sub(delta_idx)?)?;
        let sign = bits.flag()?;
        let magnitude = step(bits)?;
        let delta = if sign { -magnitude } else { magnitude };
        let count = reference.negative.len() + reference.positive.len();
        let mut used = Vec::new();
        let mut use_delta = Vec::new();
        for _ in 0..=count {
            let u = bits.flag()?;
            used.push(u);
            use_delta.push(u || bits.flag()?);
        }
        let n_neg = reference.negative.len();
        let mut rps = Rps::default();
        for (j, &(d, _)) in reference.positive.iter().enumerate().rev() {
            let poc = d + delta;
            if poc < 0 && use_delta[n_neg + j] {
                rps.negative.push((poc, used[n_neg + j]));
            }
        }
        if delta < 0 && use_delta[count] {
            rps.negative.push((delta, used[count]));
        }
        for (j, &(d, _)) in reference.negative.iter().enumerate() {
            let poc = d + delta;
            if poc < 0 && use_delta[j] {
                rps.negative.push((poc, used[j]));
            }
        }
        for (j, &(d, _)) in reference.negative.iter().enumerate().rev() {
            let poc = d + delta;
            if poc > 0 && use_delta[j] {
                rps.positive.push((poc, used[j]));
            }
        }
        if delta > 0 && use_delta[count] {
            rps.positive.push((delta, used[count]));
        }
        for (j, &(d, _)) in reference.positive.iter().enumerate() {
            let poc = d + delta;
            if poc > 0 && use_delta[n_neg + j] {
                rps.positive.push((poc, used[n_neg + j]));
            }
        }
        if rps.negative.len() + rps.positive.len() > 16 {
            return None;
        }
        Some(rps)
    }

    /// Written out in full (not predicted from another).
    fn write(&self, w: &mut BitWriter, index: usize) {
        if index != 0 {
            w.flag(false);
        }
        w.ue(self.negative.len() as u32);
        w.ue(self.positive.len() as u32);
        let mut poc = 0;
        for &(d, used) in &self.negative {
            w.ue((poc - d - 1) as u32);
            w.flag(used);
            poc = d;
        }
        poc = 0;
        for &(d, used) in &self.positive {
            w.ue((d - poc - 1) as u32);
            w.flag(used);
            poc = d;
        }
    }

    /// The pictures the current one refers to.
    pub fn used(&self) -> usize {
        self.negative.iter().chain(&self.positive).filter(|r| r.1).count()
    }
}

/// PCM's sizes, when it's on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pcm {
    pub bit_depth_luma_minus1: u32,
    pub bit_depth_chroma_minus1: u32,
    pub log2_min_minus3: u32,
    pub log2_diff_max_min: u32,
    pub loop_filter_disabled: bool,
}

/// A sequence parameter set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sps {
    pub vps_id: u32,
    pub id: u32,
    pub ptl: Ptl,
    pub chroma_format_idc: u32,
    pub width: u32,
    pub height: u32,
    /// Left, right, top and bottom, in chroma units (2 pixels for 4:2:0).
    pub conformance: [u32; 4],
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub log2_max_poc_lsb_minus4: u32,
    /// The highest sub-layer's.
    pub max_dec_pic_buffering_minus1: u32,
    pub max_num_reorder_pics: u32,
    pub max_latency_increase_plus1: u32,
    pub log2_min_cb_minus3: u32,
    pub log2_diff_max_min_cb: u32,
    pub log2_min_tb_minus2: u32,
    pub log2_diff_max_min_tb: u32,
    pub max_transform_hierarchy_depth_inter: u32,
    pub max_transform_hierarchy_depth_intra: u32,
    pub amp: bool,
    pub sao: bool,
    pub pcm: Option<Pcm>,
    pub short_term_rps: Vec<Rps>,
    /// Long-term references may be used (none are taken).
    pub long_term_refs_present: bool,
    pub temporal_mvp: bool,
    pub strong_intra_smoothing: bool,
    /// Written only: pictures a second, said in the VUI with the
    /// bitstream's restrictions. Not read.
    pub vui_fps: Option<u32>,
}

/// The most pixels across or down a picture can be: the square root of
/// eight times the largest level's most in a picture (A.4.1).
const MOST_SIDE: u32 = 16_888;

/// The most pixels in a picture: as many as H.264's largest (see
/// `h264::MOST_MBS`), 8K or so. Each side may be longer, but not both.
const MOST_PIXELS: u64 = crate::h264::MOST_MBS as u64 * 256;

impl Sps {
    /// From an SPS NAL unit (its header included). `None` for what this
    /// doesn't read (scaling lists, long-term pictures in the SPS, more
    /// than 8 bits), or what no stream could be.
    pub fn parse(nal: &[u8]) -> Option<Sps> {
        let rbsp = unescape(nal.get(2..)?);
        let mut bits = Bits::new(&rbsp);
        let vps_id = bits.take(4)?;
        let max_sub_layers_minus1 = bits.take(3)?;
        bits.take(1)?;
        let ptl = Ptl::read(&mut bits, max_sub_layers_minus1)?;
        let mut sps = Sps { vps_id, ptl, id: bits.ue()?, chroma_format_idc: bits.ue()?, ..Sps::default() };
        if sps.chroma_format_idc == 3 && bits.flag()? {
            return None;
        }
        sps.width = bits.ue()?;
        sps.height = bits.ue()?;
        if bits.flag()? {
            sps.conformance = [bits.ue()?, bits.ue()?, bits.ue()?, bits.ue()?];
        }
        sps.bit_depth_luma_minus8 = bits.ue()?;
        sps.bit_depth_chroma_minus8 = bits.ue()?;
        sps.log2_max_poc_lsb_minus4 = bits.ue()?;
        if sps.log2_max_poc_lsb_minus4 > 12 {
            return None;
        }
        let ordering_for_each = bits.flag()?;
        for _ in if ordering_for_each { 0 } else { max_sub_layers_minus1 }..=max_sub_layers_minus1 {
            sps.max_dec_pic_buffering_minus1 = bits.ue()?;
            sps.max_num_reorder_pics = bits.ue()?;
            sps.max_latency_increase_plus1 = bits.ue()?;
        }
        sps.log2_min_cb_minus3 = bits.ue()?;
        sps.log2_diff_max_min_cb = bits.ue()?;
        sps.log2_min_tb_minus2 = bits.ue()?;
        sps.log2_diff_max_min_tb = bits.ue()?;
        sps.max_transform_hierarchy_depth_inter = bits.ue()?;
        sps.max_transform_hierarchy_depth_intra = bits.ue()?;
        if sps.log2_min_cb_minus3.saturating_add(sps.log2_diff_max_min_cb) > 3 || sps.max_dec_pic_buffering_minus1 > 15 {
            return None;
        }
        if bits.flag()? {
            return None;
        }
        sps.amp = bits.flag()?;
        sps.sao = bits.flag()?;
        if bits.flag()? {
            sps.pcm = Some(Pcm { bit_depth_luma_minus1: bits.take(4)?, bit_depth_chroma_minus1: bits.take(4)?, log2_min_minus3: bits.ue()?, log2_diff_max_min: bits.ue()?, loop_filter_disabled: bits.flag()? });
        }
        let sets = bits.ue()? as usize;
        if sets > 64 {
            return None;
        }
        for index in 0..sets {
            let rps = Rps::read(&mut bits, index, &sps.short_term_rps, false)?;
            sps.short_term_rps.push(rps);
        }
        sps.long_term_refs_present = bits.flag()?;
        if sps.long_term_refs_present && bits.ue()? != 0 {
            return None;
        }
        sps.temporal_mvp = bits.flag()?;
        sps.strong_intra_smoothing = bits.flag()?;
        let within = sps.width <= MOST_SIDE
            && sps.height <= MOST_SIDE
            && u64::from(sps.width) * u64::from(sps.height) <= MOST_PIXELS
            && sps.conformance.iter().all(|&c| c <= MOST_SIDE);
        // Block sizes within the standard's (7.4.3.2.1): handed to the
        // graphics driver in a byte each.
        let blocks = sps.log2_min_tb_minus2 <= 3
            && sps.log2_diff_max_min_tb <= 3
            && sps.max_transform_hierarchy_depth_inter <= 4
            && sps.max_transform_hierarchy_depth_intra <= 4
            && sps.pcm.as_ref().is_none_or(|pcm| pcm.log2_min_minus3 <= 2 && pcm.log2_diff_max_min <= 2);
        (within && blocks && sps.id <= 15 && sps.shown().is_some()).then_some(sps)
    }

    /// The coding tree blocks' size, as a power of 2.
    pub fn log2_ctb(&self) -> u32 {
        self.log2_min_cb_minus3 + 3 + self.log2_diff_max_min_cb
    }

    /// Its size in coding tree blocks.
    pub fn ctbs(&self) -> (u32, u32) {
        let ctb = 1 << self.log2_ctb();
        (self.width.div_ceil(ctb), self.height.div_ceil(ctb))
    }

    pub fn max_poc_lsb(&self) -> u32 {
        1 << (self.log2_max_poc_lsb_minus4 + 4)
    }

    /// The picture to show: its size with the conformance window taken
    /// off, and where that starts.
    pub fn shown(&self) -> Option<(usize, usize, usize, usize)> {
        let (unit_x, unit_y) = match self.chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let [left, right, top, bottom] = self.conformance;
        let width = self.width.checked_sub((left + right) * unit_x)?;
        let height = self.height.checked_sub((top + bottom) * unit_y)?;
        Some((width as usize, height as usize, (left * unit_x) as usize, (top * unit_y) as usize))
    }

    /// As an SPS NAL unit, without a start code.
    pub fn write(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(4, self.vps_id);
        w.put(3, 0);
        w.flag(true);
        self.ptl.write(&mut w);
        w.ue(self.id);
        w.ue(self.chroma_format_idc);
        if self.chroma_format_idc == 3 {
            w.flag(false);
        }
        w.ue(self.width);
        w.ue(self.height);
        let windowed = self.conformance != [0; 4];
        w.flag(windowed);
        if windowed {
            for c in self.conformance {
                w.ue(c);
            }
        }
        w.ue(self.bit_depth_luma_minus8);
        w.ue(self.bit_depth_chroma_minus8);
        w.ue(self.log2_max_poc_lsb_minus4);
        w.flag(false);
        w.ue(self.max_dec_pic_buffering_minus1);
        w.ue(self.max_num_reorder_pics);
        w.ue(self.max_latency_increase_plus1);
        w.ue(self.log2_min_cb_minus3);
        w.ue(self.log2_diff_max_min_cb);
        w.ue(self.log2_min_tb_minus2);
        w.ue(self.log2_diff_max_min_tb);
        w.ue(self.max_transform_hierarchy_depth_inter);
        w.ue(self.max_transform_hierarchy_depth_intra);
        w.flag(false);
        w.flag(self.amp);
        w.flag(self.sao);
        w.flag(self.pcm.is_some());
        if let Some(pcm) = &self.pcm {
            w.put(4, pcm.bit_depth_luma_minus1);
            w.put(4, pcm.bit_depth_chroma_minus1);
            w.ue(pcm.log2_min_minus3);
            w.ue(pcm.log2_diff_max_min);
            w.flag(pcm.loop_filter_disabled);
        }
        w.ue(self.short_term_rps.len() as u32);
        for (index, rps) in self.short_term_rps.iter().enumerate() {
            rps.write(&mut w, index);
        }
        w.flag(self.long_term_refs_present);
        if self.long_term_refs_present {
            w.ue(0);
        }
        w.flag(self.temporal_mvp);
        w.flag(self.strong_intra_smoothing);
        w.flag(self.vui_fps.is_some());
        if let Some(fps) = self.vui_fps {
            w.flag(true);
            w.put(8, 1);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(true);
            w.put(32, 1);
            w.put(32, fps);
            w.flag(true);
            w.ue(0);
            w.flag(false);
            w.flag(true);
            w.flag(false);
            w.flag(true);
            w.flag(true);
            w.ue(0);
            w.ue(0);
            w.ue(0);
            w.ue(15);
            w.ue(15);
        }
        w.flag(false);
        unit(SPS, &w.finish())
    }
}

/// A video parameter set as an encoder of one layer writes it.
pub fn write_vps(sps: &Sps, fps: u32) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put(4, sps.vps_id);
    w.flag(true);
    w.flag(true);
    w.put(6, 0);
    w.put(3, 0);
    w.flag(true);
    w.put(16, 0xffff);
    sps.ptl.write(&mut w);
    w.flag(false);
    w.ue(sps.max_dec_pic_buffering_minus1);
    w.ue(sps.max_num_reorder_pics);
    w.ue(sps.max_latency_increase_plus1);
    w.put(6, 0);
    w.ue(0);
    w.flag(true);
    w.put(32, 1);
    w.put(32, fps);
    w.flag(true);
    w.ue(0);
    w.ue(0);
    w.flag(false);
    unit(VPS, &w.finish())
}

/// Tiles, when a PPS has them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tiles {
    pub columns_minus1: u32,
    pub rows_minus1: u32,
    /// Empty for uniform spacing.
    pub column_widths_minus1: Vec<u32>,
    pub row_heights_minus1: Vec<u32>,
    pub loop_filter_across_tiles: bool,
}

impl Tiles {
    /// Each column's width and each row's height in coding tree blocks,
    /// less one (6.5.1), for a picture `sps` describes.
    /// Whether these tiles fit a picture `sps` describes: no more columns
    /// or rows than it has blocks (6.5.1), and the sizes given leaving
    /// room for the last. Past that, `sizes_minus1` means nothing.
    pub fn fit(&self, sps: &Sps) -> bool {
        let (ctbs_x, ctbs_y) = sps.ctbs();
        let room = |given: &[u32], count: u32, total: u32| {
            let used: u64 = given.iter().map(|s| u64::from(*s) + 1).sum();
            count <= total && (given.is_empty() || used < u64::from(total))
        };
        room(&self.column_widths_minus1, self.columns_minus1 + 1, ctbs_x) && room(&self.row_heights_minus1, self.rows_minus1 + 1, ctbs_y)
    }

    pub fn sizes_minus1(&self, sps: &Sps) -> (Vec<u32>, Vec<u32>) {
        let (ctbs_x, ctbs_y) = sps.ctbs();
        let spread = |given: &[u32], count: u32, total: u32| -> Vec<u32> {
            if given.is_empty() {
                (0..count).map(|i| (((i + 1) * total) / count - (i * total) / count).saturating_sub(1)).collect()
            } else {
                let mut sizes = given.to_vec();
                let used = given.iter().fold(0u32, |used, s| used.saturating_add(s.saturating_add(1)));
                sizes.push(total.saturating_sub(used).saturating_sub(1));
                sizes
            }
        };
        (spread(&self.column_widths_minus1, self.columns_minus1 + 1, ctbs_x), spread(&self.row_heights_minus1, self.rows_minus1 + 1, ctbs_y))
    }
}

/// A picture parameter set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub init_qp_minus26: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass: bool,
    pub tiles: Option<Tiles>,
    pub entropy_coding_sync: bool,
    pub loop_filter_across_slices: bool,
    pub deblocking_control_present: bool,
    pub deblocking_override_enabled: bool,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level_minus2: u32,
    pub slice_header_extension_present: bool,
}

impl Pps {
    /// From a PPS NAL unit (its header included). `None` for what this
    /// doesn't read (scaling lists).
    pub fn parse(nal: &[u8]) -> Option<Pps> {
        let rbsp = unescape(nal.get(2..)?);
        let mut bits = Bits::new(&rbsp);
        let mut pps = Pps {
            id: bits.ue()?,
            sps_id: bits.ue()?,
            dependent_slice_segments_enabled: bits.flag()?,
            output_flag_present: bits.flag()?,
            num_extra_slice_header_bits: bits.take(3)?,
            sign_data_hiding: bits.flag()?,
            cabac_init_present: bits.flag()?,
            num_ref_idx_l0_default_active_minus1: bits.ue()?,
            num_ref_idx_l1_default_active_minus1: bits.ue()?,
            init_qp_minus26: bits.se()?,
            constrained_intra_pred: bits.flag()?,
            transform_skip: bits.flag()?,
            cu_qp_delta_enabled: bits.flag()?,
            ..Pps::default()
        };
        if pps.num_ref_idx_l0_default_active_minus1 > 14 || pps.num_ref_idx_l1_default_active_minus1 > 14 {
            return None;
        }
        if pps.cu_qp_delta_enabled {
            pps.diff_cu_qp_delta_depth = bits.ue()?;
        }
        pps.cb_qp_offset = bits.se()?;
        pps.cr_qp_offset = bits.se()?;
        pps.slice_chroma_qp_offsets_present = bits.flag()?;
        pps.weighted_pred = bits.flag()?;
        pps.weighted_bipred = bits.flag()?;
        pps.transquant_bypass = bits.flag()?;
        let tiled = bits.flag()?;
        pps.entropy_coding_sync = bits.flag()?;
        if tiled {
            let mut tiles = Tiles { columns_minus1: bits.ue()?, rows_minus1: bits.ue()?, ..Tiles::default() };
            if tiles.columns_minus1 > 18 || tiles.rows_minus1 > 20 {
                return None;
            }
            if !bits.flag()? {
                for _ in 0..tiles.columns_minus1 {
                    tiles.column_widths_minus1.push(bits.ue()?);
                }
                for _ in 0..tiles.rows_minus1 {
                    tiles.row_heights_minus1.push(bits.ue()?);
                }
            }
            tiles.loop_filter_across_tiles = bits.flag()?;
            pps.tiles = Some(tiles);
        }
        pps.loop_filter_across_slices = bits.flag()?;
        pps.deblocking_control_present = bits.flag()?;
        if pps.deblocking_control_present {
            pps.deblocking_override_enabled = bits.flag()?;
            pps.deblocking_disabled = bits.flag()?;
            if !pps.deblocking_disabled {
                pps.beta_offset_div2 = bits.se()?;
                pps.tc_offset_div2 = bits.se()?;
            }
        }
        if bits.flag()? {
            return None;
        }
        pps.lists_modification_present = bits.flag()?;
        pps.log2_parallel_merge_level_minus2 = bits.ue()?;
        pps.slice_header_extension_present = bits.flag()?;
        // Within the standard's ranges (7.4.3.3), as the SPS's are.
        let within = (-26..=25).contains(&pps.init_qp_minus26)
            && pps.diff_cu_qp_delta_depth <= 3
            && (-12..=12).contains(&pps.cb_qp_offset)
            && (-12..=12).contains(&pps.cr_qp_offset)
            && (-6..=6).contains(&pps.beta_offset_div2)
            && (-6..=6).contains(&pps.tc_offset_div2)
            && pps.log2_parallel_merge_level_minus2 <= 4;
        (within && pps.id <= 63 && pps.sps_id <= 15).then_some(pps)
    }

    /// As a PPS NAL unit, without a start code.
    pub fn write(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(self.id);
        w.ue(self.sps_id);
        w.flag(self.dependent_slice_segments_enabled);
        w.flag(self.output_flag_present);
        w.put(3, self.num_extra_slice_header_bits);
        w.flag(self.sign_data_hiding);
        w.flag(self.cabac_init_present);
        w.ue(self.num_ref_idx_l0_default_active_minus1);
        w.ue(self.num_ref_idx_l1_default_active_minus1);
        w.se(self.init_qp_minus26);
        w.flag(self.constrained_intra_pred);
        w.flag(self.transform_skip);
        w.flag(self.cu_qp_delta_enabled);
        if self.cu_qp_delta_enabled {
            w.ue(self.diff_cu_qp_delta_depth);
        }
        w.se(self.cb_qp_offset);
        w.se(self.cr_qp_offset);
        w.flag(self.slice_chroma_qp_offsets_present);
        w.flag(self.weighted_pred);
        w.flag(self.weighted_bipred);
        w.flag(self.transquant_bypass);
        w.flag(self.tiles.is_some());
        w.flag(self.entropy_coding_sync);
        if let Some(tiles) = &self.tiles {
            w.ue(tiles.columns_minus1);
            w.ue(tiles.rows_minus1);
            let uniform = tiles.column_widths_minus1.is_empty();
            w.flag(uniform);
            if !uniform {
                for &c in &tiles.column_widths_minus1 {
                    w.ue(c);
                }
                for &r in &tiles.row_heights_minus1 {
                    w.ue(r);
                }
            }
            w.flag(tiles.loop_filter_across_tiles);
        }
        w.flag(self.loop_filter_across_slices);
        w.flag(self.deblocking_control_present);
        if self.deblocking_control_present {
            w.flag(self.deblocking_override_enabled);
            w.flag(self.deblocking_disabled);
            if !self.deblocking_disabled {
                w.se(self.beta_offset_div2);
                w.se(self.tc_offset_div2);
            }
        }
        w.flag(false);
        w.flag(self.lists_modification_present);
        w.ue(self.log2_parallel_merge_level_minus2);
        w.flag(self.slice_header_extension_present);
        w.flag(false);
        unit(PPS, &w.finish())
    }
}

/// A slice segment's header, as far as a decoder needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct SliceHeader {
    pub nal_type: u8,
    pub first_slice_segment_in_pic: bool,
    pub no_output_of_prior_pics: bool,
    pub pps_id: u32,
    pub segment_address: u32,
    pub slice_type: u32,
    pub pic_output: bool,
    pub poc_lsb: u32,
    /// The SPS's set of short-term references this uses, if not its own.
    pub rps_index: Option<u32>,
    /// Its own set (when `rps_index` is `None`).
    pub rps: Rps,
    pub temporal_mvp: bool,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub num_ref_idx_active_override: bool,
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    /// Lists reordered: each entry's place in the initial list.
    pub list_entry_l0: Option<Vec<u32>>,
    pub list_entry_l1: Option<Vec<u32>>,
    pub mvd_l1_zero: bool,
    pub cabac_init: bool,
    pub collocated_from_l0: bool,
    pub collocated_ref_idx: u32,
    pub five_minus_max_num_merge_cand: u32,
    pub qp_delta: i32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub deblocking_override: bool,
    /// As the slice has them: its own, or the PPS's.
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices: bool,
    /// Offsets of tiles' or rows' data, less one.
    pub entry_points_minus1: Vec<u32>,
    /// Bytes from the NAL unit's start (its header included) to the
    /// slice's data, as read without emulation-prevention bytes.
    pub header_bytes: usize,
    /// Bits of its own set of short-term references (0 if the SPS's).
    pub st_rps_bits: usize,
}

impl Default for SliceHeader {
    fn default() -> SliceHeader {
        SliceHeader {
            nal_type: TRAIL_R,
            first_slice_segment_in_pic: true,
            no_output_of_prior_pics: false,
            pps_id: 0,
            segment_address: 0,
            slice_type: SLICE_I,
            pic_output: true,
            poc_lsb: 0,
            rps_index: None,
            rps: Rps::default(),
            temporal_mvp: false,
            sao_luma: false,
            sao_chroma: false,
            num_ref_idx_active_override: false,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            list_entry_l0: None,
            list_entry_l1: None,
            mvd_l1_zero: false,
            cabac_init: false,
            collocated_from_l0: true,
            collocated_ref_idx: 0,
            five_minus_max_num_merge_cand: 0,
            qp_delta: 0,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            deblocking_override: false,
            deblocking_disabled: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            loop_filter_across_slices: false,
            entry_points_minus1: Vec::new(),
            header_bytes: 0,
            st_rps_bits: 0,
        }
    }
}

impl SliceHeader {
    /// Its short-term references: its own, or the SPS's it names.
    pub fn rps<'a>(&'a self, sps: &'a Sps) -> &'a Rps {
        match self.rps_index {
            Some(i) => &sps.short_term_rps[i as usize],
            None => &self.rps,
        }
    }

    /// The pictures its references may be chosen from (NumPicTotalCurr).
    pub fn num_pic_total_curr(&self, sps: &Sps) -> u32 {
        if is_idr(self.nal_type) {
            0
        } else {
            self.rps(sps).used() as u32
        }
    }

    /// From a slice's NAL unit (its header included), with the parameter
    /// sets so far. `None` for what this doesn't decode.
    pub fn parse(nal: &[u8], spss: &HashMap<u32, Sps>, ppss: &HashMap<u32, Pps>) -> Option<SliceHeader> {
        let kind = nal_type(nal)?;
        let layer = ((nal.first()? & 1) << 5) | (nal.get(1)? >> 3);
        if !is_slice(kind) || nal[0] & 0x80 != 0 || layer != 0 {
            return None;
        }
        let rbsp = unescape(nal);
        let mut bits = Bits::new(&rbsp);
        bits.take(16)?;
        let mut slice = SliceHeader { nal_type: kind, first_slice_segment_in_pic: bits.flag()?, ..SliceHeader::default() };
        if is_irap(kind) {
            slice.no_output_of_prior_pics = bits.flag()?;
        }
        slice.pps_id = bits.ue()?;
        let pps = ppss.get(&slice.pps_id)?;
        let sps = spss.get(&pps.sps_id)?;
        if !slice.first_slice_segment_in_pic {
            if pps.dependent_slice_segments_enabled && bits.flag()? {
                return None;
            }
            let (x, y) = sps.ctbs();
            slice.segment_address = bits.take(bits_for(x * y))?;
        }
        bits.take(pps.num_extra_slice_header_bits)?;
        slice.slice_type = bits.ue()?;
        if slice.slice_type > SLICE_I {
            return None;
        }
        if pps.output_flag_present {
            slice.pic_output = bits.flag()?;
        }
        if !is_idr(kind) {
            slice.poc_lsb = bits.take(sps.log2_max_poc_lsb_minus4 + 4)?;
            if bits.flag()? {
                let sets = sps.short_term_rps.len() as u32;
                if sets == 0 {
                    return None;
                }
                let index = bits.take(bits_for(sets))?;
                if index >= sets {
                    return None;
                }
                slice.rps_index = Some(index);
            } else {
                let start = bits.at();
                slice.rps = Rps::read(&mut bits, sps.short_term_rps.len(), &sps.short_term_rps, true)?;
                slice.st_rps_bits = bits.at() - start;
            }
            if sps.long_term_refs_present && bits.ue()? != 0 {
                return None;
            }
            if sps.temporal_mvp {
                slice.temporal_mvp = bits.flag()?;
            }
        }
        if sps.sao {
            slice.sao_luma = bits.flag()?;
            if sps.chroma_format_idc != 0 {
                slice.sao_chroma = bits.flag()?;
            }
        }
        slice.deblocking_disabled = pps.deblocking_disabled;
        slice.beta_offset_div2 = pps.beta_offset_div2;
        slice.tc_offset_div2 = pps.tc_offset_div2;
        if slice.slice_type != SLICE_I {
            slice.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
            slice.num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
            slice.num_ref_idx_active_override = bits.flag()?;
            if slice.num_ref_idx_active_override {
                slice.num_ref_idx_l0_active_minus1 = bits.ue()?;
                if slice.slice_type == SLICE_B {
                    slice.num_ref_idx_l1_active_minus1 = bits.ue()?;
                }
            }
            if slice.num_ref_idx_l0_active_minus1 > 14 || slice.num_ref_idx_l1_active_minus1 > 14 {
                return None;
            }
            let total = slice.num_pic_total_curr(sps);
            if pps.lists_modification_present && total > 1 {
                let width = bits_for(total);
                if bits.flag()? {
                    let entries = (0..=slice.num_ref_idx_l0_active_minus1).map(|_| bits.take(width)).collect::<Option<Vec<u32>>>()?;
                    slice.list_entry_l0 = Some(entries);
                }
                if slice.slice_type == SLICE_B && bits.flag()? {
                    let entries = (0..=slice.num_ref_idx_l1_active_minus1).map(|_| bits.take(width)).collect::<Option<Vec<u32>>>()?;
                    slice.list_entry_l1 = Some(entries);
                }
            }
            if slice.slice_type == SLICE_B {
                slice.mvd_l1_zero = bits.flag()?;
            }
            if pps.cabac_init_present {
                slice.cabac_init = bits.flag()?;
            }
            if slice.temporal_mvp {
                if slice.slice_type == SLICE_B {
                    slice.collocated_from_l0 = bits.flag()?;
                }
                let refs = if slice.collocated_from_l0 { slice.num_ref_idx_l0_active_minus1 } else { slice.num_ref_idx_l1_active_minus1 };
                if refs > 0 {
                    slice.collocated_ref_idx = bits.ue()?;
                }
            }
            if (pps.weighted_pred && slice.slice_type == SLICE_P) || (pps.weighted_bipred && slice.slice_type == SLICE_B) {
                return None;
            }
            slice.five_minus_max_num_merge_cand = bits.ue()?;
        }
        slice.qp_delta = bits.se()?;
        if pps.slice_chroma_qp_offsets_present {
            slice.cb_qp_offset = bits.se()?;
            slice.cr_qp_offset = bits.se()?;
        }
        if pps.deblocking_override_enabled {
            slice.deblocking_override = bits.flag()?;
        }
        if slice.deblocking_override {
            slice.deblocking_disabled = bits.flag()?;
            if !slice.deblocking_disabled {
                slice.beta_offset_div2 = bits.se()?;
                slice.tc_offset_div2 = bits.se()?;
            }
        }
        slice.loop_filter_across_slices = pps.loop_filter_across_slices;
        if pps.loop_filter_across_slices && (slice.sao_luma || slice.sao_chroma || !slice.deblocking_disabled) {
            slice.loop_filter_across_slices = bits.flag()?;
        }
        if pps.tiles.is_some() || pps.entropy_coding_sync {
            let count = bits.ue()?;
            if count > 1024 {
                return None;
            }
            if count > 0 {
                let width = bits.ue()? + 1;
                if width > 32 {
                    return None;
                }
                for _ in 0..count {
                    slice.entry_points_minus1.push(bits.take(width)?);
                }
            }
        }
        if pps.slice_header_extension_present {
            let length = bits.ue()?;
            for _ in 0..length {
                bits.take(8)?;
            }
        }
        if !bits.flag()? {
            return None;
        }
        while !bits.at().is_multiple_of(8) {
            if bits.flag()? {
                return None;
            }
        }
        slice.header_bytes = bits.at() / 8;
        // Within the standard's ranges (7.4.7.1), as the PPS's are.
        let qp = 26 + pps.init_qp_minus26 + slice.qp_delta;
        let within = (0..=51).contains(&qp)
            && slice.five_minus_max_num_merge_cand <= 4
            && (-12..=12).contains(&slice.cb_qp_offset)
            && (-12..=12).contains(&slice.cr_qp_offset)
            && (-6..=6).contains(&slice.beta_offset_div2)
            && (-6..=6).contains(&slice.tc_offset_div2);
        within.then_some(slice)
    }

    /// The bytes from `nal`'s start to the slice's data, as `nal` has
    /// them (emulation-prevention bytes counted).
    pub fn data_offset(&self, nal: &[u8]) -> usize {
        let (mut raw, mut zeros, mut at) = (0, 0, 0);
        while raw < self.header_bytes && at < nal.len() {
            let byte = nal[at];
            at += 1;
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            raw += 1;
        }
        if zeros >= 2 && nal.get(at) == Some(&3) {
            at += 1;
        }
        at
    }

    /// As a slice's NAL unit up to its data (without a start code), for
    /// a packed slice header: escaped, whole bytes (the header ends
    /// aligned).
    pub fn write(&self, sps: &Sps, pps: &Pps) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(16, ((self.nal_type as u32) << 9) | 1);
        w.flag(self.first_slice_segment_in_pic);
        if is_irap(self.nal_type) {
            w.flag(self.no_output_of_prior_pics);
        }
        w.ue(self.pps_id);
        if !self.first_slice_segment_in_pic {
            if pps.dependent_slice_segments_enabled {
                w.flag(false);
            }
            let (x, y) = sps.ctbs();
            w.put(bits_for(x * y), self.segment_address);
        }
        w.put(pps.num_extra_slice_header_bits, 0);
        w.ue(self.slice_type);
        if pps.output_flag_present {
            w.flag(self.pic_output);
        }
        if !is_idr(self.nal_type) {
            w.put(sps.log2_max_poc_lsb_minus4 + 4, self.poc_lsb);
            w.flag(self.rps_index.is_some());
            match self.rps_index {
                Some(index) => w.put(bits_for(sps.short_term_rps.len() as u32), index),
                None => self.rps.write(&mut w, sps.short_term_rps.len()),
            }
            if sps.long_term_refs_present {
                w.ue(0);
            }
            if sps.temporal_mvp {
                w.flag(self.temporal_mvp);
            }
        }
        if sps.sao {
            w.flag(self.sao_luma);
            if sps.chroma_format_idc != 0 {
                w.flag(self.sao_chroma);
            }
        }
        if self.slice_type != SLICE_I {
            w.flag(self.num_ref_idx_active_override);
            if self.num_ref_idx_active_override {
                w.ue(self.num_ref_idx_l0_active_minus1);
                if self.slice_type == SLICE_B {
                    w.ue(self.num_ref_idx_l1_active_minus1);
                }
            }
            let total = self.num_pic_total_curr(sps);
            if pps.lists_modification_present && total > 1 {
                for (list, used) in [(&self.list_entry_l0, true), (&self.list_entry_l1, self.slice_type == SLICE_B)] {
                    if used {
                        w.flag(list.is_some());
                        for &entry in list.iter().flatten() {
                            w.put(bits_for(total), entry);
                        }
                    }
                }
            }
            if self.slice_type == SLICE_B {
                w.flag(self.mvd_l1_zero);
            }
            if pps.cabac_init_present {
                w.flag(self.cabac_init);
            }
            if self.temporal_mvp {
                if self.slice_type == SLICE_B {
                    w.flag(self.collocated_from_l0);
                }
                let refs = if self.collocated_from_l0 { self.num_ref_idx_l0_active_minus1 } else { self.num_ref_idx_l1_active_minus1 };
                if refs > 0 {
                    w.ue(self.collocated_ref_idx);
                }
            }
            w.ue(self.five_minus_max_num_merge_cand);
        }
        w.se(self.qp_delta);
        if pps.slice_chroma_qp_offsets_present {
            w.se(self.cb_qp_offset);
            w.se(self.cr_qp_offset);
        }
        if pps.deblocking_override_enabled {
            w.flag(self.deblocking_override);
        }
        if self.deblocking_override {
            w.flag(self.deblocking_disabled);
            if !self.deblocking_disabled {
                w.se(self.beta_offset_div2);
                w.se(self.tc_offset_div2);
            }
        }
        if pps.loop_filter_across_slices && (self.sao_luma || self.sao_chroma || !self.deblocking_disabled) {
            w.flag(self.loop_filter_across_slices);
        }
        if pps.tiles.is_some() || pps.entropy_coding_sync {
            w.ue(self.entry_points_minus1.len() as u32);
            if !self.entry_points_minus1.is_empty() {
                let width = self.entry_points_minus1.iter().map(|&o| 32 - o.leading_zeros()).max().unwrap_or(1).max(1);
                w.ue(width - 1);
                for &o in &self.entry_points_minus1 {
                    w.put(width, o);
                }
            }
        }
        if pps.slice_header_extension_present {
            w.ue(0);
        }
        w.put(1, 1);
        while !w.written().is_multiple_of(8) {
            w.put(1, 0);
        }
        escape(&w.bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Found by fuzzing (`fuzz/`, target h265): a chain of reference
    /// picture sets whose steps, past the standard's 2^15, overflowed;
    /// and sets claiming so many pictures that their count did.
    #[test]
    fn reference_picture_sets_too_long_are_refused_not_overflowed() {
        for stream in [&include_bytes!("../testdata/h265-rps-steps.bin")[..], include_bytes!("../testdata/h265-rps-count.bin"), include_bytes!("../testdata/h265-rps-count2.bin"), include_bytes!("../testdata/h265-rps-step-huge.bin")] {
            read_as_the_chip_does(stream);
        }
    }

    fn read_as_the_chip_does(stream: &[u8]) {
        let (mut spss, mut ppss) = (HashMap::new(), HashMap::new());
        for nal in crate::h264::nal_units(stream) {
            match nal_type(nal) {
                Some(SPS) => {
                    if let Some(sps) = Sps::parse(nal) {
                        spss.insert(sps.id, sps);
                    }
                }
                Some(PPS) => {
                    if let Some(pps) = Pps::parse(nal) {
                        ppss.insert(pps.id, pps);
                    }
                }
                Some(kind) if is_slice(kind) => {
                    let _ = SliceHeader::parse(nal, &spss, &ppss);
                }
                _ => {}
            }
        }
    }

    /// As H.264's (see `h264`'s test): the look before any decoder reads
    /// an SPS's size and pictures held as this parser does, and stops
    /// only frames asking for more than any stream needs.
    #[test]
    fn frames_asking_too_much_are_stopped_before_any_decoder() {
        use swarm_core::streamcheck::within_limits;
        let frame = |nal: Vec<u8>| [&[0, 0, 0, 1][..], &nal].concat();
        for fine in [sps(), Sps { width: 7680, height: 4320, max_dec_pic_buffering_minus1: 15, ..sps() }] {
            assert!(Sps::parse(&fine.write()).is_some());
            assert!(within_limits(&frame(fine.write()), true), "{fine:?}");
        }
        for wrong in [Sps { width: 16_000, height: 16_000, ..sps() }, Sps { max_dec_pic_buffering_minus1: 40, ..sps() }] {
            assert!(!within_limits(&frame(wrong.write()), true), "{wrong:?}");
        }
    }

    fn sps() -> Sps {
        Sps {
            ptl: Ptl::main(120),
            chroma_format_idc: 1,
            width: 1920,
            height: 1088,
            conformance: [0, 0, 0, 4],
            log2_max_poc_lsb_minus4: 4,
            max_dec_pic_buffering_minus1: 1,
            log2_min_cb_minus3: 0,
            log2_diff_max_min_cb: 3,
            log2_diff_max_min_tb: 3,
            max_transform_hierarchy_depth_inter: 3,
            max_transform_hierarchy_depth_intra: 3,
            amp: true,
            sao: true,
            temporal_mvp: true,
            short_term_rps: vec![Rps { negative: vec![(-1, true)], positive: vec![] }, Rps { negative: vec![(-1, true), (-3, false)], positive: vec![(2, true)] }],
            vui_fps: Some(30),
            ..Sps::default()
        }
    }

    #[test]
    fn parameter_sets_go_both_ways() {
        let sps = sps();
        let nal = sps.write();
        assert_eq!(nal_type(&nal), Some(SPS));
        assert_eq!(Sps::parse(&nal), Some(Sps { vui_fps: None, ..sps.clone() }));
        assert_eq!(sps.shown(), Some((1920, 1080, 0, 0)));
        assert_eq!(sps.ctbs(), (30, 17));
        for wrong in [
            Sps { log2_min_cb_minus3: u32::MAX - 1, log2_diff_max_min_cb: 2, ..sps.clone() },
            Sps { width: 1 << 30, ..sps.clone() },
            Sps { height: 1 << 30, ..sps.clone() },
            Sps { conformance: [u32::MAX - 1, u32::MAX - 1, 0, 0], ..sps.clone() },
            Sps { width: 16_000, height: 16_000, ..sps.clone() },
        ] {
            assert_eq!(Sps::parse(&wrong.write()), None, "{wrong:?}");
        }
        let pps = Pps {
            id: 1,
            init_qp_minus26: -4,
            cu_qp_delta_enabled: true,
            diff_cu_qp_delta_depth: 2,
            cb_qp_offset: -1,
            loop_filter_across_slices: true,
            deblocking_control_present: true,
            deblocking_override_enabled: true,
            beta_offset_div2: 2,
            tc_offset_div2: -1,
            lists_modification_present: true,
            tiles: Some(Tiles { columns_minus1: 2, rows_minus1: 1, column_widths_minus1: vec![9, 9], row_heights_minus1: vec![7], loop_filter_across_tiles: true }),
            entropy_coding_sync: true,
            ..Pps::default()
        };
        assert_eq!(Pps::parse(&pps.write()), Some(pps.clone()));
        // Past the standard's ranges, which the graphics driver gets in a
        // byte: not taken.
        for wrong in [
            Pps { init_qp_minus26: 26, ..pps.clone() },
            Pps { cb_qp_offset: 13, ..pps.clone() },
            Pps { cr_qp_offset: -300, ..pps.clone() },
            Pps { beta_offset_div2: 7, ..pps.clone() },
            Pps { log2_parallel_merge_level_minus2: 5, ..pps.clone() },
        ] {
            assert_eq!(Pps::parse(&wrong.write()), None, "{wrong:?}");
        }
        for wrong in [Sps { log2_min_tb_minus2: 4, ..sps.clone() }, Sps { max_transform_hierarchy_depth_intra: 200, ..sps.clone() }] {
            assert_eq!(Sps::parse(&wrong.write()), None, "{wrong:?}");
        }
        let tiles = pps.tiles.unwrap();
        assert_eq!(tiles.sizes_minus1(&sps), (vec![9, 9, 9], vec![7, 8]));
        let uniform = Tiles { columns_minus1: 3, rows_minus1: 0, ..Tiles::default() };
        assert_eq!(uniform.sizes_minus1(&sps).0, vec![6, 7, 6, 7]);
        let huge = Tiles { columns_minus1: 18, rows_minus1: 0, column_widths_minus1: vec![u32::MAX - 1; 18], ..Tiles::default() };
        assert_eq!(huge.sizes_minus1(&sps).0[18], 0);
        // And they're refused before reaching a decoder: sizes that
        // don't add up, or more tiles than a picture has blocks.
        assert!(tiles.fit(&sps) && uniform.fit(&sps));
        assert!(!huge.fit(&sps));
        let tiny = Sps { width: 64, height: 64, ..sps.clone() };
        let many = Tiles { columns_minus1: 18, rows_minus1: 0, ..Tiles::default() };
        assert!(!many.fit(&tiny));
        assert!(many.sizes_minus1(&tiny).0.iter().all(|&c| c == 0), "no underflow");
        assert_eq!(nal_type(&write_vps(&sps, 30)), Some(VPS));
    }

    /// A set predicted from another (7.4.8): the second set here, from
    /// the first moved one picture on, is the first's picture and the
    /// one it was predicted from.
    #[test]
    fn predicted_reference_sets_are_worked_out() {
        let mut w = BitWriter::default();
        let first = Rps { negative: vec![(-1, true), (-2, true)], positive: vec![(1, true)] };
        first.write(&mut w, 0);
        w.flag(true);
        w.flag(true);
        w.ue(0);
        for (used, kept) in [(true, true), (false, false), (false, true), (true, true)] {
            w.flag(used);
            if !used {
                w.flag(kept);
            }
        }
        let bytes = w.finish();
        let mut bits = Bits::new(&bytes);
        let sets = vec![Rps::read(&mut bits, 0, &[], false).unwrap()];
        assert_eq!(sets[0], first);
        let second = Rps::read(&mut bits, 1, &sets, false).unwrap();
        assert_eq!(second, Rps { negative: vec![(-1, true), (-2, true)], positive: vec![] });
    }

    /// Slice headers written read back the same, their data starting
    /// where said.
    #[test]
    fn slice_headers_go_both_ways() {
        let sps = sps();
        let plain = Pps { loop_filter_across_slices: true, ..Pps::default() };
        let busy = Pps { id: 1, output_flag_present: true, num_extra_slice_header_bits: 2, cabac_init_present: true, slice_chroma_qp_offsets_present: true, deblocking_control_present: true, deblocking_override_enabled: true, lists_modification_present: true, entropy_coding_sync: true, ..Pps::default() };
        let spss = HashMap::from([(0, sps.clone())]);
        let ppss = HashMap::from([(0, plain.clone()), (1, busy.clone())]);
        let headers = [
            (SliceHeader { nal_type: IDR_W_RADL, slice_type: SLICE_I, sao_luma: true, sao_chroma: true, qp_delta: 3, loop_filter_across_slices: true, ..SliceHeader::default() }, &plain),
            (SliceHeader { nal_type: TRAIL_R, slice_type: SLICE_P, poc_lsb: 5, rps: Rps { negative: vec![(-1, true)], positive: vec![] }, temporal_mvp: true, five_minus_max_num_merge_cand: 3, qp_delta: -2, loop_filter_across_slices: true, ..SliceHeader::default() }, &plain),
            (SliceHeader { nal_type: TRAIL_N, slice_type: SLICE_P, poc_lsb: 31, rps_index: Some(0), qp_delta: 0, ..SliceHeader::default() }, &plain),
            (
                SliceHeader {
                    nal_type: TRAIL_R,
                    first_slice_segment_in_pic: false,
                    segment_address: 77,
                    pps_id: 1,
                    slice_type: SLICE_B,
                    pic_output: false,
                    poc_lsb: 9,
                    rps_index: Some(1),
                    temporal_mvp: true,
                    num_ref_idx_active_override: true,
                    num_ref_idx_l0_active_minus1: 1,
                    num_ref_idx_l1_active_minus1: 0,
                    list_entry_l0: Some(vec![1, 0]),
                    mvd_l1_zero: true,
                    cabac_init: true,
                    collocated_from_l0: true,
                    collocated_ref_idx: 1,
                    five_minus_max_num_merge_cand: 1,
                    qp_delta: 4,
                    cb_qp_offset: -2,
                    cr_qp_offset: 1,
                    deblocking_override: true,
                    deblocking_disabled: false,
                    beta_offset_div2: -1,
                    tc_offset_div2: 2,
                    entry_points_minus1: vec![300, 4000],
                    ..SliceHeader::default()
                },
                &busy,
            ),
        ];
        for (header, pps) in headers {
            let nal = header.write(&sps, pps);
            let whole = [&nal[..], &[0x80, 0xff]].concat();
            let parsed = SliceHeader::parse(&whole, &spss, &ppss).unwrap_or_else(|| panic!("{header:?} reads"));
            assert_eq!(parsed.data_offset(&whole), nal.len());
            let expected = SliceHeader { header_bytes: parsed.header_bytes, st_rps_bits: parsed.st_rps_bits, ..header };
            assert_eq!(parsed, expected);
            assert_eq!(parsed.st_rps_bits > 0, expected.rps_index.is_none() && !is_idr(expected.nal_type));
        }
    }

    /// Where a slice's data starts is counted in the stream's bytes:
    /// emulation-prevention bytes in the header count.
    #[test]
    fn data_offsets_count_escapes() {
        let sps = Sps { log2_max_poc_lsb_minus4: 12, ..sps() };
        let spss = HashMap::from([(0, sps.clone())]);
        let pps = Pps { num_extra_slice_header_bits: 3, ..Pps::default() };
        let ppss = HashMap::from([(0, pps.clone())]);
        let rps = Rps { negative: (1..=15).map(|d| (-d, true)).collect(), positive: vec![] };
        let header = SliceHeader { nal_type: TRAIL_R, slice_type: SLICE_P, poc_lsb: 0, rps, ..SliceHeader::default() };
        let nal = header.write(&sps, &pps);
        assert!(unescape(&nal).len() < nal.len(), "no escape in {nal:x?}");
        let whole = [&nal[..], &[0xaa]].concat();
        let parsed = SliceHeader::parse(&whole, &spss, &ppss).unwrap();
        assert_eq!(parsed.data_offset(&whole), nal.len());
        assert_eq!(parsed.header_bytes, unescape(&nal).len());
    }
}
