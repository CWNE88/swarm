//! How the per-peer stores (`history`, `outbox`, `postcache`) name their
//! files: by the peer's public key, never by the local name they happen
//! to be saved under. A name is just a local label — removing a peer
//! and adding someone else under the same name must not hand the new
//! peer the old one's conversation, or (worse) deliver messages queued
//! for the old key to the new one. Keying by public key also means
//! renaming a peer keeps their history. (Deleting someone deletes their
//! files: see `forget`.)
//!
//! Lowercase hex rather than base64: standard base64 can contain `/`,
//! which can't appear in a file name. On disk the name's hidden (see
//! `store::named`); `store::names_in` lists them.

use std::path::{Path, PathBuf};

/// The file `public_key`'s entries live in under a store's `dir`: named
/// by `store::named`, so the folder doesn't show whose it is.
pub fn file_for(dir: &Path, public_key: &[u8; 32]) -> PathBuf {
    crate::store::named(dir, &file_name(public_key))
}

/// A key as lowercase hex: a file's real name, and an id in records.
pub fn file_name(public_key: &[u8; 32]) -> String {
    public_key.iter().map(|b| format!("{b:02x}")).collect()
}

/// The key a file's real name (as `names_in` gives it) is for.
pub fn key_from_name(name: &str) -> Option<[u8; 32]> {
    if !is_key_file_name(name) {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&name[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(key)
}

fn is_key_file_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_is_lowercase_hex_and_recognised_as_a_key() {
        let mut key = [0u8; 32];
        key[0] = 0xab;
        key[31] = 0x0f;
        let name = file_name(&key);
        assert_eq!(name.len(), 64);
        assert!(name.starts_with("ab00"));
        assert!(name.ends_with("0f"));
        assert!(is_key_file_name(&name));
        assert!(!is_key_file_name("laptop"));
        assert_eq!(key_from_name(&name), Some(key));
    }
}
