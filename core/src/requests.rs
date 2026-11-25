//! Contact requests: someone who has us on their peers list, but whom
//! we haven't added back. Until we do, they can't send us anything —
//! their side is told "not a contact" the moment it connects, before any
//! message leaves it (see `protocol::Frame::NotPeer`) — but they do
//! introduce themselves (`protocol::Frame::Introduce`), and that's
//! recorded here so we can decide: add them (the TUI's Add form comes
//! up pre-filled from this), dismiss them, or block them.
//!
//! One flat file (`~/.swarm/requests`), one line per key:
//! `<base64_pubkey>\t<status>\t<address>\t<first_seen>\t<last_seen>`,
//! status `pending`, `dismissed` or `blocked`. A blocked key is kept so
//! it never shows up again, and each time it dials it's told it was
//! removed (`protocol::Notice::Removed`), so it stops; it's the only
//! thing kept of someone blocked (see `forget`). Unblocking or adding
//! them (`forget`) undoes it. A contact deleted leaves nothing here.
//!
//! A user's devices share the file (`merge`, `node::ownrequests`): a
//! stranger who reached one of them is a request on all of them, and
//! what's done with it on one is done on all. So dismissing or
//! unblocking leaves a `dismissed` line, dated, and for each key the
//! line changed last wins (`last_seen` is when it last changed); once
//! every one of the user's devices has it, it goes (`prune`).

use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::Path;

/// The most pending requests kept at once. Anyone who knows our address
/// can introduce themselves, so this is a cap against a flood of made-up
/// keys; past it, the one heard from least recently is dropped — a real
/// contact keeps retrying (see `node::peer::PeerTask`),
/// which keeps their `last_seen` fresh.
pub const MAX_PENDING: usize = 50;

/// The most dismissed lines kept: only there till a dismissal reaches
/// the user's other devices (`prune`), and the oldest go first.
pub const MAX_DISMISSED: usize = 200;

/// Where a key stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Asking to be added.
    Pending,
    /// Dismissed, or let ask again: shown again if they try again.
    Dismissed,
    /// Never shown again, and told it's removed when it dials.
    Blocked,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Dismissed => "dismissed",
            Status::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub public_key: [u8; 32],
    pub status: Status,
    /// Where they last connected from, in the peers file's address form.
    pub address: String,
    pub first_seen: u64,
    pub last_seen: u64,
}

fn parse_line(line: &str) -> Option<Request> {
    let mut parts = line.splitn(5, '\t');
    let public_key: [u8; 32] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let status = match parts.next()? {
        "pending" => Status::Pending,
        "dismissed" => Status::Dismissed,
        "blocked" => Status::Blocked,
        _ => return None,
    };
    let address = parts.next()?.to_string();
    let first_seen = parts.next()?.parse().ok()?;
    let last_seen = parts.next()?.parse().ok()?;
    Some(Request { public_key, status, address, first_seen, last_seen })
}

fn load(path: &Path) -> std::io::Result<Vec<Request>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(crate::store::read(path)?.lines().filter_map(parse_line).collect())
}

fn save(path: &Path, all: &[Request]) -> std::io::Result<()> {
    let contents: String = all
        .iter()
        .map(|r| {
            format!(
                "{}\t{}\t{}\t{}\t{}\n",
                STANDARD.encode(r.public_key),
                r.status.word(),
                r.address,
                r.first_seen,
                r.last_seen
            )
        })
        .collect();
    crate::store::write_atomic(path, contents)
}

/// Every line on file, whatever its status: what the user's other
/// devices are given.
pub fn all(path: &Path) -> std::io::Result<Vec<Request>> {
    load(path)
}

/// Every pending (not dismissed or blocked) request, oldest first.
pub fn pending(path: &Path) -> std::io::Result<Vec<Request>> {
    let mut all: Vec<Request> = load(path)?.into_iter().filter(|r| r.status == Status::Pending).collect();
    all.sort_by_key(|r| r.first_seen);
    Ok(all)
}

/// Every blocked key, most recent first: what can be unblocked
/// (`forget`).
pub fn blocked(path: &Path) -> std::io::Result<Vec<Request>> {
    let mut all: Vec<Request> = load(path)?.into_iter().filter(|r| r.status == Status::Blocked).collect();
    all.sort_by_key(|r| std::cmp::Reverse(r.last_seen));
    Ok(all)
}

/// Whether `public_key` is blocked: told it was removed when it dials,
/// never shown as a request.
pub fn is_blocked(path: &Path, public_key: &[u8; 32]) -> std::io::Result<bool> {
    Ok(load(path)?.iter().any(|r| r.status == Status::Blocked && r.public_key == *public_key))
}

/// Records that `public_key` introduced itself from `address` at `now`.
/// Returns true for a request not seen before, or dismissed since. A
/// blocked key is left as it is (and returns false).
pub fn record(path: &Path, public_key: [u8; 32], address: &str, now: u64) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    let mut all = load(path)?;
    let address = crate::text::single_line(address);
    let new = match all.iter_mut().find(|r| r.public_key == public_key) {
        Some(existing) if existing.status == Status::Blocked => return Ok(false),
        Some(existing) => {
            let new = existing.status == Status::Dismissed;
            if new {
                *existing = Request { public_key, status: Status::Pending, address, first_seen: now, last_seen: now };
            } else {
                existing.address = address;
                existing.last_seen = now.max(existing.last_seen);
            }
            new
        }
        None => {
            all.push(Request { public_key, status: Status::Pending, address, first_seen: now, last_seen: now });
            true
        }
    };
    trim(&mut all);
    save(path, &all)?;
    Ok(new)
}

/// Keeps within `MAX_PENDING` and `MAX_DISMISSED`, the line heard from
/// or changed least recently going first.
fn trim(all: &mut Vec<Request>) {
    for (status, cap) in [(Status::Pending, MAX_PENDING), (Status::Dismissed, MAX_DISMISSED)] {
        while all.iter().filter(|r| r.status == status).count() > cap {
            let stalest = all
                .iter()
                .enumerate()
                .filter(|(_, r)| r.status == status)
                .min_by_key(|(_, r)| r.last_seen)
                .map(|(i, _)| i)
                .expect("over the cap, so at least one");
            all.remove(stalest);
        }
    }
}

/// Dismisses `public_key`'s request (or drops it when it's a peer's
/// device). Leaves a block in place.
pub fn remove(path: &Path, public_key: &[u8; 32]) -> std::io::Result<()> {
    dismiss_where(path, public_key, |r| r.status != Status::Blocked)
}

/// Forgets a request or a block of `public_key`. For adding them,
/// unblocking them, or deleting them as a contact.
pub fn forget(path: &Path, public_key: &[u8; 32]) -> std::io::Result<()> {
    dismiss_where(path, public_key, |_| true)
}

/// Marks `public_key`'s line dismissed, now, if `which` is true of it.
fn dismiss_where(path: &Path, public_key: &[u8; 32], which: impl Fn(&Request) -> bool) -> std::io::Result<()> {
    let _guard = crate::store::lock();
    let mut all = load(path)?;
    let Some(line) = all.iter_mut().find(|r| r.public_key == *public_key && r.status != Status::Dismissed && which(r)) else {
        return Ok(());
    };
    line.status = Status::Dismissed;
    line.last_seen = crate::unix_now().max(line.last_seen + 1);
    trim(&mut all);
    save(path, &all)
}

/// How soon a key's cancelling is taken in again, in seconds.
const CANCELLED_AGAIN: u64 = 60;

/// `public_key` has cancelled their request. A dismissed line even with
/// no line here: the request may have reached another of the user's
/// devices, and that line is what takes it off there (`merge`). Leaves a
/// block in place. Returns whether a request was showing here.
pub fn cancelled(path: &Path, public_key: &[u8; 32], now: u64) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    let mut all = load(path)?;
    let showing = match all.iter_mut().find(|r| r.public_key == *public_key) {
        Some(line) if line.status == Status::Blocked => return Ok(false),
        Some(line) if line.status == Status::Dismissed && now < line.last_seen.saturating_add(CANCELLED_AGAIN) => return Ok(false),
        Some(line) => {
            let showing = line.status == Status::Pending;
            line.status = Status::Dismissed;
            line.last_seen = now.max(line.last_seen + 1);
            showing
        }
        None => {
            all.push(Request { public_key: *public_key, status: Status::Dismissed, address: String::new(), first_seen: now, last_seen: now });
            false
        }
    };
    trim(&mut all);
    save(path, &all)?;
    Ok(showing)
}

/// Takes in another of the user's devices' lines: for each key, the one
/// changed later. Keys in `skip` (the user's own, peers') are left out.
/// Returns whether anything changed.
pub fn merge(path: &Path, theirs: Vec<Request>, skip: impl Fn(&[u8; 32]) -> bool) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    let mut all = load(path)?;
    let mut changed = false;
    for mut line in theirs {
        if skip(&line.public_key) {
            continue;
        }
        line.address = crate::text::single_line(&line.address);
        match all.iter_mut().find(|r| r.public_key == line.public_key) {
            Some(ours) if line.last_seen > ours.last_seen => {
                if ours.status == Status::Pending && line.status == Status::Pending {
                    line.first_seen = line.first_seen.min(ours.first_seen);
                }
                *ours = line;
                changed = true;
            }
            Some(_) => {}
            None if line.status == Status::Dismissed => {}
            None => {
                all.push(line);
                changed = true;
            }
        }
    }
    if changed {
        trim(&mut all);
        save(path, &all)?;
    }
    Ok(changed)
}

/// Blocks `public_key`: its request goes, and it's never recorded again.
pub fn block(path: &Path, public_key: [u8; 32], now: u64) -> std::io::Result<()> {
    set_status(path, public_key, Status::Blocked, "", now)
}

/// Drops each dismissed line `settled` is true of: one every one of the
/// user's devices has (see `node::settle`). Returns whether any went.
pub fn prune(path: &Path, mut settled: impl FnMut(&Request) -> bool) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    let mut all = load(path)?;
    let before = all.len();
    all.retain(|r| r.status != Status::Dismissed || !settled(r));
    if all.len() == before {
        return Ok(false);
    }
    save(path, &all)?;
    Ok(true)
}

fn set_status(path: &Path, public_key: [u8; 32], status: Status, address: &str, now: u64) -> std::io::Result<()> {
    let _guard = crate::store::lock();
    let mut all = load(path)?;
    match all.iter_mut().find(|r| r.public_key == public_key) {
        Some(existing) if existing.status == Status::Blocked => {}
        Some(existing) => {
            existing.status = status;
            existing.last_seen = now.max(existing.last_seen + 1);
        }
        None => all.push(Request { public_key, status, address: crate::text::single_line(address), first_seen: now, last_seen: now }),
    }
    save(path, &all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-requests-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("requests")
    }

    #[test]
    fn record_refreshes_an_existing_request_instead_of_duplicating_it() {
        let path = temp_path("refresh");
        assert!(record(&path, [1; 32], "2001:db8::1", 100).unwrap());
        assert!(!record(&path, [1; 32], "2001:db8::2", 200).unwrap());
        let all = pending(&path).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].address, "2001:db8::2");
        assert_eq!((all[0].first_seen, all[0].last_seen), (100, 200));
    }

    #[test]
    fn a_blocked_key_is_never_recorded_again() {
        let path = temp_path("block");
        record(&path, [2; 32], "2001:db8::1", 100).unwrap();
        block(&path, [2; 32], 150).unwrap();
        assert!(pending(&path).unwrap().is_empty());
        assert!(!record(&path, [2; 32], "2001:db8::1", 200).unwrap());
        assert!(pending(&path).unwrap().is_empty());
        assert!(is_blocked(&path, &[2; 32]).unwrap());
        remove(&path, &[2; 32]).unwrap();
        assert!(is_blocked(&path, &[2; 32]).unwrap());
    }

    #[test]
    fn an_unblocked_key_can_ask_again() {
        let path = temp_path("unblock");
        block(&path, [4; 32], 100).unwrap();
        assert_eq!(blocked(&path).unwrap().len(), 1);
        forget(&path, &[4; 32]).unwrap();
        assert!(blocked(&path).unwrap().is_empty());
        assert!(record(&path, [4; 32], "2001:db8::1", crate::unix_now() + 10).unwrap(), "can ask again");
    }

    #[test]
    fn a_dismissed_line_goes_once_settled() {
        let path = temp_path("prune");
        record(&path, [6; 32], "2001:db8::1", 100).unwrap();
        record(&path, [9; 32], "2001:db8::1", 100).unwrap();
        block(&path, [10; 32], 100).unwrap();
        remove(&path, &[6; 32]).unwrap();
        assert!(!prune(&path, |_| false).unwrap());
        assert!(prune(&path, |_| true).unwrap());
        let left: Vec<[u8; 32]> = all(&path).unwrap().into_iter().map(|r| r.public_key).collect();
        assert_eq!(left, vec![[9; 32], [10; 32]], "only the dismissed line goes: not a request, nor a block");
        let other = temp_path("prune-other");
        assert!(!merge(&other, vec![Request { public_key: [6; 32], status: Status::Dismissed, address: String::new(), first_seen: 1, last_seen: 200 }], |_| false).unwrap());
        assert!(all(&other).unwrap().is_empty());
    }

    #[test]
    fn a_request_cancelled_goes_and_can_be_made_again() {
        let path = temp_path("cancelled");
        record(&path, [11; 32], "2001:db8::1", 100).unwrap();
        assert!(cancelled(&path, &[11; 32], 200).unwrap());
        assert!(pending(&path).unwrap().is_empty());
        assert!(!cancelled(&path, &[11; 32], 300).unwrap(), "nothing showing any more");
        let written = all(&path).unwrap();
        assert!(!cancelled(&path, &[11; 32], 301).unwrap());
        assert_eq!(all(&path).unwrap(), written, "saying so again at once writes nothing");
        assert!(record(&path, [11; 32], "2001:db8::1", 400).unwrap(), "asked again, it's new");

        block(&path, [12; 32], 100).unwrap();
        assert!(!cancelled(&path, &[12; 32], 200).unwrap());
        assert!(is_blocked(&path, &[12; 32]).unwrap());

        let other = temp_path("cancelled-other");
        record(&other, [13; 32], "2001:db8::1", 100).unwrap();
        assert!(!cancelled(&path, &[13; 32], 200).unwrap(), "no request here");
        assert!(merge(&other, all(&path).unwrap(), |_| false).unwrap());
        assert!(pending(&other).unwrap().iter().all(|r| r.public_key != [13; 32]));
    }

    #[test]
    fn dismissing_removes_the_request() {
        let path = temp_path("dismiss");
        record(&path, [3; 32], "2001:db8::1", 100).unwrap();
        remove(&path, &[3; 32]).unwrap();
        assert!(pending(&path).unwrap().is_empty());
        assert!(blocked(&path).unwrap().is_empty(), "dismissed isn't blocked");
        assert!(record(&path, [3; 32], "2001:db8::1", crate::unix_now() + 10).unwrap(), "comes back if they try again");
    }

    /// Another device's lines: a request it was sent is one here too, and
    /// what was done with it, later, is done here.
    #[test]
    fn merging_takes_whichever_changed_last() {
        let desktop = temp_path("merge-desktop");
        let phone = temp_path("merge-phone");
        record(&desktop, [7; 32], "198.51.100.7:1977", 100).unwrap();
        record(&desktop, [8; 32], "198.51.100.8:1977", 100).unwrap();
        assert!(merge(&phone, all(&desktop).unwrap(), |k| *k == [8; 32]).unwrap());
        let keys: Vec<[u8; 32]> = pending(&phone).unwrap().into_iter().map(|r| r.public_key).collect();
        assert_eq!(keys, vec![[7; 32]], "a skipped key (a peer) isn't taken");
        assert_eq!(pending(&phone).unwrap()[0].address, "198.51.100.7:1977");

        remove(&phone, &[7; 32]).unwrap();
        assert!(merge(&desktop, all(&phone).unwrap(), |_| false).unwrap());
        assert!(!pending(&desktop).unwrap().iter().any(|r| r.public_key == [7; 32]));
        assert!(!merge(&desktop, vec![Request { public_key: [7; 32], status: Status::Pending, address: String::new(), first_seen: 100, last_seen: 100 }], |_| false).unwrap());
        assert!(record(&desktop, [7; 32], "198.51.100.7:1977", crate::unix_now() + 10).unwrap());
        merge(&phone, all(&desktop).unwrap(), |_| false).unwrap();
        assert!(pending(&phone).unwrap().iter().any(|r| r.public_key == [7; 32]));

        block(&desktop, [7; 32], crate::unix_now() + 20).unwrap();
        merge(&phone, all(&desktop).unwrap(), |_| false).unwrap();
        assert!(is_blocked(&phone, &[7; 32]).unwrap());
        forget(&phone, &[7; 32]).unwrap();
        merge(&desktop, all(&phone).unwrap(), |_| false).unwrap();
        assert!(!is_blocked(&desktop, &[7; 32]).unwrap());
        assert!(pending(&desktop).unwrap().iter().all(|r| r.public_key != [7; 32]));
    }

    #[test]
    fn past_the_cap_the_stalest_request_is_dropped() {
        let path = temp_path("cap");
        for i in 0..MAX_PENDING as u8 {
            record(&path, [i; 32], "2001:db8::1", 1000 + i as u64).unwrap();
        }
        record(&path, [0; 32], "2001:db8::1", 5000).unwrap();
        record(&path, [200; 32], "2001:db8::1", 6000).unwrap();
        let keys: Vec<[u8; 32]> = pending(&path).unwrap().into_iter().map(|r| r.public_key).collect();
        assert_eq!(keys.len(), MAX_PENDING);
        assert!(keys.contains(&[0; 32]));
        assert!(keys.contains(&[200; 32]));
        assert!(!keys.contains(&[1; 32]), "the one heard from least recently goes");
    }
}
