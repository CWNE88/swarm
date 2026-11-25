//! Notifications of messages, for the phone: who's written, never what
//! (see `prefs::Notify` for when). On a computer, where
//! nothing shows them, a sound in their place (see `sounds`).

use std::collections::HashMap;
use crate::prefs::Notify;

/// A notification to show or take away. Each chat has its own, by `id`.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    Message { id: i32, from: String },
    Clear { id: i32 },
}

/// How much was unread in each chat when last looked: what's new is
/// what's grown since.
#[derive(Default)]
pub struct Unread(HashMap<String, usize>);

impl Unread {
    /// Whether it's looked yet: before then, what's unread was there
    /// before swarm started, and isn't news.
    pub fn has_looked(&self) -> bool {
        !self.0.is_empty()
    }

    /// What to show and take away, now that each chat (its id, its name,
    /// how much is unread) is as `chats` says: a chat that's more unread
    /// is notified as `notify` has it, unless Swarm's `in_sight` (it's
    /// there to be seen); one that's been read taken away.
    pub fn notices(&mut self, chats: &[(String, String, usize)], notify: Notify, in_sight: bool) -> Vec<Notice> {
        let mut notices = Vec::new();
        for (id, name, unread) in chats {
            let before = self.0.insert(id.clone(), *unread).unwrap_or(0);
            if *unread > before {
                let tell = !in_sight && match notify {
                    Notify::Off => false,
                    Notify::First => before == 0,
                    Notify::Every => true,
                };
                if tell {
                    notices.push(Notice::Message { id: notice_id(id), from: name.clone() });
                }
            } else if *unread == 0 && before > 0 {
                notices.push(Notice::Clear { id: notice_id(id) });
            }
        }
        notices
    }
}

/// On a computer: whether a message's sound is made — for what the phone
/// would notify of (`Unread::notices`, with the window the one in use
/// for "in sight"), and never in a call, whose sound it would cut across.
pub fn sounds(notices: &[Notice], in_call: bool) -> bool {
    !in_call && notices.iter().any(|n| matches!(n, Notice::Message { .. }))
}

/// A chat's notification number: the same each time, and never the
/// service's own (1).
pub fn notice_id(id: &str) -> i32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in id.bytes() {
        hash = (hash ^ byte as u32).wrapping_mul(0x01000193);
    }
    (hash % 0x3fff_0000) as i32 + 0x1000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What was unread before the first look isn't news to sound for.
    #[test]
    fn what_was_unread_at_the_start_isnt_news() {
        let mut unread = Unread::default();
        assert!(!unread.has_looked());
        let start = chats(&[("p:bob", "Bob", 3)]);
        let looked = unread.has_looked();
        assert!(!sounds(&unread.notices(&start, Notify::Every, !looked), false));
        assert!(unread.has_looked());
        assert!(sounds(&unread.notices(&chats(&[("p:bob", "Bob", 4)]), Notify::Every, false), false));
    }

    /// A computer's sound: made as the phone would notify, while the
    /// window isn't the one in use; not for a chat read, nor in a call.
    #[test]
    fn a_sound_where_the_phone_would_notify() {
        let mut unread = Unread::default();
        let _ = unread.notices(&chats(&[("p:bob", "Bob", 0)]), Notify::First, false);
        let in_use = unread.notices(&chats(&[("p:bob", "Bob", 1)]), Notify::First, true);
        assert!(!sounds(&in_use, false), "the window in use: no sound");
        let away = unread.notices(&chats(&[("p:bob", "Bob", 2)]), Notify::Every, false);
        assert!(sounds(&away, false));
        assert!(!sounds(&away, true), "not in a call");
        let read = unread.notices(&chats(&[("p:bob", "Bob", 0)]), Notify::Every, false);
        assert!(!sounds(&read, false), "taken away: nothing to sound for");
        let off = unread.notices(&chats(&[("p:bob", "Bob", 1)]), Notify::Off, false);
        assert!(!sounds(&off, false));
    }

    fn chats(list: &[(&str, &str, usize)]) -> Vec<(String, String, usize)> {
        list.iter().map(|(id, name, unread)| (id.to_string(), name.to_string(), *unread)).collect()
    }

    /// First: once when someone writes with nothing of theirs unread, not
    /// again for more until it's read. Every: each time more comes. Off:
    /// never. Read, it's taken away (whatever the choice: it may have
    /// been shown before the choice changed).
    #[test]
    fn notified_as_chosen() {
        let mut first = Unread::default();
        let mut every = Unread::default();
        let mut off = Unread::default();
        let start = chats(&[("p:bob", "Bob", 0), ("c:dev", "#dev", 0)]);
        for unread in [&mut first, &mut every, &mut off] {
            assert_eq!(unread.notices(&start, Notify::Every, false), [], "nothing new at the start");
        }
        let one = chats(&[("p:bob", "Bob", 1), ("c:dev", "#dev", 0)]);
        let bob = Notice::Message { id: notice_id("p:bob"), from: "Bob".into() };
        assert_eq!(first.notices(&one, Notify::First, false), std::slice::from_ref(&bob));
        assert_eq!(every.notices(&one, Notify::Every, false), std::slice::from_ref(&bob));
        assert_eq!(off.notices(&one, Notify::Off, false), []);
        let three = chats(&[("p:bob", "Bob", 3), ("c:dev", "#dev", 1)]);
        let dev = Notice::Message { id: notice_id("c:dev"), from: "#dev".into() };
        assert_eq!(first.notices(&three, Notify::First, false), std::slice::from_ref(&dev), "Bob's more aren't, #dev's first is");
        assert_eq!(every.notices(&three, Notify::Every, false), [bob.clone(), dev.clone()]);
        assert_eq!(off.notices(&three, Notify::Off, false), []);
        let read = chats(&[("p:bob", "Bob", 0), ("c:dev", "#dev", 1)]);
        assert_eq!(first.notices(&read, Notify::First, false), [Notice::Clear { id: notice_id("p:bob") }]);
        assert_eq!(off.notices(&read, Notify::Off, false), [Notice::Clear { id: notice_id("p:bob") }]);
        let again = chats(&[("p:bob", "Bob", 1), ("c:dev", "#dev", 1)]);
        assert_eq!(first.notices(&again, Notify::First, false), [bob]);
        let mut looking = Unread::default();
        assert_eq!(looking.notices(&start, Notify::Every, true), []);
        assert_eq!(looking.notices(&one, Notify::Every, true), [], "in sight");
        assert_eq!(looking.notices(&one, Notify::Every, false), [], "out of sight, nothing newer");
        assert_eq!(looking.notices(&read, Notify::Every, true), [Notice::Clear { id: notice_id("p:bob") }], "read: taken away");

        assert_eq!(notice_id("p:bob"), notice_id("p:bob"));
        assert_ne!(notice_id("p:bob"), notice_id("c:dev"));
        assert!(["p:bob", "c:dev", "x"].iter().all(|id| notice_id(id) != 1));
    }
}
