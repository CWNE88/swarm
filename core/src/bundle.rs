//! What one of a user's devices gives another the first time they're
//! linked (see `node::devices`): its peers and its chat history, so the
//! new device has what the others have. (Name and picture come over the
//! link as they change, by `node::ownlook`.)
//!
//! A bundle is text, one item per line, each line one of:
//!   `peer\t<a peers-file line>`
//!   `name\t<display name>`
//!   `profile\t<base64 picture>\t<status>` (see `profile`)
//!   `history\t<peer key, hex>\t<a history line>`
//!
//! Channels and posts aren't in it: they come over the devices' links
//! afterwards.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use crate::history;
use crate::identity::Identity;
use crate::node::Paths;
use crate::peers::{self, Peer};
use crate::{displayname, peerstore, store};
use std::collections::{BTreeSet, HashMap, HashSet};

/// The largest bundle taken from another device.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;

/// Everything the user has here, as text: their peers, and their chats
/// from `since` (unix seconds) on.
pub fn build(paths: &Paths, since: u64) -> String {
    let mut out = String::new();
    let peers = peers::load(&paths.peers()).unwrap_or_default();
    for peer in &peers {
        out.push_str(&format!("peer\t{}\n", peers::format_line(peer)));
    }
    if let Some(name) = paths.my_display_name() {
        out.push_str(&format!("name\t{}\n", name.name));
    }
    if let Some(profile) = crate::profile::current(&paths.profile()) {
        out.push_str(&format!("profile\t{}\t{}\n", STANDARD.encode(&profile.avatar), profile.status));
    }
    for (name, path) in crate::store::names_in(&paths.history()) {
        let Some(key) = key_from_file_name(&name) else { continue };
        for line in crate::store::read(path).unwrap_or_default().lines() {
            if line.split('\t').next().and_then(|at| at.parse::<u64>().ok()).is_none_or(|at| at < since) {
                continue;
            }
            out.push_str(&format!("history\t{}\t{line}\n", peerstore::file_name(&key)));
        }
    }
    out
}

fn key_from_file_name(name: &str) -> Option<[u8; 32]> {
    if name.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(name.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(key)
}

/// What merging a bundle changed here.
#[derive(Debug, Default)]
pub struct Merged {
    /// Peers new here, as saved (renamed if their name was taken).
    pub peers_added: Vec<Peer>,
    /// The display name taken on, if one came and differed.
    pub name: Option<String>,
    /// A status and picture taken on (with the name).
    pub profile: bool,
    /// How many conversations gained messages.
    pub conversations: usize,
}

/// Takes `bundle` (from another of the user's devices) in here. Peers
/// whose key is one of `own_devices` are left out — that's the user
/// themselves — and so are those `had` says were had here but aren't on
/// the peers list now, with their chats: deleted, or being deleted
/// (asked under the store's lock, with the list as it is then; `had`
/// mustn't take that lock). A peer already here (by key) keeps its entry
/// here; a name already taken by someone else gets a number on the end. A display
/// name, status and picture in it are taken on, signed afresh as
/// `identity`.
pub fn merge(paths: &Paths, bundle: &str, own_devices: &[[u8; 32]], had: &dyn Fn(&[u8; 32]) -> bool, identity: &Identity) -> std::io::Result<Merged> {
    let mut merged = Merged::default();
    let mut incoming_peers = Vec::new();
    let mut histories: HashMap<[u8; 32], Vec<String>> = HashMap::new();
    let mut name = None;
    let mut profile = None;
    let not_taken: HashSet<[u8; 32]>;
    for line in bundle.lines() {
        let Some((kind, rest)) = line.split_once('\t') else { continue };
        match kind {
            "peer" => {
                if let Ok(mut parsed) = peers::parse(rest) {
                    incoming_peers.append(&mut parsed);
                }
            }
            "name" if displayname::is_valid_name(rest) => name = Some(rest.to_string()),
            "profile" => {
                let Some((avatar, status)) = rest.split_once('\t') else { continue };
                if let Ok(avatar) = STANDARD.decode(avatar) {
                    profile = Some((status.to_string(), avatar));
                }
            }
            "history" => {
                let Some((key, entry)) = rest.split_once('\t') else { continue };
                if let Some(key) = key_from_file_name(key) {
                    histories.entry(key).or_default().push(entry.to_string());
                }
            }
            _ => {}
        }
    }

    {
        let _guard = store::lock();
        let path = paths.peers();
        let mut all = peers::load(&path).map_err(std::io::Error::other)?;
        let keys = incoming_peers.iter().map(|p| p.public_key).chain(histories.keys().copied());
        not_taken = keys.filter(|k| !all.iter().any(|p| p.public_key == *k) && had(k)).collect();
        for peer in incoming_peers {
            if own_devices.contains(&peer.public_key) || not_taken.contains(&peer.public_key) || all.iter().any(|p| p.public_key == peer.public_key) {
                continue;
            }
            if peers::validate_name(&peer.name).is_err() || peers::validate_address(&peer.address).is_err() {
                continue;
            }
            all.push(peer.clone());
            merged.peers_added.push(peer);
        }
        if !merged.peers_added.is_empty() {
            let text: String = all.iter().map(|p| peers::format_line(p) + "\n").collect();
            store::write_atomic(&path, text)?;
        }
    }

    for (key, lines) in histories {
        if not_taken.contains(&key) {
            continue;
        }
        if merge_history(paths, &key, &lines)? {
            merged.conversations += 1;
        }
    }

    if let Some((status, avatar)) = profile {
        merged.profile = crate::profile::set(&paths.profile(), identity, &status, &avatar).is_ok();
    }
    if let Some(name) = name
        && paths.my_display_name().map(|d| d.name) != Some(name.clone()) {
            displayname::set(&paths.displayname(), identity, &name)?;
            merged.name = Some(name);
        }
    Ok(merged)
}

/// Adds `lines` to the history with `key`, in time order, without
/// repeats. Whether anything was added.
fn merge_history(paths: &Paths, key: &[u8; 32], lines: &[String]) -> std::io::Result<bool> {
    let _guard = store::lock();
    let ours = history::all(&paths.history(), key)?;
    let same = |e: &history::Entry| format!("{}\t{}\t{}", e.at, e.direction.tag(), e.text);
    let mut seen: BTreeSet<String> = ours.iter().map(same).collect();
    let mut all: Vec<(u64, String)> = ours.iter().map(|e| (e.at, history::format_line(e))).collect();
    let before = all.len();
    for line in lines {
        let Some(mut entry) = history::parse_line(line) else { continue };
        entry.text = crate::text::message_text(&entry.text);
        if seen.insert(same(&entry)) {
            all.push((entry.at, history::format_line(&entry)));
        }
    }
    if all.len() == before {
        return Ok(false);
    }
    all.sort_by_key(|(at, _)| *at);
    let text: String = all.into_iter().map(|(_, l)| l + "\n").collect();
    store::write_atomic(&peerstore::file_for(&paths.history(), key), text)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Direction;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("swarm-bundle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn peer(name: &str, key: u8) -> Peer {
        Peer { name: name.into(), address: "2001:db8::9".into(), public_key: [key; 32], cover_bits_per_second: None, cover_required: false }
    }

    #[test]
    fn a_new_device_takes_on_the_others_and_keeps_what_it_had() {
        let (a, b) = (dir("a"), dir("b"));
        let (pa, pb) = (Paths::new(&a), Paths::new(&b));
        let (me_a, me_b) = (Identity::generate(), Identity::generate());
        peers::add(&pa.peers(), &peer("bob", 1)).unwrap();
        peers::add(&pa.peers(), &peer("carol", 2)).unwrap();
        peers::add(&pa.peers(), &Peer { public_key: me_b.public_key_bytes(), ..peer("laptop", 0) }).unwrap();
        peers::add(&pb.peers(), &peer("bob", 3)).unwrap();
        peers::add(&pb.peers(), &Peer { public_key: me_a.public_key_bytes(), ..peer("desktop", 0) }).unwrap();
        displayname::set(&pa.displayname(), &me_a, "Owen").unwrap();
        let picture = crate::profile::tests::picture();
        crate::profile::set(&pa.profile(), &me_a, "at the beach", &picture).unwrap();
        history::append(&pa.history(), &[2; 32], Direction::Sent, "hi carol, from a", 100).unwrap();
        history::append(&pb.history(), &[2; 32], Direction::Received, "hi from carol", 50).unwrap();

        let own = [me_a.public_key_bytes(), me_b.public_key_bytes()];
        let to_b = merge(&pb, &build(&pa, 0), &own, &|_| false, &me_b).unwrap();
        let mut names: Vec<&str> = to_b.peers_added.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["bob", "carol"], "a name someone else has is no reason to change theirs");
        assert_eq!(to_b.name.as_deref(), Some("Owen"));
        assert!(displayname::verify(&pb.my_display_name().unwrap(), &me_b.public_key_bytes()));
        assert!(to_b.profile);
        let theirs = crate::profile::current(&pb.profile()).unwrap();
        assert_eq!((theirs.status.as_str(), theirs.avatar.as_slice()), ("at the beach", picture.as_slice()));
        assert!(crate::profile::verify(&theirs, &me_b.public_key_bytes()));
        assert!(!peers::load(&pb.peers()).unwrap().iter().any(|p| p.public_key == me_b.public_key_bytes()));
        let texts: Vec<String> = history::all(&pb.history(), &[2; 32]).unwrap().into_iter().map(|e| e.text).collect();
        assert_eq!(texts, vec!["hi from carol", "hi carol, from a"]);
        assert_eq!(merge(&pb, &build(&pa, 0), &own, &|_| false, &me_b).unwrap().conversations, 0);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// One had here and off the list now (deleted, or being deleted)
    /// isn't brought back, nor their chat; one had and still on the list
    /// gets the chat.
    #[test]
    fn one_deleted_here_isnt_brought_back() {
        let (a, b) = (dir("gone-a"), dir("gone-b"));
        let (pa, pb) = (Paths::new(&a), Paths::new(&b));
        let (me_a, me_b) = (Identity::generate(), Identity::generate());
        peers::add(&pa.peers(), &peer("tim", 4)).unwrap();
        peers::add(&pa.peers(), &peer("carol", 2)).unwrap();
        peers::add(&pb.peers(), &peer("carol", 2)).unwrap();
        history::append(&pa.history(), &[4; 32], Direction::Received, "hello from tim", 100).unwrap();
        history::append(&pa.history(), &[2; 32], Direction::Received, "hello from carol", 100).unwrap();
        let had = |k: &[u8; 32]| *k == [4; 32] || *k == [2; 32];
        let merged = merge(&pb, &build(&pa, 0), &[me_a.public_key_bytes()], &had, &me_b).unwrap();
        assert!(merged.peers_added.is_empty());
        assert!(!peers::load(&pb.peers()).unwrap().iter().any(|p| p.public_key == [4; 32]));
        assert!(history::all(&pb.history(), &[4; 32]).unwrap().is_empty(), "nor his chat");
        assert_eq!(history::all(&pb.history(), &[2; 32]).unwrap().len(), 1, "carol's chat, she's still on the list");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }
}
