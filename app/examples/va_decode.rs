//! Decodes an Annex-B H.264 file (H.265 if it ends `.hevc` or `.h265`)
//! with Swarm's VA-API decoder, frame by frame: to compare what it gives
//! the driver with what FFmpeg gives (run both with `LIBVA_TRACE`), on
//! the stand-in driver or a real one.
//!
//! cargo run --example va_decode -- file.h264

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use swarm_core::video::Hardware;
    let path = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: va_decode file.h264"))?;
    let h265 = path.ends_with(".hevc") || path.ends_with(".h265");
    let data = std::fs::read(&path)?;
    let va = swarm_app::video_va::VaApi::find()?;
    let codec = if h265 { swarm_core::video::Codec::H265 } else { swarm_core::video::Codec::H264 };
    let mut decoder = va.decoder(codec)?;
    let (mut shown, mut frames) = (0, 0);
    for frame in access_units(&data, h265) {
        frames += 1;
        if let Some(picture) = decoder.decode(&frame)? {
            shown += 1;
            if shown == 1 {
                println!("{}×{}", picture.width, picture.height);
            }
        }
    }
    println!("{shown} of {frames} frames shown");
    Ok(())
}

/// The stream's frames: a new one at a parameter set, delimiter or SEI
/// after a slice, or a slice starting a picture (at macroblock 0; with
/// H.265, first in its picture).
#[cfg(target_os = "linux")]
fn access_units(stream: &[u8], h265: bool) -> Vec<Vec<u8>> {
    use swarm_app::h265::{is_slice, nal_type};
    let mut units = Vec::new();
    let mut current = Vec::new();
    let mut had_slice = false;
    for nal in swarm_app::h264::nal_units(stream) {
        let (slice, first, other) = if h265 {
            let kind = nal_type(nal).unwrap_or(63);
            (is_slice(kind), nal.get(2).is_some_and(|b| b & 0x80 != 0), (32..=39).contains(&kind))
        } else {
            let kind = nal[0] & 0x1f;
            (kind == 1 || kind == 5, nal.get(1).is_some_and(|b| b & 0x80 != 0), (6..=9).contains(&kind))
        };
        if had_slice && ((slice && first) || other) {
            units.push(std::mem::take(&mut current));
            had_slice = false;
        }
        had_slice |= slice;
        current.extend_from_slice(&[0, 0, 0, 1]);
        current.extend_from_slice(nal);
    }
    if !current.is_empty() {
        units.push(current);
    }
    units
}

#[cfg(not(target_os = "linux"))]
fn main() {}
