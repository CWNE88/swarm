//! libva's types, as its headers (2.x) lay them out: only what swarm
//! uses. Sizes are checked against what a C compiler made of libva
//! 2.17's headers. Self-contained (std only): the stand-in VA-API driver
//! used in tests includes it too.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code)]

use std::ffi::c_void;

pub type VADisplay = *mut c_void;
pub type VAStatus = i32;
pub type VAGenericID = u32;
pub type VAConfigID = VAGenericID;
pub type VAContextID = VAGenericID;
pub type VASurfaceID = VAGenericID;
pub type VABufferID = VAGenericID;
pub type VAImageID = VAGenericID;

pub const VA_STATUS_SUCCESS: VAStatus = 0;
pub const VA_STATUS_ERROR_OPERATION_FAILED: VAStatus = 1;
pub const VA_STATUS_ERROR_ALLOCATION_FAILED: VAStatus = 2;
pub const VA_STATUS_ERROR_INVALID_CONFIG: VAStatus = 4;
pub const VA_STATUS_ERROR_INVALID_CONTEXT: VAStatus = 5;
pub const VA_STATUS_ERROR_INVALID_SURFACE: VAStatus = 6;
pub const VA_STATUS_ERROR_INVALID_BUFFER: VAStatus = 7;
pub const VA_STATUS_ERROR_INVALID_IMAGE: VAStatus = 8;
pub const VA_STATUS_ERROR_UNSUPPORTED_PROFILE: VAStatus = 12;
pub const VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT: VAStatus = 13;
pub const VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT: VAStatus = 14;
pub const VA_STATUS_ERROR_INVALID_PARAMETER: VAStatus = 18;
pub const VA_STATUS_ERROR_UNIMPLEMENTED: VAStatus = 20;
pub const VA_STATUS_ERROR_INVALID_VALUE: VAStatus = 25;

pub const VA_INVALID_ID: u32 = 0xffff_ffff;
pub const VA_INVALID_SURFACE: VASurfaceID = VA_INVALID_ID;

pub const VAProfileH264Main: i32 = 6;
pub const VAProfileH264High: i32 = 7;
pub const VAProfileJPEGBaseline: i32 = 12;
pub const VAProfileH264ConstrainedBaseline: i32 = 13;
pub const VAProfileHEVCMain: i32 = 17;

pub const VAEntrypointVLD: i32 = 1;
pub const VAEntrypointEncSlice: i32 = 6;
pub const VAEntrypointEncSliceLP: i32 = 8;

pub const VAConfigAttribRTFormat: i32 = 0;
pub const VAConfigAttribRateControl: i32 = 5;
pub const VAConfigAttribEncPackedHeaders: i32 = 10;
pub const VAConfigAttribEncMaxRefFrames: i32 = 13;
pub const VAConfigAttribEncHEVCFeatures: i32 = 50;
pub const VAConfigAttribEncHEVCBlockSizes: i32 = 51;
pub const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
pub const VA_RT_FORMAT_YUV420: u32 = 1;
pub const VA_RT_FORMAT_YUV422: u32 = 2;
pub const VA_RT_FORMAT_YUV444: u32 = 4;
pub const VA_RC_CBR: u32 = 2;
pub const VA_RC_VBR: u32 = 4;
pub const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 1;
pub const VA_ENC_PACKED_HEADER_PICTURE: u32 = 2;
pub const VA_ENC_PACKED_HEADER_SLICE: u32 = 4;
pub const VA_ENC_PACKED_HEADER_MISC: u32 = 8;
pub const VA_ENC_PACKED_HEADER_RAW_DATA: u32 = 0x10;

pub const VAPictureParameterBufferType: i32 = 0;
pub const VAIQMatrixBufferType: i32 = 1;
pub const VASliceParameterBufferType: i32 = 4;
pub const VASliceDataBufferType: i32 = 5;
pub const VAImageBufferType: i32 = 9;
pub const VAHuffmanTableBufferType: i32 = 12;
pub const VAEncCodedBufferType: i32 = 21;
pub const VAEncSequenceParameterBufferType: i32 = 22;
pub const VAEncPictureParameterBufferType: i32 = 23;
pub const VAEncSliceParameterBufferType: i32 = 24;
pub const VAEncPackedHeaderParameterBufferType: i32 = 25;
pub const VAEncPackedHeaderDataBufferType: i32 = 26;
pub const VAEncMiscParameterBufferType: i32 = 27;

pub const VAEncMiscParameterTypeFrameRate: i32 = 0;
pub const VAEncMiscParameterTypeRateControl: i32 = 1;
pub const VAEncMiscParameterTypeHRD: i32 = 5;

pub const VAEncPackedHeaderSequence: u32 = 1;
pub const VAEncPackedHeaderPicture: u32 = 2;
pub const VAEncPackedHeaderSlice: u32 = 3;

pub const VA_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const VA_FOURCC_YUY2: u32 = u32::from_le_bytes(*b"YUY2");
pub const VA_FOURCC_UYVY: u32 = u32::from_le_bytes(*b"UYVY");
pub const VA_FOURCC_I420: u32 = u32::from_le_bytes(*b"I420");
pub const VA_FOURCC_YV12: u32 = u32::from_le_bytes(*b"YV12");
pub const VA_FOURCC_422H: u32 = u32::from_le_bytes(*b"422H");
pub const VA_FOURCC_444P: u32 = u32::from_le_bytes(*b"444P");
pub const VA_LSB_FIRST: u32 = 1;
pub const VA_PROGRESSIVE: i32 = 1;
pub const VASurfaceAttribAlignmentSize: i32 = 10;
pub const VA_SLICE_DATA_FLAG_ALL: u32 = 0;

pub const VA_PICTURE_H264_INVALID: u32 = 0x01;
pub const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x08;

pub const VA_PICTURE_HEVC_INVALID: u32 = 0x01;
pub const VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE: u32 = 0x10;
pub const VA_PICTURE_HEVC_RPS_ST_CURR_AFTER: u32 = 0x20;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VAConfigAttrib {
    pub type_: i32,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VAImage {
    pub image_id: VAImageID,
    pub format: VAImageFormat,
    pub buf: VABufferID,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VACodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut c_void,
    pub next: *mut c_void,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VAPictureH264 {
    pub picture_id: VASurfaceID,
    pub frame_idx: u32,
    pub flags: u32,
    pub TopFieldOrderCnt: i32,
    pub BottomFieldOrderCnt: i32,
    pub va_reserved: [u32; 4],
}

impl VAPictureH264 {
    /// A slot with no picture in it.
    pub const INVALID: VAPictureH264 = VAPictureH264 { picture_id: VA_INVALID_SURFACE, frame_idx: 0, flags: VA_PICTURE_H264_INVALID, TopFieldOrderCnt: 0, BottomFieldOrderCnt: 0, va_reserved: [0; 4] };
}

/// Puts `value` in `width` bits from bit `at` (from the least
/// significant, as GCC lays out bitfields here).
pub fn bits(at: u32, width: u32, value: u32) -> u32 {
    (value & ((1u64 << width) - 1) as u32) << at
}

/// Takes the `width` bits from bit `at` out of `field`.
pub fn bits_of(field: u32, at: u32, width: u32) -> u32 {
    (field >> at) & ((1u64 << width) - 1) as u32
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureParameterBufferH264 {
    pub CurrPic: VAPictureH264,
    pub ReferenceFrames: [VAPictureH264; 16],
    pub picture_width_in_mbs_minus1: u16,
    pub picture_height_in_mbs_minus1: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames: u8,
    /// chroma_format_idc:2, residual_colour_transform_flag:1,
    /// gaps_in_frame_num_value_allowed_flag:1, frame_mbs_only_flag:1,
    /// mb_adaptive_frame_field_flag:1, direct_8x8_inference_flag:1,
    /// MinLumaBiPredSize8x8:1, log2_max_frame_num_minus4:4,
    /// pic_order_cnt_type:2, log2_max_pic_order_cnt_lsb_minus4:4,
    /// delta_pic_order_always_zero_flag:1.
    pub seq_fields: u32,
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub slice_group_change_rate_minus1: u16,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    /// entropy_coding_mode_flag:1, weighted_pred_flag:1,
    /// weighted_bipred_idc:2, transform_8x8_mode_flag:1,
    /// field_pic_flag:1, constrained_intra_pred_flag:1,
    /// pic_order_present_flag:1, deblocking_filter_control_present_flag:1,
    /// redundant_pic_cnt_present_flag:1, reference_pic_flag:1.
    pub pic_fields: u32,
    pub frame_num: u16,
    pub va_reserved: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAIQMatrixBufferH264 {
    pub ScalingList4x4: [[u8; 16]; 6],
    pub ScalingList8x8: [[u8; 64]; 2],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASliceParameterBufferH264 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_bit_offset: u16,
    pub first_mb_in_slice: u16,
    pub slice_type: u8,
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub RefPicList0: [VAPictureH264; 32],
    pub RefPicList1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    /// chroma_format_idc:2, frame_mbs_only_flag:1,
    /// mb_adaptive_frame_field_flag:1, seq_scaling_matrix_present_flag:1,
    /// direct_8x8_inference_flag:1, log2_max_frame_num_minus4:4,
    /// pic_order_cnt_type:2, log2_max_pic_order_cnt_lsb_minus4:4,
    /// delta_pic_order_always_zero_flag:1.
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    /// aspect_ratio_info_present_flag:1, timing_info_present_flag:1,
    /// bitstream_restriction_flag:1, log2_max_mv_length_horizontal:5,
    /// log2_max_mv_length_vertical:5, fixed_frame_rate_flag:1,
    /// low_delay_hrd_flag:1, motion_vectors_over_pic_boundaries_flag:1.
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferH264 {
    pub CurrPic: VAPictureH264,
    pub ReferenceFrames: [VAPictureH264; 16],
    pub coded_buf: VABufferID,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    /// idr_pic_flag:1, reference_pic_flag:2, entropy_coding_mode_flag:1,
    /// weighted_pred_flag:1, weighted_bipred_idc:2,
    /// constrained_intra_pred_flag:1, transform_8x8_mode_flag:1,
    /// deblocking_filter_control_present_flag:1,
    /// redundant_pic_cnt_present_flag:1, pic_order_present_flag:1,
    /// pic_scaling_matrix_present_flag:1.
    pub pic_fields: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    pub macroblock_info: VABufferID,
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub RefPicList0: [VAPictureH264; 32],
    pub RefPicList1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAEncMiscParameterRateControl {
    pub bits_per_second: u32,
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    /// reset:1, disable_frame_skip:1, disable_bit_stuffing:1, ...
    pub rc_flags: u32,
    pub ICQ_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAEncMiscParameterFrameRate {
    /// Numerator in the low 16 bits, denominator (0 for 1) in the high.
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAEncMiscParameterHRD {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAEncPackedHeaderParameterBuffer {
    pub type_: u32,
    pub bit_length: u32,
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAJPEGComponent {
    pub component_id: u8,
    pub h_sampling_factor: u8,
    pub v_sampling_factor: u8,
    pub quantiser_table_selector: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureParameterBufferJPEGBaseline {
    pub picture_width: u16,
    pub picture_height: u16,
    pub components: [VAJPEGComponent; 255],
    pub num_components: u8,
    pub color_space: u8,
    pub rotation: u32,
    pub va_reserved: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAIQMatrixBufferJPEGBaseline {
    pub load_quantiser_table: [u8; 4],
    /// In the file's (zig-zag) order.
    pub quantiser_table: [[u8; 64]; 4],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAJPEGHuffmanTable {
    pub num_dc_codes: [u8; 16],
    pub dc_values: [u8; 12],
    pub num_ac_codes: [u8; 16],
    pub ac_values: [u8; 162],
    pub pad: [u8; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAHuffmanTableBufferJPEGBaseline {
    pub load_huffman_table: [u8; 2],
    pub huffman_table: [VAJPEGHuffmanTable; 2],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAJPEGScanComponent {
    pub component_selector: u8,
    pub dc_table_selector: u8,
    pub ac_table_selector: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASliceParameterBufferJPEGBaseline {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_horizontal_position: u32,
    pub slice_vertical_position: u32,
    pub components: [VAJPEGScanComponent; 4],
    pub num_components: u8,
    pub restart_interval: u16,
    pub num_mcus: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VAPictureHEVC {
    pub picture_id: VASurfaceID,
    pub pic_order_cnt: i32,
    pub flags: u32,
    pub va_reserved: [u32; 4],
}

impl VAPictureHEVC {
    /// A slot with no picture in it.
    pub const INVALID: VAPictureHEVC = VAPictureHEVC { picture_id: VA_INVALID_SURFACE, pic_order_cnt: 0, flags: VA_PICTURE_HEVC_INVALID, va_reserved: [0; 4] };
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureParameterBufferHEVC {
    pub CurrPic: VAPictureHEVC,
    pub ReferenceFrames: [VAPictureHEVC; 15],
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    /// chroma_format_idc:2, separate_colour_plane_flag:1,
    /// pcm_enabled_flag:1, scaling_list_enabled_flag:1,
    /// transform_skip_enabled_flag:1, amp_enabled_flag:1,
    /// strong_intra_smoothing_enabled_flag:1,
    /// sign_data_hiding_enabled_flag:1, constrained_intra_pred_flag:1,
    /// cu_qp_delta_enabled_flag:1, weighted_pred_flag:1,
    /// weighted_bipred_flag:1, transquant_bypass_enabled_flag:1,
    /// tiles_enabled_flag:1, entropy_coding_sync_enabled_flag:1,
    /// pps_loop_filter_across_slices_enabled_flag:1,
    /// loop_filter_across_tiles_enabled_flag:1,
    /// pcm_loop_filter_disabled_flag:1, NoPicReorderingFlag:1,
    /// NoBiPredFlag:1.
    pub pic_fields: u32,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub init_qp_minus26: i8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    /// lists_modification_present_flag:1,
    /// long_term_ref_pics_present_flag:1,
    /// sps_temporal_mvp_enabled_flag:1, cabac_init_present_flag:1,
    /// output_flag_present_flag:1,
    /// dependent_slice_segments_enabled_flag:1,
    /// pps_slice_chroma_qp_offsets_present_flag:1,
    /// sample_adaptive_offset_enabled_flag:1,
    /// deblocking_filter_override_enabled_flag:1,
    /// pps_disable_deblocking_filter_flag:1,
    /// slice_segment_header_extension_present_flag:1, RapPicFlag:1,
    /// IdrPicFlag:1, IntraPicFlag:1.
    pub slice_parsing_fields: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pic_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub num_extra_slice_header_bits: u8,
    pub st_rps_bits: u32,
    pub va_reserved: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASliceParameterBufferHEVC {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_byte_offset: u32,
    pub slice_segment_address: u32,
    /// Places in `ReferenceFrames` (0xff for none).
    pub RefPicList: [[u8; 15]; 2],
    /// LastSliceOfPic:1, dependent_slice_segment_flag:1, slice_type:2,
    /// color_plane_id:2, slice_sao_luma_flag:1, slice_sao_chroma_flag:1,
    /// mvd_l1_zero_flag:1, cabac_init_flag:1,
    /// slice_temporal_mvp_enabled_flag:1,
    /// slice_deblocking_filter_disabled_flag:1, collocated_from_l0_flag:1,
    /// slice_loop_filter_across_slices_enabled_flag:1.
    pub LongSliceFlags: u32,
    pub collocated_ref_idx: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub ChromaOffsetL0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub ChromaOffsetL1: [[i8; 2]; 15],
    pub five_minus_max_num_merge_cand: u8,
    pub num_entry_point_offsets: u16,
    pub entry_offset_to_subset_array: u16,
    pub slice_data_num_emu_prevn_bytes: u16,
    pub va_reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferHEVC {
    pub general_profile_idc: u8,
    pub general_level_idc: u8,
    pub general_tier_flag: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    /// chroma_format_idc:2, separate_colour_plane_flag:1,
    /// bit_depth_luma_minus8:3, bit_depth_chroma_minus8:3,
    /// scaling_list_enabled_flag:1,
    /// strong_intra_smoothing_enabled_flag:1, amp_enabled_flag:1,
    /// sample_adaptive_offset_enabled_flag:1, pcm_enabled_flag:1,
    /// pcm_loop_filter_disabled_flag:1,
    /// sps_temporal_mvp_enabled_flag:1, low_delay_seq:1,
    /// hierachical_flag:1.
    pub seq_fields: u32,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub pcm_sample_bit_depth_luma_minus1: u32,
    pub pcm_sample_bit_depth_chroma_minus1: u32,
    pub log2_min_pcm_luma_coding_block_size_minus3: u32,
    pub log2_max_pcm_luma_coding_block_size_minus3: u32,
    pub vui_parameters_present_flag: u8,
    /// aspect_ratio_info_present_flag:1,
    /// neutral_chroma_indication_flag:1, field_seq_flag:1,
    /// vui_timing_info_present_flag:1, bitstream_restriction_flag:1,
    /// tiles_fixed_structure_flag:1,
    /// motion_vectors_over_pic_boundaries_flag:1,
    /// restricted_ref_pic_lists_flag:1, log2_max_mv_length_horizontal:5,
    /// log2_max_mv_length_vertical:5.
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub vui_num_units_in_tick: u32,
    pub vui_time_scale: u32,
    pub min_spatial_segmentation_idc: u16,
    pub max_bytes_per_pic_denom: u8,
    pub max_bits_per_min_cu_denom: u8,
    pub scc_fields: u32,
    pub va_reserved: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferHEVC {
    pub decoded_curr_pic: VAPictureHEVC,
    pub reference_frames: [VAPictureHEVC; 15],
    pub coded_buf: VABufferID,
    pub collocated_ref_pic_index: u8,
    pub last_picture: u8,
    pub pic_init_qp: u8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u8; 19],
    pub row_height_minus1: [u8; 21],
    pub log2_parallel_merge_level_minus2: u8,
    pub ctu_max_bitsize_allowed: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub slice_pic_parameter_set_id: u8,
    pub nal_unit_type: u8,
    /// idr_pic_flag:1, coding_type:3, reference_pic_flag:1,
    /// dependent_slice_segments_enabled_flag:1,
    /// sign_data_hiding_enabled_flag:1, constrained_intra_pred_flag:1,
    /// transform_skip_enabled_flag:1, cu_qp_delta_enabled_flag:1,
    /// weighted_pred_flag:1, weighted_bipred_flag:1,
    /// transquant_bypass_enabled_flag:1, tiles_enabled_flag:1,
    /// entropy_coding_sync_enabled_flag:1,
    /// loop_filter_across_tiles_enabled_flag:1,
    /// pps_loop_filter_across_slices_enabled_flag:1,
    /// scaling_list_data_present_flag:1, screen_content_flag:1,
    /// enable_gpu_weighted_prediction:1, no_output_of_prior_pics_flag:1.
    pub pic_fields: u32,
    pub hierarchical_level_plus1: u8,
    pub va_byte_reserved: u8,
    pub scc_fields: u16,
    pub va_reserved: [u32; 15],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSliceParameterBufferHEVC {
    pub slice_segment_address: u32,
    pub num_ctu_in_slice: u32,
    pub slice_type: u8,
    pub slice_pic_parameter_set_id: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list0: [VAPictureHEVC; 15],
    pub ref_pic_list1: [VAPictureHEVC; 15],
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub chroma_offset_l0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub chroma_offset_l1: [[i8; 2]; 15],
    pub max_num_merge_cand: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    /// last_slice_of_pic_flag:1, dependent_slice_segment_flag:1,
    /// colour_plane_id:2, slice_temporal_mvp_enabled_flag:1,
    /// slice_sao_luma_flag:1, slice_sao_chroma_flag:1,
    /// num_ref_idx_active_override_flag:1, mvd_l1_zero_flag:1,
    /// cabac_init_flag:1, slice_deblocking_filter_disabled_flag:2,
    /// slice_loop_filter_across_slices_enabled_flag:1,
    /// collocated_from_l0_flag:1.
    pub slice_fields: u32,
    pub pred_weight_table_bit_offset: u32,
    pub pred_weight_table_bit_length: u32,
    pub va_reserved: [u32; 6],
}

/// A surface attribute (VASurfaceAttrib): its type, flags, and value
/// (VAGenericValue: its type, then an integer, float or pointer).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VASurfaceAttrib {
    pub type_: i32,
    pub flags: u32,
    pub value_type: i32,
    pub value: [u32; 3],
}

impl VASurfaceAttrib {
    /// Its value as an integer.
    pub fn int(&self) -> i32 {
        self.value[1] as i32
    }
}

/// Plain data that can be zeroed and seen as bytes.
///
/// # Safety
/// Only for `repr(C)` structs of integers (and arrays and structs of
/// them), for which all zeros is a value.
pub unsafe trait Plain: Copy {
    fn zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self as *const Self as *const u8, std::mem::size_of::<Self>()) }
    }

    /// One from the start of `bytes`, if they're long enough.
    fn read(bytes: &[u8]) -> Option<Self> {
        (bytes.len() >= std::mem::size_of::<Self>()).then(|| unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const Self) })
    }
}

unsafe impl Plain for VAPictureParameterBufferH264 {}
unsafe impl Plain for VAIQMatrixBufferH264 {}
unsafe impl Plain for VASliceParameterBufferH264 {}
unsafe impl Plain for VAEncSequenceParameterBufferH264 {}
unsafe impl Plain for VAEncPictureParameterBufferH264 {}
unsafe impl Plain for VAEncSliceParameterBufferH264 {}
unsafe impl Plain for VAEncMiscParameterRateControl {}
unsafe impl Plain for VAEncMiscParameterFrameRate {}
unsafe impl Plain for VAEncMiscParameterHRD {}
unsafe impl Plain for VAEncPackedHeaderParameterBuffer {}
unsafe impl Plain for VAPictureParameterBufferJPEGBaseline {}
unsafe impl Plain for VAIQMatrixBufferJPEGBaseline {}
unsafe impl Plain for VAHuffmanTableBufferJPEGBaseline {}
unsafe impl Plain for VASliceParameterBufferJPEGBaseline {}
unsafe impl Plain for VAPictureParameterBufferHEVC {}
unsafe impl Plain for VASliceParameterBufferHEVC {}
unsafe impl Plain for VAEncSequenceParameterBufferHEVC {}
unsafe impl Plain for VAEncPictureParameterBufferHEVC {}
unsafe impl Plain for VAEncSliceParameterBufferHEVC {}

/// A misc parameter buffer's contents: its type, then the parameter.
pub fn misc<T: Plain>(kind: i32, parameter: &T) -> Vec<u8> {
    let mut out = kind.to_ne_bytes().to_vec();
    out.extend_from_slice(parameter.as_bytes());
    out
}

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<VAPictureH264>() == 36);
    assert!(size_of::<VAPictureParameterBufferH264>() == 672);
    assert!(offset_of!(VAPictureParameterBufferH264, picture_width_in_mbs_minus1) == 612);
    assert!(offset_of!(VAPictureParameterBufferH264, seq_fields) == 620);
    assert!(offset_of!(VAPictureParameterBufferH264, pic_init_qp_minus26) == 628);
    assert!(offset_of!(VAPictureParameterBufferH264, pic_fields) == 632);
    assert!(offset_of!(VAPictureParameterBufferH264, frame_num) == 636);
    assert!(size_of::<VAIQMatrixBufferH264>() == 240);
    assert!(size_of::<VASliceParameterBufferH264>() == 3128);
    assert!(offset_of!(VASliceParameterBufferH264, slice_data_bit_offset) == 12);
    assert!(offset_of!(VASliceParameterBufferH264, slice_type) == 16);
    assert!(offset_of!(VASliceParameterBufferH264, slice_beta_offset_div2) == 24);
    assert!(offset_of!(VASliceParameterBufferH264, RefPicList0) == 28);
    assert!(offset_of!(VASliceParameterBufferH264, luma_log2_weight_denom) == 2332);
    assert!(offset_of!(VASliceParameterBufferH264, luma_weight_l0) == 2336);
    assert!(offset_of!(VASliceParameterBufferH264, chroma_weight_l0_flag) == 2464);
    assert!(offset_of!(VASliceParameterBufferH264, chroma_weight_l0) == 2466);
    assert!(offset_of!(VASliceParameterBufferH264, luma_weight_l1_flag) == 2722);
    assert!(offset_of!(VASliceParameterBufferH264, va_reserved) == 3112);
    assert!(size_of::<VAEncSequenceParameterBufferH264>() == 1132);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, intra_period) == 4);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, picture_width_in_mbs) == 24);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, seq_fields) == 28);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, bit_depth_luma_minus8) == 32);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, num_ref_frames_in_pic_order_cnt_cycle) == 34);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, offset_for_non_ref_pic) == 36);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, frame_cropping_flag) == 1068);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, frame_crop_left_offset) == 1072);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, vui_parameters_present_flag) == 1088);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, vui_fields) == 1092);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, aspect_ratio_idc) == 1096);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, sar_width) == 1100);
    assert!(offset_of!(VAEncSequenceParameterBufferH264, time_scale) == 1112);
    assert!(size_of::<VAEncPictureParameterBufferH264>() == 648);
    assert!(offset_of!(VAEncPictureParameterBufferH264, coded_buf) == 612);
    assert!(offset_of!(VAEncPictureParameterBufferH264, pic_parameter_set_id) == 616);
    assert!(offset_of!(VAEncPictureParameterBufferH264, last_picture) == 618);
    assert!(offset_of!(VAEncPictureParameterBufferH264, frame_num) == 620);
    assert!(offset_of!(VAEncPictureParameterBufferH264, pic_init_qp) == 622);
    assert!(offset_of!(VAEncPictureParameterBufferH264, chroma_qp_index_offset) == 625);
    assert!(offset_of!(VAEncPictureParameterBufferH264, pic_fields) == 628);
    assert!(size_of::<VAEncSliceParameterBufferH264>() == 3140);
    assert!(offset_of!(VAEncSliceParameterBufferH264, macroblock_info) == 8);
    assert!(offset_of!(VAEncSliceParameterBufferH264, slice_type) == 12);
    assert!(offset_of!(VAEncSliceParameterBufferH264, idr_pic_id) == 14);
    assert!(offset_of!(VAEncSliceParameterBufferH264, pic_order_cnt_lsb) == 16);
    assert!(offset_of!(VAEncSliceParameterBufferH264, delta_pic_order_cnt_bottom) == 20);
    assert!(offset_of!(VAEncSliceParameterBufferH264, direct_spatial_mv_pred_flag) == 32);
    assert!(offset_of!(VAEncSliceParameterBufferH264, RefPicList0) == 36);
    assert!(offset_of!(VAEncSliceParameterBufferH264, luma_log2_weight_denom) == 2340);
    assert!(offset_of!(VAEncSliceParameterBufferH264, luma_weight_l0) == 2344);
    assert!(offset_of!(VAEncSliceParameterBufferH264, chroma_weight_l0_flag) == 2472);
    assert!(offset_of!(VAEncSliceParameterBufferH264, chroma_weight_l0) == 2474);
    assert!(offset_of!(VAEncSliceParameterBufferH264, luma_weight_l1_flag) == 2730);
    assert!(offset_of!(VAEncSliceParameterBufferH264, chroma_weight_l1_flag) == 2860);
    assert!(offset_of!(VAEncSliceParameterBufferH264, cabac_init_idc) == 3118);
    assert!(offset_of!(VAEncSliceParameterBufferH264, va_reserved) == 3124);
    assert!(size_of::<VAEncMiscParameterRateControl>() == 60);
    assert!(offset_of!(VAEncMiscParameterRateControl, rc_flags) == 24);
    assert!(offset_of!(VAEncMiscParameterRateControl, ICQ_quality_factor) == 28);
    assert!(size_of::<VAEncMiscParameterFrameRate>() == 24);
    assert!(size_of::<VAEncMiscParameterHRD>() == 24);
    assert!(size_of::<VAEncPackedHeaderParameterBuffer>() == 28);
    assert!(offset_of!(VAEncPackedHeaderParameterBuffer, has_emulation_bytes) == 8);
    assert!(offset_of!(VAEncPackedHeaderParameterBuffer, va_reserved) == 12);
    assert!(size_of::<VAImageFormat>() == 48);
    assert!(size_of::<VAImage>() == 120);
    assert!(offset_of!(VAImage, format) == 4);
    assert!(offset_of!(VAImage, buf) == 52);
    assert!(offset_of!(VAImage, width) == 56);
    assert!(offset_of!(VAImage, data_size) == 60);
    assert!(offset_of!(VAImage, pitches) == 68);
    assert!(offset_of!(VAImage, offsets) == 80);
    assert!(offset_of!(VAImage, num_palette_entries) == 92);
    assert!(offset_of!(VAImage, component_order) == 100);
    assert!(offset_of!(VAImage, va_reserved) == 104);
    assert!(size_of::<VACodedBufferSegment>() == 48);
    assert!(offset_of!(VACodedBufferSegment, buf) == 16);
    assert!(offset_of!(VACodedBufferSegment, next) == 24);
    assert!(size_of::<VAConfigAttrib>() == 8);
    assert!(size_of::<VAPictureParameterBufferJPEGBaseline>() == 1060);
    assert!(offset_of!(VAPictureParameterBufferJPEGBaseline, num_components) == 1024);
    assert!(offset_of!(VAPictureParameterBufferJPEGBaseline, rotation) == 1028);
    assert!(size_of::<VAIQMatrixBufferJPEGBaseline>() == 276);
    assert!(offset_of!(VAIQMatrixBufferJPEGBaseline, va_reserved) == 260);
    assert!(size_of::<VASliceParameterBufferJPEGBaseline>() == 56);
    assert!(offset_of!(VASliceParameterBufferJPEGBaseline, components) == 20);
    assert!(offset_of!(VASliceParameterBufferJPEGBaseline, num_components) == 32);
    assert!(offset_of!(VASliceParameterBufferJPEGBaseline, restart_interval) == 34);
    assert!(offset_of!(VASliceParameterBufferJPEGBaseline, num_mcus) == 36);
    assert!(size_of::<VAHuffmanTableBufferJPEGBaseline>() == 436);
    assert!(offset_of!(VAHuffmanTableBufferJPEGBaseline, huffman_table) == 2);
    assert!(offset_of!(VAHuffmanTableBufferJPEGBaseline, va_reserved) == 420);
    assert!(size_of::<VASurfaceAttrib>() == 24);
    assert!(size_of::<VAPictureHEVC>() == 28);
    assert!(size_of::<VAPictureParameterBufferHEVC>() == 604);
    assert!(offset_of!(VAPictureParameterBufferHEVC, ReferenceFrames) == 28);
    assert!(offset_of!(VAPictureParameterBufferHEVC, pic_width_in_luma_samples) == 448);
    assert!(offset_of!(VAPictureParameterBufferHEVC, pic_fields) == 452);
    assert!(offset_of!(VAPictureParameterBufferHEVC, sps_max_dec_pic_buffering_minus1) == 456);
    assert!(offset_of!(VAPictureParameterBufferHEVC, init_qp_minus26) == 469);
    assert!(offset_of!(VAPictureParameterBufferHEVC, num_tile_columns_minus1) == 474);
    assert!(offset_of!(VAPictureParameterBufferHEVC, column_width_minus1) == 476);
    assert!(offset_of!(VAPictureParameterBufferHEVC, row_height_minus1) == 514);
    assert!(offset_of!(VAPictureParameterBufferHEVC, slice_parsing_fields) == 556);
    assert!(offset_of!(VAPictureParameterBufferHEVC, log2_max_pic_order_cnt_lsb_minus4) == 560);
    assert!(offset_of!(VAPictureParameterBufferHEVC, num_extra_slice_header_bits) == 567);
    assert!(offset_of!(VAPictureParameterBufferHEVC, st_rps_bits) == 568);
    assert!(offset_of!(VAPictureParameterBufferHEVC, va_reserved) == 572);
    assert!(size_of::<VASliceParameterBufferHEVC>() == 264);
    assert!(offset_of!(VASliceParameterBufferHEVC, slice_data_byte_offset) == 12);
    assert!(offset_of!(VASliceParameterBufferHEVC, slice_segment_address) == 16);
    assert!(offset_of!(VASliceParameterBufferHEVC, RefPicList) == 20);
    assert!(offset_of!(VASliceParameterBufferHEVC, LongSliceFlags) == 52);
    assert!(offset_of!(VASliceParameterBufferHEVC, collocated_ref_idx) == 56);
    assert!(offset_of!(VASliceParameterBufferHEVC, slice_tc_offset_div2) == 63);
    assert!(offset_of!(VASliceParameterBufferHEVC, luma_log2_weight_denom) == 64);
    assert!(offset_of!(VASliceParameterBufferHEVC, delta_luma_weight_l0) == 66);
    assert!(offset_of!(VASliceParameterBufferHEVC, ChromaOffsetL1) == 216);
    assert!(offset_of!(VASliceParameterBufferHEVC, five_minus_max_num_merge_cand) == 246);
    assert!(offset_of!(VASliceParameterBufferHEVC, num_entry_point_offsets) == 248);
    assert!(offset_of!(VASliceParameterBufferHEVC, entry_offset_to_subset_array) == 250);
    assert!(offset_of!(VASliceParameterBufferHEVC, slice_data_num_emu_prevn_bytes) == 252);
    assert!(offset_of!(VASliceParameterBufferHEVC, va_reserved) == 256);
    assert!(size_of::<VAEncSequenceParameterBufferHEVC>() == 116);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, intra_period) == 4);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, pic_width_in_luma_samples) == 20);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, seq_fields) == 24);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, log2_min_luma_coding_block_size_minus3) == 28);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, max_transform_hierarchy_depth_intra) == 33);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, pcm_sample_bit_depth_luma_minus1) == 36);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, log2_max_pcm_luma_coding_block_size_minus3) == 48);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, vui_parameters_present_flag) == 52);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, vui_fields) == 56);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, aspect_ratio_idc) == 60);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, sar_width) == 64);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, vui_time_scale) == 76);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, min_spatial_segmentation_idc) == 80);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, max_bits_per_min_cu_denom) == 83);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, scc_fields) == 84);
    assert!(offset_of!(VAEncSequenceParameterBufferHEVC, va_reserved) == 88);
    assert!(size_of::<VAEncPictureParameterBufferHEVC>() == 576);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, reference_frames) == 28);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, coded_buf) == 448);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, collocated_ref_pic_index) == 452);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, pic_init_qp) == 454);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, num_tile_columns_minus1) == 458);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, column_width_minus1) == 460);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, row_height_minus1) == 479);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, log2_parallel_merge_level_minus2) == 500);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, num_ref_idx_l0_default_active_minus1) == 502);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, slice_pic_parameter_set_id) == 504);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, nal_unit_type) == 505);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, pic_fields) == 508);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, hierarchical_level_plus1) == 512);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, scc_fields) == 514);
    assert!(offset_of!(VAEncPictureParameterBufferHEVC, va_reserved) == 516);
    assert!(size_of::<VAEncSliceParameterBufferHEVC>() == 1076);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, num_ctu_in_slice) == 4);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, slice_type) == 8);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, num_ref_idx_l1_active_minus1) == 11);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, ref_pic_list0) == 12);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, ref_pic_list1) == 432);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, luma_log2_weight_denom) == 852);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, delta_luma_weight_l0) == 854);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, chroma_offset_l1) == 1004);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, max_num_merge_cand) == 1034);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, slice_tc_offset_div2) == 1039);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, slice_fields) == 1040);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, pred_weight_table_bit_offset) == 1044);
    assert!(offset_of!(VAEncSliceParameterBufferHEVC, va_reserved) == 1052);
};
