//! Video for calls: H.264 (or H.265, where both ends' graphics chips
//! have it) pictures cut into pieces that fit the cover slots, and put
//! back together on the other side. Like audio, video
//! rides the peer's one connection in-band (see `call`), so with cover on
//! it's carried in the fixed slots and a video call looks the same on the
//! wire as an idle connection.
//!
//! How big the pictures are, and how many a second, follows the bitrate
//! there's room for (`Shape::for_bitrate`): 4K (3840×2160) at 25 a
//! second on a fast link, down to a postage stamp on a slow one. A computer that
//! can't encode its share in time sends smaller pictures instead of
//! falling behind (see `VideoEncoder::encode`).
//!
//! Video is never resent. A piece that can't go out in time is dropped,
//! a frame missing a piece is never shown, and the receiver then waits
//! for the next full ("key") frame rather than decoding pictures built on
//! one it never got — asking for one at once (`VideoDecoder::push`), at
//! most once a second, besides the sender's own every
//! `KEYFRAME_EVERY_SECS`.
//!
//! H.265 needs about three fifths of H.264's bitrate for as good a
//! picture, but there's no H.265 in software here as OpenH264 is for
//! H.264: it's used only where this computer's chip encodes it and the
//! other side's says it decodes it (`CallSignal::Decodes`), and Settings
//! hasn't turned it off (`set_h265`). Each frame says which it is, and
//! anything that fails goes back to H.264.
//!
//! Capture and drawing belong to the front end; this module only ever
//! sees and returns `Picture`s.

use crate::lock::Locked;
use anyhow::{anyhow, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType};
use openh264::formats::{RgbSliceU8, YUVBuffer, YUVSlices, YUVSource};
use openh264::OpenH264API;
use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A size and pace for sending video at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub width: usize,
    pub height: usize,
    /// Pictures a second.
    pub fps: u32,
}

/// The shapes video is sent in, biggest first, each with the least
/// bitrate it looks good at: fewer pixels and fewer pictures a second
/// rather than a smeared big picture.
const SHAPES: [(u32, Shape); 9] = [
    (10_000_000, Shape { width: 3840, height: 2160, fps: 25 }),
    (6_000_000, Shape { width: 2560, height: 1440, fps: 25 }),
    (3_500_000, Shape { width: 1920, height: 1080, fps: 25 }),
    (1_800_000, Shape { width: 1280, height: 720, fps: 25 }),
    (900_000, Shape { width: 960, height: 540, fps: 25 }),
    (400_000, Shape { width: 640, height: 360, fps: 25 }),
    (160_000, Shape { width: 480, height: 270, fps: 15 }),
    (100_000, Shape { width: 320, height: 180, fps: 10 }),
    (0, Shape { width: 192, height: 108, fps: 10 }),
];

/// The steps a shared screen is sent in (see `screen_shapes`): the least
/// bitrate each needs for a 1920×1080 screen, how many pictures a second,
/// and what part of the screen's size. A screen's own size is kept to the
/// last: its text has to be read, and any smaller smears it whatever the
/// encoder (25–28 dB against 40–44 at 1080p, at 160–500 kbit/s, of an
/// xterm of code). Fewer pictures a second instead, each as sharp: 25 at
/// the top, as a camera's, so what moves on it moves as smoothly.
const SCREEN_STEPS: [(u32, u32, usize); 9] = [
    (2_500_000, 25, 1),
    (1_500_000, 15, 1),
    (1_000_000, 10, 1),
    (600_000, 8, 1),
    (400_000, 5, 1),
    (250_000, 3, 1),
    (150_000, 2, 1),
    (60_000, 2, 2),
    (0, 2, 3),
];
/// A screen's steps from this many pictures a second up go to the
/// graphics chip, where there is one: given room it's far sharper than
/// OpenH264's screen mode (an xterm at 1080p, 25 a second, 2.5 Mbit/s:
/// NVENC's H.265 57.7 dB, H.264 56.6, OpenH264 44.2, which never spends
/// more than about 600 kbit/s however much it's given), but short of it
/// far worse (500 kbit/s, 5 a second: 38.5 and 34.9 against 43.8), and
/// it sharpens a still picture over the pictures after it, which at 8 or
/// 10 a second takes too long. Measured with `examples/screen_quality`.
const SCREEN_ON_CHIP_FROM_FPS: u32 = 15;
/// The widest a screen's sent in software: wider is too slow to encode.
const SCREEN_SOFT_MOST_WIDTH: usize = 1920;

/// Whether a screen at `shape` goes to the graphics chip, if there is one.
fn screen_on_chip(shape: Shape) -> bool {
    shape.fps >= SCREEN_ON_CHIP_FROM_FPS
}

/// The shapes a screen `width_in` wide (made 16:9, see the front end's
/// `screen`; 0, not known yet: 1920) is sent in, biggest first, drawn
/// `shown_long` across on the other side (0, not known): `SCREEN_STEPS`
/// at the screen's own size, or the smallest camera shape's that's no
/// smaller than it's drawn, if that's smaller — what isn't seen only
/// costs bits. On the chip (`chip`, there's one to use) as it is; in
/// software no wider than `SCREEN_SOFT_MOST_WIDTH`. Each step's least
/// bitrate is for its size: a 4K screen needs four times 1080p's.
fn screen_shapes(width_in: usize, shown_long: usize, chip: bool) -> Vec<(u32, Shape)> {
    let own = if width_in == 0 { 1920 } else { width_in };
    let full = match shown_long {
        0 => own,
        shown => SHAPES.iter().rev().map(|(_, shape)| shape.width).find(|w| *w >= shown).map_or(own, |w| w.min(own)),
    };
    let mut shapes: Vec<(u32, Shape)> = SCREEN_STEPS
        .iter()
        .map(|&(least, fps, part)| {
            let on_chip = chip && fps >= SCREEN_ON_CHIP_FROM_FPS;
            let width = if on_chip { full } else { full.min(SCREEN_SOFT_MOST_WIDTH) };
            let pixels = width as u64 * (width as u64 * 9 / 16);
            let least = (least as u64 * pixels / (1920 * 1080)) as u32;
            let width = ((width / part).max(width.min(320)) & !1).max(2);
            (least, Shape { width, height: ((width * 9 / 16) & !1).max(2), fps })
        })
        .collect();
    if let Some(last) = shapes.last_mut() {
        last.0 = 0;
    }
    shapes
}

/// `SHAPES` for H.265, which needs about three fifths the bitrate: the
/// same shapes, in the same order, those on the chip (from
/// `HARDWARE_FROM_WIDTH`) needing less.
const H265_SHAPES: [(u32, Shape); 9] = {
    let mut shapes = SHAPES;
    let mut i = 0;
    while i < shapes.len() {
        if shapes[i].1.width >= HARDWARE_FROM_WIDTH {
            shapes[i].0 = shapes[i].0 / 5 * 3;
        }
        i += 1;
    }
    shapes
};

/// A camera's shapes.
fn camera_shapes(h265: bool) -> &'static [(u32, Shape)] {
    if h265 {
        &H265_SHAPES
    } else {
        &SHAPES
    }
}

impl Shape {
    /// The biggest shape `bitrate` is enough for.
    pub fn for_bitrate(bitrate: u32) -> Shape {
        SHAPES[shape_index(&SHAPES, bitrate)].1
    }

    /// The biggest shape video is ever sent in.
    pub const LARGEST: Shape = SHAPES[0].1;

    /// Every shape a camera's pictures are sent in, biggest first.
    pub fn all() -> impl Iterator<Item = Shape> {
        SHAPES.iter().map(|(_, shape)| *shape)
    }

    /// Its longer side: how big it is, whichever way up.
    pub fn long(self) -> usize {
        self.width.max(self.height)
    }

    /// Turned on end, if `upright`.
    pub fn turned(self, upright: bool) -> Shape {
        if upright { Shape { width: self.height, height: self.width, fps: self.fps } } else { self }
    }
}

/// The most bitrate worth spending on `shape`: past this a picture hardly
/// looks better, but encoding (and decoding, at the other end) takes
/// longer — 720p at 5 Mbit/s took twice as long both ways as at 2.5, and
/// pushed a two-core i3 receiving it over the edge. A screen's fewer
/// pictures a second need less, and H.265 three fifths as much.
fn ceiling(shape: Shape, screen: bool, h265: bool) -> u32 {
    let camera = match shape.long() {
        w if w >= 3840 => 16_000_000,
        w if w >= 2560 => 9_000_000,
        w if w >= 1920 => 5_000_000,
        w if w >= 1280 => 2_500_000,
        w if w >= 960 => 1_400_000,
        w if w >= 640 => 800_000,
        w if w >= 480 => 400_000,
        w if w >= 320 => 200_000,
        _ => 120_000,
    };
    if screen {
        (camera as u64 * shape.fps.max(8) as u64 / MAX_FPS as u64) as u32
    } else if h265 && shape.long() >= HARDWARE_FROM_WIDTH {
        camera / 5 * 3
    } else {
        camera
    }
}

fn shape_index(shapes: &[(u32, Shape)], bitrate: u32) -> usize {
    shapes.iter().position(|(least, _)| bitrate >= *least).unwrap_or(shapes.len() - 1)
}

/// The bitrate pictures drawn `long` along their longer side are worth:
/// what the smallest shape that fills that takes, and a quarter more, so
/// it's taken (see `VideoEncoder::set_bitrate`) — what a strip's small
/// pictures are given, of a camera's or a screen's.
pub fn worth_drawn(long: usize, screen: bool) -> u32 {
    if screen {
        let shapes = screen_shapes(long, 0, false);
        let at = shapes.iter().position(|(_, shape)| shape.fps <= 5).unwrap_or(shapes.len() - 1);
        return (shapes[at].0.max(MIN_BITRATE) as u64 * 5 / 4) as u32;
    }
    let shapes = camera_shapes(false);
    let fills = shapes.iter().rposition(|(_, shape)| shape.width >= long).unwrap_or(0);
    (shapes[fills].0.max(MIN_BITRATE) as u64 * 5 / 4) as u32
}

/// The most pictures a second video is ever sent at.
pub const MAX_FPS: u32 = 25;
/// A full frame at least this often, whatever else happens. The receiver
/// asks for one as soon as it's lost anything, so this is only a backstop
/// — full frames are big, and on a slow link the voice behind one waits.
pub const KEYFRAME_EVERY_SECS: u32 = 20;
/// The lowest cover rate a video call is allowed at. Below this there
/// isn't room for a picture worth seeing alongside voice.
pub const MIN_COVER_FOR_VIDEO: u64 = 128_000;
/// The most a picture may come to, whole: the receiver keeps no more
/// for one, and the sender sends none bigger. The biggest measured, a
/// full frame of noise from a 4K camera, came to 5.3 MB; a real picture
/// is a small part of that.
pub const MAX_PICTURE_BYTES: usize = 8 << 20;
/// The most a receiver keeps, of every picture still being put together.
const MAX_ASSEMBLING_BYTES: usize = 2 * MAX_PICTURE_BYTES;
/// The largest piece on a connection without cover.
pub const UNCOVERED_PIECE: usize = 1000;
/// How long a piece may wait to be sent before it's dropped instead.
pub const PIECE_STALE_AFTER: Duration = Duration::from_millis(600);
/// The fewest bits a second worth running the encoder at.
pub const MIN_BITRATE: u32 = 16_000;
/// How long the receiver waits between asking for a full frame.
const KEYFRAME_REQUEST_EVERY: Duration = Duration::from_secs(1);
/// Encoding taking more than this share of the time between pictures
/// means this computer can't keep up at that size. Not much less: on a
/// two-core i3, encoding 640×360 averaged 12ms of the 40 — but with the
/// camera, decoding and drawing on the same cores, now and then over
/// half, and at a half it stepped down to 480×270 though keeping up.
const BUSY_SHARE: f32 = 0.8;
/// How long after a step down for being slow the bigger shapes are
/// tried again, the first time; then twice as long each time, up to
/// the most.
const TRY_AGAIN_AFTER: Duration = Duration::from_secs(30);
const TRY_AGAIN_AFTER_MOST: Duration = Duration::from_secs(480);
/// The most threads an encoder uses.
const ENCODER_THREADS: usize = 4;
/// About how big an encoded slice is: about a piece without cover.
const SLICE_BYTES: u32 = 1200;
/// The most the encoder's bitrate may be raised to in place.
const MOST_BITRATE: u32 = 20_000_000;
/// A new shape means a new encoder and a full frame, so it's only taken
/// once the bitrate's stayed there a while: this long for a smaller one
/// (at once if the rate's under half what the shape needs)...
const SMALLER_AFTER: Duration = Duration::from_secs(2);
/// ...and this long for a bigger one.
const BIGGER_AFTER: Duration = Duration::from_secs(4);

/// How a picture's pixels are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// 8 bits each of red, green and blue, pixel by pixel, row by row.
    Rgb,
    /// YUV 4:2:0, as H.264 carries it (BT.601, limited range): the whole
    /// Y plane, then U, then V, each at half the width and height
    /// (rounded up). A camera's pictures come this way where it can give
    /// them so, and go to the encoder without a round trip through RGB.
    I420,
}

/// A picture, in either `Format`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub format: Format,
    pub data: Vec<u8>,
}

/// The size of a chroma plane for a picture `width`×`height`.
fn chroma_size(width: usize, height: usize) -> (usize, usize) {
    (width.div_ceil(2), height.div_ceil(2))
}

/// One pixel from Y, U and V (BT.601, limited range), as openh264's own
/// conversion does it, in fixed point.
#[inline]
pub fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let y = (y as i32 - 16) * 76_309;
    let (u, v) = (u as i32 - 128, v as i32 - 128);
    let r = y + 104_597 * v;
    let g = y - 25_675 * u - 53_279 * v;
    let b = y + 132_201 * u;
    [r, g, b].map(|c| ((c + 32_768) >> 16).clamp(0, 255) as u8)
}

impl Picture {
    pub fn rgb(width: usize, height: usize, rgb: Vec<u8>) -> Picture {
        debug_assert_eq!(rgb.len(), width * height * 3);
        Picture { width, height, format: Format::Rgb, data: rgb }
    }

    /// From its three planes, back to back (see `Format::I420`).
    pub fn i420(width: usize, height: usize, data: Vec<u8>) -> Picture {
        let (cw, ch) = chroma_size(width, height);
        debug_assert_eq!(data.len(), width * height + 2 * cw * ch);
        Picture { width, height, format: Format::I420, data }
    }

    /// The Y, U and V planes of an I420 picture.
    pub fn planes(&self) -> (&[u8], &[u8], &[u8]) {
        debug_assert_eq!(self.format, Format::I420);
        let (cw, ch) = chroma_size(self.width, self.height);
        let (y, uv) = self.data.split_at(self.width * self.height);
        let (u, v) = uv.split_at(cw * ch);
        (y, u, &v[..cw * ch])
    }

    pub fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        match self.format {
            Format::Rgb => {
                let i = (y * self.width + x) * 3;
                [self.data[i], self.data[i + 1], self.data[i + 2]]
            }
            Format::I420 => {
                let (luma, u, v) = self.planes();
                let c = (y / 2) * chroma_size(self.width, self.height).0 + x / 2;
                yuv_to_rgb(luma[y * self.width + x], u[c], v[c])
            }
        }
    }

    /// This picture as RGB: itself, or converted.
    pub fn to_rgb(&self) -> Cow<'_, Picture> {
        if self.format == Format::Rgb {
            return Cow::Borrowed(self);
        }
        let mut rgb = vec![0u8; self.width * self.height * 3];
        self.rgb_into(&mut rgb, false);
        Cow::Owned(Picture::rgb(self.width, self.height, rgb))
    }

    /// This picture as RGB into `out` (`width`×`height`×3 bytes), each
    /// row left for right if `mirror` — for a window's buffer, with no
    /// picture between and no pass of its own to mirror it (a 4K camera
    /// picture's took 20 ms).
    pub fn rgb_into(&self, out: &mut [u8], mirror: bool) {
        self.pixels_into::<3>(out, mirror, 1);
    }

    /// `rgb_into`, as RGBA (each pixel opaque): for a window that takes
    /// RGBA as it is, as Android's does (RGB, it turns into RGBA a byte at
    /// a time).
    pub fn rgba_into(&self, out: &mut [u8], mirror: bool) {
        self.pixels_into::<4>(out, mirror, 1);
    }

    /// The size of this picture with only every `step`th pixel each way
    /// (see `rgb_every_into`).
    pub fn every(&self, step: usize) -> (usize, usize) {
        (self.width.div_ceil(step), self.height.div_ceil(step))
    }

    /// `rgb_into`, of every `step`th pixel each way, the first included
    /// (into `every(step)`'s size): for a window that shows it smaller,
    /// a `step`² of the work, and of what the window copies.
    pub fn rgb_every_into(&self, out: &mut [u8], mirror: bool, step: usize) {
        self.pixels_into::<3>(out, mirror, step);
    }

    /// `rgb_every_into`, as RGBA (see `rgba_into`).
    pub fn rgba_every_into(&self, out: &mut [u8], mirror: bool, step: usize) {
        self.pixels_into::<4>(out, mirror, step);
    }

    /// `rgb_every_into` (`N` 3) or `rgba_every_into` (4).
    fn pixels_into<const N: usize>(&self, out: &mut [u8], mirror: bool, step: usize) {
        let (w, h) = self.every(step);
        if self.format == Format::Rgb {
            for (to, row) in out.chunks_exact_mut(w * N).zip(self.data.chunks_exact(self.width * 3).step_by(step)) {
                let from = row.as_chunks::<3>().0.iter().step_by(step);
                if N == 3 && !mirror && step == 1 {
                    to.copy_from_slice(row);
                } else if mirror {
                    for (to, from) in to.as_chunks_mut::<N>().0.iter_mut().rev().zip(from) {
                        to[..3].copy_from_slice(from);
                        to[3..].fill(255);
                    }
                } else {
                    for (to, from) in to.as_chunks_mut::<N>().0.iter_mut().zip(from) {
                        to[..3].copy_from_slice(from);
                        to[3..].fill(255);
                    }
                }
            }
            return;
        }
        let bands = if w * h >= 1 << 20 { std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) } else { 1 };
        let rows = h.div_ceil(bands).next_multiple_of(2);
        std::thread::scope(|scope| {
            for (i, band) in out[..w * h * N].chunks_mut(rows * w * N).enumerate() {
                if i + 1 == bands.min(h.div_ceil(rows)) {
                    self.rgb_rows::<N>(band, i * rows, mirror, step);
                } else {
                    scope.spawn(move || self.rgb_rows::<N>(band, i * rows, mirror, step));
                }
            }
        });
    }

    /// `pixels_into` for the rows from `first` that `out` holds.
    fn rgb_rows<const N: usize>(&self, out: &mut [u8], first: usize, mirror: bool, step: usize) {
        let (w, sw) = (self.every(step).0, self.width);
        let cw = chroma_size(self.width, self.height).0;
        let (luma, u, v) = self.planes();
        let (mut wide_u, mut wide_v) = (vec![0i16; w + 1], vec![0i16; w + 1]);
        let (mut r, mut g, mut b) = (vec![0u8; w], vec![0u8; w], vec![0u8; w]);
        let mut picked = vec![0u8; if step > 1 { w } else { 0 }];
        let mut chroma_row = usize::MAX;
        for (y, row) in out.chunks_exact_mut(w * N).enumerate().map(|(i, row)| ((first + i) * step, row)) {
            if y / 2 != chroma_row {
                chroma_row = y / 2;
                let (u, v) = (&u[chroma_row * cw..][..cw], &v[chroma_row * cw..][..cw]);
                if step == 1 {
                    for ((wu, wv), (&u, &v)) in wide_u.as_chunks_mut::<2>().0.iter_mut().zip(wide_v.as_chunks_mut::<2>().0).zip(u.iter().zip(v)) {
                        wu.fill(u as i16 - 128);
                        wv.fill(v as i16 - 128);
                    }
                } else {
                    for (x, (wu, wv)) in wide_u.iter_mut().zip(wide_v.iter_mut()).take(w).enumerate() {
                        (*wu, *wv) = (u[x * step / 2] as i16 - 128, v[x * step / 2] as i16 - 128);
                    }
                }
            }
            let luma = &luma[y * sw..(y + 1) * sw];
            let luma = if step == 1 {
                luma
            } else {
                for (to, &l) in picked.iter_mut().zip(luma.iter().step_by(step)) {
                    *to = l;
                }
                &picked
            };
            for ((((&l, (&cu, &cv)), r), g), b) in luma.iter().zip(wide_u.iter().zip(&wide_v)).zip(r.iter_mut()).zip(g.iter_mut()).zip(b.iter_mut()) {
                let l = (l as i16 - 16) * 75 + 32;
                *r = (l.saturating_add(102 * cv) >> 6).clamp(0, 255) as u8;
                *g = (l.saturating_add(-25 * cu - 52 * cv) >> 6).clamp(0, 255) as u8;
                *b = (l.saturating_add(129 * cu) >> 6).clamp(0, 255) as u8;
            }
            if mirror {
                r.reverse();
                g.reverse();
                b.reverse();
            }
            for (((px, &r), &g), &b) in row.as_chunks_mut::<N>().0.iter_mut().zip(&r).zip(&g).zip(&b) {
                px[0] = r;
                px[1] = g;
                px[2] = b;
                px[3..].fill(255);
            }
        }
    }

    /// This picture as I420: itself, or converted (by openh264, which
    /// does it with vector instructions). An odd width or height is
    /// rounded down by a pixel.
    pub fn to_i420(&self) -> Cow<'_, Picture> {
        if self.format == Format::I420 {
            return Cow::Borrowed(self);
        }
        let (w, h) = (self.width & !1, self.height & !1);
        let even;
        let source = if (w, h) == (self.width, self.height) {
            self
        } else {
            even = self.scaled(w, h);
            &even
        };
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&source.data, (w, h)));
        let mut data = Vec::with_capacity(w * h * 3 / 2);
        data.extend_from_slice(yuv.y());
        data.extend_from_slice(yuv.u());
        data.extend_from_slice(yuv.v());
        Cow::Owned(Picture::i420(w, h, data))
    }

    /// This picture at `width`×`height`, each new pixel the average of
    /// the ones it covers, cropped in the middle first if its shape
    /// differs (nothing is stretched). Kept in its format; an I420
    /// picture's new size is rounded up to even.
    pub fn scaled(&self, width: usize, height: usize) -> Picture {
        self.scaled_cow(width, height).into_owned()
    }

    /// `scaled`, without a copy when it's the right size already.
    pub fn scaled_cow(&self, width: usize, height: usize) -> Cow<'_, Picture> {
        let (width, height) = match self.format {
            Format::Rgb => (width, height),
            Format::I420 => (width.next_multiple_of(2), height.next_multiple_of(2)),
        };
        if self.width == width && self.height == height {
            return Cow::Borrowed(self);
        }
        let (src_w, src_h) = (self.width.max(1), self.height.max(1));
        let (crop_w, crop_h) = if src_w * height > src_h * width { (src_h * width / height, src_h) } else { (src_w, src_w * height / width) };
        let (x_off, y_off) = ((src_w - crop_w) / 2, (src_h - crop_h) / 2);
        Cow::Owned(match self.format {
            Format::Rgb => {
                let mut rgb = Vec::with_capacity(width * height * 3);
                scale_plane::<3>(&self.data, self.width, self.height, (x_off, y_off, crop_w, crop_h), width, height, &mut rgb);
                Picture::rgb(width, height, rgb)
            }
            Format::I420 => {
                let (luma, u, v) = self.planes();
                let (cw, ch) = chroma_size(self.width, self.height);
                let mut data = Vec::with_capacity(width * height * 3 / 2);
                scale_plane::<1>(luma, self.width, self.height, (x_off, y_off, crop_w, crop_h), width, height, &mut data);
                let crop = (x_off / 2, y_off / 2, crop_w.div_ceil(2).min(cw), crop_h.div_ceil(2).min(ch));
                scale_plane::<1>(u, cw, ch, crop, width / 2, height / 2, &mut data);
                scale_plane::<1>(v, cw, ch, crop, width / 2, height / 2, &mut data);
                Picture::i420(width, height, data)
            }
        })
    }
}

/// Scales the `crop` (x, y, width, height) of a plane of `C` bytes a
/// pixel to `width`×`height`, each new pixel the average of the ones it
/// covers, onto the end of `out`. Every picture sent goes through here,
/// up to 25 times a second.
fn scale_plane<const C: usize>(src: &[u8], src_w: usize, src_h: usize, crop: (usize, usize, usize, usize), width: usize, height: usize, out: &mut Vec<u8>) {
    let (x_off, y_off, crop_w, crop_h) = crop;
    if width == 0 || height == 0 {
        return;
    }
    let columns: Vec<(usize, usize)> = (0..width)
        .map(|tx| {
            let x0 = x_off + tx * crop_w / width;
            (x0.min(src_w), (x_off + ((tx + 1) * crop_w / width).max(tx * crop_w / width + 1)).min(src_w))
        })
        .collect();
    let mut rows_sum = vec![0u32; src_w * C];
    let mut reciprocals: Vec<Option<Vec<(u64, u32)>>> = Vec::new();
    for ty in 0..height {
        let (y0, y1) = (y_off + ty * crop_h / height, (y_off + ((ty + 1) * crop_h / height).max(ty * crop_h / height + 1)).min(src_h));
        rows_sum.fill(0);
        for y in y0..y1 {
            for (sum, &byte) in rows_sum.iter_mut().zip(&src[y * src_w * C..(y + 1) * src_w * C]) {
                *sum += byte as u32;
            }
        }
        let rows = y1.saturating_sub(y0);
        if reciprocals.len() <= rows {
            reciprocals.resize(rows + 1, None);
        }
        let reciprocals = reciprocals[rows].get_or_insert_with(|| {
            columns
                .iter()
                .map(|&(x0, x1)| {
                    let n = (rows as u64 * (x1 - x0) as u64).max(1);
                    (if n < 4096 { (1u64 << 32) / n + 1 } else { 0 }, n as u32)
                })
                .collect()
        });
        let at = out.len();
        out.resize(at + width * C, 0);
        for ((dest, &(x0, x1)), &(r, n)) in out[at..].as_chunks_mut::<C>().0.iter_mut().zip(&columns).zip(reciprocals.iter()) {
            let mut sum = [0u32; C];
            for px in rows_sum[x0 * C..x1 * C].as_chunks::<C>().0 {
                for c in 0..C {
                    sum[c] += px[c];
                }
            }
            for c in 0..C {
                dest[c] = if r != 0 { ((sum[c] as u64 * r) >> 32) as u8 } else { (sum[c] / n) as u8 };
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// How many pictures this thread's made for encoders (tests).
    static SIZINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A picture to encode: one of the encoder's own, or one other links are
/// sending too (see `VideoEncoder::encode_shared`).
#[derive(Clone, Copy)]
enum Source<'a> {
    Own(&'a Picture),
    Shared(&'a Arc<Picture>),
}

impl Source<'_> {
    fn picture(&self) -> &Picture {
        match self {
            Source::Own(picture) => picture,
            Source::Shared(picture) => picture,
        }
    }
}

/// Pictures made for encoders lately: each picture it was made of, its
/// size, and what was made. In a call of three or more, every link sends
/// the same pictures, each encoding them for itself, and those at the same
/// size share what's made for them (see `sized_for_encoder`).
type Made = Vec<(Arc<Picture>, usize, usize, Arc<Picture>)>;
static MADE: Mutex<Made> = Mutex::new(Vec::new());

/// `picture` made for an encoder at `width`×`height` (see `for_encoder`),
/// made once for every link sending it at that size: a 4K picture scaled
/// to 1080p takes 7.4 ms. What's kept goes once nothing else holds the
/// picture it was made of (each link's moved on to the next).
pub fn sized_for_encoder(picture: &Arc<Picture>, width: usize, height: usize) -> Arc<Picture> {
    let mut made = MADE.locked();
    let held_here = |made: &Made, picture: &Arc<Picture>| made.iter().map(|(of, _, _, sized)| Arc::ptr_eq(of, picture) as usize + Arc::ptr_eq(sized, picture) as usize).sum::<usize>();
    let gone: Vec<Arc<Picture>> = made.iter().map(|(of, ..)| of.clone()).filter(|of| Arc::strong_count(of) - 1 <= held_here(&made, of)).collect();
    made.retain(|(of, ..)| !gone.iter().any(|g| Arc::ptr_eq(g, of)));
    drop(gone);
    if let Some((.., sized)) = made.iter().find(|(of, w, h, _)| Arc::ptr_eq(of, picture) && (*w, *h) == (width, height)) {
        return sized.clone();
    }
    #[cfg(test)]
    SIZINGS.with(|n| n.set(n.get() + 1));
    let sized = match for_encoder(picture, width, height) {
        Cow::Borrowed(_) => picture.clone(),
        Cow::Owned(sized) => Arc::new(sized),
    };
    made.push((picture.clone(), width, height, sized.clone()));
    sized
}

/// `picture` at `width`×`height` (both even), as I420, for an encoder.
/// An RGB picture that needs scaling is converted first, and scaled as
/// I420: half the bytes (720p to 540p, 1.9 ms against 2.5).
fn for_encoder(picture: &Picture, width: usize, height: usize) -> Cow<'_, Picture> {
    let even = picture.width.is_multiple_of(2) && picture.height.is_multiple_of(2);
    if picture.format == Format::Rgb && even && (picture.width, picture.height) != (width, height) {
        return Cow::Owned(picture.to_i420().scaled(width, height));
    }
    match picture.scaled_cow(width, height) {
        Cow::Borrowed(picture) => picture.to_i420(),
        Cow::Owned(picture) => Cow::Owned(picture.to_i420().into_owned()),
    }
}

/// Which video coding a frame's in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265",
        }
    }
}

/// An H.264 or H.265 encoder on the graphics chip (see `Hardware`), made
/// for one size and pace.
pub trait HardEncoder: Send {
    /// Encodes `picture` (I420, at the size this encoder was made for),
    /// as a full frame if `key`: an Annex-B stream, empty when nothing
    /// came out this time.
    fn encode(&mut self, picture: &Picture, key: bool) -> Result<Vec<u8>>;
    /// Moves to `bitrate` in place, with no full frame.
    fn set_bitrate(&mut self, bitrate: u32) -> Result<()>;
    /// What it is ("Intel® Quick Sync Video H.264 Encoder MFT", say).
    fn name(&self) -> String;
}

/// An H.264 or H.265 decoder on the graphics chip.
pub trait HardDecoder: Send {
    /// Decodes one frame (Annex-B): a picture, when one comes out.
    fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>>;
    fn name(&self) -> String;
}

/// This computer's video hardware: provided by the front end where the
/// system has a way to it (Media Foundation on Windows), and used, when
/// turned on (`set_hardware`), in place of OpenH264. Anything it can't
/// do, or that fails, falls back to OpenH264 — mid-call too, with a full
/// frame — so a call never notices but for the time it takes.
pub trait Hardware: Send + Sync {
    fn encoder(&self, shape: Shape, bitrate: u32, codec: Codec) -> Result<Box<dyn HardEncoder>>;
    fn decoder(&self, codec: Codec) -> Result<Box<dyn HardDecoder>>;
    /// Whether it encodes H.265, and whether it decodes it (found out
    /// once, and kept: it's asked often).
    fn h265(&self) -> (bool, bool) {
        (false, false)
    }
    /// The way to it ("Media Foundation", say), for `video-check`.
    fn name(&self) -> String {
        "the graphics chip".to_string()
    }
}

static HARDWARE: OnceLock<Box<dyn Hardware>> = OnceLock::new();
static HARDWARE_ON: AtomicBool = AtomicBool::new(true);
/// The hardware failed to encode mid-call: not tried again until it's
/// turned off and on again (or swarm restarts) — a new encoder comes with
/// every new shape, and each failing again would cost a full frame. (A
/// decoder comes once a call, so one failing costs only that call its
/// hardware: what it failed at may have been that call's alone — a
/// picture too small for the chip, say.)
static ENCODER_FAILED: AtomicBool = AtomicBool::new(false);
/// Settings hasn't turned H.265 off ("H.264 only").
static H265_ON: AtomicBool = AtomicBool::new(true);
/// The hardware's H.265 encoder failed: H.264 from then on, as for
/// `ENCODER_FAILED`.
static H265_ENCODER_FAILED: AtomicBool = AtomicBool::new(false);
/// What last went wrong with it, for `video-check` and the like.
static HARDWARE_TROUBLE: Mutex<Option<String>> = Mutex::new(None);
/// Smaller shapes than this are always OpenH264's: they're quick in
/// software, and some hardware won't go that small.
const HARDWARE_FROM_WIDTH: usize = 640;

/// Gives video its hardware (once; later calls are ignored).
pub fn provide_hardware(hardware: Box<dyn Hardware>) {
    let _ = HARDWARE.set(hardware);
}

/// Turns the hardware's use on or off: for encoders and decoders made
/// from now on (and an encoder on hardware stops using it at once).
pub fn set_hardware(on: bool) {
    if on && !HARDWARE_ON.swap(true, Ordering::Relaxed) {
        ENCODER_FAILED.store(false, Ordering::Relaxed);
    }
    HARDWARE_ON.store(on, Ordering::Relaxed);
}

/// The width a camera's pictures are always sent at (Settings' "Video
/// size"), 0 for whatever the link carries.
static VIDEO_WIDTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Sends a camera's pictures at one size from the start (`Some` width:
/// one of the camera shapes'), or (`None`) at the biggest the link
/// carries, stepping up as it's found to carry more — each step a new
/// encoder, whose full first frame the other side waits for, so its
/// picture froze a moment at each. From the next picture.
pub fn set_video_width(width: Option<usize>) {
    VIDEO_WIDTH.store(width.unwrap_or(0), Ordering::Relaxed);
}

/// See `set_video_width`.
pub fn video_width() -> Option<usize> {
    #[cfg(test)]
    if let Some(width) = tests::TEST_VIDEO_WIDTH.with(|w| w.get()) {
        return Some(width).filter(|w| *w > 0);
    }
    Some(VIDEO_WIDTH.load(Ordering::Relaxed)).filter(|w| *w > 0)
}

/// Lets H.265 be used where both ends' chips have it (Settings'
/// "Automatic"), or not ("H.264 only"): for encoders and decoders from
/// their next frame.
pub fn set_h265(on: bool) {
    if on && !H265_ON.swap(true, Ordering::Relaxed) {
        H265_ENCODER_FAILED.store(false, Ordering::Relaxed);
    }
    H265_ON.store(on, Ordering::Relaxed);
}

/// Whether this computer decodes H.265 on its chip, and may (see
/// `set_h265`): what a call tells the other side.
pub fn decodes_h265() -> bool {
    H265_ON.load(Ordering::Relaxed) && hardware(false).is_some_and(|h| h.h265().1)
}

/// Whether this computer encodes H.265 on its chip, and may.
pub fn encodes_h265() -> bool {
    H265_ON.load(Ordering::Relaxed) && !H265_ENCODER_FAILED.load(Ordering::Relaxed) && hardware(ENCODER_FAILED.load(Ordering::Relaxed)).is_some_and(|h| h.h265().0)
}

/// Whether there's hardware to use at all, turned on or not.
pub fn hardware_provided() -> bool {
    HARDWARE.get().is_some()
}

/// The way to the hardware provided, if any (see `Hardware::name`).
pub fn hardware_name() -> Option<String> {
    HARDWARE.get().map(|h| h.name())
}

/// What last went wrong with the hardware, if anything has.
pub fn hardware_trouble() -> Option<String> {
    HARDWARE_TROUBLE.lock().ok()?.clone()
}

fn note_trouble(what: String) {
    if let Ok(mut trouble) = HARDWARE_TROUBLE.lock() {
        *trouble = Some(what);
    }
}

/// Notes trouble with hardware used outside this module (a camera's JPEG
/// decoder, say), for `hardware_trouble`.
pub fn note_hardware_trouble(what: String) {
    note_trouble(what);
}

/// The hardware, if there is some, it's turned on, and (`failed`) it
/// hasn't failed.
fn hardware(failed: bool) -> Option<&'static dyn Hardware> {
    #[cfg(test)]
    if let Some(hardware) = tests::TEST_HARDWARE.with(|h| *h.borrow()) {
        return Some(hardware);
    }
    (HARDWARE_ON.load(Ordering::Relaxed) && !failed).then(|| HARDWARE.get().map(|h| h.as_ref())).flatten()
}

/// Whether an Annex-B stream holds a full (IDR) frame.
pub fn has_idr(stream: &[u8]) -> bool {
    stream.windows(4).any(|w| w[0..3] == [0, 0, 1] && w[3] & 0x1f == 5)
}

/// Whether an Annex-B stream in `codec` holds a full frame: for H.265,
/// a picture decodable on its own (an IRAP: IDR, CRA or BLA).
pub fn is_key(stream: &[u8], codec: Codec) -> bool {
    match codec {
        Codec::H264 => has_idr(stream),
        Codec::H265 => stream.windows(4).any(|w| w[0..3] == [0, 0, 1] && (16..=21).contains(&(w[3] >> 1 & 0x3f))),
    }
}

/// What does the encoding: OpenH264, or the graphics chip.
enum Engine {
    Soft(Box<Encoder>),
    Hard {
        encoder: Box<dyn HardEncoder>,
        codec: Codec,
        /// Frames since the last full one.
        since_key: u32,
        /// A full frame's wanted (asked for, or the hardware hasn't made
        /// the one it was asked for yet).
        key_next: bool,
        /// Its first frame's been shown to decode (see `decodes`).
        proven: bool,
    },
}

/// One piece of one encoded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoPiece {
    /// Counts up by one per frame sent.
    pub frame: u32,
    pub index: u16,
    pub count: u16,
    /// A full frame — decodable on its own.
    pub key: bool,
    /// The frame's H.265, not H.264.
    pub h265: bool,
    /// Of the small pictures, for a strip's tile, not the large ones (see
    /// `CallSignal::ShownIn`): each its own stream, with its own frames.
    pub small: bool,
    pub data: Vec<u8>,
}

/// The bytes a piece adds on the wire besides its data: the frame's tag,
/// then `frame`, `index`, `count`, and `key`, `h265` and `small` in a byte (see
/// `protocol`).
pub const PIECE_OVERHEAD: usize = 1 + 4 + 2 + 2 + 1;

/// When the shapes stepped down from are tried again (see
/// `VideoEncoder::step_down`).
#[derive(Clone, Copy)]
enum Retry {
    After(Duration),
    At(Instant),
}

/// Encodes pictures into pieces at a bitrate that can change, in the
/// shape that bitrate suits.
pub struct VideoEncoder {
    encoder: Engine,
    bitrate: u32,
    shape: usize,
    /// The encoder has started (it does on its first picture), so its
    /// bitrate can be changed in place.
    started: bool,
    /// The biggest shape this computer has kept up with, and whose full
    /// frames fit in a frame's most pieces: never above one it's been too
    /// slow at, or too big at — till `retry`.
    fastest: usize,
    /// When a shape stepped down from is tried again (see `step_down`):
    /// how long after, till the next bitrate's set, then when; and how
    /// long the next step down waits, twice as long each time.
    retry: Option<Retry>,
    retry_wait: Duration,
    /// How wide the pictures coming in are (see `fit_picture`; 0 till
    /// said).
    width_in: usize,
    /// How long the other side draws our pictures' longer side (see
    /// `fit_shown`; 0 till said).
    shown_long: usize,
    /// Since when the bitrate's suited another shape, and which.
    other_shape: Option<(usize, Instant)>,
    /// How long encoding a picture takes, on average, in seconds, and
    /// over how many — ordinary ones, since this encoder started.
    busy: Option<f32>,
    judged: u32,
    /// How long a picture takes, on average over about a second, every
    /// picture counted — for showing (`encode_time`).
    average: Option<f32>,
    /// Whether to step down when encoding's too slow (not in tests that
    /// look at sizes, which a slow debug build would change).
    timed: bool,
    next_frame: u32,
    /// Encoding a shared screen, not a camera (see `set_screen`)...
    screen: bool,
    /// ...in these shapes (see `screen_shapes`; empty for a camera).
    screen_shapes: Vec<(u32, Shape)>,
    /// The pictures are taller than wide (a phone held upright): sent in
    /// the shape turned on end (see `sent`).
    upright: bool,
    /// The other side decodes H.265 (see `set_h265`)...
    h265_allowed: bool,
    /// ...and so it's sent, this computer encoding it: the shapes'
    /// bitrates are H.265's.
    h265: bool,
}

/// An encoder for `shape` at `bitrate`, or at the shape's `ceiling`: on
/// the hardware where there is some, and it's turned on — H.265 if
/// `h265` — but for a screen only at the steps given room enough
/// (`screen_on_chip`): below them, OpenH264's screen-content mode keeps
/// its text sharpest.
fn new_encoder(bitrate: u32, shape: Shape, screen: bool, h265: bool) -> Result<Engine> {
    if (!screen || screen_on_chip(shape))
        && shape.long() >= HARDWARE_FROM_WIDTH
        && let Some(hardware) = hardware(ENCODER_FAILED.load(Ordering::Relaxed))
    {
        if h265 {
            match hardware.encoder(shape, bitrate.min(ceiling(shape, screen, true)), Codec::H265) {
                Ok(encoder) => return Ok(Engine::Hard { encoder, codec: Codec::H265, since_key: 0, key_next: true, proven: false }),
                Err(e) => {
                    note_trouble(format!("no hardware H.265 encoder for {}×{}: {e:#}", shape.width, shape.height));
                    H265_ENCODER_FAILED.store(true, Ordering::Relaxed);
                }
            }
        }
        match hardware.encoder(shape, bitrate.min(ceiling(shape, screen, false)), Codec::H264) {
            Ok(encoder) => return Ok(Engine::Hard { encoder, codec: Codec::H264, since_key: 0, key_next: true, proven: false }),
            Err(e) => note_trouble(format!("no hardware encoder for {}×{}: {e:#}", shape.width, shape.height)),
        }
    }
    Ok(Engine::Soft(Box::new(soft_encoder(bitrate, shape, screen)?)))
}

/// OpenH264, for `shape` at `bitrate`, or at the shape's `ceiling` — for
/// a screen, in its screen-content mode.
fn soft_encoder(bitrate: u32, shape: Shape, screen: bool) -> Result<Encoder> {
    let bitrate = bitrate.min(ceiling(shape, screen, false));
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(ENCODER_THREADS) as u16;
    let config = EncoderConfig::new()
        .num_threads(threads)
        .max_slice_len(SLICE_BYTES)
        .complexity(Complexity::Low)
        .usage_type(if screen { UsageType::ScreenContentRealTime } else { UsageType::CameraVideoRealTime })
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(bitrate))
        .max_frame_rate(FrameRate::from_hz(shape.fps as f32))
        .skip_frames(true)
        .intra_frame_period(IntraFramePeriod::from_num_frames(shape.fps * KEYFRAME_EVERY_SECS));
    Encoder::with_api_config(OpenH264API::from_source(), config).map_err(|e| anyhow!("starting the video encoder: {e}"))
}

/// Sets one of the encoder's bitrates (`ENCODER_OPTION_BITRATE` or
/// `ENCODER_OPTION_MAX_BITRATE`) for `layer` — the way WebRTC changes
/// OpenH264's rate mid-call, with no full frame. Only once the encoder
/// has started.
fn set_encoder_bitrate(encoder: &mut Encoder, option: openh264_sys2::ENCODER_OPTION, layer: openh264_sys2::LAYER_NUM, bitrate: u32) -> Result<()> {
    let mut info = openh264_sys2::SBitrateInfo { iLayer: layer, iBitrate: bitrate.min(MOST_BITRATE) as i32 };
    let code = unsafe { encoder.raw_api().set_option(option, (&raw mut info).cast()) };
    if code != 0 {
        return Err(anyhow!("changing the video bitrate: error {code}"));
    }
    Ok(())
}

impl VideoEncoder {
    pub fn new(bitrate: u32) -> Result<Self> {
        let bitrate = bitrate.max(MIN_BITRATE);
        let shape = shape_index(&SHAPES, bitrate);
        Ok(VideoEncoder {
            encoder: new_encoder(bitrate, SHAPES[shape].1, false, false)?,
            bitrate,
            shape,
            started: false,
            fastest: 0,
            retry: None,
            retry_wait: TRY_AGAIN_AFTER,
            width_in: 0,
            shown_long: 0,
            other_shape: None,
            busy: None,
            judged: 0,
            average: None,
            timed: true,
            next_frame: 0,
            screen: false,
            screen_shapes: Vec::new(),
            upright: false,
            h265_allowed: false,
            h265: false,
        })
    }

    fn shapes(&self) -> &[(u32, Shape)] {
        if self.screen { &self.screen_shapes } else { camera_shapes(self.h265) }
    }

    /// A screen's shapes for what's known now (see `screen_shapes`): when
    /// they change (the screen's size, how big it's drawn, the chip lost
    /// or turned off), the shape's taken from the bitrate afresh, and a
    /// new one means a new encoder.
    fn follow_screen(&mut self) -> Result<()> {
        if !self.screen {
            return Ok(());
        }
        let shapes = screen_shapes(self.width_in, self.shown_long, hardware(ENCODER_FAILED.load(Ordering::Relaxed)).is_some());
        if shapes == self.screen_shapes {
            return Ok(());
        }
        let was = self.shape();
        self.screen_shapes = shapes;
        self.fastest = 0;
        self.retry = None;
        self.other_shape = None;
        self.shape = shape_index(&self.screen_shapes, self.bitrate);
        if self.shape() != was {
            self.encoder = new_encoder(self.bitrate, self.sent(self.shape()), true, self.h265)?;
            self.started = false;
            self.busy = None;
            self.judged = 0;
        }
        Ok(())
    }

    /// `shape` as it's sent: on end, for upright pictures.
    fn sent(&self, shape: Shape) -> Shape {
        shape.turned(self.upright)
    }

    /// Whether the other side decodes H.265 (it says so: see
    /// `CallSignal::Decodes`). It's sent when it does, this computer's
    /// chip encodes it, and Settings allows it (a screen's, only at the
    /// steps it's on the chip) — starting with the next picture, a full
    /// frame.
    pub fn set_h265(&mut self, allowed: bool) {
        self.h265_allowed = allowed;
    }

    /// Takes up a change in whether H.265's sent: a new encoder, its
    /// first frame a full one. The shapes are the same either way, only
    /// their bitrates differ, so the shape stays.
    fn follow_h265(&mut self) -> Result<()> {
        let h265 = self.h265_allowed && encodes_h265();
        if h265 != self.h265 {
            self.h265 = h265;
            self.encoder = new_encoder(self.bitrate, self.sent(self.shape()), self.screen, h265)?;
            self.started = false;
            self.busy = None;
            self.judged = 0;
        }
        Ok(())
    }

    /// What's being sent in now.
    pub fn codec(&self) -> Codec {
        match &self.encoder {
            Engine::Hard { codec, .. } => *codec,
            Engine::Soft(_) => Codec::H264,
        }
    }

    /// Whether what's encoded is a shared screen (sent at its own size,
    /// slower before smaller — see `screen_shapes`) or a camera. A change starts a new encoder,
    /// whose first frame is a full one; the shape's taken from the
    /// bitrate afresh.
    pub fn set_screen(&mut self, screen: bool) -> Result<()> {
        if screen == self.screen {
            return Ok(());
        }
        self.screen = screen;
        self.fastest = 0;
        self.retry = None;
        self.screen_shapes = if screen { screen_shapes(self.width_in, self.shown_long, hardware(ENCODER_FAILED.load(Ordering::Relaxed)).is_some()) } else { Vec::new() };
        self.shape = shape_index(self.shapes(), self.bitrate).max(self.filled()).max(self.shown());
        self.other_shape = None;
        self.encoder = new_encoder(self.bitrate, self.sent(self.shape()), screen, self.h265)?;
        self.started = false;
        self.busy = None;
        self.judged = 0;
        Ok(())
    }

    /// What it's encoding at: the bitrate given, up to what the shape's
    /// worth (see `ceiling`).
    pub fn bitrate(&self) -> u32 {
        self.bitrate.min(ceiling(self.shape(), self.screen, self.codec() == Codec::H265))
    }

    /// The size and pace pictures are sent at now.
    pub fn shape(&self) -> Shape {
        self.shapes()[self.shape].1
    }

    /// Notes how wide the pictures coming in are (an upright one's
    /// height: laid on its side): a shape wider than that is never sent —
    /// blowing a picture up only costs bits. (Taken at the next
    /// `set_bitrate`.)
    pub fn fit_picture(&mut self, width: usize) {
        self.width_in = width;
    }

    /// Notes how long the other side draws our pictures' longer side, in
    /// its own pixels (`CallSignal::ShownIn`; 0, not known): a shape
    /// bigger than needed for that is never sent — what isn't seen only
    /// costs bits, and the other side's time decoding it. (Taken at the
    /// next `set_bitrate`.)
    pub fn fit_shown(&mut self, long: usize) {
        self.shown_long = long;
    }

    /// The biggest shape the other side shows: the smallest no smaller
    /// than it draws ours, of those for a screen or a camera as it is.
    fn shown(&self) -> usize {
        if self.shown_long == 0 || self.screen {
            return 0;
        }
        self.shapes().iter().rposition(|(_, shape)| shape.width >= self.shown_long).unwrap_or(0)
    }

    /// The biggest shape the pictures coming in fill, of those for a
    /// screen or a camera as it is.
    fn filled(&self) -> usize {
        if self.width_in == 0 || self.screen {
            return 0;
        }
        self.shapes().iter().position(|(_, shape)| shape.width <= self.width_in).unwrap_or(self.shapes().len() - 1)
    }

    /// The shape Settings fixes a camera's pictures at (see
    /// `set_video_width`), if it does and this is a camera.
    fn fixed(&self) -> Option<usize> {
        let width = video_width().filter(|_| !self.screen)?;
        Some(self.shapes().iter().position(|(_, shape)| shape.width <= width).unwrap_or(0))
    }

    /// The shape that would be sent if the pictures were big enough: what
    /// the bitrate suits and this computer keeps up with, or what
    /// Settings fixes. A camera giving smaller pictures than this could
    /// be asked for bigger.
    pub fn wanted(&self) -> Shape {
        match self.fixed() {
            Some(fixed) => self.shapes()[fixed.max(self.shown())].1,
            None => self.shapes()[shape_index(self.shapes(), self.bitrate).max(self.fastest).max(self.shown())].1,
        }
    }

    /// The bitrate the next bigger shape would need, when there's one it
    /// could take (the pictures fill it, and this computer's kept up); at
    /// a fixed size, the least that size wants, until it has it.
    pub fn bigger_needs(&self) -> Option<u32> {
        if let Some(fixed) = self.fixed() {
            let least = self.shapes()[fixed.max(self.shown())].0;
            return (self.bitrate < least).then_some(least);
        }
        let biggest = self.fastest.max(self.filled()).max(self.shown());
        (self.shape > biggest).then(|| (self.shapes()[self.shape - 1].0 as u64 * 5 / 4) as u32)
    }

    /// How long a picture takes to encode (scaling included), on average.
    pub fn encode_time(&self) -> Option<Duration> {
        self.average.map(Duration::from_secs_f32)
    }

    /// Moves to `bitrate`: in place, or — once it's suited another shape
    /// for a while (see `SMALLER_AFTER`, `BIGGER_AFTER`) — with a new
    /// encoder, whose first frame is a full one. A bigger shape needs a
    /// quarter more than its least, so a rate near the line doesn't flip
    /// between the two.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        self.set_bitrate_at(bitrate, Instant::now())
    }

    fn set_bitrate_at(&mut self, bitrate: u32, now: Instant) -> Result<()> {
        self.follow_h265()?;
        self.follow_screen()?;
        match self.retry {
            Some(Retry::After(wait)) => self.retry = Some(Retry::At(now + wait)),
            Some(Retry::At(at)) if now >= at => {
                self.retry = None;
                self.fastest = 0;
            }
            _ => {}
        }
        let bitrate = bitrate.max(MIN_BITRATE);
        let shapes = self.shapes().to_vec();
        let mut suits = shape_index(&shapes, bitrate).max(self.fastest).max(self.filled()).max(self.shown());
        if suits < self.shape && (bitrate as u64) < shapes[suits].0 as u64 * 5 / 4 {
            suits += 1;
        }
        let fixed = self.fixed().map(|fixed| fixed.max(self.filled()).max(self.shown()));
        let shape = if let Some(fixed) = fixed {
            self.other_shape = None;
            fixed
        } else if suits == self.shape {
            self.other_shape = None;
            self.shape
        } else {
            let since = match self.other_shape {
                Some((shape, since)) if shape == suits => since,
                _ => {
                    self.other_shape = Some((suits, now));
                    now
                }
            };
            let wait = if suits > self.shape { SMALLER_AFTER } else { BIGGER_AFTER };
            let far_below = suits > self.shape && bitrate < shapes[self.shape].0 / 2;
            if far_below || now.duration_since(since) >= wait || !self.started {
                self.other_shape = None;
                suits
            } else {
                self.shape
            }
        };
        if shape != self.shape || !self.started {
            if shape != self.shape || bitrate != self.bitrate {
                self.encoder = new_encoder(bitrate, self.sent(shapes[shape].1), self.screen, self.h265)?;
                self.started = false;
                self.busy = None;
                self.judged = 0;
            }
        } else if bitrate.min(ceiling(shapes[shape].1, self.screen, self.codec() == Codec::H265)) != self.bitrate() {
            let rate = bitrate.min(ceiling(shapes[shape].1, self.screen, self.codec() == Codec::H265));
            let changed = match &mut self.encoder {
                Engine::Soft(encoder) => set_encoder_bitrate(encoder, openh264_sys2::ENCODER_OPTION_BITRATE, openh264_sys2::SPATIAL_LAYER_ALL, rate),
                Engine::Hard { encoder, .. } => encoder.set_bitrate(rate),
            };
            if let Err(e) = changed {
                if matches!(self.encoder, Engine::Soft(_)) {
                    return Err(e);
                }
                self.bitrate = bitrate;
                self.fall_back(e)?;
            }
        }
        self.bitrate = bitrate;
        self.shape = shape;
        Ok(())
    }

    /// Makes the next frame a full one — the other side lost something.
    pub fn request_keyframe(&mut self) {
        match &mut self.encoder {
            Engine::Soft(encoder) => encoder.force_intra_frame(),
            Engine::Hard { key_next, .. } => *key_next = true,
        }
    }

    /// Keeps to the shape whatever the time encoding takes (for
    /// measuring it: see `swarm video-check`).
    pub fn keep_shape(&mut self) {
        self.timed = false;
    }

    /// The hardware doing the encoding, if it is.
    pub fn hardware(&self) -> Option<String> {
        match &self.encoder {
            Engine::Soft(_) => None,
            Engine::Hard { encoder, .. } => Some(encoder.name()),
        }
    }

    /// The hardware failed: OpenH264 from now on (its first frame a full
    /// one), and the hardware not tried again (see `ENCODER_FAILED`) — or,
    /// if it was H.265 that failed, H.264 on it.
    fn fall_back(&mut self, why: anyhow::Error) -> Result<()> {
        if let Engine::Hard { encoder, codec, .. } = &self.encoder {
            if *codec == Codec::H265 {
                note_trouble(format!("{} failed at H.265, so H.264 took over: {why:#}", encoder.name()));
                H265_ENCODER_FAILED.store(true, Ordering::Relaxed);
                self.h265 = false;
                self.encoder = new_encoder(self.bitrate, self.sent(self.shape()), self.screen, false)?;
                self.started = false;
                self.busy = None;
                self.judged = 0;
                return Ok(());
            }
            note_trouble(format!("{} failed, so OpenH264 took over: {why:#}", encoder.name()));
            ENCODER_FAILED.store(true, Ordering::Relaxed);
        }
        self.h265 = false;
        self.encoder = Engine::Soft(Box::new(soft_encoder(self.bitrate, self.sent(self.shape()), self.screen)?));
        self.started = false;
        self.busy = None;
        self.judged = 0;
        Ok(())
    }

    /// Encodes `picture` (scaled to the shape's size) and cuts it into
    /// pieces of at most `max_piece` bytes of data. Empty when the
    /// encoder chose to skip this picture to keep to its bitrate.
    ///
    /// Takes a few milliseconds for a big picture, so it's best run
    /// where blocking is allowed. Once it's averaging more than
    /// `BUSY_SHARE` of the time between pictures, later pictures are
    /// sent a shape smaller.
    pub fn encode(&mut self, picture: &Picture, max_piece: usize) -> Result<Vec<VideoPiece>> {
        self.encode_from(Source::Own(picture), max_piece)
    }

    /// `encode`, for a picture other links are sending too: what's made
    /// of it at this size is shared with them (see `sized_for_encoder`).
    pub fn encode_shared(&mut self, picture: &Arc<Picture>, max_piece: usize) -> Result<Vec<VideoPiece>> {
        self.encode_from(Source::Shared(picture), max_piece)
    }

    fn encode_from(&mut self, source: Source<'_>, max_piece: usize) -> Result<Vec<VideoPiece>> {
        let picture = source.picture();
        let started = Instant::now();
        let upright = picture.height > picture.width;
        if upright != self.upright {
            self.upright = upright;
            self.encoder = new_encoder(self.bitrate, self.sent(self.shape()), self.screen, self.h265)?;
            self.started = false;
            self.busy = None;
            self.judged = 0;
        }
        let shape = self.sent(self.shape());
        if matches!(self.encoder, Engine::Hard { .. }) && !HARDWARE_ON.load(Ordering::Relaxed) {
            self.encoder = Engine::Soft(Box::new(soft_encoder(self.bitrate, shape, self.screen)?));
            self.h265 = false;
            self.started = false;
        }
        let made;
        let sized = match source {
            Source::Own(picture) => for_encoder(picture, shape.width, shape.height),
            Source::Shared(picture) => {
                made = sized_for_encoder(picture, shape.width, shape.height);
                Cow::Borrowed(&*made)
            }
        };
        let (data, key) = match &mut self.encoder {
            Engine::Soft(encoder) => {
                let (y, u, v) = sized.planes();
                let half = shape.width / 2;
                let yuv = YUVSlices::new((y, u, v), (shape.width, shape.height), (shape.width, half, half));
                let bitstream = encoder.encode(&yuv).map_err(|e| anyhow!("encoding video: {e}"))?;
                let key = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
                let skipped = matches!(bitstream.frame_type(), FrameType::Skip | FrameType::Invalid);
                let data = if skipped { Vec::new() } else { bitstream.to_vec() };
                if !self.started {
                    self.started = true;
                    set_encoder_bitrate(encoder, openh264_sys2::ENCODER_OPTION_MAX_BITRATE, openh264_sys2::SPATIAL_LAYER_0, MOST_BITRATE)?;
                }
                (data, key)
            }
            Engine::Hard { encoder, codec, since_key, key_next, proven } => {
                let codec = *codec;
                let want_key = *key_next || *since_key + 1 >= shape.fps * KEYFRAME_EVERY_SECS;
                match encoder.encode(&sized, want_key).and_then(|data| {
                    if !*proven && !data.is_empty() {
                        decodes(&data, shape, codec)?;
                        *proven = true;
                    }
                    Ok(data)
                }) {
                    Ok(data) => {
                        self.started = true;
                        let key = is_key(&data, codec);
                        if key {
                            *since_key = 0;
                            *key_next = false;
                        } else if !data.is_empty() {
                            *since_key += 1;
                            *key_next = want_key;
                        }
                        (data, key)
                    }
                    Err(e) => {
                        drop(sized);
                        self.fall_back(e)?;
                        return self.encode_from(source, max_piece);
                    }
                }
            }
        };
        let h265 = self.codec() == Codec::H265;
        let took = started.elapsed().as_secs_f32();
        self.average = Some(self.average.map_or(took, |a| a + (took - a) / shape.fps.max(1) as f32));
        self.keep_up(took, shape, key)?;
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let max_piece = max_piece.max(1);
        let count = data.len().div_ceil(max_piece);
        if count > u16::MAX as usize || data.len() > MAX_PICTURE_BYTES {
            if key && self.shape + 1 < self.shapes().len() {
                self.step_down()?;
            }
            return Ok(Vec::new());
        }
        let frame = self.next_frame;
        self.next_frame = self.next_frame.wrapping_add(1);
        Ok(data
            .chunks(max_piece)
            .enumerate()
            .map(|(i, chunk)| VideoPiece { frame, index: i as u16, count: count as u16, key, h265, small: false, data: chunk.to_vec() })
            .collect())
    }

    /// Notes that a picture at `shape` took `took` seconds, and moves to
    /// a smaller shape (for a while: see `step_down`) once that's too
    /// long on average. Hardware's not judged: its time's mostly waiting
    /// for the chip to hand the picture back — a phone's MediaCodec up to
    /// 40 ms, the whole time between pictures, at any size — while this
    /// computer does other things (a phone in a call of four went down to
    /// 320×180 for the whole call, encoding in software what its chip
    /// could have, at 200 kbit/s of a link carrying far more). Full
    /// frames don't count, nor does anything before a second's worth of
    /// ordinary ones: a new encoder's first frame is a full one and its
    /// setting up besides, several times as slow — on a slower computer
    /// (an i3 at 3.2GHz), over the line on its own, and each smaller
    /// shape's first frame then was too, so a call fell to 640×360 in
    /// its first moments however quick the encoding really was.
    fn keep_up(&mut self, took: f32, shape: Shape, key: bool) -> Result<()> {
        if !self.timed || key || self.fixed().is_some() || matches!(self.encoder, Engine::Hard { .. }) {
            return Ok(());
        }
        let weight = 1.0 / shape.fps.max(1) as f32;
        let busy = self.busy.map_or(took, |b| b + (took - b) * weight);
        self.busy = Some(busy);
        self.judged += 1;
        if self.judged >= shape.fps && busy * shape.fps as f32 > BUSY_SHARE && self.shape + 1 < self.shapes().len() {
            self.step_down()?;
        }
        Ok(())
    }

    /// Moves to the next smaller shape, with a new encoder, till
    /// `retry_wait` from now, when the bigger shapes may be had again: a
    /// slow spell (a call's start, its encoders all new at once, say)
    /// isn't the whole call's. Each time it's too slow again, twice as
    /// long, so a computer truly too slow for it isn't kept trying.
    fn step_down(&mut self) -> Result<()> {
        self.retry = Some(Retry::After(self.retry_wait));
        self.retry_wait = (self.retry_wait * 2).min(TRY_AGAIN_AFTER_MOST);
        self.fastest = self.shape + 1;
        self.encoder = new_encoder(self.bitrate, self.sent(self.shapes()[self.fastest].1), self.screen, self.h265)?;
        self.started = false;
        self.shape = self.fastest;
        self.busy = None;
        self.judged = 0;
        Ok(())
    }
}

/// What a received piece led to.
#[derive(Default)]
pub struct Received {
    /// A new picture to show.
    pub picture: Option<Picture>,
    /// Ask the sender for a full frame: something was lost.
    pub want_keyframe: bool,
    /// H.265 came that this side can't decode (any more): tell the sender
    /// (see `VideoDecoder::decodes_h265`).
    pub no_h265: bool,
}

struct Assembly {
    key: bool,
    codec: Codec,
    pieces: Vec<Option<Vec<u8>>>,
    /// The bytes of its pieces come so far.
    bytes: usize,
}

/// Pictures handed on (to the window, to the encoder), kept so that once
/// all else has let go of one, its memory takes a later picture, rather
/// than freed and as much again asked for each time: on the phone, whose
/// allocator (GrapheneOS's) hands a big block back to the system when
/// it's freed, and asks again, a tenth of Swarm's work in a video call.
#[derive(Default)]
pub struct Recycled {
    kept: VecDeque<Arc<Picture>>,
}

impl Recycled {
    /// As many as are held at once elsewhere at most: the newest where
    /// it's handed on, the one the window shows, the one before it the
    /// window's let go of, and one being encoded.
    const KEPT: usize = 4;

    /// Memory for a picture: a let-go-of picture's, as it is (what's in
    /// it is to be written over), or none yet.
    pub fn bytes(&mut self) -> Vec<u8> {
        self.kept.iter_mut().filter_map(Arc::get_mut).map(|p| std::mem::take(&mut p.data)).find(|b| b.capacity() > 0).unwrap_or_default()
    }

    /// `picture`, to hand on, kept for its memory later.
    pub fn share(&mut self, picture: Picture) -> Arc<Picture> {
        let picture = Arc::new(picture);
        if self.kept.len() == Self::KEPT {
            self.kept.pop_front();
        }
        self.kept.push_back(picture.clone());
        picture
    }
}

/// A decoded picture as I420, its planes' rows put together, in `data`'s
/// memory.
fn i420_of(yuv: &impl YUVSource, mut data: Vec<u8>) -> Picture {
    let (w, h) = yuv.dimensions();
    let (cw, ch) = chroma_size(w, h);
    let (ys, us, vs) = yuv.strides();
    data.clear();
    data.reserve(w * h + 2 * cw * ch);
    (0..h).for_each(|r| data.extend_from_slice(&yuv.y()[r * ys..][..w]));
    (0..ch).for_each(|r| data.extend_from_slice(&yuv.u()[r * us..][..cw]));
    (0..ch).for_each(|r| data.extend_from_slice(&yuv.v()[r * vs..][..cw]));
    Picture::i420(w, h, data)
}

/// Puts pieces back into frames and decodes them.
pub struct VideoDecoder {
    decoder: Decoding,
    assembling: BTreeMap<u32, Assembly>,
    last_shown: Option<u32>,
    /// Something was lost since the last full frame: nothing more can be
    /// shown until the next one.
    need_key: bool,
    last_request: Option<Instant>,
    /// The chip failed at H.265: it's not decoded again (this call).
    h265_failed: bool,
    /// Its pictures, for their memory (see `share`).
    memory: Recycled,
}

/// What does the decoding: OpenH264, or the graphics chip (in `Codec`).
enum Decoding {
    Soft(Decoder),
    Hard(Box<dyn HardDecoder>, Codec),
}

impl Decoding {
    fn codec(&self) -> Codec {
        match self {
            Decoding::Soft(_) => Codec::H264,
            Decoding::Hard(_, codec) => *codec,
        }
    }
}

/// Whether `frame`, a hardware encoder's first, is a full frame that
/// decodes to a picture of `shape`: in OpenH264 for H.264 (as every other
/// Swarm can), and for H.265 on this computer's chip, if it decodes it
/// (otherwise the other side says, should it not show).
fn decodes(frame: &[u8], shape: Shape, codec: Codec) -> Result<()> {
    if !is_key(frame, codec) {
        anyhow::bail!("its first frame wasn't a full one ({} bytes)", frame.len());
    }
    let size = match codec {
        Codec::H264 => match soft_decoder()?.decode(frame) {
            Ok(Some(yuv)) => yuv.dimensions(),
            Ok(None) => anyhow::bail!("its first frame decodes to no picture"),
            Err(e) => anyhow::bail!("its first frame doesn't decode: {e}"),
        },
        Codec::H265 => match hardware(false).filter(|h| h.h265().1).map(|h| h.decoder(Codec::H265)) {
            None => return Ok(()),
            Some(decoder) => match decoder?.decode(frame) {
                Ok(Some(picture)) => (picture.width, picture.height),
                Ok(None) => anyhow::bail!("its first frame decodes to no picture"),
                Err(e) => anyhow::bail!("its first frame doesn't decode: {e:#}"),
            },
        },
    };
    if size != (shape.width, shape.height) {
        anyhow::bail!("its first frame decodes as {size:?}, not {}×{}", shape.width, shape.height);
    }
    Ok(())
}

fn soft_decoder() -> Result<Decoder> {
    Decoder::new().map_err(|e| anyhow!("starting the video decoder: {e}"))
}

/// H.264 on the hardware where there is some, and it's turned on;
/// otherwise OpenH264.
fn h264_decoding() -> Result<Decoding> {
    let hard = hardware(false).and_then(|hardware| match hardware.decoder(Codec::H264) {
        Ok(decoder) => Some(decoder),
        Err(e) => {
            note_trouble(format!("no hardware decoder: {e:#}"));
            None
        }
    });
    Ok(match hard {
        Some(decoder) => Decoding::Hard(decoder, Codec::H264),
        None => Decoding::Soft(soft_decoder()?),
    })
}

impl VideoDecoder {
    /// On the hardware where there is some, and it's turned on.
    pub fn new() -> Result<Self> {
        Ok(Self::with(h264_decoding()?))
    }

    fn with(decoder: Decoding) -> Self {
        VideoDecoder { decoder, assembling: BTreeMap::new(), last_shown: None, need_key: true, last_request: None, h265_failed: false, memory: Recycled::default() }
    }

    /// A picture it decoded, to hand on: kept, so its memory takes a
    /// later picture once all else has let go of it.
    pub fn share(&mut self, picture: Picture) -> Arc<Picture> {
        self.memory.share(picture)
    }

    /// Whether H.265 is decoded here: what the other side's told.
    pub fn decodes_h265(&self) -> bool {
        !self.h265_failed && decodes_h265()
    }

    /// What decodes `codec` from here: for H.264, the hardware (or
    /// OpenH264); for H.265, only the hardware.
    fn switch_to(&mut self, codec: Codec) -> Result<()> {
        self.decoder = match codec {
            Codec::H264 => h264_decoding()?,
            Codec::H265 => {
                if !self.decodes_h265() {
                    anyhow::bail!("H.265 isn't decoded here");
                }
                let hardware = hardware(false).ok_or_else(|| anyhow!("no hardware"))?;
                Decoding::Hard(hardware.decoder(Codec::H265)?, Codec::H265)
            }
        };
        Ok(())
    }

    /// What's decoding now.
    pub fn codec(&self) -> Codec {
        self.decoder.codec()
    }

    /// OpenH264's, whatever hardware there is.
    pub fn software() -> Result<Self> {
        Ok(Self::with(Decoding::Soft(soft_decoder()?)))
    }

    /// The hardware doing the decoding, if it is.
    pub fn hardware(&self) -> Option<String> {
        match &self.decoder {
            Decoding::Soft(_) => None,
            Decoding::Hard(decoder, _) => Some(decoder.name()),
        }
    }

    /// For `fuzz/` only (see `protocol::fuzz_frame`): panics if more is
    /// held of pictures still being put together than `push` allows.
    #[cfg(fuzzing)]
    pub fn check_held(&self) {
        let held: usize = self.assembling.values().map(|a| a.bytes).sum();
        assert!(self.assembling.len() <= 8, "{} pictures held", self.assembling.len());
        assert!(held <= MAX_ASSEMBLING_BYTES, "{held} bytes held");
        assert!(self.assembling.values().all(|a| a.bytes <= MAX_PICTURE_BYTES), "a picture over the most");
    }

    pub fn push(&mut self, piece: VideoPiece) -> Received {
        let mut received = Received::default();
        let mut lost = false;
        let frame_no = piece.frame;
        let stale = self.last_shown.is_some_and(|last| frame_no.wrapping_sub(last) > u32::MAX / 2 || frame_no == last);
        if !stale && piece.count > 0 && piece.index < piece.count {
            let assembly = self.assembling.entry(frame_no).or_insert_with(|| Assembly { key: piece.key, codec: if piece.h265 { Codec::H265 } else { Codec::H264 }, pieces: vec![None; piece.count as usize], bytes: 0 });
            if assembly.pieces.len() == piece.count as usize && assembly.pieces[piece.index as usize].is_none() {
                assembly.bytes += piece.data.len();
                assembly.pieces[piece.index as usize] = Some(piece.data);
            }
            if assembly.bytes > MAX_PICTURE_BYTES {
                self.assembling.remove(&frame_no);
                self.need_key = true;
                lost = true;
            }
            if self.assembling.get(&frame_no).is_some_and(|a| a.pieces.iter().all(Option::is_some)) {
                let assembly = self.assembling.remove(&frame_no).expect("just looked at it");
                self.assembling.retain(|&n, _| frame_no.wrapping_sub(n) > u32::MAX / 2);
                let follows = self.last_shown.is_some_and(|last| frame_no == last.wrapping_add(1));
                if !follows && !assembly.key {
                    self.need_key = true;
                }
                if !assembly.key && self.need_key {
                    lost = true;
                }
                let mut decodable = assembly.key || !self.need_key;
                if decodable && assembly.codec != self.decoder.codec() {
                    let switched = if assembly.key { self.switch_to(assembly.codec) } else { Err(anyhow!("not a full frame")) };
                    if let Err(e) = switched {
                        if assembly.codec == Codec::H265 && assembly.key {
                            note_trouble(format!("H.265 came, and isn't decoded here: {e:#}"));
                            received.no_h265 = true;
                        }
                        self.need_key = true;
                        lost = true;
                        decodable = false;
                    }
                }
                if decodable {
                    let data: Vec<u8> = assembly.pieces.into_iter().flatten().flatten().collect();
                    let decoded = match &mut self.decoder {
                        Decoding::Soft(decoder) => match decoder.decode(&data) {
                            Ok(Some(yuv)) => Ok(Some(i420_of(&yuv, self.memory.bytes()))),
                            Ok(None) => Ok(None),
                            Err(e) => Err(anyhow!("{e}")),
                        },
                        Decoding::Hard(decoder, Codec::H265) => match decoder.decode(&data) {
                            Ok(picture) => Ok(picture),
                            Err(e) => {
                                note_trouble(format!("{} failed at H.265: {e:#}", decoder.name()));
                                self.h265_failed = true;
                                received.no_h265 = true;
                                if let Ok(decoding) = h264_decoding() {
                                    self.decoder = decoding;
                                }
                                Err(e)
                            }
                        },
                        Decoding::Hard(decoder, Codec::H264) => match decoder.decode(&data) {
                            Ok(picture) => Ok(picture),
                            Err(e) => {
                                note_trouble(format!("{} failed, so OpenH264 took over: {e:#}", decoder.name()));
                                if let Ok(soft) = soft_decoder() {
                                    self.decoder = Decoding::Soft(soft);
                                }
                                Err(e)
                            }
                        },
                    };
                    match decoded {
                        Ok(Some(picture)) => {
                            received.picture = Some(picture);
                            self.need_key = false;
                            self.last_shown = Some(frame_no);
                        }
                        Ok(None) => {
                            if matches!(self.decoder, Decoding::Hard(..)) {
                                self.need_key = false;
                            }
                            self.last_shown = Some(frame_no);
                        }
                        Err(_) => {
                            self.need_key = true;
                            lost = true;
                        }
                    }
                }
            }
        }
        while self.assembling.len() > 8 || self.assembling.values().map(|a| a.bytes).sum::<usize>() > MAX_ASSEMBLING_BYTES {
            let oldest = *self.assembling.keys().next().expect("non-empty");
            self.assembling.remove(&oldest);
            self.need_key = true;
            lost = true;
        }
        if lost && self.last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_EVERY) {
            self.last_request = Some(Instant::now());
            received.want_keyframe = true;
        }
        received
    }
}

#[cfg(test)]
mod tests {
    /// A picture's memory takes a later one once all else has let go of
    /// it, and not while anything holds it: picture memory freed and
    /// asked for again each time was a tenth of the phone's work.
    #[test]
    fn picture_memory_used_again_once_let_go_of() {
        let mut recycled = Recycled::default();
        let first = recycled.share(Picture::i420(4, 2, vec![7; 12]));
        let at = first.data.as_ptr();
        assert!(recycled.bytes().capacity() == 0, "held: not to be written over");
        drop(first);
        let bytes = recycled.bytes();
        assert_eq!((bytes.as_ptr(), bytes.capacity()), (at, 12), "let go of: its memory again");
        assert_eq!(recycled.bytes().capacity(), 0, "given once only");
        drop(bytes);
        let held: Vec<_> = (0..10).map(|i| recycled.share(Picture::i420(4, 2, vec![i; 12]))).collect();
        assert!(recycled.kept.len() <= Recycled::KEPT);
        assert_eq!(recycled.bytes().capacity(), 0);
        drop(held);
        assert_eq!(recycled.bytes().capacity(), 12);
    }

    /// Every `step`th pixel each way, as RGB or RGBA, mirrored or not, is
    /// what the whole picture's conversion has there — a phone's own
    /// camera, 1080 by 1920, for a box a third its width, as well as odd
    /// sizes and RGB pictures.
    /// A decoded picture's memory takes a later one once it's been shown
    /// and let go of, and the pictures are as they were.
    #[test]
    fn decoded_pictures_use_memory_again() {
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        let (mut decoder, mut fresh) = (VideoDecoder::new().unwrap(), VideoDecoder::new().unwrap());
        let mut memory = Vec::new();
        for t in 0..6 {
            for piece in encoder.encode(&test_picture(t).scaled(640, 360), 1200).unwrap() {
                let expected = fresh.push(piece.clone()).picture;
                if let Some(picture) = decoder.push(piece).picture {
                    assert!(Some(&picture) == expected.as_ref(), "picture {t} as decoded into new memory");
                    let shown = decoder.share(picture);
                    memory.push(shown.data.as_ptr());
                }
            }
        }
        assert_eq!(memory.len(), 6);
        assert!(memory[1..].iter().all(|&m| m == memory[0]), "each in the last one's memory: {memory:?}");
    }

    #[test]
    fn every_nth_pixel_as_the_whole_has_it() {
        let mut x = 5u32;
        let mut noise = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect()
        };
        let mut pictures = vec![Picture::rgb(7, 5, noise(7 * 5 * 3))];
        for (w, h) in [(1080, 1920), (7, 5), (6, 4)] {
            let (cw, ch) = chroma_size(w, h);
            pictures.push(Picture::i420(w, h, noise(w * h + 2 * cw * ch)));
        }
        for picture in &pictures {
            for step in [1, 2, 3] {
                let (w, h) = picture.every(step);
                for mirror in [false, true] {
                    let mut whole = vec![0; picture.width * picture.height * 3];
                    picture.rgb_into(&mut whole, false);
                    let mut three = vec![0; w * h * 3];
                    let mut four = vec![0; w * h * 4];
                    picture.rgb_every_into(&mut three, mirror, step);
                    picture.rgba_every_into(&mut four, mirror, step);
                    let mut made = Vec::with_capacity(w * h * 3);
                    for y in 0..h {
                        for x in 0..w {
                            let x = if mirror { w - 1 - x } else { x };
                            let at = (y * step * picture.width + x * step) * 3;
                            made.extend_from_slice(&whole[at..at + 3]);
                        }
                    }
                    let what = format!("{}×{} {:?}, every {step}, mirrored {mirror}", picture.width, picture.height, picture.format);
                    assert!(three == made, "RGB: {what}");
                    let made: Vec<u8> = made.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
                    assert!(four == made, "RGBA: {what}");
                }
            }
        }
    }

    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Tests that turn H.265 off in Settings, or look at what went wrong
    /// with the hardware (both the whole process's), one at a time.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
    }

    thread_local! {
        /// Hardware for this test's thread only (others keep OpenH264).
        pub static TEST_HARDWARE: std::cell::RefCell<Option<&'static dyn Hardware>> = const { std::cell::RefCell::new(None) };
        /// Settings' video size for this test's thread only (see
        /// `video_width`; 0: automatic).
        pub static TEST_VIDEO_WIDTH: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }

    /// Pretend hardware: OpenH264 underneath, counting what it's asked,
    /// and failing on its `fail_at`th picture or decode.
    struct FakeHardware {
        encoded: AtomicUsize,
        decoded: AtomicUsize,
        bitrates: Mutex<Vec<u32>>,
        fail_at: usize,
        /// Its decoder hands each picture back a frame late.
        late: bool,
        /// Its frames' NAL units have no type (as Mesa's for AMD's chips
        /// without a packed slice header): nothing a decoder shows.
        garbled: bool,
        /// Whether it encodes and decodes (pretend) H.265; its H.265
        /// encoder and decoder failing at their `fail_at`th go, if not
        /// 0.
        h265: (bool, bool),
        h265_fail_at: usize,
        h265_encoded: AtomicUsize,
        h265_decoded: AtomicUsize,
    }

    impl Default for FakeHardware {
        fn default() -> Self {
            FakeHardware {
                encoded: AtomicUsize::new(0),
                decoded: AtomicUsize::new(0),
                bitrates: Mutex::new(Vec::new()),
                fail_at: 0,
                late: false,
                garbled: false,
                h265: (false, false),
                h265_fail_at: 0,
                h265_encoded: AtomicUsize::new(0),
                h265_decoded: AtomicUsize::new(0),
            }
        }
    }

    /// Pretend H.265: OpenH264's frame, its bytes turned over so neither
    /// OpenH264 nor H.264's start codes make anything of it, behind an
    /// H.265 NAL header — an IRAP's (IDR_W_RADL) for a full frame, a
    /// TRAIL_R's otherwise.
    fn pretend_h265(frame: &[u8]) -> Vec<u8> {
        let header: [u8; 5] = if has_idr(frame) { [0, 0, 1, 19 << 1, 1] } else { [0, 0, 1, 1 << 1, 1] };
        header.iter().copied().chain(frame.iter().map(|b| !b)).collect()
    }

    fn from_pretend_h265(frame: &[u8]) -> Result<Vec<u8>> {
        match frame {
            [0, 0, 1, _, 1, rest @ ..] => Ok(rest.iter().map(|b| !b).collect()),
            _ => anyhow::bail!("not (pretend) H.265"),
        }
    }

    struct FakeEncoder(Encoder, &'static FakeHardware, Codec);
    struct FakeDecoder(Decoder, &'static FakeHardware, Option<Picture>, Codec);

    impl HardEncoder for FakeEncoder {
        fn encode(&mut self, picture: &Picture, key: bool) -> Result<Vec<u8>> {
            if self.2 == Codec::H265 && self.1.h265_encoded.fetch_add(1, Ordering::Relaxed) + 1 == self.1.h265_fail_at {
                anyhow::bail!("the pretend chip fell over at H.265");
            }
            if self.1.encoded.fetch_add(1, Ordering::Relaxed) + 1 == self.1.fail_at {
                anyhow::bail!("the pretend chip fell over");
            }
            if key {
                self.0.force_intra_frame();
            }
            let (y, u, v) = picture.planes();
            let yuv = YUVSlices::new((y, u, v), (picture.width, picture.height), (picture.width, picture.width / 2, picture.width / 2));
            let mut frame = self.0.encode(&yuv).map_err(|e| anyhow!("{e}"))?.to_vec();
            if self.1.garbled {
                for i in 3..frame.len() {
                    if frame[i - 3..i] == [0, 0, 1] {
                        frame[i] = 0;
                    }
                }
            }
            Ok(if self.2 == Codec::H265 { pretend_h265(&frame) } else { frame })
        }
        fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
            self.1.bitrates.locked().push(bitrate);
            Ok(())
        }
        fn name(&self) -> String {
            "pretend chip".to_string()
        }
    }

    impl HardDecoder for FakeDecoder {
        fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
            if self.3 == Codec::H265 && self.1.h265_decoded.fetch_add(1, Ordering::Relaxed) + 1 == self.1.h265_fail_at {
                anyhow::bail!("the pretend chip fell over at H.265");
            }
            if self.1.decoded.fetch_add(1, Ordering::Relaxed) + 1 == self.1.fail_at {
                anyhow::bail!("the pretend chip fell over");
            }
            let unwrapped;
            let frame = if self.3 == Codec::H265 {
                unwrapped = from_pretend_h265(frame)?;
                &unwrapped[..]
            } else {
                frame
            };
            let picture = self.0.decode(frame).map_err(|e| anyhow!("{e}"))?.map(|yuv| {
                let (w, h) = yuv.dimensions();
                let mut rgb = vec![0u8; w * h * 3];
                yuv.write_rgb8(&mut rgb);
                Picture::rgb(w, h, rgb).to_i420().into_owned()
            });
            Ok(if self.1.late { std::mem::replace(&mut self.2, picture) } else { picture })
        }
        fn name(&self) -> String {
            "pretend chip".to_string()
        }
    }

    impl Hardware for &'static FakeHardware {
        fn encoder(&self, shape: Shape, bitrate: u32, codec: Codec) -> Result<Box<dyn HardEncoder>> {
            if codec == Codec::H265 && !self.h265.0 {
                anyhow::bail!("no H.265 here");
            }
            let config = EncoderConfig::new().bitrate(BitRate::from_bps(bitrate)).max_frame_rate(FrameRate::from_hz(shape.fps as f32)).intra_frame_period(IntraFramePeriod::from_num_frames(10_000));
            Ok(Box::new(FakeEncoder(Encoder::with_api_config(OpenH264API::from_source(), config)?, self, codec)))
        }
        fn decoder(&self, codec: Codec) -> Result<Box<dyn HardDecoder>> {
            if codec == Codec::H265 && !self.h265.1 {
                anyhow::bail!("no H.265 here");
            }
            Ok(Box::new(FakeDecoder(soft_decoder()?, self, None, codec)))
        }
        fn h265(&self) -> (bool, bool) {
            self.h265
        }
    }

    /// Pretend hardware for this thread, failing at its `fail_at`th go
    /// (0: never).
    fn pretend_hardware(fail_at: usize) -> &'static FakeHardware {
        pretend_hardware_late(fail_at, false)
    }

    fn pretend_hardware_late(fail_at: usize, late: bool) -> &'static FakeHardware {
        pretend(FakeHardware { fail_at, late, ..Default::default() })
    }

    fn pretend(fake: FakeHardware) -> &'static FakeHardware {
        let fake: &'static FakeHardware = Box::leak(Box::new(fake));
        let hardware: &'static dyn Hardware = Box::leak(Box::new(fake));
        TEST_HARDWARE.with(|h| *h.borrow_mut() = Some(hardware));
        fake
    }

    /// Video goes through the hardware where there is some: pictures
    /// come out, a full frame when asked for, a new bitrate in place —
    /// and a screen, or a small shape, stays on OpenH264.
    #[test]
    fn video_goes_through_the_hardware_when_there_is_some() {
        let fake = pretend_hardware(0);
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        assert_eq!(encoder.hardware().as_deref(), Some("pretend chip"));
        let mut decoder = VideoDecoder::new().unwrap();
        assert_eq!(decoder.hardware().as_deref(), Some("pretend chip"));
        let mut send = |encoder: &mut VideoEncoder, t: usize| {
            let pieces = encoder.encode(&test_picture(t).scaled(1280, 720), 1200).unwrap();
            let key = pieces.first().is_some_and(|p| p.key);
            let mut shown = None;
            for piece in pieces {
                shown = decoder.push(piece).picture.or(shown);
            }
            (key, shown)
        };
        let (key, shown) = send(&mut encoder, 0);
        assert!(key, "the first frame's a full one");
        let shown = shown.expect("a picture came through");
        assert_eq!((shown.width, shown.height, shown.format), (1280, 720, Format::I420));
        assert!(!send(&mut encoder, 1).0);
        encoder.request_keyframe();
        assert!(send(&mut encoder, 2).0, "a full frame when asked");
        assert!(!send(&mut encoder, 3).0);
        encoder.set_bitrate(2_200_000).unwrap();
        assert_eq!(*fake.bitrates.locked(), vec![2_200_000]);
        assert!(!send(&mut encoder, 4).0, "a new bitrate takes no full frame");
        assert_eq!(fake.encoded.load(Ordering::Relaxed), 5);
        encoder.set_screen(true).unwrap();
        assert!(encoder.hardware().is_some(), "a screen with room for 15 a second is the chip's");
        encoder.set_bitrate(600_000).unwrap();
        assert_eq!(encoder.hardware(), None, "short of that, OpenH264's");
        assert_eq!(VideoEncoder::new(200_000).unwrap().hardware(), None, "so is a small shape");
    }

    /// Encodes one picture and gives its pieces to `decoder`: whether it
    /// was H.265, a full frame, what was shown, and whether the decoder
    /// said it doesn't take H.265.
    fn send_one(encoder: &mut VideoEncoder, decoder: &mut VideoDecoder, t: usize) -> (bool, bool, Option<Picture>, bool) {
        let pieces = encoder.encode(&test_picture(t).scaled(1280, 720), 1200).unwrap();
        let (h265, key) = pieces.first().map_or((false, false), |p| (p.h265, p.key));
        let (mut shown, mut no_h265) = (None, false);
        for piece in pieces {
            let received = decoder.push(piece);
            shown = received.picture.or(shown);
            no_h265 |= received.no_h265;
        }
        (h265, key, shown, no_h265)
    }

    /// H.265's sent only once the other side says it decodes it, this
    /// chip encodes it and Settings allows it — each change a full frame
    /// — with H.265's lower bitrates; the decoder follows, frame by frame.
    #[test]
    fn h265_is_sent_only_where_both_ends_have_it() {
        let _one = one_at_a_time();
        pretend(FakeHardware { h265: (true, true), ..Default::default() });
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        let mut decoder = VideoDecoder::new().unwrap();
        assert!(decoder.decodes_h265());
        let (h265, key, shown, _) = send_one(&mut encoder, &mut decoder, 0);
        assert!(!h265 && key && shown.is_some(), "H.264 until they say");
        encoder.set_h265(true);
        encoder.set_bitrate(2_000_000).unwrap();
        assert_eq!(encoder.codec(), Codec::H265);
        assert_eq!(encoder.bitrate(), 1_500_000, "H.265's ceiling");
        let (h265, key, shown, _) = send_one(&mut encoder, &mut decoder, 1);
        assert!(h265 && key, "H.265, from a full frame");
        assert_eq!(shown.map(|p| (p.width, p.height)), Some((1280, 720)));
        assert_eq!(decoder.codec(), Codec::H265);
        let (h265, key, shown, _) = send_one(&mut encoder, &mut decoder, 2);
        assert!(h265 && !key && shown.is_some());
        set_h265(false);
        assert!(!decoder.decodes_h265());
        encoder.set_bitrate(2_000_000).unwrap();
        let (h265, key, shown, _) = send_one(&mut encoder, &mut decoder, 3);
        set_h265(true);
        assert!(!h265 && key && shown.is_some(), "H.264 again, from a full frame");
        assert_eq!(decoder.codec(), Codec::H264);
        encoder.set_bitrate(2_000_000).unwrap();
        assert_eq!(encoder.codec(), Codec::H265);
        encoder.set_screen(true).unwrap();
        encoder.set_bitrate(2_000_000).unwrap();
        assert_eq!((encoder.codec(), encoder.shape().fps), (Codec::H265, 15));
        assert!(encoder.hardware().is_some());
        encoder.set_bitrate(500_000).unwrap();
        assert_eq!((encoder.codec(), encoder.shape().fps), (Codec::H264, 5));
        assert!(encoder.hardware().is_none(), "OpenH264's screen mode");
        pretend(FakeHardware { h265: (false, true), ..Default::default() });
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.set_h265(true);
        encoder.set_bitrate(2_000_000).unwrap();
        assert_eq!(encoder.codec(), Codec::H264);
    }

    /// H.265 to a side that doesn't decode it isn't shown, and it says
    /// so; a chip failing at H.265 goes back to H.264 at once, sending
    /// and receiving, and the receiver says it no longer decodes it.
    #[test]
    fn h265_that_fails_goes_back_to_h264() {
        let _one = one_at_a_time();
        pretend(FakeHardware { h265: (true, true), ..Default::default() });
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        encoder.set_h265(true);
        encoder.set_bitrate(2_000_000).unwrap();
        let pieces = encoder.encode(&test_picture(0).scaled(1280, 720), 1200).unwrap();
        assert!(pieces[0].h265);
        pretend(FakeHardware { h265: (true, false), ..Default::default() });
        let mut h264_only = VideoDecoder::new().unwrap();
        assert!(!h264_only.decodes_h265());
        let mut said = false;
        for piece in pieces {
            let received = h264_only.push(piece);
            assert!(received.picture.is_none());
            said |= received.no_h265 && received.want_keyframe;
        }
        assert!(said, "says it doesn't decode H.265, and wants a full frame");

        pretend(FakeHardware { h265: (true, true), h265_fail_at: 3, ..Default::default() });
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        encoder.set_h265(true);
        encoder.set_bitrate(2_000_000).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        let (h265, _, shown, _) = send_one(&mut encoder, &mut decoder, 0);
        assert!(h265 && shown.is_some());
        let (_, _, shown, no_h265) = send_one(&mut encoder, &mut decoder, 1);
        assert!(shown.is_none() && no_h265, "the decoder fell over at H.265, and says so");
        assert!(!decoder.decodes_h265());
        encoder.set_h265(false);
        encoder.set_bitrate(2_000_000).unwrap();
        let (h265, key, shown, _) = send_one(&mut encoder, &mut decoder, 2);
        assert!(!h265 && key && shown.is_some(), "H.264 again");

        pretend(FakeHardware { h265: (true, true), h265_fail_at: 2, ..Default::default() });
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        encoder.set_h265(true);
        encoder.set_bitrate(2_000_000).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        assert!(send_one(&mut encoder, &mut decoder, 0).0);
        let (h265, key, shown, _) = send_one(&mut encoder, &mut decoder, 1);
        assert!(!h265 && key && shown.is_some(), "H.264 from a full frame, the picture kept");
        assert_eq!(encoder.hardware().as_deref(), Some("pretend chip"));
        assert!(!encodes_h265(), "not tried again");
        set_h265(false);
        set_h265(true);
        assert!(encodes_h265(), "till Settings turns it off and on");
    }

    /// The H.265 shapes are the H.264 ones, in the same order (an
    /// encoder moving between the two keeps its shape), needing three
    /// fifths the bitrate on the chip.
    #[test]
    fn h265_shapes_need_less() {
        for ((h264, a), (h265, b)) in SHAPES.iter().zip(H265_SHAPES.iter()) {
            assert_eq!(a, b);
            assert_eq!(*h265, if a.width >= HARDWARE_FROM_WIDTH { h264 / 5 * 3 } else { *h264 });
        }
        assert!(H265_SHAPES.windows(2).all(|w| w[0].0 > w[1].0));
        assert_eq!(H265_SHAPES[0].0, 6_000_000, "4K from 6 Mbit/s");
        assert_eq!(ceiling(Shape::LARGEST, false, true), 9_600_000);
    }

    /// The pretend H.265's full frames are H.265's, and not H.264's.
    #[test]
    fn full_frames_are_told_apart_by_codec() {
        let key = pretend_h265(&[0, 0, 0, 1, 0x65, 1, 2]);
        assert!(is_key(&key, Codec::H265) && !has_idr(&key));
        assert!(!is_key(&pretend_h265(&[0, 0, 0, 1, 0x41, 1]), Codec::H265));
        assert!(is_key(&[0, 0, 1, 21 << 1, 1], Codec::H265));
        assert!(is_key(&[0, 0, 1, 16 << 1, 1], Codec::H265));
        assert!(!is_key(&[0, 0, 1, 0, 1], Codec::H265));
    }

    /// A hardware decoder handing pictures back a frame late (Media
    /// Foundation's can) still shows every one after the first, and asks
    /// for nothing: its "nothing yet" for a full frame isn't a loss.
    #[test]
    fn a_decoder_a_frame_late_still_shows_every_picture() {
        let _fake = pretend_hardware_late(0, true);
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        let mut decoder = VideoDecoder::new().unwrap();
        let (mut shown, mut asked) = (0, false);
        for t in 0..10 {
            for piece in encoder.encode(&test_picture(t).scaled(1280, 720), 1200).unwrap() {
                let received = decoder.push(piece);
                shown += received.picture.is_some() as usize;
                asked |= received.want_keyframe;
            }
        }
        assert_eq!(shown, 9);
        assert!(!asked);
    }

    /// Hardware that fails mid-call hands over to OpenH264: the encoder's
    /// next frame is a full one from OpenH264, the decoder asks for one
    /// and decodes it with OpenH264, and pictures carry on.
    #[test]
    fn hardware_that_fails_mid_call_hands_over_to_openh264() {
        let _one = one_at_a_time();
        let _fake = pretend_hardware(4);
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        let mut decoder = VideoDecoder::new().unwrap();
        let mut shown = 0;
        let mut asked = false;
        let mut keys = Vec::new();
        for t in 0..8 {
            let pieces = encoder.encode(&test_picture(t).scaled(1280, 720), 1200).unwrap();
            keys.push(pieces.first().is_some_and(|p| p.key));
            for piece in pieces {
                let received = decoder.push(piece);
                shown += received.picture.is_some() as usize;
                asked |= received.want_keyframe;
            }
            if asked {
                encoder.request_keyframe();
                asked = false;
            }
        }
        assert_eq!(encoder.hardware(), None);
        assert_eq!(decoder.hardware(), None);
        assert_eq!(&keys[..4], &[true, false, false, true]);
        assert!(shown >= 6, "{shown} of 8 shown");
        assert!(hardware_trouble().is_some_and(|t| t.contains("pretend chip failed")));
    }

    /// A hardware encoder whose frames nothing can show (Mesa's for an
    /// AMD chip, given no packed slice header, codes its slices as NAL
    /// units of no type) is found out on its first frame, and OpenH264
    /// takes over at once: every picture's shown.
    #[test]
    fn a_hardware_encoder_whose_frames_dont_decode_hands_over_at_once() {
        let _one = one_at_a_time();
        let fake = pretend(FakeHardware { garbled: true, ..Default::default() });
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        let mut decoder = VideoDecoder::new().unwrap();
        let mut shown = 0;
        for t in 0..5 {
            for piece in encoder.encode(&test_picture(t).scaled(1280, 720), 1200).unwrap() {
                shown += decoder.push(piece).picture.is_some() as usize;
            }
        }
        assert_eq!(shown, 5);
        assert_eq!(encoder.hardware(), None);
        assert_eq!(fake.encoded.load(Ordering::Relaxed), 1, "only its first frame tried");
        assert!(hardware_trouble().is_some_and(|t| t.contains("pretend chip failed")), "{:?}", hardware_trouble());
    }

    fn test_picture(t: usize) -> Picture {
        let (width, height) = (320, 240);
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let inside = (x as i32 - (100 + t as i32 * 4)).pow(2) + (y as i32 - 120).pow(2) < 900;
                rgb.extend(if inside { [255, 128, 0] } else { [(x / 2) as u8, (y / 2) as u8, 90] });
            }
        }
        Picture::rgb(width, height, rgb)
    }

    /// The scaler as it first was: plain, and slow (each pixel looked up
    /// on its own) — the faster one must give exactly the same pictures.
    fn scaled_plainly(p: &Picture, width: usize, height: usize) -> Picture {
        let (src_w, src_h) = (p.width.max(1), p.height.max(1));
        let (crop_w, crop_h) = if src_w * height > src_h * width { (src_h * width / height, src_h) } else { (src_w, src_w * height / width) };
        let (x_off, y_off) = ((src_w - crop_w) / 2, (src_h - crop_h) / 2);
        let mut rgb = Vec::with_capacity(width * height * 3);
        for ty in 0..height {
            let (y0, y1) = (y_off + ty * crop_h / height, y_off + ((ty + 1) * crop_h / height).max(ty * crop_h / height + 1));
            for tx in 0..width {
                let (x0, x1) = (x_off + tx * crop_w / width, x_off + ((tx + 1) * crop_w / width).max(tx * crop_w / width + 1));
                let mut sum = [0u32; 3];
                let mut n = 0;
                for y in y0..y1.min(p.height) {
                    for x in x0..x1.min(p.width) {
                        let px = p.pixel(x, y);
                        for c in 0..3 {
                            sum[c] += px[c] as u32;
                        }
                        n += 1;
                    }
                }
                let n = n.max(1);
                rgb.extend([sum[0] / n, sum[1] / n, sum[2] / n].map(|v| v as u8));
            }
        }
        Picture::rgb(width, height, rgb)
    }

    #[test]
    fn scaling_quickly_gives_the_same_pictures() {
        let mut x = 7u32;
        for (w, h) in [(1920, 1080), (1280, 720), (640, 480), (333, 251), (100, 400)] {
            let rgb = (0..w * h * 3)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect();
            let p = Picture::rgb(w, h, rgb);
            for (tw, th) in [(1280, 720), (960, 540), (640, 360), (480, 270), (192, 108), (7, 3), (500, 500)] {
                assert!(p.scaled(tw, th) == scaled_plainly(&p, tw, th), "{w}×{h} to {tw}×{th}");
            }
        }
    }

    /// The plain scaler, plane by plane, for an I420 picture.
    fn i420_scaled_plainly(p: &Picture, width: usize, height: usize) -> Picture {
        let plane = |data: &[u8], w: usize, h: usize, crop: (usize, usize, usize, usize), tw: usize, th: usize| {
            let (x_off, y_off, crop_w, crop_h) = crop;
            let mut out = Vec::new();
            for ty in 0..th {
                let (y0, y1) = (y_off + ty * crop_h / th, y_off + ((ty + 1) * crop_h / th).max(ty * crop_h / th + 1));
                for tx in 0..tw {
                    let (x0, x1) = (x_off + tx * crop_w / tw, x_off + ((tx + 1) * crop_w / tw).max(tx * crop_w / tw + 1));
                    let (mut sum, mut n) = (0u32, 0u32);
                    for y in y0..y1.min(h) {
                        for x in x0..x1.min(w) {
                            sum += data[y * w + x] as u32;
                            n += 1;
                        }
                    }
                    out.push((sum / n.max(1)) as u8);
                }
            }
            out
        };
        let (src_w, src_h) = (p.width, p.height);
        let (crop_w, crop_h) = if src_w * height > src_h * width { (src_h * width / height, src_h) } else { (src_w, src_w * height / width) };
        let (x_off, y_off) = ((src_w - crop_w) / 2, (src_h - crop_h) / 2);
        let (y, u, v) = p.planes();
        let (cw, ch) = (src_w.div_ceil(2), src_h.div_ceil(2));
        let ccrop = (x_off / 2, y_off / 2, crop_w.div_ceil(2).min(cw), crop_h.div_ceil(2).min(ch));
        let mut data = plane(y, src_w, src_h, (x_off, y_off, crop_w, crop_h), width, height);
        data.extend(plane(u, cw, ch, ccrop, width / 2, height / 2));
        data.extend(plane(v, cw, ch, ccrop, width / 2, height / 2));
        Picture::i420(width, height, data)
    }

    /// RGBA, for a window that takes it as it is (Android's), is the
    /// RGB with each pixel opaque, mirrored or not, from I420 or RGB.
    #[test]
    fn rgba_is_rgb_opaque() {
        let rgb = Picture::rgb(6, 4, (0..6 * 4 * 3).map(|i| (i * 7 % 256) as u8).collect());
        for picture in [rgb.clone(), rgb.to_i420().into_owned()] {
            for mirror in [false, true] {
                let mut three = vec![0; 6 * 4 * 3];
                let mut four = vec![0; 6 * 4 * 4];
                picture.rgb_into(&mut three, mirror);
                picture.rgba_into(&mut four, mirror);
                let made: Vec<u8> = three.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
                assert_eq!(four, made, "{:?}, mirrored {mirror}", picture.format);
            }
        }
    }

    #[test]
    fn i420_pictures_scale_plane_by_plane() {
        let mut x = 11u32;
        for (w, h) in [(1920, 1080), (1280, 720), (334, 252)] {
            let data = (0..w * h * 3 / 2)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect();
            let p = Picture::i420(w, h, data);
            for (tw, th) in [(960, 540), (640, 360), (192, 108), (500, 500)] {
                assert!(p.scaled(tw, th) == i420_scaled_plainly(&p, tw, th), "{w}×{h} to {tw}×{th}");
            }
        }
    }

    /// YUV to RGB here gives what openh264's own does (to within three
    /// steps: it's done in 16-bit numbers, for speed), and a picture survives
    /// RGB to I420 and back.
    #[test]
    fn i420_converts_as_openh264_does() {
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        let picture = test_picture(3).scaled(320, 180);
        let mut decoder = Decoder::new().unwrap();
        let mut checked = false;
        for _ in 0..3 {
            let frame: Vec<u8> = encoder.encode(&picture, 1200).unwrap().into_iter().flat_map(|p| p.data).collect();
            if let Ok(Some(yuv)) = decoder.decode(&frame) {
                let (w, h) = yuv.dimensions();
                let mut theirs = vec![0u8; w * h * 3];
                yuv.write_rgb8(&mut theirs);
                let (ys, us, vs) = yuv.strides();
                let mut data = Vec::new();
                (0..h).for_each(|r| data.extend_from_slice(&yuv.y()[r * ys..r * ys + w]));
                (0..h / 2).for_each(|r| data.extend_from_slice(&yuv.u()[r * us..r * us + w / 2]));
                (0..h / 2).for_each(|r| data.extend_from_slice(&yuv.v()[r * vs..r * vs + w / 2]));
                let ours = Picture::i420(w, h, data);
                let most = ours.to_rgb().data.iter().zip(&theirs).map(|(&a, &b)| a.abs_diff(b)).max().unwrap();
                assert!(most <= 3, "differ by up to {most}");
                checked = true;
            }
        }
        assert!(checked);
        let back = picture.to_i420().to_rgb().into_owned();
        assert!(psnr(&back, &picture) > 34.0, "{:.1} dB", psnr(&back, &picture));
    }

    /// A camera's I420 picture goes to the encoder as it is (a different
    /// size included) and comes out the other side as the same picture.
    #[test]
    fn i420_pictures_are_encoded_as_they_are() {
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.timed = false;
        let mut decoder = VideoDecoder::new().unwrap();
        let picture = test_picture(5).scaled(640, 480);
        let i420 = picture.to_i420().into_owned();
        let mut shown = None;
        for _ in 0..5 {
            for piece in encoder.encode(&i420, 1200).unwrap() {
                shown = decoder.push(piece).picture.or(shown);
            }
        }
        let shown = shown.expect("a picture came through");
        let sent = picture.scaled(shown.width, shown.height);
        assert!(psnr(&shown, &sent) > 30.0, "{:.1} dB", psnr(&shown, &sent));
    }

    #[test]
    fn scaling_keeps_the_shape_and_averages() {
        let p = Picture::rgb(4, 2, [[0, 0, 0], [255, 255, 255]].repeat(4).concat());
        let s = p.scaled(2, 1);
        assert_eq!((s.width, s.height), (2, 1));
        assert!(s.pixel(0, 0)[0] > 100 && s.pixel(0, 0)[0] < 155);
        let wide = Picture::rgb(400, 100, vec![9; 400 * 100 * 3]);
        assert_eq!(wide.scaled(160, 120).data.len(), 160 * 120 * 3);
    }

    /// A screenful of small dark text on white, standing still, as a
    /// page of code is: lines of letters 6 pixels by 11 (each a few
    /// strokes), some short, some empty.
    fn text_screen() -> Picture {
        let (width, height) = (1920, 1080);
        let mut rgb = vec![250u8; width * height * 3];
        let mut seed = 5u32;
        let mut next = move |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 16) % n
        };
        let strokes: [(usize, usize, usize, usize); 6] = [(0, 0, 1, 9), (4, 0, 1, 9), (2, 0, 1, 9), (0, 0, 5, 1), (0, 4, 5, 1), (0, 8, 5, 1)];
        for line in 0..height / 14 {
            let length = if next(4) == 0 { 0 } else { 10 + next(100) as usize };
            let indent = 4 * next(4) as usize;
            for column in indent..(indent + length).min(width / 7 - 1) {
                if next(6) == 0 {
                    continue;
                }
                let (x0, y0) = (8 + column * 7, 2 + line * 14);
                for _ in 0..2 + next(2) {
                    let (sx, sy, w, h) = strokes[next(6) as usize];
                    for y in y0 + sy..y0 + sy + h {
                        for x in x0 + sx..x0 + sx + w {
                            let i = (y * width + x) * 3;
                            rgb[i..i + 3].copy_from_slice(&[30, 30, 40]);
                        }
                    }
                }
            }
        }
        Picture::rgb(width, height, rgb)
    }

    /// How alike two pictures are, in dB (higher is more alike).
    fn psnr(a: &Picture, b: &Picture) -> f64 {
        let (a, b) = (a.to_rgb(), b.to_rgb());
        let squared: f64 = a.data.iter().zip(&b.data).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum();
        10.0 * (255.0f64.powi(2) / (squared / a.data.len() as f64).max(1e-9)).log10()
    }

    /// A shared screen's text comes through far sharper as a screen than
    /// it would as a camera's picture, at the same bitrate.
    #[test]
    fn a_screen_is_sent_sharper_than_a_camera_would_send_it() {
        let screen = text_screen();
        let sent_as = |as_screen: bool| -> f64 {
            let mut encoder = VideoEncoder::new(800_000).unwrap();
            encoder.timed = false;
            encoder.set_screen(as_screen).unwrap();
            encoder.fit_picture(screen.width);
            encoder.set_bitrate(800_000).unwrap();
            let mut decoder = VideoDecoder::new().unwrap();
            let mut shown = None;
            for _ in 0..encoder.shape().fps * 2 {
                for piece in encoder.encode(&screen, 1200).unwrap() {
                    if let Some(picture) = decoder.push(piece).picture {
                        shown = Some(picture);
                    }
                }
            }
            psnr(&shown.expect("a picture came through").scaled(screen.width, screen.height), &screen)
        };
        let (as_camera, as_screen) = (sent_as(false), sent_as(true));
        assert!(as_screen > as_camera + 6.0, "as a screen {as_screen:.1}dB, as a camera {as_camera:.1}dB");
    }

    /// A screen's sent at its own size, fewer pictures a second as the
    /// bitrate falls, only smaller at the very least; on the chip at the
    /// top (its own size there, a 4K screen's least bitrates four times
    /// 1080p's), in software no wider than 1080p; no bigger than it's
    /// drawn.
    #[test]
    fn a_screen_keeps_its_size_and_slows_down_instead() {
        let sizes = |shapes: &[(u32, Shape)]| shapes.iter().map(|(least, s)| (*least, s.width, s.height, s.fps)).collect::<Vec<_>>();
        assert_eq!(
            sizes(&screen_shapes(1920, 0, true)),
            [
                (2_500_000, 1920, 1080, 25),
                (1_500_000, 1920, 1080, 15),
                (1_000_000, 1920, 1080, 10),
                (600_000, 1920, 1080, 8),
                (400_000, 1920, 1080, 5),
                (250_000, 1920, 1080, 3),
                (150_000, 1920, 1080, 2),
                (60_000, 960, 540, 2),
                (0, 640, 360, 2),
            ]
        );
        let four_k = screen_shapes(3840, 0, true);
        assert_eq!(sizes(&four_k[..3]), [(10_000_000, 3840, 2160, 25), (6_000_000, 3840, 2160, 15), (1_000_000, 1920, 1080, 10)]);
        assert!(screen_shapes(3840, 0, false).iter().all(|(_, s)| s.width <= 1920), "no wider than 1080p in software");
        assert!(screen_shapes(3840, 1000, true).iter().all(|(_, s)| s.width <= 1280), "drawn 1000 across: 720p at most");
        assert_eq!(sizes(&screen_shapes(1366, 0, false)[..1]), [(1_264_814, 1366, 768, 25)]);
        for shapes in [screen_shapes(1920, 0, true), four_k, screen_shapes(320, 0, false)] {
            assert!(shapes.windows(2).all(|w| w[0].0 > w[1].0 || w[1].0 == 0), "biggest first: {shapes:?}");
        }
        let mut encoder = VideoEncoder::new(3_000_000).unwrap();
        encoder.timed = false;
        encoder.set_screen(true).unwrap();
        encoder.fit_picture(1920);
        let mut now = Instant::now();
        for (rate, fps) in [(3_000_000, 25), (1_600_000, 15), (700_000, 8), (300_000, 3), (160_000, 2)] {
            for _ in 0..4 {
                now += Duration::from_secs(1);
                encoder.set_bitrate_at(rate, now).unwrap();
            }
            assert_eq!((encoder.shape().width, encoder.shape().fps), (1920, fps), "at {rate}");
        }
    }

    /// Too slow at a screen in software: fewer pictures a second, the
    /// size kept.
    #[test]
    fn a_screen_too_slow_to_encode_slows_down_not_shrinks() {
        let mut encoder = VideoEncoder::new(3_000_000).unwrap();
        encoder.set_screen(true).unwrap();
        encoder.fit_picture(1920);
        encoder.set_bitrate(3_000_000).unwrap();
        assert_eq!(encoder.shape().fps, 25);
        for _ in 0..30 {
            encoder.keep_up(0.039, encoder.shape(), false).unwrap();
        }
        assert_eq!((encoder.shape().width, encoder.shape().fps), (1920, 15));
    }

    /// A contact claiming pictures of the most pieces a frame can have,
    /// and sending them, never has more kept than two of the biggest
    /// pictures sent: each over the most one may be is dropped.
    #[test]
    fn pieces_beyond_a_pictures_most_are_never_kept() {
        let mut decoder = VideoDecoder::new().unwrap();
        let data = vec![7u8; 1200];
        for frame in 0..12u32 {
            for index in 0..(MAX_PICTURE_BYTES / data.len() + 10) as u16 {
                decoder.push(VideoPiece { frame, index, count: u16::MAX, key: true, h265: false, small: false, data: data.clone() });
                let held: usize = decoder.assembling.values().map(|a| a.bytes).sum();
                assert!(held <= MAX_ASSEMBLING_BYTES && decoder.assembling.values().all(|a| a.bytes <= MAX_PICTURE_BYTES));
            }
        }
    }

    /// A full frame needing more pieces than a frame can have (a screen
    /// of fine noise, sent big) can never be sent: the shape steps down
    /// until one fits, and pictures come through.
    #[test]
    fn a_full_frame_too_big_to_send_steps_the_shape_down() {
        let mut seed = 1u32;
        let noise = Picture::rgb(
            1920,
            1080,
            (0..1920 * 1080 * 3)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    (seed >> 16) as u8
                })
                .collect(),
        );
        let mut encoder = VideoEncoder::new(1_500_000).unwrap();
        encoder.timed = false;
        encoder.set_screen(true).unwrap();
        encoder.fit_picture(noise.width);
        encoder.set_bitrate(1_500_000).unwrap();
        assert_eq!(encoder.shape().width, 1920);
        let mut decoder = VideoDecoder::new().unwrap();
        let mut shown = None;
        for _ in 0..10 {
            for piece in encoder.encode(&noise, 4).unwrap() {
                shown = decoder.push(piece).picture.or(shown);
            }
        }
        let shown = shown.expect("a picture came through");
        assert!(shown.width < 1920, "shown at {}", shown.width);
        assert_eq!(encoder.shape().width, shown.width);
    }

    /// How long OpenH264 takes to decode a picture here, and to turn it
    /// into RGB, at each size a camera's sent in: for a phone, run there
    /// (`--ignored`).
    #[test]
    #[ignore]
    fn decoding_timed_at_each_size() {
        for (width, height) in [(640, 360), (960, 540), (1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
            let config = EncoderConfig::new().bitrate(BitRate::from_bps(ceiling(Shape { width, height, fps: 25 }, false, false))).max_frame_rate(FrameRate::from_hz(25.0));
            let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).unwrap();
            let mut decoder = soft_decoder().unwrap();
            let (mut decoding, mut turning, mut n) = (Duration::ZERO, Duration::ZERO, 0u32);
            for t in 0..50usize {
                let rgb = (0..width * height)
                    .flat_map(|i| {
                        let (x, y) = (i % width + t * 3, i / width + t);
                        let h = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 7;
                        [(x / 4 + h % 40) as u8, (y / 3 + h % 50) as u8, ((x + y) / 5) as u8]
                    })
                    .collect();
                let picture = Picture::rgb(width, height, rgb).to_i420().into_owned();
                let (y, u, v) = picture.planes();
                let yuv = YUVSlices::new((y, u, v), (width, height), (width, width / 2, width / 2));
                let data = encoder.encode(&yuv).unwrap().to_vec();
                let started = Instant::now();
                let decoded = decoder.decode(&data).unwrap();
                let decoded_at = Instant::now();
                if let Some(yuv) = decoded {
                    let mut rgb = vec![0u8; width * height * 3];
                    yuv.write_rgb8(&mut rgb);
                    turning += decoded_at.elapsed();
                    decoding += decoded_at - started;
                    n += 1;
                }
            }
            eprintln!("{width}×{height}: decoding {:.1} ms, into RGB {:.1} ms", decoding.as_secs_f32() * 1000.0 / n as f32, turning.as_secs_f32() * 1000.0 / n as f32);
        }
    }

    /// A picture taller than wide (a phone held upright) is sent upright,
    /// in its shape turned on end, all of it: not its middle cropped to a
    /// band across. Laid on its side again, it's sent as before.
    #[test]
    fn upright_pictures_are_sent_upright() {
        let upright = |t: usize| {
            let p = test_picture(t);
            let mut rgb = Vec::with_capacity(p.data.len());
            for x in 0..p.width {
                for y in (0..p.height).rev() {
                    rgb.extend_from_slice(&p.data[(y * p.width + x) * 3..][..3]);
                }
            }
            Picture::rgb(p.height, p.width, rgb)
        };
        let mut encoder = VideoEncoder::new(400_000).unwrap();
        encoder.timed = false;
        encoder.fit_picture(320);
        encoder.set_bitrate(400_000).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        let mut sizes = Vec::new();
        for t in 0..12 {
            let picture = if t < 6 { upright(t) } else { test_picture(t) };
            for piece in encoder.encode(&picture, 1200).unwrap() {
                if let Some(shown) = decoder.push(piece).picture {
                    sizes.push((shown.width, shown.height));
                }
            }
        }
        let shape = encoder.shape();
        assert_eq!(sizes.first(), Some(&(shape.height, shape.width)), "upright: {sizes:?}");
        assert_eq!(sizes.last(), Some(&(shape.width, shape.height)), "on its side again: {sizes:?}");
    }

    /// What's decoded comes out as the decoder has it, I420, not turned
    /// into RGB: the window turns it into its own pixels in any case, and
    /// on a phone the extra turning was a seventh of all Swarm's work in
    /// a video call.
    #[test]
    fn pictures_are_decoded_to_i420() {
        let mut encoder = VideoEncoder::new(400_000).unwrap();
        encoder.timed = false;
        let mut decoder = VideoDecoder::new().unwrap();
        let mut shown = None;
        for t in 0..4 {
            for piece in encoder.encode(&test_picture(t), 1200).unwrap() {
                shown = decoder.push(piece).picture.or(shown);
            }
        }
        let shown = shown.expect("a picture came through");
        assert_eq!(shown.format, Format::I420);
        let [r, g, b] = shown.pixel(shown.width * 112 / 320, shown.height / 2);
        assert!(r > 200 && (90..170).contains(&g) && b < 60, "{:?}", [r, g, b]);
    }

    #[test]
    fn frames_go_through_in_pieces_and_come_out_as_pictures() {
        let mut encoder = VideoEncoder::new(60_000).unwrap();
        encoder.timed = false;
        let mut decoder = VideoDecoder::new().unwrap();
        let mut shown = 0;
        let mut pieces_seen = 0;
        for t in 0..20 {
            for piece in encoder.encode(&test_picture(t), 200).unwrap() {
                assert!(piece.data.len() <= 200);
                pieces_seen += 1;
                if let Some(picture) = decoder.push(piece).picture {
                    let shape = Shape::for_bitrate(60_000);
                    assert_eq!((picture.width, picture.height), (shape.width, shape.height));
                    shown += 1;
                }
            }
        }
        assert!(shown >= 15, "only {shown} of 20 shown");
        assert!(pieces_seen > shown, "frames should need more than one piece at 200 bytes");
    }

    #[test]
    fn after_a_lost_piece_nothing_is_shown_until_a_full_frame_which_is_asked_for() {
        let mut encoder = VideoEncoder::new(60_000).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        for t in 0..3 {
            for piece in encoder.encode(&test_picture(t), 200).unwrap() {
                decoder.push(piece);
            }
        }
        let mut lost = encoder.encode(&test_picture(3), 200).unwrap();
        lost.remove(0);
        for piece in lost {
            decoder.push(piece);
        }
        let mut asked = false;
        let mut shown_before_key = 0;
        for t in 4..8 {
            for piece in encoder.encode(&test_picture(t), 200).unwrap() {
                let received = decoder.push(piece);
                asked |= received.want_keyframe;
                shown_before_key += received.picture.is_some() as usize;
            }
        }
        assert!(asked, "the receiver should ask for a full frame");
        assert_eq!(shown_before_key, 0, "nothing built on the lost frame is shown");
        encoder.request_keyframe();
        let mut shown = false;
        for piece in encoder.encode(&test_picture(8), 200).unwrap() {
            assert!(piece.key);
            shown |= decoder.push(piece).picture.is_some();
        }
        assert!(shown, "the full frame gets things going again");
    }

    /// A new bitrate in the same shape changes the encoder in place: no
    /// full frame, which on a slow link holds up the voice behind it.
    #[test]
    fn a_new_bitrate_takes_no_full_frame_unless_the_shape_changes() {
        let mut encoder = VideoEncoder::new(500_000).unwrap();
        encoder.timed = false;
        let mut decoder = VideoDecoder::new().unwrap();
        let noise = |t: usize| {
            let mut picture = test_picture(t);
            let mut x = (t as u32).wrapping_mul(2_654_435_761) | 1;
            for c in picture.data.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *c = c.saturating_add((x % 12) as u8);
            }
            picture
        };
        let mut send = |encoder: &mut VideoEncoder, t: usize| -> (bool, usize, Option<Picture>) {
            let pieces = encoder.encode(&noise(t), 1000).unwrap();
            let key = pieces.first().is_some_and(|p| p.key);
            let bytes = pieces.iter().map(|p| p.data.len()).sum();
            (key, bytes, pieces.into_iter().filter_map(|p| decoder.push(p).picture).last())
        };
        assert!(send(&mut encoder, 0).0, "the first frame is a full one");
        for t in 1..10 {
            send(&mut encoder, t);
        }
        encoder.set_bitrate(700_000).unwrap();
        assert_eq!(encoder.bitrate(), 700_000);
        let mut shown = 0;
        for t in 10..20 {
            let (key, _, picture) = send(&mut encoder, t);
            assert!(!key, "no full frame for a new bitrate");
            shown += picture.is_some() as usize;
        }
        assert!(shown >= 5, "pictures still show ({shown} of 10; the rest skipped to keep to the rate)");
        let small: usize = (20..40).map(|t| send(&mut encoder, t).1).sum();
        encoder.set_bitrate(1_100_000).unwrap();
        assert_eq!(encoder.shape().width, 640, "still under a quarter more than the next shape needs");
        let big: usize = (40..60).map(|t| send(&mut encoder, t).1).sum();
        assert!(big > small, "{big} bytes at the higher rate, {small} before");
        let now = Instant::now();
        encoder.set_bitrate_at(2_500_000, now).unwrap();
        assert_eq!(encoder.shape().width, 640, "not yet");
        assert!(!send(&mut encoder, 60).0);
        encoder.set_bitrate_at(2_500_000, now + BIGGER_AFTER).unwrap();
        let (key, _, shown) = send(&mut encoder, 61);
        assert!(key);
        assert_eq!(shown.map(|p| p.width), Some(1280));
    }

    #[test]
    fn the_shape_follows_the_bitrate() {
        assert_eq!(Shape::for_bitrate(12_000_000), Shape::LARGEST);
        assert_eq!((Shape::LARGEST.width, Shape::LARGEST.height, Shape::LARGEST.fps), (3840, 2160, 25));
        assert_eq!(Shape::for_bitrate(7_000_000).width, 2560);
        assert_eq!(Shape::for_bitrate(5_000_000).width, 1920);
        assert_eq!(Shape::for_bitrate(2_500_000).width, 1280);
        assert_eq!(Shape::for_bitrate(500_000).width, 640);
        assert_eq!(Shape::for_bitrate(MIN_BITRATE).width, 192);
        let mut encoder = VideoEncoder::new(12_000_000).unwrap();
        encoder.timed = false;
        assert_eq!(encoder.shape(), Shape::LARGEST);
        encoder.set_bitrate(500_000).unwrap();
        assert_eq!(encoder.shape().width, 640);
        let pieces = encoder.encode(&test_picture(0), 1000).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        let shown = pieces.into_iter().find_map(|p| decoder.push(p).picture).expect("a full first frame");
        assert_eq!((shown.width, shown.height), (640, 360));
    }

    /// A rate going back and forth over a shape's line doesn't change
    /// the shape each time — each change is a full frame.
    #[test]
    fn the_shape_changes_only_once_the_rate_has_stayed() {
        let mut encoder = VideoEncoder::new(700_000).unwrap();
        encoder.timed = false;
        encoder.encode(&test_picture(0), 1000).unwrap();
        let start = Instant::now();
        for second in 0..10 {
            let rate = if second % 2 == 0 { 350_000 } else { 600_000 };
            encoder.set_bitrate_at(rate, start + Duration::from_secs(second)).unwrap();
            assert_eq!(encoder.shape().width, 640, "second {second}");
        }
        encoder.set_bitrate_at(350_000, start + Duration::from_secs(10)).unwrap();
        encoder.set_bitrate_at(350_000, start + Duration::from_secs(10) + SMALLER_AFTER).unwrap();
        assert_eq!(encoder.shape().width, 480);
        let mut encoder = VideoEncoder::new(700_000).unwrap();
        encoder.timed = false;
        encoder.encode(&test_picture(0), 1000).unwrap();
        encoder.set_bitrate(100_000).unwrap();
        assert!(encoder.shape().width < 480);
    }

    /// Pictures smaller than the bitrate suits aren't blown up; what
    /// would be sent with bigger ones is said, for asking the camera.
    /// More bitrate than a shape's worth isn't spent on it.
    #[test]
    fn a_shape_gets_no_more_than_its_worth() {
        let mut encoder = VideoEncoder::new(5_000_000).unwrap();
        encoder.timed = false;
        encoder.fit_picture(1280);
        encoder.set_bitrate(5_000_000).unwrap();
        assert_eq!(encoder.shape().width, 1280);
        assert_eq!(encoder.bitrate(), 2_500_000);
        encoder.fit_picture(1920);
        encoder.set_bitrate(5_000_000).unwrap();
        assert_eq!((encoder.shape().width, encoder.bitrate()), (1920, 5_000_000));
    }

    /// No bigger shape's sent than the other side draws our pictures at
    /// (the longer side, in its own pixels): the smallest that's no
    /// smaller than that, whatever the bitrate allows — a phone showing
    /// pictures 1080 across is sent 720p, not the 4K the link had room
    /// for, which took it 153 ms to decode. And the camera's asked only
    /// for that (`wanted`). Unknown (0): no limit.
    #[test]
    fn a_shape_is_never_bigger_than_the_other_side_shows() {
        let mut encoder = VideoEncoder::new(20_000_000).unwrap();
        encoder.timed = false;
        encoder.fit_picture(3840);
        for (shown, width) in [(1080, 1280), (1280, 1280), (480, 480), (500, 640), (100, 192), (0, 3840), (5000, 3840)] {
            encoder.fit_shown(shown);
            encoder.set_bitrate(20_000_000).unwrap();
            assert_eq!(encoder.shape().width, width, "shown {shown} across");
            assert_eq!(encoder.wanted().width, width, "shown {shown} across: the camera asked for");
        }
        encoder.set_screen(true).unwrap();
        encoder.fit_shown(1000);
        encoder.set_bitrate(20_000_000).unwrap();
        assert_eq!(encoder.shape().width, 1280);
    }

    /// Links sending the same picture at the same size share what's made
    /// of it for their encoders, made once: a 4K camera's picture scaled
    /// to 1080p takes 7.4 ms, which each link did for itself, 30 times a
    /// second. At another size, or of another picture, it's made afresh.
    #[test]
    fn links_at_one_size_share_the_picture_made_for_them() {
        let grey = |w: usize, h: usize, v: u8| Arc::new(Picture::i420(w, h, vec![v; w * h * 3 / 2]));
        let (camera, next) = (grey(1280, 720, 100), grey(1280, 720, 150));
        let made = || SIZINGS.with(|n| n.get());
        let before = made();
        let a = sized_for_encoder(&camera, 640, 360);
        let b = sized_for_encoder(&camera, 640, 360);
        assert!(Arc::ptr_eq(&a, &b), "the same picture, shared");
        assert_eq!(made() - before, 1, "made once");
        let c = sized_for_encoder(&camera, 480, 270);
        assert_eq!((c.width, c.height), (480, 270));
        let d = sized_for_encoder(&next, 640, 360);
        assert!(!Arc::ptr_eq(&a, &d), "the next picture's its own");
        assert_eq!(made() - before, 3);
        assert_eq!(a.data, for_encoder(&camera, 640, 360).data);
    }

    /// What's made of a picture is let go once nothing else holds the
    /// picture — made at two sizes too (links at different sizes), each
    /// holding it: they held each other's picture for good, and in a
    /// conference a 4K camera's pictures filled the phone's memory in a
    /// few minutes (3.6 GB, killed by Android).
    #[test]
    fn whats_made_of_pictures_is_let_go() {
        let mut last: Option<std::sync::Weak<Picture>> = None;
        for i in 0..20u8 {
            let p = Arc::new(Picture::rgb(640, 360, vec![i; 640 * 360 * 3]));
            let _ = (sized_for_encoder(&p, 320, 180), sized_for_encoder(&p, 192, 108));
            if let Some(last) = &last {
                assert!(last.upgrade().is_none(), "the one before's let go of, picture {i}");
            }
            last = Some(Arc::downgrade(&p));
        }
    }

    #[test]
    fn a_shape_is_never_bigger_than_the_pictures() {
        let mut encoder = VideoEncoder::new(12_000_000).unwrap();
        encoder.timed = false;
        encoder.fit_picture(1280);
        encoder.set_bitrate(12_000_000).unwrap();
        assert_eq!(encoder.shape().width, 1280);
        assert_eq!(encoder.wanted(), Shape::LARGEST);
        encoder.fit_picture(640);
        encoder.set_bitrate(12_000_000).unwrap();
        assert_eq!(encoder.shape().width, 640);
    }

    #[test]
    fn a_computer_too_slow_for_a_shape_sends_smaller_ones_for_a_while() {
        let mut encoder = VideoEncoder::new(12_000_000).unwrap();
        encoder.keep_up(0.040, encoder.shape(), true).unwrap();
        encoder.keep_up(0.025, encoder.shape(), false).unwrap();
        for _ in 0..50 {
            encoder.keep_up(0.005, encoder.shape(), false).unwrap();
            encoder.keep_up(0.020, encoder.shape(), false).unwrap();
        }
        assert_eq!(encoder.shape(), Shape::LARGEST);
        for _ in 0..50 {
            encoder.keep_up(0.036, encoder.shape(), false).unwrap();
        }
        let smaller = encoder.shape();
        assert!(smaller.width < Shape::LARGEST.width);
        encoder.set_bitrate(16_000_000).unwrap();
        assert_eq!(encoder.shape().width, smaller.width);
    }

    /// A shape stepped down from for being slow is tried again later — a
    /// slow spell (a call's start, all its encoders new at once) isn't the
    /// whole call's — and later each time it's still too slow.
    #[test]
    fn a_shape_too_slow_for_is_tried_again_later_and_later_each_time() {
        let mut encoder = VideoEncoder::new(16_000_000).unwrap();
        let slow = |encoder: &mut VideoEncoder| {
            let was = encoder.shape();
            while encoder.shape() == was {
                encoder.keep_up(0.036, encoder.shape(), false).unwrap();
            }
        };
        let mut now = Instant::now();
        let mut waited = Vec::new();
        for _ in 0..3 {
            slow(&mut encoder);
            assert_eq!(encoder.shape(), Shape::all().nth(1).unwrap(), "one step down");
            let stepped = now;
            while encoder.shape() != Shape::LARGEST {
                assert!(now - stepped < Duration::from_secs(600), "never tried again: {waited:?}");
                now += Duration::from_secs(1);
                encoder.set_bitrate_at(16_000_000, now).unwrap();
            }
            waited.push((now - stepped).as_secs());
        }
        assert!((30..40).contains(&waited[0]), "{waited:?}");
        assert!((60..70).contains(&waited[1]), "{waited:?}");
        assert!((120..130).contains(&waited[2]), "{waited:?}");
    }

    /// Hardware isn't stepped down for the time it takes: that's mostly
    /// waiting for the chip to hand each picture back (a phone's
    /// MediaCodec, up to 40 ms of its own), not this computer busy.
    #[test]
    fn a_chip_slow_to_hand_each_picture_back_is_not_stepped_down() {
        let _fake = pretend_hardware(0);
        let mut encoder = VideoEncoder::new(2_000_000).unwrap();
        encoder.set_bitrate(2_000_000).unwrap();
        assert!(encoder.hardware().is_some());
        for _ in 0..50 {
            encoder.keep_up(0.036, encoder.shape(), false).unwrap();
        }
        assert_eq!(encoder.shape().width, 1280);
    }

    /// A size fixed in Settings is sent from the first picture, however
    /// little the link offers at first, and kept as it offers more: no
    /// step up (each a new encoder, a full frame the other side froze
    /// for), nor down for being slow. No bigger than the camera gives; a
    /// shared screen goes on by the bitrate.
    #[test]
    fn a_fixed_size_is_sent_from_the_start_and_kept() {
        TEST_VIDEO_WIDTH.with(|w| w.set(Some(1280)));
        let mut encoder = VideoEncoder::new(100_000).unwrap();
        encoder.timed = false;
        encoder.set_bitrate(100_000).unwrap();
        assert_eq!(encoder.shape().width, 1280, "not 320, as 100 kbit/s suits");
        assert_eq!(encoder.wanted().width, 1280);
        assert_eq!(encoder.bigger_needs(), Some(1_800_000), "the link's asked for what the size wants");
        let mut decoder = VideoDecoder::new().unwrap();
        let mut keys = 0;
        let start = Instant::now();
        for (i, rate) in [100_000, 400_000, 900_000, 1_800_000, 2_500_000].into_iter().enumerate() {
            encoder.set_bitrate_at(rate, start + BIGGER_AFTER * 2 * i as u32).unwrap();
            for t in 0..3 {
                for piece in encoder.encode(&test_picture(i * 3 + t), 1000).unwrap() {
                    keys += usize::from(piece.key && piece.index == 0);
                    if let Some(shown) = decoder.push(piece).picture {
                        assert_eq!((shown.width, shown.height), (1280, 720));
                    }
                }
            }
            assert_eq!(encoder.shape().width, 1280, "at {rate}");
        }
        assert_eq!(keys, 1, "one full frame: the first");
        assert_eq!(encoder.bigger_needs(), None);
        encoder.timed = true;
        for _ in 0..50 {
            encoder.keep_up(0.039, encoder.shape(), false).unwrap();
        }
        assert_eq!(encoder.shape().width, 1280);
        encoder.fit_picture(960);
        encoder.set_bitrate(2_500_000).unwrap();
        assert_eq!(encoder.shape().width, 960);
        encoder.set_screen(true).unwrap();
        encoder.set_bitrate(100_000).unwrap();
        let shapes = screen_shapes(960, 0, false);
        assert_eq!(encoder.shape(), shapes[shape_index(&shapes, 100_000)].1);
        encoder.set_screen(false).unwrap();
        TEST_VIDEO_WIDTH.with(|w| w.set(Some(0)));
        encoder.set_bitrate(100_000).unwrap();
        assert_eq!(encoder.shape().width, 320);
        TEST_VIDEO_WIDTH.with(|w| w.set(None));
    }
}
