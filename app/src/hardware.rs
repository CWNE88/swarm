//! Video on the graphics chip (see `swarm_core::video::Hardware`),
//! always used where it can be (not a setting): set up when a call
//! starts, and checked by `swarm
//! video-check`. Windows has it through Media Foundation (`video_mf`),
//! Linux through NVENC and NVDEC (`video_nv`, NVIDIA's chips) or VA-API
//! (`video_va`, Intel's and AMD's), Android through MediaCodec
//! (`video_android`, encoding only); elsewhere, so far, video is
//! OpenH264's.

use swarm_core::lock::Locked;
use std::time::{Duration, Instant};
use swarm_core::video::{Picture, VideoDecoder, VideoEncoder, VideoPiece};
pub use swarm_media::common::*;

/// Why there's no hardware, when there isn't.
static NONE_BECAUSE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Gives video the hardware this system has (once), and the video size
/// Settings has.
pub fn init() {
    static GIVEN: std::sync::Once = std::sync::Once::new();
    GIVEN.call_once(|| {
        #[cfg(windows)]
        swarm_core::video::provide_hardware(Box::new(swarm_media::video_mf::MediaFoundation));
        #[cfg(target_os = "android")]
        swarm_core::video::provide_hardware(Box::new(swarm_media::video_android::MediaCodec));
        #[cfg(target_os = "linux")]
        match swarm_media::video_nv::Nvidia::find() {
            Ok(nvidia) => swarm_core::video::provide_hardware(Box::new(nvidia)),
            Err(nvidia) => match swarm_media::video_va::VaApi::find() {
                Ok(va) => swarm_core::video::provide_hardware(Box::new(va)),
                Err(e) => *NONE_BECAUSE.locked() = format!("NVIDIA: {nvidia:#}; VA-API: {e:#}"),
            },
        }
    });
    swarm_core::video::set_video_width(crate::prefs::video_width(&crate::paths().settings()));
}

/// What `swarm video-check` reports, line by line, as it goes: whether
/// this computer's graphics chip encodes and decodes video for calls, how
/// quickly against OpenH264, whether what it encodes comes out right in
/// OpenH264 (as every other swarm decodes it) and OpenH264's in it, and
/// what the camera gives. Changes nothing: the chip's put back in use.
pub fn check(say: &mut dyn FnMut(String)) {
    init();
    if !swarm_core::video::hardware_provided() {
        let why = NONE_BECAUSE.locked().clone();
        let why = if why.is_empty() { "Windows and Linux only, so far".to_string() } else { why };
        say(format!("Video hardware: none that Swarm can use on this system ({why}) — video is OpenH264's."));
    } else {
        say(format!("Video hardware: {}.", swarm_core::video::hardware_name().unwrap_or_default()));
        swarm_core::video::set_hardware(true);
        let h265 = match (swarm_core::video::encodes_h265(), swarm_core::video::decodes_h265()) {
            (true, true) => "the chip encodes and decodes it, so it's sent to others whose chips decode it",
            (true, false) => "the chip encodes it but doesn't decode it, so H.264 goes both ways",
            (false, true) => "the chip decodes it but doesn't encode it, so it only comes from others",
            (false, false) => "the chip neither encodes nor decodes it: H.264 only",
        };
        say(format!("H.265: {h265}."));
        let mut verdicts = Vec::new();
        for (width, height, bitrate) in [(960, 540, 1_000_000), (1280, 720, 2_000_000), (1920, 1080, 4_000_000), (3840, 2160, 12_000_000)] {
            say(String::new());
            say(format!("{width}×{height} at {} kbit/s, 3 s of a moving test picture:", bitrate / 1000));
            verdicts.push((format!("{width}×{height}"), check_size(width, height, bitrate, say)));
        }
        swarm_core::video::set_hardware(true);
        say(String::new());
        let summary = |part: fn(&Verdict) -> Option<bool>| {
            let at = |want: bool| verdicts.iter().filter(|(_, v)| part(v) == Some(want)).map(|(size, _)| size.as_str()).collect::<Vec<_>>().join(", ");
            match (at(true), at(false)) {
                (yes, no) if yes.is_empty() && no.is_empty() => "none to try".to_string(),
                (yes, no) if no.is_empty() => format!("works ({yes})"),
                (yes, no) if yes.is_empty() => format!("doesn't work ({no}) — OpenH264 is used"),
                (yes, no) => format!("works at {yes}; not at {no}, where OpenH264 is used"),
            }
        };
        say(format!("On the graphics chip: encoding {}; decoding {}.", summary(|v| v.encoding), summary(|v| v.decoding)));
    }
    say(String::new());
    say("A pretend 720p call — the camera's pictures encoded, decoded and made ready to show, as a call does both ways — for 8 s:".to_string());
    let ways: &[(bool, &str)] = if swarm_core::video::hardware_provided() { &[(true, "on the chip"), (false, "in software")] } else { &[(false, "in software")] };
    for &(hardware, how) in ways {
        match pretend_call(hardware, Duration::from_secs(8)) {
            Ok(line) => say(format!("  {how}: {line}")),
            Err(e) => say(format!("  {how}: {e:#}")),
        }
    }
    swarm_core::video::set_hardware(true);
    say(String::new());
    check_camera(say);
}

/// The processor time this process has had, all threads.
fn cpu_time() -> Option<Duration> {
    #[cfg(unix)]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return None;
        }
        let micros = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
        Some(Duration::from_micros(micros(usage.ru_utime) + micros(usage.ru_stime)))
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let (mut created, mut ended, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut ended, &mut kernel, &mut user) }.ok()?;
        let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
        Some(Duration::from_nanos((ticks(kernel) + ticks(user)) * 100))
    }
    #[cfg(not(any(unix, windows)))]
    None
}

/// A call's video work for `time`, on the chip or not: each of the
/// camera's pictures (or the test pattern's, with no camera) encoded at
/// 720p, decoded, and turned into RGB as the window does — what one side
/// of a call does both ways. Says how much of one core it took, and how
/// many pictures a second.
fn pretend_call(hardware: bool, time: Duration) -> anyhow::Result<String> {
    swarm_core::video::set_hardware(hardware);
    crate::camera::Camera::insist_on_chip(Some(hardware));
    let camera = crate::camera::Camera::open_at((1280, 720));
    crate::camera::Camera::insist_on_chip(None);
    let (camera, from) = match camera {
        Ok(camera) => {
            let name = camera.info().name.clone();
            (Some(camera), if name == "test pattern" { "the test pattern".to_string() } else { name })
        }
        Err(_) => (None, "the test pattern".to_string()),
    };
    let background = swarm_media::pattern::test_background((1280, 720));
    let mut encoder = VideoEncoder::new(2_000_000)?;
    encoder.keep_shape();
    let mut decoder = VideoDecoder::new()?;
    std::thread::sleep(Duration::from_millis(500));
    let (started, cpu_before) = (Instant::now(), cpu_time());
    let (mut last, mut pictures, mut shown) = (None, 0u32, 0u32);
    while started.elapsed() < time {
        let picture = match &camera {
            Some(camera) => match camera.latest() {
                Some(frame) if last.as_ref().is_none_or(|l| !std::sync::Arc::ptr_eq(l, &frame)) => {
                    last = Some(frame.clone());
                    frame
                }
                _ => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
            },
            None => {
                let due = started + Duration::from_millis(40) * pictures;
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
                std::sync::Arc::new(swarm_media::pattern::test_pattern(started.elapsed().as_secs_f32(), &background))
            }
        };
        if pictures == 0 {
            encoder.fit_picture(picture.width);
            encoder.set_bitrate(2_000_000)?;
        }
        pictures += 1;
        for piece in encoder.encode(&picture, 1200)? {
            if let Some(picture) = decoder.push(piece).picture {
                shown += 1;
                std::hint::black_box(picture.to_rgb());
            }
        }
    }
    let took = cpu_time().zip(cpu_before).map(|(after, before)| after.saturating_sub(before));
    let seconds = started.elapsed().as_secs_f64();
    let shape = encoder.shape();
    Ok(format!(
        "{} of one core; {:.0} pictures a second from {from}, sent at {}×{}, {:.0} shown (encoding {}, decoding {})",
        took.map_or("?".to_string(), |t| format!("{:.0}%", t.as_secs_f64() / seconds * 100.0)),
        pictures as f64 / seconds,
        shape.width,
        shape.height,
        shown as f64 / seconds,
        if encoder.hardware().is_some() { "on the chip" } else { "OpenH264" },
        if decoder.hardware().is_some() { "on the chip" } else { "OpenH264" },
    ))
}

/// How long the camera's watched at each size: long enough to see past
/// its start, and over the second half of which it's measured.
const CAMERA_FOR: Duration = Duration::from_secs(6);

/// What the camera gives. On Windows, both ways it can — NV12, as with
/// video on the graphics chip, and RGB, as without — and whether the two
/// pictures agree: a mistake in reading NV12 (rows, planes) would show
/// as a picture unlike the other.
fn check_camera(say: &mut dyn FnMut(String)) {
    let ways: &[(Option<bool>, &str)] = if cfg!(windows) {
        &[(Some(true), " as NV12 (video on the chip)"), (Some(false), " as RGB (video in software)")]
    } else if cfg!(target_os = "linux") && jpeg_on_chip() {
        &[(Some(true), " with its JPEG on the chip (if it sends JPEG)"), (Some(false), " in software")]
    } else {
        &[(None, "")]
    };
    let found = crate::camera::found();
    say(match crate::camera::chosen() {
        None => "Camera chosen in Settings: none, so the first that starts.".to_string(),
        Some(name) if !found.iter().any(|c| c.name == name) => format!("Camera chosen in Settings: {name}, not plugged in, so the first that starts."),
        Some(name) => format!("Camera chosen in Settings: {name}."),
    });
    let smaller = crate::camera::SIZES[1];
    match crate::camera::Camera::open_at(smaller) {
        Ok(camera) => {
            std::thread::sleep(CAMERA_FOR);
            let info = camera.info();
            if let Some(trouble) = crate::camera::chosen_trouble() {
                say(format!("  The camera chosen wouldn't start ({trouble}), so this is the first that would."));
            }
            say(format!("Camera asked for {}×{}: {} — {}×{} {}, {:.1} a second", smaller.0, smaller.1, info.name, info.width, info.height, info.format, camera.frames_per_second()));
            say(format!("  {}", camera.pace(CAMERA_FOR / 2)));
        }
        Err(e) => say(format!("Camera asked for {}×{}: {e:#}", smaller.0, smaller.1)),
    }
    let mut seen = Vec::new();
    for &(nv12, how) in ways {
        crate::camera::Camera::insist_on_chip(nv12);
        match crate::camera::Camera::open() {
            Ok(camera) => {
                std::thread::sleep(CAMERA_FOR);
                let info = camera.info();
                let latest = camera.latest();
                let format = latest.as_ref().map(|f| format!("{:?}", f.format)).unwrap_or_else(|| "nothing yet".to_string());
                say(format!("Camera{how}: {} — {}×{} {}, pictures as {format}, {:.1} a second", info.name, info.width, info.height, info.format, camera.frames_per_second()));
                say(format!("  {}", camera.pace(CAMERA_FOR / 2)));
                if let (Some(folder), Some(frame)) = (std::env::var_os("SWARM_VIDEO_DUMP"), &latest) {
                    let rgb = frame.to_rgb();
                    let mut ppm = format!("P6\n{} {}\n255\n", rgb.width, rgb.height).into_bytes();
                    ppm.extend_from_slice(&rgb.data);
                    let _ = std::fs::write(std::path::Path::new(&folder).join(format!("camera-{:?}.ppm", frame.format)), ppm);
                }
                seen.extend(latest);
            }
            Err(e) => say(format!("Camera{how}: {e:#}")),
        }
    }
    crate::camera::Camera::insist_on_chip(None);
    #[cfg(target_os = "linux")]
    if ways.len() == 2 {
        match (jpeg_checked(), seen.first()) {
            (Some(alike), Some(first)) if first.format == swarm_core::video::Format::I420 => say(format!("  The chip's picture of a frame matched software's of the same frame: {alike}.")),
            _ => {
                if let Some(trouble) = swarm_core::video::hardware_trouble() {
                    say(format!("  {trouble}"));
                }
            }
        }
        return;
    }
    if let [a, b] = &seen[..] {
        if a.format != b.format {
            let alike = psnr(&a.scaled(64, 36), &b.scaled(64, 36));
            say(if alike > 20.0 {
                format!("  The two agree ({alike:.0} dB): the chip's pictures are read right.")
            } else {
                format!("  The two DIFFER ({alike:.0} dB): the chip's pictures may be read wrong — if the camera looks wrong in calls, this is why; please report it.")
            });
        }
    }
}

/// Whether this computer's chip decodes a camera's JPEG (the camera's
/// then are).
fn jpeg_on_chip() -> bool {
    #[cfg(target_os = "linux")]
    return JpegOnChip::new().is_ok();
    #[cfg(not(target_os = "linux"))]
    false
}

/// `count` pictures `width`×`height`: the test pattern's ball moving over
/// its bars, with a little noise as a camera has.
fn moving_pictures(width: usize, height: usize, count: usize) -> Vec<Picture> {
    let background = swarm_media::pattern::test_background((width as u32, height as u32));
    let mut seed = 7u32;
    (0..count)
        .map(|i| {
            let mut picture = swarm_media::pattern::test_pattern(i as f32 / 25.0, &background);
            for byte in picture.data.iter_mut() {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *byte = byte.saturating_add(((seed >> 16) % 8) as u8).saturating_sub(4);
            }
            picture
        })
        .collect()
}

/// What an encoder made of the pictures.
struct Encoded {
    hardware: Option<String>,
    started_in: Duration,
    /// Per picture, average, after the first.
    took: Duration,
    frames: Vec<Vec<VideoPiece>>,
    keys: Vec<usize>,
}

/// Encodes `pictures` with (`hardware`) or without the hardware: a full
/// frame asked for at picture 40, and the bitrate lowered at 55.
fn encode_all(hardware: bool, h265: bool, pictures: &[Picture], bitrate: u32) -> anyhow::Result<Encoded> {
    swarm_core::video::set_hardware(hardware);
    let started = Instant::now();
    let mut encoder = VideoEncoder::new(bitrate)?;
    encoder.keep_shape();
    encoder.set_h265(h265);
    encoder.fit_picture(pictures[0].width);
    encoder.set_bitrate(bitrate)?;
    let started_in = started.elapsed();
    let (mut took, mut frames, mut keys) = (Duration::ZERO, Vec::new(), Vec::new());
    let hardware_at_start = encoder.hardware();
    for (i, picture) in pictures.iter().enumerate() {
        if i == 40 {
            encoder.request_keyframe();
        }
        if i == 55 {
            encoder.set_bitrate(bitrate * 4 / 5)?;
        }
        let t = Instant::now();
        let pieces = encoder.encode(picture, 1200)?;
        if i > 0 {
            took += t.elapsed();
        }
        if pieces.first().is_some_and(|p| p.key) {
            keys.push(i);
        }
        frames.push(pieces);
    }
    let hardware = match (hardware_at_start, encoder.hardware()) {
        (Some(name), Some(_)) => Some(name),
        (Some(name), None) => Some(format!("{name}, until it failed")),
        (None, _) => None,
    };
    Ok(Encoded { hardware, started_in, took: took / (pictures.len() as u32 - 1), frames, keys })
}

/// How alike two pictures are, in dB, the second scaled to the first.
fn psnr(shown: &Picture, sent: &Picture) -> f64 {
    let sent = sent.scaled(shown.width, shown.height);
    let (a, b) = (shown.to_rgb(), sent.to_rgb());
    let squared: f64 = a.data.iter().zip(&b.data).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum();
    10.0 * (255.0f64.powi(2) / (squared / a.data.len().max(1) as f64).max(1e-9)).log10()
}

/// What a decoder made of a run of frames.
struct Decoded {
    shown: usize,
    of: usize,
    took: Duration,
    /// How like the pictures sent the last few are, in dB, each matched
    /// to the picture it's most like, and by how many pictures late.
    alike: f64,
    late: usize,
}

impl Decoded {
    /// Every picture after the first few came through, and looks right.
    fn good(&self) -> bool {
        self.shown + 3 >= self.of && self.alike > 30.0
    }
}

impl std::fmt::Display for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} of {} shown, {:.1} ms a frame, {:.1} dB like what was sent", self.shown, self.of, self.took.as_secs_f64() * 1000.0 / self.of.max(1) as f64, self.alike)?;
        match self.late {
            0 => Ok(()),
            1 => write!(f, " (a picture late)"),
            n => write!(f, " ({n} pictures late)"),
        }
    }
}

/// Decodes `frames` (made from `pictures`), the last few pictures
/// matched to the picture they're most like, as a decoder may hand them
/// back late.
fn decode_all(mut decoder: VideoDecoder, frames: &[Vec<VideoPiece>], pictures: &[Picture]) -> Decoded {
    let (mut shown, mut took, mut alike, mut late) = (0, Duration::ZERO, Vec::new(), Vec::new());
    for (i, pieces) in frames.iter().enumerate() {
        let t = Instant::now();
        let mut picture = None;
        for piece in pieces.clone() {
            picture = decoder.push(piece).picture.or(picture);
        }
        took += t.elapsed();
        if let Some(picture) = picture {
            shown += 1;
            if let (Some(folder), true) = (std::env::var_os("SWARM_VIDEO_DUMP"), i + 1 == frames.len()) {
                let rgb = picture.to_rgb();
                let mut ppm = format!("P6\n{} {}\n255\n", rgb.width, rgb.height).into_bytes();
                ppm.extend_from_slice(&rgb.data);
                let name = format!("{}-{}x{}.ppm", decoder.hardware().map_or("openh264", |_| "hardware"), rgb.width, rgb.height);
                let _ = std::fs::write(std::path::Path::new(&folder).join(name), ppm);
            }
            if i + 10 >= frames.len() {
                let (by, best) = (0..=i.min(20)).map(|k| (k, psnr(&picture, &pictures[i - k]))).fold((0, f64::MIN), |a, b| if b.1 > a.1 { b } else { a });
                late.push(by);
                alike.push(best);
            }
        }
    }
    late.sort_unstable();
    Decoded {
        shown,
        of: frames.len(),
        took,
        alike: alike.iter().sum::<f64>() / alike.len().max(1) as f64,
        late: late.get(late.len() / 2).copied().unwrap_or(0),
    }
}

/// Encoding and decoding on the chip at one size: whether each works
/// (`None`: there was none to try).
struct Verdict {
    encoding: Option<bool>,
    decoding: Option<bool>,
}

fn check_size(width: usize, height: usize, bitrate: u32, say: &mut dyn FnMut(String)) -> Verdict {
    let mut verdict = Verdict { encoding: None, decoding: None };
    let pictures = moving_pictures(width, height, 75);
    let soft = match encode_all(false, false, &pictures, bitrate) {
        Ok(soft) => soft,
        Err(e) => {
            say(format!("  OpenH264 couldn't encode it: {e:#}"));
            return verdict;
        }
    };
    let size = |e: &Encoded| e.frames.iter().flatten().map(|p| p.data.len()).sum::<usize>() * 8 / 3 / 1000;
    say(format!("  OpenH264 encoding: {:.1} ms a picture, {} kbit/s, full frames at {:?}", soft.took.as_secs_f64() * 1000.0, size(&soft), soft.keys));
    let mut hard_frames = None;
    match encode_all(true, false, &pictures, bitrate) {
        Err(e) => {
            say(format!("  Hardware encoding: failed: {e:#}"));
            verdict.encoding = Some(false);
        }
        Ok(hard) => match &hard.hardware {
            None => say(format!("  Hardware encoding: none — {}", swarm_core::video::hardware_trouble().unwrap_or_default())),
            Some(name) => {
                say(format!(
                    "  Hardware encoding ({name}): started in {} ms, {:.1} ms a picture, {} kbit/s, full frames at {:?} (asked for at 40)",
                    hard.started_in.as_millis(),
                    hard.took.as_secs_f64() * 1000.0,
                    size(&hard),
                    hard.keys
                ));
                if name.contains("until it failed") {
                    say(format!("    {}", swarm_core::video::hardware_trouble().unwrap_or_default()));
                }
                let asked = hard.keys.iter().any(|&k| (40..=42).contains(&k));
                if let Ok(decoder) = VideoDecoder::software() {
                    let decoded = decode_all(decoder, &hard.frames, &pictures);
                    say(format!("    in OpenH264 (as other swarms decode it): {decoded}"));
                    verdict.encoding = Some(decoded.good() && asked && !name.contains("until it failed"));
                }
                hard_frames = Some(hard.frames);
            }
        },
    }
    if let Ok(decoder) = VideoDecoder::software() {
        say(format!("  OpenH264 decoding OpenH264's: {}", decode_all(decoder, &soft.frames, &pictures)));
    }
    swarm_core::video::set_hardware(true);
    match VideoDecoder::new() {
        Ok(decoder) => match decoder.hardware() {
            Some(name) => {
                let decoded = decode_all(decoder, &soft.frames, &pictures);
                say(format!("  Hardware decoding ({name}) OpenH264's: {decoded}"));
                verdict.decoding = Some(decoded.good());
                if let (Some(frames), Ok(decoder)) = (&hard_frames, VideoDecoder::new()) {
                    say(format!("    and the hardware encoder's: {}", decode_all(decoder, frames, &pictures)));
                }
            }
            None => say(format!("  Hardware decoding: none — {}", swarm_core::video::hardware_trouble().unwrap_or_default())),
        },
        Err(e) => {
            say(format!("  Hardware decoding: failed: {e:#}"));
            verdict.decoding = Some(false);
        }
    }
    if swarm_core::video::encodes_h265() {
        match encode_all(true, true, &pictures, bitrate / 5 * 3) {
            Err(e) => say(format!("  Hardware H.265 encoding: failed: {e:#}")),
            Ok(hard) if !hard.frames.iter().flatten().any(|p| p.h265) => say(format!("  Hardware H.265 encoding: none — {}", swarm_core::video::hardware_trouble().unwrap_or_default())),
            Ok(hard) => {
                say(format!("  Hardware H.265 encoding at {} kbit/s: {:.1} ms a picture, {} kbit/s, full frames at {:?}", bitrate / 5 * 3 / 1000, hard.took.as_secs_f64() * 1000.0, size(&hard), hard.keys));
                if let (true, Ok(decoder)) = (swarm_core::video::decodes_h265(), VideoDecoder::new()) {
                    say(format!("    decoded on the chip: {}", decode_all(decoder, &hard.frames, &pictures)));
                }
            }
        }
    }
    verdict
}
