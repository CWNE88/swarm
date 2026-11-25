//! A self-announced display name — "I currently go by this" — signed by
//! the identity it's about and servable to anyone who asks, the same
//! anyone-can-ask model `posts::Visibility::Public` already uses. This
//! is a property of the identity itself, not of any particular peer or
//! channel relationship, and it's a courtesy label, never proof of
//! anything: nothing stops a different key from also claiming "Bob".
//! The pubkey stays the only thing actually authenticated — a display
//! name should always be shown *alongside* the key it belongs to (e.g.
//! `Bob (067pwt0x)`), never in place of it, so an impersonation attempt
//! is still visibly a different key even if it copies the name.
//!
//! One flat file (`~/.swarm/displayname`) holding this device's own
//! current claim — a single overwritten record, not a log: changing
//! your name is just publishing a newer one. `<unix_secs>\t<base64_signature>\t<name>`.
//!
//! A second flat file (`~/.swarm/displaynames`) caches whatever claims
//! this device has learned about *other* identities, one per pubkey,
//! same "keep the newest, since the old one might just be stale"
//! reasoning `postcache` uses — `<base64_pubkey>\t<unix_secs>\t<base64_signature>\t<name>`
//! per line.

use crate::identity::{self, Identity};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The longest display name accepted, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Whether `name` is acceptable as a display name at all: not empty, not
/// too long, and on one line (see `crate::text` — a name is stored and
/// shown as it is, so it can't carry a newline or a terminal escape).
/// Part of `verify`, so a name that fails this never counts as verified.
pub fn is_valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.chars().count() <= MAX_NAME_CHARS && crate::text::is_single_line(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayName {
    pub name: String,
    pub at: u64,
    pub signature: [u8; 64],
}

fn signable_bytes(name: &str, at: u64) -> Vec<u8> {
    format!("{at}\t{name}").into_bytes()
}

/// Whether `claimed` was genuinely signed by whoever holds the private
/// key for `author_public_key` — the entire point of `signature`
/// existing (see the module docs). A caller receiving a claim over the
/// network must call this before caching or displaying it; nothing in
/// this module does it automatically, same discipline `posts::verify`
/// already requires of its callers.
pub fn verify(claimed: &DisplayName, author_public_key: &[u8; 32]) -> bool {
    is_valid_name(&claimed.name)
        && identity::verify(author_public_key, &signable_bytes(&claimed.name, claimed.at), &claimed.signature)
}
/// Publishes (overwriting any previous claim) a new display name for
/// this device's own identity. Always newer than the one it replaces,
/// even within the same second or with the clock gone back, so whoever
/// has the old one takes it.
pub fn set(path: &Path, identity: &Identity, name: &str) -> std::io::Result<()> {
    if !is_valid_name(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("a display name has to be 1 to {MAX_NAME_CHARS} characters on one line"),
        ));
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let at = now.max(current(path).ok().flatten().map_or(0, |d| d.at + 1));
    let signature = identity.sign(&signable_bytes(name, at));
    crate::store::write_atomic(path, format!("{at}\t{}\t{name}\n", STANDARD.encode(signature)))
}

/// This device's own currently published name, if it's ever set one.
pub fn current(path: &Path) -> std::io::Result<Option<DisplayName>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(crate::store::read(path)?.lines().next().and_then(parse_own_line))
}

fn parse_own_line(line: &str) -> Option<DisplayName> {
    let mut parts = line.splitn(3, '\t');
    let at = parts.next()?.parse().ok()?;
    let signature: [u8; 64] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let name = parts.next()?.to_string();
    Some(DisplayName { name, at, signature })
}

/// The most names cached. Anyone can introduce themselves under a key
/// made up on the spot, so without a limit strangers could grow the
/// cache (and the time spent checking it) without end. Past it, the one
/// longest unheard-from goes.
pub const MAX_CACHED: usize = 500;

/// The cache as it is in the file, longest unheard-from first, each
/// entry checked against its key.
fn cached_in_order(path: &Path) -> std::io::Result<Vec<([u8; 32], DisplayName)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(crate::store::read(path)?.lines().filter_map(parse_cache_line).filter(|(key, dn)| verify(dn, key)).collect())
}

/// Every other identity's display name this device currently has
/// cached, keyed by their pubkey.
pub fn cached_all(path: &Path) -> std::io::Result<HashMap<[u8; 32], DisplayName>> {
    Ok(cached_in_order(path)?.into_iter().collect())
}

/// This device's cached claim for one identity, if it has one.
pub fn cached_one(path: &Path, pubkey: &[u8; 32]) -> std::io::Result<Option<DisplayName>> {
    Ok(cached_all(path)?.remove(pubkey))
}

/// Records `claimed` for `pubkey`, replacing any existing cache entry
/// only if `claimed` is actually newer (a stale re-delivery of an old
/// claim shouldn't undo a rename the person made since). A claim that
/// doesn't verify against `pubkey` is ignored. Hearing it again (even
/// unchanged) counts as hearing from them, for `MAX_CACHED`.
pub fn cache_one(path: &Path, pubkey: [u8; 32], claimed: &DisplayName) -> std::io::Result<()> {
    cache_one_within(path, pubkey, claimed, MAX_CACHED)
}

fn cache_one_within(path: &Path, pubkey: [u8; 32], claimed: &DisplayName, most: usize) -> std::io::Result<()> {
    if !verify(claimed, &pubkey) {
        return Ok(());
    }
    let _guard = crate::store::lock();
    let mut all = cached_in_order(path)?;
    let existing = all.iter().position(|(key, _)| *key == pubkey);
    let keep = match existing {
        Some(i) if i + 1 == all.len() && claimed.at <= all[i].1.at => return Ok(()),
        Some(i) => {
            let (_, dn) = all.remove(i);
            if claimed.at > dn.at { claimed.clone() } else { dn }
        }
        None => claimed.clone(),
    };
    all.push((pubkey, keep));
    if all.len() > most {
        all.drain(..all.len() - most);
    }
    let contents: String = all
        .iter()
        .map(|(key, dn)| format!("{}\t{}\t{}\t{}\n", STANDARD.encode(key), dn.at, STANDARD.encode(dn.signature), dn.name))
        .collect();
    crate::store::write_atomic(path, contents)
}

fn parse_cache_line(line: &str) -> Option<([u8; 32], DisplayName)> {
    let mut parts = line.splitn(4, '\t');
    let pubkey: [u8; 32] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let at = parts.next()?.parse().ok()?;
    let signature: [u8; 64] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let name = parts.next()?.to_string();
    Some((pubkey, DisplayName { name, at, signature }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch_path(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-displayname-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        dir.join("file")
    }

    #[test]
    fn set_then_current_round_trips_and_verifies() {
        let path = scratch_path("own");
        let identity = Identity::generate();
        set(&path, &identity, "Bob").unwrap();

        let claimed = current(&path).unwrap().unwrap();
        assert_eq!(claimed.name, "Bob");
        assert!(verify(&claimed, &identity.public_key_bytes()));
    }

    #[test]
    fn setting_again_overwrites_rather_than_accumulating() {
        let path = scratch_path("overwrite");
        let identity = Identity::generate();
        set(&path, &identity, "Bob").unwrap();
        set(&path, &identity, "Bobby").unwrap();

        assert_eq!(current(&path).unwrap().unwrap().name, "Bobby");
    }

    #[test]
    fn verify_rejects_the_wrong_key_or_a_tampered_name() {
        let path = scratch_path("verify");
        let author = Identity::generate();
        let impostor = Identity::generate();
        set(&path, &author, "Bob").unwrap();
        let claimed = current(&path).unwrap().unwrap();

        assert!(!verify(&claimed, &impostor.public_key_bytes()));

        let mut tampered = claimed;
        tampered.name = "NotBob".to_string();
        assert!(!verify(&tampered, &author.public_key_bytes()));
    }

    fn signed(identity: &Identity, name: &str, at: u64) -> DisplayName {
        DisplayName { name: name.to_string(), at, signature: identity.sign(&signable_bytes(name, at)) }
    }

    #[test]
    fn cache_keeps_the_newest_claim_per_identity() {
        let path = scratch_path("cache");
        let bob = Identity::generate();
        let key = bob.public_key_bytes();
        let old = signed(&bob, "Bob", 100);
        let new = signed(&bob, "Bobby", 200);

        cache_one(&path, key, &old).unwrap();
        assert_eq!(cached_one(&path, &key).unwrap().unwrap().name, "Bob");

        cache_one(&path, key, &new).unwrap();
        assert_eq!(cached_one(&path, &key).unwrap().unwrap().name, "Bobby");

        cache_one(&path, key, &old).unwrap();
        assert_eq!(cached_one(&path, &key).unwrap().unwrap().name, "Bobby");
    }

    #[test]
    fn cached_all_holds_multiple_identities_independently() {
        let path = scratch_path("multi");
        let alice = Identity::generate();
        let bob = Identity::generate();
        cache_one(&path, alice.public_key_bytes(), &signed(&alice, "Alice", 1)).unwrap();
        cache_one(&path, bob.public_key_bytes(), &signed(&bob, "Bob", 1)).unwrap();

        let all = cached_all(&path).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all.get(&alice.public_key_bytes()).unwrap().name, "Alice");
        assert_eq!(all.get(&bob.public_key_bytes()).unwrap().name, "Bob");
    }

    /// Strangers introducing themselves under ever new keys can't grow
    /// the cache past its limit; whoever's heard from again stays.
    #[test]
    fn the_cache_is_limited_and_the_longest_unheard_from_goes() {
        let path = scratch_path("limit");
        let people: Vec<Identity> = (0..8).map(|_| Identity::generate()).collect();
        let name = |i: usize| signed(&people[i], &format!("P{i}"), 1);
        let heard = |i: usize| cache_one_within(&path, people[i].public_key_bytes(), &name(i), 5).unwrap();
        (0..5).for_each(heard);
        heard(0);
        (5..7).for_each(heard);
        let all = cached_all(&path).unwrap();
        assert_eq!(all.len(), 5);
        assert!(all.contains_key(&people[0].public_key_bytes()), "heard from again, so kept");
        assert!(!all.contains_key(&people[1].public_key_bytes()) && !all.contains_key(&people[2].public_key_bytes()));
        assert!(all.contains_key(&people[6].public_key_bytes()));
    }

    /// A stranger signs their own name with a newline in it, trying to
    /// slip a second cache line in that renames someone else.
    #[test]
    fn a_name_with_a_newline_never_reaches_the_cache_and_forged_lines_are_ignored() {
        let path = scratch_path("forge");
        let stranger = Identity::generate();
        let victim = [3u8; 32];
        let forged_line = format!("{}\t999\t{}\tEvil", STANDARD.encode(victim), STANDARD.encode([0u8; 64]));
        let sneaky = signed(&stranger, &format!("Nice\n{forged_line}"), 5);
        assert!(!verify(&sneaky, &stranger.public_key_bytes()));
        cache_one(&path, stranger.public_key_bytes(), &sneaky).unwrap();
        assert!(cached_all(&path).unwrap().is_empty());

        fs::write(&path, format!("{forged_line}\n")).unwrap();
        assert!(cached_one(&path, &victim).unwrap().is_none());
    }

    #[test]
    fn set_refuses_a_name_that_is_empty_too_long_or_multi_line() {
        let path = scratch_path("invalid");
        let me = Identity::generate();
        assert!(set(&path, &me, "").is_err());
        assert!(set(&path, &me, "two\nlines").is_err());
        assert!(set(&path, &me, &"x".repeat(MAX_NAME_CHARS + 1)).is_err());
        assert!(set(&path, &me, "Owen S").is_ok());
    }
}
