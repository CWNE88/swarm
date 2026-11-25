//! When a "gone" line can go: a contact deleted (`ownpeers`, not
//! present) or a request dismissed (`ownrequests`, dismissed) is kept
//! only until every one of the user's other devices has it, so that none
//! of them brings it back, and then nothing of it is left.
//!
//! Each device's books, as they last arrived here, are remembered (in
//! memory only), with when they arrived. A gone line has settled once,
//! from every other device, a book has arrived since this device first
//! had the line that has it gone too (as late, or later), or hasn't got
//! the key at all: a device that has nothing of the key can't bring it
//! back, and one given the line it's missing doesn't take it (there's
//! nothing to undo). After a restart, every device's book is waited for
//! again: each link sends its books when it starts.

use std::collections::HashMap;
use std::time::Instant;

/// The two books.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Book {
    Peers,
    Requests,
}

/// What a device's book says of one key: gone, as of when; or there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Held {
    Gone(u64),
    There,
}

/// A device's book as it last arrived: when, and what it has.
type Arrived = (Instant, HashMap<[u8; 32], Held>);

#[derive(Default)]
pub(super) struct Settle {
    /// Each gone line, by book and key: as of when, and when it was
    /// first had here.
    since: HashMap<(Book, [u8; 32]), (u64, Instant)>,
    /// Each device's book, as it last arrived, and when.
    books: HashMap<(Book, [u8; 32]), Arrived>,
}

impl Settle {
    /// Device `device`'s whole `book`, just arrived.
    pub(super) fn arrived(&mut self, book: Book, device: [u8; 32], held: HashMap<[u8; 32], Held>) {
        self.books.insert((book, device), (Instant::now(), held));
    }

    /// Whether the gone line for `key` in `book`, as of `at`, is had by
    /// every device in `others`.
    pub(super) fn settled(&mut self, book: Book, key: [u8; 32], at: u64, others: &[[u8; 32]]) -> bool {
        let since = match self.since.get(&(book, key)) {
            Some((was, since)) if *was == at => *since,
            _ => {
                let now = Instant::now();
                self.since.insert((book, key), (at, now));
                now
            }
        };
        others.iter().all(|device| match self.books.get(&(book, *device)) {
            Some((when, held)) if *when > since => match held.get(&key) {
                None => true,
                Some(Held::Gone(theirs)) => *theirs >= at,
                Some(Held::There) => false,
            },
            _ => false,
        })
    }

    /// The line for `key` is gone from `book` here: nothing more to wait
    /// for.
    pub(super) fn dropped(&mut self, book: Book, key: &[u8; 32]) {
        self.since.remove(&(book, *key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_once_every_other_device_has_it_or_never_had_it() {
        let mut s = Settle::default();
        let (phone, laptop) = ([1; 32], [2; 32]);
        let key = [9; 32];
        assert!(s.settled(Book::Peers, key, 100, &[]));
        s.arrived(Book::Peers, phone, HashMap::from([(key, Held::Gone(100))]));
        s.dropped(Book::Peers, &key);
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(!s.settled(Book::Peers, key, 100, &[phone, laptop]));
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.arrived(Book::Peers, phone, HashMap::from([(key, Held::There)]));
        s.arrived(Book::Peers, laptop, HashMap::new());
        assert!(!s.settled(Book::Peers, key, 100, &[phone, laptop]));
        s.arrived(Book::Peers, phone, HashMap::from([(key, Held::Gone(50))]));
        assert!(!s.settled(Book::Peers, key, 100, &[phone, laptop]));
        s.arrived(Book::Peers, phone, HashMap::from([(key, Held::Gone(100))]));
        assert!(s.settled(Book::Peers, key, 100, &[phone, laptop]));
        assert!(!s.settled(Book::Requests, key, 100, &[phone]));
    }
}
