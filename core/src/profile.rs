//! What your peers see of you besides your name: a short status and a
//! small picture (an avatar). Unlike the display name (`displayname`),
//! which anyone may ask for, a profile is only ever given to a peer, on
//! a peer's connection — a stranger never gets it, nor has a picture of
//! theirs decoded here.
//!
//! One record holds both, signed by the identity it's about, so it can
//! be checked wherever it came from: `<unix_secs>\t<base64_signature>\t
//! <base64_avatar>\t<status>`. Your own in `~/.swarm/profile`; your
//! peers' one file each under `~/.swarm/profiles`, keeping the newest.
//!
//! The picture is untrusted however it arrived, so it has to be the one
//! simple thing this module allows: a baseline JPEG, square, at most
//! `MAX_AVATAR_SIDE` across, three components, at most
//! `MAX_AVATAR_BYTES`. Whoever sets one re-encodes it to that (see
//! `swarm_app::avatar`); here only its headers are checked, and anything
//! else refuses the whole record.
//!
//! That's more than a frame holds, so a profile's sent in parts
//! (`part`), each with the record's time, signature and status and a
//! piece of the picture, and put back together as they come (`gather`).

use crate::identity::{self, Identity};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The longest status, in characters.
pub const MAX_STATUS_CHARS: usize = 80;
/// An avatar's largest width and height, in pixels.
pub const MAX_AVATAR_SIDE: usize = 640;
/// The largest avatar, in bytes: a photo at that size, well compressed.
pub const MAX_AVATAR_BYTES: usize = 96 * 1024;
/// The most of a picture in one part (see `part`): with the rest of the
/// record, well inside a frame.
pub const PART_BYTES: usize = 6 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub at: u64,
    /// "" for none.
    pub status: String,
    /// The JPEG; empty for none.
    pub avatar: Vec<u8>,
    pub signature: [u8; 64],
}

/// Whether `status` can be one: short, and on one line (see `crate::text`).
pub fn is_valid_status(status: &str) -> bool {
    status.chars().count() <= MAX_STATUS_CHARS && crate::text::is_single_line(status)
}

/// Whether `jpeg` is an avatar this module allows (empty: none): a
/// baseline JPEG, square and at most `MAX_AVATAR_SIDE` across, in
/// colour, not too big. Only its headers, up to the scan, are read.
pub fn is_valid_avatar(jpeg: &[u8]) -> bool {
    if jpeg.is_empty() {
        return true;
    }
    if jpeg.len() > MAX_AVATAR_BYTES || !jpeg.starts_with(&[0xFF, 0xD8]) || !jpeg.ends_with(&[0xFF, 0xD9]) {
        return false;
    }
    let mut at = 2;
    let mut frame = false;
    loop {
        if jpeg.get(at) != Some(&0xFF) {
            return false;
        }
        let Some(&marker) = jpeg.get(at + 1) else { return false };
        let Some(len) = jpeg.get(at + 2..at + 4).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) else { return false };
        let Some(segment) = jpeg.get(at + 4..at + 2 + len).filter(|_| len >= 2) else { return false };
        match marker {
            0xC0 if !frame => {
                if segment.len() < 6 || segment[0] != 8 {
                    return false;
                }
                let height = u16::from_be_bytes([segment[1], segment[2]]) as usize;
                let width = u16::from_be_bytes([segment[3], segment[4]]) as usize;
                if height != width || width == 0 || width > MAX_AVATAR_SIDE || segment[5] != 3 {
                    return false;
                }
                frame = true;
            }
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xCC | 0xC8 | 0xD0..=0xD9 | 0x01 | 0xDC | 0xDE | 0xDF => return false,
            0xDA => return frame,
            _ => {}
        }
        at += 2 + len;
    }
}

fn signable_bytes(at: u64, status: &str, avatar: &[u8]) -> Vec<u8> {
    let mut bytes = b"swarm profile\0".to_vec();
    bytes.extend_from_slice(&at.to_be_bytes());
    bytes.extend_from_slice(&Sha256::digest(avatar));
    bytes.extend_from_slice(status.as_bytes());
    bytes
}

/// Whether `profile` is one this module allows, signed by whoever holds
/// `author_public_key`. Whatever arrives has to pass this before it's
/// kept or shown; `cache` checks it itself.
pub fn verify(profile: &Profile, author_public_key: &[u8; 32]) -> bool {
    is_valid_status(&profile.status)
        && is_valid_avatar(&profile.avatar)
        && identity::verify(author_public_key, &signable_bytes(profile.at, &profile.status, &profile.avatar), &profile.signature)
}

fn format(profile: &Profile) -> String {
    format!("{}\t{}\t{}\t{}\n", profile.at, STANDARD.encode(profile.signature), STANDARD.encode(&profile.avatar), profile.status)
}

fn parse(text: &str) -> Option<Profile> {
    let line = text.lines().next()?;
    let mut parts = line.splitn(4, '\t');
    let at = parts.next()?.parse().ok()?;
    let signature = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let avatar = STANDARD.decode(parts.next()?).ok()?;
    let status = parts.next()?.to_string();
    Some(Profile { at, status, avatar, signature })
}

/// Publishes this device's profile (replacing the one before): `status`
/// ("" for none) and `avatar` (empty for none). Always newer than the
/// one it replaces, even if the clock went back, so peers take it.
pub fn set(path: &Path, identity: &Identity, status: &str, avatar: &[u8]) -> std::io::Result<Profile> {
    let invalid = |what: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, what);
    if !is_valid_status(status) {
        return Err(invalid(format!("a status has to be at most {MAX_STATUS_CHARS} characters on one line")));
    }
    if !is_valid_avatar(avatar) {
        return Err(invalid(format!("a picture has to be a square baseline JPEG of at most {MAX_AVATAR_SIDE}×{MAX_AVATAR_SIDE} and {MAX_AVATAR_BYTES} bytes")));
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let at = now.max(current(path).map_or(0, |p| p.at + 1));
    let signature = identity.sign(&signable_bytes(at, status, avatar));
    let profile = Profile { at, status: status.to_string(), avatar: avatar.to_vec(), signature };
    crate::store::write_atomic(path, format(&profile))?;
    Ok(profile)
}

/// This device's own profile, if it's ever set one.
pub fn current(path: &Path) -> Option<Profile> {
    if !path.exists() {
        return None;
    }
    parse(&crate::store::read(path).ok()?)
}

/// The profile `pubkey`'s peer last gave, checked again on reading.
pub fn cached(dir: &Path, pubkey: &[u8; 32]) -> Option<Profile> {
    let path = crate::peerstore::file_for(dir, pubkey);
    if !path.exists() {
        return None;
    }
    parse(&crate::store::read(path).ok()?).filter(|p| verify(p, pubkey))
}

/// Keeps `profile` as `pubkey`'s if it's theirs and newer than the one
/// kept. Whether it was kept.
pub fn cache(dir: &Path, pubkey: &[u8; 32], profile: &Profile) -> std::io::Result<bool> {
    if !verify(profile, pubkey) || cached(dir, pubkey).is_some_and(|kept| kept.at >= profile.at) {
        return Ok(false);
    }
    std::fs::create_dir_all(dir)?;
    crate::store::write_atomic(&crate::peerstore::file_for(dir, pubkey), format(profile))?;
    Ok(true)
}

/// One part of a profile as it's sent: the whole record but for its
/// picture, of which `data` is the piece from `from`, of `avatar_len`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePart {
    pub at: u64,
    pub signature: [u8; 64],
    pub status: String,
    pub avatar_len: u32,
    pub from: u32,
    pub data: Vec<u8>,
}

/// The part of `profile` from `from` in its picture: at most
/// `PART_BYTES` of it (none past its end).
pub fn part(profile: &Profile, from: usize) -> ProfilePart {
    let from = from.min(profile.avatar.len());
    let to = (from + PART_BYTES).min(profile.avatar.len());
    ProfilePart {
        at: profile.at,
        signature: profile.signature,
        status: profile.status.clone(),
        avatar_len: profile.avatar.len() as u32,
        from: from as u32,
        data: profile.avatar[from..to].to_vec(),
    }
}

/// A profile being put back together.
#[derive(Debug)]
pub enum Gathered {
    /// All of it (still to be checked: `cache` does).
    Whole(Profile),
    /// Not yet: `data` holds the picture so far; the next part's from
    /// its end.
    More(ProfilePart),
    /// A part that doesn't follow on from what's gathered, or can't be
    /// one: given up on.
    Wrong,
}

/// `so_far` with `next` added. A first part (from 0) always starts
/// afresh — they changed theirs since the last was asked for.
pub fn gather(so_far: Option<ProfilePart>, next: ProfilePart) -> Gathered {
    let len = next.avatar_len as usize;
    if len > MAX_AVATAR_BYTES || next.from as usize + next.data.len() > len || (next.data.is_empty() && len > 0) {
        return Gathered::Wrong;
    }
    let mut got = match so_far {
        _ if next.from == 0 => next,
        Some(mut got)
            if (got.at, got.signature, &got.status, got.avatar_len) == (next.at, next.signature, &next.status, next.avatar_len)
                && got.data.len() == next.from as usize =>
        {
            got.data.extend_from_slice(&next.data);
            got
        }
        _ => return Gathered::Wrong,
    };
    if got.data.len() < len {
        return Gathered::More(got);
    }
    Gathered::Whole(Profile { at: got.at, status: std::mem::take(&mut got.status), avatar: got.data, signature: got.signature })
}

/// Forgets `pubkey`'s profile (they're no longer a peer).
pub fn forget(dir: &Path, pubkey: &[u8; 32]) {
    let _ = std::fs::remove_file(crate::peerstore::file_for(dir, pubkey));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-profile-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// The headers of a JPEG `side` square (baseline unless `sof` says
    /// otherwise), padded to `len` bytes: all `is_valid_avatar` reads.
    pub(crate) fn jpeg(sof: u8, side: u16, components: u8, len: usize) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend_from_slice(&[0xFF, 0xE0, 0, 4, 0, 0]);
        j.extend_from_slice(&[0xFF, sof, 0, 8 + 3 * components as u16 as u8, 8]);
        j.extend_from_slice(&side.to_be_bytes());
        j.extend_from_slice(&side.to_be_bytes());
        j.push(components);
        for c in 0..components {
            j.extend_from_slice(&[c + 1, 0x11, 0]);
        }
        j.extend_from_slice(&[0xFF, 0xDA, 0, 2]);
        j.resize(len.max(j.len() + 2) - 2, 0x55);
        j.extend_from_slice(&[0xFF, 0xD9]);
        j
    }

    pub(crate) fn picture() -> Vec<u8> {
        jpeg(0xC0, 128, 3, 3000)
    }

    #[test]
    fn only_a_square_baseline_colour_jpeg_up_to_640_is_an_avatar() {
        assert!(is_valid_avatar(&[]));
        assert!(is_valid_avatar(&picture()));
        assert!(is_valid_avatar(&jpeg(0xC0, 640, 3, MAX_AVATAR_BYTES)));
        assert!(is_valid_avatar(&jpeg(0xC0, 30, 3, 3000)), "a small one, as it came");
        const _: () = assert!(MAX_AVATAR_BYTES >= 64 * 1024);
        assert!(!is_valid_avatar(&jpeg(0xC0, 640, 3, MAX_AVATAR_BYTES + 1)), "too big");
        assert!(!is_valid_avatar(&jpeg(0xC2, 128, 3, 3000)), "progressive");
        assert!(!is_valid_avatar(&jpeg(0xC1, 128, 3, 3000)), "extended");
        assert!(!is_valid_avatar(&jpeg(0xC0, 641, 3, 3000)), "over 640");
        assert!(!is_valid_avatar(&jpeg(0xC0, 0, 3, 3000)), "empty");
        let mut wide = jpeg(0xC0, 640, 3, 3000);
        wide[13] = 0x01;
        assert!(!is_valid_avatar(&wide), "not square");
        assert!(!is_valid_avatar(&jpeg(0xC0, 128, 1, 3000)), "grey");
        let mut cut = picture();
        cut.truncate(12);
        assert!(!is_valid_avatar(&cut), "cut short");
        assert!(!is_valid_avatar(b"\x89PNG\r\n\x1a\n...."), "not a JPEG");
        let mut long = picture();
        long[4] = 0xFF;
        assert!(!is_valid_avatar(&long));
        assert!(!is_valid_avatar(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2, 0xFF, 0xD9]));
    }

    /// A profile goes in parts, and is put back together from them: a
    /// part that doesn't follow on is refused, and one starting again
    /// (they changed it meanwhile) starts it again.
    #[test]
    fn a_profile_goes_in_parts_and_comes_back_whole() {
        let me = Identity::generate();
        let dir = scratch("parts");
        let big = set(&dir.join("big"), &me, "out", &jpeg(0xC0, 640, 3, MAX_AVATAR_BYTES)).unwrap();
        let mut so_far = None;
        let mut parts = 0;
        let whole = loop {
            let from = so_far.as_ref().map_or(0, |p: &ProfilePart| p.data.len());
            let next = part(&big, from);
            assert!(next.data.len() <= PART_BYTES);
            parts += 1;
            match gather(so_far.take(), next) {
                Gathered::Whole(profile) => break profile,
                Gathered::More(more) => so_far = Some(more),
                Gathered::Wrong => panic!("refused a good part"),
            }
        };
        assert_eq!(whole, big);
        assert_eq!(parts, MAX_AVATAR_BYTES.div_ceil(PART_BYTES));
        let bare = set(&dir.join("bare"), &me, "in", &[]).unwrap();
        assert!(matches!(gather(None, part(&bare, 0)), Gathered::Whole(p) if p == bare));
        let first = match gather(None, part(&big, 0)) {
            Gathered::More(first) => first,
            _ => panic!("more to come"),
        };
        assert!(matches!(gather(Some(first.clone()), part(&big, 2 * PART_BYTES)), Gathered::Wrong));
        assert!(matches!(gather(None, part(&big, PART_BYTES)), Gathered::Wrong), "not the start");
        let mut long = part(&big, 0);
        long.avatar_len = MAX_AVATAR_BYTES as u32 + 1;
        assert!(matches!(gather(None, long), Gathered::Wrong), "longer than any picture");
        let newer = set(&dir.join("big"), &me, "back", &jpeg(0xC0, 640, 3, 2 * PART_BYTES)).unwrap();
        match gather(Some(first), part(&newer, 0)) {
            Gathered::More(again) => assert_eq!((again.at, again.data.len()), (newer.at, PART_BYTES)),
            _ => panic!("started again"),
        }
    }

    #[test]
    fn a_status_is_short_and_on_one_line() {
        assert!(is_valid_status(""));
        assert!(is_valid_status(&"é".repeat(MAX_STATUS_CHARS)));
        assert!(!is_valid_status(&"a".repeat(MAX_STATUS_CHARS + 1)));
        assert!(!is_valid_status("out\nback soon"));
        assert!(!is_valid_status("\x1b[31mred"));
    }

    #[test]
    fn set_then_current_round_trips_and_verifies_and_each_is_newer() {
        let dir = scratch("own");
        let path = dir.join("profile");
        let me = Identity::generate();
        let first = set(&path, &me, "at the beach", &picture()).unwrap();
        assert_eq!(current(&path), Some(first.clone()));
        assert!(verify(&first, &me.public_key_bytes()));
        let second = set(&path, &me, "", &[]).unwrap();
        assert!(second.at > first.at, "newer even within the same second");
        assert_eq!(current(&path).unwrap().status, "");
        assert!(set(&path, &me, "x", &jpeg(0xC2, 128, 3, 3000)).is_err());
        assert!(set(&path, &me, &"a".repeat(81), &[]).is_err());
        assert_eq!(current(&path), Some(second));
    }

    #[test]
    fn a_profile_tampered_with_or_from_someone_else_isnt_kept() {
        let dir = scratch("tamper");
        let them = Identity::generate();
        let other = Identity::generate();
        let theirs = set(&dir.join("theirs"), &them, "hello", &picture()).unwrap();
        let profiles = dir.join("profiles");

        assert!(!cache(&profiles, &other.public_key_bytes(), &theirs).unwrap());
        let mut changed = theirs.clone();
        changed.status = "goodbye".to_string();
        assert!(!cache(&profiles, &them.public_key_bytes(), &changed).unwrap());
        let mut repainted = theirs.clone();
        repainted.avatar[100] ^= 1;
        assert!(!cache(&profiles, &them.public_key_bytes(), &repainted).unwrap());
        assert_eq!(cached(&profiles, &them.public_key_bytes()), None);

        assert!(cache(&profiles, &them.public_key_bytes(), &theirs).unwrap());
        assert_eq!(cached(&profiles, &them.public_key_bytes()), Some(theirs));
    }

    #[test]
    fn an_older_profile_doesnt_replace_a_newer_one() {
        let dir = scratch("older");
        let them = Identity::generate();
        let key = them.public_key_bytes();
        let own = dir.join("theirs");
        let old = set(&own, &them, "old", &[]).unwrap();
        let new = set(&own, &them, "new", &picture()).unwrap();
        let profiles = dir.join("profiles");
        assert!(cache(&profiles, &key, &new).unwrap());
        assert!(!cache(&profiles, &key, &old).unwrap());
        assert!(!cache(&profiles, &key, &new).unwrap(), "the same again changes nothing");
        assert_eq!(cached(&profiles, &key).unwrap().status, "new");
        forget(&profiles, &key);
        assert_eq!(cached(&profiles, &key), None);
    }
}
