//! The sound a message makes on a computer, where there are no
//! notifications (the phone has its own): one the person chose (a WAV,
//! Ogg, FLAC, MP3 or M4A file, its first `MOST_SECONDS`) or Swarm's own,
//! played once on the speaker. When it's made is the window's business
//! (see swarm_desktop's controller).
//!
//! Everything's mono at 48kHz here, as a call's audio is, and turned into
//! whatever the speaker takes as it's played.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The rate sounds are kept at.
const RATE: u32 = swarm_core::call::SAMPLE_RATE;
/// The most of a file that's played: a sound, not a song.
pub const MOST_SECONDS: usize = 10;
/// The largest file taken.
pub const MOST_BYTES: u64 = 50 * 1024 * 1024;
/// Where the chosen file is kept (its path), in settings.
pub const SETTING: &str = "message_sound";

/// A sound: mono, at `RATE`.
#[derive(Clone, Debug, PartialEq)]
pub struct Sound(pub Vec<f32>);

/// Swarm's own, until the person picks theirs: two notes, rising, each
/// fading out.
pub fn built_in() -> Sound {
    let mut samples = Vec::new();
    for (hz, seconds) in [(880.0f32, 0.12f32), (1318.5, 0.28)] {
        let n = (seconds * RATE as f32) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let attack = (i as f32 / (0.004 * RATE as f32)).min(1.0);
            let fade = (1.0 - i as f32 / n as f32).powi(2);
            samples.push(0.35 * attack * fade * (std::f32::consts::TAU * hz * t).sin());
        }
    }
    Sound(samples)
}

/// A sound file, as a `Sound`: its first `MOST_SECONDS`, its channels
/// mixed together.
pub fn from_file(path: &Path) -> Result<Sound> {
    read(path, MOST_SECONDS)
}

/// A sound file's first `seconds`, as `from_file` (the ringtone's are
/// longer).
pub(crate) fn read(path: &Path, seconds: usize) -> Result<Sound> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if file.metadata()?.len() > MOST_BYTES {
        bail!("{} is over {} MB: a sound, not a song", path.display(), MOST_BYTES / 1024 / 1024);
    }
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }
    let source = MediaSourceStream::new(Box::new(file), Default::default());
    let probed = symphonia::default::get_probe()
        .format(&hint, source, &FormatOptions::default(), &MetadataOptions::default())
        .with_context(|| format!("{} isn't a sound Swarm can play", path.display()))?;
    let mut format = probed.format;
    let track = format.default_track().context("there's no sound in it")?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).context("its sound isn't one Swarm can play")?;
    let most = seconds * RATE as usize;
    let mut samples = Vec::new();
    let mut resample: Option<(u32, crate::audio::Linear)> = None;
    while samples.len() < most {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired) => break,
            Err(e) => return Err(e).context("reading it"),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(Error::DecodeError(_)) => continue,
            Err(e) => return Err(e).context("decoding it"),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buffer.copy_interleaved_ref(decoded);
        let (rate, linear) = resample.get_or_insert_with(|| (spec.rate, crate::audio::Linear::new(spec.rate.max(1), RATE)));
        if *rate != spec.rate {
            *linear = crate::audio::Linear::new(spec.rate.max(1), RATE);
            *rate = spec.rate;
        }
        for frame in buffer.samples().chunks(channels) {
            linear.push(frame.iter().sum::<f32>() / channels as f32, &mut samples);
        }
    }
    samples.truncate(most);
    if samples.iter().all(|s| s.abs() < 1e-4) {
        bail!("{} is silent", path.display());
    }
    for s in &mut samples {
        *s = s.clamp(-1.0, 1.0);
    }
    Ok(Sound(samples))
}

/// The sound chosen in `settings` (Swarm's own if none, or if it can't
/// be read now), and its name ("" for Swarm's own).
pub fn chosen(settings: &Path) -> (Sound, String) {
    match swarm_core::settings::get(settings, SETTING).filter(|p| !p.is_empty()) {
        Some(path) => match from_file(Path::new(&path)) {
            Ok(sound) => (sound, name_of(&path)),
            Err(_) => (built_in(), String::new()),
        },
        None => (built_in(), String::new()),
    }
}

/// What the chosen sound's called, as Settings shows it ("" for Swarm's
/// own), without reading it.
pub fn chosen_name(settings: &Path) -> String {
    swarm_core::settings::get(settings, SETTING).filter(|p| !p.is_empty()).map(|p| name_of(&p)).unwrap_or_default()
}

pub(crate) fn name_of(path: &str) -> String {
    Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// A sound's playing (one at a time: a second asked for meanwhile isn't).
static PLAYING: AtomicBool = AtomicBool::new(false);

/// Plays the sound chosen in `settings` once, on the speaker, on a thread
/// of its own (reading the file there too).
pub fn play_chosen(settings: &Path) {
    let settings = settings.to_path_buf();
    play_lazily(move || chosen(&settings).0);
}

/// Plays `sound` once, as `play_chosen`.
pub fn play(sound: Sound) {
    play_lazily(move || sound);
}

/// Plays the sound `sound` makes, once, as `play_chosen` (made on the
/// playing thread).
pub fn play_lazily(sound: impl FnOnce() -> Sound + Send + 'static) {
    if PLAYING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        if let Err(e) = play_now(&sound()) {
            eprintln!("{} a sound couldn't be played: {e:#}", crate::unix_now());
        }
        PLAYING.store(false, Ordering::Release);
    });
}

/// Plays `sound` on the default speaker, waiting until it's done.
fn play_now(sound: &Sound) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = host.default_output_device().context("no speaker")?;
    let (config, channels) = crate::audio::pick_config(&host, &device, false)?;
    let mut ready = Vec::with_capacity(sound.0.len() * config.sample_rate as usize / RATE as usize + 16);
    let mut linear = crate::audio::Linear::new(RATE, config.sample_rate);
    for &s in &sound.0 {
        linear.push(s, &mut ready);
    }
    let length = Duration::from_secs_f64(ready.len() as f64 / config.sample_rate as f64);
    let mut next = ready.into_iter();
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    frame.fill(next.next().unwrap_or(0.0));
                }
            },
            |_| {},
            None,
        )
        .context("opening the speaker")?;
    stream.play().context("starting the speaker")?;
    std::thread::sleep(length + Duration::from_millis(300));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WAV file, 16-bit: `seconds` of a tone at `rate`, on `channels`.
    fn wav(path: &Path, rate: u32, channels: u16, seconds: f32) {
        let frames = (rate as f32 * seconds) as usize;
        let mut data = Vec::new();
        for i in 0..frames {
            let s = ((std::f32::consts::TAU * 440.0 * i as f32 / rate as f32).sin() * 16000.0) as i16;
            for _ in 0..channels {
                data.extend_from_slice(&s.to_le_bytes());
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        std::fs::write(path, out).unwrap();
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-sound-test-{label}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        swarm_core::store::open_vault(&dir, &swarm_core::store::Keys { seal: [3; 32].into(), names: [4; 32].into() });
        dir
    }

    #[test]
    fn swarms_own_is_short_and_heard() {
        let Sound(samples) = built_in();
        assert!(samples.len() < RATE as usize, "under a second");
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.2 && peak <= 1.0, "peak {peak}");
    }

    /// A file at another rate, in stereo, comes out mono at 48kHz, as
    /// long as it was; a long one's cut to `MOST_SECONDS`.
    #[test]
    fn a_wav_file_is_read_at_the_rate_sounds_are_kept() {
        let dir = scratch("wav");
        let short = dir.join("ping.wav");
        wav(&short, 22050, 2, 1.5);
        let Sound(samples) = from_file(&short).unwrap();
        assert!((samples.len() as i64 - (1.5 * RATE as f32) as i64).abs() < 200, "{} samples", samples.len());
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 16000.0 / 32768.0).abs() < 0.05, "peak {peak}");
        let long = dir.join("song.wav");
        wav(&long, 8000, 1, 25.0);
        assert_eq!(from_file(&long).unwrap().0.len(), MOST_SECONDS * RATE as usize);
    }

    #[test]
    fn what_isnt_a_sound_is_refused_and_swarms_own_stands_in() {
        let dir = scratch("refused");
        let text = dir.join("notes.wav");
        std::fs::write(&text, "not a sound at all").unwrap();
        assert!(from_file(&text).is_err());
        assert!(from_file(&dir.join("missing.wav")).is_err());
        let quiet = dir.join("quiet.wav");
        wav(&quiet, 8000, 1, 0.0);
        assert!(from_file(&quiet).is_err(), "nothing to hear");
        let settings = dir.join("settings");
        let _ = std::fs::remove_file(&settings);
        assert_eq!(chosen(&settings), (built_in(), String::new()));
        swarm_core::settings::set(&settings, SETTING, &text.to_string_lossy()).unwrap();
        assert_eq!(chosen(&settings).0, built_in(), "one that can't be read now: Swarm's own");
        assert_eq!(chosen_name(&settings), "notes.wav");
        let good = dir.join("ping.wav");
        wav(&good, 44100, 1, 0.5);
        swarm_core::settings::set(&settings, SETTING, &good.to_string_lossy()).unwrap();
        assert_eq!(chosen(&settings).1, "ping.wav");
        assert_ne!(chosen(&settings).0, built_in());
    }
}
