//! The known-peers list: a flat local file, one line per peer.
//!
//! Format: `name address pubkey_base64 [cover_bits_per_second [require]]`
//! — the 4th and 5th fields are optional and only ever written by
//! `set_cover` (`swarm contact cover`), never by `add`/`update`; no 4th
//! field means "use whatever `swarm listen --cover` was started with."
//! A literal `require` 5th field means this rate is also *required* of
//! the peer's direction — see `Peer::cover_required`.
//!
//! No separate port column: a peer listening on `transport::DEFAULT_PORT`
//! (1977, same idea as SSH always being port 22 — one less thing to pass
//! around in an invite) is just given as a bare address. A peer who's
//! chosen a non-default port (`swarm listen --port`) is given as the
//! ordinary bracketed `[address]:port` form instead (`a.b.c.d:port` for
//! IPv4). Someone reachable at more than one address (IPv6 and IPv4, or
//! a LAN address as well) is given as a comma-separated list of them,
//! tried in turn — see `resolve_address`.
//!
//! Unlike `phone`'s contacts file, the public key isn't optional here —
//! this project has no unauthenticated mode. `dial` runs Noise_IK as the
//! initiator, which needs the responder's static key up front to encrypt
//! the first handshake message. No registry: this file is authoritative,
//! same as `phone`'s contacts. (Finding someone on the same network,
//! `swarm_app::discovery`, only fills in Add a contact's form.)

use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub address: String,
    pub public_key: [u8; 32],
    /// This peer's own cover-mode rate (bits/second), overriding
    /// whatever `swarm listen --cover` was started with for every other
    /// peer — `None` (no 4th field in the file) means "use the session
    /// default." Set via `swarm contact cover` / `set_cover`, not `add`/
    /// `update` (which only ever touch address/pubkey) — see `set_cover`'s
    /// docs for why that's a separate operation.
    pub cover_bits_per_second: Option<u64>,
    /// Whether `cover_bits_per_second` is also required of everything
    /// the peer sends *us*: told to them at connection start (see
    /// `protocol::send_cover_hello`), and their side raises its own
    /// outbound cover to at least this rate for the connection. Without
    /// it, each direction is shaped (or not) by its own sender's choice
    /// alone — and a peer with cover off then shows all its traffic,
    /// calls included, which also shows up in our acknowledgements.
    /// Meaningless without a rate.
    pub cover_required: bool,
}

#[derive(Debug)]
pub enum PeersError {
    Io(std::io::Error),
    Parse { line: usize, message: String },
}

impl fmt::Display for PeersError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeersError::Io(e) => write!(f, "io error: {e}"),
            PeersError::Parse { line, message } => write!(f, "line {line}: {message}"),
        }
    }
}

impl std::error::Error for PeersError {}

impl From<std::io::Error> for PeersError {
    fn from(e: std::io::Error) -> Self {
        PeersError::Io(e)
    }
}

/// The items of an address field: one address, or several separated by
/// commas (`2001:db8::1,192.168.1.20`) — every place someone might be
/// reached, tried in turn.
pub fn address_items(address: &str) -> impl Iterator<Item = &str> {
    address.split(',').map(str::trim).filter(|a| !a.is_empty())
}

/// Resolves a peer's address field to where to dial, in the order given:
/// each item almost always a bare IP, on `transport::DEFAULT_PORT`, or
/// the bracketed `[ip]:port` form (`ip:port` for IPv4) for a peer
/// listening somewhere else; a host name works too. An item that can't
/// be resolved is skipped; it's an error only if none can.
///
/// A host name is looked up without holding up anything else (the
/// system's lookup blocks, for seconds when the network's half there),
/// and given up on after `LOOKUP_TIMEOUT`. What it last gave is kept
/// (`LOOKED_UP`) and used at once, while it's looked up afresh in the
/// background for the next try: so a dynamic-DNS name is followed when
/// it changes, the IPs given beside it are never kept waiting, and with
/// DNS gone (a modem's link asleep) the last answer still serves. Only
/// with nothing else to dial is a lookup waited for.
pub async fn resolve_address(address: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    resolve_with(address, |item| async move { tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host(name_and_port(&item))).await.map(|r| r.map(|a| a.collect::<Vec<_>>())) }).await
}

/// A host name's lookup, or its running out of time.
type NameLookup = Result<std::io::Result<Vec<std::net::SocketAddr>>, tokio::time::error::Elapsed>;

/// Host name items, each with its last answer and when.
type Answers = std::collections::HashMap<String, (std::time::Instant, Vec<std::net::SocketAddr>)>;

/// Each host name item's last answer, and when.
static LOOKED_UP: std::sync::OnceLock<std::sync::Mutex<Answers>> = std::sync::OnceLock::new();
/// Host name items being looked up in the background now.
static LOOKING_UP: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();

fn looked_up() -> std::sync::MutexGuard<'static, Answers> {
    LOOKED_UP.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

fn looking_up() -> std::sync::MutexGuard<'static, std::collections::HashSet<String>> {
    LOOKING_UP.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// `resolve_address`, with `lookup` doing the looking up.
async fn resolve_with<F, Fut>(address: &str, lookup: F) -> std::io::Result<Vec<std::net::SocketAddr>>
where
    F: Fn(String) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = NameLookup> + Send + 'static,
{
    let mut found = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for item in address_items(address) {
        if let Some(addr) = literal(item) {
            found.push(addr);
            continue;
        }
        match looked_up().get(item) {
            Some((at, addrs)) => {
                found.extend(addrs.iter().copied());
                if at.elapsed() >= LOOKUP_FRESH {
                    names.push(item.to_string());
                }
            }
            None => names.push(item.to_string()),
        }
    }
    let mut last_error = None;
    if found.is_empty() {
        let all = names.into_iter().map(|item| {
            let lookup = lookup.clone();
            async move { (item.clone(), lookup(item).await) }
        });
        for (item, got) in futures_join(all).await {
            match got {
                Ok(Ok(addrs)) => {
                    looked_up().insert(item, (std::time::Instant::now(), addrs.clone()));
                    found.extend(addrs);
                }
                Ok(Err(e)) => last_error = Some(e),
                Err(_) => last_error = Some(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("looking up {item} took too long"))),
            }
        }
    } else {
        for item in names {
            if !looking_up().insert(item.clone()) {
                continue;
            }
            let lookup = lookup.clone();
            tokio::spawn(async move {
                if let Ok(Ok(addrs)) = lookup(item.clone()).await {
                    looked_up().insert(item.clone(), (std::time::Instant::now(), addrs));
                }
                looking_up().remove(&item);
            });
        }
    }
    let mut unique = Vec::new();
    for addr in found {
        if !unique.contains(&addr) {
            unique.push(addr);
        }
    }
    match (unique.is_empty(), last_error) {
        (true, Some(e)) => Err(e),
        _ => Ok(unique),
    }
}

/// Every future in `all`, run together, their outputs in order.
async fn futures_join<T: Send + 'static>(all: impl Iterator<Item = impl std::future::Future<Output = T> + Send + 'static>) -> Vec<T> {
    let handles: Vec<_> = all.map(tokio::spawn).collect();
    let mut out = Vec::new();
    for h in handles {
        if let Ok(v) = h.await {
            out.push(v);
        }
    }
    out
}

/// A host name item as the name to look up and the port: `name:port`,
/// or a bare name on `transport::DEFAULT_PORT`.
fn name_and_port(item: &str) -> (&str, u16) {
    match item.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') => match port.parse() {
            Ok(port) => (name, port),
            Err(_) => (item, crate::transport::DEFAULT_PORT),
        },
        _ => (item, crate::transport::DEFAULT_PORT),
    }
}

/// The longest a host name's lookup is waited for.
const LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long a host name's answer is used before it's looked up again
/// (in the background, the old answer used meanwhile).
const LOOKUP_FRESH: std::time::Duration = std::time::Duration::from_secs(30);

/// One address item written as an IP (`ip`, `[ip]:port`, `ip:port`).
fn literal(item: &str) -> Option<std::net::SocketAddr> {
    item.parse().ok().or_else(|| item.parse::<std::net::IpAddr>().ok().map(|ip| std::net::SocketAddr::new(ip, crate::transport::DEFAULT_PORT)))
}

/// The items of an address field written as IPs — no host names looked
/// up, for where waiting on one won't do.
pub fn literal_addresses(address: &str) -> Vec<std::net::SocketAddr> {
    let mut found: Vec<std::net::SocketAddr> = address_items(address).filter_map(literal).collect();
    found.dedup();
    found
}

/// The address-field form of where a connection came from — the inverse
/// of `resolve_address` for one address: the bare IP on the default
/// port, `[ip]:port` (or `ip:port`) otherwise. A long-running swarm
/// dials from the port it listens on, so this is where to reach them
/// back.
pub fn address_for(remote: std::net::SocketAddr) -> String {
    let remote = crate::transport::canonical(remote);
    if remote.port() == crate::transport::DEFAULT_PORT { remote.ip().to_string() } else { remote.to_string() }
}

/// What the app calls the peer with this key, in the node and between
/// its parts. A peer's name is only a label, which any number of them
/// can share; this comes from their key, so it's theirs alone and stays
/// the same when they're renamed.
pub fn handle_of(key: &[u8; 32]) -> String {
    STANDARD.encode(key)
}

impl Peer {
    /// See `handle_of`.
    pub fn handle(&self) -> String {
        handle_of(&self.public_key)
    }
}

/// Why `find` found nobody to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Nobody,
    /// More than one peer goes by that name: told apart as `name~start of key`.
    Several(String, Vec<String>),
}

impl std::fmt::Display for Lookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lookup::Nobody => write!(f, "no contact by that name"),
            Lookup::Several(name, keys) => write!(f, "{} contacts are called '{name}': say which ({})", keys.len(), keys.join(", ")),
        }
    }
}

impl From<Lookup> for PeersError {
    fn from(e: Lookup) -> PeersError {
        PeersError::Parse { line: 0, message: e.to_string() }
    }
}

/// The peer someone means by `who`: their handle (`handle_of`), which is
/// all the app itself ever uses; or, as typed by a person, their name if
/// only one peer has it, or the start of their key. Never a guess: if
/// that's more than one peer, it says so.
pub fn find<'a>(all: &'a [Peer], who: &str) -> Result<&'a Peer, Lookup> {
    if let Some(exact) = all.iter().find(|p| p.handle() == who) {
        return Ok(exact);
    }
    let named: Vec<&Peer> = all.iter().filter(|p| p.name == who).collect();
    match named[..] {
        [one] => return Ok(one),
        [] => {}
        _ => return Err(Lookup::Several(who.to_string(), named.iter().map(|p| format!("{}~{}", p.name, p.handle().chars().take(6).collect::<String>())).collect())),
    }
    if let Some((name, start)) = who.rsplit_once('~') {
        let told: Vec<&Peer> = all.iter().filter(|p| p.name == name && p.handle().starts_with(start)).collect();
        if let [one] = told[..] {
            return Ok(one);
        }
    }
    if who.len() >= 4 {
        let keyed: Vec<&Peer> = all.iter().filter(|p| p.handle().starts_with(who)).collect();
        match keyed[..] {
            [one] => return Ok(one),
            [] => {}
            _ => return Err(Lookup::Several(who.to_string(), keyed.iter().map(|p| format!("{}~{}", p.name, p.handle().chars().take(6).collect::<String>())).collect())),
        }
    }
    Err(Lookup::Nobody)
}

pub fn load(path: &Path) -> Result<Vec<Peer>, PeersError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = crate::store::read(path)?;
    parse(&text)
}

pub fn parse(input: &str) -> Result<Vec<Peer>, PeersError> {
    let mut peers = Vec::new();
    for (idx, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_no = idx + 1;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if !(3..=5).contains(&fields.len()) || fields.get(4).is_some_and(|f| *f != "require") {
            return Err(PeersError::Parse {
                line: line_no,
                message: "expected: name address pubkey [cover_bits_per_second [require]]".into(),
            });
        }
        let public_key = decode_public_key(fields[2]).map_err(|_| PeersError::Parse {
            line: line_no,
            message: "invalid public key (expected base64 of 32 bytes)".into(),
        })?;
        let cover_bits_per_second = match fields.get(3) {
            Some(raw) => Some(raw.parse().map_err(|_| PeersError::Parse {
                line: line_no,
                message: "invalid cover rate (expected a plain number of bits/second)".into(),
            })?),
            None => None,
        };
        peers.push(Peer {
            name: fields[0].to_string(),
            address: fields[1].to_string(),
            public_key,
            cover_bits_per_second,
            cover_required: fields.len() == 5,
        });
    }
    Ok(peers)
}

/// A name has to fit the file's one-line, whitespace-separated format —
/// a space would shift every field after it, and a leading `#` would
/// turn the whole line into a comment.
pub fn validate_name(name: &str) -> Result<(), PeersError> {
    let problem = if name.is_empty() {
        Some("a contact's name can't be empty")
    } else if name.chars().any(char::is_whitespace) {
        Some("a contact's name can't contain spaces")
    } else if !crate::text::is_single_line(name) {
        Some("a contact's name can't contain control or direction characters")
    } else if name.starts_with('#') {
        Some("a contact's name can't start with '#'")
    } else {
        None
    };
    match problem {
        Some(message) => Err(PeersError::Parse { line: 0, message: message.into() }),
        None => Ok(()),
    }
}

/// A label made from `wanted` (someone's display name, say): spaces
/// become dashes and a leading `#` goes, so it fits the file. `fallback`
/// if nothing's left of `wanted`.
pub fn label_from(wanted: &str, fallback: &str) -> String {
    let base: String = wanted.split_whitespace().collect::<Vec<_>>().join("-").trim_start_matches('#').to_string();
    if base.is_empty() { fallback.to_string() } else { base }
}

/// An address has to be one whitespace-free field of the peers file
/// (several addresses are separated by commas, not spaces).
pub fn validate_address(address: &str) -> Result<(), PeersError> {
    if address_items(address).next().is_none() || address.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(PeersError::Parse { line: 0, message: "an address can't be empty or contain spaces (separate several with commas)".into() });
    }
    Ok(())
}

/// Appends one peer to the file, creating it if it doesn't exist yet.
/// One line per key: the key's who they are, and any number of people
/// can go by the same name.
pub fn add(path: &Path, peer: &Peer) -> Result<(), PeersError> {
    let _guard = crate::store::lock();
    validate_name(&peer.name)?;
    validate_address(&peer.address)?;
    let existing = load(path)?;
    if let Some(same) = existing.iter().find(|p| p.public_key == peer.public_key) {
        return Err(PeersError::Parse { line: 0, message: format!("that key is already your contact '{}'", same.name) });
    }
    crate::store::append_line(path, &format_line(peer))?;
    Ok(())
}

/// Removes the peer `who` (see `find`). `Ok(false)` (not an error) means
/// there was no such peer to begin with.
pub fn remove(path: &Path, who: &str) -> Result<bool, PeersError> {
    let _guard = crate::store::lock();
    let all = load(path)?;
    let key = match find(&all, who) {
        Ok(peer) => peer.public_key,
        Err(Lookup::Nobody) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let remaining: Vec<Peer> = all.into_iter().filter(|p| p.public_key != key).collect();
    write_all(path, &remaining)?;
    Ok(true)
}

/// Replaces the name, address and key of the peer `who` (see `find`),
/// keeping their position in the file. Errors if there's no such peer:
/// `add` is for that, so a typo can't silently make a new entry.
pub fn update(path: &Path, who: &str, new_name: &str, address: String, public_key: [u8; 32]) -> Result<(), PeersError> {
    let _guard = crate::store::lock();
    validate_name(new_name)?;
    validate_address(&address)?;
    let mut all = load(path)?;
    let old_key = find(&all, who).map_err(PeersError::from)?.public_key;
    if let Some(same) = all.iter().find(|p| p.public_key != old_key && p.public_key == public_key) {
        return Err(PeersError::Parse { line: 0, message: format!("that key is already your contact '{}'", same.name) });
    }
    let peer = all.iter_mut().find(|p| p.public_key == old_key).expect("found above");
    peer.name = new_name.to_string();
    peer.address = address;
    peer.public_key = public_key;
    write_all(path, &all)
}

/// Sets (or clears, with `None`) the cover-mode rate of the peer `who`
/// (see `find`), leaving the rest of their line alone: changing the rate
/// has nothing to do with changing how to reach someone. Errors if
/// there's no such peer, as `update` does.
pub fn set_cover(path: &Path, who: &str, cover_bits_per_second: Option<u64>, required: bool) -> Result<(), PeersError> {
    let _guard = crate::store::lock();
    let mut all = load(path)?;
    let key = find(&all, who).map_err(PeersError::from)?.public_key;
    let peer = all.iter_mut().find(|p| p.public_key == key).expect("found above");
    peer.cover_bits_per_second = cover_bits_per_second;
    peer.cover_required = required && cover_bits_per_second.is_some();
    write_all(path, &all)
}

/// Rewrites the whole file from `peers`, one line each. Used by
/// `remove`/`update`, which (unlike `add`) can't just append — note
/// this normalises away any blank lines or `#` comments that were in
/// the file before, same as re-saving from a parsed, in-memory list
/// always would.
fn write_all(path: &Path, peers: &[Peer]) -> Result<(), PeersError> {
    let mut contents = String::new();
    for p in peers {
        contents.push_str(&format_line(p));
        contents.push('\n');
    }
    crate::store::write_atomic(path, contents)?;
    Ok(())
}

/// One peer's line, in the format `parse` reads back — the 4th
/// (cover-rate) field is omitted when there isn't one.
pub fn format_line(peer: &Peer) -> String {
    match (peer.cover_bits_per_second, peer.cover_required) {
        (Some(rate), true) => format!("{} {} {} {rate} require", peer.name, peer.address, STANDARD.encode(peer.public_key)),
        (Some(rate), false) => format!("{} {} {} {rate}", peer.name, peer.address, STANDARD.encode(peer.public_key)),
        (None, _) => format!("{} {} {}", peer.name, peer.address, STANDARD.encode(peer.public_key)),
    }
}

fn decode_public_key(s: &str) -> Result<[u8; 32], ()> {
    let bytes = STANDARD.decode(s).map_err(|_| ())?;
    bytes.try_into().map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A contact's name can't carry an escape a terminal would act on, or
    /// a direction character turning it into another name.
    #[test]
    fn a_name_with_control_or_direction_characters_is_refused() {
        assert!(validate_name("Ann").is_ok());
        assert!(validate_name("Zoë-Ł").is_ok());
        for wrong in ["evil\u{1b}[2J", "Ann\u{202e}nimda", "\u{2066}Bob", "a\u{7}b"] {
            assert!(validate_name(wrong).is_err(), "{wrong:?}");
        }
    }

    /// A lookup that answers `ip` after `delay`, or fails.
    fn fake(ip: Option<&'static str>, delay: u64) -> impl Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = NameLookup> + Send>> + Clone + Send + 'static {
        move |_item| {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                Ok(match ip {
                    Some(ip) => Ok(vec![std::net::SocketAddr::new(ip.parse().unwrap(), 1977)]),
                    None => Err(std::io::Error::other("no DNS")),
                })
            })
        }
    }

    #[tokio::test]
    async fn ips_beside_a_host_name_are_never_kept_waiting_and_its_answer_comes_next_time() {
        let started = std::time::Instant::now();
        let got = resolve_with("192.0.2.1,slow-one.example", fake(Some("198.51.100.7"), 300)).await.unwrap();
        assert_eq!(got, ["192.0.2.1:1977".parse().unwrap()]);
        assert!(started.elapsed() < std::time::Duration::from_millis(200), "not waited for");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let got = resolve_with("192.0.2.1,slow-one.example", fake(None, 0)).await.unwrap();
        assert_eq!(got, ["192.0.2.1:1977".parse().unwrap(), "198.51.100.7:1977".parse().unwrap()], "looked up meanwhile, kept");
    }

    #[tokio::test]
    async fn a_host_name_alone_is_waited_for_and_its_last_answer_outlives_dns() {
        let got = resolve_with("only-name.example", fake(Some("203.0.113.9"), 10)).await.unwrap();
        assert_eq!(got, ["203.0.113.9:1977".parse().unwrap()]);
        let got = resolve_with("only-name.example", fake(None, 0)).await.unwrap();
        assert_eq!(got, ["203.0.113.9:1977".parse().unwrap()]);
        assert!(resolve_with("never-answered.example", fake(None, 0)).await.is_err(), "nothing known and nothing found");
    }

    #[test]
    fn parses_peers() {
        let key = STANDARD.encode([7u8; 32]);
        let input = format!("# comment\n\nbob 2001:db8::1 {key}\n");
        let peers = parse(&input).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "bob");
        assert_eq!(peers[0].public_key, [7u8; 32]);
    }

    /// A peers file edited by hand often ends without a newline; adding
    /// a peer then mustn't run its line into the last one (which made
    /// the whole file unreadable, so every peer seemed gone).
    #[test]
    fn adding_to_a_file_without_a_final_newline_keeps_both_lines() {
        let dir = std::env::temp_dir().join(format!("swarm-peers-newline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers");
        std::fs::write(&path, crate::store::sealed_line(&path, &format!("bob 2001:db8::1 {}", STANDARD.encode([7u8; 32])))).unwrap();
        let ann = Peer { name: "ann".into(), address: "2001:db8::2".into(), public_key: [8; 32], cover_bits_per_second: None, cover_required: false };
        add(&path, &ann).unwrap();
        let names: Vec<String> = load(&path).unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["bob", "ann"]);
    }

    #[test]
    fn rejects_malformed_line() {
        assert!(parse("onlyname address\n").is_err());
    }

    #[test]
    fn two_can_share_a_name_and_are_told_apart_by_key() {
        let dir = std::env::temp_dir().join(format!("swarm-peers-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers");
        let bob = |k: u8| Peer { name: "bob".into(), address: "2001:db8::2".into(), public_key: [k; 32], cover_bits_per_second: None, cover_required: false };
        add(&path, &bob(1)).unwrap();
        add(&path, &bob(2)).unwrap();
        let all = load(&path).unwrap();
        assert_eq!(all.len(), 2);
        assert!(matches!(find(&all, "bob"), Err(Lookup::Several(..))), "never a guess");
        assert_eq!(find(&all, &bob(2).handle()).unwrap().public_key, [2; 32]);
        let told = format!("bob~{}", &bob(2).handle()[..6]);
        assert_eq!(find(&all, &told).unwrap().public_key, [2; 32]);
        assert!(remove(&path, "bob").is_err(), "which one?");
        assert!(remove(&path, &bob(1).handle()).unwrap());
        let left = load(&path).unwrap();
        assert_eq!(left, vec![bob(2)]);
        assert_eq!(find(&left, "bob").unwrap().public_key, [2; 32], "only one now");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_appends_and_rejects_duplicate_names() {
        let dir = std::env::temp_dir().join(format!("swarm-peers-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers");
        let peer = Peer {
            name: "alice".into(),
            address: "2001:db8::2".into(),
            public_key: [1u8; 32],
            cover_bits_per_second: None,
            cover_required: false,
        };
        add(&path, &peer).unwrap();
        assert_eq!(load(&path).unwrap(), vec![peer.clone()]);
        assert!(add(&path, &peer).is_err());
        let again = Peer { name: "alice2".into(), ..peer.clone() };
        assert!(add(&path, &again).is_err());
        let bob = Peer { name: "bob".into(), public_key: [2u8; 32], ..peer.clone() };
        add(&path, &bob).unwrap();
        assert!(update(&path, "bob", "bob", bob.address.clone(), [1u8; 32]).is_err());
        assert_eq!(load(&path).unwrap(), vec![peer.clone(), bob]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn update_replaces_in_place_and_errors_on_unknown_name() {
        let dir = std::env::temp_dir().join(format!("swarm-peers-test-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers");
        add(&path, &Peer { name: "alice".into(), address: "2001:db8::2".into(), public_key: [1u8; 32], cover_bits_per_second: None, cover_required: false }).unwrap();
        add(&path, &Peer { name: "bob".into(), address: "2001:db8::3".into(), public_key: [2u8; 32], cover_bits_per_second: None, cover_required: false }).unwrap();

        update(&path, "alice", "alice", "2001:db8::9".into(), [9u8; 32]).unwrap();
        let all = load(&path).unwrap();
        assert_eq!(all[0].name, "alice");
        assert_eq!(all[0].address, "2001:db8::9");
        assert_eq!(all[0].public_key, [9u8; 32]);
        assert_eq!(all[1], Peer { name: "bob".into(), address: "2001:db8::3".into(), public_key: [2u8; 32], cover_bits_per_second: None, cover_required: false });

        assert!(update(&path, "nobody", "nobody", "2001:db8::9".into(), [9u8; 32]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn update_can_rename_keeping_everything_else_and_refuses_clashes() {
        let dir = std::env::temp_dir().join(format!("swarm-peers-test-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers");
        add(&path, &Peer { name: "alice".into(), address: "2001:db8::2".into(), public_key: [1u8; 32], cover_bits_per_second: None, cover_required: false }).unwrap();
        add(&path, &Peer { name: "bob".into(), address: "2001:db8::3".into(), public_key: [2u8; 32], cover_bits_per_second: None, cover_required: false }).unwrap();
        set_cover(&path, "alice", Some(64_000), false).unwrap();

        update(&path, "alice", "carol", "2001:db8::2".into(), [1u8; 32]).unwrap();
        let all = load(&path).unwrap();
        assert_eq!(all[0], Peer { name: "carol".into(), address: "2001:db8::2".into(), public_key: [1u8; 32], cover_bits_per_second: Some(64_000), cover_required: false });

        assert!(update(&path, "carol", "has space", "2001:db8::2".into(), [1u8; 32]).is_err());
        assert!(add(&path, &Peer { name: "".into(), address: "x".into(), public_key: [3u8; 32], cover_bits_per_second: None, cover_required: false }).is_err());
        assert_eq!(load(&path).unwrap()[0].name, "carol", "a refused rename leaves the file alone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_drops_the_named_peer_and_reports_when_theres_none() {
        let dir = std::env::temp_dir().join(format!("swarm-peers-test-remove-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers");
        add(&path, &Peer { name: "alice".into(), address: "2001:db8::2".into(), public_key: [1u8; 32], cover_bits_per_second: None, cover_required: false }).unwrap();
        add(&path, &Peer { name: "bob".into(), address: "2001:db8::3".into(), public_key: [2u8; 32], cover_bits_per_second: None, cover_required: false }).unwrap();

        assert!(remove(&path, "alice").unwrap());
        assert_eq!(load(&path).unwrap().len(), 1);
        assert_eq!(load(&path).unwrap()[0].name, "bob");
        assert!(!remove(&path, "alice").unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_cover_round_trips_without_disturbing_address_or_pubkey() {
        let dir = std::env::temp_dir().join(format!("swarm-peers-test-cover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("peers");
        add(&path, &Peer { name: "alice".into(), address: "2001:db8::2".into(), public_key: [1u8; 32], cover_bits_per_second: None, cover_required: false }).unwrap();
        add(&path, &Peer { name: "bob".into(), address: "2001:db8::3".into(), public_key: [2u8; 32], cover_bits_per_second: None, cover_required: false }).unwrap();

        set_cover(&path, "alice", Some(64_000), false).unwrap();
        let all = load(&path).unwrap();
        assert_eq!(all[0].address, "2001:db8::2");
        assert_eq!(all[0].public_key, [1u8; 32]);
        assert_eq!(all[0].cover_bits_per_second, Some(64_000));
        assert_eq!(all[1].cover_bits_per_second, None);

        set_cover(&path, "alice", None, false).unwrap();
        assert_eq!(load(&path).unwrap()[0].cover_bits_per_second, None);

        assert!(set_cover(&path, "nobody", Some(1), false).is_err());

        set_cover(&path, "bob", Some(32_000), true).unwrap();
        assert!(crate::store::read(&path).unwrap().contains(" 32000 require"));
        let bob = load(&path).unwrap().into_iter().find(|p| p.name == "bob").unwrap();
        assert_eq!((bob.cover_bits_per_second, bob.cover_required), (Some(32_000), true));
        set_cover(&path, "bob", None, true).unwrap();
        let bob = load(&path).unwrap().into_iter().find(|p| p.name == "bob").unwrap();
        assert_eq!((bob.cover_bits_per_second, bob.cover_required), (None, false));
        assert!(parse(&format!("x ::1 {} 1000 bogus", STANDARD.encode([1u8; 32]))).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_address_can_be_a_list() {
        let addrs = resolve_address("2001:db8::1, 192.168.1.20,[2001:db8::2]:4000,10.0.0.1:5000").await.unwrap();
        assert_eq!(literal_addresses("2001:db8::1, 192.168.1.20,[2001:db8::2]:4000,10.0.0.1:5000,example.org"), addrs);
        assert!(resolve_address("localhost").await.unwrap().iter().all(|a| a.ip().is_loopback() && a.port() == crate::transport::DEFAULT_PORT));
        let named = resolve_address("localhost:4000").await.unwrap();
        assert!(!named.is_empty() && named.iter().all(|a| a.ip().is_loopback() && a.port() == 4000), "{named:?}");
        let expect: Vec<std::net::SocketAddr> =
            ["[2001:db8::1]:1977", "192.168.1.20:1977", "[2001:db8::2]:4000", "10.0.0.1:5000"].iter().map(|a| a.parse().unwrap()).collect();
        assert_eq!(addrs, expect);
        assert!(validate_address("2001:db8::1,192.168.1.20").is_ok());
        assert!(validate_address(",").is_err());
    }

    #[test]
    fn an_ipv4_connection_is_written_as_plain_ipv4() {
        assert_eq!(address_for("[::ffff:192.168.1.20]:1977".parse().unwrap()), "192.168.1.20");
        assert_eq!(address_for("[::ffff:192.168.1.20]:4000".parse().unwrap()), "192.168.1.20:4000");
        assert_eq!(address_for("[2001:db8::1]:1977".parse().unwrap()), "2001:db8::1");
    }

}
