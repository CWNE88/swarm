//! A look at a received frame's sequence parameter sets before any
//! decoder sees it: the picture size and the number of reference
//! pictures they ask for. Every decoder (OpenH264, Media Foundation,
//! NVDEC, VA-API) sets aside memory for that many pictures of that size
//! as soon as it reads one, so a single frame from anyone could otherwise
//! ask for gigabytes. Only those few fields are read, and anything else
//! about the stream is left to the decoder: a frame is refused only when
//! what it asks for is past what any real stream needs.

/// The most macroblocks in a picture: H.264's largest level's (6.2), a
/// picture of 8192 by 4352 or so. The same pixel count for H.265.
pub const MOST_MBS: u64 = 139_264;
/// The most reference pictures a stream may keep (H.264's largest
/// MaxDpbFrames; H.265's DPB holds as many).
pub const MOST_REFERENCES: u32 = 16;

/// Whether `frame` (Annex B: start codes, then NAL units) asks for no
/// more than any real stream does. `h265` says which codec it is.
pub fn within_limits(frame: &[u8], h265: bool) -> bool {
    nal_units(frame).into_iter().all(|nal| {
        let asked = if h265 { h265_asks(nal) } else { h264_asks(nal) };
        asked.is_none_or(|(mbs, references)| mbs <= MOST_MBS && references <= MOST_REFERENCES)
    })
}

/// The NAL units in an Annex B stream, after each start code.
fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts.iter().enumerate().map(|(n, &start)| &stream[start..starts.get(n + 1).map_or(stream.len(), |&next| next - 3)]).collect()
}

/// A NAL unit's payload with its emulation-prevention bytes taken out.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn take(&mut self, n: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.take(1)? == 1)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.take(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(self.take(zeros)?)) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
    }
}

/// An H.264 SPS's picture size in macroblocks and reference frames
/// (7.3.2.1.1). `None` for any other NAL unit, or one cut short.
fn h264_asks(nal: &[u8]) -> Option<(u64, u32)> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(nal.get(1..)?);
    let mut bits = Bits { data: &rbsp, at: 0 };
    let profile = bits.take(8)?;
    bits.take(16)?;
    bits.ue()?;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = bits.ue()?;
        if chroma == 3 {
            bits.take(1)?;
        }
        bits.ue()?;
        bits.ue()?;
        bits.take(1)?;
        if bits.flag()? {
            for list in 0..if chroma == 3 { 12 } else { 8 } {
                if bits.flag()? {
                    let size = if list < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + bits.se()? + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    bits.ue()?;
    match bits.ue()? {
        0 => {
            bits.ue()?;
        }
        1 => {
            bits.take(1)?;
            bits.se()?;
            bits.se()?;
            for _ in 0..bits.ue()?.min(255) {
                bits.se()?;
            }
        }
        _ => {}
    }
    let references = bits.ue()?;
    bits.take(1)?;
    let width = u64::from(bits.ue()?) + 1;
    let map_units = u64::from(bits.ue()?) + 1;
    let frames_only = bits.flag()?;
    Some((width * map_units * if frames_only { 1 } else { 2 }, references))
}

/// An H.265 SPS's picture size (in 16 by 16 blocks, as H.264 counts) and
/// the pictures its decoder must hold (7.3.2.2.1). `None` for any other
/// NAL unit, or one cut short.
fn h265_asks(nal: &[u8]) -> Option<(u64, u32)> {
    if (nal.first()? >> 1) & 0x3f != 33 {
        return None;
    }
    let rbsp = unescape(nal.get(2..)?);
    let mut bits = Bits { data: &rbsp, at: 0 };
    bits.take(4)?;
    let sub_layers = bits.take(3)?;
    bits.take(1)?;
    // Profile, tier and level (7.3.3): the general one (96 bits, its
    // level last), then which sub-layers say theirs, then theirs (88 bits
    // for a profile, 8 for a level).
    bits.take(32)?;
    bits.take(32)?;
    bits.take(32)?;
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((bits.flag()?, bits.flag()?));
    }
    if sub_layers > 0 {
        bits.take(2 * (8 - sub_layers))?;
    }
    for (profile, level) in present {
        if profile {
            bits.take(32)?;
            bits.take(32)?;
            bits.take(24)?;
        }
        if level {
            bits.take(8)?;
        }
    }
    bits.ue()?;
    if bits.ue()? == 3 {
        bits.take(1)?;
    }
    let width = u64::from(bits.ue()?);
    let height = u64::from(bits.ue()?);
    if bits.flag()? {
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    bits.ue()?;
    bits.ue()?;
    bits.ue()?;
    let each = bits.flag()?;
    let mut buffering = 0;
    for _ in if each { 0 } else { sub_layers }..=sub_layers {
        buffering = buffering.max(bits.ue()?);
        bits.ue()?;
        bits.ue()?;
    }
    Some(((width * height).div_ceil(256), buffering))
}
