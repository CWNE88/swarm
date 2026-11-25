//! A stand-in VA-API driver, for swarm's tests (and to check swarm
//! against FFmpeg's use of VA-API): libva loads it as it would Intel's or
//! AMD's driver (`LIBVA_DRIVER_NAME=swarm_fake` with `LIBVA_DRIVERS_PATH`
//! where `swarm_fake_drv_video.so` is), and it does the work with
//! OpenH264, checking what it's given as a chip would need it.
//!
//! - Encoding: the picture given is encoded at the size the sequence's
//!   cropping says; the frame types, references, frame numbers and
//!   orders, rate and packed headers are checked.
//! - Decoding: an SPS and PPS are made from what VA-API is given (as
//!   AMD's firmware works from), and OpenH264 decodes the slices with
//!   them, so a wrong field shows; the references and slice data offsets
//!   are checked.
//!
//! Switches, from the environment:
//! - `FAKEVA_HEADERS=first`: the encoder's SPS and PPS come with its
//!   first full frame only.
//! - `FAKEVA_PACKED=0`: it takes no packed headers.
//! - `FAKEVA_MESA=1`: as Mesa's driver for AMD's chips (radeonsi): it
//!   offers every kind of packed header, and takes its slices' NAL unit
//!   type from the packed slice header, coding a slice of no type (0)
//!   without one; and it writes the SPS and PPS only with one.
//! - `FAKEVA_DERIVE=0`: its surfaces can't be derived (images are copied).
//! - `FAKEVA_JPEG422=422H`: 4:2:2 JPEGs decode to planar surfaces (as
//!   Intel's driver), not YUY2 (as AMD's). Images are only copied to and
//!   from surfaces of their own format, as some drivers.
//! - `FAKEVA_JPEG=0`: no JPEG decoding.
//! - `FAKEVA_VERBOSE=1`: problems are printed as found.
//! - `FAKEVA_DUMP_HEADERS=file`: each packed header given goes to
//!   `file.1`, `file.2`, … in turn.
//! - `FAKEVA_H265=0`: no H.265; `decode`: decoding only. (H.265 needs
//!   libde265 to decode, and libx265 to encode: see `hevc`.)
//!
//! What it found wrong fails that call, and `swarm_fakeva_report` says.

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc, clippy::too_many_arguments)]

#[path = "../../../media/src/va_sys.rs"]
mod va_sys;
#[path = "../../../media/src/h264.rs"]
#[allow(dead_code)]
mod h264;
#[path = "../../../media/src/h265.rs"]
#[allow(dead_code)]
mod h265;
mod hevc;
#[path = "../../../media/src/jpeg.rs"]
#[allow(dead_code)]
mod jpeg;

use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod};
use openh264::formats::{YUVSlices, YUVSource};
use openh264::OpenH264API;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::sync::Mutex;
use va_sys::*;

#[repr(C)]
pub struct DriverContext {
    driver_data: *mut c_void,
    vtable: *mut [*const c_void; 113],
    _other_vtables: [*mut c_void; 3],
    _native_dpy: *mut c_void,
    _x11_screen: c_int,
    version_major: c_int,
    version_minor: c_int,
    max_profiles: c_int,
    max_entrypoints: c_int,
    max_attributes: c_int,
    max_image_formats: c_int,
    max_subpic_formats: c_int,
    max_display_attributes: c_int,
    str_vendor: *const c_char,
}

const _: () = assert!(std::mem::offset_of!(DriverContext, version_major) == 52);
const _: () = assert!(std::mem::offset_of!(DriverContext, str_vendor) == 88);

#[derive(Default)]
struct Report {
    frames: u32,
    idrs: Vec<u32>,
    bitrates: Vec<u32>,
    packed: u32,
    decoded: u32,
    problems: Vec<String>,
}

static REPORT: Mutex<Option<Report>> = Mutex::new(None);

fn report<R>(f: impl FnOnce(&mut Report) -> R) -> R {
    f(REPORT.lock().unwrap().get_or_insert_with(Report::default))
}

/// What it's done and found, and empties it: `frames=… idrs=…
/// bitrates=… packed=… decoded=…`, then a line for each problem.
#[no_mangle]
pub extern "C" fn swarm_fakeva_report() -> *mut c_char {
    let r = REPORT.lock().unwrap().take().unwrap_or_default();
    let list = |v: &[u32]| v.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    let mut text = format!("frames={} idrs={} bitrates={} packed={} decoded={}", r.frames, list(&r.idrs), list(&r.bitrates), r.packed, r.decoded);
    for problem in r.problems {
        text.push('\n');
        text.push_str(&problem);
    }
    CString::new(text).unwrap().into_raw()
}

fn switch(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

struct Surface {
    width: usize,
    height: usize,
    /// NV12 (4:2:0); YUY2 or 422H (4:2:2, as AMD's and Intel's drivers
    /// keep JPEGs); 444P.
    fourcc: u32,
    pitch: usize,
    /// Rows allocated (more than the height, as chips do).
    rows: usize,
    data: Vec<u8>,
    /// For a decoded picture: its frame number (H.264), or its order
    /// (H.265).
    frame_num: Option<u32>,
    poc: Option<i32>,
}

/// Where each plane of a picture in `fourcc` is: (offset, pitch, bytes
/// of a row, rows).
fn planes(fourcc: u32, width: usize, height: usize, pitches: [usize; 3], offsets: [usize; 3]) -> Vec<(usize, usize, usize, usize)> {
    match fourcc {
        VA_FOURCC_NV12 => vec![(offsets[0], pitches[0], width, height), (offsets[1], pitches[1], width, height / 2)],
        VA_FOURCC_YUY2 => vec![(offsets[0], pitches[0], width * 2, height)],
        VA_FOURCC_422H => vec![(offsets[0], pitches[0], width, height), (offsets[1], pitches[1], width / 2, height), (offsets[2], pitches[2], width / 2, height)],
        _ => vec![(offsets[0], pitches[0], width, height), (offsets[1], pitches[1], width, height), (offsets[2], pitches[2], width, height)],
    }
}

impl Surface {
    fn of(fourcc: u32, width: usize, height: usize) -> Surface {
        let pitch = if fourcc == VA_FOURCC_YUY2 { 2 * width } else { width }.next_multiple_of(64) + 64;
        let rows = height.next_multiple_of(32);
        let size = match fourcc {
            VA_FOURCC_NV12 => pitch * rows * 3 / 2,
            VA_FOURCC_YUY2 => pitch * rows,
            _ => pitch * rows * 3,
        };
        let mut data = vec![16u8; size];
        if fourcc != VA_FOURCC_YUY2 {
            data[pitch * rows..].fill(128);
        }
        Surface { width, height, fourcc, pitch, rows, data, frame_num: None, poc: None }
    }

    fn pitches(&self) -> [usize; 3] {
        [self.pitch; 3]
    }

    fn offsets(&self) -> [usize; 3] {
        [0, self.pitch * self.rows, 2 * self.pitch * self.rows]
    }

    /// Puts a full-resolution YCbCr picture (as zune-jpeg gives it) in,
    /// its colour averaged down as the format needs.
    fn put_ycbcr(&mut self, ycbcr: &[u8], width: usize, height: usize) {
        let (w, h, pitch) = (width.min(self.width), height.min(self.height), self.pitch);
        let at = |x: usize, y: usize, c: usize| ycbcr[(y * width + x) * 3 + c] as u32;
        let average = |x: usize, y: usize, c: usize, dx: usize, dy: usize| {
            let (mut sum, mut n) = (0, 0);
            for yy in y..(y + dy).min(h) {
                for xx in x..(x + dx).min(w) {
                    sum += at(xx, yy, c);
                    n += 1;
                }
            }
            ((sum + n / 2) / n.max(1)) as u8
        };
        let [_, second, third] = self.offsets();
        for y in 0..h {
            for x in 0..w {
                match self.fourcc {
                    VA_FOURCC_YUY2 => self.data[y * pitch + 2 * x] = at(x, y, 0) as u8,
                    _ => self.data[y * pitch + x] = at(x, y, 0) as u8,
                }
            }
        }
        match self.fourcc {
            VA_FOURCC_NV12 => {
                for y in (0..h).step_by(2) {
                    for x in (0..w).step_by(2) {
                        self.data[second + y / 2 * pitch + x] = average(x, y, 1, 2, 2);
                        self.data[second + y / 2 * pitch + x + 1] = average(x, y, 2, 2, 2);
                    }
                }
            }
            VA_FOURCC_YUY2 => {
                for y in 0..h {
                    for x in (0..w).step_by(2) {
                        self.data[y * pitch + 2 * x + 1] = average(x, y, 1, 2, 1);
                        self.data[y * pitch + 2 * x + 3] = average(x, y, 2, 2, 1);
                    }
                }
            }
            VA_FOURCC_422H => {
                for y in 0..h {
                    for x in (0..w).step_by(2) {
                        self.data[second + y * pitch + x / 2] = average(x, y, 1, 2, 1);
                        self.data[third + y * pitch + x / 2] = average(x, y, 2, 2, 1);
                    }
                }
            }
            _ => {
                for y in 0..h {
                    for x in 0..w {
                        self.data[second + y * pitch + x] = at(x, y, 1) as u8;
                        self.data[third + y * pitch + x] = at(x, y, 2) as u8;
                    }
                }
            }
        }
    }

    fn uv_at(&self) -> usize {
        self.pitch * self.rows
    }

    /// Its picture (`width`×`height` from the top left) as I420.
    fn i420(&self, width: usize, height: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 3 / 2);
        for row in 0..height {
            out.extend_from_slice(&self.data[row * self.pitch..][..width]);
        }
        let (mut u, mut v) = (Vec::new(), Vec::new());
        for row in 0..height / 2 {
            for pair in self.data[self.uv_at() + row * self.pitch..][..width].chunks_exact(2) {
                u.push(pair[0]);
                v.push(pair[1]);
            }
        }
        out.extend(u);
        out.extend(v);
        out
    }

    /// Puts an I420 picture `width`×`height` in (as much as fits).
    fn put_i420(&mut self, i420: &[u8], width: usize, height: usize) {
        let (w, h) = (width.min(self.width), height.min(self.height));
        let (pitch, uv_at) = (self.pitch, self.uv_at());
        let (u, v) = (&i420[width * height..], &i420[width * height + width * height / 4..]);
        for row in 0..h {
            self.data[row * pitch..][..w].copy_from_slice(&i420[row * width..][..w]);
        }
        for row in 0..h / 2 {
            for x in 0..w / 2 {
                self.data[uv_at + row * pitch + 2 * x] = u[row * (width / 2) + x];
                self.data[uv_at + row * pitch + 2 * x + 1] = v[row * (width / 2) + x];
            }
        }
    }

    fn put_yuv(&mut self, yuv: &impl YUVSource) {
        let (w, h) = yuv.dimensions();
        let (ys, us, vs) = yuv.strides();
        let (w, h) = (w.min(self.width), h.min(self.height));
        let (pitch, uv_at) = (self.pitch, self.uv_at());
        for row in 0..h {
            self.data[row * pitch..][..w].copy_from_slice(&yuv.y()[row * ys..][..w]);
        }
        for row in 0..h / 2 {
            for x in 0..w / 2 {
                self.data[uv_at + row * pitch + 2 * x] = yuv.u()[row * us + x];
                self.data[uv_at + row * pitch + 2 * x + 1] = yuv.v()[row * vs + x];
            }
        }
    }
}

enum Data {
    Plain(Vec<u8>),
    Coded(Vec<u8>, Box<VACodedBufferSegment>),
    /// A derived image's: the surface's own memory.
    Surface(VASurfaceID),
}

struct Buffer {
    kind: i32,
    data: Data,
}

struct Image {
    image: VAImage,
}

struct Config {
    profile: i32,
    entrypoint: i32,
    packed: u32,
}

fn jpeg_offered() -> bool {
    switch("FAKEVA_JPEG").as_deref() != Some("0")
}

struct Enc {
    encoder: Option<Encoder>,
    seq: Option<VAEncSequenceParameterBufferH264>,
    /// The last picture's reconstruction, and its frame number.
    last: Option<(VASurfaceID, u32)>,
    keys: u32,
    /// The packed SPS and PPS, which packed slice headers are read with.
    spss: std::collections::HashMap<u32, h264::Sps>,
    ppss: std::collections::HashMap<u32, h264::Pps>,
}

struct Context {
    config: VAConfigID,
    target: VASurfaceID,
    pending: Vec<VABufferID>,
    enc: Enc,
    dec: Option<Decoder>,
    hevc_dec: Option<hevc::Dec>,
    hevc_enc: Option<hevc::Enc>,
}

#[derive(Default)]
struct State {
    next: u32,
    configs: HashMap<u32, Config>,
    surfaces: HashMap<u32, Surface>,
    contexts: HashMap<u32, Context>,
    buffers: HashMap<u32, Buffer>,
    images: HashMap<u32, Image>,
}

impl State {
    fn id(&mut self) -> u32 {
        self.next += 1;
        self.next
    }
}

unsafe fn state<'a>(ctx: *mut DriverContext) -> std::sync::MutexGuard<'a, State> {
    (*((*ctx).driver_data as *const Mutex<State>)).lock().unwrap()
}

fn problem(text: String) -> VAStatus {
    if switch("FAKEVA_VERBOSE").is_some() {
        eprintln!("fakeva: {text}");
    }
    report(|r| r.problems.push(text));
    VA_STATUS_ERROR_INVALID_PARAMETER
}

macro_rules! entry {
    ($vtable:expr, $index:expr, $f:expr) => {
        $vtable[$index] = $f as *const c_void
    };
}

unsafe extern "C" fn unimplemented() -> VAStatus {
    VA_STATUS_ERROR_UNIMPLEMENTED
}

#[no_mangle]
pub unsafe extern "C" fn __vaDriverInit_1_0(ctx: *mut DriverContext) -> VAStatus {
    let vtable = &mut *(*ctx).vtable;
    for slot in vtable.iter_mut().take(44) {
        *slot = unimplemented as *const c_void;
    }
    entry!(vtable, 0, terminate);
    entry!(vtable, 1, query_config_profiles);
    entry!(vtable, 2, query_config_entrypoints);
    entry!(vtable, 3, get_config_attributes);
    entry!(vtable, 4, create_config);
    entry!(vtable, 5, destroy_config);
    entry!(vtable, 6, query_config_attributes);
    entry!(vtable, 7, create_surfaces);
    entry!(vtable, 8, destroy_surfaces);
    entry!(vtable, 9, create_context);
    entry!(vtable, 10, destroy_context);
    entry!(vtable, 11, create_buffer);
    entry!(vtable, 12, buffer_set_num_elements);
    entry!(vtable, 13, map_buffer);
    entry!(vtable, 14, unmap_buffer);
    entry!(vtable, 15, destroy_buffer);
    entry!(vtable, 16, begin_picture);
    entry!(vtable, 17, render_picture);
    entry!(vtable, 18, end_picture);
    entry!(vtable, 19, sync_surface);
    entry!(vtable, 20, query_surface_status);
    entry!(vtable, 23, query_image_formats);
    entry!(vtable, 24, create_image);
    entry!(vtable, 25, derive_image);
    entry!(vtable, 26, destroy_image);
    entry!(vtable, 28, get_image);
    entry!(vtable, 29, put_image);
    entry!(vtable, 30, query_subpicture_formats);
    entry!(vtable, 38, query_display_attributes);
    entry!(vtable, 41, buffer_info);
    entry!(vtable, 45, create_surfaces2);
    entry!(vtable, 46, query_surface_attributes);
    let c = &mut *ctx;
    c.version_major = 1;
    c.version_minor = 0;
    c.max_profiles = 5;
    c.max_entrypoints = 2;
    c.max_attributes = 4;
    c.max_image_formats = 4;
    c.max_subpic_formats = 1;
    c.max_display_attributes = 1;
    c.str_vendor = c"swarm stand-in VA-API driver".as_ptr();
    c.driver_data = Box::into_raw(Box::new(Mutex::new(State::default()))) as *mut c_void;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn terminate(ctx: *mut DriverContext) -> VAStatus {
    drop(Box::from_raw((*ctx).driver_data as *mut Mutex<State>));
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn query_config_profiles(_: *mut DriverContext, list: *mut i32, n: *mut c_int) -> VAStatus {
    let mut profiles = vec![VAProfileH264ConstrainedBaseline, VAProfileH264Main, VAProfileH264High];
    if jpeg_offered() {
        profiles.push(VAProfileJPEGBaseline);
    }
    if hevc::offered() {
        profiles.push(VAProfileHEVCMain);
    }
    for (i, &p) in profiles.iter().enumerate() {
        *list.add(i) = p;
    }
    *n = profiles.len() as c_int;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn query_config_entrypoints(_: *mut DriverContext, profile: i32, list: *mut i32, n: *mut c_int) -> VAStatus {
    *list = VAEntrypointVLD;
    *n = 1;
    if profile == VAProfileH264ConstrainedBaseline || (profile == VAProfileHEVCMain && hevc::encodes()) {
        *list.add(1) = VAEntrypointEncSlice;
        *n = 2;
    }
    VA_STATUS_SUCCESS
}

fn mesa() -> bool {
    switch("FAKEVA_MESA").as_deref() == Some("1")
}

fn packed_offered() -> u32 {
    if switch("FAKEVA_PACKED").as_deref() == Some("0") {
        0
    } else if mesa() {
        VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE | VA_ENC_PACKED_HEADER_SLICE | VA_ENC_PACKED_HEADER_MISC | VA_ENC_PACKED_HEADER_RAW_DATA
    } else {
        VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE
    }
}

unsafe extern "C" fn get_config_attributes(_: *mut DriverContext, profile: i32, entrypoint: i32, list: *mut VAConfigAttrib, n: c_int) -> VAStatus {
    for i in 0..n as usize {
        let attribute = &mut *list.add(i);
        attribute.value = match (attribute.type_, entrypoint == VAEntrypointEncSlice) {
            (VAConfigAttribRTFormat, _) if profile == VAProfileJPEGBaseline => VA_RT_FORMAT_YUV420 | VA_RT_FORMAT_YUV422 | VA_RT_FORMAT_YUV444,
            (VAConfigAttribRTFormat, _) => VA_RT_FORMAT_YUV420,
            (VAConfigAttribRateControl, true) => VA_RC_CBR | VA_RC_VBR,
            (VAConfigAttribEncPackedHeaders, true) => packed_offered(),
            (VAConfigAttribEncMaxRefFrames, true) => 1,
            (VAConfigAttribEncHEVCFeatures, true) if profile == VAProfileHEVCMain && mesa() => hevc::mesa_features(),
            (VAConfigAttribEncHEVCBlockSizes, true) if profile == VAProfileHEVCMain && mesa() => hevc::mesa_block_sizes(),
            _ => VA_ATTRIB_NOT_SUPPORTED,
        };
    }
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn create_config(ctx: *mut DriverContext, profile: i32, entrypoint: i32, list: *mut VAConfigAttrib, n: c_int, id: *mut VAConfigID) -> VAStatus {
    let mut packed = 0;
    for i in 0..n.max(0) as usize {
        let attribute = *list.add(i);
        match attribute.type_ {
            VAConfigAttribRTFormat if attribute.value != VA_RT_FORMAT_YUV420 && !(profile == VAProfileJPEGBaseline && matches!(attribute.value, VA_RT_FORMAT_YUV422 | VA_RT_FORMAT_YUV444)) => return VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT,
            VAConfigAttribEncPackedHeaders => {
                if attribute.value & !packed_offered() != 0 {
                    return problem(format!("packed headers {:#x} asked for, not offered", attribute.value));
                }
                packed = attribute.value;
            }
            _ => {}
        }
    }
    let mut s = state(ctx);
    let new = s.id();
    s.configs.insert(new, Config { profile, entrypoint, packed });
    *id = new;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn destroy_config(ctx: *mut DriverContext, id: VAConfigID) -> VAStatus {
    state(ctx).configs.remove(&id);
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn query_config_attributes(ctx: *mut DriverContext, id: VAConfigID, profile: *mut i32, entrypoint: *mut i32, _: *mut VAConfigAttrib, n: *mut c_int) -> VAStatus {
    let s = state(ctx);
    let Some(config) = s.configs.get(&id) else { return VA_STATUS_ERROR_INVALID_CONFIG };
    *profile = config.profile;
    *entrypoint = config.entrypoint;
    *n = 0;
    VA_STATUS_SUCCESS
}

unsafe fn make_surfaces(ctx: *mut DriverContext, format: u32, width: usize, height: usize, n: usize, out: *mut VASurfaceID) -> VAStatus {
    let fourcc = match format {
        VA_RT_FORMAT_YUV420 => VA_FOURCC_NV12,
        VA_RT_FORMAT_YUV422 if switch("FAKEVA_JPEG422").as_deref() == Some("422H") => VA_FOURCC_422H,
        VA_RT_FORMAT_YUV422 => VA_FOURCC_YUY2,
        VA_RT_FORMAT_YUV444 => VA_FOURCC_444P,
        _ => return VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT,
    };
    let mut s = state(ctx);
    for i in 0..n {
        let id = s.id();
        s.surfaces.insert(id, Surface::of(fourcc, width, height));
        *out.add(i) = id;
    }
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn create_surfaces(ctx: *mut DriverContext, width: c_int, height: c_int, _: c_int, n: c_int, out: *mut VASurfaceID) -> VAStatus {
    make_surfaces(ctx, VA_RT_FORMAT_YUV420, width as usize, height as usize, n as usize, out)
}

unsafe extern "C" fn create_surfaces2(ctx: *mut DriverContext, format: c_uint, width: c_uint, height: c_uint, out: *mut VASurfaceID, n: c_uint, _: *mut c_void, _: c_uint) -> VAStatus {
    make_surfaces(ctx, format, width as usize, height as usize, n as usize, out)
}

unsafe extern "C" fn destroy_surfaces(ctx: *mut DriverContext, list: *mut VASurfaceID, n: c_int) -> VAStatus {
    let mut s = state(ctx);
    for i in 0..n as usize {
        s.surfaces.remove(&*list.add(i));
    }
    VA_STATUS_SUCCESS
}

/// A VASurfaceAttrib: the pixel format (NV12), gettable and settable.
#[repr(C)]
struct SurfaceAttrib {
    type_: i32,
    flags: u32,
    value_type: i32,
    value: [u32; 3],
}

const _: () = assert!(std::mem::size_of::<SurfaceAttrib>() == 24);

unsafe extern "C" fn query_surface_attributes(ctx: *mut DriverContext, config: VAConfigID, list: *mut SurfaceAttrib, n: *mut c_uint) -> VAStatus {
    let aligned = mesa() && state(ctx).configs.get(&config).is_some_and(|c| c.profile == VAProfileHEVCMain && c.entrypoint == VAEntrypointEncSlice);
    if !list.is_null() {
        *list = SurfaceAttrib { type_: 1, flags: 3, value_type: 1, value: [0, VA_FOURCC_NV12, 0] };
        if aligned && *n >= 2 {
            *list.add(1) = SurfaceAttrib { type_: 10, flags: 1, value_type: 1, value: [0, hevc::MESA_ALIGNMENT, 0] };
        }
    }
    *n = if aligned { 2 } else { 1 };
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn create_context(ctx: *mut DriverContext, config: VAConfigID, _: c_int, _: c_int, _: c_int, _: *mut VASurfaceID, _: c_int, id: *mut VAContextID) -> VAStatus {
    let mut s = state(ctx);
    let Some(cfg) = s.configs.get(&config) else { return VA_STATUS_ERROR_INVALID_CONFIG };
    let hevc = cfg.profile == VAProfileHEVCMain;
    let hevc_dec = if hevc && cfg.entrypoint == VAEntrypointVLD {
        match hevc::Dec::new() {
            Some(d) => Some(d),
            None => return problem("libde265's decoder wouldn't start".into()),
        }
    } else {
        None
    };
    let dec = if cfg.entrypoint == VAEntrypointVLD && !hevc {
        match Decoder::new() {
            Ok(d) => Some(d),
            Err(e) => return problem(format!("OpenH264's decoder: {e}")),
        }
    } else {
        None
    };
    let new = s.id();
    s.contexts.insert(new, Context { config, target: VA_INVALID_SURFACE, pending: Vec::new(), enc: Enc { encoder: None, seq: None, last: None, keys: 0, spss: Default::default(), ppss: Default::default() }, dec, hevc_dec, hevc_enc: None });
    *id = new;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn destroy_context(ctx: *mut DriverContext, id: VAContextID) -> VAStatus {
    state(ctx).contexts.remove(&id);
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn create_buffer(ctx: *mut DriverContext, _: VAContextID, kind: i32, size: c_uint, n: c_uint, data: *mut c_void, id: *mut VABufferID) -> VAStatus {
    let len = size as usize * n as usize;
    let bytes = if data.is_null() { vec![0u8; len] } else { std::slice::from_raw_parts(data as *const u8, len).to_vec() };
    let data = if kind == VAEncCodedBufferType {
        Data::Coded(bytes, Box::new(VACodedBufferSegment { size: 0, bit_offset: 0, status: 0, reserved: 0, buf: std::ptr::null_mut(), next: std::ptr::null_mut(), va_reserved: [0; 4] }))
    } else {
        Data::Plain(bytes)
    };
    let mut s = state(ctx);
    let new = s.id();
    s.buffers.insert(new, Buffer { kind, data });
    *id = new;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn buffer_set_num_elements(_: *mut DriverContext, _: VABufferID, _: c_uint) -> VAStatus {
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn map_buffer(ctx: *mut DriverContext, id: VABufferID, out: *mut *mut c_void) -> VAStatus {
    let mut guard = state(ctx);
    let s = &mut *guard;
    let Some(buffer) = s.buffers.get_mut(&id) else { return VA_STATUS_ERROR_INVALID_BUFFER };
    *out = match &mut buffer.data {
        Data::Plain(bytes) => bytes.as_mut_ptr() as *mut c_void,
        Data::Coded(bytes, segment) => {
            segment.buf = bytes.as_mut_ptr() as *mut c_void;
            &mut **segment as *mut VACodedBufferSegment as *mut c_void
        }
        Data::Surface(surface) => match s.surfaces.get_mut(surface) {
            Some(surface) => surface.data.as_mut_ptr() as *mut c_void,
            None => return VA_STATUS_ERROR_INVALID_SURFACE,
        },
    };
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn buffer_info(ctx: *mut DriverContext, id: VABufferID, kind: *mut i32, size: *mut c_uint, n: *mut c_uint) -> VAStatus {
    let s = state(ctx);
    let Some(buffer) = s.buffers.get(&id) else { return VA_STATUS_ERROR_INVALID_BUFFER };
    *kind = buffer.kind;
    *size = match &buffer.data {
        Data::Plain(bytes) | Data::Coded(bytes, _) => bytes.len() as c_uint,
        Data::Surface(_) => 0,
    };
    *n = 1;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn unmap_buffer(_: *mut DriverContext, _: VABufferID) -> VAStatus {
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn destroy_buffer(ctx: *mut DriverContext, id: VABufferID) -> VAStatus {
    state(ctx).buffers.remove(&id);
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn begin_picture(ctx: *mut DriverContext, context: VAContextID, target: VASurfaceID) -> VAStatus {
    let mut s = state(ctx);
    let Some(c) = s.contexts.get_mut(&context) else { return VA_STATUS_ERROR_INVALID_CONTEXT };
    c.target = target;
    c.pending.clear();
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn render_picture(ctx: *mut DriverContext, context: VAContextID, list: *mut VABufferID, n: c_int) -> VAStatus {
    let mut s = state(ctx);
    let Some(c) = s.contexts.get_mut(&context) else { return VA_STATUS_ERROR_INVALID_CONTEXT };
    c.pending.extend_from_slice(std::slice::from_raw_parts(list, n as usize));
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn end_picture(ctx: *mut DriverContext, context: VAContextID) -> VAStatus {
    let mut guard = state(ctx);
    let s = &mut *guard;
    let Some(mut c) = s.contexts.remove(&context) else { return VA_STATUS_ERROR_INVALID_CONTEXT };
    let entrypoint = s.configs.get(&c.config).map(|c| (c.entrypoint, c.packed, c.profile));
    let result = match entrypoint {
        Some((VAEntrypointEncSlice, packed, VAProfileHEVCMain)) => hevc::encode(s, &mut c, packed),
        Some((VAEntrypointEncSlice, packed, _)) => encode(s, &mut c, packed),
        Some((_, _, VAProfileJPEGBaseline)) => decode_jpeg(s, &mut c),
        Some((_, _, VAProfileHEVCMain)) => hevc::decode(s, &mut c),
        Some(_) => decode(s, &mut c),
        None => VA_STATUS_ERROR_INVALID_CONFIG,
    };
    s.contexts.insert(context, c);
    result
}

fn plain<'a>(s: &'a State, id: &VABufferID) -> Option<(i32, &'a [u8])> {
    match &s.buffers.get(id)?.data {
        Data::Plain(bytes) => Some((s.buffers[id].kind, bytes)),
        _ => None,
    }
}

fn encode(s: &mut State, c: &mut Context, packed: u32) -> VAStatus {
    let (mut seq, mut pic, mut slices, mut rate, mut headers) = (None, None, Vec::new(), None, Vec::new());
    let mut header_kind = None;
    for id in &c.pending {
        let Some((kind, bytes)) = plain(s, id) else { continue };
        match kind {
            VAEncSequenceParameterBufferType => seq = VAEncSequenceParameterBufferH264::read(bytes),
            VAEncPictureParameterBufferType => pic = VAEncPictureParameterBufferH264::read(bytes),
            VAEncSliceParameterBufferType => slices.extend(VAEncSliceParameterBufferH264::read(bytes)),
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
    let Some(pic) = pic else { return problem("a picture to encode without its picture parameters".into()) };
    let idr = bits_of(pic.pic_fields, 0, 1) == 1;
    if let Some(seq) = seq {
        c.enc.seq = Some(seq);
    }
    let Some(seq) = c.enc.seq else { return problem("encoding before any sequence parameters".into()) };
    if idr && headers.is_empty() && packed != 0 {
        return problem("a full frame without the packed headers set up for".into());
    }
    if c.enc.last.is_none() && !idr {
        return problem("the first picture not a full frame".into());
    }
    if c.enc.last.is_none() && rate.is_none() {
        return problem("no rate control with the first picture".into());
    }
    if let Some(rate) = rate {
        report(|r| r.bitrates.push(rate.bits_per_second));
    }
    let recon = pic.CurrPic.picture_id;
    if recon == c.target || !s.surfaces.contains_key(&recon) {
        return problem(format!("reconstructing into {recon}: the input, or no surface"));
    }
    if slices.is_empty() || slices.iter().map(|s| s.num_macroblocks).sum::<u32>() != seq.picture_width_in_mbs as u32 * seq.picture_height_in_mbs as u32 {
        return problem("slices not covering the picture".into());
    }
    let max_frame_num = 1u32 << (bits_of(seq.seq_fields, 6, 4) + 4);
    let max_lsb = 1u32 << (bits_of(seq.seq_fields, 12, 4) + 4);
    let frame_num = pic.frame_num as u32;
    for slice in &slices {
        if (slice.slice_type == h264::SLICE_I as u8) != idr {
            return problem(format!("slice type {} on a picture full {idr}", slice.slice_type));
        }
        if bits_of(seq.seq_fields, 10, 2) == 0 && slice.pic_order_cnt_lsb as u32 != (2 * frame_num) % max_lsb {
            return problem(format!("picture order {} with frame number {frame_num}", slice.pic_order_cnt_lsb));
        }
    }
    if idr {
        if frame_num != 0 {
            return problem(format!("a full frame numbered {frame_num}"));
        }
    } else {
        let (last, last_num) = c.enc.last.unwrap();
        if frame_num != (last_num + 1) % max_frame_num {
            return problem(format!("frame number {frame_num} after {last_num}"));
        }
        let reference = pic.ReferenceFrames[0];
        if reference.picture_id != last || reference.flags & VA_PICTURE_H264_SHORT_TERM_REFERENCE == 0 || reference.frame_idx != last_num {
            return problem(format!("reference {reference:?}, not the last picture ({last}, {last_num})"));
        }
        if slices.iter().any(|s| s.RefPicList0[0].picture_id != last) {
            return problem("a P slice not referring to the last picture".into());
        }
    }
    let (width, height) = (seq.picture_width_in_mbs as usize * 16 - 2 * (seq.frame_crop_left_offset + seq.frame_crop_right_offset) as usize, seq.picture_height_in_mbs as usize * 16 - 2 * (seq.frame_crop_top_offset + seq.frame_crop_bottom_offset) as usize);
    let mut slice_header = None;
    for (header, data) in &headers {
        if let Some(path) = switch("FAKEVA_DUMP_HEADERS") {
            static DUMPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = DUMPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let _ = std::fs::write(format!("{path}.{n}"), data);
        }
        if header.type_ == VAEncPackedHeaderSlice {
            if packed & VA_ENC_PACKED_HEADER_SLICE == 0 {
                return problem("a packed slice header not set up for".into());
            }
            if slice_header.is_some() || slices.len() != 1 {
                return problem("more than one packed slice header, or slice".into());
            }
            let units = h264::nal_units(data);
            let [unit] = units[..] else { return problem("a packed slice header not one NAL unit".into()) };
            let Some(parsed) = h264::SliceHeader::parse(&[unit, &[0xff; 4]].concat(), &c.enc.spss, &c.enc.ppss) else { return problem("a packed slice header that doesn't read".into()) };
            let start = data.len() - unit.len();
            let escapes = unit.len() - h264::unescape(unit).len();
            if header.bit_length as usize != start * 8 + parsed.header_bits + 8 * escapes || header.has_emulation_bytes != 1 {
                return problem(format!("a packed slice header of {} bits, read as {} after its start code", header.bit_length, parsed.header_bits));
            }
            let slice = &slices[0];
            if parsed.idr != idr || parsed.nal_ref_idc == 0 || parsed.first_mb_in_slice != 0 || parsed.slice_type != slice.slice_type as u32 || parsed.frame_num != frame_num || (idr && parsed.idr_pic_id != slice.idr_pic_id as u32) || parsed.slice_qp_delta != slice.slice_qp_delta as i32 {
                return problem(format!("a packed slice header unlike the slice parameters: {parsed:?}"));
            }
            slice_header = Some(unit[0]);
            continue;
        }
        if header.bit_length as usize != data.len() * 8 {
            return problem("a packed header's length not its data's".into());
        }
        for unit in h264::nal_units(data) {
        let kind = match (header.type_, unit[0] & 0x1f) {
            (VAEncPackedHeaderSequence, 7) => VAEncPackedHeaderSequence,
            (VAEncPackedHeaderSequence | VAEncPackedHeaderPicture, 8) => VAEncPackedHeaderPicture,
            (kind, unit) => return problem(format!("NAL unit {unit} in a packed header of type {kind}")),
        };
        match kind {
            VAEncPackedHeaderSequence => {
                let Some(sps) = h264::Sps::parse(unit) else { return problem("a packed SPS that doesn't read".into()) };
                c.enc.spss.insert(sps.id, sps.clone());
                let (w, h, _, _) = sps.shown().unwrap_or_default();
                if sps.profile_idc != 66 || sps.constraints & 0x40 == 0 || sps.width_in_mbs != seq.picture_width_in_mbs as u32 || sps.height_in_mbs() != seq.picture_height_in_mbs as u32 || (w, h) != (width, height) || sps.log2_max_frame_num_minus4 != bits_of(seq.seq_fields, 6, 4) || sps.pic_order_cnt_type != bits_of(seq.seq_fields, 10, 2) || sps.max_num_ref_frames != seq.max_num_ref_frames {
                    return problem(format!("a packed SPS unlike the sequence parameters: {sps:?}"));
                }
                report(|r| r.packed += 1);
            }
            VAEncPackedHeaderPicture => {
                let Some(pps) = h264::Pps::parse(unit) else { return problem("a packed PPS that doesn't read".into()) };
                c.enc.ppss.insert(pps.id, pps.clone());
                if pps.deblocking_filter_control_present_flag != (bits_of(pic.pic_fields, 9, 1) == 1) || pps.entropy_coding_mode_flag {
                    return problem(format!("a packed PPS unlike the picture parameters: {pps:?}"));
                }
            }
            _ => unreachable!(),
        }
        }
    }
    if slice_header.is_none() && packed & VA_ENC_PACKED_HEADER_SLICE != 0 && !mesa() {
        return problem("no packed slice header, though set up for".into());
    }
    let Some(source) = s.surfaces.get(&c.target) else { return VA_STATUS_ERROR_INVALID_SURFACE };
    let i420 = source.i420(width, height);
    if c.enc.encoder.is_none() {
        let bitrate = rate.map_or(seq.bits_per_second, |r| r.bits_per_second);
        let config = EncoderConfig::new().bitrate(BitRate::from_bps(bitrate.max(100_000))).max_frame_rate(FrameRate::from_hz(seq.time_scale.max(2) as f32 / 2.0)).intra_frame_period(IntraFramePeriod::from_num_frames(100_000));
        c.enc.encoder = Some(Encoder::with_api_config(OpenH264API::from_source(), config).unwrap());
    }
    let encoder = c.enc.encoder.as_mut().unwrap();
    if idr {
        encoder.force_intra_frame();
    }
    let (y, uv) = i420.split_at(width * height);
    let (u, v) = uv.split_at(width * height / 4);
    let mut frame = match encoder.encode(&YUVSlices::new((y, u, v), (width, height), (width, width / 2, width / 2))) {
        Ok(stream) => stream.to_vec(),
        Err(e) => return problem(format!("OpenH264: {e}")),
    };
    if idr {
        c.enc.keys += 1;
        if switch("FAKEVA_HEADERS").as_deref() == Some("first") && c.enc.keys > 1 {
            frame = h264::nal_units(&frame).into_iter().filter(|n| !matches!(n[0] & 0x1f, 7 | 8)).flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect();
        }
    }
    if mesa() {
        let units: Vec<Vec<u8>> = h264::nal_units(&frame).into_iter().map(|n| n.to_vec()).collect();
        frame = Vec::new();
        for mut unit in units {
            match unit[0] & 0x1f {
                1 | 5 => unit[0] = slice_header.unwrap_or(0),
                7 | 8 if slice_header.is_none() => continue,
                _ => {}
            }
            frame.extend([0, 0, 0, 1]);
            frame.extend(unit);
        }
    }
    c.enc.last = Some((recon, frame_num));
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

fn decode(s: &mut State, c: &mut Context) -> VAStatus {
    let (mut pic, mut iq, mut slices) = (None, false, Vec::new());
    let mut param = None;
    for id in &c.pending {
        let Some((kind, bytes)) = plain(s, id) else { continue };
        match kind {
            VAPictureParameterBufferType => pic = VAPictureParameterBufferH264::read(bytes),
            VAIQMatrixBufferType => iq = true,
            VASliceParameterBufferType => param = VASliceParameterBufferH264::read(bytes),
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
    if !iq {
        return problem("no inverse quantisation matrix".into());
    }
    if slices.is_empty() {
        return problem("a picture with no slices".into());
    }
    if pic.CurrPic.picture_id != c.target || pic.CurrPic.frame_idx != pic.frame_num as u32 {
        return problem(format!("the current picture {:?}, decoding into {}", pic.CurrPic, c.target));
    }
    let seq = pic.seq_fields;
    let cabac = bits_of(pic.pic_fields, 0, 1) == 1;
    let transform_8x8 = bits_of(pic.pic_fields, 4, 1) == 1;
    let high = cabac || transform_8x8;
    let sps = h264::Sps {
        profile_idc: if high { 100 } else { 66 },
        level_idc: 51,
        chroma_format_idc: bits_of(seq, 0, 2),
        gaps_in_frame_num_value_allowed_flag: bits_of(seq, 3, 1) == 1,
        frame_mbs_only_flag: bits_of(seq, 4, 1) == 1,
        mb_adaptive_frame_field_flag: bits_of(seq, 5, 1) == 1,
        direct_8x8_inference_flag: bits_of(seq, 6, 1) == 1,
        log2_max_frame_num_minus4: bits_of(seq, 8, 4),
        pic_order_cnt_type: bits_of(seq, 12, 2),
        log2_max_pic_order_cnt_lsb_minus4: bits_of(seq, 14, 4),
        delta_pic_order_always_zero_flag: bits_of(seq, 18, 1) == 1,
        max_num_ref_frames: pic.num_ref_frames as u32,
        width_in_mbs: pic.picture_width_in_mbs_minus1 as u32 + 1,
        height_in_map_units: pic.picture_height_in_mbs_minus1 as u32 + 1,
        ..h264::Sps::default()
    };
    let read_pps_id = |nal: &[u8]| {
        let rbsp = h264::unescape(&nal[1..]);
        let mut bits = h264::Bits::new(&rbsp);
        bits.ue()?;
        bits.ue()?;
        bits.ue()
    };
    let Some(pps_id) = read_pps_id(&slices[0].1) else { return problem("a slice whose header doesn't read".into()) };
    let default_refs = slices.iter().find(|(p, _)| p.slice_type as u32 == h264::SLICE_P).map_or(0, |(p, _)| p.num_ref_idx_l0_active_minus1 as u32);
    let pps = h264::Pps {
        id: pps_id,
        sps_id: 0,
        entropy_coding_mode_flag: cabac,
        bottom_field_pic_order_in_frame_present_flag: bits_of(pic.pic_fields, 7, 1) == 1,
        num_ref_idx_l0_default_active_minus1: default_refs,
        weighted_pred_flag: bits_of(pic.pic_fields, 1, 1) == 1,
        weighted_bipred_idc: bits_of(pic.pic_fields, 2, 2),
        pic_init_qp_minus26: pic.pic_init_qp_minus26 as i32,
        pic_init_qs_minus26: pic.pic_init_qs_minus26 as i32,
        chroma_qp_index_offset: pic.chroma_qp_index_offset as i32,
        deblocking_filter_control_present_flag: bits_of(pic.pic_fields, 8, 1) == 1,
        constrained_intra_pred_flag: bits_of(pic.pic_fields, 6, 1) == 1,
        redundant_pic_cnt_present_flag: bits_of(pic.pic_fields, 9, 1) == 1,
        transform_8x8_mode_flag: transform_8x8,
        second_chroma_qp_index_offset: pic.second_chroma_qp_index_offset as i32,
        ..h264::Pps::default()
    };
    let spss = HashMap::from([(0, sps.clone())]);
    let ppss = HashMap::from([(pps_id, pps.clone())]);
    let mut stream = [&[0, 0, 0, 1][..], &sps.write(), &[0, 0, 0, 1], &pps.write()].concat();
    for (param, nal) in &slices {
        let Some(header) = h264::SliceHeader::parse(nal, &spss, &ppss) else { return problem("a slice whose header doesn't read with the parameters given".into()) };
        if header.header_bits != param.slice_data_bit_offset as usize || header.first_mb_in_slice != param.first_mb_in_slice as u32 || header.slice_type != param.slice_type as u32 {
            return problem(format!("slice parameters (offset {}, first macroblock {}, type {}) unlike its header ({}, {}, {})", param.slice_data_bit_offset, param.first_mb_in_slice, param.slice_type, header.header_bits, header.first_mb_in_slice, header.slice_type));
        }
        if header.slice_type == h264::SLICE_P {
            for i in 0..=param.num_ref_idx_l0_active_minus1 as usize {
                let entry = param.RefPicList0[i];
                if entry.flags & VA_PICTURE_H264_INVALID != 0 {
                    continue;
                }
                let held = s.surfaces.get(&entry.picture_id).and_then(|s| s.frame_num);
                if held != Some(entry.frame_idx) || !pic.ReferenceFrames.iter().any(|r| r.picture_id == entry.picture_id && r.flags & VA_PICTURE_H264_INVALID == 0) {
                    return problem(format!("reference {i} {entry:?}: the surface holds {held:?}, or isn't among the references"));
                }
            }
        }
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(nal);
    }
    let decoder = c.dec.as_mut().unwrap();
    let Some(target) = s.surfaces.get_mut(&c.target) else { return VA_STATUS_ERROR_INVALID_SURFACE };
    target.frame_num = Some(pic.frame_num as u32);
    match decoder.decode(&stream) {
        Ok(Some(yuv)) => {
            target.put_yuv(&yuv);
            report(|r| r.decoded += 1);
        }
        Ok(None) if high => {}
        Ok(None) => return problem("OpenH264 gave no picture for it".into()),
        Err(e) => return problem(format!("OpenH264 couldn't decode it: {e}")),
    }
    VA_STATUS_SUCCESS
}

fn decode_jpeg(s: &mut State, c: &mut Context) -> VAStatus {
    let (mut pic, mut iq, mut huffman, mut slice, mut data) = (None, None, None, None, None);
    for id in &c.pending {
        let Some((kind, bytes)) = plain(s, id) else { continue };
        match kind {
            VAPictureParameterBufferType => pic = VAPictureParameterBufferJPEGBaseline::read(bytes),
            VAIQMatrixBufferType => iq = VAIQMatrixBufferJPEGBaseline::read(bytes),
            VAHuffmanTableBufferType => huffman = VAHuffmanTableBufferJPEGBaseline::read(bytes),
            VASliceParameterBufferType => slice = VASliceParameterBufferJPEGBaseline::read(bytes),
            VASliceDataBufferType => data = Some(bytes.to_vec()),
            _ => {}
        }
    }
    let (Some(pic), Some(iq), Some(huffman), Some(slice), Some(data)) = (pic, iq, huffman, slice, data) else { return problem("a JPEG without all its buffers".into()) };
    if slice.slice_data_size as usize != data.len() {
        return problem("JPEG data not the size its slice says".into());
    }
    let table = |load: u8, counts: [u8; 16], values: &[u8]| (load == 1).then(|| jpeg::Huffman { counts, values: values[..counts.iter().map(|&c| c as usize).sum::<usize>().min(values.len())].to_vec() });
    let jpeg = jpeg::Jpeg {
        width: pic.picture_width,
        height: pic.picture_height,
        components: pic.components[..pic.num_components as usize].iter().map(|c| jpeg::Component { id: c.component_id, h: c.h_sampling_factor, v: c.v_sampling_factor, tq: c.quantiser_table_selector }).collect(),
        quant: std::array::from_fn(|n| (iq.load_quantiser_table[n] == 1).then_some(iq.quantiser_table[n])),
        dc: std::array::from_fn(|n| table(huffman.load_huffman_table[n], huffman.huffman_table[n].num_dc_codes, &huffman.huffman_table[n].dc_values)),
        ac: std::array::from_fn(|n| table(huffman.load_huffman_table[n], huffman.huffman_table[n].num_ac_codes, &huffman.huffman_table[n].ac_values)),
        restart_interval: slice.restart_interval,
        scan: slice.components[..slice.num_components as usize].iter().map(|c| jpeg::ScanComponent { id: c.component_selector, dc: c.dc_table_selector, ac: c.ac_table_selector }).collect(),
        data: &data,
    };
    if slice.num_mcus != jpeg.mcus() {
        return problem(format!("{} MCUs said, where the picture has {}", slice.num_mcus, jpeg.mcus()));
    }
    let file = jpeg.write();
    let options = zune_jpeg::zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::YCbCr);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(&file, options);
    let ycbcr = match decoder.decode() {
        Ok(ycbcr) => ycbcr,
        Err(e) => return problem(format!("the JPEG made from what was given doesn't decode: {e:?}")),
    };
    let Some(target) = s.surfaces.get_mut(&c.target) else { return VA_STATUS_ERROR_INVALID_SURFACE };
    target.put_ycbcr(&ycbcr, jpeg.width as usize, jpeg.height as usize);
    report(|r| r.decoded += 1);
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn sync_surface(_: *mut DriverContext, _: VASurfaceID) -> VAStatus {
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn query_surface_status(_: *mut DriverContext, _: VASurfaceID, status: *mut c_int) -> VAStatus {
    *status = 4;
    VA_STATUS_SUCCESS
}

const IMAGE_FORMATS: [u32; 4] = [VA_FOURCC_NV12, VA_FOURCC_YUY2, VA_FOURCC_422H, VA_FOURCC_444P];

fn format_of(fourcc: u32) -> VAImageFormat {
    VAImageFormat { fourcc, byte_order: VA_LSB_FIRST, bits_per_pixel: if fourcc == VA_FOURCC_NV12 { 12 } else { 16 }, ..Default::default() }
}

unsafe extern "C" fn query_image_formats(_: *mut DriverContext, list: *mut VAImageFormat, n: *mut c_int) -> VAStatus {
    for (i, &fourcc) in IMAGE_FORMATS.iter().enumerate() {
        *list.add(i) = format_of(fourcc);
    }
    *n = IMAGE_FORMATS.len() as c_int;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn create_image(ctx: *mut DriverContext, format: *mut VAImageFormat, width: c_int, height: c_int, out: *mut VAImage) -> VAStatus {
    let fourcc = (*format).fourcc;
    if !IMAGE_FORMATS.contains(&fourcc) {
        return VA_STATUS_ERROR_INVALID_IMAGE;
    }
    let (width, height) = (width as usize, height as usize);
    let pitch = if fourcc == VA_FOURCC_YUY2 { 2 * width } else { width } + 32;
    let (planes, size) = match fourcc {
        VA_FOURCC_NV12 => (2, pitch * height * 3 / 2),
        VA_FOURCC_YUY2 => (1, pitch * height),
        _ => (3, pitch * height * 3),
    };
    let mut s = state(ctx);
    let (image_id, buf) = (s.id(), s.id());
    s.buffers.insert(buf, Buffer { kind: VAImageBufferType, data: Data::Plain(vec![0; size]) });
    let image = VAImage { image_id, format: format_of(fourcc), buf, width: width as u16, height: height as u16, data_size: size as u32, num_planes: planes, pitches: [pitch as u32; 3], offsets: [0, (pitch * height) as u32, (2 * pitch * height) as u32], ..Default::default() };
    s.images.insert(image_id, Image { image });
    *out = image;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn derive_image(ctx: *mut DriverContext, surface: VASurfaceID, out: *mut VAImage) -> VAStatus {
    if switch("FAKEVA_DERIVE").as_deref() == Some("0") {
        return VA_STATUS_ERROR_OPERATION_FAILED;
    }
    let mut s = state(ctx);
    let Some(sf) = s.surfaces.get(&surface) else { return VA_STATUS_ERROR_INVALID_SURFACE };
    let (width, height, fourcc, pitches, offsets, size) = (sf.width, sf.height, sf.fourcc, sf.pitches(), sf.offsets(), sf.data.len());
    let (image_id, buf) = (s.id(), s.id());
    s.buffers.insert(buf, Buffer { kind: VAImageBufferType, data: Data::Surface(surface) });
    let image = VAImage { image_id, format: format_of(fourcc), buf, width: width as u16, height: height as u16, data_size: size as u32, num_planes: planes(fourcc, width, height, pitches, offsets).len() as u32, pitches: pitches.map(|p| p as u32), offsets: offsets.map(|o| o as u32), ..Default::default() };
    s.images.insert(image_id, Image { image });
    *out = image;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn destroy_image(ctx: *mut DriverContext, id: VAImageID) -> VAStatus {
    let mut s = state(ctx);
    if let Some(image) = s.images.remove(&id) {
        s.buffers.remove(&image.image.buf);
    }
    VA_STATUS_SUCCESS
}

/// Copies between a surface and an image of its format: `width`×`height`
/// from the top left.
fn copy(surface: &mut Surface, image: &VAImage, bytes: &mut [u8], width: usize, height: usize, into_image: bool) {
    let theirs = planes(image.format.fourcc, width, height, image.pitches.map(|p| p as usize), image.offsets.map(|o| o as usize));
    let ours = planes(surface.fourcc, width, height, surface.pitches(), surface.offsets());
    for ((a_at, a_pitch, row_bytes, rows), (b_at, b_pitch, _, _)) in ours.into_iter().zip(theirs) {
        for row in 0..rows {
            let (a, b) = (a_at + row * a_pitch, b_at + row * b_pitch);
            if into_image {
                bytes[b..b + row_bytes].copy_from_slice(&surface.data[a..a + row_bytes]);
            } else {
                surface.data[a..a + row_bytes].copy_from_slice(&bytes[b..b + row_bytes]);
            }
        }
    }
}

unsafe fn transfer(ctx: *mut DriverContext, surface: VASurfaceID, image: VAImageID, x: c_int, y: c_int, width: c_uint, height: c_uint, into_image: bool) -> VAStatus {
    let mut guard = state(ctx);
    let s = &mut *guard;
    let Some(image) = s.images.get(&image).map(|i| i.image) else { return VA_STATUS_ERROR_INVALID_IMAGE };
    let (width, height) = (width as usize, height as usize);
    let Some(sf) = s.surfaces.get_mut(&surface) else { return VA_STATUS_ERROR_INVALID_SURFACE };
    if sf.fourcc != image.format.fourcc {
        return VA_STATUS_ERROR_OPERATION_FAILED;
    }
    if x != 0 || y != 0 || width > sf.width || height > sf.height || width > image.width as usize || height > image.height as usize {
        return problem(format!("copying {width}×{height} at {x},{y} between a {}×{} surface and a {}×{} image", sf.width, sf.height, image.width, image.height));
    }
    let Some(Buffer { data: Data::Plain(bytes), .. }) = s.buffers.get_mut(&image.buf) else { return VA_STATUS_ERROR_INVALID_BUFFER };
    copy(sf, &image, bytes, width, height, into_image);
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn get_image(ctx: *mut DriverContext, surface: VASurfaceID, x: c_int, y: c_int, width: c_uint, height: c_uint, image: VAImageID) -> VAStatus {
    transfer(ctx, surface, image, x, y, width, height, true)
}

unsafe extern "C" fn put_image(ctx: *mut DriverContext, surface: VASurfaceID, image: VAImageID, src_x: c_int, src_y: c_int, src_w: c_uint, src_h: c_uint, dst_x: c_int, dst_y: c_int, dst_w: c_uint, dst_h: c_uint) -> VAStatus {
    if (src_x, src_y, src_w, src_h) != (dst_x, dst_y, dst_w, dst_h) {
        return problem("a scaled or moved vaPutImage".into());
    }
    transfer(ctx, surface, image, src_x, src_y, src_w, src_h, false)
}

unsafe extern "C" fn query_subpicture_formats(_: *mut DriverContext, _: *mut VAImageFormat, _: *mut c_uint, n: *mut c_uint) -> VAStatus {
    *n = 0;
    VA_STATUS_SUCCESS
}

unsafe extern "C" fn query_display_attributes(_: *mut DriverContext, _: *mut c_void, n: *mut c_int) -> VAStatus {
    *n = 0;
    VA_STATUS_SUCCESS
}
