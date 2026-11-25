//! The test pattern a camera named "test" sends, and the chip checks
//! and tests encode.

use swarm_core::video::Picture;

/// Colour bars, a grey ramp, and a ball bouncing across them — something
/// that shows both colour accuracy and whether motion is smooth.
/// The test pattern's still part, drawn once: colour bars over a grey ramp.
pub fn test_background(want: (u32, u32)) -> Picture {
    let (width, height) = (want.0 as usize, want.1 as usize);
    const BARS: [[u8; 3]; 8] = [[255, 255, 255], [255, 255, 0], [0, 255, 255], [0, 255, 0], [255, 0, 255], [255, 0, 0], [0, 0, 255], [0, 0, 0]];
    let mut rgb = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for x in 0..width {
            rgb.extend(if y < height * 2 / 3 {
                BARS[x * BARS.len() / width]
            } else {
                let g = (x * 255 / width) as u8;
                [g, g, g]
            });
        }
    }
    Picture::rgb(width, height, rgb)
}

/// The test pattern at `t` seconds: `background` with the ball on it
/// (only the ball's square is worked out each frame, so even 1080p keeps
/// 30 a second in a debug build).
pub fn test_pattern(t: f32, background: &Picture) -> Picture {
    let (width, height) = (background.width, background.height);
    let (bx, by) = (
        (width as f32 / 2.0) + (width as f32 / 2.5) * (t * 1.3).sin(),
        (height as f32 / 2.0) + (height as f32 / 3.0) * (t * 1.9).cos(),
    );
    let radius = width as f32 / 32.0;
    let mut frame = background.clone();
    let span = |centre: f32, most: usize| ((centre - radius).max(0.0) as usize, ((centre + radius).ceil().max(0.0) as usize).min(most));
    let ((x0, x1), (y0, y1)) = (span(bx, width), span(by, height));
    for y in y0..y1 {
        for x in x0..x1 {
            let (dx, dy) = (x as f32 - bx, y as f32 - by);
            if dx * dx + dy * dy < radius * radius {
                let i = (y * width + x) * 3;
                frame.data[i..i + 3].copy_from_slice(&[255, 128, 0]);
            }
        }
    }
    frame
}

/// The test pattern with grain all over, as a camera's sensor gives in
/// ordinary room light (each pixel up to 6 levels either way, fresh each
/// picture): an encoder spends all the bitrate it's given on it, as on a
/// real camera's pictures, where the plain pattern takes a megabit even
/// at 4K. For trying a call's rate control on a link
/// (`SWARM_CAMERA=test-busy`).
pub fn test_pattern_busy(t: f32, background: &Picture) -> Picture {
    let mut frame = test_pattern(t, background);
    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ (t * 1000.0) as u64;
    for chunk in frame.data.chunks_mut(8) {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let bits = state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        for (pixel, noise) in chunk.iter_mut().zip(bits) {
            *pixel = pixel.saturating_add(noise % 13).saturating_sub(6);
        }
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grain is small and different each picture.
    #[test]
    fn the_busy_pattern_is_the_pattern_with_grain() {
        let background = test_background((64, 36));
        let (plain, busy, later) = (test_pattern(1.0, &background), test_pattern_busy(1.0, &background), test_pattern_busy(1.04, &background));
        assert!(plain.data.iter().zip(&busy.data).all(|(a, b)| a.abs_diff(*b) <= 6));
        assert!(plain.data.iter().zip(&busy.data).filter(|(a, b)| a != b).count() > plain.data.len() / 2);
        assert_ne!(busy.data, later.data);
    }
}
