//! Small preferences a front end keeps between runs (`~/.swarm/settings`,
//! one `key=value` per line) — such as the folder downloads last went to,
//! how chats are shown, and which peers' calls are answered without asking.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use std::path::Path;

/// `key`'s value, if it has one.
pub fn get(path: &Path, key: &str) -> Option<String> {
    crate::store::read(path).ok()?.lines().find_map(|line| line.strip_prefix(key)?.strip_prefix('=').map(str::to_string))
}

/// Sets `key` to `value` (one line: control characters become spaces).
pub fn set(path: &Path, key: &str, value: &str) -> std::io::Result<()> {
    let _guard = crate::store::lock();
    let value = crate::text::single_line(value);
    let mut lines: Vec<String> = crate::store::read(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.starts_with(&format!("{key}=")))
        .map(str::to_string)
        .collect();
    lines.push(format!("{key}={value}"));
    crate::store::write_atomic(path, lines.join("\n") + "\n")
}

/// The setting for this device dialling no one: it only takes
/// connections made to it (see `node::devices::set_receive_only`).
const RECEIVE_ONLY: &str = "receive-only";

/// Whether this device is set to receive connections only.
pub fn receive_only(path: &Path) -> bool {
    get(path, RECEIVE_ONLY).as_deref() == Some("yes")
}

pub fn set_receive_only(path: &Path, on: bool) -> std::io::Result<()> {
    set(path, RECEIVE_ONLY, if on { "yes" } else { "no" })
}

/// The setting for the most this device sends, in bits a second (see
/// `transport::set_bandwidth_limit`): this device's own, as its link is.
const BANDWIDTH_LIMIT: &str = "bandwidth-limit";

/// This device's bandwidth limit, if it has one.
pub fn bandwidth_limit(path: &Path) -> Option<u64> {
    get(path, BANDWIDTH_LIMIT)?.parse().ok().filter(|&l: &u64| l > 0)
}

pub fn set_bandwidth_limit(path: &Path, limit: Option<u64>) -> std::io::Result<()> {
    set(path, BANDWIDTH_LIMIT, &limit.unwrap_or(0).to_string())
}

/// The setting for the most this device wants sent to it, in bits a
/// second: its contacts are asked to keep under it (see
/// `protocol::Notice::ReceiveLimit`). This device's own, as its link is.
const RECEIVE_LIMIT: &str = "receive-limit";

/// This device's receive limit, if it has one.
pub fn receive_limit(path: &Path) -> Option<u64> {
    get(path, RECEIVE_LIMIT)?.parse().ok().filter(|&l: &u64| l > 0)
}

pub fn set_receive_limit(path: &Path, limit: Option<u64>) -> std::io::Result<()> {
    set(path, RECEIVE_LIMIT, &limit.unwrap_or(0).to_string())
}

/// Whether a peer's calls are answered without asking — for a device
/// left listening somewhere (a radio monitor, say). Off unless set, and
/// only ever for one named peer: it's filed under their root key, so it
/// stays with them through a rename and can't apply to anyone else.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AutoAnswer {
    #[default]
    Off,
    /// Voice calls; a video call is answered with voice only.
    Voice,
    /// Voice calls, and video calls with video.
    VoiceAndVideo,
}

impl AutoAnswer {
    pub fn name(self) -> &'static str {
        match self {
            AutoAnswer::Off => "off",
            AutoAnswer::Voice => "voice",
            AutoAnswer::VoiceAndVideo => "video",
        }
    }

    /// `off`, `voice`, or `video` (voice and video).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "no" | "" => Some(AutoAnswer::Off),
            "voice" | "audio" | "on" => Some(AutoAnswer::Voice),
            "video" | "both" => Some(AutoAnswer::VoiceAndVideo),
            _ => None,
        }
    }

    /// Off, voice, voice and video, off again.
    pub fn next(self) -> Self {
        match self {
            AutoAnswer::Off => AutoAnswer::Voice,
            AutoAnswer::Voice => AutoAnswer::VoiceAndVideo,
            AutoAnswer::VoiceAndVideo => AutoAnswer::Off,
        }
    }

    /// For a call offering video or not: `None` to ring as usual, or
    /// `Some(with_video)` to answer straight away.
    pub fn answers(self, video_offered: bool) -> Option<bool> {
        match self {
            AutoAnswer::Off => None,
            AutoAnswer::Voice => Some(false),
            AutoAnswer::VoiceAndVideo => Some(video_offered),
        }
    }
}

fn auto_answer_key(peer: &[u8; 32]) -> String {
    format!("answer.{}", URL_SAFE_NO_PAD.encode(peer))
}

/// Whether calls from `peer` (their root key) are answered without asking.
pub fn auto_answer(path: &Path, peer: &[u8; 32]) -> AutoAnswer {
    get(path, &auto_answer_key(peer)).and_then(|v| AutoAnswer::parse(&v)).unwrap_or_default()
}

pub fn set_auto_answer(path: &Path, peer: &[u8; 32], answer: AutoAnswer) -> std::io::Result<()> {
    set(path, &auto_answer_key(peer), answer.name())
}

fn auto_control_key(peer: &[u8; 32]) -> String {
    format!("control.{}", URL_SAFE_NO_PAD.encode(peer))
}

/// Whether a peer (their root key, or another of your devices' own)
/// may control this screen without being asked each time: someone
/// trusted helping a person who can't see the screen. It only ever
/// answers their asking for it, in a call (the screen shared for it if
/// it isn't, even in a call answered by itself), and Stop, or using
/// your own mouse, still works. Off unless set.
pub fn auto_control(path: &Path, peer: &[u8; 32]) -> bool {
    get(path, &auto_control_key(peer)).is_some_and(|v| v == "yes")
}

pub fn set_auto_control(path: &Path, peer: &[u8; 32], on: bool) -> std::io::Result<()> {
    set(path, &auto_control_key(peer), if on { "yes" } else { "no" })
}

fn receive_to_key(peer: &[u8; 32]) -> String {
    format!("receive_to.{}", URL_SAFE_NO_PAD.encode(peer))
}

/// The folder files from `peer` are saved to without asking, if they're
/// set to be (off unless set: anyone else's offers wait for an answer).
pub fn receive_to(path: &Path, peer: &[u8; 32]) -> Option<std::path::PathBuf> {
    get(path, &receive_to_key(peer)).filter(|f| !f.trim().is_empty()).map(std::path::PathBuf::from)
}

/// Sets (or, `None`, clears) the folder `peer`'s files go to unasked.
pub fn set_receive_to(path: &Path, peer: &[u8; 32], folder: Option<&Path>) -> std::io::Result<()> {
    if folder.is_none() && get(path, &receive_to_key(peer)).is_none() {
        return Ok(());
    }
    set(path, &receive_to_key(peer), &folder.map(|f| f.display().to_string()).unwrap_or_default())
}

fn named_after_them_key(peer: &[u8; 32]) -> String {
    format!("named_after_them.{}", URL_SAFE_NO_PAD.encode(peer))
}

/// Whether `peer` was added without a name, so their entry takes the
/// name they publish (and follows it when they change it) until it's
/// renamed by hand.
pub fn named_after_them(path: &Path, peer: &[u8; 32]) -> bool {
    get(path, &named_after_them_key(peer)).as_deref() == Some("yes")
}

pub fn set_named_after_them(path: &Path, peer: &[u8; 32], on: bool) -> std::io::Result<()> {
    if !on && get(path, &named_after_them_key(peer)).is_none() {
        return Ok(());
    }
    set(path, &named_after_them_key(peer), if on { "yes" } else { "no" })
}

fn removed_us_key(peer: &[u8; 32]) -> String {
    format!("removed_us.{}", URL_SAFE_NO_PAD.encode(peer))
}

/// Whether `peer` has told us they've removed us
/// (`protocol::Notice::Removed`): they're not dialled till they say
/// otherwise, and the front ends ask for them to be removed here too.
pub fn removed_us(path: &Path, peer: &[u8; 32]) -> bool {
    get(path, &removed_us_key(peer)).as_deref() == Some("yes")
}

pub fn set_removed_us(path: &Path, peer: &[u8; 32], on: bool) -> std::io::Result<()> {
    if on == removed_us(path, peer) {
        return Ok(());
    }
    set(path, &removed_us_key(peer), if on { "yes" } else { "no" })
}

fn asked_key(peer: &[u8; 32]) -> String {
    format!("asked.{}", URL_SAFE_NO_PAD.encode(peer))
}

/// Whether `peer` was last heard to say they haven't added us: our
/// request to them is out, and can be cancelled. Remembered, as they may
/// be away for long after.
pub fn asked(path: &Path, peer: &[u8; 32]) -> bool {
    get(path, &asked_key(peer)).as_deref() == Some("yes")
}

pub fn set_asked(path: &Path, peer: &[u8; 32], on: bool) -> std::io::Result<()> {
    if on == asked(path, peer) {
        return Ok(());
    }
    set(path, &asked_key(peer), if on { "yes" } else { "no" })
}

/// Forgets everything set for `peer` (their root key): auto-answer,
/// control without asking,
/// the folder their files go to unasked, being named after themselves,
/// their having removed us, our request to them —
/// for a peer who's been removed, so adding them again some day starts
/// afresh instead of quietly answering their calls.
pub fn forget_peer(path: &Path, peer: &[u8; 32]) -> std::io::Result<()> {
    let _guard = crate::store::lock();
    let Ok(text) = crate::store::read(path) else { return Ok(()) };
    let theirs = [auto_answer_key(peer), auto_control_key(peer), receive_to_key(peer), named_after_them_key(peer), removed_us_key(peer), asked_key(peer)].map(|k| format!("{k}="));
    let kept: Vec<&str> = text.lines().filter(|line| !theirs.iter().any(|k| line.starts_with(k.as_str()))).collect();
    if kept.len() == text.lines().count() {
        return Ok(());
    }
    crate::store::write_atomic(path, kept.iter().map(|l| format!("{l}\n")).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_removed_peers_settings_are_forgotten_and_no_one_elses() {
        let path = std::env::temp_dir().join(format!("swarm-settings-forget-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (gone, stays) = ([1u8; 32], [2u8; 32]);
        for key in [gone, stays] {
            set_auto_answer(&path, &key, AutoAnswer::Voice).unwrap();
            set_auto_control(&path, &key, true).unwrap();
            set_receive_to(&path, &key, Some(Path::new("/srv/in"))).unwrap();
            set_named_after_them(&path, &key, true).unwrap();
        }
        set(&path, "downloads", "/home/me").unwrap();
        forget_peer(&path, &gone).unwrap();
        assert_eq!(auto_answer(&path, &gone), AutoAnswer::Off);
        assert!(!auto_control(&path, &gone));
        assert_eq!(receive_to(&path, &gone), None);
        assert!(!named_after_them(&path, &gone));
        assert_eq!(auto_answer(&path, &stays), AutoAnswer::Voice);
        assert!(auto_control(&path, &stays));
        assert!(receive_to(&path, &stays).is_some() && named_after_them(&path, &stays));
        assert_eq!(get(&path, "downloads").as_deref(), Some("/home/me"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn auto_answer_is_off_unless_set_and_per_key() {
        let path = std::env::temp_dir().join(format!("swarm-settings-answer-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (a, b) = ([1u8; 32], [2u8; 32]);
        assert_eq!(auto_answer(&path, &a), AutoAnswer::Off);
        set_auto_answer(&path, &a, AutoAnswer::VoiceAndVideo).unwrap();
        assert_eq!(auto_answer(&path, &a), AutoAnswer::VoiceAndVideo);
        assert_eq!(auto_answer(&path, &b), AutoAnswer::Off);
        assert_eq!(AutoAnswer::Voice.answers(true), Some(false));
        assert_eq!(AutoAnswer::VoiceAndVideo.answers(true), Some(true));
        assert_eq!(AutoAnswer::Off.answers(false), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn values_are_set_replaced_and_read() {
        let path = std::env::temp_dir().join(format!("swarm-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(get(&path, "downloads"), None);
        set(&path, "downloads", "/home/me/Downloads").unwrap();
        set(&path, "other", "x").unwrap();
        set(&path, "downloads", "/srv/files").unwrap();
        assert_eq!(get(&path, "downloads").as_deref(), Some("/srv/files"));
        assert_eq!(get(&path, "other").as_deref(), Some("x"));
    }
}
