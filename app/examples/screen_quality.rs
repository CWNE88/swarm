//! How sharp a shared screen comes through, encoded by OpenH264's
//! screen mode (as Swarm sends a screen) and by the graphics chip (NVENC,
//! H.264 and H.265), at the same size, pace and bitrate: what decides
//! whether screens may go to the chip.
//!
//! cargo run --release -p swarm-app --example screen_quality -- still.ppm scroll.ppm
//!
//! `still.ppm` is a 1920×1080 screen (an xterm of code, say), sent as it
//! stands for two seconds: the dB of the last picture shown, and of the
//! first (how it looks the moment something changes). `scroll.ppm` is
//! 1920 wide and taller than a screen: a 1080-high window onto it moves
//! down 300 pixels a second for four seconds, the average dB over every
//! picture the viewer had on screen at each moment. dB are against the
//! screen itself, at 1920×1080, of all three planes (I420).

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType};
    use openh264::formats::YUVSlices;
    use swarm_core::video::{Codec, Hardware, Picture, Shape};

    fn ppm(path: &str) -> anyhow::Result<Picture> {
        let bytes = std::fs::read(path)?;
        let mut fields = Vec::new();
        let mut at = 0;
        while fields.len() < 4 {
            while bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            let start = at;
            while !bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            fields.push(String::from_utf8_lossy(&bytes[start..at]).to_string());
        }
        anyhow::ensure!(fields[0] == "P6" && fields[3] == "255", "{path}: not a P6 PPM");
        let (w, h): (usize, usize) = (fields[1].parse()?, fields[2].parse()?);
        Ok(Picture::rgb(w, h, bytes[at + 1..][..w * h * 3].to_vec()))
    }

    fn crop(p: &Picture, top: usize) -> Picture {
        Picture::rgb(1920, 1080, p.data[top * 1920 * 3..][..1920 * 1080 * 3].to_vec())
    }

    fn psnr(a: &Picture, b: &Picture) -> f64 {
        let (a, b) = (a.to_i420(), b.to_i420());
        let squared: f64 = a.data.iter().zip(&b.data).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum();
        10.0 * (255.0f64.powi(2) / (squared / a.data.len() as f64).max(1e-9)).log10()
    }

    fn openh264_picture(decoder: &mut openh264::decoder::Decoder, frame: &[u8]) -> Option<Picture> {
        use openh264::formats::YUVSource;
        let yuv = decoder.decode(frame).ok()??;
        let (w, h) = yuv.dimensions();
        let (ys, us, vs) = yuv.strides();
        let mut data = Vec::new();
        (0..h).for_each(|r| data.extend_from_slice(&yuv.y()[r * ys..][..w]));
        (0..h / 2).for_each(|r| data.extend_from_slice(&yuv.u()[r * us..][..w / 2]));
        (0..h / 2).for_each(|r| data.extend_from_slice(&yuv.v()[r * vs..][..w / 2]));
        Some(Picture::i420(w, h, data))
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Way {
        Soft,
        Chip(Codec),
    }

    enum Enc {
        Soft(Box<Encoder>),
        Chip(Box<dyn swarm_core::video::HardEncoder>),
    }

    enum Dec {
        Soft(openh264::decoder::Decoder),
        Chip(Box<dyn swarm_core::video::HardDecoder>),
    }

    let nvidia = swarm_app::video_nv::Nvidia::find()?;
    let make = |way: Way, shape: Shape, bitrate: u32| -> anyhow::Result<(Enc, Dec)> {
        Ok(match way {
            Way::Soft => {
                let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4) as u16;
                let config = EncoderConfig::new()
                    .num_threads(threads)
                    .max_slice_len(1200)
                    .complexity(Complexity::Low)
                    .usage_type(UsageType::ScreenContentRealTime)
                    .rate_control_mode(RateControlMode::Bitrate)
                    .bitrate(BitRate::from_bps(bitrate))
                    .max_frame_rate(FrameRate::from_hz(shape.fps as f32))
                    .skip_frames(true)
                    .intra_frame_period(IntraFramePeriod::from_num_frames(shape.fps * 20));
                let encoder = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)?;
                (Enc::Soft(Box::new(encoder)), Dec::Soft(openh264::decoder::Decoder::new()?))
            }
            Way::Chip(codec) => {
                let encoder = nvidia.encoder(shape, bitrate, codec)?;
                let decoder = match codec {
                    Codec::H264 => Dec::Soft(openh264::decoder::Decoder::new()?),
                    Codec::H265 => Dec::Chip(nvidia.decoder(codec)?),
                };
                (Enc::Chip(encoder), decoder)
            }
        })
    };

    let run = |way: Way, shape: Shape, bitrate: u32, frames: u32, source: &dyn Fn(u32) -> Picture| -> anyhow::Result<(Vec<f64>, usize)> {
        let (mut enc, mut dec) = make(way, shape, bitrate)?;
        let mut shown: Option<Picture> = None;
        let (mut alike, mut bytes) = (Vec::new(), 0);
        for i in 0..frames {
            let screen = source(i);
            let sized = screen.to_i420().scaled(shape.width, shape.height);
            let frame = match &mut enc {
                Enc::Soft(e) => {
                    let (y, u, v) = sized.planes();
                    let half = shape.width / 2;
                    let yuv = YUVSlices::new((y, u, v), (shape.width, shape.height), (shape.width, half, half));
                    e.encode(&yuv)?.to_vec()
                }
                Enc::Chip(e) => e.encode(&sized, i == 0)?,
            };
            bytes += frame.len();
            if !frame.is_empty() {
                let picture = match &mut dec {
                    Dec::Soft(d) => openh264_picture(d, &frame),
                    Dec::Chip(d) => d.decode(&frame)?,
                };
                if let Some(p) = picture {
                    shown = Some(p.scaled(1920, 1080));
                }
            }
            alike.push(shown.as_ref().map_or(0.0, |s| psnr(s, &screen)));
        }
        Ok((alike, bytes))
    };

    let args: Vec<String> = std::env::args().collect();
    let still = ppm(&args[1])?;
    let tall = ppm(&args[2])?;
    anyhow::ensure!((still.width, still.height) == (1920, 1080) && tall.width == 1920 && tall.height > 1080 + 1200);

    let ways = [(Way::Soft, "OpenH264 screen"), (Way::Chip(Codec::H264), "NVENC H.264"), (Way::Chip(Codec::H265), "NVENC H.265")];
    let cases: &[(u32, usize, usize, u32)] = &[
        (2_500_000, 1920, 1080, 25),
        (1_500_000, 1920, 1080, 15),
        (800_000, 1920, 1080, 8),
        (500_000, 1920, 1080, 5),
        (500_000, 1280, 720, 8),
        (300_000, 1920, 1080, 3),
        (300_000, 960, 540, 6),
        (160_000, 1920, 1080, 2),
        (160_000, 960, 540, 3),
    ];
    println!("{:>10} {:>11} {:>16} | {:>9} {:>9} {:>9} {:>9} | {:>9}", "kbit/s", "shape", "way", "still dB", "first dB", "at 1 s", "kbit/s", "scroll dB");
    for &(bitrate, w, h, fps) in cases {
        let shape = Shape { width: w, height: h, fps };
        for (way, name) in ways {
            if (w < 640) && way != Way::Soft {
                continue;
            }
            let still_run = run(way, shape, bitrate, fps * 2, &|_| still.clone());
            let scroll_run = run(way, shape, bitrate, fps * 4, &|i| crop(&tall, (i as usize * 300 / fps as usize).min(tall.height - 1080)));
            match (still_run, scroll_run) {
                (Ok((s, bytes)), Ok((r, _))) => {
                    let first = s.iter().copied().find(|d| *d > 0.0).unwrap_or(0.0);
                    let scroll = r.iter().sum::<f64>() / r.len() as f64;
                    let kbits = bytes as f64 * 8.0 / 2.0 / 1000.0;
                    println!("{:>10} {:>11} {:>16} | {:>9.1} {:>9.1} {:>9.1} {:>9.0} | {:>9.1}", bitrate / 1000, format!("{w}×{h}@{fps}"), name, s.last().unwrap(), first, s[fps as usize - 1], kbits, scroll);
                }
                (a, b) => println!("{:>10} {:>11} {:>16} | failed: {:?} {:?}", bitrate / 1000, format!("{w}×{h}@{fps}"), name, a.err(), b.err()),
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {}
