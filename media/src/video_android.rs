//! Android phones' video encoder (see `swarm_core::video::Hardware`),
//! through the NDK's MediaCodec: H.264 on the phone's own chip, as
//! OpenH264 encoding its camera kept it hot enough to slow everything
//! down. Constrained baseline, as every swarm decodes. Decoding stays
//! OpenH264's: it's quick enough on a phone.

use anyhow::{anyhow, bail, Result};
use std::ffi::{c_char, c_int, c_void, CStr};
use swarm_core::video::{Codec, HardDecoder, HardEncoder, Hardware, Picture, Shape};

type Status = c_int;
const OK: Status = 0;
const CONFIGURE_ENCODE: u32 = 1;
const TRY_AGAIN_LATER: isize = -1;
const FLAG_KEY_FRAME: u32 = 1;
const FLAG_CODEC_CONFIG: u32 = 2;
/// COLOR_FormatYUV420SemiPlanar (NV12) and COLOR_FormatYUV420Planar
/// (I420): what the phone's encoder may be given pictures in.
const SEMI_PLANAR: i32 = 21;
const PLANAR: i32 = 19;
/// AVCProfileConstrainedBaseline, and AVCProfileBaseline for encoders
/// that don't know it (which make it anyway, with no B frames).
const CONSTRAINED_BASELINE: i32 = 0x10000;
const BASELINE: i32 = 1;
/// MediaFormat's BITRATE_MODE_CBR.
const CBR: i32 = 2;
/// How long to wait for the encoder to take a picture, and to give its
/// frame back, in µs.
const WAIT_IN: i64 = 20_000;
const WAIT_OUT: i64 = 40_000;

#[repr(C)]
struct BufferInfo {
    offset: i32,
    size: i32,
    time: i64,
    flags: u32,
}

#[link(name = "mediandk")]
extern "C" {
    fn AMediaCodec_createEncoderByType(mime: *const c_char) -> *mut c_void;
    fn AMediaCodec_configure(codec: *mut c_void, format: *const c_void, window: *mut c_void, crypto: *mut c_void, flags: u32) -> Status;
    fn AMediaCodec_start(codec: *mut c_void) -> Status;
    fn AMediaCodec_stop(codec: *mut c_void) -> Status;
    fn AMediaCodec_delete(codec: *mut c_void) -> Status;
    fn AMediaCodec_dequeueInputBuffer(codec: *mut c_void, timeout: i64) -> isize;
    fn AMediaCodec_getInputBuffer(codec: *mut c_void, index: usize, size: *mut usize) -> *mut u8;
    fn AMediaCodec_queueInputBuffer(codec: *mut c_void, index: usize, offset: isize, size: usize, time: u64, flags: u32) -> Status;
    fn AMediaCodec_dequeueOutputBuffer(codec: *mut c_void, info: *mut BufferInfo, timeout: i64) -> isize;
    fn AMediaCodec_getOutputBuffer(codec: *mut c_void, index: usize, size: *mut usize) -> *mut u8;
    fn AMediaCodec_releaseOutputBuffer(codec: *mut c_void, index: usize, render: bool) -> Status;
    fn AMediaCodec_getInputFormat(codec: *mut c_void) -> *mut c_void;
    fn AMediaCodec_setParameters(codec: *mut c_void, format: *const c_void) -> Status;
    fn AMediaCodec_getName(codec: *mut c_void, name: *mut *mut c_char) -> Status;
    fn AMediaCodec_releaseName(codec: *mut c_void, name: *mut c_char);
    fn AMediaFormat_new() -> *mut c_void;
    fn AMediaFormat_delete(format: *mut c_void) -> Status;
    fn AMediaFormat_setString(format: *mut c_void, name: *const c_char, value: *const c_char);
    fn AMediaFormat_setInt32(format: *mut c_void, name: *const c_char, value: i32);
    fn AMediaFormat_getInt32(format: *mut c_void, name: *const c_char, value: *mut i32) -> bool;
}

/// A MediaFormat, deleted when done with.
struct Format(*mut c_void);

impl Format {
    fn new() -> Format {
        Format(unsafe { AMediaFormat_new() })
    }

    fn set(&self, name: &CStr, value: i32) -> &Format {
        unsafe { AMediaFormat_setInt32(self.0, name.as_ptr(), value) };
        self
    }

    fn get(&self, name: &CStr) -> Option<i32> {
        let mut value = 0;
        unsafe { AMediaFormat_getInt32(self.0, name.as_ptr(), &mut value) }.then_some(value)
    }
}

impl Drop for Format {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { AMediaFormat_delete(self.0) };
        }
    }
}

/// The phone's MediaCodec.
pub struct MediaCodec;

impl Hardware for MediaCodec {
    fn encoder(&self, shape: Shape, bitrate: u32, codec: Codec) -> Result<Box<dyn HardEncoder>> {
        if codec != Codec::H264 {
            bail!("only H.264 on the phone");
        }
        Ok(Box::new(Encoder::new(shape, bitrate)?))
    }

    fn decoder(&self, _codec: Codec) -> Result<Box<dyn HardDecoder>> {
        bail!("the phone decodes in OpenH264")
    }

    fn name(&self) -> String {
        "MediaCodec".to_string()
    }
}

/// How the encoder wants a picture laid out.
struct Layout {
    format: i32,
    stride: usize,
    rows: usize,
}

pub struct Encoder {
    codec: *mut c_void,
    shape: Shape,
    layout: Layout,
    bitrate: u32,
    /// The stream's SPS and PPS, which each full frame's given if it
    /// hasn't them.
    config: Vec<u8>,
    frames: u64,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(shape: Shape, bitrate: u32) -> Result<Encoder> {
        let codec = unsafe { AMediaCodec_createEncoderByType(c"video/avc".as_ptr()) };
        if codec.is_null() {
            bail!("no H.264 encoder");
        }
        let mut encoder = Encoder { codec, shape, layout: Layout { format: SEMI_PLANAR, stride: shape.width, rows: shape.height }, bitrate, config: Vec::new(), frames: 0 };
        let mut tried = Vec::new();
        let configured = [(CONSTRAINED_BASELINE, SEMI_PLANAR), (BASELINE, SEMI_PLANAR), (CONSTRAINED_BASELINE, PLANAR), (BASELINE, PLANAR)]
            .into_iter()
            .find(|&(profile, colour)| {
                let status = unsafe { AMediaCodec_configure(codec, encoder.format(profile, colour).0, std::ptr::null_mut(), std::ptr::null_mut(), CONFIGURE_ENCODE) };
                tried.push(status);
                status == OK
            });
        let Some((_, colour)) = configured else {
            bail!("the H.264 encoder won't take {}×{} ({tried:?})", shape.width, shape.height);
        };
        let input = Format(unsafe { AMediaCodec_getInputFormat(codec) });
        encoder.layout = Layout {
            format: input.get(c"color-format").filter(|f| [SEMI_PLANAR, PLANAR].contains(f)).unwrap_or(colour),
            stride: input.get(c"stride").map_or(shape.width, |s| (s as usize).max(shape.width)),
            rows: input.get(c"slice-height").map_or(shape.height, |s| (s as usize).max(shape.height)),
        };
        check(unsafe { AMediaCodec_start(codec) }, "starting the encoder")?;
        Ok(encoder)
    }

    fn format(&self, profile: i32, colour: i32) -> Format {
        let format = Format::new();
        unsafe { AMediaFormat_setString(format.0, c"mime".as_ptr(), c"video/avc".as_ptr()) };
        format
            .set(c"width", self.shape.width as i32)
            .set(c"height", self.shape.height as i32)
            .set(c"color-format", colour)
            .set(c"bitrate", self.bitrate as i32)
            .set(c"bitrate-mode", CBR)
            .set(c"frame-rate", self.shape.fps as i32)
            .set(c"i-frame-interval", 3600)
            .set(c"profile", profile)
            .set(c"max-bframes", 0)
            .set(c"prepend-sps-pps-to-idr-frames", 1)
            .set(c"priority", 0)
            .set(c"latency", 1);
        format
    }

    /// Puts `picture` (I420, this encoder's size) into an input buffer.
    unsafe fn give(&mut self, picture: &Picture, key: bool) -> Result<bool> {
        let index = AMediaCodec_dequeueInputBuffer(self.codec, WAIT_IN);
        if index < 0 {
            return Ok(false);
        }
        let index = index as usize;
        let mut size = 0;
        let buffer = AMediaCodec_getInputBuffer(self.codec, index, &mut size);
        if buffer.is_null() {
            bail!("no input buffer");
        }
        let buffer = std::slice::from_raw_parts_mut(buffer, size);
        let filled = fill(buffer, picture, &self.layout)?;
        if key {
            let sync = Format::new();
            sync.set(c"request-sync", 0);
            check(AMediaCodec_setParameters(self.codec, sync.0), "asking for a full frame")?;
        }
        let time = self.frames * 1_000_000 / self.shape.fps.max(1) as u64;
        self.frames += 1;
        check(AMediaCodec_queueInputBuffer(self.codec, index, 0, filled, time, 0), "giving the encoder a picture")?;
        Ok(true)
    }

    /// The frames the encoder has ready, waiting a little for the first
    /// if `wait`.
    unsafe fn take(&mut self, wait: bool) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut timeout = if wait { WAIT_OUT } else { 0 };
        loop {
            let mut info = BufferInfo { offset: 0, size: 0, time: 0, flags: 0 };
            let index = AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, timeout);
            if index == TRY_AGAIN_LATER {
                return Ok(out);
            }
            if index < 0 {
                continue;
            }
            let index = index as usize;
            let mut size = 0;
            let buffer = AMediaCodec_getOutputBuffer(self.codec, index, &mut size);
            let data = if buffer.is_null() || info.size <= 0 {
                &[][..]
            } else {
                let all = std::slice::from_raw_parts(buffer, size);
                &all[info.offset as usize..(info.offset + info.size) as usize]
            };
            if info.flags & FLAG_CODEC_CONFIG != 0 {
                self.config = data.to_vec();
            } else if !data.is_empty() {
                if info.flags & FLAG_KEY_FRAME != 0 && !has_sps(data) {
                    out.extend_from_slice(&self.config);
                }
                out.extend_from_slice(data);
                timeout = 0;
            }
            AMediaCodec_releaseOutputBuffer(self.codec, index, false);
        }
    }
}

impl HardEncoder for Encoder {
    fn encode(&mut self, picture: &Picture, key: bool) -> Result<Vec<u8>> {
        unsafe {
            let given = self.give(picture, key)?;
            self.take(given)
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        if bitrate != self.bitrate {
            let format = Format::new();
            format.set(c"video-bitrate", bitrate as i32);
            check(unsafe { AMediaCodec_setParameters(self.codec, format.0) }, "changing the bitrate")?;
            self.bitrate = bitrate;
        }
        Ok(())
    }

    fn name(&self) -> String {
        let mut name = std::ptr::null_mut();
        if unsafe { AMediaCodec_getName(self.codec, &mut name) } != OK || name.is_null() {
            return "the phone's H.264 encoder".to_string();
        }
        let named = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
        unsafe { AMediaCodec_releaseName(self.codec, name) };
        named
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            AMediaCodec_stop(self.codec);
            AMediaCodec_delete(self.codec);
        }
    }
}

fn check(status: Status, doing: &str) -> Result<()> {
    if status == OK {
        Ok(())
    } else {
        Err(anyhow!("{doing}: MediaCodec said {status}"))
    }
}

/// Whether an Annex-B stream has an SPS.
fn has_sps(stream: &[u8]) -> bool {
    stream.windows(4).any(|w| w[0..3] == [0, 0, 1] && w[3] & 0x1f == 7)
}

/// Lays an I420 `picture` out in `buffer` as `layout` says: NV12 or
/// I420, each row `stride` long, `rows` rows of brightness before the
/// colour. How much of the buffer it filled.
fn fill(buffer: &mut [u8], picture: &Picture, layout: &Layout) -> Result<usize> {
    let (width, height) = (picture.width, picture.height);
    let (stride, rows) = (layout.stride, layout.rows);
    let (half_width, half_height) = (width.div_ceil(2), height.div_ceil(2));
    let luma = stride * rows;
    let size = match layout.format {
        PLANAR => luma + 2 * (stride / 2) * (rows / 2),
        _ => luma + stride * (rows / 2),
    };
    let needs = match layout.format {
        PLANAR => luma + (stride / 2) * (rows / 2) + (stride / 2) * (half_height - 1) + half_width,
        _ => luma + stride * (half_height - 1) + 2 * half_width,
    };
    if buffer.len() < needs {
        bail!("the encoder's buffer is too small ({} for {needs})", buffer.len());
    }
    let size = size.min(buffer.len());
    let (y, u, v) = picture.planes();
    for row in 0..height {
        buffer[row * stride..row * stride + width].copy_from_slice(&y[row * width..(row + 1) * width]);
    }
    let colour = &mut buffer[luma..];
    match layout.format {
        PLANAR => {
            let half_stride = stride / 2;
            let (us, vs) = colour.split_at_mut(half_stride * (rows / 2));
            for row in 0..half_height {
                let from = row * half_width..(row + 1) * half_width;
                us[row * half_stride..row * half_stride + half_width].copy_from_slice(&u[from.clone()]);
                vs[row * half_stride..row * half_stride + half_width].copy_from_slice(&v[from]);
            }
        }
        _ => {
            for row in 0..half_height {
                let line = &mut colour[row * stride..row * stride + 2 * half_width];
                for (i, pair) in line.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    pair[0] = u[row * half_width + i];
                    pair[1] = v[row * half_width + i];
                }
            }
        }
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::video::Format as PictureFormat;

    fn picture(width: usize, height: usize) -> Picture {
        let mut data = vec![0; width * height * 3 / 2];
        for (i, v) in data.iter_mut().enumerate() {
            *v = (i % 251) as u8;
        }
        Picture { width, height, format: PictureFormat::I420, data }
    }

    #[test]
    fn pictures_laid_out_as_the_encoder_wants() {
        let picture = picture(4, 2);
        let (y, u, v) = picture.planes();
        let mut buffer = vec![9; 64];
        let filled = fill(&mut buffer, &picture, &Layout { format: SEMI_PLANAR, stride: 8, rows: 4 }).unwrap();
        assert_eq!(filled, 8 * 4 + 8 * 2);
        assert_eq!(&buffer[0..4], &y[0..4]);
        assert_eq!(&buffer[8..12], &y[4..8]);
        assert_eq!(&buffer[32..36], &[u[0], v[0], u[1], v[1]]);
        let mut buffer = vec![9; 64];
        let filled = fill(&mut buffer, &picture, &Layout { format: PLANAR, stride: 8, rows: 4 }).unwrap();
        assert_eq!(filled, 8 * 4 + 2 * 4 * 2);
        assert_eq!(&buffer[32..34], &u[0..2]);
        assert_eq!(&buffer[40..42], &v[0..2]);
        assert!(fill(&mut [0; 40], &picture, &Layout { format: PLANAR, stride: 8, rows: 4 }).is_err());
        let mut buffer = vec![9; 8 * 4 + 8 - 4];
        assert_eq!(fill(&mut buffer, &picture, &Layout { format: SEMI_PLANAR, stride: 8, rows: 4 }).unwrap(), buffer.len());
        assert_eq!(&buffer[32..36], &[u[0], v[0], u[1], v[1]]);
    }

    /// On the phone (scratchpad phone-test.sh): its encoder's frames
    /// decode in OpenH264 at its size, the first full, and how long each
    /// takes.
    /// Binder threads, as an app has from the start: without them, a
    /// test's MediaCodec never hears back from the phone's codec service.
    fn binder_threads() {
        extern "C" {
            fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
            fn dlsym(library: *mut c_void, name: *const c_char) -> *mut c_void;
        }
        unsafe {
            let binder = dlopen(c"libbinder_ndk.so".as_ptr(), 2);
            assert!(!binder.is_null());
            let most: extern "C" fn(u32) -> bool = std::mem::transmute(dlsym(binder, c"ABinderProcess_setThreadPoolMaxThreadCount".as_ptr()));
            let start: extern "C" fn() = std::mem::transmute(dlsym(binder, c"ABinderProcess_startThreadPool".as_ptr()));
            most(4);
            start();
        }
    }

    #[test]
    #[ignore]
    fn phone_encodes_what_openh264_decodes() {
        binder_threads();
        for (width, height) in [(1920, 1080), (1080, 1920), (1280, 720), (640, 360)] {
            let shape = Shape { width, height, fps: 30 };
            let mut encoder = Encoder::new(shape, 4_000_000).unwrap();
            let mut decoder = openh264::decoder::Decoder::new().unwrap();
            let mut decoded = 0;
            let started = std::time::Instant::now();
            for i in 0..60 {
                let frame = encoder.encode(&picture(width, height), i == 0).unwrap();
                if decoded == 0 && !frame.is_empty() {
                    assert!(swarm_core::video::has_idr(&frame), "first frame not full");
                }
                if let Ok(Some(yuv)) = decoder.decode(&frame) {
                    use openh264::formats::YUVSource;
                    assert_eq!(yuv.dimensions(), (width, height));
                    decoded += 1;
                }
            }
            println!("{} {width}×{height}: {decoded} of 60 decoded, {:.1} ms each", encoder.name(), started.elapsed().as_secs_f32() * 1000.0 / 60.0);
            assert!(decoded >= 55);
        }
    }
}
