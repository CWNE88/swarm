//! Video on the graphics chip through VA-API: Linux, on Intel's and
//! AMD's chips (Mesa's radeonsi, Intel's iHD). libva is loaded when
//! wanted, as PipeWire is, so nothing more is linked and a system without
//! it just has OpenH264.
//!
//! - **Encoding**: H.264 Constrained Baseline, one reference, every
//!   picture a frame out at once. Swarm chooses each picture's type (a
//!   full frame when asked, and every `KEYFRAME_EVERY_SECS` as a
//!   backstop), keeps the bitrate constant and changes it in place, and
//!   writes its own SPS and PPS where the driver takes them (otherwise
//!   the driver's are used).
//! - **Decoding**: VA-API is given what's in the stream rather than the
//!   stream, so swarm reads the parameter sets and slice headers
//!   (`h264`), and keeps the reference pictures, as H.264 says. Only what
//!   calls send is taken (frames, P and I slices, short-term references);
//!   anything else fails, and OpenH264 decodes that call instead.
//! - **H.265** (Main), likewise: encoding as FFmpeg's hevc_vaapi does
//!   (the driver's block sizes, Swarm's VPS, SPS, PPS and slice headers
//!   packed in); decoding from what swarm reads of the stream (`h265`),
//!   each picture's order and references worked out as H.265 says. Taken
//!   from others' chips only (there's no H.265 in software): what fails
//!   goes back to H.264.
//!
//! One display is opened for the whole process, on the first render node
//! with H.264 (or the one `SWARM_VA_DEVICE` names). `SWARM_VA_DEBUG=1`
//! says what goes to and from the driver.

use swarm_core::lock::Locked;
use crate::h264::{nal_units, Pps, SliceHeader, Sps, SLICE_I, SLICE_P};
use crate::h265;
use crate::common::{i420_from_nv12, parameter_sets, planes_psnr};
use crate::va_sys::*;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::os::fd::IntoRawFd;
use swarm_core::video::{Codec, HardDecoder, HardEncoder, Hardware, Picture, Shape};

fn debugging() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SWARM_VA_DEBUG").is_ok_and(|v| v == "1"))
}

type MessageCallback = unsafe extern "C" fn(*mut c_void, *const c_char);

/// libva's functions, from libva.so.2 and libva-drm.so.2.
struct Lib {
    get_display_drm: unsafe extern "C" fn(c_int) -> VADisplay,
    initialize: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    terminate: unsafe extern "C" fn(VADisplay) -> VAStatus,
    set_error_callback: unsafe extern "C" fn(VADisplay, Option<MessageCallback>, *mut c_void) -> Option<MessageCallback>,
    set_info_callback: unsafe extern "C" fn(VADisplay, Option<MessageCallback>, *mut c_void) -> Option<MessageCallback>,
    error_str: unsafe extern "C" fn(VAStatus) -> *const c_char,
    query_vendor_string: unsafe extern "C" fn(VADisplay) -> *const c_char,
    max_num_profiles: unsafe extern "C" fn(VADisplay) -> c_int,
    max_num_entrypoints: unsafe extern "C" fn(VADisplay) -> c_int,
    query_config_profiles: unsafe extern "C" fn(VADisplay, *mut i32, *mut c_int) -> VAStatus,
    query_config_entrypoints: unsafe extern "C" fn(VADisplay, i32, *mut i32, *mut c_int) -> VAStatus,
    get_config_attributes: unsafe extern "C" fn(VADisplay, i32, i32, *mut VAConfigAttrib, c_int) -> VAStatus,
    create_config: unsafe extern "C" fn(VADisplay, i32, i32, *mut VAConfigAttrib, c_int, *mut VAConfigID) -> VAStatus,
    destroy_config: unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus,
    create_surfaces: unsafe extern "C" fn(VADisplay, c_uint, c_uint, c_uint, *mut VASurfaceID, c_uint, *mut c_void, c_uint) -> VAStatus,
    destroy_surfaces: unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus,
    create_context: unsafe extern "C" fn(VADisplay, VAConfigID, c_int, c_int, c_int, *mut VASurfaceID, c_int, *mut VAContextID) -> VAStatus,
    destroy_context: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    create_buffer: unsafe extern "C" fn(VADisplay, VAContextID, i32, c_uint, c_uint, *mut c_void, *mut VABufferID) -> VAStatus,
    map_buffer: unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus,
    unmap_buffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    destroy_buffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    begin_picture: unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus,
    render_picture: unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus,
    end_picture: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    sync_surface: unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus,
    create_image: unsafe extern "C" fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus,
    derive_image: unsafe extern "C" fn(VADisplay, VASurfaceID, *mut VAImage) -> VAStatus,
    destroy_image: unsafe extern "C" fn(VADisplay, VAImageID) -> VAStatus,
    get_image: unsafe extern "C" fn(VADisplay, VASurfaceID, c_int, c_int, c_uint, c_uint, VAImageID) -> VAStatus,
    put_image: unsafe extern "C" fn(VADisplay, VASurfaceID, VAImageID, c_int, c_int, c_uint, c_uint, c_int, c_int, c_uint, c_uint) -> VAStatus,
    query_surface_attributes: unsafe extern "C" fn(VADisplay, VAConfigID, *mut VASurfaceAttrib, *mut c_uint) -> VAStatus,
    _libraries: (libloading::Library, libloading::Library),
}

impl Lib {
    /// libva, loaded once.
    fn get() -> Result<&'static Lib> {
        static LIB: std::sync::OnceLock<Result<Lib, String>> = std::sync::OnceLock::new();
        LIB.get_or_init(|| unsafe { Lib::load() }.map_err(|e| format!("{e:#}"))).as_ref().map_err(|e| anyhow!("{e}"))
    }

    unsafe fn load() -> Result<Lib> {
        let va = libloading::Library::new("libva.so.2").context("no libva (libva2)")?;
        let drm = libloading::Library::new("libva-drm.so.2").context("no libva-drm (libva-drm2)")?;
        macro_rules! f {
            ($lib:expr, $name:literal) => {
                *$lib.get(concat!($name, "\0").as_bytes()).with_context(|| format!("libva without {}", $name))?
            };
        }
        Ok(Lib {
            get_display_drm: f!(drm, "vaGetDisplayDRM"),
            initialize: f!(va, "vaInitialize"),
            terminate: f!(va, "vaTerminate"),
            set_error_callback: f!(va, "vaSetErrorCallback"),
            set_info_callback: f!(va, "vaSetInfoCallback"),
            error_str: f!(va, "vaErrorStr"),
            query_vendor_string: f!(va, "vaQueryVendorString"),
            max_num_profiles: f!(va, "vaMaxNumProfiles"),
            max_num_entrypoints: f!(va, "vaMaxNumEntrypoints"),
            query_config_profiles: f!(va, "vaQueryConfigProfiles"),
            query_config_entrypoints: f!(va, "vaQueryConfigEntrypoints"),
            get_config_attributes: f!(va, "vaGetConfigAttributes"),
            create_config: f!(va, "vaCreateConfig"),
            destroy_config: f!(va, "vaDestroyConfig"),
            create_surfaces: f!(va, "vaCreateSurfaces"),
            destroy_surfaces: f!(va, "vaDestroySurfaces"),
            create_context: f!(va, "vaCreateContext"),
            destroy_context: f!(va, "vaDestroyContext"),
            create_buffer: f!(va, "vaCreateBuffer"),
            map_buffer: f!(va, "vaMapBuffer"),
            unmap_buffer: f!(va, "vaUnmapBuffer"),
            destroy_buffer: f!(va, "vaDestroyBuffer"),
            begin_picture: f!(va, "vaBeginPicture"),
            render_picture: f!(va, "vaRenderPicture"),
            end_picture: f!(va, "vaEndPicture"),
            sync_surface: f!(va, "vaSyncSurface"),
            create_image: f!(va, "vaCreateImage"),
            derive_image: f!(va, "vaDeriveImage"),
            destroy_image: f!(va, "vaDestroyImage"),
            get_image: f!(va, "vaGetImage"),
            put_image: f!(va, "vaPutImage"),
            query_surface_attributes: f!(va, "vaQuerySurfaceAttributes"),
            _libraries: (va, drm),
        })
    }
}

/// libva's messages while a display opens, for saying why it wouldn't.
static MESSAGES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// libva's messages: kept while opening, else dropped (they'd land in
/// the TUI), but for debugging.
unsafe extern "C" fn message(_: *mut c_void, text: *const c_char) {
    if text.is_null() {
        return;
    }
    let text = CStr::from_ptr(text).to_string_lossy();
    if debugging() {
        eprint!("libva: {text}");
    }
    if let Ok(mut kept) = MESSAGES.lock() {
        if kept.len() < 50 {
            kept.push(text.trim().to_string());
        }
    }
}

/// Why libva wouldn't open a display, from what it said.
fn why_not() -> Option<String> {
    let said = std::mem::take(&mut *MESSAGES.lock().ok()?);
    let driver = said.iter().rev().find_map(|m| m.strip_prefix("Trying to open "));
    match driver {
        Some(driver) => Some(format!("its driver ({driver}) is missing or didn't start")),
        None => said.into_iter().find(|m| m.contains("error") || m.contains("fail")),
    }
}

/// An opened display (one render node), for the whole process.
struct Device {
    lib: &'static Lib,
    display: VADisplay,
    /// The render node, and what the driver calls itself.
    name: String,
    /// Its encoding entrypoint, if it encodes.
    encode: Option<i32>,
    /// The profile decoding is set up with, if it decodes.
    decode: Option<i32>,
    /// Its encoding entrypoint for H.265 (Main), if it encodes that.
    encode_h265: Option<i32>,
    /// Whether it decodes H.265 (Main).
    decode_h265: bool,
    /// Whether it decodes JPEG (a camera's MJPEG).
    jpeg: bool,
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    /// `status` as an error, saying what was being done.
    fn check(&self, status: VAStatus, what: &str) -> Result<()> {
        if status == VA_STATUS_SUCCESS {
            return Ok(());
        }
        let text = unsafe { CStr::from_ptr((self.lib.error_str)(status)) }.to_string_lossy().into_owned();
        Err(anyhow!("{what}: {text} ({status})"))
    }

    /// Opens `path`: `None` if it has no H.264 to offer.
    unsafe fn open(lib: &'static Lib, path: &str) -> Result<Device> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).with_context(|| format!("opening {path}"))?;
        let fd = file.into_raw_fd();
        let display = (lib.get_display_drm)(fd);
        if display.is_null() {
            libc::close(fd);
            bail!("{path}: libva wouldn't use it");
        }
        (lib.set_error_callback)(display, Some(message), std::ptr::null_mut());
        (lib.set_info_callback)(display, Some(message), std::ptr::null_mut());
        let (mut major, mut minor) = (0, 0);
        let mut device = Device { lib, display, name: path.to_string(), encode: None, decode: None, encode_h265: None, decode_h265: false, jpeg: false };
        MESSAGES.locked().clear();
        let started = device.check((lib.initialize)(display, &mut major, &mut minor), path);
        let why = why_not();
        if let Err(e) = started {
            (lib.terminate)(display);
            libc::close(fd);
            return Err(why.map_or(e, |why| anyhow!("{path}: {why}")));
        }
        let vendor = CStr::from_ptr((lib.query_vendor_string)(display)).to_string_lossy().into_owned();
        device.name = format!("{vendor}, {path}");
        if ["NVDEC", "VDPAU", "Splitted-Desktop"].iter().any(|w| vendor.contains(w)) {
            (lib.terminate)(display);
            libc::close(fd);
            bail!("{path}: {vendor} (not used)");
        }
        let profiles = Device::list(|out, n| (lib.query_config_profiles)(display, out, n), (lib.max_num_profiles)(display));
        let entrypoints = |profile: i32| Device::list(|out, n| (lib.query_config_entrypoints)(display, profile, out, n), (lib.max_num_entrypoints)(display));
        let encodes = |profile: i32| {
            if !profiles.contains(&profile) {
                return None;
            }
            let points = entrypoints(profile);
            let point = [VAEntrypointEncSlice, VAEntrypointEncSliceLP].into_iter().find(|p| points.contains(p))?;
            let mut attributes = [VAConfigAttrib { type_: VAConfigAttribRTFormat, value: 0 }, VAConfigAttrib { type_: VAConfigAttribRateControl, value: 0 }];
            let offered = (lib.get_config_attributes)(display, profile, point, attributes.as_mut_ptr(), attributes.len() as c_int) == VA_STATUS_SUCCESS
                && attributes[0].value & VA_RT_FORMAT_YUV420 != 0
                && attributes[0].value != VA_ATTRIB_NOT_SUPPORTED
                && attributes[1].value != VA_ATTRIB_NOT_SUPPORTED
                && attributes[1].value & (VA_RC_CBR | VA_RC_VBR) != 0;
            offered.then_some(point)
        };
        device.encode = encodes(VAProfileH264ConstrainedBaseline);
        device.encode_h265 = encodes(VAProfileHEVCMain);
        device.decode_h265 = profiles.contains(&VAProfileHEVCMain) && entrypoints(VAProfileHEVCMain).contains(&VAEntrypointVLD);
        device.decode = [VAProfileH264ConstrainedBaseline, VAProfileH264Main, VAProfileH264High].into_iter().find(|&p| profiles.contains(&p) && entrypoints(p).contains(&VAEntrypointVLD));
        device.jpeg = profiles.contains(&VAProfileJPEGBaseline) && entrypoints(VAProfileJPEGBaseline).contains(&VAEntrypointVLD);
        if debugging() {
            eprintln!("va: {}: VA-API {major}.{minor}, encoding {:?}, decoding {:?}, H.265 encoding {:?}, decoding {}, JPEG {}", device.name, device.encode, device.decode, device.encode_h265, device.decode_h265, device.jpeg);
        }
        if device.encode.is_none() && device.decode.is_none() {
            (lib.terminate)(display);
            libc::close(fd);
            bail!("{path}: {vendor} has no H.264");
        }
        Ok(device)
    }

    /// A list libva fills: profiles, or entrypoints.
    fn list(fill: impl FnOnce(*mut i32, *mut c_int) -> VAStatus, most: c_int) -> Vec<i32> {
        let mut out = vec![0i32; most.max(0) as usize];
        let mut n = 0;
        if fill(out.as_mut_ptr(), &mut n) != VA_STATUS_SUCCESS {
            return Vec::new();
        }
        out.truncate(n.max(0) as usize);
        out
    }

    /// A buffer of `kind` holding `data`.
    unsafe fn buffer(&self, context: VAContextID, kind: i32, data: &[u8]) -> Result<VABufferID> {
        let mut id = VA_INVALID_ID;
        self.check((self.lib.create_buffer)(self.display, context, kind, data.len() as c_uint, 1, data.as_ptr() as *mut c_void, &mut id), "making a buffer")?;
        Ok(id)
    }

    unsafe fn surfaces(&self, width: usize, height: usize, count: usize) -> Result<Vec<VASurfaceID>> {
        let mut surfaces = vec![VA_INVALID_SURFACE; count];
        self.check((self.lib.create_surfaces)(self.display, VA_RT_FORMAT_YUV420, width as c_uint, height as c_uint, surfaces.as_mut_ptr(), count as c_uint, std::ptr::null_mut(), 0), "making surfaces")?;
        Ok(surfaces)
    }

    /// Draws `buffers` into `target` and waits until it's done.
    unsafe fn render(&self, context: VAContextID, target: VASurfaceID, buffers: &mut [VABufferID]) -> Result<()> {
        let result = (|| {
            self.check((self.lib.begin_picture)(self.display, context, target), "starting a picture")?;
            self.check((self.lib.render_picture)(self.display, context, buffers.as_mut_ptr(), buffers.len() as c_int), "giving it the picture")?;
            self.check((self.lib.end_picture)(self.display, context), "finishing the picture")?;
            self.check((self.lib.sync_surface)(self.display, target), "waiting for the picture")
        })();
        for &buffer in buffers.iter() {
            (self.lib.destroy_buffer)(self.display, buffer);
        }
        result
    }
}

/// An NV12 image of a surface, mapped: to read or write.
struct Mapped<'a> {
    device: &'a Device,
    image: VAImage,
    data: *mut u8,
    /// Made for this (derived), so destroyed after.
    own: bool,
}

impl Mapped<'_> {
    fn len(&self) -> usize {
        self.image.data_size as usize
    }

    unsafe fn bytes(&mut self) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.data, self.len())
    }
}

impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.device.lib.unmap_buffer)(self.device.display, self.image.buf);
            if self.own {
                (self.device.lib.destroy_image)(self.device.display, self.image.image_id);
            }
        }
    }
}

/// How pictures go between memory and a surface: derived from the
/// surface (no copy), or through an image of its own (a copy on the
/// chip), whichever the driver does.
enum Transfer {
    Derive,
    Image(VAImage),
}

impl Transfer {
    /// Which `device` does for a surface `width`×`height`: `read` first
    /// tries an image (reading a derived surface straight from the chip's
    /// memory can be slow); writing tries deriving first.
    unsafe fn choose(device: &Device, surface: VASurfaceID, width: usize, height: usize, read: bool) -> Result<Transfer> {
        let image = || -> Result<Transfer> {
            let mut format = VAImageFormat { fourcc: VA_FOURCC_NV12, byte_order: VA_LSB_FIRST, bits_per_pixel: 12, ..Default::default() };
            let mut image = VAImage::default();
            device.check((device.lib.create_image)(device.display, &mut format, width as c_int, height as c_int, &mut image), "making an NV12 image")?;
            Ok(Transfer::Image(image))
        };
        let derive = || -> Result<Transfer> {
            let mut image = VAImage::default();
            device.check((device.lib.derive_image)(device.display, surface, &mut image), "deriving an image")?;
            (device.lib.destroy_image)(device.display, image.image_id);
            if image.format.fourcc != VA_FOURCC_NV12 {
                bail!("its surfaces aren't NV12");
            }
            Ok(Transfer::Derive)
        };
        if read {
            image().or_else(|e| derive().map_err(|again| anyhow!("{e:#}; {again:#}")))
        } else {
            derive().or_else(|e| image().map_err(|again| anyhow!("{e:#}; {again:#}")))
        }
    }

    /// `surface` mapped, to read (its picture copied in first if through
    /// an image) or to write (then `put` after).
    unsafe fn map<'a>(&self, device: &'a Device, surface: VASurfaceID, width: usize, height: usize, read: bool) -> Result<Mapped<'a>> {
        let (image, own) = match self {
            Transfer::Derive => {
                let mut image = VAImage::default();
                device.check((device.lib.derive_image)(device.display, surface, &mut image), "deriving an image")?;
                (image, true)
            }
            Transfer::Image(image) => {
                if read {
                    device.check((device.lib.get_image)(device.display, surface, 0, 0, width as c_uint, height as c_uint, image.image_id), "copying the picture out")?;
                }
                (*image, false)
            }
        };
        let mut data = std::ptr::null_mut();
        if let Err(e) = device.check((device.lib.map_buffer)(device.display, image.buf, &mut data), "mapping the picture") {
            if own {
                (device.lib.destroy_image)(device.display, image.image_id);
            }
            return Err(e);
        }
        Ok(Mapped { device, image, data: data as *mut u8, own })
    }

    /// After writing a mapped image: into `surface`.
    unsafe fn put(&self, device: &Device, surface: VASurfaceID, width: usize, height: usize) -> Result<()> {
        if let Transfer::Image(image) = self {
            let (w, h) = (width as c_uint, height as c_uint);
            device.check((device.lib.put_image)(device.display, surface, image.image_id, 0, 0, w, h, 0, 0, w, h), "copying the picture in")?;
        }
        Ok(())
    }

    unsafe fn release(&self, device: &Device) {
        if let Transfer::Image(image) = self {
            (device.lib.destroy_image)(device.display, image.image_id);
        }
    }
}

/// VA-API's hardware, for `swarm_core::video::provide_hardware`.
pub struct VaApi {
    device: &'static Device,
}

impl VaApi {
    /// The first render node with H.264 on its chip (or the one
    /// `SWARM_VA_DEVICE` names), opened once for the process.
    pub fn find() -> Result<VaApi> {
        static FOUND: std::sync::OnceLock<Result<&'static Device, String>> = std::sync::OnceLock::new();
        let device = FOUND.get_or_init(|| VaApi::open().map(|va| va.device).map_err(|e| format!("{e:#}")));
        device.as_ref().map(|&device| VaApi { device }).map_err(|e| anyhow!("{e}"))
    }

    fn open() -> Result<VaApi> {
        let lib = Lib::get()?;
        let paths: Vec<String> = match std::env::var("SWARM_VA_DEVICE") {
            Ok(path) => vec![path],
            Err(_) => {
                let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
                    .map(|dir| dir.filter_map(|e| e.ok()).map(|e| e.path().to_string_lossy().into_owned()).filter(|p| p.contains("renderD")).collect())
                    .unwrap_or_default();
                nodes.sort();
                nodes
            }
        };
        if paths.is_empty() {
            bail!("no graphics render node (/dev/dri/renderD*)");
        }
        let mut tried = Vec::new();
        for path in paths {
            match unsafe { Device::open(lib, &path) } {
                Ok(device) => return Ok(VaApi { device: Box::leak(Box::new(device)) }),
                Err(e) => tried.push(format!("{e:#}")),
            }
        }
        bail!("{}", tried.join("; "))
    }
}

impl Hardware for VaApi {
    fn encoder(&self, shape: Shape, bitrate: u32, codec: Codec) -> Result<Box<dyn HardEncoder>> {
        let entrypoint = match codec {
            Codec::H264 => self.device.encode.ok_or_else(|| anyhow!("{} doesn't encode H.264", self.device.name))?,
            Codec::H265 => self.device.encode_h265.ok_or_else(|| anyhow!("{} doesn't encode H.265", self.device.name))?,
        };
        Ok(Box::new(unsafe { VaEncoder::open(self.device, entrypoint, shape, bitrate, codec) }?))
    }

    fn decoder(&self, codec: Codec) -> Result<Box<dyn HardDecoder>> {
        if codec == Codec::H265 {
            if !self.device.decode_h265 {
                bail!("{} doesn't decode H.265", self.device.name);
            }
            return Ok(Box::new(HevcDecoder::new(self.device)));
        }
        let profile = self.device.decode.ok_or_else(|| anyhow!("{} doesn't decode H.264", self.device.name))?;
        Ok(Box::new(VaDecoder::new(self.device, profile)))
    }

    fn h265(&self) -> (bool, bool) {
        (self.device.encode_h265.is_some(), self.device.decode_h265)
    }

    fn name(&self) -> String {
        format!("VA-API ({})", self.device.name)
    }
}

/// The H.264 level for pictures `width`×`height` at `fps` (Table A-1).
fn level_for(width: usize, height: usize, fps: u32) -> u8 {
    let frame = (width.div_ceil(16) * height.div_ceil(16)) as u32;
    let rate = frame * fps;
    [(1620, 40_500, 30), (3600, 108_000, 31), (5120, 216_000, 32), (8192, 245_760, 40), (8704, 522_240, 42), (22_080, 589_824, 50)]
        .into_iter()
        .find(|&(most, per_second, _)| frame <= most && rate <= per_second)
        .map_or(51, |(_, _, level)| level)
}

/// What the driver aligns an encoder's surfaces to, across and down
/// (VASurfaceAttribAlignmentSize, libva 2.21 on; 1 if it doesn't say).
unsafe fn surface_alignment(device: &Device, config: VAConfigID) -> (usize, usize) {
    let mut count: c_uint = 0;
    if (device.lib.query_surface_attributes)(device.display, config, std::ptr::null_mut(), &mut count) != VA_STATUS_SUCCESS || count == 0 {
        return (1, 1);
    }
    let mut attributes = vec![VASurfaceAttrib::default(); count as usize];
    if (device.lib.query_surface_attributes)(device.display, config, attributes.as_mut_ptr(), &mut count) != VA_STATUS_SUCCESS {
        return (1, 1);
    }
    attributes.truncate(count as usize);
    match attributes.iter().find(|a| a.type_ == VASurfaceAttribAlignmentSize) {
        Some(a) => (1 << (a.int() & 0xf), 1 << ((a.int() >> 4) & 0xf)),
        None => (1, 1),
    }
}

/// The H.265 level (×30, as said) for pictures `width`×`height` at
/// `fps` (Table A.8).
fn h265_level_for(width: usize, height: usize, fps: u32) -> u8 {
    let size = (width * height) as u64;
    let rate = size * fps as u64;
    let longest = width.max(height) as u64;
    [(36_864, 552_960, 30), (122_880, 3_686_400, 60), (245_760, 7_372_800, 63), (552_960, 16_588_800, 90), (983_040, 33_177_600, 93), (2_228_224, 66_846_720, 120), (2_228_224, 133_693_440, 123), (8_912_896, 267_386_880, 150), (8_912_896, 534_773_760, 153), (8_912_896, 1_069_547_520, 156)]
        .into_iter()
        .find(|&(most, per_second, _)| size <= most && rate <= per_second && longest * longest <= 8 * most)
        .map_or(180, |(_, _, level)| level)
}

/// H.265's picture order low bits take 12 (FFmpeg's), wrapping every
/// 4096 pictures.
const H265_LOG2_MAX_POC_LSB_MINUS4: u32 = 8;

/// What an H.265 encoder was told the chip does, and writes for it.
struct Hevc {
    sps: h265::Sps,
    pps: h265::Pps,
    /// Coding tree blocks in a picture (its one slice).
    ctbs: u32,
}

/// Frame numbers take 8 bits (wrapping every 256 frames). Picture order
/// is type 2, worked out from the frame number (with no B-frames it's
/// the same order), as FFmpeg has it for these drivers; its count's low
/// bits, said in each slice for drivers that look, take 4.
const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 4;
const LOG2_MAX_POC_LSB_MINUS4: u32 = 0;
const POC_TYPE: u32 = 2;

struct VaEncoder {
    device: &'static Device,
    config: VAConfigID,
    context: VAContextID,
    shape: Shape,
    /// Its surfaces' size: the picture's, to whole blocks.
    width: usize,
    height: usize,
    /// H.264's size in whole macroblocks.
    columns: usize,
    rows: usize,
    /// H.265's set-up (`None` for H.264).
    hevc: Option<Hevc>,
    /// The picture given, and the two it codes into and refers to in turn.
    input: VASurfaceID,
    recon: [VASurfaceID; 2],
    transfer: Transfer,
    coded: VABufferID,
    /// Which of `recon` this picture codes into.
    current: usize,
    /// Pictures since the last full frame (none before the first).
    since_idr: Option<u32>,
    idr_pic_id: u16,
    bitrate: u32,
    /// Whether the rate needs saying again (a new one, or a full frame).
    tell_rate: bool,
    /// Whether the driver takes Swarm's SPS and PPS.
    packed: bool,
    /// Whether it takes Swarm's slice headers too (Mesa's driver for
    /// AMD's chips needs them: without, it codes slices of no NAL type).
    slice_headers: bool,
    sps: Sps,
    pps: Pps,
    /// The latest SPS and PPS the stream had, put in front of a full
    /// frame that comes without them.
    parameter_sets: Option<Vec<u8>>,
}

unsafe impl Send for VaEncoder {}

impl VaEncoder {
    unsafe fn open(device: &'static Device, entrypoint: i32, shape: Shape, bitrate: u32, codec: Codec) -> Result<VaEncoder> {
        let profile = if codec == Codec::H265 { VAProfileHEVCMain } else { VAProfileH264ConstrainedBaseline };
        let mut attributes = vec![VAConfigAttrib { type_: VAConfigAttribRTFormat, value: VA_RT_FORMAT_YUV420 }, VAConfigAttrib { type_: VAConfigAttribRateControl, value: VA_RC_CBR }];
        let mut offered = [
            VAConfigAttrib { type_: VAConfigAttribRateControl, value: 0 },
            VAConfigAttrib { type_: VAConfigAttribEncPackedHeaders, value: 0 },
            VAConfigAttrib { type_: VAConfigAttribEncHEVCFeatures, value: 0 },
            VAConfigAttrib { type_: VAConfigAttribEncHEVCBlockSizes, value: 0 },
        ];
        let asked = if codec == Codec::H265 { 4 } else { 2 };
        (device.lib.get_config_attributes)(device.display, profile, entrypoint, offered.as_mut_ptr(), asked);
        if offered[0].value & VA_RC_CBR == 0 {
            attributes[1].value = VA_RC_VBR;
        }
        let packed = offered[1].value != VA_ATTRIB_NOT_SUPPORTED && offered[1].value & VA_ENC_PACKED_HEADER_SEQUENCE != 0 && std::env::var_os("SWARM_VA_DRIVER_HEADERS").is_none();
        let slice_headers = packed && offered[1].value & VA_ENC_PACKED_HEADER_SLICE != 0;
        if packed {
            let value = if slice_headers { VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_SLICE } else { VA_ENC_PACKED_HEADER_SEQUENCE };
            attributes.push(VAConfigAttrib { type_: VAConfigAttribEncPackedHeaders, value });
        }
        let mut config = VA_INVALID_ID;
        device.check((device.lib.create_config)(device.display, profile, entrypoint, attributes.as_mut_ptr(), attributes.len() as c_int, &mut config), "setting up encoding")?;
        let known = |a: &VAConfigAttrib| (a.value != VA_ATTRIB_NOT_SUPPORTED && a.value != 0).then_some(a.value);
        let (features, block_sizes) = (known(&offered[2]), known(&offered[3]));
        let (log2_ctb, log2_min_cb) = match block_sizes {
            Some(b) => (bits_of(b, 0, 2) + 3, bits_of(b, 4, 2) + 3),
            None => (5, 4),
        };
        let (width, height) = if codec == Codec::H265 {
            let (align_x, align_y) = surface_alignment(device, config);
            (shape.width.next_multiple_of((1 << log2_min_cb).max(align_x)), shape.height.next_multiple_of((1 << log2_min_cb).max(align_y)))
        } else {
            (shape.width.next_multiple_of(16), shape.height.next_multiple_of(16))
        };
        let (columns, rows) = (width / 16, height / 16);
        if codec == Codec::H265 && !packed && (width, height) != (shape.width, shape.height) {
            (device.lib.destroy_config)(device.display, config);
            bail!("{} writes its own H.265 parameter sets, which can't say to show {}×{} of {width}×{height}", device.name, shape.width, shape.height);
        }
        let mut encoder = VaEncoder {
            device,
            config,
            context: VA_INVALID_ID,
            shape,
            width,
            height,
            columns,
            rows,
            hevc: None,
            input: VA_INVALID_SURFACE,
            recon: [VA_INVALID_SURFACE; 2],
            transfer: Transfer::Derive,
            coded: VA_INVALID_ID,
            current: 0,
            since_idr: None,
            idr_pic_id: 0,
            bitrate,
            tell_rate: true,
            packed,
            slice_headers,
            sps: Sps::default(),
            pps: Pps::default(),
            parameter_sets: None,
        };
        let mut surfaces = device.surfaces(width, height, 3)?;
        encoder.input = surfaces[0];
        encoder.recon = [surfaces[1], surfaces[2]];
        device.check((device.lib.create_context)(device.display, config, width as c_int, height as c_int, VA_PROGRESSIVE, surfaces.as_mut_ptr(), 3, &mut encoder.context), "starting an encoder")?;
        let size = (width * height * 3 / 2 + 65_536) as c_uint;
        device.check((device.lib.create_buffer)(device.display, encoder.context, VAEncCodedBufferType, size, 1, std::ptr::null_mut(), &mut encoder.coded), "making its output buffer")?;
        encoder.transfer = Transfer::choose(device, encoder.input, width, height, false)?;
        if codec == Codec::H265 {
            let feature = |at: u32, otherwise: bool| features.map_or(otherwise, |f| bits_of(f, at, 2) != 0);
            let (min_tb, max_tb, depth_inter, depth_intra) = match block_sizes {
                Some(b) => (bits_of(b, 8, 2), bits_of(b, 6, 2), bits_of(b, 10, 2), bits_of(b, 14, 2)),
                None => (0, 3, 3, 3),
            };
            let sps = h265::Sps {
                ptl: h265::Ptl::main(h265_level_for(shape.width, shape.height, shape.fps)),
                chroma_format_idc: 1,
                width: width as u32,
                height: height as u32,
                conformance: [0, ((width - shape.width) / 2) as u32, 0, ((height - shape.height) / 2) as u32],
                log2_max_poc_lsb_minus4: H265_LOG2_MAX_POC_LSB_MINUS4,
                max_dec_pic_buffering_minus1: 1,
                log2_min_cb_minus3: log2_min_cb - 3,
                log2_diff_max_min_cb: log2_ctb - log2_min_cb,
                log2_min_tb_minus2: min_tb,
                log2_diff_max_min_tb: max_tb.saturating_sub(min_tb),
                max_transform_hierarchy_depth_inter: depth_inter,
                max_transform_hierarchy_depth_intra: depth_intra,
                amp: feature(4, true),
                sao: feature(6, false),
                temporal_mvp: feature(10, false),
                vui_fps: Some(shape.fps),
                ..h265::Sps::default()
            };
            let cu_qp_delta = feature(22, true);
            let pps = h265::Pps {
                cu_qp_delta_enabled: cu_qp_delta,
                diff_cu_qp_delta_depth: if cu_qp_delta && features.is_some() { sps.log2_diff_max_min_cb } else { 0 },
                transform_skip: feature(20, false),
                loop_filter_across_slices: true,
                ..h265::Pps::default()
            };
            let (x, y) = sps.ctbs();
            encoder.hevc = Some(Hevc { sps, pps, ctbs: x * y });
            if debugging() {
                eprintln!("va: H.265 encoder {}×{} (surfaces {width}×{height}), coding trees of {}, blocks from {}, features {features:?}, block sizes {block_sizes:?}, {} headers (offered {:#x}), {:?}", shape.width, shape.height, 1 << log2_ctb, 1 << log2_min_cb, if slice_headers { "Swarm's, slices' too" } else if packed { "Swarm's" } else { "the driver's" }, offered[1].value, if attributes[1].value == VA_RC_CBR { "CBR" } else { "VBR" });
            }
            return Ok(encoder);
        }
        let (crop_right, crop_bottom) = (((columns * 16 - shape.width) / 2) as u32, ((rows * 16 - shape.height) / 2) as u32);
        encoder.sps = Sps {
            profile_idc: 66,
            constraints: 0xc0,
            level_idc: level_for(shape.width, shape.height, shape.fps),
            chroma_format_idc: 1,
            log2_max_frame_num_minus4: LOG2_MAX_FRAME_NUM_MINUS4,
            pic_order_cnt_type: POC_TYPE,
            log2_max_pic_order_cnt_lsb_minus4: LOG2_MAX_POC_LSB_MINUS4,
            max_num_ref_frames: 1,
            width_in_mbs: columns as u32,
            height_in_map_units: rows as u32,
            frame_mbs_only_flag: true,
            direct_8x8_inference_flag: true,
            crop: [0, crop_right, 0, crop_bottom],
            vui_fps: Some(shape.fps),
            ..Sps::default()
        };
        encoder.pps = Pps::default();
        if debugging() {
            eprintln!("va: encoder {}×{} ({columns}×{rows} macroblocks), entrypoint {entrypoint}, {} headers (offered {:#x}), {:?}", shape.width, shape.height, if slice_headers { "Swarm's, slices' too" } else if packed { "Swarm's" } else { "the driver's" }, offered[1].value, if attributes[1].value == VA_RC_CBR { "CBR" } else { "VBR" });
        }
        Ok(encoder)
    }

    /// Swarm's parameter sets (H.264's SPS and PPS; H.265's VPS too),
    /// each with its start code.
    fn parameter_sets_written(&self) -> Vec<u8> {
        match &self.hevc {
            Some(hevc) => [&[0, 0, 0, 1][..], &h265::write_vps(&hevc.sps, self.shape.fps), &[0, 0, 0, 1], &hevc.sps.write(), &[0, 0, 0, 1], &hevc.pps.write()].concat(),
            None => [&[0, 0, 0, 1][..], &self.sps.write(), &[0, 0, 0, 1], &self.pps.write()].concat(),
        }
    }

    fn gop(&self) -> u32 {
        self.shape.fps * swarm_core::video::KEYFRAME_EVERY_SECS
    }

    unsafe fn upload(&self, picture: &Picture) -> Result<()> {
        let (width, height) = (self.width, self.height);
        let mut mapped = self.transfer.map(self.device, self.input, width, height, false)?;
        let image = mapped.image;
        let (pitch_y, pitch_uv) = (image.pitches[0] as usize, image.pitches[1] as usize);
        let (at_y, at_uv) = (image.offsets[0] as usize, image.offsets[1] as usize);
        let (pw, ph) = (picture.width, picture.height);
        if pitch_y < pw || at_y + pitch_y * (height - 1) + pw > mapped.len() || at_uv + pitch_uv * (height / 2 - 1) + pw > mapped.len() {
            bail!("an image too small for the picture ({}×{}, pitches {pitch_y} and {pitch_uv})", image.width, image.height);
        }
        let picture = picture.to_i420();
        let (y, u, v) = picture.planes();
        let out = mapped.bytes();
        for row in 0..height {
            let from = row.min(ph - 1) * pw;
            out[at_y + row * pitch_y..][..pw].copy_from_slice(&y[from..from + pw]);
        }
        for row in 0..height / 2 {
            let from = row.min(ph / 2 - 1) * (pw / 2);
            let line = &mut out[at_uv + row * pitch_uv..][..pw];
            for (x, pair) in line.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                pair[0] = u[from + x];
                pair[1] = v[from + x];
            }
        }
        drop(mapped);
        self.transfer.put(self.device, self.input, width, height)
    }

    unsafe fn encode(&mut self, picture: &Picture, key: bool) -> Result<Vec<u8>> {
        if (picture.width, picture.height) != (self.shape.width, self.shape.height) {
            bail!("a {}×{} picture for a {}×{} encoder", picture.width, picture.height, self.shape.width, self.shape.height);
        }
        self.upload(picture)?;
        let idr = key || self.since_idr.is_none_or(|n| n + 1 >= self.gop());
        if idr && self.since_idr.is_some() {
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
        }
        let n = if idr { 0 } else { self.since_idr.unwrap() + 1 };
        let frame_num = n % (1 << (LOG2_MAX_FRAME_NUM_MINUS4 + 4));
        let poc = 2 * n;
        let (reference, recon) = (self.recon[1 - self.current], self.recon[self.current]);
        let picture_of = |surface: VASurfaceID, frame_num: u32, poc: u32, flags: u32| VAPictureH264 { picture_id: surface, frame_idx: frame_num, flags, TopFieldOrderCnt: poc as i32, BottomFieldOrderCnt: poc as i32, va_reserved: [0; 4] };
        let mut buffers = Vec::new();
        let made = (|| -> Result<()> {
            let device = self.device;
            if self.hevc.is_some() {
                return self.hevc_buffers(&mut buffers, idr, n, reference, recon);
            }
            if idr {
                let mut seq: VAEncSequenceParameterBufferH264 = Plain::zeroed();
                seq.level_idc = self.sps.level_idc;
                seq.intra_period = self.gop();
                seq.intra_idr_period = self.gop();
                seq.ip_period = 1;
                seq.bits_per_second = self.bitrate;
                seq.max_num_ref_frames = 1;
                seq.picture_width_in_mbs = self.columns as u16;
                seq.picture_height_in_mbs = self.rows as u16;
                seq.seq_fields = bits(0, 2, 1) | bits(2, 1, 1) | bits(5, 1, 1) | bits(6, 4, LOG2_MAX_FRAME_NUM_MINUS4) | bits(10, 2, POC_TYPE) | bits(12, 4, LOG2_MAX_POC_LSB_MINUS4);
                let [_, right, _, bottom] = self.sps.crop;
                if right + bottom > 0 {
                    seq.frame_cropping_flag = 1;
                    seq.frame_crop_right_offset = right;
                    seq.frame_crop_bottom_offset = bottom;
                }
                seq.vui_parameters_present_flag = 1;
                seq.vui_fields = bits(0, 1, 1) | bits(1, 1, 1) | bits(2, 1, 1) | bits(3, 5, 15) | bits(8, 5, 15) | bits(15, 1, 1);
                seq.aspect_ratio_idc = 1;
                seq.num_units_in_tick = 1;
                seq.time_scale = self.shape.fps * 2;
                buffers.push(device.buffer(self.context, VAEncSequenceParameterBufferType, seq.as_bytes())?);
                if self.packed {
                    let data = self.parameter_sets_written();
                    let header = VAEncPackedHeaderParameterBuffer { type_: VAEncPackedHeaderSequence, bit_length: data.len() as u32 * 8, has_emulation_bytes: 1, va_reserved: [0; 4] };
                    buffers.push(device.buffer(self.context, VAEncPackedHeaderParameterBufferType, header.as_bytes())?);
                    buffers.push(device.buffer(self.context, VAEncPackedHeaderDataBufferType, &data)?);
                }
            }
            if idr || self.tell_rate {
                self.rate_buffers(&mut buffers)?;
            }
            let mut pic: VAEncPictureParameterBufferH264 = Plain::zeroed();
            pic.CurrPic = picture_of(recon, frame_num, poc, 0);
            pic.ReferenceFrames = [VAPictureH264::INVALID; 16];
            let previous = (n > 0).then(|| picture_of(reference, (n - 1) % (1 << (LOG2_MAX_FRAME_NUM_MINUS4 + 4)), 2 * (n - 1), VA_PICTURE_H264_SHORT_TERM_REFERENCE));
            if let Some(previous) = previous {
                pic.ReferenceFrames[0] = previous;
            }
            pic.coded_buf = self.coded;
            pic.frame_num = frame_num as u16;
            pic.pic_init_qp = 26;
            pic.pic_fields = bits(0, 1, idr as u32) | bits(1, 2, 1);
            buffers.push(device.buffer(self.context, VAEncPictureParameterBufferType, pic.as_bytes())?);
            if self.slice_headers {
                let header = SliceHeader {
                    nal_ref_idc: if idr { 3 } else { 2 },
                    idr,
                    slice_type: if idr { SLICE_I } else { SLICE_P },
                    frame_num,
                    idr_pic_id: self.idr_pic_id as u32,
                    pic_order_cnt_lsb: poc % (1 << (LOG2_MAX_POC_LSB_MINUS4 + 4)),
                    num_ref_idx_l0_active_minus1: self.pps.num_ref_idx_l0_default_active_minus1,
                    ..SliceHeader::default()
                };
                let (unit, bits) = header.write(&self.sps, &self.pps);
                let data = [&[0, 0, 0, 1][..], &unit].concat();
                let packed = VAEncPackedHeaderParameterBuffer { type_: VAEncPackedHeaderSlice, bit_length: (32 + bits) as u32, has_emulation_bytes: 1, va_reserved: [0; 4] };
                buffers.push(device.buffer(self.context, VAEncPackedHeaderParameterBufferType, packed.as_bytes())?);
                buffers.push(device.buffer(self.context, VAEncPackedHeaderDataBufferType, &data)?);
            }
            let mut slice: VAEncSliceParameterBufferH264 = Plain::zeroed();
            slice.num_macroblocks = (self.columns * self.rows) as u32;
            slice.macroblock_info = VA_INVALID_ID;
            slice.slice_type = if idr { SLICE_I } else { SLICE_P } as u8;
            slice.idr_pic_id = self.idr_pic_id;
            slice.pic_order_cnt_lsb = (poc % (1 << (LOG2_MAX_POC_LSB_MINUS4 + 4))) as u16;
            slice.RefPicList0 = [VAPictureH264::INVALID; 32];
            slice.RefPicList1 = [VAPictureH264::INVALID; 32];
            if let (Some(previous), false) = (previous, idr) {
                slice.RefPicList0[0] = previous;
            }
            buffers.push(device.buffer(self.context, VAEncSliceParameterBufferType, slice.as_bytes())?);
            Ok(())
        })();
        if let Err(e) = made {
            for buffer in buffers {
                (self.device.lib.destroy_buffer)(self.device.display, buffer);
            }
            return Err(e);
        }
        self.device.render(self.context, self.input, &mut buffers)?;
        let mut frame = self.coded_frame()?;
        self.since_idr = Some(n);
        self.current = 1 - self.current;
        self.tell_rate = false;
        if debugging() {
            let kinds: Vec<u8> = nal_units(&frame).iter().filter_map(|n| if self.hevc.is_some() { h265::nal_type(n) } else { n.first().map(|b| b & 0x1f) }).collect();
            eprintln!("va: picture {n} (full {idr}, frame_num {frame_num}): {} bytes, NAL units {kinds:?}", frame.len());
        }
        let sets = if self.hevc.is_some() { h265::parameter_sets(&frame) } else { parameter_sets(&frame) };
        match sets {
            Some(sets) => self.parameter_sets = Some(sets),
            None if idr => {
                let sets = self.parameter_sets.clone().unwrap_or_else(|| self.parameter_sets_written());
                frame.splice(0..0, sets.iter().copied());
            }
            None => {}
        }
        Ok(frame)
    }

    /// The rate, as a constant bitrate with no frames skipped.
    unsafe fn rate_buffers(&self, buffers: &mut Vec<VABufferID>) -> Result<()> {
        let device = self.device;
        let rate = VAEncMiscParameterRateControl {
            bits_per_second: self.bitrate,
            target_percentage: 100,
            window_size: 1000,
            rc_flags: bits(1, 1, 1) | bits(2, 1, 1),
            ..Default::default()
        };
        let fps = VAEncMiscParameterFrameRate { framerate: (1 << 16) | self.shape.fps, ..Default::default() };
        let hrd = VAEncMiscParameterHRD { buffer_size: self.bitrate, initial_buffer_fullness: self.bitrate / 2 * 3 / 2, ..Default::default() };
        buffers.push(device.buffer(self.context, VAEncMiscParameterBufferType, &misc(VAEncMiscParameterTypeRateControl, &rate))?);
        buffers.push(device.buffer(self.context, VAEncMiscParameterBufferType, &misc(VAEncMiscParameterTypeFrameRate, &fps))?);
        buffers.push(device.buffer(self.context, VAEncMiscParameterBufferType, &misc(VAEncMiscParameterTypeHRD, &hrd))?);
        Ok(())
    }

    /// H.265's buffers for picture `n` since the last full frame
    /// (coded into `recon`, referring to `reference`), as FFmpeg gives
    /// them: the sequence's and the parameter sets with a full frame, the
    /// rate when it's new, the picture's, and its one slice's with its
    /// header.
    unsafe fn hevc_buffers(&self, buffers: &mut Vec<VABufferID>, idr: bool, n: u32, reference: VASurfaceID, recon: VASurfaceID) -> Result<()> {
        let device = self.device;
        let hevc = self.hevc.as_ref().unwrap();
        let (sps, pps) = (&hevc.sps, &hevc.pps);
        if idr {
            let mut seq: VAEncSequenceParameterBufferHEVC = Plain::zeroed();
            seq.general_profile_idc = sps.ptl.profile_idc;
            seq.general_level_idc = sps.ptl.level_idc;
            seq.intra_period = self.gop();
            seq.intra_idr_period = self.gop();
            seq.ip_period = 1;
            seq.bits_per_second = self.bitrate;
            seq.pic_width_in_luma_samples = sps.width as u16;
            seq.pic_height_in_luma_samples = sps.height as u16;
            seq.seq_fields = bits(0, 2, sps.chroma_format_idc) | bits(10, 1, sps.strong_intra_smoothing as u32) | bits(11, 1, sps.amp as u32) | bits(12, 1, sps.sao as u32) | bits(15, 1, sps.temporal_mvp as u32);
            seq.log2_min_luma_coding_block_size_minus3 = sps.log2_min_cb_minus3 as u8;
            seq.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_cb as u8;
            seq.log2_min_transform_block_size_minus2 = sps.log2_min_tb_minus2 as u8;
            seq.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_tb as u8;
            seq.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter as u8;
            seq.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra as u8;
            buffers.push(device.buffer(self.context, VAEncSequenceParameterBufferType, seq.as_bytes())?);
            if self.packed {
                let data = self.parameter_sets_written();
                let header = VAEncPackedHeaderParameterBuffer { type_: VAEncPackedHeaderSequence, bit_length: data.len() as u32 * 8, has_emulation_bytes: 1, va_reserved: [0; 4] };
                buffers.push(device.buffer(self.context, VAEncPackedHeaderParameterBufferType, header.as_bytes())?);
                buffers.push(device.buffer(self.context, VAEncPackedHeaderDataBufferType, &data)?);
            }
        }
        if idr || self.tell_rate {
            self.rate_buffers(buffers)?;
        }
        let previous = VAPictureHEVC { picture_id: reference, pic_order_cnt: n as i32 - 1, flags: VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE, va_reserved: [0; 4] };
        let mut pic: VAEncPictureParameterBufferHEVC = Plain::zeroed();
        pic.decoded_curr_pic = VAPictureHEVC { picture_id: recon, pic_order_cnt: n as i32, flags: 0, va_reserved: [0; 4] };
        pic.reference_frames = [VAPictureHEVC::INVALID; 15];
        if !idr {
            pic.reference_frames[0] = previous;
        }
        pic.coded_buf = self.coded;
        pic.collocated_ref_pic_index = if sps.temporal_mvp { 0 } else { 0xff };
        pic.pic_init_qp = (pps.init_qp_minus26 + 26) as u8;
        pic.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth as u8;
        pic.nal_unit_type = if idr { h265::IDR_W_RADL } else { h265::TRAIL_R };
        pic.pic_fields = bits(0, 1, idr as u32) | bits(1, 3, if idr { 1 } else { 2 }) | bits(4, 1, 1) | bits(8, 1, pps.transform_skip as u32) | bits(9, 1, pps.cu_qp_delta_enabled as u32) | bits(16, 1, pps.loop_filter_across_slices as u32);
        buffers.push(device.buffer(self.context, VAEncPictureParameterBufferType, pic.as_bytes())?);
        let header = h265::SliceHeader {
            nal_type: pic.nal_unit_type,
            slice_type: if idr { h265::SLICE_I } else { h265::SLICE_P },
            poc_lsb: n % sps.max_poc_lsb(),
            rps: h265::Rps { negative: if idr { vec![] } else { vec![(-1, true)] }, positive: vec![] },
            temporal_mvp: sps.temporal_mvp,
            sao_luma: sps.sao,
            sao_chroma: sps.sao,
            ..h265::SliceHeader::default()
        };
        if self.slice_headers {
            let unit = header.write(sps, pps);
            let data = [&[0, 0, 0, 1][..], &unit].concat();
            let packed = VAEncPackedHeaderParameterBuffer { type_: VAEncPackedHeaderSlice, bit_length: data.len() as u32 * 8, has_emulation_bytes: 1, va_reserved: [0; 4] };
            buffers.push(device.buffer(self.context, VAEncPackedHeaderParameterBufferType, packed.as_bytes())?);
            buffers.push(device.buffer(self.context, VAEncPackedHeaderDataBufferType, &data)?);
        }
        let mut slice: VAEncSliceParameterBufferHEVC = Plain::zeroed();
        slice.num_ctu_in_slice = hevc.ctbs;
        slice.slice_type = header.slice_type as u8;
        slice.ref_pic_list0 = [VAPictureHEVC::INVALID; 15];
        slice.ref_pic_list1 = [VAPictureHEVC::INVALID; 15];
        if !idr {
            slice.ref_pic_list0[0] = previous;
        }
        slice.max_num_merge_cand = 5 - header.five_minus_max_num_merge_cand as u8;
        slice.slice_qp_delta = header.qp_delta as i8;
        slice.slice_fields = bits(0, 1, 1) | bits(4, 1, header.temporal_mvp as u32) | bits(5, 1, header.sao_luma as u32) | bits(6, 1, header.sao_chroma as u32) | bits(13, 1, !idr as u32);
        buffers.push(device.buffer(self.context, VAEncSliceParameterBufferType, slice.as_bytes())?);
        Ok(())
    }

    /// What it coded, from its output buffer.
    unsafe fn coded_frame(&self) -> Result<Vec<u8>> {
        let mut segment: *mut c_void = std::ptr::null_mut();
        self.device.check((self.device.lib.map_buffer)(self.device.display, self.coded, &mut segment), "reading its output")?;
        let mut frame = Vec::new();
        while !segment.is_null() {
            let seg = &*(segment as *const VACodedBufferSegment);
            if !seg.buf.is_null() && seg.size > 0 {
                frame.extend_from_slice(std::slice::from_raw_parts(seg.buf as *const u8, seg.size as usize));
            }
            segment = seg.next;
        }
        (self.device.lib.unmap_buffer)(self.device.display, self.coded);
        Ok(frame)
    }
}

impl Drop for VaEncoder {
    fn drop(&mut self) {
        unsafe {
            let (lib, display) = (self.device.lib, self.device.display);
            self.transfer.release(self.device);
            if self.coded != VA_INVALID_ID {
                (lib.destroy_buffer)(display, self.coded);
            }
            if self.context != VA_INVALID_ID {
                (lib.destroy_context)(display, self.context);
            }
            let mut surfaces: Vec<VASurfaceID> = [self.input, self.recon[0], self.recon[1]].into_iter().filter(|&s| s != VA_INVALID_SURFACE).collect();
            if !surfaces.is_empty() {
                (lib.destroy_surfaces)(display, surfaces.as_mut_ptr(), surfaces.len() as c_int);
            }
            (lib.destroy_config)(display, self.config);
        }
    }
}

impl HardEncoder for VaEncoder {
    fn encode(&mut self, picture: &Picture, key: bool) -> Result<Vec<u8>> {
        unsafe { VaEncoder::encode(self, picture, key) }
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            self.tell_rate = true;
        }
        Ok(())
    }

    fn name(&self) -> String {
        self.device.name.clone()
    }
}

/// A picture kept for reference.
#[derive(Clone, Copy, Debug)]
struct Reference {
    surface: VASurfaceID,
    frame_num: u32,
    poc: i32,
}

/// The decoder's set-up for one size of picture.
struct Session {
    config: VAConfigID,
    context: VAContextID,
    surfaces: Vec<VASurfaceID>,
    transfer: Transfer,
    /// Its surfaces' size, and the most references it was made for.
    width: u32,
    height: u32,
    references: u32,
}

impl Session {
    /// Decoding for `profile`, onto `count` surfaces `width`×`height`.
    unsafe fn open(device: &Device, profile: i32, width: u32, height: u32, count: usize, references: u32) -> Result<Session> {
        let mut attributes = [VAConfigAttrib { type_: VAConfigAttribRTFormat, value: VA_RT_FORMAT_YUV420 }];
        let mut config = VA_INVALID_ID;
        device.check((device.lib.create_config)(device.display, profile, VAEntrypointVLD, attributes.as_mut_ptr(), 1, &mut config), "setting up decoding")?;
        let (w, h) = (width as usize, height as usize);
        let mut surfaces = match device.surfaces(w, h, count) {
            Ok(surfaces) => surfaces,
            Err(e) => {
                (device.lib.destroy_config)(device.display, config);
                return Err(e);
            }
        };
        let mut context = VA_INVALID_ID;
        let made = device.check((device.lib.create_context)(device.display, config, width as c_int, height as c_int, VA_PROGRESSIVE, surfaces.as_mut_ptr(), count as c_int, &mut context), "starting a decoder");
        let transfer = made.and_then(|()| Transfer::choose(device, surfaces[0], w, h, true));
        match transfer {
            Ok(transfer) => {
                if debugging() {
                    eprintln!("va: decoder for {width}×{height}, {count} surfaces, {}", if matches!(transfer, Transfer::Derive) { "derived" } else { "copied out" });
                }
                Ok(Session { config, context, surfaces, transfer, width, height, references })
            }
            Err(e) => {
                if context != VA_INVALID_ID {
                    (device.lib.destroy_context)(device.display, context);
                }
                (device.lib.destroy_surfaces)(device.display, surfaces.as_mut_ptr(), count as c_int);
                (device.lib.destroy_config)(device.display, config);
                Err(e)
            }
        }
    }

    unsafe fn close(mut self, device: &Device) {
        let (lib, display) = (device.lib, device.display);
        self.transfer.release(device);
        (lib.destroy_context)(display, self.context);
        (lib.destroy_surfaces)(display, self.surfaces.as_mut_ptr(), self.surfaces.len() as c_int);
        (lib.destroy_config)(display, self.config);
    }

    /// `surface`'s picture, `shown` (width, height, left, top) of it, as
    /// I420.
    unsafe fn read(&self, device: &Device, surface: VASurfaceID, shown: (usize, usize, usize, usize)) -> Result<Picture> {
        let mut mapped = self.transfer.map(device, surface, self.width as usize, self.height as usize, true)?;
        let image = mapped.image;
        let (shown_width, shown_height, left, top) = shown;
        let (pitch, at_y, at_uv) = (image.pitches[0] as usize, image.offsets[0] as usize, image.offsets[1] as usize);
        if image.pitches[1] as usize != pitch {
            bail!("an image with planes of different pitches");
        }
        let (start, uv_start) = (at_y + top * pitch + left, at_uv + top / 2 * pitch + left);
        if uv_start < start {
            bail!("an image with its colour before its brightness");
        }
        let data = mapped.bytes().get(start..).ok_or_else(|| anyhow!("an image smaller than its picture"))?;
        i420_from_nv12(data, shown_width, shown_height, pitch, uv_start - start).ok_or_else(|| anyhow!("an image smaller than its picture"))
    }
}

/// What a picture order count of type 0 or 2 is worked out from: the
/// last reference picture's, or the last picture's.
#[derive(Default)]
struct Order {
    prev_msb: i32,
    prev_lsb: i32,
    prev_frame_num: u32,
    prev_frame_num_offset: u32,
}

impl Order {
    /// The picture order count of a picture (8.2.1, types 0 and 2), and
    /// what's kept for the next.
    fn count(&mut self, sps: &Sps, slice: &SliceHeader) -> i32 {
        let order = self;
        let poc = if sps.pic_order_cnt_type == 0 {
            let max_lsb = 1i32 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);
            if slice.idr {
                order.prev_msb = 0;
                order.prev_lsb = 0;
            }
            let lsb = slice.pic_order_cnt_lsb as i32;
            let msb = if lsb < order.prev_lsb && order.prev_lsb - lsb >= max_lsb / 2 {
                order.prev_msb + max_lsb
            } else if lsb > order.prev_lsb && lsb - order.prev_lsb > max_lsb / 2 {
                order.prev_msb - max_lsb
            } else {
                order.prev_msb
            };
            if slice.nal_ref_idc != 0 {
                order.prev_msb = msb;
                order.prev_lsb = lsb;
            }
            msb + lsb
        } else {
            let offset = if slice.idr {
                0
            } else if order.prev_frame_num > slice.frame_num {
                order.prev_frame_num_offset + sps.max_frame_num()
            } else {
                order.prev_frame_num_offset
            };
            order.prev_frame_num_offset = offset;
            if slice.idr {
                0
            } else {
                2 * (offset + slice.frame_num) as i32 - (slice.nal_ref_idc == 0) as i32
            }
        };
        order.prev_frame_num = slice.frame_num;
        poc
    }
}

struct VaDecoder {
    device: &'static Device,
    profile: i32,
    spss: HashMap<u32, Sps>,
    ppss: HashMap<u32, Pps>,
    session: Option<Session>,
    references: Vec<Reference>,
    order: Order,
}

unsafe impl Send for VaDecoder {}

impl VaDecoder {
    fn new(device: &'static Device, profile: i32) -> VaDecoder {
        VaDecoder { device, profile, spss: HashMap::new(), ppss: HashMap::new(), session: None, references: Vec::new(), order: Order::default() }
    }

    /// A session for pictures as `sps` has them, made anew if the size
    /// (or the references) changed.
    unsafe fn session_for(&mut self, sps: &Sps) -> Result<()> {
        let (width, height) = (sps.width_in_mbs * 16, sps.height_in_mbs() * 16);
        if self.session.as_ref().is_some_and(|s| s.width == width && s.height == height && s.references >= sps.max_num_ref_frames) {
            return Ok(());
        }
        self.end_session();
        self.references.clear();
        let count = sps.max_num_ref_frames.max(1) as usize + 2;
        self.session = Some(Session::open(self.device, self.profile, width, height, count, sps.max_num_ref_frames)?);
        Ok(())
    }

    fn end_session(&mut self) {
        if let Some(session) = self.session.take() {
            unsafe { session.close(self.device) };
        }
    }

    /// List 0 for a P slice (8.2.4): the references by descending picture
    /// number, reordered as the slice says, `active` long.
    fn list_0(&self, sps: &Sps, slice: &SliceHeader) -> Result<Vec<Option<Reference>>> {
        let max = sps.max_frame_num() as i64;
        let current = slice.frame_num as i64;
        let pic_num = |r: &Reference| if r.frame_num as i64 > current { r.frame_num as i64 - max } else { r.frame_num as i64 };
        let mut initial = self.references.clone();
        initial.sort_by_key(|r| std::cmp::Reverse(pic_num(r)));
        let active = slice.num_ref_idx_l0_active_minus1 as usize + 1;
        let mut list: Vec<Option<Reference>> = initial.into_iter().map(Some).collect();
        list.resize(active, None);
        let mut predicted = current;
        for (index, &(idc, abs_diff_minus1)) in slice.modifications.iter().enumerate() {
            if index >= active {
                bail!("more reorderings than references");
            }
            let diff = abs_diff_minus1 as i64 + 1;
            let mut no_wrap = if idc == 0 { predicted - diff } else { predicted + diff };
            if no_wrap < 0 {
                no_wrap += max;
            } else if no_wrap >= max {
                no_wrap -= max;
            }
            predicted = no_wrap;
            let wanted = if no_wrap > current { no_wrap - max } else { no_wrap };
            let picture = self.references.iter().copied().find(|r| pic_num(r) == wanted);
            list.insert(index, picture);
            let mut kept = index + 1;
            for c in index + 1..list.len() {
                let same = match (list[c], picture) {
                    (Some(a), Some(b)) => pic_num(&a) == pic_num(&b),
                    _ => false,
                };
                if !same {
                    list[kept] = list[c];
                    kept += 1;
                }
            }
            list.truncate(active);
        }
        Ok(list)
    }

    /// Marks the picture just decoded as a reference (8.2.5).
    fn mark(&mut self, sps: &Sps, slice: &SliceHeader, current: Reference) {
        if slice.nal_ref_idc == 0 {
            return;
        }
        if slice.idr {
            self.references.clear();
        } else if slice.adaptive_marking {
            let max = sps.max_frame_num() as i64;
            for &(operation, difference_minus1) in &slice.mmco {
                match operation {
                    1 => {
                        let mut gone = slice.frame_num as i64 - (difference_minus1 as i64 + 1);
                        if gone < 0 {
                            gone += max;
                        }
                        self.references.retain(|r| r.frame_num as i64 != gone);
                    }
                    5 => {
                        self.references.clear();
                        self.order = Order::default();
                    }
                    _ => {}
                }
            }
        } else {
            let most = sps.max_num_ref_frames.max(1) as usize;
            while self.references.len() >= most {
                let current_num = slice.frame_num as i64;
                let max = sps.max_frame_num() as i64;
                let oldest = (0..self.references.len()).min_by_key(|&i| {
                    let n = self.references[i].frame_num as i64;
                    if n > current_num { n - max } else { n }
                });
                self.references.remove(oldest.unwrap());
            }
        }
        self.references.push(current);
    }

    unsafe fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        let mut slices = Vec::new();
        for nal in nal_units(frame) {
            match nal.first().map(|b| b & 0x1f) {
                Some(7) => {
                    // After a slice, it would change what that slice was
                    // read with: encoders send them first.
                    if !slices.is_empty() {
                        bail!("a parameter set after a slice in the same frame");
                    }
                    let sps = Sps::parse(nal).ok_or_else(|| anyhow!("an SPS it can't take (scaling lists?)"))?;
                    self.spss.insert(sps.id, sps);
                }
                Some(8) => {
                    // After a slice, it would change what that slice was
                    // read with: encoders send them first.
                    if !slices.is_empty() {
                        bail!("a parameter set after a slice in the same frame");
                    }
                    let pps = Pps::parse(nal).ok_or_else(|| anyhow!("a PPS it can't take (slice groups or scaling lists?)"))?;
                    self.ppss.insert(pps.id, pps);
                }
                Some(1 | 5) => {
                    let header = SliceHeader::parse(nal, &self.spss, &self.ppss).ok_or_else(|| anyhow!("a slice it can't take (B, fields, weighted or long-term?), or without its SPS and PPS"))?;
                    slices.push((header, nal));
                }
                _ => {}
            }
        }
        let Some((first, _)) = slices.first() else {
            return Ok(None);
        };
        let first = first.clone();
        let pps = self.ppss.get(&first.pps_id).cloned().ok_or_else(|| anyhow!("a slice whose PPS has gone"))?;
        let sps = self.spss.get(&pps.sps_id).cloned().ok_or_else(|| anyhow!("a PPS whose SPS there isn't"))?;
        if sps.chroma_format_idc != 1 || sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
            bail!("not 8-bit 4:2:0");
        }
        self.session_for(&sps)?;
        if first.idr {
            self.references.clear();
        }
        let poc = self.order.count(&sps, &first);
        let session = self.session.as_ref().unwrap();
        let target = *session.surfaces.iter().find(|s| !self.references.iter().any(|r| r.surface == **s)).ok_or_else(|| anyhow!("no surface free"))?;
        let as_picture = |r: &Reference| VAPictureH264 { picture_id: r.surface, frame_idx: r.frame_num, flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE, TopFieldOrderCnt: r.poc, BottomFieldOrderCnt: r.poc, va_reserved: [0; 4] };
        let mut pic: VAPictureParameterBufferH264 = Plain::zeroed();
        pic.CurrPic = VAPictureH264 { picture_id: target, frame_idx: first.frame_num, flags: 0, TopFieldOrderCnt: poc, BottomFieldOrderCnt: poc + first.delta_pic_order_cnt_bottom, va_reserved: [0; 4] };
        pic.ReferenceFrames = [VAPictureH264::INVALID; 16];
        for (slot, reference) in pic.ReferenceFrames.iter_mut().zip(&self.references) {
            *slot = as_picture(reference);
        }
        pic.picture_width_in_mbs_minus1 = (sps.width_in_mbs - 1) as u16;
        pic.picture_height_in_mbs_minus1 = (sps.height_in_mbs() - 1) as u16;
        pic.num_ref_frames = sps.max_num_ref_frames as u8;
        pic.seq_fields = bits(0, 2, sps.chroma_format_idc)
            | bits(3, 1, sps.gaps_in_frame_num_value_allowed_flag as u32)
            | bits(4, 1, sps.frame_mbs_only_flag as u32)
            | bits(5, 1, sps.mb_adaptive_frame_field_flag as u32)
            | bits(6, 1, sps.direct_8x8_inference_flag as u32)
            | bits(7, 1, (sps.level_idc >= 31) as u32)
            | bits(8, 4, sps.log2_max_frame_num_minus4)
            | bits(12, 2, sps.pic_order_cnt_type)
            | bits(14, 4, sps.log2_max_pic_order_cnt_lsb_minus4)
            | bits(18, 1, sps.delta_pic_order_always_zero_flag as u32);
        pic.pic_init_qp_minus26 = pps.pic_init_qp_minus26 as i8;
        pic.pic_init_qs_minus26 = pps.pic_init_qs_minus26 as i8;
        pic.chroma_qp_index_offset = pps.chroma_qp_index_offset as i8;
        pic.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset as i8;
        pic.pic_fields = bits(0, 1, pps.entropy_coding_mode_flag as u32)
            | bits(4, 1, pps.transform_8x8_mode_flag as u32)
            | bits(6, 1, pps.constrained_intra_pred_flag as u32)
            | bits(7, 1, pps.bottom_field_pic_order_in_frame_present_flag as u32)
            | bits(8, 1, pps.deblocking_filter_control_present_flag as u32)
            | bits(9, 1, pps.redundant_pic_cnt_present_flag as u32)
            | bits(10, 1, (first.nal_ref_idc != 0) as u32);
        pic.frame_num = first.frame_num as u16;
        let iq = VAIQMatrixBufferH264 { ScalingList4x4: [[16; 16]; 6], ScalingList8x8: [[16; 64]; 2], va_reserved: [0; 4] };
        let device = self.device;
        let context = session.context;
        let mut buffers = Vec::new();
        let made = (|| -> Result<()> {
            buffers.push(device.buffer(context, VAPictureParameterBufferType, pic.as_bytes())?);
            buffers.push(device.buffer(context, VAIQMatrixBufferType, iq.as_bytes())?);
            for (header, nal) in &slices {
                if header.pps_id != first.pps_id || header.frame_num != first.frame_num {
                    bail!("slices of more than one picture in a frame");
                }
                let mut slice: VASliceParameterBufferH264 = Plain::zeroed();
                slice.slice_data_size = nal.len() as u32;
                slice.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
                slice.slice_data_bit_offset = header.header_bits as u16;
                slice.first_mb_in_slice = header.first_mb_in_slice as u16;
                slice.slice_type = header.slice_type as u8;
                slice.num_ref_idx_l0_active_minus1 = if header.slice_type == SLICE_P { header.num_ref_idx_l0_active_minus1 as u8 } else { 0 };
                slice.cabac_init_idc = header.cabac_init_idc as u8;
                slice.slice_qp_delta = header.slice_qp_delta as i8;
                slice.disable_deblocking_filter_idc = header.disable_deblocking_filter_idc as u8;
                slice.slice_alpha_c0_offset_div2 = header.slice_alpha_c0_offset_div2 as i8;
                slice.slice_beta_offset_div2 = header.slice_beta_offset_div2 as i8;
                slice.RefPicList0 = [VAPictureH264::INVALID; 32];
                slice.RefPicList1 = [VAPictureH264::INVALID; 32];
                if header.slice_type == SLICE_P {
                    for (slot, reference) in slice.RefPicList0.iter_mut().zip(self.list_0(&sps, header)?) {
                        if let Some(reference) = reference {
                            *slot = as_picture(&reference);
                        }
                    }
                }
                buffers.push(device.buffer(context, VASliceParameterBufferType, slice.as_bytes())?);
                buffers.push(device.buffer(context, VASliceDataBufferType, nal)?);
            }
            Ok(())
        })();
        if let Err(e) = made {
            for buffer in buffers {
                (device.lib.destroy_buffer)(device.display, buffer);
            }
            return Err(e);
        }
        device.render(context, target, &mut buffers)?;
        let current = Reference { surface: target, frame_num: first.frame_num, poc };
        self.mark(&sps, &first, current);
        let shown = sps.shown().ok_or_else(|| anyhow!("cropped to nothing"))?;
        let picture = self.session.as_ref().unwrap().read(device, target, shown)?;
        Ok(Some(picture))
    }
}

impl Drop for VaDecoder {
    fn drop(&mut self) {
        self.end_session();
    }
}

impl HardDecoder for VaDecoder {
    fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        unsafe { VaDecoder::decode(self, frame) }
    }

    fn name(&self) -> String {
        self.device.name.clone()
    }
}

/// A picture an H.265 decoder keeps, for reference.
#[derive(Clone, Copy, Debug)]
struct Kept {
    surface: VASurfaceID,
    poc: i32,
}

/// H.265 decoded on the chip: as `VaDecoder`, swarm reads the parameter
/// sets and slice headers (`h265`), works out each picture's order and
/// references (8.3.1 to 8.3.4), and keeps the references.
struct HevcDecoder {
    device: &'static Device,
    spss: HashMap<u32, h265::Sps>,
    ppss: HashMap<u32, h265::Pps>,
    session: Option<Session>,
    /// The pictures kept, as the last picture's reference set left them.
    kept: Vec<Kept>,
    /// The order of the last picture of the lowest temporal layer that
    /// could be referred to, which the next picture's is worked out from.
    prev_tid0_poc: i32,
    /// Whether a picture has been decoded since the stream (re)started
    /// (an intra picture other than an IDR starts it afresh before).
    started: bool,
    /// Leading pictures of the intra picture the stream started on: they
    /// refer to what came before it, so can't be decoded.
    skip_rasl: bool,
}

unsafe impl Send for HevcDecoder {}

impl HevcDecoder {
    fn new(device: &'static Device) -> HevcDecoder {
        HevcDecoder { device, spss: HashMap::new(), ppss: HashMap::new(), session: None, kept: Vec::new(), prev_tid0_poc: 0, started: false, skip_rasl: false }
    }

    unsafe fn session_for(&mut self, sps: &h265::Sps) -> Result<()> {
        let references = sps.max_dec_pic_buffering_minus1;
        if self.session.as_ref().is_some_and(|s| s.width == sps.width && s.height == sps.height && s.references >= references) {
            return Ok(());
        }
        self.end_session();
        self.kept.clear();
        let count = references.max(1) as usize + 2;
        self.session = Some(Session::open(self.device, VAProfileHEVCMain, sps.width, sps.height, count, references)?);
        Ok(())
    }

    fn end_session(&mut self) {
        if let Some(session) = self.session.take() {
            unsafe { session.close(self.device) };
        }
    }

    /// A picture's order (8.3.1), from the last picture of temporal layer
    /// 0 that could be referred to; `restart` for an intra picture that
    /// starts the stream afresh.
    fn order(&self, sps: &h265::Sps, slice: &h265::SliceHeader, restart: bool) -> i32 {
        let max = sps.max_poc_lsb() as i32;
        let lsb = slice.poc_lsb as i32;
        if restart {
            return lsb;
        }
        let prev_lsb = self.prev_tid0_poc.rem_euclid(max);
        let prev_msb = self.prev_tid0_poc - prev_lsb;
        let msb = if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
            prev_msb + max
        } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
            prev_msb - max
        } else {
            prev_msb
        };
        msb + lsb
    }

    unsafe fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        let mut slices = Vec::new();
        for nal in nal_units(frame) {
            match h265::nal_type(nal) {
                Some(h265::SPS) => {
                    // After a slice, it would change what that slice was
                    // read with: encoders send them first.
                    if !slices.is_empty() {
                        bail!("a parameter set after a slice in the same frame");
                    }
                    let sps = h265::Sps::parse(nal).ok_or_else(|| anyhow!("an SPS it can't take (scaling lists, or long-term pictures?)"))?;
                    self.spss.insert(sps.id, sps);
                }
                Some(h265::PPS) => {
                    // After a slice, it would change what that slice was
                    // read with: encoders send them first.
                    if !slices.is_empty() {
                        bail!("a parameter set after a slice in the same frame");
                    }
                    let pps = h265::Pps::parse(nal).ok_or_else(|| anyhow!("a PPS it can't take (scaling lists?)"))?;
                    self.ppss.insert(pps.id, pps);
                }
                Some(kind) if h265::is_slice(kind) => {
                    let header = h265::SliceHeader::parse(nal, &self.spss, &self.ppss).ok_or_else(|| anyhow!("a slice it can't take (weighted, long-term or dependent?), or without its SPS and PPS"))?;
                    slices.push((header, nal));
                }
                _ => {}
            }
        }
        let Some((first, first_nal)) = slices.first() else {
            return Ok(None);
        };
        let first = first.clone();
        if !first.first_slice_segment_in_pic {
            bail!("a picture without its first slice");
        }
        let temporal_id = first_nal.get(1).map_or(0, |b| (b & 7).saturating_sub(1));
        let pps = self.ppss.get(&first.pps_id).cloned().ok_or_else(|| anyhow!("a slice whose PPS has gone"))?;
        let sps = self.spss.get(&pps.sps_id).cloned().ok_or_else(|| anyhow!("a PPS whose SPS there isn't"))?;
        if sps.chroma_format_idc != 1 || sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
            bail!("not 8-bit 4:2:0");
        }
        let kind = first.nal_type;
        let irap = h265::is_irap(kind);
        let restart = irap && (kind < h265::CRA || !self.started);
        if irap {
            self.skip_rasl = restart;
        }
        if (kind == 8 || kind == h265::RASL_R) && self.skip_rasl {
            return Ok(None);
        }
        self.session_for(&sps)?;
        let poc = self.order(&sps, &first, restart);
        let rps = first.rps(&sps).clone();
        let mut kept = Vec::new();
        let (mut before, mut after) = (Vec::new(), Vec::new());
        if !restart {
            for (set, deltas) in [(&mut before, &rps.negative), (&mut after, &rps.positive)] {
                for &(delta, used) in deltas {
                    match self.kept.iter().find(|k| k.poc == poc + delta) {
                        Some(&k) => {
                            kept.push(k);
                            if used {
                                set.push(k);
                            }
                        }
                        None if used => bail!("picture {poc} refers to {}, which isn't here", poc + delta),
                        None => {}
                    }
                }
            }
        }
        // The VA picture has room for 15 (`ReferenceFrames`).
        if kept.len() > 15 {
            bail!("picture {poc} keeps {} references, more than 15", kept.len());
        }
        self.kept = kept;
        let session = self.session.as_ref().unwrap();
        let target = *session.surfaces.iter().find(|s| !self.kept.iter().any(|k| k.surface == **s)).ok_or_else(|| anyhow!("no surface free"))?;
        let mut pic: VAPictureParameterBufferHEVC = Plain::zeroed();
        pic.CurrPic = VAPictureHEVC { picture_id: target, pic_order_cnt: poc, flags: 0, va_reserved: [0; 4] };
        pic.ReferenceFrames = [VAPictureHEVC::INVALID; 15];
        for (slot, k) in pic.ReferenceFrames.iter_mut().zip(&self.kept) {
            let flags = if before.iter().any(|b| b.surface == k.surface) {
                VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
            } else if after.iter().any(|a| a.surface == k.surface) {
                VA_PICTURE_HEVC_RPS_ST_CURR_AFTER
            } else {
                0
            };
            *slot = VAPictureHEVC { picture_id: k.surface, pic_order_cnt: k.poc, flags, va_reserved: [0; 4] };
        }
        pic.pic_width_in_luma_samples = sps.width as u16;
        pic.pic_height_in_luma_samples = sps.height as u16;
        let pcm = sps.pcm.clone().unwrap_or_default();
        let tiles = pps.tiles.clone();
        pic.pic_fields = bits(0, 2, sps.chroma_format_idc)
            | bits(3, 1, sps.pcm.is_some() as u32)
            | bits(5, 1, pps.transform_skip as u32)
            | bits(6, 1, sps.amp as u32)
            | bits(7, 1, sps.strong_intra_smoothing as u32)
            | bits(8, 1, pps.sign_data_hiding as u32)
            | bits(9, 1, pps.constrained_intra_pred as u32)
            | bits(10, 1, pps.cu_qp_delta_enabled as u32)
            | bits(11, 1, pps.weighted_pred as u32)
            | bits(12, 1, pps.weighted_bipred as u32)
            | bits(13, 1, pps.transquant_bypass as u32)
            | bits(14, 1, tiles.is_some() as u32)
            | bits(15, 1, pps.entropy_coding_sync as u32)
            | bits(16, 1, pps.loop_filter_across_slices as u32)
            | bits(17, 1, tiles.as_ref().is_some_and(|t| t.loop_filter_across_tiles) as u32)
            | bits(18, 1, pcm.loop_filter_disabled as u32);
        pic.sps_max_dec_pic_buffering_minus1 = sps.max_dec_pic_buffering_minus1 as u8;
        if sps.pcm.is_some() {
            pic.pcm_sample_bit_depth_luma_minus1 = pcm.bit_depth_luma_minus1 as u8;
            pic.pcm_sample_bit_depth_chroma_minus1 = pcm.bit_depth_chroma_minus1 as u8;
            pic.log2_min_pcm_luma_coding_block_size_minus3 = pcm.log2_min_minus3 as u8;
            pic.log2_diff_max_min_pcm_luma_coding_block_size = pcm.log2_diff_max_min as u8;
        }
        pic.log2_min_luma_coding_block_size_minus3 = sps.log2_min_cb_minus3 as u8;
        pic.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_cb as u8;
        pic.log2_min_transform_block_size_minus2 = sps.log2_min_tb_minus2 as u8;
        pic.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_tb as u8;
        pic.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra as u8;
        pic.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter as u8;
        pic.init_qp_minus26 = pps.init_qp_minus26 as i8;
        pic.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth as u8;
        pic.pps_cb_qp_offset = pps.cb_qp_offset as i8;
        pic.pps_cr_qp_offset = pps.cr_qp_offset as i8;
        pic.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2 as u8;
        if let Some(tiles) = &tiles {
            if !tiles.fit(&sps) {
                bail!("tiles that don't fit the picture");
            }
            pic.num_tile_columns_minus1 = tiles.columns_minus1 as u8;
            pic.num_tile_rows_minus1 = tiles.rows_minus1 as u8;
            let (columns, rows) = tiles.sizes_minus1(&sps);
            for (slot, c) in pic.column_width_minus1.iter_mut().zip(columns) {
                *slot = c as u16;
            }
            for (slot, r) in pic.row_height_minus1.iter_mut().zip(rows) {
                *slot = r as u16;
            }
        }
        pic.slice_parsing_fields = bits(0, 1, pps.lists_modification_present as u32)
            | bits(1, 1, sps.long_term_refs_present as u32)
            | bits(2, 1, sps.temporal_mvp as u32)
            | bits(3, 1, pps.cabac_init_present as u32)
            | bits(4, 1, pps.output_flag_present as u32)
            | bits(5, 1, pps.dependent_slice_segments_enabled as u32)
            | bits(6, 1, pps.slice_chroma_qp_offsets_present as u32)
            | bits(7, 1, sps.sao as u32)
            | bits(8, 1, pps.deblocking_override_enabled as u32)
            | bits(9, 1, pps.deblocking_disabled as u32)
            | bits(10, 1, pps.slice_header_extension_present as u32)
            | bits(11, 1, irap as u32)
            | bits(12, 1, h265::is_idr(kind) as u32)
            | bits(13, 1, irap as u32);
        pic.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_poc_lsb_minus4 as u8;
        pic.num_short_term_ref_pic_sets = sps.short_term_rps.len() as u8;
        pic.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1 as u8;
        pic.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1 as u8;
        pic.pps_beta_offset_div2 = pps.beta_offset_div2 as i8;
        pic.pps_tc_offset_div2 = pps.tc_offset_div2 as i8;
        pic.num_extra_slice_header_bits = pps.num_extra_slice_header_bits as u8;
        pic.st_rps_bits = first.st_rps_bits as u32;
        let place = |k: &Kept| self.kept.iter().position(|o| o.surface == k.surface).map_or(0xff, |p| p as u8);
        let device = self.device;
        let context = session.context;
        let mut buffers = Vec::new();
        let count = slices.len();
        let made = (|| -> Result<()> {
            buffers.push(device.buffer(context, VAPictureParameterBufferType, pic.as_bytes())?);
            for (n, (header, nal)) in slices.iter().enumerate() {
                if header.pps_id != first.pps_id || (n > 0 && header.first_slice_segment_in_pic) {
                    bail!("slices of more than one picture in a frame");
                }
                let mut slice: VASliceParameterBufferHEVC = Plain::zeroed();
                slice.slice_data_size = nal.len() as u32;
                slice.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
                slice.slice_data_byte_offset = header.data_offset(nal) as u32;
                slice.slice_segment_address = header.segment_address;
                slice.RefPicList = [[0xff; 15]; 2];
                if header.slice_type != h265::SLICE_I {
                    let total = before.len() + after.len();
                    if total == 0 {
                        bail!("a predicted slice with nothing to refer to");
                    }
                    let lists = [(header.num_ref_idx_l0_active_minus1, &header.list_entry_l0, [&before, &after]), (header.num_ref_idx_l1_active_minus1, &header.list_entry_l1, [&after, &before])];
                    for (list, (active_minus1, entries, order)) in lists.into_iter().enumerate().take(if header.slice_type == h265::SLICE_B { 2 } else { 1 }) {
                        let active = active_minus1 as usize + 1;
                        let initial: Vec<Kept> = order.iter().flat_map(|s| s.iter().copied()).cycle().take(active.max(total)).collect();
                        for i in 0..active {
                            let at = entries.as_ref().map_or(i, |e| e[i] as usize);
                            let picture = initial.get(at).ok_or_else(|| anyhow!("a list entry past the references"))?;
                            slice.RefPicList[list][i] = place(picture);
                        }
                    }
                }
                slice.LongSliceFlags = bits(0, 1, (n + 1 == count) as u32)
                    | bits(2, 2, header.slice_type)
                    | bits(6, 1, header.sao_luma as u32)
                    | bits(7, 1, header.sao_chroma as u32)
                    | bits(8, 1, header.mvd_l1_zero as u32)
                    | bits(9, 1, header.cabac_init as u32)
                    | bits(10, 1, header.temporal_mvp as u32)
                    | bits(11, 1, header.deblocking_disabled as u32)
                    | bits(12, 1, header.collocated_from_l0 as u32)
                    | bits(13, 1, header.loop_filter_across_slices as u32);
                slice.collocated_ref_idx = if header.temporal_mvp { header.collocated_ref_idx as u8 } else { 0xff };
                if header.slice_type != h265::SLICE_I {
                    slice.num_ref_idx_l0_active_minus1 = header.num_ref_idx_l0_active_minus1 as u8;
                    slice.five_minus_max_num_merge_cand = header.five_minus_max_num_merge_cand as u8;
                }
                if header.slice_type == h265::SLICE_B {
                    slice.num_ref_idx_l1_active_minus1 = header.num_ref_idx_l1_active_minus1 as u8;
                }
                slice.slice_qp_delta = header.qp_delta as i8;
                slice.slice_cb_qp_offset = header.cb_qp_offset as i8;
                slice.slice_cr_qp_offset = header.cr_qp_offset as i8;
                slice.slice_beta_offset_div2 = header.beta_offset_div2 as i8;
                slice.slice_tc_offset_div2 = header.tc_offset_div2 as i8;
                buffers.push(device.buffer(context, VASliceParameterBufferType, slice.as_bytes())?);
                buffers.push(device.buffer(context, VASliceDataBufferType, nal)?);
            }
            Ok(())
        })();
        if let Err(e) = made {
            for buffer in buffers {
                (device.lib.destroy_buffer)(device.display, buffer);
            }
            return Err(e);
        }
        device.render(context, target, &mut buffers)?;
        self.kept.push(Kept { surface: target, poc });
        self.started = true;
        let referable = !(6..=9).contains(&kind) && !(kind <= 14 && kind % 2 == 0);
        if temporal_id == 0 && referable {
            self.prev_tid0_poc = poc;
        }
        if !first.pic_output {
            return Ok(None);
        }
        let shown = sps.shown().ok_or_else(|| anyhow!("cropped to nothing"))?;
        Ok(Some(self.session.as_ref().unwrap().read(device, target, shown)?))
    }
}

impl Drop for HevcDecoder {
    fn drop(&mut self) {
        self.end_session();
    }
}

impl HardDecoder for HevcDecoder {
    fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        unsafe { HevcDecoder::decode(self, frame) }
    }

    fn name(&self) -> String {
        self.device.name.clone()
    }
}

/// A camera's JPEGs (MJPEG) decoded on the chip, as I420 pictures.
///
/// Drivers differ in what they decode into and how they give it back, so
/// each way is tried in turn — the picture's own sampling (4:2:2, say),
/// then 4:2:0; read back as NV12, as the sampling, or derived — until one
/// gives what software gives (within 30 dB, checked on the first
/// picture). Failing all of them, the camera's JPEGs are decoded in
/// software.
pub struct JpegDecoder {
    device: &'static Device,
    session: Option<JpegSession>,
    /// Whether a picture has matched software's yet.
    checked: bool,
    /// The ways (surface format, read-back) that gave a wrong picture.
    wrong: Vec<(u32, usize)>,
}

unsafe impl Send for JpegDecoder {}

struct JpegSession {
    config: VAConfigID,
    context: VAContextID,
    surface: VASurfaceID,
    format: u32,
    width: u16,
    height: u16,
    chroma: (u8, u8),
    /// How pictures are read back: an image of `READ_BACK[n]`'s format,
    /// or derived (`READ_BACK.len()`); `None` until found.
    read: Option<(usize, Option<VAImage>)>,
}

/// The image formats a decoded JPEG is read back in (else it's derived).
const READ_BACK: [u32; 2] = [VA_FOURCC_NV12, 0];

impl JpegDecoder {
    /// One on this computer's chip, if it decodes JPEG.
    pub fn new() -> Result<JpegDecoder> {
        let va = VaApi::find()?;
        if !va.device.jpeg {
            bail!("{} doesn't decode JPEG", va.device.name);
        }
        Ok(JpegDecoder { device: va.device, session: None, checked: false, wrong: Vec::new() })
    }

    /// What it decodes on.
    pub fn name(&self) -> String {
        self.device.name.clone()
    }

    /// Decodes a camera's JPEG.
    pub fn decode(&mut self, file: &[u8]) -> Result<Picture> {
        let jpeg = crate::jpeg::Jpeg::parse(file).ok_or_else(|| anyhow!("not a baseline JPEG"))?;
        let chroma = jpeg.chroma().filter(|c| matches!(c, (2, 2) | (2, 1) | (1, 1))).ok_or_else(|| anyhow!("a JPEG sampled oddly"))?;
        let native = match chroma {
            (2, 2) => VA_RT_FORMAT_YUV420,
            (2, 1) => VA_RT_FORMAT_YUV422,
            _ => VA_RT_FORMAT_YUV444,
        };
        let formats: Vec<u32> = [native, VA_RT_FORMAT_YUV420].into_iter().enumerate().filter(|&(i, f)| i == 0 || f != native).map(|(_, f)| f).collect();
        loop {
            let fits = self.session.as_ref().is_some_and(|s| (s.width, s.height, s.chroma) == (jpeg.width, jpeg.height, chroma));
            if !fits {
                self.end_session();
                let mut last = anyhow!("no way left to decode it");
                for &format in &formats {
                    if (0..=READ_BACK.len()).all(|read| self.wrong.contains(&(format, read))) {
                        continue;
                    }
                    match unsafe { self.start(&jpeg, chroma, format) } {
                        Ok(session) => {
                            self.session = Some(session);
                            break;
                        }
                        Err(e) => last = e,
                    }
                }
                if self.session.is_none() {
                    return Err(last);
                }
            }
            unsafe { self.render(&jpeg)? };
            match unsafe { self.read_back(file) } {
                Ok(picture) => return Ok(picture),
                Err(e) if !self.checked => {
                    let format = self.session.as_ref().unwrap().format;
                    for read in 0..=READ_BACK.len() {
                        if !self.wrong.contains(&(format, read)) {
                            self.wrong.push((format, read));
                        }
                    }
                    if formats.iter().all(|&f| (0..=READ_BACK.len()).all(|read| self.wrong.contains(&(f, read)))) {
                        return Err(e);
                    }
                    self.end_session();
                }
                Err(e) => return Err(e),
            }
        }
    }

    unsafe fn start(&mut self, jpeg: &crate::jpeg::Jpeg, chroma: (u8, u8), format: u32) -> Result<JpegSession> {
        let device = self.device;
        let mut attributes = [VAConfigAttrib { type_: VAConfigAttribRTFormat, value: format }];
        let mut config = VA_INVALID_ID;
        device.check((device.lib.create_config)(device.display, VAProfileJPEGBaseline, VAEntrypointVLD, attributes.as_mut_ptr(), 1, &mut config), "setting up JPEG decoding")?;
        let mut surface = VA_INVALID_SURFACE;
        let (width, height) = (jpeg.width as c_uint, jpeg.height as c_uint);
        if let Err(e) = device.check((device.lib.create_surfaces)(device.display, format, width, height, &mut surface, 1, std::ptr::null_mut(), 0), "making a surface") {
            (device.lib.destroy_config)(device.display, config);
            return Err(e);
        }
        let mut context = VA_INVALID_ID;
        if let Err(e) = device.check((device.lib.create_context)(device.display, config, width as c_int, height as c_int, VA_PROGRESSIVE, &mut surface, 1, &mut context), "starting a JPEG decoder") {
            (device.lib.destroy_surfaces)(device.display, &mut surface, 1);
            (device.lib.destroy_config)(device.display, config);
            return Err(e);
        }
        if debugging() {
            eprintln!("va: JPEG decoder {}×{}, sampling {chroma:?}, surface format {format}", jpeg.width, jpeg.height);
        }
        Ok(JpegSession { config, context, surface, format, width: jpeg.width, height: jpeg.height, chroma, read: None })
    }

    unsafe fn render(&self, jpeg: &crate::jpeg::Jpeg) -> Result<()> {
        let session = self.session.as_ref().unwrap();
        let mut pic: VAPictureParameterBufferJPEGBaseline = Plain::zeroed();
        pic.picture_width = jpeg.width;
        pic.picture_height = jpeg.height;
        for (slot, c) in pic.components.iter_mut().zip(&jpeg.components) {
            *slot = VAJPEGComponent { component_id: c.id, h_sampling_factor: c.h, v_sampling_factor: c.v, quantiser_table_selector: c.tq };
        }
        pic.num_components = jpeg.components.len() as u8;
        let mut iq: VAIQMatrixBufferJPEGBaseline = Plain::zeroed();
        for (n, table) in jpeg.quant.iter().enumerate() {
            if let Some(table) = table {
                iq.load_quantiser_table[n] = 1;
                iq.quantiser_table[n] = *table;
            }
        }
        let mut huffman: VAHuffmanTableBufferJPEGBaseline = Plain::zeroed();
        for n in 0..2 {
            if let (Some(dc), Some(ac)) = (&jpeg.dc[n], &jpeg.ac[n]) {
                let table = &mut huffman.huffman_table[n];
                huffman.load_huffman_table[n] = 1;
                table.num_dc_codes = dc.counts;
                table.dc_values[..dc.values.len()].copy_from_slice(&dc.values);
                table.num_ac_codes = ac.counts;
                table.ac_values[..ac.values.len()].copy_from_slice(&ac.values);
            }
        }
        let mut slice: VASliceParameterBufferJPEGBaseline = Plain::zeroed();
        slice.slice_data_size = jpeg.data.len() as u32;
        slice.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
        for (slot, c) in slice.components.iter_mut().zip(&jpeg.scan) {
            *slot = VAJPEGScanComponent { component_selector: c.id, dc_table_selector: c.dc, ac_table_selector: c.ac };
        }
        slice.num_components = jpeg.scan.len() as u8;
        slice.restart_interval = jpeg.restart_interval;
        slice.num_mcus = jpeg.mcus();
        let device = self.device;
        let mut buffers = Vec::new();
        let made = (|| -> Result<()> {
            buffers.push(device.buffer(session.context, VAPictureParameterBufferType, pic.as_bytes())?);
            buffers.push(device.buffer(session.context, VAIQMatrixBufferType, iq.as_bytes())?);
            buffers.push(device.buffer(session.context, VAHuffmanTableBufferType, huffman.as_bytes())?);
            buffers.push(device.buffer(session.context, VASliceParameterBufferType, slice.as_bytes())?);
            buffers.push(device.buffer(session.context, VASliceDataBufferType, jpeg.data)?);
            Ok(())
        })();
        if let Err(e) = made {
            for buffer in buffers {
                (device.lib.destroy_buffer)(device.display, buffer);
            }
            return Err(e);
        }
        device.render(session.context, session.surface, &mut buffers)
    }

    /// The decoded picture, read back the way that works (found, the
    /// first time, by trying each against software's picture).
    unsafe fn read_back(&mut self, file: &[u8]) -> Result<Picture> {
        let device = self.device;
        let session = self.session.as_mut().unwrap();
        let (width, height) = (session.width as usize, session.height as usize);
        if let Some((_, image)) = &session.read {
            return read_image(device, session.surface, image.as_ref(), width, height);
        }
        let reference = crate::common::mjpeg_to_rgb(file).ok_or_else(|| anyhow!("software can't decode it either"))?.to_i420().into_owned();
        let mut tried = Vec::new();
        for read in 0..=READ_BACK.len() {
            if self.wrong.contains(&(session.format, read)) {
                continue;
            }
            let image = match READ_BACK.get(read) {
                Some(&fourcc) => {
                    let fourcc = if fourcc == 0 { derived_format(device, session.surface).unwrap_or(0) } else { fourcc };
                    let mut format = VAImageFormat { fourcc, byte_order: VA_LSB_FIRST, bits_per_pixel: 12, ..Default::default() };
                    let mut image = VAImage::default();
                    if fourcc == 0 || (device.lib.create_image)(device.display, &mut format, width as c_int, height as c_int, &mut image) != VA_STATUS_SUCCESS {
                        tried.push(format!("no image {}", fourcc_name(fourcc)));
                        self.wrong.push((session.format, read));
                        continue;
                    }
                    Some(image)
                }
                None => None,
            };
            let picture = read_image(device, session.surface, image.as_ref(), width, height);
            let alike = match &picture {
                Ok(p) if (p.width, p.height) == (reference.width, reference.height) => Ok(planes_psnr(p, &reference)),
                Ok(p) => Err(format!("{}×{}, not {}×{}", p.width, p.height, reference.width, reference.height)),
                Err(e) => Err(format!("{e:#}")),
            };
            if debugging() {
                eprintln!("va: JPEG read back {}: {alike:?}", image.map_or("derived".to_string(), |i| fourcc_name(i.format.fourcc)));
            }
            match (picture, alike) {
                (Ok(picture), Ok([y, u, v])) if y >= 30.0 && u >= 28.0 && v >= 28.0 => {
                    let how = image.map_or("derived".to_string(), |i| format!("as {}", fourcc_name(i.format.fourcc)));
                    crate::common::note_jpeg_checked(format!("{y:.0}, {u:.0} and {v:.0} dB (read back {how})"));
                    session.read = Some((read, image));
                    self.checked = true;
                    return Ok(picture);
                }
                (_, alike) => {
                    tried.push(match alike {
                        Err(e) => e,
                        Ok([y, u, v]) => format!("{y:.0}, {u:.0} and {v:.0} dB from software's"),
                    });
                    if let Some(image) = image {
                        (device.lib.destroy_image)(device.display, image.image_id);
                    }
                    self.wrong.push((session.format, read));
                }
            }
        }
        bail!("its pictures came back wrong: {}", tried.join("; "))
    }

    fn end_session(&mut self) {
        if let Some(mut session) = self.session.take() {
            let (lib, display) = (self.device.lib, self.device.display);
            unsafe {
                if let Some((_, Some(image))) = session.read {
                    (lib.destroy_image)(display, image.image_id);
                }
                (lib.destroy_context)(display, session.context);
                (lib.destroy_surfaces)(display, &mut session.surface, 1);
                (lib.destroy_config)(display, session.config);
            }
        }
    }
}

impl Drop for JpegDecoder {
    fn drop(&mut self) {
        self.end_session();
    }
}

fn fourcc_name(fourcc: u32) -> String {
    String::from_utf8_lossy(&fourcc.to_le_bytes()).into_owned()
}

/// The format of `surface`'s pixels, as deriving an image of it says.
unsafe fn derived_format(device: &Device, surface: VASurfaceID) -> Option<u32> {
    let mut image = VAImage::default();
    if (device.lib.derive_image)(device.display, surface, &mut image) != VA_STATUS_SUCCESS {
        return None;
    }
    (device.lib.destroy_image)(device.display, image.image_id);
    Some(image.format.fourcc)
}

/// `surface`'s JPEG picture, through `image` (or derived), as I420.
unsafe fn read_image(device: &Device, surface: VASurfaceID, image: Option<&VAImage>, width: usize, height: usize) -> Result<Picture> {
    let transfer = match image {
        Some(image) => Transfer::Image(*image),
        None => Transfer::Derive,
    };
    let mut mapped = transfer.map(device, surface, width, height, true)?;
    let image = mapped.image;
    let mut picture = i420_from_image(mapped.bytes(), &image, width, height).ok_or_else(|| anyhow!("an image in {} it can't read", fourcc_name(image.format.fourcc)))?;
    crate::common::limited_range(&mut picture);
    Ok(picture)
}

/// An image's picture (`width`×`height` of it) as I420, from NV12,
/// YUY2, UYVY or planar 4:2:0, 4:2:2 or 4:4:4.
fn i420_from_image(data: &[u8], image: &VAImage, width: usize, height: usize) -> Option<Picture> {
    let (w, h) = (width & !1, height & !1);
    let pitch = |n: usize| image.pitches[n] as usize;
    let at = |n: usize| image.offsets[n] as usize;
    let byte = |plane: usize, x: usize, y: usize| data.get(at(plane) + y * pitch(plane) + x).copied();
    let mut out = Vec::with_capacity(w * h * 3 / 2);
    let fourcc = image.format.fourcc;
    match fourcc {
        VA_FOURCC_NV12 => return i420_from_nv12(data.get(at(0)..)?, w, h, pitch(0), at(1).checked_sub(at(0))?),
        VA_FOURCC_YUY2 | VA_FOURCC_UYVY => {
            let (y_at, u_at, v_at) = if fourcc == VA_FOURCC_YUY2 { (0, 1, 3) } else { (1, 0, 2) };
            for y in 0..h {
                for x in 0..w {
                    out.push(byte(0, 2 * x + y_at, y)?);
                }
            }
            for at_ in [u_at, v_at] {
                for y in (0..h).step_by(2) {
                    for x in (0..w).step_by(2) {
                        let (a, b) = (byte(0, 2 * x + at_, y)? as u16, byte(0, 2 * x + at_, y + 1)? as u16);
                        out.push((a + b).div_ceil(2) as u8);
                    }
                }
            }
        }
        VA_FOURCC_I420 | VA_FOURCC_YV12 | VA_FOURCC_422H | VA_FOURCC_444P => {
            let (sx, sy) = match fourcc {
                VA_FOURCC_I420 | VA_FOURCC_YV12 => (2, 2),
                VA_FOURCC_422H => (2, 1),
                _ => (1, 1),
            };
            let (u, v) = if fourcc == VA_FOURCC_YV12 { (2, 1) } else { (1, 2) };
            for y in 0..h {
                out.extend_from_slice(data.get(at(0) + y * pitch(0)..)?.get(..w)?);
            }
            for plane in [u, v] {
                for y in (0..h).step_by(2) {
                    for x in (0..w).step_by(2) {
                        let (xs, ys) = ((x / sx)..=((x + 1) / sx), (y / sy)..=((y + 1) / sy));
                        let (mut sum, mut n) = (0u32, 0u32);
                        for yy in ys {
                            for xx in xs.clone() {
                                sum += byte(plane, xx, yy)? as u32;
                                n += 1;
                            }
                        }
                        out.push(((sum + n / 2) / n) as u8);
                    }
                }
            }
        }
        _ => return None,
    }
    Some(Picture::i420(w, h, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{MutexGuard, OnceLock};
    use swarm_core::video::{VideoDecoder, VideoEncoder, VideoPiece};
    use swarm_core::video::Picture;

    /// Picture order counts go on rising through the wrap of their low
    /// bits (type 0) and of frame numbers (type 2), non-reference
    /// pictures in between.
    #[test]
    fn picture_order_counts_rise_through_wraps() {
        for kind in [0, 2] {
            let sps = Sps { pic_order_cnt_type: kind, log2_max_frame_num_minus4: 0, log2_max_pic_order_cnt_lsb_minus4: 0, ..Sps::default() };
            let mut order = Order::default();
            let (mut last, mut next_frame_num) = (None, 0);
            for n in 0..200u32 {
                let reference = n % 3 != 2;
                let slice = SliceHeader { idr: n == 0, nal_ref_idc: reference as u32 * 3, frame_num: next_frame_num % 16, pic_order_cnt_lsb: (2 * n) % 16, ..SliceHeader::default() };
                next_frame_num += reference as u32;
                let poc = order.count(&sps, &slice);
                if kind == 0 {
                    assert_eq!(poc, 2 * n as i32, "type 0, picture {n}");
                }
                if let Some(last) = last {
                    assert!(poc > last, "type {kind}, picture {n}: {poc} after {last}");
                }
                last = Some(poc);
            }
        }
    }

    /// The stand-in driver (tools/fakeva), built and set for libva: `None`
    /// where there's no render node to open a display on.
    fn stand_in() -> Option<MutexGuard<'static, ()>> {
        static READY: OnceLock<bool> = OnceLock::new();
        let ready = *READY.get_or_init(|| {
            let nodes = std::fs::read_dir("/dev/dri").map(|d| d.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with("renderD"))).unwrap_or(false);
            if !nodes || Lib::get().is_err() {
                eprintln!("no render node or no libva: the VA-API tests are skipped");
                return false;
            }
            let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../tools/fakeva");
            let built = std::process::Command::new(option_env!("CARGO").unwrap_or("cargo")).args(["build", "--quiet", "--manifest-path", &format!("{root}/Cargo.toml")]).status();
            assert!(built.is_ok_and(|s| s.success()), "building the stand-in driver");
            let dir = std::env::temp_dir().join(format!("swarm-fakeva-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let link = dir.join("swarm_fake_drv_video.so");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(format!("{root}/target/debug/libswarm_fakeva.so"), &link).unwrap();
            // SAFETY: set once, before any display is opened.
            unsafe {
                std::env::set_var("LIBVA_DRIVER_NAME", "swarm_fake");
                std::env::set_var("LIBVA_DRIVERS_PATH", &dir);
            }
            true
        });
        ready.then(|| crate::common::CHIP_TESTS.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Sets the stand-in's switches (see tools/fakeva), all others off.
    fn switches(on: &[(&str, &str)]) {
        for name in ["FAKEVA_HEADERS", "FAKEVA_PACKED", "FAKEVA_DERIVE", "FAKEVA_JPEG422", "FAKEVA_MESA", "FAKEVA_H265"] {
            // SAFETY: the tests that read them hold `stand_in`'s lock.
            unsafe { std::env::remove_var(name) };
        }
        for (name, value) in on {
            unsafe { std::env::set_var(name, value) };
        }
    }

    /// What the stand-in did and found since it was last asked.
    fn report() -> String {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn() -> *mut c_char> = stand_in_lib().get(b"swarm_fakeva_report\0").unwrap();
            std::ffi::CString::from_raw(f()).into_string().unwrap()
        }
    }

    fn piece(frame: Vec<u8>, number: u32) -> VideoPiece {
        VideoPiece { frame: number, index: 0, count: 1, key: swarm_core::video::has_idr(&frame), h265: false, small: false, data: frame }
    }

    /// Swarm's encoder on the chip, through libva: full frames where
    /// asked, the bitrate changed in place, its SPS and PPS packed in, and
    /// every frame decoding in OpenH264 as other swarms decode it.
    fn encode_through(on: &[(&str, &str)], packed: bool) {
        let Some(_one) = stand_in() else { return };
        switches(on);
        report();
        let va = VaApi::find().unwrap();
        let (width, height) = (960, 540);
        let mut encoder = va.encoder(Shape { width, height, fps: 30 }, 1_000_000, Codec::H264).unwrap();
        let background = crate::pattern::test_background((width as u32, height as u32));
        let mut decoder = VideoDecoder::software().unwrap();
        let (mut keys, mut shown) = (Vec::new(), 0);
        for i in 0..40 {
            if i == 30 {
                encoder.set_bitrate(500_000).unwrap();
            }
            let picture = crate::pattern::test_pattern(i as f32 / 30.0, &background);
            let frame = encoder.encode(&picture, i == 20).unwrap();
            if swarm_core::video::has_idr(&frame) {
                keys.push(i);
                assert!(parameter_sets(&frame).is_some(), "full frame {i} without its SPS and PPS");
            }
            let decoded = decoder.push(piece(frame, i)).picture;
            if let Some(decoded) = decoded {
                assert_eq!((decoded.width, decoded.height), (width, height));
                shown += 1;
            }
        }
        assert_eq!(keys, vec![0, 20]);
        assert_eq!(shown, 40);
        let report = report();
        let first = report.lines().next().unwrap();
        assert!(!report.contains('\n'), "the driver found: {report}");
        assert!(first.contains("idrs=0,20 ") && first.contains("bitrates=1000000,1000000,500000 "), "{first}");
        assert_eq!(first.contains("packed=0"), !packed, "{first}");
    }

    #[test]
    fn the_encoder_drives_a_chip_as_va_api_says() {
        encode_through(&[], true);
    }

    #[test]
    fn full_frames_get_their_sps_and_pps_when_the_driver_gives_them_once() {
        encode_through(&[("FAKEVA_HEADERS", "first")], true);
    }

    /// Mesa's driver for AMD's chips takes its slices' NAL unit type
    /// from a packed slice header (none: units of no type, which nothing
    /// shows, as on a Radeon 820M), and its SPS and PPS with one.
    #[test]
    fn a_driver_wanting_packed_slice_headers_gets_them() {
        encode_through(&[("FAKEVA_MESA", "1")], true);
    }

    #[test]
    fn a_driver_taking_no_packed_headers_writes_its_own() {
        encode_through(&[("FAKEVA_PACKED", "0")], false);
    }

    #[test]
    fn a_driver_whose_surfaces_cant_be_derived_is_given_copies() {
        encode_through(&[("FAKEVA_DERIVE", "0")], true);
        decode_through(&[("FAKEVA_DERIVE", "0")]);
    }

    /// OpenH264's frames (as other swarms send them: slices of 1200 bytes,
    /// reordered references) decoded on the chip: the same pictures
    /// OpenH264 makes of them.
    fn decode_through(on: &[(&str, &str)]) {
        let Some(_one) = stand_in() else { return };
        switches(on);
        report();
        let va = VaApi::find().unwrap();
        let mut chip = va.decoder(Codec::H264).unwrap();
        let mut software = openh264::decoder::Decoder::new().unwrap();
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.keep_shape();
        let background = crate::pattern::test_background((1280, 720));
        let mut same = 0;
        for i in 0..30 {
            if i == 15 {
                encoder.request_keyframe();
            }
            let pieces = encoder.encode(&crate::pattern::test_pattern(i as f32 / 25.0, &background), 1200).unwrap();
            let frame: Vec<u8> = pieces.iter().flat_map(|p| p.data.clone()).collect();
            let ours = chip.decode(&frame).unwrap().expect("a picture for every frame");
            let theirs = {
                use openh264::formats::YUVSource;
                let yuv = software.decode(&frame).unwrap().expect("OpenH264's picture");
                let (w, h) = yuv.dimensions();
                let (ys, us, vs) = yuv.strides();
                let mut data = Vec::new();
                (0..h).for_each(|r| data.extend_from_slice(&yuv.y()[r * ys..][..w]));
                (0..h / 2).for_each(|r| data.extend_from_slice(&yuv.u()[r * us..][..w / 2]));
                (0..h / 2).for_each(|r| data.extend_from_slice(&yuv.v()[r * vs..][..w / 2]));
                Picture::i420(w, h, data)
            };
            assert_eq!(ours, theirs, "frame {i} differs");
            same += 1;
        }
        assert_eq!(same, 30);
        let report = report();
        assert!(!report.contains('\n'), "the driver found: {report}");
        assert!(report.contains("decoded=30"), "{report}");
    }

    #[test]
    fn the_decoder_gives_what_openh264_gives() {
        decode_through(&[]);
    }

    /// The stand-in driver's library, for what it offers tests besides
    /// being a driver (left loaded: libva has it too).
    fn stand_in_lib() -> &'static libloading::Library {
        static LIB: OnceLock<usize> = OnceLock::new();
        let lib = *LIB.get_or_init(|| {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tools/fakeva/target/debug/libswarm_fakeva.so");
            Box::leak(Box::new(unsafe { libloading::Library::new(path) }.unwrap())) as *const _ as usize
        });
        unsafe { &*(lib as *const libloading::Library) }
    }

    /// `pictures` encoded to H.265 by libx265 (through the stand-in), with
    /// `extra` of x265's settings: each frame's stream, in the order sent.
    fn x265_frames(pictures: &[Picture], extra: &str) -> Vec<Vec<u8>> {
        type New = unsafe extern "C" fn(usize, usize, *const c_char) -> *mut c_void;
        type Encode = unsafe extern "C" fn(*mut c_void, *const u8, usize, bool, *mut u8, usize) -> isize;
        type Free = unsafe extern "C" fn(*mut c_void);
        let lib = stand_in_lib();
        unsafe {
            let new: libloading::Symbol<New> = lib.get(b"swarm_fakeva_x265_new\0").unwrap();
            let encode: libloading::Symbol<Encode> = lib.get(b"swarm_fakeva_x265_encode\0").unwrap();
            let free: libloading::Symbol<Free> = lib.get(b"swarm_fakeva_x265_free\0").unwrap();
            let extra = std::ffi::CString::new(extra).unwrap();
            let x = new(pictures[0].width, pictures[0].height, extra.as_ptr());
            assert!(!x.is_null(), "libx265 with {extra:?}");
            let mut out = vec![0u8; 16 << 20];
            let mut frames = Vec::new();
            let mut push = |n: isize, out: &[u8]| {
                assert!(n >= 0, "libx265 failed");
                if n > 0 {
                    frames.push(out[..n as usize].to_vec());
                }
                n
            };
            for (i, picture) in pictures.iter().enumerate() {
                let i420 = picture.to_i420();
                let n = encode(x, i420.data.as_ptr(), i420.data.len(), i == 0, out.as_mut_ptr(), out.len());
                push(n, &out);
            }
            while push(encode(x, std::ptr::null(), 0, false, out.as_mut_ptr(), out.len()), &out) > 0 {}
            free(x);
            frames
        }
    }

    /// An H.265 stream decoded by libde265 (through the stand-in): its
    /// pictures, in the order shown.
    fn libde265_pictures(stream: &[u8], width: usize, height: usize) -> Vec<Picture> {
        type Decode = unsafe extern "C" fn(*const u8, usize, *mut u8, usize, *mut usize) -> c_int;
        let lib = stand_in_lib();
        let size = width * height * 3 / 2;
        let mut out = vec![0u8; size * 400];
        let mut count = 0;
        let status = unsafe {
            let decode: libloading::Symbol<Decode> = lib.get(b"swarm_fakeva_decode_h265\0").unwrap();
            decode(stream.as_ptr(), stream.len(), out.as_mut_ptr(), out.len(), &mut count)
        };
        assert_eq!(status, 0, "libde265 decoding");
        out.chunks_exact(size).take(count).map(|p| Picture::i420(width, height, p.to_vec())).collect()
    }

    /// H.265 as libx265 sends it with each of `settings`, decoded on the
    /// chip: every picture as libde265 decodes it (shown as decoded:
    /// with B-frames, in another order).
    fn h265_decodes_as_libde265_does(settings: &[&str]) {
        let Some(_one) = stand_in() else { return };
        switches(&[]);
        report();
        let va = VaApi::find().unwrap();
        if !va.h265().1 {
            eprintln!("no libde265: the H.265 tests are skipped");
            return;
        }
        let (width, height) = (640, 360);
        let background = crate::pattern::test_background((width as u32, height as u32));
        let pictures: Vec<Picture> = (0..24).map(|i| crate::pattern::test_pattern(i as f32 / 25.0, &background).to_i420().into_owned()).collect();
        for extra in settings {
            let frames = x265_frames(&pictures, extra);
            let theirs = libde265_pictures(&frames.concat(), width, height);
            assert_eq!(theirs.len(), pictures.len(), "{extra}: libde265's pictures");
            let mut chip = va.decoder(Codec::H265).unwrap();
            let mut ours = Vec::new();
            for (i, frame) in frames.iter().enumerate() {
                let picture = chip.decode(frame).unwrap_or_else(|e| panic!("{extra}: frame {i}: {e:#}"));
                ours.push(picture.unwrap_or_else(|| panic!("{extra}: frame {i}: no picture")));
            }
            assert_eq!(ours.len(), theirs.len(), "{extra}");
            for (i, picture) in ours.iter().enumerate() {
                assert!(theirs.contains(picture), "{extra}: picture {i} isn't one libde265 makes");
            }
            let report = report();
            assert!(!report.contains('\n'), "{extra}: the driver found: {report}");
            assert!(report.contains(&format!("decoded={}", frames.len())), "{extra}: {report}");
        }
    }

    /// As Swarm's encoders send it (P pictures on one reference, one
    /// slice), and with the rows coded in parallel (wavefronts: entry
    /// points in the slice headers) and loop filtering across slices.
    #[test]
    fn h265_decodes_on_the_chip() {
        h265_decodes_as_libde265_does(&["wpp=0", "wpp=1:sao=1:keyint=10"]);
    }

    /// What other encoders send: B-frames in a pyramid on four references
    /// (reference sets with pictures after, lists of both kinds); slices;
    /// open GOPs (intra pictures mid-stream with leading pictures).
    #[test]
    fn h265_with_b_frames_slices_and_open_gops_decodes_on_the_chip() {
        h265_decodes_as_libde265_does(&["bframes=3:b-pyramid=1:ref=4:rc-lookahead=8:rect=1:amp=1", "slices=3", "bframes=2:rc-lookahead=8:keyint=8:open-gop=1"]);
    }

    /// NVENC's H.265 (what a desktop with an NVIDIA chip sends), decoded on
    /// the chip through VA-API: as NVDEC decodes it.
    #[test]
    fn nvencs_h265_decodes_through_va_api_as_through_nvdec() {
        let Some(_one) = stand_in() else { return };
        switches(&[]);
        report();
        let (Ok(nvidia), Ok(va)) = (crate::video_nv::Nvidia::find(), VaApi::find()) else {
            eprintln!("no NVIDIA chip: skipped");
            return;
        };
        if nvidia.h265() != (true, true) || !va.h265().1 {
            eprintln!("no H.265 on the NVIDIA chip, or no libde265: skipped");
            return;
        }
        let (width, height) = (1280, 720);
        let background = crate::pattern::test_background((width as u32, height as u32));
        let mut encoder = nvidia.encoder(Shape { width, height, fps: 30 }, 2_000_000, Codec::H265).unwrap();
        let (mut nvdec, mut chip) = (nvidia.decoder(Codec::H265).unwrap(), va.decoder(Codec::H265).unwrap());
        for i in 0..30 {
            let frame = encoder.encode(&crate::pattern::test_pattern(i as f32 / 25.0, &background), i == 15).unwrap();
            let theirs = nvdec.decode(&frame).unwrap().expect("NVDEC's picture");
            let ours = chip.decode(&frame).unwrap_or_else(|e| panic!("frame {i}: {e:#}")).expect("a picture for every frame");
            assert!(ours == theirs, "frame {i} differs");
        }
        let report = report();
        assert!(!report.contains('\n'), "the driver found: {report}");
    }

    /// Swarm's H.265 encoder on the chip: full frames where asked, the
    /// bitrate changed in place, its VPS, SPS and PPS packed in (and with
    /// each full frame), and every frame decoding on the chip as libde265
    /// decodes the whole stream.
    fn encode_h265_through(on: &[(&str, &str)], packed: bool) {
        let Some(_one) = stand_in() else { return };
        switches(on);
        report();
        let va = VaApi::find().unwrap();
        if va.h265() != (true, true) {
            eprintln!("no libx265 or libde265: skipped");
            return;
        }
        let (width, height) = (960, 540);
        let mut encoder = va.encoder(Shape { width, height, fps: 30 }, 1_000_000, Codec::H265).unwrap();
        let mut chip = va.decoder(Codec::H265).unwrap();
        let background = crate::pattern::test_background((width as u32, height as u32));
        let (mut keys, mut ours, mut stream) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..40 {
            if i == 30 {
                encoder.set_bitrate(500_000).unwrap();
            }
            let picture = crate::pattern::test_pattern(i as f32 / 30.0, &background);
            let frame = encoder.encode(&picture, i == 20).unwrap();
            if swarm_core::video::is_key(&frame, Codec::H265) {
                keys.push(i);
                assert!(h265::parameter_sets(&frame).is_some_and(|s| s.len() > 20), "full frame {i} without its parameter sets");
            }
            let decoded = chip.decode(&frame).unwrap_or_else(|e| panic!("frame {i}: {e:#}")).expect("a picture for every frame");
            assert_eq!((decoded.width, decoded.height), (width, height));
            ours.push(decoded);
            stream.extend_from_slice(&frame);
        }
        assert_eq!(keys, vec![0, 20]);
        assert!(libde265_pictures(&stream, width, height) == ours, "the chip decodes it otherwise than libde265");
        let report = report();
        let first = report.lines().next().unwrap();
        assert!(!report.contains('\n'), "the driver found: {report}");
        assert!(first.contains("idrs=0,20 ") && first.contains("bitrates=1000000,1000000,500000 "), "{first}");
        assert_eq!(first.contains("packed=0"), !packed, "{first}");
    }

    #[test]
    fn the_h265_encoder_drives_a_chip_as_va_api_says() {
        encode_h265_through(&[], true);
    }

    #[test]
    fn h265_full_frames_get_their_parameter_sets_when_the_driver_gives_them_once() {
        encode_h265_through(&[("FAKEVA_HEADERS", "first")], true);
    }

    /// Mesa's driver for AMD's chips: coding trees of 64, blocks from 8,
    /// SAO, surfaces aligned 64 across and 16 down (540 rows coded as 544,
    /// 540 shown), and packed slice headers wanted.
    #[test]
    fn h265_on_amds_chips_is_given_their_blocks_and_slice_headers() {
        encode_h265_through(&[("FAKEVA_MESA", "1")], true);
    }

    #[test]
    fn h265_to_a_driver_whose_surfaces_cant_be_derived_is_given_copies() {
        encode_h265_through(&[("FAKEVA_DERIVE", "0")], true);
    }

    /// A driver taking no packed headers writes its own parameter sets,
    /// which VA-API can't tell to crop: H.265 only at sizes of whole
    /// blocks.
    #[test]
    fn h265_to_a_driver_taking_no_packed_headers_is_only_for_whole_blocks() {
        let Some(_one) = stand_in() else { return };
        switches(&[("FAKEVA_PACKED", "0")]);
        report();
        let va = VaApi::find().unwrap();
        if va.h265() != (true, true) {
            return;
        }
        let refused = va.encoder(Shape { width: 960, height: 540, fps: 30 }, 1_000_000, Codec::H265).err().expect("960×540 refused");
        assert!(format!("{refused:#}").contains("can't say to show 960×540 of 960×544"), "{refused:#}");
        let mut encoder = va.encoder(Shape { width: 1280, height: 720, fps: 30 }, 1_000_000, Codec::H265).unwrap();
        let mut chip = va.decoder(Codec::H265).unwrap();
        let background = crate::pattern::test_background((1280, 720));
        for i in 0..5 {
            let frame = encoder.encode(&crate::pattern::test_pattern(i as f32 / 30.0, &background), false).unwrap();
            let decoded = chip.decode(&frame).unwrap().expect("a picture");
            assert_eq!((decoded.width, decoded.height), (1280, 720));
        }
        let report = report();
        assert!(!report.contains('\n') && report.contains("packed=0"), "{report}");
    }

    /// A camera's JPEGs decoded on the chip give what software gives:
    /// 4:2:2 with restart markers, 4:2:0, and 4:2:0 without its Huffman
    /// tables; with the driver's 4:2:2 surfaces YUY2 or planar, and
    /// with or without deriving (each way found by trying).
    #[test]
    fn camera_jpegs_decode_on_the_chip_as_in_software() {
        use crate::jpeg::tests::{CAMERA_420, CAMERA_420_NO_TABLES, CAMERA_422};
        let Some(_one) = stand_in() else { return };
        for on in [&[][..], &[("FAKEVA_JPEG422", "422H")], &[("FAKEVA_DERIVE", "0")]] {
            switches(on);
            report();
            for (name, file) in [("4:2:2", CAMERA_422), ("4:2:0", CAMERA_420), ("4:2:0 without tables", CAMERA_420_NO_TABLES)] {
                let mut decoder = JpegDecoder::new().unwrap();
                crate::common::forget_jpeg_check();
                let software = crate::common::mjpeg_to_rgb(file).unwrap().to_i420().into_owned();
                for _ in 0..2 {
                    let chip = decoder.decode(file).unwrap_or_else(|e| panic!("{name} with {on:?}: {e:#}"));
                    assert_eq!((chip.width, chip.height), (320, 240));
                    let [y, u, v] = crate::common::planes_psnr(&chip, &software);
                    assert!(y > 40.0 && u > 35.0 && v > 35.0, "{name} with {on:?}: {y:.1}, {u:.1} and {v:.1} dB");
                }
                let checked = crate::common::jpeg_checked().unwrap_or_default();
                assert!(checked.contains(" dB (read back "), "{name} with {on:?}: {checked}");
            }
            let report = report();
            assert!(!report.contains('\n'), "the driver found: {report}");
        }
    }
}
