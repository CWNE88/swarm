//! Drawing a picture (a camera frame, for now) with nothing but text, so
//! it works on any console, down to the plain Linux one: each character
//! cell is the "upper half block" `▀`, its foreground colour the top
//! pixel and its background the bottom one — two pixels per cell.
//!
//! How many colours that gets depends on the console (`ColourMode`): true
//! colour on most desktop terminals, the 256-colour palette, the 16 basic
//! colours (with dithering, so shades still read), or — with no colour
//! at all — brightness drawn as ASCII characters.

use swarm_app::camera::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColourMode {
    TrueColour,
    Ansi256,
    Ansi16,
    Ascii,
}

impl ColourMode {
    /// From `SWARM_COLOURS` (`truecolour`, `256`, `16`, `ascii`) if set,
    /// otherwise from what the environment says the console can do (the
    /// environment's own names are American: `COLORTERM=truecolor`).
    pub fn detect() -> ColourMode {
        let get = |name| std::env::var(name).unwrap_or_default().to_ascii_lowercase();
        if let Some(mode) = ColourMode::parse(&get("SWARM_COLOURS")) {
            return mode;
        }
        if cfg!(windows) {
            return ColourMode::TrueColour;
        }
        let (term, colorterm) = (get("TERM"), get("COLORTERM"));
        if colorterm == "truecolor" || colorterm == "24bit" {
            ColourMode::TrueColour
        } else if term.contains("256color") {
            ColourMode::Ansi256
        } else if term.is_empty() || term == "dumb" {
            ColourMode::Ascii
        } else {
            ColourMode::Ansi16
        }
    }

    pub fn parse(s: &str) -> Option<ColourMode> {
        match s {
            "truecolour" | "truecolor" | "24bit" => Some(ColourMode::TrueColour),
            "256" => Some(ColourMode::Ansi256),
            "16" => Some(ColourMode::Ansi16),
            "ascii" => Some(ColourMode::Ascii),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ColourMode::TrueColour => "true colour",
            ColourMode::Ansi256 => "256 colours",
            ColourMode::Ansi16 => "16 colours",
            ColourMode::Ascii => "ASCII",
        }
    }

    /// The next mode down, wrapping round — for trying them in turn.
    pub fn next(self) -> ColourMode {
        match self {
            ColourMode::TrueColour => ColourMode::Ansi256,
            ColourMode::Ansi256 => ColourMode::Ansi16,
            ColourMode::Ansi16 => ColourMode::Ascii,
            ColourMode::Ascii => ColourMode::TrueColour,
        }
    }
}

/// `frame` drawn into whatever area it's rendered to, as large as fits
/// with its shape kept, centred. `mirror` flips it left to right — how a
/// view of yourself looks natural.
pub struct Picture<'a> {
    pub frame: &'a Frame,
    pub mode: ColourMode,
    pub mirror: bool,
}

/// Brightness ramp for `ColourMode::Ascii`, darkest first.
const RAMP: &[u8] = b" .:-=+*#%@";

impl Widget for Picture<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let frame = self.frame;
        if area.width == 0 || area.height == 0 || frame.width == 0 || frame.height == 0 {
            return;
        }
        let per_cell = if self.mode == ColourMode::Ascii { 1 } else { 2 };
        let aspect = frame.width as f32 / frame.height as f32;
        let rows_px = area.height as f32 * per_cell as f32;
        let squash = if per_cell == 1 { 2.0 } else { 1.0 };
        let mut width = area.width as f32;
        let mut height = width / aspect / squash;
        if height > rows_px {
            height = rows_px;
            width = height * aspect * squash;
        }
        let (width, height) = ((width as usize).max(1), (height as usize).max(1));
        let left = area.x + (area.width - width as u16) / 2;
        let top = area.y + ((rows_px as usize - height) / per_cell) as u16 / 2;

        let sample = |tx: usize, ty: usize| -> [u8; 3] {
            let tx = if self.mirror { width - 1 - tx } else { tx };
            let (x0, x1) = (tx * frame.width / width, ((tx + 1) * frame.width / width).max(tx * frame.width / width + 1));
            let (y0, y1) = (ty * frame.height / height, ((ty + 1) * frame.height / height).max(ty * frame.height / height + 1));
            let mut sum = [0u32; 3];
            let mut n = 0;
            for y in y0..y1.min(frame.height) {
                for x in x0..x1.min(frame.width) {
                    let p = frame.pixel(x, y);
                    for c in 0..3 {
                        sum[c] += p[c] as u32;
                    }
                    n += 1;
                }
            }
            let n = n.max(1);
            [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]
        };

        for cy in 0..(height / per_cell).max(1) {
            for cx in 0..width {
                let Some(cell) = buf.cell_mut((left + cx as u16, top + cy as u16)) else { continue };
                if self.mode == ColourMode::Ascii {
                    let p = sample(cx, cy);
                    let level = luminance(p) as usize * (RAMP.len() - 1) / 255;
                    cell.set_char(RAMP[level] as char).set_fg(Color::Reset).set_bg(Color::Reset);
                    continue;
                }
                let upper = sample(cx, cy * 2);
                let lower = if cy * 2 + 1 < height { sample(cx, cy * 2 + 1) } else { upper };
                let (fg, bg) = match self.mode {
                    ColourMode::TrueColour => (rgb(upper), rgb(lower)),
                    ColourMode::Ansi256 => (Color::Indexed(nearest_256(upper)), Color::Indexed(nearest_256(lower))),
                    _ => (ansi16(dither(upper, cx, cy * 2), 16), ansi16(dither(lower, cx, cy * 2 + 1), 8)),
                };
                cell.set_char('▀').set_fg(fg).set_bg(bg);
            }
        }
    }
}

fn rgb(p: [u8; 3]) -> Color {
    Color::Rgb(p[0], p[1], p[2])
}

fn luminance(p: [u8; 3]) -> u8 {
    ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8
}

fn distance(a: [u8; 3], b: [u8; 3]) -> u32 {
    (0..3).map(|i| (a[i] as i32 - b[i] as i32).pow(2) as u32).sum()
}

/// The xterm 256-colour palette entry nearest `p`: from the 6×6×6 colour
/// cube (16–231) or the grey ramp (232–255), whichever is closer.
fn nearest_256(p: [u8; 3]) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |c: u8| (0..6).min_by_key(|&i| (LEVELS[i] as i32 - c as i32).abs()).expect("six levels");
    let (r, g, b) = (level(p[0]), level(p[1]), level(p[2]));
    let cube = [LEVELS[r], LEVELS[g], LEVELS[b]];
    let grey_step = ((luminance(p) as i32 - 8).max(0) / 10).min(23) as u8;
    let grey_value = 8 + grey_step * 10;
    if distance(p, [grey_value; 3]) < distance(p, cube) {
        232 + grey_step
    } else {
        16 + 36 * r as u8 + 6 * g as u8 + b as u8
    }
}

/// The basic 16 colours, roughly as consoles show them.
const ANSI16: [([u8; 3], Color); 16] = [
    ([0, 0, 0], Color::Black),
    ([170, 0, 0], Color::Red),
    ([0, 170, 0], Color::Green),
    ([170, 85, 0], Color::Yellow),
    ([0, 0, 170], Color::Blue),
    ([170, 0, 170], Color::Magenta),
    ([0, 170, 170], Color::Cyan),
    ([170, 170, 170], Color::Gray),
    ([85, 85, 85], Color::DarkGray),
    ([255, 85, 85], Color::LightRed),
    ([85, 255, 85], Color::LightGreen),
    ([255, 255, 85], Color::LightYellow),
    ([85, 85, 255], Color::LightBlue),
    ([255, 85, 255], Color::LightMagenta),
    ([85, 255, 255], Color::LightCyan),
    ([255, 255, 255], Color::White),
];

/// The nearest of the first `count` (8 or 16) basic colours.
fn ansi16(p: [u8; 3], count: usize) -> Color {
    ANSI16[..count].iter().min_by_key(|(c, _)| distance(p, *c)).expect("non-empty").1
}

/// Ordered (Bayer 4×4) dithering: nudges each pixel up or down by a
/// position-dependent amount before it's matched to a few colours, so
/// in-between shades come out as a fine pattern instead of flat bands.
fn dither(p: [u8; 3], x: usize, y: usize) -> [u8; 3] {
    const BAYER: [[i32; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    let offset = (BAYER[y % 4][x % 4] - 8) * 6;
    p.map(|c| (c as i32 + offset).clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, p: [u8; 3]) -> Frame {
        Frame::rgb(w, h, p.repeat(w * h))
    }

    #[test]
    fn palette_matching_lands_on_the_obvious_colours() {
        assert_eq!(nearest_256([255, 0, 0]), 196);
        assert_eq!(nearest_256([0, 0, 0]), 16);
        assert!((232..=255).contains(&nearest_256([128, 128, 128])));
        assert_eq!(ansi16([250, 250, 250], 16), Color::White);
        assert_eq!(ansi16([250, 250, 250], 8), Color::Gray);
        assert_eq!(ansi16([0, 160, 0], 16), Color::Green);
    }

    #[test]
    fn a_frame_fills_the_area_with_its_shape_kept_and_centred() {
        let frame = solid(40, 20, [255, 0, 0]);
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        Picture { frame: &frame, mode: ColourMode::TrueColour, mirror: false }.render(area, &mut buf);
        assert_eq!(buf[(0, 4)].symbol(), " ");
        assert_eq!(buf[(0, 5)].symbol(), "▀");
        assert_eq!(buf[(0, 5)].fg, Color::Rgb(255, 0, 0));
        assert_eq!(buf[(39, 14)].symbol(), "▀");
        assert_eq!(buf[(0, 15)].symbol(), " ");
    }

    #[test]
    fn ascii_mode_draws_brightness_with_characters() {
        let frame = solid(8, 8, [255, 255, 255]);
        let area = Rect::new(0, 0, 8, 4);
        let mut buf = Buffer::empty(area);
        Picture { frame: &frame, mode: ColourMode::Ascii, mirror: false }.render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "@");
    }

    #[test]
    fn mirroring_flips_left_and_right() {
        let mut frame = solid(2, 2, [0, 0, 0]);
        frame.data[0..3].copy_from_slice(&[255, 255, 255]);
        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        Picture { frame: &frame, mode: ColourMode::TrueColour, mirror: true }.render(area, &mut buf);
        assert_eq!(buf[(1, 0)].fg, Color::Rgb(255, 255, 255));
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(0, 0, 0));
    }
}
