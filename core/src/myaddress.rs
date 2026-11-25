//! The address the user sets by hand ("Manually set IP address", under
//! You): where they can be reached that no device of theirs can see for
//! itself, typically their home router's public IPv4 address with a port
//! forwarded to one of their devices, or a dynamic-DNS name for it.
//!
//! It's the user's, not one device's: kept the same on all of their
//! devices (`node::devices` sends it over their links, `Notice::
//! ManualAddress`; the later change wins), put first in their contact
//! code wherever they show it (`swarm_app::contact`), and its IPs told
//! to their contacts with the addresses each device finds for itself.
//! A host name goes in the code only: what contacts are told is kept as
//! literal IPs, never a name to look up (`addressbook`).
//!
//! One line in `~/.swarm/manual-address`: `<unix secs>\t<item>,<item>`,
//! the time it was last set (`0` and nothing: never set).

use std::path::Path;

/// The most items it holds: one or two in practice; the code that
/// carries them has room for six addresses in all.
pub const MOST_ITEMS: usize = 4;
/// The longest host name (as DNS has it).
const MOST_NAME_CHARS: usize = 253;

/// The manually set address, and when it was set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manual {
    /// Unix seconds; 0: never set.
    pub at: u64,
    /// Each an IP, `ip:port` (`[ip]:port` for IPv6) or a host name
    /// (`name:port` too), as typed; empty: none.
    pub items: Vec<String>,
}

impl Manual {
    /// As typed into an address field: comma-separated.
    pub fn text(&self) -> String {
        self.items.join(",")
    }

    /// Its items written as IPs, in the form contacts keep (`addressbook::
    /// checked`): what can be told to them. Host names left out.
    pub fn literal(&self) -> Vec<String> {
        self.items.iter().filter_map(|item| crate::addressbook::checked(item)).collect()
    }
}

/// `text` (comma-separated) as the items to keep, or what's wrong with
/// it. Empty: none.
pub fn check(text: &str) -> Result<Vec<String>, String> {
    let mut items: Vec<String> = Vec::new();
    for item in crate::peers::address_items(text) {
        if !is_address(item) {
            return Err(format!("\"{item}\" isn't an address: give an IP (203.0.113.7), one with a port (203.0.113.7:5000, [2001:db8::1]:5000) or a host name"));
        }
        if !items.iter().any(|i| i == item) {
            items.push(item.to_string());
        }
    }
    if items.len() > MOST_ITEMS {
        return Err(format!("at most {MOST_ITEMS} addresses"));
    }
    Ok(items)
}

/// An IP, an IP and port, or a host name with or without a port.
fn is_address(item: &str) -> bool {
    if let Ok(addr) = item.parse::<std::net::SocketAddr>() {
        return addr.port() != 0;
    }
    if item.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let (name, port) = match item.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (item, None),
    };
    if port.is_some_and(|p| p.parse::<u16>().map_or(true, |p| p == 0)) {
        return false;
    }
    !name.is_empty()
        && name.len() <= MOST_NAME_CHARS
        && name.chars().any(|c| c.is_ascii_alphabetic())
        && name.trim_end_matches('.').split('.').all(|label| {
            !label.is_empty() && label.len() <= 63 && !label.starts_with('-') && !label.ends_with('-') && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// What's kept (never set: the default).
pub fn load(path: &Path) -> Manual {
    let text = crate::store::read(path).unwrap_or_default();
    let line = text.lines().next().unwrap_or_default();
    let (at, list) = line.split_once('\t').unwrap_or((line, ""));
    let items = check(list).unwrap_or_default();
    Manual { at: at.trim().parse().unwrap_or(0), items }
}

/// Sets it to `items` (already `check`ed), as now: later than what it
/// replaces, though in the same second. Returns what's kept.
pub fn set(path: &Path, items: Vec<String>, now: u64) -> std::io::Result<Manual> {
    let before = load(path);
    let manual = Manual { at: now.max(before.at + 1), items };
    save(path, &manual)?;
    Ok(manual)
}

/// Takes `theirs` (another of the user's devices') if it's newer than
/// what's kept. Whether it was.
pub fn take(path: &Path, theirs: &Manual) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    if theirs.at <= load(path).at {
        return Ok(false);
    }
    let items = theirs.items.iter().filter(|i| is_address(i)).take(MOST_ITEMS).cloned().collect();
    save(path, &Manual { at: theirs.at, items })?;
    Ok(true)
}

fn save(path: &Path, manual: &Manual) -> std::io::Result<()> {
    crate::store::write_atomic(path, format!("{}\t{}\n", manual.at, manual.text()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ips_ports_and_names_are_addresses_and_little_else() {
        let good = ["203.0.113.7", "203.0.113.7:5000", "2001:db8::1", "[2001:db8::1]:5000", "home.example.org", "home.example.org:5000", "my-router.dyndns.net."];
        for item in good {
            assert!(is_address(item), "{item}");
        }
        let bad = ["", "203.0.113.7:0", "203.0.113.7:99999", "300.1.2.3", "home example", "-home.org", "home..org", "a_b.org", "name:port", "1.2.3", "http://x.org"];
        for item in bad {
            assert!(!is_address(item), "{item}");
        }
    }

    #[test]
    fn a_list_is_checked_whole() {
        assert_eq!(check(" 203.0.113.7 , home.example.org:5000 ").unwrap(), ["203.0.113.7", "home.example.org:5000"]);
        assert_eq!(check("").unwrap(), Vec::<String>::new());
        assert_eq!(check("1.1.1.1,1.1.1.1").unwrap(), ["1.1.1.1"], "the same one once");
        assert!(check("1.1.1.1,not an address").is_err());
        assert!(check("1.1.1.1,1.1.1.2,1.1.1.3,1.1.1.4,1.1.1.5").is_err());
    }

    #[test]
    fn only_ips_are_told() {
        let manual = Manual { at: 1, items: vec!["203.0.113.7".into(), "home.example.org".into(), "[2001:db8::1]:5000".into(), "198.51.100.2:1977".into()] };
        assert_eq!(manual.literal(), ["203.0.113.7", "[2001:db8::1]:5000", "198.51.100.2"]);
    }

    #[test]
    fn set_is_always_later_and_the_newer_is_taken() {
        let dir = std::env::temp_dir().join(format!("swarm-myaddress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manual-address");
        assert_eq!(load(&path), Manual::default());
        let first = set(&path, vec!["203.0.113.7".into()], 100).unwrap();
        let second = set(&path, vec!["home.example.org".into()], 100).unwrap();
        assert!(second.at > first.at, "the same second is still later");
        assert_eq!(load(&path), second);
        assert!(!take(&path, &first).unwrap(), "older isn't taken");
        let newer = Manual { at: second.at + 5, items: vec!["198.51.100.2".into(), "not an address".into()] };
        assert!(take(&path, &newer).unwrap());
        assert_eq!(load(&path).items, ["198.51.100.2"], "anything that isn't an address is left out");
        let cleared = set(&path, Vec::new(), 0).unwrap();
        assert!(cleared.items.is_empty() && cleared.at > newer.at);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
