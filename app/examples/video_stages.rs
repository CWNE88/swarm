//! How long each stage of a video call takes a picture, on this computer:
//! `cargo run --release -p swarm-app --example video_stages -- <file.mjpg>`
//! where the file is frames from a camera (`v4l2-ctl --stream-to`).

use openh264::decoder::Decoder;
use openh264::encoder::Encoder;
use openh264::formats::{RgbSliceU8, YUVBuffer, YUVSource};
use std::time::Instant;
use swarm_core::video::{Picture, VideoDecoder, VideoEncoder};

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (f64, T) {
    let mut out = f();
    let start = Instant::now();
    for _ in 0..runs {
        out = f();
    }
    (start.elapsed().as_secs_f64() * 1000.0 / runs as f64, out)
}

fn main() {
    let path = std::env::args().nth(1).expect("a file of MJPEG frames");
    let bytes = std::fs::read(path).unwrap();
    let start = bytes.windows(3).rposition(|w| w == [0xff, 0xd8, 0xff]).unwrap();
    let jpeg = &bytes[start..];
    let (t, picture) = time(20, || {
        let mut d = zune_jpeg::JpegDecoder::new(jpeg);
        let rgb = d.decode().unwrap();
        let info = d.info().unwrap();
        Picture::rgb(info.width as usize, info.height as usize, rgb)
    });
    let (w, h) = (picture.width, picture.height);
    println!("{}x{} camera", picture.width, picture.height);
    println!("  MJPEG → RGB (zune-jpeg)       {t:6.2} ms");
    let i420 = picture.to_i420().into_owned();
    let (t, _) = time(20, || i420.scaled(w * 3 / 4 / 16 * 16, h * 3 / 4 / 16 * 16));
    println!("  scale I420 to 3/4             {t:6.2} ms");
    let (t, _) = time(20, || i420.to_rgb().into_owned());
    println!("  I420 → RGB (ours)             {t:6.2} ms");
    let (t, _) = time(20, || i420.scaled(w / 3, h / 3).to_rgb().into_owned());
    println!("  inset: I420 shrunk, then RGB  {t:6.2} ms");
    let (t, _) = time(20, || picture.scaled(w * 3 / 4 / 16 * 16, h * 3 / 4 / 16 * 16));
    println!("  scale RGB to 3/4              {t:6.2} ms");
    let (t, _) = time(20, || picture.to_i420().scaled(w * 3 / 4 / 16 * 16, h * 3 / 4 / 16 * 16));
    println!("  RGB → I420, then scale to 3/4 {t:6.2} ms");
    let (t, _) = time(20, || picture.scaled(w * 3 / 4 / 16 * 16, h * 3 / 4 / 16 * 16).to_i420().into_owned());
    println!("  scale RGB to 3/4, then → I420 {t:6.2} ms");
    let (t, yuv) = time(20, || YUVBuffer::from_rgb8_source(RgbSliceU8::new(&picture.data, (w, h))));
    println!("  RGB → YUV (openh264)          {t:6.2} ms");
    let mut encoder = Encoder::new().unwrap();
    let (t, _) = time(20, || encoder.encode(&yuv).unwrap().to_vec());
    println!("  encode, plain openh264        {t:6.2} ms");
    let mut pictures = Vec::new();
    let mut at = 0;
    while let Some(next) = bytes[at..].windows(3).position(|w| w == [0xff, 0xd8, 0xff]) {
        let from = at + next;
        let end = bytes[from + 3..].windows(3).position(|w| w == [0xff, 0xd8, 0xff]).map_or(bytes.len(), |e| from + 3 + e);
        let mut d = zune_jpeg::JpegDecoder::new(&bytes[from..end]);
        if let (Ok(rgb), Some(info)) = (d.decode(), d.info()) {
            pictures.push(Picture::rgb(info.width as usize, info.height as usize, rgb));
        }
        at = end;
    }
    let mut ours = VideoEncoder::new(if w >= 1920 { 4_000_000 } else { 2_000_000 }).unwrap();
    let mut stream = Vec::new();
    let mut i = 0;
    let (t, _) = time(100, || {
        let k = i % (2 * pictures.len() - 2);
        let k = if k < pictures.len() { k } else { 2 * pictures.len() - 2 - k };
        i += 1;
        let pieces = ours.encode(&pictures[k], 1000).unwrap();
        stream.push(pieces);
    });
    println!("  VideoEncoder::encode (all)    {t:6.2} ms  at {:?}", ours.shape());
    let mut decoder = Decoder::new().unwrap();
    let frames: Vec<Vec<u8>> = stream.iter().map(|p| p.iter().flat_map(|p| p.data.clone()).collect()).collect();
    let mut n = 0;
    let (t, _) = time(frames.len() - 1, || {
        let f = &frames[n % frames.len()];
        n += 1;
        decoder.decode(f).ok().flatten().map(|y| y.dimensions())
    });
    println!("  decode, plain openh264        {t:6.2} ms");
    let mut decoder = Decoder::new().unwrap();
    let mut last = None;
    for f in &frames {
        if let Ok(Some(y)) = decoder.decode(f) {
            let (dw, dh) = y.dimensions();
            let (t, _) = time(20, || {
                let mut rgb = vec![0u8; dw * dh * 3];
                y.write_rgb8(&mut rgb);
                rgb
            });
            last = Some((t, dw, dh));
        }
    }
    let (t, dw, dh) = last.unwrap();
    println!("  YUV → RGB (openh264)          {t:6.2} ms  ({dw}x{dh})");
    let mut vd = VideoDecoder::new().unwrap();
    let pieces: Vec<_> = stream.into_iter().flatten().collect();
    let count = pieces.iter().map(|p| p.frame).collect::<std::collections::BTreeSet<_>>().len();
    let start = Instant::now();
    let mut shown = 0;
    for p in pieces {
        if vd.push(p).picture.is_some() {
            shown += 1;
        }
    }
    println!("  VideoDecoder::push (all)      {:6.2} ms  ({shown} of {count} shown)", start.elapsed().as_secs_f64() * 1000.0 / count as f64);
}
