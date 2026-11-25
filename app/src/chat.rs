//! What's in a conversation, ready to draw: the lines of a one-to-one
//! chat, and a group's lines, members and name. Names are
//! resolved (see `names`), colours given as roles (see `colours`), and
//! the person's display choices (`prefs`) already applied.

use crate::colours::{self, NameColour};
use crate::names::{short_key, ChannelNames};
use crate::prefs::Prefs;
use crate::sidebar::{PeerRow, Sidebar};
use crate::{names, paths};
use std::collections::HashMap;
use swarm_core::channel::{self, ChannelId, RosterAction};
use swarm_core::history;
use swarm_core::node::NodeHandle;

/// One line of a chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatItem {
    /// Unix seconds.
    pub at: u64,
    pub what: ChatWhat,
    /// For a message the user sent to a contact: whether it's reached
    /// them yet.
    pub sent: Sent,
    /// In a group, someone else's line that names you (`for_me`),
    /// shown faintly highlighted, as IRC does.
    pub for_me: bool,
    /// In a contact's chat, the history lines it is, for deleting it
    /// (`actions::delete_line`): one, or for missed calls shown as one,
    /// each. None for a line that can't be deleted (a channel's).
    pub lines: Vec<history::LineRef>,
}

/// Where a message the user sent to a contact has got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Sent {
    /// Not one the user sent to a contact (theirs, a channel's, a note).
    #[default]
    NotOurs,
    /// None of their devices has it yet (`◷`).
    Waiting,
    /// One of their devices has it (`✓`).
    Delivered,
    /// Deleted, and none of their devices has heard yet (`◷`, the whole
    /// line faint): it goes when one has.
    Deleting,
}

impl Sent {
    /// The faint mark shown after it: `◷`, `✓`, or nothing.
    pub fn mark(self) -> &'static str {
        match self {
            Sent::NotOurs => "",
            Sent::Waiting | Sent::Deleting => "◷",
            Sent::Delivered => "✓",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatWhat {
    /// Someone said something.
    Said { name: String, colour: NameColour, text: String },
    /// Something happened (a join, a topic, a change of name): a mark
    /// (`→`, `←`, `*`) and a sentence about it.
    Happened { mark: &'static str, text: String },
    /// Someone did something, as IRC's `/me` says it: "/me waves" is
    /// "Owen waves". `text` is what's after `/me `.
    Did { name: String, colour: NameColour, text: String },
    /// A call from `name` that nobody answered: "Missed call from Owen".
    Missed { name: String },
}

impl ChatWhat {
    /// Something `name` said: what they did, if it starts `/me `.
    pub fn said(name: String, colour: NameColour, text: String) -> ChatWhat {
        match action(&text) {
            Some(did) => ChatWhat::Did { name, colour, text: did.to_string() },
            None => ChatWhat::Said { name, colour, text },
        }
    }
}

/// A missed call's line: "Missed call from Owen".
pub fn missed_call(name: &str) -> String {
    format!("Missed call from {name}")
}

/// What's done, if `text` is IRC's `/me` (`/me waves`: `waves`).
pub fn action(text: &str) -> Option<&str> {
    text.strip_prefix("/me ").map(str::trim).filter(|did| !did.is_empty())
}

/// Your conversation with a peer, one however many devices they're on,
/// in the order things happened here. Not sorted: a message's time is
/// the sender's clock, and from someone whose clock is slow a reply
/// would go above the question it answers.
pub fn peer_chat(row: &PeerRow) -> Vec<ChatItem> {
    peer_chat_last(row, usize::MAX)
}

/// The newest `most` of `peer_chat` — for a screen that only shows the
/// end: a long conversation made in full on every redraw took a core.
pub fn peer_chat_last(row: &PeerRow, most: usize) -> Vec<ChatItem> {
    let me = names::my_name().unwrap_or_else(|| "you".to_string());
    let waiting: Vec<(swarm_core::outbox::MessageId, u64, String)> = swarm_core::waiting::all(&row.folder.waiting(), &row.public_key)
        .unwrap_or_default()
        .into_iter()
        .map(|w| (w.id, w.at, w.text))
        .chain(swarm_core::outbox::pending(&row.folder.outbox(), &row.public_key).unwrap_or_default().into_iter().map(|m| (m.id, m.sent_at, m.text)))
        .collect();
    let waiting_ids: std::collections::HashSet<swarm_core::outbox::MessageId> = waiting.iter().map(|(id, _, _)| *id).collect();
    let deleting = swarm_core::unsaying::shown(&row.folder.unsaying(), &row.public_key).unwrap_or_default();
    let mut items: Vec<ChatItem> = Vec::new();
    for e in history::last(&row.folder.history(), &row.public_key, most).unwrap_or_default() {
        let item = match e.direction {
            history::Direction::Sent => {
                let line = e.line_ref();
                let sent = if deleting.iter().any(|d| d.matches(&e)) {
                    Sent::Deleting
                } else if e.id.is_some_and(|id| waiting_ids.contains(&id)) {
                    Sent::Waiting
                } else {
                    Sent::Delivered
                };
                ChatItem { at: e.at, what: ChatWhat::said(me.clone(), NameColour::Me, e.text), sent, for_me: false, lines: vec![line] }
            }
            history::Direction::Received => {
                let line = e.line_ref();
                ChatItem { at: e.at, what: ChatWhat::said(row.label.clone(), NameColour::Peer, e.text), sent: Sent::NotOurs, for_me: false, lines: vec![line] }
            }
            history::Direction::Missed => {
                if let Some(together) = items.iter_mut().rev().take(3).find(|i| matches!(i.what, ChatWhat::Missed { .. }) && i.at.abs_diff(e.at) < MISSED_TOGETHER) {
                    together.lines.push(e.line_ref());
                    continue;
                }
                ChatItem { at: e.at, what: ChatWhat::Missed { name: row.label.clone() }, sent: Sent::NotOurs, for_me: false, lines: vec![e.line_ref()] }
            }
        };
        items.push(item);
    }
    items
}

/// Missed calls this close together are one call, noted by more than
/// one of the user's devices (see `peer_chat_last`).
const MISSED_TOGETHER: u64 = 60;

/// Tab completing a name in the typing box: `har` and Tab gives
/// `Harrison `. What the last Tab made, so the next one (on the text
/// as it left it) goes on to the next name that fits.
#[derive(Clone, Debug, Default)]
pub struct Completing {
    /// The text before the word being completed.
    before: String,
    /// The names that fit what was typed, in order.
    fits: Vec<String>,
    /// Which of them is in.
    at: usize,
    /// The text as the last Tab left it.
    made: String,
}

/// `text` with the word at its end completed to one of `names` that
/// starts with it (case aside), and a space after; `None` if no name
/// fits (or there's no word there). Tab again, on the text as this left
/// it, gives the next name that fits, round and round (the same, if only
/// one does).
pub fn complete(text: &str, names: &[String], last: &mut Option<Completing>) -> Option<String> {
    if let Some(c) = last.as_mut().filter(|c| c.made == text) {
        c.at = (c.at + 1) % c.fits.len();
        c.made = format!("{}{} ", c.before, c.fits[c.at]);
        return Some(c.made.clone());
    }
    let start = text.rfind(char::is_whitespace).map_or(0, |i| i + text[i..].chars().next().map_or(1, char::len_utf8));
    let (before, word) = text.split_at(start);
    if word.is_empty() {
        *last = None;
        return None;
    }
    let word = word.to_lowercase();
    let mut fits: Vec<String> = names.iter().filter(|n| !n.is_empty() && n.to_lowercase().starts_with(&word)).cloned().collect();
    fits.sort_by_key(|n| n.to_lowercase());
    fits.dedup();
    if fits.is_empty() {
        *last = None;
        return None;
    }
    let made = format!("{before}{} ", fits[0]);
    *last = Some(Completing { before: before.to_string(), fits, at: 0, made: made.clone() });
    Some(made)
}

/// The names Tab completes to in `row`'s chat: theirs, and yours.
pub fn peer_names(row: &PeerRow) -> Vec<String> {
    std::iter::once(row.label.clone()).chain(names::my_name()).collect()
}

/// The names Tab completes to in a group: its members'.
pub fn channel_names(view: &ChannelView) -> Vec<String> {
    view.members.iter().map(|m| m.name.clone()).collect()
}

/// Someone in a group.
#[derive(Clone, Debug)]
pub struct Member {
    pub key: [u8; 32],
    pub name: String,
    /// One of your contacts (or you): someone who isn't can be asked to
    /// be one.
    pub contact: bool,
    /// Connected to you now (you always are).
    pub connected: bool,
    pub colour: NameColour,
}

/// A group, as its view shows it.
#[derive(Clone, Debug)]
pub struct ChannelView {
    pub id: ChannelId,
    /// What it's called (`names::group_name`).
    pub name: String,
    /// Whether a member gave it that name (else it's its members').
    pub named: bool,
    /// Whether you're in it (you can say something in it).
    pub member: bool,
    /// You first, then your contacts, then the rest, each by name.
    pub members: Vec<Member>,
    /// Messages, with who came and went in between, oldest first.
    pub items: Vec<ChatItem>,
}

impl ChannelView {
    /// Whether you can still say anything in it.
    pub fn can_post(&self) -> bool {
        self.member
    }
}

/// Group `id` as it stands on this device.
pub fn channel_view(node: &NodeHandle, sidebar: &Sidebar, id: &ChannelId, prefs: &Prefs) -> ChannelView {
    let dir = paths().channels();
    let me = node.identity().public_key_bytes();
    let events = channel::load_roster(&dir, id).unwrap_or_default();
    let roster = channel::replay(id, &events);
    let slots = colours::channel_colours(channel::counted(id, &events).into_iter().filter(|e| e.action == RosterAction::Joined).map(|e| e.subject), &me);
    let colour_of = |key: &[u8; 32]| {
        if *key == me {
            NameColour::Me
        } else {
            slots.get(key).map_or(NameColour::Other, |slot| NameColour::Member(*slot))
        }
    };
    let mut posts = channel::load_posts(&dir, id).unwrap_or_default();
    posts.sort_by_key(|p| p.created_at);
    let peers = sidebar.peer_names();
    let names = ChannelNames::load(peers.clone(), me, &posts);

    let mut members: Vec<Member> = roster
        .members
        .keys()
        .map(|key| Member {
            key: *key,
            name: names.current(key),
            contact: *key == me || names.is_peer(key),
            connected: *key == me || node.is_connected(key),
            colour: colour_of(key),
        })
        .collect();
    order_members(&mut members, &me);

    let mut items = Vec::new();
    let mut last_name: HashMap<[u8; 32], Option<String>> = HashMap::new();
    for post in posts {
        let said_as = names.said_as(&post);
        let was = last_name.insert(post.author, post.name.clone());
        if let Some(was) = was {
            if post.name.is_some() && was != post.name && !names.is_peer(&post.author) {
                let before = match &was {
                    Some(old) if post.author != me => names.claimed(&post.author, Some(old)),
                    Some(old) => old.clone(),
                    None => short_key(&post.author),
                };
                let text = if post.author == me { format!("you're now known as {said_as}") } else { format!("{before} is now known as {said_as}") };
                items.push(ChatItem { at: post.created_at, what: ChatWhat::Happened { mark: "*", text }, sent: Sent::NotOurs, for_me: false, lines: Vec::new() });
            }
        }
        items.push(ChatItem { at: post.created_at, what: ChatWhat::said(said_as, colour_of(&post.author), post.text), sent: Sent::NotOurs, for_me: false, lines: Vec::new() });
    }
    let since = roster.joined(&me).unwrap_or(0);
    items.extend(roster_items(id, &events, |key| names.current(key), prefs.show_joins).into_iter().filter(|item| item.at >= since));
    items.sort_by_key(|item| item.at);
    let my_name = paths().my_display_name().map(|d| d.name);
    for item in &mut items {
        item.for_me = for_me(item, my_name.as_deref());
    }
    if let Some(wanted) = crate::commands::finding(id) {
        items = found(items, &wanted);
    }

    ChannelView { id: id.clone(), name: names::group_name(&peers, me, id, &roster), named: roster.name.is_some(), member: roster.is_member(&me), members, items }
}

/// Whether `text` names `name`, as IRC highlights a nick: the whole name,
/// ignoring case, not inside a longer word ("owen" in "Owen, lunch?" or
/// "@owen", not in "owens").
pub fn names(text: &str, name: &str) -> bool {
    let (text, name) = (text.to_lowercase(), name.trim().to_lowercase());
    if name.is_empty() {
        return false;
    }
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    text.match_indices(&name).any(|(at, _)| !word(text[..at].chars().next_back()) && !word(text[at + name.len()..].chars().next()))
}

/// Whether a line of a channel's chat names you (see `names`): something
/// someone else said or did, with your name in it.
pub fn for_me(item: &ChatItem, my_name: Option<&str>) -> bool {
    match (&item.what, my_name) {
        (ChatWhat::Said { colour, text, .. } | ChatWhat::Did { colour, text, .. }, Some(name)) => *colour != NameColour::Me && names(text, name),
        _ => false,
    }
}

/// What `/find` leaves of a channel's chat: the lines with `wanted` in
/// them (ignoring case), in what was said, who said it or what happened,
/// under a line saying that's what's shown.
fn found(items: Vec<ChatItem>, wanted: &str) -> Vec<ChatItem> {
    let wanted = wanted.to_lowercase();
    let has = |item: &ChatItem| match &item.what {
        ChatWhat::Said { name, text, .. } | ChatWhat::Did { name, text, .. } => text.to_lowercase().contains(&wanted) || name.to_lowercase().contains(&wanted),
        ChatWhat::Happened { text, .. } => text.to_lowercase().contains(&wanted),
        ChatWhat::Missed { name } => name.to_lowercase().contains(&wanted),
    };
    let kept: Vec<ChatItem> = items.into_iter().filter(has).collect();
    let at = kept.first().map_or(0, |item| item.at);
    let heading = ChatItem { at, what: ChatWhat::Happened { mark: "*", text: format!("only the {} line{} with \"{wanted}\" — /find on its own shows them all", kept.len(), if kept.len() == 1 { "" } else { "s" }) }, sent: Sent::NotOurs, for_me: false, lines: Vec::new() };
    std::iter::once(heading).chain(kept).collect()
}

/// You first, then your contacts, then the rest, each by name — and,
/// for two of the same name, by key: the roster's a map, so without it
/// they'd swap places each time the list is drawn.
fn order_members(members: &mut [Member], me: &[u8; 32]) {
    members.sort_by_key(|m| (m.key != *me, !m.contact, m.name.to_lowercase(), m.key));
}

/// The lines a group's roster gives its chat: who made it, who added
/// whom, who left, its name and its timer.
fn roster_items(id: &ChannelId, events: &[channel::RosterEvent], name: impl Fn(&[u8; 32]) -> String, show_joins: bool) -> Vec<ChatItem> {
    let mut items = Vec::new();
    for event in channel::counted(id, events) {
        let (subject, by) = (name(&event.subject), name(&event.by));
        let (mark, text) = match &event.action {
            RosterAction::Named { text } if text.trim().is_empty() => ("*", format!("{by} took the group's name off")),
            RosterAction::Named { text } => ("*", format!("{by} named the group {text}")),
            RosterAction::Timer { seconds: 0 } => ("*", format!("{by} set messages to be kept")),
            RosterAction::Timer { seconds } => ("*", format!("{by} set messages to delete themselves after {}", swarm_core::disappearing::describe(*seconds))),
            RosterAction::Joined if event.by == event.subject => ("*", format!("{subject} started the group")),
            RosterAction::Joined if !show_joins => continue,
            RosterAction::Joined => ("→", format!("{by} added {subject}")),
            RosterAction::Left if !show_joins => continue,
            RosterAction::Left => ("←", format!("{subject} left")),
        };
        items.push(ChatItem { at: event.at, what: ChatWhat::Happened { mark, text }, sent: Sent::NotOurs, for_me: false, lines: Vec::new() });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Who made it, who added whom, who left, and its name, each a line.
    #[test]
    fn a_groups_roster_is_told_in_its_chat() {
        use swarm_core::identity::Identity;
        let (ann, bob, cat) = (Identity::generate(), Identity::generate(), Identity::generate());
        let id = ChannelId::new_group(ann.public_key_bytes());
        let mut events = Vec::new();
        let mut sign = |by: &Identity, subject: &Identity, action: RosterAction| {
            let prev = if events.is_empty() { Vec::new() } else { channel::heads(&id, &events) };
            events.push(channel::sign_roster_event(&id, by, prev, subject.public_key_bytes(), action));
        };
        sign(&ann, &ann, RosterAction::Joined);
        sign(&ann, &bob, RosterAction::Joined);
        sign(&bob, &cat, RosterAction::Joined);
        sign(&bob, &bob, RosterAction::Named { text: "Lunch".into() });
        sign(&cat, &cat, RosterAction::Left);
        let name = |key: &[u8; 32]| [(&ann, "ann"), (&bob, "bob"), (&cat, "cat")].iter().find(|(i, _)| i.public_key_bytes() == *key).unwrap().1.to_string();
        let lines = |show_joins| -> Vec<String> {
            roster_items(&id, &events, name, show_joins)
                .into_iter()
                .map(|item| match item.what {
                    ChatWhat::Happened { text, .. } => text,
                    _ => String::new(),
                })
                .collect()
        };
        assert_eq!(lines(true), ["ann started the group", "ann added bob", "bob added cat", "bob named the group Lunch", "cat left"]);
        assert_eq!(lines(false), ["ann started the group", "bob named the group Lunch"]);
    }

    /// `/find` keeps the lines with the text in (said, by whom, or what
    /// happened), ignoring case, under a line saying so.
    #[test]
    fn find_keeps_the_lines_with_it_in() {
        let item = |at, what| ChatItem { at, what, sent: Sent::NotOurs, for_me: false, lines: Vec::new() };
        let said = |name: &str, text: &str| ChatWhat::said(name.to_string(), NameColour::Other, text.to_string());
        let items = vec![
            item(1, said("bob", "Lunch at noon?")),
            item(2, said("carol", "can't today")),
            item(3, ChatWhat::Happened { mark: "*", text: "alice named the group lunch plans".to_string() }),
            item(4, said("Lunchbox", "hi")),
        ];
        let texts: Vec<String> = found(items, "LUNCH")
            .into_iter()
            .map(|i| match i.what {
                ChatWhat::Said { text, .. } | ChatWhat::Happened { text, .. } => text,
                _ => String::new(),
            })
            .collect();
        assert_eq!(texts, ["only the 3 lines with \"lunch\" — /find on its own shows them all", "Lunch at noon?", "alice named the group lunch plans", "hi"]);
    }

    /// A name's highlighted where it's a word of its own, as IRC does it.
    #[test]
    fn a_line_names_you_only_as_a_word_of_its_own() {
        for named in ["Owen, lunch?", "lunch owen", "@owen are you in", "ok OWEN.", "owen"] {
            assert!(names(named, "Owen"), "{named}");
        }
        for not in ["Pauline's here", "apaul", "paul_s", "nobody", ""] {
            assert!(!names(not, "Owen"), "{not}");
        }
        assert!(names("is Owen S about?", "Owen S"), "a name with a space, whole");
        assert!(!names("anything", "  "), "no name, nothing named");
        let item = |colour, text: &str| ChatItem { at: 0, what: ChatWhat::said("x".to_string(), colour, text.to_string()), sent: Sent::NotOurs, for_me: false, lines: Vec::new() };
        assert!(for_me(&item(NameColour::Other, "hi owen"), Some("Owen")));
        assert!(!for_me(&item(NameColour::Me, "I'm owen"), Some("Owen")), "not your own lines");
        assert!(!for_me(&item(NameColour::Other, "hi owen"), None));
    }

    #[test]
    fn slash_me_is_something_done() {
        let said = |text: &str| ChatWhat::said("Owen".to_string(), NameColour::Me, text.to_string());
        assert_eq!(said("/me waves"), ChatWhat::Did { name: "Owen".to_string(), colour: NameColour::Me, text: "waves".to_string() });
        for plain in ["/me", "/me   ", "/meow", "hello /me waves", "/ME waves"] {
            assert!(matches!(said(plain), ChatWhat::Said { .. }), "{plain}");
        }
    }

    #[test]
    fn tab_completes_a_name_and_goes_round_those_that_fit() {
        let names: Vec<String> = ["Harrison", "Harriet", "Owen", "Zoë"].map(String::from).to_vec();
        let mut last = None;
        assert_eq!(complete("har", &names, &mut last).as_deref(), Some("Harriet "));
        assert_eq!(complete("Harriet ", &names, &mut last).as_deref(), Some("Harrison "));
        assert_eq!(complete("Harrison ", &names, &mut last).as_deref(), Some("Harriet "));
        let mut last = None;
        assert_eq!(complete("thanks OW", &names, &mut last).as_deref(), Some("thanks Owen "));
        assert_eq!(complete("thanks Owen ", &names, &mut last).as_deref(), Some("thanks Owen "));
        assert_eq!(complete("hi zo", &names, &mut None).as_deref(), Some("hi Zoë "));
        for text in ["", "hello ", "bob"] {
            assert_eq!(complete(text, &names, &mut None), None, "{text:?}");
        }
        let mut last = None;
        complete("har", &names, &mut last);
        assert_eq!(complete("Harriet and ow", &names, &mut last).as_deref(), Some("Harriet and Owen "));
    }

    #[test]
    fn members_of_the_same_name_keep_their_places() {
        let member = |key: u8, connected: bool| Member { key: [key; 32], name: "Owen".into(), contact: false, connected, colour: NameColour::Other };
        let mut one = vec![member(2, false), member(1, true)];
        let mut other = vec![member(1, true), member(2, false)];
        order_members(&mut one, &[9; 32]);
        order_members(&mut other, &[9; 32]);
        assert_eq!(one.iter().map(|m| m.key).collect::<Vec<_>>(), other.iter().map(|m| m.key).collect::<Vec<_>>());
    }
}
