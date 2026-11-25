//! What a signed log came to, remembered. A channel's roster is worked
//! out from every event in it (which count, given who could do what
//! when), in time that grows with the square of its length — a roster of
//! 2000 events took 120 ms — and it's asked for on every redraw, though
//! it rarely changes.

use crate::lock::Locked;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, RandomState};
use std::sync::{LazyLock, Mutex};

/// How many results are kept before starting afresh.
const MOST_KEPT: usize = 256;

/// This process's own key for hashing a log: nobody sending events can
/// know it, so nobody can make one log pass for another.
static KEY: LazyLock<RandomState> = LazyLock::new(RandomState::new);
static KEPT: LazyLock<Mutex<HashMap<u64, Vec<usize>>>> = LazyLock::new(Mutex::default);

/// Which of `events` count, under `scope` (a channel, an account): as
/// `work` finds, or found last time for exactly these events.
pub(crate) fn counted<'a, T: Hash>(scope: &impl Hash, events: &'a [T], work: impl FnOnce() -> Vec<&'a T>) -> Vec<&'a T> {
    let key = KEY.hash_one((std::any::type_name::<T>(), scope, events));
    if let Some(positions) = KEPT.locked().get(&key) {
        return positions.iter().map(|&i| &events[i]).collect();
    }
    let counted = work();
    let position: HashMap<*const T, usize> = events.iter().enumerate().map(|(i, e)| (e as *const T, i)).collect();
    let positions = counted.iter().map(|e| position[&(*e as *const T)]).collect();
    let mut kept = KEPT.locked();
    if kept.len() >= MOST_KEPT {
        kept.clear();
    }
    kept.insert(key, positions);
    counted
}
