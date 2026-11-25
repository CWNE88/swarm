//! H.265 for the stand-in driver: decoding with libde265 and encoding
//! with libx265, both loaded when wanted (without either, H.265 isn't
//! offered). As for H.264, what VA-API is given is checked as a chip
//! would need it.
//!
//! - Decoding: the parameter sets are made from what VA-API is given (as
//!   AMD's firmware works from), each slice's header read with them and
//!   checked against its slice parameters, and its references against
//!   the pictures the surfaces hold; libde265 decodes the slices with the
//!   parameter sets made.
//! - Encoding: the packed VPS, SPS and PPS are checked against the
//!   sequence and picture parameters, the packed slice header against
//!   the slice's, and the references and picture order against the
//!   pictures before; libx265 encodes the picture at the size the SPS
//!   shows. With `FAKEVA_MESA=1`, as Mesa's driver for AMD's chips: it
//!   says its blocks (coding trees of 64, from 8) and features, and
//!   aligns surfaces to 64 across and 16 down.

use super::*;

/// libde265's functions.
struct De265Lib {
    new_decoder: unsafe extern "C" fn() -> *mut c_void,
    free_decoder: unsafe extern "C" fn(*mut c_void) -> c_int,
    push_nal: unsafe extern "C" fn(*mut c_void, *const c_void, c_int, i64, *mut c_void) -> c_int,
    push_end_of_frame: unsafe extern "C" fn(*mut c_void),
    flush_data: unsafe extern "C" fn(*mut c_void) -> c_int,
    decode: unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int,
    get_next_picture: unsafe extern "C" fn(*mut c_void) -> *const c_void,
    get_image_plane: unsafe extern "C" fn(*const c_void, c_int, *mut c_int) -> *const u8,
    get_image_width: unsafe extern "C" fn(*const c_void, c_int) -> c_int,
    get_image_height: unsafe extern "C" fn(*const c_void, c_int) -> c_int,
    _lib: libloading::Library,
}

fn de265() -> Option<&'static De265Lib> {
    static LIB: std::sync::OnceLock<Option<De265Lib>> = std::sync::OnceLock::new();
    LIB.get_or_init(|| unsafe {
        let lib = libloading::Library::new("libde265.so.0").ok()?;
        macro_rules! f {
            ($name:literal) => {
                *lib.get(concat!($name, "\0").as_bytes()).ok()?
            };
        }
        Some(De265Lib {
            new_decoder: f!("de265_new_decoder"),
            free_decoder: f!("de265_free_decoder"),
            push_nal: f!("de265_push_NAL"),
            push_end_of_frame: f!("de265_push_end_of_frame"),
            flush_data: f!("de265_flush_data"),
            decode: f!("de265_decode"),
            get_next_picture: f!("de265_get_next_picture"),
            get_image_plane: f!("de265_get_image_plane"),
            get_image_width: f!("de265_get_image_width"),
            get_image_height: f!("de265_get_image_height"),
            _lib: lib,
        })
    })
    .as_ref()
}

/// Whether H.265 is offered: libde265 is there, and `FAKEVA_H265` isn't
/// 0.
pub fn offered() -> bool {
    switch("FAKEVA_H265").as_deref() != Some("0") && de265().is_some()
}

/// Whether H.265 encoding is offered: libx265 is there too, and
/// `FAKEVA_H265` isn't `decode`.
pub fn encodes() -> bool {
    offered() && switch("FAKEVA_H265").as_deref() != Some("decode") && x265().is_some()
}

/// A libde265 decoder.
pub struct De265 {
    lib: &'static De265Lib,
    ctx: *mut c_void,
}

unsafe impl Send for De265 {}

impl De265 {
    pub fn new() -> Option<De265> {
        let lib = de265()?;
        let ctx = unsafe { (lib.new_decoder)() };
        (!ctx.is_null()).then_some(De265 { lib, ctx })
    }

    /// Decodes a frame's NAL units (without start codes): its picture as
    /// I420 (width, height, data), if one came out.
    pub fn decode(&mut self, units: &[Vec<u8>]) -> Result<Option<(usize, usize, Vec<u8>)>, String> {
        unsafe {
            for unit in units {
                let status = (self.lib.push_nal)(self.ctx, unit.as_ptr() as *const c_void, unit.len() as c_int, 0, std::ptr::null_mut());
                if status != 0 {
                    return Err(format!("libde265 took no NAL unit ({status})"));
                }
            }
            (self.lib.push_end_of_frame)(self.ctx);
            for _ in 0..1000 {
                let mut more = 0;
                let status = (self.lib.decode)(self.ctx, &mut more);
                let image = (self.lib.get_next_picture)(self.ctx);
                if !image.is_null() {
                    return Ok(Some(self.i420(image)));
                }
                if status == 13 || more == 0 {
                    return Ok(None);
                }
                if status != 0 && status < 1000 {
                    return Err(format!("libde265 couldn't decode it ({status})"));
                }
            }
            Ok(None)
        }
    }

    unsafe fn i420(&self, image: *const c_void) -> (usize, usize, Vec<u8>) {
        let (width, height) = ((self.lib.get_image_width)(image, 0) as usize, (self.lib.get_image_height)(image, 0) as usize);
        let mut out = Vec::with_capacity(width * height * 3 / 2);
        for channel in 0..3 {
            let (w, h) = if channel == 0 { (width, height) } else { (width / 2, height / 2) };
            let mut stride = 0;
            let plane = (self.lib.get_image_plane)(image, channel, &mut stride);
            for row in 0..h {
                out.extend_from_slice(std::slice::from_raw_parts(plane.add(row * stride as usize), w));
            }
        }
        (width, height, out)
    }
}

impl Drop for De265 {
    fn drop(&mut self) {
        unsafe { (self.lib.free_decoder)(self.ctx) };
    }
}

/// What an H.265 decoding context keeps between pictures.
pub struct Dec {
    de265: De265,
    /// The parameter sets last given libde265 (given again only when
    /// they change).
    sets: Vec<u8>,
}

impl Dec {
    pub fn new() -> Option<Dec> {
        Some(Dec { de265: De265::new()?, sets: Vec::new() })
    }
}

/// The pictures' order and reference flags for a slice's references:
/// those before and after it that it uses, by its reference set.
fn expected_lists(header: &h265::SliceHeader, sps: &h265::Sps, poc: i32) -> (Vec<i32>, Vec<i32>) {
    let rps = header.rps(sps);
    let before = rps.negative.iter().filter(|r| r.1).map(|r| poc + r.0).collect();
    let after = rps.positive.iter().filter(|r| r.1).map(|r| poc + r.0).collect();
    (before, after)
}

pub fn decode(s: &mut State, c: &mut Context) -> VAStatus {
    let (mut pic, mut slices) = (None, Vec::new());
    let mut param = None;
    for id in &c.pending {
        let Some((kind, bytes)) = plain(s, id) else { continue };
        match kind {
            VAPictureParameterBufferType => pic = VAPictureParameterBufferHEVC::read(bytes),
            VAIQMatrixBufferType => return problem("an inverse quantisation matrix without scaling lists".into()),
            VASliceParameterBufferType => param = VASliceParameterBufferHEVC::read(bytes),
            VASliceDataBufferType => match param.take() {
                Some(p) => {
                    let Some(nal) = bytes.get(p.slice_data_offset as usize..(p.slice_data_offset + p.slice_data_size) as usize) else { return problem("slice data past its buffer".into()) };
                    slices.push((p, nal.to_vec()));
                }
                None => return problem("slice data without its parameters".into()),
            },
            _ => {}
        }
    }
    let Some(pic) = pic else { return problem("a picture to decode without its picture parameters".into()) };
    if slices.is_empty() {
        return problem("a picture with no slices".into());
    }
    if pic.CurrPic.picture_id != c.target {
        return problem(format!("the current picture {:?}, decoding into {}", pic.CurrPic, c.target));
    }
    let f = pic.pic_fields;
    let p = pic.slice_parsing_fields;
    let sps = h265::Sps {
        ptl: h265::Ptl::main(186),
        chroma_format_idc: bits_of(f, 0, 2),
        width: pic.pic_width_in_luma_samples as u32,
        height: pic.pic_height_in_luma_samples as u32,
        bit_depth_luma_minus8: pic.bit_depth_luma_minus8 as u32,
        bit_depth_chroma_minus8: pic.bit_depth_chroma_minus8 as u32,
        log2_max_poc_lsb_minus4: pic.log2_max_pic_order_cnt_lsb_minus4 as u32,
        max_dec_pic_buffering_minus1: pic.sps_max_dec_pic_buffering_minus1 as u32,
        log2_min_cb_minus3: pic.log2_min_luma_coding_block_size_minus3 as u32,
        log2_diff_max_min_cb: pic.log2_diff_max_min_luma_coding_block_size as u32,
        log2_min_tb_minus2: pic.log2_min_transform_block_size_minus2 as u32,
        log2_diff_max_min_tb: pic.log2_diff_max_min_transform_block_size as u32,
        max_transform_hierarchy_depth_inter: pic.max_transform_hierarchy_depth_inter as u32,
        max_transform_hierarchy_depth_intra: pic.max_transform_hierarchy_depth_intra as u32,
        amp: bits_of(f, 6, 1) == 1,
        sao: bits_of(p, 7, 1) == 1,
        pcm: (bits_of(f, 3, 1) == 1).then(|| h265::Pcm {
            bit_depth_luma_minus1: pic.pcm_sample_bit_depth_luma_minus1 as u32,
            bit_depth_chroma_minus1: pic.pcm_sample_bit_depth_chroma_minus1 as u32,
            log2_min_minus3: pic.log2_min_pcm_luma_coding_block_size_minus3 as u32,
            log2_diff_max_min: pic.log2_diff_max_min_pcm_luma_coding_block_size as u32,
            loop_filter_disabled: bits_of(f, 18, 1) == 1,
        }),
        long_term_refs_present: bits_of(p, 1, 1) == 1,
        temporal_mvp: bits_of(p, 2, 1) == 1,
        strong_intra_smoothing: bits_of(f, 7, 1) == 1,
        short_term_rps: vec![h265::Rps::default(); pic.num_short_term_ref_pic_sets as usize],
        ..h265::Sps::default()
    };
    let read_pps_id = |nal: &[u8]| {
        let rbsp = h264::unescape(&nal[2..]);
        let mut bits = h264::Bits::new(&rbsp);
        bits.take(1)?;
        if h265::is_irap(h265::nal_type(nal)?) {
            bits.take(1)?;
        }
        bits.ue()
    };
    let Some(pps_id) = read_pps_id(&slices[0].1) else { return problem("a slice whose header doesn't read".into()) };
    let tiles = (bits_of(f, 14, 1) == 1).then(|| {
        let (columns, rows) = (pic.num_tile_columns_minus1 as usize, pic.num_tile_rows_minus1 as usize);
        h265::Tiles {
            columns_minus1: columns as u32,
            rows_minus1: rows as u32,
            column_widths_minus1: pic.column_width_minus1[..columns].iter().map(|&w| w as u32).collect(),
            row_heights_minus1: pic.row_height_minus1[..rows].iter().map(|&h| h as u32).collect(),
            loop_filter_across_tiles: bits_of(f, 17, 1) == 1,
        }
    });
    let pps = h265::Pps {
        id: pps_id,
        sps_id: 0,
        dependent_slice_segments_enabled: bits_of(p, 5, 1) == 1,
        output_flag_present: bits_of(p, 4, 1) == 1,
        num_extra_slice_header_bits: pic.num_extra_slice_header_bits as u32,
        sign_data_hiding: bits_of(f, 8, 1) == 1,
        cabac_init_present: bits_of(p, 3, 1) == 1,
        num_ref_idx_l0_default_active_minus1: pic.num_ref_idx_l0_default_active_minus1 as u32,
        num_ref_idx_l1_default_active_minus1: pic.num_ref_idx_l1_default_active_minus1 as u32,
        init_qp_minus26: pic.init_qp_minus26 as i32,
        constrained_intra_pred: bits_of(f, 9, 1) == 1,
        transform_skip: bits_of(f, 5, 1) == 1,
        cu_qp_delta_enabled: bits_of(f, 10, 1) == 1,
        diff_cu_qp_delta_depth: pic.diff_cu_qp_delta_depth as u32,
        cb_qp_offset: pic.pps_cb_qp_offset as i32,
        cr_qp_offset: pic.pps_cr_qp_offset as i32,
        slice_chroma_qp_offsets_present: bits_of(p, 6, 1) == 1,
        weighted_pred: bits_of(f, 11, 1) == 1,
        weighted_bipred: bits_of(f, 12, 1) == 1,
        transquant_bypass: bits_of(f, 13, 1) == 1,
        tiles,
        entropy_coding_sync: bits_of(f, 15, 1) == 1,
        loop_filter_across_slices: bits_of(f, 16, 1) == 1,
        deblocking_control_present: true,
        deblocking_override_enabled: bits_of(p, 8, 1) == 1,
        deblocking_disabled: bits_of(p, 9, 1) == 1,
        beta_offset_div2: pic.pps_beta_offset_div2 as i32,
        tc_offset_div2: pic.pps_tc_offset_div2 as i32,
        lists_modification_present: bits_of(p, 0, 1) == 1,
        log2_parallel_merge_level_minus2: pic.log2_parallel_merge_level_minus2 as u32,
        slice_header_extension_present: bits_of(p, 10, 1) == 1,
    };
    let spss = HashMap::from([(0, sps.clone())]);
    let ppss = HashMap::from([(pps_id, pps.clone())]);
    let poc = pic.CurrPic.pic_order_cnt;
    let references: Vec<VAPictureHEVC> = pic.ReferenceFrames.iter().copied().filter(|r| r.flags & VA_PICTURE_HEVC_INVALID == 0).collect();
    for r in &references {
        let held = s.surfaces.get(&r.picture_id).and_then(|s| s.poc);
        if held != Some(r.pic_order_cnt) {
            return problem(format!("reference {r:?}: the surface holds {held:?}"));
        }
    }
    let mut units = Vec::new();
    let count = slices.len();
    for (n, (param, nal)) in slices.iter().enumerate() {
        let Some(header) = h265::SliceHeader::parse(nal, &spss, &ppss) else { return problem("a slice whose header doesn't read with the parameters given".into()) };
        let (header, nal) = match header.rps_index {
            Some(_) => {
                let mut rps = h265::Rps::default();
                let mut listed: Vec<&VAPictureHEVC> = references.iter().collect();
                listed.sort_by_key(|r| (r.pic_order_cnt - poc).abs());
                for r in listed {
                    let delta = r.pic_order_cnt - poc;
                    if delta < 0 {
                        rps.negative.push((delta, r.flags & VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE != 0));
                    } else {
                        rps.positive.push((delta, r.flags & VA_PICTURE_HEVC_RPS_ST_CURR_AFTER != 0));
                    }
                }
                let own = h265::SliceHeader { rps_index: None, rps, ..header.clone() };
                let mut rewritten = own.write(&sps, &pps);
                rewritten.extend_from_slice(&nal[header.data_offset(nal)..]);
                (h265::SliceHeader { header_bytes: header.header_bytes, st_rps_bits: header.st_rps_bits, ..own }, rewritten)
            }
            None => (header, nal.clone()),
        };
        let nal = &nal;
        let original = &slices[n].1;
        let flags = param.LongSliceFlags;
        let kind = header.nal_type;
        if n == 0 {
            if bits_of(p, 11, 1) != h265::is_irap(kind) as u32 || bits_of(p, 12, 1) != h265::is_idr(kind) as u32 {
                return problem(format!("intra flags {:#x} for a picture of NAL type {kind}", p));
            }
            if pic.st_rps_bits as usize != header.st_rps_bits {
                return problem(format!("st_rps_bits {}, where the header's set takes {}", pic.st_rps_bits, header.st_rps_bits));
            }
            let lsb = poc.rem_euclid(sps.max_poc_lsb() as i32) as u32;
            if lsb != header.poc_lsb && !h265::is_idr(kind) || (h265::is_idr(kind) && poc != 0) {
                return problem(format!("picture order {poc}, where its header says {} (low bits)", header.poc_lsb));
            }
        }
        let fields = [
            ("data offset", param.slice_data_byte_offset as i64, header.data_offset(original) as i64),
            ("address", param.slice_segment_address as i64, header.segment_address as i64),
            ("last", bits_of(flags, 0, 1) as i64, (n + 1 == count) as i64),
            ("type", bits_of(flags, 2, 2) as i64, header.slice_type as i64),
            ("SAO luma", bits_of(flags, 6, 1) as i64, header.sao_luma as i64),
            ("SAO chroma", bits_of(flags, 7, 1) as i64, header.sao_chroma as i64),
            ("mvd_l1_zero", bits_of(flags, 8, 1) as i64, header.mvd_l1_zero as i64),
            ("cabac_init", bits_of(flags, 9, 1) as i64, header.cabac_init as i64),
            ("temporal MVP", bits_of(flags, 10, 1) as i64, header.temporal_mvp as i64),
            ("deblocking off", bits_of(flags, 11, 1) as i64, header.deblocking_disabled as i64),
            ("collocated from l0", bits_of(flags, 12, 1) as i64, header.collocated_from_l0 as i64),
            ("loop filter across slices", bits_of(flags, 13, 1) as i64, header.loop_filter_across_slices as i64),
            ("collocated_ref_idx", param.collocated_ref_idx as i64, if header.temporal_mvp { header.collocated_ref_idx as i64 } else { 0xff }),
            ("qp delta", param.slice_qp_delta as i64, header.qp_delta as i64),
            ("beta", param.slice_beta_offset_div2 as i64, header.beta_offset_div2 as i64),
            ("tc", param.slice_tc_offset_div2 as i64, header.tc_offset_div2 as i64),
        ];
        for (name, given, read) in fields {
            if given != read {
                return problem(format!("slice {n}'s {name}: {given} given, its header says {read}"));
            }
        }
        if header.slice_type != h265::SLICE_I {
            let (before, after) = expected_lists(&header, &sps, poc);
            for (pocs, flag) in [(&before, VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE), (&after, VA_PICTURE_HEVC_RPS_ST_CURR_AFTER)] {
                for &want in pocs {
                    if !references.iter().any(|r| r.pic_order_cnt == want && r.flags & flag != 0) {
                        return problem(format!("picture {poc}: reference {want} not among the references flagged {flag:#x}: {references:?}"));
                    }
                }
            }
            let lists = [(header.num_ref_idx_l0_active_minus1, &header.list_entry_l0, [&before, &after], param.num_ref_idx_l0_active_minus1), (header.num_ref_idx_l1_active_minus1, &header.list_entry_l1, [&after, &before], param.num_ref_idx_l1_active_minus1)];
            for (list, (active_minus1, entries, order, given)) in lists.into_iter().enumerate().take(if header.slice_type == h265::SLICE_B { 2 } else { 1 }) {
                if given as u32 != active_minus1 {
                    return problem(format!("list {list}: {} references given, its header says {}", given as u32 + 1, active_minus1 + 1));
                }
                let initial: Vec<i32> = order.iter().flat_map(|s| s.iter().copied()).cycle().take((active_minus1 as usize + 1).max(before.len() + after.len())).collect();
                for i in 0..=active_minus1 as usize {
                    let want = initial[entries.as_ref().map_or(i, |e| e[i] as usize)];
                    let place = param.RefPicList[list][i];
                    let got = pic.ReferenceFrames.get(place as usize).filter(|r| r.flags & VA_PICTURE_HEVC_INVALID == 0).map(|r| r.pic_order_cnt);
                    if got != Some(want) {
                        return problem(format!("picture {poc}, list {list}[{i}]: place {place} ({got:?}), where {want} is wanted"));
                    }
                }
            }
        }
        units.push(nal.clone());
    }
    let sets = [h265::write_vps(&sps, 30), sps.write(), pps.write()];
    let dec = c.hevc_dec.as_mut().unwrap();
    let flat = sets.concat();
    if flat != dec.sets {
        dec.sets = flat;
        units.splice(0..0, sets);
    }
    let decoded = match dec.de265.decode(&units) {
        Ok(Some(picture)) => picture,
        Ok(None) => return problem("libde265 gave no picture for it".into()),
        Err(e) => return problem(e),
    };
    let Some(target) = s.surfaces.get_mut(&c.target) else { return VA_STATUS_ERROR_INVALID_SURFACE };
    target.poc = Some(poc);
    let (width, height, data) = decoded;
    target.put_i420(&data, width, height);
    report(|r| r.decoded += 1);
    VA_STATUS_SUCCESS
}

/// Decodes a whole H.265 stream with libde265, for tests to compare
/// with: its pictures as I420, one after another, in the order shown.
#[no_mangle]
pub unsafe extern "C" fn swarm_fakeva_decode_h265(stream: *const u8, length: usize, out: *mut u8, room: usize, pictures: *mut usize) -> c_int {
    let Some(decoder) = De265::new() else { return -1 };
    let (lib, ctx) = (decoder.lib, decoder.ctx);
    let stream = std::slice::from_raw_parts(stream, length);
    let out = std::slice::from_raw_parts_mut(out, room);
    let mut seen: Vec<&[u8]> = Vec::new();
    for unit in h264::nal_units(stream) {
        if matches!(h265::nal_type(unit), Some(h265::VPS | h265::SPS | h265::PPS)) {
            if seen.contains(&unit) {
                continue;
            }
            seen.push(unit);
        }
        (lib.push_nal)(ctx, unit.as_ptr() as *const c_void, unit.len() as c_int, 0, std::ptr::null_mut());
    }
    (lib.flush_data)(ctx);
    let (mut at, mut n) = (0, 0);
    loop {
        let mut more = 0;
        let status = (lib.decode)(ctx, &mut more);
        loop {
            let image = (lib.get_next_picture)(ctx);
            if image.is_null() {
                break;
            }
            let (_, _, picture) = decoder.i420(image);
            let Some(slot) = out.get_mut(at..at + picture.len()) else { return -2 };
            slot.copy_from_slice(&picture);
            at += picture.len();
            n += 1;
        }
        if more == 0 || (status != 0 && status != 13 && status < 1000) {
            break;
        }
    }
    *pictures = n;
    0
}

/// libx265's functions (any of a few builds: the encoder's opening is
/// named for its build).
struct X265Lib {
    param_alloc: unsafe extern "C" fn() -> *mut c_void,
    param_free: unsafe extern "C" fn(*mut c_void),
    param_default_preset: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    param_parse: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    picture_alloc: unsafe extern "C" fn() -> *mut c_void,
    picture_init: unsafe extern "C" fn(*mut c_void, *mut c_void),
    picture_free: unsafe extern "C" fn(*mut c_void),
    encoder_open: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    encoder_encode: unsafe extern "C" fn(*mut c_void, *mut *const X265Nal, *mut u32, *mut c_void, *mut c_void) -> c_int,
    encoder_close: unsafe extern "C" fn(*mut c_void),
    _lib: libloading::Library,
}

#[repr(C)]
struct X265Nal {
    kind: u32,
    size: u32,
    payload: *const u8,
}

const PICTURE_PLANES: usize = 24;
const PICTURE_STRIDES: usize = 48;
const PICTURE_TYPE: usize = 64;
const X265_TYPE_IDR: c_int = 1;

fn x265() -> Option<&'static X265Lib> {
    static LIB: std::sync::OnceLock<Option<X265Lib>> = std::sync::OnceLock::new();
    LIB.get_or_init(|| unsafe {
        for build in [215, 212, 209, 205, 199, 192] {
            let Ok(lib) = libloading::Library::new(format!("libx265.so.{build}")) else { continue };
            macro_rules! f {
                ($name:expr) => {
                    *lib.get(format!("{}\0", $name).as_bytes()).ok()?
                };
            }
            return Some(X265Lib {
                param_alloc: f!("x265_param_alloc"),
                param_free: f!("x265_param_free"),
                param_default_preset: f!("x265_param_default_preset"),
                param_parse: f!("x265_param_parse"),
                picture_alloc: f!("x265_picture_alloc"),
                picture_init: f!("x265_picture_init"),
                picture_free: f!("x265_picture_free"),
                encoder_open: f!(format!("x265_encoder_open_{build}")),
                encoder_encode: f!("x265_encoder_encode"),
                encoder_close: f!("x265_encoder_close"),
                _lib: lib,
            });
        }
        None
    })
    .as_ref()
}

/// A libx265 encoder of I420 pictures.
pub struct X265 {
    lib: &'static X265Lib,
    param: *mut c_void,
    encoder: *mut c_void,
    picture: *mut c_void,
    width: usize,
    height: usize,
}

unsafe impl Send for X265 {}

impl X265 {
    /// One for `width`×`height` at `fps` and `kbps`: P pictures on one
    /// reference, each out as it's given; `extra` sets more
    /// (`name=value:…`, x265's own).
    pub fn new(width: usize, height: usize, fps: u32, kbps: u32, extra: &str) -> Result<X265, String> {
        let lib = x265().ok_or("no libx265")?;
        unsafe {
            let param = (lib.param_alloc)();
            (lib.param_default_preset)(param, c"ultrafast".as_ptr(), c"zerolatency".as_ptr());
            let size = format!("{width}x{height}");
            let (fps, kbps) = (fps.to_string(), kbps.to_string());
            let mut settings = vec![("input-res", size.as_str()), ("fps", &fps), ("bitrate", &kbps), ("bframes", "0"), ("ref", "1"), ("keyint", "-1"), ("open-gop", "0"), ("scenecut", "0"), ("repeat-headers", "1"), ("annexb", "1"), ("info", "0"), ("weightp", "0"), ("frame-threads", "1"), ("pools", "2"), ("log-level", "none")];
            settings.extend(extra.split(':').filter(|s| !s.is_empty()).filter_map(|s| s.split_once('=')));
            for (name, value) in settings {
                let (n, v) = (CString::new(name).unwrap(), CString::new(value).unwrap());
                if (lib.param_parse)(param, n.as_ptr(), v.as_ptr()) != 0 {
                    (lib.param_free)(param);
                    return Err(format!("x265 won't take {name}={value}"));
                }
            }
            let encoder = (lib.encoder_open)(param);
            if encoder.is_null() {
                (lib.param_free)(param);
                return Err("x265 wouldn't open an encoder".into());
            }
            let picture = (lib.picture_alloc)();
            (lib.picture_init)(param, picture);
            Ok(X265 { lib, param, encoder, picture, width, height })
        }
    }

    /// Encodes a picture (`None`: flushes what's held back): the NAL
    /// units out, with their start codes.
    pub fn encode(&mut self, i420: Option<&[u8]>, idr: bool) -> Result<Vec<u8>, String> {
        unsafe {
            let input = match i420 {
                Some(i420) => {
                    let (w, h) = (self.width, self.height);
                    if i420.len() < w * h * 3 / 2 {
                        return Err("a picture too small".into());
                    }
                    let base = self.picture as *mut u8;
                    let planes = [i420.as_ptr(), i420.as_ptr().add(w * h), i420.as_ptr().add(w * h + w * h / 4)];
                    std::ptr::write_unaligned(base.add(PICTURE_PLANES) as *mut [*const u8; 3], planes);
                    std::ptr::write_unaligned(base.add(PICTURE_STRIDES) as *mut [c_int; 3], [w as c_int, (w / 2) as c_int, (w / 2) as c_int]);
                    std::ptr::write_unaligned(base.add(PICTURE_TYPE) as *mut c_int, if idr { X265_TYPE_IDR } else { 0 });
                    self.picture
                }
                None => std::ptr::null_mut(),
            };
            let (mut nals, mut count) = (std::ptr::null(), 0u32);
            if (self.lib.encoder_encode)(self.encoder, &mut nals, &mut count, input, std::ptr::null_mut()) < 0 {
                return Err("x265 couldn't encode it".into());
            }
            let mut out = Vec::new();
            for i in 0..count as usize {
                let nal = &*nals.add(i);
                out.extend_from_slice(std::slice::from_raw_parts(nal.payload, nal.size as usize));
            }
            Ok(out)
        }
    }
}

impl Drop for X265 {
    fn drop(&mut self) {
        unsafe {
            (self.lib.encoder_close)(self.encoder);
            (self.lib.picture_free)(self.picture);
            (self.lib.param_free)(self.param);
        }
    }
}

/// For tests: a libx265 encoder (see `X265::new`), or null.
#[no_mangle]
pub unsafe extern "C" fn swarm_fakeva_x265_new(width: usize, height: usize, extra: *const c_char) -> *mut X265 {
    let extra = std::ffi::CStr::from_ptr(extra).to_string_lossy();
    match X265::new(width, height, 30, 2000, &extra) {
        Ok(x) => Box::into_raw(Box::new(x)),
        Err(e) => {
            eprintln!("fakeva: {e}");
            std::ptr::null_mut()
        }
    }
}

/// For tests: encodes an I420 picture (null: flushes), the stream out
/// into `out`; its length, or -1.
#[no_mangle]
pub unsafe extern "C" fn swarm_fakeva_x265_encode(x: *mut X265, i420: *const u8, length: usize, idr: bool, out: *mut u8, room: usize) -> isize {
    let picture = (!i420.is_null()).then(|| std::slice::from_raw_parts(i420, length));
    match (*x).encode(picture, idr) {
        Ok(stream) if stream.len() <= room => {
            std::ptr::copy_nonoverlapping(stream.as_ptr(), out, stream.len());
            stream.len() as isize
        }
        _ => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn swarm_fakeva_x265_free(x: *mut X265) {
    drop(Box::from_raw(x));
}

/// What an H.265 encoding context keeps between pictures.
#[derive(Default)]
pub struct Enc {
    x265: Option<X265>,
    seq: Option<VAEncSequenceParameterBufferHEVC>,
    /// The last picture's reconstruction, and its order.
    last: Option<(VASurfaceID, i32)>,
    keys: u32,
    /// The packed parameter sets, which packed slice headers are read with.
    spss: HashMap<u32, h265::Sps>,
    ppss: HashMap<u32, h265::Pps>,
    vps: bool,
}

/// The features Mesa's driver says AMD's chips have (two bits each:
/// AMP, SAO, strong intra smoothing, constrained intra prediction,
/// transform skip, quantising by block, turning deblocking off).
pub fn mesa_features() -> u32 {
    bits(4, 2, 1) | bits(6, 2, 1) | bits(12, 2, 1) | bits(18, 2, 1) | bits(20, 2, 1) | bits(22, 2, 1) | bits(28, 2, 1)
}

/// Mesa's block sizes for AMD's chips: coding trees of 64, coding blocks
/// from 8, transforms 4 to 32, hierarchies up to 3 deep.
pub fn mesa_block_sizes() -> u32 {
    bits(0, 2, 3) | bits(2, 2, 3) | bits(4, 2, 0) | bits(6, 2, 3) | bits(8, 2, 0) | bits(10, 2, 3) | bits(14, 2, 3)
}

/// Mesa's surface alignment for AMD's encoders: 64 across, 16 down.
pub const MESA_ALIGNMENT: u32 = 6 | (4 << 4);

pub fn encode(s: &mut State, c: &mut Context, packed: u32) -> VAStatus {
    let (mut seq, mut pic, mut slices, mut rate, mut headers) = (None, None, Vec::new(), None, Vec::new());
    let mut header_kind = None;
    for id in &c.pending {
        let Some((kind, bytes)) = plain(s, id) else { continue };
        match kind {
            VAEncSequenceParameterBufferType => seq = VAEncSequenceParameterBufferHEVC::read(bytes),
            VAEncPictureParameterBufferType => pic = VAEncPictureParameterBufferHEVC::read(bytes),
            VAEncSliceParameterBufferType => slices.extend(VAEncSliceParameterBufferHEVC::read(bytes)),
            VAEncMiscParameterBufferType => {
                if i32::from_ne_bytes(bytes[..4].try_into().unwrap()) == VAEncMiscParameterTypeRateControl {
                    rate = VAEncMiscParameterRateControl::read(&bytes[4..]);
                }
            }
            VAEncPackedHeaderParameterBufferType => header_kind = VAEncPackedHeaderParameterBuffer::read(bytes),
            VAEncPackedHeaderDataBufferType => {
                if let Some(h) = header_kind.take() {
                    headers.push((h, bytes.to_vec()));
                }
            }
            _ => {}
        }
    }
    let enc = c.hevc_enc.get_or_insert_with(Enc::default);
    let Some(pic) = pic else { return problem("a picture to encode without its picture parameters".into()) };
    let f = pic.pic_fields;
    let idr = bits_of(f, 0, 1) == 1;
    if idr && seq.is_none() {
        return problem("a full frame without the sequence parameters".into());
    }
    if let Some(seq) = seq {
        enc.seq = Some(seq);
    }
    let Some(seq) = enc.seq else { return problem("encoding before any sequence parameters".into()) };
    if idr && packed != 0 && !headers.iter().any(|(h, _)| h.type_ == VAEncPackedHeaderSequence) {
        return problem("a full frame without the packed headers set up for".into());
    }
    if enc.last.is_none() && !idr {
        return problem("the first picture not a full frame".into());
    }
    if enc.last.is_none() && rate.is_none() {
        return problem("no rate control with the first picture".into());
    }
    if let Some(rate) = rate {
        report(|r| r.bitrates.push(rate.bits_per_second));
    }
    let (want_type, want_kind) = if idr { (1, [h265::IDR_W_RADL, h265::IDR_N_LP]) } else { (2, [h265::TRAIL_R, h265::TRAIL_R]) };
    if bits_of(f, 1, 3) != want_type || !want_kind.contains(&pic.nal_unit_type) || bits_of(f, 4, 1) != 1 {
        return problem(format!("a picture full {idr} of coding type {}, NAL type {}, reference {}", bits_of(f, 1, 3), pic.nal_unit_type, bits_of(f, 4, 1)));
    }
    let recon = pic.decoded_curr_pic.picture_id;
    if recon == c.target || !s.surfaces.contains_key(&recon) {
        return problem(format!("reconstructing into {recon}: the input, or no surface"));
    }
    let poc = pic.decoded_curr_pic.pic_order_cnt;
    match (idr, enc.last) {
        (true, _) if poc != 0 => return problem(format!("a full frame of order {poc}")),
        (false, Some((last, last_poc))) => {
            if poc != last_poc + 1 {
                return problem(format!("order {poc} after {last_poc}"));
            }
            let reference = pic.reference_frames[0];
            if reference.picture_id != last || reference.pic_order_cnt != last_poc || pic.reference_frames[1].flags & VA_PICTURE_HEVC_INVALID == 0 {
                return problem(format!("references {:?}, not the last picture ({last}, {last_poc})", &pic.reference_frames[..2]));
            }
            if slices.iter().any(|s| s.ref_pic_list0[0].picture_id != last || s.ref_pic_list0[1].flags & VA_PICTURE_HEVC_INVALID == 0) {
                return problem("a P slice not referring to the last picture alone".into());
            }
        }
        _ => {}
    }
    let tmvp = bits_of(seq.seq_fields, 15, 1) == 1;
    if (pic.collocated_ref_pic_index == 0xff) == tmvp {
        return problem(format!("collocated_ref_pic_index {} with temporal motion vectors {tmvp}", pic.collocated_ref_pic_index));
    }
    let log2_ctb = seq.log2_min_luma_coding_block_size_minus3 as u32 + 3 + seq.log2_diff_max_min_luma_coding_block_size as u32;
    let ctbs = (seq.pic_width_in_luma_samples as u32).div_ceil(1 << log2_ctb) * (seq.pic_height_in_luma_samples as u32).div_ceil(1 << log2_ctb);
    if slices.is_empty() || slices.iter().map(|s| s.num_ctu_in_slice).sum::<u32>() != ctbs {
        return problem(format!("slices not covering the picture's {ctbs} coding tree blocks"));
    }
    if slices.iter().any(|s| s.slice_type as u32 != if idr { h265::SLICE_I } else { h265::SLICE_P }) {
        return problem("a slice of the wrong type".into());
    }
    if mesa() && (seq.log2_min_luma_coding_block_size_minus3 != 0 || log2_ctb != 6) {
        return problem(format!("blocks from {} in trees of {}, where the driver said 8 and 64", 8 << seq.log2_min_luma_coding_block_size_minus3, 1 << log2_ctb));
    }
    let mut slice_header = None;
    for (header, data) in &headers {
        if let Some(path) = switch("FAKEVA_DUMP_HEADERS") {
            static DUMPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = DUMPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let _ = std::fs::write(format!("{path}.{n}"), data);
        }
        if header.bit_length as usize != data.len() * 8 || header.has_emulation_bytes != 1 {
            return problem(format!("a packed header of {} bits in {} bytes", header.bit_length, data.len()));
        }
        if header.type_ == VAEncPackedHeaderSlice {
            if packed & VA_ENC_PACKED_HEADER_SLICE == 0 {
                return problem("a packed slice header not set up for".into());
            }
            let units = h264::nal_units(data);
            let [unit] = units[..] else { return problem("a packed slice header not one NAL unit".into()) };
            let Some(parsed) = h265::SliceHeader::parse(&[unit, &[0x80]].concat(), &enc.spss, &enc.ppss) else { return problem("a packed slice header that doesn't read".into()) };
            if parsed.header_bytes != h264::unescape(unit).len() {
                return problem("a packed slice header with more after it".into());
            }
            let Some(sps) = enc.ppss.get(&parsed.pps_id).and_then(|p| enc.spss.get(&p.sps_id)) else { return problem("no SPS".into()) };
            let slice = &slices[0];
            let sf = slice.slice_fields;
            let rps_ok = if idr { true } else { parsed.rps(sps) == &h265::Rps { negative: vec![(-1, true)], positive: vec![] } };
            if parsed.nal_type != pic.nal_unit_type || parsed.slice_type != slice.slice_type as u32 || parsed.poc_lsb != poc as u32 % sps.max_poc_lsb() || !rps_ok || parsed.temporal_mvp != (bits_of(sf, 4, 1) == 1) || parsed.sao_luma != (bits_of(sf, 5, 1) == 1) || parsed.sao_chroma != (bits_of(sf, 6, 1) == 1) || parsed.qp_delta != slice.slice_qp_delta as i32 || 5 - parsed.five_minus_max_num_merge_cand != slice.max_num_merge_cand as u32 {
                return problem(format!("a packed slice header unlike the parameters: {parsed:?}"));
            }
            slice_header = Some(parsed);
            continue;
        }
        if header.type_ != VAEncPackedHeaderSequence {
            return problem(format!("a packed header of type {}", header.type_));
        }
        for unit in h264::nal_units(data) {
            match h265::nal_type(unit) {
                Some(h265::VPS) => enc.vps = true,
                Some(h265::SPS) => {
                    let Some(sps) = h265::Sps::parse(unit) else { return problem("a packed SPS that doesn't read".into()) };
                    let sf = seq.seq_fields;
                    if sps.width != seq.pic_width_in_luma_samples as u32 || sps.height != seq.pic_height_in_luma_samples as u32 || sps.log2_min_cb_minus3 != seq.log2_min_luma_coding_block_size_minus3 as u32 || sps.log2_diff_max_min_cb != seq.log2_diff_max_min_luma_coding_block_size as u32 || sps.log2_min_tb_minus2 != seq.log2_min_transform_block_size_minus2 as u32 || sps.log2_diff_max_min_tb != seq.log2_diff_max_min_transform_block_size as u32 || sps.amp != (bits_of(sf, 11, 1) == 1) || sps.sao != (bits_of(sf, 12, 1) == 1) || sps.temporal_mvp != tmvp || sps.ptl.level_idc != seq.general_level_idc || sps.ptl.profile_idc != seq.general_profile_idc {
                        return problem(format!("a packed SPS unlike the sequence parameters: {sps:?}"));
                    }
                    if mesa() && (sps.width % 64 != 0 && sps.width.div_ceil(64) * 64 - sps.width > 62 || sps.height % 16 != 0) {
                        return problem(format!("a picture {}×{}, where surfaces are aligned to 64 across and 16 down", sps.width, sps.height));
                    }
                    enc.spss.insert(sps.id, sps);
                    report(|r| r.packed += 1);
                }
                Some(h265::PPS) => {
                    let Some(pps) = h265::Pps::parse(unit) else { return problem("a packed PPS that doesn't read".into()) };
                    if pps.cu_qp_delta_enabled != (bits_of(f, 9, 1) == 1) || pps.transform_skip != (bits_of(f, 8, 1) == 1) || pps.diff_cu_qp_delta_depth != pic.diff_cu_qp_delta_depth as u32 || pps.init_qp_minus26 + 26 != pic.pic_init_qp as i32 || pps.loop_filter_across_slices != (bits_of(f, 16, 1) == 1) {
                        return problem(format!("a packed PPS unlike the picture parameters: {pps:?}"));
                    }
                    enc.ppss.insert(pps.id, pps);
                }
                kind => return problem(format!("NAL unit {kind:?} in a packed sequence header")),
            }
        }
        if !enc.vps {
            return problem("packed parameter sets without a VPS".into());
        }
    }
    if slice_header.is_none() && packed & VA_ENC_PACKED_HEADER_SLICE != 0 {
        return problem("no packed slice header, though set up for".into());
    }
    let coded = (seq.pic_width_in_luma_samples as usize, seq.pic_height_in_luma_samples as usize);
    let (width, height) = enc.spss.values().next().and_then(|s| s.shown()).map_or(coded, |(w, h, _, _)| (w, h));
    let Some(source) = s.surfaces.get(&c.target) else { return VA_STATUS_ERROR_INVALID_SURFACE };
    if source.width < width || source.height < height {
        return problem(format!("an input surface {}×{} for pictures {width}×{height}", source.width, source.height));
    }
    let i420 = source.i420(width, height);
    if enc.x265.is_none() {
        let bitrate = rate.map_or(seq.bits_per_second, |r| r.bits_per_second);
        match X265::new(width, height, 30, (bitrate / 1000).max(100), "") {
            Ok(x) => enc.x265 = Some(x),
            Err(e) => return problem(e),
        }
    }
    let mut frame = match enc.x265.as_mut().unwrap().encode(Some(&i420), idr) {
        Ok(frame) => frame,
        Err(e) => return problem(e),
    };
    if idr {
        enc.keys += 1;
    }
    let sets_only_first = switch("FAKEVA_HEADERS").as_deref() == Some("first") && enc.keys > 1;
    let no_sets = mesa() && !headers.iter().any(|(h, _)| h.type_ == VAEncPackedHeaderSequence);
    if sets_only_first || no_sets {
        frame = h264::nal_units(&frame).into_iter().filter(|n| !matches!(h265::nal_type(n), Some(h265::VPS | h265::SPS | h265::PPS))).flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect();
    }
    enc.last = Some((recon, poc));
    report(|r| {
        if idr {
            r.idrs.push(r.frames);
        }
        r.frames += 1;
    });
    let Some(Buffer { data: Data::Coded(bytes, segment), .. }) = s.buffers.get_mut(&pic.coded_buf) else { return problem("no coded buffer".into()) };
    if frame.len() > bytes.len() {
        return problem("a frame bigger than its coded buffer".into());
    }
    bytes[..frame.len()].copy_from_slice(&frame);
    segment.size = frame.len() as u32;
    VA_STATUS_SUCCESS
}
