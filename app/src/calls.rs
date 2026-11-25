//! Calls, from an app's side: placing one, a ring coming in (answered
//! at once for a peer set to auto-answer), answering or refusing it,
//! the live call with its microphone, speaker and — for video — camera
//! and pictures, and hanging up. One call at a time, which may be a
//! conference: a voice call that anyone in it has added a peer of
//! theirs to (see `swarm_core::conference`), held as a link to each
//! participant who's our peer, their voices mixed. Also the camera
//! preview, since a camera can only be open once: a video call takes it
//! from the preview.
//!
//! Every change of state comes back as a `Note` to show. Opening and
//! closing audio and camera devices blocks briefly, so it's marked as
//! such (`block_in_place`) — unmarked, it could hold up a connection's
//! cover-slot timer long enough to show on the wire.

use swarm_core::lock::Locked;
use crate::audio::AudioHandle;
use crate::camera::{self, Camera};
use crate::screen::Screen;
use crate::notes::Note;
use crate::sidebar::{PeerRow, Sidebar};
use crate::meter::Meter;
use crate::talking::Talking;
use crate::remote::{RemoteControl, Watching};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use swarm_core::call::{self, CallCommand, CallLink, CallSignal};
use swarm_core::conference::{self, Roster};
use swarm_core::node::devices::{self, DeviceInfo};
use swarm_core::node::{format_bitrate, CallEvent, InUse, NodeHandle};
use swarm_core::protocol;
use swarm_core::video::Picture;
use tokio::sync::{mpsc, oneshot, watch};

/// A ring waiting on an answer. Resolved by answering, refusing, the
/// ring timing out, or the caller giving up.
pub struct Incoming {
    /// Who's calling, as shown.
    pub from: String,
    /// Their entry's name, which the call's answered on.
    name: String,
    /// They offer a video call.
    pub video: bool,
    pub deadline: Instant,
    /// Into a conference: who else is in it, by name.
    pub with: Vec<String>,
    conference: Option<Roster>,
    link: CallLink,
    /// When it started ringing (unix seconds), to note it at if it's
    /// missed; 0, it isn't noted (a conference member's ask for a link).
    at: u64,
}

impl Incoming {
    /// A ring nobody answered, on any of the user's devices: noted in
    /// the caller's chat (see `NodeHandle::note_missed_call`).
    fn note_missed(&self, node: &NodeHandle) {
        if self.at != 0 {
            node.note_missed_call(&self.name, self.video, self.at);
        }
    }
}

impl Active {
    /// Where the link to `name` is, an entry's name as the node gives it
    /// in a call event: that entry's; or, for a call through another of
    /// your devices (named after the first of their entries there), the
    /// link to another of the same person's entries.
    fn link_named(&self, name: &str) -> Option<usize> {
        let user = swarm_core::node::user_name(name);
        self.links.iter().position(|l| l.name == name).or_else(|| self.links.iter().position(|l| swarm_core::node::user_name(&l.name) == user))
    }

    /// What the call's sending and receiving, in bits a second over the
    /// last second: its voice and pictures, on every link. `None` until
    /// it's gone a second.
    pub fn traffic(&self) -> Option<(u64, u64)> {
        use std::sync::atomic::Ordering::Relaxed;
        let counts: Vec<_> = self.links.iter().map(|l| (l.key, l.budget.sent.load(Relaxed), l.budget.received.load(Relaxed))).collect();
        self.traffic.lock().ok()?.sample(Instant::now(), &counts)
    }

    /// The microphone's muted (it sends silence).
    pub fn muted(&self) -> bool {
        self.audio.muted()
    }

    /// Whom a file sent from the call goes to (see `file_goes_to`), given
    /// the keys of the person whose chat is open.
    pub fn file_goes_to(&self, open: &[[u8; 32]]) -> Option<[u8; 32]> {
        let others: Vec<[u8; 32]> = self.heard().into_iter().filter_map(|(_, _, key, _)| key).collect();
        file_goes_to(&others, open, self.shown_remote().map(|(remote, _)| remote.key))
    }

    /// It's a conference.
    pub fn in_conference(&self) -> bool {
        self.conference.is_some()
    }

    /// The other side whose pictures are shown large, with their name as
    /// shown: the speaker (see `speaker`) if we have their pictures, else
    /// the first with a picture, else the first. A new speaker's shown
    /// once their large pictures are there to show — a clean cut, the last
    /// one kept till then — or, should none come, after
    /// `SHOWN_UNREADY_AFTER`, their small blown up. Whose large pictures
    /// are decoded is set here too (see `call::VideoStats::large_off`):
    /// the one shown, the speaker, and one more who's talking, ready for
    /// when they take over; in a call shown without a strip, everyone's.
    pub fn shown_remote(&self) -> Option<(&Remote, String)> {
        let video = self.video.as_ref()?;
        let now = Instant::now();
        let talking = self.talking.locked();
        let remote = video.shown_large(&mut self.shown.locked(), talking.speaker(), &|key| talking.is_talking(key, now), now)?;
        drop(talking);
        Some((remote, self.name_of(&remote.key)))
    }

    /// Our screen's size, while it's shared.
    fn sharing_size(&self) -> Option<(u32, u32)> {
        let screen = self.video.as_ref()?.screen.as_ref()?;
        Some((screen.info().width, screen.info().height))
    }

    /// Whose screen we could ask to control: the one shown large, while
    /// it's a screen they're sharing, on a link straight to them.
    pub fn controllable(&self) -> Option<[u8; 32]> {
        let (remote, _) = self.shown_remote()?;
        self.links.iter().find(|l| l.key == remote.key)?;
        remote.their_screen().then_some(remote.key)
    }

    /// Whose screen we could ask to control: the one shown large, while
    /// they're sharing it; or, in a call with one other (and no screen of
    /// theirs shown), theirs, to see as well — they share it as they
    /// allow it.
    pub fn askable(&self) -> Option<[u8; 32]> {
        self.controllable().or_else(|| match &self.links[..] {
            [only] if self.conference.is_none() => Some(only.key),
            _ => None,
        })
    }

    /// What's to be shown about control of a screen, either way: who's
    /// asking to control ours, who's controlling it, and whose we're
    /// asking for or controlling.
    pub fn control_state(&self) -> ControlState {
        let name = |key: [u8; 32]| self.name_of(&key);
        ControlState {
            asking: self.control.asking().map(name),
            asking_screen: self.control.asking().is_some_and(|key| self.control.wants_screen(key)),
            controller: self.control.controller().map(|(key, paused)| (name(key), paused)),
            watching: self.control.watching().map(|(key, state)| (name(key), state)),
            can_ask: self.askable().is_some(),
        }
    }

    /// `key`'s name as shown: in a conference, as it shows members; else
    /// the link's.
    pub fn name_of(&self, key: &[u8; 32]) -> String {
        if let Some(conference) = &self.conference {
            return conference.shown(key);
        }
        self.links.iter().find(|l| l.key == *key).map_or_else(|| crate::names::published_or_key(key), |l| l.shown.clone())
    }

    /// Our voice's level as a meter shows it, 0 to 1: what's sent, so
    /// none while muted.
    pub fn own_level(&self) -> f32 {
        self.own.locked().shown(Instant::now())
    }

    /// `key`'s voice's level as a meter shows it, 0 to 1.
    pub fn level(&self, key: &[u8; 32]) -> f32 {
        self.talking.locked().level(key, Instant::now())
    }

    /// Everyone in the call, for its list and meters: a conference's
    /// members by number, else us and the other side (number 0: none
    /// shown). Each's name as shown ("you" for us), key (None for us),
    /// and whether we can remove them.
    pub fn heard(&self) -> Vec<(u32, String, Option<[u8; 32]>, bool)> {
        let numbered = self.numbered();
        if numbered.is_empty() {
            let others = self.links.iter().map(|l| (0, l.shown.clone(), Some(l.key), false));
            return std::iter::once((0, "you".to_string(), None, false)).chain(others).collect();
        }
        let me = self.conference.as_ref().map(|c| c.me);
        numbered.into_iter().map(|(number, name, key, removable)| (number, name, (Some(key) != me).then_some(key), removable)).collect()
    }

    /// How the call's going, a row each, label and value, for Settings'
    /// call details: the round trip, what it sends and receives, and in
    /// a video call the pictures' sizes and coding.
    pub fn details(&self) -> Vec<(&'static str, String)> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut rows = Vec::new();
        let trips: Vec<(String, u64)> = self.links.iter().map(|l| (l.shown.clone(), l.budget.round_trip.load(Relaxed))).filter(|(_, micros)| *micros > 0).collect();
        match trips.as_slice() {
            [] => {}
            [(_, micros)] => rows.push(("round trip", round_trip_text(*micros))),
            _ => rows.push(("round trip", trips.iter().map(|(name, micros)| format!("{name} {}", round_trip_text(*micros))).collect::<Vec<_>>().join(", "))),
        }
        if let Some(traffic) = self.traffic() {
            rows.push(("traffic", traffic_text(traffic)));
        }
        let (Some(video), Some((remote, name))) = (&self.video, self.shown_remote()) else { return rows };
        let stats = &remote.stats;
        let ms = |a: &std::sync::atomic::AtomicU64| a.load(Relaxed) as f32 / 1000.0;
        let (encoding_on, decoding_on) = stats.on_chip();
        let (sent, shown) = stats.codecs();
        let size = remote.large_view().borrow().as_ref().map(|p| format!("{}×{} ", p.width, p.height)).unwrap_or_default();
        let theirs = if remote.their_screen() { format!("{name}'s screen") } else { name.clone() };
        rows.push(("showing", format!("{theirs}, {size}{shown}, decoding {:.1} ms{decoding_on}", ms(&stats.decode_micros))));
        if let Some(s) = stats.sending() {
            rows.push(("sending", format!("to {name} {}×{} {sent}, encoding {:.1} ms{encoding_on}", s.width, s.height, ms(&stats.encode_micros))));
        }
        rows.push(("video up to", format!("{} kbit/s", stats.bitrate.load(Relaxed) / 1000)));
        match (&video.screen, &video.camera) {
            (Some(screen), _) => rows.push(("sharing", format!("your {}", screen.info().name))),
            (None, Some(c)) => rows.push(("camera", format!("{}×{} at {:.0}/s", c.info().width, c.info().height, c.frames_per_second()))),
            (None, None) => {}
        }
        rows
    }

    /// Whether `key`'s talking now (see `talking`).
    pub fn is_talking(&self, key: &[u8; 32]) -> bool {
        self.talking.locked().is_talking(key, Instant::now())
    }

    /// Who a speaker view shows large: the last to take over talking.
    pub fn speaker(&self) -> Option<[u8; 32]> {
        self.talking.locked().speaker()
    }

    /// In a conference, who's in it that we can't hear, and why — or
    /// `None` when we hear everyone.
    pub fn conference_note(&self) -> Option<String> {
        let conference = self.conference.as_ref()?;
        let (mut not_peers, mut connecting) = (Vec::new(), Vec::new());
        for key in conference.roster.members() {
            if key == conference.me || self.links.iter().any(|l| l.key == key) || conference.heard_through.contains_key(&key) {
                continue;
            }
            match conference.names.get(&key) {
                Some(Named::Peer(name)) => connecting.push(name.clone()),
                Some(Named::Other(name)) => not_peers.push(name.clone()),
                None => not_peers.push(crate::names::published_or_key(&key)),
            }
        }
        let mut parts = Vec::new();
        if !not_peers.is_empty() {
            parts.push(format!("can't hear {} (not your {}, and no one here hears you both)", not_peers.join(", "), if not_peers.len() == 1 { "contact" } else { "contacts" }));
        }
        if !connecting.is_empty() {
            parts.push(format!("not yet hearing {}", connecting.join(", ")));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    /// In a conference, who's in it by number (see `conference`), as
    /// shown, with whether we outrank them (and so can remove them).
    pub fn numbered(&self) -> Vec<(u32, String, [u8; 32], bool)> {
        let Some(conference) = &self.conference else { return Vec::new() };
        let roster = &conference.roster;
        let shown = |key: &[u8; 32]| if *key == conference.me { "you".to_string() } else { conference.shown(key) };
        roster.members().iter().map(|key| (roster.number(key).unwrap_or(0), shown(key), *key, roster.outranks(&conference.me, key))).collect()
    }

    /// In a conference, ends the links to anyone no longer in it — left,
    /// or removed — after they've been told the roster.
    fn drop_non_members(&mut self) {
        let Some(conference) = &self.conference else { return };
        let gone: Vec<usize> = (0..self.links.len()).filter(|&i| !conference.roster.is_member(&self.links[i].key)).collect();
        for i in gone.into_iter().rev() {
            let link = self.links.remove(i);
            self.forget_link(&link);
            let _ = link.hangup.send(());
        }
    }

    /// Who's heard, as shown: one name, or each participant's — through
    /// whom, for one whose voice is passed on, or through which of your
    /// devices, for a link that goes through one.
    fn update_name(&mut self) {
        let mut heard: Vec<String> = self.links.iter().map(Link::label).collect();
        let Some(conference) = &self.conference else {
            if !heard.is_empty() {
                self.peer_name = heard.join(", ");
            }
            return;
        };
        for key in conference.roster.members() {
            if let Some((via, _)) = conference.heard_through.get(&key) {
                heard.push(format!("{} (through {})", conference.shown(&key), conference.shown(via)));
            }
        }
        self.peer_name = if heard.is_empty() { "nobody".to_string() } else { heard.join(", ") };
    }

    /// In a conference, after anything changes: tells every link whom we
    /// hear directly (if that's changed), and has the voices we can't hear
    /// directly passed on by a member who hears them (see
    /// `conference::forwarder`) — or stops, where that's no longer needed
    /// or the one passing them on no longer can.
    fn refresh(&mut self) {
        let Some(conference) = self.conference.as_mut() else { return };
        let mut hearing: Vec<[u8; 32]> = self.links.iter().map(|l| l.key).collect();
        hearing.sort();
        if hearing != conference.announced {
            conference.announced = hearing.clone();
            for link in &self.links {
                let _ = link.commands.send(CallCommand::Signal(CallSignal::Hearing(hearing.clone())));
            }
        }
        conference.hearing.retain(|key, _| hearing.contains(key));
        let members = conference.roster.members();
        self.talking.locked().keep_only(&members);
        let link_to = |key: &[u8; 32]| self.links.iter().find(|l| l.key == *key);
        for (from, (via, id)) in conference.heard_through.clone() {
            let wanted = members.contains(&from) && link_to(&from).is_none();
            if wanted && conference::forwarder(&from, &conference.hearing, Some(via)) == Some(via) {
                continue;
            }
            conference.heard_through.remove(&from);
            self.relays.incoming.locked().remove(&id);
            self.relays.incoming_video.locked().remove(&id);
            if let (Some(video), None) = (self.video.as_mut(), link_to(&from)) {
                video.unlink(&from);
            }
            if let Some(link) = link_to(&via) {
                let _ = link.commands.send(CallCommand::Signal(CallSignal::Forward { from, id, on: false }));
            }
        }
        for from in members {
            if from == conference.me || link_to(&from).is_some() || conference.heard_through.contains_key(&from) {
                continue;
            }
            let Some(via) = conference::forwarder(&from, &conference.hearing, None) else { continue };
            let Some(link) = link_to(&via) else { continue };
            let mut incoming = self.relays.incoming.locked();
            let id = (0..=u8::MAX).map(|n| conference.next_id.wrapping_add(n)).find(|id| !incoming.contains_key(id)).unwrap_or(0);
            conference.next_id = id.wrapping_add(1);
            let (opus_tx, mut opus_rx) = mpsc::unbounded_channel::<Vec<u8>>();
            let (speaker, talking) = (self.audio.speaker(), self.talking.clone());
            tokio::spawn(async move {
                let Ok(mut decoder) = call::AudioDecoder::new() else { return };
                while let Some(opus) = opus_rx.recv().await {
                    if let Ok(pcm) = decoder.decode(&opus) {
                        talking.locked().hear(from, &pcm, Instant::now());
                        if speaker.send(pcm).is_err() {
                            break;
                        }
                    }
                }
            });
            incoming.insert(id, opus_tx);
            conference.heard_through.insert(from, (via, id));
            let _ = link.commands.send(CallCommand::Signal(CallSignal::Forward { from, id, on: true }));
        }
        if let Some(video) = self.video.as_mut() {
            for (from, (via, id)) in conference.heard_through.clone() {
                let Some(link) = link_to(&via) else { continue };
                if video.remote(&from).is_some() {
                    continue;
                }
                let (display, small, stats) = video.relayed(from);
                let (to_show, mut back) = call::show_relayed(display, small, stats);
                self.relays.incoming_video.locked().insert(id, to_show);
                let commands = link.commands.clone();
                tokio::spawn(async move {
                    while let Some(signal) = back.recv().await {
                        if commands.send(CallCommand::Signal(CallSignal::Via { id, signal: Box::new(signal) })).is_err() {
                            break;
                        }
                    }
                });
            }
        }
        self.update_name();
    }

    /// A member asks us to pass `from`'s voice on to them (`on`), or to
    /// stop: only someone we hear directly, to someone else we do. Each is
    /// encoded afresh for the link it goes on, sharing its room with ours.
    fn forward(&mut self, to_name: &str, from: [u8; 32], id: u8, on: bool) {
        if self.conference.is_none() {
            return;
        }
        let Some(to) = self.link_named(to_name).map(|at| &self.links[at]) else { return };
        let mut outgoing = self.relays.outgoing.locked();
        if let Some(list) = outgoing.get_mut(&from) {
            list.retain(|f| f.to != to.key);
        }
        if on && from != to.key && self.links.iter().any(|l| l.key == from) {
            let (pcm_tx, mut pcm_rx) = mpsc::unbounded_channel::<Vec<f32>>();
            let (commands, budget) = (to.commands.clone(), to.budget.clone());
            tokio::spawn(async move {
                let Ok(mut encoder) = call::AudioEncoder::new() else { return };
                while let Some(pcm) = pcm_rx.recv().await {
                    let Ok(opus) = encoder.encode(&pcm, budget.audio.load(std::sync::atomic::Ordering::Relaxed)) else { continue };
                    if commands.send(CallCommand::Signal(CallSignal::Relayed { id, opus })).is_err() {
                        break;
                    }
                }
            });
            outgoing.entry(from).or_default().push(Forwarding { to: to.key, pcm: pcm_tx });
        }
        let count = outgoing.values().flatten().filter(|f| f.to == to.key).count();
        to.budget.relayed.store(count, std::sync::atomic::Ordering::Relaxed);
        drop(outgoing);
        let mut video_out = self.relays.outgoing_video.locked();
        let list = video_out.entry(from).or_default();
        list.retain(|p| p.to != to.key);
        let from_link = self.links.iter().find(|l| l.key == from);
        if let (true, Some(from_link)) = (on && from != to.key && !to.budget.covered.load(std::sync::atomic::Ordering::Relaxed), from_link) {
            let from_stats = self.video.as_ref().and_then(|v| v.remote(&from)).map(|r| r.stats.clone());
            list.push(PassingOn { to: to.key, id, to_link: to.commands.clone(), from_link: from_link.commands.clone(), from_stats, shown_in: ((0, 0), (0, 0)), no_h265: false });
            for small in [false, true] {
                let _ = from_link.commands.send(CallCommand::Signal(CallSignal::KeyframeRequest { small }));
            }
        }
        if let Some(stats) = self.video.as_ref().and_then(|v| v.remote(&from)).map(|r| r.stats.clone()) {
            ask_for_all(&stats, list);
        }
    }

    /// A link's gone: nothing more is passed on to or from them.
    fn forget_link(&mut self, link: &Link) {
        link.budget.relayed.store(0, std::sync::atomic::Ordering::Relaxed);
        self.control.link_gone(link.key);
        self.control_links.locked().remove(&link.key);
        if let Some(video) = self.video.as_mut() {
            video.unlink(&link.key);
        }
        let mut outgoing = self.relays.outgoing.locked();
        outgoing.remove(&link.key);
        for list in outgoing.values_mut() {
            list.retain(|f| f.to != link.key);
        }
        drop(outgoing);
        let mut video_out = self.relays.outgoing_video.locked();
        video_out.remove(&link.key);
        for list in video_out.values_mut() {
            list.retain(|p| p.to != link.key);
            if let Some(stats) = list.first().and_then(|p| p.from_stats.clone()) {
                ask_for_all(&stats, list);
            }
        }
        drop(video_out);
        if let Some(conference) = self.conference.as_mut() {
            conference.hearing.remove(&link.key);
        }
    }

    /// Sends what we know of the roster on every link.
    fn tell_all(&self) {
        let Some(conference) = &self.conference else { return };
        for link in &self.links {
            let _ = link.commands.send(CallCommand::Signal(CallSignal::Roster(conference.roster.clone())));
        }
    }
}

/// Control of a shared screen as a front end shows it (see
/// `Active::control_state`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ControlState {
    /// Who's asking to control our screen.
    pub asking: Option<String>,
    /// They asked while it wasn't shared: allowing shares it too.
    pub asking_screen: bool,
    /// Who's controlling it, and whether they're paused (we're using our
    /// own mouse).
    pub controller: Option<(String, bool)>,
    /// Whose screen we're asking for, or controlling, and how it stands.
    pub watching: Option<(String, Watching)>,
    /// The screen shown large can be asked for.
    pub can_ask: bool,
}

/// A live call, placed or answered here.
pub struct Active {
    /// Who it's with, as shown: in a conference, everyone we hear.
    pub peer_name: String,
    pub started_at: Instant,
    /// Set for a video call.
    pub video: Option<CallVideo>,
    audio: AudioHandle,
    /// A link to each other side: one, but for a conference.
    links: Vec<Link>,
    /// Feeds each link the microphone.
    mics: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<Vec<f32>>>>>,
    conference: Option<Conference>,
    relays: Relays,
    /// Answered here, rather than placed: in a conference it grows into,
    /// the other side's number 1.
    answered: bool,
    /// What it's sending and receiving (see `Active::traffic`).
    traffic: std::sync::Mutex<Rate>,
    /// Who's talking, and who's the speaker, from the voices heard.
    talking: Arc<std::sync::Mutex<Talking>>,
    /// Who's shown large (see `shown_remote`).
    shown: std::sync::Mutex<Shown>,
    /// Our own voice's level, as sent (see `own_level`).
    own: Arc<std::sync::Mutex<Meter>>,
    /// Control of a shared screen, ours or theirs (see `remote`).
    pub control: RemoteControl,
    /// Someone allowed to see and control the screen, waiting for the
    /// desktop's dialog to give it (see `Calls::allow_control`).
    pending_allow: Option<[u8; 32]>,
    /// Where `control` sends: each link's commands, by key.
    control_links: Arc<std::sync::Mutex<std::collections::HashMap<[u8; 32], mpsc::UnboundedSender<CallCommand>>>>,
}

/// Who a call shows large: who it is, and who's to be next once their
/// large pictures are there, since when.
#[derive(Default)]
struct Shown {
    key: Option<[u8; 32]>,
    next: Option<([u8; 32], Instant)>,
}

/// How long a new speaker's large pictures are waited for before they're
/// shown large without: their small, blown up.
const SHOWN_UNREADY_AFTER: Duration = Duration::from_secs(1);

/// A call's traffic (see `Active::traffic`) as shown: "↑ 1.2 Mbit/s
/// ↓ 850 kbit/s".
pub fn traffic_text((sent, received): (u64, u64)) -> String {
    let rate = |bits: u64| if bits >= 1_000_000 { format!("{:.1} Mbit/s", bits as f64 / 1e6) } else { format!("{} kbit/s", bits / 1000) };
    format!("↑ {}  ↓ {}", rate(sent), rate(received))
}

/// A round trip of `micros` as shown: to a tenth of a millisecond while
/// it's under ten ("0.4 ms", "23 ms").
pub fn round_trip_text(micros: u64) -> String {
    if micros < 10_000 { format!("{:.1} ms", micros as f64 / 1000.0) } else { format!("{} ms", micros / 1000) }
}

/// Each link's bytes sent and received, by its key.
type Counts = std::collections::HashMap<[u8; 32], (u64, u64)>;

/// A call's voice and pictures each way, in bits a second, over the last
/// second: from each link's counts (`CallBudget::sent`, `received`).
#[derive(Default)]
struct Rate {
    /// The counts last taken, and when: each link's (by key).
    last: Option<(Instant, Counts)>,
    bits: Option<(u64, u64)>,
}

impl Rate {
    /// Takes each link's counts at `now`; once a second has passed since
    /// they were last taken, the rate over it. A link not there last time
    /// (one joining a conference) counts from now: its counts include
    /// earlier calls.
    fn sample(&mut self, now: Instant, counts: &[([u8; 32], u64, u64)]) -> Option<(u64, u64)> {
        let taken = || counts.iter().map(|(key, sent, received)| (*key, (*sent, *received))).collect();
        match &self.last {
            None => self.last = Some((now, taken())),
            Some((then, before)) if now.duration_since(*then) >= Duration::from_secs(1) => {
                let (mut sent, mut received) = (0, 0);
                for (key, now_sent, now_received) in counts {
                    if let Some((was_sent, was_received)) = before.get(key) {
                        sent += now_sent.saturating_sub(*was_sent);
                        received += now_received.saturating_sub(*was_received);
                    }
                }
                let secs = now.duration_since(*then).as_secs_f64();
                self.bits = Some(((sent as f64 * 8.0 / secs) as u64, (received as f64 * 8.0 / secs) as u64));
                self.last = Some((now, taken()));
            }
            Some(_) => {}
        }
        self.bits
    }
}

/// In a conference, voices passed on: others' to us, through a member who
/// hears them, and ours of those we hear, to members who can't.
#[derive(Default)]
struct Relays {
    /// Passed on to us: where each id's frames are decoded.
    incoming: Arc<std::sync::Mutex<std::collections::HashMap<u8, mpsc::UnboundedSender<Vec<u8>>>>>,
    /// Passed on by us: for each member we hear, whom to.
    outgoing: Arc<std::sync::Mutex<std::collections::HashMap<[u8; 32], Vec<Forwarding>>>>,
    /// Pictures passed on to us: where each id's go to be shown (see
    /// `call::show_relayed`).
    incoming_video: Arc<std::sync::Mutex<std::collections::HashMap<u8, mpsc::UnboundedSender<CallSignal>>>>,
    /// Pictures passed on by us: for each member we see, whom to.
    outgoing_video: Arc<std::sync::Mutex<std::collections::HashMap<[u8; 32], Vec<PassingOn>>>>,
}

/// One member's pictures we pass on, as they come (`CallSignal::Via`): to
/// `to`, as `id`, along `to_link`; and what that side asks of them, put
/// to theirs — `from_link`, and its counts, which say what's wanted of
/// them for everyone (see `call::VideoStats::show_passed_on_in`).
struct PassingOn {
    to: [u8; 32],
    id: u8,
    to_link: mpsc::UnboundedSender<CallCommand>,
    from_link: mpsc::UnboundedSender<CallCommand>,
    from_stats: Option<Arc<call::VideoStats>>,
    /// The boxes that side shows them in, large and small, and whether it
    /// decodes H.265.
    shown_in: ((u16, u16), (u16, u16)),
    no_h265: bool,
}

/// Has a member's pictures asked for big enough, and in a coding, for
/// everyone in `list` we pass them on to, along with us.
fn ask_for_all(stats: &call::VideoStats, list: &[PassingOn]) {
    let biggest = |of: fn(&PassingOn) -> (u16, u16)| list.iter().map(of).fold((0, 0), |(w, h), (pw, ph)| (w.max(pw as usize), h.max(ph as usize)));
    stats.show_passed_on_in(biggest(|p| p.shown_in.0), biggest(|p| p.shown_in.1));
    stats.passed_on_no_h265.store(list.iter().any(|p| p.no_h265), std::sync::atomic::Ordering::Relaxed);
}

/// One voice we pass on: to `to`, its sound going to the encoder for that
/// link.
struct Forwarding {
    to: [u8; 32],
    pcm: mpsc::UnboundedSender<Vec<f32>>,
}

/// One link of a call: to `name` (a peer entry, so a device), who has
/// `key`, shown as `shown`.
struct Link {
    name: String,
    shown: String,
    key: [u8; 32],
    hangup: oneshot::Sender<()>,
    commands: mpsc::UnboundedSender<CallCommand>,
    budget: Arc<call::CallBudget>,
    /// Video for it, once the call has some (see `Calls::start_video_now`).
    video: mpsc::UnboundedSender<call::VideoIo>,
    /// Which of your devices it goes through, by name, for a contact
    /// reached only through one (see `through_device`).
    through: Option<String>,
}

impl Link {
    /// Who it's with, as the call bar shows them.
    fn label(&self) -> String {
        named_through(&self.shown, self.through.as_deref())
    }
}

/// Someone in a call as shown, with the device of yours their link goes
/// through, if it does.
fn named_through(shown: &str, through: Option<&str>) -> String {
    match through {
        Some(device) => format!("{shown} (through your device {device})"),
        None => shown.to_string(),
    }
}

/// The device of yours that peer `key` (their user key) is reached
/// through, by name, while this one has no connection of its own to them.
/// A call through it stays through it, so it's asked once, as the link
/// starts.
fn through_device(node: &NodeHandle, key: &[u8; 32]) -> Option<String> {
    match node.connection_to(key) {
        Some(InUse::Through(device)) => Some(device),
        _ => None,
    }
}

/// A conference's side of a call.
/// A group's call, ringing, before anyone's answered (`call_group`).
struct GroupRing {
    /// The group, as shown.
    name: String,
    /// The conference they're rung into: this side alone so far.
    roster: Roster,
    /// Each of them rung: their name, their devices, the ringing task.
    rings: Vec<(String, Vec<String>, tokio::task::JoinHandle<()>)>,
}

struct Conference {
    roster: Roster,
    me: [u8; 32],
    /// What each member's called: our name for a peer, else theirs.
    names: std::collections::HashMap<[u8; 32], Named>,
    /// Peers in it being asked for the link (by peer name), and when each
    /// member was last asked.
    asking: Vec<String>,
    asked: std::collections::HashMap<[u8; 32], Instant>,
    /// Members whose link ended without their leaving (their network,
    /// say), and in which round of theirs: not asked again unless they're
    /// added again.
    lost: Vec<([u8; 32], u32)>,
    /// Peers being rung into it from here (several at once, calling a
    /// group): who (as shown), every device rung, and the ringing task.
    adding: Vec<(String, Vec<String>, tokio::task::JoinHandle<()>)>,
    /// Whom each member we hear directly says they hear directly.
    hearing: std::collections::HashMap<[u8; 32], Vec<[u8; 32]>>,
    /// What we last told them we hear.
    announced: Vec<[u8; 32]>,
    /// Members we hear through another, and the id their voice comes with.
    heard_through: std::collections::HashMap<[u8; 32], ([u8; 32], u8)>,
    next_id: u8,
}

enum Named {
    Peer(String),
    Other(String),
}

/// How soon a member who didn't take the link is asked again.
const ASK_AGAIN: Duration = Duration::from_secs(10);

/// Where pictures go to be shown or sent: the newest only.
type Shows = watch::Sender<Option<Arc<Picture>>>;

/// A video call's picture side: our camera (while it's on) or our screen
/// (while it's shared, in the camera's place), the pictures going to the
/// call, the other side's pictures, and counts.
pub struct CallVideo {
    pub camera: Option<Camera>,
    /// Why the camera isn't on, when it isn't.
    pub camera_note: Option<String>,
    /// Our screen, while it's shared.
    pub screen: Option<Screen>,
    /// The screen being opened, on a thread of its own: on Wayland that
    /// waits on the desktop's dialog, which the call mustn't.
    opening_screen: Option<std::sync::mpsc::Receiver<std::result::Result<Screen, String>>>,
    /// The camera was on when sharing started: it's back on when sharing
    /// stops.
    camera_before_sharing: bool,
    /// Each other side's pictures, and its link's counts: one, but for a
    /// conference (see `CallVideo::link`).
    pub remotes: Vec<Remote>,
    /// Our pictures, to every link.
    to_call: Shows,
    camera_feed: Option<tokio::task::JoinHandle<()>>,
    /// Since when the call's wanted the camera at another size, and which.
    other_size: Option<(camera::Size, Instant)>,
    /// Whether the camera keeps pace at its size.
    pace: camera::PaceCheck,
    /// The biggest camera size the call's wanted: the camera's never
    /// made smaller than it.
    most_wanted: Option<camera::Size>,
    /// The size the call settled at, below the largest, and the camera
    /// went to (see `fit_camera`).
    settled: Option<camera::Size>,
    /// The highest bitrate the link's offered video, and when it was
    /// reached: while it's still rising, the call may yet want more.
    peak: Option<(u64, Instant)>,
    /// When `SWARM_CAMERA_DEBUG` last said how things stand.
    said: Option<Instant>,
    /// Whose one-to-one call this is, for the size it settles at to be
    /// remembered for the next (`SETTLED_FOR`).
    for_whom: Option<[u8; 32]>,
}

/// The size each contact's last one-to-one video call settled the camera
/// at, below the largest: the next is opened at it, rather than at the
/// largest and reopened smaller ten seconds in, the picture frozen for
/// a moment (the call itself starts near its last rate too, see
/// `call::UncoveredCall::starting_at`). Only while Swarm runs.
static SETTLED_FOR: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<[u8; 32], camera::Size>>> = std::sync::LazyLock::new(Default::default);

/// One other side's pictures in a video call, and the counts for our link
/// to them: what we send them, and what they send us.
pub struct Remote {
    pub key: [u8; 32],
    /// Their newest large picture: while it's wanted (see
    /// `call::VideoStats::large_off`).
    pub picture: watch::Receiver<Option<Arc<Picture>>>,
    /// Their newest small one, for a strip's tile: while they're shown in
    /// one (see `call::VideoStats::show_in`).
    pub small: watch::Receiver<Option<Arc<Picture>>>,
    pub stats: Arc<call::VideoStats>,
}

impl Remote {
    /// Where what's drawn of them large comes from: their large pictures,
    /// or, while there are none (not wanted till now, and no full frame
    /// yet), their small, blown up.
    pub fn large_view(&self) -> &watch::Receiver<Option<Arc<Picture>>> {
        if self.picture.borrow().is_some() || self.small.borrow().is_none() { &self.picture } else { &self.small }
    }

    /// Where what's drawn of them in a strip's tile comes from: their
    /// small pictures, or, while there are none (they've just been put in
    /// the strip), their large.
    pub fn tile_view(&self) -> &watch::Receiver<Option<Arc<Picture>>> {
        if self.small.borrow().is_some() || self.picture.borrow().is_none() { &self.small } else { &self.picture }
    }

    /// What to show where their picture goes while there isn't one: their
    /// camera's off, or none has come yet. `None` while there's a picture.
    pub fn note(&self, name: &str) -> Option<String> {
        if self.picture.borrow().is_some() || self.small.borrow().is_some() {
            return None;
        }
        Some(if self.stats.their_camera_off.load(std::sync::atomic::Ordering::Relaxed) {
            format!("{name}'s camera is off")
        } else if self.their_screen() {
            format!("waiting for {name}'s screen...")
        } else {
            format!("waiting for {name}'s picture...")
        })
    }

    /// They're sharing their screen.
    pub fn their_screen(&self) -> bool {
        self.stats.their_screen.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// `SWARM_CAMERA_DEBUG=1`: when a call's camera is opened and at what,
/// and every 5 s what the call's link offers and wants.
fn camera_debugging() -> bool {
    std::env::var("SWARM_CAMERA_DEBUG").is_ok_and(|v| v == "1")
}

/// How long the call has to want the camera at another size before it's
/// reopened at it: reopening leaves a gap, so not for a passing change.
const RESIZE_AFTER: Duration = Duration::from_secs(4);
/// How long the link's bitrate has to have stopped rising, below what the
/// largest size needs, before the camera's reopened smaller. A call works
/// up to its rate step by step (1080p 22 s into a call, on a fast link),
/// so it's judged by the rising stopping, not by the time taken.
#[cfg(not(test))]
const SETTLED: Duration = Duration::from_secs(10);
#[cfg(test)]
const SETTLED: Duration = Duration::from_secs(2);
/// How long a call has to want the camera smaller than it's had it,
/// once it's had it bigger, before it's reopened smaller: longer than
/// `RESIZE_AFTER`, as a link that's dropped often comes back.
#[cfg(not(test))]
const DROPPED: Duration = Duration::from_secs(20);
#[cfg(test)]
const DROPPED: Duration = Duration::from_secs(5);

impl CallVideo {
    /// A video call's pictures, with the camera on if `camera` (or why
    /// not, in `camera_note`); each link's given its video by `link`.
    /// `fit_camera` wants calling now and then for as long as it lasts.
    pub fn start(camera: bool) -> CallVideo {
        CallVideo::start_for(camera, None)
    }

    /// `start`, for a one-to-one call with `whom`: the camera opened at the
    /// size their last call settled at, if that's smaller than the
    /// largest (`SETTLED_FOR`), and what this one settles at remembered.
    pub fn start_for(camera: bool, whom: Option<[u8; 32]>) -> CallVideo {
        crate::hardware::init();
        let (to_call, _) = watch::channel(None);
        let mut video = CallVideo {
            camera: None,
            camera_note: None,
            screen: None,
            opening_screen: None,
            camera_before_sharing: false,
            to_call,
            camera_feed: None,
            other_size: None,
            pace: camera::PaceCheck::default(),
            most_wanted: None,
            settled: whom.and_then(|key| SETTLED_FOR.locked().get(&key).copied()),
            peak: None,
            said: None,
            remotes: Vec::new(),
            for_whom: whom,
        };
        if camera {
            video.camera_on();
        }
        video
    }

    /// The video for our link to `key`, for `call::run`: our pictures, and
    /// theirs shown (see `remotes`). Each link encodes our pictures for
    /// itself, at its own rate.
    pub fn link(&mut self, key: [u8; 32]) -> call::VideoIo {
        self.unlink(&key);
        let (display, picture) = watch::channel(None);
        let (small, small_picture) = watch::channel(None);
        let stats = Arc::new(call::VideoStats::default());
        stats.screen.store(self.screen.is_some(), std::sync::atomic::Ordering::Relaxed);
        self.remotes.push(Remote { key, picture, small: small_picture, stats: stats.clone() });
        call::VideoIo { camera: self.to_call.subscribe(), display, small, stats }
    }

    /// Where the pictures of `key` — a member seen through another, who
    /// passes them on (see `call::show_relayed`) — are shown, large and
    /// small, and their counts: shown as anyone linked directly is.
    pub fn relayed(&mut self, key: [u8; 32]) -> (Shows, Shows, Arc<call::VideoStats>) {
        self.unlink(&key);
        let (display, picture) = watch::channel(None);
        let (small, small_picture) = watch::channel(None);
        let stats = Arc::new(call::VideoStats::default());
        self.remotes.push(Remote { key, picture, small: small_picture, stats: stats.clone() });
        (display, small, stats)
    }

    /// Our link to `key`'s gone (or their pictures are no longer passed
    /// on): their pictures with it.
    pub fn unlink(&mut self, key: &[u8; 32]) {
        self.remotes.retain(|r| r.key != *key);
    }

    /// Who's shown small, beside `large` (shown large): with pictures
    /// from two or more, everyone else, in the order their links came,
    /// and then us (`None`). With one other, nobody: a call of two is
    /// shown as it always was.
    pub fn strip(&self, large: Option<[u8; 32]>) -> Vec<Option<[u8; 32]>> {
        if self.remotes.len() < 2 {
            return Vec::new();
        }
        self.remotes.iter().map(|r| r.key).filter(|key| Some(*key) != large).map(Some).chain([None]).collect()
    }

    pub fn remote(&self, key: &[u8; 32]) -> Option<&Remote> {
        self.remotes.iter().find(|r| r.key == *key)
    }

    /// Tells everyone whose pictures come the boxes they're drawn in here
    /// (see `call::VideoStats::show_in`): whoever's large now, and whoever
    /// may be next, so none is sent bigger than it can be shown.
    pub fn show_in(&self, large: (usize, usize), small: (usize, usize)) {
        for remote in &self.remotes {
            remote.stats.show_in(large, small);
        }
    }

    /// Who's shown large (see `Active::shown_remote`), as `shown` last had
    /// it, with `speaker` the speaker and `talking` saying who's talking:
    /// and whose large pictures are decoded, set.
    fn shown_large(&self, shown: &mut Shown, speaker: Option<[u8; 32]>, talking: &dyn Fn(&[u8; 32]) -> bool, now: Instant) -> Option<&Remote> {
        let speaker = speaker.and_then(|key| self.remote(&key));
        let wanted = speaker.or_else(|| self.remotes.iter().find(|r| r.picture.borrow().is_some() || r.small.borrow().is_some())).or_else(|| self.remotes.first())?;
        let remote = match shown.key.and_then(|key| self.remote(&key)) {
            Some(current) if current.key != wanted.key => {
                let since = match shown.next {
                    Some((key, since)) if key == wanted.key => since,
                    _ => shown.next.insert((wanted.key, now)).1,
                };
                let ready = wanted.picture.borrow().is_some();
                let nothing_shown = current.picture.borrow().is_none() && current.small.borrow().is_none();
                if ready || nothing_shown || now.duration_since(since) >= SHOWN_UNREADY_AFTER { wanted } else { current }
            }
            _ => wanted,
        };
        if remote.key == wanted.key {
            shown.next = None;
        }
        shown.key = Some(remote.key);
        let warming = self.remotes.iter().map(|r| r.key).find(|key| *key != remote.key && *key != wanted.key && talking(key));
        let strip = self.remotes.len() >= 2;
        for other in &self.remotes {
            let large = !strip || other.key == remote.key || other.key == wanted.key || Some(other.key) == warming;
            other.stats.large_off.store(!large, std::sync::atomic::Ordering::Relaxed);
        }
        Some(remote)
    }

    /// The link wanting the biggest pictures of us: the camera's sized for
    /// it (the others make theirs smaller as they encode).
    fn lead(&self) -> Option<&Arc<call::VideoStats>> {
        self.remotes.iter().map(|r| &r.stats).max_by_key(|s| s.wanted().map_or(0, |w| w.width * w.height))
    }

    /// Says on every link whether it's a screen we're sending.
    fn say_screen(&self, screen: bool) {
        for remote in &self.remotes {
            remote.stats.screen.store(screen, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Turns the camera on: opens it, and feeds its pictures to the call
    /// (in place of the screen, if that's being shared). At the largest
    /// size, unless the call's found it won't send that: reopening the
    /// camera bigger as the call grows would leave the picture frozen for
    /// a moment (see `fit_camera`).
    pub fn camera_on(&mut self) {
        self.stop_screen();
        self.camera_on_at(self.usual_size());
    }

    /// The size the camera's opened at: the largest it keeps pace at, or
    /// what the call's settled at, if that's smaller.
    fn usual_size(&self) -> camera::Size {
        let best = camera::best_size();
        match self.settled {
            Some(size) if size.0 < best.0 => size,
            _ => best,
        }
    }

    fn camera_on_at(&mut self, size: camera::Size) {
        if camera_debugging() {
            let offered = self.lead().map_or(0, |s| s.offered.load(std::sync::atomic::Ordering::Relaxed));
            eprintln!("{} camera: opening at {}×{} (the link offering {} kbit/s)", crate::unix_now(), size.0, size.1, offered / 1000);
        }
        match tokio::task::block_in_place(|| Camera::open_at(size)) {
            Ok(camera) => {
                self.feed(camera.subscribe());
                self.camera = Some(camera);
                self.camera_note = None;
            }
            Err(e) => self.camera_note = Some(format!("{e:#}")),
        }
    }

    /// Sends `pictures` to the call, from now on. Until their first, the
    /// call keeps the last picture it had: a camera reopened at another
    /// size, or a screen starting, has none for a moment, and the call
    /// takes none for the camera turned off, which the other side shows
    /// as nothing. (`camera_off` says so itself.)
    fn feed(&mut self, mut pictures: watch::Receiver<Option<Arc<Picture>>>) {
        if let Some(feed) = self.camera_feed.take() {
            feed.abort();
        }
        let to_call = self.to_call.clone();
        let send = move |picture: Option<Arc<Picture>>| {
            if picture.is_some() {
                to_call.send_replace(picture);
            }
        };
        send(pictures.borrow_and_update().clone());
        self.camera_feed = Some(tokio::spawn(async move {
            while pictures.changed().await.is_ok() {
                send(pictures.borrow_and_update().clone());
            }
        }));
    }

    /// Shares the screen in the camera's place: the other side's told
    /// it's a screen, and it's sent sharper and slower than a camera (see
    /// `swarm_core::video`). Opened on a thread of its own, as on Wayland
    /// the desktop asks the user first: `Ok(false)` while it's still
    /// asking (see `screen_opened`). `Err` says why it can't be.
    pub fn share_screen(&mut self) -> std::result::Result<bool, String> {
        if self.screen.is_some() {
            return Ok(true);
        }
        if self.opening_screen.is_none() {
            let (opened_tx, opened_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = opened_tx.send(Screen::open().map_err(|e| format!("{e:#}")));
            });
            self.opening_screen = Some(opened_rx);
        }
        let waited = tokio::task::block_in_place(|| self.opening_screen.as_ref().map(|opening| opening.recv_timeout(Duration::from_millis(500))));
        match waited {
            Some(Ok(opened)) => {
                self.opening_screen = None;
                self.start_sharing(opened?);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// The screen being asked for: whether that's ended, and how — `None`
    /// while it's still being asked (or never was). Call it now and then.
    pub fn screen_opened(&mut self) -> Option<std::result::Result<(), String>> {
        let opened = match self.opening_screen.as_ref()?.try_recv() {
            Ok(opened) => opened,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("the screen couldn't be opened".to_string()),
        };
        self.opening_screen = None;
        Some(opened.map(|screen| self.start_sharing(screen)))
    }

    /// The screen's being asked for (the desktop's dialog is up).
    pub fn asking_for_screen(&self) -> bool {
        self.opening_screen.is_some()
    }

    fn start_sharing(&mut self, screen: Screen) {
        self.camera_before_sharing = self.camera.is_some();
        self.close_camera();
        self.say_screen(true);
        self.feed(screen.subscribe());
        self.screen = Some(screen);
    }

    /// Stops sharing the screen: the camera's back, if it was on before.
    pub fn stop_sharing(&mut self) {
        if self.screen.is_none() {
            self.opening_screen = None;
            return;
        }
        let camera_back = self.camera_before_sharing;
        self.stop_screen();
        if camera_back {
            self.camera_on_at(self.usual_size());
        } else {
            self.to_call.send_replace(None);
            self.camera_note = Some("camera off".to_string());
        }
    }

    fn stop_screen(&mut self) {
        self.opening_screen = None;
        let Some(screen) = self.screen.take() else { return };
        if let Some(feed) = self.camera_feed.take() {
            feed.abort();
        }
        self.say_screen(false);
        tokio::task::block_in_place(|| drop(screen));
    }

    /// What's being sent, to show as ours: the shared screen (as it is),
    /// or the camera (mirrored, as a mirror shows you, if it faces you —
    /// a phone's back camera's shown as it sees) — with whether to mirror
    /// it.
    pub fn local_picture(&self) -> Option<(Arc<Picture>, bool)> {
        let last = || self.to_call.borrow().clone();
        match (&self.screen, &self.camera) {
            (Some(screen), _) => screen.latest().or_else(last).map(|p| (p, false)),
            (None, Some(camera)) => camera.latest().or_else(last).map(|p| (p, !camera.info().faces_away)),
            (None, None) => None,
        }
    }

    /// Turns the camera off (and stops sharing the screen): nothing more
    /// is sent until it's back on.
    pub fn camera_off(&mut self) {
        self.stop_screen();
        self.close_camera();
        self.to_call.send_replace(None);
        self.camera_note = Some("camera off".to_string());
    }

    /// The camera's stopped of its own accord (taken off USB, say): it's
    /// turned off, as if the user had, so the other side shows our picture
    /// in place of a frozen one, and the Camera button says off. True the
    /// once, when that's just happened.
    pub fn camera_stopped(&mut self) -> bool {
        let Some(camera) = &self.camera else { return false };
        if !camera.ended() {
            return false;
        }
        eprintln!("{} the camera ({}) stopped giving pictures on its own, so it's off", crate::unix_now(), camera.info().name);
        self.camera_off();
        self.camera_note = Some("camera stopped".to_string());
        true
    }

    fn close_camera(&mut self) {
        if let Some(feed) = self.camera_feed.take() {
            feed.abort();
        }
        let camera = self.camera.take();
        tokio::task::block_in_place(|| drop(camera));
    }

    /// The camera at the size the call needs, called now and then during
    /// a call. It's opened at the largest (see `camera_on`), and reopened —
    /// which freezes the picture for a moment — only:
    /// - smaller, if it's too slow at its size (`camera::PaceCheck`);
    /// - smaller, once, to the biggest the call's wanted, if that's less
    ///   and the link's stopped offering more for `SETTLED`: it works up
    ///   to its rate step by step, so the time taken doesn't say it won't
    ///   get there, but its stopping does (and a camera at 4K as JPEGs
    ///   then gives 1440p uncompressed, say, and costs less);
    /// - bigger, should the call later want that after all (for
    ///   `RESIZE_AFTER`);
    /// - smaller than the call's once wanted only once the link's dropped
    ///   for `DROPPED`.
    pub fn fit_camera(&mut self) {
        let Some(camera) = &self.camera else { return };
        if self.pace.too_slow(camera) {
            self.other_size = None;
            self.close_camera();
            self.camera_on_at(self.usual_size());
            return;
        }
        let Some(lead) = self.lead().cloned() else { return };
        let offered = lead.offered.load(std::sync::atomic::Ordering::Relaxed);
        match &mut self.peak {
            Some((top, since)) if offered > *top => (*top, *since) = (offered, Instant::now()),
            None if offered > 0 => self.peak = Some((offered, Instant::now())),
            _ => {}
        }
        let Some(wanted) = lead.wanted() else { return };
        if camera_debugging() && self.said.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            self.said = Some(Instant::now());
            let rose = self.peak.map_or("never".to_string(), |(_, since)| format!("{:.0} s ago", since.elapsed().as_secs_f32()));
            let sending = lead.sending().map_or("nothing".to_string(), |s| format!("{}×{}", s.width, s.height));
            let theirs = self.remotes.iter().filter_map(|r| r.picture.borrow().as_ref().map(|p| format!("{}×{}", p.width, p.height))).collect::<Vec<_>>().join(", ");
            eprintln!("{} camera: {}×{} (asked {}×{}); the link offering {} kbit/s (last rose {rose}), wanting {}×{}, sending {sending} {}; theirs shown at {theirs} {}", crate::unix_now(), camera.info().width, camera.info().height, camera.asked().0, camera.asked().1, offered / 1000, wanted.width, wanted.height, lead.codecs().0, lead.codecs().1);
        }
        let need = camera::size_for(wanted.width);
        let most = match self.most_wanted {
            Some(most) if most.0 >= need.0 => most,
            _ => *self.most_wanted.insert(need),
        };
        let (asked, gives) = (camera.asked(), camera.info().width);
        if asked.0 > most.0 && gives > most.0 {
            let settled = self.peak.is_some_and(|(_, since)| since.elapsed() >= SETTLED) && camera.running_for() >= SETTLED;
            if settled {
                self.settled = Some(most);
                self.remember_settled();
                self.other_size = None;
                self.close_camera();
                self.camera_on_at(most);
            }
            return;
        }
        let bigger = need.0 > asked.0 && gives >= asked.0;
        let smaller = need.0 < asked.0.min(gives);
        if !bigger && !smaller {
            self.other_size = None;
            return;
        }
        let after = if bigger { RESIZE_AFTER } else { DROPPED };
        match self.other_size {
            Some((size, since)) if size == need && since.elapsed() >= after => {
                self.other_size = None;
                self.settled = Some(need);
                self.remember_settled();
                if smaller {
                    self.most_wanted = Some(need);
                }
                self.pace = camera::PaceCheck::default();
                self.close_camera();
                self.camera_on_at(need);
            }
            Some((size, _)) if size == need => {}
            _ => self.other_size = Some((need, Instant::now())),
        }
    }
}

impl CallVideo {
    /// The size the call's settled at, kept for the next call with them
    /// (`SETTLED_FOR`): one-to-one calls only.
    fn remember_settled(&self) {
        if let (Some(whom), Some(size), 1) = (self.for_whom, self.settled, self.remotes.len()) {
            SETTLED_FOR.locked().insert(whom, size);
        }
    }
}

impl Drop for CallVideo {
    fn drop(&mut self) {
        if let Some(feed) = self.camera_feed.take() {
            feed.abort();
        }
    }
}

/// The call side of an app.
#[derive(Default)]
pub struct Calls {
    incoming: Option<Incoming>,
    /// The peer a call placed from here is ringing, and the task ringing.
    /// A call placed from here, ringing: who (the row's first name),
    /// every device being rung, and the ringing task.
    outgoing: Option<(String, Vec<String>, tokio::task::JoinHandle<()>)>,
    /// A group being called from here, before anyone's answered.
    group_ring: Option<GroupRing>,
    active: Option<Active>,
    /// The camera preview, while it's open: the camera, or why it
    /// wouldn't open.
    preview: Option<std::result::Result<Camera, String>>,
    /// Whether the preview's camera keeps pace at the largest size.
    preview_pace: camera::PaceCheck,
    /// The microphone, listened to for Settings' meter while there's no
    /// call: its level, or why it wouldn't open.
    listening: Option<std::result::Result<Listening, String>>,
}

/// The microphone listened to with no call (see `Calls::keep_listening`).
struct Listening {
    _audio: AudioHandle,
    level: Arc<std::sync::Mutex<Meter>>,
}

impl Calls {
    pub fn incoming(&self) -> Option<&Incoming> {
        self.incoming.as_ref()
    }
    /// Who a call placed from here is ringing.
    pub fn outgoing(&self) -> Option<&str> {
        self.outgoing.as_ref().map(|(name, _, _)| name.as_str()).or(self.group_ring.as_ref().map(|g| g.name.as_str()))
    }
    /// Who's being rung into the conference from here (the first, if
    /// several are).
    pub fn adding(&self) -> Option<&str> {
        self.active.as_ref()?.conference.as_ref()?.adding.first().map(|(name, _, _)| name.as_str())
    }
    pub fn active(&self) -> Option<&Active> {
        self.active.as_ref()
    }
    pub fn active_mut(&mut self) -> Option<&mut Active> {
        self.active.as_mut()
    }
    pub fn busy(&self) -> bool {
        self.incoming.is_some() || self.outgoing.is_some() || self.group_ring.is_some() || self.active.is_some()
    }

    /// Whether `row` can be added to the live call: they're not in it
    /// already.
    pub fn can_add(&self, row: &PeerRow) -> bool {
        self.active.as_ref().is_some_and(|a| {
            !a.links.iter().any(|l| l.key == row.public_key)
        })
    }

    /// Why `row` can't be rung now (`video`, or voice only), if they
    /// can't: what greys the front ends' Call buttons, and what `place`
    /// says. In a call, voice is adding them to it (see `why_not_add`).
    pub fn why_not_call(&self, node: &NodeHandle, row: &PeerRow, video: bool) -> Option<String> {
        if self.active.is_some() && !video {
            return self.why_not_add(node, row);
        }
        if self.busy() {
            return Some("one call at a time".to_string());
        }
        if !row.online {
            return Some(format!("{} isn't connected — calls need them online", row.label));
        }
        if let Some(reason) = voice_blocked_by_cover(node, row) {
            return Some(format!("can't call {}: {reason}", row.label));
        }
        video.then(|| video_blocked_by_cover(node, row).map(|reason| format!("can't video call {}: {reason}", row.label))).flatten()
    }

    /// Why `row` can't be rung into the live call now, if they can't.
    pub fn why_not_add(&self, node: &NodeHandle, row: &PeerRow) -> Option<String> {
        let Some(active) = self.active.as_ref() else { return Some("adding someone needs a call".to_string()) };
        if active.links.iter().any(|l| devices::call_device(&l.name).is_some()) {
            return Some("a call with another of your devices takes nobody else".to_string());
        }
        if !self.can_add(row) {
            return Some(format!("{} is in the call already", row.label));
        }
        if active.conference.as_ref().is_some_and(|c| c.adding.iter().any(|(name, _, _)| *name == row.handle)) {
            return Some(format!("already ringing {} into the call", row.label));
        }
        if !row.online {
            return Some(format!("{} isn't connected — calls need them online", row.label));
        }
        if let Some(reason) = voice_blocked_by_cover(node, row) {
            return Some(format!("can't add {}: {reason}", row.label));
        }
        (!row.entries.iter().any(|n| node.peer_status(n).is_some_and(|s| s.online) && has_link(node, n))).then(|| format!("{} isn't connected — calls need them online", row.label))
    }

    /// Why `device`, another of yours, can't be rung now (`video`, or
    /// voice only), if it can't: what greys its Call buttons, and what
    /// `place_device` says.
    pub fn why_not_call_device(&self, node: &NodeHandle, device: &DeviceInfo, video: bool) -> Option<String> {
        let label = crate::devices::label(device);
        if device.this {
            return Some("that's this device".to_string());
        }
        if self.busy() {
            return Some("one call at a time".to_string());
        }
        let name = devices::call_name(&device.key);
        if node.call_handle(&name).filter(|_| node.peer_status(&name).is_some_and(|s| s.online)).is_none() {
            return Some(format!("{label} isn't connected — calls need it online"));
        }
        let rate = node.default_cover()?;
        if !protocol::cover_rate_carries_voice(rate) {
            return Some(format!("can't call {label}: cover rate {} is too low to carry voice (needs about 18k or more)", format_bitrate(rate)));
        }
        (video && !protocol::cover_rate_carries_video(rate)).then(|| format!("can't video call {label}: cover rate {} is too low for video (needs {} or more)", format_bitrate(rate), format_bitrate(swarm_core::video::MIN_COVER_FOR_VIDEO)))
    }

    /// Rings `device`, another of yours, alone (video, or voice only), in
    /// the background, over the link between them; the answer comes back
    /// as a call event (see `on_event`).
    pub fn place_device(&mut self, node: &NodeHandle, device: &DeviceInfo, video: bool) -> Note {
        if let Some(why) = self.why_not_call_device(node, device, video) {
            return Note::info(why);
        }
        let (name, shown) = (devices::call_name(&device.key), crate::devices::in_calls(device));
        let (node2, call_events, ringing, peer_name) = (node.clone(), node.call_events(), name.clone(), shown.clone());
        let task = tokio::spawn(async move {
            let event = match ring(&node2, &ringing, &Ring::Call { video }).await {
                Ok(Some((link, video))) => CallEvent::Accepted { peer_name: ringing, link, video },
                Ok(None) => CallEvent::Declined { peer_name },
                Err(e) => CallEvent::Failed { peer_name, reason: format!("{e:#}") },
            };
            let _ = call_events.send(event);
        });
        self.outgoing = Some((shown.clone(), vec![name], task));
        Note::info(format!("{} {shown}...", if video { "video calling" } else { "calling" }))
    }

    /// Rings `row` (video, or voice only) in the background; the answer
    /// comes back as a call event (see `on_event`).
    pub fn place(&mut self, node: &NodeHandle, row: &PeerRow, video: bool) -> Note {
        if self.busy() {
            return Note::info("one call at a time");
        }
        if let Some(why) = self.why_not_call(node, row, video) {
            return Note::info(why);
        }
        let mut devices: Vec<String> = row.entries.iter().filter(|n| node.peer_status(n).is_some_and(|s| s.online)).cloned().collect();
        if devices.first().is_some_and(|n| node.calls_go_through(n)) {
            devices.truncate(1);
        }
        let (node2, call_events, peer_name, ringing) = (node.clone(), node.call_events(), row.handle.clone(), devices.clone());
        let task = tokio::spawn(async move {
            let event = match ring_all(&node2, &ringing, Ring::Call { video }).await {
                Ok(Some((device, link, video))) => CallEvent::Accepted { peer_name: device, link, video },
                Ok(None) => CallEvent::Declined { peer_name },
                Err(e) => CallEvent::Failed { peer_name, reason: format!("{e:#}") },
            };
            let _ = call_events.send(event);
        });
        self.outgoing = Some((row.handle.clone(), devices, task));
        Note::info(format!("{} {}...", if video { "video calling" } else { "calling" }, row.label))
    }

    /// Calls a group (`name`, as shown): rings every one of `rows`, the
    /// contacts in it, at once, on every device of theirs that's online.
    /// The first to answer starts a conference with this side; the rest
    /// go on ringing into it, and join as they answer. Already in a call:
    /// those of them not in it are rung into it, as Add to call does.
    /// Anyone in it who isn't a contact can't be rung from here (they're
    /// reached through someone in the call who knows both, once there).
    pub fn call_group(&mut self, node: &NodeHandle, sidebar: &Sidebar, name: &str, rows: &[&PeerRow]) -> Note {
        if self.active.is_some() {
            let addable: Vec<&PeerRow> = rows.iter().copied().filter(|row| self.why_not_add(node, row).is_none()).collect();
            if addable.is_empty() {
                return Note::info(format!("nobody in {name} to ring into the call: they're in it, or not online"));
            }
            for row in &addable {
                self.add(node, sidebar, row);
            }
            return Note::info(format!("ringing {} into the call...", addable.iter().map(|r| r.label.as_str()).collect::<Vec<_>>().join(", ")));
        }
        if self.busy() {
            return Note::info("one call at a time");
        }
        let callable: Vec<&PeerRow> = rows.iter().copied().filter(|row| self.why_not_call(node, row, false).is_none()).collect();
        if callable.is_empty() {
            return Note::info(format!("nobody in {name} can be called now: your contacts in it aren't online"));
        }
        let identity = identity_for(node, &callable[0].handle);
        let roster = Roster::start(&identity);
        let mut rings = Vec::new();
        for row in &callable {
            let mut devices: Vec<String> = row.entries.iter().filter(|n| node.peer_status(n).is_some_and(|s| s.online) && has_link(node, n)).cloned().collect();
            if devices.first().is_some_and(|n| node.calls_go_through(n)) {
                devices.truncate(1);
            }
            let mut theirs = roster.clone();
            theirs.add(&identity, row.public_key);
            let invites: Vec<(String, Roster)> = devices.iter().map(|d| (d.clone(), theirs.clone())).collect();
            let (node2, call_events, shown, ringing) = (node.clone(), node.call_events(), row.handle.clone(), devices.clone());
            let task = tokio::spawn(async move {
                let event = match ring_all(&node2, &ringing, Ring::Into(invites)).await {
                    Ok(Some((device, link, _))) => CallEvent::Accepted { peer_name: device, link, video: false },
                    Ok(None) => CallEvent::Declined { peer_name: shown },
                    Err(e) => CallEvent::Failed { peer_name: shown, reason: format!("{e:#}") },
                };
                let _ = call_events.send(event);
            });
            rings.push((row.handle.clone(), devices, task));
        }
        self.group_ring = Some(GroupRing { name: name.to_string(), roster, rings });
        Note::info(format!("calling {name}..."))
    }

    /// One rung by a group's call, before anyone's answered, has said no
    /// or couldn't be rung: their ring's over, and with nobody left
    /// ringing, the call is.
    fn group_ring_over(&mut self, peer_name: &str, failed: Option<&str>) -> Option<Note> {
        let ring = self.group_ring.as_mut()?;
        let at = ring.rings.iter().position(|(name, _, _)| name == peer_name)?;
        ring.rings.remove(at);
        if !ring.rings.is_empty() {
            return None;
        }
        let name = self.group_ring.take().expect("checked").name;
        Some(match failed {
            Some(reason) => Note::error(format!("calling {name} failed: {reason}")),
            None => Note::warning(format!("nobody in {name} answered")),
        })
    }

    /// Rings `row` into the live call, making it a conference (if it
    /// isn't one yet). Everyone in it who's their peer links to them
    /// once they answer, sending them pictures too if their camera's on.
    pub fn add(&mut self, node: &NodeHandle, sidebar: &Sidebar, row: &PeerRow) -> Note {
        if let Some(why) = self.why_not_add(node, row) {
            return Note::info(why);
        }
        let active = self.active.as_mut().expect("checked above");
        let mut devices: Vec<(String, [u8; 32])> = row.entries.iter().filter(|n| node.peer_status(n).is_some_and(|s| s.online) && has_link(node, n)).map(|n| (n.clone(), row.public_key)).collect();
        if devices.first().is_some_and(|(n, _)| node.calls_go_through(n)) {
            devices.truncate(1);
        }
        let identity = identity_for(node, &row.handle);
        if active.conference.is_none() {
            let other = active.links.first().map(|l| l.key).unwrap_or_default();
            let mut roster = if active.answered { Roster::start_answered(&identity, other) } else { Roster::start(&identity) };
            roster.add(&identity, other);
            active.conference = Some(Conference::new(roster, identity.public_key_bytes(), sidebar));
            active.tell_all();
            active.refresh();
        }
        let conference = active.conference.as_mut().expect("just made");
        let invites: Vec<(String, Roster)> = devices
            .iter()
            .map(|(name, key)| {
                let mut roster = conference.roster.clone();
                roster.add(&identity, *key);
                (name.clone(), roster)
            })
            .collect();
        let names: Vec<String> = devices.iter().map(|(n, _)| n.clone()).collect();
        let (node2, call_events, shown) = (node.clone(), node.call_events(), row.handle.clone());
        let task = tokio::spawn(async move {
            let event = match ring_all(&node2, &names, Ring::Into(invites)).await {
                Ok(Some((device, link, _))) => CallEvent::Accepted { peer_name: device, link, video: false },
                Ok(None) => CallEvent::Declined { peer_name: shown },
                Err(e) => CallEvent::Failed { peer_name: shown, reason: format!("{e:#}") },
            };
            let _ = call_events.send(event);
        });
        conference.adding.push((row.handle.clone(), devices.into_iter().map(|(n, _)| n).collect(), task));
        Note::info(format!("ringing {} into the call...", row.label))
    }

    /// Removes `key` from the conference: only someone we outrank (see
    /// `conference`). Everyone's told, them included, and our link to
    /// them ends; everyone else's ends as the news reaches them.
    pub fn remove(&mut self, node: &NodeHandle, key: [u8; 32]) -> Note {
        let Some(active) = self.active.as_mut() else { return Note::info("removing someone needs a call") };
        let identity = identity_for(node, active.links.first().map(|l| l.name.as_str()).unwrap_or_default());
        let Some(conference) = active.conference.as_mut() else { return Note::info("removing someone needs a conference call") };
        let shown = conference.shown(&key);
        if !conference.roster.remove(&identity, key) {
            return Note::info(format!("you can't remove {shown}: only someone who came into the call before them can"));
        }
        active.tell_all();
        active.drop_non_members();
        if active.links.is_empty() {
            self.end_active();
            return Note::info(format!("removed {shown}: the call ended, as there's no one left in it you can hear"));
        }
        active.refresh();
        Note::info(format!("removed {shown} from the call"))
    }

    /// Takes in a call event from the node: a ring (answered straight
    /// away for an auto-answer peer), or the outcome of a call placed here
    /// or one under way.
    pub fn on_event(&mut self, node: &NodeHandle, sidebar: &Sidebar, event: CallEvent) -> Option<Note> {
        match event {
            CallEvent::Incoming { from, link, conference: Some(roster), at, .. } => self.on_conference_ring(node, sidebar, from, link, roster, at),
            CallEvent::LinkAsked { from, link, roster } => {
                if self.active.is_none() {
                    let _ = link.reject();
                    return None;
                }
                self.on_conference_ring(node, sidebar, from, link, roster, 0)
            }
            CallEvent::Incoming { from, link, video, conference: None, at } => {
                if self.busy() {
                    let _ = link.reject();
                    return None;
                }
                let answer = match swarm_core::node::devices::call_device(&from) {
                    Some(key) => crate::devices::auto_answer(node, &key),
                    None => sidebar.peer_by_name(&from).map(|r| r.auto_answer).unwrap_or_default(),
                };
                let shown = shown_name(sidebar, &from);
                if let Some(with_video) = answer.answers(video) {
                    let note = self.answer_link(node, sidebar, from, link, with_video, None);
                    return Some(if self.active.is_some() { Note::info(format!("answered {shown} automatically")) } else { note });
                }
                let what = if video { "a video call" } else { "a call" };
                let note = Note::warning(format!("{shown} is calling — {what}"));
                self.incoming = Some(Incoming { from: shown, name: from, video, deadline: Instant::now() + call::RING_TIMEOUT, with: Vec::new(), conference: None, link, at });
                Some(note)
            }
            CallEvent::Accepted { peer_name, link, video } => {
                if self.outgoing.as_ref().is_some_and(|(_, devices, _)| devices.contains(&peer_name)) {
                    self.outgoing = None;
                    return Some(self.go_live(node, sidebar, peer_name, link, video, None, false));
                }
                if self.group_ring.as_ref().is_some_and(|g| g.rings.iter().any(|(_, devices, _)| devices.contains(&peer_name))) {
                    let mut ring = self.group_ring.take().expect("checked");
                    ring.rings.retain(|(_, devices, _)| !devices.contains(&peer_name));
                    let mut roster = ring.roster;
                    if let Some(key) = key_of(sidebar, &peer_name) {
                        roster.add(&identity_for(node, &peer_name), key);
                    }
                    let note = self.go_live(node, sidebar, peer_name, link, false, Some(roster), false);
                    match self.active.as_mut().and_then(|a| a.conference.as_mut()) {
                        Some(conference) => conference.adding = ring.rings,
                        None => {
                            for (_, devices, task) in ring.rings {
                                task.abort();
                                hang_up_rings(node, &devices);
                            }
                        }
                    }
                    return Some(note);
                }
                let Some(conference) = self.active.as_mut().and_then(|a| a.conference.as_mut()) else {
                    let _ = link.hang_up();
                    return None;
                };
                let added = conference.adding.iter().any(|(_, devices, _)| devices.contains(&peer_name));
                let asked = conference.asking.contains(&peer_name);
                if !added && !asked {
                    let _ = link.hang_up();
                    return None;
                }
                conference.done_asking(sidebar, &peer_name);
                if added {
                    conference.adding.retain(|(_, devices, _)| !devices.contains(&peer_name));
                    if let Some(key) = key_of(sidebar, &peer_name) {
                        conference.roster.add(&identity_for(node, &peer_name), key);
                        conference.remember(sidebar, key);
                    }
                }
                self.add_link(node, sidebar, peer_name.clone(), link);
                let active = self.active.as_mut()?;
                active.tell_all();
                let shown = shown_name(sidebar, &peer_name);
                Some(Note::success(if added { format!("{shown} joined the call") } else { format!("now hearing {shown}") }))
            }
            CallEvent::Declined { peer_name } => {
                if self.outgoing.as_ref().is_some_and(|(name, _, _)| *name == peer_name) {
                    self.outgoing = None;
                    return Some(Note::warning(format!("{} didn't answer", shown_name(sidebar, &peer_name))));
                }
                if let Some(note) = self.group_ring_over(&peer_name, None) {
                    return Some(note);
                }
                let conference = self.active.as_mut()?.conference.as_mut()?;
                conference.done_asking(sidebar, &peer_name);
                if let Some(at) = conference.adding.iter().position(|(name, _, _)| *name == peer_name) {
                    conference.adding.remove(at);
                    return Some(Note::warning(format!("{} didn't answer", shown_name(sidebar, &peer_name))));
                }
                None
            }
            CallEvent::Failed { peer_name, reason } => {
                if self.outgoing.as_ref().is_some_and(|(name, _, _)| *name == peer_name) {
                    self.outgoing = None;
                    return Some(Note::error(format!("call to {shown} failed: {reason}", shown = shown_name(sidebar, &peer_name))));
                }
                if let Some(note) = self.group_ring_over(&peer_name, Some(&reason)) {
                    return Some(note);
                }
                let conference = self.active.as_mut()?.conference.as_mut()?;
                conference.done_asking(sidebar, &peer_name);
                if let Some(at) = conference.adding.iter().position(|(name, _, _)| *name == peer_name) {
                    conference.adding.remove(at);
                    return Some(Note::error(format!("couldn't add {shown}: {reason}", shown = shown_name(sidebar, &peer_name))));
                }
                None
            }
            CallEvent::Ended { peer_name } => {
                let active = self.active.as_mut()?;
                let at = active.link_named(&peer_name)?;
                let link = active.links.remove(at);
                active.forget_link(&link);
                if active.links.is_empty() {
                    let note = match &active.conference {
                        Some(c) if c.roster.members().iter().any(|k| *k != c.me && *k != link.key) => format!("{} left, and there's no one else in the call you can hear", link.shown),
                        Some(_) => "the call ended".to_string(),
                        None => format!("call with {} ended", link.shown),
                    };
                    self.end_active();
                    return Some(Note::info(note));
                }
                let conference = active.conference.as_mut()?;
                let left = conference.roster.has_left(&link.key);
                if let Some(by) = conference.roster.removed_by(&link.key) {
                    let by = conference.shown(&by);
                    active.refresh();
                    return Some(Note::info(format!("{by} removed {} from the call", link.shown)));
                }
                if !left {
                    conference.lost.push((link.key, conference.roster.round(&link.key).unwrap_or(0)));
                }
                active.refresh();
                Some(Note::info(if left { format!("{} left the call", link.shown) } else { format!("lost {} from the call", link.shown) }))
            }
            CallEvent::Roster { peer_name, roster } => self.on_roster(node, sidebar, &peer_name, roster),
            CallEvent::TheirVideo { peer_name, via } => {
                let active = self.active.as_ref()?;
                let link = &active.links[active.link_named(&peer_name)?];
                if active.video.is_some() {
                    return None;
                }
                let sender = via.and_then(|id| active.conference.as_ref()?.heard_through.iter().find(|(_, (by, i))| *by == link.key && *i == id).map(|(from, _)| *from));
                let shown = match sender {
                    Some(from) => active.name_of(&from),
                    None => shown_name(sidebar, &peer_name),
                };
                self.start_video_now(node, sidebar, false).or_else(|| Some(Note::info(format!("{shown} turned their camera on"))))
            }
            CallEvent::Conference { peer_name, signal } => {
                let active = self.active.as_mut()?;
                match signal {
                    CallSignal::Hearing(keys) => {
                        let key = active.links[active.link_named(&peer_name)?].key;
                        active.conference.as_mut()?.hearing.insert(key, keys);
                        active.refresh();
                    }
                    CallSignal::Forward { from, id, on } => active.forward(&peer_name, from, id, on),
                    _ => {}
                }
                None
            }
            CallEvent::Control { peer_name, signal } => {
                let active = self.active.as_mut()?;
                let key = active.links[active.link_named(&peer_name)?].key;
                let name = active.name_of(&key);
                let sharing = active.sharing_size();
                let asks = matches!(signal, CallSignal::Control(swarm_core::remote::Control::Ask));
                let note = active.control.on_signal(key, &name, signal, sharing);
                let unasked = asks && active.control.has_asked(key) && swarm_core::settings::auto_control(&node.paths().settings(), &key);
                if unasked {
                    return self.allow_control(node, sidebar, key, true);
                }
                note
            }
        }
    }

    /// A ring into a conference: the link from a member of ours (taken
    /// without asking), or someone asking us into theirs (rung, like any
    /// call, if we're free).
    fn on_conference_ring(&mut self, node: &NodeHandle, sidebar: &Sidebar, from: String, link: CallLink, roster: Roster, at: u64) -> Option<Note> {
        let from_key = key_of(sidebar, &from);
        if let Some(active) = self.active.as_mut() {
            let ours = active.conference.as_mut().filter(|c| c.roster.id == roster.id);
            let Some(conference) = ours else {
                let _ = link.reject();
                return None;
            };
            let news = conference.roster.merge(&roster);
            let member = from_key.is_some_and(|k| conference.roster.is_member(&k));
            let cover_too_slow = sidebar.peer_by_name(&from).is_some_and(|row| voice_blocked_by_cover(node, row).is_some());
            if !member || cover_too_slow || active.links.iter().any(|l| Some(l.key) == from_key) {
                let _ = link.reject();
                return None;
            }
            conference.done_asking(sidebar, &from);
            if let Some(key) = from_key {
                conference.remember(sidebar, key);
            }
            if link.accept(false).is_err() {
                return None;
            }
            self.add_link(node, sidebar, from.clone(), link);
            let active = self.active.as_mut()?;
            if news {
                active.tell_all();
            }
            return Some(Note::success(format!("now hearing {}", shown_name(sidebar, &from))));
        }
        if self.busy() {
            let _ = link.reject();
            return None;
        }
        let me = identity_for(node, &from).public_key_bytes();
        if !roster.is_member(&me) || !from_key.is_some_and(|k| roster.is_member(&k)) {
            let _ = link.reject();
            return None;
        }
        let with: Vec<String> = roster.members().iter().filter(|k| **k != me && Some(**k) != from_key).map(|k| name_for(sidebar, k)).collect();
        let row = sidebar.peer_by_name(&from);
        if row.and_then(|r| r.auto_answer.answers(false)).is_some() {
            let note = self.answer_link(node, sidebar, from.clone(), link, false, Some(roster));
            return Some(if self.active.is_some() { Note::info(format!("answered {from} automatically")) } else { note });
        }
        let shown = shown_name(sidebar, &from);
        let note = if with.is_empty() { Note::warning(format!("{shown} is calling — a group call")) } else { Note::warning(format!("{shown} is calling — a conference with {}", with.join(", "))) };
        self.incoming = Some(Incoming { from: shown, name: from, video: false, deadline: Instant::now() + call::RING_TIMEOUT, with, conference: Some(roster), link, at });
        Some(note)
    }

    /// Answers the ringing call — with video if they offered it and
    /// `video` is set.
    pub fn answer(&mut self, node: &NodeHandle, sidebar: &Sidebar, video: bool) -> Option<Note> {
        let Incoming { name, link, video: offered, conference, .. } = self.incoming.take()?;
        Some(self.answer_link(node, sidebar, name, link, video && offered, conference))
    }

    /// Refuses the ringing call.
    pub fn reject(&mut self) -> Option<Note> {
        let incoming = self.incoming.take()?;
        let _ = incoming.link.reject();
        Some(Note::info(format!("declined the call from {}", incoming.from)))
    }

    /// Ends the live call (leaving it, if it's a conference), or gives up
    /// on one still ringing.
    pub fn hang_up(&mut self, node: &NodeHandle) -> Option<Note> {
        if let Some(active) = self.active.as_mut() {
            let name = active.peer_name.clone();
            let identity = identity_for(node, active.links.first().map(|l| l.name.as_str()).unwrap_or_default());
            if let Some(conference) = active.conference.as_mut() {
                conference.roster.leave(&identity);
                for (_, devices, task) in conference.adding.drain(..) {
                    task.abort();
                    hang_up_rings(node, &devices);
                }
                active.tell_all();
                self.end_active();
                return Some(Note::info("left the call"));
            }
            self.end_active();
            return Some(Note::info(format!("call with {name} ended")));
        }
        if let Some(ring) = self.group_ring.take() {
            for (_, devices, task) in ring.rings {
                hang_up_rings(node, &devices);
                task.abort();
            }
            return Some(Note::info(format!("cancelled the call to {}", ring.name)));
        }
        let (peer_name, devices, task) = self.outgoing.take()?;
        hang_up_rings(node, &devices);
        task.abort();
        Some(Note::info(format!("cancelled the call to {}", node.label_of(&peer_name))))
    }

    /// Checks on a ringing call: the caller giving up, or the ring
    /// running out of time (refused, as if declined); and in a
    /// conference, asks for the links still missing. Call it now and
    /// then — a fraction of a second late doesn't matter here.
    pub fn tick(&mut self, node: &NodeHandle, sidebar: &Sidebar) -> Option<Note> {
        self.ask_for_links(node, sidebar);
        let slow = match &self.preview {
            Some(Ok(camera)) => self.preview_pace.too_slow(camera),
            _ => false,
        };
        if slow {
            self.close_preview();
            self.open_preview();
        }
        if let Some(active) = self.active.as_mut() {
            if active.sharing_size().is_none() {
                active.control.sharing_ended();
            }
            let video = active.video.as_ref();
            if let Some(note) = active.control.tick(|key| video.and_then(|v| v.remote(&key)).is_some_and(Remote::their_screen)) {
                return Some(note);
            }
            if let Some(video) = active.video.as_mut() {
                if video.camera_stopped() {
                    return Some(Note::error("your camera stopped (unplugged, or it failed), so they see your picture instead; turn it on again once it's back"));
                }
                video.fit_camera();
                match video.screen_opened() {
                    Some(Ok(())) => {
                        if let Some(key) = active.pending_allow.take() {
                            let name = active.name_of(&key);
                            let said = active.control.allow(key, active.sharing_size().is_some());
                            if active.control.controller().is_some_and(|(k, _)| k == key) {
                                return Some(said.unwrap_or_else(|| Note::warning(format!("sharing your screen with {name}, who is controlling it"))));
                            }
                            return said.or_else(|| Some(sharing_note(&active.peer_name)));
                        }
                        return Some(sharing_note(&active.peer_name));
                    }
                    Some(Err(why)) => {
                        if let Some(key) = active.pending_allow.take() {
                            active.control.refuse_because(key, &format!("their screen couldn't be shared: {why}"));
                        }
                        return Some(Note::error(format!("couldn't share your screen: {why}")));
                    }
                    None => {}
                }
                if video.screen.as_ref().and_then(Screen::ended).is_some() {
                    video.stop_sharing();
                    return Some(Note::info("your desktop stopped sharing your screen"));
                }
            }
        }
        let incoming = self.incoming.as_mut()?;
        if incoming.link.caller_gave_up() {
            let incoming = self.incoming.take()?;
            if incoming.link.answered_elsewhere() {
                return Some(Note::info(format!("the call from {} was answered on another of your devices", incoming.from)));
            }
            incoming.note_missed(node);
            return Some(Note::info(format!("missed call from {} — they hung up", incoming.from)));
        }
        if Instant::now() < incoming.deadline {
            return None;
        }
        let incoming = self.incoming.take()?;
        let _ = incoming.link.reject();
        incoming.note_missed(node);
        Some(Note::info(format!("missed call from {}", incoming.from)))
    }
    /// Mutes the microphone in the live call, or unmutes it.
    pub fn toggle_mute(&mut self) {
        if let Some(active) = self.active.as_ref() {
            active.audio.set_muted(!active.audio.muted());
        }
    }

    /// Turns the camera on or off (on in place of the screen, if that's
    /// being shared). In a voice call, on makes it a video call.
    pub fn toggle_camera(&mut self, node: &NodeHandle, sidebar: &Sidebar) -> Option<Note> {
        let Some(video) = self.active.as_mut().and_then(|c| c.video.as_mut()) else {
            return self.start_video_now(node, sidebar, true);
        };
        if video.camera.is_some() {
            video.camera_off();
        } else {
            video.camera_on();
        }
        None
    }

    /// Video in a voice call already going, on every link: our camera on
    /// (`camera`), or theirs to be shown with ours off. Cover that can't
    /// carry video on any link keeps the camera off (each link's cover is
    /// a hard limit; one that can't carry it sends nothing).
    fn start_video_now(&mut self, node: &NodeHandle, sidebar: &Sidebar, camera: bool) -> Option<Note> {
        let active = self.active.as_ref()?;
        if active.video.is_some() {
            return None;
        }
        if camera {
            let blocked = active.links.iter().find_map(|l| sidebar.peer_by_name(&l.name).and_then(|row| video_blocked_by_cover(node, row)).map(|why| (l.shown.clone(), why)));
            if let Some((who, reason)) = blocked {
                return Some(Note::error(format!("can't add video with {who}: {reason}")));
            }
        }
        self.close_preview();
        let mut call_video = CallVideo::start(camera);
        let active = self.active.as_mut()?;
        for link in &active.links {
            let _ = link.video.send(call_video.link(link.key));
        }
        for (from, list) in active.relays.outgoing_video.locked().iter_mut() {
            let stats = call_video.remote(from).map(|r| r.stats.clone());
            for passing in list.iter_mut() {
                passing.from_stats = stats.clone();
            }
            if let Some(stats) = stats {
                ask_for_all(&stats, list);
            }
        }
        active.video = Some(call_video);
        active.refresh();
        None
    }

    /// Uses the camera at `index` of `camera::choices` in the call (and
    /// in Settings, so the next call starts with it too), turning it on if
    /// it's off: the one on is closed first, as a phone can't have two
    /// going at once, then the new one opened at its size.
    pub fn use_camera(&mut self, node: &NodeHandle, sidebar: &Sidebar, index: usize) -> Option<Note> {
        let _ = camera::choose_at(&crate::paths().settings(), index);
        let on = self.active.as_mut().and_then(|c| c.video.as_mut()).filter(|v| v.camera.is_some());
        match on {
            Some(video) => {
                let size = video.camera.as_ref().map(|c| c.asked())?;
                video.close_camera();
                video.camera_on_at(size);
                None
            }
            None => self.toggle_camera(node, sidebar),
        }
    }

    /// Starts or stops sharing the screen, in a video call. Says what
    /// happened, or why it couldn't.
    /// Asks to control the screen shown large, or — asking or
    /// controlling already — gives it back.
    pub fn toggle_control(&mut self) -> Option<Note> {
        let active = self.active.as_mut()?;
        if active.control.give_back() {
            return Some(Note::info("you've stopped controlling their screen"));
        }
        let Some(key) = active.askable() else {
            return Some(Note::info("there's no shared screen here to control"));
        };
        active.control.ask(key);
        let name = active.name_of(&key);
        Some(Note::info(if active.controllable().is_some() { format!("asked {name} to let you control their screen...") } else { format!("asked {name} to share their screen and let you control it...") }))
    }

    /// Answers whoever's first asked to control our screen.
    pub fn answer_control(&mut self, node: &NodeHandle, sidebar: &Sidebar, allow: bool) -> Option<Note> {
        let active = self.active.as_mut()?;
        let key = active.control.asking()?;
        let name = active.name_of(&key);
        if !allow {
            active.control.refuse(key);
            return Some(Note::info(format!("refused {name} control of your screen")));
        }
        self.allow_control(node, sidebar, key, false)
    }

    /// Lets `key`, who's asked, control our screen: shared first if it
    /// isn't (video added to a voice call for it), and allowed once it
    /// is — after the desktop's dialog, if there's one (see `tick`).
    /// `unasked`: allowed by their setting, not by Allow pressed.
    fn allow_control(&mut self, node: &NodeHandle, sidebar: &Sidebar, key: [u8; 32], unasked: bool) -> Option<Note> {
        let name = self.active.as_ref()?.name_of(&key);
        let mut shared_now = false;
        if self.active.as_ref()?.sharing_size().is_none() {
            let shared = self.video_for_screen(node, sidebar).and_then(|()| self.active.as_mut().and_then(|a| a.video.as_mut()).ok_or_else(|| "no video".to_string())?.share_screen());
            let active = self.active.as_mut()?;
            match shared {
                Ok(true) => shared_now = true,
                Ok(false) => {
                    active.pending_allow = Some(key);
                    return Some(Note::info(format!("choose what to share with {name} in your desktop's dialog...")));
                }
                Err(why) => {
                    active.control.refuse_because(key, &format!("their screen couldn't be shared: {why}"));
                    return Some(Note::error(format!("couldn't share your screen with {name}: {why}")));
                }
            }
        }
        let active = self.active.as_mut()?;
        let sharing = active.sharing_size().is_some();
        let said = active.control.allow(key, sharing);
        if active.control.controller().is_some_and(|(k, _)| k == key) {
            let how = if unasked { " (allowed without asking)" } else { "" };
            let what = if shared_now { format!("sharing your screen with {name}, who is controlling it{how}") } else { format!("{name} is controlling your screen{how}") };
            return Some(match said {
                Some(note) => Note { text: format!("{what} — {}", note.text), ..note },
                None => Note::warning(what),
            });
        }
        said
    }

    /// Video for sharing the screen in a call that has none yet (a voice
    /// call), with no camera; or why it can't have it.
    fn video_for_screen(&mut self, node: &NodeHandle, sidebar: &Sidebar) -> std::result::Result<(), String> {
        let active = self.active.as_ref().ok_or("there's no call")?;
        if active.video.is_some() {
            return Ok(());
        }
        if active.in_conference() {
            return Err("a conference's video is started by whoever began it".to_string());
        }
        if let Some((who, why)) = active.links.iter().find_map(|l| sidebar.peer_by_name(&l.name).and_then(|row| video_blocked_by_cover(node, row)).map(|why| (l.shown.clone(), why))) {
            return Err(format!("can't add video with {who}: {why}"));
        }
        self.start_video_now(node, sidebar, false);
        Ok(())
    }

    /// Stops whoever's controlling our screen.
    pub fn stop_control(&mut self) -> Option<Note> {
        let active = self.active.as_mut()?;
        let name = active.control.controller().map(|(key, _)| active.name_of(&key))?;
        active.control.stop();
        Some(Note::info(format!("{name} is no longer controlling your screen")))
    }

    /// While we control their screen: where `control` is.
    pub fn control(&mut self) -> Option<&mut RemoteControl> {
        self.active.as_mut().map(|a| &mut a.control).filter(|c| c.controlling().is_some())
    }

    /// Shares the screen, or stops sharing it. In a voice call, video's
    /// added for it (with the camera off).
    pub fn toggle_screen(&mut self, node: &NodeHandle, sidebar: &Sidebar) -> Option<Note> {
        if let Err(why) = self.video_for_screen(node, sidebar) {
            return Some(Note::info(format!("can't share your screen: {why}")));
        }
        let active = self.active.as_mut()?;
        let video = active.video.as_mut()?;
        if video.screen.is_some() || video.asking_for_screen() {
            let was_asking = video.screen.is_none();
            video.stop_sharing();
            active.control.sharing_ended();
            if let Some(key) = active.pending_allow.take() {
                active.control.refuse_because(key, "their screen wasn't shared after all");
            }
            return Some(Note::info(if was_asking { "not sharing your screen after all" } else { "stopped sharing your screen" }));
        }
        Some(match video.share_screen() {
            Ok(true) => sharing_note(&active.peer_name),
            Ok(false) => Note::info("choose what to share in your desktop's dialog..."),
            Err(why) => Note::error(format!("couldn't share your screen: {why}")),
        })
    }

    /// The camera preview: the camera, or why it wouldn't open.
    pub fn preview(&self) -> Option<&std::result::Result<Camera, String>> {
        self.preview.as_ref()
    }

    /// Settings' microphone meter: the microphone's listened to while
    /// `wanted`, unless a call has it, and let go otherwise. One that
    /// won't open isn't tried again till it's wanted afresh.
    pub fn keep_listening(&mut self, wanted: bool) {
        if !wanted || self.active.is_some() {
            self.stop_listening();
            return;
        }
        if self.listening.is_some() {
            return;
        }
        let (mic_tx, mut mic_rx) = mpsc::unbounded_channel::<Vec<f32>>();
        self.listening = Some(match tokio::task::block_in_place(|| crate::audio::listen(mic_tx)) {
            Ok(audio) => {
                let level: Arc<std::sync::Mutex<Meter>> = Arc::default();
                let metered = level.clone();
                tokio::spawn(async move {
                    while let Some(pcm) = mic_rx.recv().await {
                        metered.locked().hear(&pcm, Instant::now());
                    }
                });
                Ok(Listening { _audio: audio, level })
            }
            Err(e) => Err(format!("{e:#}")),
        });
    }

    fn stop_listening(&mut self) {
        if self.listening.is_none() {
            return;
        }
        let listening = self.listening.take();
        tokio::task::block_in_place(|| drop(listening));
    }

    /// The microphone's level for Settings' meter, 0 to 1: a call's, as
    /// sent, or what's listened to (see `keep_listening`); or why it
    /// can't be had. None while neither.
    pub fn microphone_level(&self) -> Option<std::result::Result<f32, &str>> {
        if let Some(call) = &self.active {
            return Some(Ok(call.own_level()));
        }
        match self.listening.as_ref()? {
            Ok(listening) => Some(Ok(listening.level.locked().shown(Instant::now()))),
            Err(why) => Some(Err(why)),
        }
    }

    /// Opens the camera preview (not during a video call, which has it).
    pub fn open_preview(&mut self) {
        if self.active.as_ref().is_some_and(|c| c.video.is_some()) {
            return;
        }
        self.preview_pace = camera::PaceCheck::default();
        self.preview = Some(tokio::task::block_in_place(Camera::open).map_err(|e| format!("{e:#}")));
    }

    /// `open_preview`, with the camera a QR code's read with (the back
    /// one, on a phone).
    pub fn open_reader(&mut self) {
        if self.active.as_ref().is_some_and(|c| c.video.is_some()) {
            return;
        }
        self.preview_pace = camera::PaceCheck::default();
        self.preview = Some(tokio::task::block_in_place(Camera::open_for_reading).map_err(|e| format!("{e:#}")));
    }

    pub fn close_preview(&mut self) {
        let preview = self.preview.take();
        tokio::task::block_in_place(|| drop(preview));
    }

    /// Opens audio *before* saying yes on the wire: if it can't be
    /// opened, they're refused instead — they see "declined", not a call
    /// that goes dead the instant it connects.
    fn answer_link(&mut self, node: &NodeHandle, sidebar: &Sidebar, from: String, link: CallLink, mut video: bool, conference: Option<Roster>) -> Note {
        let row = sidebar.peer_by_name(&from);
        if let Some(reason) = row.and_then(|r| voice_blocked_by_cover(node, r)) {
            let _ = link.reject();
            return Note::error(format!("couldn't answer {}: {reason}", shown_name(sidebar, &from)));
        }
        let video_refused = if video { row.and_then(|r| video_blocked_by_cover(node, r)) } else { None };
        if video_refused.is_some() {
            video = false;
        }
        let note = self.go_live(node, sidebar, from.clone(), link, video, conference, true);
        match (video_refused, self.active.is_some()) {
            (Some(reason), true) => Note::warning(format!("call with {} live, voice only: {reason}", shown_name(sidebar, &from))),
            _ => note,
        }
    }

    /// A call's live: placed from here and answered (`answering` false), or
    /// answered here. Audio's opened first: if it won't open, a call
    /// placed from here is ended cleanly, so their side sees it end rather
    /// than hang, and one being answered is refused instead.
    #[allow(clippy::too_many_arguments)]
    fn go_live(&mut self, node: &NodeHandle, sidebar: &Sidebar, peer_name: String, link: CallLink, video: bool, conference: Option<Roster>, answering: bool) -> Note {
        self.stop_listening();
        let (mic_tx, mut mic_rx) = mpsc::unbounded_channel::<Vec<f32>>();
        let (_, speaker_rx) = mpsc::unbounded_channel();
        let audio = match tokio::task::block_in_place(|| crate::audio::start(mic_tx, speaker_rx)) {
            Ok(audio) => audio,
            Err(e) => {
                if answering {
                    let _ = link.reject();
                    return Note::error(format!("couldn't answer {}: {e:#}", shown_name(sidebar, &peer_name)));
                }
                let _ = link.hang_up();
                return Note::error(format!("call with {}: {e:#}", shown_name(sidebar, &peer_name)));
            }
        };
        if answering {
            if let Err(e) = link.accept(video) {
                return Note::error(format!("couldn't answer {}: {e:#}", shown_name(sidebar, &peer_name)));
            }
        }
        let mics: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<Vec<f32>>>>> = Arc::default();
        let to_links = mics.clone();
        let own: Arc<std::sync::Mutex<Meter>> = Arc::default();
        let metered = own.clone();
        tokio::spawn(async move {
            while let Some(pcm) = mic_rx.recv().await {
                metered.locked().hear(&pcm, Instant::now());
                to_links.locked().retain(|link| link.send(pcm.clone()).is_ok());
            }
        });
        let whom = if conference.is_none() { key_of(sidebar, &peer_name) } else { None };
        let call_video = self.start_video(video, whom);
        let note = live_note(&shown_name(sidebar, &peer_name), &audio);
        let me = identity_for(node, &peer_name).public_key_bytes();
        let conference = conference.map(|roster| Conference::new(roster, me, sidebar));
        let control_links: Arc<std::sync::Mutex<std::collections::HashMap<[u8; 32], mpsc::UnboundedSender<CallCommand>>>> = Arc::default();
        let to_links = control_links.clone();
        let control = RemoteControl::new(
            Arc::new(move |key, signal| {
                if let Some(link) = to_links.locked().get(&key) {
                    let _ = link.send(CallCommand::Signal(signal));
                }
            }),
            Arc::new(crate::remote::open_injector),
        );
        self.active = Some(Active { peer_name: shown_name(sidebar, &peer_name), started_at: Instant::now(), video: call_video, audio, links: Vec::new(), mics, conference, relays: Relays::default(), answered: answering, traffic: Default::default(), talking: Arc::default(), shown: Default::default(), own, control, pending_allow: None, control_links });
        self.add_link(node, sidebar, peer_name, link);
        self.ask_for_links(node, sidebar);
        note
    }

    /// Starts the call on `link`, to `name`, alongside any others.
    fn add_link(&mut self, node: &NodeHandle, sidebar: &Sidebar, name: String, mut link: CallLink) {
        let Some(active) = self.active.as_mut() else { return };
        let (mic_tx, mic_rx) = mpsc::unbounded_channel();
        active.mics.locked().push(mic_tx);
        let key = key_of(sidebar, &name).unwrap_or_default();
        let video = active.video.as_mut().map(|v| v.link(key));
        let (relayed_tx, mut relayed_rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
        link.relay_to(relayed_tx);
        let (pictures_tx, mut pictures_rx) = mpsc::unbounded_channel::<CallSignal>();
        link.pictures_to(pictures_tx);
        let video_out = active.relays.outgoing_video.clone();
        tokio::spawn(async move {
            while let Some(signal) = pictures_rx.recv().await {
                if let Some(list) = video_out.locked().get(&key) {
                    for to in list {
                        let _ = to.to_link.send(CallCommand::Signal(CallSignal::Via { id: to.id, signal: Box::new(signal.clone()) }));
                    }
                }
            }
        });
        let (vias_tx, mut vias_rx) = mpsc::unbounded_channel::<(u8, CallSignal)>();
        link.vias_to(vias_tx);
        let (incoming_video, video_out) = (active.relays.incoming_video.clone(), active.relays.outgoing_video.clone());
        let (call_events, link_name) = (node.call_events(), name.clone());
        tokio::spawn(async move {
            let mut said = false;
            while let Some((id, signal)) = vias_rx.recv().await {
                match signal {
                    CallSignal::Video(_) | CallSignal::CameraOff | CallSignal::Screen(_) => {
                        let to_show = incoming_video.locked().get(&id).cloned();
                        match to_show {
                            Some(to_show) => {
                                let _ = to_show.send(signal);
                            }
                            None if matches!(signal, CallSignal::Video(_)) && !said => {
                                said = true;
                                let _ = call_events.send(CallEvent::TheirVideo { peer_name: link_name.clone(), via: Some(id) });
                            }
                            None => {}
                        }
                    }
                    asked => {
                        let mut video_out = video_out.locked();
                        let Some(list) = video_out.values_mut().find(|list| list.iter().any(|p| p.to == key && p.id == id)) else { continue };
                        let Some(this) = list.iter_mut().find(|p| p.to == key && p.id == id) else { continue };
                        match asked {
                            CallSignal::KeyframeRequest { small } => {
                                let _ = this.from_link.send(CallCommand::Signal(CallSignal::KeyframeRequest { small }));
                            }
                            CallSignal::ShownIn { width, height, small_width, small_height } => this.shown_in = ((width, height), (small_width, small_height)),
                            CallSignal::Decodes { h265 } => this.no_h265 = !h265,
                            _ => {}
                        }
                        if let Some(stats) = list.first().and_then(|p| p.from_stats.clone()) {
                            ask_for_all(&stats, list);
                        }
                    }
                }
            }
        });
        let incoming = active.relays.incoming.clone();
        tokio::spawn(async move {
            while let Some((id, opus)) = relayed_rx.recv().await {
                let decoder = incoming.locked().get(&id).cloned();
                if let Some(decoder) = decoder {
                    let _ = decoder.send(opus);
                }
            }
        });
        let (heard_tx, mut heard_rx) = mpsc::unbounded_channel::<Vec<f32>>();
        let (speaker, outgoing, talking) = (active.audio.speaker(), active.relays.outgoing.clone(), active.talking.clone());
        tokio::spawn(async move {
            while let Some(pcm) = heard_rx.recv().await {
                talking.locked().hear(key, &pcm, Instant::now());
                if let Some(list) = outgoing.locked().get_mut(&key) {
                    list.retain(|f| f.pcm.send(pcm.clone()).is_ok());
                }
                if speaker.send(pcm).is_err() {
                    break;
                }
            }
        });
        let (video_tx, video_rx) = mpsc::unbounded_channel();
        let (theirs_tx, mut theirs_rx) = mpsc::unbounded_channel();
        link.video_later(video_rx, theirs_tx);
        let (call_events, peer_name) = (node.call_events(), name.clone());
        tokio::spawn(async move {
            if theirs_rx.recv().await.is_some() {
                let _ = call_events.send(CallEvent::TheirVideo { peer_name, via: None });
            }
        });
        let (commands, budget) = (link.commands(), link.budget());
        active.control_links.locked().insert(key, commands.clone());
        let hangup = spawn_call(node, name.clone(), link, mic_rx, heard_tx, video);
        let through = through_device(node, &key);
        active.links.push(Link { shown: shown_name(sidebar, &name), name, key, hangup, commands, budget, video: video_tx, through });
        active.update_name();
        active.refresh();
    }

    /// What a member's told us of the conference's roster: taken in, and
    /// passed on if it's news. A two-person call the other side's made a
    /// conference becomes one here too.
    fn on_roster(&mut self, node: &NodeHandle, sidebar: &Sidebar, from: &str, roster: Roster) -> Option<Note> {
        let me = identity_for(node, from).public_key_bytes();
        let active = self.active.as_mut()?;
        let from_key = active.links[active.link_named(from)?].key;
        let before = active.conference.as_ref().map(|c| c.roster.members()).unwrap_or_default();
        match active.conference.as_mut() {
            Some(conference) if conference.roster.id == roster.id => {
                if !conference.roster.merge(&roster) {
                    return None;
                }
            }
            None if roster.is_member(&me) && roster.is_member(&from_key) => {
                active.conference = Some(Conference::new(roster, me, sidebar));
            }
            _ => return None,
        }
        active.tell_all();
        let conference = active.conference.as_mut()?;
        if let Some(by) = conference.roster.removed_by(&me) {
            let by = conference.shown(&by);
            self.end_active();
            return Some(Note::warning(format!("{by} removed you from the call")));
        }
        active.drop_non_members();
        if active.links.is_empty() {
            self.end_active();
            return Some(Note::info("the call ended: there's no one left in it you can hear"));
        }
        let conference = active.conference.as_mut()?;
        let now = conference.roster.members();
        for key in &now {
            conference.remember(sidebar, *key);
        }
        let joined: Vec<String> = now.iter().filter(|k| !before.contains(k) && **k != me && **k != from_key && !before.is_empty()).map(|k| conference.shown(k)).collect();
        active.refresh();
        self.ask_for_links(node, sidebar);
        let conference = self.active.as_ref()?.conference.as_ref()?;
        let removed = before.iter().filter_map(|k| Some((conference.shown(k), conference.shown(&conference.roster.removed_by(k)?))));
        let mut notes: Vec<String> = removed.map(|(who, by)| format!("{by} removed {who} from the call")).collect();
        if !joined.is_empty() {
            notes.push(format!("{} joined the call", joined.join(", ")));
        }
        (!notes.is_empty()).then(|| Note::info(notes.join("; ")))
    }

    /// In a conference, asks each member who's our peer, and whose link
    /// we're the one to ask for (see `conference::asks`), for it.
    fn ask_for_links(&mut self, node: &NodeHandle, sidebar: &Sidebar) {
        let Some(active) = self.active.as_mut() else { return };
        let Some(conference) = active.conference.as_mut() else { return };
        let me = conference.me;
        for key in conference.roster.members() {
            let round = conference.roster.round(&key).unwrap_or(0);
            if key == me || !conference::asks(&me, &key) || active.links.iter().any(|l| l.key == key) || conference.lost.contains(&(key, round)) {
                continue;
            }
            let Some(row) = sidebar.peer_by_key(&key) else { continue };
            let name = row.handle.clone();
            if voice_blocked_by_cover(node, row).is_some() {
                continue;
            }
            if conference.asking.contains(&name) || conference.asked.get(&key).is_some_and(|at| at.elapsed() < ASK_AGAIN) {
                continue;
            }
            let mut devices: Vec<String> = row.entries.iter().filter(|n| node.peer_status(n).is_some_and(|s| s.online) && has_link(node, n)).cloned().collect();
            if devices.first().is_some_and(|n| node.calls_go_through(n)) {
                devices.truncate(1);
            }
            if devices.is_empty() {
                continue;
            }
            conference.asked.insert(key, Instant::now());
            conference.asking.push(name.clone());
            conference.asking.extend(devices.iter().filter(|d| **d != name).cloned());
            let (node2, roster, call_events) = (node.clone(), conference.roster.clone(), node.call_events());
            tokio::spawn(async move {
                let event = match ring_all(&node2, &devices, Ring::Link(roster)).await {
                    Ok(Some((device, link, _))) => CallEvent::Accepted { peer_name: device, link, video: false },
                    Ok(None) => CallEvent::Declined { peer_name: name },
                    Err(e) => CallEvent::Failed { peer_name: name, reason: format!("{e:#}") },
                };
                let _ = call_events.send(event);
            });
        }
    }

    /// For a video call: closes the preview (a camera can only be open
    /// once), turns the camera on, and wires up the pictures both ways.
    /// Without a camera the call still shows the other side.
    fn start_video(&mut self, video: bool, whom: Option<[u8; 32]>) -> Option<CallVideo> {
        if !video {
            return None;
        }
        self.close_preview();
        Some(CallVideo::start_for(true, whom))
    }

    fn end_active(&mut self) {
        let Some(mut call) = self.active.take() else { return };
        for link in call.links.drain(..) {
            link.budget.relayed.store(0, std::sync::atomic::Ordering::Relaxed);
            let _ = link.hangup.send(());
        }
        if let Some(video) = &mut call.video {
            video.camera_off();
        }
        let audio = call.audio;
        tokio::task::block_in_place(|| audio.stop());
    }
}

impl Conference {
    fn new(roster: Roster, me: [u8; 32], sidebar: &Sidebar) -> Conference {
        let mut conference = Conference {
            roster,
            me,
            names: Default::default(),
            asking: Vec::new(),
            asked: Default::default(),
            lost: Vec::new(),
            adding: Vec::new(),
            hearing: Default::default(),
            announced: Vec::new(),
            heard_through: Default::default(),
            next_id: 0,
        };
        for key in conference.roster.members() {
            conference.remember(sidebar, key);
        }
        conference
    }

    /// Notes what `key` is called (a peer's name doesn't change during a
    /// call; anyone else's is looked up once).
    fn remember(&mut self, sidebar: &Sidebar, key: [u8; 32]) {
        if self.names.contains_key(&key) {
            return;
        }
        let named = match sidebar.peer_by_key(&key) {
            Some(row) => Named::Peer(row.label.clone()),
            None => Named::Other(crate::names::published_or_key(&key)),
        };
        self.names.insert(key, named);
    }

    /// Asking `name` (a person or any of their devices) for our link is
    /// over: all of theirs come off the list.
    fn done_asking(&mut self, sidebar: &Sidebar, name: &str) {
        match key_of(sidebar, name) {
            Some(key) => self.asking.retain(|n| *n != name && key_of(sidebar, n) != Some(key)),
            None => self.asking.retain(|n| *n != name),
        }
    }

    fn shown(&self, key: &[u8; 32]) -> String {
        match self.names.get(key) {
            Some(Named::Peer(name) | Named::Other(name)) => name.clone(),
            None => crate::names::published_or_key(key),
        }
    }
}

/// A peer entry's key, or another of your devices' (named by
/// `devices::call_name`).
fn key_of(sidebar: &Sidebar, name: &str) -> Option<[u8; 32]> {
    devices::call_device(name).or_else(|| sidebar.peer_by_name(name).map(|row| row.public_key))
}

/// What a peer entry's shown as: the person's name (one person may be
/// several entries, one for each device); another of your devices, as
/// `devices::in_calls` has it.
fn shown_name(sidebar: &Sidebar, name: &str) -> String {
    if let Some(key) = devices::call_device(name) {
        return sidebar.device_by_key(&key).map_or_else(|| "another of your devices".to_string(), crate::devices::in_calls);
    }
    sidebar.peer_by_name(name).map(|row| row.label.clone()).unwrap_or_else(|| name.to_string())
}

/// What someone in a conference is called: our name for a peer, else
/// theirs.
fn name_for(sidebar: &Sidebar, key: &[u8; 32]) -> String {
    sidebar.peer_by_key(key).map(|row| row.label.clone()).unwrap_or_else(|| crate::names::published_or_key(key))
}

/// Tells every device rung that the ring's over.
/// The contacts in group `id` (not you): who calling it rings.
pub fn group_contacts<'a>(node: &NodeHandle, sidebar: &'a Sidebar, id: &swarm_core::channel::ChannelId) -> Vec<&'a PeerRow> {
    let me = node.identity().public_key_bytes();
    let roster = swarm_core::channel::replay(id, &swarm_core::channel::load_roster(&crate::paths().channels(), id).unwrap_or_default());
    let mut keys: Vec<[u8; 32]> = roster.members.keys().copied().filter(|k| *k != me).collect();
    keys.sort();
    keys.iter().filter_map(|k| sidebar.peer_by_key(k)).collect()
}

fn hang_up_rings(node: &NodeHandle, devices: &[String]) {
    for device in devices {
        if let Some(handle) = node.call_handle(device) {
            let _ = handle.commands.send(CallCommand::Signal(CallSignal::Hangup));
        }
    }
}

/// "call with X live", plus a note when only one audio device opened.
fn live_note(peer_name: &str, audio: &AudioHandle) -> Note {
    match audio.one_way_note() {
        Some(note) => Note::success(format!("call with {peer_name} live — {note}")),
        None => Note::success(format!("call with {peer_name} live")),
    }
}

/// Runs the call in the background and, once it ends for *any* reason
/// (either side hanging up, a device failing, the network), reports
/// `CallEvent::Ended` — so an app never shows a call that's over.
fn spawn_call(
    node: &NodeHandle,
    peer_name: String,
    link: CallLink,
    mic_rx: mpsc::UnboundedReceiver<Vec<f32>>,
    speaker_tx: mpsc::UnboundedSender<Vec<f32>>,
    video: Option<call::VideoIo>,
) -> oneshot::Sender<()> {
    let (hangup_tx, hangup_rx) = oneshot::channel();
    let call_events = node.call_events();
    tokio::spawn(async move {
        let _ = call::run(link, mic_rx, speaker_tx, hangup_rx, video).await;
        let _ = call_events.send(CallEvent::Ended { peer_name });
    });
    hangup_tx
}

/// What a ring asks: a call (with video or not), or into a conference,
/// with the roster each device is rung with (by peer name).
enum Ring {
    Call { video: bool },
    Into(Vec<(String, Roster)>),
    /// A member asking for the link between us in this conference.
    Link(Roster),
}

/// The identity a call with peer `name` is ours under.
fn identity_for(node: &NodeHandle, _name: &str) -> Arc<swarm_core::identity::Identity> {
    node.identity().clone()
}

/// Rings every one of `devices` (one person's) at once. The first to
/// answer has the call; the rest are told it was answered elsewhere, and
/// one that answers at the same moment is hung up on. `Ok(None)`: all of
/// them refused, or nobody answered. `Ok(Some)` says which answered.
async fn ring_all(node: &NodeHandle, devices: &[String], how: Ring) -> Result<Option<(String, CallLink, bool)>> {
    let how = Arc::new(how);
    if devices.len() < 2 {
        let device = devices.first().cloned().unwrap_or_default();
        return Ok(ring(node, &device, &how).await?.map(|(link, video)| (device, link, video)));
    }
    let (tx, mut answers) = mpsc::unbounded_channel();
    let mut ringing = Vec::new();
    for device in devices {
        let (node, device, tx, how) = (node.clone(), device.clone(), tx.clone(), how.clone());
        ringing.push(tokio::spawn(async move {
            let answer = ring(&node, &device, &how).await;
            let _ = tx.send((device, answer));
        }));
    }
    drop(tx);
    let mut last_error = None;
    while let Some((device, answer)) = answers.recv().await {
        match answer {
            Ok(Some((link, video))) => {
                for other in devices.iter().filter(|d| **d != device) {
                    if let Some(handle) = node.call_handle(other) {
                        let _ = handle.commands.send(CallCommand::Signal(CallSignal::AnsweredElsewhere));
                    }
                }
                for task in &ringing {
                    task.abort();
                }
                tokio::spawn(async move {
                    while let Some((_, late)) = answers.recv().await {
                        if let Ok(Some((link, _))) = late {
                            let _ = link.hang_up();
                        }
                    }
                });
                return Ok(Some((device, link, video)));
            }
            Ok(None) => {}
            Err(e) => last_error = Some(e),
        }
    }
    match last_error {
        Some(e) if devices.iter().all(|d| node.call_handle(d).is_none()) => Err(e),
        _ => Ok(None),
    }
}

/// Attaches to the peer's connection (calls ride it — see
/// `swarm_core::call`) and rings. `Ok(None)`: refused, or nobody answered.
async fn ring(node: &NodeHandle, peer_name: &str, how: &Ring) -> Result<Option<(CallLink, bool)>> {
    let online = node.peer_status(peer_name).is_some_and(|s| s.online);
    let (true, Some(handle)) = (online, node.call_handle(peer_name)) else {
        anyhow::bail!("not connected to {peer_name}");
    };
    let mut link = CallLink::attach(handle.commands, handle.budget)?;
    match how {
        Ring::Call { video } => Ok(call::ring(&mut link, *video).await.context("ringing")?.map(|video| (link, video))),
        Ring::Into(rosters) => {
            let roster = rosters.iter().find(|(n, _)| n == peer_name).map(|(_, r)| r.clone()).context("no roster for them")?;
            Ok(call::ring_into(&mut link, roster).await.context("ringing")?.then_some((link, false)))
        }
        Ring::Link(roster) => Ok(call::ask_link(&mut link, roster.clone()).await.context("asking for the link")?.then_some((link, false))),
    }
}
/// Whether connection `name` has a link a call can go on.
fn has_link(node: &NodeHandle, name: &str) -> bool {
    node.call_handle(name).is_some()
}

/// The cover rate this connection sends at now, or will start at.
fn cover_rate(node: &NodeHandle, row: &PeerRow) -> Option<u64> {
    match node.peer_status(&row.handle).and_then(|s| s.cover) {
        Some(live) => live.sends,
        None => row.cover_bits_per_second.or(node.default_cover()),
    }
}

/// Why a call can't happen under this connection's cover rate, if it
/// can't: voice has to fit inside the slots, and below about 18kbit/s
/// there isn't room for even rough speech.
pub fn voice_blocked_by_cover(node: &NodeHandle, row: &PeerRow) -> Option<String> {
    let rate = cover_rate(node, row)?;
    (!protocol::cover_rate_carries_voice(rate)).then(|| format!("cover rate {} is too low to carry voice (needs about 18k or more)", format_bitrate(rate)))
}

/// Why a video call can't happen under this connection's cover rate.
pub fn video_blocked_by_cover(node: &NodeHandle, row: &PeerRow) -> Option<String> {
    let rate = cover_rate(node, row)?;
    (!protocol::cover_rate_carries_video(rate)).then(|| {
        format!("cover rate {} is too low for video (needs {} or more)", format_bitrate(rate), format_bitrate(swarm_core::video::MIN_COVER_FOR_VIDEO))
    })
}

/// Said once a screen's being shared.
fn sharing_note(peer_name: &str) -> Note {
    Note::warning(format!("sharing your screen with {peer_name} — everything on it"))
}

/// Whom a file sent from a call goes to, of `others` in it: the person
/// whose chat is open (any of `open`, their devices' keys), if they're in
/// it; else whoever's shown large; else the first.
fn file_goes_to(others: &[[u8; 32]], open: &[[u8; 32]], shown: Option<[u8; 32]>) -> Option<[u8; 32]> {
    let open = others.iter().find(|key| open.contains(key)).copied();
    open.or(shown.filter(|key| others.contains(key))).or(others.first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::video::Shape;

    /// A file sent from a call goes to whoever's chat is open, if they're
    /// in it; else to whoever's shown large; else to the first other.
    #[test]
    fn a_file_sent_from_a_call_goes_to_someone_in_it() {
        let (a, b, c, d) = ([1; 32], [2; 32], [3; 32], [4; 32]);
        assert_eq!(file_goes_to(&[a, b], &[b], Some(a)), Some(b), "the chat open");
        assert_eq!(file_goes_to(&[a, b], &[d, b], None), Some(b), "any of the open one's devices");
        assert_eq!(file_goes_to(&[a, b], &[c], Some(b)), Some(b), "not in the call: whoever's large");
        assert_eq!(file_goes_to(&[a, b], &[], None), Some(a), "no chat, no video: the first");
        assert_eq!(file_goes_to(&[], &[c], None), None, "no one in the call");
    }

    /// Someone reached through another of your devices is shown so in
    /// the call bar; anyone else by name alone.
    #[test]
    fn someone_reached_through_another_of_your_devices_is_shown_so() {
        assert_eq!(named_through("Tim", Some("desktop")), "Tim (through your device desktop)");
        assert_eq!(named_through("Rupert", None), "Rupert");
    }

    /// A round trip under a millisecond (a link on one network) isn't
    /// shown as none.
    #[test]
    fn a_short_round_trip_is_shown_to_a_tenth_of_a_millisecond() {
        assert_eq!(round_trip_text(400), "0.4 ms");
        assert_eq!(round_trip_text(9_960), "10.0 ms");
        assert_eq!(round_trip_text(23_400), "23 ms");
    }

    /// A call's traffic is shown a second at a time, in bits; a link that
    /// joins (a conference) counts from then, not with all it carried in
    /// calls before.
    #[test]
    fn a_calls_traffic_is_shown_per_second() {
        let (a, b) = ([1; 32], [2; 32]);
        let start = Instant::now();
        let mut rate = Rate::default();
        assert_eq!(rate.sample(start, &[(a, 1_000, 2_000)]), None, "nothing to go by yet");
        assert_eq!(rate.sample(start + Duration::from_millis(400), &[(a, 50_000, 2_000)]), None, "not a second yet");
        let second = start + Duration::from_secs(1);
        assert_eq!(rate.sample(second, &[(a, 126_000, 64_500)]), Some((1_000_000, 500_000)));
        let two = start + Duration::from_secs(2);
        assert_eq!(rate.sample(two, &[(a, 251_000, 127_000), (b, 9_000_000, 9_000_000)]), Some((1_000_000, 500_000)));
        let three = start + Duration::from_secs(3);
        assert_eq!(rate.sample(three, &[(a, 376_000, 189_500), (b, 9_125_000, 9_062_500)]), Some((2_000_000, 1_000_000)));
        assert_eq!(rate.sample(three + Duration::from_millis(500), &[(a, 400_000, 200_000)]), Some((2_000_000, 1_000_000)));
    }

    #[test]
    fn a_calls_traffic_reads_well() {
        assert_eq!(traffic_text((1_234_000, 850_000)), "↑ 1.2 Mbit/s  ↓ 850 kbit/s");
        assert_eq!(traffic_text((0, 64_000)), "↑ 0 kbit/s  ↓ 64 kbit/s");
    }

    /// A member of a conference asking a device that isn't in a call for
    /// the link between them (it's another of the person's devices, not
    /// the one in the conference): turned down at once, and never rung.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_link_asked_of_a_device_not_in_the_conference_is_turned_down_without_ringing() {
        let dir = std::env::temp_dir().join(format!("swarm-link-asked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = swarm_core::node::NodeConfig { paths: swarm_core::node::Paths::new(&dir), port: 0, cover_bits_per_second: None, log: Arc::new(|_| {}) };
        let (node, _call_events) = swarm_core::node::start(Arc::new(swarm_core::identity::Identity::generate()), config).await.unwrap();
        let carol = swarm_core::identity::Identity::generate();
        let mut roster = Roster::start(&carol);
        roster.add(&carol, node.identity().public_key_bytes());
        let (commands, mut sent) = mpsc::unbounded_channel();
        let (_inbound_tx, inbound) = mpsc::unbounded_channel();
        let link = CallLink::new(commands, inbound, call::CallBudget::new());
        let mut calls = Calls::default();
        let note = calls.on_event(&node, &Sidebar::default(), CallEvent::LinkAsked { from: "carol".to_string(), link, roster });
        assert!(note.is_none(), "nothing said");
        assert!(calls.incoming().is_none(), "not rung");
        assert!(matches!(sent.try_recv(), Ok(CallCommand::Signal(CallSignal::Reject))), "turned down");
        node.shutdown().await;
    }

    /// A video call's picture side, and what it sends to the call.
    /// A video call with one link, and what's sent on it.
    /// The size a one-to-one call settles at is the next one's with
    /// them, from the start; a conference's isn't kept.
    #[test]
    fn a_call_starts_at_the_size_the_last_one_with_them_settled_at() {
        let whom = [42; 32];
        let mut video = CallVideo::start_for(false, Some(whom));
        assert_eq!(video.settled, None, "nothing known yet");
        video.link(whom);
        video.settled = Some((1280, 720));
        video.remember_settled();
        assert_eq!(CallVideo::start_for(false, Some(whom)).settled, Some((1280, 720)));
        assert_eq!(CallVideo::start_for(false, Some([43; 32])).settled, None, "someone else's");
        let mut conference = CallVideo::start_for(false, Some([44; 32]));
        conference.link([44; 32]);
        conference.link([45; 32]);
        conference.settled = Some((640, 360));
        conference.remember_settled();
        assert_eq!(CallVideo::start_for(false, Some([44; 32])).settled, None);
    }

    fn video_and_sent() -> (CallVideo, watch::Receiver<Option<Arc<Picture>>>) {
        let (to_call, _) = watch::channel(None);
        let mut video = CallVideo {
            camera: None,
            camera_note: None,
            screen: None,
            opening_screen: None,
            camera_before_sharing: false,
            remotes: Vec::new(),
            to_call,
            camera_feed: None,
            other_size: None,
            pace: camera::PaceCheck::default(),
            most_wanted: None,
            settled: None,
            peak: None,
            said: None,
            for_whom: None,
        };
        let sent = video.link([1; 32]).camera;
        (video, sent)
    }

    fn picture(width: usize) -> Arc<Picture> {
        Arc::new(Picture::rgb(width, 2, vec![0; width * 2 * 3]))
    }

    /// Our pictures go to every link, each encoding them for itself; each
    /// link's pictures from the other side are its own; and a link that's
    /// gone takes its pictures with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn ours_go_on_every_link_and_each_others_are_kept_apart() {
        let (mut video, to_bob) = video_and_sent();
        let to_carol = video.link([2; 32]);
        video.to_call.send_replace(Some(picture(64)));
        assert_eq!(to_bob.borrow().as_ref().map(|p| p.width), Some(64));
        assert_eq!(to_carol.camera.borrow().as_ref().map(|p| p.width), Some(64));
        to_carol.display.send_replace(Some(picture(32)));
        assert_eq!(video.remote(&[2; 32]).and_then(|r| r.picture.borrow().as_ref().map(|p| p.width)), Some(32));
        assert!(video.remote(&[1; 32]).is_some_and(|r| r.picture.borrow().is_none()));
        video.unlink(&[2; 32]);
        assert!(video.remote(&[2; 32]).is_none());
        assert_eq!(video.remotes.len(), 1);
    }

    /// In a call with pictures from two or more, everyone not shown
    /// large is shown small, beside them, in the order their links came,
    /// and then us (`None`); as the one shown large changes, they swap.
    /// With one other, nobody is: the call of two is as it was.
    #[tokio::test(flavor = "multi_thread")]
    async fn everyone_not_shown_large_is_in_the_strip_and_us_last() {
        let (mut video, _) = video_and_sent();
        assert_eq!(video.strip(Some([1; 32])), vec![], "a call of two");
        let _to_carol = video.link([2; 32]);
        let _to_dave = video.link([3; 32]);
        assert_eq!(video.strip(Some([1; 32])), vec![Some([2; 32]), Some([3; 32]), None]);
        assert_eq!(video.strip(Some([2; 32])), vec![Some([1; 32]), Some([3; 32]), None]);
        video.unlink(&[3; 32]);
        assert_eq!(video.strip(Some([2; 32])), vec![Some([1; 32]), None]);
    }

    /// Someone seen through another member (their pictures passed on)
    /// is shown as anyone linked directly is: their pictures their own,
    /// large or in the strip; and gone when they're no longer passed on.
    #[tokio::test(flavor = "multi_thread")]
    async fn someone_seen_through_another_is_shown_as_any_other() {
        let (mut video, _) = video_and_sent();
        let (display, _small, stats) = video.relayed([5; 32]);
        display.send_replace(Some(picture(48)));
        assert_eq!(video.remote(&[5; 32]).and_then(|r| r.picture.borrow().as_ref().map(|p| p.width)), Some(48));
        assert!(Arc::ptr_eq(&video.remote(&[5; 32]).unwrap().stats, &stats));
        assert_eq!(video.strip(Some([1; 32])), vec![Some([5; 32]), None]);
        video.unlink(&[5; 32]);
        assert!(video.remote(&[5; 32]).is_none());
    }

    /// Everyone's told the box their pictures are drawn in, not only
    /// whoever's shown now: those who weren't told sent theirs as big as
    /// the link carried (to a terminal, 4K), to be decoded for nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn everyone_is_told_the_box_their_pictures_go_in() {
        let (mut video, _) = video_and_sent();
        for key in [[1; 32], [2; 32], [3; 32]] {
            video.link(key);
        }
        video.show_in((120, 64), (0, 0));
        for remote in &video.remotes {
            assert_eq!(remote.stats.shown_large_in(), (120, 64));
        }
    }

    /// A new speaker's shown large once their large pictures are there —
    /// the last one kept till then — or, should none come, a second on,
    /// their small blown up. The large pictures decoded are the one
    /// shown's, the speaker's and one more talking's; the rest, not.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_speaker_is_shown_large_once_their_large_pictures_are_there() {
        let (mut video, _) = video_and_sent();
        let (bob, carol, dave) = ([1; 32], [2; 32], [3; 32]);
        let ios: Vec<_> = [bob, carol, dave].iter().map(|key| video.link(*key)).collect();
        for io in &ios {
            io.small.send_replace(Some(picture(16)));
        }
        ios[0].display.send_replace(Some(picture(64)));
        let large_off = |video: &CallVideo, key: [u8; 32]| video.remote(&key).unwrap().stats.large_off.load(std::sync::atomic::Ordering::Relaxed);
        let mut shown = Shown::default();
        let start = Instant::now();
        let nobody = |_: &[u8; 32]| false;
        assert_eq!(video.shown_large(&mut shown, Some(bob), &nobody, start).map(|r| r.key), Some(bob));
        assert!(!large_off(&video, bob));
        assert!(large_off(&video, carol) && large_off(&video, dave), "nobody else's large decoded");
        let dave_talks = |key: &[u8; 32]| *key == dave;
        assert_eq!(video.shown_large(&mut shown, Some(bob), &dave_talks, start).map(|r| r.key), Some(bob));
        assert!(!large_off(&video, dave) && large_off(&video, carol));
        let at = start + Duration::from_millis(100);
        assert_eq!(video.shown_large(&mut shown, Some(carol), &nobody, at).map(|r| r.key), Some(bob));
        assert!(!large_off(&video, carol) && !large_off(&video, bob));
        assert_eq!(video.shown_large(&mut shown, Some(carol), &nobody, at + Duration::from_millis(500)).map(|r| r.key), Some(bob));
        ios[1].display.send_replace(Some(picture(64)));
        assert_eq!(video.shown_large(&mut shown, Some(carol), &nobody, at + Duration::from_millis(600)).map(|r| r.key), Some(carol));
        assert!(large_off(&video, bob), "Bob's large no longer decoded");
        let at = at + Duration::from_secs(2);
        assert_eq!(video.shown_large(&mut shown, Some(dave), &nobody, at).map(|r| r.key), Some(carol));
        assert_eq!(video.shown_large(&mut shown, Some(dave), &nobody, at + Duration::from_millis(900)).map(|r| r.key), Some(carol));
        assert_eq!(video.shown_large(&mut shown, Some(dave), &nobody, at + SHOWN_UNREADY_AFTER).map(|r| r.key), Some(dave));
        let dave_remote = video.remote(&dave).unwrap();
        assert!(std::ptr::eq(dave_remote.large_view(), &dave_remote.small), "his small, blown up");
    }

    /// The camera's sized for the link wanting the biggest pictures.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_camera_is_sized_for_the_link_wanting_most() {
        let (mut video, _) = video_and_sent();
        let _to_carol = video.link([2; 32]);
        video.remotes[0].stats.set_sending(shape(640, 360), shape(640, 360));
        video.remotes[1].stats.set_sending(shape(1920, 1080), shape(1920, 1080));
        assert!(Arc::ptr_eq(video.lead().unwrap(), &video.remotes[1].stats));
    }

    /// Sharing the screen sends it in the camera's place, says it's a
    /// screen, and shows it unmirrored; stopping brings the camera back,
    /// as it was on before.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_screen_is_shared_in_the_cameras_place_and_the_camera_comes_back() {
        std::env::set_var("SWARM_CAMERA", "test");
        std::env::set_var("SWARM_SCREEN", "test");
        let (mut video, mut sent) = video_and_sent();
        video.camera_on();
        let camera_width = video.camera.as_ref().unwrap().info().width as usize;
        assert_eq!(video.share_screen(), Ok(true), "a screen that's no dialog away is shared at once");
        assert!(video.camera.is_none(), "the camera's closed meanwhile");
        let _to_carol = video.link([2; 32]);
        assert!(video.remotes.iter().all(|r| r.stats.screen.load(std::sync::atomic::Ordering::Relaxed)), "said on every link, one made while sharing too");
        let screen = sent.wait_for(|p| p.as_ref().is_some_and(|p| p.width == 2134)).await.unwrap().clone().unwrap();
        let (shown, mirrored) = video.local_picture().unwrap();
        assert_eq!(shown.width, screen.width);
        assert!(!mirrored, "a screen isn't shown mirrored");
        video.stop_sharing();
        assert!(video.screen.is_none());
        assert!(video.remotes.iter().all(|r| !r.stats.screen.load(std::sync::atomic::Ordering::Relaxed)));
        assert!(video.camera.is_some(), "the camera's back on");
        sent.wait_for(|p| p.as_ref().is_some_and(|p| p.width == camera_width)).await.unwrap();
        assert!(video.local_picture().unwrap().1, "the camera's shown mirrored");
        video.camera.as_mut().unwrap().face_away();
        assert!(!video.local_picture().unwrap().1, "a camera facing away isn't shown mirrored");
        video.camera_off();
        video.share_screen().unwrap();
        video.stop_sharing();
        assert!(video.camera.is_none());
        sent.wait_for(Option::is_none).await.unwrap();
    }

    /// A camera that stops on its own (taken off USB) is turned off, as if
    /// the user had: the call's sent no picture, which the other side
    /// shows as ours, never the last frame frozen.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_camera_that_stops_on_its_own_is_turned_off() {
        std::env::set_var("SWARM_CAMERA", "test");
        let (mut video, mut sent) = video_and_sent();
        video.camera_on();
        sent.wait_for(Option::is_some).await.unwrap();
        assert!(!video.camera_stopped(), "a working camera hasn't stopped");
        video.camera.as_mut().unwrap().unplug();
        assert!(video.camera_stopped(), "said the once");
        assert!(video.camera.is_none(), "it's off");
        assert_eq!(video.camera_note.as_deref(), Some("camera stopped"));
        sent.wait_for(Option::is_none).await.unwrap();
        assert!(!video.camera_stopped(), "not said again");
        video.camera_on();
        assert!(video.camera.is_some(), "and can be turned on again");
    }

    /// Counts the times the call's left without a picture from now on
    /// (which the other side would take for the camera turned off, and
    /// show nothing).
    fn count_gaps(sent: &watch::Receiver<Option<Arc<Picture>>>) -> (Arc<std::sync::atomic::AtomicUsize>, tokio::task::JoinHandle<()>) {
        let gaps = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (mut sent, counted) = (sent.clone(), gaps.clone());
        let watching = tokio::spawn(async move {
            while sent.changed().await.is_ok() {
                if sent.borrow_and_update().is_none() {
                    counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
        (gaps, watching)
    }

    fn offer(video: &CallVideo, bitrate: u64, wanted: Shape) {
        video.remotes[0].stats.offered.store(bitrate, std::sync::atomic::Ordering::Relaxed);
        video.remotes[0].stats.set_sending(wanted, wanted);
    }

    fn shape(width: usize, height: usize) -> Shape {
        Shape { width, height, fps: 25 }
    }

    /// A call opens the camera at the largest size and keeps it there
    /// while the link's offering more and more, however long that takes —
    /// so growing to the largest never reopens it. Once the link's stopped
    /// rising short of the largest, it's reopened at what the call wants,
    /// once; and bigger, a size at a time, should the call want that
    /// after all. The call's never left without a picture.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_camera_starts_at_the_largest_and_goes_smaller_only_once_the_call_settles_below() {
        std::env::set_var("SWARM_CAMERA", "test");
        let (mut video, mut sent) = video_and_sent();
        video.camera_on();
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some(camera::LARGEST));
        sent.wait_for(Option::is_some).await.unwrap();
        let (gaps, watching) = count_gaps(&sent);
        let usual = shape(1920, 1080);
        for step in 0..(2 * SETTLED.as_millis() / 250) as u64 {
            offer(&video, 500_000 + step * 20_000, usual);
            video.fit_camera();
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some(camera::LARGEST), "not while the link's still rising");
        tokio::time::sleep(SETTLED).await;
        video.fit_camera();
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some((1920, 1080)));
        assert!(video.local_picture().is_some(), "the preview went blank");
        offer(&video, 7_000_000, shape(2560, 1440));
        video.fit_camera();
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some((1920, 1080)), "not for a passing change");
        tokio::time::sleep(RESIZE_AFTER).await;
        video.fit_camera();
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some((2560, 1440)));
        offer(&video, 1_000_000, shape(1280, 720));
        tokio::time::sleep(SETTLED).await;
        video.fit_camera();
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some((2560, 1440)));
        offer(&video, 14_000_000, Shape::LARGEST);
        video.fit_camera();
        tokio::time::sleep(RESIZE_AFTER).await;
        video.fit_camera();
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some(camera::LARGEST));
        assert_eq!(camera::best_size(), camera::LARGEST, "the test pattern keeps pace");
        tokio::time::sleep(Duration::from_millis(300)).await;
        watching.abort();
        assert_eq!(gaps.load(std::sync::atomic::Ordering::Relaxed), 0, "the call was left without a picture");
        video.camera_off();
    }

    /// A call that's wanted the largest and dropped to less keeps the
    /// camera at the largest for a passing drop (reopening freezes the
    /// picture), but a lasting one reopens it at what's sent — a camera
    /// at 4K as JPEGs then gives 1440p uncompressed, say — and it grows
    /// again should the link.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_that_drops_for_long_reopens_the_camera_at_what_it_sends() {
        std::env::set_var("SWARM_CAMERA", "test");
        let (mut video, _sent) = video_and_sent();
        video.camera_on();
        offer(&video, 14_000_000, Shape::LARGEST);
        video.fit_camera();
        offer(&video, 7_000_000, shape(2560, 1440));
        video.fit_camera();
        tokio::time::sleep(RESIZE_AFTER).await;
        video.fit_camera();
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some(camera::LARGEST), "not for a passing drop");
        tokio::time::sleep(DROPPED).await;
        video.fit_camera();
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some((2560, 1440)));
        offer(&video, 14_000_000, Shape::LARGEST);
        video.fit_camera();
        tokio::time::sleep(RESIZE_AFTER).await;
        video.fit_camera();
        assert_eq!(video.camera.as_ref().map(|c| c.asked()), Some(camera::LARGEST));
        video.camera_off();
    }
}
