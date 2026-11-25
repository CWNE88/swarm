//! What the user has taken back and someone still has to be told
//! (`~/.swarm/withdrawals`, one per line): a contact request cancelled.
//! Kept the same on all
//! of their devices (`node::withdraw`), and whichever reaches that person
//! first tells them.
//!
//! One no longer `waiting` (told, or changed their mind) stays a while, so
//! a device that missed that doesn't tell them again. Deleting a contact
//! otherwise leaves nothing of them (`forget`), but a request can't be
//! cancelled without their key and where they might be: both stay here
//! till they've been told, or `GIVE_UP_AFTER`, and the address goes then.

use crate::store;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::Path;

/// The most lines kept; past it, the oldest finished go first.
pub const MAX_WITHDRAWALS: usize = 64;

/// How long someone who never turns up is waited for, in seconds.
pub const GIVE_UP_AFTER: u64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    /// The contact request this user made to them.
    ContactRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    pub what: What,
    /// Who's to be told (user key).
    pub to: [u8; 32],
    /// Where to reach them: for a request, as they're no longer a contact
    /// to look up. Empty once finished.
    pub address: String,
    /// When this line last changed: the later wins between devices.
    pub changed_at: u64,
    pub waiting: bool,
}

impl Withdrawal {
    /// Whether they're still to be told, as of `now`.
    pub fn is_due(&self, now: u64) -> bool {
        self.waiting && now < self.changed_at.saturating_add(GIVE_UP_AFTER)
    }
}

fn format_line(w: &Withdrawal) -> String {
    let what = match &w.what {
        What::ContactRequest => "r".to_string(),
    };
    format!("{}\t{}\t{}\t{}\t{what}\n", w.changed_at, STANDARD.encode(w.to), if w.waiting { "w" } else { "d" }, w.address)
}

fn parse_line(line: &str) -> Option<Withdrawal> {
    let mut parts = line.splitn(7, '\t');
    let changed_at = parts.next()?.parse().ok()?;
    let to: [u8; 32] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let waiting = match parts.next()? {
        "w" => true,
        "d" => false,
        _ => return None,
    };
    let address = parts.next()?.to_string();
    let what = match parts.next()? {
        "r" => What::ContactRequest,
        _ => return None,
    };
    Some(Withdrawal { what, to, address, changed_at, waiting })
}

/// Every line, waiting or not.
pub fn load(path: &Path) -> Vec<Withdrawal> {
    store::read(path).map(|s| s.lines().filter_map(parse_line).collect()).unwrap_or_default()
}

/// Saved, within `MAX_WITHDRAWALS`: the oldest finished go first.
fn save(path: &Path, all: &mut Vec<Withdrawal>) -> std::io::Result<()> {
    all.sort_by_key(|w| (w.waiting, w.changed_at));
    let excess = all.len().saturating_sub(MAX_WITHDRAWALS);
    all.drain(..excess);
    store::write_atomic(path, all.iter().map(format_line).collect::<String>())
}

/// `to` is to be told `what` has been taken back; `address`, where they
/// might be. Replaces a line about the same thing.
pub fn add(path: &Path, what: What, to: [u8; 32], address: &str, now: u64) -> std::io::Result<()> {
    let _guard = store::lock();
    let mut all = load(path);
    let line = all.iter().position(|w| w.what == what && w.to == to);
    let changed_at = line.map_or(now, |at| now.max(all[at].changed_at + 1));
    if let Some(at) = line {
        all.remove(at);
    }
    all.push(Withdrawal { what, to, address: crate::text::single_line(address), changed_at, waiting: true });
    save(path, &mut all)
}

/// `to` has been told about `what`, or no longer needs to be. Returns
/// whether they were still to be.
pub fn finish(path: &Path, what: &What, to: &[u8; 32], now: u64) -> std::io::Result<bool> {
    let _guard = store::lock();
    let mut all = load(path);
    let Some(line) = all.iter_mut().find(|w| w.what == *what && w.to == *to && w.waiting) else { return Ok(false) };
    line.changed_at = now.max(line.changed_at + 1);
    line.waiting = false;
    line.address.clear();
    save(path, &mut all)?;
    Ok(true)
}

/// Past `GIVE_UP_AFTER`: anyone never reached is given up on, which lets
/// go of where they might be, and a line finished that long ago goes
/// altogether. Returns whether anything changed.
pub fn sweep(path: &Path, now: u64) -> std::io::Result<bool> {
    let _guard = store::lock();
    let mut all = load(path);
    let before = all.len();
    all.retain(|w| w.waiting || now < w.changed_at.saturating_add(GIVE_UP_AFTER));
    let mut changed = all.len() != before;
    for line in all.iter_mut().filter(|w| w.waiting && !w.is_due(now)) {
        line.changed_at = now.max(line.changed_at + 1);
        line.waiting = false;
        line.address.clear();
        changed = true;
    }
    if changed {
        save(path, &mut all)?;
    }
    Ok(changed)
}

/// Takes in another of the user's devices' lines: for each, the one
/// changed later, and finished if they're the same age. Returns whether
/// anything changed.
pub fn merge(path: &Path, theirs: Vec<Withdrawal>) -> std::io::Result<bool> {
    let _guard = store::lock();
    let mut all = load(path);
    let mut changed = false;
    for mut line in theirs {
        line.address = crate::text::single_line(&line.address);
        match all.iter_mut().find(|w| w.what == line.what && w.to == line.to) {
            Some(ours) if line.changed_at > ours.changed_at || (line.changed_at == ours.changed_at && ours.waiting && !line.waiting) => {
                *ours = line;
                changed = true;
            }
            Some(_) => {}
            None => {
                all.push(line);
                changed = true;
            }
        }
    }
    if changed {
        save(path, &mut all)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("swarm-withdrawals-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_withdrawal_waits_ends_and_the_later_change_wins() {
        let path = temp_path("wins");
        add(&path, What::ContactRequest, [2; 32], "2001:db8::2,192.0.2.2", 100).unwrap();
        let before = load(&path);
        assert_eq!(before.len(), 1);
        assert_eq!(before.iter().find(|w| w.what == What::ContactRequest).unwrap().address, "2001:db8::2,192.0.2.2");

        assert!(finish(&path, &What::ContactRequest, &[2; 32], 100).unwrap(), "a clock that hasn't moved still makes it later");
        assert!(!finish(&path, &What::ContactRequest, &[2; 32], 100).unwrap(), "only once");
        assert!(!merge(&path, before).unwrap(), "another device's older copy doesn't start it again");
        let done = load(&path).into_iter().find(|w| w.what == What::ContactRequest).unwrap();
        assert!(!done.waiting);
        assert!(done.address.is_empty(), "where they are isn't kept once they've been told");

        let other = Withdrawal { what: What::ContactRequest, to: [3; 32], address: String::new(), changed_at: 9, waiting: false };
        assert!(merge(&path, vec![other]).unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn someone_who_never_turns_up_is_given_up_on() {
        let line = Withdrawal { what: What::ContactRequest, to: [2; 32], address: String::new(), changed_at: 1000, waiting: true };
        assert!(line.is_due(1000 + GIVE_UP_AFTER - 1));
        assert!(!line.is_due(1000 + GIVE_UP_AFTER));
    }

    #[test]
    fn past_the_time_limit_nothing_of_them_is_kept_for_ever() {
        let path = temp_path("sweep");
        add(&path, What::ContactRequest, [2; 32], "2001:db8::2", 1000).unwrap();
        assert!(!sweep(&path, 1000 + GIVE_UP_AFTER - 1).unwrap(), "still waiting for them");
        assert!(sweep(&path, 1000 + GIVE_UP_AFTER).unwrap());
        let line = load(&path).remove(0);
        assert!(!line.waiting);
        assert!(line.address.is_empty(), "where they might be goes");
        assert!(sweep(&path, line.changed_at + GIVE_UP_AFTER).unwrap());
        assert!(load(&path).is_empty(), "and in time, the line");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn past_the_cap_what_is_finished_goes_first() {
        let path = temp_path("cap");
        add(&path, What::ContactRequest, [0; 32], "", 10).unwrap();
        finish(&path, &What::ContactRequest, &[0; 32], 11).unwrap();
        for i in 1..=MAX_WITHDRAWALS as u8 {
            add(&path, What::ContactRequest, [i; 32], "", 5).unwrap();
        }
        let all = load(&path);
        assert_eq!(all.len(), MAX_WITHDRAWALS);
        assert!(all.iter().all(|w| w.waiting), "all those still to tell are kept");
        let _ = std::fs::remove_file(&path);
    }
}
