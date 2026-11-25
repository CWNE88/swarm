//! Deleting a contact: everything kept about them goes from this device's
//! disk.
//!
//! Their conversation, what was waiting to go to them, the ids of their
//! messages, their posts, status, picture and name, where they can be
//! reached, when they were last seen, their devices, what was set for
//! them, following either way, transfers with them,
//! and copies of messages with them still waiting for the user's other
//! devices. Files they sent stay (they're the user's now), and so do
//! their posts in groups (the group's, not theirs).
//!
//! Blocking does the same but keeps their key (`requests`, blocked) and
//! the name they last gave, for the list of blocked keys. The peers list
//! itself, and the book shared with the user's other devices, are the
//! caller's (`peerbook::remove`, `node::ownpeers`).

use crate::node::Paths;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::Path;

/// Records in the peers book that `key` was deleted, with no node
/// running on this folder: the user's other devices are told when one
/// next runs.
pub fn from_book(paths: &Paths, key: &[u8; 32], name: &str) {
    crate::node::forgotten_on_disk(paths, key, name);
}

/// Deletes everything kept about `key` (a user key), and about the
/// devices of theirs met. `block`: their key is kept as blocked, with
/// the name they last gave; else nothing of theirs is left.
pub fn contact(paths: &Paths, key: &[u8; 32], block: bool) {
    let devices = devices_of(paths, key);
    let everyone: Vec<[u8; 32]> = std::iter::once(*key).chain(devices.iter().copied()).collect();
    for k in &everyone {
        for dir in [paths.history(), paths.outbox(), paths.waiting(), paths.unsaying(), paths.seen(), paths.postcache(), paths.profiles(), paths.addresses()] {
            remove(&crate::peerstore::file_for(&dir, k));
        }
    }
    let _ = crate::settings::forget_peer(&paths.settings(), key);
    let _ = crate::following::remove(&paths.following(), key);
    let b64: Vec<String> = everyone.iter().map(|k| STANDARD.encode(k)).collect();
    let hex: Vec<String> = everyone.iter().map(crate::peerstore::file_name).collect();
    let first_field_ours = |line: &str| line.split('\t').next().is_some_and(|f| b64.iter().any(|k| k == f));
    retain(&paths.followers(), |line| !first_field_ours(line));
    if !block {
        retain(&paths.displaynames(), |line| !first_field_ours(line));
    }
    retain(&paths.lastseen(), |line| !line.split(' ').next().is_some_and(|f| hex.iter().any(|k| k == f)));
    let user = STANDARD.encode(key);
    for file in ["peer-device-keys", "refused-devices"] {
        retain(&paths.dir().join(file), |line| line.split('\t').next() != Some(user.as_str()));
    }
    {
        let _guard = crate::store::lock();
        let all = crate::transfer::load(&paths.transfers());
        let (theirs, rest): (Vec<_>, Vec<_>) = all.into_iter().partition(|t| everyone.contains(&t.peer));
        if !theirs.is_empty() {
            for t in &theirs {
                crate::transfer::discard_part(t);
            }
            let _ = crate::transfer::save_all(&paths.transfers(), &rest);
        }
    }
    let carbons = paths.dir().join("carbons");
    for (_, file) in crate::store::names_in(&carbons) {
        retain(&file, |line| line.split('\t').next() != Some(user.as_str()));
    }
    let _ = crate::readmarks::forget(&paths.read_marks(), &crate::readmarks::Chat::Contact(*key));
    let requests = paths.requests();
    let _ = if block { crate::requests::block(&requests, *key, crate::unix_now()) } else { crate::requests::forget(&requests, key) };
}

/// The devices of `user` met here (`peer-device-keys`).
fn devices_of(paths: &Paths, user: &[u8; 32]) -> Vec<[u8; 32]> {
    let user = STANDARD.encode(user);
    crate::store::read(paths.dir().join("peer-device-keys"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (u, d) = line.split_once('\t')?;
            (u == user).then(|| STANDARD.decode(d).ok()?.try_into().ok()).flatten()
        })
        .collect()
}

fn remove(path: &Path) {
    let _guard = crate::store::lock();
    let _ = std::fs::remove_file(path);
}

/// Keeps only the lines of `path` that `keep` is true of; a file left
/// empty goes.
fn retain(path: &Path, keep: impl Fn(&str) -> bool) {
    let _guard = crate::store::lock();
    let Ok(text) = crate::store::read(path) else { return };
    let kept: Vec<&str> = text.lines().filter(|l| keep(l)).collect();
    if kept.len() == text.lines().count() {
        return;
    }
    let _ = if kept.is_empty() { std::fs::remove_file(path) } else { crate::store::write_atomic(path, kept.iter().map(|l| format!("{l}\n")).collect::<String>()) };
}
