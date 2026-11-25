//! The address the user typed for another of their own devices (Set
//! address, in its details), as a contact's typed address is kept in the
//! peers file: for when it can't have told this one where it is (both
//! moved to a new network, say), or for a host name (a dynamic-DNS name
//! for the router in front of it) that's looked up afresh at each dial,
//! so it's still found when the IP behind the name changes.
//!
//! Kept on this device only, apart from `addressbook` (which holds
//! literal IPs, as devices tell them): dialled alongside where it's
//! been, every time, till it's cleared. One file per device under
//! `typed-addresses`, named by its key (see `peerstore`), holding its
//! items as typed, comma-separated (see `myaddress::check`).

use crate::{peerstore, store};
use std::path::Path;

/// What was typed for `key`, comma-separated; empty if nothing.
pub fn load(dir: &Path, key: &[u8; 32]) -> String {
    let path = peerstore::file_for(dir, key);
    if !path.exists() {
        return String::new();
    }
    store::read(&path).map(|t| t.trim().to_string()).unwrap_or_default()
}

/// Keeps `items` for `key`; none clears it. Whether anything changed.
pub fn set(dir: &Path, key: &[u8; 32], items: &[String]) -> std::io::Result<bool> {
    let _guard = store::lock();
    let text = items.join(",");
    if load(dir, key) == text {
        return Ok(false);
    }
    let path = peerstore::file_for(dir, key);
    if text.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(true),
        };
    }
    store::write_atomic(&path, text + "\n")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_replaced_and_cleared() {
        let dir = std::env::temp_dir().join(format!("swarm-typedaddress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&dir, &[1; 32]), "");
        assert!(set(&dir, &[1; 32], &["home.example.net:5000".to_string(), "203.0.113.7".to_string()]).unwrap());
        assert!(!set(&dir, &[1; 32], &["home.example.net:5000".to_string(), "203.0.113.7".to_string()]).unwrap(), "the same: no change");
        assert_eq!(load(&dir, &[1; 32]), "home.example.net:5000,203.0.113.7");
        assert_eq!(load(&dir, &[2; 32]), "", "each device's own");
        assert!(set(&dir, &[1; 32], &["office.example.org".to_string()]).unwrap());
        assert_eq!(load(&dir, &[1; 32]), "office.example.org", "replaced, not added to");
        assert!(set(&dir, &[1; 32], &[]).unwrap());
        assert!(!peerstore::file_for(&dir, &[1; 32]).exists(), "cleared: no file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
