//! Messages that delete themselves: each conversation has a timer —
//! off, or a time after which a message is deleted from both sides'
//! history. Either person sets it; both are told who did. A message
//! carries the timer it was sent under (its *lifetime*), so a later
//! change doesn't shorten or lengthen it: the sender's copy goes that
//! long after it was sent, the receiver's that long after it arrived.
//!
//! It's a courtesy between peers, not a guarantee: someone can always
//! copy what they read before it goes.
//!
//! The timer is kept in `settings` under the peer's key, with when it
//! was set and by whom. Each side tells the other its timer when they
//! connect and whenever it's set; the one set later wins on both (the
//! larger key if at the same second), so they agree however the
//! settings crossed.

use crate::lock::Locked;
use std::path::Path;

/// The timers to choose from, in seconds (0: off).
pub const CHOICES: [u64; 4] = [0, 60 * 60, 24 * 60 * 60, 7 * 24 * 60 * 60];

/// A conversation's timer: how long its messages last (0, for good),
/// when it was set (unix seconds; 0, never) and by whom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timer {
    pub seconds: u64,
    pub set_at: u64,
    pub by: [u8; 32],
}

impl Timer {
    /// Whether `self` was set after `other`, so wins over it.
    fn after(&self, other: &Timer) -> bool {
        (self.set_at, self.by) > (other.set_at, other.by)
    }

    /// Whether it could have been set by `now` (unix seconds), give or
    /// take a clock that's wrong by up to `MOST_AHEAD`. A peer says when
    /// theirs was set: one from far ahead would win over every timer set
    /// after it, and could never be changed again.
    pub fn believable(&self, now: u64) -> bool {
        self.set_at <= now.saturating_add(MOST_AHEAD)
    }
}

/// How far ahead of this device's clock a timer may say it was set.
pub const MOST_AHEAD: u64 = 24 * 60 * 60;

/// How a timer's said: "an hour", "a day", "a week" (or the seconds
/// counted out, for any other).
pub fn describe(seconds: u64) -> String {
    match seconds {
        0 => "off".to_string(),
        3600 => "an hour".to_string(),
        86_400 => "a day".to_string(),
        604_800 => "a week".to_string(),
        s if s % 86_400 == 0 => format!("{} days", s / 86_400),
        s if s % 3600 == 0 => format!("{} hours", s / 3600),
        s => format!("{} minutes", s.div_ceil(60)),
    }
}

/// A timer as typed: `off`, `hour`, `day` or `week` (or `1h`, `1d`, `1w`).
pub fn parse(text: &str) -> Option<u64> {
    match text.trim().to_lowercase().trim_start_matches("an ").trim_start_matches("a ").trim_start_matches('1') {
        "off" | "none" | "0" | "never" => Some(0),
        "h" | "hour" => Some(3600),
        "d" | "day" => Some(86_400),
        "w" | "week" => Some(604_800),
        _ => None,
    }
}

/// The choice after `seconds`, round to off again.
pub fn next(seconds: u64) -> u64 {
    let at = CHOICES.iter().position(|c| *c == seconds).unwrap_or(0);
    CHOICES[(at + 1) % CHOICES.len()]
}

fn key(peer: &[u8; 32]) -> String {
    format!("timer.{}", crate::peerstore::file_name(peer))
}

/// The conversation with `peer`'s timer (off, if it's never been set).
pub fn get(settings: &Path, peer: &[u8; 32]) -> Timer {
    let Some(value) = crate::settings::get(settings, &key(peer)) else { return Timer::default() };
    let mut fields = value.split(' ');
    let (Some(seconds), Some(set_at), Some(by)) = (fields.next(), fields.next(), fields.next()) else { return Timer::default() };
    match (seconds.parse(), set_at.parse(), crate::peerstore::key_from_name(by)) {
        (Ok(seconds), Ok(set_at), Some(by)) => Timer { seconds, set_at, by },
        _ => Timer::default(),
    }
}

/// Keeps `timer` for the conversation with `peer` if it was set later
/// than the one kept (or that one isn't `believable`): whether it was
/// (and so has changed).
pub fn merge(settings: &Path, peer: &[u8; 32], timer: Timer) -> std::io::Result<bool> {
    static MERGING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = MERGING.locked();
    let kept = get(settings, peer);
    if kept.believable(crate::unix_now()) && !timer.after(&kept) {
        return Ok(false);
    }
    crate::settings::set(settings, &key(peer), &format!("{} {} {}", timer.seconds, timer.set_at, crate::peerstore::file_name(&timer.by)))?;
    Ok(true)
}

/// When a message lasts until, from `at` (sent or arrived), under
/// `lifetime` seconds: 0, for good.
pub fn expires(at: u64, lifetime: u64) -> u64 {
    if lifetime == 0 { 0 } else { at.saturating_add(lifetime) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-disappearing-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("settings")
    }

    #[test]
    fn the_timer_set_later_wins_whichever_way_round_it_arrives() {
        let (alice, bob, peer) = ([1u8; 32], [2u8; 32], [9u8; 32]);
        let early = Timer { seconds: 3600, set_at: 100, by: alice };
        let late = Timer { seconds: 86_400, set_at: 200, by: bob };
        for (first, second) in [(early, late), (late, early)] {
            let path = settings(&format!("order-{}", first.set_at));
            assert_eq!(get(&path, &peer), Timer::default(), "off until set");
            assert!(merge(&path, &peer, first).unwrap());
            assert_eq!(merge(&path, &peer, second).unwrap(), second == late);
            assert_eq!(get(&path, &peer), late);
        }
        let path = settings("tie");
        let by_alice = Timer { seconds: 3600, set_at: 300, by: alice };
        let by_bob = Timer { seconds: 0, set_at: 300, by: bob };
        merge(&path, &peer, by_bob).unwrap();
        assert!(!merge(&path, &peer, by_alice).unwrap());
        assert_eq!(get(&path, &peer), by_bob);
    }

    #[test]
    fn choices_go_round_and_are_said_plainly() {
        assert_eq!(next(0), 3600);
        assert_eq!(next(604_800), 0);
        assert_eq!(describe(86_400), "a day");
        assert_eq!(describe(0), "off");
        assert_eq!(expires(100, 0), 0);
        assert_eq!(expires(100, 60), 160);
    }
}
