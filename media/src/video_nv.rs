//! Video on the graphics chip through NVIDIA's own ways to it: NVENC to
//! encode, NVDEC to decode — Linux, on NVIDIA's chips with NVIDIA's
//! driver (VA-API doesn't reach them but through wrappers). The
//! libraries (`libcuda`, `libnvidia-encode`, `libnvcuvid`) come with the
//! driver, and are loaded when wanted, as libva is, so nothing more is
//! linked and a system without them just has OpenH264.
//!
//! - **Encoding**: H.264 Constrained Baseline, or H.265 Main, one slice,
//!   one reference, every picture a frame out at once (the "ultra low
//!   latency" tuning).
//!   Swarm chooses each picture's type — a full frame, with its SPS and
//!   PPS, when asked; the encoder never on its own — keeps the bitrate
//!   constant, and changes it in place.
//! - **Decoding**: NVIDIA's parser reads the stream and drives the
//!   decoder, with no display delay: each frame is said to be a whole
//!   picture, so it comes out at once.
//!
//! Everything runs in the chip's primary CUDA context, made current on
//! whichever thread is using it. `SWARM_NV_DEBUG=1` says what goes to and
//! from the driver; `SWARM_NVIDIA=0` leaves NVIDIA's chip alone (VA-API
//! is tried instead).

use crate::common::{i420_from_nv12_mapped, limited_range_tables};
use crate::nv_sys::*;
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr};
use swarm_core::video::{Codec, HardDecoder, HardEncoder, Hardware, Picture, Shape};

fn debugging() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SWARM_NV_DEBUG").is_ok_and(|v| v == "1"))
}

/// The CUDA driver's functions, from libcuda.so.1.
struct Cuda {
    init: unsafe extern "C" fn(c_uint) -> CUresult,
    driver_get_version: unsafe extern "C" fn(*mut c_int) -> CUresult,
    device_get_count: unsafe extern "C" fn(*mut c_int) -> CUresult,
    device_get: unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult,
    device_get_name: unsafe extern "C" fn(*mut c_char, c_int, CUdevice) -> CUresult,
    primary_ctx_retain: unsafe extern "C" fn(*mut CUcontext, CUdevice) -> CUresult,
    primary_ctx_set_flags: unsafe extern "C" fn(CUdevice, c_uint) -> CUresult,
    ctx_push_current: unsafe extern "C" fn(CUcontext) -> CUresult,
    ctx_pop_current: unsafe extern "C" fn(*mut CUcontext) -> CUresult,
    memcpy_2d: unsafe extern "C" fn(*const CUDA_MEMCPY2D) -> CUresult,
    mem_alloc_host: unsafe extern "C" fn(*mut *mut c_void, usize) -> CUresult,
    mem_free_host: unsafe extern "C" fn(*mut c_void) -> CUresult,
    get_error_name: unsafe extern "C" fn(CUresult, *mut *const c_char) -> CUresult,
    _library: libloading::Library,
}

/// NVENC's: the function list it fills in.
struct Nvenc {
    api: NV_ENCODE_API_FUNCTION_LIST,
    _library: libloading::Library,
}

/// NVDEC's, from libnvcuvid.so.1.
struct Cuvid {
    get_decoder_caps: unsafe extern "C" fn(*mut CUVIDDECODECAPS) -> CUresult,
    create_decoder: unsafe extern "C" fn(*mut CUvideodecoder, *mut CUVIDDECODECREATEINFO) -> CUresult,
    destroy_decoder: unsafe extern "C" fn(CUvideodecoder) -> CUresult,
    decode_picture: unsafe extern "C" fn(CUvideodecoder, *mut CUVIDPICPARAMS) -> CUresult,
    map_video_frame: unsafe extern "C" fn(CUvideodecoder, c_int, *mut u64, *mut c_uint, *mut CUVIDPROCPARAMS) -> CUresult,
    unmap_video_frame: unsafe extern "C" fn(CUvideodecoder, u64) -> CUresult,
    create_video_parser: unsafe extern "C" fn(*mut CUvideoparser, *mut CUVIDPARSERPARAMS) -> CUresult,
    parse_video_data: unsafe extern "C" fn(CUvideoparser, *mut CUVIDSOURCEDATAPACKET) -> CUresult,
    destroy_video_parser: unsafe extern "C" fn(CUvideoparser) -> CUresult,
    _library: libloading::Library,
}

macro_rules! f {
    ($lib:expr, $name:literal) => {
        *$lib.get(concat!($name, "\0").as_bytes()).with_context(|| format!("{} missing", $name))?
    };
}

impl Cuda {
    unsafe fn load() -> Result<Cuda> {
        let lib = libloading::Library::new("libcuda.so.1").context("no libcuda (NVIDIA's driver isn't installed)")?;
        Ok(Cuda {
            init: f!(lib, "cuInit"),
            driver_get_version: f!(lib, "cuDriverGetVersion"),
            device_get_count: f!(lib, "cuDeviceGetCount"),
            device_get: f!(lib, "cuDeviceGet"),
            device_get_name: f!(lib, "cuDeviceGetName"),
            primary_ctx_retain: f!(lib, "cuDevicePrimaryCtxRetain"),
            primary_ctx_set_flags: f!(lib, "cuDevicePrimaryCtxSetFlags"),
            ctx_push_current: f!(lib, "cuCtxPushCurrent_v2"),
            ctx_pop_current: f!(lib, "cuCtxPopCurrent_v2"),
            memcpy_2d: f!(lib, "cuMemcpy2D_v2"),
            mem_alloc_host: f!(lib, "cuMemAllocHost_v2"),
            mem_free_host: f!(lib, "cuMemFreeHost"),
            get_error_name: f!(lib, "cuGetErrorName"),
            _library: lib,
        })
    }

    /// `result` as an error, saying what was being done.
    fn check(&self, result: CUresult, what: &str) -> Result<()> {
        if result == CUDA_SUCCESS {
            return Ok(());
        }
        let mut name = std::ptr::null();
        let name = if unsafe { (self.get_error_name)(result, &mut name) } == CUDA_SUCCESS && !name.is_null() { unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned() } else { "error".to_string() };
        Err(anyhow!("{what}: {name} ({result})"))
    }
}

impl Nvenc {
    unsafe fn load() -> Result<Nvenc> {
        let lib = libloading::Library::new("libnvidia-encode.so.1").context("no libnvidia-encode")?;
        let most: unsafe extern "C" fn(*mut u32) -> NVENCSTATUS = *lib.get(b"NvEncodeAPIGetMaxSupportedVersion\0").context("NvEncodeAPIGetMaxSupportedVersion missing")?;
        let create: unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS = *lib.get(b"NvEncodeAPICreateInstance\0").context("NvEncodeAPICreateInstance missing")?;
        let mut version = 0;
        if most(&mut version) != NV_ENC_SUCCESS {
            bail!("NVENC wouldn't say its version");
        }
        let (major, minor) = (version >> 4, version & 0xf);
        if (major, minor) < (NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION) {
            bail!("NVENC {major}.{minor} is older than {NVENCAPI_MAJOR_VERSION}.{NVENCAPI_MINOR_VERSION} (a driver from 470 on is needed)");
        }
        let mut api = NV_ENCODE_API_FUNCTION_LIST::zeroed();
        api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        let status = create(&mut api);
        if status != NV_ENC_SUCCESS {
            bail!("NVENC wouldn't start: {}", status_name(status));
        }
        Ok(Nvenc { api, _library: lib })
    }
}

impl Cuvid {
    unsafe fn load() -> Result<Cuvid> {
        let lib = libloading::Library::new("libnvcuvid.so.1").context("no libnvcuvid")?;
        Ok(Cuvid {
            get_decoder_caps: f!(lib, "cuvidGetDecoderCaps"),
            create_decoder: f!(lib, "cuvidCreateDecoder"),
            destroy_decoder: f!(lib, "cuvidDestroyDecoder"),
            decode_picture: f!(lib, "cuvidDecodePicture"),
            map_video_frame: f!(lib, "cuvidMapVideoFrame64"),
            unmap_video_frame: f!(lib, "cuvidUnmapVideoFrame64"),
            create_video_parser: f!(lib, "cuvidCreateVideoParser"),
            parse_video_data: f!(lib, "cuvidParseVideoData"),
            destroy_video_parser: f!(lib, "cuvidDestroyVideoParser"),
            _library: lib,
        })
    }
}

fn status_name(status: NVENCSTATUS) -> String {
    NV_ENC_STATUS_NAMES.get(status as usize).map_or_else(|| format!("status {status}"), |name| format!("{name} ({status})"))
}

/// NVIDIA's chip, opened once for the whole process.
struct Device {
    cuda: Cuda,
    nvenc: Option<Nvenc>,
    cuvid: Option<Cuvid>,
    context: CUcontext,
    /// The chip's name ("NVIDIA GeForce RTX 4060").
    name: String,
    /// Why it doesn't encode or decode, when it doesn't.
    no_encoding: Option<String>,
    no_decoding: Option<String>,
    /// NVDEC decodes H.265 (8-bit 4:2:0).
    decodes_h265: bool,
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

/// The context made current on this thread, until dropped.
struct Current<'a>(&'a Device);

impl Drop for Current<'_> {
    fn drop(&mut self) {
        let mut popped = std::ptr::null_mut();
        unsafe { (self.0.cuda.ctx_pop_current)(&mut popped) };
    }
}

impl Device {
    fn current(&self) -> Result<Current<'_>> {
        self.cuda.check(unsafe { (self.cuda.ctx_push_current)(self.context) }, "making the chip's context current")?;
        Ok(Current(self))
    }

    unsafe fn open() -> Result<Device> {
        let cuda = Cuda::load()?;
        cuda.check((cuda.init)(0), "starting CUDA")?;
        let mut count = 0;
        cuda.check((cuda.device_get_count)(&mut count), "counting NVIDIA's chips")?;
        let index = std::env::var("SWARM_NV_DEVICE").ok().and_then(|i| i.parse().ok()).unwrap_or(0);
        if index >= count {
            bail!("no NVIDIA chip{}", if count > 0 { format!(" {index} (there are {count})") } else { String::new() });
        }
        let mut device = 0;
        cuda.check((cuda.device_get)(&mut device, index), "finding the chip")?;
        let mut name = [0 as c_char; 100];
        cuda.check((cuda.device_get_name)(name.as_mut_ptr(), name.len() as c_int, device), "naming the chip")?;
        let name = CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned();
        let blocking = (cuda.primary_ctx_set_flags)(device, CU_CTX_SCHED_BLOCKING_SYNC);
        let mut context = std::ptr::null_mut();
        cuda.check((cuda.primary_ctx_retain)(&mut context, device), "making a context on the chip")?;
        if debugging() && blocking != CUDA_SUCCESS {
            eprintln!("nv: the context was made already: waits spin");
        }
        let (nvenc, no_encoding) = match Nvenc::load() {
            Ok(nvenc) => (Some(nvenc), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let (cuvid, no_decoding) = match Cuvid::load() {
            Ok(cuvid) => (Some(cuvid), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let mut device = Device { cuda, nvenc, cuvid, context, name, no_encoding, no_decoding, decodes_h265: false };
        if let Some(cuvid) = &device.cuvid {
            let current = device.current()?;
            let mut caps = CUVIDDECODECAPS::zeroed();
            caps.eCodecType = cudaVideoCodec_H264;
            caps.eChromaFormat = cudaVideoChromaFormat_420;
            let asked = device.cuda.check((cuvid.get_decoder_caps)(&mut caps), "asking what NVDEC decodes");
            let mut hevc = CUVIDDECODECAPS::zeroed();
            hevc.eCodecType = cudaVideoCodec_HEVC;
            hevc.eChromaFormat = cudaVideoChromaFormat_420;
            let decodes_h265 = (cuvid.get_decoder_caps)(&mut hevc) == CUDA_SUCCESS && hevc.bIsSupported != 0 && hevc.nOutputFormatMask & 1 != 0;
            drop(current);
            device.decodes_h265 = decodes_h265;
            match asked {
                Err(e) => device.no_decoding = Some(format!("{e:#}")),
                Ok(()) if caps.bIsSupported == 0 => device.no_decoding = Some("its NVDEC doesn't decode H.264".to_string()),
                Ok(()) => {}
            }
        }
        if device.no_decoding.is_some() {
            device.cuvid = None;
        }
        if debugging() {
            let mut version = 0;
            (device.cuda.driver_get_version)(&mut version);
            eprintln!("nv: {} (CUDA {}.{}), encoding {:?}, decoding {:?} (H.265 {})", device.name, version / 1000, version % 1000 / 10, device.no_encoding.as_deref().unwrap_or("yes"), device.no_decoding.as_deref().unwrap_or("yes"), device.decodes_h265);
        }
        if device.nvenc.is_none() && device.cuvid.is_none() {
            bail!("{}: neither NVENC ({}) nor NVDEC ({})", device.name, device.no_encoding.unwrap_or_default(), device.no_decoding.unwrap_or_default());
        }
        Ok(device)
    }

    /// Copies a picture `width`×`height` (rows `pitch` apart) off the
    /// chip, into `out` (rows `width` apart).
    unsafe fn copy_down(&self, from: CUdeviceptr, pitch: usize, width: usize, height: usize, out: &mut [u8]) -> Result<()> {
        assert!(out.len() >= width * height);
        let mut copy = CUDA_MEMCPY2D::zeroed();
        copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
        copy.srcDevice = from;
        copy.srcPitch = pitch;
        copy.dstMemoryType = CU_MEMORYTYPE_HOST;
        copy.dstHost = out.as_mut_ptr() as *mut c_void;
        copy.dstPitch = width;
        copy.WidthInBytes = width;
        copy.Height = height;
        self.cuda.check((self.cuda.memcpy_2d)(&copy), "copying the picture off the chip")
    }
}

/// NVIDIA's hardware, for `swarm_core::video::provide_hardware`.
pub struct Nvidia {
    device: &'static Device,
}

impl Nvidia {
    /// NVIDIA's chip (the first, or the one `SWARM_NV_DEVICE` numbers),
    /// opened once for the process.
    pub fn find() -> Result<Nvidia> {
        static FOUND: std::sync::OnceLock<Result<&'static Device, String>> = std::sync::OnceLock::new();
        if std::env::var("SWARM_NVIDIA").is_ok_and(|v| v == "0") {
            bail!("left alone (SWARM_NVIDIA=0)");
        }
        let device = FOUND.get_or_init(|| unsafe { Device::open() }.map(|d| &*Box::leak(Box::new(d))).map_err(|e| format!("{e:#}")));
        device.as_ref().map(|&device| Nvidia { device }).map_err(|e| anyhow!("{e}"))
    }
}

impl Hardware for Nvidia {
    fn encoder(&self, shape: Shape, bitrate: u32, codec: Codec) -> Result<Box<dyn HardEncoder>> {
        let nvenc = self.device.nvenc.as_ref().ok_or_else(|| anyhow!("{}: no NVENC ({})", self.device.name, self.device.no_encoding.as_deref().unwrap_or_default()))?;
        Ok(Box::new(unsafe { NvEncoder::open(self.device, nvenc, shape, bitrate, codec) }?))
    }

    fn decoder(&self, codec: Codec) -> Result<Box<dyn HardDecoder>> {
        let cuvid = self.device.cuvid.as_ref().ok_or_else(|| anyhow!("{}: no NVDEC ({})", self.device.name, self.device.no_decoding.as_deref().unwrap_or_default()))?;
        let codec = match codec {
            Codec::H264 => cudaVideoCodec_H264,
            Codec::H265 if self.device.decodes_h265 => cudaVideoCodec_HEVC,
            Codec::H265 => bail!("{}'s NVDEC doesn't decode H.265", self.device.name),
        };
        Ok(Box::new(unsafe { NvDecoder::new(self.device, cuvid, codec) }?))
    }

    fn h265(&self) -> (bool, bool) {
        static ENCODES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let encodes = *ENCODES.get_or_init(|| {
            let shape = Shape { width: 1280, height: 720, fps: 25 };
            let opened = self.device.nvenc.as_ref().map(|nvenc| unsafe { NvEncoder::open(self.device, nvenc, shape, 1_000_000, Codec::H265) });
            if let (true, Some(Err(e))) = (debugging(), &opened) {
                eprintln!("nv: no H.265 encoding: {e:#}");
            }
            opened.is_some_and(|o| o.is_ok())
        });
        (encodes, self.device.decodes_h265)
    }

    fn name(&self) -> String {
        let ways = match (self.device.nvenc.is_some(), self.device.cuvid.is_some()) {
            (true, true) => "NVENC and NVDEC",
            (true, false) => "NVENC",
            _ => "NVDEC",
        };
        format!("{ways} ({})", self.device.name)
    }
}

/// The preset: P1 is quickest, P7 best. Any is quick enough on the chip
/// for a call (see the handoff's measurements); `SWARM_NV_PRESET=1`…`4`
/// tries another.
fn preset() -> GUID {
    match std::env::var("SWARM_NV_PRESET").ok().as_deref() {
        Some("1") => NV_ENC_PRESET_P1_GUID,
        Some("2") => NV_ENC_PRESET_P2_GUID,
        Some("3") => NV_ENC_PRESET_P3_GUID,
        _ => NV_ENC_PRESET_P4_GUID,
    }
}

struct NvEncoder {
    device: &'static Device,
    api: &'static NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    input: *mut c_void,
    output: *mut c_void,
    shape: Shape,
    codec: Codec,
    bitrate: u32,
    /// What it was set up with (pointing to `config`), kept for changing
    /// the bitrate.
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    config: Box<NV_ENC_CONFIG>,
    /// Pictures given.
    count: u32,
}

unsafe impl Send for NvEncoder {}

impl NvEncoder {
    /// `status` as an error, with what NVENC says of it.
    fn check(&self, status: NVENCSTATUS, what: &str) -> Result<()> {
        if status == NV_ENC_SUCCESS {
            return Ok(());
        }
        let said = match (self.encoder.is_null(), self.api.nvEncGetLastErrorString) {
            (false, Some(last)) => unsafe { CStr::from_ptr(last(self.encoder)) }.to_string_lossy().trim().to_string(),
            _ => String::new(),
        };
        Err(anyhow!("{what}: {}{}", status_name(status), if said.is_empty() { String::new() } else { format!(" — {said}") }))
    }

    unsafe fn open(device: &'static Device, nvenc: &'static Nvenc, shape: Shape, bitrate: u32, codec: Codec) -> Result<NvEncoder> {
        let guid = match codec {
            Codec::H264 => NV_ENC_CODEC_H264_GUID,
            Codec::H265 => NV_ENC_CODEC_HEVC_GUID,
        };
        let _current = device.current()?;
        let api = &nvenc.api;
        let missing = || anyhow!("NVENC without a function Swarm needs");
        let mut encoder = NvEncoder {
            device,
            api,
            encoder: std::ptr::null_mut(),
            input: std::ptr::null_mut(),
            output: std::ptr::null_mut(),
            shape,
            codec,
            bitrate,
            init: Box::new(NV_ENC_INITIALIZE_PARAMS::zeroed()),
            config: Box::new(NV_ENC_CONFIG::zeroed()),
            count: 0,
        };
        let mut session = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS::zeroed();
        session.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        session.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
        session.device = device.context;
        session.apiVersion = NVENCAPI_VERSION;
        let status = (api.nvEncOpenEncodeSessionEx.ok_or_else(missing)?)(&mut session, &mut encoder.encoder);
        if status != NV_ENC_SUCCESS {
            if let (false, Some(destroy)) = (encoder.encoder.is_null(), api.nvEncDestroyEncoder) {
                destroy(encoder.encoder);
            }
            encoder.encoder = std::ptr::null_mut();
            if status == NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY {
                bail!("{} has as many NVENC sessions open as it allows", device.name);
            }
            bail!("starting an NVENC session: {}", status_name(status));
        }
        let mut count = 0;
        encoder.check((api.nvEncGetEncodeGUIDCount.ok_or_else(missing)?)(encoder.encoder, &mut count), "asking what it encodes")?;
        let mut codecs = vec![GUID::default(); count as usize];
        encoder.check((api.nvEncGetEncodeGUIDs.ok_or_else(missing)?)(encoder.encoder, codecs.as_mut_ptr(), count, &mut count), "asking what it encodes")?;
        if !codecs[..count as usize].contains(&guid) {
            bail!("{}'s NVENC doesn't encode {}", device.name, codec.name());
        }
        let cap = |which: c_int| {
            let mut param = NV_ENC_CAPS_PARAM::zeroed();
            param.version = NV_ENC_CAPS_PARAM_VER;
            param.capsToQuery = which;
            let mut value = 0;
            api.nvEncGetEncodeCaps.map(|get| get(encoder.encoder, guid, &mut param, &mut value));
            value
        };
        let (most_wide, most_high) = (cap(NV_ENC_CAPS_WIDTH_MAX), cap(NV_ENC_CAPS_HEIGHT_MAX));
        if (most_wide > 0 && shape.width as c_int > most_wide) || (most_high > 0 && shape.height as c_int > most_high) {
            bail!("{}×{} is bigger than NVENC takes ({most_wide}×{most_high})", shape.width, shape.height);
        }
        let mut given = Box::new(NV_ENC_PRESET_CONFIG::zeroed());
        given.version = NV_ENC_PRESET_CONFIG_VER;
        given.presetCfg.version = NV_ENC_CONFIG_VER;
        let tuning = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        encoder.check((api.nvEncGetEncodePresetConfigEx.ok_or_else(missing)?)(encoder.encoder, guid, preset(), tuning, &mut *given), "getting its preset")?;
        let mut config = given.presetCfg;
        config.version = NV_ENC_CONFIG_VER;
        config.profileGUID = match codec {
            Codec::H264 => NV_ENC_H264_PROFILE_BASELINE_GUID,
            Codec::H265 => NV_ENC_HEVC_PROFILE_MAIN_GUID,
        };
        config.gopLength = NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = 1;
        config.frameFieldMode = NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
        config.rcParams.version = NV_ENC_RC_PARAMS_VER;
        config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
        config.rcParams.flags = 0;
        config.rcParams.lookaheadDepth = 0;
        config.rcParams.multiPass = 0;
        set_rate(&mut config, bitrate, shape.fps);
        match codec {
            Codec::H264 => {
                let h264 = &mut config.h264;
                h264.flags = (h264.flags | H264_REPEAT_SPSPPS) & !(H264_OUTPUT_AUD | H264_DISABLE_SPSPPS | H264_ENABLE_INTRA_REFRESH | H264_ENABLE_LTR | H264_ENABLE_FILLER_DATA);
                h264.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                h264.entropyCodingMode = NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC;
                h264.adaptiveTransformMode = NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE;
                h264.fmoMode = NV_ENC_H264_FMO_DISABLE;
                h264.bdirectMode = NV_ENC_H264_BDIRECT_MODE_DISABLE;
                h264.maxNumRefFrames = 1;
                h264.numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
                h264.numRefL1 = 0;
                h264.chromaFormatIDC = 1;
                h264.sliceMode = 0;
                h264.sliceModeData = 0;
                h264.level = 0;
                h264.numTemporalLayers = 0;
                h264.h264VUIParameters.bitstreamRestrictionFlag = 1;
            }
            Codec::H265 => {
                let hevc = config.hevc();
                hevc.flags = (hevc.flags | HEVC_REPEAT_SPSPPS) & !(HEVC_OUTPUT_AUD | HEVC_DISABLE_SPSPPS | HEVC_ENABLE_INTRA_REFRESH | HEVC_ENABLE_LTR | HEVC_ENABLE_FILLER_DATA | HEVC_CHROMA_MASK | HEVC_DEPTH_MASK) | HEVC_CHROMA_420;
                hevc.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                hevc.maxNumRefFramesInDPB = 1;
                hevc.numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
                hevc.numRefL1 = 0;
                hevc.sliceMode = 0;
                hevc.sliceModeData = 0;
                hevc.level = 0;
                hevc.maxTemporalLayersMinus1 = 0;
                hevc.hevcVUIParameters.bitstreamRestrictionFlag = 1;
            }
        }
        *encoder.config = config;
        let init = &mut *encoder.init;
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = guid;
        init.presetGUID = preset();
        init.encodeWidth = shape.width as u32;
        init.encodeHeight = shape.height as u32;
        init.darWidth = shape.width as u32;
        init.darHeight = shape.height as u32;
        init.frameRateNum = shape.fps;
        init.frameRateDen = 1;
        init.enablePTD = 1;
        init.encodeConfig = &mut *encoder.config;
        init.maxEncodeWidth = shape.width as u32;
        init.maxEncodeHeight = shape.height as u32;
        init.tuningInfo = tuning;
        init.bufferFormat = NV_ENC_BUFFER_FORMAT_IYUV;
        let status = (api.nvEncInitializeEncoder.ok_or_else(missing)?)(encoder.encoder, &mut *encoder.init);
        encoder.check(status, "setting up the encoder")?;
        let mut input = NV_ENC_CREATE_INPUT_BUFFER::zeroed();
        input.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
        input.width = shape.width as u32;
        input.height = shape.height as u32;
        input.memoryHeap = NV_ENC_MEMORY_HEAP_AUTOSELECT;
        input.bufferFmt = NV_ENC_BUFFER_FORMAT_IYUV;
        encoder.check((api.nvEncCreateInputBuffer.ok_or_else(missing)?)(encoder.encoder, &mut input), "making its input buffer")?;
        encoder.input = input.inputBuffer;
        let mut output = NV_ENC_CREATE_BITSTREAM_BUFFER::zeroed();
        output.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        encoder.check((api.nvEncCreateBitstreamBuffer.ok_or_else(missing)?)(encoder.encoder, &mut output), "making its output buffer")?;
        encoder.output = output.bitstreamBuffer;
        if debugging() {
            eprintln!("nv: {} encoder {}×{} at {} fps, {} kbit/s, preset P{}", codec.name(), shape.width, shape.height, shape.fps, bitrate / 1000, std::env::var("SWARM_NV_PRESET").unwrap_or_else(|_| "4".to_string()));
        }
        Ok(encoder)
    }

    /// The picture into its input buffer: I420, rows `pitch` apart, and
    /// its chroma's half as far.
    unsafe fn upload(&self, picture: &Picture) -> Result<()> {
        let missing = || anyhow!("NVENC without a function Swarm needs");
        let mut lock = NV_ENC_LOCK_INPUT_BUFFER::zeroed();
        lock.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
        lock.inputBuffer = self.input;
        self.check((self.api.nvEncLockInputBuffer.ok_or_else(missing)?)(self.encoder, &mut lock), "getting at its input buffer")?;
        let (width, height, pitch) = (self.shape.width, self.shape.height, lock.pitch as usize);
        let (half_pitch, half_height) = (pitch.div_ceil(2), height.div_ceil(2));
        let written = (|| {
            if pitch < width || lock.bufferDataPtr.is_null() {
                bail!("an input buffer too small for the picture (pitch {pitch})");
            }
            let picture = picture.to_i420();
            let (y, u, v) = picture.planes();
            let buffer = std::slice::from_raw_parts_mut(lock.bufferDataPtr as *mut u8, pitch * height + 2 * half_pitch * half_height);
            let (luma, chroma) = buffer.split_at_mut(pitch * height);
            let (cb, cr) = chroma.split_at_mut(half_pitch * half_height);
            for row in 0..height {
                luma[row * pitch..][..width].copy_from_slice(&y[row * width..][..width]);
            }
            for row in 0..height / 2 {
                cb[row * half_pitch..][..width / 2].copy_from_slice(&u[row * (width / 2)..][..width / 2]);
                cr[row * half_pitch..][..width / 2].copy_from_slice(&v[row * (width / 2)..][..width / 2]);
            }
            Ok(lock.pitch)
        })();
        let unlocked = self.check((self.api.nvEncUnlockInputBuffer.ok_or_else(missing)?)(self.encoder, self.input), "letting go of its input buffer");
        written.and(unlocked)
    }

    unsafe fn encode(&mut self, picture: &Picture, key: bool) -> Result<Vec<u8>> {
        if (picture.width, picture.height) != (self.shape.width, self.shape.height) {
            bail!("a {}×{} picture for a {}×{} encoder", picture.width, picture.height, self.shape.width, self.shape.height);
        }
        let missing = || anyhow!("NVENC without a function Swarm needs");
        let _current = self.device.current()?;
        self.upload(picture)?;
        let mut params = NV_ENC_PIC_PARAMS::zeroed();
        params.version = NV_ENC_PIC_PARAMS_VER;
        params.inputWidth = self.shape.width as u32;
        params.inputHeight = self.shape.height as u32;
        params.inputPitch = self.shape.width as u32;
        params.encodePicFlags = if key { NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS } else { 0 };
        params.frameIdx = self.count;
        params.inputTimeStamp = self.count as u64;
        params.inputBuffer = self.input;
        params.outputBitstream = self.output;
        params.bufferFmt = NV_ENC_BUFFER_FORMAT_IYUV;
        params.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        let status = (self.api.nvEncEncodePicture.ok_or_else(missing)?)(self.encoder, &mut params);
        self.count += 1;
        if status == NV_ENC_ERR_NEED_MORE_INPUT {
            return Ok(Vec::new());
        }
        self.check(status, "encoding")?;
        let mut lock = NV_ENC_LOCK_BITSTREAM::zeroed();
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.output;
        self.check((self.api.nvEncLockBitstream.ok_or_else(missing)?)(self.encoder, &mut lock), "getting what it coded")?;
        let frame = if lock.bitstreamBufferPtr.is_null() { Vec::new() } else { std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, lock.bitstreamSizeInBytes as usize).to_vec() };
        let picture_type = lock.pictureType;
        self.check((self.api.nvEncUnlockBitstream.ok_or_else(missing)?)(self.encoder, self.output), "letting go of what it coded")?;
        if debugging() {
            let kind = |b: &u8| if self.codec == Codec::H265 { b >> 1 & 0x3f } else { b & 0x1f };
            let kinds: Vec<u8> = crate::h264::nal_units(&frame).iter().filter_map(|n| n.first().map(kind)).collect();
            eprintln!("nv: picture {} (full {key}, type {picture_type}): {} bytes, NAL units {kinds:?}", self.count - 1, frame.len());
        }
        if key && picture_type != NV_ENC_PIC_TYPE_IDR {
            bail!("asked for a full frame, it coded a picture of type {picture_type}");
        }
        Ok(frame)
    }

    unsafe fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        if bitrate == self.bitrate {
            return Ok(());
        }
        let missing = || anyhow!("NVENC without a function Swarm needs");
        let _current = self.device.current()?;
        let was = self.config.rcParams;
        set_rate(&mut self.config, bitrate, self.shape.fps);
        let mut again = Box::new(NV_ENC_RECONFIGURE_PARAMS::zeroed());
        again.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        again.reInitEncodeParams = *self.init;
        again.reInitEncodeParams.encodeConfig = &mut *self.config;
        again.flags = 0;
        if let Err(e) = self.check((self.api.nvEncReconfigureEncoder.ok_or_else(missing)?)(self.encoder, &mut *again), "changing the bitrate") {
            self.config.rcParams = was;
            return Err(e);
        }
        if debugging() {
            eprintln!("nv: bitrate {} → {} kbit/s", self.bitrate / 1000, bitrate / 1000);
        }
        self.bitrate = bitrate;
        Ok(())
    }
}

/// A constant `bitrate`, its buffer a second's worth (as on VA-API).
fn set_rate(config: &mut NV_ENC_CONFIG, bitrate: u32, _fps: u32) {
    let rc = &mut config.rcParams;
    rc.averageBitRate = bitrate;
    rc.maxBitRate = bitrate;
    rc.vbvBufferSize = bitrate;
    rc.vbvInitialDelay = bitrate * 3 / 4;
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        unsafe {
            let _current = self.device.current();
            if let (false, Some(destroy)) = (self.input.is_null(), self.api.nvEncDestroyInputBuffer) {
                destroy(self.encoder, self.input);
            }
            if let (false, Some(destroy)) = (self.output.is_null(), self.api.nvEncDestroyBitstreamBuffer) {
                destroy(self.encoder, self.output);
            }
            if let Some(destroy) = self.api.nvEncDestroyEncoder {
                destroy(self.encoder);
            }
        }
    }
}

impl HardEncoder for NvEncoder {
    fn encode(&mut self, picture: &Picture, key: bool) -> Result<Vec<u8>> {
        unsafe { NvEncoder::encode(self, picture, key) }
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        unsafe { NvEncoder::set_bitrate(self, bitrate) }
    }

    fn name(&self) -> String {
        format!("NVENC ({})", self.device.name)
    }
}

/// What the parser's callbacks work on: the decoder, made when the
/// stream says its size, and the pictures it's said are ready.
struct Decoding {
    device: &'static Device,
    cuvid: &'static Cuvid,
    /// H.264, H.265, or JPEG (a camera's).
    codec: c_int,
    decoder: CUvideodecoder,
    /// The coded size it was made for, and the size shown (cropped).
    coded: (u32, u32),
    shown: (usize, usize),
    surfaces: c_int,
    /// Page-locked memory pictures are copied into: the chip copies to
    /// it itself, where into ordinary memory the processor copies too.
    host: Option<(*mut u8, usize)>,
    ready: Vec<c_int>,
    /// What went wrong in a callback, for `decode` to say.
    trouble: Option<String>,
}

impl Decoding {
    /// (In the chip's context.)
    fn end(&mut self) {
        if !self.decoder.is_null() {
            unsafe { (self.cuvid.destroy_decoder)(self.decoder) };
            self.decoder = std::ptr::null_mut();
        }
        if let Some((host, _)) = self.host.take() {
            unsafe { (self.device.cuda.mem_free_host)(host as *mut c_void) };
        }
    }

    /// A decoder for `format`'s pictures: how many surfaces it has.
    unsafe fn start(&mut self, format: &CUVIDEOFORMAT) -> Result<c_int> {
        if format.codec != self.codec || (self.codec != cudaVideoCodec_JPEG && format.chroma_format != cudaVideoChromaFormat_420) || format.bit_depth_luma_minus8 != 0 {
            bail!("a stream NVDEC isn't set up for (codec {}, chroma {}, {} bits)", format.codec, format.chroma_format, format.bit_depth_luma_minus8 + 8);
        }
        let [left, top, right, bottom] = format.display_area;
        let shown = ((right - left).max(0) as usize & !1, (bottom - top).max(0) as usize & !1);
        let surfaces = format.min_num_decode_surfaces as c_int + 2;
        if !self.decoder.is_null() && self.coded == (format.coded_width, format.coded_height) && self.shown == shown && self.surfaces >= surfaces {
            return Ok(self.surfaces);
        }
        self.end();
        let mut caps = CUVIDDECODECAPS::zeroed();
        caps.eCodecType = self.codec;
        caps.eChromaFormat = format.chroma_format;
        self.device.cuda.check((self.cuvid.get_decoder_caps)(&mut caps), "asking what NVDEC decodes")?;
        if caps.bIsSupported == 0 || caps.nOutputFormatMask & 1 == 0 {
            bail!("NVDEC doesn't decode this (codec {}, chroma {}) to NV12", self.codec, format.chroma_format);
        }
        if format.coded_width > caps.nMaxWidth || format.coded_height > caps.nMaxHeight || format.coded_width < caps.nMinWidth as u32 || format.coded_height < caps.nMinHeight as u32 {
            bail!("{}×{} is outside what NVDEC decodes ({}×{} to {}×{})", format.coded_width, format.coded_height, caps.nMinWidth, caps.nMinHeight, caps.nMaxWidth, caps.nMaxHeight);
        }
        let mut info = CUVIDDECODECREATEINFO::zeroed();
        info.ulWidth = format.coded_width as c_ulong;
        info.ulHeight = format.coded_height as c_ulong;
        info.ulNumDecodeSurfaces = surfaces as c_ulong;
        info.CodecType = self.codec;
        info.ChromaFormat = format.chroma_format;
        info.ulCreationFlags = cudaVideoCreate_PreferCUVID;
        info.ulMaxWidth = format.coded_width as c_ulong;
        info.ulMaxHeight = format.coded_height as c_ulong;
        info.display_area = [left as i16, top as i16, right as i16, bottom as i16];
        info.OutputFormat = cudaVideoSurfaceFormat_NV12;
        info.DeinterlaceMode = cudaVideoDeinterlaceMode_Weave;
        info.ulTargetWidth = shown.0 as c_ulong;
        info.ulTargetHeight = shown.1 as c_ulong;
        info.ulNumOutputSurfaces = 2;
        self.device.cuda.check((self.cuvid.create_decoder)(&mut self.decoder, &mut info), "starting NVDEC")?;
        let size = shown.0 * shown.1 * 3 / 2;
        let mut host = std::ptr::null_mut();
        if (self.device.cuda.mem_alloc_host)(&mut host, size) == CUDA_SUCCESS && !host.is_null() {
            self.host = Some((host as *mut u8, size));
        }
        self.coded = (format.coded_width, format.coded_height);
        self.shown = shown;
        self.surfaces = surfaces;
        if debugging() {
            eprintln!("nv: decoder (codec {}, chroma {}) {}×{} coded, {}×{} shown, {surfaces} surfaces", self.codec, format.chroma_format, format.coded_width, format.coded_height, shown.0, shown.1);
        }
        Ok(surfaces)
    }

    /// The picture at `index`, off the chip.
    unsafe fn picture(&self, index: c_int) -> Result<Picture> {
        let mut process = CUVIDPROCPARAMS::zeroed();
        process.progressive_frame = 1;
        let (mut at, mut pitch) = (0u64, 0 as c_uint);
        self.device.cuda.check((self.cuvid.map_video_frame)(self.decoder, index, &mut at, &mut pitch, &mut process), "getting at the picture")?;
        let (width, height) = self.shown;
        let mut ordinary = Vec::new();
        let nv12: &mut [u8] = match self.host {
            Some((host, size)) => std::slice::from_raw_parts_mut(host, size),
            None => {
                ordinary.resize(width * height * 3 / 2, 0);
                &mut ordinary
            }
        };
        let (luma, chroma) = nv12.split_at_mut(width * height);
        let pitch = pitch as usize;
        let copied = self.device.copy_down(at, pitch, width, height, luma).and_then(|_| self.device.copy_down(at + (pitch * ((height + 1) & !1)) as u64, pitch, width, height / 2, chroma));
        (self.cuvid.unmap_video_frame)(self.decoder, at);
        copied?;
        let range = (self.codec == cudaVideoCodec_JPEG).then(limited_range_tables);
        i420_from_nv12_mapped(nv12, width, height, width, width * height, range).ok_or_else(|| anyhow!("a picture of no size"))
    }
}

unsafe extern "C" fn sequence_callback(data: *mut c_void, format: *mut CUVIDEOFORMAT) -> c_int {
    let decoding = &mut *(data as *mut Decoding);
    match decoding.start(&*format) {
        Ok(surfaces) => surfaces,
        Err(e) => {
            decoding.trouble = Some(format!("{e:#}"));
            0
        }
    }
}

unsafe extern "C" fn decode_callback(data: *mut c_void, picture: *mut CUVIDPICPARAMS) -> c_int {
    let decoding = &mut *(data as *mut Decoding);
    if decoding.decoder.is_null() {
        decoding.trouble = Some("a picture before its stream's size".to_string());
        return 0;
    }
    match decoding.device.cuda.check((decoding.cuvid.decode_picture)(decoding.decoder, picture), "decoding") {
        Ok(()) => 1,
        Err(e) => {
            decoding.trouble = Some(format!("{e:#}"));
            0
        }
    }
}

unsafe extern "C" fn display_callback(data: *mut c_void, info: *mut CUVIDPARSERDISPINFO) -> c_int {
    let decoding = &mut *(data as *mut Decoding);
    if !info.is_null() {
        decoding.ready.push((*info).picture_index);
    }
    1
}

struct NvDecoder {
    device: &'static Device,
    cuvid: &'static Cuvid,
    parser: CUvideoparser,
    /// Boxed: the parser holds a pointer to it.
    decoding: Box<Decoding>,
    count: i64,
}

unsafe impl Send for NvDecoder {}

impl NvDecoder {
    unsafe fn new(device: &'static Device, cuvid: &'static Cuvid, codec: c_int) -> Result<NvDecoder> {
        let _current = device.current()?;
        let decoding = Box::new(Decoding { device, cuvid, codec, decoder: std::ptr::null_mut(), coded: (0, 0), shown: (0, 0), surfaces: 0, host: None, ready: Vec::new(), trouble: None });
        let mut decoder = NvDecoder { device, cuvid, parser: std::ptr::null_mut(), decoding, count: 0 };
        let mut params = CUVIDPARSERPARAMS::zeroed();
        params.CodecType = codec;
        params.ulMaxNumDecodeSurfaces = 1;
        params.ulMaxDisplayDelay = 0;
        params.pUserData = &mut *decoder.decoding as *mut Decoding as *mut c_void;
        params.pfnSequenceCallback = Some(sequence_callback);
        params.pfnDecodePicture = Some(decode_callback);
        params.pfnDisplayPicture = Some(display_callback);
        device.cuda.check((cuvid.create_video_parser)(&mut decoder.parser, &mut params), "starting NVDEC's parser")?;
        Ok(decoder)
    }

    unsafe fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        let _current = self.device.current()?;
        let mut packet = CUVIDSOURCEDATAPACKET::zeroed();
        packet.flags = CUVID_PKT_ENDOFPICTURE | CUVID_PKT_TIMESTAMP;
        packet.payload_size = frame.len() as c_ulong;
        packet.payload = frame.as_ptr();
        packet.timestamp = self.count;
        self.count += 1;
        let parsed = self.device.cuda.check((self.cuvid.parse_video_data)(self.parser, &mut packet), "giving NVDEC the frame");
        let ready = std::mem::take(&mut self.decoding.ready);
        if let Some(trouble) = self.decoding.trouble.take() {
            bail!("{trouble}");
        }
        parsed?;
        if debugging() && ready.len() != 1 {
            eprintln!("nv: frame {} gave {} pictures", self.count - 1, ready.len());
        }
        match ready.last() {
            Some(&index) => self.decoding.picture(index).map(Some),
            None => Ok(None),
        }
    }
}

impl Drop for NvDecoder {
    fn drop(&mut self) {
        unsafe {
            let _current = self.device.current();
            if !self.parser.is_null() {
                (self.cuvid.destroy_video_parser)(self.parser);
            }
            self.decoding.end();
        }
    }
}

impl HardDecoder for NvDecoder {
    fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        unsafe { NvDecoder::decode(self, frame) }
    }

    fn name(&self) -> String {
        format!("NVDEC ({})", self.device.name)
    }
}

/// A camera's JPEGs (MJPEG) decoded by NVDEC, as I420 pictures: sampled
/// 4:2:0, 4:2:2 or 4:4:4, it gives NV12. The first picture is checked
/// against software's of the same JPEG.
pub struct JpegDecoder {
    decoder: NvDecoder,
    checked: bool,
}

impl JpegDecoder {
    /// One on NVIDIA's chip, if there is one and its NVDEC decodes JPEG.
    pub fn new() -> Result<JpegDecoder> {
        let nvidia = Nvidia::find()?;
        let device = nvidia.device;
        let cuvid = device.cuvid.as_ref().ok_or_else(|| anyhow!("{}: no NVDEC ({})", device.name, device.no_decoding.as_deref().unwrap_or_default()))?;
        let decoder = unsafe { NvDecoder::new(device, cuvid, cudaVideoCodec_JPEG) }?;
        let mut caps = CUVIDDECODECAPS::zeroed();
        caps.eCodecType = cudaVideoCodec_JPEG;
        caps.eChromaFormat = cudaVideoChromaFormat_420;
        {
            let _current = device.current()?;
            device.cuda.check(unsafe { (cuvid.get_decoder_caps)(&mut caps) }, "asking what NVDEC decodes")?;
        }
        if caps.bIsSupported == 0 {
            bail!("{}'s NVDEC doesn't decode JPEG", device.name);
        }
        Ok(JpegDecoder { decoder, checked: false })
    }

    /// What it decodes on.
    pub fn name(&self) -> String {
        format!("NVDEC ({})", self.decoder.device.name)
    }

    /// Decodes a camera's JPEG.
    pub fn decode(&mut self, file: &[u8]) -> Result<Picture> {
        let jpeg = crate::jpeg::Jpeg::parse(file).ok_or_else(|| anyhow!("not a baseline JPEG"))?;
        let has_tables = file.windows(2).any(|w| w == [0xff, 0xc4]);
        let whole = if has_tables { std::borrow::Cow::Borrowed(file) } else { std::borrow::Cow::Owned(jpeg.write()) };
        let picture = unsafe { self.decoder.decode(&whole) }?.ok_or_else(|| anyhow!("NVDEC gave no picture"))?;
        let (width, height) = (jpeg.width as usize & !1, jpeg.height as usize & !1);
        if (picture.width, picture.height) != (width, height) {
            bail!("a {}×{} picture of a {width}×{height} JPEG", picture.width, picture.height);
        }
        if !self.checked {
            let software = crate::common::mjpeg_to_rgb(file).ok_or_else(|| anyhow!("a JPEG software can't decode"))?;
            let software = software.scaled(width, height).to_i420().into_owned();
            let [y, u, v] = crate::common::planes_psnr(&picture, &software);
            if debugging() {
                eprintln!("nv: JPEG {width}×{height} against software: {y:.1}, {u:.1} and {v:.1} dB");
            }
            if y < 30.0 || u < 28.0 || v < 28.0 {
                bail!("its picture came back wrong: {y:.0}, {u:.0} and {v:.0} dB from software's");
            }
            crate::common::note_jpeg_checked(format!("{y:.0}, {u:.0} and {v:.0} dB (NV12 from NVDEC)"));
            self.checked = true;
        }
        Ok(picture)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::{nal_units, Sps};
    use std::sync::MutexGuard;
    use swarm_core::video::{has_idr, is_key, VideoDecoder, VideoEncoder, VideoPiece};

    /// NVIDIA's chip, one test at a time (GeForce chips allow only so
    /// many NVENC sessions at once): `None`, and the test skipped, where
    /// there isn't one.
    fn chip() -> Option<(Nvidia, MutexGuard<'static, ()>)> {
        let one = crate::common::CHIP_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        match Nvidia::find() {
            Ok(nvidia) => Some((nvidia, one)),
            Err(e) => {
                eprintln!("no NVIDIA chip ({e:#}): the NVENC and NVDEC tests are skipped");
                None
            }
        }
    }

    fn piece(frame: Vec<u8>, number: u32) -> VideoPiece {
        VideoPiece { frame: number, index: 0, count: 1, key: has_idr(&frame), h265: false, small: false, data: frame }
    }

    /// OpenH264's picture of a frame, decoded as other swarms decode it.
    fn openh264_picture(decoder: &mut openh264::decoder::Decoder, frame: &[u8]) -> Option<Picture> {
        use openh264::formats::YUVSource;
        let yuv = decoder.decode(frame).unwrap()?;
        let (w, h) = yuv.dimensions();
        let (ys, us, vs) = yuv.strides();
        let mut data = Vec::new();
        (0..h).for_each(|r| data.extend_from_slice(&yuv.y()[r * ys..][..w]));
        (0..h / 2).for_each(|r| data.extend_from_slice(&yuv.u()[r * us..][..w / 2]));
        (0..h / 2).for_each(|r| data.extend_from_slice(&yuv.v()[r * vs..][..w / 2]));
        Some(Picture::i420(w, h, data))
    }

    fn psnr(a: &Picture, b: &Picture) -> f64 {
        let (a, b) = (a.to_i420(), b.to_i420());
        let squared: f64 = a.data.iter().zip(&b.data).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum();
        10.0 * (255.0f64.powi(2) / (squared / a.data.len() as f64).max(1e-9)).log10()
    }

    /// What NVENC makes, as other swarms need it: Constrained Baseline
    /// with one reference, full frames (with their SPS and PPS) only where
    /// asked, one slice a picture, the bitrate changed in place; and every
    /// frame shown by OpenH264 at once, looking like what was sent.
    #[test]
    fn the_encoder_makes_what_other_swarms_show() {
        let Some((nvidia, _one)) = chip() else { return };
        let (width, height) = (960, 540);
        let mut encoder = nvidia.encoder(Shape { width, height, fps: 30 }, 1_000_000, Codec::H264).unwrap();
        let background = crate::pattern::test_background((width as u32, height as u32));
        let mut decoder = VideoDecoder::software().unwrap();
        let (mut keys, mut alike) = (Vec::new(), Vec::new());
        for i in 0..60u32 {
            if i == 40 {
                encoder.set_bitrate(500_000).unwrap();
            }
            let picture = crate::pattern::test_pattern(i as f32 / 30.0, &background);
            let frame = encoder.encode(&picture, i == 25).unwrap();
            let kinds: Vec<u8> = nal_units(&frame).iter().map(|n| n[0] & 0x1f).collect();
            if has_idr(&frame) {
                keys.push(i);
                assert_eq!(kinds, [7, 8, 5], "full frame {i}");
                let sps = Sps::parse(nal_units(&frame)[0]).unwrap();
                assert_eq!((sps.profile_idc, sps.constraints & 0xc0), (66, 0xc0), "Constrained Baseline");
                assert_eq!(sps.max_num_ref_frames, 1);
                assert!(sps.frame_mbs_only_flag);
                assert_eq!(sps.shown().map(|s| (s.0, s.1)), Some((width, height)));
            } else {
                assert_eq!(kinds, [1], "frame {i}: one slice, nothing else");
            }
            let decoded = decoder.push(piece(frame, i)).picture.unwrap_or_else(|| panic!("frame {i} not shown at once"));
            assert_eq!((decoded.width, decoded.height), (width, height));
            if i >= 50 {
                alike.push(psnr(&decoded, &picture));
            }
        }
        assert_eq!(keys, [0, 25], "full frames where asked, the new bitrate taking none");
        let alike = alike.iter().sum::<f64>() / alike.len() as f64;
        assert!(alike > 35.0, "{alike:.1} dB like what was sent");
    }

    /// H.265 on the chip, where it has it: full frames where asked, each
    /// with its parameter sets, one slice a picture otherwise; NVDEC
    /// shows each at once, looking like what was sent — at 720p and 4K.
    #[test]
    fn h265_goes_through_the_chip() {
        let Some((nvidia, _one)) = chip() else { return };
        if nvidia.h265() != (true, true) {
            eprintln!("no H.265 on {}: {:?}", nvidia.device.name, nvidia.h265());
            return;
        }
        for (width, height, bitrate) in [(1280, 720, 1_500_000), (3840, 2160, 9_000_000)] {
            let mut encoder = nvidia.encoder(Shape { width, height, fps: 25 }, bitrate, Codec::H265).unwrap();
            let mut decoder = nvidia.decoder(Codec::H265).unwrap();
            let background = crate::pattern::test_background((width as u32, height as u32));
            let mut alike = Vec::new();
            for i in 0..40 {
                let picture = crate::pattern::test_pattern(i as f32 / 25.0, &background).to_i420().into_owned();
                let key = i == 0 || i == 25;
                let frame = encoder.encode(&picture, key).unwrap();
                assert_eq!(is_key(&frame, Codec::H265), key, "frame {i}");
                let kinds: Vec<u8> = nal_units(&frame).iter().filter_map(|n| n.first().map(|b| b >> 1 & 0x3f)).collect();
                if key {
                    assert!([32, 33, 34].iter().all(|k| kinds.contains(k)), "frame {i}: VPS, SPS and PPS with it: {kinds:?}");
                } else {
                    assert_eq!(kinds, [1], "frame {i}: one slice, nothing else");
                }
                let decoded = decoder.decode(&frame).unwrap().unwrap_or_else(|| panic!("frame {i} not shown at once"));
                assert_eq!((decoded.width, decoded.height), (width, height));
                if i >= 5 {
                    alike.push(psnr(&decoded, &picture));
                }
            }
            let alike = alike.iter().sum::<f64>() / alike.len() as f64;
            assert!(alike > 35.0, "{width}×{height}: {alike:.1} dB like what was sent");
        }
    }

    /// The bitrate it's set to is what it sends (roughly: a constant rate
    /// over a second's buffer), before and after a change in place.
    #[test]
    fn the_encoder_keeps_to_its_bitrate() {
        let Some((nvidia, _one)) = chip() else { return };
        let (width, height) = (1280, 720);
        let mut encoder = nvidia.encoder(Shape { width, height, fps: 25 }, 2_000_000, Codec::H264).unwrap();
        let background = crate::pattern::test_background((width as u32, height as u32));
        let mut sent = [0usize; 2];
        for i in 0..200 {
            if i == 100 {
                encoder.set_bitrate(1_000_000).unwrap();
            }
            let mut picture = crate::pattern::test_pattern(i as f32 / 25.0, &background);
            let mut seed = i as u32;
            for byte in picture.data.iter_mut() {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *byte = byte.saturating_add((seed >> 27) as u8);
            }
            let frame = encoder.encode(&picture, i == 0).unwrap();
            if (25..75).contains(&i) || (125..175).contains(&i) {
                sent[(i >= 100) as usize] += frame.len();
            }
        }
        let rates = sent.map(|bytes| bytes * 8 / 2);
        assert!((1_600_000..2_400_000).contains(&rates[0]), "at 2 Mbit/s it sent {} kbit/s", rates[0] / 1000);
        assert!((800_000..1_200_000).contains(&rates[1]), "at 1 Mbit/s it sent {} kbit/s", rates[1] / 1000);
    }

    /// OpenH264's frames (as other swarms send them: slices of 1200 bytes)
    /// decoded on the chip give exactly the pictures OpenH264 makes of
    /// them, each at once — and a stream changing size (a new shape)
    /// mid-call carries on.
    #[test]
    fn the_decoder_gives_what_openh264_gives() {
        let Some((nvidia, _one)) = chip() else { return };
        let mut chip = nvidia.decoder(Codec::H264).unwrap();
        let mut software = openh264::decoder::Decoder::new().unwrap();
        let mut n = 0;
        for (bitrate, (width, height)) in [(2_000_000, (1280, 720)), (1_000_000, (960, 540)), (4_000_000, (1920, 1080)), (12_000_000, (3840, 2160))] {
            let mut encoder = VideoEncoder::new(bitrate).unwrap();
            encoder.keep_shape();
            assert_eq!((encoder.shape().width, encoder.shape().height), (width, height));
            let background = crate::pattern::test_background((width as u32, height as u32));
            for i in 0..20 {
                if i == 12 {
                    encoder.request_keyframe();
                }
                let pieces = encoder.encode(&crate::pattern::test_pattern(i as f32 / 25.0, &background), 1200).unwrap();
                let frame: Vec<u8> = pieces.iter().flat_map(|p| p.data.clone()).collect();
                let ours = chip.decode(&frame).unwrap().unwrap_or_else(|| panic!("{width}×{height} frame {i}: no picture at once"));
                let theirs = openh264_picture(&mut software, &frame).expect("OpenH264's picture");
                assert!(ours == theirs, "{width}×{height} frame {i} differs ({:.1} dB)", psnr(&ours, &theirs));
                n += 1;
            }
        }
        assert_eq!(n, 80);
    }

    /// NVENC's frames decoded by NVDEC: exactly what OpenH264 makes of
    /// them, so a call between two NVIDIA chips sees what any swarm would.
    #[test]
    fn the_chip_decodes_its_own_frames_as_openh264_does() {
        let Some((nvidia, _one)) = chip() else { return };
        let (width, height) = (1920, 1080);
        let mut encoder = nvidia.encoder(Shape { width, height, fps: 25 }, 4_000_000, Codec::H264).unwrap();
        let mut chip = nvidia.decoder(Codec::H264).unwrap();
        let mut software = openh264::decoder::Decoder::new().unwrap();
        let background = crate::pattern::test_background((width as u32, height as u32));
        for i in 0..30 {
            let frame = encoder.encode(&crate::pattern::test_pattern(i as f32 / 25.0, &background), i == 0 || i == 20).unwrap();
            let ours = chip.decode(&frame).unwrap().unwrap_or_else(|| panic!("frame {i}: no picture at once"));
            let theirs = openh264_picture(&mut software, &frame).expect("OpenH264's picture");
            assert!(ours == theirs, "frame {i} differs ({:.1} dB)", psnr(&ours, &theirs));
        }
    }

    /// A camera's JPEGs decoded by NVDEC give what software gives: 4:2:2
    /// with restart markers, 4:2:0, and 4:2:0 without its Huffman tables
    /// — and video-check hears of the check on the first.
    #[test]
    fn camera_jpegs_decode_on_the_chip_as_in_software() {
        use crate::jpeg::tests::{CAMERA_420, CAMERA_420_NO_TABLES, CAMERA_422};
        let Some((_nvidia, _one)) = chip() else { return };
        for (name, file) in [("4:2:2", CAMERA_422), ("4:2:0", CAMERA_420), ("4:2:0 without tables", CAMERA_420_NO_TABLES)] {
            let mut decoder = JpegDecoder::new().unwrap();
            crate::common::forget_jpeg_check();
            let software = crate::common::mjpeg_to_rgb(file).unwrap().to_i420().into_owned();
            for _ in 0..3 {
                let chip = decoder.decode(file).unwrap_or_else(|e| panic!("{name}: {e:#}"));
                assert_eq!((chip.width, chip.height), (320, 240));
                let [y, u, v] = crate::common::planes_psnr(&chip, &software);
                assert!(y > 40.0 && u > 30.0 && v > 30.0, "{name}: {y:.1}, {u:.1} and {v:.1} dB");
            }
            let checked = crate::common::jpeg_checked().unwrap_or_default();
            assert!(checked.contains(" dB (NV12 from NVDEC)"), "{name}: {checked}");
        }
    }

    /// GeForce chips take only so many encoders at once: one too many
    /// fails cleanly (OpenH264 then encodes), and each dropped gives its
    /// session back.
    #[test]
    fn encoders_beyond_what_the_chip_takes_fail_cleanly() {
        let Some((nvidia, _one)) = chip() else { return };
        let shape = Shape { width: 640, height: 360, fps: 25 };
        let picture = Picture::i420(640, 360, vec![128; 640 * 360 * 3 / 2]);
        for _ in 0..2 {
            let mut open = Vec::new();
            let refused = loop {
                match nvidia.encoder(shape, 500_000, Codec::H264) {
                    Ok(mut encoder) => {
                        assert!(has_idr(&encoder.encode(&picture, true).unwrap()));
                        open.push(encoder);
                    }
                    Err(e) => break Some(e),
                }
                if open.len() == 40 {
                    break None;
                }
            };
            eprintln!("{} encoders at once; then {refused:?}", open.len());
            assert!(!open.is_empty());
        }
    }
}
