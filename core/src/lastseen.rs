//! When each peer was last connected (`~/.swarm/lastseen`, one
//! `<key hex> <unix secs>` line per peer) — so "offline since" still
//! means something after a restart. Written when a connection ends, and
//! once a minute while one is up, so a crash or a quit loses at most a
//! minute of it.

use std::path::Path;

/// How often a live connection's time is saved.
pub const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// When `peer` (their root key) was last connected, if ever.
pub fn get(path: &Path, peer: &[u8; 32]) -> Option<u64> {
    let wanted = crate::peerstore::file_name(peer);
    crate::store::read(path).ok()?.lines().find_map(|line| {
        let (key, at) = line.split_once(' ')?;
        (key == wanted).then(|| at.trim().parse().ok()).flatten()
    })
}

/// Records `peer` as connected at `at`.
pub fn set(path: &Path, peer: &[u8; 32], at: u64) -> std::io::Result<()> {
    let _guard = crate::store::lock();
    let key = crate::peerstore::file_name(peer);
    let mut lines: Vec<String> =
        crate::store::read(path).unwrap_or_default().lines().filter(|line| !line.starts_with(&format!("{key} "))).map(str::to_string).collect();
    lines.push(format!("{key} {at}"));
    crate::store::write_atomic(path, lines.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_latest_per_peer() {
        let path = std::env::temp_dir().join(format!("swarm-lastseen-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(get(&path, &[1; 32]), None);
        set(&path, &[1; 32], 100).unwrap();
        set(&path, &[2; 32], 200).unwrap();
        set(&path, &[1; 32], 300).unwrap();
        assert_eq!(get(&path, &[1; 32]), Some(300));
        assert_eq!(get(&path, &[2; 32]), Some(200));
        let _ = std::fs::remove_file(&path);
    }
}
