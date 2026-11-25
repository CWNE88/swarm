//! Decodes a file of JPEGs (a camera's MJPEG, one after another) on the
//! chip — NVIDIA's NVDEC, or VA-API's — comparing each with software's
//! decoding and timing both.
//!
//! cargo run --release --example va_jpeg -- frames.mjpeg

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};
    let path = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: va_jpeg frames.mjpeg"))?;
    let data = std::fs::read(path)?;
    let mut decoder = swarm_app::hardware::JpegOnChip::new()?;
    let (mut chip_time, mut soft_time, mut frames, mut worst) = (Duration::ZERO, Duration::ZERO, 0, f64::MAX);
    let cpu = || {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        let micros = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
        Duration::from_micros(micros(usage.ru_utime) + micros(usage.ru_stime))
    };
    let (mut chip_cpu, mut soft_cpu) = (Duration::ZERO, Duration::ZERO);
    let mut start = 0;
    while let Some(end) = data[start..].windows(2).position(|w| w == [0xff, 0xd9]).map(|e| start + e + 2) {
        let frame = &data[start..end];
        start = end;
        let (began, used) = (Instant::now(), cpu());
        let chip = decoder.decode(frame)?;
        chip_time += began.elapsed();
        chip_cpu += cpu() - used;
        let (began, used) = (Instant::now(), cpu());
        let soft = swarm_app::camera::mjpeg_to_rgb(frame).ok_or_else(|| anyhow::anyhow!("software can't decode frame {frames}"))?;
        let soft = soft.to_i420();
        soft_time += began.elapsed();
        soft_cpu += cpu() - used;
        let (a, b) = (chip.planes().0, soft.planes().0);
        let mse = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
        worst = worst.min(10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10());
        frames += 1;
        if start + 2 > data.len() {
            break;
        }
    }
    let each = |t: Duration| t.as_secs_f64() * 1000.0 / frames.max(1) as f64;
    println!("{frames} frames on {}: {:.1} ms each on the chip ({:.1} ms of the processor's), {:.1} ms in software to I420 ({:.1} ms); brightness at worst {worst:.1} dB from software's", decoder.name(), each(chip_time), each(chip_cpu), each(soft_time), each(soft_cpu));
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {}
