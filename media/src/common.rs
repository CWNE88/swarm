//! What the chip modules share: the camera's JPEGs on the chip, NV12
//! and I420, the range H.264 uses, and an Annex-B stream's parameter
//! sets and size.

use swarm_core::video::Picture;

/// A camera's JPEGs decoded on the graphics chip: NVIDIA's (NVDEC) or,
/// failing that, VA-API's, as `init` chooses for video.
#[cfg(target_os = "linux")]
pub enum JpegOnChip {
    Nvidia(crate::video_nv::JpegDecoder),
    VaApi(crate::video_va::JpegDecoder),
}

#[cfg(target_os = "linux")]
impl JpegOnChip {
    pub fn new() -> anyhow::Result<JpegOnChip> {
        match crate::video_nv::JpegDecoder::new() {
            Ok(decoder) => Ok(JpegOnChip::Nvidia(decoder)),
            Err(nvidia) => crate::video_va::JpegDecoder::new().map(JpegOnChip::VaApi).map_err(|e| anyhow::anyhow!("NVIDIA: {nvidia:#}; VA-API: {e:#}")),
        }
    }

    pub fn name(&self) -> String {
        match self {
            JpegOnChip::Nvidia(decoder) => decoder.name(),
            JpegOnChip::VaApi(decoder) => decoder.name(),
        }
    }

    pub fn decode(&mut self, file: &[u8]) -> anyhow::Result<Picture> {
        match self {
            JpegOnChip::Nvidia(decoder) => decoder.decode(file),
            JpegOnChip::VaApi(decoder) => decoder.decode(file),
        }
    }
}

/// How alike the chip's and software's pictures of the same JPEG were,
/// per plane, when a camera's JPEGs were first decoded on the chip.
static JPEG_CHECKED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Notes a camera's JPEG on the chip matching software's (see
/// `JPEG_CHECKED`).
pub fn note_jpeg_checked(how: String) {
    *JPEG_CHECKED.lock().unwrap_or_else(|e| e.into_inner()) = Some(how);
}

/// The last check of a camera's JPEG on the chip against software, if one
/// passed.
pub fn jpeg_checked() -> Option<String> {
    JPEG_CHECKED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Tests that use a graphics chip (or the stand-in driver) take turns:
/// they share which chip decodes the camera's JPEG, and what was found
/// of it.
#[cfg(test)]
pub static CHIP_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub fn forget_jpeg_check() {
    *JPEG_CHECKED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// A JPEG's YCbCr (all of 0 to 255) put in the range H.264 uses (16 to
/// 235 for brightness, 16 to 240 for colour), as the rest of Swarm's
/// pictures are.
pub fn limited_range(picture: &mut Picture) {
    let (luma, chroma) = limited_range_tables();
    let pixels = picture.width * picture.height;
    let (y, colour) = picture.data.split_at_mut(pixels);
    y.iter_mut().for_each(|v| *v = luma[*v as usize]);
    colour.iter_mut().for_each(|v| *v = chroma[*v as usize]);
}

/// What `limited_range` maps brightness and colour values through.
pub fn limited_range_tables() -> &'static ([u8; 256], [u8; 256]) {
    static TABLES: std::sync::OnceLock<([u8; 256], [u8; 256])> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        let luma = std::array::from_fn(|v| (16.0 + v as f32 * 219.0 / 255.0).round() as u8);
        let chroma = std::array::from_fn(|v| (128.0 + (v as f32 - 128.0) * 224.0 / 255.0).round() as u8);
        (luma, chroma)
    })
}

/// How alike two I420 pictures of a size are, plane by plane (Y, U and
/// V), in dB (99 for the same).
pub fn planes_psnr(a: &Picture, b: &Picture) -> [f64; 3] {
    let (a, b) = (a.planes(), b.planes());
    [(a.0, b.0), (a.1, b.1), (a.2, b.2)].map(|(a, b)| {
        let mse = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len().max(1) as f64;
        if mse == 0.0 { 99.0 } else { 10.0 * (255.0 * 255.0 / mse).log10() }
    })
}

/// An I420 picture as NV12 (the same Y plane, then U and V taken turn
/// about in one plane), which is what hardware takes.
pub fn nv12_from(picture: &Picture) -> Vec<u8> {
    let i420 = picture.to_i420();
    let (y, u, v) = i420.planes();
    let mut nv12 = Vec::with_capacity(y.len() + u.len() * 2);
    nv12.extend_from_slice(y);
    for (&u, &v) in u.iter().zip(v) {
        nv12.extend([u, v]);
    }
    nv12
}

/// An NV12 picture `width`×`height` (rows `pitch` bytes apart, the UV
/// plane starting `uv_at` bytes in) as I420, cropped to the even size.
pub fn i420_from_nv12(data: &[u8], width: usize, height: usize, pitch: usize, uv_at: usize) -> Option<Picture> {
    i420_from_nv12_mapped(data, width, height, pitch, uv_at, None)
}

/// `i420_from_nv12`, each brightness and colour value through `map`'s
/// tables on the way (`limited_range_tables`, say) — one pass, not two.
pub fn i420_from_nv12_mapped(data: &[u8], width: usize, height: usize, pitch: usize, uv_at: usize, map: Option<&([u8; 256], [u8; 256])>) -> Option<Picture> {
    let (width, height) = (width & !1, height & !1);
    if width == 0 || height == 0 || pitch < width || data.len() < uv_at + pitch * (height / 2 - 1) + width || uv_at < pitch * (height - 1) + width {
        return None;
    }
    let half = width / 2;
    let mut out = vec![0u8; width * height + 2 * half * (height / 2)];
    let (luma, chroma) = out.split_at_mut(width * height);
    let (u, v) = chroma.split_at_mut(half * (height / 2));
    for y in 0..height {
        let (to, from) = (&mut luma[y * width..(y + 1) * width], &data[y * pitch..y * pitch + width]);
        match map {
            None => to.copy_from_slice(from),
            Some((table, _)) => to.iter_mut().zip(from).for_each(|(to, &from)| *to = table[from as usize]),
        }
    }
    let identity: [u8; 256] = std::array::from_fn(|v| v as u8);
    let table = map.map_or(&identity, |(_, chroma)| chroma);
    for y in 0..height / 2 {
        let row = &data[uv_at + y * pitch..uv_at + y * pitch + width];
        for (x, pair) in row.as_chunks::<2>().0.iter().enumerate() {
            u[y * half + x] = table[pair[0] as usize];
            v[y * half + x] = table[pair[1] as usize];
        }
    }
    Some(Picture::i420(width, height, out))
}

/// The SPS and PPS of an Annex-B stream, if it has them.
pub fn parameter_sets(stream: &[u8]) -> Option<Vec<u8>> {
    let mut sets = Vec::new();
    for nal in nal_units(stream) {
        if matches!(nal.first().map(|b| b & 0x1f), Some(7 | 8)) {
            sets.extend_from_slice(&[0, 0, 0, 1]);
            sets.extend_from_slice(nal);
        }
    }
    (!sets.is_empty()).then_some(sets)
}

pub use crate::h264::nal_units;

/// The picture size an Annex-B stream's SPS gives, cropping included —
/// for a decoder that hands back its padded size (1088 rows for 1080)
/// without saying what to show. `None` without an SPS, or one this
/// doesn't read (scaling lists).
pub fn sps_size(stream: &[u8]) -> Option<(usize, usize)> {
    let nal = nal_units(stream).into_iter().find(|n| n.first().map(|b| b & 0x1f) == Some(7))?;
    let (width, height, _, _) = crate::h264::Sps::parse(nal)?.shown()?;
    Some((width, height))
}

/// A JPEG to RGB. (Straight to I420, from the JPEG's own YUV, measured
/// slower: zune-jpeg's conversion to RGB is quick, and so is openh264's
/// from RGB.) One without its Huffman tables (as MJPEG may come) is
/// given the standard ones, which zune-jpeg doesn't assume.
pub fn mjpeg_to_rgb(buf: &[u8]) -> Option<Picture> {
    let decode = |buf: &[u8]| {
        let mut decoder = zune_jpeg::JpegDecoder::new(buf);
        let rgb = decoder.decode().ok()?;
        let info = decoder.info()?;
        let (width, height) = (info.width as usize, info.height as usize);
        (rgb.len() == width * height * 3).then(|| Picture::rgb(width, height, rgb))
    };
    decode(buf).or_else(|| decode(&crate::jpeg::Jpeg::parse(buf)?.write()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::video::Format;

    #[test]
    fn nv12_goes_both_ways() {
        let picture = Picture::i420(4, 2, vec![1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 20, 21]);
        let nv12 = nv12_from(&picture);
        assert_eq!(nv12, vec![1, 2, 3, 4, 5, 6, 7, 8, 10, 20, 11, 21]);
        let back = i420_from_nv12(&nv12, 4, 2, 4, 8).unwrap();
        assert_eq!(back, picture);
        assert_eq!(back.format, Format::I420);
        let padded = [1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0, 9, 9, 9, 9, 9, 9, 9, 9, 10, 20, 11, 21, 0, 0, 0, 0];
        assert_eq!(i420_from_nv12(&padded, 4, 2, 8, 24).unwrap(), picture);
        assert!(i420_from_nv12(&padded[..20], 4, 2, 8, 24).is_none());
    }

    #[test]
    fn parameter_sets_are_found_and_units_split() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9, 9];
        assert_eq!(nal_units(&stream), vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
        assert_eq!(parameter_sets(&stream).unwrap(), vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
        assert!(parameter_sets(&[0, 0, 1, 0x41, 5]).is_none());
    }

    /// The size OpenH264's streams say, cropping included (1080 is 1088
    /// rows cropped by 8).
    #[test]
    fn the_picture_size_is_read_from_the_sps() {
        for (width, height, bitrate) in [(1920, 1080, 4_000_000), (1280, 720, 2_000_000), (960, 540, 1_000_000), (480, 270, 200_000)] {
            let mut encoder = swarm_core::video::VideoEncoder::new(bitrate).unwrap();
            encoder.keep_shape();
            let picture = Picture::rgb(width, height, vec![90; width * height * 3]);
            let shape = encoder.shape();
            assert_eq!((shape.width, shape.height), (width, height));
            let frame: Vec<u8> = encoder.encode(&picture.scaled(shape.width, shape.height), 100_000).unwrap().into_iter().flat_map(|p| p.data).collect();
            assert_eq!(sps_size(&frame), Some((shape.width, shape.height)));
        }
        assert_eq!(sps_size(&[0, 0, 1, 0x65, 1, 2]), None);
    }

    /// A camera that leaves its Huffman tables out (as MJPEG may) still
    /// gives pictures: the standard tables are put in.
    #[test]
    fn jpegs_without_their_tables_decode() {
        assert_eq!(mjpeg_to_rgb(crate::jpeg::tests::CAMERA_420_NO_TABLES).expect("a picture"), mjpeg_to_rgb(crate::jpeg::tests::CAMERA_420).unwrap());
    }
}
