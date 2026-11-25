//! What this device has sent and received, by kind of frame, and the
//! dials it's made, by kind of address: counted all the time (a few
//! atomic adds each), and told in the log once an hour (`report`), so
//! where a device's traffic goes can be seen without a packet capture —
//! a phone's above all, which can't be captured on.
//!
//! Counted for the whole process: one node in it, but for tests.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// How often the counts are told, and started again.
pub const EVERY: Duration = Duration::from_secs(60 * 60);
/// When they're first told: soon enough to look at after starting.
pub const FIRST: Duration = Duration::from_secs(10 * 60);
/// How many kinds are named in a line, the most bytes first.
const NAMED: usize = 10;

struct Count {
    times: AtomicU64,
    bytes: AtomicU64,
}

impl Count {
    const fn new() -> Self {
        Count { times: AtomicU64::new(0), bytes: AtomicU64::new(0) }
    }

    fn add(&self, bytes: usize) {
        self.times.fetch_add(1, Relaxed);
        self.bytes.fetch_add(bytes as u64, Relaxed);
    }

    fn take(&self) -> (u64, u64) {
        (self.times.swap(0, Relaxed), self.bytes.swap(0, Relaxed))
    }
}

/// Frames on QUIC's stream, by tag (their first byte).
static SENT: [Count; 256] = [const { Count::new() }; 256];
static RECEIVED: [Count; 256] = [const { Count::new() }; 256];
/// Cover slots (QUIC datagrams), whatever they carry.
static SLOTS_SENT: Count = Count::new();
static SLOTS_RECEIVED: Count = Count::new();
/// Dials to one address, by its kind (`Reach`), answered or not.
static DIALS: [[AtomicU64; 2]; 3] = [const { [const { AtomicU64::new(0) }; 2] }; 3];

/// A frame of `bytes` on the wire (sealed, with its length), its first
/// plain byte `tag`.
pub(crate) fn sent(tag: u8, bytes: usize) {
    SENT[tag as usize].add(bytes);
}

pub(crate) fn received(tag: u8, bytes: usize) {
    RECEIVED[tag as usize].add(bytes);
}

pub(crate) fn slot_sent(bytes: usize) {
    SLOTS_SENT.add(bytes);
}

pub(crate) fn slot_received(bytes: usize) {
    SLOTS_RECEIVED.add(bytes);
}

/// The kind of address a dial went to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// IPv6 `fe80::/10`, IPv4 `169.254/16`: this link only.
    LinkLocal,
    /// A LAN's or a VPN's: private IPv4, IPv6 unique-local, and the
    /// carriers' shared `100.64/10`.
    Local,
    Global,
}

fn reach(address: SocketAddr) -> Reach {
    match crate::transport::canonical(address).ip() {
        IpAddr::V4(ip) if ip.is_link_local() => Reach::LinkLocal,
        IpAddr::V4(ip) if ip.is_private() || ip.is_loopback() || (ip.octets()[0] == 100 && ip.octets()[1] & 0xC0 == 64) => Reach::Local,
        IpAddr::V6(ip) if ip.is_unicast_link_local() => Reach::LinkLocal,
        IpAddr::V6(ip) if ip.is_unique_local() || ip.is_loopback() => Reach::Local,
        _ => Reach::Global,
    }
}

/// A dial to `address` is over: `answered`, or not.
pub(crate) fn dialled(address: SocketAddr, answered: bool) {
    DIALS[reach(address) as usize][answered as usize].fetch_add(1, Relaxed);
}

/// The counts since the last report, as lines for the log, and each
/// started again at nothing.
pub fn report() -> Vec<String> {
    let sent = take_frames(&SENT);
    let received = take_frames(&RECEIVED);
    let slots = (SLOTS_SENT.take(), SLOTS_RECEIVED.take());
    let dials: Vec<(Reach, u64, u64)> = [Reach::LinkLocal, Reach::Local, Reach::Global]
        .into_iter()
        .map(|r| {
            let [failed, answered] = &DIALS[r as usize];
            (r, failed.swap(0, Relaxed), answered.swap(0, Relaxed))
        })
        .collect();
    let mut lines = vec![frames_line("sent", &sent, slots.0), frames_line("received", &received, slots.1)];
    let dial_parts: Vec<String> = dials
        .iter()
        .filter(|(_, failed, answered)| failed + answered > 0)
        .map(|(r, failed, answered)| {
            let kind = match r {
                Reach::LinkLocal => "link-local",
                Reach::Local => "local",
                Reach::Global => "global",
            };
            format!("{} {kind} ({answered} answered)", failed + answered)
        })
        .collect();
    lines.push(if dial_parts.is_empty() { "dials: none".to_string() } else { format!("dials: {}", dial_parts.join(", ")) });
    lines
}

/// Each tag's (tag, times, bytes) with any, the most bytes first.
fn take_frames(counts: &[Count; 256]) -> Vec<(u8, u64, u64)> {
    let mut all: Vec<(u8, u64, u64)> = counts
        .iter()
        .enumerate()
        .filter_map(|(tag, c)| {
            let (times, bytes) = c.take();
            (times > 0).then_some((tag as u8, times, bytes))
        })
        .collect();
    all.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    all
}

fn frames_line(what: &str, frames: &[(u8, u64, u64)], (slots, slot_bytes): (u64, u64)) -> String {
    let bytes: u64 = frames.iter().map(|f| f.2).sum();
    let count: u64 = frames.iter().map(|f| f.1).sum();
    let mut line = format!("{what}: {} in {count} frames", size(bytes));
    let named: Vec<String> = frames.iter().take(NAMED).map(|(tag, times, bytes)| format!("{} {} ({times})", crate::protocol::tag_name(*tag), size(*bytes))).collect();
    if !named.is_empty() {
        line += &format!(" — {}", named.join(", "));
    }
    if frames.len() > NAMED {
        line += &format!(", {} more kinds", frames.len() - NAMED);
    }
    if slots > 0 {
        line += &format!("; cover slots {} ({slots})", size(slot_bytes));
    }
    line
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KB", bytes / 1024),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_told_apart_by_how_far_they_reach() {
        let r = |a: &str| reach(a.parse().unwrap());
        assert_eq!(r("[fe80::1%3]:1977"), Reach::LinkLocal);
        assert_eq!(r("169.254.4.5:1977"), Reach::LinkLocal);
        assert_eq!(r("192.168.3.10:1977"), Reach::Local);
        assert_eq!(r("100.72.0.1:1977"), Reach::Local);
        assert_eq!(r("[fd00::1]:1977"), Reach::Local);
        assert_eq!(r("[::ffff:10.0.0.1]:1977"), Reach::Local);
        assert_eq!(r("[2403:580b::10]:1977"), Reach::Global);
        assert_eq!(r("8.8.8.8:1977"), Reach::Global);
    }

    #[test]
    fn a_line_names_the_biggest_kinds_first() {
        let frames = vec![(0x88, 10, 5000), (0x01, 3, 90)];
        assert_eq!(frames_line("sent", &frames, (0, 0)), "sent: 4 KB in 13 frames — call audio 4 KB (10), ping 90 B (3)");
        assert_eq!(frames_line("received", &[], (2, 2400)), "received: 0 B in 0 frames; cover slots 2 KB (2)");
    }
}
