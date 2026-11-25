//! Messages the user has deleted that the contact they went to hasn't yet
//! heard are gone (`Request::Unsay`). Kept the same on all of the user's
//! devices (`Notice::Unsaid`, with the copies of messages), and whichever
//! reaches the contact first tells them.
//!
//! One delivered stays in the chat, faint and marked `◷`, until one of the
//! contact's devices acknowledges the deletion, and then goes; that waits
//! as long as it takes. One that hadn't reached them goes from the chat at
//! once (`hidden`): it's only told in case a copy got there after all (sent
//! by another of the user's devices as it was deleted), and is let go after
//! `HIDDEN_KEPT` if they never turn up.
//!
//! One file per contact under `unsaying`, named by their key (see
//! `peerstore`), one line per message: its `history::LineRef`, encoded,
//! then when it was deleted and `h` if it's hidden (a line with neither,
//! from the first build of this, is a delivered one).

use crate::history::LineRef;
use crate::{peerstore, store};
use std::path::Path;

/// How long a deletion of a message that hadn't been delivered is kept
/// to tell them, in seconds.
pub const HIDDEN_KEPT: u64 = 30 * 24 * 60 * 60;

/// One deletion still to tell them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsaying {
    pub line: LineRef,
    pub deleted_at: u64,
    /// It hadn't been delivered, and is no longer in the chat.
    pub hidden: bool,
}

fn format_line(u: &Unsaying) -> String {
    format!("{}\t{}{}", u.line.encode(), u.deleted_at, if u.hidden { "\th" } else { "" })
}

fn parse_line(line: &str) -> Option<Unsaying> {
    let mut f = line.split('\t');
    let line = LineRef::decode(f.next()?)?;
    let deleted_at = f.next()?.parse().ok()?;
    Some(Unsaying { line, deleted_at, hidden: f.next() == Some("h") })
}

fn load(dir: &Path, key: &[u8; 32]) -> std::io::Result<Vec<Unsaying>> {
    let path = peerstore::file_for(dir, key);
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(store::read(&path)?.lines().filter_map(parse_line).collect())
}

fn save(dir: &Path, key: &[u8; 32], all: &[Unsaying]) -> std::io::Result<()> {
    let path = peerstore::file_for(dir, key);
    if all.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    store::write_atomic(&path, all.iter().map(|u| format_line(u) + "\n").collect::<String>())
}

/// Notes `line`, to `key`, as deleted at `now` (`hidden`: it hadn't been
/// delivered), unless it's there already. Whether it's new.
pub fn add(dir: &Path, key: &[u8; 32], line: &LineRef, now: u64, hidden: bool) -> std::io::Result<bool> {
    let _guard = store::lock();
    if load(dir, key)?.iter().any(|u| u.line == *line) {
        return Ok(false);
    }
    store::append_line(&peerstore::file_for(dir, key), &format_line(&Unsaying { line: *line, deleted_at: now, hidden }))?;
    Ok(true)
}

/// Takes `line` off `key`'s list: they've heard. Whether it was there.
pub fn remove(dir: &Path, key: &[u8; 32], line: &LineRef) -> std::io::Result<bool> {
    let _guard = store::lock();
    let all = load(dir, key)?;
    let rest: Vec<Unsaying> = all.iter().filter(|u| u.line != *line).copied().collect();
    if rest.len() == all.len() {
        return Ok(false);
    }
    save(dir, key, &rest)?;
    Ok(true)
}

/// Everything `key` still has to hear has gone, oldest first; none if
/// there's no file.
pub fn all(dir: &Path, key: &[u8; 32]) -> std::io::Result<Vec<LineRef>> {
    Ok(load(dir, key)?.into_iter().map(|u| u.line).collect())
}

/// Those still in the chat, faint till they've heard: not `hidden`.
pub fn shown(dir: &Path, key: &[u8; 32]) -> std::io::Result<Vec<LineRef>> {
    Ok(load(dir, key)?.into_iter().filter(|u| !u.hidden).map(|u| u.line).collect())
}

/// Lets go of every hidden deletion (in every conversation) older than
/// `HIDDEN_KEPT` by `now`. How many went.
pub fn sweep(dir: &Path, now: u64) -> std::io::Result<usize> {
    let mut gone = 0;
    for (_, path) in store::names_in(dir) {
        let _guard = store::lock();
        let text = store::read(&path)?;
        let all: Vec<Unsaying> = text.lines().filter_map(parse_line).collect();
        let kept: Vec<Unsaying> = all.iter().filter(|u| !u.hidden || u.deleted_at.saturating_add(HIDDEN_KEPT) > now).copied().collect();
        if kept.len() < all.len() {
            gone += all.len() - kept.len();
            if kept.is_empty() {
                std::fs::remove_file(&path)?;
            } else {
                store::write_atomic(&path, kept.iter().map(|u| format_line(u) + "\n").collect::<String>())?;
            }
        }
    }
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{digest, Direction};

    #[test]
    fn a_deletion_waits_once_and_goes_when_heard() {
        let dir = std::env::temp_dir().join(format!("swarm-unsaying-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let one = LineRef { direction: Direction::Sent, id: Some([1; 16]), at: 10, digest: digest("one") };
        let two = LineRef { direction: Direction::Sent, id: None, at: 20, digest: digest("two") };
        assert!(add(&dir, &[5; 32], &one, 100, false).unwrap());
        assert!(!add(&dir, &[5; 32], &one, 100, false).unwrap(), "once");
        assert!(add(&dir, &[5; 32], &two, 100, true).unwrap());
        assert_eq!(all(&dir, &[5; 32]).unwrap(), [one, two]);
        assert_eq!(shown(&dir, &[5; 32]).unwrap(), [one], "a hidden one isn't in the chat");
        assert!(all(&dir, &[6; 32]).unwrap().is_empty(), "each contact's own");
        assert!(remove(&dir, &[5; 32], &one).unwrap());
        assert!(!remove(&dir, &[5; 32], &one).unwrap());
        assert!(remove(&dir, &[5; 32], &two).unwrap());
        assert!(!peerstore::file_for(&dir, &[5; 32]).exists(), "nothing left, no file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hidden deletion is let go after 30 days; one still shown waits
    /// for them however long it takes.
    #[test]
    fn a_hidden_deletion_is_let_go_after_thirty_days() {
        let dir = std::env::temp_dir().join(format!("swarm-unsaying-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shown_one = LineRef { direction: Direction::Sent, id: Some([1; 16]), at: 10, digest: digest("one") };
        let hidden_one = LineRef { direction: Direction::Sent, id: Some([2; 16]), at: 20, digest: digest("two") };
        add(&dir, &[5; 32], &shown_one, 100, false).unwrap();
        add(&dir, &[5; 32], &hidden_one, 100, true).unwrap();
        assert_eq!(sweep(&dir, 100 + HIDDEN_KEPT - 1).unwrap(), 0);
        assert_eq!(sweep(&dir, 100 + HIDDEN_KEPT).unwrap(), 1);
        assert_eq!(all(&dir, &[5; 32]).unwrap(), [shown_one]);
        assert_eq!(sweep(&dir, 100 + 10 * HIDDEN_KEPT).unwrap(), 0, "shown: kept till they've heard");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
