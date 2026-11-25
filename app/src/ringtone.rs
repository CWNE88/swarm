//! The sound a call rings with: one the person chose (a WAV, Ogg, FLAC,
//! MP3 or M4A file, its first `MOST_SECONDS`) or Swarm's own, played over
//! and over while a call's ringing here, and stopped as soon as it isn't
//! (answered, here or on another of your devices, rejected, or rung out).
//!
//! On a computer it's played on the speaker, as a message's sound is
//! (`crate::sound`). On the phone it's handed to the app (`set_phone`),
//! which plays it as a ringtone, so the phone's ring volume and silent
//! and vibrate settings go for it, and it rings with the screen off.
//!
//! The front ends say when a call's ringing (`Ringer::set`).

use crate::sound::Sound;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The rate sounds are kept at.
const RATE: u32 = swarm_core::call::SAMPLE_RATE;
/// The most of a file that's played before it starts again.
pub const MOST_SECONDS: usize = 30;
/// Where the chosen file is kept (its path), in settings.
pub const SETTING: &str = "ringtone";
/// The quiet after a chosen file, before it rings again (Swarm's own
/// has its quiet in it).
const GAP: Duration = Duration::from_secs(1);

/// Swarm's own, until the person picks theirs: four notes rising, twice,
/// each ringing on as a bell's does, then a pause; about three seconds
/// a time round.
pub fn built_in() -> Sound {
    let rate = RATE as f32;
    let mut samples = vec![0.0f32; 3 * RATE as usize];
    let ring = 0.7;
    for start in [0.0f32, 0.9] {
        for (k, hz) in [659.26f32, 830.61, 987.77, 1318.51].into_iter().enumerate() {
            let from = ((start + 0.12 * k as f32) * rate) as usize;
            let n = (ring * rate) as usize;
            for i in 0..n {
                let t = i as f32 / rate;
                let attack = (t / 0.005).min(1.0);
                let decay = (-t * 6.0).exp() * (1.0 - i as f32 / n as f32);
                let tau = std::f32::consts::TAU * hz * t;
                samples[from + i] += attack * decay * (tau.sin() + 0.3 * (2.0 * tau).sin() + 0.08 * (3.0 * tau).sin());
            }
        }
    }
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    for s in &mut samples {
        *s *= 0.6 / peak;
    }
    Sound(samples)
}

/// A sound file, as a `Sound`: its first `MOST_SECONDS`, its channels
/// mixed together.
pub fn from_file(path: &Path) -> Result<Sound> {
    crate::sound::read(path, MOST_SECONDS)
}

/// The ringtone chosen in `settings` (Swarm's own if none, or if it can't
/// be read now), and its name ("" for Swarm's own).
pub fn chosen(settings: &Path) -> (Sound, String) {
    match swarm_core::settings::get(settings, SETTING).filter(|p| !p.is_empty()) {
        Some(path) => match from_file(Path::new(&path)) {
            Ok(sound) => (sound, crate::sound::name_of(&path)),
            Err(_) => (built_in(), String::new()),
        },
        None => (built_in(), String::new()),
    }
}

/// What the chosen ringtone's called, as Settings shows it ("" for
/// Swarm's own), without reading it.
pub fn chosen_name(settings: &Path) -> String {
    swarm_core::settings::get(settings, SETTING).filter(|p| !p.is_empty()).map(|p| crate::sound::name_of(&p)).unwrap_or_default()
}

/// Once round, as it rings: the sound chosen in `settings`, with the
/// quiet before it starts again.
pub fn once_round(settings: &Path) -> Sound {
    let (Sound(mut samples), name) = chosen(settings);
    if !name.is_empty() {
        samples.extend(std::iter::repeat_n(0.0, (GAP.as_secs_f32() * RATE as f32) as usize));
    }
    Sound(samples)
}

/// `path` chosen as the ringtone, if it's a sound Swarm can play: kept
/// in `settings`, and the sound, to be heard. On the phone, where what's
/// picked is a copy that may go, it's moved into a folder of Swarm's
/// own first (what was there before going).
pub fn choose(settings: &Path, path: &Path) -> Result<(Sound, String)> {
    let sound = from_file(path)?;
    let kept = keep(path)?;
    swarm_core::settings::set(settings, SETTING, &kept.to_string_lossy()).context("saving it")?;
    Ok((sound, crate::sound::name_of(&kept.to_string_lossy())))
}

/// Back to Swarm's own.
pub fn choose_built_in(settings: &Path) -> Result<()> {
    swarm_core::settings::set(settings, SETTING, "").context("saving it")?;
    #[cfg(target_os = "android")]
    let _ = std::fs::remove_dir_all(crate::paths::data_dir().join("ringtone"));
    Ok(())
}

#[cfg(target_os = "android")]
fn keep(path: &Path) -> Result<PathBuf> {
    let folder = crate::paths::data_dir().join("ringtone");
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).with_context(|| format!("making {}", folder.display()))?;
    let to = folder.join(path.file_name().unwrap_or_else(|| "ringtone".as_ref()));
    if std::fs::rename(path, &to).is_err() {
        std::fs::copy(path, &to).with_context(|| format!("copying it into {}", folder.display()))?;
    }
    Ok(to)
}

#[cfg(not(target_os = "android"))]
fn keep(path: &Path) -> Result<PathBuf> {
    Ok(path.to_path_buf())
}

/// What the phone's app is asked: to start a ringtone (16-bit samples at
/// 48kHz, mono), over and over or once (to hear it in Settings), or to
/// stop it.
#[derive(Clone, Debug, PartialEq)]
pub enum Phone {
    Start { samples: Vec<i16>, looped: bool },
    Stop,
}

/// The phone's app, which plays ringtones itself (android/).
static PHONE: OnceLock<Box<dyn Fn(Phone) + Send + Sync>> = OnceLock::new();
/// Held while the phone's app is asked anything, so a stop asked for
/// while a start's on its way is never overtaken by the start.
static ASKING: Mutex<()> = Mutex::new(());

/// The phone's app, to play ringtones (see `Phone`).
pub fn set_phone(phone: impl Fn(Phone) + Send + Sync + 'static) {
    let _ = PHONE.set(Box::new(phone));
}

fn ask_phone(phone: &(dyn Fn(Phone) + Send + Sync), stop: Option<&AtomicBool>, what: Phone) {
    let _asking = ASKING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
        return;
    }
    phone(what);
}

fn samples_16(Sound(samples): &Sound) -> Vec<i16> {
    samples.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect()
}

/// The ringtone chosen in `settings`, played once round (Settings' Play).
pub fn play_once(settings: &Path) {
    let settings = settings.to_path_buf();
    match PHONE.get() {
        Some(phone) => {
            std::thread::spawn(move || ask_phone(phone.as_ref(), None, Phone::Start { samples: samples_16(&chosen(&settings).0), looped: false }));
        }
        None => crate::sound::play_lazily(move || chosen(&settings).0),
    }
}

/// Rings while a call's ringing here: told whether one is (`set`), as
/// often as liked, it starts the ringtone when one starts and stops it
/// when it stops. Dropped, it stops.
#[derive(Default)]
pub struct Ringer {
    /// Ringing: set to stop it.
    stop: Option<Arc<AtomicBool>>,
}

impl Ringer {
    /// A call's ringing here, or not, with the ringtone chosen in
    /// `settings`.
    pub fn set(&mut self, ringing: bool, settings: &Path) {
        match (ringing, &self.stop) {
            (true, None) => self.stop = Some(start(settings.to_path_buf())),
            (false, Some(_)) => self.stop_now(),
            _ => {}
        }
    }

    pub fn ringing(&self) -> bool {
        self.stop.is_some()
    }

    fn stop_now(&mut self) {
        let Some(stop) = self.stop.take() else { return };
        match PHONE.get() {
            Some(phone) => {
                let _asking = ASKING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                stop.store(true, Ordering::Release);
                phone(Phone::Stop);
            }
            None => stop.store(true, Ordering::Release),
        }
    }
}

impl Drop for Ringer {
    fn drop(&mut self) {
        self.stop_now();
    }
}

/// Starts ringing, on a thread of its own (reading the file there too);
/// set what's returned to stop.
fn start(settings: PathBuf) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::spawn(move || {
        let sound = once_round(&settings);
        match PHONE.get() {
            Some(phone) => ask_phone(phone.as_ref(), Some(&stopped), Phone::Start { samples: samples_16(&sound), looped: true }),
            None => {
                if let Err(e) = ring_on_speaker(&sound, &stopped) {
                    eprintln!("{} the ringtone couldn't be played: {e:#}", crate::unix_now());
                }
            }
        }
    });
    stop
}

/// Plays `sound` over and over on the default speaker until `stop`'s
/// set, then fades it out.
fn ring_on_speaker(sound: &Sound, stop: &Arc<AtomicBool>) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let host = cpal::default_host();
    let device = host.default_output_device().context("no speaker")?;
    let (config, channels) = crate::audio::pick_config(&host, &device, false)?;
    let mut ready = Vec::with_capacity(sound.0.len() * config.sample_rate as usize / RATE as usize + 16);
    let mut linear = crate::audio::Linear::new(RATE, config.sample_rate);
    for &s in &sound.0 {
        linear.push(s, &mut ready);
    }
    if ready.is_empty() {
        return Ok(());
    }
    let fade_step = 50.0 / config.sample_rate as f32;
    let mut gain = 1.0f32;
    let mut at = 0;
    let heard = stop.clone();
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let stopping = heard.load(Ordering::Acquire);
                for frame in data.chunks_mut(channels) {
                    if stopping {
                        gain = (gain - fade_step).max(0.0);
                    }
                    frame.fill(ready[at] * gain);
                    at = (at + 1) % ready.len();
                }
            },
            |_| {},
            None,
        )
        .context("opening the speaker")?;
    stream.play().context("starting the speaker")?;
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(80));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swarms_own_rings_then_pauses() {
        let Sound(samples) = built_in();
        let seconds = samples.len() as f32 / RATE as f32;
        assert!((2.0..4.0).contains(&seconds), "{seconds}s round");
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.6).abs() < 1e-3, "peak {peak}");
        let tail = &samples[samples.len() - RATE as usize / 2..];
        assert!(tail.iter().all(|s| s.abs() < 1e-3), "quiet at the end, before it rings again");
        assert_ne!(built_in(), crate::sound::built_in(), "not the message sound");
    }

    /// The phone's asked to start once, however often it's told a call's
    /// ringing, and to stop once; ringing again starts it again. Settings'
    /// Play is once round, not over and over.
    #[test]
    fn the_phone_is_asked_to_ring_and_stop() {
        let dir = std::env::temp_dir().join(format!("swarm-ringtone-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        swarm_core::store::open_vault(&dir, &swarm_core::store::Keys { seal: [3; 32].into(), names: [4; 32].into() });
        let settings = dir.join("settings");
        static ASKED: Mutex<Vec<(bool, bool, usize)>> = Mutex::new(Vec::new());
        set_phone(|what| {
            ASKED.lock().unwrap().push(match what {
                Phone::Start { samples, looped } => (true, looped, samples.len()),
                Phone::Stop => (false, false, 0),
            })
        });
        let asked = || ASKED.lock().unwrap().clone();
        let wait_for = |n: usize| {
            for _ in 0..200 {
                if asked().len() >= n {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let round = built_in().0.len();
        let mut ringer = Ringer::default();
        ringer.set(true, &settings);
        wait_for(1);
        ringer.set(true, &settings);
        assert!(ringer.ringing());
        ringer.set(false, &settings);
        ringer.set(false, &settings);
        assert!(!ringer.ringing());
        assert_eq!(asked(), [(true, true, round), (false, false, 0)]);
        ringer.set(true, &settings);
        wait_for(3);
        drop(ringer);
        assert_eq!(asked()[2..], [(true, true, round), (false, false, 0)]);
        play_once(&settings);
        wait_for(5);
        assert_eq!(asked()[4..], [(true, false, round)]);
        for _ in 0..20 {
            let mut ringer = Ringer::default();
            ringer.set(true, &settings);
            ringer.set(false, &settings);
        }
        std::thread::sleep(Duration::from_millis(300));
        let after = asked()[5..].to_vec();
        assert_ne!(after.last(), Some(&(true, true, round)), "left ringing: {after:?}");
    }

    /// A file chosen: kept in settings, a second's quiet after it each
    /// time round; one that can't be read now rings with Swarm's own.
    #[test]
    fn a_chosen_file_rings_with_a_pause() {
        let dir = std::env::temp_dir().join(format!("swarm-ringtone-file-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        swarm_core::store::open_vault(&dir, &swarm_core::store::Keys { seal: [3; 32].into(), names: [4; 32].into() });
        let settings = dir.join("settings");
        let _ = std::fs::remove_file(&settings);
        assert_eq!(chosen_name(&settings), "");
        assert_eq!(once_round(&settings), built_in());
        let not_a_sound = dir.join("tune.mp3");
        std::fs::write(&not_a_sound, "words").unwrap();
        assert!(choose(&settings, &not_a_sound).is_err());
        assert_eq!(chosen_name(&settings), "", "nothing kept");
        let path = dir.join("bell.wav");
        let n = RATE as usize / 2;
        let mut data = Vec::new();
        for i in 0..n {
            let s = ((std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin() * 16000.0) as i16;
            data.extend_from_slice(&s.to_le_bytes());
        }
        let mut wav = Vec::new();
        for part in [&b"RIFF"[..], &(36 + data.len() as u32).to_le_bytes(), b"WAVEfmt ", &16u32.to_le_bytes(), &1u16.to_le_bytes(), &1u16.to_le_bytes(), &RATE.to_le_bytes(), &(RATE * 2).to_le_bytes(), &2u16.to_le_bytes(), &16u16.to_le_bytes(), b"data", &(data.len() as u32).to_le_bytes(), &data] {
            wav.extend_from_slice(part);
        }
        std::fs::write(&path, wav).unwrap();
        let (sound, name) = choose(&settings, &path).unwrap();
        assert_eq!((sound.0.len(), name.as_str()), (n, "bell.wav"));
        assert_eq!(chosen_name(&settings), "bell.wav");
        assert_eq!(once_round(&settings).0.len(), n + RATE as usize);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(once_round(&settings), built_in(), "gone: Swarm's own");
        assert_eq!(chosen_name(&settings), "bell.wav", "still chosen, should it come back");
        choose_built_in(&settings).unwrap();
        assert_eq!(chosen_name(&settings), "");
    }
}
