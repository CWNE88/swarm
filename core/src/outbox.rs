//! Per-peer store-and-forward outbox: a message that couldn't be
//! delivered because the recipient was unreachable gets queued here
//! instead of just failing, and the node's per-peer task
//! (`node::peer`) delivers it automatically the moment that peer is
//! next seen online.
//!
//! One flat file per peer, named by the peer's public key (see
//! `peerstore` for why never by name), one message per line:
//! `<sent_at>\t<id>\t<lifetime>\t<text>`, the text kept to one line (see
//! `text`), `lifetime` how long it lasts once sent (see `disappearing`;
//! 0, for good) — one whose time is up before it's delivered isn't.
//! Every message has a random `MessageId`, carried on the wire too, so
//! the receiver can drop a copy it already has (a resend after a lost
//! Ack) and a delivered message is removed by id, never by position.
//!
//! This is a deliberately minimal, sender-side-only mechanism: it
//! requires *this* device to eventually run `listen` again while the
//! recipient is reachable — it isn't the durable, relay-anyone-can-hold
//! store-and-forward the design doc's log primitive will eventually
//! give every kind of message, just enough to smooth over "they were
//! offline when I hit send."

use std::fs;
use std::path::Path;

use crate::{peerstore, store, text};

/// A message's identity: random, chosen when it's first sent, and the
/// same on every resend — what the receiving side de-duplicates on.
pub type MessageId = [u8; 16];

pub fn new_message_id() -> MessageId {
    rand::random()
}

pub fn format_id(id: &MessageId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_id(s: &str) -> Option<MessageId> {
    if s.len() != 32 {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(id)
}

/// A message queued in an outbox, together with when it was originally
/// sent (Unix seconds, UTC) — kept around so the recipient can be told
/// it's a delayed delivery rather than something that just happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedMessage {
    pub id: MessageId,
    pub sent_at: u64,
    /// How long it lasts, in seconds; 0, for good.
    pub lifetime: u64,
    pub text: String,
}

/// Appends `message` (as `id`) to `public_key`'s outbox, creating the
/// directory and file as needed. Control characters in the text become
/// spaces (see `text`), so it can't spill onto a second line.
///
/// `sent_at` is caller-supplied rather than computed here so the exact
/// same value can also be used for the wire send and the sender's own
/// `history` entry — one canonical "when this was sent" per message,
/// not three independent clock reads a few milliseconds apart.
pub fn enqueue(dir: &Path, public_key: &[u8; 32], id: MessageId, message: &str, sent_at: u64, lifetime: u64) -> std::io::Result<()> {
    let _guard = store::lock();
    store::append_line(&peerstore::file_for(dir, public_key), &format_line(id, sent_at, lifetime, message))
}

fn format_line(id: MessageId, sent_at: u64, lifetime: u64, message: &str) -> String {
    format!("{sent_at}\t{}\t{lifetime}\t{}", format_id(&id), text::single_line(message))
}

/// Drops every queued message in `dir` whose time is up by `now`, not
/// yet delivered: it's not to be. How many went.
pub fn sweep(dir: &Path, now: u64) -> std::io::Result<usize> {
    let mut gone = 0;
    for (_, path) in store::names_in(dir) {
        let _guard = store::lock();
        let text = store::read(&path)?;
        let up = |m: &QueuedMessage| m.lifetime > 0 && m.sent_at.saturating_add(m.lifetime) <= now;
        let kept: Vec<&str> = text.lines().filter(|l| !parse_line(l).is_some_and(|m| up(&m))).collect();
        let dropped = text.lines().count() - kept.len();
        if dropped > 0 {
            store::write_atomic(&path, kept.iter().map(|l| format!("{l}\n")).collect::<String>())?;
            gone += dropped;
        }
    }
    Ok(gone)
}

/// Every message currently queued for `public_key`, oldest first.
/// Doesn't remove anything — see `remove_first`, called once a message
/// is actually confirmed sent.
pub fn pending(dir: &Path, public_key: &[u8; 32]) -> std::io::Result<Vec<QueuedMessage>> {
    let path = peerstore::file_for(dir, public_key);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = crate::store::read(path)?;
    Ok(text.lines().filter_map(parse_line).collect())
}

/// Splits one stored line back into its parts:
/// `<sent_at>\t<id>\t<lifetime>\t<text>` (the text holds no tab, see
/// `text`, so a fourth one is its own). `None` for a damaged line, which
/// is passed over.
fn parse_line(line: &str) -> Option<QueuedMessage> {
    let mut fields = line.splitn(4, '\t');
    let sent_at = fields.next()?.parse().ok()?;
    let id = parse_id(fields.next()?)?;
    let lifetime = fields.next()?.parse().ok()?;
    let text = fields.next()?.to_string();
    Some(QueuedMessage { id, sent_at, lifetime, text })
}

/// Formats a Unix timestamp (seconds) as `YYYY-MM-DD HH:MM UTC`.
/// Hand-rolled instead of pulling in a date/time crate for one
/// conversion — the calendar math is Howard Hinnant's well-known
/// `civil_from_days` algorithm.
pub fn format_timestamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let h = secs_of_day / 3600;
    let min = (secs_of_day % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02} UTC")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Removes the message `id` from `public_key`'s outbox, once it's been
/// delivered — by id, not position, so a message queued meanwhile is
/// never the one removed. A no-op if it isn't there.
pub fn remove(dir: &Path, public_key: &[u8; 32], id: &MessageId) -> std::io::Result<()> {
    let _guard = store::lock();
    let path = peerstore::file_for(dir, public_key);
    let all = pending(dir, public_key)?;
    let rest: Vec<&QueuedMessage> = all.iter().filter(|m| m.id != *id).collect();
    if rest.len() == all.len() {
        return Ok(());
    }
    if rest.is_empty() {
        fs::remove_file(&path)
    } else {
        let contents: String = rest.iter().map(|m| format_line(m.id, m.sent_at, m.lifetime, &m.text) + "\n").collect();
        store::write_atomic(&path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOB: [u8; 32] = [1; 32];
    const ALICE: [u8; 32] = [2; 32];

    fn texts(messages: &[QueuedMessage]) -> Vec<&str> {
        messages.iter().map(|m| m.text.as_str()).collect()
    }

    #[test]
    fn enqueue_then_pending_returns_messages_in_order() {
        let dir = std::env::temp_dir().join(format!("swarm-outbox-test-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        enqueue(&dir, &BOB, [1; 16], "first", 1_700_000_000, 0).unwrap();
        enqueue(&dir, &BOB, [2; 16], "second\nwith a newline", 1_700_000_005, 0).unwrap();
        let queued = pending(&dir, &BOB).unwrap();
        assert_eq!(texts(&queued), vec!["first", "second with a newline"]);
        assert_eq!(queued.iter().map(|m| (m.id, m.sent_at)).collect::<Vec<_>>(), vec![([1; 16], 1_700_000_000), ([2; 16], 1_700_000_005)]);
        assert_eq!(pending(&dir, &ALICE).unwrap(), Vec::new());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_takes_out_only_that_message_and_cleans_up_when_empty() {
        let dir = std::env::temp_dir().join(format!("swarm-outbox-test-remove-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        enqueue(&dir, &BOB, [1; 16], "first", 1_700_000_000, 0).unwrap();
        enqueue(&dir, &BOB, [2; 16], "second", 1_700_000_005, 0).unwrap();

        remove(&dir, &BOB, &[2; 16]).unwrap();
        assert_eq!(texts(&pending(&dir, &BOB).unwrap()), vec!["first"]);
        remove(&dir, &BOB, &[9; 16]).unwrap();
        assert_eq!(texts(&pending(&dir, &BOB).unwrap()), vec!["first"]);

        remove(&dir, &BOB, &[1; 16]).unwrap();
        assert_eq!(pending(&dir, &BOB).unwrap(), Vec::new());
        assert!(!peerstore::file_for(&dir, &BOB).exists(), "file should be removed once empty, not left as an empty file");

        remove(&dir, &BOB, &[1; 16]).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_message_whose_time_is_up_before_its_delivered_isnt() {
        let dir = std::env::temp_dir().join(format!("swarm-outbox-test-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        enqueue(&dir, &BOB, [1; 16], "for good", 100, 0).unwrap();
        enqueue(&dir, &BOB, [2; 16], "for an hour", 100, 3600).unwrap();
        enqueue(&dir, &BOB, [3; 16], "7\tthen a tab", 100, 60).unwrap();
        assert_eq!(pending(&dir, &BOB).unwrap()[1].lifetime, 3600);
        assert_eq!(texts(&pending(&dir, &BOB).unwrap()), vec!["for good", "for an hour", "7 then a tab"]);
        assert_eq!(sweep(&dir, 160).unwrap(), 1);
        assert_eq!(sweep(&dir, 3700).unwrap(), 1);
        assert_eq!(texts(&pending(&dir, &BOB).unwrap()), vec!["for good"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_line_is_passed_over() {
        let dir = std::env::temp_dir().join(format!("swarm-outbox-test-damaged-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        enqueue(&dir, &BOB, [1; 16], "kept", 100, 0).unwrap();
        store::append_line(&peerstore::file_for(&dir, &BOB), "not a queued message").unwrap();
        assert_eq!(texts(&pending(&dir, &BOB).unwrap()), vec!["kept"]);
        assert_eq!(sweep(&dir, 1_000_000).unwrap(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn format_timestamp_renders_known_epoch_values() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14 22:13 UTC");
    }
}
