//! Your picture, as peers see it (see `swarm_core::profile`): made from
//! any picture — a photo file, or one taken with the camera — by taking
//! the middle square, scaling it down to `MAX_AVATAR_SIDE` if it's
//! larger, and encoding it as the one kind of JPEG a profile allows.
//! And a peer's, decoded only once `profile` has passed it, and kept at
//! `SHOWN_SIDE` to show.

use swarm_core::lock::Locked;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use swarm_core::profile::{self, MAX_AVATAR_BYTES, MAX_AVATAR_SIDE};
use swarm_core::video::Picture;

/// The largest picture file taken, each way: a photo, not a poster.
const MOST_FILE_SIDE: u32 = 16384;

/// A picture's width and height as it's kept to show: sharp at the
/// sizes it's drawn, without holding a whole 640 for each peer.
pub const SHOWN_SIDE: usize = 192;

/// The middle square of `picture`, scaled to `out_side`, as RGB: each
/// pixel the average of the ones it covers (or the nearest, from a
/// picture smaller than that).
pub fn square(picture: &Picture, out_side: usize) -> Vec<u8> {
    let side = picture.width.min(picture.height);
    let (left, top) = ((picture.width - side) / 2, (picture.height - side) / 2);
    let mut out = Vec::with_capacity(out_side * out_side * 3);
    for oy in 0..out_side {
        let (y0, y1) = (oy * side / out_side, ((oy + 1) * side / out_side).max(oy * side / out_side + 1));
        for ox in 0..out_side {
            let (x0, x1) = (ox * side / out_side, ((ox + 1) * side / out_side).max(ox * side / out_side + 1));
            let (sy, sx) = ((y1 - y0).div_ceil(8), (x1 - x0).div_ceil(8));
            let mut sum = [0u32; 3];
            let mut n = 0;
            for y in (y0..y1).step_by(sy) {
                for x in (x0..x1).step_by(sx) {
                    let p = picture.pixel(left + x, top + y);
                    for c in 0..3 {
                        sum[c] += p[c] as u32;
                    }
                    n += 1;
                }
            }
            out.extend(sum.map(|s| ((s + n / 2) / n) as u8));
        }
    }
    out
}

/// `picture` as an avatar: its middle square, no larger than
/// `MAX_AVATAR_SIDE`, encoded as `profile` allows — at the best quality
/// that fits.
pub fn from_picture(picture: &Picture) -> Result<Vec<u8>> {
    if picture.width == 0 || picture.height == 0 {
        bail!("the picture is empty");
    }
    let side = picture.width.min(picture.height).min(MAX_AVATAR_SIDE);
    let rgb = square(picture, side);
    for quality in [90, 80, 70, 60, 50, 40, 30, 20] {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality)
            .encode(&rgb, side as u32, side as u32, image::ExtendedColorType::Rgb8)
            .context("encoding the picture")?;
        if jpeg.len() <= MAX_AVATAR_BYTES {
            if !profile::is_valid_avatar(&jpeg) {
                bail!("the picture didn't come out as a profile allows");
            }
            return Ok(jpeg);
        }
    }
    bail!("the picture wouldn't fit in {MAX_AVATAR_BYTES} bytes")
}

/// A picture file (JPEG or PNG) as an avatar.
pub fn from_file(path: &Path) -> Result<Vec<u8>> {
    let mut reader = image::ImageReader::open(path).with_context(|| format!("opening {}", path.display()))?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MOST_FILE_SIDE);
    limits.max_image_height = Some(MOST_FILE_SIDE);
    reader.limits(limits);
    let rgb = reader.decode().with_context(|| format!("reading {} as a picture", path.display()))?.to_rgb8();
    let (width, height) = (rgb.width() as usize, rgb.height() as usize);
    from_picture(&Picture::rgb(width, height, rgb.into_raw()))
}

/// An avatar `profile` has passed, as its side and RGB pixels; `None`
/// if it doesn't decode to the square its headers said.
pub fn decode(jpeg: &[u8]) -> Option<(usize, Vec<u8>)> {
    if jpeg.is_empty() || !profile::is_valid_avatar(jpeg) {
        return None;
    }
    let options = zune_jpeg::zune_core::options::DecoderOptions::default()
        .set_max_width(MAX_AVATAR_SIDE)
        .set_max_height(MAX_AVATAR_SIDE)
        .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(jpeg, options);
    let rgb = decoder.decode().ok()?;
    let info = decoder.info()?;
    let side = info.width as usize;
    (info.height as usize == side && rgb.len() == side * side * 3).then_some((side, rgb))
}

/// A decoded avatar (`side` square) as it's kept to show: `SHOWN_SIDE`.
pub fn shown(side: usize, rgb: &[u8]) -> Vec<u8> {
    square(&Picture::rgb(side, side, rgb.to_vec()), SHOWN_SIDE)
}

/// How long an avatar's taken as it was last found, before its file's
/// looked at again: a call's strip asks several times a second.
const CHECKED_FOR: Duration = Duration::from_secs(3);

/// One found: when it was looked for, the profile's time, its pixels and
/// its status.
#[derive(Clone)]
struct Found {
    checked: Instant,
    at: u64,
    pixels: Option<Arc<Vec<u8>>>,
    status: String,
}

/// Profiles found, by whose (`own`: this device's): each picture decoded
/// once, however often it's shown, until it changes.
static FOUND: Mutex<Option<HashMap<Whose, Found>>> = Mutex::new(None);

/// Someone's key, whether it's this device's own, and the folder it's
/// kept in.
type Whose = ([u8; 32], bool, std::path::PathBuf);

/// `key`'s profile — a peer's as kept, or this device's own (`own`) —
/// as last found, looked at again if it's been a while.
fn found(key: &[u8; 32], own: bool) -> Found {
    found_in(&crate::paths(), key, own)
}

fn found_in(paths: &swarm_core::node::Paths, key: &[u8; 32], own: bool) -> Found {
    let whose = (*key, own, paths.dir().to_path_buf());
    let mut all = FOUND.locked();
    let all = all.get_or_insert_with(HashMap::new);
    let before = all.get(&whose);
    if let Some(found) = before.filter(|f| f.checked.elapsed() < CHECKED_FOR) {
        return found.clone();
    }
    let profile = if own { profile::current(&paths.profile()) } else { profile::cached(&paths.profiles(), key) };
    let at = profile.as_ref().map_or(0, |p| p.at);
    let pixels = match before {
        Some(found) if found.at == at => found.pixels.clone(),
        _ => profile.as_ref().and_then(|p| decode(&p.avatar)).map(|(side, rgb)| Arc::new(shown(side, &rgb))),
    };
    let found = Found { checked: Instant::now(), at, pixels, status: profile.map(|p| p.status).unwrap_or_default() };
    all.insert(whose, found.clone());
    found
}

/// `key`'s avatar as RGB (`SHOWN_SIDE` square) — a peer's as kept, or
/// this device's own (`own`) — if they have one that decodes.
pub fn pixels(key: &[u8; 32], own: bool) -> Option<Arc<Vec<u8>>> {
    found(key, own).pixels
}

/// `key`'s status ("" for none): a peer's as kept, or this device's own.
pub fn status(key: &[u8; 32], own: bool) -> String {
    found(key, own).status
}

/// A peer's avatar (as `pixels`) and status (as `status`), as kept in
/// `folder`.
pub fn of_peer(folder: &swarm_core::node::Paths, key: &[u8; 32]) -> (Option<Arc<Vec<u8>>>, String) {
    let found = found_in(folder, key, false);
    (found.pixels, found.status)
}

/// Forgets what's been found of this device's own, which has just been
/// changed here: shown changed at once.
pub fn changed_own() {
    if let Some(found) = FOUND.locked().as_mut() {
        found.retain(|(_, own, _), _| !own);
    }
}

/// Forgets what's been found of `key` (a contact deleted): nothing of
/// them is kept in memory either.
pub fn forget(key: &[u8; 32]) {
    if let Some(found) = FOUND.locked().as_mut() {
        found.retain(|(k, own, _), _| *own || k != key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture with a different colour in each corner, and a white
    /// middle: what's kept of it shows where it was cut and scaled.
    fn quarters(width: usize, height: usize) -> Picture {
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let (cx, cy) = (x as i64 - width as i64 / 2, y as i64 - height as i64 / 2);
                let middle = cx.abs() < height as i64 / 8 && cy.abs() < height as i64 / 8;
                rgb.extend(match (middle, x < width / 2, y < height / 2) {
                    (true, ..) => [255, 255, 255],
                    (_, true, true) => [200, 0, 0],
                    (_, false, true) => [0, 200, 0],
                    (_, true, false) => [0, 0, 200],
                    (_, false, false) => [200, 200, 0],
                });
            }
        }
        Picture::rgb(width, height, rgb)
    }

    fn near(a: &[u8], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(&a, b)| (a as i32 - b as i32).abs() < 24)
    }

    /// Its middle square, as large as it is up to `MAX_AVATAR_SIDE`, and
    /// shown at `SHOWN_SIDE` whatever its size.
    #[test]
    fn a_wide_photo_becomes_its_middle_square_up_to_640() {
        for (width, height, side) in [(3840, 2160, 640), (1280, 720, 640), (720, 1280, 640), (600, 900, 600), (128, 128, 128), (50, 30, 30)] {
            let jpeg = from_picture(&quarters(width, height)).unwrap();
            assert!(profile::is_valid_avatar(&jpeg) && !jpeg.is_empty(), "{width}×{height}");
            assert!(jpeg.len() <= MAX_AVATAR_BYTES);
            let (got, rgb) = decode(&jpeg).expect("it decodes");
            assert_eq!(got, side, "{width}×{height}");
            let shown = shown(side, &rgb);
            assert_eq!(shown.len(), SHOWN_SIDE * SHOWN_SIDE * 3);
            let at = |x: usize, y: usize| &shown[(y * SHOWN_SIDE + x) * 3..][..3];
            let far = SHOWN_SIDE - 5;
            assert!(near(at(4, 4), [200, 0, 0]), "{width}×{height}: {:?}", at(4, 4));
            assert!(near(at(far, 4), [0, 200, 0]));
            assert!(near(at(4, far), [0, 0, 200]));
            assert!(near(at(far, far), [200, 200, 0]));
            assert!(near(at(SHOWN_SIDE / 2, SHOWN_SIDE / 2), [255, 255, 255]));
        }
    }

    #[test]
    fn noise_that_compresses_badly_is_squeezed_until_it_fits() {
        let mut seed = 1u32;
        let rgb = (0..512 * 512 * 3)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (seed >> 16) as u8
            })
            .collect();
        let jpeg = from_picture(&Picture::rgb(512, 512, rgb)).unwrap();
        assert!(jpeg.len() <= MAX_AVATAR_BYTES && profile::is_valid_avatar(&jpeg));
    }

    #[test]
    fn a_png_or_jpeg_file_becomes_an_avatar_and_anything_else_doesnt() {
        let dir = std::env::temp_dir().join(format!("swarm-avatar-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let picture = quarters(300, 200);
        let png = dir.join("photo.png");
        image::save_buffer(&png, &picture.data, 300, 200, image::ExtendedColorType::Rgb8).unwrap();
        let jpg = dir.join("photo.jpeg");
        image::save_buffer(&jpg, &picture.data, 300, 200, image::ExtendedColorType::Rgb8).unwrap();
        for file in [&png, &jpg] {
            let (_, rgb) = decode(&from_file(file).unwrap()).unwrap();
            assert!(near(&rgb[..3], [200, 0, 0]), "{}", file.display());
        }
        let text = dir.join("notes.png");
        std::fs::write(&text, "not a picture").unwrap();
        assert!(from_file(&text).is_err());
        assert!(from_file(&dir.join("missing.png")).is_err());
    }

    #[test]
    fn only_what_a_profile_allows_is_decoded() {
        assert_eq!(decode(&[]), None);
        let jpeg = from_picture(&quarters(128, 128)).unwrap();
        assert!(decode(&jpeg).is_some());
        let mut big = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut big, 50).encode(&quarters(700, 700).data, 700, 700, image::ExtendedColorType::Rgb8).unwrap();
        assert_eq!(decode(&big), None);
        let mut damaged = jpeg.clone();
        let len = damaged.len();
        for b in &mut damaged[len / 2..len - 2] {
            *b = 0xFF;
        }
        let _ = decode(&damaged);
    }
}
