//! Encodes a moving test picture with Swarm's VA-API encoder, a full
//! frame asked for at 20 and the bitrate halved at 30: to see what it
//! gives the driver (run with `LIBVA_TRACE`) and what comes out.
//!
//! cargo run --example va_encode -- [width height [out.h264]]

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use swarm_core::video::{Hardware, Shape};
    let args: Vec<String> = std::env::args().collect();
    let width = args.get(1).map_or(Ok(960), |a| a.parse())?;
    let height = args.get(2).map_or(Ok(540), |a| a.parse())?;
    let va = swarm_app::video_va::VaApi::find()?;
    let mut encoder = va.encoder(Shape { width, height, fps: 30 }, 1_000_000, swarm_core::video::Codec::H264)?;
    let background = swarm_app::camera::test_background((width as u32, height as u32));
    let mut out = Vec::new();
    for i in 0..40 {
        if i == 30 {
            encoder.set_bitrate(500_000)?;
        }
        let picture = swarm_app::camera::test_pattern(i as f32 / 30.0, &background);
        let frame = encoder.encode(&picture, i == 20)?;
        println!("picture {i}: {} bytes{}", frame.len(), if swarm_core::video::has_idr(&frame) { ", full" } else { "" });
        out.extend_from_slice(&frame);
    }
    if let Some(path) = args.get(3) {
        std::fs::write(path, out)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {}
