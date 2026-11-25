//! Which messages from each peer this device has already received, by
//! `outbox::MessageId` — so a message that arrives twice (resent after
//! its Ack was lost, or after a reconnect) is acknowledged again but
//! shown and stored only once. One file per peer (see `peerstore`), one
//! id per line, keeping the most recent `MAX_REMEMBERED`.

use crate::outbox::{format_id, MessageId};
use crate::{peerstore, store};
use std::path::Path;

/// How many ids are kept per peer, at the least: the file's added to,
/// and cut back to this once it's twice as long. A resend only ever
/// repeats something recent, so this is plenty.
pub const MAX_REMEMBERED: usize = 4096;

/// Each peer's ids, read on from where they got to (see `store::Kept`):
/// a message's arrival only adds a line, so only that's read again.
static KEPT: store::Kept<String> = store::Kept::new(8);

/// Records `id` as received from `public_key`. True if it's new — the
/// caller should store and show it; false if it's already been seen.
pub fn first_time(dir: &Path, public_key: &[u8; 32], id: &MessageId) -> std::io::Result<bool> {
    let _guard = store::lock();
    let path = peerstore::file_for(dir, public_key);
    let hex = format_id(id);
    let (there, count) = KEPT.with(&path, |l| Some(l.to_string()), |all| (all.contains(&hex), all.len()))?;
    if there {
        return Ok(false);
    }
    if count < 2 * MAX_REMEMBERED {
        store::append_line(&path, &hex)?;
    } else {
        let mut lines = KEPT.read(&path, |l| Some(l.to_string()))?;
        lines.push(hex);
        let start = lines.len().saturating_sub(MAX_REMEMBERED);
        store::write_atomic(&path, lines[start..].iter().map(|l| format!("{l}\n")).collect::<String>())?;
    }
    Ok(true)
}

/// Forgets `id` as received from `public_key`: it wasn't kept after all,
/// so when it comes again it's taken.
pub fn forget(dir: &Path, public_key: &[u8; 32], id: &MessageId) -> std::io::Result<()> {
    let _guard = store::lock();
    let path = peerstore::file_for(dir, public_key);
    let hex = format_id(id);
    let lines = KEPT.read(&path, |l| Some(l.to_string()))?;
    if lines.contains(&hex) {
        store::write_atomic(&path, lines.iter().filter(|l| **l != hex).map(|l| format!("{l}\n")).collect::<String>())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_message_counts_once_however_often_it_arrives() {
        let dir = std::env::temp_dir().join(format!("swarm-seen-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(first_time(&dir, &[1; 32], &[5; 16]).unwrap());
        assert!(!first_time(&dir, &[1; 32], &[5; 16]).unwrap());
        assert!(first_time(&dir, &[2; 32], &[5; 16]).unwrap(), "per peer");
        assert!(first_time(&dir, &[1; 32], &[6; 16]).unwrap());
        forget(&dir, &[1; 32], &[5; 16]).unwrap();
        assert!(first_time(&dir, &[1; 32], &[5; 16]).unwrap(), "forgotten: taken again");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The file's cut back to the newest once it's twice as long as what's
    /// kept, and a recent one's still known.
    #[test]
    fn only_so_many_are_kept() {
        let dir = std::env::temp_dir().join(format!("swarm-seen-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let id = |n: u32| { let mut id = [0u8; 16]; id[..4].copy_from_slice(&n.to_be_bytes()); id };
        for n in 0..(2 * MAX_REMEMBERED as u32 + 1) {
            assert!(first_time(&dir, &[1; 32], &id(n)).unwrap());
        }
        let kept = crate::store::read(peerstore::file_for(&dir, &[1; 32])).unwrap().lines().count();
        assert_eq!(kept, MAX_REMEMBERED);
        assert!(!first_time(&dir, &[1; 32], &id(2 * MAX_REMEMBERED as u32)).unwrap());
        assert!(first_time(&dir, &[1; 32], &id(0)).unwrap(), "long gone");
        fs::remove_dir_all(&dir).unwrap();
    }
}
