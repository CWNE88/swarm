//! The list down the side of every swarm app: you, your peers, groups,
//! people asking to be added and people followed — each under a heading that opens and
//! closes, peers in the order the person chose, with who's online, since
//! when, and how much is unread. In Contacts, each section ends with a
//! way to add to it (`Adding`).
//! Built afresh from the files and the node each time (`refresh`), so a
//! change made anywhere — another `swarm` command, the network — shows.

use crate::prefs::{PeerOrder, Prefs};
use crate::{names, paths};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use swarm_core::channel::{self, ChannelId};
use swarm_core::node::devices::DeviceInfo;
use swarm_core::node::{format_bitrate, NodeHandle};
use swarm_core::settings::AutoAnswer;
use swarm_core::readmarks::Chat;
use swarm_core::{displayname, history, peers, requests};

/// One of your peers, on however many devices.
#[derive(Clone, Debug)]
pub struct PeerRow {
    /// The first of them: the entry edits, calls and posts go to.
    /// A key-derived handle (`peers::handle_of`), never shown.
    pub handle: String,
    /// What to call them.
    pub label: String,
    /// What they're called in the list.
    pub list_label: String,
    /// The node's entries that are connected to them: one for each of
    /// their devices that's been met (see `peerdevices`), else theirs
    /// (`name`). Messages and rings go to all of them; all have
    /// `public_key`, so they share one history.
    pub entries: Vec<String>,
    pub address: String,
    pub public_key: [u8; 32],
    pub online: bool,
    /// They haven't added us as a contact (yet).
    pub not_added: bool,
    /// They've removed us: all that's left is to remove them too
    /// (`actions::remove_peer`, no question asked).
    pub removed_us: bool,
    /// Our request to them is out and they haven't accepted it, whether
    /// or not they can be reached now: deleting them cancels it.
    pub asked: bool,
    /// Messages from them not yet seen.
    pub unread: usize,
    /// Online since, or offline since (unix secs); `None`: never seen.
    pub since: Option<u64>,
    /// Their own cover rate, if they have one (`None`: the default).
    pub cover_bits_per_second: Option<u64>,
    /// That rate is required of them too.
    pub cover_required: bool,
    /// The cover rate in effect for them, as a short tag (`64k`,
    /// `64k req'd by them`), if there's cover at all.
    pub cover_tag: Option<String>,
    /// Whether their calls are answered without asking.
    pub auto_answer: AutoAnswer,
    /// Whether they may control your shared screen without asking.
    pub auto_control: bool,
    /// The folder their history and settings are in.
    pub folder: swarm_core::node::Paths,
}

impl PeerRow {
    /// What's shown beside the name in the list: the cover rate if
    /// there's cover.
    pub fn tag(&self) -> String {
        self.cover_tag.clone().unwrap_or_default()
    }
}

impl PeerRow {
    /// `online since 14:02 (3 hours)`, `offline since Thu 15 Oct 14:02
    /// (17 hours)`, `waiting for them to add you` or `has removed you`.
    pub fn status_text(&self, utc: bool) -> String {
        if self.removed_us {
            return "has removed you".to_string();
        }
        if self.not_added {
            return "waiting for them to add you".to_string();
        }
        let state = if self.online { "online" } else { "offline" };
        match self.since {
            Some(at) => {
                let now = crate::unix_now();
                format!("{state} since {} ({})", crate::time::moment(at, now, utc), crate::time::how_long(now.saturating_sub(at)))
            }
            None => state.to_string(),
        }
    }

    /// Whether their calls are answered without asking, in words.
    pub fn auto_answer_text(&self) -> &'static str {
        auto_answer_text(self.auto_answer)
    }

    /// Their cover setting in words, for the peer's details.
    pub fn cover_text(&self, node: &NodeHandle) -> String {
        let mut cover = match (self.cover_bits_per_second, node.default_cover()) {
            (Some(rate), _) if self.cover_required => format!("{}, required of them too", format_bitrate(rate)),
            (Some(rate), _) => format_bitrate(rate),
            (None, Some(default)) => format!("{} (the default)", format_bitrate(default)),
            (None, None) => "off".to_string(),
        };
        if let Some(live) = node.peer_status(&self.handle).and_then(|s| s.cover).filter(|live| live.raised_by_peer) {
            cover.push_str(&format!(" — sending {} now: they require it", live.sends.map(format_bitrate).unwrap_or_default()));
        }
        cover
    }
}

/// Whether someone may control your shared screen unasked, said short.
pub fn auto_control_text(on: bool) -> &'static str {
    if on {
        "allowed without asking"
    } else {
        "asked each time"
    }
}

/// An auto-answer setting in words.
pub fn auto_answer_text(answer: AutoAnswer) -> &'static str {
    match answer {
        AutoAnswer::Off => "ring as usual",
        AutoAnswer::Voice => "answered automatically (voice only)",
        AutoAnswer::VoiceAndVideo => "answered automatically, video too",
    }
}

/// Someone who has added us, whom we haven't added.
#[derive(Clone, Debug)]
pub struct RequestRow {
    pub public_key: [u8; 32],
    pub address: String,
    /// Their display name, if they sent one with a valid signature.
    pub name: Option<String>,
    /// Where their request is kept.
    pub folder: swarm_core::node::Paths,
}

impl RequestRow {
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| names::short_key(&self.public_key))
    }

    /// The peer name to add them with, typed as `name`.
    pub fn peer_name(&self, name: &str) -> String {
        name.to_string()
    }

    /// A peers-file name to start adding them with: their display name
    /// made to fit (see `peers::validate_name`), or a short form of their
    /// key.
    pub fn suggested_peer_name(&self) -> String {
        let from_display: String =
            self.name.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join("-").trim_start_matches('#').to_string();
        if from_display.is_empty() { names::short_key(&self.public_key) } else { from_display }
    }
}

/// Someone whose public posts you follow, without being their peer.
#[derive(Clone, Debug)]
pub struct FollowRow {
    pub public_key: [u8; 32],
    pub address: String,
    /// The name they publish, or a short form of their key.
    pub label: String,
    /// Posts of theirs not yet seen.
    pub unread: usize,
}

/// A group you're in.
#[derive(Clone, Debug)]
pub struct ChannelRow {
    pub id: ChannelId,
    /// What it's called (`names::group_name`).
    pub name: String,
    /// Messages from others not yet seen.
    pub unread: usize,
    /// Of those, how many name you (`chat::names`), as IRC highlights.
    pub mentions: usize,
    /// Muted here (`/mute`): no sound or notification, unless you're named.
    pub muted: bool,
}

impl ChannelRow {
    /// What a sound or notification goes by: what's unread, or, muted,
    /// only what names you.
    pub fn noticed(&self) -> usize {
        if self.muted { self.mentions } else { self.unread }
    }

    /// Beside its name: `@` while something unread names you, else
    /// `muted` if it is.
    pub fn tag(&self) -> &'static str {
        if self.mentions > 0 {
            "@"
        } else if self.muted {
            "muted"
        } else {
            ""
        }
    }
}

/// The parts of the list, each under its heading, in the list's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// You.
    You,
    /// Your devices: a section inside You, shown only while You is open.
    Devices,
    /// Everyone you have as a peer, with anyone asking to be added
    /// first, and those you follow last.
    Peers,
    /// Groups you're in.
    Channels,
}

impl Section {
    pub const ALL: [Section; 4] = [Section::You, Section::Devices, Section::Peers, Section::Channels];

    pub fn title(self) -> &'static str {
        match self {
            Section::You => "YOU",
            Section::Devices => "DEVICES",
            Section::Peers => "CONTACTS",
            Section::Channels => "GROUPS",
        }
    }

    /// How it's kept in the settings file (see `Prefs::closed`).
    pub fn key(self) -> &'static str {
        match self {
            Section::You => "you",
            Section::Devices => "devices",
            Section::Peers => "peers",
            Section::Channels => "groups",
        }
    }

    /// The section it's a step in under, if any: closing that hides it
    /// too.
    pub fn within(self) -> Option<Section> {
        match self {
            Section::Devices => Some(Section::You),
            _ => None,
        }
    }

    pub fn from_key(key: &str) -> Option<Section> {
        Section::ALL.into_iter().find(|s| s.key() == key)
    }

    fn bit(self) -> u8 {
        1 << Section::ALL.iter().position(|s| *s == self).expect("every section is in ALL")
    }
}

/// Which sections are closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Closed(u8);

impl Closed {
    pub fn contains(self, section: Section) -> bool {
        self.0 & section.bit() != 0
    }

    pub fn toggle(&mut self, section: Section) {
        self.0 ^= section.bit();
    }

    /// As kept in the settings file: the closed sections' keys, joined
    /// with commas.
    pub fn to_text(self) -> String {
        Section::ALL.into_iter().filter(|s| self.contains(*s)).map(Section::key).collect::<Vec<_>>().join(",")
    }

    /// From `to_text`'s; anything not a section's key is passed over.
    pub fn from_text(text: &str) -> Closed {
        Closed(text.split(',').filter_map(|key| Section::from_key(key.trim())).fold(0, |bits, s| bits | s.bit()))
    }
}

/// A heading, above what's in its section: shown under it while it's
/// open; while it's closed, how many there are, and whether any of them
/// has something unread or is waiting on an answer.
#[derive(Clone, Debug)]
pub struct HeadingRow {
    pub section: Section,
    pub open: bool,
    pub count: usize,
    pub attention: bool,
}

impl HeadingRow {
    /// Its triangle: pointing down while open, right while closed.
    pub fn triangle(&self) -> &'static str {
        if self.open { "▾" } else { "▸" }
    }

    /// What's after the title while it's closed: the count, with a dot
    /// if anything in it wants seeing.
    pub fn closed_text(&self) -> String {
        match (self.open, self.attention) {
            (true, _) => String::new(),
            (false, false) if self.count == 0 => String::new(),
            (false, true) => format!("{} •", self.count),
            (false, false) => self.count.to_string(),
        }
    }
}

/// A way to add to a section, at its end in Contacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adding {
    Device,
    Contact,
    Follow,
    NewChannel,
}

impl Adding {
    /// What it says in the list.
    pub fn text(self) -> &'static str {
        match self {
            Adding::Device => "Add a device",
            Adding::Contact => "Add a contact",
            Adding::Follow => "Follow someone",
            Adding::NewChannel => "New group",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Adding::Device => "device",
            Adding::Contact => "contact",
            Adding::Follow => "follow",
            Adding::NewChannel => "new-group",
        }
    }

    /// Those at the end of `section`.
    pub fn in_section(section: Section) -> &'static [Adding] {
        match section {
            Section::You => &[],
            Section::Devices => &[Adding::Device],
            Section::Peers => &[Adding::Contact, Adding::Follow],
            Section::Channels => &[Adding::NewChannel],
        }
    }

    pub fn section(self) -> Section {
        match self {
            Adding::Device => Section::Devices,
            Adding::Contact | Adding::Follow => Section::Peers,
            Adding::NewChannel => Section::Channels,
        }
    }
}

/// One line of the list.
#[derive(Clone, Debug)]
pub enum Entry {
    /// A section's heading: Enter or a click opens or closes it.
    Heading(HeadingRow),
    /// You — always first.
    You,
    /// One of your devices, under You: this one first, the rest by
    /// name (see `devices::in_list_order`).
    Device(DeviceInfo),
    Peer(PeerRow),
    Request(RequestRow),
    Channel(ChannelRow),
    Followed(FollowRow),
    /// A way to add to its section, at its end: shown only in Contacts
    /// (`Sidebar::set_adding`), never selected.
    Add(Adding),
}

/// What an entry is, apart from its details — to find it again after the
/// list is rebuilt (it may have moved).
pub type EntryId = String;

/// Group `id`'s entry, to select it (just made, say).
pub fn channel_entry_id(id: &ChannelId) -> EntryId {
    format!("channel {} {}", id.name, STANDARD.encode(id.owner))
}

impl Entry {
    pub fn id(&self) -> EntryId {
        match self {
            Entry::Heading(row) => format!("heading {}", row.section.key()),
            Entry::You => "you".to_string(),
            Entry::Device(device) => format!("device {}", STANDARD.encode(device.key)),
            Entry::Peer(row) => format!("peer {}", STANDARD.encode(row.public_key)),
            Entry::Request(row) => format!("request {}", STANDARD.encode(row.public_key)),
            Entry::Channel(row) => channel_entry_id(&row.id),
            Entry::Followed(row) => format!("followed {}", STANDARD.encode(row.public_key)),
            Entry::Add(adding) => format!("add {}", adding.key()),
        }
    }

    /// The section it's listed under (a heading: its own).
    pub fn section(&self) -> Section {
        match self {
            Entry::Heading(row) => row.section,
            Entry::You => Section::You,
            Entry::Device(_) => Section::Devices,
            Entry::Peer(_) | Entry::Request(_) | Entry::Followed(_) => Section::Peers,
            Entry::Channel(_) => Section::Channels,
            Entry::Add(adding) => adding.section(),
        }
    }

    /// Where it comes in its section: anyone waiting on an answer first.
    fn place(&self) -> u8 {
        match self {
            Entry::Request(_) => 0,
            Entry::Followed(_) => 2,
            _ => 1,
        }
    }

    /// Something unread in it, or it's waiting on an answer.
    fn wants_seeing(&self) -> bool {
        match self {
            Entry::Peer(row) => row.unread > 0,
            Entry::Channel(row) => row.unread > 0,
            Entry::Followed(row) => row.unread > 0,
            Entry::Request(_) => true,
            Entry::Heading(_) | Entry::You | Entry::Device(_) | Entry::Add(_) => false,
        }
    }
}

/// The list, and what it remembers between refreshes: how much of each
/// conversation had been seen, and which contact requests were known.
#[derive(Default)]
pub struct Sidebar {
    /// What's shown: each section's heading, and what's in it if it's
    /// open.
    pub rows: Vec<Entry>,
    /// Everything in the list, closed sections' too, without headings:
    /// what's looked through for someone, or for what's unread.
    pub all: Vec<Entry>,
    /// The sections closed (see `Prefs::closed`).
    closed: Closed,
    /// Each section ending with its ways to add (`Adding`): in Contacts.
    adding: bool,
    /// A section inside another (Devices, in You) as a tree under it, as
    /// the window draws it: no heading of its own, so always open.
    tree: bool,
    /// Your devices left out (the window's tabs but Contacts: they've no
    /// chat, transfers or settings of their own).
    no_devices: bool,
    /// Per peer (by entry): how many of their messages had been seen.
    last_read: HashMap<EntryId, usize>,
    /// Per channel: how many messages by others had been seen.
    channel_last_read: HashMap<ChannelId, usize>,
    /// Per person followed: how many of their posts had been seen.
    followed_last_read: HashMap<[u8; 32], usize>,
    /// How far each chat was marked read when last on screen here: only
    /// a mark that's moved since says another device has read something.
    mark_when_read: HashMap<Chat, u64>,
    /// The contact requests seen so far (by entry); `None` before the
    /// first refresh.
    known_requests: Option<HashSet<EntryId>>,
}

/// Used this recently (or a chat on screen counts only here): a window
/// left open at home mustn't mark as read what comes in to the phone.
pub const LOOKED_AT_WITHIN: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Peers who've moved, in a sentence: `owen has moved — now at a, b`.
fn moved_news(moves: Vec<(String, Vec<String>)>) -> Option<String> {
    let mut latest: Vec<(String, Vec<String>)> = Vec::new();
    for (name, list) in moves {
        latest.retain(|(n, _)| *n != name);
        latest.push((name, list));
    }
    let said: Vec<String> = latest.into_iter().map(|(name, list)| format!("{name} has moved — now at {}", list.join(", "))).collect();
    (!said.is_empty()).then(|| said.join("; "))
}

impl Sidebar {
    pub fn new() -> Sidebar {
        let mut sidebar = Sidebar { all: vec![Entry::You], ..Default::default() };
        sidebar.lay_out();
        sidebar
    }

    /// Rebuilds the list from the peers file (`peers_path`), the node and
    /// the data folder, in `prefs`' order, and recounts unread for
    /// everything but `open` (the entry on screen, which counts as read;
    /// `looked_at`: someone's really reading it, see `mark_read`).
    /// Returns something to tell the person, if there's news: a new
    /// contact request (or, the first time, how many are waiting).
    pub fn refresh(&mut self, node: &NodeHandle, peers_path: &Path, prefs: &Prefs, open: Option<&EntryId>, looked_at: bool) -> Option<String> {
        let paths = paths();
        let devices = crate::devices::in_list_order(node.devices().devices);
        let own: Vec<[u8; 32]> = devices.iter().filter(|d| !d.this).map(|d| d.key).collect();
        let mut known: Vec<peers::Peer> = peers::load(peers_path).unwrap_or_default().into_iter().filter(|p| !own.contains(&p.public_key)).collect();
        let mut same_name: HashMap<String, usize> = HashMap::new();
        for p in &known {
            *same_name.entry(p.name.clone()).or_default() += 1;
        }
        match prefs.peer_order {
            PeerOrder::Added => {}
            PeerOrder::Name => known.sort_by_key(|p| p.name.to_lowercase()),
            PeerOrder::Recent => known.sort_by_cached_key(|p| {
                let online = node.peer_status(&p.handle()).is_some_and(|s| s.online);
                let last = history::summary(&paths.history(), &p.public_key).map(|s| s.last_at).unwrap_or(0);
                (!online, std::cmp::Reverse(last), p.name.to_lowercase())
            }),
        }
        let peer_keys: HashSet<[u8; 32]> = known.iter().map(|p| p.public_key).collect();
        let mut rows = vec![Entry::You];
        rows.extend(devices.into_iter().map(Entry::Device));
        for p in known {
            let status = node.peer_status(&p.handle());
            let cover_tag = cover_tag(node, &p.handle(), p.cover_bits_per_second, p.cover_required);
            rows.push(Entry::Peer(PeerRow {
                online: status.is_some_and(|s| s.online),
                not_added: status.is_some_and(|s| s.not_added),
                removed_us: status.is_some_and(|s| s.removed_us),
                asked: status.is_some_and(|s| s.asked && !s.removed_us),
                since: status.and_then(|s| s.since),
                unread: 0,
                auto_answer: swarm_core::settings::auto_answer(&paths.settings(), &p.public_key),
                auto_control: swarm_core::settings::auto_control(&paths.settings(), &p.public_key),
                cover_tag,
                label: p.name.clone(),
                list_label: if same_name.get(&p.name).is_some_and(|n| *n > 1) { format!("{}~{}", p.name, p.handle().chars().take(6).collect::<String>()) } else { p.name.clone() },
                entries: node.same_person(&p.handle()),
                handle: p.handle(),
                address: p.address,
                public_key: p.public_key,
                cover_bits_per_second: p.cover_bits_per_second,
                cover_required: p.cover_required,
                folder: paths.clone(),
            }));
        }
        let published = displayname::cached_all(&paths.displaynames()).unwrap_or_default();
        let pending: Vec<RequestRow> = requests::pending(&paths.requests())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !peer_keys.contains(&r.public_key))
            .map(|r| RequestRow { name: published.get(&r.public_key).map(|dn| dn.name.clone()), public_key: r.public_key, address: r.address, folder: paths.clone() })
            .collect();
        let published = displayname::cached_all(&paths.displaynames()).unwrap_or_default();
        let news = self.note_new_requests(&pending).or_else(|| moved_news(node.take_moves()));
        rows.extend(pending.into_iter().map(Entry::Request));
        let me = node.identity().public_key_bytes();
        let peer_names: HashMap<[u8; 32], String> = rows.iter().filter_map(|e| match e {
            Entry::Peer(row) => Some((row.public_key, row.label.clone())),
            _ => None,
        }).collect();
        let mut groups: Vec<ChannelRow> = channel::known_channels(&paths.channels())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| {
                let roster = channel::replay(&id, &channel::load_roster(&paths.channels(), &id).unwrap_or_default());
                roster.is_member(&me).then(|| ChannelRow { name: names::group_name(&peer_names, me, &id, &roster), id, unread: 0, mentions: 0, muted: false })
            })
            .collect();
        groups.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        rows.extend(groups.into_iter().map(Entry::Channel));
        rows.extend(node.following().into_iter().filter(|f| !peer_keys.contains(&f.public_key)).map(|f| {
            let label = published.get(&f.public_key).map(|dn| dn.name.clone()).unwrap_or_else(|| names::short_key(&f.public_key));
            Entry::Followed(FollowRow { public_key: f.public_key, address: f.address, label, unread: 0 })
        }));
        rows.sort_by_key(Entry::section);
        self.all = rows;
        self.closed = prefs.closed;
        self.mark_read(node, open, looked_at);
        news
    }

    /// Each open section ending with its ways to add to it (in Contacts),
    /// or not.
    pub fn set_adding(&mut self, adding: bool) {
        if self.adding != adding {
            self.adding = adding;
            self.lay_out();
        }
    }

    /// Sections inside others as trees, with no headings (the window).
    pub fn set_tree(&mut self, tree: bool) {
        if self.tree != tree {
            self.tree = tree;
            self.lay_out();
        }
    }

    /// Your devices listed (the default), or left out.
    pub fn set_devices_shown(&mut self, shown: bool) {
        if self.no_devices == shown {
            self.no_devices = !shown;
            self.lay_out();
        }
    }

    /// Opens section `section` if it's closed, closes it if it's open,
    /// for now and (in `prefs`, kept at `path`) from now on.
    pub fn toggle(&mut self, section: Section, prefs: &mut Prefs, path: &Path) -> std::io::Result<()> {
        self.closed.toggle(section);
        self.lay_out();
        prefs.set_closed(path, self.closed)
    }

    /// `rows` from `all`: each section that has anything, its heading,
    /// and what's in it unless it's closed, then (in Contacts) its ways
    /// to add to it, which show a section even while it's empty. A
    /// section inside another (Devices, in You) is left out while that
    /// one's closed, and counted in its heading; as a tree (`tree`), it
    /// has no heading and is always open.
    fn lay_out(&mut self) {
        let mut rows = Vec::new();
        for section in Section::ALL {
            if section.within().is_some_and(|outer| self.closed.contains(outer)) || (self.no_devices && section == Section::Devices) {
                continue;
            }
            let mut entries: Vec<&Entry> = self.all.iter().filter(|e| e.section() == section).collect();
            entries.sort_by_key(|e| e.place());
            let adds: &[Adding] = if self.adding { Adding::in_section(section) } else { &[] };
            if entries.is_empty() && adds.is_empty() {
                continue;
            }
            let inside: Vec<&Entry> = self.all.iter().filter(|e| e.section().within() == Some(section)).collect();
            let tree = self.tree && section.within().is_some();
            let open = tree || !self.closed.contains(section);
            let count = entries.iter().filter(|e| !matches!(e, Entry::You)).count() + inside.len();
            let attention = entries.iter().chain(&inside).any(|e| e.wants_seeing());
            if !tree {
                rows.push(Entry::Heading(HeadingRow { section, open, count, attention }));
            }
            if open {
                rows.extend(entries.into_iter().cloned());
                rows.extend(adds.iter().copied().map(Entry::Add));
            }
        }
        self.rows = rows;
    }

    /// The heading shown for `section`: its own, or while the one it's
    /// inside is closed, that one's.
    pub fn shown_heading(&self, section: Section) -> Option<usize> {
        let at = |section: Section| self.rows.iter().position(|e| matches!(e, Entry::Heading(h) if h.section == section));
        at(section).or_else(|| section.within().and_then(at))
    }

    /// Recounts unread: everything since it was last on screen, with
    /// `open` (on screen now) counted as read, less whatever another of
    /// the user's devices has read since. Whatever's seen for the first
    /// time sets its own starting point, so nothing from before the app
    /// started shows as unread. With `looked_at` (someone's really using
    /// this device, not just left it open) `open` counts as read on all
    /// of the user's devices too.
    pub fn mark_read(&mut self, node: &NodeHandle, open: Option<&EntryId>, looked_at: bool) {
        let paths = paths();
        let me = node.identity().public_key_bytes();
        let my_name = paths.my_display_name().map(|d| d.name);
        let marks = node.read_marks();
        let mark_of = |chat: &Chat| marks.get(chat).copied().unwrap_or(0);
        let read_to = |chat: &Chat, newest: u64| {
            if looked_at && newest > marks.get(chat).copied().unwrap_or(0) {
                node.read_up_to(chat.clone(), newest);
            }
        };
        for entry in &mut self.all {
            let id = entry.id();
            let is_open = open == Some(&id);
            match entry {
                Entry::Peer(row) => {
                    let history = row.folder.history();
                    let summary = history::summary(&history, &row.public_key).unwrap_or_default();
                    let seen = self.last_read.entry(id).or_insert(summary.received);
                    let chat = Chat::Contact(row.public_key);
                    let noted = self.mark_when_read.entry(chat.clone()).or_insert(mark_of(&chat));
                    if is_open {
                        *seen = summary.received;
                        read_to(&chat, summary.last_received);
                        *noted = mark_of(&chat).max(if looked_at { summary.last_received } else { 0 });
                    }
                    row.unread = summary.received.saturating_sub(*seen);
                    if let (true, Some(mark)) = (row.unread > 0, marks.get(&chat).filter(|mark| **mark > *noted)) {
                        let after = history::all(&history, &row.public_key).unwrap_or_default().iter().filter(|e| e.direction.theirs() && e.at > *mark).count();
                        row.unread = row.unread.min(after);
                    }
                }
                Entry::Channel(row) => {
                    let (theirs, newest) = channel::with_posts(&paths.channels(), &row.id, |posts| {
                        let theirs = posts.iter().filter(|p| p.author != me);
                        (theirs.clone().count(), theirs.map(|p| p.created_at).max().unwrap_or(0))
                    })
                    .unwrap_or((0, 0));
                    let seen = self.channel_last_read.entry(row.id.clone()).or_insert(theirs);
                    let chat = Chat::Channel(row.id.clone());
                    let noted = self.mark_when_read.entry(chat.clone()).or_insert(mark_of(&chat));
                    if is_open {
                        *seen = theirs;
                        read_to(&chat, newest);
                        *noted = mark_of(&chat).max(if looked_at { newest } else { 0 });
                    }
                    row.unread = theirs.saturating_sub(*seen);
                    if let (true, Some(mark)) = (row.unread > 0, marks.get(&chat).filter(|mark| **mark > *noted)) {
                        let after = channel::with_posts(&paths.channels(), &row.id, |posts| posts.iter().filter(|p| p.author != me && p.created_at > *mark).count()).unwrap_or(0);
                        row.unread = row.unread.min(after);
                    }
                    row.mentions = match &my_name {
                        Some(name) if row.unread > 0 => channel::with_posts(&paths.channels(), &row.id, |posts| {
                            let mut theirs: Vec<&channel::ChannelPost> = posts.iter().filter(|p| p.author != me).collect();
                            theirs.sort_by_key(|p| p.created_at);
                            theirs.iter().rev().take(row.unread).filter(|p| crate::chat::names(&p.text, name)).count()
                        })
                        .unwrap_or(0),
                        _ => 0,
                    };
                    row.muted = crate::commands::muted(&row.id);
                }
                Entry::Followed(row) => {
                    let posts = swarm_core::postcache::cached(&paths.postcache(), &row.public_key).map(|p| p.len()).unwrap_or(0);
                    let seen = self.followed_last_read.entry(row.public_key).or_insert(posts);
                    if is_open {
                        *seen = posts;
                    }
                    row.unread = posts.saturating_sub(*seen);
                }
                _ => {}
            }
        }
        self.lay_out();
    }

    fn note_new_requests(&mut self, pending: &[RequestRow]) -> Option<String> {
        let current: HashSet<EntryId> = pending.iter().map(|r| Entry::Request(r.clone()).id()).collect();
        let news = match &self.known_requests {
            None if !pending.is_empty() => Some(format!("{} contact request(s) waiting — select one to add them", pending.len())),
            Some(known) => pending
                .iter()
                .find(|r| !known.contains(&Entry::Request((*r).clone()).id()))
                .map(|new| format!("contact request from {} — they've added you; select them to add them back", new.label())),
            None => None,
        };
        self.known_requests = Some(current);
        news
    }

    /// Where the entry `id` is in the list now (`None` if it's gone, or
    /// in a closed section).
    pub fn position(&self, id: &EntryId) -> Option<usize> {
        self.rows.iter().position(|e| &e.id() == id)
    }

    /// The entry `id`, shown or in a closed section.
    pub fn entry(&self, id: &EntryId) -> Option<&Entry> {
        self.all.iter().find(|e| &e.id() == id)
    }

    /// One of your devices, by its key.
    pub fn device_by_key(&self, key: &[u8; 32]) -> Option<&DeviceInfo> {
        self.all.iter().find_map(|e| match e {
            Entry::Device(device) if device.key == *key => Some(device),
            _ => None,
        })
    }

    /// The peer row with this key (any of a person's devices), if
    /// they're a peer.
    pub fn peer_by_key(&self, key: &[u8; 32]) -> Option<&PeerRow> {
        self.all.iter().find_map(|e| match e {
            Entry::Peer(row) if row.public_key == *key => Some(row),
            _ => None,
        })
    }

    /// The peer row with this name (any of a person's entries).
    pub fn peer_by_name(&self, name: &str) -> Option<&PeerRow> {
        self.all.iter().find_map(|e| match e {
            Entry::Peer(row) if row.handle == name || row.entries.iter().any(|n| n == name) => Some(row),
            _ => None,
        })
    }

    /// Your peers' keys and the names you gave them.
    pub fn peer_names(&self) -> HashMap<[u8; 32], String> {
        self.all
            .iter()
            .filter_map(|e| match e {
                Entry::Peer(row) => Some((row.public_key, row.label.clone())),
                _ => None,
            })
            .collect()
    }

    /// How many peers are online, out of how many.
    pub fn online_count(&self) -> (usize, usize) {
        self.all.iter().fold((0, 0), |(on, all), e| match e {
            Entry::Peer(row) => (on + usize::from(row.online), all + 1),
            _ => (on, all),
        })
    }
}

/// The cover rate really in effect for a peer: what their connection is
/// sending at if they're connected (which a requirement of theirs can
/// raise above ours — marked `req'd by them`), otherwise what it'll
/// start at. `req` marks our own requirement of them.
fn cover_tag(node: &NodeHandle, name: &str, own: Option<u64>, required: bool) -> Option<String> {
    let live = node.peer_status(name).and_then(|s| s.cover);
    let mut tag = match live {
        Some(live) => {
            let rate = format_bitrate(live.sends?);
            if live.raised_by_peer { format!("{rate} req'd by them") } else { rate }
        }
        None => format_bitrate(own.or(node.default_cover())?),
    };
    if required {
        tag.push_str(", req");
    }
    Some(tag)
}

#[cfg(test)]
mod tests {
    use super::{moved_news, Adding, Closed, DeviceInfo, Entry, FollowRow, Section, Sidebar};

    /// Muted, a group's sound and notification go by what names you
    /// alone; its tag says `@` while something does, else `muted`.
    #[test]
    fn a_muted_group_is_noticed_only_when_you_are_named() {
        use super::{ChannelId, ChannelRow};
        let row = |unread, mentions, muted| ChannelRow { id: ChannelId { name: "general".to_string(), owner: [1; 32] }, name: "Ann, Bob".to_string(), unread, mentions, muted };
        assert_eq!((row(5, 0, false).noticed(), row(5, 0, false).tag()), (5, ""));
        assert_eq!((row(5, 2, false).noticed(), row(5, 2, false).tag()), (5, "@"));
        assert_eq!((row(5, 0, true).noticed(), row(5, 0, true).tag()), (0, "muted"));
        assert_eq!((row(5, 2, true).noticed(), row(5, 2, true).tag()), (2, "@"));
    }

    #[test]
    fn in_contacts_each_section_ends_with_its_ways_to_add_and_the_followed_are_contacts() {
        let device = Entry::Device(DeviceInfo { key: [1; 32], name: "phone".to_string(), address: String::new(), typed: String::new(), added_at: 0, this: true, online: false, relay: false, receive_only: false, through: None });
        let followed = Entry::Followed(FollowRow { public_key: [2; 32], address: String::new(), label: "ann".to_string(), unread: 0 });
        let mut sidebar = Sidebar { all: vec![Entry::You, device, followed], ..Default::default() };
        sidebar.lay_out();
        let ids = |sidebar: &Sidebar| sidebar.rows.iter().map(Entry::id).collect::<Vec<_>>();
        let plain = ids(&sidebar);
        assert_eq!(plain[4], "heading peers");
        assert!(plain[5].starts_with("followed "));
        assert_eq!(plain.len(), 6, "no GROUPS while there are none: {plain:?}");
        sidebar.set_adding(true);
        let adding = ids(&sidebar);
        assert_eq!(
            adding[4..],
            ["add device", "heading peers", plain[5].as_str(), "add contact", "add follow", "heading groups", "add new-group"]
        );
        assert!(matches!(&sidebar.rows[5], Entry::Heading(h) if h.count == 1));
        sidebar.closed.toggle(Section::Peers);
        sidebar.lay_out();
        assert!(!ids(&sidebar).iter().any(|id| id == "add contact"));
        assert!(sidebar.entry(&"add device".to_string()).is_none());
        sidebar.set_adding(false);
        assert!(!sidebar.rows.iter().any(|e| matches!(e, Entry::Add(_))));
        assert_eq!(Adding::in_section(Section::You), []);
    }

    #[test]
    fn your_devices_are_under_their_own_heading_in_you() {
        let device = |name: &str, this: bool| Entry::Device(DeviceInfo { key: [name.len() as u8; 32], name: name.to_string(), address: String::new(), typed: String::new(), added_at: 0, this, online: false, relay: false, receive_only: false, through: None });
        let mut sidebar = Sidebar { all: vec![Entry::You, device("phone", true), device("server", false)], ..Default::default() };
        sidebar.lay_out();
        let ids: Vec<String> = sidebar.rows.iter().map(Entry::id).collect();
        assert_eq!(ids[0], "heading you");
        assert_eq!(ids[1], sidebar.all[0].id());
        assert_eq!(ids[2], "heading devices");
        assert_eq!(&ids[3..], [sidebar.all[1].id(), sidebar.all[2].id()]);
        assert!(ids[3].starts_with("device ") && ids[3] != ids[4]);
        assert!(matches!(&sidebar.rows[0], Entry::Heading(h) if h.count == 2), "your devices, not you");
        assert!(matches!(&sidebar.rows[2], Entry::Heading(h) if h.count == 2));
        sidebar.closed.toggle(Section::Devices);
        sidebar.lay_out();
        assert_eq!(sidebar.rows.iter().map(Entry::id).collect::<Vec<_>>(), ids[..3]);
        assert_eq!(sidebar.shown_heading(Section::Devices), Some(2));
        sidebar.closed.toggle(Section::You);
        sidebar.lay_out();
        assert_eq!(sidebar.rows.len(), 1);
        assert_eq!(sidebar.shown_heading(Section::Devices), Some(0));
        assert!(sidebar.entry(&ids[4]).is_some());
        sidebar.set_tree(true);
        assert_eq!(sidebar.rows.len(), 1);
        sidebar.closed.toggle(Section::You);
        sidebar.lay_out();
        assert_eq!(sidebar.rows.iter().map(Entry::id).collect::<Vec<_>>(), [&ids[..2], &ids[3..]].concat());
        assert_eq!(sidebar.shown_heading(Section::Devices), Some(0), "You's");
        sidebar.set_devices_shown(false);
        assert_eq!(sidebar.rows.iter().map(Entry::id).collect::<Vec<_>>(), ids[..2]);
        assert!(sidebar.entry(&ids[4]).is_some(), "still to be found");
        sidebar.set_devices_shown(true);
        assert_eq!(sidebar.rows.len(), 4);
    }

    #[test]
    fn closed_sections_are_kept_as_their_keys_and_read_back() {
        let mut closed = Closed::default();
        assert_eq!(closed.to_text(), "");
        closed.toggle(Section::Channels);
        closed.toggle(Section::You);
        assert_eq!(closed.to_text(), "you,groups");
        assert_eq!(Closed::from_text("you, groups,nonsense"), closed);
        closed.toggle(Section::You);
        assert!(!closed.contains(Section::You) && closed.contains(Section::Channels));
    }

    #[test]
    fn a_move_is_told_once_at_its_latest() {
        assert_eq!(moved_news(Vec::new()), None);
        let moves = vec![
            ("owen".to_string(), vec!["fd06::1".to_string()]),
            ("mike".to_string(), vec!["2001:db8::5".to_string()]),
            ("owen".to_string(), vec!["fd06::2".to_string(), "192.168.1.4".to_string()]),
        ];
        assert_eq!(moved_news(moves).unwrap(), "mike has moved — now at 2001:db8::5; owen has moved — now at fd06::2, 192.168.1.4");
    }
}
