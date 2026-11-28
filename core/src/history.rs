//! Per-peer conversation history: every message sent to, or received
//! from, a known peer, kept indefinitely — unlike `outbox`, which only
//! holds a message until it's delivered, then drops it. This is what
//! lets a conversation build up over time instead of only ever showing
//! whatever's currently in flight.
//!
//! One flat file per peer under a `history` directory, named by the
//! peer's public key (see `peerstore` for why never by name), one
//! message per line: `<unix_secs>\t<direction>\t<expires>\t<text>`,
//! `direction` being `>` (sent by this device) or `<` (received from the
//! peer), and `expires` when it deletes itself (unix seconds; 0, never:
//! see `disappearing`) — same tab-separated, single-line-message
//! convention as `outbox`/`posts`. A message's id (`outbox::MessageId`,
//! in hex) comes between `expires` and the text; a missed call has none.
//!
//! A line is found again (to delete it, `remove`) by a `LineRef`: its id,
//! or, for a line without one, its time and a digest of its text.

use crate::lock::Locked;
use crate::outbox::MessageId;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::{peerstore, store};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Sent,
    Received,
    /// A call from them that nobody answered on any of the user's
    /// devices (`!`; its text says what kind: `call`, `video call`).
    /// Counted as from them, so it's unread until seen. A build from
    /// before these passes the line over.
    Missed,
}

impl Direction {
    pub fn tag(self) -> &'static str {
        match self {
            Direction::Sent => ">",
            Direction::Received => "<",
            Direction::Missed => "!",
        }
    }

    pub fn parse(tag: &str) -> Option<Self> {
        match tag {
            ">" => Some(Direction::Sent),
            "<" => Some(Direction::Received),
            "!" => Some(Direction::Missed),
            _ => None,
        }
    }

    /// From them: a message, or a call missed.
    pub fn theirs(self) -> bool {
        self != Direction::Sent
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub at: u64,
    pub direction: Direction,
    pub text: String,
    /// When it deletes itself (unix seconds); 0, never.
    pub expires: u64,
    /// The message's id, if it's known (see the module's docs).
    pub id: Option<MessageId>,
}

impl Entry {
    /// What finds this line again.
    pub fn line_ref(&self) -> LineRef {
        LineRef { direction: self.direction, id: self.id, at: self.at, digest: digest(&self.text) }
    }
}

/// One line of a conversation, as it's found again on any device that
/// has it, whether that one knows its id or not: on the sender's side
/// and the receiver's, a message has the same id, time and text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineRef {
    pub direction: Direction,
    pub id: Option<MessageId>,
    pub at: u64,
    /// The start of its text's SHA-256 (`digest`).
    pub digest: [u8; 16],
}

impl LineRef {
    /// Whether `entry` is this line: the same id if both have one,
    /// otherwise the same time and text; always the same direction.
    pub fn matches(&self, entry: &Entry) -> bool {
        if entry.direction != self.direction {
            return false;
        }
        match (self.id, entry.id) {
            (Some(a), Some(b)) => a == b,
            _ => entry.at == self.at && digest(&entry.text) == self.digest,
        }
    }

    /// As text, for a queue or a front end to keep: `<direction>:<id or
    /// ->:<at>:<digest>`.
    pub fn encode(&self) -> String {
        let id = self.id.map_or("-".to_string(), |id| crate::outbox::format_id(&id));
        let digest: String = self.digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}:{id}:{}:{digest}", self.direction.tag(), self.at)
    }

    /// `encode`'s text back.
    pub fn decode(text: &str) -> Option<LineRef> {
        let mut f = text.split(':');
        let direction = Direction::parse(f.next()?)?;
        let id = match f.next()? {
            "-" => None,
            id => Some(crate::outbox::parse_id(id)?),
        };
        let at = f.next()?.parse().ok()?;
        let digest = crate::outbox::parse_id(f.next()?)?;
        if f.next().is_some() {
            return None;
        }
        Some(LineRef { direction, id, at, digest })
    }
}

/// What `LineRef` knows of a text: the first 16 bytes of its SHA-256.
pub fn digest(text: &str) -> [u8; 16] {
    let hash = Sha256::digest(text.as_bytes());
    hash[..16].try_into().expect("SHA-256 is 32 bytes")
}

/// Appends one entry to `public_key`'s history, creating the directory
/// and file as needed. `at` is caller-supplied — always the message's
/// original send time, never when this device happened to write the
/// entry: for something just sent, those are (for all practical
/// purposes) the same instant; for something received after being
/// queued while this device was offline, they can be far apart, and
/// it's the *send* time that belongs here (see `protocol::Frame
/// ::Message`, which carries it across the wire for exactly this).
///
/// Control characters in `text` become spaces (see `crate::text`): a
/// newline in a peer's message must not become a forged history line.
/// A time further ahead than `CLOCK_SLACK` (a sender whose clock is
/// wrong) is taken as now — or it would sit at the end of the chat for
/// good, under a heading from some absurd year.
pub fn append(dir: &Path, public_key: &[u8; 32], direction: Direction, text: &str, at: u64) -> std::io::Result<()> {
    append_expiring(dir, public_key, direction, text, at, 0)
}

/// `append`, for a message that deletes itself at `expires` (unix
/// seconds; 0, never: see `disappearing`).
pub fn append_expiring(dir: &Path, public_key: &[u8; 32], direction: Direction, text: &str, at: u64, expires: u64) -> std::io::Result<()> {
    append_message(dir, public_key, direction, text, at, expires, None)
}

/// `append_expiring`, for a message whose id is known.
pub fn append_message(dir: &Path, public_key: &[u8; 32], direction: Direction, text: &str, at: u64, expires: u64, id: Option<MessageId>) -> std::io::Result<()> {
    let _guard = store::lock();
    append_held(dir, public_key, direction, text, at, expires, id)
}

/// `append_message`, with the store's lock already held.
fn append_held(dir: &Path, public_key: &[u8; 32], direction: Direction, text: &str, at: u64, expires: u64, id: Option<MessageId>) -> std::io::Result<()> {
    let entry = Entry { at: believable(at), direction, text: crate::text::message_text(text), expires, id };
    let path = peerstore::file_for(dir, public_key);
    let line = format_line(&entry);
    if is_late(entry.at, unix_now()) && path.exists() {
        let text = store::read(&path)?;
        let mut lines: Vec<&str> = text.lines().collect();
        let place = lines.iter().rposition(|l| parse_line(l).is_some_and(|e| e.at <= entry.at)).map_or(0, |i| i + 1);
        if place < lines.len() {
            lines.insert(place, &line);
            return store::write_atomic(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>());
        }
    }
    store::append_line(&path, &line)
}

/// Whether a message sent at `at` arrives late at `now`: long enough
/// after it was sent that it wasn't part of the conversation as it went.
fn is_late(at: u64, now: u64) -> bool {
    at.saturating_add(CLOCK_SLACK) < now
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// One line of the file, as `parse_line` reads it.
pub(crate) fn format_line(e: &Entry) -> String {
    match e.id {
        Some(id) => format!("{}\t{}\t{}\t{}\t{}", e.at, e.direction.tag(), e.expires, crate::outbox::format_id(&id), crate::text::to_field(&e.text)),
        None => format!("{}\t{}\t{}\t{}", e.at, e.direction.tag(), e.expires, crate::text::to_field(&e.text)),
    }
}

/// Deletes every line of the conversation with `public_key` that's
/// `line`. Whether there was one.
pub fn remove(dir: &Path, public_key: &[u8; 32], line: &LineRef) -> std::io::Result<bool> {
    let _guard = store::lock();
    let path = peerstore::file_for(dir, public_key);
    if !path.exists() {
        return Ok(false);
    }
    let text = store::read(&path)?;
    let kept: Vec<&str> = text.lines().filter(|l| !parse_line(l).is_some_and(|e| line.matches(&e))).collect();
    if kept.len() == text.lines().count() {
        return Ok(false);
    }
    store::write_atomic(&path, kept.iter().map(|l| format!("{l}\n")).collect::<String>())?;
    Ok(true)
}

/// Deletes every message in `dir` (every conversation) whose time is up
/// by `now`. How many went.
pub fn sweep(dir: &Path, now: u64) -> std::io::Result<usize> {
    let mut gone = 0;
    for (_, path) in store::names_in(dir) {
        let _guard = store::lock();
        let Ok(text) = store::read(&path) else { continue };
        let kept: Vec<&str> = text.lines().filter(|l| parse_line(l).is_none_or(|e| e.expires == 0 || e.expires > now)).collect();
        let dropped = text.lines().count() - kept.len();
        if dropped > 0 {
            store::write_atomic(&path, kept.iter().map(|l| format!("{l}\n")).collect::<String>())?;
            gone += dropped;
        }
    }
    Ok(gone)
}

/// How far ahead of this device's clock a message's time may be.
pub const CLOCK_SLACK: u64 = 5 * 60;

fn believable(at: u64) -> u64 {
    let now = unix_now();
    if at > now.saturating_add(CLOCK_SLACK) { now } else { at }
}

/// `append`, unless the same message (time, direction and text) is
/// already there — for a copy that may have arrived another way too.
/// Whether it was added.
pub fn append_new(dir: &Path, public_key: &[u8; 32], direction: Direction, text: &str, at: u64, expires: u64, id: Option<MessageId>) -> std::io::Result<bool> {
    let _guard = store::lock();
    let text = crate::text::message_text(text);
    let at = believable(at);
    let there = KEPT.with(&peerstore::file_for(dir, public_key), parse_line, |all| {
        all.iter().rev().take(500).any(|e| e.at == at && e.direction == direction && e.text == text)
    })?;
    if there {
        return Ok(false);
    }
    append_held(dir, public_key, direction, &text, at, expires, id)?;
    Ok(true)
}

/// The full conversation with `public_key`, oldest first. A peer with no
/// history yet reads as empty, not an error.
///
/// Kept in memory for the few conversations last asked for, and only
/// what's been added since read (see `store::Kept`): a screen showing one asks
/// for it every redraw, and a long one took a whole core re-reading it.
pub fn all(dir: &Path, public_key: &[u8; 32]) -> std::io::Result<Vec<Entry>> {
    KEPT.read(&peerstore::file_for(dir, public_key), parse_line)
}

/// The newest `most` of `all` — for a screen that shows only the end.
pub fn last(dir: &Path, public_key: &[u8; 32], most: usize) -> std::io::Result<Vec<Entry>> {
    KEPT.with(&peerstore::file_for(dir, public_key), parse_line, |all| all[all.len().saturating_sub(most)..].to_vec())
}

static KEPT: store::Kept<Entry> = store::Kept::new(4);

/// What the side list needs of a conversation: when the last message
/// was, and how many have come from them (missed calls among them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub last_at: u64,
    /// When the newest message (or missed call) from them was (0: none yet).
    pub last_received: u64,
    pub received: usize,
    /// Every message, both ways.
    pub count: usize,
}

/// `public_key`'s `Summary` — kept for every conversation, and brought
/// up to date from only what's been added since (it's asked for every
/// conversation, several times a second).
pub fn summary(dir: &Path, public_key: &[u8; 32]) -> std::io::Result<Summary> {
    static KEPT: Mutex<Vec<(PathBuf, store::Seen, Summary)>> = Mutex::new(Vec::new());
    let path = peerstore::file_for(dir, public_key);
    let mut kept = KEPT.locked();
    let i = match kept.iter().position(|(p, _, _)| *p == path) {
        Some(i) => i,
        None => {
            kept.push((path.clone(), store::Seen::default(), Summary::default()));
            kept.len() - 1
        }
    };
    let (_, seen, summary) = &mut kept[i];
    let add = |summary: &mut Summary, text: &str| {
        for e in text.lines().filter_map(parse_line) {
            summary.last_at = e.at;
            if e.direction.theirs() {
                summary.received += 1;
                summary.last_received = summary.last_received.max(e.at);
            }
            summary.count += 1;
        }
    };
    match seen.read_on(&path)? {
        Some(store::More::After(text)) => add(summary, &text),
        Some(store::More::Whole(text)) => {
            *summary = Summary::default();
            add(summary, &text);
        }
        None => {}
    }
    Ok(*summary)
}

/// Skips a line that doesn't parse (e.g. hand-edited or corrupted)
/// rather than failing the whole read — one bad line shouldn't hide the
/// rest of a conversation.
pub(crate) fn parse_line(line: &str) -> Option<Entry> {
    let mut parts = line.splitn(3, '\t');
    let at: u64 = parts.next()?.parse().ok()?;
    let direction = Direction::parse(parts.next()?)?;
    let rest = parts.next()?;
    let fields: Vec<&str> = rest.splitn(3, '\t').collect();
    let (expires, id, text) = match fields[..] {
        [expires, text] => (expires.parse().ok()?, None, text),
        [expires, id, text] => (expires.parse().ok()?, Some(crate::outbox::parse_id(id)?), text),
        _ => return None,
    };
    Some(Entry { at, direction, text: crate::text::from_field(text), expires, id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_newline_in_a_message_cannot_forge_a_second_entry() {
        let dir = std::env::temp_dir().join(format!("swarm-history-test-forge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        append(&dir, &[7; 32], Direction::Received, "hi\n1700000000\t>\tI never said this", 1_700_000_001).unwrap();
        let entries = all(&dir, &[7; 32]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].direction, Direction::Received);
        assert_eq!(entries[0].text, "hi\n1700000000 > I never said this", "the line break kept, as the message's own");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_message_over_several_lines_is_kept_as_written() {
        let dir = std::env::temp_dir().join(format!("swarm-history-test-lines-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        append(&dir, &[7; 32], Direction::Sent, "first\nsecond\n\nfourth", 1_700_000_001).unwrap();
        append(&dir, &[7; 32], Direction::Received, "next", 1_700_000_002).unwrap();
        let entries = all(&dir, &[7; 32]).unwrap();
        assert_eq!(entries.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["first\nsecond\n\nfourth", "next"]);
        assert!(!append_new(&dir, &[7; 32], Direction::Sent, "first\r\nsecond\n\nfourth", 1_700_000_001, 0, None).unwrap(), "the same message, come another way");
        fs::remove_dir_all(&dir).unwrap();
    }
    use std::path::PathBuf;

    const BOB: [u8; 32] = [1; 32];
    const ALICE: [u8; 32] = [2; 32];

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-history-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn append_then_all_round_trips_in_order() {
        let dir = scratch_dir("order");
        append(&dir, &BOB, Direction::Sent, "hey", 1_700_000_000).unwrap();
        append(&dir, &BOB, Direction::Received, "hi back", 1_700_000_005).unwrap();

        let entries = all(&dir, &BOB).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].direction, Direction::Sent);
        assert_eq!(entries[0].text, "hey");
        assert_eq!(entries[0].at, 1_700_000_000);
        assert_eq!(entries[1].direction, Direction::Received);
        assert_eq!(entries[1].text, "hi back");
        assert_eq!(entries[1].at, 1_700_000_005);

        assert_eq!(all(&dir, &ALICE).unwrap(), Vec::new());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_message_from_a_clock_far_ahead_is_kept_as_arriving_now() {
        let dir = scratch_dir("future");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        append(&dir, &[1; 32], Direction::Received, "from 2066", now + 40 * 365 * 86400).unwrap();
        append(&dir, &[1; 32], Direction::Received, "a minute fast", now + 60).unwrap();
        let all = all(&dir, &[1; 32]).unwrap();
        assert!(all[0].at <= now + 5, "taken as now: {}", all[0].at);
        assert_eq!(all[1].at, now + 60, "a little fast is left alone");
    }

    /// Kept in memory and read on from where it got to: what's appended
    /// shows, a line still being written waits, and a file rewritten
    /// (merged from another of your devices) is read again whole.
    #[test]
    fn what_is_kept_follows_the_file_whatever_happens_to_it() {
        let dir = scratch_dir("kept");
        let key = [9u8; 32];
        let path = peerstore::file_for(&dir, &key);
        let texts = |dir: &Path| all(dir, &key).unwrap().into_iter().map(|e| e.text).collect::<Vec<_>>();
        assert!(texts(&dir).is_empty());
        append(&dir, &key, Direction::Received, "one", 100).unwrap();
        append(&dir, &key, Direction::Sent, "two", 101).unwrap();
        assert_eq!(texts(&dir), ["one", "two"]);
        assert_eq!(summary(&dir, &key).unwrap(), Summary { last_at: 101, last_received: 100, received: 1, count: 2 });
        use std::io::Write;
        let sealed = store::sealed_line(&path, "102\t<\t0\tthree");
        let (first, rest) = sealed.split_at(sealed.len() / 2);
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(first.as_bytes()).unwrap();
        assert_eq!(texts(&dir), ["one", "two"]);
        assert_eq!(summary(&dir, &key).unwrap().received, 1);
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(format!("{rest}\n").as_bytes()).unwrap();
        assert_eq!(texts(&dir), ["one", "two", "three"]);
        assert_eq!(summary(&dir, &key).unwrap(), Summary { last_at: 102, last_received: 102, received: 2, count: 3 });
        store::write_atomic(&path, "100\t<\t0\tone\n100\t<\t0\tmerged in\n101\t>\t0\ttwo\n102\t<\t0\tthree\n").unwrap();
        assert_eq!(texts(&dir), ["one", "merged in", "two", "three"]);
        assert_eq!(summary(&dir, &key).unwrap(), Summary { last_at: 102, last_received: 102, received: 3, count: 4 });
        fs::remove_file(&path).unwrap();
        assert!(texts(&dir).is_empty());
        assert_eq!(summary(&dir, &key).unwrap(), Summary::default());
    }

    #[test]
    fn messages_go_when_their_time_is_up_and_others_stay() {
        let dir = scratch_dir("sweep");
        append(&dir, &BOB, Direction::Sent, "for good", 100).unwrap();
        append_expiring(&dir, &BOB, Direction::Received, "for an hour", 100, 3700).unwrap();
        append_expiring(&dir, &BOB, Direction::Sent, "for a day", 200, 86_600).unwrap();
        append_expiring(&dir, &ALICE, Direction::Sent, "alice's, for an hour", 100, 3700).unwrap();
        let texts = |key: &[u8; 32]| all(&dir, key).unwrap().into_iter().map(|e| e.text).collect::<Vec<_>>();
        assert_eq!(all(&dir, &BOB).unwrap()[1].expires, 3700);
        assert_eq!(sweep(&dir, 3699).unwrap(), 0);
        assert_eq!(sweep(&dir, 3700).unwrap(), 2);
        assert_eq!(texts(&BOB), ["for good", "for a day"]);
        assert!(texts(&ALICE).is_empty());
        assert_eq!(summary(&dir, &BOB).unwrap().count, 2);
        assert_eq!(sweep(&dir, 86_600).unwrap(), 1);
        assert_eq!(texts(&BOB), ["for good"]);
        append_expiring(&dir, &BOB, Direction::Sent, "12\tnot a tab", 300, 0).unwrap();
        assert_eq!(texts(&BOB).last().unwrap(), "12 not a tab");
    }

    #[test]
    fn all_on_missing_file_is_empty_not_an_error() {
        let dir = scratch_dir("missing");
        assert_eq!(all(&dir, &ALICE).unwrap(), Vec::new());
    }

    #[test]
    fn a_malformed_line_is_skipped_not_fatal() {
        let dir = scratch_dir("malformed");
        fs::create_dir_all(&dir).unwrap();
        store::write_atomic(&peerstore::file_for(&dir, &BOB), "not a valid line at all\n100\t>\t0\tgood one\n").unwrap();
        let entries = all(&dir, &BOB).unwrap();
        assert_eq!(entries, vec![Entry { at: 100, direction: Direction::Sent, text: "good one".into(), expires: 0, id: None }]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_line_is_found_again_by_its_id_or_else_its_time_and_text() {
        let dir = scratch_dir("remove");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        store::write_atomic(&peerstore::file_for(&dir, &BOB), "90\t<\t0\tfirst\n").unwrap();
        append(&dir, &BOB, Direction::Sent, "no id", 100).unwrap();
        append_message(&dir, &BOB, Direction::Sent, "same words", 200, 0, Some([1; 16])).unwrap();
        append_message(&dir, &BOB, Direction::Sent, "same words", 200, 0, Some([2; 16])).unwrap();
        append_message(&dir, &BOB, Direction::Received, "theirs", 300, 900, Some([3; 16])).unwrap();
        let entries = all(&dir, &BOB).unwrap();
        assert_eq!(entries.iter().map(|e| e.id).collect::<Vec<_>>(), [None, None, Some([1; 16]), Some([2; 16]), Some([3; 16])]);
        assert_eq!(entries[4].expires, 900, "an id doesn't hide the expiry");
        assert_eq!(entries[4].text, "theirs");

        assert!(remove(&dir, &BOB, &entries[3].line_ref()).unwrap());
        assert_eq!(all(&dir, &BOB).unwrap().iter().filter(|e| e.text == "same words").count(), 1);
        assert!(remove(&dir, &BOB, &entries[1].line_ref()).unwrap());
        assert!(remove(&dir, &BOB, &entries[0].line_ref()).unwrap());
        assert!(!remove(&dir, &BOB, &entries[0].line_ref()).unwrap(), "only once");

        let theirs = LineRef { id: Some([9; 16]), ..all(&dir, &BOB).unwrap()[1].line_ref() };
        let without = Entry { id: None, ..all(&dir, &BOB).unwrap()[1].clone() };
        assert!(theirs.matches(&without));
        assert!(!LineRef { direction: Direction::Received, ..all(&dir, &BOB).unwrap()[0].line_ref() }.matches(&all(&dir, &BOB).unwrap()[0]));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_line_ref_reads_back_as_written() {
        for line in [
            LineRef { direction: Direction::Sent, id: Some([0xab; 16]), at: 1_700_000_000, digest: digest("hello") },
            LineRef { direction: Direction::Missed, id: None, at: 5, digest: digest("call") },
        ] {
            assert_eq!(LineRef::decode(&line.encode()), Some(line));
        }
        assert_eq!(LineRef::decode("x:-:5:00"), None);
    }

    #[test]
    fn late_messages_go_in_by_time_and_live_ones_at_the_end() {
        let dir = scratch_dir("late");
        let now = unix_now();
        let night = now - 10 * 3600;
        append(&dir, &BOB, Direction::Received, "evening", night).unwrap();
        append(&dir, &BOB, Direction::Received, "how are you", night + 120).unwrap();
        append(&dir, &BOB, Direction::Received, "night", night + 600).unwrap();
        append_new(&dir, &BOB, Direction::Sent, "hello", night + 60, 0, None).unwrap();
        append_new(&dir, &BOB, Direction::Sent, "well, you?", night + 300, 0, None).unwrap();
        assert!(!append_new(&dir, &BOB, Direction::Sent, "hello", night + 60, 0, None).unwrap());
        append_new(&dir, &BOB, Direction::Sent, "before any of it", night - 60, 0, None).unwrap();
        append(&dir, &BOB, Direction::Received, "morning", now).unwrap();
        append(&dir, &BOB, Direction::Sent, "morning to you", now - 3).unwrap();
        let texts: Vec<String> = all(&dir, &BOB).unwrap().into_iter().map(|e| e.text).collect();
        assert_eq!(texts, ["before any of it", "evening", "hello", "how are you", "well, you?", "night", "morning", "morning to you"]);
        assert_eq!(summary(&dir, &BOB).unwrap().count, 8);
        fs::remove_dir_all(&dir).unwrap();
    }

}
