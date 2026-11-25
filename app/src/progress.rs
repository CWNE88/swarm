//! How the files going to or from peers are getting on, in one line, for
//! the top of the screen (the Transfers view has the details): what, how
//! far, how fast and how long is left.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use swarm_core::node::TransferStatus;
use swarm_core::transfer::{Direction, State};

/// Files under way: what to call them, how far they've got, how fast
/// they're going and how long is left.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// "↑ report.pdf" for one file (↑ sending, ↓ receiving), "3 files"
    /// for several.
    pub what: String,
    /// 0 to 100, of all of them together.
    pub percent: u8,
    /// "3.21 MB/s", at most [`SPEED_CHARS`] long; empty until there's a
    /// second to go on.
    pub speed: String,
    /// "1:05 left", at most [`LEFT_CHARS`] long; empty while the speed
    /// isn't known or nothing is moving.
    pub left: String,
}

/// The longest a file's name is shown here before it's shortened.
const NAME_CHARS: usize = 24;
/// The widest [`Progress::speed`] is, for a column that keeps still.
pub const SPEED_CHARS: usize = 9;
/// The widest [`Progress::left`] is.
pub const LEFT_CHARS: usize = 12;

/// The speed is over this last stretch, so it doesn't jump about.
const WINDOW: Duration = Duration::from_secs(5);
/// Shown once it's over at least this long.
const FIRST: Duration = Duration::from_secs(1);
/// At most one reading in this long is kept.
const STEP: Duration = Duration::from_millis(200);

/// Keeps the last few seconds of how far the files under way have got,
/// to tell how fast they're going. One for each front end, asked each
/// time the top line is drawn.
#[derive(Default)]
pub struct Meter {
    /// When, bytes done, and the total size then.
    readings: Mutex<VecDeque<(Instant, u128, u128)>>,
}

impl Meter {
    /// Every transfer under way, together; none if nothing is.
    pub fn of(&self, all: &[TransferStatus]) -> Option<Progress> {
        self.at(all, Instant::now())
    }

    fn at(&self, all: &[TransferStatus], now: Instant) -> Option<Progress> {
        let going: Vec<&TransferStatus> = all.iter().filter(|t| t.transfer.state == State::Active).collect();
        let (done, size) = going.iter().fold((0u128, 0u128), |(d, s), t| (d + t.done_bytes.min(t.transfer.size) as u128, s + t.transfer.size as u128));
        let mut readings = self.readings.lock().unwrap_or_else(|e| e.into_inner());
        let what = match going.as_slice() {
            [] => {
                readings.clear();
                return None;
            }
            [one] => {
                let arrow = if one.transfer.direction == Direction::Out { "↑" } else { "↓" };
                format!("{arrow} {}", crate::text::shorten_middle(&one.transfer.name, NAME_CHARS))
            }
            several => format!("{} files", several.len()),
        };
        let percent = (done * 100).checked_div(size).map_or(0, |p| p.min(100) as u8);
        if readings.back().is_some_and(|&(_, d, s)| s != size || done < d) {
            readings.clear();
        }
        if readings.back().is_none_or(|&(t, _, _)| now.saturating_duration_since(t) >= STEP) {
            readings.push_back((now, done, size));
        }
        while readings.len() > 2 && readings.get(1).is_some_and(|&(t, _, _)| now.saturating_duration_since(t) >= WINDOW) {
            readings.pop_front();
        }
        let (mut speed, mut left) = (String::new(), String::new());
        if let Some(&(since, from, _)) = readings.front() {
            let took = now.saturating_duration_since(since);
            if took >= FIRST {
                let per_second = (done - from) as f64 / took.as_secs_f64();
                speed = speed_text(per_second);
                if per_second >= 1.0 {
                    left = format!("{} left", clock((size - done) as f64 / per_second));
                }
            }
        }
        Some(Progress { what, percent, speed, left })
    }
}

/// "3.21 MB/s", "45.6 KB/s", " 812 KB/s": always three figures, so it's
/// always the same length.
fn speed_text(bytes_per_second: f64) -> String {
    const UNITS: [&str; 4] = ["KB/s", "MB/s", "GB/s", "TB/s"];
    let mut value = bytes_per_second / 1000.0;
    let mut unit = 0;
    while value >= 999.5 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    let number = if value < 9.995 {
        format!("{value:.2}")
    } else if value < 99.95 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    };
    format!("{number} {}", UNITS[unit])
}

/// "0:42", "12:05", "1:02:09"; "10 h+" past that.
fn clock(seconds: f64) -> String {
    let s = seconds.ceil() as u64;
    if s >= 10 * 3600 {
        "10 h+".into()
    } else if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::transfer::Transfer;

    fn status(name: &str, direction: Direction, state: State, size: u64, done: u64) -> TransferStatus {
        let transfer = Transfer { id: [0; 16], direction, peer: [0; 32], state, size, sha256: [0; 32], created: 0, name: name.into(), path: Default::default(), note: String::new(), cleared: false, settled: false };
        TransferStatus { transfer, peer_name: None, peer_key: None, done_bytes: done }
    }

    #[test]
    fn nothing_under_way_shows_nothing() {
        assert_eq!(Meter::default().of(&[status("a", Direction::Out, State::Done, 10, 10)]), None);
    }

    #[test]
    fn one_file_by_name_several_together() {
        let meter = Meter::default();
        let one = status("photo.jpg", Direction::In, State::Active, 200, 50);
        let shown = meter.of(std::slice::from_ref(&one)).unwrap();
        assert_eq!((shown.what.as_str(), shown.percent), ("↓ photo.jpg", 25));
        let two = status("a-very-long-name-for-a-file-indeed.tar", Direction::Out, State::Active, 200, 150);
        assert_eq!(meter.of(std::slice::from_ref(&two)).unwrap().what, "↑ a-very-long-name-fo….tar");
        let both = meter.of(&[one, two]).unwrap();
        assert_eq!((both.what.as_str(), both.percent), ("2 files", 50));
    }

    #[test]
    fn speed_and_time_left_after_a_second() {
        let meter = Meter::default();
        let start = Instant::now();
        let at = |seconds: f64, done: u64| meter.at(&[status("f", Direction::In, State::Active, 10_000_000, done)], start + Duration::from_secs_f64(seconds)).unwrap();
        let first = at(0.0, 0);
        assert_eq!((first.speed.as_str(), first.left.as_str()), ("", ""));
        assert_eq!(at(0.5, 1_000_000).speed, "");
        let later = at(2.0, 4_000_000);
        assert_eq!((later.speed.as_str(), later.left.as_str()), ("2.00 MB/s", "0:03 left"));
        for second in 3u32..=20 {
            at(f64::from(second), 4_000_000 + u64::from(second - 2) * 100_000);
        }
        assert_eq!(at(20.0, 5_800_000).speed, "100 KB/s");
    }

    #[test]
    fn speeds_and_times_keep_their_width() {
        for (rate, text) in [(0.0, "0.00 KB/s"), (9_994.0, "9.99 KB/s"), (9_996.0, "10.0 KB/s"), (812_000.0, "812 KB/s"), (999_600.0, "1.00 MB/s"), (3_210_000.0, "3.21 MB/s"), (5e12, "5.00 TB/s")] {
            assert_eq!(speed_text(rate), text);
            assert!(text.chars().count() <= SPEED_CHARS);
        }
        for (seconds, text) in [(0.2, "0:01"), (65.0, "1:05"), (3599.0, "59:59"), (3600.0, "1:00:00"), (35_999.0, "9:59:59"), (36_000.0, "10 h+")] {
            assert_eq!(clock(seconds), text);
            assert!(format!("{text} left").chars().count() <= LEFT_CHARS);
        }
    }
}
