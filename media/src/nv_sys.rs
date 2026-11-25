//! NVIDIA's video interfaces, as their headers lay them out: the CUDA
//! driver's little that's needed, NVENC (`nvEncodeAPI.h`, Video Codec SDK
//! 11.1: drivers from 470 on) and NVDEC (`cuviddec.h`, `nvcuvid.h`). Only
//! what swarm uses is named; the rest is kept as padding. Sizes and
//! offsets are checked against what a C compiler made of FFmpeg's copy
//! of the headers (nv-codec-headers n11.1.5.3).

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};

pub type CUresult = c_int;
pub type CUdevice = c_int;
pub type CUcontext = *mut c_void;
pub type CUdeviceptr = u64;

pub const CUDA_SUCCESS: CUresult = 0;
pub const CU_CTX_SCHED_BLOCKING_SYNC: c_uint = 0x04;
pub const CU_MEMORYTYPE_HOST: c_uint = 1;
pub const CU_MEMORYTYPE_DEVICE: c_uint = 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUDA_MEMCPY2D {
    pub srcXInBytes: usize,
    pub srcY: usize,
    pub srcMemoryType: c_uint,
    pub srcHost: *const c_void,
    pub srcDevice: CUdeviceptr,
    pub srcArray: *mut c_void,
    pub srcPitch: usize,
    pub dstXInBytes: usize,
    pub dstY: usize,
    pub dstMemoryType: c_uint,
    pub dstHost: *mut c_void,
    pub dstDevice: CUdeviceptr,
    pub dstArray: *mut c_void,
    pub dstPitch: usize,
    pub WidthInBytes: usize,
    pub Height: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GUID {
    pub Data1: u32,
    pub Data2: u16,
    pub Data3: u16,
    pub Data4: [u8; 8],
}

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> GUID {
    GUID { Data1: a, Data2: b, Data3: c, Data4: d }
}

pub const NV_ENC_CODEC_H264_GUID: GUID = guid(0x6bc82762, 0x4e63, 0x4ca4, [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]);
pub const NV_ENC_CODEC_HEVC_GUID: GUID = guid(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
pub const NV_ENC_HEVC_PROFILE_MAIN_GUID: GUID = guid(0xb514c39a, 0xb55b, 0x40fa, [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec]);
pub const NV_ENC_H264_PROFILE_BASELINE_GUID: GUID = guid(0x0727bcaa, 0x78c4, 0x4c83, [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a]);
pub const NV_ENC_PRESET_P1_GUID: GUID = guid(0xfc0a8d3e, 0x45f8, 0x4cf8, [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf]);
pub const NV_ENC_PRESET_P2_GUID: GUID = guid(0xf581cfb8, 0x88d6, 0x4381, [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab]);
pub const NV_ENC_PRESET_P3_GUID: GUID = guid(0x36850110, 0x3a07, 0x441f, [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6]);
pub const NV_ENC_PRESET_P4_GUID: GUID = guid(0x90a7b826, 0xdf06, 0x4862, [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81]);

pub type NVENCSTATUS = c_int;
pub const NV_ENC_SUCCESS: NVENCSTATUS = 0;
pub const NV_ENC_ERR_NEED_MORE_INPUT: NVENCSTATUS = 17;
pub const NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY: NVENCSTATUS = 21;
/// NVENCSTATUS's names, in order from 0.
pub const NV_ENC_STATUS_NAMES: [&str; 26] = [
    "success",
    "no encode device",
    "unsupported device",
    "invalid encoder device",
    "invalid device",
    "device doesn't exist",
    "invalid pointer",
    "invalid event",
    "invalid parameter",
    "invalid call",
    "out of memory",
    "encoder not initialised",
    "unsupported parameter",
    "lock busy",
    "not enough buffer",
    "invalid version",
    "map failed",
    "needs more input",
    "encoder busy",
    "event not registered",
    "generic error",
    "incompatible client key",
    "unimplemented",
    "resource register failed",
    "resource not registered",
    "resource not mapped",
];

pub const NVENCAPI_MAJOR_VERSION: u32 = 11;
pub const NVENCAPI_MINOR_VERSION: u32 = 1;
pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);
const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}
pub const NV_ENC_CAPS_PARAM_VER: u32 = struct_version(1);
pub const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_RC_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENC_CONFIG_VER: u32 = struct_version(7) | (1 << 31);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(5) | (1 << 31);
pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(1) | (1 << 31);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(4) | (1 << 31);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(1);
pub const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);

pub const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;
pub const NV_ENC_PARAMS_RC_CBR: u32 = 2;
pub const NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME: u32 = 1;
pub const NV_ENC_PIC_STRUCT_FRAME: u32 = 1;
pub const NV_ENC_PIC_TYPE_IDR: u32 = 3;
pub const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x1;
pub const NV_ENC_BUFFER_FORMAT_IYUV: u32 = 0x100;
pub const NV_ENC_MEMORY_HEAP_AUTOSELECT: u32 = 0;
pub const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
pub const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
pub const NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC: u32 = 2;
pub const NV_ENC_H264_BDIRECT_MODE_DISABLE: u32 = 1;
pub const NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE: u32 = 1;
pub const NV_ENC_H264_FMO_DISABLE: u32 = 2;
pub const NV_ENC_NUM_REF_FRAMES_1: u32 = 1;
pub const NV_ENC_DEVICE_TYPE_CUDA: u32 = 1;
pub const NV_ENC_TUNING_INFO_LOW_LATENCY: u32 = 2;
pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
pub const NV_ENC_CAPS_WIDTH_MAX: c_int = 16;
pub const NV_ENC_CAPS_HEIGHT_MAX: c_int = 17;

/// The structs below are plain data, given to NVIDIA's libraries zeroed
/// but for what's set (as their samples do).
pub trait Zeroed: Sized {
    fn zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
pub struct NV_ENC_CAPS_PARAM {
    pub version: u32,
    pub capsToQuery: c_int,
    pub reserved: [u32; 62],
}

#[repr(C)]
pub struct NV_ENC_CREATE_INPUT_BUFFER {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub memoryHeap: u32,
    pub bufferFmt: u32,
    pub reserved: u32,
    pub inputBuffer: *mut c_void,
    pub pSysMemBuffer: *mut c_void,
    pub reserved1: [u32; 57],
    pub reserved2: [*mut c_void; 63],
}

#[repr(C)]
pub struct NV_ENC_CREATE_BITSTREAM_BUFFER {
    pub version: u32,
    pub size: u32,
    pub memoryHeap: u32,
    pub reserved: u32,
    pub bitstreamBuffer: *mut c_void,
    pub bitstreamBufferPtr: *mut c_void,
    pub reserved1: [u32; 58],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_QP {
    pub qpInterP: u32,
    pub qpInterB: u32,
    pub qpIntra: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_RC_PARAMS {
    pub version: u32,
    pub rateControlMode: u32,
    pub constQP: NV_ENC_QP,
    pub averageBitRate: u32,
    pub maxBitRate: u32,
    pub vbvBufferSize: u32,
    pub vbvInitialDelay: u32,
    /// enableMinQP (bit 0), enableMaxQP, enableInitialRCQP, enableAQ,
    /// -, enableLookahead (5), disableIadapt, disableBadapt,
    /// enableTemporalAQ (8), zeroReorderDelay (9), enableNonRefP (10),
    /// strictGOPTarget (11), aqStrength (12, 4 bits).
    pub flags: u32,
    pub minQP: NV_ENC_QP,
    pub maxQP: NV_ENC_QP,
    pub initialRCQP: NV_ENC_QP,
    pub temporallayerIdxMask: u32,
    pub temporalLayerQP: [u8; 8],
    pub targetQuality: u8,
    pub targetQualityLSB: u8,
    pub lookaheadDepth: u16,
    pub lowDelayKeyFrameScale: u8,
    pub reserved1: [u8; 3],
    pub qpMapMode: u32,
    pub multiPass: u32,
    pub alphaLayerBitrateRatio: u32,
    pub cbQPIndexOffset: i8,
    pub crQPIndexOffset: i8,
    pub reserved2: u16,
    pub reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_H264_VUI_PARAMETERS {
    pub overscanInfoPresentFlag: u32,
    pub overscanInfo: u32,
    pub videoSignalTypePresentFlag: u32,
    pub videoFormat: u32,
    pub videoFullRangeFlag: u32,
    pub colourDescriptionPresentFlag: u32,
    pub colourPrimaries: u32,
    pub transferCharacteristics: u32,
    pub colourMatrix: u32,
    pub chromaSampleLocationFlag: u32,
    pub chromaSampleLocationTop: u32,
    pub chromaSampleLocationBot: u32,
    pub bitstreamRestrictionFlag: u32,
    pub reserved: [u32; 15],
}

/// `flags`: bits as `NV_ENC_CONFIG_H264`'s bit fields.
pub const H264_OUTPUT_AUD: u32 = 1 << 6;
pub const H264_DISABLE_SPSPPS: u32 = 1 << 7;
pub const H264_ENABLE_INTRA_REFRESH: u32 = 1 << 10;
pub const H264_REPEAT_SPSPPS: u32 = 1 << 12;
pub const H264_ENABLE_LTR: u32 = 1 << 14;
pub const H264_ENABLE_FILLER_DATA: u32 = 1 << 17;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_H264 {
    pub flags: u32,
    pub level: u32,
    pub idrPeriod: u32,
    pub separateColourPlaneFlag: u32,
    pub disableDeblockingFilterIDC: u32,
    pub numTemporalLayers: u32,
    pub spsId: u32,
    pub ppsId: u32,
    pub adaptiveTransformMode: u32,
    pub fmoMode: u32,
    pub bdirectMode: u32,
    pub entropyCodingMode: u32,
    pub stereoMode: u32,
    pub intraRefreshPeriod: u32,
    pub intraRefreshCnt: u32,
    pub maxNumRefFrames: u32,
    pub sliceMode: u32,
    pub sliceModeData: u32,
    pub h264VUIParameters: NV_ENC_CONFIG_H264_VUI_PARAMETERS,
    pub ltrNumFrames: u32,
    pub ltrTrustMode: u32,
    pub chromaFormatIDC: u32,
    pub maxTemporalLayers: u32,
    pub useBFramesAsRef: u32,
    pub numRefL0: u32,
    pub numRefL1: u32,
    pub reserved1: [u32; 267],
    pub reserved2: [*mut c_void; 64],
}

/// `flags`: bits as `NV_ENC_CONFIG_HEVC`'s bit fields.
pub const HEVC_OUTPUT_AUD: u32 = 1 << 4;
pub const HEVC_ENABLE_LTR: u32 = 1 << 5;
pub const HEVC_DISABLE_SPSPPS: u32 = 1 << 6;
pub const HEVC_REPEAT_SPSPPS: u32 = 1 << 7;
pub const HEVC_ENABLE_INTRA_REFRESH: u32 = 1 << 8;
/// `chromaFormatIDC` (two bits): 1, 4:2:0.
pub const HEVC_CHROMA_420: u32 = 1 << 9;
pub const HEVC_CHROMA_MASK: u32 = 3 << 9;
/// `pixelBitDepthMinus8` (three bits).
pub const HEVC_DEPTH_MASK: u32 = 7 << 11;
pub const HEVC_ENABLE_FILLER_DATA: u32 = 1 << 14;

/// HEVC's member of `NV_ENC_CONFIG`'s union (see `NV_ENC_CONFIG::hevc`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_HEVC {
    pub level: u32,
    pub tier: u32,
    pub minCUSize: u32,
    pub maxCUSize: u32,
    pub flags: u32,
    pub idrPeriod: u32,
    pub intraRefreshPeriod: u32,
    pub intraRefreshCnt: u32,
    pub maxNumRefFramesInDPB: u32,
    pub ltrNumFrames: u32,
    pub vpsId: u32,
    pub spsId: u32,
    pub ppsId: u32,
    pub sliceMode: u32,
    pub sliceModeData: u32,
    pub maxTemporalLayersMinus1: u32,
    pub hevcVUIParameters: NV_ENC_CONFIG_H264_VUI_PARAMETERS,
    pub ltrTrustMode: u32,
    pub useBFramesAsRef: u32,
    pub numRefL0: u32,
    pub numRefL1: u32,
    pub reserved1: [u32; 214],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG {
    pub version: u32,
    pub profileGUID: GUID,
    pub gopLength: u32,
    pub frameIntervalP: i32,
    pub monoChromeEncoding: u32,
    pub frameFieldMode: u32,
    pub mvPrecision: u32,
    pub rcParams: NV_ENC_RC_PARAMS,
    /// A union; H.264's is the biggest of its members (HEVC's is had
    /// through `hevc`).
    pub h264: NV_ENC_CONFIG_H264,
    pub reserved: [u32; 278],
    pub reserved2: [*mut c_void; 64],
}

impl NV_ENC_CONFIG {
    /// The codec's union as HEVC's member.
    pub fn hevc(&mut self) -> &mut NV_ENC_CONFIG_HEVC {
        const { assert!(std::mem::size_of::<NV_ENC_CONFIG_HEVC>() <= std::mem::size_of::<NV_ENC_CONFIG_H264>()) };
        unsafe { &mut *(&raw mut self.h264).cast::<NV_ENC_CONFIG_HEVC>() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE {
    pub counts: u32,
    pub reserved1: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_INITIALIZE_PARAMS {
    pub version: u32,
    pub encodeGUID: GUID,
    pub presetGUID: GUID,
    pub encodeWidth: u32,
    pub encodeHeight: u32,
    pub darWidth: u32,
    pub darHeight: u32,
    pub frameRateNum: u32,
    pub frameRateDen: u32,
    pub enableEncodeAsync: u32,
    pub enablePTD: u32,
    pub flags: u32,
    pub privDataSize: u32,
    pub privData: *mut c_void,
    pub encodeConfig: *mut NV_ENC_CONFIG,
    pub maxEncodeWidth: u32,
    pub maxEncodeHeight: u32,
    pub maxMEHintCountsPerBlock: [NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE; 2],
    pub tuningInfo: u32,
    pub bufferFormat: u32,
    pub reserved: [u32; 287],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct NV_ENC_RECONFIGURE_PARAMS {
    pub version: u32,
    pub reInitEncodeParams: NV_ENC_INITIALIZE_PARAMS,
    /// resetEncoder (bit 0), forceIDR (bit 1).
    pub flags: u32,
}

#[repr(C)]
pub struct NV_ENC_PRESET_CONFIG {
    pub version: u32,
    pub presetCfg: NV_ENC_CONFIG,
    pub reserved1: [u32; 255],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct NV_ENC_PIC_PARAMS {
    pub version: u32,
    pub inputWidth: u32,
    pub inputHeight: u32,
    pub inputPitch: u32,
    pub encodePicFlags: u32,
    pub frameIdx: u32,
    pub inputTimeStamp: u64,
    pub inputDuration: u64,
    pub inputBuffer: *mut c_void,
    pub outputBitstream: *mut c_void,
    pub completionEvent: *mut c_void,
    pub bufferFmt: u32,
    pub pictureStruct: u32,
    pub pictureType: u32,
    /// A union (H.264's, HEVC's): none of it's set.
    pub codecPicParams: [u64; 192],
    pub meHintCountsPerBlock: [NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE; 2],
    pub meExternalHints: *mut c_void,
    pub reserved1: [u32; 6],
    pub reserved2: [*mut c_void; 2],
    pub qpDeltaMap: *mut i8,
    pub qpDeltaMapSize: u32,
    pub reservedBitFields: u32,
    pub meHintRefPicDist: [u16; 2],
    pub alphaBuffer: *mut c_void,
    pub reserved3: [u32; 286],
    pub reserved4: [*mut c_void; 59],
}

#[repr(C)]
pub struct NV_ENC_LOCK_BITSTREAM {
    pub version: u32,
    /// doNotWait (bit 0), ltrFrame, getRCStats.
    pub flags: u32,
    pub outputBitstream: *mut c_void,
    pub sliceOffsets: *mut u32,
    pub frameIdx: u32,
    pub hwEncodeStatus: u32,
    pub numSlices: u32,
    pub bitstreamSizeInBytes: u32,
    pub outputTimeStamp: u64,
    pub outputDuration: u64,
    pub bitstreamBufferPtr: *mut c_void,
    pub pictureType: u32,
    pub pictureStruct: u32,
    pub frameAvgQP: u32,
    pub frameSatd: u32,
    pub ltrFrameIdx: u32,
    pub ltrFrameBitmap: u32,
    pub temporalId: u32,
    pub reserved: [u32; 12],
    pub intraMBCount: u32,
    pub interMBCount: u32,
    pub averageMVX: i32,
    pub averageMVY: i32,
    pub alphaLayerSizeInBytes: u32,
    pub reserved1: [u32; 218],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct NV_ENC_LOCK_INPUT_BUFFER {
    pub version: u32,
    pub flags: u32,
    pub inputBuffer: *mut c_void,
    pub bufferDataPtr: *mut c_void,
    pub pitch: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    pub version: u32,
    pub deviceType: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub apiVersion: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

type Encoder = *mut c_void;
type Status = NVENCSTATUS;

/// NVENC's functions, which `NvEncodeAPICreateInstance` fills in. Those
/// swarm doesn't call are left as pointers.
#[repr(C)]
pub struct NV_ENCODE_API_FUNCTION_LIST {
    pub version: u32,
    pub reserved: u32,
    pub nvEncOpenEncodeSession: *mut c_void,
    pub nvEncGetEncodeGUIDCount: Option<unsafe extern "C" fn(Encoder, *mut u32) -> Status>,
    pub nvEncGetEncodeProfileGUIDCount: *mut c_void,
    pub nvEncGetEncodeProfileGUIDs: *mut c_void,
    pub nvEncGetEncodeGUIDs: Option<unsafe extern "C" fn(Encoder, *mut GUID, u32, *mut u32) -> Status>,
    pub nvEncGetInputFormatCount: *mut c_void,
    pub nvEncGetInputFormats: *mut c_void,
    pub nvEncGetEncodeCaps: Option<unsafe extern "C" fn(Encoder, GUID, *mut NV_ENC_CAPS_PARAM, *mut c_int) -> Status>,
    pub nvEncGetEncodePresetCount: *mut c_void,
    pub nvEncGetEncodePresetGUIDs: *mut c_void,
    pub nvEncGetEncodePresetConfig: *mut c_void,
    pub nvEncInitializeEncoder: Option<unsafe extern "C" fn(Encoder, *mut NV_ENC_INITIALIZE_PARAMS) -> Status>,
    pub nvEncCreateInputBuffer: Option<unsafe extern "C" fn(Encoder, *mut NV_ENC_CREATE_INPUT_BUFFER) -> Status>,
    pub nvEncDestroyInputBuffer: Option<unsafe extern "C" fn(Encoder, *mut c_void) -> Status>,
    pub nvEncCreateBitstreamBuffer: Option<unsafe extern "C" fn(Encoder, *mut NV_ENC_CREATE_BITSTREAM_BUFFER) -> Status>,
    pub nvEncDestroyBitstreamBuffer: Option<unsafe extern "C" fn(Encoder, *mut c_void) -> Status>,
    pub nvEncEncodePicture: Option<unsafe extern "C" fn(Encoder, *mut NV_ENC_PIC_PARAMS) -> Status>,
    pub nvEncLockBitstream: Option<unsafe extern "C" fn(Encoder, *mut NV_ENC_LOCK_BITSTREAM) -> Status>,
    pub nvEncUnlockBitstream: Option<unsafe extern "C" fn(Encoder, *mut c_void) -> Status>,
    pub nvEncLockInputBuffer: Option<unsafe extern "C" fn(Encoder, *mut NV_ENC_LOCK_INPUT_BUFFER) -> Status>,
    pub nvEncUnlockInputBuffer: Option<unsafe extern "C" fn(Encoder, *mut c_void) -> Status>,
    pub nvEncGetEncodeStats: *mut c_void,
    pub nvEncGetSequenceParams: *mut c_void,
    pub nvEncRegisterAsyncEvent: *mut c_void,
    pub nvEncUnregisterAsyncEvent: *mut c_void,
    pub nvEncMapInputResource: *mut c_void,
    pub nvEncUnmapInputResource: *mut c_void,
    pub nvEncDestroyEncoder: Option<unsafe extern "C" fn(Encoder) -> Status>,
    pub nvEncInvalidateRefFrames: *mut c_void,
    pub nvEncOpenEncodeSessionEx: Option<unsafe extern "C" fn(*mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, *mut Encoder) -> Status>,
    pub nvEncRegisterResource: *mut c_void,
    pub nvEncUnregisterResource: *mut c_void,
    pub nvEncReconfigureEncoder: Option<unsafe extern "C" fn(Encoder, *mut NV_ENC_RECONFIGURE_PARAMS) -> Status>,
    pub reserved1: *mut c_void,
    pub nvEncCreateMVBuffer: *mut c_void,
    pub nvEncDestroyMVBuffer: *mut c_void,
    pub nvEncRunMotionEstimationOnly: *mut c_void,
    pub nvEncGetLastErrorString: Option<unsafe extern "C" fn(Encoder) -> *const c_char>,
    pub nvEncSetIOCudaStreams: *mut c_void,
    pub nvEncGetEncodePresetConfigEx: Option<unsafe extern "C" fn(Encoder, GUID, GUID, u32, *mut NV_ENC_PRESET_CONFIG) -> Status>,
    pub nvEncGetSequenceParamEx: *mut c_void,
    pub reserved2: [*mut c_void; 277],
}

impl Zeroed for NV_ENC_CAPS_PARAM {}
impl Zeroed for NV_ENC_CREATE_INPUT_BUFFER {}
impl Zeroed for NV_ENC_CREATE_BITSTREAM_BUFFER {}
impl Zeroed for NV_ENC_CONFIG {}
impl Zeroed for NV_ENC_INITIALIZE_PARAMS {}
impl Zeroed for NV_ENC_RECONFIGURE_PARAMS {}
impl Zeroed for NV_ENC_PRESET_CONFIG {}
impl Zeroed for NV_ENC_PIC_PARAMS {}
impl Zeroed for NV_ENC_LOCK_BITSTREAM {}
impl Zeroed for NV_ENC_LOCK_INPUT_BUFFER {}
impl Zeroed for NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {}
impl Zeroed for NV_ENCODE_API_FUNCTION_LIST {}

pub type CUvideodecoder = *mut c_void;
pub type CUvideoparser = *mut c_void;
pub type CUvideoctxlock = *mut c_void;

pub const cudaVideoCodec_H264: c_int = 4;
pub const cudaVideoCodec_JPEG: c_int = 5;
pub const cudaVideoCodec_HEVC: c_int = 8;
pub const cudaVideoChromaFormat_420: c_int = 1;
pub const cudaVideoSurfaceFormat_NV12: c_int = 0;
pub const cudaVideoDeinterlaceMode_Weave: c_int = 0;
pub const cudaVideoCreate_PreferCUVID: c_ulong = 4;
pub const CUVID_PKT_ENDOFSTREAM: c_ulong = 0x01;
pub const CUVID_PKT_TIMESTAMP: c_ulong = 0x02;
pub const CUVID_PKT_ENDOFPICTURE: c_ulong = 0x08;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CUVIDEOFORMAT {
    pub codec: c_int,
    pub frame_rate: [c_uint; 2],
    pub progressive_sequence: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub min_num_decode_surfaces: u8,
    pub coded_width: c_uint,
    pub coded_height: c_uint,
    /// Left, top, right, bottom.
    pub display_area: [c_int; 4],
    pub chroma_format: c_int,
    pub bitrate: c_uint,
    pub display_aspect_ratio: [c_int; 2],
    pub video_signal_description: [u8; 4],
    pub seqhdr_data_length: c_uint,
}

#[repr(C)]
pub struct CUVIDSOURCEDATAPACKET {
    pub flags: c_ulong,
    pub payload_size: c_ulong,
    pub payload: *const u8,
    pub timestamp: i64,
}

#[repr(C)]
pub struct CUVIDPARSERDISPINFO {
    pub picture_index: c_int,
    pub progressive_frame: c_int,
    pub top_field_first: c_int,
    pub repeat_first_field: c_int,
    pub timestamp: i64,
}

/// Opaque: what the parser hands to `cuvidDecodePicture`, untouched.
#[repr(C)]
pub struct CUVIDPICPARAMS {
    _opaque: [u8; 0],
}

pub type SequenceCallback = unsafe extern "C" fn(*mut c_void, *mut CUVIDEOFORMAT) -> c_int;
pub type DecodeCallback = unsafe extern "C" fn(*mut c_void, *mut CUVIDPICPARAMS) -> c_int;
pub type DisplayCallback = unsafe extern "C" fn(*mut c_void, *mut CUVIDPARSERDISPINFO) -> c_int;

#[repr(C)]
pub struct CUVIDPARSERPARAMS {
    pub CodecType: c_int,
    pub ulMaxNumDecodeSurfaces: c_uint,
    pub ulClockRate: c_uint,
    pub ulErrorThreshold: c_uint,
    pub ulMaxDisplayDelay: c_uint,
    /// bAnnexb (bit 0).
    pub flags: c_uint,
    pub uReserved1: [c_uint; 4],
    pub pUserData: *mut c_void,
    pub pfnSequenceCallback: Option<SequenceCallback>,
    pub pfnDecodePicture: Option<DecodeCallback>,
    pub pfnDisplayPicture: Option<DisplayCallback>,
    pub pfnGetOperatingPoint: *mut c_void,
    pub pvReserved2: [*mut c_void; 6],
    pub pExtVideoInfo: *mut c_void,
}

#[repr(C)]
pub struct CUVIDDECODECAPS {
    pub eCodecType: c_int,
    pub eChromaFormat: c_int,
    pub nBitDepthMinus8: c_uint,
    pub reserved1: [c_uint; 3],
    pub bIsSupported: u8,
    pub nNumNVDECs: u8,
    pub nOutputFormatMask: u16,
    pub nMaxWidth: c_uint,
    pub nMaxHeight: c_uint,
    pub nMaxMBCount: c_uint,
    pub nMinWidth: u16,
    pub nMinHeight: u16,
    pub bIsHistogramSupported: u8,
    pub nCounterBitDepth: u8,
    pub nMaxHistogramBins: u16,
    pub reserved3: [c_uint; 10],
}

#[repr(C)]
pub struct CUVIDDECODECREATEINFO {
    pub ulWidth: c_ulong,
    pub ulHeight: c_ulong,
    pub ulNumDecodeSurfaces: c_ulong,
    pub CodecType: c_int,
    pub ChromaFormat: c_int,
    pub ulCreationFlags: c_ulong,
    pub bitDepthMinus8: c_ulong,
    pub ulIntraDecodeOnly: c_ulong,
    pub ulMaxWidth: c_ulong,
    pub ulMaxHeight: c_ulong,
    pub Reserved1: c_ulong,
    /// Left, top, right, bottom.
    pub display_area: [i16; 4],
    pub OutputFormat: c_int,
    pub DeinterlaceMode: c_int,
    pub ulTargetWidth: c_ulong,
    pub ulTargetHeight: c_ulong,
    pub ulNumOutputSurfaces: c_ulong,
    pub vidLock: CUvideoctxlock,
    pub target_rect: [i16; 4],
    pub enableHistogram: c_ulong,
    pub Reserved2: [c_ulong; 4],
}

#[repr(C)]
pub struct CUVIDPROCPARAMS {
    pub progressive_frame: c_int,
    pub second_field: c_int,
    pub top_field_first: c_int,
    pub unpaired_field: c_int,
    pub reserved_flags: c_uint,
    pub reserved_zero: c_uint,
    pub raw_input_dptr: u64,
    pub raw_input_pitch: c_uint,
    pub raw_input_format: c_uint,
    pub raw_output_dptr: u64,
    pub raw_output_pitch: c_uint,
    pub Reserved1: c_uint,
    pub output_stream: *mut c_void,
    pub Reserved: [c_uint; 46],
    pub histogram_dptr: *mut u64,
    pub Reserved2: [*mut c_void; 1],
}

impl Zeroed for CUVIDPARSERPARAMS {}
impl Zeroed for CUVIDDECODECAPS {}
impl Zeroed for CUVIDDECODECREATEINFO {}
impl Zeroed for CUVIDPROCPARAMS {}
impl Zeroed for CUVIDSOURCEDATAPACKET {}
impl Zeroed for CUDA_MEMCPY2D {}

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<GUID>() == 16);
    assert!(size_of::<NV_ENC_CAPS_PARAM>() == 256);
    assert!(size_of::<NV_ENC_CREATE_INPUT_BUFFER>() == 776);
    assert!(offset_of!(NV_ENC_CREATE_INPUT_BUFFER, inputBuffer) == 24);
    assert!(offset_of!(NV_ENC_CREATE_INPUT_BUFFER, reserved1) == 40);
    assert!(offset_of!(NV_ENC_CREATE_INPUT_BUFFER, reserved2) == 272);
    assert!(size_of::<NV_ENC_CREATE_BITSTREAM_BUFFER>() == 776);
    assert!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBuffer) == 16);
    assert!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, reserved2) == 264);
    assert!(size_of::<NV_ENC_RC_PARAMS>() == 128);
    assert!(offset_of!(NV_ENC_RC_PARAMS, averageBitRate) == 20);
    assert!(offset_of!(NV_ENC_RC_PARAMS, vbvInitialDelay) == 32);
    assert!(offset_of!(NV_ENC_RC_PARAMS, minQP) == 40);
    assert!(offset_of!(NV_ENC_RC_PARAMS, temporalLayerQP) == 80);
    assert!(offset_of!(NV_ENC_RC_PARAMS, lookaheadDepth) == 90);
    assert!(offset_of!(NV_ENC_RC_PARAMS, qpMapMode) == 96);
    assert!(offset_of!(NV_ENC_RC_PARAMS, cbQPIndexOffset) == 108);
    assert!(offset_of!(NV_ENC_RC_PARAMS, reserved) == 112);
    assert!(size_of::<NV_ENC_CONFIG_H264_VUI_PARAMETERS>() == 112);
    assert!(size_of::<NV_ENC_CONFIG_H264>() == 1792);
    assert!(offset_of!(NV_ENC_CONFIG_H264, level) == 4);
    assert!(offset_of!(NV_ENC_CONFIG_H264, adaptiveTransformMode) == 32);
    assert!(offset_of!(NV_ENC_CONFIG_H264, maxNumRefFrames) == 60);
    assert!(offset_of!(NV_ENC_CONFIG_H264, sliceMode) == 64);
    assert!(offset_of!(NV_ENC_CONFIG_H264, h264VUIParameters) == 72);
    assert!(offset_of!(NV_ENC_CONFIG_H264, ltrNumFrames) == 184);
    assert!(offset_of!(NV_ENC_CONFIG_H264, numRefL1) == 208);
    assert!(offset_of!(NV_ENC_CONFIG_H264, reserved1) == 212);
    assert!(offset_of!(NV_ENC_CONFIG_H264, reserved2) == 1280);
    assert!(size_of::<NV_ENC_CONFIG_HEVC>() == 1560);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, flags) == 16);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, idrPeriod) == 20);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, maxNumRefFramesInDPB) == 32);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, sliceMode) == 52);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, maxTemporalLayersMinus1) == 60);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, hevcVUIParameters) == 64);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, ltrTrustMode) == 176);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, numRefL0) == 184);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, reserved1) == 192);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, reserved2) == 1048);
    assert!(size_of::<NV_ENC_CONFIG>() == 3584);
    assert!(offset_of!(NV_ENC_CONFIG, profileGUID) == 4);
    assert!(offset_of!(NV_ENC_CONFIG, gopLength) == 20);
    assert!(offset_of!(NV_ENC_CONFIG, mvPrecision) == 36);
    assert!(offset_of!(NV_ENC_CONFIG, rcParams) == 40);
    assert!(offset_of!(NV_ENC_CONFIG, h264) == 168);
    assert!(offset_of!(NV_ENC_CONFIG, reserved) == 1960);
    assert!(offset_of!(NV_ENC_CONFIG, reserved2) == 3072);
    assert!(size_of::<NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE>() == 16);
    assert!(size_of::<NV_ENC_INITIALIZE_PARAMS>() == 1808);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeGUID) == 4);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, presetGUID) == 20);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeWidth) == 36);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, enablePTD) == 64);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, privDataSize) == 72);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, privData) == 80);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeConfig) == 88);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, maxEncodeWidth) == 96);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, maxMEHintCountsPerBlock) == 104);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, tuningInfo) == 136);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, bufferFormat) == 140);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, reserved) == 144);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, reserved2) == 1296);
    assert!(size_of::<NV_ENC_RECONFIGURE_PARAMS>() == 1824);
    assert!(offset_of!(NV_ENC_RECONFIGURE_PARAMS, reInitEncodeParams) == 8);
    assert!(offset_of!(NV_ENC_RECONFIGURE_PARAMS, flags) == 1816);
    assert!(size_of::<NV_ENC_PRESET_CONFIG>() == 5128);
    assert!(offset_of!(NV_ENC_PRESET_CONFIG, presetCfg) == 8);
    assert!(offset_of!(NV_ENC_PRESET_CONFIG, reserved1) == 3592);
    assert!(offset_of!(NV_ENC_PRESET_CONFIG, reserved2) == 4616);
    assert!(size_of::<NV_ENC_PIC_PARAMS>() == 3344);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, inputTimeStamp) == 24);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, inputBuffer) == 40);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, outputBitstream) == 48);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, bufferFmt) == 64);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, pictureType) == 72);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, codecPicParams) == 80);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, meHintCountsPerBlock) == 1616);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, meExternalHints) == 1648);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, reserved1) == 1656);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, reserved2) == 1680);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, qpDeltaMap) == 1696);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, qpDeltaMapSize) == 1704);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, meHintRefPicDist) == 1712);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, alphaBuffer) == 1720);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, reserved3) == 1728);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, reserved4) == 2872);
    assert!(size_of::<NV_ENC_LOCK_BITSTREAM>() == 1544);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputBitstream) == 8);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, sliceOffsets) == 16);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, frameIdx) == 24);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes) == 36);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputTimeStamp) == 40);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr) == 56);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, pictureType) == 64);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, frameAvgQP) == 72);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, reserved) == 92);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, intraMBCount) == 140);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, alphaLayerSizeInBytes) == 156);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, reserved1) == 160);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, reserved2) == 1032);
    assert!(size_of::<NV_ENC_LOCK_INPUT_BUFFER>() == 1544);
    assert!(offset_of!(NV_ENC_LOCK_INPUT_BUFFER, inputBuffer) == 8);
    assert!(offset_of!(NV_ENC_LOCK_INPUT_BUFFER, bufferDataPtr) == 16);
    assert!(offset_of!(NV_ENC_LOCK_INPUT_BUFFER, pitch) == 24);
    assert!(offset_of!(NV_ENC_LOCK_INPUT_BUFFER, reserved2) == 1032);
    assert!(size_of::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>() == 1552);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, device) == 8);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, apiVersion) == 24);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, reserved2) == 1040);
    assert!(size_of::<NV_ENCODE_API_FUNCTION_LIST>() == 2552);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSession) == 8);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeGUIDs) == 40);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeCaps) == 64);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncInitializeEncoder) == 96);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncEncodePicture) == 136);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockInputBuffer) == 168);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyEncoder) == 224);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSessionEx) == 240);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncReconfigureEncoder) == 264);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetLastErrorString) == 304);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfigEx) == 320);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, reserved2) == 336);
    assert!(size_of::<CUVIDEOFORMAT>() == 64);
    assert!(offset_of!(CUVIDEOFORMAT, progressive_sequence) == 12);
    assert!(offset_of!(CUVIDEOFORMAT, min_num_decode_surfaces) == 15);
    assert!(offset_of!(CUVIDEOFORMAT, coded_width) == 16);
    assert!(offset_of!(CUVIDEOFORMAT, display_area) == 24);
    assert!(offset_of!(CUVIDEOFORMAT, chroma_format) == 40);
    assert!(offset_of!(CUVIDEOFORMAT, bitrate) == 44);
    assert!(offset_of!(CUVIDEOFORMAT, display_aspect_ratio) == 48);
    assert!(offset_of!(CUVIDEOFORMAT, video_signal_description) == 56);
    assert!(offset_of!(CUVIDEOFORMAT, seqhdr_data_length) == 60);
    assert!(size_of::<CUVIDSOURCEDATAPACKET>() == 32);
    assert!(offset_of!(CUVIDSOURCEDATAPACKET, payload) == 16);
    assert!(offset_of!(CUVIDSOURCEDATAPACKET, timestamp) == 24);
    assert!(size_of::<CUVIDPARSERDISPINFO>() == 24);
    assert!(offset_of!(CUVIDPARSERDISPINFO, timestamp) == 16);
    assert!(size_of::<CUVIDPARSERPARAMS>() == 136);
    assert!(offset_of!(CUVIDPARSERPARAMS, ulMaxDisplayDelay) == 16);
    assert!(offset_of!(CUVIDPARSERPARAMS, uReserved1) == 24);
    assert!(offset_of!(CUVIDPARSERPARAMS, pUserData) == 40);
    assert!(offset_of!(CUVIDPARSERPARAMS, pfnSequenceCallback) == 48);
    assert!(offset_of!(CUVIDPARSERPARAMS, pfnGetOperatingPoint) == 72);
    assert!(offset_of!(CUVIDPARSERPARAMS, pvReserved2) == 80);
    assert!(offset_of!(CUVIDPARSERPARAMS, pExtVideoInfo) == 128);
    assert!(size_of::<CUVIDDECODECAPS>() == 88);
    assert!(offset_of!(CUVIDDECODECAPS, bIsSupported) == 24);
    assert!(offset_of!(CUVIDDECODECAPS, nOutputFormatMask) == 26);
    assert!(offset_of!(CUVIDDECODECAPS, nMaxWidth) == 28);
    assert!(offset_of!(CUVIDDECODECAPS, nMinWidth) == 40);
    assert!(offset_of!(CUVIDDECODECAPS, reserved3) == 48);
    assert!(size_of::<CUVIDDECODECREATEINFO>() == 176);
    assert!(offset_of!(CUVIDDECODECREATEINFO, CodecType) == 24);
    assert!(offset_of!(CUVIDDECODECREATEINFO, ulCreationFlags) == 32);
    assert!(offset_of!(CUVIDDECODECREATEINFO, Reserved1) == 72);
    assert!(offset_of!(CUVIDDECODECREATEINFO, display_area) == 80);
    assert!(offset_of!(CUVIDDECODECREATEINFO, OutputFormat) == 88);
    assert!(offset_of!(CUVIDDECODECREATEINFO, ulTargetWidth) == 96);
    assert!(offset_of!(CUVIDDECODECREATEINFO, vidLock) == 120);
    assert!(offset_of!(CUVIDDECODECREATEINFO, target_rect) == 128);
    assert!(offset_of!(CUVIDDECODECREATEINFO, enableHistogram) == 136);
    assert!(offset_of!(CUVIDDECODECREATEINFO, Reserved2) == 144);
    assert!(size_of::<CUVIDPROCPARAMS>() == 264);
    assert!(offset_of!(CUVIDPROCPARAMS, raw_input_dptr) == 24);
    assert!(offset_of!(CUVIDPROCPARAMS, raw_output_dptr) == 40);
    assert!(offset_of!(CUVIDPROCPARAMS, output_stream) == 56);
    assert!(offset_of!(CUVIDPROCPARAMS, Reserved) == 64);
    assert!(offset_of!(CUVIDPROCPARAMS, histogram_dptr) == 248);
    assert!(offset_of!(CUVIDPROCPARAMS, Reserved2) == 256);
    assert!(size_of::<CUDA_MEMCPY2D>() == 128);
    assert!(offset_of!(CUDA_MEMCPY2D, srcMemoryType) == 16);
    assert!(offset_of!(CUDA_MEMCPY2D, srcHost) == 24);
    assert!(offset_of!(CUDA_MEMCPY2D, srcDevice) == 32);
    assert!(offset_of!(CUDA_MEMCPY2D, srcPitch) == 48);
    assert!(offset_of!(CUDA_MEMCPY2D, dstMemoryType) == 72);
    assert!(offset_of!(CUDA_MEMCPY2D, dstHost) == 80);
    assert!(offset_of!(CUDA_MEMCPY2D, dstPitch) == 104);
    assert!(offset_of!(CUDA_MEMCPY2D, WidthInBytes) == 112);
    assert!(offset_of!(CUDA_MEMCPY2D, Height) == 120);
    assert!(NV_ENC_CONFIG_VER == 0xf107000b);
    assert!(NV_ENC_INITIALIZE_PARAMS_VER == 0xf105000b);
    assert!(NV_ENC_RECONFIGURE_PARAMS_VER == 0xf101000b);
    assert!(NV_ENC_PRESET_CONFIG_VER == 0xf104000b);
    assert!(NV_ENC_PIC_PARAMS_VER == 0xf104000b);
    assert!(NV_ENC_LOCK_BITSTREAM_VER == 0x7101000b);
    assert!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER == 0x7101000b);
    assert!(NV_ENCODE_API_FUNCTION_LIST_VER == 0x7102000b);
};
