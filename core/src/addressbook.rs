//! Where peers have said they can be reached (`protocol::Notice::
//! Addresses`, sent by each side of a peer connection when it comes up
//! and whenever its addresses change), and where a dial last got
//! through. Kept apart from the peers list, which holds only what was
//! typed; both are dialled. So two peers who move one at a time keep
//! finding each other: whichever moves dials the other, says where it is
//! now, and the other can dial it there later.
//!
//! One file per peer (`~/.swarm/addresses/<key hex>`), one address per
//! line, most recently seen first, at most `MAX_PER_PEER`. Addresses are
//! literal IPs only (never a name to look up), in the peers file's form:
//! the bare IP on the default port, `[ip]:port` otherwise. An IPv6
//! link-local address is kept without its interface, which only means
//! something on the device that sent it; it's tried on each of ours.
//!
//! What they last said they're at, and when, is a first line of its own
//! (`told <unix secs> <address>,<address>...`) — so their details can
//! show where they are now apart from where they've been. It isn't an
//! address, so a reader that doesn't know it skips it.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;

pub const MAX_PER_PEER: usize = 64;

/// `text` as an address worth keeping, in its kept form — `None` for
/// anything that isn't a literal IP (or IP and port) someone could be
/// reached at.
pub fn checked(text: &str) -> Option<String> {
    let text = text.trim();
    let addr = match text.parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(_) => SocketAddr::new(text.split('%').next()?.parse::<IpAddr>().ok()?, crate::transport::DEFAULT_PORT),
    };
    if addr.port() == 0 {
        return None;
    }
    Some(form(addr)).filter(|a| !a.is_empty())
}

/// Where a connection someone dialled came from, as somewhere to find
/// them again: only when it came from Swarm's own port. Anything else is
/// a port of the moment (a one-off connection's, from a fresh socket, or
/// one a NAT picked) that nothing listens on afterwards; dialled later it
/// only ever gets "port unreachable". They say where they are as soon as
/// the link's up anyway.
pub fn came_from(remote: SocketAddr) -> Vec<String> {
    if remote.port() == crate::transport::DEFAULT_PORT { vec![form(remote)] } else { Vec::new() }
}

/// The kept form of `addr` (see the module docs).
pub fn form(addr: SocketAddr) -> String {
    let addr = crate::transport::canonical(addr);
    let ip = addr.ip();
    if ip.is_unspecified() || ip.is_multicast() {
        return String::new();
    }
    let bare = match addr {
        SocketAddr::V6(mut v6) => {
            v6.set_scope_id(0);
            SocketAddr::V6(v6)
        }
        v4 => v4,
    };
    crate::peers::address_for(bare)
}

/// `key`'s addresses, most recently seen first.
pub fn load(dir: &Path, key: &[u8; 32]) -> Vec<String> {
    crate::store::read(crate::peerstore::file_for(dir, key)).unwrap_or_default().lines().filter_map(checked).collect()
}

/// What a peer last said it's at (`protocol::Notice::Addresses`), and
/// when (Unix seconds).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Told {
    pub at: u64,
    pub addresses: Vec<String>,
}

const TOLD: &str = "told ";

fn told_line(told: &Told) -> String {
    format!("{TOLD}{} {}\n", told.at, told.addresses.join(","))
}

fn parse_told(line: &str) -> Option<Told> {
    let (at, list) = line.strip_prefix(TOLD)?.split_once(' ')?;
    let addresses: Vec<String> = list.split(',').filter_map(checked).collect();
    Some(Told { at: at.parse().ok()?, addresses }).filter(|t| !t.addresses.is_empty())
}

/// What `key` last said it's at, if it ever has.
pub fn told(dir: &Path, key: &[u8; 32]) -> Option<Told> {
    parse_told(crate::store::read(crate::peerstore::file_for(dir, key)).ok()?.lines().next()?)
}

/// `key` saying it's at `list` now (at `at`): kept as what it last said,
/// and each address seen (see `see`). Whether that's different from what
/// it said before — it's moved, or it's the first time.
pub fn hear(dir: &Path, key: &[u8; 32], list: &[String], at: u64) -> std::io::Result<bool> {
    let addresses: Vec<String> = list.iter().filter_map(|a| checked(a)).collect();
    if addresses.is_empty() {
        return Ok(false);
    }
    see(dir, key, &addresses)?;
    let _guard = crate::store::lock();
    let path = crate::peerstore::file_for(dir, key);
    let text = crate::store::read(&path).unwrap_or_default();
    let before = text.lines().next().and_then(parse_told);
    let moved = before.as_ref().is_none_or(|b| b.addresses != addresses);
    let rest: String = text.lines().filter(|l| checked(l).is_some()).map(|l| format!("{l}\n")).collect();
    crate::store::write_atomic(&path, told_line(&Told { at, addresses }) + &rest)?;
    Ok(moved)
}

/// Puts `seen` at the front of `key`'s addresses, in the order given,
/// keeping the newest `MAX_PER_PEER`. Whether anything new came of it.
pub fn see(dir: &Path, key: &[u8; 32], seen: &[String]) -> std::io::Result<bool> {
    let seen: Vec<String> = seen.iter().filter_map(|a| checked(a)).filter(|a| !a.is_empty()).collect();
    if seen.is_empty() {
        return Ok(false);
    }
    let _guard = crate::store::lock();
    let old = load(dir, key);
    let told = told(dir, key);
    let mut all = seen.clone();
    for a in &old {
        if !all.contains(a) {
            all.push(a.clone());
        }
    }
    all.dedup();
    all.truncate(MAX_PER_PEER);
    if all == old {
        return Ok(false);
    }
    let new = seen.iter().any(|a| !old.contains(a));
    std::fs::create_dir_all(dir)?;
    let told = told.as_ref().map(told_line).unwrap_or_default();
    crate::store::write_atomic(&crate::peerstore::file_for(dir, key), told + &all.iter().map(|a| format!("{a}\n")).collect::<String>())?;
    Ok(new)
}

/// `key`'s addresses to dial: each kept one as socket addresses, a
/// link-local IPv6 one once per interface in `scopes`.
pub fn dialable(addresses: &[String], scopes: &[u32]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for a in addresses {
        for addr in crate::peers::literal_addresses(a) {
            match addr {
                SocketAddr::V6(v6) if v6.ip().is_unicast_link_local() && v6.scope_id() == 0 => {
                    for scope in scopes {
                        let mut scoped = v6;
                        scoped.set_scope_id(*scope);
                        out.push(SocketAddr::V6(scoped));
                    }
                }
                addr => out.push(addr),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn where_a_connection_came_from_is_kept_only_from_swarms_own_port() {
        assert_eq!(came_from("192.168.5.1:1977".parse().unwrap()), ["192.168.5.1"]);
        assert!(came_from("192.168.5.1:35024".parse().unwrap()).is_empty(), "a port of the moment");
        assert!(came_from("[2001:db8::1]:51234".parse().unwrap()).is_empty());
    }

    #[test]
    fn newest_first_capped_and_literal_only() {
        let dir = std::env::temp_dir().join(format!("swarm-addressbook-test-{}", std::process::id()));
        let key = [1u8; 32];
        assert!(see(&dir, &key, &["2001:db8::1".into(), "192.168.1.5".into()]).unwrap());
        assert!(see(&dir, &key, &["[2001:db8::2]:5000".into(), "example.com".into(), "0.0.0.0".into(), "ff02::1".into(), "fe80::1%3".into()]).unwrap());
        assert_eq!(load(&dir, &key), vec!["[2001:db8::2]:5000", "fe80::1", "2001:db8::1", "192.168.1.5"]);
        assert!(!see(&dir, &key, &["192.168.1.5".into()]).unwrap());
        assert_eq!(load(&dir, &key)[0], "192.168.1.5");
        let many: Vec<String> = (0..100).map(|i| format!("2001:db8::{i:x}")).collect();
        see(&dir, &key, &many).unwrap();
        assert_eq!(load(&dir, &key).len(), MAX_PER_PEER);
        assert_eq!(load(&dir, &key)[0], "2001:db8::");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_they_last_said_is_kept_apart() {
        let dir = std::env::temp_dir().join(format!("swarm-addressbook-told-{}", std::process::id()));
        let key = [2u8; 32];
        assert!(told(&dir, &key).is_none());
        assert!(hear(&dir, &key, &["2001:db8:1::1".into(), "192.168.1.5".into()], 100).unwrap());
        assert!(!hear(&dir, &key, &["2001:db8:1::1".into(), "192.168.1.5".into()], 110).unwrap());
        assert_eq!(told(&dir, &key), Some(Told { at: 110, addresses: vec!["2001:db8:1::1".into(), "192.168.1.5".into()] }));
        assert!(hear(&dir, &key, &["fd06::1".into(), "fe80::9%3".into()], 200).unwrap());
        assert_eq!(told(&dir, &key).unwrap().addresses, vec!["fd06::1", "fe80::9"]);
        assert_eq!(load(&dir, &key), vec!["fd06::1", "fe80::9", "2001:db8:1::1", "192.168.1.5"]);
        see(&dir, &key, &["192.168.1.5".into()]).unwrap();
        assert_eq!(told(&dir, &key).unwrap().at, 200);
        assert_eq!(load(&dir, &key)[0], "192.168.1.5");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn link_local_is_tried_on_each_interface() {
        let addrs = dialable(&["fe80::1".into(), "2001:db8::1".into()], &[2, 5]);
        let shown: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        assert_eq!(shown, vec!["[fe80::1%2]:1977", "[fe80::1%5]:1977", "[2001:db8::1]:1977"]);
    }
}
