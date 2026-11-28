//! Groups: chats of several people, with no server. In the code a group
//! is a channel.
//!
//! A group is identified by whoever made it and a random tag
//! (`ChannelId`), never by its name, which any member can change. Two
//! signed, append-only logs per group:
//!
//! - **the roster** (`RosterEvent`s): who's been added and who's left,
//!   the group's name and its timer, each entry signed by whoever made it;
//! - **the posts** (`ChannelPost`s), each signed by its author, so a post
//!   passed on by any member is as good as the original.
//!
//! `replay` turns the roster into "who's in now". Order comes from the
//! entries themselves, not their timestamps (which the signer chooses):
//! each names the entries its signer had already seen (`prev`), so the log
//! is a graph nobody can rewrite. An entry counts only if its signer was a
//! member as of everything before it in that graph; the very first, the
//! maker putting themselves in, needs nothing before it.
//!
//! There's no owner and no ops: any member adds their own contacts (the
//! node checks they're contacts; the roster can't), and any member leaves,
//! renames the group or sets its timer. Someone added sees what's said from
//! then on (`Roster::joined`), nothing before.
//!
//! A group only exists on this device once it's been made or joined here
//! (`create`, or a roster arriving through `merge_roster`), and tags are
//! checked before they get anywhere near a path (`validate_name`).

use crate::identity::{self, Identity};
use crate::store;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use crate::unix_now;
use crate::wire::hex;

/// The longest group tag accepted, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Whether `name` can be a group's tag (`ChannelId::name`): 1 to
/// `MAX_NAME_CHARS` characters, one line, and safe as part of a directory
/// name on every platform this is meant to run on. A tag this build makes
/// is 32 hex digits; one from anyone else is checked as it arrives, and
/// again by every function here that touches the disk.
pub fn validate_name(name: &str) -> Result<(), String> {
    const FORBIDDEN: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("a group's tag has to be 1 to {MAX_NAME_CHARS} characters"));
    }
    if !crate::text::is_single_line(name) || name.contains(FORBIDDEN) || name == "." || name == ".." || name.ends_with('.') || name.ends_with(' ') {
        return Err("a group's tag can't contain / \\ : * ? \" < > | or control characters, or end in '.' or a space".to_string());
    }
    Ok(())
}

/// A group's identity: whoever made it, and a tag they chose at random.
/// Its name is something else (`Roster::name`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId {
    /// The random tag (32 hex digits).
    pub name: String,
    /// Whoever made it.
    pub owner: [u8; 32],
}

impl ChannelId {
    /// A new group's identity, made by `creator`.
    pub fn new_group(creator: [u8; 32]) -> ChannelId {
        let mut tag = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut tag);
        ChannelId { name: hex(&tag), owner: creator }
    }

    /// A filesystem-safe directory name for this group's local storage
    /// (hex rather than base64 for the key, whose `/` can't be in a path).
    fn dir_name(&self) -> String {
        format!("{}-{}", hex(&self.owner), self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RosterAction {
    /// `subject` is in, added by `by` (a member), or by themselves as the
    /// group's very first entry, when they make it.
    Joined,
    /// `subject` left, of their own accord (`by == subject`).
    Left,
    /// The group's name, set by a member (`subject` is them too). One
    /// line, at most `MAX_GROUP_NAME_CHARS`; empty goes back to the
    /// members' names.
    Named { text: String },
    /// How long posts last before they delete themselves (see
    /// `disappearing`), set by a member; 0, for good. A post keeps the
    /// timer in force when it was written.
    Timer { seconds: u64 },
}

/// The longest group name, in characters.
pub const MAX_GROUP_NAME_CHARS: usize = 64;

/// The most members a group takes (its maker counted): every member keeps
/// every entry, so there has to be a limit.
pub const MAX_MEMBERS: usize = 100;

/// Whether `text` can be a group's name.
pub fn validate_group_name(text: &str) -> Result<(), String> {
    if text.chars().count() > MAX_GROUP_NAME_CHARS {
        return Err(format!("a group's name can be at most {MAX_GROUP_NAME_CHARS} characters"));
    }
    if !crate::text::is_single_line(text) {
        return Err("a group's name has to be one line".to_string());
    }
    Ok(())
}

fn action_tag(action: &RosterAction) -> &'static str {
    match action {
        RosterAction::Joined => "joined",
        RosterAction::Left => "left",
        RosterAction::Named { .. } => "named",
        RosterAction::Timer { .. } => "timer",
    }
}

/// A roster entry's identity: the hash of everything signed plus the
/// signature (see `event_id`).
pub type EventId = [u8; 32];

/// One entry in a group's roster — see `replay` for how a set of these
/// becomes "who's in now".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RosterEvent {
    /// When the signer says this happened. Shown, and where someone's
    /// history starts; never trusted for ordering (see `prev`).
    pub at: u64,
    /// Whoever made it: a member as of everything before it (`prev`), for
    /// it to count (checked by `replay`).
    pub by: [u8; 32],
    /// Who it's about.
    pub subject: [u8; 32],
    pub action: RosterAction,
    /// The newest entries the signer had (`heads`) — what this one comes
    /// after. Signed, so it can't be changed later. Empty for the first.
    pub prev: Vec<EventId>,
    pub signature: [u8; 64],
}

fn parse_event_id(s: &str) -> Option<EventId> {
    if s.len() != 64 {
        return None;
    }
    let mut id = [0u8; 32];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(id)
}

/// What an entry's signature covers. Starts `group`, so nothing signed
/// for another purpose can pass as one.
fn roster_event_signable_bytes(channel: &ChannelId, event: &RosterEvent) -> Vec<u8> {
    let mut s = format!(
        "group\t{}\t{}\t{}\t{}\t{}\t{}",
        channel.name,
        STANDARD.encode(channel.owner),
        event.at,
        STANDARD.encode(event.by),
        STANDARD.encode(event.subject),
        action_tag(&event.action)
    );
    match &event.action {
        RosterAction::Named { text } => {
            s.push('\t');
            s.push_str(text);
        }
        RosterAction::Timer { seconds } => {
            s.push('\t');
            s.push_str(&seconds.to_string());
        }
        RosterAction::Joined | RosterAction::Left => {}
    }
    s.push('\t');
    s.push_str(&event.prev.iter().map(|id| hex(id)).collect::<Vec<_>>().join(","));
    s.into_bytes()
}

/// `event`'s id: a hash over exactly what was signed, and the signature.
pub fn event_id(channel: &ChannelId, event: &RosterEvent) -> EventId {
    let mut hasher = Sha256::new();
    hasher.update(roster_event_signable_bytes(channel, event));
    hasher.update(event.signature);
    hasher.finalize().into()
}

/// Signs a new roster entry as `identity` (who becomes `by`) about
/// `subject`, coming after `prev` — normally `heads` of the roster this
/// device has. Whether `identity` may is `replay`'s to say, once it
/// reaches anyone; one that may not is ignored there.
pub fn sign_roster_event(channel: &ChannelId, identity: &Identity, prev: Vec<EventId>, subject: [u8; 32], action: RosterAction) -> RosterEvent {
    let mut event = RosterEvent { at: unix_now(), by: identity.public_key_bytes(), subject, action, prev, signature: [0; 64] };
    event.signature = identity.sign(&roster_event_signable_bytes(channel, &event));
    event
}

pub fn verify_roster_event(channel: &ChannelId, event: &RosterEvent) -> bool {
    let fits = match &event.action {
        RosterAction::Named { text } => validate_group_name(text).is_ok(),
        _ => true,
    };
    fits && event.prev.len() <= MAX_PREV && identity::verify(&event.by, &roster_event_signable_bytes(channel, event), &event.signature)
}

/// The most earlier entries one roster entry may name as its `prev`.
pub const MAX_PREV: usize = 32;

/// The entries in `events` that nothing else in it comes after — what a
/// new entry signed here should name as its `prev` (at most `MAX_PREV`;
/// naming fewer than all only makes the new one look concurrent with the
/// rest). Only entries that verify count.
pub fn heads(channel: &ChannelId, events: &[RosterEvent]) -> Vec<EventId> {
    let valid: Vec<&RosterEvent> = events.iter().filter(|e| verify_roster_event(channel, e)).collect();
    let referenced: HashSet<EventId> = valid.iter().flat_map(|e| e.prev.iter().copied()).collect();
    let mut heads: Vec<EventId> = valid.iter().map(|e| event_id(channel, e)).filter(|id| !referenced.contains(id)).collect();
    heads.sort();
    heads.dedup();
    heads.truncate(MAX_PREV);
    heads
}

/// Someone in a group now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// When they were (last) added: where their history starts.
    pub joined_at: u64,
    /// Who added them (themselves, for whoever made the group).
    pub added_by: [u8; 32],
}

/// A group's name, and who set it when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupName {
    pub text: String,
    pub by: [u8; 32],
    pub at: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Roster {
    pub members: HashMap<[u8; 32], Member>,
    /// The name a member gave it; none, the members' names.
    pub name: Option<GroupName>,
    /// Each time a member set the timer: when, and to how long, in order.
    pub timers: Vec<(u64, u64)>,
    /// When each one who's ever been in was: from their being added to
    /// their leaving (`None`: still in).
    pub spans: HashMap<[u8; 32], Vec<Span>>,
}

/// A stretch of someone's membership (see `Roster::spans`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub from: u64,
    pub until: Option<u64>,
}

/// How far a post's time may be from its author's joining or leaving:
/// their clock and the adder's needn't agree.
const SPAN_SLACK: u64 = 5 * 60;

impl Roster {
    /// Whether a post `key` wrote at `at` was written as a member — so one
    /// who's since left still has what they said kept, by everyone
    /// catching up.
    pub fn wrote_as_member(&self, key: &[u8; 32], at: u64) -> bool {
        self.is_member(key)
            || self.spans.get(key).is_some_and(|spans| {
                spans.iter().any(|s| s.from <= at.saturating_add(SPAN_SLACK) && s.until.is_none_or(|until| at <= until.saturating_add(SPAN_SLACK)))
            })
    }

    /// Whether `key` is in the group now.
    pub fn is_member(&self, key: &[u8; 32]) -> bool {
        self.members.contains_key(key)
    }

    /// When `key` was (last) added, if they're in: what's said from then
    /// on is theirs to see, nothing before.
    pub fn joined(&self, key: &[u8; 32]) -> Option<u64> {
        self.members.get(key).map(|m| m.joined_at)
    }

    /// Whether `key` has ever been in it.
    pub fn ever_in(&self, key: &[u8; 32]) -> bool {
        self.spans.contains_key(key)
    }

    /// The timer now (0, off).
    pub fn timer(&self) -> u64 {
        self.timers.last().map_or(0, |(_, s)| *s)
    }

    /// When a post written at `at` deletes itself: under the timer in
    /// force then (0, never).
    pub fn expires(&self, at: u64) -> u64 {
        let seconds = self.timers.iter().rev().find(|(set, _)| *set <= at).map_or(0, |(_, s)| *s);
        crate::disappearing::expires(at, seconds)
    }
}

/// Works out who's in now from a set of entries:
///
/// 1. Entries that don't verify, repeats, and entries whose `prev` names
///    something not in the set (they can't be placed yet) are dropped.
/// 2. The rest are put in an order where every entry comes after all of
///    its `prev` (ties broken by timestamp, then id, so everyone with the
///    same entries gets the same order).
/// 3. An entry counts if its signer is a member as of the entries that
///    came before it in the graph (its ancestors), not as of whatever
///    happens to be earlier in the order. The group's maker putting
///    themselves in, with nothing before it, starts it. A `Left` counts
///    only from the one leaving; a name or timer only as the signer's own.
/// 4. The entries that count are applied in order.
///
/// An entry's time is the signer's clock, so it's never taken as earlier
/// than the entries it comes after: nobody can date a joining or a timer
/// back past what they'd seen.
pub fn replay(channel: &ChannelId, events: &[RosterEvent]) -> Roster {
    replay_with(channel, events, &HashMap::new())
}

/// `replay`, with when this device first had each timer change
/// (`timers_seen`): a timer is in force here from no earlier than that,
/// less `SPAN_SLACK`, so one dated back can't delete what's already here.
fn replay_with(channel: &ChannelId, events: &[RosterEvent], seen: &HashMap<EventId, u64>) -> Roster {
    let mut roster = Roster::default();
    let mut effective: HashMap<EventId, u64> = HashMap::new();
    for event in counted(channel, events) {
        let id = event_id(channel, event);
        // Dated ahead (a fast clock), it counts as no later than a little
        // past now: nothing after it is pushed into the future with it.
        let own = event.at.min(unix_now() + AHEAD_SLACK);
        let at = event.prev.iter().filter_map(|p| effective.get(p)).fold(own, |at, p| at.max(*p));
        effective.insert(id, at);
        let at = match (&event.action, seen.get(&id)) {
            (RosterAction::Timer { .. }, Some(seen)) => at.max(seen.saturating_sub(SPAN_SLACK)),
            _ => at,
        };
        apply(&mut roster, event, at);
    }
    roster.timers.sort_by_key(|(at, _)| *at);
    roster
}

/// The roster as this device should act on it: `replay`, with timers in
/// force from no earlier than they reached here (see `replay_with`).
pub fn replay_here(base: &Path, channel: &ChannelId) -> std::io::Result<Roster> {
    Ok(replay_with(channel, &load_roster(base, channel)?, &load_timers_seen(base, channel)?))
}

/// `event` applied, as of `at` (its time, never earlier than what it
/// comes after: see `replay`).
fn apply(roster: &mut Roster, event: &RosterEvent, at: u64) {
    match &event.action {
        RosterAction::Joined => {
            if !roster.members.contains_key(&event.subject) {
                roster.spans.entry(event.subject).or_default().push(Span { from: at, until: None });
                roster.members.insert(event.subject, Member { joined_at: at, added_by: event.by });
            }
        }
        RosterAction::Left => {
            if roster.members.remove(&event.subject).is_some()
                && let Some(span) = roster.spans.get_mut(&event.subject).and_then(|s| s.last_mut())
            {
                span.until = Some(at);
            }
        }
        RosterAction::Named { text } => {
            roster.name = (!text.trim().is_empty()).then(|| GroupName { text: text.clone(), by: event.by, at });
        }
        RosterAction::Timer { seconds } => roster.timers.push((at, *seconds)),
    }
}

/// The entries of `events` that count (steps 1 to 3 of `replay`), in
/// order — what actually happened, for showing.
pub fn counted<'a>(channel: &ChannelId, events: &'a [RosterEvent]) -> Vec<&'a RosterEvent> {
    crate::memo::counted(channel, events, || work_out(channel, events))
}

fn work_out<'a>(channel: &ChannelId, events: &'a [RosterEvent]) -> Vec<&'a RosterEvent> {
    let mut by_id: HashMap<EventId, &RosterEvent> = HashMap::new();
    for event in events {
        if verify_roster_event(channel, event) {
            by_id.entry(event_id(channel, event)).or_insert(event);
        }
    }
    loop {
        let orphans: Vec<EventId> = by_id.iter().filter(|(_, e)| e.prev.iter().any(|p| !by_id.contains_key(p))).map(|(id, _)| *id).collect();
        if orphans.is_empty() {
            break;
        }
        for id in orphans {
            by_id.remove(&id);
        }
    }

    let mut waiting_on: HashMap<EventId, usize> = by_id.iter().map(|(id, e)| (*id, e.prev.len())).collect();
    let mut children: HashMap<EventId, Vec<EventId>> = HashMap::new();
    for (id, e) in &by_id {
        for p in &e.prev {
            children.entry(*p).or_default().push(*id);
        }
    }
    let mut ready: BTreeSet<(u64, EventId)> = waiting_on.iter().filter(|(_, n)| **n == 0).map(|(id, _)| (by_id[id].at, *id)).collect();
    let mut order: Vec<EventId> = Vec::with_capacity(by_id.len());
    while let Some((_, id)) = ready.pop_first() {
        order.push(id);
        for child in children.get(&id).into_iter().flatten() {
            let n = waiting_on.get_mut(child).expect("every child is in by_id");
            *n -= 1;
            if *n == 0 {
                ready.insert((by_id[child].at, *child));
            }
        }
    }
    let index: HashMap<EventId, usize> = order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let ordered: Vec<&RosterEvent> = order.iter().map(|id| by_id[id]).collect();
    let n = ordered.len();

    let mut ancestors: Vec<Bits> = Vec::with_capacity(n);
    for event in &ordered {
        let mut set = Bits::new(n);
        for p in &event.prev {
            let pi = index[p];
            set.insert(pi);
            let (set_words, theirs) = (&mut set.0, &ancestors[pi].0);
            for (w, t) in set_words.iter_mut().zip(theirs) {
                *w |= t;
            }
        }
        ancestors.push(set);
    }

    let mut counts = vec![false; n];
    for i in 0..n {
        let e = ordered[i];
        let mut members: HashSet<[u8; 32]> = HashSet::new();
        for a in ancestors[i].iter() {
            if counts[a] {
                match ordered[a].action {
                    RosterAction::Joined => {
                        members.insert(ordered[a].subject);
                    }
                    RosterAction::Left => {
                        members.remove(&ordered[a].subject);
                    }
                    _ => {}
                }
            }
        }
        let starts_it = e.prev.is_empty() && e.by == channel.owner && e.subject == channel.owner && e.action == RosterAction::Joined;
        counts[i] = match e.action {
            RosterAction::Joined => starts_it || members.contains(&e.by),
            RosterAction::Left => e.by == e.subject && members.contains(&e.by),
            RosterAction::Named { .. } | RosterAction::Timer { .. } => e.subject == e.by && members.contains(&e.by),
        };
    }
    ordered.into_iter().enumerate().filter(|(i, _)| counts[*i]).map(|(_, e)| e).collect()
}

/// A set of positions, a bit each (see `work_out`).
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Bits {
        Bits(vec![0; n.div_ceil(64)])
    }

    fn insert(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }

    /// The positions in it, lowest first.
    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(w, &word)| {
            let mut word = word;
            std::iter::from_fn(move || {
                (word != 0).then(|| {
                    let bit = word.trailing_zeros() as usize;
                    word &= word - 1;
                    w * 64 + bit
                })
            })
        })
    }
}

/// The newest counted entry about `key` (their being added, or their
/// leaving) — what tells someone they're in a group, or that they left.
pub fn latest_about<'a>(channel: &ChannelId, events: &'a [RosterEvent], key: &[u8; 32]) -> Option<&'a RosterEvent> {
    counted(channel, events).into_iter().rev().find(|e| e.subject == *key && matches!(e.action, RosterAction::Joined | RosterAction::Left))
}

/// One message in a group. `author` travels with it, since a post can
/// reach you passed on by any member, not just whoever wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPost {
    pub author: [u8; 32],
    pub created_at: u64,
    pub text: String,
    /// The name the author went by when they wrote it (their display
    /// name, see `crate::displayname`), signed with the post, so a chat
    /// shows each line under the name it was said under, and nobody
    /// passing it on can change it. `None`: they hadn't set one.
    pub name: Option<String>,
    pub signature: [u8; 64],
}

/// What a post's signature covers (starting `group post`, as nothing
/// signed for a channel can pass as one). With a name, it's one more
/// field before the text; neither can contain a tab.
fn channel_post_signable_bytes(channel: &ChannelId, author: [u8; 32], created_at: u64, name: Option<&str>, text: &str) -> Vec<u8> {
    let head = format!("group post\t{}\t{}\t{}\t{created_at}", channel.name, STANDARD.encode(channel.owner), STANDARD.encode(author));
    match name {
        Some(name) => format!("{head}\t{name}\t{text}").into_bytes(),
        None => format!("{head}\t\t{text}").into_bytes(),
    }
}

/// Signs `text` as a post to `channel`, under `name` (the author's
/// current display name, if they have one). Control characters in the
/// text but line breaks become spaces first (see `crate::text`); a name
/// that isn't a valid display name is left off.
pub fn sign_channel_post(channel: &ChannelId, identity: &Identity, text: &str, name: Option<&str>) -> ChannelPost {
    let text = crate::text::message_text(text);
    let name = name.filter(|n| crate::displayname::is_valid_name(n)).map(str::to_string);
    let created_at = unix_now();
    let author = identity.public_key_bytes();
    let signature = identity.sign(&channel_post_signable_bytes(channel, author, created_at, name.as_deref(), &text));
    ChannelPost { author, created_at, text, name, signature }
}

/// Checks the signature (and that the text has no control characters
/// but line breaks). Whether the author was in the group is the roster's
/// to say (`Roster::wrote_as_member`, which `merge_posts` asks).
pub fn verify_channel_post(channel: &ChannelId, post: &ChannelPost) -> bool {
    crate::text::is_message_text(&post.text)
        && post.text.len() <= crate::posts::MAX_POST_BYTES
        && post.name.as_deref().is_none_or(crate::displayname::is_valid_name)
        && identity::verify(&post.author, &channel_post_signable_bytes(channel, post.author, post.created_at, post.name.as_deref(), &post.text), &post.signature)
}

pub(crate) fn format_roster_event(event: &RosterEvent) -> String {
    let mut line = format!(
        "{}\t{}\t{}\t{}\t{}",
        event.at,
        STANDARD.encode(event.by),
        STANDARD.encode(event.subject),
        STANDARD.encode(event.signature),
        action_tag(&event.action)
    );
    match &event.action {
        RosterAction::Named { text } => {
            line.push('\t');
            line.push_str(text);
        }
        RosterAction::Timer { seconds } => {
            line.push('\t');
            line.push_str(&seconds.to_string());
        }
        RosterAction::Joined | RosterAction::Left => {}
    }
    line.push_str("\tp=");
    line.push_str(&event.prev.iter().map(|id| hex(id)).collect::<Vec<_>>().join(","));
    line.push('\n');
    line
}

pub(crate) fn parse_roster_event(line: &str) -> Option<RosterEvent> {
    let mut parts = line.split('\t');
    let at = parts.next()?.parse().ok()?;
    let by: [u8; 32] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let subject: [u8; 32] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let signature: [u8; 64] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let action = match parts.next()? {
        "joined" => RosterAction::Joined,
        "left" => RosterAction::Left,
        "named" => RosterAction::Named { text: parts.next()?.to_string() },
        "timer" => RosterAction::Timer { seconds: parts.next()?.parse().ok()? },
        _ => return None,
    };
    let prev = parts.next()?.strip_prefix("p=")?;
    let prev = if prev.is_empty() { Vec::new() } else { prev.split(',').map(parse_event_id).collect::<Option<Vec<_>>>()? };
    if parts.next().is_some() {
        return None;
    }
    Some(RosterEvent { at, by, subject, action, prev, signature })
}

/// `<at>\t<author>\t<signature>[\t<name>]\t<text>` — the name only when
/// there is one (neither it nor the text can hold a tab, so the field
/// count says which).
pub(crate) fn format_channel_post(post: &ChannelPost) -> String {
    let head = format!("{}\t{}\t{}", post.created_at, STANDARD.encode(post.author), STANDARD.encode(post.signature));
    match &post.name {
        Some(name) => format!("{head}\t{name}\t{}\n", crate::text::to_field(&post.text)),
        None => format!("{head}\t{}\n", crate::text::to_field(&post.text)),
    }
}

pub(crate) fn parse_channel_post(line: &str) -> Option<ChannelPost> {
    let parts: Vec<&str> = line.split('\t').collect();
    let (name, text) = match parts.len() {
        4 => (None, parts[3]),
        5 => (Some(parts[3].to_string()), parts[4]),
        _ => return None,
    };
    let created_at = parts[0].parse().ok()?;
    let author: [u8; 32] = STANDARD.decode(parts[1]).ok()?.try_into().ok()?;
    let signature: [u8; 64] = STANDARD.decode(parts[2]).ok()?.try_into().ok()?;
    Some(ChannelPost { author, created_at, name, signature, text: crate::text::from_field(text) })
}

/// Where `channel` lives on disk — or an error, never a path, for a tag
/// `validate_name` refuses.
fn channel_dir(base: &Path, channel: &ChannelId) -> std::io::Result<PathBuf> {
    validate_name(&channel.name).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    Ok(store::named(base, &channel.dir_name()))
}

/// Whether this device has `channel` at all — made or joined here.
pub fn exists(base: &Path, channel: &ChannelId) -> bool {
    channel_dir(base, channel).map(|dir| dir.join("roster").exists()).unwrap_or(false)
}

/// Creates `channel` on this device (an empty roster, if it has none
/// yet), so it `exists`.
pub fn create(base: &Path, channel: &ChannelId) -> std::io::Result<()> {
    let dir = channel_dir(base, channel)?;
    fs::create_dir_all(&dir)?;
    let _guard = store::lock();
    let roster = dir.join("roster");
    if !roster.exists() {
        store::write_atomic(&roster, "")?;
    }
    Ok(())
}

/// `channel`'s roster as stored. Kept in memory between reads (see
/// `store::Kept`): it's asked for on every redraw of a group.
pub fn load_roster(base: &Path, channel: &ChannelId) -> std::io::Result<Vec<RosterEvent>> {
    static KEPT: store::Kept<RosterEvent> = store::Kept::new(32);
    KEPT.read(&channel_dir(base, channel)?.join("roster"), parse_roster_event)
}

/// Appends one entry this device just made. Entries from elsewhere go
/// through `merge_roster` instead.
pub fn append_roster_event(base: &Path, channel: &ChannelId, event: &RosterEvent) -> std::io::Result<()> {
    let _guard = store::lock();
    store::append_line(&channel_dir(base, channel)?.join("roster"), format_roster_event(event).trim_end())?;
    note_timers_seen(base, channel, std::slice::from_ref(event))
}

/// How far ahead of this device's clock a roster entry's time is taken
/// as it is: clocks needn't agree, but one from the future would push
/// everything after it into the future too. Later, it counts as this far
/// ahead (see `replay_with`; posts, see `merge_posts`).
pub const AHEAD_SLACK: u64 = 10 * 60;

/// When this device first had each of `channel`'s timer changes, by
/// entry id (`timers_seen`, one `<id hex> <unix time>` a line).
fn load_timers_seen(base: &Path, channel: &ChannelId) -> std::io::Result<HashMap<EventId, u64>> {
    let path = channel_dir(base, channel)?.join("timers_seen");
    if !path.exists() {
        return Ok(HashMap::new());
    }
    Ok(store::read(&path)?
        .lines()
        .filter_map(|line| {
            let (id, at) = line.split_once(' ')?;
            Some((parse_event_id(id)?, at.parse().ok()?))
        })
        .collect())
}

/// Notes the timer changes among `events` as had here now (store lock
/// held by the caller).
fn note_timers_seen(base: &Path, channel: &ChannelId, events: &[RosterEvent]) -> std::io::Result<()> {
    let path = channel_dir(base, channel)?.join("timers_seen");
    let now = unix_now();
    for event in events.iter().filter(|e| matches!(e.action, RosterAction::Timer { .. })) {
        store::append_line(&path, &format!("{} {now}", hex(&event_id(channel, event))))?;
    }
    Ok(())
}

/// Merges fetched roster entries in, keeping only ones that verify and
/// aren't already stored (by `event_id`). Returns the ones that were new
/// here — what's worth passing on.
pub fn merge_roster(base: &Path, channel: &ChannelId, fetched: &[RosterEvent]) -> std::io::Result<Vec<RosterEvent>> {
    let dir = channel_dir(base, channel)?;
    let path = dir.join("roster");
    let _guard = store::lock();
    let mut all = if path.exists() { load_roster(base, channel)? } else { Vec::new() };
    let mut known: HashSet<EventId> = all.iter().map(|e| event_id(channel, e)).collect();
    let mut new = Vec::new();
    for event in fetched {
        if verify_roster_event(channel, event) && known.insert(event_id(channel, event)) {
            all.push(event.clone());
            new.push(event.clone());
        }
    }
    if !new.is_empty() || !path.exists() {
        fs::create_dir_all(&dir)?;
        let contents: String = all.iter().map(format_roster_event).collect();
        store::write_atomic(&path, contents)?;
        note_timers_seen(base, channel, &new)?;
    }
    Ok(new)
}

/// `channel`'s posts as stored, oldest first. Kept in memory between
/// reads, like the roster.
pub fn load_posts(base: &Path, channel: &ChannelId) -> std::io::Result<Vec<ChannelPost>> {
    POSTS_KEPT.read(&channel_dir(base, channel)?.join("posts"), parse_channel_post)
}

/// `f` of `channel`'s posts, without a copy of them all (see
/// `store::Kept::with`): for counting them, or picking a few out.
pub fn with_posts<R>(base: &Path, channel: &ChannelId, f: impl FnOnce(&[ChannelPost]) -> R) -> std::io::Result<R> {
    POSTS_KEPT.with(&channel_dir(base, channel)?.join("posts"), parse_channel_post, f)
}

static POSTS_KEPT: store::Kept<ChannelPost> = store::Kept::new(32);

/// The most posts kept per group: older ones are dropped as new ones
/// arrive, so a busy group's history can't grow without bound.
pub const MAX_POSTS: usize = 5000;

/// The most groups kept on one device, and the most of those that being
/// added to has made but that aren't caught up on yet (see
/// `node::channels`): what a contact adding us to group after group can
/// leave behind.
pub const MOST_GROUPS: usize = 1000;
pub const MOST_WAITING_GROUPS: usize = 20;

/// Merges fetched posts in, keeping only ones that verify, were written
/// by a member at the time (per the roster stored here — so merge the
/// roster first), aren't from before `me` (this device's user) was
/// (last) added, and aren't already stored. A post passed on by anyone is
/// as checkable as one from its author.
///
/// Keeps the newest `MAX_POSTS`. Returns the posts that were new here
/// (and kept) — what's worth passing on.
pub fn merge_posts(base: &Path, channel: &ChannelId, me: &[u8; 32], fetched: &[ChannelPost]) -> std::io::Result<Vec<ChannelPost>> {
    let path = channel_dir(base, channel)?.join("posts");
    let roster = replay_here(base, channel)?;
    let Some(joined) = roster.joined(me) else { return Ok(Vec::new()) };
    let _guard = store::lock();
    let mut all = load_posts(base, channel)?;
    let mut known: HashSet<[u8; 64]> = all.iter().map(|p| p.signature).collect();
    let mut new = Vec::new();
    let now = unix_now();
    for post in fetched {
        let expires = roster.expires(post.created_at);
        let in_time = expires == 0 || expires > now;
        if post.created_at >= joined
            && in_time
            && verify_channel_post(channel, post)
            && roster.wrote_as_member(&post.author, post.created_at)
            && known.insert(post.signature)
        {
            all.push(post.clone());
            new.push(post.clone());
        }
    }
    if new.is_empty() {
        return Ok(new);
    }
    all.sort_by_key(|p| p.created_at);
    if all.len() > MAX_POSTS {
        // The oldest go, and before them any dated well ahead of this
        // device's clock (a fast clock, or posts sent to push out the
        // rest): those are kept until the group's full, then go first.
        let ahead = now + AHEAD_SLACK;
        let mut order: Vec<usize> = (0..all.len()).collect();
        order.sort_by_key(|&i| (all[i].created_at <= ahead, all[i].created_at, i));
        let gone: HashSet<[u8; 64]> = order[..all.len() - MAX_POSTS].iter().map(|&i| all[i].signature).collect();
        all.retain(|p| !gone.contains(&p.signature));
        new.retain(|p| !gone.contains(&p.signature));
    }
    let contents: String = all.iter().map(format_channel_post).collect();
    store::write_atomic(&path, contents)?;
    Ok(new)
}

/// Deletes every post whose time is up by `now` (see
/// `RosterAction::Timer`) from every group under `base`. How many went.
pub fn sweep(base: &Path, now: u64) -> std::io::Result<usize> {
    let mut gone = 0;
    for channel in known_channels(base)? {
        let roster = replay_here(base, &channel)?;
        if roster.timers.iter().all(|(_, s)| *s == 0) {
            continue;
        }
        let path = channel_dir(base, &channel)?.join("posts");
        let _guard = store::lock();
        let all = load_posts(base, &channel)?;
        let kept: Vec<&ChannelPost> = all.iter().filter(|p| roster.expires(p.created_at) == 0 || roster.expires(p.created_at) > now).collect();
        if kept.len() < all.len() {
            gone += all.len() - kept.len();
            store::write_atomic(&path, kept.into_iter().map(format_channel_post).collect::<String>())?;
        }
    }
    Ok(gone)
}

/// Deletes every group `me` was told they'd been added to but never
/// caught up on (so never in it, as far as its roster here knows) whose
/// roster is older than `wait`. How many went.
pub fn drop_waiting(base: &Path, me: &[u8; 32], wait: std::time::Duration) -> std::io::Result<usize> {
    let mut gone = 0;
    for channel in known_channels(base)? {
        let roster = replay(&channel, &load_roster(base, &channel)?);
        if roster.ever_in(me) {
            continue;
        }
        let made = fs::metadata(channel_dir(base, &channel)?.join("roster")).and_then(|m| m.modified());
        if made.is_ok_and(|made| made.elapsed().is_ok_and(|age| age > wait)) {
            delete(base, &channel)?;
            gone += 1;
        }
    }
    Ok(gone)
}

/// Catching up: what one side has, summed up per hour so the other can
/// tell what it's missing without a list of every post. A bucket is an
/// hour of `created_at`; its digest the count and an XOR over the posts'
/// signatures — any difference in what two sides hold for that hour
/// shows up in one or the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketDigest {
    pub bucket: u64,
    pub count: u16,
    pub xor: u64,
}

/// The longest stretch a catch-up compares: this many of the newest
/// hours that have posts in them. Older history is left as it is.
pub const MAX_DIGESTS: usize = 200;
/// The most posts one catch-up answer carries (the newest of what's
/// missing).
pub const MAX_CATCH_UP_POSTS: usize = 1000;

fn bucket_of(post: &ChannelPost) -> u64 {
    post.created_at / 3600
}

/// Our side of a catch-up: digests of the newest `MAX_DIGESTS` hours we
/// have posts in, and the oldest hour worth sending us anything from
/// (0 for everything, when we hold all we've ever had).
pub fn digests(posts: &[ChannelPost]) -> (u64, Vec<BucketDigest>) {
    let mut by_bucket: std::collections::BTreeMap<u64, BucketDigest> = std::collections::BTreeMap::new();
    for post in posts {
        let bucket = bucket_of(post);
        let d = by_bucket.entry(bucket).or_insert(BucketDigest { bucket, count: 0, xor: 0 });
        d.count = d.count.saturating_add(1);
        d.xor ^= u64::from_le_bytes(post.signature[..8].try_into().expect("8 bytes"));
    }
    let all: Vec<BucketDigest> = by_bucket.into_values().collect();
    let newest: Vec<BucketDigest> = all[all.len().saturating_sub(MAX_DIGESTS)..].to_vec();
    let from = if all.len() > MAX_DIGESTS || posts.len() >= MAX_POSTS { newest.first().map(|d| d.bucket).unwrap_or(0) } else { 0 };
    (from, newest)
}

/// Their side of a catch-up: of `ours`, what the asker (who sent `from`
/// and `theirs`, and was added at `joined`) is missing — every post from
/// when they were added on, in an hour from `from` on whose digest
/// differs from theirs, newest `MAX_CATCH_UP_POSTS` at most.
pub fn missing_for(ours: &[ChannelPost], joined: u64, from: u64, theirs: &[BucketDigest]) -> Vec<ChannelPost> {
    let theirs_now: Vec<ChannelPost> = ours.iter().filter(|p| p.created_at >= joined).cloned().collect();
    let (_, mine) = digests(&theirs_now);
    let theirs: HashMap<u64, &BucketDigest> = theirs.iter().map(|d| (d.bucket, d)).collect();
    let differ: HashSet<u64> = mine
        .iter()
        .filter(|d| d.bucket >= from && theirs.get(&d.bucket).is_none_or(|t| t.count != d.count || t.xor != d.xor))
        .map(|d| d.bucket)
        .collect();
    let mut missing: Vec<ChannelPost> = theirs_now.into_iter().filter(|p| differ.contains(&bucket_of(p))).collect();
    missing.sort_by_key(|p| p.created_at);
    let skip = missing.len().saturating_sub(MAX_CATCH_UP_POSTS);
    missing.drain(..skip);
    missing
}

/// Deletes `channel`'s posts here, keeping its roster (which still says
/// this user left, to anyone who hasn't heard).
pub fn delete_posts(base: &Path, channel: &ChannelId) -> std::io::Result<()> {
    let path = channel_dir(base, channel)?.join("posts");
    let _guard = store::lock();
    match fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Deletes everything stored here for `channel` — it's as if it had
/// never been made or joined here.
pub fn delete(base: &Path, channel: &ChannelId) -> std::io::Result<()> {
    let dir = channel_dir(base, channel)?;
    let _guard = store::lock();
    match fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Every group this device knows about locally (has a directory for).
/// An entry that doesn't parse back into a `ChannelId` is skipped.
pub fn known_channels(base: &Path) -> std::io::Result<Vec<ChannelId>> {
    if !base.exists() {
        return Ok(Vec::new());
    }
    let mut channels = Vec::new();
    for (dir_name, _) in store::names_in(base) {
        let Some((hex, name)) = dir_name.split_once('-') else { continue };
        if hex.len() != 64 {
            continue;
        }
        let mut owner = [0u8; 32];
        let mut valid = true;
        for (i, byte) in owner.iter_mut().enumerate() {
            match u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
                Ok(b) => *byte = b,
                Err(_) => {
                    valid = false;
                    break;
                }
            }
        }
        if valid && validate_name(name).is_ok() {
            channels.push(ChannelId { name: name.to_string(), owner });
        }
    }
    channels.sort();
    Ok(channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-group-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A group made by `maker`, with its first entry (`maker` in).
    fn group(maker: &Identity) -> (ChannelId, RosterEvent) {
        let channel = ChannelId::new_group(maker.public_key_bytes());
        let first = sign_roster_event(&channel, maker, Vec::new(), maker.public_key_bytes(), RosterAction::Joined);
        (channel, first)
    }

    /// An entry by `signer`, coming after `after`.
    fn ev(channel: &ChannelId, signer: &Identity, after: &[&RosterEvent], subject: &Identity, action: RosterAction) -> RosterEvent {
        let prev = after.iter().map(|e| event_id(channel, e)).collect();
        sign_roster_event(channel, signer, prev, subject.public_key_bytes(), action)
    }

    fn redate(channel: &ChannelId, by: &Identity, mut e: RosterEvent, at: u64) -> RosterEvent {
        e.at = at;
        e.signature = by.sign(&roster_event_signable_bytes(channel, &e));
        e
    }

    fn post_at(channel: &ChannelId, by: &Identity, at: u64, text: &str) -> ChannelPost {
        let mut p = sign_channel_post(channel, by, text, None);
        p.created_at = at;
        p.signature = by.sign(&channel_post_signable_bytes(channel, p.author, at, None, text));
        p
    }

    fn is_member(roster: &Roster, who: &Identity) -> bool {
        roster.is_member(&who.public_key_bytes())
    }

    #[test]
    fn a_post_over_several_lines_is_signed_stored_and_read_back_as_written() {
        let ann = Identity::generate();
        let (channel, _) = group(&ann);
        let post = sign_channel_post(&channel, &ann, "one\r\ntwo\tthree", None);
        assert_eq!(post.text, "one\ntwo three");
        assert!(verify_channel_post(&channel, &post));
        let line = format_channel_post(&post);
        assert_eq!(line.matches('\n').count(), 1, "one line in the file");
        let back = parse_channel_post(line.trim_end_matches('\n')).unwrap();
        assert_eq!(back.text, "one\ntwo three");
        assert!(verify_channel_post(&channel, &back));
        let mut tabbed = post.clone();
        tabbed.text = "one\ttwo".to_string();
        assert!(!verify_channel_post(&channel, &tabbed), "other control characters still refused");
    }

    #[test]
    fn a_new_group_has_a_tag_of_its_own_that_is_safe_on_disk() {
        let me = Identity::generate();
        let (a, b) = (ChannelId::new_group(me.public_key_bytes()), ChannelId::new_group(me.public_key_bytes()));
        assert_ne!(a, b);
        assert_eq!(a.name.len(), 32);
        assert!(validate_name(&a.name).is_ok());
    }

    #[test]
    fn the_maker_starts_it_and_any_member_adds_others() {
        let (ann, bob, cat) = (Identity::generate(), Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        assert!(is_member(&replay(&channel, std::slice::from_ref(&first)), &ann));
        let bob_in = ev(&channel, &ann, &[&first], &bob, RosterAction::Joined);
        let cat_in = ev(&channel, &bob, &[&bob_in], &cat, RosterAction::Joined);
        let roster = replay(&channel, &[first, bob_in, cat_in]);
        assert!(is_member(&roster, &ann) && is_member(&roster, &bob) && is_member(&roster, &cat));
        assert_eq!(roster.members[&cat.public_key_bytes()].added_by, bob.public_key_bytes());
    }

    #[test]
    fn nobody_outside_can_add_or_start_it_for_someone_else() {
        let (ann, bob, eve) = (Identity::generate(), Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let eve_adds = ev(&channel, &eve, &[&first], &bob, RosterAction::Joined);
        let eve_starts = ev(&channel, &eve, &[], &eve, RosterAction::Joined);
        let roster = replay(&channel, &[first, eve_adds, eve_starts]);
        assert!(!is_member(&roster, &bob) && !is_member(&roster, &eve));
    }

    #[test]
    fn a_member_leaves_by_themselves_and_can_be_added_back() {
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let bob_in = ev(&channel, &ann, &[&first], &bob, RosterAction::Joined);
        let ann_puts_bob_out = ev(&channel, &ann, &[&bob_in], &bob, RosterAction::Left);
        assert!(is_member(&replay(&channel, &[first.clone(), bob_in.clone(), ann_puts_bob_out]), &bob), "only bob can say he's left");
        let bob_out = redate(&channel, &bob, ev(&channel, &bob, &[&bob_in], &bob, RosterAction::Left), bob_in.at + 10);
        let roster = replay(&channel, &[first.clone(), bob_in.clone(), bob_out.clone()]);
        assert!(!is_member(&roster, &bob) && roster.ever_in(&bob.public_key_bytes()));
        let renamed = ev(&channel, &bob, &[&bob_out], &bob, RosterAction::Named { text: "mine".into() });
        assert!(replay(&channel, &[first.clone(), bob_in.clone(), bob_out.clone(), renamed]).name.is_none());
        let back = redate(&channel, &ann, ev(&channel, &ann, &[&bob_out], &bob, RosterAction::Joined), bob_in.at + 100);
        let roster = replay(&channel, &[first, bob_in.clone(), bob_out, back]);
        assert_eq!(roster.joined(&bob.public_key_bytes()), Some(bob_in.at + 100));
    }

    #[test]
    fn any_member_names_it_and_sets_its_timer() {
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let bob_in = ev(&channel, &ann, &[&first], &bob, RosterAction::Joined);
        let named = ev(&channel, &bob, &[&bob_in], &bob, RosterAction::Named { text: "Lunch".into() });
        let timer = ev(&channel, &bob, &[&named], &bob, RosterAction::Timer { seconds: 3600 });
        let roster = replay(&channel, &[first.clone(), bob_in.clone(), named.clone(), timer]);
        assert_eq!(roster.name.as_ref().map(|n| n.text.as_str()), Some("Lunch"));
        assert_eq!(roster.timer(), 3600);
        let cleared = ev(&channel, &ann, &[&named], &ann, RosterAction::Named { text: String::new() });
        assert!(replay(&channel, &[first, bob_in, named, cleared]).name.is_none(), "empty goes back to the members' names");
        assert!(validate_group_name("two\nlines").is_err());
    }

    #[test]
    fn an_entry_after_something_missing_waits_until_it_arrives() {
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let bob_in = ev(&channel, &ann, &[&first], &bob, RosterAction::Joined);
        assert!(!is_member(&replay(&channel, std::slice::from_ref(&bob_in)), &bob));
        assert!(is_member(&replay(&channel, &[bob_in, first]), &bob));
    }

    #[test]
    fn heads_are_the_entries_nothing_comes_after() {
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let b = ev(&channel, &ann, &[&first], &bob, RosterAction::Joined);
        assert_eq!(heads(&channel, &[first.clone(), b.clone()]), vec![event_id(&channel, &b)]);
        let events = [first, b.clone()];
        assert_eq!(latest_about(&channel, &events, &bob.public_key_bytes()), Some(&b));
    }

    #[test]
    fn a_tampered_or_foreign_entry_fails_verification() {
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let mut e = ev(&channel, &ann, &[&first], &bob, RosterAction::Joined);
        assert!(verify_roster_event(&channel, &e));
        let other = ChannelId::new_group(ann.public_key_bytes());
        assert!(!verify_roster_event(&other, &e), "signed for another group");
        e.subject = ann.public_key_bytes();
        assert!(!verify_roster_event(&channel, &e));
    }

    #[test]
    fn a_catch_up_sends_just_what_the_other_side_is_missing_from_when_they_joined() {
        let ann = Identity::generate();
        let (channel, _) = group(&ann);
        let shared_old = post_at(&channel, &ann, 3600 * 10, "old, both have it");
        let only_theirs = post_at(&channel, &ann, 3600 * 10 + 5, "same hour, only they have it");
        let only_theirs_later = post_at(&channel, &ann, 3600 * 20, "a later hour, only they have it");
        let mine = vec![shared_old.clone()];
        let theirs = vec![shared_old, only_theirs.clone(), only_theirs_later.clone()];
        let (from, digests) = digests(&mine);
        assert_eq!(from, 0, "a full history asks for everything");
        let missing = missing_for(&theirs, 0, from, &digests);
        assert_eq!(missing.len(), 3);
        assert!(missing.contains(&only_theirs) && missing.contains(&only_theirs_later));
        let (from, digests) = super::digests(&[]);
        assert_eq!(missing_for(&theirs, 3600 * 15, from, &digests), vec![only_theirs_later]);
        let (from, digests) = super::digests(&theirs);
        assert!(missing_for(&theirs, 0, from, &digests).is_empty());
    }

    #[test]
    fn posts_are_kept_from_members_and_from_when_you_joined() {
        let dir = scratch_dir("merge");
        let (ann, bob, eve) = (Identity::generate(), Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let first = redate(&channel, &ann, first, 1000);
        let bob_in = redate(&channel, &ann, ev(&channel, &ann, &[&first], &bob, RosterAction::Joined), 5000);
        merge_roster(&dir, &channel, &[first, bob_in]).unwrap();
        let before_bob = post_at(&channel, &ann, 2000, "before bob");
        let after = post_at(&channel, &ann, 6000, "after");
        let from_bob = post_at(&channel, &bob, 6001, "bob here");
        let outsider = post_at(&channel, &eve, 6002, "not in it");
        let mut forged = post_at(&channel, &eve, 6003, "pretending");
        forged.author = ann.public_key_bytes();
        let all = [before_bob, after, from_bob, outsider, forged];
        let kept = |posts: Vec<ChannelPost>| posts.into_iter().map(|p| p.text).collect::<Vec<_>>();
        assert_eq!(kept(merge_posts(&dir, &channel, &bob.public_key_bytes(), &all).unwrap()), vec!["after", "bob here"], "bob sees from when he joined");
        assert!(merge_posts(&dir, &channel, &bob.public_key_bytes(), &all).unwrap().is_empty(), "a repeat isn't new");
        assert!(merge_posts(&dir, &channel, &eve.public_key_bytes(), &all).unwrap().is_empty(), "nothing for someone not in it");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_post_from_one_who_left_is_kept() {
        let dir = scratch_dir("merge-left");
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let bob_in = ev(&channel, &ann, &[&first], &bob, RosterAction::Joined);
        let said = sign_channel_post(&channel, &bob, "before I go", None);
        let bob_out = ev(&channel, &bob, &[&bob_in], &bob, RosterAction::Left);
        merge_roster(&dir, &channel, &[first, bob_in, bob_out]).unwrap();
        merge_posts(&dir, &channel, &ann.public_key_bytes(), &[said]).unwrap();
        assert_eq!(load_posts(&dir, &channel).unwrap().len(), 1);
        delete_posts(&dir, &channel).unwrap();
        assert!(load_posts(&dir, &channel).unwrap().is_empty() && exists(&dir, &channel), "the roster stays");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_members_timer_deletes_posts_written_under_it() {
        let dir = scratch_dir("timer");
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let first = redate(&channel, &ann, first, 1000);
        let bob_in = redate(&channel, &ann, ev(&channel, &ann, &[&first], &bob, RosterAction::Joined), 1001);
        let now = unix_now();
        let timer = redate(&channel, &bob, ev(&channel, &bob, &[&bob_in], &bob, RosterAction::Timer { seconds: 3600 }), now - 100);
        merge_roster(&dir, &channel, &[first, bob_in, timer]).unwrap();
        let kept = post_at(&channel, &ann, 1500, "before the timer");
        let goes = post_at(&channel, &ann, now - 10, "under it");
        merge_posts(&dir, &channel, &ann.public_key_bytes(), &[kept, goes]).unwrap();
        assert_eq!(sweep(&dir, now + 3600).unwrap(), 1);
        assert_eq!(load_posts(&dir, &channel).unwrap().into_iter().map(|p| p.text).collect::<Vec<_>>(), vec!["before the timer"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A member dating an entry back gets no further back than the
    /// entries it comes after: a joining or a timer from 1970 counts from
    /// the signer's own joining.
    #[test]
    fn an_entry_is_never_earlier_than_what_it_comes_after() {
        let (ann, bob, cat) = (Identity::generate(), Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let first = redate(&channel, &ann, first, 1000);
        let bob_in = redate(&channel, &ann, ev(&channel, &ann, &[&first], &bob, RosterAction::Joined), 5000);
        let cat_in = redate(&channel, &bob, ev(&channel, &bob, &[&bob_in], &cat, RosterAction::Joined), 0);
        let timer = redate(&channel, &bob, ev(&channel, &bob, &[&cat_in], &bob, RosterAction::Timer { seconds: 1 }), 0);
        let roster = replay(&channel, &[first, bob_in, cat_in, timer]);
        assert_eq!(roster.joined(&cat.public_key_bytes()), Some(5000), "cat's history starts no earlier than bob's");
        assert_eq!(roster.expires(4999), 0, "nothing from before bob joined is touched by his timer");
        assert_ne!(roster.expires(5000), 0);
    }

    /// A timer dated back deletes nothing this device already had from
    /// before it arrived; one dated as it's set works as ever.
    #[test]
    fn a_timer_dated_back_deletes_nothing_already_here() {
        let dir = scratch_dir("timer-back");
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let first = redate(&channel, &ann, first, 1000);
        let bob_in = redate(&channel, &ann, ev(&channel, &ann, &[&first], &bob, RosterAction::Joined), 1001);
        merge_roster(&dir, &channel, &[first, bob_in.clone()]).unwrap();
        let now = unix_now();
        let said = post_at(&channel, &ann, now - 3 * 3600, "said earlier");
        merge_posts(&dir, &channel, &ann.public_key_bytes(), &[said]).unwrap();
        let timer = redate(&channel, &bob, ev(&channel, &bob, &[&bob_in], &bob, RosterAction::Timer { seconds: 1 }), 1002);
        merge_roster(&dir, &channel, &[timer]).unwrap();
        assert_eq!(sweep(&dir, now + 60).unwrap(), 0, "what was here before the timer stays");
        let later = post_at(&channel, &ann, now + 30, "under it");
        merge_posts(&dir, &channel, &ann.public_key_bytes(), &[later]).unwrap();
        assert_eq!(sweep(&dir, now + 60).unwrap(), 1, "what's written once it's here goes");
        assert_eq!(replay(&channel, &load_roster(&dir, &channel).unwrap()).expires(2000), 1 + 2000, "the roster itself still says when it was set");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// What's dated ahead of this device's clock (a fast clock) is kept,
    /// but an adding from next year starts its history now, and a group's
    /// worth of posts from the future push out nothing: once the group's
    /// full, they're the first to go.
    #[test]
    fn what_is_dated_ahead_pushes_nothing_out() {
        let dir = scratch_dir("ahead");
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        let first = redate(&channel, &ann, first, 1000);
        let now = unix_now();
        let year = 365 * 24 * 3600;
        let bob_in = redate(&channel, &ann, ev(&channel, &ann, &[&first], &bob, RosterAction::Joined), now + year);
        assert_eq!(merge_roster(&dir, &channel, &[first, bob_in]).unwrap().len(), 2, "both kept");
        let joined = replay(&channel, &load_roster(&dir, &channel).unwrap()).joined(&bob.public_key_bytes()).unwrap();
        assert!(joined <= unix_now() + AHEAD_SLACK, "bob's history starts about now, not next year");
        let before = post_at(&channel, &ann, now - 60, "said before");
        merge_posts(&dir, &channel, &ann.public_key_bytes(), &[before]).unwrap();
        let future: Vec<ChannelPost> = (0..MAX_POSTS as u64).map(|i| post_at(&channel, &ann, now + year + i, "from the future")).collect();
        merge_posts(&dir, &channel, &ann.public_key_bytes(), &future).unwrap();
        let after = post_at(&channel, &ann, unix_now(), "said after");
        let kept: Vec<String> = merge_posts(&dir, &channel, &ann.public_key_bytes(), &[after]).unwrap().into_iter().map(|p| p.text).collect();
        assert_eq!(kept, vec!["said after"], "what's said after isn't pushed out");
        let texts: Vec<String> = load_posts(&dir, &channel).unwrap().into_iter().map(|p| p.text).collect();
        assert_eq!(texts.len(), MAX_POSTS);
        assert!(texts.contains(&"said before".to_string()) && texts.contains(&"said after".to_string()), "nor what was said before");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A group we were told we're in but never caught up on goes after a
    /// while; one we're in, or left, stays.
    #[test]
    fn a_group_never_caught_up_on_goes() {
        let dir = scratch_dir("waiting");
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (theirs, their_first) = group(&ann);
        let bob_in = ev(&theirs, &ann, &[&their_first], &bob, RosterAction::Joined);
        merge_roster(&dir, &theirs, &[bob_in]).unwrap();
        let (ours, our_first) = group(&bob);
        merge_roster(&dir, &ours, &[our_first]).unwrap();
        assert_eq!(drop_waiting(&dir, &bob.public_key_bytes(), std::time::Duration::from_secs(3600)).unwrap(), 0, "not yet");
        assert_eq!(drop_waiting(&dir, &bob.public_key_bytes(), std::time::Duration::ZERO).unwrap(), 1);
        assert!(!exists(&dir, &theirs) && exists(&dir, &ours));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_post_is_signed_with_its_name_and_only_for_its_group() {
        let ann = Identity::generate();
        let (channel, _) = group(&ann);
        let post = sign_channel_post(&channel, &ann, "hi\nthere", Some("Ann"));
        assert_eq!(post.text, "hi\nthere");
        assert!(verify_channel_post(&channel, &post));
        assert!(!verify_channel_post(&ChannelId::new_group(ann.public_key_bytes()), &post));
        let mut renamed = post.clone();
        renamed.name = Some("Eve".into());
        assert!(!verify_channel_post(&channel, &renamed));
        assert_eq!(parse_channel_post(format_channel_post(&post).trim_end()), Some(post));
    }

    #[test]
    fn roster_and_posts_round_trip_through_storage() {
        let dir = scratch_dir("storage");
        let (ann, bob) = (Identity::generate(), Identity::generate());
        let (channel, first) = group(&ann);
        create(&dir, &channel).unwrap();
        append_roster_event(&dir, &channel, &first).unwrap();
        assert_eq!(load_roster(&dir, &channel).unwrap(), vec![first.clone()]);
        let named = ev(&channel, &ann, &[&first], &ann, RosterAction::Named { text: "Tea, cake".into() });
        let bob_in = ev(&channel, &ann, &[&named], &bob, RosterAction::Joined);
        merge_roster(&dir, &channel, &[first.clone(), named.clone(), bob_in.clone()]).unwrap();
        assert_eq!(load_roster(&dir, &channel).unwrap(), vec![first, named, bob_in]);
        let post = sign_channel_post(&channel, &bob, "hi everyone", None);
        merge_posts(&dir, &channel, &ann.public_key_bytes(), std::slice::from_ref(&post)).unwrap();
        assert_eq!(load_posts(&dir, &channel).unwrap(), vec![post]);
        assert_eq!(known_channels(&dir).unwrap(), vec![channel]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_tag_that_could_escape_the_groups_directory_is_refused() {
        let dir = scratch_dir("traversal");
        let me = Identity::generate();
        for bad in ["x/../../escaped", "..", "a\\b", "two\nlines", "", "c:d", "trailing."] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be refused");
            let channel = ChannelId { name: bad.to_string(), owner: me.public_key_bytes() };
            let event = ev(&channel, &me, &[], &me, RosterAction::Joined);
            assert!(append_roster_event(&dir, &channel, &event).is_err());
            assert!(create(&dir, &channel).is_err());
            assert!(!exists(&dir, &channel));
        }
        assert!(!dir.exists(), "nothing should have been written anywhere");
    }
}
