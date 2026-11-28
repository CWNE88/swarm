//! Messages sent to a contact, from any of the user's devices, that none
//! of the contact's devices has acknowledged yet: what the chat marks as
//! waiting (`◷`) rather than delivered (`✓`).
//!
//! The outbox holds a message only on the device that's to deliver it,
//! and only until it's handed on (to another of the user's devices that
//! relays, say). This holds it on every device of the user's that knows
//! of it until one of the contact's devices has it: put here as it's
//! sent (or as a copy of it arrives from another of the user's devices,
//! `node::devices`), and taken off as it's acknowledged here or another
//! device says it was (`Notice::Delivered`).
//!
//! One file per contact under `waiting`, named by their key (see
//! `peerstore`), one message per line: `<sent_at>\t<id>\t<expires>\t<text>`,
//! the text kept to one line (see `text`); `expires` when it deletes
//! itself (unix seconds; 0, never: see `disappearing`). A message is
//! matched to its line in the history by its time and text, which is
//! all the history keeps of it.

use std::path::Path;

use crate::outbox::{format_id, parse_id, MessageId};
use crate::{peerstore, store};

/// One message waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    pub id: MessageId,
    pub at: u64,
    pub expires: u64,
    pub text: String,
}

fn format_line(w: &Waiting) -> String {
    format!("{}\t{}\t{}\t{}", w.at, format_id(&w.id), w.expires, crate::text::to_field(&w.text))
}

fn parse_line(line: &str) -> Option<Waiting> {
    let mut f = line.splitn(4, '\t');
    let at = f.next()?.parse().ok()?;
    let id = parse_id(f.next()?)?;
    let expires = f.next()?.parse().ok()?;
    Some(Waiting { id, at, expires, text: crate::text::from_field(f.next()?) })
}

/// Notes message `id`, sent to `key` at `at`, as waiting, unless it's
/// there already.
pub fn add(dir: &Path, key: &[u8; 32], id: MessageId, at: u64, expires: u64, text: &str) -> std::io::Result<()> {
    let _guard = store::lock();
    if all(dir, key)?.iter().any(|w| w.id == id) {
        return Ok(());
    }
    store::append_line(&peerstore::file_for(dir, key), &format_line(&Waiting { id, at, expires, text: text.to_string() }))
}

/// Takes message `id` off `key`'s list: one of their devices has it.
/// Whether it was there.
pub fn remove(dir: &Path, key: &[u8; 32], id: &MessageId) -> std::io::Result<bool> {
    let _guard = store::lock();
    let all = all(dir, key)?;
    let rest: Vec<&Waiting> = all.iter().filter(|w| w.id != *id).collect();
    if rest.len() == all.len() {
        return Ok(false);
    }
    let path = peerstore::file_for(dir, key);
    if rest.is_empty() {
        std::fs::remove_file(&path)?;
    } else {
        store::write_atomic(&path, rest.iter().map(|w| format_line(w) + "\n").collect::<String>())?;
    }
    Ok(true)
}

/// Everything waiting for `key`, oldest first; none if there's no file.
pub fn all(dir: &Path, key: &[u8; 32]) -> std::io::Result<Vec<Waiting>> {
    let path = peerstore::file_for(dir, key);
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(store::read(&path)?.lines().filter_map(parse_line).collect())
}

/// Drops whatever's deleted itself by `now` (its line in the history has
/// gone too). How many went.
pub fn sweep(dir: &Path, now: u64) -> std::io::Result<usize> {
    let mut gone = 0;
    for (_, path) in store::names_in(dir) {
        let _guard = store::lock();
        let text = store::read(&path)?;
        let kept: Vec<&str> = text.lines().filter(|l| !parse_line(l).is_some_and(|w| w.expires != 0 && w.expires <= now)).collect();
        let dropped = text.lines().count() - kept.len();
        if dropped > 0 {
            if kept.is_empty() {
                std::fs::remove_file(&path)?;
            } else {
                store::write_atomic(&path, kept.iter().map(|l| format!("{l}\n")).collect::<String>())?;
            }
            gone += dropped;
        }
    }
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-waiting-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_message_waits_until_its_taken_off_and_only_once() {
        let dir = scratch("once");
        let bob = [1; 32];
        add(&dir, &bob, [1; 16], 100, 0, "first").unwrap();
        add(&dir, &bob, [2; 16], 101, 0, "second\twith a tab").unwrap();
        add(&dir, &bob, [1; 16], 100, 0, "first").unwrap();
        let texts = || all(&dir, &bob).unwrap().into_iter().map(|w| w.text).collect::<Vec<_>>();
        assert_eq!(texts(), ["first", "second with a tab"]);
        assert!(all(&dir, &[2; 32]).unwrap().is_empty());
        assert!(remove(&dir, &bob, &[1; 16]).unwrap());
        assert!(!remove(&dir, &bob, &[1; 16]).unwrap());
        assert_eq!(texts(), ["second with a tab"]);
        assert!(remove(&dir, &bob, &[2; 16]).unwrap());
        assert!(!peerstore::file_for(&dir, &bob).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_that_deletes_itself_stops_waiting_when_it_goes() {
        let dir = scratch("sweep");
        add(&dir, &[1; 32], [1; 16], 100, 0, "for good").unwrap();
        add(&dir, &[1; 32], [2; 16], 100, 3700, "for an hour").unwrap();
        assert_eq!(sweep(&dir, 3699).unwrap(), 0);
        assert_eq!(sweep(&dir, 3700).unwrap(), 1);
        assert_eq!(all(&dir, &[1; 32]).unwrap().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
