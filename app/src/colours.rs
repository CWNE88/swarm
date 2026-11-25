//! Which colour each name is drawn in — as a role, which each front end
//! turns into a colour of its own (the terminal's 16, a desktop theme).
//!
//! - **You:** your own colour.
//! - **The other person in a one-to-one chat:** one colour for everyone
//!   (it's only ever them).
//! - **Channel members:** ten slots, handed out in the order people
//!   joined — so no two share one until there are more than ten, and
//!   nobody's changes when someone else joins or leaves. Front ends order
//!   their ten from most to least distinct, and leave out red (that's for
//!   things going wrong).

use std::collections::HashMap;

/// How many member colours there are.
pub const MEMBER_COLOURS: u8 = 10;

/// What colour a name is, as a role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameColour {
    Me,
    /// The other person in a one-to-one chat.
    Peer,
    /// A channel member's slot, `0..MEMBER_COLOURS`.
    Member(u8),
    /// Someone with no slot (not a member any more, say).
    Other,
}

/// Each member's slot in a channel: everyone who has ever joined, in the
/// order they joined (`members`; you're left out), gets the next one.
pub fn channel_colours(members: impl IntoIterator<Item = [u8; 32]>, me: &[u8; 32]) -> HashMap<[u8; 32], u8> {
    let mut colours = HashMap::new();
    for key in members {
        if key != *me && !colours.contains_key(&key) {
            let next = (colours.len() % MEMBER_COLOURS as usize) as u8;
            colours.insert(key, next);
        }
    }
    colours
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_members_get_different_colours_in_join_order() {
        let me = [0; 32];
        let colours = channel_colours([[1; 32], me, [2; 32], [1; 32], [3; 32]], &me);
        assert_eq!(colours.len(), 3);
        assert!(!colours.contains_key(&me));
        assert_eq!((colours[&[1; 32]], colours[&[2; 32]], colours[&[3; 32]]), (0, 1, 2));
    }
}
