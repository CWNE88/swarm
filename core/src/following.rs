//! Following: reading someone's public posts without being their peer.
//! Nothing else goes to them — no messages, calls or files — and on
//! their side it isn't a contact request, only counted (see
//! `followers`).
//!
//! One flat file (`~/.swarm/following`), one line per person:
//! `<base64_pubkey>\t<address>`, in the order they were followed.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Followed {
    pub public_key: [u8; 32],
    /// Where to fetch from, in the peers file's address form.
    pub address: String,
}

fn parse_line(line: &str) -> Option<Followed> {
    let (key, address) = line.split_once('\t')?;
    let public_key = STANDARD.decode(key).ok()?.try_into().ok()?;
    Some(Followed { public_key, address: address.trim().to_string() })
}

fn format_line(f: &Followed) -> String {
    format!("{}\t{}\n", STANDARD.encode(f.public_key), f.address)
}

/// Everyone followed (none if there's no file yet).
pub fn load(path: &Path) -> std::io::Result<Vec<Followed>> {
    match crate::store::read(path) {
        Ok(text) => Ok(text.lines().filter_map(parse_line).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn save(path: &Path, all: &[Followed]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::store::write_atomic(path, all.iter().map(format_line).collect::<String>())
}

/// Follows `public_key` at `address`; already followed, the address is
/// brought up to date. The address is checked as a peer's is.
pub fn add(path: &Path, public_key: [u8; 32], address: &str) -> anyhow::Result<()> {
    crate::peers::validate_address(address).map_err(|e| anyhow::anyhow!("{e}"))?;
    let _guard = crate::store::lock();
    let mut all = load(path)?;
    match all.iter_mut().find(|f| f.public_key == public_key) {
        Some(f) => f.address = address.to_string(),
        None => all.push(Followed { public_key, address: address.to_string() }),
    }
    Ok(save(path, &all)?)
}

/// Stops following `public_key`. Whether they were followed.
pub fn remove(path: &Path, public_key: &[u8; 32]) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    let all = load(path)?;
    let rest: Vec<Followed> = all.iter().filter(|f| &f.public_key != public_key).cloned().collect();
    if rest.len() == all.len() {
        return Ok(false);
    }
    save(path, &rest)?;
    Ok(true)
}

pub fn is_followed(path: &Path, public_key: &[u8; 32]) -> bool {
    load(path).unwrap_or_default().iter().any(|f| &f.public_key == public_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_update_remove() {
        let dir = std::env::temp_dir().join(format!("swarm-following-test-{}", std::process::id()));
        let path = dir.join("following");
        add(&path, [1; 32], "2001:db8::1").unwrap();
        add(&path, [2; 32], "2001:db8::2").unwrap();
        add(&path, [1; 32], "2001:db8::9").unwrap();
        assert_eq!(load(&path).unwrap(), vec![Followed { public_key: [1; 32], address: "2001:db8::9".into() }, Followed { public_key: [2; 32], address: "2001:db8::2".into() }]);
        assert!(add(&path, [3; 32], "has space").is_err());
        assert!(remove(&path, &[1; 32]).unwrap());
        assert!(!remove(&path, &[1; 32]).unwrap());
        assert!(!is_followed(&path, &[1; 32]) && is_followed(&path, &[2; 32]));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
