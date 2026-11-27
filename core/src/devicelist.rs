//! A user's devices: which device keys are
//! theirs, each with its name, kept by every one of them and never sent
//! to peers.
//!
//! Every device holds the user key, so any of them can sign an entry,
//! and entries are signed by nothing else. The list is the set of
//! entries: copies merge by putting them together, so two devices that
//! changed it apart come to the same list once they meet. A device is in
//! once it's added and until it's removed; removal is for good (a device
//! that comes back makes a new key). Its name, and whether it relays, are
//! the latest entry's that says.

use crate::identity::{self, Identity};
use crate::store;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::collections::BTreeMap;
use std::path::Path;

/// The most devices one user can have.
pub const MAX_DEVICES: usize = 8;
/// The longest device name, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Whether `name` can be a device's name: 1 to `MAX_NAME_CHARS`
/// characters on one line, with no tabs.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("a device name has to be 1 to {MAX_NAME_CHARS} characters"));
    }
    if !crate::text::is_single_line(name) || name.contains('\t') {
        return Err("a device name has to be one line".to_string());
    }
    Ok(())
}

/// A name for this device to start from: its host name, as the system
/// has it.
pub fn default_name() -> String {
    let from_system = host_name();
    let name: String = from_system.unwrap_or_default().trim().chars().take(MAX_NAME_CHARS).collect();
    if validate_name(&name).is_ok() { name } else { "this device".to_string() }
}

#[cfg(target_os = "macos")]
fn host_name() -> Option<String> {
    let mut name: [libc::c_char; 256] = [0; 256];
    if unsafe { libc::gethostname(name.as_mut_ptr(), name.len()) } != 0 {
        return None;
    }
    let bytes = name.map(|byte| byte as u8);
    Some(std::ffi::CStr::from_bytes_until_nul(&bytes).ok()?.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn host_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn host_name() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname").or_else(|_| std::fs::read_to_string("/etc/hostname")).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A device of the user's now, called `name`.
    Added { name: String },
    /// Called `name` from now on.
    Renamed { name: String },
    /// May relay for the user's other devices (or not, any more).
    Relay { allowed: bool },
    /// Not one of the user's devices any more, for good.
    Removed,
}

impl Change {
    fn tag(&self) -> &'static str {
        match self {
            Change::Added { .. } => "added",
            Change::Renamed { .. } => "renamed",
            Change::Relay { .. } => "relay",
            Change::Removed => "removed",
        }
    }

    fn argument(&self) -> String {
        match self {
            Change::Added { name } | Change::Renamed { name } => name.clone(),
            Change::Relay { allowed } => (if *allowed { "yes" } else { "no" }).to_string(),
            Change::Removed => String::new(),
        }
    }
}

/// One entry in the list, signed with the user key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// When it was signed: which of two names is the later. Never used
    /// for whether a device is in.
    pub at: u64,
    pub device: [u8; 32],
    pub change: Change,
    pub signature: [u8; 64],
}

/// What the user key signs. Starts with something no other signed thing
/// does, so nothing else can be passed off as an entry.
fn signable(user: &[u8; 32], at: u64, device: &[u8; 32], change: &Change) -> Vec<u8> {
    format!("swarm:device-list\t{}\t{at}\t{}\t{}\t{}", STANDARD.encode(user), STANDARD.encode(device), change.tag(), change.argument()).into_bytes()
}

/// Whether `entry` is well formed and signed by `user`.
///
/// The list's read and checked every second or two, entry by entry: the
/// last that checked out are remembered (`VERIFIED`) and not checked
/// again, which on a phone with nothing to do was a tenth of its time.
pub fn verify(user: &[u8; 32], entry: &Entry) -> bool {
    use crate::lock::Locked;
    if VERIFIED.locked().iter().any(|(u, e)| u == user && e == entry) {
        return true;
    }
    let fields_ok = match &entry.change {
        Change::Added { name } | Change::Renamed { name } => validate_name(name).is_ok(),
        Change::Relay { .. } | Change::Removed => true,
    };
    let ok = fields_ok && identity::verify(user, &signable(user, entry.at, &entry.device, &entry.change), &entry.signature);
    if ok {
        let mut verified = VERIFIED.locked();
        if verified.len() >= VERIFIED_MOST {
            verified.remove(0);
        }
        verified.push((*user, entry.clone()));
    }
    ok
}

/// Entries that checked out (see `verify`), the oldest first.
static VERIFIED: std::sync::Mutex<Vec<([u8; 32], Entry)>> = std::sync::Mutex::new(Vec::new());
/// Enough for every entry of a few users' lists.
const VERIFIED_MOST: usize = 256;

/// A device as the list has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    /// When it was added.
    pub added_at: u64,
    /// It may relay for the user's other devices.
    pub relay: bool,
}

/// A user's device list: their key and every entry this device has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    pub user: [u8; 32],
    pub entries: Vec<Entry>,
}

impl List {
    pub fn new(user: [u8; 32]) -> List {
        List { user, entries: Vec::new() }
    }

    /// Signs `change` to `device` with the user key, and adds it.
    pub fn sign(&mut self, user: &Identity, device: [u8; 32], change: Change) -> Entry {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let latest = self.entries.iter().filter(|e| e.device == device).map(|e| e.at).max().unwrap_or(0);
        let at = now.max(latest.saturating_add(1));
        let signature = user.sign(&signable(&self.user, at, &device, &change));
        let entry = Entry { at, device, change, signature };
        self.entries.push(entry.clone());
        entry
    }

    /// The user's devices now.
    pub fn devices(&self) -> BTreeMap<[u8; 32], Device> {
        let mut ordered: Vec<&Entry> = self.entries.iter().filter(|e| verify(&self.user, e)).collect();
        ordered.sort_by_key(|e| (e.at, e.signature));
        let removed: Vec<[u8; 32]> = ordered.iter().filter(|e| e.change == Change::Removed).map(|e| e.device).collect();
        let mut devices: BTreeMap<[u8; 32], Device> = BTreeMap::new();
        for entry in ordered.into_iter().filter(|e| !removed.contains(&e.device)) {
            match &entry.change {
                Change::Added { name } => {
                    let device = devices.entry(entry.device).or_insert(Device { name: String::new(), added_at: entry.at, relay: false });
                    device.name = name.clone();
                }
                Change::Renamed { name } => {
                    if let Some(device) = devices.get_mut(&entry.device) {
                        device.name = name.clone();
                    }
                }
                Change::Relay { allowed } => {
                    if let Some(device) = devices.get_mut(&entry.device) {
                        device.relay = *allowed;
                    }
                }
                Change::Removed => {}
            }
        }
        devices
    }

    /// Whether `device` was ever taken off the list.
    pub fn removed(&self, device: &[u8; 32]) -> bool {
        self.entries.iter().any(|e| e.device == *device && e.change == Change::Removed && verify(&self.user, e))
    }

    /// Drops entries that change nothing now: for a device removed, all
    /// but one removal; for the rest, all but its first addition and the
    /// latest that names it or sets its relay. Copies that differ in what
    /// they've dropped still merge to the same list.
    pub fn compact(&mut self) {
        let user = self.user;
        let mut valid: Vec<Entry> = self.entries.drain(..).filter(|e| verify(&user, e)).collect();
        valid.sort_by_key(|e| (e.at, e.signature));
        let mut keep: Vec<Entry> = Vec::new();
        let devices: std::collections::BTreeSet<[u8; 32]> = valid.iter().map(|e| e.device).collect();
        for device in devices {
            let its: Vec<&Entry> = valid.iter().filter(|e| e.device == device).collect();
            if let Some(removal) = its.iter().find(|e| e.change == Change::Removed) {
                keep.push((*removal).clone());
                continue;
            }
            let first_added = its.iter().find(|e| matches!(e.change, Change::Added { .. }));
            let named = its.iter().rev().find(|e| matches!(e.change, Change::Added { .. } | Change::Renamed { .. }));
            let relay = its.iter().rev().find(|e| matches!(e.change, Change::Relay { .. }));
            for e in [first_added, named, relay].into_iter().flatten() {
                if !keep.contains(e) {
                    keep.push((*e).clone());
                }
            }
        }
        self.entries = keep;
    }

    /// Adds the entries of `other` that verify and aren't here yet;
    /// returns the ones that were new.
    pub fn merge(&mut self, other: &[Entry]) -> Vec<Entry> {
        let mut new = Vec::new();
        for entry in other {
            if verify(&self.user, entry) && !self.entries.contains(entry) {
                self.entries.push(entry.clone());
                new.push(entry.clone());
            }
        }
        new
    }
}

/// The list as text: `user <key>` on the first line, then one entry per
/// line — as on file, and as it's handed to a device being added.
pub fn to_text(list: &List) -> String {
    let mut text = format!("user {}\n", STANDARD.encode(list.user));
    for e in &list.entries {
        text.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", e.at, STANDARD.encode(e.device), e.change.tag(), e.change.argument(), STANDARD.encode(e.signature)));
    }
    text
}

/// A list from `to_text`'s text. Lines that don't parse are left out
/// (whether the rest verify is for `List::devices`).
pub fn from_text(text: &str) -> Option<List> {
    let mut lines = text.lines();
    let user: [u8; 32] = STANDARD.decode(lines.next()?.strip_prefix("user ")?.trim()).ok()?.try_into().ok()?;
    let entries = lines
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [at, device, tag, argument, signature] = f[..] else { return None };
            let change = match tag {
                "added" => Change::Added { name: argument.to_string() },
                "renamed" => Change::Renamed { name: argument.to_string() },
                "relay" => Change::Relay { allowed: argument == "yes" },
                "removed" => Change::Removed,
                _ => return None,
            };
            Some(Entry { at: at.parse().ok()?, device: STANDARD.decode(device).ok()?.try_into().ok()?, change, signature: STANDARD.decode(signature).ok()?.try_into().ok()? })
        })
        .collect();
    Some(List { user, entries })
}

/// `from_text`, the same text not read twice: the list's loaded every
/// second or two, and reading it each time was a tenth of a phone's time
/// with nothing to do.
fn parsed(text: &str) -> Option<List> {
    use crate::lock::Locked;
    static LAST: std::sync::Mutex<Vec<(String, List)>> = std::sync::Mutex::new(Vec::new());
    if let Some((_, list)) = LAST.locked().iter().find(|(t, _)| t == text) {
        return Some(list.clone());
    }
    let list = from_text(text)?;
    let mut last = LAST.locked();
    if last.len() >= 8 {
        last.remove(0);
    }
    last.push((text.to_string(), list.clone()));
    Some(list)
}

/// The list at `path`; none there yet, an empty one for `user`. One of
/// another user's is never read as `user`'s.
pub fn load(path: &Path, user: [u8; 32]) -> std::io::Result<List> {
    match store::read(path) {
        Ok(text) => match parsed(&text) {
            Some(list) if list.user == user => Ok(list),
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: not this user's device list", path.display()))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(List::new(user)),
        Err(e) => Err(e),
    }
}

/// Loads, changes and saves the list under the store lock; returns what
/// `change` returned.
pub fn update<T>(path: &Path, user: [u8; 32], change: impl FnOnce(&mut List) -> T) -> std::io::Result<T> {
    let _guard = store::lock();
    let mut list = load(path, user)?;
    let out = change(&mut list);
    list.compact();
    store::write_atomic(path, to_text(&list))?;
    Ok(out)
}

/// Merges `entries` (from another of the user's devices) into the list
/// at `path`; returns the ones that were new.
pub fn merge_file(path: &Path, user: [u8; 32], entries: &[Entry]) -> std::io::Result<Vec<Entry>> {
    update(path, user, |list| list.merge(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Change {
        Change::Added { name: name.to_string() }
    }

    #[test]
    fn devices_come_and_go_and_names_are_the_latest() {
        let owen = Identity::generate();
        let (desk, phone) = ([1; 32], [2; 32]);
        let mut list = List::new(owen.public_key_bytes());
        list.sign(&owen, desk, named("desk"));
        list.sign(&owen, phone, named("phone"));
        list.sign(&owen, phone, Change::Renamed { name: "pixel".into() });
        list.sign(&owen, desk, Change::Relay { allowed: true });
        let devices = list.devices();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[&phone].name, "pixel");
        assert!(devices[&desk].relay);
        list.sign(&owen, phone, Change::Removed);
        list.sign(&owen, phone, named("phone"));
        assert_eq!(list.devices().keys().copied().collect::<Vec<_>>(), vec![desk]);
        assert!(list.removed(&phone));
        assert_eq!(from_text(&to_text(&list)).unwrap(), list);
        let before = list.devices();
        list.compact();
        assert_eq!(list.devices(), before);
        assert_eq!(list.entries.len(), 3, "desk's addition and relay, phone's removal");
    }

    #[test]
    fn copies_changed_apart_merge_to_the_same_list() {
        let owen = Identity::generate();
        let mut a = List::new(owen.public_key_bytes());
        a.sign(&owen, [1; 32], named("desk"));
        let mut b = a.clone();
        a.sign(&owen, [2; 32], named("phone"));
        b.sign(&owen, [3; 32], named("laptop"));
        b.sign(&owen, [1; 32], Change::Removed);
        let (from_a, from_b) = (a.entries.clone(), b.entries.clone());
        a.merge(&from_b);
        b.merge(&from_a);
        assert_eq!(a.devices(), b.devices());
        assert_eq!(a.devices().len(), 2);
    }

    #[test]
    fn only_the_user_key_signs_entries() {
        let owen = Identity::generate();
        let stranger = Identity::generate();
        let mut list = List::new(owen.public_key_bytes());
        let mut theirs = List::new(owen.public_key_bytes());
        let forged = theirs.sign(&stranger, [9; 32], named("mine now"));
        assert!(list.merge(&[forged]).is_empty());
        let mut entry = list.sign(&owen, [1; 32], named("desk"));
        entry.change = named("evil");
        assert!(!verify(&list.user, &entry));
        assert!(list.devices()[&[1; 32]].name == "desk");
    }
}
