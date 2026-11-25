//! Who follows this device's public posts (see `following`): only
//! counted, never listed or answered. A follower says so each time they
//! fetch (`protocol::Notice::Follow`), so someone not heard from in
//! `WINDOW` has most likely stopped (or gone), and drops out of the
//! count; one who unfollows while we're reachable says that too.
//!
//! One flat file (`~/.swarm/followers`), one line per key:
//! `<base64_pubkey>\t<last_seen>`.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::Path;

/// How long a follower counts after they were last heard from.
pub const WINDOW: u64 = 30 * 24 * 60 * 60;
/// The most kept at once — anyone can make keys; past it, the one heard
/// from longest ago goes.
pub const MAX_KEPT: usize = 5000;

fn load(path: &Path) -> Vec<([u8; 32], u64)> {
    crate::store::read(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (key, at) = line.split_once('\t')?;
            Some((STANDARD.decode(key).ok()?.try_into().ok()?, at.trim().parse().ok()?))
        })
        .collect()
}

fn save(path: &Path, all: &[([u8; 32], u64)]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::store::write_atomic(path, all.iter().map(|(k, at)| format!("{}\t{at}\n", STANDARD.encode(k))).collect::<String>())
}

/// `public_key` follows us (`following`), heard at `now`, or has stopped.
pub fn record(path: &Path, public_key: [u8; 32], following: bool, now: u64) -> std::io::Result<()> {
    let _guard = crate::store::lock();
    let mut all: Vec<([u8; 32], u64)> = load(path).into_iter().filter(|(k, _)| *k != public_key).collect();
    if following {
        all.push((public_key, now));
        all.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
        all.truncate(MAX_KEPT);
    }
    save(path, &all)
}

/// How many follow us: heard from within `WINDOW` of `now`, leaving out
/// anyone in `except` (peers, who see our posts anyway).
pub fn count(path: &Path, now: u64, except: &[[u8; 32]]) -> usize {
    load(path).iter().filter(|(k, at)| now.saturating_sub(*at) <= WINDOW && !except.contains(k)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_recent_followers_once() {
        let dir = std::env::temp_dir().join(format!("swarm-followers-test-{}", std::process::id()));
        let path = dir.join("followers");
        let now = 1_800_000_000;
        record(&path, [1; 32], true, now).unwrap();
        record(&path, [1; 32], true, now + 5).unwrap();
        record(&path, [2; 32], true, now - WINDOW - 1).unwrap();
        record(&path, [3; 32], true, now).unwrap();
        assert_eq!(count(&path, now, &[]), 2);
        assert_eq!(count(&path, now, &[[3; 32]]), 1);
        record(&path, [1; 32], false, now).unwrap();
        assert_eq!(count(&path, now, &[]), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
