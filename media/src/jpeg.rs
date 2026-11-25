//! A JPEG's tables and headers, read (and written back): for decoding a
//! camera's pictures on the graphics chip, which is given these rather
//! than the file (VA-API). Baseline only, as webcams send: 8-bit,
//! Huffman-coded, one scan of all the components. Anything else is
//! refused (`None`), and it's decoded in software.
//!
//! Self-contained (std only): the stand-in VA-API driver used in tests
//! includes it too.

/// One of the picture's components (Y, Cb or Cr).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Component {
    pub id: u8,
    pub h: u8,
    pub v: u8,
    /// Its quantisation table.
    pub tq: u8,
}

/// A component in the scan, with its Huffman tables.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanComponent {
    pub id: u8,
    pub dc: u8,
    pub ac: u8,
}

/// A Huffman table as the file has it: how many codes of each length
/// (1 to 16 bits), then their values.
#[derive(Clone, Debug, PartialEq)]
pub struct Huffman {
    pub counts: [u8; 16],
    pub values: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Jpeg<'a> {
    pub width: u16,
    pub height: u16,
    pub components: Vec<Component>,
    /// Quantisation tables 0 to 3, in the file's (zig-zag) order.
    pub quant: [Option<[u8; 64]>; 4],
    /// Huffman tables 0 and 1, for DC and for AC.
    pub dc: [Option<Huffman>; 2],
    pub ac: [Option<Huffman>; 2],
    /// Macroblocks (MCUs) between restart markers, 0 for none.
    pub restart_interval: u16,
    pub scan: Vec<ScanComponent>,
    /// The coded data: from after the scan's header to the end.
    pub data: &'a [u8],
}

const DC_LUMA: ([u8; 16], [u8; 12]) = ([0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0], [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b]);
const DC_CHROMA: ([u8; 16], [u8; 12]) = ([0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0], [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b]);
const AC_LUMA: ([u8; 16], [u8; 162]) = (
    [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125],
    [
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
);
const AC_CHROMA: ([u8; 16], [u8; 162]) = (
    [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119],
    [
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96,
        0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
);

impl<'a> Jpeg<'a> {
    /// Reads a JPEG's headers, up to its scan. Missing Huffman tables are
    /// the standard ones (webcams' MJPEG often leaves them out).
    pub fn parse(file: &'a [u8]) -> Option<Jpeg<'a>> {
        if file.get(..2)? != [0xff, 0xd8] {
            return None;
        }
        let mut jpeg = Jpeg { width: 0, height: 0, components: Vec::new(), quant: [None; 4], dc: [None, None], ac: [None, None], restart_interval: 0, scan: Vec::new(), data: &[] };
        let mut at = 2;
        loop {
            while *file.get(at)? == 0xff && *file.get(at + 1)? == 0xff {
                at += 1;
            }
            if *file.get(at)? != 0xff {
                return None;
            }
            let marker = *file.get(at + 1)?;
            let length = u16::from_be_bytes([*file.get(at + 2)?, *file.get(at + 3)?]) as usize;
            let segment = file.get(at + 4..at + 2 + length)?;
            match marker {
                0xc0 | 0xc1 => {
                    if segment[0] != 8 || !jpeg.components.is_empty() {
                        return None;
                    }
                    jpeg.height = u16::from_be_bytes([segment[1], segment[2]]);
                    jpeg.width = u16::from_be_bytes([segment[3], segment[4]]);
                    let count = segment[5] as usize;
                    if !(1..=3).contains(&count) || jpeg.width == 0 || jpeg.height == 0 {
                        return None;
                    }
                    for c in segment.get(6..6 + 3 * count)?.as_chunks::<3>().0 {
                        let (h, v) = (c[1] >> 4, c[1] & 15);
                        if !(1..=2).contains(&h) || !(1..=2).contains(&v) || c[2] > 3 {
                            return None;
                        }
                        jpeg.components.push(Component { id: c[0], h, v, tq: c[2] });
                    }
                }
                0xc2..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => return None,
                0xc4 => {
                    let mut rest = segment;
                    while !rest.is_empty() {
                        let (class, id) = (rest[0] >> 4, rest[0] & 15);
                        let counts: [u8; 16] = rest.get(1..17)?.try_into().ok()?;
                        let n = counts.iter().map(|&c| c as usize).sum::<usize>();
                        let values = rest.get(17..17 + n)?.to_vec();
                        if id > 1 || class > 1 || n > if class == 0 { 12 } else { 162 } {
                            return None;
                        }
                        let table = Some(Huffman { counts, values });
                        if class == 0 {
                            jpeg.dc[id as usize] = table;
                        } else {
                            jpeg.ac[id as usize] = table;
                        }
                        rest = &rest[17 + n..];
                    }
                }
                0xdb => {
                    let mut rest = segment;
                    while !rest.is_empty() {
                        let (precision, id) = (rest[0] >> 4, rest[0] & 15);
                        if precision != 0 || id > 3 {
                            return None;
                        }
                        jpeg.quant[id as usize] = Some(rest.get(1..65)?.try_into().ok()?);
                        rest = &rest[65..];
                    }
                }
                0xdd => jpeg.restart_interval = u16::from_be_bytes([*segment.first()?, *segment.get(1)?]),
                0xda => {
                    let count = *segment.first()? as usize;
                    for c in segment.get(1..1 + 2 * count)?.as_chunks::<2>().0 {
                        let (dc, ac) = (c[1] >> 4, c[1] & 15);
                        if dc > 1 || ac > 1 {
                            return None;
                        }
                        jpeg.scan.push(ScanComponent { id: c[0], dc, ac });
                    }
                    let [start, end, approximation] = *segment.get(1 + 2 * count..4 + 2 * count)? else { return None };
                    if count != jpeg.components.len() || start != 0 || end != 63 || approximation != 0 || jpeg.scan.iter().zip(&jpeg.components).any(|(s, c)| s.id != c.id) {
                        return None;
                    }
                    jpeg.data = &file[at + 2 + length..];
                    break;
                }
                0xd8 | 0xd9 | 0x01 | 0xd0..=0xd7 => return None,
                _ => {}
            }
            at += 2 + length;
        }
        if jpeg.components.iter().any(|c| jpeg.quant[c.tq as usize].is_none()) {
            return None;
        }
        let standard = |(counts, values): ([u8; 16], &[u8])| Some(Huffman { counts, values: values.to_vec() });
        jpeg.dc[0] = jpeg.dc[0].take().or_else(|| standard((DC_LUMA.0, &DC_LUMA.1)));
        jpeg.dc[1] = jpeg.dc[1].take().or_else(|| standard((DC_CHROMA.0, &DC_CHROMA.1)));
        jpeg.ac[0] = jpeg.ac[0].take().or_else(|| standard((AC_LUMA.0, &AC_LUMA.1)));
        jpeg.ac[1] = jpeg.ac[1].take().or_else(|| standard((AC_CHROMA.0, &AC_CHROMA.1)));
        Some(jpeg)
    }

    /// The most horizontal and vertical sampling of its components.
    pub fn sampling(&self) -> (u8, u8) {
        (self.components.iter().map(|c| c.h).max().unwrap_or(1), self.components.iter().map(|c| c.v).max().unwrap_or(1))
    }

    /// Its colour as `(h, v)` subsampling: (2, 2) for 4:2:0, (2, 1) for
    /// 4:2:2, (1, 1) for 4:4:4; `None` for grey or anything odder.
    pub fn chroma(&self) -> Option<(u8, u8)> {
        let [y, cb, cr] = self.components[..] else { return None };
        (cb.h == cr.h && cb.v == cr.v && y.h % cb.h == 0 && y.v % cb.v == 0).then_some((y.h / cb.h, y.v / cb.v))
    }

    /// The number of MCUs (blocks of 8×8 at the most sampling) it has.
    pub fn mcus(&self) -> u32 {
        let (h, v) = self.sampling();
        let (across, down) = ((self.width as u32).div_ceil(8 * h as u32), (self.height as u32).div_ceil(8 * v as u32));
        if self.components.len() == 1 {
            return (self.width as u32).div_ceil(8) * (self.height as u32).div_ceil(8);
        }
        across * down
    }

    /// As a JPEG again: the same picture, every table written out.
    pub fn write(&self) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        let mut segment = |marker: u8, body: &[u8]| {
            out.extend_from_slice(&[0xff, marker]);
            out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(body);
        };
        for (id, table) in self.quant.iter().enumerate() {
            if let Some(table) = table {
                segment(0xdb, &[&[id as u8][..], table].concat());
            }
        }
        let mut frame = vec![8];
        frame.extend_from_slice(&self.height.to_be_bytes());
        frame.extend_from_slice(&self.width.to_be_bytes());
        frame.push(self.components.len() as u8);
        for c in &self.components {
            frame.extend_from_slice(&[c.id, (c.h << 4) | c.v, c.tq]);
        }
        segment(0xc0, &frame);
        for (class, tables) in [(0u8, &self.dc), (1, &self.ac)] {
            for (id, table) in tables.iter().enumerate() {
                if let Some(table) = table {
                    segment(0xc4, &[&[(class << 4) | id as u8][..], &table.counts, &table.values].concat());
                }
            }
        }
        if self.restart_interval != 0 {
            segment(0xdd, &self.restart_interval.to_be_bytes());
        }
        let mut scan = vec![self.scan.len() as u8];
        for c in &self.scan {
            scan.extend_from_slice(&[c.id, (c.dc << 4) | c.ac]);
        }
        scan.extend_from_slice(&[0, 63, 0]);
        segment(0xda, &scan);
        out.extend_from_slice(self.data);
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn rgb(file: &[u8]) -> Vec<u8> {
        zune_jpeg::JpegDecoder::new(file).decode().unwrap()
    }

    /// Test pictures as webcams send them (made by FFmpeg): 4:2:2 with
    /// restart markers; 4:2:0; and 4:2:0 without its Huffman tables.
    pub const CAMERA_422: &[u8] = include_bytes!("../testdata/camera-422-restarts.jpg");
    pub const CAMERA_420: &[u8] = include_bytes!("../testdata/camera-420.jpg");
    pub const CAMERA_420_NO_TABLES: &[u8] = include_bytes!("../testdata/camera-420-no-tables.jpg");

    /// What's read is all there is: written back, the same picture.
    #[test]
    fn jpegs_are_read_whole() {
        let jpeg = Jpeg::parse(CAMERA_422).unwrap();
        assert_eq!((jpeg.width, jpeg.height, jpeg.chroma(), jpeg.restart_interval), (320, 240, Some((2, 1)), jpeg.restart_interval));
        assert!(jpeg.restart_interval > 0);
        assert_eq!(jpeg.mcus(), 20 * 15);
        assert_eq!(rgb(&jpeg.write()), rgb(CAMERA_422));
        let jpeg = Jpeg::parse(CAMERA_420).unwrap();
        assert_eq!((jpeg.chroma(), jpeg.mcus()), (Some((2, 2)), 20 * 15));
        assert_eq!(rgb(&jpeg.write()), rgb(CAMERA_420));
        let bare = Jpeg::parse(CAMERA_420_NO_TABLES).unwrap();
        assert_eq!(bare.dc, jpeg.dc);
        assert_eq!(bare.ac, jpeg.ac);
        assert_eq!(rgb(&bare.write()), rgb(CAMERA_420));
        let mut progressive = CAMERA_420.to_vec();
        let sof = progressive.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();
        progressive[sof + 1] = 0xc2;
        assert!(Jpeg::parse(&progressive).is_none());
        assert!(Jpeg::parse(b"not a jpeg").is_none());
    }

    /// The standard tables are what they should be: as many values as
    /// their counts say.
    #[test]
    fn the_standard_tables_add_up() {
        for (counts, values) in [(DC_LUMA.0, &DC_LUMA.1[..]), (DC_CHROMA.0, &DC_CHROMA.1[..]), (AC_LUMA.0, &AC_LUMA.1[..]), (AC_CHROMA.0, &AC_CHROMA.1[..])] {
            assert_eq!(counts.iter().map(|&c| c as usize).sum::<usize>(), values.len());
        }
    }
}
