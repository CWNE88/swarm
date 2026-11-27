//! Webcam capture for the TUI's camera preview (`w`) and video calls:
//! V4L2 on Linux, Media Foundation on Windows, AVFoundation on macOS,
//! and Camera2 on Android. Like audio, capture belongs to the front end — core
//! will only ever see frames handed to it.
//!
//! A thread of its own reads the camera and keeps only the newest frame
//! (`Camera::latest`): if drawing falls behind, frames are skipped, never
//! queued up into a growing delay.
//!
//! Settings picks the camera, by name (`choose`), where there's more
//! than one; one not plugged in is passed over. `SWARM_CAMERA` picks the
//! device whatever Settings says (`/dev/video2`; on Windows or macOS a
//! number or part of the camera's name), `test` for a moving test pattern (`test-busy`
//! with grain all over, taking all the bitrate it's given), for
//! trying the preview with no camera at all, or `still:<file>` for a
//! picture file held up to it (a QR code, say). Otherwise the first camera
//! that will deliver video is used.

#[cfg(windows)]
#[path = "camera_mf.rs"]
mod mf;
#[cfg(target_os = "android")]
#[path = "camera_android.rs"]
mod android;
#[cfg(target_os = "macos")]
#[path = "camera_macos.rs"]
mod avfoundation;

use anyhow::Result;
#[cfg(not(any(windows, target_os = "android", target_os = "macos")))]
use anyhow::bail;
#[cfg(target_os = "linux")]
use anyhow::Context;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::Arc;
use tokio::sync::watch;
use std::time::{Duration, Instant};
#[cfg(target_os = "linux")]
use v4l::{buffer::Type, io::traits::CaptureStream, prelude::*, video::Capture, FourCC};

/// A size to ask a camera for.
pub type Size = (u32, u32);
/// The sizes a camera's opened at, biggest first: those a call sends at
/// 720p and up (`swarm_core::video::Shape`), so a call's picture is
/// never blown up. Each is a step down for a camera, or a computer, too
/// slow at the one above (a C922 gave 1080p at 11 frames a second on a
/// two-core i3, 720p at 24).
pub const SIZES: [Size; 4] = [(3840, 2160), (2560, 1440), (1920, 1080), (1280, 720)];
/// The biggest a call sends (`swarm_core::video::Shape::LARGEST`): what
/// the preview and a call open a camera at, so a call growing to it never
/// has to reopen the camera (which freezes the picture for a moment).
pub const LARGEST: Size = SIZES[0];

/// Which of `SIZES` is the biggest this computer's camera hasn't been
/// found too slow at: it's opened no bigger from then on, for as long as
/// Swarm runs.
static SMOOTH_UP_TO: AtomicUsize = AtomicUsize::new(0);

/// The size to open the camera at: `LARGEST`, or the size Settings
/// sends video at — unless it's been found too slow at that (and then the
/// biggest it wasn't).
pub fn best_size() -> Size {
    let chosen = swarm_core::video::video_width().map_or(0, |width| SIZES.iter().position(|size| size.0 as usize <= width).unwrap_or(SIZES.len() - 1));
    SIZES[SMOOTH_UP_TO.load(Ordering::Relaxed).max(chosen).min(SIZES.len() - 1)]
}

/// The smallest of `SIZES` at least `width` wide — what a call sending
/// pictures that wide needs — but no bigger than `best_size`.
pub fn size_for(width: usize) -> Size {
    let best = SMOOTH_UP_TO.load(Ordering::Relaxed).min(SIZES.len() - 1);
    SIZES[best..].iter().rev().find(|size| size.0 as usize >= width).copied().unwrap_or(SIZES[best])
}

/// Which of `SIZES` a camera giving pictures `width` wide is at: the
/// biggest no wider (a camera asked for 4K may give 1080p).
fn rung(width: u32) -> usize {
    SIZES.iter().position(|size| size.0 <= width).unwrap_or(SIZES.len() - 1)
}

/// A camera giving fewer pictures a second than this (measured over
/// `PACE_CHECK`, once it's been going that long: its first moments are
/// slow in any case) is too slow at its size — a smooth smaller picture
/// is better than a jerky big one.
const SMOOTH_ENOUGH_FPS: f32 = 20.0;
const PACE_CHECK: Duration = Duration::from_secs(2);

/// Watches a camera keep pace at its size (see `SMOOTH_ENOUGH_FPS`), for
/// the preview and a call alike.
#[derive(Default)]
pub struct PaceCheck {
    /// Frames captured, and when, at the start of measuring; `done` once
    /// it's been found smooth enough.
    start: Option<(u64, Instant)>,
    done: bool,
}

impl PaceCheck {
    /// Whether `camera` has turned out too slow at the size it gives
    /// (then noted, so it's opened a size smaller from now on: see
    /// `best_size`). Call it now and then.
    pub fn too_slow(&mut self, camera: &Camera) -> bool {
        let at = rung(camera.info().width);
        if self.done || at + 1 >= SIZES.len() || camera.running_for() < PACE_CHECK {
            return false;
        }
        match self.start {
            None => self.start = Some((camera.frames_captured(), Instant::now())),
            Some((frames, since)) if since.elapsed() >= PACE_CHECK => {
                let fps = camera.frames_captured().saturating_sub(frames) as f32 / since.elapsed().as_secs_f32();
                self.done = true;
                if fps < SMOOTH_ENOUGH_FPS {
                    SMOOTH_UP_TO.fetch_max(at + 1, Ordering::Relaxed);
                    return true;
                }
            }
            _ => {}
        }
        false
    }
}
/// As smoothly as the camera goes.
const WANT_FPS: u32 = 30;
/// Fewer pictures a second than this at the wanted size, and a smaller
/// size that's smoother is taken instead.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
const SMOOTH_FPS: u32 = 25;
/// V4L2's "exposure, dynamic framerate": lets a camera slow down to
/// let in more light. Smooth video matters more in a call, so it's off
/// while swarm has the camera, and put back afterwards.
#[cfg(target_os = "linux")]
const EXPOSURE_DYNAMIC_FRAMERATE: u32 = 0x009a_0903;

/// A camera there is: what it's called (numbered, should two share a
/// name), and how it's opened (a device on Linux, its number on Windows).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    open: String,
}

/// The cameras there are, in the order they're tried, for choosing one.
/// Looking starts none of them. Kept for a few seconds: Settings asks
/// each time it's drawn.
pub fn found() -> Vec<Found> {
    static SEEN: std::sync::Mutex<Option<(Instant, Vec<Found>)>> = std::sync::Mutex::new(None);
    let Ok(mut seen) = SEEN.lock() else { return look_for_cameras() };
    match &*seen {
        Some((at, cameras)) if at.elapsed() < Duration::from_secs(3) => cameras.clone(),
        _ => {
            let cameras = look_for_cameras();
            *seen = Some((Instant::now(), cameras.clone()));
            cameras
        }
    }
}

#[cfg(target_os = "linux")]
fn look_for_cameras() -> Vec<Found> {
    let cameras = (0..64).filter_map(|n| {
        let path = format!("/dev/video{n}");
        let caps = Device::with_path(&path).ok()?.query_caps().ok()?;
        caps.capabilities.contains(v4l::capability::Flags::VIDEO_CAPTURE).then_some((caps.card, path))
    });
    numbered(cameras.collect())
}

#[cfg(windows)]
fn look_for_cameras() -> Vec<Found> {
    numbered(mf::names().into_iter().enumerate().map(|(i, name)| (name, i.to_string())).collect())
}

#[cfg(target_os = "android")]
fn look_for_cameras() -> Vec<Found> {
    numbered(android::cameras())
}

#[cfg(target_os = "macos")]
fn look_for_cameras() -> Vec<Found> {
    numbered(avfoundation::cameras())
}

#[cfg(not(any(target_os = "linux", windows, target_os = "android", target_os = "macos")))]
fn look_for_cameras() -> Vec<Found> {
    Vec::new()
}

/// Names and how each is opened, as `Found`: a name shared with one
/// before it gets a number ("C922 2").
#[cfg_attr(not(any(target_os = "linux", windows, target_os = "android", target_os = "macos")), allow(dead_code))]
fn numbered(cameras: Vec<(String, String)>) -> Vec<Found> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (name, open) in cameras {
        let alike = seen.iter().filter(|n| **n == name).count();
        out.push(Found { name: if alike == 0 { name.clone() } else { format!("{name} {}", alike + 1) }, open });
        seen.push(name);
    }
    out
}

/// The camera a QR code's read with: the first facing away (a phone's
/// back camera) — a call's is usually the one facing you. None: as a call.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn for_reading(found: &[Found]) -> Option<&Found> {
    found.iter().find(|f| f.name.starts_with("back camera"))
}

/// The camera chosen in Settings, by name: `None`, the first that starts.
pub fn chosen() -> Option<String> {
    swarm_core::settings::get(&crate::paths().settings(), "camera").filter(|name| !name.is_empty())
}

/// Why the camera chosen in Settings didn't start when last opened (the
/// first that would was used instead), for `swarm video-check`.
static CHOSEN_TROUBLE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn chosen_trouble() -> Option<String> {
    CHOSEN_TROUBLE.lock().ok()?.clone()
}

/// The chosen camera, if it started; why not noted otherwise.
#[cfg(any(target_os = "linux", windows, target_os = "android", target_os = "macos"))]
fn chosen_started(started: Result<Camera>) -> Option<Camera> {
    let trouble = started.as_ref().err().map(|e| format!("{e:#}"));
    if let Ok(mut noted) = CHOSEN_TROUBLE.lock() {
        *noted = trouble;
    }
    started.ok()
}

/// Chooses the camera (`None`: the first that starts), for each time one
/// is opened from now on.
pub fn choose(path: &std::path::Path, name: Option<&str>) -> std::io::Result<()> {
    swarm_core::settings::set(path, "camera", name.unwrap_or(""))
}

/// The next camera in `found` after the one chosen (after the last, the
/// first again) — for Settings in the terminal.
pub fn choose_next(path: &std::path::Path) -> std::io::Result<()> {
    let cameras = found();
    if cameras.is_empty() {
        return Ok(());
    }
    let at = chosen().and_then(|name| cameras.iter().position(|c| c.name == name));
    let next = at.map_or(if cameras.len() > 1 { 1 } else { 0 }, |i| (i + 1) % cameras.len());
    choose(path, Some(&cameras[next].name))
}

/// The cameras to choose from, as the window lists them, and which is
/// chosen: the one chosen, though not plugged in, is listed last, saying
/// so. (Choosing it again is `choose_at`.)
pub fn choices() -> (Vec<String>, usize) {
    let mut names: Vec<String> = found().into_iter().map(|c| c.name).collect();
    let at = match chosen() {
        Some(name) => names.iter().position(|n| *n == name).unwrap_or_else(|| {
            names.push(format!("{name} (not plugged in)"));
            names.len() - 1
        }),
        None => 0,
    };
    if names.is_empty() {
        names.push("none found".to_string());
    }
    (names, at)
}

/// The cameras there are, by the short names the phone's call offers
/// them under (Camera's list: Selfie, Back), in `choices`' order.
pub fn call_choices() -> Vec<String> {
    found().into_iter().map(|c| short_name(&c.name)).collect()
}

/// A phone's camera as a call's list names it ("back camera 2", Back 2);
/// any other by its own name.
fn short_name(name: &str) -> String {
    for (long, short) in [("front camera", "Selfie"), ("back camera", "Back"), ("plugged-in camera", "Plugged in")] {
        if let Some(rest) = name.strip_prefix(long) {
            return format!("{short}{rest}");
        }
    }
    name.to_string()
}

/// Chooses the camera at `index` in `choices` (one not plugged in stays
/// chosen).
pub fn choose_at(path: &std::path::Path, index: usize) -> std::io::Result<()> {
    match found().get(index) {
        Some(camera) => choose(path, Some(&camera.name)),
        None => Ok(()),
    }
}

/// The camera as Settings shows it: the one chosen (or used, when none
/// is), and how many there are to choose from.
pub fn choice_label() -> String {
    let cameras = found();
    let chosen = chosen();
    let using = chosen.clone().or_else(|| cameras.first().map(|c| c.name.clone()));
    match (using, cameras.len()) {
        (None, _) => "none found".to_string(),
        (Some(name), _) if chosen.is_some() && !cameras.iter().any(|c| c.name == name) => format!("{name} (not plugged in: the first there is meanwhile)"),
        (Some(name), 1) => format!("{name} (the only one)"),
        (Some(name), n) => format!("{name} ({n} to choose from)"),
    }
}

/// One frame, as 8-bit RGB, row by row — core's picture type, so a
/// frame can go straight to a video call.
pub use swarm_core::video::Picture as Frame;
pub use swarm_media::common::mjpeg_to_rgb;
pub use swarm_media::pattern::{test_background, test_pattern};

/// Which camera, and what it agreed to deliver.
#[derive(Clone)]
pub struct CameraInfo {
    pub device: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub format: String,
    /// It faces away from whoever's using it (a phone's back camera): its
    /// pictures are shown to them as it sees, not mirrored.
    pub faces_away: bool,
}

/// Frames captured, and when the first came: a camera can take a second
/// or more to start, and a rate counted from opening would take that as
/// slowness.
#[derive(Default)]
pub struct Captured {
    count: AtomicU64,
    first: OnceLock<Instant>,
    /// When the last few came, and the camera's number for the next
    /// (V4L2 numbers each picture the camera starts: one missing from
    /// the run never reached swarm), for `swarm video-check` to say how
    /// they come.
    recent: std::sync::Mutex<(std::collections::VecDeque<Instant>, Option<u32>)>,
    skipped: AtomicU64,
}

/// How many arrival times `Captured` keeps: some seconds' worth.
const RECENT: usize = 300;

impl Captured {
    pub fn add(&self) {
        self.add_numbered(None);
    }

    /// `add`, with the camera's number for it (see `recent`).
    pub fn add_numbered(&self, sequence: Option<u32>) {
        let now = Instant::now();
        self.first.get_or_init(|| now);
        self.count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut recent) = self.recent.lock() {
            let (times, next) = &mut *recent;
            if times.len() == RECENT {
                times.pop_front();
            }
            times.push_back(now);
            if let (Some(sequence), Some(expected)) = (sequence, *next) {
                self.skipped.fetch_add(sequence.wrapping_sub(expected).min(1000) as u64, Ordering::Relaxed);
            }
            *next = sequence.map(|s| s.wrapping_add(1));
        }
    }

    /// How pictures have come since `opened`: the first's wait, the rate
    /// over the last `over`, the longest gap in that time, and how many
    /// the camera numbered that never came.
    fn pace(&self, opened: Instant, over: Duration) -> String {
        let Ok(recent) = self.recent.lock() else { return String::new() };
        let Some(first) = self.first.get() else { return "no picture yet".to_string() };
        let now = Instant::now();
        let lately: Vec<Instant> = recent.0.iter().copied().filter(|t| now.duration_since(*t) <= over).collect();
        let longest = lately.windows(2).map(|w| w[1] - w[0]).max().unwrap_or_default();
        let rate = match (lately.first(), lately.last()) {
            (Some(a), Some(b)) if b > a => (lately.len() - 1) as f32 / (*b - *a).as_secs_f32(),
            _ => 0.0,
        };
        format!(
            "first picture {} ms after opening; over the last {} s, {rate:.1} a second, gaps of up to {} ms; {} lost on the way (by the camera's numbering)",
            first.duration_since(opened).as_millis(),
            over.as_secs(),
            longest.as_millis(),
            self.skipped.load(Ordering::Relaxed)
        )
    }

    fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Frames a second from the first to `now`.
    fn rate(&self, now: Instant) -> f32 {
        match self.first.get() {
            Some(first) => self.count().saturating_sub(1) as f32 / now.duration_since(*first).as_secs_f32().max(0.001),
            None => 0.0,
        }
    }
}

pub struct Camera {
    info: CameraInfo,
    asked: Size,
    opened: Instant,
    latest: watch::Sender<Option<Arc<Frame>>>,
    captured: Arc<Captured>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Camera {
    /// Opens the camera `SWARM_CAMERA` names, or the first that works,
    /// and starts capturing at `best_size`. Blocks briefly while the device
    /// starts.
    pub fn open() -> Result<Camera> {
        Camera::open_at(best_size())
    }

    /// `open`, but for reading a QR code: the back camera, if there is one
    /// (see `for_reading`) and it starts.
    pub fn open_for_reading() -> Result<Camera> {
        #[cfg(target_os = "android")]
        if std::env::var("SWARM_CAMERA").map_or(true, |s| s.is_empty()) {
            if let Some(Ok(camera)) = for_reading(&found()).map(|back| Camera::start_android(Some(back.open.clone()), best_size())) {
                return Ok(camera);
            }
        }
        Camera::open()
    }

    /// For `swarm video-check`: cameras use the graphics chip
    /// (`Some(true)`: NV12 on Windows, decoded on the chip; JPEGs decoded
    /// on the chip on Linux) or not (`Some(false)`), or (`None`) on the
    /// chip where it can again.
    pub fn insist_on_chip(chip: Option<bool>) {
        INSIST_ON_CHIP.store(chip.map_or(0, |on| if on { 2 } else { 1 }), std::sync::atomic::Ordering::Relaxed);
    }

    /// `open`, asking for `want` (the camera gives the nearest it can).
    pub fn open_at(want: Size) -> Result<Camera> {
        let choice = std::env::var("SWARM_CAMERA").ok().filter(|s| !s.is_empty());
        let in_settings = || chosen().and_then(|name| found().into_iter().find(|camera| camera.name == name));
        match choice.as_deref() {
            Some("test") => Ok(Camera::start_test_pattern(want, false)),
            Some("test-busy") => Ok(Camera::start_test_pattern(want, true)),
            Some(still) if still.starts_with("still:") => Camera::start_still(Path::new(&still["still:".len()..])),
            #[cfg(target_os = "linux")]
            Some(path) => Camera::start_device(path, want),
            #[cfg(windows)]
            Some(other) => Camera::start_mf(Some(other.to_string()), want),
            #[cfg(windows)]
            None => match in_settings().map(|camera| Camera::start_mf(Some(camera.open), want)).and_then(chosen_started) {
                Some(camera) => Ok(camera),
                None => Camera::start_mf(None, want),
            },
            #[cfg(target_os = "android")]
            Some(other) => Camera::start_android(Some(other.to_string()), want),
            #[cfg(target_os = "android")]
            None => match in_settings().map(|camera| Camera::start_android(Some(camera.open), want)).and_then(chosen_started) {
                Some(camera) => Ok(camera),
                None => Camera::start_android(None, want),
            },
            #[cfg(target_os = "macos")]
            Some(other) => Camera::start_avfoundation(Some(other.to_string()), want),
            #[cfg(target_os = "macos")]
            None => match in_settings().map(|camera| Camera::start_avfoundation(Some(camera.open), want)).and_then(chosen_started) {
                Some(camera) => Ok(camera),
                None => Camera::start_avfoundation(None, want),
            },
            #[cfg(not(any(target_os = "linux", windows, target_os = "android", target_os = "macos")))]
            _ => bail!("cameras aren't supported on this system yet — SWARM_CAMERA=test shows a test pattern"),
            #[cfg(target_os = "linux")]
            None => {
                if let Some(camera) = in_settings().map(|camera| Camera::start_device(&camera.open, want)).and_then(chosen_started) {
                    return Ok(camera);
                }
                let mut tried = Vec::new();
                for n in 0..64 {
                    let path = format!("/dev/video{n}");
                    if !std::path::Path::new(&path).exists() {
                        continue;
                    }
                    match Camera::start_device(&path, want) {
                        Ok(camera) => return Ok(camera),
                        Err(e) => tried.push(format!("{path}: {e:#}")),
                    }
                }
                if tried.is_empty() {
                    bail!("no camera found (no /dev/video* devices) — SWARM_CAMERA=test shows a test pattern");
                }
                bail!("no camera would start ({})", tried.join("; "))
            }
        }
    }

    /// The size it was asked for.
    pub fn asked(&self) -> Size {
        self.asked
    }

    pub fn info(&self) -> &CameraInfo {
        &self.info
    }

    /// Takes it for one facing away (tests).
    #[cfg(test)]
    pub fn face_away(&mut self) {
        self.info.faces_away = true;
    }

    /// It's stopped giving pictures of its own accord: taken off USB, or
    /// failed, so its thread ended without being asked to.
    pub fn ended(&self) -> bool {
        self.thread.as_ref().is_some_and(std::thread::JoinHandle::is_finished) && !self.stop.load(Ordering::Relaxed)
    }

    /// For a test: as if it were taken off USB (`ended` from now on).
    #[cfg(test)]
    pub fn unplug(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = &self.thread {
            while !thread.is_finished() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        self.stop.store(false, Ordering::Relaxed);
    }

    /// The newest frame captured, if any has been yet.
    pub fn latest(&self) -> Option<Arc<Frame>> {
        self.latest.borrow().clone()
    }

    /// Follows the newest frame as it changes — for a video call.
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<Frame>>> {
        self.latest.subscribe()
    }

    /// How many frames have been captured since opening.
    pub fn frames_captured(&self) -> u64 {
        self.captured.count()
    }

    /// How long since it was opened.
    pub fn running_for(&self) -> Duration {
        self.opened.elapsed()
    }

    /// How its pictures have come (see `Captured::pace`), over the last
    /// `over`.
    pub fn pace(&self, over: Duration) -> String {
        self.captured.pace(self.opened, over)
    }

    /// Frames a second it's actually giving, on average since the first
    /// (a camera can give fewer than it agreed to, in dim light say).
    pub fn frames_per_second(&self) -> f32 {
        self.captured.rate(Instant::now())
    }

    #[cfg(target_os = "linux")]
    fn start_device(path: &str, want: Size) -> Result<Camera> {
        let device = Device::with_path(path).with_context(|| format!("opening {path}"))?;
        let name = device.query_caps().map(|caps| caps.card).unwrap_or_else(|_| path.to_string());
        let modes = modes_of(&device)?;
        let Some(mode) = choose_mode(&modes, want) else {
            bail!("offers none of NV12, YUYV or MJPEG video");
        };
        let fourcc = FourCC::new(&mode.fourcc);
        let mut format = device.format().context("reading format")?;
        (format.width, format.height, format.fourcc) = (mode.width, mode.height, fourcc);
        device.set_format(&format).context("setting format")?;
        let _ = device.set_params(&v4l::video::capture::Parameters::with_fps(mode.fps.min(WANT_FPS)));
        let format = device.format().context("reading format")?;
        if format.fourcc != fourcc {
            bail!("wouldn't switch to {fourcc}");
        }
        let exposure_was = steady_frame_rate(&device);
        let info = CameraInfo {
            device: path.to_string(),
            name,
            width: format.width,
            height: format.height,
            format: fourcc.str().unwrap_or("?").to_string(),
            faces_away: false,
        };

        let latest = watch::Sender::new(None);
        let captured = Arc::new(Captured::default());
        let stop = Arc::new(AtomicBool::new(false));
        let mut stream = MmapStream::with_buffers(&device, Type::VideoCapture, 4).context("starting the stream")?;
        let mut chip = (fourcc == FourCC::new(b"MJPG") && chip_wanted()).then(crate::hardware::JpegOnChip::new).and_then(Result::ok);
        let thread = {
            let (latest, captured, stop) = (latest.clone(), captured.clone(), stop.clone());
            let (width, height, stride) = (format.width as usize, format.height as usize, format.stride as usize);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((buf, meta)) = stream.next() else { break };
                    let sequence = meta.sequence;
                    let frame = if fourcc == FourCC::new(b"YUYV") {
                        Some(yuyv_to_i420(buf, width, height, stride))
                    } else if fourcc == FourCC::new(b"NV12") {
                        let stride = stride.max(width);
                        crate::hardware::i420_from_nv12(buf, width, height, stride, stride * height)
                    } else if let Some(decoder) = chip.as_mut() {
                        match decoder.decode(buf) {
                            Ok(picture) => Some(picture),
                            Err(e) => {
                                let frame = mjpeg_to_rgb(buf);
                                if frame.is_some() {
                                    swarm_core::video::note_hardware_trouble(format!("{} failed with the camera's JPEG, so it's decoded in software: {e:#}", decoder.name()));
                                    chip = None;
                                }
                                frame
                            }
                        }
                    } else {
                        mjpeg_to_rgb(buf)
                    };
                    if let Some(frame) = frame {
                        latest.send_replace(Some(Arc::new(frame)));
                        captured.add_numbered(Some(sequence));
                    }
                }
                drop(stream);
                if let Some(was) = exposure_was {
                    let _ = device.set_control(v4l::control::Control { id: EXPOSURE_DYNAMIC_FRAMERATE, value: was });
                }
            })
        };
        Ok(Camera { info, asked: want, opened: Instant::now(), latest, captured, stop, thread: Some(thread) })
    }

    #[cfg(windows)]
    fn start_mf(choice: Option<String>, want: Size) -> Result<Camera> {
        let latest = watch::Sender::new(None);
        let captured = Arc::new(Captured::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (started, thread) = mf::start(choice, want, latest.clone(), captured.clone(), stop.clone())?;
        let info = CameraInfo { device: started.device, name: started.name, width: started.width, height: started.height, format: started.format, faces_away: false };
        Ok(Camera { info, asked: want, opened: Instant::now(), latest, captured, stop, thread: Some(thread) })
    }

    #[cfg(target_os = "android")]
    fn start_android(choice: Option<String>, want: Size) -> Result<Camera> {
        let latest = watch::Sender::new(None);
        let captured = Arc::new(Captured::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (started, thread) = android::start(choice, want, latest.clone(), captured.clone(), stop.clone())?;
        let info = CameraInfo { device: started.device, name: started.name, width: started.width, height: started.height, format: "YUV_420_888".into(), faces_away: started.faces_away };
        Ok(Camera { info, asked: want, opened: Instant::now(), latest, captured, stop, thread: Some(thread) })
    }

    #[cfg(target_os = "macos")]
    fn start_avfoundation(choice: Option<String>, want: Size) -> Result<Camera> {
        let latest = watch::Sender::new(None);
        let captured = Arc::new(Captured::default());
        let stop = Arc::new(AtomicBool::new(false));
        let opened = Instant::now();
        let (info, thread) = avfoundation::start(choice, want, latest.clone(), captured.clone(), stop.clone())?;
        Ok(Camera { info, asked: want, opened, latest, captured, stop, thread: Some(thread) })
    }

    /// The test pattern; `busy`, with grain all over, so it takes all the
    /// bitrate it's given, as a real camera's pictures do.
    fn start_test_pattern(want: Size, busy: bool) -> Camera {
        let info = CameraInfo { device: "test".into(), name: "test pattern".into(), width: want.0, height: want.1, format: "RGB".into(), faces_away: false };
        let background = test_background(want);
        Camera::start_made(info, want, move |seconds| if busy { swarm_media::pattern::test_pattern_busy(seconds, &background) } else { test_pattern(seconds, &background) })
    }

    /// A picture file, shown as a camera would, over and over.
    fn start_still(path: &Path) -> Result<Camera> {
        let rgb = anyhow::Context::with_context(image::ImageReader::open(path), || format!("opening {}", path.display()))?.with_guessed_format()?.decode()?.to_rgb8();
        let (width, height) = (rgb.width() as usize, rgb.height() as usize);
        let picture = Arc::new(swarm_core::video::Picture::rgb(width, height, rgb.into_raw()));
        let info = CameraInfo { device: path.display().to_string(), name: "a still picture".into(), width: width as u32, height: height as u32, format: "RGB".into(), faces_away: false };
        Ok(Camera::start_made(info, (width as u32, height as u32), move |_| (*picture).clone()))
    }

    /// A camera of made pictures: `make` given the seconds since it started.
    fn start_made(info: CameraInfo, want: Size, make: impl Fn(f32) -> swarm_core::video::Picture + Send + 'static) -> Camera {
        let latest = watch::Sender::new(None);
        let captured = Arc::new(Captured::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (latest, captured, stop) = (latest.clone(), captured.clone(), stop.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let start = Instant::now();
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    latest.send_replace(Some(Arc::new(make(start.elapsed().as_secs_f32()))));
                    captured.add();
                    n += 1;
                    let due = start + Duration::from_secs_f64(n as f64 / WANT_FPS as f64);
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                }
            })
        };
        Camera { info, asked: want, opened: Instant::now(), latest, captured, stop, thread: Some(thread) }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Asks `device` for the wanted size and rate as `fourcc`; returns the
/// width and the pictures a second it agreed to (the nearest it has).
/// A size a camera offers in a format (`NV12`, `YUYV`, `MJPG`), and the
/// most pictures a second it gives at it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mode {
    fourcc: [u8; 4],
    width: u32,
    height: u32,
    fps: u32,
}

/// The formats swarm reads, those that come as the camera saw them first:
/// NV12 and YUYV have no JPEG's blocks and nothing to decode.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const FORMATS: [[u8; 4]; 3] = [*b"NV12", *b"YUYV", *b"MJPG"];

/// Everything the camera offers in `FORMATS`, as its lists say
/// (`VIDIOC_ENUM_FMT`, `_FRAMESIZES`, `_FRAMEINTERVALS`): what
/// `v4l2-ctl --list-formats-ext` shows. Reading them changes nothing.
#[cfg(target_os = "linux")]
fn modes_of(device: &Device) -> Result<Vec<Mode>> {
    use v4l::framesize::FrameSizeEnum;
    let offered: Vec<FourCC> = device.enum_formats().context("listing formats")?.into_iter().map(|f| f.fourcc).collect();
    let mut modes = Vec::new();
    for fourcc in FORMATS.iter().map(FourCC::new).filter(|f| offered.contains(f)) {
        for size in device.enum_framesizes(fourcc).unwrap_or_default() {
            let sizes: Vec<(u32, u32)> = match size.size {
                FrameSizeEnum::Discrete(d) => vec![(d.width, d.height)],
                FrameSizeEnum::Stepwise(r) => SIZES
                    .iter()
                    .copied()
                    .filter(|&(w, h)| (r.min_width..=r.max_width).contains(&w) && (r.min_height..=r.max_height).contains(&h))
                    .filter(|&(w, h)| (w - r.min_width) % r.step_width.max(1) == 0 && (h - r.min_height) % r.step_height.max(1) == 0)
                    .collect(),
            };
            for (width, height) in sizes {
                let fps = device.enum_frameintervals(fourcc, width, height).unwrap_or_default().into_iter().map(|i| most_fps(&i.interval)).max().unwrap_or(0);
                modes.push(Mode { fourcc: fourcc.repr, width, height, fps });
            }
        }
    }
    Ok(modes)
}

/// The most pictures a second an interval (or a range of them) allows.
#[cfg(target_os = "linux")]
fn most_fps(interval: &v4l::frameinterval::FrameIntervalEnum) -> u32 {
    let fps = |f: &v4l::Fraction| f.denominator / f.numerator.max(1);
    match interval {
        v4l::frameinterval::FrameIntervalEnum::Discrete(f) => fps(f),
        v4l::frameinterval::FrameIntervalEnum::Stepwise(s) => fps(&s.min),
    }
}

/// The mode to open the camera in, wanting `want`: as near that size as
/// it goes, no bigger (none smaller: the smallest bigger) and smoothly
/// (`SMOOTH_FPS`) if it can; then the closest in shape; then the
/// smoothest, to `WANT_FPS`; then the first of `FORMATS`. An MX Brio:
/// 1440p as NV12, 4K only as MJPEG, both at 30.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn choose_mode(modes: &[Mode], want: Size) -> Option<Mode> {
    let fits = |m: &Mode| m.width <= want.0 && m.height <= want.1;
    let any_fit = modes.iter().any(fits);
    let format_at = |m: &Mode| FORMATS.iter().position(|f| *f == m.fourcc).unwrap_or(FORMATS.len());
    modes.iter().copied().filter(|m| !any_fit || fits(m)).max_by_key(|m| {
        let near = if any_fit { m.width as i64 } else { -(m.width as i64) };
        let off_shape = (m.width as i64 * want.1 as i64 - m.height as i64 * want.0 as i64).abs();
        (near, m.fps >= SMOOTH_FPS, std::cmp::Reverse(off_shape), m.fps.min(WANT_FPS), std::cmp::Reverse(format_at(m)))
    })
}

/// Stops the camera slowing down for more light (see
/// `EXPOSURE_DYNAMIC_FRAMERATE`); returns what it was, to put back.
#[cfg(target_os = "linux")]
fn steady_frame_rate(device: &Device) -> Option<v4l::control::Value> {
    use v4l::control::{Control, Value};
    let was = device.control(EXPOSURE_DYNAMIC_FRAMERATE).ok()?.value;
    if matches!(was, Value::Boolean(false) | Value::Integer(0)) {
        return None;
    }
    let off = if matches!(was, Value::Boolean(_)) { Value::Boolean(false) } else { Value::Integer(0) };
    device.set_control(Control { id: EXPOSURE_DYNAMIC_FRAMERATE, value: off }).ok()?;
    Some(was)
}

/// See `Camera::insist_on_chip`: 0 the chip where it can, 1 not, 2 the chip.
static INSIST_ON_CHIP: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(any(target_os = "linux", windows))]
fn chip_wanted() -> bool {
    match INSIST_ON_CHIP.load(std::sync::atomic::Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => true,
    }
}

/// A frame much bigger than wanted, shrunk by a whole factor (each pixel
/// the average of a square of them) — there's no use for more than about
/// twice the wanted size, and smaller frames are quicker everywhere after.
#[cfg_attr(not(windows), allow(dead_code))]
fn shrink(frame: Frame, want: Size) -> Frame {
    let factor = (frame.width / (want.0 as usize * 2)).min(frame.height / (want.1 as usize * 2)) * 2;
    if factor < 2 {
        return frame;
    }
    frame.scaled(frame.width / factor, frame.height / factor)
}

/// YUYV (4:2:2: two pixels share one U and one V) to I420, which has
/// half as many rows of U and V: each the average of two. Cameras give
/// YUYV in the same limited range H.264 uses, so nothing else changes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn yuyv_to_i420(buf: &[u8], width: usize, height: usize, stride: usize) -> Frame {
    let stride = if stride >= width * 2 { stride } else { width * 2 };
    let (width, height) = (width & !1, (height.min(buf.len() / stride.max(1))) & !1);
    let half = width / 2;
    let mut data = vec![0u8; width * height + 2 * half * (height / 2)];
    let (luma, chroma) = data.split_at_mut(width * height);
    let (u, v) = chroma.split_at_mut(half * (height / 2));
    for y in (0..height).step_by(2) {
        let (top, bottom) = (&buf[y * stride..y * stride + width * 2], &buf[(y + 1) * stride..(y + 1) * stride + width * 2]);
        for (row, dest) in [(top, y), (bottom, y + 1)] {
            for (out, pair) in luma[dest * width..(dest + 1) * width].iter_mut().zip(row.iter().step_by(2)) {
                *out = *pair;
            }
        }
        let at = (y / 2) * half;
        for (x, (a, b)) in top.as_chunks::<4>().0.iter().zip(bottom.as_chunks::<4>().0).enumerate() {
            u[at + x] = (a[1] as u16 + b[1] as u16).div_ceil(2) as u8;
            v[at + x] = (a[3] as u16 + b[3] as u16).div_ceil(2) as u8;
        }
    }
    Frame::i420(width, height, data)
}

/// How far the phone's screen is turned (see `turn_for`), as the app
/// last said.
static DISPLAY_ROTATION: AtomicU32 = AtomicU32::new(0);

/// The phone's screen has turned to `degrees` (0, 90, 180 or 270), as
/// Android's `Display.getRotation` says: its camera's pictures are
/// turned to match from the next.
pub fn set_display_rotation(degrees: u32) {
    DISPLAY_ROTATION.store(degrees % 360, Ordering::Relaxed);
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn display_rotation() -> u32 {
    DISPLAY_ROTATION.load(Ordering::Relaxed)
}

/// One plane of a camera's picture as Android gives it (YUV_420_888):
/// its bytes, how far one row starts from the next, and one pixel from
/// the next (2 where U and V are interleaved).
#[derive(Clone, Copy)]
pub struct Plane<'a> {
    pub data: &'a [u8],
    pub row: usize,
    pub pixel: usize,
}

/// How far to turn a phone camera's pictures (clockwise, in degrees) to
/// have them the right way up: its sensor's mounted turned
/// `sensor` degrees, and the screen's turned `display` (Android's
/// `Display.getRotation`: the phone turned that far anticlockwise). A
/// front camera faces the other way, so the phone's turn counts the
/// other way round for it.
pub fn turn_for(sensor: u32, front: bool, display: u32) -> u32 {
    if front { (sensor + display) % 360 } else { (sensor + 360 - display % 360) % 360 }
}

/// The biggest picture a phone's camera is asked for: each is turned
/// and shown on the phone as well as encoded, and 1080p's already plenty
/// of work for it.
const MOST_WIDE: u32 = 1920;
/// The steadiest rate a phone's camera's asked for, at most this.
const MOST_FPS: i32 = 30;

/// The size to ask for: the smallest 16:9 at least as wide as `want`
/// (up to `MOST_WIDE`), else the widest 16:9 below it; any shape if it
/// gives no 16:9.
pub fn yuv_size_for(sizes: &[Size], want: Size) -> Option<Size> {
    let wide = want.0.min(MOST_WIDE);
    let sixteen_nine: Vec<Size> = sizes.iter().copied().filter(|(w, h)| w * 9 == h * 16 && *w <= MOST_WIDE).collect();
    let pool = if sixteen_nine.is_empty() { sizes.iter().copied().filter(|(w, _)| *w <= MOST_WIDE).collect() } else { sixteen_nine };
    pool.iter().copied().filter(|(w, _)| *w >= wide).min_by_key(|(w, _)| *w).or_else(|| pool.iter().copied().max_by_key(|(w, _)| *w))
}

/// The steadiest rate up to `MOST_FPS`: the highest top, then the
/// highest bottom (30 to 30 before 15 to 30).
pub fn rate_for(rates: &[(i32, i32)]) -> Option<(i32, i32)> {
    rates.iter().copied().filter(|(_, top)| *top <= MOST_FPS).max_by_key(|(bottom, top)| (*top, *bottom))
}

/// A YUV_420_888 picture `width`×`height` (Y, U and V planes), turned
/// `turn` degrees clockwise, as I420, in `memory` (whatever's in it is
/// written over: see `Recycled`).
pub fn yuv420_turned(planes: [Plane; 3], width: usize, height: usize, turn: u32, memory: Vec<u8>) -> Frame {
    let on_end = turn % 180 == 90;
    let (out_w, out_h) = if on_end { (height, width) } else { (width, height) };
    let (cw, ch) = (width / 2, height / 2);
    let mut data = memory;
    data.resize(out_w * out_h + 2 * cw * ch, 0);
    let (luma, chroma) = data.split_at_mut(out_w * out_h);
    let (u, v) = chroma.split_at_mut(cw * ch);
    turn_plane(planes[0], width, height, turn, luma);
    turn_plane(planes[1], cw, ch, turn, u);
    turn_plane(planes[2], cw, ch, turn, v);
    Frame::i420(out_w, out_h, data)
}

/// One plane `w`×`h` turned `turn` degrees clockwise into `out`: rows
/// copied as they are where they can be; otherwise in tiles, so what's
/// read and what's written both stay in the cache (a pixel at a time
/// across the whole picture, turning a quarter read a row apart each
/// time: a phone's 1080p took 6 ms on a fast core).
#[allow(clippy::needless_range_loop)]
fn turn_plane(plane: Plane, w: usize, h: usize, turn: u32, out: &mut [u8]) {
    const TILE: usize = 32;
    let (row, pixel, data) = (plane.row, plane.pixel, plane.data);
    if turn == 0 && pixel == 1 {
        for (y, to) in out.chunks_exact_mut(w).enumerate() {
            to.copy_from_slice(&data[y * row..][..w]);
        }
        return;
    }
    if turn == 0 || turn == 180 {
        for (y, to) in out.chunks_exact_mut(w).enumerate() {
            let from = &data[(if turn == 0 { y } else { h - 1 - y }) * row..];
            if turn == 0 {
                to.iter_mut().zip(from.iter().step_by(pixel)).for_each(|(to, from)| *to = *from);
            } else {
                to.iter_mut().rev().zip(from.iter().step_by(pixel)).for_each(|(to, from)| *to = *from);
            }
        }
        return;
    }
    for ty in (0..w).step_by(TILE) {
        for tx in (0..h).step_by(TILE) {
            for y in ty..(ty + TILE).min(w) {
                let to = &mut out[y * h..][..h];
                for x in tx..(tx + TILE).min(h) {
                    let (sx, sy) = if turn == 90 { (y, h - 1 - x) } else { (w - 1 - y, x) };
                    to[x] = data[sy * row + sx * pixel];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phone's cameras by the names a call's list gives them; others
    /// by their own.
    #[test]
    fn a_phones_cameras_have_short_names() {
        let names = ["front camera", "back camera", "back camera 2", "plugged-in camera", "C922 Pro Stream Webcam"].map(short_name);
        assert_eq!(names, ["Selfie", "Back", "Back 2", "Plugged in", "C922 Pro Stream Webcam"]);
    }

    /// A picture turned each way, whatever its planes' strides: every
    /// pixel lands where turning it puts it, as I420.
    #[test]
    fn phone_pictures_are_turned_upright() {
        let y = [0, 1, 2, 3, 99, 99, 4, 5, 6, 7, 99, 99];
        let uv = [10, 20, 11, 21, 99];
        let planes = [Plane { data: &y, row: 6, pixel: 1 }, Plane { data: &uv, row: 8, pixel: 2 }, Plane { data: &uv[1..], row: 8, pixel: 2 }];
        let at = |turn| {
            let p = yuv420_turned(planes, 4, 2, turn, Vec::new());
            for used in [vec![55; 40], vec![66; 3]] {
                let (at, room) = (used.as_ptr(), used.capacity());
                let again = yuv420_turned(planes, 4, 2, turn, used);
                assert_eq!(again, p, "turned {turn} into used memory");
                if room >= again.data.len() {
                    assert_eq!(again.data.as_ptr(), at, "turned {turn}: in the memory given");
                }
            }
            let (luma, u, v) = p.planes();
            (p.width, p.height, luma.to_vec(), u.to_vec(), v.to_vec())
        };
        assert_eq!(at(0), (4, 2, vec![0, 1, 2, 3, 4, 5, 6, 7], vec![10, 11], vec![20, 21]));
        assert_eq!(at(90), (2, 4, vec![4, 0, 5, 1, 6, 2, 7, 3], vec![10, 11], vec![20, 21]));
        assert_eq!(at(180), (4, 2, vec![7, 6, 5, 4, 3, 2, 1, 0], vec![11, 10], vec![21, 20]));
        assert_eq!(at(270), (2, 4, vec![3, 7, 2, 6, 1, 5, 0, 4], vec![11, 10], vec![21, 20]));
    }

    #[test]
    fn phone_camera_sizes_and_rates_chosen() {
        let sizes = [(4032, 3024), (3840, 2160), (1920, 1080), (1440, 1080), (1280, 720), (640, 480)];
        assert_eq!(yuv_size_for(&sizes, (3840, 2160)), Some((1920, 1080)), "no bigger than 1080p");
        assert_eq!(yuv_size_for(&sizes, (1280, 720)), Some((1280, 720)));
        assert_eq!(yuv_size_for(&sizes, (960, 540)), Some((1280, 720)));
        assert_eq!(yuv_size_for(&[(1440, 1080), (640, 480)], (1280, 720)), Some((1440, 1080)), "no 16:9: any shape");
        assert_eq!(rate_for(&[(15, 30), (30, 30), (7, 60), (24, 24)]), Some((30, 30)));
    }

    /// How long turning a phone camera's 1080p picture takes here, each
    /// way: for a phone, run there (`--ignored`).
    #[test]
    #[ignore]
    fn turning_timed() {
        let (w, h) = (1920, 1080);
        let y: Vec<u8> = (0..w * h).map(|i| (i % 251) as u8).collect();
        let uv: Vec<u8> = (0..w * h / 2).map(|i| (i % 241) as u8).collect();
        let planes = [Plane { data: &y, row: w, pixel: 1 }, Plane { data: &uv, row: w, pixel: 2 }, Plane { data: &uv[1..], row: w, pixel: 2 }];
        for turn in [0, 90, 180, 270] {
            let started = Instant::now();
            for _ in 0..20 {
                std::hint::black_box(yuv420_turned(planes, w, h, turn, Vec::new()));
            }
            eprintln!("turned {turn}: {:.1} ms", started.elapsed().as_secs_f32() * 1000.0 / 20.0);
        }
    }

    /// A phone held upright: its back camera (sensor at 90°, as most are)
    /// turned a quarter clockwise, its front (270°) three quarters. Held
    /// on its side, anticlockwise (the screen's turned 90°), a quarter
    /// less for the back, a quarter more (all the way round) for the front.
    #[test]
    fn turned_as_the_phone_is_held() {
        assert_eq!(turn_for(90, false, 0), 90);
        assert_eq!(turn_for(270, true, 0), 270);
        assert_eq!(turn_for(90, false, 90), 0);
        assert_eq!(turn_for(270, true, 90), 0);
        assert_eq!(turn_for(90, false, 270), 180);
        assert_eq!(turn_for(270, true, 270), 180);
    }

    #[test]
    fn a_slow_start_isnt_counted_as_slowness() {
        let captured = Captured::default();
        let first = Instant::now();
        captured.first.set(first).unwrap();
        captured.count.store(61, Ordering::Relaxed);
        let rate = captured.rate(first + Duration::from_secs(2));
        assert!((rate - 30.0).abs() < 0.01, "{rate}");
        assert_eq!(Captured::default().rate(first), 0.0);
    }

    #[test]
    fn pictures_missing_from_the_cameras_numbering_are_counted() {
        let captured = Captured::default();
        let opened = Instant::now();
        for sequence in [7, 8, 9, 12, 13, 15] {
            captured.add_numbered(Some(sequence));
        }
        assert_eq!(captured.skipped.load(Ordering::Relaxed), 3);
        let said = captured.pace(opened, Duration::from_secs(3));
        assert!(said.ends_with("3 lost on the way (by the camera's numbering)"), "{said}");
        let captured = Captured::default();
        captured.add();
        captured.add();
        assert_eq!(captured.skipped.load(Ordering::Relaxed), 0);
    }

    /// Much as an MX Brio lists its modes: uncompressed to 1440p (YUYV
    /// slow above 720p), MJPEG to 4096×2160 and at 60 to 1080p.
    fn brio() -> Vec<Mode> {
        let mut modes = Vec::new();
        for (w, h) in [(640, 480), (1280, 720), (1920, 1080), (2560, 1440)] {
            modes.push(Mode { fourcc: *b"NV12", width: w, height: h, fps: 30 });
            modes.push(Mode { fourcc: *b"YUYV", width: w, height: h, fps: if w <= 1280 { 30 } else { 5 } });
        }
        for (w, h, fps) in [(640, 480, 60), (1280, 720, 60), (1920, 1080, 60), (2560, 1440, 30), (3840, 2160, 30), (4096, 2160, 30)] {
            modes.push(Mode { fourcc: *b"MJPG", width: w, height: h, fps });
        }
        modes
    }

    #[test]
    fn the_mode_is_chosen_from_what_the_camera_lists() {
        let chosen = |modes: &[Mode], want| choose_mode(modes, want).map(|m| (std::str::from_utf8(&m.fourcc).unwrap().to_string(), m.width, m.height, m.fps));
        let mode = |f: &str, w, h, fps| Some((f.to_string(), w, h, fps));
        assert_eq!(chosen(&brio(), (3840, 2160)), mode("MJPG", 3840, 2160, 30), "not 4096 wide");
        assert_eq!(chosen(&brio(), (2560, 1440)), mode("NV12", 2560, 1440, 30));
        assert_eq!(chosen(&brio(), (1280, 720)), mode("NV12", 1280, 720, 30));
        let no_nv12: Vec<Mode> = brio().into_iter().filter(|m| m.fourcc != *b"NV12").collect();
        assert_eq!(chosen(&no_nv12, (1280, 720)), mode("YUYV", 1280, 720, 30));
        assert_eq!(chosen(&no_nv12, (1920, 1080)), mode("MJPG", 1920, 1080, 60));
        let fhd = [Mode { fourcc: *b"MJPG", width: 1920, height: 1080, fps: 30 }, Mode { fourcc: *b"MJPG", width: 1440, height: 1080, fps: 30 }, Mode { fourcc: *b"MJPG", width: 640, height: 480, fps: 30 }];
        assert_eq!(chosen(&fhd, (3840, 2160)), mode("MJPG", 1920, 1080, 30));
        let four_three = [Mode { fourcc: *b"YUYV", width: 1280, height: 960, fps: 30 }, Mode { fourcc: *b"YUYV", width: 1280, height: 720, fps: 30 }];
        assert_eq!(chosen(&four_three, (1280, 720)), mode("YUYV", 1280, 720, 30));
        let big = [Mode { fourcc: *b"MJPG", width: 3840, height: 2160, fps: 30 }, Mode { fourcc: *b"MJPG", width: 1920, height: 1080, fps: 30 }];
        assert_eq!(chosen(&big, (1280, 720)), mode("MJPG", 1920, 1080, 30));
        assert_eq!(chosen(&[], (1280, 720)), None);
    }

    #[test]
    fn yuyv_grey_and_colour_convert_sensibly() {
        let buf = [128, 128, 128, 128, 81, 90, 81, 240];
        let frame = yuyv_to_i420(&[buf, buf].concat(), 4, 2, 8);
        assert_eq!(frame.width * frame.height, 8);
        let grey = frame.pixel(0, 0);
        assert!(grey.iter().all(|&c| (120..=140).contains(&c)), "{grey:?}");
        let red = frame.pixel(2, 0);
        assert!(red[0] > 200 && red[1] < 60 && red[2] < 60, "{red:?}");
    }

    #[test]
    fn cameras_sharing_a_name_are_numbered() {
        let found = numbered(vec![("C922".into(), "/dev/video0".into()), ("MX Brio".into(), "/dev/video2".into()), ("C922".into(), "/dev/video4".into())]);
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["C922", "MX Brio", "C922 2"]);
        assert_eq!(found[2].open, "/dev/video4");
    }

    /// A code's read with the camera facing away (a phone's back one),
    /// whichever's chosen for calls; with none, as a call would.
    #[test]
    fn a_code_is_read_with_the_back_camera() {
        let phone = numbered(vec![("front camera".into(), "1".into()), ("back camera".into(), "0".into()), ("back camera".into(), "2".into())]);
        assert_eq!(for_reading(&phone).map(|f| f.open.as_str()), Some("0"));
        let computer = numbered(vec![("C922".into(), "/dev/video0".into())]);
        assert!(for_reading(&computer).is_none());
    }

    /// The sizes a camera's opened at are those a call sends, 720p up.
    #[test]
    fn camera_sizes_are_the_calls_own() {
        let calls: Vec<Size> = swarm_core::video::Shape::all().filter(|s| s.width >= 1280).map(|s| (s.width as u32, s.height as u32)).collect();
        assert_eq!(calls, SIZES);
        assert_eq!(size_for(1920), (1920, 1080));
        assert_eq!(size_for(2000), (2560, 1440));
        assert_eq!(size_for(640), (1280, 720));
        assert_eq!(rung(1920), 2);
        assert_eq!(rung(3840), 0);
        assert_eq!(rung(640), 3);
    }

    #[test]
    fn big_frames_shrink_by_averaging_and_small_ones_are_left() {
        let small = Frame::rgb(2560, 1440, vec![9; 2560 * 1440 * 3]);
        assert_eq!(shrink(small, (1920, 1080)).width, 2560);
        let mut rgb = vec![0u8; 3840 * 2160 * 3];
        rgb[0] = 200;
        let big = shrink(Frame::rgb(3840, 2160, rgb), (1920, 1080));
        assert_eq!((big.width, big.height), (1920, 1080));
        assert_eq!(big.pixel(0, 0), [50, 0, 0]);
    }

}
