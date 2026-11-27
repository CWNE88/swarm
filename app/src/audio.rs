//! Microphone capture and speaker playback for voice calls, via `cpal`
//! (PulseAudio on Linux when there's a server — PipeWire's will do —
//! else ALSA; WASAPI on Windows) — the platform-specific half of calling;
//! the codec/wire side lives in `swarm_core::call`.
//!
//! Not ALSA where there's a sound server: going through ALSA's plugin
//! for it, cpal's two audio threads spun a whole core each for the whole
//! call (200% of a core, against 1% this way).
//!
//! Calls are 48kHz mono. A device that won't open that way (Windows
//! usually runs each device at its own fixed format, often stereo, and
//! sometimes 44.1kHz) is opened at its own format instead, and converted:
//! channels mixed down or copied out, the rate changed by straight-line
//! interpolation — plenty for speech.
//!
//! The microphone's audio is cleaned up before it's sent, through
//! WebRTC's audio processing (as the `sonora` port), each part unless
//! it's turned off in Settings (see `Voice`). Echo is cancelled: what
//! the speaker plays is handed to the echo canceller (AEC3), which
//! takes it back out of what the microphone hears — so the other side
//! doesn't hear themselves from your speakers. It works out for itself
//! how late the speaker's sound comes back, and keeps working it out
//! through the call (the room, the volume, the two devices' clocks
//! drifting apart). Steady background noise is turned down, and the
//! voice's level kept even.
//!
//! Both streams run on one dedicated OS thread, not a tokio task:
//! `cpal::Stream` holds raw platform audio-handle state that has to
//! stay alive on the thread that created it, and parking it there also
//! sidesteps ever needing it to be `Send` across an `.await` point —
//! same reasoning `tui.rs`'s crossterm input-polling thread already
//! uses for a similar "this needs a real OS thread" case.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use crate::audio_devices::Way;
use swarm_core::call::FRAME_SAMPLES;
use tokio::sync::mpsc;

/// How much decoded audio playback keeps in hand before starting (see
/// `Playout`): two 20ms frames, one cover slot's worth.
const PREBUFFER_SAMPLES: usize = FRAME_SAMPLES * 2;
/// The most playback holds: past this, audio has come in a burst after
/// a stall (a lost packet resent, the link gone for a moment), and
/// playing all of it would leave the call that far behind for good. A
/// quarter of a second rides out ordinary jitter.
const MOST_HELD_SAMPLES: usize = FRAME_SAMPLES * 12;
/// What playback cuts back to once it holds too much: the oldest audio
/// goes, so the call is live again.
const TRIMMED_TO_SAMPLES: usize = FRAME_SAMPLES * 3;
/// How long playback watches the least it holds before deciding some of
/// it is only delay (see `Playout::pop`): two seconds.
const STANDING_WINDOW_SAMPLES: usize = swarm_core::call::SAMPLE_RATE as usize * 2;

/// The block the microphone's audio is cleaned up in: 10ms.
const CLEANUP_BLOCK: usize = swarm_core::call::SAMPLE_RATE as usize / 100;
/// The most of the speaker's audio waiting for the echo canceller (half
/// a second): the microphone takes it every 20ms or so, so it only
/// fills if the microphone has stalled, and then the newest is dropped.
const MOST_PLAYED_BLOCKS_WAITING: usize = 50;

/// How the microphone's audio is cleaned up before it's sent, as chosen
/// in Settings (see `crate::prefs`, which keeps the choice).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Voice {
    /// The speaker's sound taken back out, so the other side doesn't
    /// hear themselves.
    pub cancel_echo: bool,
    /// Steady background noise (fans, hum, hiss) turned down.
    pub suppress_noise: bool,
    /// A quiet or loud microphone brought to an even level.
    pub steady_level: bool,
}

impl Voice {
    const NONE: Voice = Voice { cancel_echo: false, suppress_noise: false, steady_level: false };

    fn bits(self) -> u8 {
        self.cancel_echo as u8 | (self.suppress_noise as u8) << 1 | (self.steady_level as u8) << 2
    }
}

/// The `Voice` chosen — read by the microphone as it goes, so a change
/// in Settings takes effect in a call already going.
static VOICE: AtomicU8 = AtomicU8::new(0b111);

/// Sets how the microphone's audio is cleaned up, for calls going and
/// to come.
pub fn set_voice(voice: Voice) {
    VOICE.store(voice.bits(), Ordering::Relaxed);
}

fn voice() -> Voice {
    let bits = VOICE.load(Ordering::Relaxed);
    Voice { cancel_echo: bits & 1 != 0, suppress_noise: bits & 2 != 0, steady_level: bits & 4 != 0 }
}

/// Live mic+speaker streams for one call — or just one of the two, or
/// neither, if they couldn't be opened (see `start`). Dropping this (or
/// calling `stop` explicitly) tears them down and joins the audio thread.
pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    /// The microphone sends silence while set: the call carries on
    /// sending as usual, so nothing on the wire says it's muted.
    muted: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Which device is missing, if one is (`MISSING_MICROPHONE`,
    /// `MISSING_SPEAKER`), for telling the user why they can't be heard
    /// (or can't hear) rather than leaving them to guess. It can change
    /// as a device is chosen in a call.
    missing: Arc<AtomicU8>,
    /// Where another voice to play goes in (see `AudioHandle::speaker`).
    sources: std::sync::mpsc::Sender<mpsc::UnboundedReceiver<Vec<f32>>>,
}

const MISSING_MICROPHONE: u8 = 1;
const MISSING_SPEAKER: u8 = 2;

impl AudioHandle {
    /// `Some("listen only (no microphone)")`-style note when this call
    /// is running on one device or none; `None` for a normal two-way call.
    pub fn one_way_note(&self) -> Option<&'static str> {
        match self.missing.load(Ordering::Relaxed) {
            MISSING_MICROPHONE => Some("listen only (no microphone)"),
            MISSING_SPEAKER => Some("talk only (no speaker)"),
            0 => None,
            _ => Some("no microphone or speaker"),
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Another voice to play, mixed with the rest: a conference's other
    /// participants, each on their own link. It's played until the
    /// sender's dropped and what came is played out.
    pub fn speaker(&self) -> mpsc::UnboundedSender<Vec<f32>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = self.sources.send(rx);
        tx
    }

    pub fn stop(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Opens the microphone and speaker chosen in Settings (see
/// `crate::audio_devices`; one chosen during the call is opened in the
/// other's place at once) at `swarm_core::call`'s fixed format (48kHz mono) and starts streaming: captured audio is
/// chunked to `FRAME_SAMPLES` and pushed onto `mic_tx`; whatever arrives
/// on `speaker_rx` is played back, with silence filling any gap where
/// nothing's arrived yet (see the output callback below) rather than
/// blocking — an audio callback that blocks waiting for data is how you
/// get audible glitches or a hung device.
///
/// Each device is opened independently: one that's missing or won't
/// open just leaves the call one-way — a Pi with a speaker and no mic
/// can still listen — and with neither the call still goes on, silent
/// (a machine with no sound at all can still share or be controlled;
/// one chosen in Settings meanwhile is opened). With no microphone,
/// `mic_tx` is still held open (and simply never sent on) until the
/// call stops: `call::run` treats its mic channel closing as the local
/// side hanging up, so letting it close would end the call on the spot.
/// Blocks briefly (device open + first callback setup) — call it from a
/// spawned task, never inline on a UI thread.
pub fn start(mic_tx: mpsc::UnboundedSender<Vec<f32>>, speaker_rx: mpsc::UnboundedReceiver<Vec<f32>>) -> Result<AudioHandle> {
    open(mic_tx, Some(speaker_rx))
}

/// The microphone alone, as `start` has it for a call (its noise and
/// level seen to as Settings say; no echo, with no speaker), onto
/// `mic_tx`: for showing how loud we'd be heard. An error if it won't
/// open.
pub fn listen(mic_tx: mpsc::UnboundedSender<Vec<f32>>) -> Result<AudioHandle> {
    open(mic_tx, None)
}

fn open(mic_tx: mpsc::UnboundedSender<Vec<f32>>, speaker_rx: Option<mpsc::UnboundedReceiver<Vec<f32>>>) -> Result<AudioHandle> {
    let listening = speaker_rx.is_none();
    set_voice(crate::prefs::voice(&crate::paths().settings()));
    let stop = Arc::new(AtomicBool::new(false));
    let muted = Arc::new(AtomicBool::new(false));
    let missing = Arc::new(AtomicU8::new(0));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let (sources, added) = std::sync::mpsc::channel();
    if let Some(speaker_rx) = speaker_rx {
        let _ = sources.send(speaker_rx);
    }

    let thread_stop = stop.clone();
    let thread_muted = muted.clone();
    let thread_missing = missing.clone();
    let thread = std::thread::spawn(move || {
        let host = cpal::default_host();
        let (played_tx, played_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(MOST_PLAYED_BLOCKS_WAITING);
        let mic_side = Arc::new(std::sync::Mutex::new(MicSide {
            capture_buf: Vec::with_capacity(FRAME_SAMPLES * 2),
            cleanup: Cleanup::new(played_rx),
            sent_buf: Vec::with_capacity(FRAME_SAMPLES * 2),
        }));
        let speaker_side = (!listening).then(|| {
            Arc::new(std::sync::Mutex::new(SpeakerSide { ready: Vec::new(), mixer: Mixer::new(added), played: Vec::with_capacity(CLEANUP_BLOCK), played_tx }))
        });

        let open_mic = || {
            let (device, chosen) = crate::audio_devices::device(&host, Way::Microphone);
            (device.ok_or_else(|| anyhow!("no microphone found")).and_then(|d| mic_stream(&host, &d, mic_side.clone(), mic_tx.clone(), thread_muted.clone())), chosen)
        };
        let open_speaker = |side: &Arc<std::sync::Mutex<SpeakerSide>>| {
            let (device, chosen) = crate::audio_devices::device(&host, Way::Speaker);
            (device.ok_or_else(|| anyhow!("no speaker found")).and_then(|d| speaker_stream(&host, &d, side.clone())), chosen)
        };
        let note_missing = |input: &Result<MicStream>, output: &Option<Result<cpal::Stream>>| {
            let gone = if input.is_err() { MISSING_MICROPHONE } else { 0 } | if matches!(output, Some(Err(_))) { MISSING_SPEAKER } else { 0 };
            thread_missing.store(gone, Ordering::Relaxed);
        };

        let (mut input, mut mic_chosen) = open_mic();
        let (mut output, mut speaker_chosen) = match &speaker_side {
            Some(side) => {
                let (stream, chosen) = open_speaker(side);
                (Some(stream), chosen)
            }
            None => (None, None),
        };
        if let (Err(mic), None) = (&input, &output) {
            let _ = ready_tx.send(Err(anyhow!("{mic:#}")));
            return;
        }
        note_missing(&input, &output);
        let _ = ready_tx.send(Ok(()));

        let mut seen = crate::audio_devices::changes();
        while !thread_stop.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(50));
            // With no speaker playing, nothing takes the voices in: they're
            // let go here instead of piling up for the whole call.
            if let (Some(side), false) = (&speaker_side, matches!(output, Some(Ok(_)))) {
                swarm_core::lock::Locked::locked(&**side).mixer.discard();
            }
            if crate::audio_devices::changes() == seen {
                continue;
            }
            seen = crate::audio_devices::changes();
            if crate::audio_devices::chosen(Way::Microphone) != mic_chosen {
                drop(std::mem::replace(&mut input, Err(anyhow!("changing"))));
                (input, mic_chosen) = open_mic();
            }
            if let Some(side) = &speaker_side {
                if crate::audio_devices::chosen(Way::Speaker) != speaker_chosen {
                    drop(output.take());
                    let (stream, chosen) = open_speaker(side);
                    (output, speaker_chosen) = (Some(stream), chosen);
                }
            }
            note_missing(&input, &output);
        }
        drop((input, output));
        drop(mic_tx);
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(AudioHandle { stop, muted, thread: Some(thread), missing, sources }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(anyhow!("audio thread ended before it could start streaming"))
        }
    }
}

/// The microphone's side of the audio thread, kept while the device it
/// comes from is changed.
struct MicSide {
    capture_buf: Vec<f32>,
    cleanup: Cleanup,
    sent_buf: Vec<f32>,
}

/// The speaker's side, kept the same way.
struct SpeakerSide {
    /// At the device's rate, waiting to be played.
    ready: Vec<f32>,
    mixer: Mixer,
    /// What's been played, for the echo canceller (see `played_tx`).
    played: Vec<f32>,
    played_tx: std::sync::mpsc::SyncSender<Vec<f32>>,
}

/// A microphone's audio as it comes, `channels` to a frame at `rate`:
/// mixed down, brought to 48kHz, cleaned up, and onto `mic_tx` in
/// `FRAME_SAMPLES` chunks (silence while `muted`). Shared by however the
/// microphone was opened.
struct MicIn {
    side: Arc<std::sync::Mutex<MicSide>>,
    mic_tx: mpsc::UnboundedSender<Vec<f32>>,
    muted: Arc<AtomicBool>,
    /// The device's rate to 48kHz, made for the rate it's at.
    resample: Option<(u32, Linear)>,
}

impl MicIn {
    fn take(&mut self, data: &[f32], channels: usize, rate: u32) {
        let resample = match &mut self.resample {
            Some((at, resample)) if *at == rate => resample,
            slot => &mut slot.insert((rate, Linear::new(rate, swarm_core::call::SAMPLE_RATE))).1,
        };
        let mut side = self.side.lock().unwrap_or_else(|e| e.into_inner());
        let MicSide { capture_buf, cleanup, sent_buf } = &mut *side;
        for frame in data.chunks(channels.max(1)) {
            let mono = frame.iter().sum::<f32>() / frame.len() as f32;
            resample.push(mono, capture_buf);
        }
        cleanup.take_played();
        while capture_buf.len() >= CLEANUP_BLOCK {
            let mut block: Vec<f32> = capture_buf.drain(..CLEANUP_BLOCK).collect();
            cleanup.process(&mut block);
            sent_buf.extend_from_slice(&block);
        }
        while sent_buf.len() >= FRAME_SAMPLES {
            let mut chunk: Vec<f32> = sent_buf.drain(..FRAME_SAMPLES).collect();
            if self.muted.load(Ordering::Relaxed) {
                chunk.fill(0.0);
            }
            let _ = self.mic_tx.send(chunk);
        }
    }
}

/// An open microphone, kept for as long as it's in use.
enum MicStream {
    Cpal(#[allow(dead_code)] cpal::Stream),
    #[cfg(target_os = "android")]
    Voice(#[allow(dead_code)] ndk::audio::AudioStream),
    /// Android's Java recording, stopped when this goes.
    #[cfg(target_os = "android")]
    Java(JavaMicOn),
}

/// The microphone through Android's Java `AudioRecord`, with the
/// voice-communication source, mono, 16-bit: what calling apps use (MS
/// Teams among them), and so what a car maker's audio is tested with.
/// The app sets it (`android/src/lib.rs`, the activity's `startMic` and
/// `stopMic`); the recording's thread hands its audio to `java_mic_in`.
/// Used where the microphone opened natively gives more channels than
/// asked for (BYD's cars), whose native audio crackled however it was
/// opened; phones, which give one, open it natively.
#[cfg(target_os = "android")]
pub struct JavaMic {
    /// Starts the recording; false if it couldn't be.
    pub start: fn() -> bool,
    /// Stops it, once its thread has handed over the last of it.
    pub stop: fn(),
}

#[cfg(target_os = "android")]
static JAVA_MIC: std::sync::OnceLock<JavaMic> = std::sync::OnceLock::new();

/// Where the Java recording's audio goes while it runs.
#[cfg(target_os = "android")]
static JAVA_MIC_IN: std::sync::Mutex<Option<JavaIn>> = std::sync::Mutex::new(None);

#[cfg(target_os = "android")]
struct JavaIn {
    mic: MicIn,
    mono: Vec<f32>,
    /// Frames so far, and their energy, for one log line a few seconds in.
    counted: usize,
    energy: f64,
}

/// Sets how to record through Android's Java (see `JavaMic`).
#[cfg(target_os = "android")]
pub fn set_java_mic(java: JavaMic) {
    let _ = JAVA_MIC.set(java);
}

/// Audio from the Java recording: `samples`, 16-bit, `channels` to a
/// frame, at `rate`.
#[cfg(target_os = "android")]
pub fn java_mic_in(samples: &[i16], channels: usize, rate: u32) {
    let mut held = JAVA_MIC_IN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(java) = held.as_mut() else { return };
    let channels = channels.max(1);
    java.mono.clear();
    java.mono.extend(samples.chunks(channels).map(|frame| frame.iter().map(|s| f32::from(*s)).sum::<f32>() / (frame.len() as f32 * 32768.0)));
    let log_after = rate as usize * 3;
    if java.counted < log_after {
        java.energy += java.mono.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>();
        java.counted += java.mono.len();
        if java.counted >= log_after {
            let level = 10.0 * (java.energy / java.counted as f64).max(1e-12).log10();
            eprintln!("the microphone, recorded through Java: {channels} channel(s) at {rate} Hz, level {level:.1} dB");
        }
    }
    let JavaIn { mic, mono, .. } = java;
    mic.take(mono, 1, rate);
}

/// The Java recording while it runs; stopping it when dropped.
#[cfg(target_os = "android")]
struct JavaMicOn;

#[cfg(target_os = "android")]
impl Drop for JavaMicOn {
    fn drop(&mut self) {
        if let Some(java) = JAVA_MIC.get() {
            (java.stop)();
        }
        *JAVA_MIC_IN.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Starts the Java recording into `mic`; `mic` back if it can't be.
#[cfg(target_os = "android")]
fn java_mic(mic: MicIn) -> std::result::Result<JavaMicOn, MicIn> {
    let Some(java) = JAVA_MIC.get() else { return Err(mic) };
    *JAVA_MIC_IN.lock().unwrap_or_else(|e| e.into_inner()) = Some(JavaIn { mic, mono: Vec::new(), counted: 0, energy: 0.0 });
    if (java.start)() {
        eprintln!("the microphone gives more channels than asked for: recording through Java, as calling apps do");
        return Ok(JavaMicOn);
    }
    let back = JAVA_MIC_IN.lock().unwrap_or_else(|e| e.into_inner()).take();
    match back {
        Some(JavaIn { mic, .. }) => Err(mic),
        None => unreachable!("only java_mic and JavaMicOn's drop take it"),
    }
}

/// The microphone's stream from `device`, into a `MicIn`.
fn mic_stream(host: &cpal::Host, device: &cpal::Device, side: Arc<std::sync::Mutex<MicSide>>, mic_tx: mpsc::UnboundedSender<Vec<f32>>, muted: Arc<AtomicBool>) -> Result<MicStream> {
    let mut mic = MicIn { side, mic_tx, muted, resample: None };
    #[cfg(target_os = "android")]
    if android_mic_channels(swarm_core::call::SAMPLE_RATE) > 1 {
        return match java_mic(mic) {
            Ok(on) => Ok(MicStream::Java(on)),
            Err(mic) => {
                eprintln!("the microphone couldn't be recorded through Java: opening it natively");
                android_voice_mic(mic).map(MicStream::Voice)
            }
        };
    }
    let (config, channels) = pick_config(host, device, true)?;
    let rate = config.sample_rate;
    let stream = device
        .build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| mic.take(data, channels, rate),
            |err| eprintln!("\nmicrophone stream error: {err}"),
            None,
        )
        .context("opening microphone stream")?;
    stream.play().context("starting microphone stream")?;
    Ok(MicStream::Cpal(stream))
}

/// The microphone opened as a voice call's, the way calling apps open
/// it, for a device whose microphone opened the usual way gives more
/// channels than asked for. BYD's cars (DiLink 3.0) say four whatever
/// is asked for: in floating point that's true, one of them the
/// microphone and three of them silence; in 16-bit it isn't (the data is
/// one channel, and read as four it played four times too fast). So:
/// floating point, and only the channels within 40 dB of the loudest,
/// over the whole call, mixed: the car's three empty channels aren't
/// quite zero (the odd stray sample, which mixed in sounded like
/// crackle), and averaged in they made the voice a quarter as loud.
/// Each channel's level is logged once, three seconds in, so a strange
/// layout shows in the log.
#[cfg(target_os = "android")]
fn android_voice_mic(mut mic: MicIn) -> Result<ndk::audio::AudioStream> {
    use ndk::audio::{AudioCallbackResult, AudioDirection, AudioFormat, AudioInputPreset, AudioStreamBuilder};
    const LOG_AFTER: usize = swarm_core::call::SAMPLE_RATE as usize * 3;
    let mut levels: Vec<f64> = Vec::new();
    let mut counted = 0usize;
    let mut energy: Vec<f64> = Vec::new();
    let mut live: Vec<bool> = Vec::new();
    let mut mono: Vec<f32> = Vec::new();
    let mut xruns = 0;
    let mut since = std::time::Instant::now();
    let (mut calls, mut fewest, mut most, mut longest_gap) = (0u32, usize::MAX, 0usize, std::time::Duration::ZERO);
    let mut last_call: Option<std::time::Instant> = None;
    let callback_frames = swarm_core::call::SAMPLE_RATE as i32 / 50;
    let stream = AudioStreamBuilder::new()
        .and_then(|b| {
            b.direction(AudioDirection::Input)
                .input_preset(AudioInputPreset::VoiceCommunication)
                .channel_count(1)
                .sample_rate(swarm_core::call::SAMPLE_RATE as i32)
                .format(AudioFormat::PCM_Float)
                .frames_per_data_callback(callback_frames)
                .buffer_capacity_in_frames(callback_frames * 4)
                .data_callback(Box::new(move |stream, data, frames| {
                    let now = std::time::Instant::now();
                    if let Some(last) = last_call.replace(now) {
                        longest_gap = longest_gap.max(now - last);
                    }
                    calls += 1;
                    fewest = fewest.min(usize::try_from(frames).unwrap_or(0));
                    most = most.max(usize::try_from(frames).unwrap_or(0));
                    if now - since >= std::time::Duration::from_secs(10) {
                        let lost = stream.x_run_count();
                        if lost != xruns {
                            eprintln!(
                                "microphone: audio lost {} time(s) in the last {:.0} s ({} callbacks of {fewest} to {most} frames, longest gap {} ms, buffer {} of {} frames)",
                                lost - xruns,
                                (now - since).as_secs_f32(),
                                calls,
                                longest_gap.as_millis(),
                                stream.buffer_size_in_frames(),
                                stream.buffer_capacity_in_frames()
                            );
                            xruns = lost;
                        }
                        (since, calls, fewest, most, longest_gap) = (now, 0, usize::MAX, 0, std::time::Duration::ZERO);
                    }
                    let channels = usize::try_from(stream.channel_count()).unwrap_or(1).max(1);
                    let rate = u32::try_from(stream.sample_rate()).unwrap_or(swarm_core::call::SAMPLE_RATE);
                    let Ok(frames) = usize::try_from(frames) else { return AudioCallbackResult::Continue };
                    // SAFETY: AAudio hands an input callback `frames` frames
                    // of `channels` floats each (the format asked for, which
                    // it keeps), valid until the callback returns.
                    let samples = unsafe { std::slice::from_raw_parts(data as *const f32, frames * channels) };
                    energy.resize(channels, 0.0);
                    for frame in samples.chunks(channels) {
                        for (energy, s) in energy.iter_mut().zip(frame) {
                            *energy += f64::from(*s) * f64::from(*s);
                        }
                    }
                    let loudest = energy.iter().copied().fold(0.0, f64::max);
                    live.clear();
                    live.extend(energy.iter().map(|e| *e >= loudest * 1e-4));
                    let heard = live.iter().filter(|l| **l).count().max(1) as f32;
                    mono.clear();
                    mono.extend(samples.chunks(channels).map(|frame| {
                        frame.iter().zip(&live).filter(|(_, l)| **l).map(|(s, _)| *s).sum::<f32>() / heard
                    }));
                    if counted < LOG_AFTER {
                        levels.resize(channels, 0.0);
                        for frame in samples.chunks(channels) {
                            for (level, s) in levels.iter_mut().zip(frame) {
                                let s = f64::from(*s);
                                *level += s * s;
                            }
                        }
                        counted += frames;
                        if counted >= LOG_AFTER {
                            let each: Vec<String> = levels.iter().map(|sum| format!("{:.1} dB", 10.0 * (sum / counted as f64).max(1e-12).log10())).collect();
                            eprintln!("the microphone, opened for a call: {channels} channel(s) at {rate} Hz, levels {}; using {} of them", each.join(", "), heard as usize);
                        }
                    }
                    mic.take(&mono, 1, rate);
                    AudioCallbackResult::Continue
                }))
                .error_callback(Box::new(|_, err| eprintln!("\nmicrophone stream error: {err}")))
                .open_stream()
        })
        .context("opening the microphone for a call")?;
    eprintln!(
        "the microphone gives more channels than asked for: opened for a call instead ({} channel(s), {:?} frames a callback, buffer {} of {} frames)",
        stream.channel_count(),
        stream.frames_per_data_callback(),
        stream.buffer_size_in_frames(),
        stream.buffer_capacity_in_frames()
    );
    stream.request_start().context("starting the microphone")?;
    Ok(stream)
}

/// The speaker's stream to `device`: everyone's voice, mixed.
fn speaker_stream(host: &cpal::Host, device: &cpal::Device, side: Arc<std::sync::Mutex<SpeakerSide>>) -> Result<cpal::Stream> {
    let (config, channels) = pick_config(host, device, false)?;
    let mut resample = Linear::new(swarm_core::call::SAMPLE_RATE, config.sample_rate);
    side.lock().unwrap_or_else(|e| e.into_inner()).ready.clear();
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut side = side.lock().unwrap_or_else(|e| e.into_inner());
                let SpeakerSide { ready, mixer, played, played_tx } = &mut *side;
                mixer.take_in();
                for frame in data.chunks_mut(channels) {
                    while ready.is_empty() {
                        let next = mixer.pop();
                        resample.push(next, ready);
                        played.push(next);
                        if played.len() == CLEANUP_BLOCK {
                            let _ = played_tx.try_send(std::mem::replace(played, Vec::with_capacity(CLEANUP_BLOCK)));
                        }
                    }
                    frame.fill(ready.remove(0));
                }
            },
            |err| eprintln!("\nspeaker stream error: {err}"),
            None,
        )
        .context("opening speaker stream")?;
    stream.play().context("starting speaker stream")?;
    Ok(stream)
}

/// The format to open `device` at: 48kHz mono if it'll take that,
/// otherwise its own default (which has to be 32-bit float, as WASAPI's
/// and most others' are). Returns the config and its channel count.
///
/// With PulseAudio, in 20ms chunks: left to itself the server picks its
/// own buffering, which can be far more than a call wants.
pub(crate) fn pick_config(host: &cpal::Host, device: &cpal::Device, input: bool) -> Result<(cpal::StreamConfig, usize)> {
    let rate = swarm_core::call::SAMPLE_RATE;
    let fits = |c: &cpal::SupportedStreamConfigRange| {
        c.channels() == 1 && c.sample_format() == cpal::SampleFormat::F32 && c.min_sample_rate() <= rate && rate <= c.max_sample_rate()
    };
    let wanted = if input {
        device.supported_input_configs().map(|mut all| all.any(|c| fits(&c))).unwrap_or(false)
    } else {
        device.supported_output_configs().map(|mut all| all.any(|c| fits(&c))).unwrap_or(false)
    };
    let buffer = |rate: u32| {
        if is_pulse(host) {
            cpal::BufferSize::Fixed(rate * swarm_core::call::FRAME_DURATION.as_millis() as u32 / 1000)
        } else {
            cpal::BufferSize::Default
        }
    };
    if wanted {
        return Ok((cpal::StreamConfig { channels: 1, sample_rate: rate, buffer_size: buffer(rate) }, 1));
    }
    let default = if input { device.default_input_config() } else { device.default_output_config() }.context("reading the device's format")?;
    if default.sample_format() != cpal::SampleFormat::F32 {
        anyhow::bail!("the device only offers {} samples, not floating point", default.sample_format());
    }
    let config = default.config();
    let channels = (config.channels as usize).max(1);
    Ok((cpal::StreamConfig { buffer_size: buffer(config.sample_rate), ..config }, channels))
}

/// How many channels Android really opens the microphone with when
/// asked for mono at `rate`, the way cpal opens it: BYD's cars (DiLink)
/// quietly make it four, and say so only when asked, while cpal goes by
/// what it asked for. Found by opening a stream, closed again
/// unstarted; 1 if that fails.
#[cfg(target_os = "android")]
fn android_mic_channels(rate: u32) -> i32 {
    let opened = ndk::audio::AudioStreamBuilder::new().and_then(|b| {
        b.direction(ndk::audio::AudioDirection::Input)
            .channel_count(1)
            .sample_rate(rate as i32)
            .format(ndk::audio::AudioFormat::PCM_Float)
            .open_stream()
    });
    opened.map_or(1, |stream| stream.channel_count())
}

#[cfg(target_os = "linux")]
fn is_pulse(host: &cpal::Host) -> bool {
    host.id() == cpal::HostId::PulseAudio
}

#[cfg(not(target_os = "linux"))]
fn is_pulse(_: &cpal::Host) -> bool {
    false
}

/// The audio waiting to be played: a small jitter buffer. Decoded audio
/// arrives in bursts (two 20ms frames per 40ms cover slot — see
/// `swarm_core::call` — or a run of them after a stall), not a steady
/// trickle, so playback only starts (or restarts, after running dry)
/// once `PREBUFFER_SAMPLES` are in hand, rather than playing each frame
/// the instant it lands and clicking through every gap between bursts.
/// Past `MOST_HELD_SAMPLES` the oldest is dropped: a call is better a
/// moment short than seconds behind. Below that, audio that's never
/// needed — held through two seconds without ever running down to the
/// prebuffer — is dropped too: a burst once (the start of a video call,
/// say) would otherwise leave the call that much behind for good. Never
/// blocks: short of audio, the player plays silence — an audio callback
/// that waits is how you get glitches or a hung device.
struct Playout {
    held: std::collections::VecDeque<f32>,
    priming: bool,
    /// The least held since `popped` was last reset.
    least: usize,
    popped: usize,
}

impl Playout {
    fn new() -> Playout {
        Playout { held: std::collections::VecDeque::new(), priming: true, least: usize::MAX, popped: 0 }
    }

    fn push(&mut self, samples: &[f32]) {
        self.held.extend(samples);
        if self.held.len() > MOST_HELD_SAMPLES {
            self.held.drain(..self.held.len() - TRIMMED_TO_SAMPLES);
        }
        if self.held.len() >= PREBUFFER_SAMPLES {
            self.priming = false;
        }
    }

    /// The next sample to play, or `None` while there's nothing (yet).
    fn pop(&mut self) -> Option<f32> {
        if self.priming {
            return None;
        }
        let sample = self.held.pop_front();
        if self.held.is_empty() {
            self.priming = true;
        }
        self.least = self.least.min(self.held.len());
        self.popped += 1;
        if self.popped >= STANDING_WINDOW_SAMPLES {
            if !self.priming && self.least > PREBUFFER_SAMPLES {
                self.held.drain(..self.least - PREBUFFER_SAMPLES);
            }
            self.least = usize::MAX;
            self.popped = 0;
        }
        sample
    }
}

/// Everyone's voice, mixed: each with its own jitter buffer, as each
/// comes on its own link at its own times (a conference's — see
/// `AudioHandle::speaker`). A voice that's ended is let go once it's
/// played out.
struct Mixer {
    voices: Vec<Voiced>,
    added: std::sync::mpsc::Receiver<mpsc::UnboundedReceiver<Vec<f32>>>,
}

struct Voiced {
    from: mpsc::UnboundedReceiver<Vec<f32>>,
    playout: Playout,
    ended: bool,
}

impl Mixer {
    fn new(added: std::sync::mpsc::Receiver<mpsc::UnboundedReceiver<Vec<f32>>>) -> Mixer {
        Mixer { voices: Vec::new(), added }
    }

    /// Takes in whatever's arrived, and any new voices. Never blocks.
    fn take_in(&mut self) {
        while let Ok(from) = self.added.try_recv() {
            self.voices.push(Voiced { from, playout: Playout::new(), ended: false });
        }
        for voice in &mut self.voices {
            loop {
                match voice.from.try_recv() {
                    Ok(chunk) => voice.playout.push(&chunk),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        voice.ended = true;
                        break;
                    }
                }
            }
        }
        self.voices.retain(|v| !(v.ended && (v.playout.held.is_empty() || v.playout.priming)));
    }

    /// Takes in whatever's arrived and lets it go unplayed: for when
    /// there's no speaker to play it.
    fn discard(&mut self) {
        self.take_in();
        for voice in &mut self.voices {
            voice.playout.held.clear();
            voice.playout.priming = true;
        }
        self.voices.retain(|v| !v.ended);
    }

    /// The next sample: every voice's, added together.
    fn pop(&mut self) -> f32 {
        self.voices.iter_mut().map(|v| v.playout.pop().unwrap_or(0.0)).sum::<f32>().clamp(-1.0, 1.0)
    }
}

/// Cleans up the microphone's audio as Settings say (see `Voice`), 10ms
/// at a time: it takes in what the speaker has played since the
/// microphone last looked (for cancelling its echo), then processes the
/// microphone's audio. With nothing to do, it lets the audio through
/// untouched; changed in Settings, it's started afresh. With no speaker
/// (none, or it wouldn't open), there's no echo to cancel.
struct Cleanup {
    played: Receiver<Vec<f32>>,
    processor: Option<(Voice, sonora::AudioProcessing)>,
    no_speaker: bool,
}

impl Cleanup {
    fn new(played: Receiver<Vec<f32>>) -> Cleanup {
        Cleanup { played, processor: None, no_speaker: false }
    }

    fn take_played(&mut self) {
        let wanted = Voice { cancel_echo: voice().cancel_echo && !self.no_speaker, ..voice() };
        if wanted == Voice::NONE {
            self.processor = None;
        } else if self.processor.as_ref().map(|(had, _)| *had) != Some(wanted) {
            let config = sonora::Config {
                echo_canceller: wanted.cancel_echo.then(Default::default),
                noise_suppression: wanted.suppress_noise.then(Default::default),
                gain_controller2: wanted.steady_level.then(|| sonora::config::GainController2 {
                    input_volume_controller: false,
                    adaptive_digital: Some(Default::default()),
                    fixed_digital: Default::default(),
                }),
                ..Default::default()
            };
            let format = sonora::StreamConfig::new(swarm_core::call::SAMPLE_RATE, 1);
            self.processor = Some((wanted, sonora::AudioProcessing::builder().config(config).capture_config(format).render_config(format).build()));
        }
        loop {
            match self.played.try_recv() {
                Ok(block) => {
                    if let Some((Voice { cancel_echo: true, .. }, processor)) = &mut self.processor {
                        let mut unused = [0f32; CLEANUP_BLOCK];
                        let _ = processor.process_render_f32(&[&block], &mut [&mut unused]);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.no_speaker = true;
                    break;
                }
            }
        }
    }

    /// `block` is `CLEANUP_BLOCK` long.
    fn process(&mut self, block: &mut [f32]) {
        if let Some((_, processor)) = &mut self.processor {
            let mut out = [0f32; CLEANUP_BLOCK];
            if processor.process_capture_f32(&[block], &mut [&mut out]).is_ok() {
                block.copy_from_slice(&out);
            }
        }
    }
}

/// Changes a stream of samples from one rate to another by drawing
/// straight lines between them. Fed one sample at a time; what comes out
/// is appended to the given buffer. Same rate in and out passes samples
/// straight through (a sample late).
pub(crate) struct Linear {
    /// Input samples per output sample.
    step: f64,
    /// Where the next output falls between `prev` (0) and `next` (1).
    at: f64,
    prev: f32,
    next: f32,
}

impl Linear {
    pub(crate) fn new(from: u32, to: u32) -> Linear {
        Linear { step: from as f64 / to as f64, at: 0.0, prev: 0.0, next: 0.0 }
    }

    pub(crate) fn push(&mut self, sample: f32, out: &mut Vec<f32>) {
        self.prev = self.next;
        self.next = sample;
        while self.at < 1.0 {
            out.push(self.prev + (self.next - self.prev) * self.at as f32);
            self.at += self.step;
        }
        self.at -= 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audio that piles up after a stall (here three seconds of it, as
    /// after a three-second cut) mustn't all be played: the call would
    /// stay three seconds behind for the rest of it.
    #[test]
    fn a_burst_after_a_stall_leaves_the_call_live() {
        let mut playout = Playout::new();
        let frame = vec![0.5f32; FRAME_SAMPLES];
        for _ in 0..150 {
            playout.push(&frame);
        }
        assert!(playout.held.len() <= MOST_HELD_SAMPLES, "{} samples held", playout.held.len());
        for _ in 0..1000 {
            playout.push(&frame);
            for _ in 0..FRAME_SAMPLES {
                assert!(playout.pop().is_some());
            }
        }
        assert!(playout.held.len() <= MOST_HELD_SAMPLES);
    }

    /// A burst once, under the most held, then steady arrival: what the
    /// burst added is only delay, and it goes.
    #[test]
    fn a_standing_delay_drains_away() {
        let mut playout = Playout::new();
        let frame = vec![0.5f32; FRAME_SAMPLES];
        for _ in 0..10 {
            playout.push(&frame);
        }
        for _ in 0..200 {
            playout.push(&frame);
            for _ in 0..FRAME_SAMPLES {
                assert!(playout.pop().is_some(), "never runs dry");
            }
        }
        assert!(playout.held.len() <= PREBUFFER_SAMPLES + FRAME_SAMPLES, "{} samples still held", playout.held.len());
    }

    /// With no speaker, what's heard is let go as it comes, not kept for
    /// the whole call; a voice that's ended goes with it.
    #[test]
    fn with_no_speaker_voices_are_let_go() {
        let (sources, added) = std::sync::mpsc::channel();
        let mut mixer = Mixer::new(added);
        let (a, a_rx) = mpsc::unbounded_channel();
        let (b, b_rx) = mpsc::unbounded_channel();
        sources.send(a_rx).unwrap();
        sources.send(b_rx).unwrap();
        for _ in 0..50 {
            a.send(vec![0.25f32; FRAME_SAMPLES]).unwrap();
            b.send(vec![0.5f32; FRAME_SAMPLES]).unwrap();
        }
        drop(b);
        mixer.discard();
        assert_eq!(mixer.voices.len(), 1, "the ended voice is gone");
        assert!(mixer.voices[0].playout.held.is_empty() && mixer.voices[0].from.is_empty(), "nothing's left waiting");
    }

    /// Two voices at once are both heard; one that's ended is let go
    /// once played out, and the other carries on.
    #[test]
    fn voices_are_mixed_and_let_go_when_done() {
        let (sources, added) = std::sync::mpsc::channel();
        let mut mixer = Mixer::new(added);
        let (a, a_rx) = mpsc::unbounded_channel();
        let (b, b_rx) = mpsc::unbounded_channel();
        sources.send(a_rx).unwrap();
        sources.send(b_rx).unwrap();
        for _ in 0..2 {
            a.send(vec![0.25f32; FRAME_SAMPLES]).unwrap();
            b.send(vec![0.5f32; FRAME_SAMPLES]).unwrap();
        }
        mixer.take_in();
        assert_eq!(mixer.pop(), 0.75);
        drop(b);
        for _ in 1..PREBUFFER_SAMPLES {
            mixer.pop();
        }
        a.send(vec![0.25f32; FRAME_SAMPLES]).unwrap();
        a.send(vec![0.25f32; FRAME_SAMPLES]).unwrap();
        mixer.take_in();
        assert_eq!(mixer.voices.len(), 1, "the ended voice is let go once played out");
        assert_eq!(mixer.pop(), 0.25);
        let (c, c_rx) = mpsc::unbounded_channel();
        sources.send(c_rx).unwrap();
        for _ in 0..2 {
            a.send(vec![0.9f32; FRAME_SAMPLES]).unwrap();
            c.send(vec![0.9f32; FRAME_SAMPLES]).unwrap();
        }
        mixer.take_in();
        for _ in 0..FRAME_SAMPLES {
            mixer.pop();
        }
        assert_eq!(mixer.pop(), 1.0);
    }

    #[test]
    fn playback_waits_for_a_little_audio_before_starting() {
        let mut playout = Playout::new();
        playout.push(&vec![0.5f32; FRAME_SAMPLES]);
        assert_eq!(playout.pop(), None, "one frame isn't enough to start on");
        playout.push(&vec![0.5f32; FRAME_SAMPLES]);
        for _ in 0..PREBUFFER_SAMPLES {
            assert_eq!(playout.pop(), Some(0.5));
        }
        playout.push(&vec![0.5f32; FRAME_SAMPLES]);
        assert_eq!(playout.pop(), None);
    }

    #[test]
    fn rates_change_by_the_right_amount() {
        for (from, to) in [(48_000, 48_000), (44_100, 48_000), (48_000, 44_100), (16_000, 48_000)] {
            let mut linear = Linear::new(from, to);
            let mut out = Vec::new();
            for i in 0..from {
                linear.push((i as f32 / 10.0).sin(), &mut out);
            }
            let expected = to as i64;
            assert!((out.len() as i64 - expected).abs() <= 2, "{from}->{to}: {} samples", out.len());
        }
    }

    /// The voice settings are the process's: tests that change them take
    /// turns.
    static VOICE_SET: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const RATE: usize = swarm_core::call::SAMPLE_RATE as usize;

    /// Speech-like: noise through a gentle low-pass, in bursts a few
    /// syllables long, every third second silent.
    fn speech(seconds: usize) -> Vec<f32> {
        let mut noise = noise(7);
        let mut low = 0f32;
        (0..RATE * seconds)
            .map(|i| {
                low += (noise() - low) * 0.2;
                let syllable = ((i as f32 / RATE as f32) * 4.0 * std::f32::consts::PI).sin().max(0.0);
                low * syllable * if is_silent(i / RATE) { 0.0 } else { 1.0 }
            })
            .collect()
    }

    fn is_silent(second: usize) -> bool {
        second % 3 == 2
    }

    fn noise(mut seed: u64) -> impl FnMut() -> f32 {
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 20_000) as f32 / 10_000.0 - 1.0
        }
    }

    fn power(x: &[f32]) -> f32 {
        x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32
    }

    fn db(power: f32) -> f32 {
        10.0 * power.max(1e-12).log10()
    }

    /// The microphone's audio as it's sent, cleaned up as `voice` says,
    /// with the speaker playing `played` meanwhile.
    fn sent(voice: Voice, mic: &[f32], played: &[f32]) -> Vec<f32> {
        set_voice(voice);
        let (played_tx, played_rx) = std::sync::mpsc::sync_channel(MOST_PLAYED_BLOCKS_WAITING);
        let mut cleanup = Cleanup::new(played_rx);
        let mut sent = Vec::new();
        for at in (0..mic.len() / CLEANUP_BLOCK).map(|b| b * CLEANUP_BLOCK) {
            played_tx.send(played[at..at + CLEANUP_BLOCK].to_vec()).unwrap();
            let mut block = mic[at..at + CLEANUP_BLOCK].to_vec();
            cleanup.take_played();
            cleanup.process(&mut block);
            sent.extend_from_slice(&block);
        }
        sent
    }

    const ONLY_ECHO: Voice = Voice { cancel_echo: true, ..Voice::NONE };

    /// A loud speaker in a small room: the echo comes back 60ms late,
    /// spread over 30ms, as loud as the voice of the one talking. Within
    /// seconds, almost none of it is sent; and when the echo's delay
    /// jumps (the speaker turned up, a Bluetooth one connected), it's
    /// found again.
    #[test]
    fn echo_is_cancelled_and_found_again_when_it_moves() {
        let _turn = VOICE_SET.lock().unwrap_or_else(|e| e.into_inner());
        let played = speech(16);
        let room: [(usize, f32); 7] = [(0, 0.5), (90, -0.3), (230, 0.2), (480, -0.15), (700, 0.1), (1000, -0.06), (1400, 0.04)];
        let mic: Vec<f32> = (0..played.len())
            .map(|i| {
                let delay = if i < RATE * 8 { RATE * 60 / 1000 } else { RATE * 150 / 1000 };
                room.iter().filter(|(k, _)| i >= delay + k).map(|(k, h)| h * played[i - delay - k]).sum()
            })
            .collect();
        let sent = sent(ONLY_ECHO, &mic, &played);
        let taken_out = |from: usize, to: usize| db(power(&mic[from * RATE..to * RATE])) - db(power(&sent[from * RATE..to * RATE]));
        let settled = taken_out(4, 8);
        assert!(settled > 30.0, "only {settled:.1}dB of the echo taken out");
        let moved = taken_out(11, 16);
        assert!(moved > 30.0, "only {moved:.1}dB of the echo taken out after it moved");
    }

    /// A fan's steady hiss under the voice: once it's been heard a few
    /// seconds, it's turned well down between words, and the voice isn't.
    #[test]
    fn steady_noise_is_turned_down_and_the_voice_kept() {
        let _turn = VOICE_SET.lock().unwrap_or_else(|e| e.into_inner());
        let voice = speech(15).iter().map(|v| v * 0.5).collect::<Vec<f32>>();
        let mut hiss = noise(99);
        let mic: Vec<f32> = voice.iter().map(|v| v + hiss() * 0.02).collect();
        let sent = sent(Voice { suppress_noise: true, ..Voice::NONE }, &mic, &vec![0.0; mic.len()]);
        let seconds = |silent: bool| (5..15).filter(move |s| is_silent(*s) == silent);
        let level = |x: &[f32], silent: bool| db(seconds(silent).map(|s| power(&x[s * RATE..(s + 1) * RATE])).sum::<f32>());
        let hiss_down = level(&mic, true) - level(&sent, true);
        assert!(hiss_down > 6.0, "the hiss only {hiss_down:.1}dB down");
        let voice_down = level(&mic, false) - level(&sent, false);
        assert!(voice_down < 3.0, "the voice {voice_down:.1}dB down");
    }

    /// A quiet microphone (or someone far from it) is brought up to an
    /// even level within seconds, and never past full scale.
    #[test]
    fn a_quiet_voice_is_brought_up() {
        let _turn = VOICE_SET.lock().unwrap_or_else(|e| e.into_inner());
        let mic = speech(15).iter().map(|v| v * 0.01).collect::<Vec<f32>>();
        let sent = sent(Voice { steady_level: true, ..Voice::NONE }, &mic, &vec![0.0; mic.len()]);
        let talking = |x: &[f32]| db((10..15).filter(|s| !is_silent(*s)).map(|s| power(&x[s * RATE..(s + 1) * RATE])).sum::<f32>());
        let up = talking(&sent) - talking(&mic);
        assert!(up > 10.0, "only {up:.1}dB up");
        assert!(sent.iter().all(|v| v.abs() <= 1.0));
    }

    /// With everything off, the microphone's audio is sent as it was.
    #[test]
    fn with_nothing_chosen_the_microphone_is_untouched() {
        let _turn = VOICE_SET.lock().unwrap_or_else(|e| e.into_inner());
        let mic = speech(3);
        assert_eq!(sent(Voice::NONE, &mic, &mic), mic);
    }

    #[test]
    fn same_rate_passes_samples_through() {
        let mut linear = Linear::new(48_000, 48_000);
        let mut out = Vec::new();
        for s in [0.1, 0.2, 0.3] {
            linear.push(s, &mut out);
        }
        assert_eq!(out, vec![0.0, 0.1, 0.2]);
    }
}
