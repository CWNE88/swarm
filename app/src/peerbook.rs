//! Changes to your peers list (`~/.swarm/peers`): add, update, rename,
//! set a cover rate, remove. The file is changed first; then whatever
//! swarm is running is told, so the change takes effect at once — the
//! node in this same process if there is one (`Some(node)`), otherwise
//! whichever app is running on this device, through the control socket.
//! Each returns a sentence saying what was done, for the front end to
//! show however it shows things.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::{Path, PathBuf};
use swarm_core::node::{format_bitrate, InUse, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};
use swarm_core::requests;

/// A base64 public key, as typed or pasted.
pub fn decode_key(text: &str) -> Result<[u8; 32]> {
    STANDARD
        .decode(text.trim())
        .context("the key isn't valid base64")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("a key has to be exactly 32 bytes"))
}

/// An address field as typed, or as a contact line gives it: several
/// addresses separated by commas, any spaces around them dropped
/// ("2001:db8::5, 203.0.113.7" is "2001:db8::5,203.0.113.7").
fn tidy_address(address: &str) -> String {
    peers::address_items(address).collect::<Vec<_>>().join(",")
}

/// Where a change to peer `name` goes: on `path`, in the device's folder.
struct Target {
    peers: PathBuf,
    folder: Paths,
    name: String,
    node: Option<NodeHandle>,
}

fn target(path: &Path, name: &str, node: Option<&NodeHandle>) -> Target {
    Target { peers: path.to_path_buf(), folder: crate::paths(), name: name.to_string(), node: node.cloned() }
}

/// A name as typed.
fn untagged(name: &str) -> &str {
    name
}

fn on_file(path: &Path, name: &str) -> Option<Peer> {
    let all = peers::load(path).ok()?;
    peers::find(&all, name).ok().cloned()
}

/// An empty `name` names them after themselves: the name they publish,
/// once it's known (a short form of their key until then), following it
/// when they change it, until they're renamed by hand.
pub fn add(path: &Path, name: &str, address: &str, key: &str, node: Option<&NodeHandle>) -> Result<String> {
    let shown = name;
    let Target { peers: path, folder, name, node } = target(path, name, node);
    let (path, name, node) = (path.as_path(), name.as_str(), node.as_ref());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let public_key = decode_key(key)?;
    let settings = folder.settings();
    let after_them = name.trim().is_empty();
    let name = if after_them {
        let published = swarm_core::displayname::cached_one(&folder.displaynames(), &public_key).ok().flatten().map(|d| d.name).unwrap_or_default();
        let fallback: String = STANDARD.encode(public_key).chars().take(8).collect();
        peers::label_from(&published, &fallback)
    } else {
        name.to_string()
    };
    let name = name.as_str();
    let peer = Peer { name: name.to_string(), address: tidy_address(address), public_key, cover_bits_per_second: None, cover_required: false };
    peers::add(path, &peer).with_context(|| format!("adding them to {}", path.display()))?;
    let _ = swarm_core::settings::set_named_after_them(&settings, &public_key, after_them);
    let _ = requests::forget(&folder.requests(), &public_key);
    let _ = swarm_core::following::remove(&folder.following(), &public_key);
    match node {
        Some(node) => node.add_peer_live(peer),
        None => crate::control::notify(&format!("ADD_PEER\n{}\n", peer.handle())),
    }
    Ok(if after_them {
        format!("added {name}")
    } else {
        format!("added {shown}")
    })
}

/// `new_name` renames them too (the same name keeps it); blank, they're
/// called what they call themselves again, and follow it when they change
/// it, as one added without a name is. Message history stays with them:
/// it's filed under their key, not their name.
pub fn update(path: &Path, name: &str, new_name: &str, address: &str, key: &str, node: Option<&NodeHandle>) -> Result<String> {
    let Target { peers: path, folder, name, node } = target(path, name, node);
    let new_name = if folder.dir() == crate::paths().dir() { new_name } else { untagged(new_name) };
    let (path, name, node) = (path.as_path(), name.as_str(), node.as_ref());
    let public_key = decode_key(key)?;
    let old = on_file(path, name);
    let old_key = old.as_ref().map(|p| p.public_key);
    let shown = old.as_ref().map_or_else(|| name.to_string(), |p| p.name.clone());
    let after_them = new_name.trim().is_empty();
    let published = after_them.then(|| swarm_core::displayname::cached_one(&folder.displaynames(), &public_key).ok().flatten().map(|d| d.name)).flatten();
    let known = published.as_deref().is_some_and(|p| !p.trim().is_empty());
    let theirs = after_them.then(|| peers::label_from(published.as_deref().unwrap_or_default(), &shown));
    let new_name = theirs.as_deref().unwrap_or(new_name);
    let shown_new = new_name;
    peers::update(path, name, new_name, tidy_address(address), public_key).with_context(|| format!("updating {}", path.display()))?;
    if after_them {
        let _ = swarm_core::settings::set_named_after_them(&folder.settings(), &public_key, true);
    } else if new_name != shown {
        let _ = swarm_core::settings::set_named_after_them(&folder.settings(), &public_key, false);
    }
    match node {
        Some(node) => {
            if let Some(peer) = on_file(path, &peers::handle_of(&public_key)) {
                node.update_peer_live(&old_key.map_or_else(|| name.to_string(), |old| peers::handle_of(&old)), peer);
            }
        }
        None => {
            if let Some(old) = old_key.filter(|old| *old != public_key) {
                swarm_core::forget::from_book(&folder, &old, name);
            }
            let was = old_key.map_or_else(|| name.to_string(), |old| peers::handle_of(&old));
            crate::control::notify(&format!("UPDATE_PEER\n{was}\n{}\n", peers::handle_of(&public_key)));
        }
    }
    Ok(match (after_them, new_name == shown) {
        (true, _) if !known => format!("{shown} until they're heard from: then the name they go by"),
        (true, true) => format!("{shown}: the name they go by"),
        (true, false) => format!("renamed {shown} to {shown_new}, the name they go by"),
        (false, true) => format!("updated {shown}"),
        (false, false) => format!("renamed {shown} to {shown_new}"),
    })
}

/// Where `name`'s files go without asking: `folder` (`~` allowed; made
/// if it isn't there), or, blank or `off`, nowhere — they wait for an
/// answer as usual.
pub fn set_receive(row: &crate::sidebar::PeerRow, folder: &str) -> Result<String> {
    let (name, public_key, settings) = (&row.label, &row.public_key, row.folder.settings());
    let folder = folder.trim();
    if folder.is_empty() || folder == "off" {
        swarm_core::settings::set_receive_to(&settings, public_key, None)?;
        return Ok(format!("files from {name}: you're asked each time"));
    }
    let folder = crate::expand_home(Path::new(folder));
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let folder = folder.canonicalize()?;
    swarm_core::settings::set_receive_to(&settings, public_key, Some(&folder))?;
    Ok(format!("files from {name}: saved to {} without asking", folder.display()))
}

/// Where they can be reached, one address a row, as `(label, value)`
/// — the label only on a group's first row: the connection in use now,
/// the address on file, where they last said they're at (and when), then
/// where else they've been seen, newest first.
pub fn address_rows(node: &NodeHandle, row: &crate::sidebar::PeerRow, utc: bool) -> Vec<(&'static str, String)> {
    const SEEN_SHOWN: usize = 4;
    let mut rows = Vec::new();
    fn group(label: &'static str, values: Vec<String>, rows: &mut Vec<(&'static str, String)>) {
        for (i, value) in values.into_iter().enumerate() {
            rows.push((if i == 0 { label } else { "" }, value));
        }
    }
    let in_use: Vec<String> = node.connection_to(&row.public_key).map(|c| in_use_text(&c)).into_iter().collect();
    group("in use", if in_use.is_empty() { vec!["not connected".to_string()] } else { in_use }, &mut rows);
    let typed: Vec<String> = row.address.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
    group("address", if typed.is_empty() { vec!["none".to_string()] } else { typed.clone() }, &mut rows);
    let told = node.told_addresses(&row.public_key);
    if let Some(told) = &told {
        let now = crate::unix_now();
        let mut values = told.addresses.clone();
        values.push(format!("said {} ({} ago)", crate::time::moment(told.at, now, utc), crate::time::how_long(now.saturating_sub(told.at))));
        group("now at", values, &mut rows);
    }
    let told_list = told.map(|t| t.addresses).unwrap_or_default();
    let others: Vec<String> = node.learned_addresses(&row.public_key).into_iter().filter(|a| !typed.contains(a) && !told_list.contains(a)).collect();
    let label = if told_list.is_empty() { "also at" } else { "seen before" };
    let mut values: Vec<String> = others.iter().take(SEEN_SHOWN).cloned().collect();
    if others.len() > SEEN_SHOWN {
        values.push(format!("and {} more", others.len() - SEEN_SHOWN));
    }
    if values.is_empty() && told_list.is_empty() {
        values.push("nowhere else yet".to_string());
    }
    group(label, values, &mut rows);
    let seen: Vec<String> = node.seen_by(&row.public_key).into_iter().map(|ip| ip.to_string()).collect();
    group("they see you at", seen, &mut rows);
    rows
}

/// A connection as shown in a peer's details: this end, then theirs,
/// the arrow from whichever dialled.
pub fn in_use_text(in_use: &InUse) -> String {
    match in_use {
        InUse::Direct { here, port, there, dialled } => {
            let here = here.map(|ip| std::net::SocketAddr::new(ip, *port).to_string()).unwrap_or_else(|| format!(":{port}"));
            format!("{here} {} {there}", if *dialled { "→" } else { "←" })
        }
        InUse::Through(device) => format!("through your device {device}"),
    }
}

/// Their receive setting in words, for the peer's details.
pub fn receive_text(folder: &swarm_core::node::Paths, public_key: &[u8; 32]) -> String {
    match swarm_core::settings::receive_to(&folder.settings(), public_key) {
        Some(folder) => format!("saved to {} without asking", folder.display()),
        None => "you're asked each time".to_string(),
    }
}

/// Sets their own cover rate, or clears it (`None`: back to the default).
/// `required` asks them to send at least that rate to us too.
pub fn set_cover(path: &Path, name: &str, rate: Option<u64>, required: bool, node: Option<&NodeHandle>) -> Result<String> {
    let Target { peers: path, name, node, .. } = target(path, name, node);
    let (path, name, node) = (path.as_path(), name.as_str(), node.as_ref());
    let found = on_file(path, name);
    let shown = found.as_ref().map_or_else(|| name.to_string(), |p| p.name.clone());
    let name = found.map_or_else(|| name.to_string(), |p| p.handle());
    let name = name.as_str();
    let required = required && rate.is_some();
    peers::set_cover(path, name, rate, required).with_context(|| format!("updating {}", path.display()))?;
    match node {
        Some(node) => node.set_cover_live(name, rate, required),
        None => crate::control::notify(&format!("SET_PEER_COVER\n{name}\n")),
    }
    Ok(match (rate, required) {
        (Some(rate), true) => format!("{shown}'s cover rate set to {}, and required of them too", format_bitrate(rate)),
        (Some(rate), false) => format!("{shown}'s cover rate set to {}", format_bitrate(rate)),
        (None, _) => format!("{shown}'s cover rate cleared — back to the default, if any"),
    })
}

/// Deletes them: off the list, and everything kept about them gone,
/// here and on the user's other devices (`swarm_core::forget`). They can
/// ask to be added again. `block`: they can't, and their key is kept, as
/// blocked, with the name they last gave.
pub fn remove(path: &Path, name: &str, block: bool, node: Option<&NodeHandle>) -> Result<String> {
    let shown = name;
    let Target { peers: path, folder, name, node } = target(path, name, node);
    let (path, name, node) = (path.as_path(), name.as_str(), node.as_ref());
    let gone = on_file(path, name);
    if !peers::remove(path, name).with_context(|| format!("updating {}", path.display()))? {
        anyhow::bail!("no contact called {shown}");
    }
    let Some(Peer { public_key: key, address, name: gone_name, .. }) = gone else { anyhow::bail!("no contact called {shown}") };
    let gone_name = gone_name.as_str();
    match node {
        Some(node) => node.forget_contact(&peers::handle_of(&key), key, block),
        None => {
            if swarm_core::settings::asked(&folder.settings(), &key) {
                let _ = swarm_core::withdrawals::add(&folder.withdrawals(), swarm_core::withdrawals::What::ContactRequest, key, &address, crate::unix_now());
            }
            swarm_core::forget::contact(&folder, &key, block);
            swarm_core::forget::from_book(&folder, &key, gone_name);
            crate::control::notify(&format!("REMOVE_PEER\n{}\n{}\n{}\n", peers::handle_of(&key), STANDARD.encode(key), if block { "block" } else { "delete" }));
        }
    }
    crate::avatar::forget(&key);
    let shown = gone_name;
    Ok(if block { format!("blocked {shown}: everything about them is deleted, and they can't ask to be added again") } else { format!("deleted {shown} and everything about them") })
}

/// A peer's own cover setting as the cover prompt and Update form show
/// and accept it: `64k`, `64k require`, or blank for none.
pub fn cover_setting_text(row: &crate::sidebar::PeerRow) -> String {
    match row.cover_bits_per_second {
        Some(rate) if row.cover_required => format!("{} require", format_bitrate(rate)),
        Some(rate) => format_bitrate(rate),
        None => String::new(),
    }
}

/// Parses a cover setting: `off`, or a rate (as `--cover` takes it)
/// optionally followed by `require` — see `cover_setting_text`.
pub fn parse_cover_setting(input: &str) -> Result<(Option<u64>, bool), String> {
    let mut words = input.split_whitespace();
    let rate = words.next().unwrap_or("");
    let required = match words.next() {
        None => false,
        Some(word) if word.eq_ignore_ascii_case("require") || word.eq_ignore_ascii_case("required") => true,
        Some(other) => return Err(format!("'{other}' isn't understood after the rate — only 'require' is")),
    };
    if words.next().is_some() {
        return Err("too many words — try e.g. '64k' or '64k require'".to_string());
    }
    if rate.eq_ignore_ascii_case("off") {
        return Ok((None, false));
    }
    Ok((Some(swarm_core::node::parse_bitrate(rate)?), required))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection reads this end, then theirs, the arrow from whichever
    /// dialled; IPv6 in brackets; this end's port alone where its address
    /// can't be told.
    #[test]
    fn a_connection_in_use_reads_from_here_to_there() {
        let there = "[2001:db8::2]:1977".parse().unwrap();
        assert_eq!(in_use_text(&InUse::Direct { here: Some("2001:db8::1".parse().unwrap()), port: 1977, there, dialled: true }), "[2001:db8::1]:1977 → [2001:db8::2]:1977");
        assert_eq!(in_use_text(&InUse::Direct { here: Some("2001:db8::1".parse().unwrap()), port: 1977, there, dialled: false }), "[2001:db8::1]:1977 ← [2001:db8::2]:1977");
        assert_eq!(in_use_text(&InUse::Direct { here: None, port: 1977, there: "203.0.113.5:4000".parse().unwrap(), dialled: true }), ":1977 → 203.0.113.5:4000");
        assert_eq!(in_use_text(&InUse::Through("desktop".into())), "through your device desktop");
    }

    #[test]
    fn addresses_typed_with_spaces_after_their_commas_are_taken_as_a_list() {
        let tidied = tidy_address(" 2001:db8::5, 203.0.113.7 ,192.168.1.20 ");
        assert_eq!(tidied, "2001:db8::5,203.0.113.7,192.168.1.20");
        assert!(peers::validate_address(&tidied).is_ok());
        assert!(peers::validate_address(&tidy_address("2001:db8::5, 203.0 .113.7")).is_err());
    }
}
