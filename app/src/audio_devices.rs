//! The microphone and speaker chosen in Settings, each kept by name (the
//! settings file's `microphone` and `speaker`) as the camera's is (see
//! `crate::camera`). None chosen, or the one chosen not plugged in: the
//! system's own, which follows the system's sound settings.
//!
//! A sound server's "monitor" of a speaker ("Monitor of …": what that
//! speaker plays) is offered as a microphone too, to send what's playing
//! rather than what's heard.
//!
//! A choice takes effect at once: `choose` moves `CHANGES` on, and a call
//! going (or Settings' meter) opens the device now chosen (see
//! `crate::audio`).

use cpal::traits::{DeviceTrait, HostTrait};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Which way the sound goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Way {
    Microphone,
    Speaker,
}

impl Way {
    /// The settings file's key.
    fn key(self) -> &'static str {
        match self {
            Way::Microphone => "microphone",
            Way::Speaker => "speaker",
        }
    }
}

/// The first choice: none chosen, the system's own.
pub const SYSTEM: &str = "the system's own";

/// How many times a choice has been made, for the audio thread to see
/// that one has.
static CHANGES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn changes() -> u64 {
    CHANGES.load(Ordering::Relaxed)
}

/// The devices there are, by name (a name shared with one before it gets
/// a number, "USB Audio 2"). Kept for a few seconds: Settings asks each
/// time it's drawn.
pub fn names(way: Way) -> Vec<String> {
    type Seen = Option<(Instant, Vec<String>)>;
    static MICROPHONES: std::sync::Mutex<Seen> = std::sync::Mutex::new(None);
    static SPEAKERS: std::sync::Mutex<Seen> = std::sync::Mutex::new(None);
    let look = || found(&cpal::default_host(), way).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
    let Ok(mut seen) = (match way {
        Way::Microphone => &MICROPHONES,
        Way::Speaker => &SPEAKERS,
    })
    .lock() else {
        return look();
    };
    match &*seen {
        Some((at, names)) if at.elapsed() < Duration::from_secs(3) => names.clone(),
        _ => {
            let names = look();
            *seen = Some((Instant::now(), names.clone()));
            names
        }
    }
}

/// Each device and its name, in the system's order.
fn found(host: &cpal::Host, way: Way) -> Vec<(String, cpal::Device)> {
    let devices = match way {
        Way::Microphone => host.input_devices().map(|d| d.collect::<Vec<_>>()),
        Way::Speaker => host.output_devices().map(|d| d.collect::<Vec<_>>()),
    };
    let Ok(devices) = devices else { return Vec::new() };
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for device in devices {
        let Ok(about) = device.description() else { continue };
        let fits = match way {
            Way::Microphone => about.supports_input(),
            Way::Speaker => about.supports_output(),
        };
        if !fits {
            continue;
        }
        let name = about.name().to_string();
        let alike = seen.iter().filter(|n| **n == name).count();
        out.push((if alike == 0 { name.clone() } else { format!("{name} {}", alike + 1) }, device));
        seen.push(name);
    }
    out
}

/// The device to open: the one chosen if it's there, else the system's
/// own; and the name it was chosen by (None: the system's own), for
/// seeing later whether the choice has changed.
pub(crate) fn device(host: &cpal::Host, way: Way) -> (Option<cpal::Device>, Option<String>) {
    let chosen = chosen(way);
    let device = chosen
        .as_ref()
        .and_then(|name| found(host, way).into_iter().find(|(n, _)| n == name).map(|(_, d)| d))
        .or_else(|| match way {
            Way::Microphone => host.default_input_device(),
            Way::Speaker => host.default_output_device(),
        });
    (device, chosen)
}

/// The one chosen in Settings, by name: `None`, the system's own.
pub fn chosen(way: Way) -> Option<String> {
    swarm_core::settings::get(&crate::paths().settings(), way.key()).filter(|name| !name.is_empty())
}

/// Chooses the device (`None`: the system's own), for calls going and
/// to come.
pub fn choose(path: &std::path::Path, way: Way, name: Option<&str>) -> std::io::Result<()> {
    let saved = swarm_core::settings::set(path, way.key(), name.unwrap_or(""));
    CHANGES.fetch_add(1, Ordering::Relaxed);
    saved
}

/// The choices as the window lists them, and which is chosen: the
/// system's own first, then each found; the one chosen, though not
/// plugged in, last, saying so.
pub fn choices(way: Way) -> (Vec<String>, usize) {
    listed(names(way), chosen(way))
}

fn listed(found: Vec<String>, chosen: Option<String>) -> (Vec<String>, usize) {
    let mut names = vec![capitalised(SYSTEM)];
    names.extend(found);
    let at = match chosen {
        Some(name) => names.iter().skip(1).position(|n| *n == name).map(|i| i + 1).unwrap_or_else(|| {
            names.push(format!("{name} (not plugged in)"));
            names.len() - 1
        }),
        None => 0,
    };
    (names, at)
}

fn capitalised(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

/// Chooses the one at `index` in `choices` (0: the system's own; one not
/// plugged in stays chosen).
pub fn choose_at(path: &std::path::Path, way: Way, index: usize) -> std::io::Result<()> {
    match index {
        0 => choose(path, way, None),
        i => match names(way).get(i - 1) {
            Some(name) => choose(path, way, Some(name)),
            None => Ok(()),
        },
    }
}

/// The next choice after the one chosen (after the last, the system's
/// own again), for Settings in the terminal.
pub fn choose_next(path: &std::path::Path, way: Way) -> std::io::Result<()> {
    let names = names(way);
    let at = chosen(way).and_then(|name| names.iter().position(|n| *n == name)).map_or(0, |i| i + 1);
    choose(path, way, names.get(at).map(|n| n.as_str()))
}

/// The device as the terminal's Settings shows it.
pub fn choice_label(way: Way) -> String {
    let names = names(way);
    match chosen(way) {
        None => format!("{SYSTEM} ({} to choose from)", names.len()),
        Some(name) if !names.contains(&name) => format!("{name} (not plugged in: the system's own meanwhile)"),
        Some(name) => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_systems_own_comes_first_and_one_unplugged_last() {
        let found = vec!["Built-in".to_string(), "Headset".to_string()];
        assert_eq!(listed(found.clone(), None), (vec!["The system's own".to_string(), "Built-in".to_string(), "Headset".to_string()], 0));
        assert_eq!(listed(found.clone(), Some("Headset".to_string())).1, 2);
        let (names, at) = listed(found, Some("USB".to_string()));
        assert_eq!((names[at].as_str(), at), ("USB (not plugged in)", 3));
    }
}
