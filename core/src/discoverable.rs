//! Whether the user is discoverable on the network a device is on: each
//! of their devices saying so over mDNS (`swarm_app::discovery`), with
//! their name, user key and that device's addresses there, for anyone on
//! the same network to find under Add a contact. On unless turned off.
//!
//! It's the user's, not one device's: kept the same on all of their
//! devices (`node::devices` sends it over their links, `Notice::
//! Discoverable`; the later change wins), as the address set by hand is
//! (`myaddress`).
//!
//! One line in `~/.swarm/discoverable`: `<unix secs>\t<on|off>`, the time
//! it was last set (no file, or `0`: never set, so on).

use std::path::Path;

/// Whether the user is discoverable, and when that was set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Discoverable {
    /// Unix seconds; 0: never set.
    pub at: u64,
    pub on: bool,
}

impl Default for Discoverable {
    /// Never set: on (it's for finding each other on one network).
    fn default() -> Self {
        Discoverable { at: 0, on: true }
    }
}

/// What's kept (never set: the default, on).
pub fn load(path: &Path) -> Discoverable {
    let text = crate::store::read(path).unwrap_or_default();
    let line = text.lines().next().unwrap_or_default();
    let Some((at, on)) = line.split_once('\t') else { return Discoverable::default() };
    match (at.trim().parse(), on.trim()) {
        (Ok(at), "on") => Discoverable { at, on: true },
        (Ok(at), "off") => Discoverable { at, on: false },
        _ => Discoverable::default(),
    }
}

/// Sets it, as now: later than what it replaces, though in the same
/// second. Returns what's kept.
pub fn set(path: &Path, on: bool, now: u64) -> std::io::Result<Discoverable> {
    let before = load(path);
    let kept = Discoverable { at: now.max(before.at + 1), on };
    save(path, kept)?;
    Ok(kept)
}

/// Takes `theirs` (another of the user's devices') if it's newer than
/// what's kept. Whether it was.
pub fn take(path: &Path, theirs: Discoverable) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    if theirs.at <= load(path).at {
        return Ok(false);
    }
    save(path, theirs)?;
    Ok(true)
}

fn save(path: &Path, kept: Discoverable) -> std::io::Result<()> {
    crate::store::write_atomic(path, format!("{}\t{}\n", kept.at, if kept.on { "on" } else { "off" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_until_turned_off_and_the_newer_is_taken() {
        let dir = std::env::temp_dir().join(format!("swarm-discoverable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("discoverable");
        assert_eq!(load(&path), Discoverable { at: 0, on: true }, "never set: on");
        let off = set(&path, false, 100).unwrap();
        assert!(!off.on && off.at == 100);
        let on = set(&path, true, 100).unwrap();
        assert!(on.on && on.at > off.at, "the same second is still later");
        assert_eq!(load(&path), on);
        assert!(!take(&path, off).unwrap(), "older isn't taken");
        let newer = Discoverable { at: on.at + 5, on: false };
        assert!(take(&path, newer).unwrap());
        assert_eq!(load(&path), newer);
        std::fs::write(&path, "nonsense\n").unwrap();
        assert_eq!(load(&path), Discoverable::default(), "unreadable: as never set");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
