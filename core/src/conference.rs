//! Conference calls: who's in one.
//!
//! A conference is a set of ordinary two-person call links, one between
//! each two participants who are each other's peers, carried on their
//! usual connection (see `call`). What ties them together is the
//! *roster*: who's in, as a small signed log every participant keeps and
//! passes on. Anyone in it can add one of their own peers. Each addition
//! is signed by whoever made it, who has to have been in it themself;
//! each leaving is signed by whoever left, or by someone who outranks
//! them (a removal). Someone who's left can be added again: each time is
//! a new *round* of theirs, which their next leaving ends. Rosters are
//! merged (the union of what each side knows), so they agree however the
//! news travels.
//!
//! Each addition carries a *number*, the order they came in: whoever
//! placed the call it grew from is 1, whoever answered 2, and each
//! addition after that one more than any its adder knew of. A lower
//! number outranks a higher, and can remove them; two given the same
//! number at once (added by different people) are ranked by key. The
//! call never depends on number 1: when they leave, the rest carry on.
//!
//! The roster never makes anyone a peer: a participant links only to the
//! participants on its own peer list (`node::NodeHandle::call_handle`),
//! and hears nobody else.

use crate::identity::{self, Identity};
use rand::RngCore;

/// A conference's id: random, so two conferences between the same
/// people never mix.
pub type ConferenceId = [u8; 16];

/// The most people in one conference: every one of them is a link and a
/// voice for each of the others.
pub const MOST_MEMBERS: usize = 16;
/// The most entries a roster keeps: people coming and going.
pub const MOST_ENTRIES: usize = 4 * MOST_MEMBERS;

/// `member` was added to the conference by `by` (who signed it), for
/// the `round`th time (from 0), as `number`. The starter adds themself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub member: [u8; 32],
    pub by: [u8; 32],
    pub round: u32,
    pub number: u32,
    pub signature: [u8; 64],
}

/// `member` left, ending that round of theirs: signed by them (`by` is
/// `member`), or removed by `by` in their own `by_round`, who outranked
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Left {
    pub member: [u8; 32],
    pub round: u32,
    pub by: [u8; 32],
    pub by_round: u32,
    pub signature: [u8; 64],
}

impl Left {
    /// Removed by someone else, rather than leaving.
    pub fn removed(&self) -> bool {
        self.by != self.member
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roster {
    pub id: ConferenceId,
    /// Who started it: the one addition that needs nobody before it.
    pub founder: [u8; 32],
    pub added: Vec<Added>,
    pub left: Vec<Left>,
}

fn added_message(id: &ConferenceId, member: &[u8; 32], round: u32, number: u32) -> Vec<u8> {
    [b"swarm conference added\0".as_slice(), id, member, &round.to_le_bytes(), &number.to_le_bytes()].concat()
}

fn left_message(id: &ConferenceId, member: &[u8; 32], round: u32) -> Vec<u8> {
    [b"swarm conference left\0".as_slice(), id, member, &round.to_le_bytes()].concat()
}

fn removed_message(id: &ConferenceId, member: &[u8; 32], round: u32, by_round: u32) -> Vec<u8> {
    [b"swarm conference removed\0".as_slice(), id, member, &round.to_le_bytes(), &by_round.to_le_bytes()].concat()
}

/// Where someone stands: their number, then their key. Lower outranks.
type Rank = (u32, [u8; 32]);

impl Roster {
    /// A new conference, with `identity` in it as number 1.
    pub fn start(identity: &Identity) -> Roster {
        Roster::started(identity, 1)
    }

    /// A two-person call `identity` answered becoming a conference: the
    /// one who placed it, `caller`, is number 1, and `identity` number 2.
    pub fn start_answered(identity: &Identity, caller: [u8; 32]) -> Roster {
        let mut roster = Roster::started(identity, 2);
        roster.push_added(identity, caller, 0, 1);
        roster
    }

    fn started(identity: &Identity, number: u32) -> Roster {
        let mut id = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut id);
        let me = identity.public_key_bytes();
        let mut roster = Roster { id, founder: me, added: Vec::new(), left: Vec::new() };
        roster.push_added(identity, me, 0, number);
        roster
    }

    fn push_added(&mut self, identity: &Identity, member: [u8; 32], round: u32, number: u32) {
        let signature = identity.sign(&added_message(&self.id, &member, round, number));
        self.added.push(Added { member, by: identity.public_key_bytes(), round, number, signature });
    }

    /// Adds `member` (again, if they've left), signed by `identity` (who
    /// has to be in it), numbered after everyone known of. Adding someone
    /// who's in is a no-op.
    pub fn add(&mut self, identity: &Identity, member: [u8; 32]) {
        if self.is_member(&member) || self.added.len() >= MOST_ENTRIES {
            return;
        }
        let round = self.round(&member).map_or(0, |r| r.saturating_add(1));
        let number = self.added.iter().map(|a| a.number).max().unwrap_or(0).saturating_add(1);
        self.push_added(identity, member, round, number);
    }

    /// `identity` leaves.
    pub fn leave(&mut self, identity: &Identity) {
        let me = identity.public_key_bytes();
        let Some(round) = self.round(&me).filter(|_| self.is_member(&me)) else { return };
        let signature = identity.sign(&left_message(&self.id, &me, round));
        self.left.push(Left { member: me, round, by: me, by_round: round, signature });
    }

    /// `identity` removes `member`, if both are in it and `identity`
    /// outranks them. `true` if they did.
    pub fn remove(&mut self, identity: &Identity, member: [u8; 32]) -> bool {
        let me = identity.public_key_bytes();
        if !self.outranks(&me, &member) || self.left.len() >= MOST_ENTRIES {
            return false;
        }
        let (Some(round), Some(by_round)) = (self.round(&member), self.round(&me)) else { return false };
        let signature = identity.sign(&removed_message(&self.id, &member, round, by_round));
        self.left.push(Left { member, round, by: me, by_round, signature });
        true
    }

    /// `member`'s latest round, if they've ever been added.
    pub fn round(&self, member: &[u8; 32]) -> Option<u32> {
        self.added.iter().filter(|a| a.member == *member).map(|a| a.round).max()
    }

    /// `member`'s number, this round.
    pub fn number(&self, member: &[u8; 32]) -> Option<u32> {
        let round = self.round(member)?;
        self.added.iter().find(|a| a.member == *member && a.round == round).map(|a| a.number)
    }

    /// Both are in it, and `a` outranks `b`: a lower number, or the same
    /// and a lower key.
    pub fn outranks(&self, a: &[u8; 32], b: &[u8; 32]) -> bool {
        let rank = |k: &[u8; 32]| self.is_member(k).then(|| self.number(k).map(|n| (n, *k))).flatten();
        matches!((rank(a), rank(b)), (Some(ra), Some(rb)) if ra < rb)
    }

    /// Just the entries that hold up, so what comes from the wire is
    /// taken through this before anything else:
    /// - each addition signed by its adder, who was in it themself. The
    ///   founder adds themself first, as 1 or 2; only they give out 1
    ///   and 2 (to the one who placed the call, if that wasn't them);
    ///   anyone else gives a number above their own.
    /// - each leaving signed by whoever left, or a removal signed by
    ///   someone who outranked them in the rounds it names.
    pub fn verified(self) -> Roster {
        let Roster { id, founder, added, left } = self;
        let mut good: Vec<Added> = Vec::new();
        let mut waiting: Vec<Added> = added.into_iter().filter(|a| identity::verify(&a.by, &added_message(&id, &a.member, a.round, a.number), &a.signature)).collect();
        loop {
            let before = good.len();
            waiting.retain(|a| {
                let first_two = a.by == founder && a.round == 0 && (a.number == 1 || a.number == 2);
                let counts = if a.member == founder && a.round == 0 {
                    first_two
                } else if a.number <= 2 {
                    first_two && good.iter().any(|g| g.member == founder) && !good.iter().any(|g| g.number == a.number)
                } else {
                    good.iter().any(|g| g.member == a.by && g.number < a.number)
                };
                if counts && !good.iter().any(|g| g.member == a.member && g.round == a.round) && good.len() < MOST_ENTRIES {
                    good.push(a.clone());
                    false
                } else {
                    true
                }
            });
            if good.len() == before {
                break;
            }
        }
        let rank = |key: &[u8; 32], round: u32| -> Option<Rank> { good.iter().find(|g| g.member == *key && g.round == round).map(|g| (g.number, *key)) };
        let mut gone: Vec<Left> = Vec::new();
        for l in left {
            let again = gone.iter().any(|g| g.member == l.member && g.round == l.round);
            let holds = match (rank(&l.member, l.round), l.removed()) {
                (None, _) => false,
                (Some(_), false) => l.by_round == l.round && identity::verify(&l.member, &left_message(&id, &l.member, l.round), &l.signature),
                (Some(theirs), true) => rank(&l.by, l.by_round).is_some_and(|by| by < theirs) && identity::verify(&l.by, &removed_message(&id, &l.member, l.round, l.by_round), &l.signature),
            };
            if holds && !again {
                gone.push(l);
            }
        }
        Roster { id, founder, added: good, left: gone }
    }

    /// Takes in what `other` knows of the same conference (already
    /// `verified`). `true` if that's anything new.
    pub fn merge(&mut self, other: &Roster) -> bool {
        if other.id != self.id || other.founder != self.founder {
            return false;
        }
        let mut changed = false;
        for a in &other.added {
            if !self.added.iter().any(|mine| mine.member == a.member && mine.round == a.round) && self.added.len() < MOST_ENTRIES {
                self.added.push(a.clone());
                changed = true;
            }
        }
        for l in &other.left {
            if !self.left.iter().any(|mine| mine.member == l.member && mine.round == l.round) && self.left.len() < MOST_ENTRIES {
                self.left.push(l.clone());
                changed = true;
            }
        }
        changed
    }

    /// Who's in it now: added, and not left since, in rank order.
    pub fn members(&self) -> Vec<[u8; 32]> {
        let mut members: Vec<[u8; 32]> = Vec::new();
        for a in &self.added {
            if !members.contains(&a.member) && self.is_member(&a.member) {
                members.push(a.member);
            }
        }
        members.sort_by_key(|k| (self.number(k), *k));
        members
    }

    /// In it now: their latest round's not been left.
    pub fn is_member(&self, key: &[u8; 32]) -> bool {
        self.round(key).is_some_and(|round| !self.left.iter().any(|l| l.member == *key && l.round == round))
    }

    /// They were in it, and have left (and not been added again).
    pub fn has_left(&self, key: &[u8; 32]) -> bool {
        self.round(key).is_some() && !self.is_member(key)
    }

    /// Who removed `member`, if their latest round ended that way.
    pub fn removed_by(&self, member: &[u8; 32]) -> Option<[u8; 32]> {
        let round = self.round(member)?;
        self.left.iter().find(|l| l.member == *member && l.round == round && l.removed()).map(|l| l.by)
    }

    /// Who added `member`, the latest time.
    pub fn added_by(&self, member: &[u8; 32]) -> Option<[u8; 32]> {
        let round = self.round(member)?;
        self.added.iter().find(|a| a.member == *member && a.round == round).map(|a| a.by)
    }
}

/// Of two participants who are each other's peers, which one asks for
/// the link between them: the one with the lower key. So exactly one of
/// them does, whoever learnt of the other first.
pub fn asks(me: &[u8; 32], other: &[u8; 32]) -> bool {
    me < other
}

/// Whom to ask to pass `from`'s voice on: of the members we hear
/// directly (`hearing` says whom each of them hears directly in turn),
/// one who hears `from`. The one asked already (`current`) while they
/// still do, else the lowest key — so a forwarder that leaves is
/// replaced by the next, and nothing changes while they stay. `None`:
/// nobody we hear hears them.
pub fn forwarder(from: &[u8; 32], hearing: &std::collections::HashMap<[u8; 32], Vec<[u8; 32]>>, current: Option<[u8; 32]>) -> Option<[u8; 32]> {
    let hears = |via: &[u8; 32]| via != from && hearing.get(via).is_some_and(|heard| heard.contains(from));
    match current {
        Some(via) if hears(&via) => Some(via),
        _ => hearing.keys().filter(|via| hears(via)).min().copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_roster_keeps_only_additions_by_someone_in_it() {
        let (a, b, c, outsider) = (Identity::generate(), Identity::generate(), Identity::generate(), Identity::generate());
        let mut roster = Roster::start(&a);
        roster.add(&a, b.public_key_bytes());
        roster.add(&b, c.public_key_bytes());
        let mut forged = roster.clone();
        forged.add(&outsider, outsider.public_key_bytes());
        forged.add(&outsider, [7; 32]);
        forged.added.push(Added { member: [9; 32], by: a.public_key_bytes(), round: 0, number: 9, signature: [0; 64] });
        let checked = forged.verified();
        assert_eq!(checked.members(), vec![a.public_key_bytes(), b.public_key_bytes(), c.public_key_bytes()]);
        assert_eq!(checked.added_by(&c.public_key_bytes()), Some(b.public_key_bytes()));
    }

    /// A member's addition numbered (or rounded) as high as goes doesn't
    /// stop anyone being added after it: it once overflowed.
    #[test]
    fn an_addition_numbered_as_high_as_goes_doesnt_stop_the_next() {
        let (a, b, c) = (Identity::generate(), Identity::generate(), Identity::generate());
        let mut roster = Roster::start(&a);
        roster.push_added(&a, b.public_key_bytes(), u32::MAX, u32::MAX);
        let mut theirs = roster.clone();
        theirs.leave(&b);
        roster.merge(&theirs);
        roster.add(&a, c.public_key_bytes());
        roster.add(&a, b.public_key_bytes());
        assert!(roster.is_member(&c.public_key_bytes()));
    }

    #[test]
    fn additions_count_whatever_order_they_arrive_in() {
        let (a, b, c) = (Identity::generate(), Identity::generate(), Identity::generate());
        let mut roster = Roster::start(&a);
        roster.add(&a, b.public_key_bytes());
        roster.add(&b, c.public_key_bytes());
        roster.added.reverse();
        assert_eq!(roster.verified().members().len(), 3);
    }

    #[test]
    fn only_the_one_leaving_can_say_so() {
        let (a, b) = (Identity::generate(), Identity::generate());
        let mut roster = Roster::start(&a);
        roster.add(&a, b.public_key_bytes());
        let mut forged = roster.clone();
        forged.left.push(Left { member: b.public_key_bytes(), round: 0, by: b.public_key_bytes(), by_round: 0, signature: a.sign(&left_message(&roster.id, &b.public_key_bytes(), 0)) });
        assert!(forged.verified().is_member(&b.public_key_bytes()));
        roster.leave(&b);
        let checked = roster.verified();
        assert!(!checked.is_member(&b.public_key_bytes()));
        assert_eq!(checked.members(), vec![a.public_key_bytes()]);
    }

    /// News of the same conference travelling different ways ends up the
    /// same everywhere; another conference's is left alone.
    #[test]
    fn rosters_merge_to_the_same_whatever_way_round() {
        let (a, b, c, d) = (Identity::generate(), Identity::generate(), Identity::generate(), Identity::generate());
        let mut base = Roster::start(&a);
        base.add(&a, b.public_key_bytes());
        let (mut at_a, mut at_b) = (base.clone(), base.clone());
        at_a.add(&a, c.public_key_bytes());
        at_b.add(&b, d.public_key_bytes());
        at_b.leave(&b);
        let (a_was, b_was) = (at_a.clone(), at_b.clone());
        assert!(at_a.merge(&b_was));
        assert!(at_b.merge(&a_was));
        assert!(!at_a.merge(&at_b.clone()), "nothing new the second time");
        let sorted = |r: &Roster| {
            let mut m = r.members();
            m.sort();
            m
        };
        assert_eq!(sorted(&at_a), sorted(&at_b));
        assert_eq!(at_a.members().len(), 3);
        let other = Roster::start(&c);
        assert!(!at_a.merge(&other));
    }

    /// Someone who's left can be added again, and leave again; a leaving
    /// from before doesn't count against the new round.
    #[test]
    fn someone_who_left_can_be_added_again() {
        let (a, b, c) = (Identity::generate(), Identity::generate(), Identity::generate());
        let mut roster = Roster::start(&a);
        roster.add(&a, b.public_key_bytes());
        roster.leave(&b);
        assert!(!roster.is_member(&b.public_key_bytes()));
        roster.add(&a, c.public_key_bytes());
        roster.add(&c, b.public_key_bytes());
        let checked = roster.clone().verified();
        assert!(checked.is_member(&b.public_key_bytes()), "back in");
        assert_eq!(checked.round(&b.public_key_bytes()), Some(1));
        assert_eq!(checked.added_by(&b.public_key_bytes()), Some(c.public_key_bytes()));
        let mut behind = Roster { added: roster.added[..2].to_vec(), left: roster.left.clone(), ..roster.clone() };
        assert!(behind.merge(&checked));
        assert!(behind.is_member(&b.public_key_bytes()));
        roster.leave(&b);
        assert!(!roster.verified().is_member(&b.public_key_bytes()));
    }

    /// Whoever placed the call is 1 and whoever answered 2, whichever of
    /// them made it a conference; each added after is numbered in turn.
    #[test]
    fn members_are_numbered_in_the_order_they_came() {
        let (a, b, c, d) = (Identity::generate(), Identity::generate(), Identity::generate(), Identity::generate());
        let (ak, bk, ck, dk) = (a.public_key_bytes(), b.public_key_bytes(), c.public_key_bytes(), d.public_key_bytes());
        let mut placed = Roster::start(&a);
        placed.add(&a, bk);
        placed.add(&b, ck);
        let placed = placed.verified();
        assert_eq!([placed.number(&ak), placed.number(&bk), placed.number(&ck)], [Some(1), Some(2), Some(3)]);
        let mut answered = Roster::start_answered(&b, ak);
        answered.add(&b, ck);
        answered.add(&c, dk);
        let answered = answered.verified();
        assert_eq!(answered.members(), vec![ak, bk, ck, dk]);
        assert_eq!([answered.number(&ak), answered.number(&bk), answered.number(&ck), answered.number(&dk)], [Some(1), Some(2), Some(3), Some(4)]);
        assert!(answered.outranks(&ak, &bk) && answered.outranks(&bk, &dk) && !answered.outranks(&dk, &ck));
    }

    /// Nobody gets ahead by the number they're given: only the founder
    /// gives out 1 and 2, and anyone else a number above their own.
    #[test]
    fn numbers_given_out_of_turn_are_dropped() {
        let (a, b, c) = (Identity::generate(), Identity::generate(), Identity::generate());
        let (bk, ck) = (b.public_key_bytes(), c.public_key_bytes());
        let mut roster = Roster::start(&a);
        roster.add(&a, bk);
        roster.add(&a, ck);
        for number in [1, 2, 3] {
            let mut forged = roster.clone();
            forged.added.retain(|x| x.member != ck);
            forged.push_added(&c, ck, 0, number);
            forged.added.last_mut().unwrap().by = bk;
            forged.added.last_mut().unwrap().signature = b.sign(&added_message(&forged.id, &ck, 0, number));
            assert_eq!(forged.verified().number(&ck), (number == 3).then_some(3), "B (2) giving C {number}");
        }
        let mut forged = roster.clone();
        forged.added.retain(|x| x.member != ck);
        forged.added.push(Added { member: ck, by: ck, round: 0, number: 1, signature: c.sign(&added_message(&roster.id, &ck, 0, 1)) });
        assert!(!forged.verified().is_member(&ck));
    }

    /// A lower number can remove a higher; never the other way, and not
    /// someone of the same number with a lower key. Someone removed can
    /// be added back, as a new number.
    #[test]
    fn only_someone_who_outranks_them_can_remove_them() {
        let (a, b, c, d) = (Identity::generate(), Identity::generate(), Identity::generate(), Identity::generate());
        let (ak, bk, ck, dk) = (a.public_key_bytes(), b.public_key_bytes(), c.public_key_bytes(), d.public_key_bytes());
        let mut roster = Roster::start(&a);
        roster.add(&a, bk);
        roster.add(&b, ck);
        roster.add(&c, dk);
        assert!(!roster.clone().remove(&c, bk), "3 can't remove 2");
        assert!(!roster.clone().remove(&d, dk), "nor themself, by removing");
        let mut forged = roster.clone();
        forged.left.push(Left { member: bk, round: 0, by: dk, by_round: 0, signature: d.sign(&removed_message(&roster.id, &bk, 0, 0)) });
        assert!(forged.verified().is_member(&bk));
        let mut forged = roster.clone();
        forged.left.push(Left { member: dk, round: 0, by: ak, by_round: 0, signature: c.sign(&removed_message(&roster.id, &dk, 0, 0)) });
        assert!(forged.verified().is_member(&dk));
        assert!(roster.remove(&b, dk), "2 removes 4");
        let checked = roster.clone().verified();
        assert_eq!(checked.members(), vec![ak, bk, ck]);
        assert_eq!(checked.removed_by(&dk), Some(bk));
        assert!(roster.remove(&a, ck), "1 removes 3");
        assert!(!roster.remove(&a, dk), "4's already gone");
        roster.add(&b, dk);
        let checked = roster.verified();
        assert_eq!(checked.members(), vec![ak, bk, dk]);
        assert_eq!(checked.number(&dk), Some(5));
        assert_eq!(checked.removed_by(&dk), None);
    }

    /// Two added at once by different people get the same number, and
    /// are ranked by key, the same everywhere.
    #[test]
    fn the_same_number_is_ranked_by_key() {
        let (a, b, c, d) = (Identity::generate(), Identity::generate(), Identity::generate(), Identity::generate());
        let (ck, dk) = (c.public_key_bytes(), d.public_key_bytes());
        let mut base = Roster::start(&a);
        base.add(&a, b.public_key_bytes());
        let (mut at_a, mut at_b) = (base.clone(), base.clone());
        at_a.add(&a, ck);
        at_b.add(&b, dk);
        at_a.merge(&at_b.clone());
        let merged = at_a.verified();
        assert_eq!((merged.number(&ck), merged.number(&dk)), (Some(3), Some(3)));
        let (high, low) = if ck < dk { (&c, dk) } else { (&d, ck) };
        assert!(merged.outranks(&high.public_key_bytes(), &low));
        let mut removing = merged.clone();
        assert!(removing.remove(high, low));
        assert!(!removing.verified().is_member(&low));
    }

    /// Someone heard only through others: one who hears them is asked,
    /// kept while they do, and replaced by the next when they go.
    #[test]
    fn a_forwarder_is_chosen_kept_and_replaced() {
        use std::collections::HashMap;
        let (x, y, z, carol) = ([1; 32], [2; 32], [3; 32], [9; 32]);
        let mut hearing: HashMap<[u8; 32], Vec<[u8; 32]>> = HashMap::new();
        hearing.insert(z, vec![carol]);
        hearing.insert(y, vec![carol, z]);
        hearing.insert(x, vec![y]);
        assert_eq!(forwarder(&carol, &hearing, None), Some(y), "the lowest key that hears her");
        assert_eq!(forwarder(&carol, &hearing, Some(z)), Some(z), "kept while it hears her");
        hearing.remove(&z);
        assert_eq!(forwarder(&carol, &hearing, Some(z)), Some(y), "replaced once it's gone");
        hearing.get_mut(&y).unwrap().retain(|k| *k != carol);
        assert_eq!(forwarder(&carol, &hearing, Some(y)), None, "nobody left who hears her");
        assert_eq!(forwarder(&y, &hearing, None), Some(x));
    }

    #[test]
    fn exactly_one_of_two_asks() {
        let (x, y) = ([1; 32], [2; 32]);
        assert!(asks(&x, &y) != asks(&y, &x));
    }
}
