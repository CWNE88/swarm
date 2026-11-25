//! What the window shows, worked out from the app's state as plain data
//! (`Snapshot`, which can cross from the controller's thread to the
//! window's), and applying it to the window. Lists are only replaced
//! when they've changed, so scrolling and selection aren't disturbed by
//! the regular refresh.

use crate::controller::{Asking, State, View};
use crate::{App, CallMember, ChatLine, Controlling, Detail, Hint, LineKind, Member, NoteKind, Pane, PeerState, Pictured, PostItem, Row, RowKind, StripTile, TransferItem, TransferState};
use slint::{Image, ModelRc, Rgb8Pixel, SharedPixelBuffer, SharedString, StyledText, VecModel};
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use swarm_app::chat::{self, ChatItem, ChatWhat, Sent};
use swarm_app::links;
use swarm_app::colours::NameColour;
use swarm_app::names::{my_name, short_key};
use swarm_app::sidebar::Entry;
use swarm_app::text::human_size;
use swarm_app::{paths, time};
use swarm_core::node::PostsResult;
use swarm_core::posts::{self, Post, Visibility};
use swarm_core::video::Picture;
use swarm_app::calls::Remote;
use tokio::sync::watch;

/// Someone's picture, as RGB pixels (`SHOWN_SIDE` square), if they've one.
pub type Avatar = Option<Arc<Vec<u8>>>;

/// Everything the window shows, as plain data.
#[derive(Default)]
pub struct Snapshot {
    pub me: crate::YouShown,
    pub my_avatar: Avatar,
    pub top: crate::TopLine,
    pub call: crate::CallShown,
    pub call_members: Vec<CallMember>,
    /// Everyone who can be added to the call (the phone's Add, in a video
    /// call).
    pub addable: Vec<SharedString>,
    /// Whom what's typed in a picker matches (see `picks`).
    pub picks: Vec<SharedString>,
    pub fatal: String,
    /// This device was taken off your devices (`fatal` says so).
    pub wiped: bool,
    pub view: Pane,
    /// Your 12 recovery words, on the screen that shows them once.
    pub recovery_words: Vec<String>,
    pub first_run_error: String,
    /// Adding this device: asking the other one.
    pub first_run_busy: bool,
    /// The first run's camera is open, reading another device's code.
    pub first_run_scanning: bool,
    /// A code it read, to put in the code's box.
    pub first_run_code: Option<String>,
    /// A passphrase offered, on the first-run and unlock screens.
    pub suggested: String,
    /// The list narrowed to pictures, and the width it was dragged to
    /// (0, by the window's).
    pub list_narrow: bool,
    pub list_width: u32,
    /// A channel's members folded away, and the width they were dragged
    /// to (0, the usual).
    pub members_hidden: bool,
    pub members_width: u32,
    pub theme: i32,
    /// Percent of normal.
    pub text_size: u32,
    pub settings: crate::SettingsShown,
    pub rows: Vec<Row>,
    /// Each row's picture, if it has one (`rows`' own is filled in by
    /// the window, which alone can hold an image).
    pub row_avatars: Vec<Avatar>,
    pub peer: crate::PeerShown,
    pub peer_avatar: Avatar,
    pub lines: Vec<ChatLine>,
    /// The names Tab completes to in the chat shown (see `chat::complete`).
    pub chat_names: Vec<String>,
    pub chat: crate::ChatShown,
    pub public_posts: Vec<PostItem>,
    pub private_posts: Vec<PostItem>,
    pub channel: crate::ChannelShown,
    pub members: Vec<Member>,
    pub my_public: Vec<PostItem>,
    pub my_private: Vec<PostItem>,
    pub global_addresses: Vec<String>,
    pub local_addresses: Vec<String>,
    /// They see you at: an IP others see this device's connections come
    /// from (label), and who (value: "Rupert, Ann").
    pub seen_at: Vec<Detail>,
    pub req: crate::RequestDetails,
    pub pd: crate::PeerDetails,
    pub pd_avatar: Avatar,
    /// Where they can be reached, one address a row (see `peerbook::address_rows`).
    pub pd_places: Vec<Detail>,
    pub blocked: Vec<Detail>,
    /// Who's on this network, while Add a contact is open (see
    /// `swarm_app::discovery`).
    pub nearby: Vec<crate::NearbyRow>,
    pub nearby_looked: bool,
    /// One of your devices, chosen under You.
    pub dd: crate::DeviceDetails,
    /// A code for adding a device, while it's shown ("" not).
    pub device_code: String,
    pub cd: crate::ChannelDetails,
    pub detected_address: String,
    pub transfers: Vec<TransferItem>,
    /// Whose they are, when they're one peer's ("": everyone's).
    pub transfers_with: SharedString,
    pub transfers_finished: i32,
    /// The cameras to choose from, and the one chosen (Settings and
    /// calls only).
    pub cameras: Vec<String>,
    /// The microphones and speakers to choose from, the system's own
    /// first (Settings only).
    pub microphones: Vec<String>,
    pub speakers: Vec<String>,
    /// The same by short names, for the phone's Camera in a call.
    pub call_cameras: Vec<String>,
    pub ask: crate::Asking,
    /// Control of a shared screen, either way.
    pub control: swarm_app::calls::ControlState,
    pub remote_avatar: Avatar,
    /// How the call's going, a row each (Settings, Call details).
    pub call_details: Vec<Detail>,
    pub preview_shown: bool,
    pub preview_info: String,
}

impl Snapshot {
    /// The first-run screen: no identity yet.
    pub fn first_run() -> Snapshot {
        Snapshot { view: Pane::FirstRun, text_size: 100, settings: crate::SettingsShown { text_size: 1, ..Default::default() }, ..Default::default() }
    }

    /// The first-run screen again, saying why the words typed weren't
    /// taken.
    pub fn first_run_error(why: String) -> Snapshot {
        Snapshot { first_run_error: why, ..Snapshot::first_run() }
    }

    /// Asking for the passphrase, and why the last one didn't open it.
    pub fn unlock(why: String) -> Snapshot {
        Snapshot { view: Pane::Unlock, first_run_error: why, text_size: 100, settings: crate::SettingsShown { text_size: 1, ..Default::default() }, ..Default::default() }
    }

    /// A new identity's 12 recovery words, shown once.
    pub fn recovery_words(words: Vec<String>) -> Snapshot {
        Snapshot { view: Pane::Words, recovery_words: words, text_size: 100, settings: crate::SettingsShown { text_size: 1, ..Default::default() }, ..Default::default() }
    }

    /// Taken off your devices by another of yours, and wiped.
    pub fn wiped() -> Snapshot {
        Snapshot { wiped: true, ..Snapshot::fatal(swarm_app::devices::WIPED.to_string()) }
    }

    /// Couldn't start: says why.
    pub fn fatal(why: String) -> Snapshot {
        Snapshot { view: Pane::Fatal, fatal: why, text_size: 100, settings: crate::SettingsShown { text_size: 1, ..Default::default() }, ..Default::default() }
    }
}

fn role(colour: NameColour) -> i32 {
    match colour {
        NameColour::Me => 0,
        NameColour::Peer => 1,
        NameColour::Member(slot) => 2 + slot as i32,
        NameColour::Other => 12,
    }
}

fn s(text: impl Into<SharedString>) -> SharedString {
    text.into()
}

/// Longest between two messages of one person's that are shown as one
/// run (seconds).
const RUN_GAP: u64 = 5 * 60;

/// A chat's lines for the window: a heading when the day changes, then
/// each line with its time; and every name said in it, one per line, for
/// the window to measure the name column by (the widest drawn, not the
/// one with most letters: "Bram" is wider than "Lilli").
fn chat_lines(items: &[ChatItem], utc: bool) -> (Vec<ChatLine>, SharedString) {
    let mut lines = Vec::new();
    let mut last_day = None;
    let mut names: Vec<&str> = Vec::new();
    let mut run: Option<(&str, swarm_app::colours::NameColour, u64)> = None;
    for item in items {
        let when = time::when(item.at, utc);
        if last_day != Some(when.day_number) {
            last_day = Some(when.day_number);
            run = None;
            lines.push(ChatLine { kind: LineKind::Day, time: s(""), name: s(""), role: 12, text: s(time::day_heading(item.at, utc)), mark: s(""), for_me: false, linked: false, rich: StyledText::default(), refs: s(""), deleting: false, first: true });
        }
        let clock = s(format!("{:02}:{:02}", when.hour, when.minute));
        let mark = s(item.sent.mark());
        let first = match &item.what {
            ChatWhat::Said { name, colour, .. } => {
                let first = !run.is_some_and(|(n, c, at)| n == name.as_str() && c == *colour && item.at.saturating_sub(at) < RUN_GAP);
                run = Some((name.as_str(), *colour, item.at));
                first
            }
            _ => {
                run = None;
                true
            }
        };
        let line = match &item.what {
            ChatWhat::Said { name, colour, text } => {
                if !names.contains(&name.as_str()) {
                    names.push(name);
                }
                ChatLine { kind: LineKind::Said, time: clock, name: s(name.as_str()), role: role(*colour), text: s(text.as_str()), mark, for_me: false, linked: false, rich: StyledText::default(), refs: s(""), deleting: false, first: true }
            }
            ChatWhat::Happened { mark: sign, text } => ChatLine { kind: LineKind::Happened, time: clock, name: s(*sign), role: 12, text: s(text.as_str()), mark, for_me: false, linked: false, rich: StyledText::default(), refs: s(""), deleting: false, first: true },
            ChatWhat::Did { name, colour, text } => ChatLine { kind: LineKind::Did, time: clock, name: s("*"), role: role(*colour), text: s(format!("{name} {text}")), mark, for_me: false, linked: false, rich: StyledText::default(), refs: s(""), deleting: false, first: true },
            ChatWhat::Missed { name } => ChatLine { kind: LineKind::Missed, time: clock, name: s(""), role: 12, text: s(chat::missed_call(name)), mark, for_me: false, linked: false, rich: StyledText::default(), refs: s(""), deleting: false, first: true },
        };
        let refs = s(item.lines.iter().map(|l| l.encode()).collect::<Vec<_>>().join(","));
        lines.push(with_links(ChatLine { for_me: item.for_me, refs, deleting: item.sent == Sent::Deleting, first, ..line }));
    }
    (lines, names.join("\n").into())
}

/// Chat lines `from` to `to` (either way round, both included) as
/// they're copied: the heading of the day the first is in, then one line
/// each, with its time and who.
pub fn chat_text(lines: &[ChatLine], from: i32, to: i32) -> String {
    let (Ok(first), Ok(last)) = (usize::try_from(from.min(to)), usize::try_from(from.max(to))) else { return String::new() };
    let last = last.min(lines.len().saturating_sub(1));
    if first > last || lines.is_empty() {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    if lines[first].kind != LineKind::Day {
        if let Some(day) = lines[..first].iter().rev().find(|l| l.kind == LineKind::Day) {
            out.push(format!("--- {} ---", day.text));
        }
    }
    for line in &lines[first..=last] {
        out.push(match line.kind {
            LineKind::Day => format!("--- {} ---", line.text),
            LineKind::Said => format!("{} {}: {}", line.time, line.name, line.text),
            LineKind::Happened => format!("{} {} {}", line.time, line.name, line.text),
            LineKind::Did => format!("{} * {}", line.time, line.text),
            LineKind::Missed => format!("{} {}", line.time, line.text),
        });
    }
    out.join("\n")
}

/// The text selected in chat lines: line `first` from byte `from` to line
/// `last` up to byte `to`, each message alone (no time or name) on a line
/// of its own, day headings left out.
pub fn chat_selection(lines: &[ChatLine], first: i32, from: i32, last: i32, to: i32) -> String {
    let (Ok(first), Ok(last)) = (usize::try_from(first), usize::try_from(last)) else { return String::new() };
    let mut out: Vec<&str> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(last.saturating_add(1)).skip(first) {
        if line.kind == LineKind::Day {
            continue;
        }
        let text = line.text.as_str();
        let start = if i == first { boundary(text, from) } else { 0 };
        let end = if i == last { boundary(text, to) } else { text.len() };
        out.push(text.get(start..end.max(start)).unwrap_or(""));
    }
    out.join("\n")
}

/// `at` as a byte offset in `text`, at a character's start.
fn boundary(text: &str, at: i32) -> usize {
    let mut at = usize::try_from(at).unwrap_or(0).min(text.len());
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// The part of `text` a click at byte `at` selects: that point (unit 1),
/// the word there (2: a run of letters and digits, of spaces, or of
/// anything else), or the whole of it (3).
pub fn span_around(text: &str, at: usize, unit: i32) -> (usize, usize) {
    let at = boundary(text, at as i32);
    match unit {
        2 => {
            let kind = |c: char| if c.is_alphanumeric() || c == '_' || c == '\'' { 0 } else if c.is_whitespace() { 1 } else { 2 };
            let Some(here) = text[at..].chars().next().or_else(|| text[..at].chars().next_back()) else { return (at, at) };
            let k = kind(here);
            let start = text[..at].char_indices().rev().take_while(|(_, c)| kind(*c) == k).last().map_or(at, |(i, _)| i);
            let end = text[at..].char_indices().find(|(_, c)| kind(*c) != k).map_or(text.len(), |(i, _)| at + i);
            (start, end)
        }
        3 => (0, text.len()),
        _ => (at, at),
    }
}

/// A chat line with its web addresses made links (`rich`), if it has any.
pub(crate) fn with_links(line: ChatLine) -> ChatLine {
    let Some(md) = links::markdown(&line.text, line.kind == LineKind::Did) else { return line };
    let rich = StyledText::from_markdown(&md).unwrap_or_else(|_| StyledText::from_plain_text(&line.text));
    ChatLine { linked: true, rich, ..line }
}

/// `mine`: the user's own, which can be deleted (`PostItem::refs`).
fn post_items(posts: &[&Post], utc: bool, mine: bool) -> Vec<PostItem> {
    posts
        .iter()
        .map(|p| {
            let refs = if mine { s(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, p.signature)) } else { s("") };
            PostItem { refs, ..post_item(s(time::date_time(p.created_at, utc)), &p.text) }
        })
        .collect()
}

/// A post, its web addresses made links (`rich`) if it has any.
pub(crate) fn post_item(when: SharedString, text: &str) -> PostItem {
    let rich = links::markdown(text, false).map(|md| StyledText::from_markdown(&md).unwrap_or_else(|_| StyledText::from_plain_text(text)));
    PostItem { when, text: s(text), linked: rich.is_some(), rich: rich.unwrap_or_default(), refs: s("") }
}

/// Newest first, split into for-anyone and for-peers; `mine`, the user's.
fn split_posts(all: &[Post], utc: bool, mine: bool) -> (Vec<PostItem>, Vec<PostItem>) {
    let (public, private): (Vec<&Post>, Vec<&Post>) = all.iter().rev().partition(|p| p.visibility == Visibility::Public);
    (post_items(&public, utc, mine), post_items(&private, utc, mine))
}

/// The most of a conversation the window holds: made afresh several
/// times a second, a long one in full took most of a core.
const CHAT_SHOWN: usize = 2000;

/// The newest `CHAT_SHOWN` of a conversation, after a line saying how
/// many earlier ones there are, if any.
fn newest_of_chat(row: &swarm_app::sidebar::PeerRow) -> Vec<chat::ChatItem> {
    let mut items = chat::peer_chat_last(row, CHAT_SHOWN + 1);
    if items.len() > CHAT_SHOWN {
        let earlier = swarm_core::history::summary(&row.folder.history(), &row.public_key).map_or(0, |s| s.count).saturating_sub(CHAT_SHOWN);
        items.drain(..items.len() - CHAT_SHOWN);
        let at = items.first().map_or(0, |i| i.at);
        items.insert(0, chat::ChatItem { at, what: chat::ChatWhat::Happened { mark: "…", text: format!("{earlier} earlier messages aren't shown here (the terminal UI has them all)") }, sent: chat::Sent::NotOurs, for_me: false, lines: Vec::new() });
    }
    items
}

/// What the window shows now.
/// Whether the cameras are looked for: in Settings, to choose one, and
/// in a call, for the phone's Camera to offer.
fn cameras_listed(view: Pane, in_call: bool) -> bool {
    view == Pane::Settings || in_call
}

pub fn build(state: &mut State) -> Snapshot {
    let node = state.node.clone();
    let utc = state.prefs.utc;
    let me = node.identity().public_key_bytes();
    let outside = swarm_app::addrs::seen_from_outside(&state.node, &state.sidebar.peer_names());
    let my_avatar = swarm_app::avatar::pixels(&me, true);
    let (scanned_count, scanned) = &state.scanned;
    let mut snap = Snapshot {
        me: crate::YouShown {
            name: my_name().unwrap_or_else(|| "you".to_string()).into(),
            says: swarm_app::avatar::status(&me, true).into(),
            pictured: my_avatar.is_some(),
            key: node.identity().public_key_base64().into(),
            scanned_count: *scanned_count,
            scanned_name: scanned.as_ref().map(|c| c.name.as_str().into()).unwrap_or_default(),
            scanned_address: scanned.as_ref().map(|c| c.address_text().into()).unwrap_or_default(),
            scanned_key: scanned.as_ref().map(|c| c.key_text().into()).unwrap_or_default(),
            listening_text: {
                let local = state.node.local_addr();
                let families = if local.as_ref().is_ok_and(|a| a.is_ipv4()) { "IPv4 only" } else { "IPv6 and IPv4" };
                format!("listening on port {}, {families}", local.map(|a| a.port()).unwrap_or(0)).into()
            },
            ..Default::default()
        },
        my_avatar,
        top: crate::TopLine { clock: time::clock(swarm_app::unix_now(), utc).into(), ..Default::default() },
        theme: state.prefs.theme.index(),
        text_size: state.prefs.text_size,
        list_narrow: state.prefs.list_narrow,
        list_width: state.prefs.list_width,
        members_hidden: state.prefs.members_hidden,
        members_width: state.prefs.members_width,
        chat: crate::ChatShown { show_times: state.prefs.show_times, ..Default::default() },
        peer: crate::PeerShown { tab: state.peer_tab, ..Default::default() },
        settings: crate::SettingsShown {
            text_size: swarm_app::prefs::TEXT_SIZES.iter().position(|s| *s == state.prefs.text_size).unwrap_or(1) as i32,
            times: state.prefs.show_times,
            utc,
            joins: state.prefs.show_joins,
            echo: state.prefs.voice.cancel_echo,
            noise: state.prefs.voice.suppress_noise,
            level: state.prefs.voice.steady_level,
            video_size: swarm_app::prefs::VIDEO_WIDTHS.iter().position(|w| *w == state.prefs.video_width).unwrap_or(0) as i32,
            lock_index: swarm_app::prefs::LOCK_AFTER.iter().position(|m| *m == state.prefs.lock_after).unwrap_or(0) as i32,
            call_info: state.prefs.call_info,
            notify: state.prefs.notify.index(),
            sound_name: swarm_app::sound::chosen_name(&paths().settings()).into(),
            ringtone_name: swarm_app::ringtone::chosen_name(&paths().settings()).into(),
            order: match state.prefs.peer_order {
                swarm_app::prefs::PeerOrder::Added => 0,
                swarm_app::prefs::PeerOrder::Name => 1,
                swarm_app::prefs::PeerOrder::Recent => 2,
            },
            zone_text: time::zone_name().into(),
            ..Default::default()
        },
        detected_address: swarm_app::addrs::detected_address(),
        global_addresses: swarm_app::addrs::global_addresses().into_iter().map(|a| a.to_string()).collect(),
        local_addresses: swarm_app::addrs::local_addresses().into_iter().map(|a| a.to_string()).collect(),
        seen_at: outside.iter().map(|(a, who)| Detail { label: a.to_string().into(), value: who.join(", ").into() }).collect(),
        ..Default::default()
    };
    let addrs = swarm_app::addrs::global_addresses().into_iter().chain(outside.iter().map(|(a, _)| *a)).chain(swarm_app::addrs::local_addresses());
    let manual = state.node.manual_address();
    snap.me.manual = manual.text().into();
    snap.me.contact = swarm_app::contact::line(&me, &manual.items, &addrs.collect::<Vec<_>>(), &my_name().unwrap_or_default()).into();
    let (online, total) = state.sidebar.online_count();
    snap.top.online = format!("{online} of {total} online").into();
    snap.top.name = snap.me.name.clone();
    if let Some(progress) = state.progress.of(&state.node.transfers()) {
        snap.top.progress_what = progress.what.into();
        snap.top.progress_percent = format!("{}%", progress.percent).into();
        snap.top.progress_speed = progress.speed.into();
        snap.top.progress_left = progress.left.into();
        snap.top.progress = f32::from(progress.percent) / 100.0;
    }

    if let Some((note, _)) = &state.status {
        snap.top.status = note.text.clone().into();
        snap.top.status_kind = match note.kind {
            swarm_app::notes::Kind::Info => NoteKind::Info,
            swarm_app::notes::Kind::Success => NoteKind::Success,
            swarm_app::notes::Kind::Warning => NoteKind::Warning,
            swarm_app::notes::Kind::Error => NoteKind::Error,
        };
    }

    if let Some(call) = state.calls.active() {
        let secs = call.started_at.elapsed().as_secs();
        snap.call.text = format!("● on a call with {}  {:02}:{:02}", call.peer_name, secs / 60, secs % 60).into();
        if let Some(name) = state.calls.adding() {
            snap.call.text.push_str(&format!("  ringing {name}..."));
        }
        if let Some(note) = call.conference_note() {
            snap.call.text.push_str(&format!("  {note}"));
        }
        snap.call_members = call.heard().into_iter().map(|(number, name, key, removable)| CallMember { number: number as i32, name: name.into(), removable, talking: key.is_some_and(|key| call.is_talking(&key)) }).collect();
        snap.call.live = true;
        snap.call.muted = call.muted();
        snap.call.can_camera = call.video.is_some() || !call.in_conference();
        snap.control = call.control_state();
        let control = &snap.control;
        snap.call.control_asking = control.asking.clone().unwrap_or_default().into();
        snap.call.control_asking_screen = control.asking_screen;
        snap.call.controlled_by = control.controller.as_ref().map(|(name, _)| name.clone()).unwrap_or_default().into();
        snap.call.control_paused = control.controller.as_ref().is_some_and(|(_, paused)| *paused);
        snap.call.controlling = control.watching.as_ref().map(|(name, _)| name.clone()).unwrap_or_default().into();
        snap.call.control_state = match control.watching.as_ref().map(|(_, state)| *state) {
            None => Controlling::No,
            Some(swarm_app::remote::Watching::Asking) => Controlling::Asking,
            Some(swarm_app::remote::Watching::Controlling) => Controlling::Yes,
            Some(swarm_app::remote::Watching::Paused) => Controlling::Paused,
        };
        snap.call.can_control = control.can_ask;
        if let (Some(video), Some((remote, name))) = (&call.video, call.shown_remote()) {
            snap.call.video_shown = true;
            snap.call.local_on = video.camera.is_some();
            snap.call.sharing = video.screen.is_some();
            snap.call.remote_note = remote.note(&name).unwrap_or_default().into();
            snap.remote_avatar = swarm_app::avatar::pixels(&remote.key, false);
            snap.call.remote_pictured = snap.remote_avatar.is_some();
            let theirs = if remote.their_screen() { format!("{name}'s screen") } else { format!("with {name}") };
            let note = video.camera_note.as_ref().filter(|_| video.screen.is_none()).map(|n| format!(" — {n}")).unwrap_or_default();
            snap.call.video_info = format!("{theirs}{note}").into();
        } else {
            let key = call.heard().into_iter().find_map(|(_, _, key, _)| key);
            snap.remote_avatar = key.and_then(|key| swarm_app::avatar::pixels(&key, false));
            snap.call.remote_pictured = snap.remote_avatar.is_some();
            snap.call.remote_note = call.peer_name.clone().into();
            snap.call.video_info = format!("with {}", call.peer_name).into();
        }
        if state.prefs.call_info {
            snap.call_details = call.details().into_iter().map(|(label, value)| Detail { label: label.into(), value: value.into() }).collect();
        }
    } else if let Some(name) = state.calls.outgoing() {
        let shown = node.label_of(name);
        snap.call.text = format!("calling {shown}...").into();
        snap.call.calling_out = true;
        let key = state.sidebar.peer_by_name(name).map(|row| row.public_key).or_else(|| state.sidebar.all.iter().find_map(|e| match e {
            Entry::Device(d) if format!("#device {}", base64(&d.key)) == name || swarm_app::devices::label(d) == name => Some(d.key),
            _ => None,
        }));
        snap.remote_avatar = key.and_then(|key| swarm_app::avatar::pixels(&key, false));
        snap.call.remote_pictured = snap.remote_avatar.is_some();
        snap.call.remote_note = format!("calling {shown}...").into();
    }
    if let Some(incoming) = state.calls.incoming() {
        snap.ask.ring_shown = true;
        snap.ask.ring_from = incoming.from.clone().into();
        if !incoming.with.is_empty() {
            snap.ask.ring_from = format!("{} (a conference with {})", incoming.from, incoming.with.join(", ")).into();
        }
        snap.ask.ring_video = incoming.video;
        snap.ask.ring_seconds = incoming.deadline.saturating_duration_since(std::time::Instant::now()).as_secs() as i32;
    }
    if let Some(preview) = state.calls.preview() {
        snap.preview_shown = true;
        snap.me.avatar_taking = state.avatar_taking;
        snap.me.scanning = state.scanning;
        snap.preview_info = match preview {
            Ok(camera) => {
                let info = camera.info();
                format!("{} — {}×{} {} at {:.0}/s (calls start at this; up to 3840×2160 when they can)", info.name, info.width, info.height, info.format, camera.frames_per_second())
            }
            Err(why) => format!("unavailable: {why}"),
        };
    }

    let contacts = matches!(state.view, View::Peers | View::Channels);
    state.sidebar.set_adding(contacts);
    state.sidebar.set_devices_shown(contacts);
    if !contacts && matches!(state.selected_entry(), Some(Entry::Device(_))) {
        state.selected = Some(Entry::You.id());
    }
    let mut rows = Vec::new();
    let mut row_avatars = Vec::new();
    let mut row_map = Vec::new();
    let selected = state.selected.clone();
    for (i, entry) in state.sidebar.rows.iter().enumerate() {
        let selected = selected.as_ref() == Some(&entry.id());
        let row = match entry {
            Entry::Heading(h) => Row { kind: RowKind::Heading, mark: s(h.triangle()), mark_role: 12, name: s(h.section.title()), tag: s(h.closed_text()), unread: 0, dim: false, selected: false, letter: s(" "), inner: h.section.within().is_some(), ..Default::default() },
            Entry::You => Row { kind: RowKind::You, mark: s("»"), mark_role: 0, name: s(snap.me.name.as_str()), sub: s(short_key(&me)), ring: 1, unread: 0, dim: false, selected, ..Default::default() },
            Entry::Device(d) => {
                let up = d.this || d.online;
                let mut sub = swarm_app::devices::state(d).to_string();
                if d.relay {
                    sub += " · relays";
                }
                if d.receive_only {
                    sub += " · receives only";
                }
                Row { kind: RowKind::Device, mark: s(if up { "●" } else { "○" }), mark_role: if up { 1 } else { 12 }, name: s(swarm_app::devices::label(d)), sub: s(sub), ring: i32::from(up), unread: 0, dim: !up, selected, inner: true, ..Default::default() }
            }
            Entry::Peer(p) => {
                let (mark, mark_role, ring) = if p.not_added || p.removed_us { ("○", 13, 2) } else if p.online { ("●", 1, 1) } else { ("○", 12, 0) };
                let tag = p.tag();
                let sub = if tag.is_empty() { short_key(&p.public_key) } else { format!("{} · {tag}", short_key(&p.public_key)) };
                Row { kind: RowKind::Peer, mark: s(mark), mark_role, name: s(p.list_label.as_str()), tag: s(tag), sub: s(sub), ring, unread: p.unread as i32, dim: !p.online, selected, ..Default::default() }
            }
            Entry::Request(r) => Row { kind: RowKind::Request, mark: s("?"), mark_role: 13, name: s(r.label()), tag: s("asks to be added"), sub: s(format!("{} · asks to be added", short_key(&r.public_key))), ring: 2, unread: 0, dim: false, selected, ..Default::default() },
            Entry::Followed(f) => Row { kind: RowKind::Followed, mark: s("~"), mark_role: 14, name: s(f.label.as_str()), sub: s(format!("{} · followed", short_key(&f.public_key))), unread: f.unread as i32, dim: false, selected, ..Default::default() },
            Entry::Channel(c) => {
                let tag = c.tag();
                let sub = if tag.is_empty() { "group".to_string() } else { format!("group · {tag}") };
                Row { kind: RowKind::Channel, mark: s("#"), mark_role: 14, name: s(c.name.as_str()), tag: s(tag), sub: s(sub), letter: s("#"), unread: c.unread as i32, dim: false, selected, ..Default::default() }
            }
            Entry::Add(adding) => Row { kind: RowKind::Add, mark: s(adding.key()), name: s(adding.text()), letter: s("+"), inner: adding.section().within().is_some(), ..Default::default() },
        };
        let row = if row.letter.is_empty() { Row { letter: s(row.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()), ..row } } else { row };
        rows.push(row);
        row_avatars.push(match entry {
            Entry::You => snap.my_avatar.clone(),
            Entry::Peer(p) => swarm_app::avatar::of_peer(&p.folder, &p.public_key).0,
            _ => None,
        });
        row_map.push(Some(i));
    }
    snap.rows = rows;
    snap.row_avatars = row_avatars;
    state.row_map = row_map;

    let entry = state.selected_entry().cloned();
    snap.view = match state.view {
        View::Chats => match &entry {
            Some(Entry::Device(_)) => Pane::Peers,
            Some(Entry::Peer(_)) => Pane::Peer,
            Some(Entry::Channel(_)) => Pane::Channel,
            Some(Entry::Request(_)) => Pane::Request,
            Some(Entry::Followed(_)) => Pane::Followed,
            _ => Pane::Mine,
        },
        View::Peers if matches!(entry, Some(Entry::Channel(_))) => Pane::Channels,
        View::Peers => Pane::Peers,
        View::Channels => Pane::Channels,
        View::Transfers => Pane::Transfers,
        View::Settings => Pane::Settings,
        View::Help => Pane::Help,
    };
    match &entry {
        Some(Entry::Peer(row)) => {
            if let Some((3, typed)) = &state.picking {
                let names = state.sidebar.all.iter().filter_map(|e| match e {
                    Entry::Peer(other) if other.public_key != row.public_key => Some(other.list_label.clone()),
                    _ => None,
                });
                snap.picks = picks(names, typed);
            }
            snap.peer.name = row.label.clone().into();
            snap.peer.status = row.status_text(utc).into();
            let timer = node.timer(&row.handle).seconds;
            if timer > 0 {
                snap.peer.status = format!("{} · messages deleted after {}", snap.peer.status, swarm_core::disappearing::describe(timer)).into();
            }
            snap.peer.state = if row.removed_us { PeerState::Removed } else if row.not_added { PeerState::NotAdded } else if row.online { PeerState::Online } else { PeerState::Offline };
            snap.peer.asked = row.asked;
            let (avatar, says) = swarm_app::avatar::of_peer(&row.folder, &row.public_key);
            snap.peer.says = says.into();
            snap.peer_avatar = avatar;
            snap.peer.pictured = snap.peer_avatar.is_some();
            snap.pd.says = snap.peer.says.clone();
            snap.pd_avatar = snap.peer_avatar.clone();
            snap.pd.pictured = snap.pd_avatar.is_some();
            snap.peer.new_posts = state.timelines.has_new(&row.handle);
            (snap.lines, snap.chat.widest_name) = chat_lines(&newest_of_chat(row), utc);
            snap.chat_names = chat::peer_names(row);
            snap.chat.key = format!("peer {}", row.handle).into();
            snap.chat.empty = format!("nothing said yet — Enter to write to {}", row.label).into();
            let (posts, note) = match state.timelines.posts.get(&row.handle) {
                None => (Vec::new(), "asking them for their posts...".to_string()),
                Some(PostsResult::Live(posts)) => (posts.clone(), String::new()),
                Some(PostsResult::Cached { posts, .. }) => (posts.clone(), "they're not reachable — this is what you saw last".to_string()),
            };
            (snap.public_posts, snap.private_posts) = split_posts(&posts, utc, false);
            snap.peer.posts_note = note.into();
            snap.pd.shown = true;
            snap.pd.name = row.label.clone().into();
            snap.pd.status = row.status_text(utc).into();
            snap.pd.address = row.address.clone().into();
            snap.pd_places = swarm_app::peerbook::address_rows(&node, row, utc).into_iter().map(|(label, value)| Detail { label: label.into(), value: value.into() }).collect();
            snap.pd.key = base64(&row.public_key).into();
            snap.pd.contact = swarm_app::contact::peer_line(&row.public_key, &row.address, &row.label).into();
            snap.pd.cover = row.cover_text(&node).into();
            snap.pd.cover_setting = swarm_app::peerbook::cover_setting_text(row).into();
            snap.pd.calls = row.auto_answer_text().to_string().into();
            snap.pd.answer_index = answer_index(row.auto_answer);
            snap.pd.control = row.auto_control;
            snap.pd.timer_index = swarm_core::disappearing::CHOICES.iter().position(|c| *c == timer).unwrap_or(0) as i32;
            snap.pd.files = swarm_app::peerbook::receive_text(&row.folder, &row.public_key).into();
            snap.pd.receive = swarm_core::settings::receive_to(&row.folder.settings(), &row.public_key).map(|f| f.display().to_string()).unwrap_or_default().into();
        }
        Some(Entry::Device(d)) => {
            let now = swarm_app::unix_now();
            snap.dd = crate::DeviceDetails {
                shown: true,
                name: swarm_app::devices::label(d).into(),
                status: swarm_app::devices::state(d).into(),
                in_use: swarm_app::devices::in_use(&node, d).unwrap_or_default().into(),
                added: swarm_app::devices::added(d, now).into(),
                address: swarm_app::devices::addresses(d).join("\n").into(),
                typed: d.typed.clone().into(),
                key: base64(&d.key).into(),
                this: d.this,
                relay: d.relay,
                receive_only: d.receive_only,
                bandwidth_limit: if d.this { swarm_app::actions::bandwidth_limit_text(&node).into() } else { Default::default() },
                receive_limit: if d.this { swarm_app::actions::receive_limit_text(&node).into() } else { Default::default() },
                linked: node.devices().linked(),
                calls: swarm_app::sidebar::auto_answer_text(swarm_app::devices::auto_answer(&node, &d.key)).into(),
                answer_index: answer_index(swarm_app::devices::auto_answer(&node, &d.key)),
                control: swarm_app::devices::auto_control(&node, &d.key),
            };
        }
        Some(Entry::Channel(row)) => {
            let view = chat::channel_view(&node, &state.sidebar, &row.id, &state.prefs);
            let people = match view.members.len() {
                1 => "just you".to_string(),
                n => format!("{n} people"),
            };
            snap.channel.name = view.name.clone().into();
            snap.channel.detail = people.clone().into();
            snap.members = view
                .members
                .iter()
                .map(|m| {
                    let contact = state.sidebar.peer_by_key(&m.key);
                    let summary = contact.map(|row| {
                        let in_use = swarm_app::peerbook::address_rows(&node, row, utc).into_iter().find(|(label, _)| *label == "in use").map(|(_, value)| value).unwrap_or_default();
                        format!("{}\nin use: {in_use}", row.status_text(utc))
                    });
                    let summary = summary.or_else(|| (!m.contact).then(|| "not your contact — click to ask them to add you".to_string()));
                    Member { name: s(m.name.as_str()), connected: m.connected, role: role(m.colour), contact: s(contact.map(|row| row.handle.as_str()).unwrap_or_default()), summary: s(summary.unwrap_or_default()) }
                })
                .collect();
            if let Some((2, typed)) = &state.picking {
                let members: Vec<[u8; 32]> = view.members.iter().map(|m| m.key).collect();
                let names = state.sidebar.all.iter().filter_map(|e| match e {
                    Entry::Peer(row) if !members.contains(&row.public_key) => Some(row.list_label.clone()),
                    _ => None,
                });
                snap.picks = picks(names, typed);
            }
            (snap.lines, snap.chat.widest_name) = chat_lines(&view.items, utc);
            snap.chat_names = chat::channel_names(&view);
            snap.chat.key = format!("channel {}", base64(&row.id.owner) + &row.id.name).into();
            snap.chat.empty = if view.can_post() { "nothing said yet — Add people… puts your contacts in it".to_string() } else { String::new() }.into();
            snap.cd.shown = true;
            snap.cd.name = view.name.into();
            snap.cd.members = people.into();
            snap.cd.named = view.named;
        }
        Some(Entry::Request(row)) => {
            snap.req.label = row.label().into();
            snap.req.key = base64(&row.public_key).into();
            snap.req.suggested = row.suggested_peer_name().into();
            snap.req.address = if row.address.is_empty() { "unknown".to_string() } else { row.address.clone() }.into();
        }
        Some(Entry::Followed(row)) => {
            snap.peer.name = row.label.clone().into();
            snap.peer.status = format!("following · {}", short_key(&row.public_key)).into();
            let posts = swarm_core::postcache::cached(&paths().postcache(), &row.public_key).unwrap_or_default();
            (snap.public_posts, _) = split_posts(&posts, utc, false);
            snap.peer.posts_note = if posts.is_empty() { "nothing yet".to_string() } else { String::new() }.into();
        }
        _ => {}
    }
    snap.pd.me = matches!(entry, Some(Entry::You));
    snap.pd.request = matches!(entry, Some(Entry::Request(_)));
    if snap.view == Pane::Mine {
        snap.me.followers_text = match node.followers() {
            1 => "1 follower".into(),
            n => format!("{n} followers").into(),
        };
        let mine = posts::all(&paths().posts()).unwrap_or_default();
        (snap.my_public, snap.my_private) = split_posts(&mine, utc, true);
    }
    let mut transfers = if snap.view == Pane::Transfers { node.transfers() } else { Vec::new() };
    if let Some(Entry::Peer(row)) = &entry {
        transfers.retain(|t| t.is_with(&row.public_key));
        snap.transfers_with = row.label.clone().into();
    }
    state.transfer_ids = transfers.iter().map(|t| t.transfer.id).collect();
    snap.transfers = transfers.iter().map(transfer_item).collect();
    snap.transfers_finished = transfers.iter().filter(|t| t.transfer.state.is_finished()).count() as i32;

    if let Some(offer) = &state.offer {
        snap.ask.offer_shown = true;
        snap.ask.offer_who = offer.peer_name.clone().unwrap_or_else(|| "someone".to_string()).into();
        snap.ask.offer_name = offer.transfer.name.clone().into();
        snap.ask.offer_size = (human_size(offer.transfer.size) + &swarm_app::prompts::transfer_estimate(&node, &state.sidebar, &snap.ask.offer_who, offer.transfer.size)).into();
    }
    if cameras_listed(snap.view, snap.call.live || snap.call.video_shown) {
        let (cameras, at) = swarm_app::camera::choices();
        (snap.cameras, snap.settings.camera_index) = (cameras, at as i32);
        snap.call_cameras = swarm_app::camera::call_choices();
    }
    if snap.view == Pane::Settings {
        use swarm_app::audio_devices::{choices, Way};
        let (microphones, at) = choices(Way::Microphone);
        (snap.microphones, snap.settings.microphone_index) = (microphones, at as i32);
        let (speakers, at) = choices(Way::Speaker);
        (snap.speakers, snap.settings.speaker_index) = (speakers, at as i32);
    }
    snap.blocked = swarm_app::actions::blocked(&node).into_iter().map(|r| Detail { label: r.label().into(), value: r.key().into() }).collect();
    snap.me.discoverable = node.discoverable().on;
    if swarm_app::discovery::browsing() {
        snap.nearby = swarm_app::discovery::nearby(&node)
            .into_iter()
            .map(|n| crate::NearbyRow {
                name: n.name.clone().into(),
                key: swarm_app::names::short_key(&n.key).into(),
                addresses: n.addresses.join(", ").into(),
                code: n.code().into(),
                known: n.known,
            })
            .collect();
        snap.nearby_looked = swarm_app::discovery::looked();
    }
    snap.device_code = state.device_code.as_ref().map(|c| c.line.clone()).unwrap_or_default();
    if let Some((asking, question)) = &state.asking {
        snap.ask.confirm_shown = true;
        snap.ask.confirm_question = question.clone().into();
        snap.ask.confirm_delete = matches!(asking, Asking::RemovePeer(..));
        snap.ask.confirm_request = matches!(asking, Asking::RemovePeer(_, true));
    }
    if state.calls.active().is_some() {
        snap.addable = state.sidebar.all.iter().filter_map(|e| match e {
            Entry::Peer(row) if state.calls.can_add(row) => Some(SharedString::from(row.list_label.as_str())),
            _ => None,
        }).collect();
        if let Some((1, typed)) = &state.picking {
            snap.picks = picks(snap.addable.iter().map(|n| n.to_string()), typed);
        }
    }
    if let Some(Entry::Peer(row)) = state.selected_entry() {
        if snap.view == Pane::Peer && state.calls.active().is_some() && state.calls.can_add(row) {
            snap.call.add_to_call = row.list_label.clone().into();
        }
        snap.call.call_why = state.calls.why_not_call(&node, row, false).unwrap_or_default().into();
        snap.call.video_why = state.calls.why_not_call(&node, row, true).unwrap_or_default().into();
    }
    if let Some(Entry::Device(device)) = state.selected_entry() {
        snap.call.call_why = state.calls.why_not_call_device(&node, device, false).unwrap_or_default().into();
        snap.call.video_why = state.calls.why_not_call_device(&node, device, true).unwrap_or_default().into();
    }
    snap
}

/// How calls are answered, as the details' choices list it.
fn answer_index(answer: swarm_core::settings::AutoAnswer) -> i32 {
    match answer {
        swarm_core::settings::AutoAnswer::Off => 0,
        swarm_core::settings::AutoAnswer::Voice => 1,
        swarm_core::settings::AutoAnswer::VoiceAndVideo => 2,
    }
}

/// At most five of `names` matching `typed` (anywhere in them, any
/// case), those starting with it first, then by name.
fn picks(names: impl Iterator<Item = String>, typed: &str) -> Vec<SharedString> {
    let typed = typed.trim().to_lowercase();
    let mut found: Vec<(bool, String)> = names.filter(|n| n.to_lowercase().contains(&typed)).map(|n| (!n.to_lowercase().starts_with(&typed), n)).collect();
    found.sort_by_key(|(later, n)| (*later, n.to_lowercase()));
    found.into_iter().take(5).map(|(_, n)| n.into()).collect()
}

fn base64(key: &[u8; 32]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.encode(key)
}

fn transfer_item(t: &swarm_core::node::TransferStatus) -> TransferItem {
    use swarm_core::transfer::{Direction, State};
    let size = t.transfer.size.max(1);
    let done = t.done_bytes.min(size);
    let incoming = t.transfer.direction == Direction::In;
    let (state, kind) = match t.transfer.state {
        State::Offered if incoming => ("offered to you".to_string(), TransferState::Offered),
        State::Offered => ("waiting for them to accept".to_string(), TransferState::Offered),
        State::Active => (format!("{}% — {} of {}", done * 100 / size, human_size(done), human_size(t.transfer.size)), TransferState::Moving),
        State::Done if incoming => (format!("saved to {}", t.transfer.path.display()), TransferState::Done),
        State::Done => ("delivered".to_string(), TransferState::Done),
        State::Declined => ("declined".to_string(), TransferState::Ended),
        State::Cancelled | State::Failed => (if t.transfer.note.is_empty() { "cancelled".to_string() } else { t.transfer.note.clone() }, TransferState::Ended),
    };
    TransferItem {
        arrow: s(if incoming { "↓" } else { "↑" }),
        who: s(format!("{} {}", if incoming { "from" } else { "to" }, t.peer_name.clone().unwrap_or_else(|| "?".to_string()))),
        name: s(t.transfer.name.as_str()),
        size: s(human_size(t.transfer.size)),
        state: s(state),
        progress: done as f32 / size as f32,
        kind,
        can_accept: incoming && t.transfer.state == State::Offered,
        can_cancel: !t.transfer.state.is_finished(),
        can_clear: t.transfer.state.is_finished(),
    }
}

/// The lists as last given to the window, so unchanged ones aren't
/// replaced (which would reset their scrolling).
#[derive(Default)]
struct Shown {
    rows: Option<Rc<VecModel<Row>>>,
    lines: Option<Rc<VecModel<ChatLine>>>,
    chat_names: Vec<String>,
    /// The last Tab's completion of a name.
    completing: Option<chat::Completing>,
    members: Option<Rc<VecModel<Member>>>,
    call_members: Option<Rc<VecModel<CallMember>>>,
    addable: Option<Rc<VecModel<SharedString>>>,
    picks: Option<Rc<VecModel<SharedString>>>,
    hints: Option<Rc<VecModel<Hint>>>,
    public_posts: Option<Rc<VecModel<PostItem>>>,
    private_posts: Option<Rc<VecModel<PostItem>>>,
    my_public: Option<Rc<VecModel<PostItem>>>,
    my_private: Option<Rc<VecModel<PostItem>>>,
    transfers: Option<Rc<VecModel<TransferItem>>>,
    global_addresses: Option<Rc<VecModel<SharedString>>>,
    local_addresses: Option<Rc<VecModel<SharedString>>>,
    seen_at: Option<Rc<VecModel<Detail>>>,
    pd_places: Option<Rc<VecModel<Detail>>>,
    blocked: Option<Rc<VecModel<Detail>>>,
    nearby: Option<Rc<VecModel<crate::NearbyRow>>>,
    row_pictures: Option<Rc<VecModel<Pictured>>>,
    call_details: Option<Rc<VecModel<Detail>>>,
}

thread_local! {
    static SHOWN: RefCell<Shown> = RefCell::new(Shown::default());
    /// Pictures made for the window, by the pixels they're of (held, so
    /// those are never another's): each made once, and so the same image
    /// each time — a list holding it is seen as unchanged.
    static AVATARS: RefCell<Vec<(Arc<Vec<u8>>, Image)>> = const { RefCell::new(Vec::new()) };
    /// The QR codes shown (yours, and a peer's), and the lines they're of.
    static QR: RefCell<Vec<(String, Image)>> = const { RefCell::new(Vec::new()) };
}

/// A picture (`SHOWN_SIDE` square, RGB) as a circle: the corners clear,
/// its edge softened over a pixel. Round whatever draws it — the window's
/// software renderer doesn't clip a picture to rounded corners.
fn round(rgb: &[u8]) -> SharedPixelBuffer<slint::Rgba8Pixel> {
    let side = swarm_app::avatar::SHOWN_SIDE;
    let mut out = SharedPixelBuffer::<slint::Rgba8Pixel>::new(side as u32, side as u32);
    let radius = side as f32 / 2.0;
    for (i, px) in out.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % side) as f32 + 0.5 - radius, (i / side) as f32 + 0.5 - radius);
        let alpha = ((radius - (x * x + y * y).sqrt()) + 0.5).clamp(0.0, 1.0);
        let a = (alpha * 255.0).round() as u8;
        *px = slint::Rgba8Pixel { r: rgb[i * 3], g: rgb[i * 3 + 1], b: rgb[i * 3 + 2], a };
    }
    out
}

/// A QR code of `text` for the window: a pixel to each of its squares
/// (drawn larger, unsmoothed), dark on light, in the margin a reader
/// needs; made again only when the text changes. The two last asked for
/// are kept — yours and the peer shown — each moved to the back as it's
/// asked for, so going from peer to peer keeps yours.
pub(crate) fn qr_image(text: &str) -> Image {
    QR.with(|made| {
        let mut made = made.borrow_mut();
        let image = match made.iter().position(|(line, _)| line == text) {
            Some(at) => made.remove(at).1,
            None => {
                let code = swarm_app::contact::qr(text);
                let margin = 4;
                let side = code.len() + 2 * margin;
                let mut buffer = SharedPixelBuffer::<slint::Rgb8Pixel>::new(side as u32, side as u32);
                for (i, px) in buffer.make_mut_slice().iter_mut().enumerate() {
                    let (x, y) = (i % side, i / side);
                    let dark = x >= margin && y >= margin && code.get(y - margin).and_then(|row| row.get(x - margin)).copied().unwrap_or(false);
                    let v = if dark { 0 } else { 255 };
                    *px = slint::Rgb8Pixel { r: v, g: v, b: v };
                }
                Image::from_rgb8(buffer)
            }
        };
        if made.len() >= 2 {
            made.remove(0);
        }
        made.push((text.to_string(), image.clone()));
        image
    })
}

/// `avatar` as an image for the window (none: an empty one).
fn avatar_image(avatar: &Avatar) -> Image {
    let Some(pixels) = avatar else { return Image::default() };
    AVATARS.with(|made| {
        let mut made = made.borrow_mut();
        if let Some((_, image)) = made.iter().find(|(p, _)| Arc::ptr_eq(p, pixels)) {
            return image.clone();
        }
        made.retain(|(p, _)| Arc::strong_count(p) > 1);
        let image = Image::from_rgba8(round(pixels));
        made.push((pixels.clone(), image.clone()));
        image
    })
}

/// Puts `items` into the model `slot` keeps (making it the first time),
/// only if they differ from what's there; returns the model if it's new.
fn update<T: Clone + PartialEq + 'static>(slot: &mut Option<Rc<VecModel<T>>>, items: Vec<T>) -> Option<ModelRc<T>> {
    use slint::Model;
    match slot {
        Some(model) => {
            let same = model.row_count() == items.len() && items.iter().enumerate().all(|(i, item)| model.row_data(i).as_ref() == Some(item));
            if !same {
                model.set_vec(items);
            }
            None
        }
        None => {
            let model = Rc::new(VecModel::from(items));
            *slot = Some(model.clone());
            Some(ModelRc::from(model))
        }
    }
}

/// Tab in the typing box: `text` with the name at its end completed to
/// one in the chat shown, "" if none fits.
pub fn complete_name(text: &str) -> SharedString {
    SHOWN.with(|shown| {
        let shown = &mut *shown.borrow_mut();
        chat::complete(text, &shown.chat_names, &mut shown.completing).unwrap_or_default().into()
    })
}

/// Shows `snap` in the window.
pub fn apply(app: &App, window: &slint::Window, snap: Snapshot) {
    SHOWN.with(|shown| {
        let mut shown = shown.borrow_mut();
        if let Some(m) = update(&mut shown.rows, snap.rows) {
            app.set_rows(m);
        }
        if let Some(m) = update(&mut shown.lines, snap.lines) {
            app.set_lines(m);
        }
        shown.chat_names = snap.chat_names;
        if let Some(m) = update(&mut shown.members, snap.members) {
            app.set_members(m);
        }
        if let Some(m) = update(&mut shown.call_members, snap.call_members) {
            app.set_call_members(m);
        }
        if let Some(m) = update(&mut shown.addable, snap.addable) {
            app.set_addable(m);
        }
        if let Some(m) = update(&mut shown.picks, snap.picks) {
            app.set_picks(m);
        }
        if let Some(m) = update(&mut shown.public_posts, snap.public_posts) {
            app.set_public_posts(m);
        }
        if let Some(m) = update(&mut shown.private_posts, snap.private_posts) {
            app.set_private_posts(m);
        }
        if let Some(m) = update(&mut shown.my_public, snap.my_public) {
            app.set_my_public(m);
        }
        if let Some(m) = update(&mut shown.my_private, snap.my_private) {
            app.set_my_private(m);
        }
        if let Some(m) = update(&mut shown.transfers, snap.transfers) {
            app.set_transfers(m);
        }
        if let Some(m) = update(&mut shown.global_addresses, snap.global_addresses.into_iter().map(SharedString::from).collect()) {
            app.set_global_addresses(m);
        }
        if let Some(m) = update(&mut shown.local_addresses, snap.local_addresses.into_iter().map(SharedString::from).collect()) {
            app.set_local_addresses(m);
        }
        if let Some(m) = update(&mut shown.seen_at, snap.seen_at) {
            app.set_seen_at(m);
        }
        let pictures = snap.row_avatars.iter().map(|avatar| Pictured { shown: avatar.is_some(), picture: avatar_image(avatar) }).collect();
        if let Some(m) = update(&mut shown.row_pictures, pictures) {
            app.set_row_pictures(m);
        }
        if let Some(m) = update(&mut shown.pd_places, snap.pd_places) {
            app.set_pd_places(m);
        }
        if let Some(m) = update(&mut shown.blocked, snap.blocked) {
            app.set_blocked(m);
        }
        if let Some(m) = update(&mut shown.nearby, snap.nearby) {
            app.set_nearby(m);
        }
        app.set_nearby_looked(snap.nearby_looked);
        if let Some(m) = update(&mut shown.call_details, snap.call_details) {
            app.set_call_details(m);
        }
    });
    app.set_me(snap.me.clone());
    app.set_my_avatar(avatar_image(&snap.my_avatar));
    app.set_top(snap.top.clone());
    app.set_my_qr(qr_image(&snap.me.contact));
    app.set_call(snap.call.clone());
    app.set_fatal(snap.fatal.into());
    app.set_wiped(snap.wiped);
    if app.get_device_code().as_str() != snap.device_code {
        app.set_device_qr(qr_image(&snap.device_code));
        app.set_device_code(snap.device_code.into());
    }
    app.set_recovery_line(snap.recovery_words.join(" ").into());
    app.set_recovery_words(std::rc::Rc::new(slint::VecModel::from(snap.recovery_words.into_iter().map(Into::into).collect::<Vec<slint::SharedString>>())).into());
    app.set_first_run_error(snap.first_run_error.into());
    app.set_first_run_busy(snap.first_run_busy);
    app.set_first_run_scanning(snap.first_run_scanning);
    if let Some(code) = snap.first_run_code {
        app.set_first_run_code(code.into());
    }
    app.set_suggested(snap.suggested.into());
    app.set_view(snap.view);
    app.set_list_narrow(snap.list_narrow);
    app.set_list_width(snap.list_width as f32);
    app.set_members_hidden(snap.members_hidden);
    app.set_members_width(snap.members_width as f32);
    app.set_theme(snap.theme);
    app.set_settings(snap.settings.clone());
    scale(window, snap.text_size);
    app.set_peer(snap.peer.clone());
    app.set_peer_avatar(avatar_image(&snap.peer_avatar));
    app.set_chat(snap.chat.clone());
    app.set_channel(snap.channel.clone());
    app.set_req(snap.req.clone());
    app.set_pd(snap.pd.clone());
    app.set_dd(snap.dd.clone());
    app.set_pd_avatar(avatar_image(&snap.pd_avatar));
    if snap.pd.shown {
        app.set_pd_qr(qr_image(&snap.pd.contact));
    }
    app.set_cd(snap.cd.clone());
    app.set_detected_address(snap.detected_address.into());
    app.set_transfers_with(snap.transfers_with);
    app.set_transfers_finished(snap.transfers_finished);
    {
        use slint::Model;
        let shown = app.get_cameras();
        if shown.row_count() != snap.cameras.len() || shown.iter().zip(&snap.cameras).any(|(a, b)| a.as_str() != b) {
            app.set_cameras(std::rc::Rc::new(VecModel::from(snap.cameras.iter().map(|c| SharedString::from(c.as_str())).collect::<Vec<_>>())).into());
        }
        let shown = app.get_microphones();
        if shown.row_count() != snap.microphones.len() || shown.iter().zip(&snap.microphones).any(|(a, b)| a.as_str() != b) {
            app.set_microphones(std::rc::Rc::new(VecModel::from(snap.microphones.iter().map(|c| SharedString::from(c.as_str())).collect::<Vec<_>>())).into());
        }
        let shown = app.get_speakers();
        if shown.row_count() != snap.speakers.len() || shown.iter().zip(&snap.speakers).any(|(a, b)| a.as_str() != b) {
            app.set_speakers(std::rc::Rc::new(VecModel::from(snap.speakers.iter().map(|c| SharedString::from(c.as_str())).collect::<Vec<_>>())).into());
        }
        let shown = app.get_call_cameras();
        if shown.row_count() != snap.call_cameras.len() || shown.iter().zip(&snap.call_cameras).any(|(a, b)| a.as_str() != b) {
            app.set_call_cameras(std::rc::Rc::new(VecModel::from(snap.call_cameras.iter().map(|c| SharedString::from(c.as_str())).collect::<Vec<_>>())).into());
        }
    }
    app.set_ask(snap.ask.clone());
    let control = &snap.control;
    crate::control_bar::show(control.controller.as_ref().map(|(name, paused)| if *paused { format!("{name} is paused while you use your mouse") } else { format!("{name} is controlling your screen") }));
    app.set_remote_avatar(avatar_image(&snap.remote_avatar));
    app.set_preview_shown(snap.preview_shown);
    app.set_preview_info(snap.preview_info.into());
    show_hints(app);
}

/// The key hints for what the window shows now (see `keys`).
pub fn show_hints(app: &App) {
    let hints = crate::keys::keys(&crate::keys::Showing::read(app)).hints();
    SHOWN.with(|shown| {
        if let Some(m) = update(&mut shown.borrow_mut().hints, hints) {
            app.set_hints(m);
        }
    });
}

thread_local! {
    /// The system's own scale for the window, and the scale last given it
    /// here (its own times the text size).
    static SCALE: std::cell::Cell<Option<(f32, f32)>> = const { std::cell::Cell::new(None) };
}

/// Makes everything in the window `percent` of its normal size — text,
/// buttons, spacing — on top of whatever scale the system gives it. The
/// system's is whatever the window has that wasn't given it here: it
/// changes, and is taken again. On Android it's set only once the window's
/// made (the screen's density), after the first look, and
/// again whenever the configuration changes, undoing the text size; taken
/// once and kept, 1 then, every size chosen was far too small.
fn scale(window: &slint::Window, percent: u32) {
    let percent = if percent == 0 { 100 } else { percent };
    let current = window.scale_factor();
    let base = match SCALE.with(|s| s.get()) {
        Some((base, given)) if (current - given).abs() <= f32::EPSILON => base,
        _ => if current.is_normal() && current > 0.0 { current } else { 1.0 },
    };
    let scale_factor = base * percent as f32 / 100.0;
    SCALE.with(|s| s.set(Some((base, scale_factor))));
    if (scale_factor - current).abs() > f32::EPSILON {
        window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor });
        let size = window.size();
        window.dispatch_event(slint::platform::WindowEvent::Resized {
            size: slint::LogicalSize::new(size.width as f32 / scale_factor, size.height as f32 / scale_factor),
        });
    }
}

/// The window's pixels: RGBA on Android, whose renderer (Skia) takes it
/// as it is, where RGB it turns into RGBA a byte at a time — on a phone,
/// a fifth of all Swarm's work in a video call; RGB elsewhere, which the
/// software renderer draws as it is, a quarter less to write.
#[cfg(target_os = "android")]
type Pixel = slint::Rgba8Pixel;
#[cfg(not(target_os = "android"))]
type Pixel = Rgb8Pixel;

/// A kind of pixel the window's pictures are written in.
trait WindowPixel: Clone + Default + Sized {
    /// Every `step`th pixel of `picture` into `buffer`, mirrored if
    /// `mirror`.
    fn write(picture: &Picture, buffer: &mut SharedPixelBuffer<Self>, mirror: bool, step: usize);
    fn image(buffer: SharedPixelBuffer<Self>) -> Image;
}

impl WindowPixel for Rgb8Pixel {
    fn write(picture: &Picture, buffer: &mut SharedPixelBuffer<Self>, mirror: bool, step: usize) {
        picture.rgb_every_into(buffer.make_mut_bytes(), mirror, step);
    }
    fn image(buffer: SharedPixelBuffer<Self>) -> Image {
        Image::from_rgb8(buffer)
    }
}

impl WindowPixel for slint::Rgba8Pixel {
    fn write(picture: &Picture, buffer: &mut SharedPixelBuffer<Self>, mirror: bool, step: usize) {
        picture.rgba_every_into(buffer.make_mut_bytes(), mirror, step);
    }
    fn image(buffer: SharedPixelBuffer<Self>) -> Image {
        Image::from_rgba8(buffer)
    }
}

/// Pictures for the window: the other side's, ours (mirrored, like a
/// mirror, if the camera faces us), and the camera preview's — each only
/// when it's a new one, so the window isn't redrawn for a picture it
/// already shows.
pub struct Frames {
    remote: Option<SharedPixelBuffer<Pixel>>,
    local: Option<SharedPixelBuffer<Pixel>>,
    preview: Option<SharedPixelBuffer<Pixel>>,
    /// The strip, whole: who's in it, and their pictures.
    strip: Vec<Spot>,
}

/// One in the strip (see `StripTile` in parts.slint).
struct Spot {
    name: String,
    note: String,
    /// Us: our picture's the window's own (`local-frame`).
    own: bool,
    picture: Option<SharedPixelBuffer<Pixel>>,
    /// Theirs (ours), shown while there's no picture.
    avatar: Avatar,
}

/// The pictures last handed to the window, to tell a new one from them,
/// and the buffers they went in.
#[derive(Default)]
pub struct PicturesShown {
    remote: Option<Arc<Picture>>,
    local: Option<Arc<Picture>>,
    preview: Option<Arc<Picture>>,
    buffers: [Buffers<Pixel>; 3],
    /// Those in the strip (see `CallVideo::strip`): each's picture drawn
    /// last, and its buffers.
    strip: Vec<SpotShown>,
}

/// One in the strip, as last drawn.
struct SpotShown {
    key: [u8; 32],
    picture: Option<Arc<Picture>>,
    buffers: Buffers<Pixel>,
    drawn: Option<SharedPixelBuffer<Pixel>>,
}

/// The window's size in its own pixels, packed, as last handed pictures
/// (there's one window): what they're cut down to (see `step_into`), and
/// what the others are told to send. Unknown, 0, until then.
static WINDOW_SIZE: AtomicU64 = AtomicU64::new(0);

fn window_size() -> (usize, usize) {
    let size = WINDOW_SIZE.load(Ordering::Relaxed);
    ((size >> 32) as usize, (size & 0xffff_ffff) as usize)
}

impl PicturesShown {
    /// `key`'s spot in the strip, as last drawn.
    fn spot(&mut self, key: [u8; 32]) -> &mut SpotShown {
        if let Some(i) = self.strip.iter().position(|s| s.key == key) {
            return &mut self.strip[i];
        }
        self.strip.push(SpotShown { key, picture: None, buffers: Buffers::default(), drawn: None });
        self.strip.last_mut().unwrap()
    }
}

/// The last two buffers a source's pictures went to the window in. The
/// one before last has been let go of by the window (it shows the last),
/// so it takes the next picture, written over where it is: a new 4K
/// buffer each time took 9 ms. Should the window still hold it, writing
/// copies it first (see `SharedPixelBuffer::make_mut_bytes`), and what's
/// shown is never written over.
struct Buffers<P> {
    last: Option<SharedPixelBuffer<P>>,
    before: Option<SharedPixelBuffer<P>>,
}

impl<P> Default for Buffers<P> {
    fn default() -> Self {
        Buffers { last: None, before: None }
    }
}

impl<P: WindowPixel> Buffers<P> {
    /// A buffer for the next picture: held nowhere else, if the window's
    /// let go of it, so it's written where it is.
    fn take(&mut self, width: usize, height: usize) -> SharedPixelBuffer<P> {
        let buffer = match self.before.take() {
            Some(buffer) if (buffer.width(), buffer.height()) == (width as u32, height as u32) => buffer,
            _ => SharedPixelBuffer::new(width as u32, height as u32),
        };
        self.before = self.last.take();
        buffer
    }

    /// Keeps `buffer`, written and on its way to the window, for the
    /// picture after next.
    fn keep(&mut self, buffer: &SharedPixelBuffer<P>) {
        self.last = Some(buffer.clone());
    }
}

/// `picture` for the window, mirrored if `mirror`, turned into RGB
/// straight into a buffer for it. Never shrunk first: the window's
/// drawing costs the same whatever size it's given (a 4K picture drawn
/// into its box as one shrunk to it), and shrinking cost more than
/// turning the whole picture into RGB. The whole of a 4K camera picture
/// made RGB, then mirrored, then copied, took 52 ms on a fast desktop,
/// more than the 40 between pictures, and the preview stuttered; now
/// about 4. Only every `step`th pixel each way, given one.
fn pixels<P: WindowPixel>(picture: &Picture, mirror: bool, step: usize, buffers: &mut Buffers<P>) -> SharedPixelBuffer<P> {
    let (width, height) = picture.every(step);
    let mut buffer = buffers.take(width, height);
    P::write(picture, &mut buffer, mirror, step);
    buffers.keep(&buffer);
    buffer
}

/// On the phone, our own picture's box, which it's given no smaller
/// than: a third of the screen's width at most, and 16:9 — 640 on a
/// screen 1080 wide, upright or on its side. Its camera's 1080p, made
/// RGBA whole, the window (Skia) copied and freed each time: three
/// quarters of the window's work in a video call.
const PHONE_OWN_BOX: (usize, usize) = (640, 640);

/// In the window, our own picture's box, in the window's own pixels (it
/// being `window`): a quarter of its width at most, never taller than a
/// third of its height, 16:9 (see app.slint; the video-only view's is
/// smaller still). Unknown, (0, 0), while the window's size is.
fn own_box((width, height): (usize, usize)) -> (usize, usize) {
    if cfg!(target_os = "android") {
        return PHONE_OWN_BOX;
    }
    let width = (width / 4).min(height / 3 * 16 / 9);
    (width, width * 9 / 16)
}

/// Their picture's box: the window at most.
fn window_box(window: (usize, usize)) -> (usize, usize) {
    window
}

/// The size of each of the strip's `spots` tiles (see `CallStrip` in
/// parts.slint), in a window `window` big, in its own pixels: 16:9, down
/// the left of a window wider than tall, a fifth of its width at most;
/// along the top of one taller than wide (a phone held upright, an
/// upright monitor), a sixth of its height at most — all of them fitting
/// along it either way. Unknown, (0, 0), while the window's size is.
fn strip_tile((width, height): (usize, usize), spots: usize) -> (usize, usize) {
    let spots = spots.max(1);
    let tile = if width >= height { (width / 5).min(height / spots * 16 / 9) } else { (height / 6 * 16 / 9).min(width / spots) };
    (tile, tile * 9 / 16)
}

/// Which pixels of `picture` the window's given, to draw in `into` (its
/// box, in the window's own pixels): every this many each way, as many
/// as leave it no smaller there than drawn (`image-fit: contain`).
/// Every one, while the box isn't known.
fn step_into(picture: &Picture, into: (usize, usize)) -> usize {
    match into {
        (0, _) | (_, 0) => 1,
        (w, h) => (picture.width / w).max(picture.height / h).max(1),
    }
}

/// The first run's camera's picture, reading another device's code
/// (there's no call or preview yet to keep buffers for).
pub fn first_run_preview(app: &App, window: &slint::Window, picture: &Picture, mirror: bool) {
    let size = window.size();
    let into = (size.width as usize, size.height as usize);
    app.set_preview_frame(Pixel::image(pixels(picture, mirror, step_into(picture, into), &mut Buffers::default())));
}

/// `now` if it isn't the picture `shown` already is (and remembers it).
fn new_picture(shown: &mut Option<Arc<Picture>>, now: Option<Arc<Picture>>) -> Option<Arc<Picture>> {
    if now.as_ref().map(Arc::as_ptr) == shown.as_ref().map(Arc::as_ptr) {
        return None;
    }
    shown.clone_from(&now);
    now
}

/// A picture's source, and the picture of it shown now.
pub type PictureSource = (watch::Receiver<Option<Arc<Picture>>>, Option<Arc<Picture>>);

/// Where the window's pictures come from, each with the picture of it
/// shown now: the preview's camera, and in a call theirs and our camera
/// or screen.
pub fn picture_sources(state: &State, shown: &PicturesShown) -> Vec<PictureSource> {
    let mut sources = Vec::new();
    if let Some(Ok(camera)) = state.calls.preview() {
        sources.push((camera.subscribe(), shown.preview.clone()));
    }
    if let Some(video) = state.calls.active().and_then(|c| c.video.as_ref()) {
        let shown_key = state.calls.active().and_then(|c| c.shown_remote()).map(|(r, _)| r.key);
        sources.extend(remote_sources(&video.remotes, shown_key, shown));
        match (&video.screen, &video.camera) {
            (Some(screen), _) => sources.push((screen.subscribe(), shown.local.clone())),
            (None, Some(camera)) => sources.push((camera.subscribe(), shown.local.clone())),
            (None, None) => {}
        }
    }
    sources
}

/// The call's pictures to wake for: everyone's, each judged by what's
/// drawn of them — the one shown large (`shown_key`), and the rest, in
/// the strip, from their large pictures or their small (see
/// `Remote::large_view`, `tile_view`). (Each is drawn, so its picture's
/// taken as shown: one never drawn would always count as new, and the
/// window be woken over and over, for nothing.) And each's other
/// pictures while there are none, to wake when they come: a speaker's
/// large, to be shown large from then.
fn remote_sources(remotes: &[Remote], shown_key: Option<[u8; 32]>, shown: &PicturesShown) -> Vec<PictureSource> {
    let mut sources = Vec::new();
    for remote in remotes {
        let large = Some(remote.key) == shown_key;
        let (view, drawn) = if large {
            (remote.large_view(), shown.remote.clone())
        } else {
            (remote.tile_view(), shown.strip.iter().find(|s| s.key == remote.key).and_then(|s| s.picture.clone()))
        };
        let other = if std::ptr::eq(view, &remote.picture) { &remote.small } else { &remote.picture };
        sources.push((view.clone(), drawn));
        if other.borrow().is_none() {
            sources.push((other.clone(), None));
        }
    }
    sources
}

/// Waits until one of `sources` has a picture that isn't the one shown
/// of it — so each is shown as it comes, rather than whichever's newest
/// on a timer's tick, which (a tick every 40 ms against a camera's 33)
/// misses one picture in six, and on Windows, whose timers keep to
/// 15.6 ms, misses them unevenly: the picture stutters.
pub async fn picture_to_show(mut sources: Vec<PictureSource>) {
    for (source, _) in &mut sources {
        source.mark_unchanged();
    }
    let unshown = |(source, shown): &PictureSource| {
        source.borrow().as_ref().is_some_and(|now| shown.as_ref().is_none_or(|shown| !Arc::ptr_eq(now, shown)))
    };
    if sources.iter().any(unshown) {
        return;
    }
    let mut waits: Vec<_> = sources.into_iter().map(|(mut source, _)| Some(Box::pin(async move { source.changed().await }))).collect();
    std::future::poll_fn(|cx| {
        for wait in &mut waits {
            if let Some(changed) = wait {
                match changed.as_mut().poll(cx) {
                    std::task::Poll::Ready(Ok(())) => return std::task::Poll::Ready(()),
                    std::task::Poll::Ready(Err(_)) => *wait = None,
                    std::task::Poll::Pending => {}
                }
            }
        }
        std::task::Poll::Pending
    })
    .await
}

pub fn frames(state: &State, shown: &mut PicturesShown) -> Frames {
    let call = state.calls.active();
    let video = call.and_then(|c| c.video.as_ref());
    let large = call.and_then(|c| c.shown_remote());
    let large_key = large.as_ref().map(|(r, _)| r.key);
    let strip = video.map_or(Vec::new(), |v| v.strip(large_key));
    let window = window_size();
    let tile = strip_tile(window, strip.len());
    if let Some(video) = video {
        video.show_in(window_box(window), if strip.is_empty() { (0, 0) } else { tile });
    }
    let remote = new_picture(&mut shown.remote, large.and_then(|(r, _)| r.large_view().borrow().clone()));
    let local = video.and_then(|v| v.local_picture());
    let mirror = local.as_ref().is_none_or(|(_, mirror)| *mirror);
    let local = new_picture(&mut shown.local, local.map(|(picture, _)| picture));
    let preview_camera = state.calls.preview().and_then(|p| p.as_ref().ok());
    let preview_mirror = preview_camera.is_none_or(|c| !c.info().faces_away);
    let preview = new_picture(&mut shown.preview, preview_camera.and_then(|c| c.latest()));
    let own_box = if strip.is_empty() { own_box(window) } else { tile };
    shown.strip.retain(|spot| strip.contains(&Some(spot.key)));
    let strip = strip
        .iter()
        .filter_map(|spot| {
            let (Some(call), Some(video)) = (call, video) else { return None };
            let Some(key) = spot else {
                let note = if video.local_picture().is_none() { "camera off" } else { "" };
                return Some(Spot { name: "you".into(), note: note.into(), own: true, picture: None, avatar: swarm_app::avatar::pixels(&state.node.identity().public_key_bytes(), true) });
            };
            let remote = video.remote(key)?;
            let name = call.name_of(key);
            let drawn = shown.spot(*key);
            let now = remote.tile_view().borrow().clone();
            if now.is_none() {
                drawn.drawn = None;
            }
            if let Some(p) = new_picture(&mut drawn.picture, now) {
                drawn.drawn = Some(pixels(&p, false, step_into(&p, tile), &mut drawn.buffers));
            }
            Some(Spot { note: remote.note(&name).unwrap_or_default(), name, own: false, picture: drawn.drawn.clone(), avatar: swarm_app::avatar::pixels(key, false) })
        })
        .collect();
    let [remote_buffers, local_buffers, preview_buffers] = &mut shown.buffers;
    Frames {
        remote: remote.map(|p| pixels(&p, false, step_into(&p, window_box(window)), remote_buffers)),
        local: local.map(|p| pixels(&p, mirror, step_into(&p, own_box), local_buffers)),
        preview: preview.map(|p| pixels(&p, preview_mirror, step_into(&p, window_box(window)), preview_buffers)),
        strip,
    }
}

/// The meters' levels, each 0 to 1 (see swarm_app's meter.rs): the
/// strip's, in its order; the call's list's (see `Active::heard`); ours; and
/// Settings' microphone's, or why there's none.
pub struct Levels {
    strip: Vec<f32>,
    members: Vec<f32>,
    own: f32,
    microphone: f32,
    microphone_note: String,
}

/// Whether any meter's to be seen: in a call, or in Settings.
pub fn levels_shown(state: &State) -> bool {
    state.locked.is_none() && (state.calls.active().is_some() || state.view == View::Settings)
}

pub fn levels(state: &State) -> Levels {
    let (microphone, microphone_note) = match state.calls.microphone_level() {
        Some(Ok(level)) => (level, String::new()),
        Some(Err(why)) => (0.0, format!("can't be heard: {why}")),
        None => (0.0, String::new()),
    };
    let Some(call) = state.calls.active() else { return Levels { strip: Vec::new(), members: Vec::new(), own: 0.0, microphone, microphone_note } };
    let level = |key: Option<[u8; 32]>| key.map_or_else(|| call.own_level(), |key| call.level(&key));
    let strip = call.video.as_ref().map_or(Vec::new(), |video| video.strip(call.shown_remote().map(|(r, _)| r.key)).into_iter().map(level).collect());
    let members = call.heard().into_iter().map(|(_, _, key, _)| level(key)).collect();
    Levels { strip, members, own: call.own_level(), microphone, microphone_note }
}

thread_local! {
    /// The meters' levels as given to the window: the strip's, the list's.
    static LEVELS: RefCell<[Option<Rc<VecModel<f32>>>; 2]> = const { RefCell::new([None, None]) };
}

pub fn apply_levels(app: &App, levels: Levels) {
    app.set_own_level(levels.own);
    app.set_mic_level(levels.microphone);
    app.set_mic_note(levels.microphone_note.into());
    LEVELS.with(|models| {
        use slint::Model;
        let mut models = models.borrow_mut();
        for (i, values) in [levels.strip, levels.members].into_iter().enumerate() {
            match &models[i] {
                Some(model) if model.row_count() == values.len() => {
                    for (row, value) in values.into_iter().enumerate() {
                        model.set_row_data(row, value);
                    }
                }
                Some(model) => model.set_vec(values),
                None => {
                    let model = Rc::new(VecModel::from(values));
                    let rows = ModelRc::from(model.clone());
                    if i == 0 { app.set_strip_levels(rows) } else { app.set_member_levels(rows) }
                    models[i] = Some(model);
                }
            }
        }
    });
}

thread_local! {
    /// The strip as given to the window.
    static STRIP: RefCell<Option<Rc<VecModel<StripTile>>>> = const { RefCell::new(None) };
}

pub fn apply_frames(app: &App, window: &slint::Window, frames: Frames) {
    crate::note_in_use(window);
    let size = window.size();
    WINDOW_SIZE.store(u64::from(size.width) << 32 | u64::from(size.height), Ordering::Relaxed);
    if let Some(buffer) = frames.remote {
        app.set_remote_frame(Pixel::image(buffer));
    }
    if let Some(buffer) = frames.local {
        app.set_local_frame(Pixel::image(buffer));
    }
    if let Some(buffer) = frames.preview {
        app.set_preview_frame(Pixel::image(buffer));
    }
    let tiles: Vec<StripTile> = frames
        .strip
        .into_iter()
        .map(|spot| StripTile {
            name: spot.name.into(),
            note: spot.note.into(),
            own: spot.own,
            picture: spot.picture.map(Pixel::image).unwrap_or_default(),
            pictured: spot.avatar.is_some(),
            avatar: avatar_image(&spot.avatar),
        })
        .collect();
    STRIP.with(|strip| {
        use slint::Model;
        let mut strip = strip.borrow_mut();
        match strip.as_ref() {
            Some(model) if model.row_count() == tiles.len() => {
                for (i, tile) in tiles.into_iter().enumerate() {
                    model.set_row_data(i, tile);
                }
            }
            Some(model) => model.set_vec(tiles),
            None => {
                let model = Rc::new(VecModel::from(tiles));
                app.set_strip(ModelRc::from(model.clone()));
                *strip = Some(model);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of one person's messages: the same name and colour, each
    /// under five minutes after the last, nothing else between and no new
    /// day; only its first is `first`.
    #[test]
    fn a_run_of_one_persons_messages_is_named_once() {
        use swarm_app::colours::NameColour;
        let said = |at: u64, name: &str, colour: NameColour| ChatItem { at, what: ChatWhat::Said { name: name.into(), colour, text: "x".into() }, sent: Sent::NotOurs, for_me: false, lines: Vec::new() };
        let noon = 1_763_035_200;
        let items = [
            said(noon, "Dave", NameColour::Other),
            said(noon + 60, "Dave", NameColour::Other),
            said(noon + 60 + 299, "Dave", NameColour::Other),
            said(noon + 60 + 299 + 300, "Dave", NameColour::Other),
            said(noon + 700, "Dave", NameColour::Me),
            ChatItem { at: noon + 710, what: ChatWhat::Happened { mark: "→", text: "Ann joined".into() }, sent: Sent::NotOurs, for_me: false, lines: Vec::new() },
            said(noon + 720, "Dave", NameColour::Me),
            said(noon + 86_400, "Dave", NameColour::Me),
        ];
        let (lines, _) = chat_lines(&items, true);
        let firsts: Vec<bool> = lines.iter().filter(|l| l.kind == LineKind::Said).map(|l| l.first).collect();
        assert_eq!(firsts, [true, false, false, true, true, true, true]);
    }

    /// The text size goes on top of the system's own scale, whenever that
    /// comes or changes: on Android, the screen's density is given only
    /// after the window's first look, and given again (the size undone)
    /// when the configuration changes.
    #[test]
    fn the_text_size_is_on_top_of_the_systems_scale_whenever_it_comes() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::WindowEvent;
        use slint::ComponentHandle;
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(crate::Offscreen { window: window.clone() })).unwrap();
        let ui = crate::PhoneWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(1080, 2400));
        let w = ui.window();
        let close = |want: f32| assert!((w.scale_factor() - want).abs() < 0.001, "{} not {want}", w.scale_factor());
        scale(w, 100);
        close(1.0);
        w.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 2.625 });
        scale(w, 100);
        close(2.625);
        scale(w, 175);
        close(2.625 * 1.75);
        w.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 2.625 });
        scale(w, 175);
        close(2.625 * 1.75);
        scale(w, 100);
        close(2.625);
        scale(w, 90);
        close(2.625 * 0.9);
    }

    /// A picker's matches: anywhere in the name, any case, those starting
    /// with it first, five at most.
    #[test]
    fn picks_put_names_starting_with_it_first() {
        let names = ["Joanne", "ann", "Bob", "Annie", "Dan", "Hannah", "Nan", "Fran"].map(String::from);
        assert_eq!(picks(names.clone().into_iter(), "An"), ["ann", "Annie", "Dan", "Fran", "Hannah"]);
        assert_eq!(picks(names.clone().into_iter(), "bo"), ["Bob"]);
        assert_eq!(picks(names.clone().into_iter(), "").len(), 5);
        assert!(picks(names.into_iter(), "zed").is_empty());
    }

    /// A picture's shown as a circle: its corners clear, its middle as it
    /// was, its edge in between.
    #[test]
    fn a_picture_is_made_round() {
        let side = swarm_app::avatar::SHOWN_SIDE;
        let rgb: Vec<u8> = (0..side * side).flat_map(|i| [(i % 251) as u8, 7, 9]).collect();
        let round = round(&rgb);
        let at = |x: usize, y: usize| round.as_slice()[y * side + x];
        for (x, y) in [(0, 0), (side - 1, 0), (0, side - 1), (side - 1, side - 1), (10, 10)] {
            assert_eq!(at(x, y).a, 0, "a corner, ({x}, {y})");
        }
        let middle = at(side / 2, side / 2);
        assert_eq!((middle.a, middle.r, middle.g), (255, ((side / 2 * side + side / 2) % 251) as u8, 7));
        assert!(at(side / 2, 0).a > 250, "the top edge's middle is in");
        let e = (side as f64 / 2.0 * (1.0 - std::f64::consts::FRAC_1_SQRT_2)) as usize;
        let edge = at(e, e).a;
        assert!(at(e - 1, e - 1).a < edge && edge <= at(e + 1, e + 1).a, "softened across the edge");
    }

    /// On the phone our own picture goes to the window about the size
    /// of its box, no smaller: its camera's 1080p upright (or on its
    /// side) as 360 by 640, 720p as 640 by 360, anything smaller whole.
    #[test]
    fn own_picture_given_the_phone_at_its_box_size() {
        for ((w, h), shown) in [((1080, 1920), (360, 640)), ((1920, 1080), (640, 360)), ((1280, 720), (640, 360)), ((640, 360), (640, 360)), ((320, 180), (320, 180))] {
            let p = grey(w, h);
            let buffer = pixels(&p, true, step_into(&p, PHONE_OWN_BOX), &mut Buffers::<slint::Rgba8Pixel>::default());
            assert_eq!((buffer.width() as usize, buffer.height() as usize), shown, "{w}×{h}");
        }
    }

    /// In the window, our own picture (in its corner, a quarter of the
    /// window's width at most) and theirs (the window at most) go to it
    /// about the size they're drawn, no smaller — not whole: a 4K camera
    /// made RGB whole thirty times a second for a box 480 wide kept the
    /// desktop window's thread flat out in a call. Before the window's
    /// size is known, whole.
    #[test]
    fn pictures_given_the_window_at_their_box_size() {
        let window = (1920, 1080);
        for ((w, h), own, theirs) in [
            ((3840, 2160), (480, 270), (1920, 1080)),
            ((2560, 1440), (512, 288), (2560, 1440)),
            ((1080, 1920), (155, 275), (1080, 1920)),
            ((1280, 720), (640, 360), (1280, 720)),
            ((320, 180), (320, 180), (320, 180)),
        ] {
            let p = grey(w, h);
            let mut buffers = Buffers::<Rgb8Pixel>::default();
            let size = |b: SharedPixelBuffer<Rgb8Pixel>| (b.width() as usize, b.height() as usize);
            assert_eq!(size(pixels(&p, true, step_into(&p, own_box(window)), &mut buffers)), own, "ours, {w}×{h}");
            assert_eq!(size(pixels(&p, false, step_into(&p, window_box(window)), &mut buffers)), theirs, "theirs, {w}×{h}");
            assert_eq!(step_into(&p, own_box((0, 0))), 1, "size unknown");
            assert_eq!(step_into(&p, window_box((0, 0))), 1, "size unknown");
        }
    }

    /// The strip's tiles, 16:9: down the left of a window wider than
    /// tall, a fifth of its width at most; along the top of one taller
    /// than wide (a phone held upright, an upright monitor), a sixth of
    /// its height at most — either way all fitting along it.
    #[test]
    fn the_strips_tiles_fit_along_it() {
        assert_eq!(strip_tile((1920, 1080), 2), (384, 216));
        assert_eq!(strip_tile((1920, 1080), 6), (320, 180));
        assert_eq!(strip_tile((1080, 2400), 1), (711, 399));
        assert_eq!(strip_tile((1080, 2400), 2), (540, 303));
        assert_eq!(strip_tile((0, 0), 2), (0, 0), "size unknown");
    }

    fn grey(w: usize, h: usize) -> Picture {
        Picture::i420(w, h, vec![128; w * h + 2 * w.div_ceil(2) * h.div_ceil(2)])
    }

    /// The cameras are there to choose from in Settings, and in a call
    /// (the phone's Camera offers them, with more than one): none were
    /// in a call once, and the phone's Switch never showed.
    #[test]
    fn cameras_are_listed_in_settings_and_calls() {
        assert!(cameras_listed(Pane::Settings, false), "Settings");
        assert!(cameras_listed(Pane::Mine, true), "a call, on any view");
        assert!(!cameras_listed(Pane::Mine, false), "neither: not looked for");
    }

    /// A camera's I420 picture comes out for the window as its RGB one
    /// would (to within YUV's own rounding): shrunk for the inset, and
    /// mirrored, left for right.
    #[test]
    fn an_i420_picture_is_drawn_as_its_rgb_one_is() {
        let (w, h) = (640, 360);
        let rgb: Vec<u8> = (0..w * h).flat_map(|i| [(i % w * 255 / w) as u8, (i / w * 255 / h) as u8, 90]).collect();
        let picture = Picture::rgb(w, h, rgb);
        let i420 = picture.to_i420().into_owned();
        let mut buffers = Buffers::<Rgb8Pixel>::default();
        for mirror in [false, true, false, true] {
            let (a, b) = (pixels(&picture, mirror, 1, &mut Buffers::<Rgb8Pixel>::default()), pixels(&i420, mirror, 1, &mut buffers));
            assert_eq!((a.width(), a.height()), (b.width(), b.height()));
            let most = a.as_bytes().iter().zip(b.as_bytes()).map(|(&x, &y)| x.abs_diff(y)).max().unwrap();
            assert!(most <= 12, "mirror {mirror}: differ by up to {most}");
        }
        let mirrored = pixels(&i420, true, 1, &mut Buffers::<Rgb8Pixel>::default());
        assert!(mirrored.as_bytes()[0] > 200 && mirrored.as_bytes()[(w - 1) * 3] < 50);
        let mut rgba = Buffers::<slint::Rgba8Pixel>::default();
        for mirror in [false, true, false, true] {
            let (a, b) = (pixels(&i420, mirror, 1, &mut Buffers::<Rgb8Pixel>::default()), pixels(&i420, mirror, 1, &mut rgba));
            let opaque: Vec<u8> = a.as_bytes().chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect();
            assert_eq!(b.as_bytes(), &opaque[..], "mirror {mirror}");
        }
    }

    /// Every picture of a camera giving 30 a second is shown, each as it
    /// comes — not whichever's newest on a tick, which missed one in six —
    /// and the window's woken only for them.
    #[tokio::test(flavor = "multi_thread")]
    async fn each_picture_is_shown_as_it_comes() {
        let (camera, source) = watch::channel(None);
        let given = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            for n in 1..=60u32 {
                camera.send_replace(Some(Arc::new(Picture::rgb(2, 2, vec![n as u8; 12]))));
                std::thread::sleep((start + std::time::Duration::from_secs(n.into()) / 30).saturating_duration_since(std::time::Instant::now()));
            }
        });
        let mut shown = PicturesShown::default();
        let (mut count, mut woken) = (0, 0);
        let watching = async {
            loop {
                picture_to_show(vec![(source.clone(), shown.preview.clone())]).await;
                woken += 1;
                if new_picture(&mut shown.preview, source.borrow().clone()).is_some() {
                    count += 1;
                }
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_millis(2200), watching).await;
        given.join().unwrap();
        assert!(count >= 59, "{count} of 60 pictures shown");
        assert!(woken <= 62, "woken {woken} times for 60 pictures");
    }

    /// In a call of three, the window's woken for the pictures of each
    /// shown — large, or small in the strip — judged against what's
    /// drawn of each: once for each new one, never over and over. And
    /// once when someone in the strip's large pictures come, to show them
    /// large from then, but not for each after.
    #[tokio::test(flavor = "multi_thread")]
    async fn each_shown_wakes_the_window_once_for_each_new_picture() {
        let picture = || Some(Arc::new(Picture::rgb(2, 2, vec![7; 12])));
        let (bob_sends, bob) = watch::channel(None);
        let (bob_small_sends, bob_small) = watch::channel(picture());
        let (carol_sends, carol) = watch::channel(picture());
        let (carol_small_sends, carol_small) = watch::channel(picture());
        let remote = |key: u8, picture, small| Remote { key: [key; 32], picture, small, stats: Default::default() };
        let remotes = [remote(1, bob, bob_small), remote(2, carol, carol_small)];
        let mut shown = PicturesShown::default();
        new_picture(&mut shown.remote, remotes[1].large_view().borrow().clone());
        new_picture(&mut shown.spot([1; 32]).picture, remotes[0].tile_view().borrow().clone());
        let woken = |shown: &PicturesShown| {
            let sources = remote_sources(&remotes, Some([2; 32]), shown);
            async move { tokio::time::timeout(std::time::Duration::from_millis(300), picture_to_show(sources)).await.is_ok() }
        };
        assert!(!woken(&shown).await, "nothing new to show");
        bob_small_sends.send_replace(picture());
        assert!(woken(&shown).await, "Bob's new picture");
        new_picture(&mut shown.spot([1; 32]).picture, remotes[0].tile_view().borrow().clone());
        assert!(!woken(&shown).await, "Bob's drawn");
        let waiting = tokio::spawn(woken(&shown));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bob_sends.send_replace(picture());
        assert!(waiting.await.unwrap(), "Bob's large pictures");
        bob_sends.send_replace(picture());
        assert!(!woken(&shown).await, "Bob's large, not drawn, again");
        carol_small_sends.send_replace(picture());
        assert!(!woken(&shown).await, "Carol's small, not drawn");
        drop((bob_sends, bob_small_sends, carol_sends, carol_small_sends));
    }

    /// A 4K camera picture is ready for the window well within the 40 ms
    /// between pictures, written into the buffer the picture before last
    /// went in: made RGB whole, then mirrored, then copied, it took 52 ms
    /// on a fast desktop, and the preview stuttered. (Timed only where
    /// the code's optimised.)
    #[test]
    fn a_4k_picture_is_ready_for_the_window_in_time() {
        let (w, h) = (3840, 2160);
        let picture = Picture::rgb(w, h, (0..w * h).flat_map(|i| [(i % w) as u8, (i / w) as u8, 90]).collect()).to_i420().into_owned();
        let mut buffers = Buffers::<Pixel>::default();
        let mut shown = Vec::new();
        let mut each = std::time::Duration::MAX;
        for _ in 0..10 {
            let started = std::time::Instant::now();
            shown = vec![pixels(&picture, true, 1, &mut buffers)];
            each = each.min(started.elapsed());
        }
        assert_eq!((shown[0].width(), shown[0].height()), (3840, 2160));
        if !cfg!(debug_assertions) {
            assert!(each < std::time::Duration::from_millis(20), "{each:?} a picture");
        }
    }
}
