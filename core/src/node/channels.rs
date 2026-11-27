//! Groups, live: what every connection does for the groups both of its
//! ends are in (`channel` has the logs themselves).
//!
//! - Posts and roster entries go from member to member as they happen
//!   (`Notice::ChannelPost`/`ChannelEvent`). Whoever receives one that's
//!   new to them stores it and passes it on to every other member they're
//!   connected to. Each is signed by whoever wrote it, so a copy passed on
//!   by anyone is as good as the original, and a repeat goes no further.
//! - Only over contacts' connections (and between the user's own
//!   devices): two members who aren't contacts never connect, and hear
//!   each other through members they have in common.
//! - When two members connect, each catches up on what it missed
//!   (`Request::ChannelCatchUp`), for every group both are in: from when
//!   the asker was added, nothing before.
//! - Someone added learns of it from whoever added them, or any member
//!   connected to them (their own entry, `Notice::ChannelEvent`), and then
//!   catches up. Someone who's left tells the members they meet the same
//!   way. Nobody hears of a group they're not in.

use crate::lock::Locked;
use super::{Ctx, NodeHandle};
use crate::channel::{self, ChannelId, ChannelPost, RosterAction, RosterEvent};
use crate::displayname::{self, DisplayName};
use crate::groupintro::{self, Intro};
use crate::protocol::{self, ChannelData, ChannelReply, Notice, Reply, Request, RequestId};
use crate::transport::AuthenticatedConnection;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch};

/// Something new about a group, for every connection to act on.
#[derive(Clone, Debug)]
pub(crate) enum Broadcast {
    /// A post new here; `from` is the connection it came in on (0: made
    /// here), which doesn't need it back.
    Post { channel: ChannelId, post: ChannelPost, from: u64 },
    Event { channel: ChannelId, event: RosterEvent, from: u64 },
    /// A group was made or joined here (or `again`: asked to be caught up
    /// on afresh) — every connection checks whether it should catch up on
    /// it.
    Changed { channel: ChannelId, again: bool },
    /// An ask to be added, new here (`groupintro`).
    Intro { intro: Intro, from: u64 },
}

/// How many broadcasts a connection may fall behind by before it
/// misses some — and then catches up on everything instead.
pub(crate) const BROADCAST_CAPACITY: usize = 256;
/// A catch-up that's gone this long without any part of its answer is
/// given up on.
const CATCH_UP_STALL: Duration = Duration::from_secs(60);

impl Ctx {
    /// Tells every connection, and the front end, about `b`.
    pub(crate) fn channel_broadcast(&self, b: Broadcast) {
        let _ = self.channel_tx.send(b);
        self.channel_changed();
    }

    /// Tells the front end something about groups changed.
    pub(crate) fn channel_changed(&self) {
        self.channel_updates.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub(super) fn roster_of(&self, channel: &ChannelId) -> channel::Roster {
        channel::replay(channel, &channel::load_roster(&self.paths.channels(), channel).unwrap_or_default())
    }

    fn me(&self) -> [u8; 32] {
        self.identity.public_key_bytes()
    }

    /// Whether this device's user is in `channel`.
    pub(crate) fn in_channel(&self, channel: &ChannelId) -> bool {
        channel::exists(&self.paths.channels(), channel) && self.roster_of(channel).is_member(&self.me())
    }

    /// Whether this device's user and `key` are both in `channel`, as far
    /// as the roster here knows.
    pub(crate) fn both_in(&self, channel: &ChannelId, key: &[u8; 32]) -> bool {
        if !channel::exists(&self.paths.channels(), channel) {
            return false;
        }
        let roster = self.roster_of(channel);
        roster.is_member(&self.me()) && roster.is_member(key)
    }

    /// Members' own signed display names, as far as they're known here,
    /// to pass along with a group's data.
    fn member_names(&self, channel: &ChannelId) -> Vec<([u8; 32], DisplayName)> {
        let roster = self.roster_of(channel);
        let me = self.me();
        let mut names: Vec<([u8; 32], DisplayName)> = displayname::cached_all(&self.paths.displaynames())
            .unwrap_or_default()
            .into_iter()
            .filter(|(key, _)| *key != me && roster.is_member(key))
            .collect();
        if let Some(mine) = self.paths.my_display_name() {
            names.push((me, mine));
        }
        names
    }

    /// Stores what a group answer brought, and passes on whatever of it
    /// was new (`from`: the connection it came in on).
    pub(crate) fn absorb(&self, channel: &ChannelId, data: ChannelData, from: u64) -> (usize, usize) {
        let dir = self.paths.channels();
        let events = channel::merge_roster(&dir, channel, &data.roster).unwrap_or_default();
        let posts = channel::merge_posts(&dir, channel, &self.me(), &data.posts).unwrap_or_default();
        let me = self.me();
        for (key, name) in &data.names {
            if *key != me {
                let _ = displayname::cache_one(&self.paths.displaynames(), *key, name);
            }
        }
        let counts = (events.len(), posts.len());
        self.after_roster(channel, &events);
        for event in events {
            let _ = self.channel_tx.send(Broadcast::Event { channel: channel.clone(), event, from });
        }
        for post in posts {
            let _ = self.channel_tx.send(Broadcast::Post { channel: channel.clone(), post, from });
        }
        self.channel_changed();
        counts
    }

    /// After entries `new` arrived: if they say this user has left (on
    /// another of their devices), what was said goes here too.
    fn after_roster(&self, channel: &ChannelId, new: &[RosterEvent]) {
        let me = self.me();
        if new.iter().any(|e| e.subject == me && e.action == RosterAction::Left) && !self.roster_of(channel).is_member(&me) {
            let _ = channel::delete_posts(&self.paths.channels(), channel);
        }
    }

    /// Every group this device has, with its roster.
    fn known_groups(&self) -> Vec<(ChannelId, Vec<RosterEvent>)> {
        let dir = self.paths.channels();
        channel::known_channels(&dir).unwrap_or_default().into_iter().map(|c| {
            let events = channel::load_roster(&dir, &c).unwrap_or_default();
            (c, events)
        }).collect()
    }
}

impl Ctx {
    /// An ask to be added that's come here (`from`: the connection it came
    /// in on; 0, made here): kept to pass on while it lasts, and, if it's
    /// for this user, a contact request; if it's this user's own (made on
    /// another of their devices), remembered, so whoever was asked is
    /// added back. Only between two in the group as this side has it, as
    /// its messages are.
    pub(crate) fn take_intro(&self, intro: Intro, from: u64) {
        let (now, me) = (super::unix_now(), self.me());
        if !intro.live(now) || !intro.verify() || self.intros.locked().contains_key(&intro.id()) {
            return;
        }
        let roster = self.roster_of(&intro.channel);
        if !roster.is_member(&me) || !roster.is_member(&intro.from) || !roster.is_member(&intro.to) {
            return;
        }
        if intro.to == me {
            let known = crate::peers::load(&self.paths.peers()).unwrap_or_default().iter().any(|p| p.public_key == intro.from);
            match intro.open(&self.identity) {
                Ok(code) if !known && crate::peers::validate_address(&code.addresses).is_ok() => {
                    let path = self.paths.requests();
                    if !crate::requests::is_blocked(&path, &intro.from).unwrap_or(false)
                        && let Ok(true) = crate::requests::record(&path, intro.from, &code.addresses, now)
                    {
                        log!(self, "{} asks to be added, through a group you're both in", code.name);
                    }
                }
                Ok(_) => {}
                Err(e) => log!(self, "an ask to be added, through a group, that didn't open: {e:#}"),
            }
        }
        if intro.from == me {
            let _ = groupintro::keep(&self.paths.group_asks(), &intro, now);
        }
        self.intros.locked().retain(|_, i| i.live(now));
        self.intros.locked().insert(intro.id(), intro.clone());
        self.channel_broadcast(Broadcast::Intro { intro, from });
    }

    /// Asks to be added that are still passed on: those met here, and
    /// this user's own (kept across restarts).
    fn live_intros(&self) -> Vec<Intro> {
        let now = super::unix_now();
        let mut all: Vec<Intro> = self.intros.locked().values().filter(|i| i.live(now)).cloned().collect();
        for mine in groupintro::asked(&self.paths.group_asks(), now) {
            if !all.iter().any(|i| i.id() == mine.id()) {
                all.push(mine);
            }
        }
        all
    }
}

/// `channel`'s whole roster, `posts`, and members' names.
fn data_for(ctx: &Ctx, channel: &ChannelId, posts: Vec<ChannelPost>) -> ChannelReply {
    let roster = channel::load_roster(&ctx.paths.channels(), channel).unwrap_or_default();
    ChannelReply::Data(ChannelData { roster, posts, names: ctx.member_names(channel) })
}

/// A request this connection's `ChannelWork` is waiting on the answer to.
struct Pending {
    channel: ChannelId,
    data: ChannelData,
    progress: Instant,
}

/// One connection's part in groups: telling the other end what they're
/// in, passing things on, catching up, and answering catch-ups. Counts
/// the other end as connected (for `NodeHandle::is_connected`) for as
/// long as it exists.
pub(super) struct ChannelWork {
    ctx: Arc<Ctx>,
    /// The other end's root key (this user's own, between their devices).
    remote: [u8; 32],
    /// This connection, as `Broadcast`s name where they came from.
    id: u64,
    rx: broadcast::Receiver<Broadcast>,
    /// Groups already caught up on (or being) on this connection.
    caught_up: HashSet<ChannelId>,
    pending: HashMap<RequestId, Pending>,
}

impl ChannelWork {
    pub(super) fn new(ctx: Arc<Ctx>, remote: [u8; 32]) -> Self {
        let id = ctx.next_connection_id.fetch_add(1, Ordering::Relaxed) + 1;
        *ctx.connected.locked().entry(remote).or_insert(0) += 1;
        ctx.channel_changed();
        let rx = ctx.channel_tx.subscribe();
        ChannelWork { ctx, remote, id, rx, caught_up: HashSet::new(), pending: HashMap::new() }
    }

    /// For every group both ends are in: tells the other end its own
    /// entry (so it knows it's in, if it didn't), and catches up. For one
    /// this user has left and the other end is in: tells them so. Called
    /// once the connection is ready for traffic.
    pub(super) async fn start(&mut self, conn: &mut AuthenticatedConnection) -> Result<()> {
        let me = self.ctx.me();
        for (channel, events) in self.ctx.known_groups() {
            let roster = channel::replay(&channel, &events);
            let (mine, theirs) = (roster.is_member(&me), roster.is_member(&self.remote) || self.remote == me);
            let tell = if mine && theirs {
                channel::latest_about(&channel, &events, &self.remote)
            } else if !mine && roster.ever_in(&me) && theirs {
                channel::latest_about(&channel, &events, &me)
            } else {
                None
            };
            if let Some(event) = tell {
                protocol::send_notice(conn, Notice::ChannelEvent { channel: channel.clone(), event: event.clone() }).await?;
            }
            if mine && theirs {
                self.catch_up(conn, channel).await?;
            }
        }
        for intro in self.ctx.live_intros() {
            if self.ctx.both_in(&intro.channel, &self.remote) {
                protocol::send_notice(conn, Notice::GroupIntro(intro)).await?;
            }
        }
        Ok(())
    }

    async fn catch_up(&mut self, conn: &mut AuthenticatedConnection, channel: ChannelId) -> Result<()> {
        if self.pending.values().any(|p| p.channel == channel) {
            return Ok(());
        }
        let (from, digests) = channel::with_posts(&self.ctx.paths.channels(), &channel, channel::digests).unwrap_or_else(|_| channel::digests(&[]));
        let id = protocol::request(conn, Request::ChannelCatchUp { channel: channel.clone(), from, digests }).await?;
        self.caught_up.insert(channel.clone());
        self.pending.insert(id, Pending { channel, data: ChannelData::default(), progress: Instant::now() });
        Ok(())
    }

    async fn maybe_catch_up(&mut self, conn: &mut AuthenticatedConnection, channel: &ChannelId) -> Result<()> {
        if !self.caught_up.contains(channel) && self.ctx.both_in(channel, &self.remote) {
            self.catch_up(conn, channel.clone()).await?;
        }
        Ok(())
    }

    /// The next broadcast — `None` when this connection fell too far
    /// behind and missed some (`on_broadcast` then catches up afresh).
    /// Cancel-safe.
    pub(super) async fn next(&mut self) -> Option<Broadcast> {
        loop {
            match self.rx.recv().await {
                Ok(b) => return Some(b),
                Err(broadcast::error::RecvError::Lagged(_)) => return None,
                Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
            }
        }
    }

    pub(super) async fn on_broadcast(&mut self, conn: &mut AuthenticatedConnection, b: Option<Broadcast>) -> Result<()> {
        match b {
            None => {
                self.caught_up.clear();
                self.start(conn).await?;
            }
            Some(Broadcast::Post { channel, post, from }) => {
                let since = self.ctx.both_in(&channel, &self.remote).then(|| self.ctx.roster_of(&channel).joined(&self.remote)).flatten();
                if from != self.id && since.is_some_and(|since| post.created_at >= since) {
                    protocol::send_notice(conn, Notice::ChannelPost { channel, post }).await?;
                }
            }
            Some(Broadcast::Event { channel, event, from }) => {
                let me = self.ctx.me();
                if from != self.id
                    && channel::exists(&self.ctx.paths.channels(), &channel)
                    && (self.ctx.roster_of(&channel).is_member(&self.remote) || event.subject == self.remote || self.remote == me)
                {
                    protocol::send_notice(conn, Notice::ChannelEvent { channel: channel.clone(), event }).await?;
                }
                self.maybe_catch_up(conn, &channel).await?;
            }
            Some(Broadcast::Intro { intro, from }) => {
                if from != self.id && self.ctx.both_in(&intro.channel, &self.remote) {
                    protocol::send_notice(conn, Notice::GroupIntro(intro)).await?;
                }
            }
            Some(Broadcast::Changed { channel, again }) => {
                if again {
                    self.caught_up.remove(&channel);
                }
                let me = self.ctx.me();
                if self.remote == me && self.ctx.in_channel(&channel) {
                    let events = channel::load_roster(&self.ctx.paths.channels(), &channel).unwrap_or_default();
                    if let Some(event) = channel::latest_about(&channel, &events, &me) {
                        protocol::send_notice(conn, Notice::ChannelEvent { channel: channel.clone(), event: event.clone() }).await?;
                    }
                }
                self.maybe_catch_up(conn, &channel).await?;
            }
        }
        Ok(())
    }

    /// A group notice from the other end.
    pub(super) async fn on_notice(&mut self, conn: &mut AuthenticatedConnection, notice: Notice) -> Result<()> {
        let dir = self.ctx.paths.channels();
        let me = self.ctx.me();
        match notice {
            Notice::ChannelPost { channel, post } => {
                if !self.ctx.in_channel(&channel) {
                    return Ok(());
                }
                let new = channel::merge_posts(&dir, &channel, &me, std::slice::from_ref(&post)).unwrap_or_default();
                if !new.is_empty() {
                    self.ctx.channel_broadcast(Broadcast::Post { channel, post, from: self.id });
                } else if channel::verify_channel_post(&channel, &post) && !self.ctx.roster_of(&channel).wrote_as_member(&post.author, post.created_at) {
                    self.caught_up.remove(&channel);
                    self.maybe_catch_up(conn, &channel).await?;
                }
            }
            Notice::GroupIntro(intro) => self.ctx.take_intro(intro, self.id),
            Notice::ChannelEvent { channel, event } => {
                let about_me = event.subject == me && event.action == RosterAction::Joined;
                if !channel::exists(&dir, &channel) {
                    if !about_me || channel::validate_name(&channel.name).is_err() || !channel::verify_roster_event(&channel, &event) {
                        return Ok(());
                    }
                    // However many a contact adds us to, only so many are
                    // kept, and only a few not caught up on yet (which
                    // aren't listed, so nobody would see them pile up).
                    let known = channel::known_channels(&dir).unwrap_or_default();
                    let waiting = known.iter().filter(|c| !self.ctx.roster_of(c).ever_in(&me)).count();
                    if known.len() >= channel::MOST_GROUPS || waiting >= channel::MOST_WAITING_GROUPS {
                        log!(self.ctx, "added to a group: not kept, as there are {} already ({waiting} not caught up on)", known.len());
                        return Ok(());
                    }
                    log!(self.ctx, "added to a group: catching up on it");
                }
                let new = channel::merge_roster(&dir, &channel, std::slice::from_ref(&event)).unwrap_or_default();
                self.ctx.after_roster(&channel, &new);
                if !new.is_empty() {
                    self.ctx.channel_broadcast(Broadcast::Event { channel: channel.clone(), event, from: self.id });
                }
                if about_me && !self.ctx.in_channel(&channel) {
                    self.caught_up.remove(&channel);
                    self.catch_up(conn, channel).await?;
                } else {
                    self.maybe_catch_up(conn, &channel).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Answers a catch-up. Returns false for any other request.
    pub(super) async fn on_request(&mut self, conn: &mut AuthenticatedConnection, id: RequestId, request: &Request) -> Result<bool> {
        let Request::ChannelCatchUp { channel, from, digests } = request else { return Ok(false) };
        let joined = self.ctx.both_in(channel, &self.remote).then(|| self.ctx.roster_of(channel).joined(&self.remote)).flatten();
        let answer = match joined {
            Some(joined) => {
                let missing = channel::with_posts(&self.ctx.paths.channels(), channel, |posts| channel::missing_for(posts, joined, *from, digests)).unwrap_or_default();
                data_for(&self.ctx, channel, missing)
            }
            None => ChannelReply::Error(protocol::NO_SUCH_CHANNEL.to_string()),
        };
        protocol::respond_channel(conn, id, answer).await?;
        self.maybe_catch_up(conn, channel).await?;
        Ok(true)
    }

    /// Whether reply `id` is one this is waiting for.
    pub(super) fn owns(&self, id: RequestId) -> bool {
        self.pending.contains_key(&id)
    }

    pub(super) fn on_reply(&mut self, id: RequestId, reply: Reply) {
        let Some(mut pending) = self.pending.remove(&id) else { return };
        match reply {
            Reply::ChannelData { more, roster, posts, names } => {
                if pending.data.extend(roster, posts, names) && more {
                    pending.progress = Instant::now();
                    self.pending.insert(id, pending);
                } else {
                    let channel = pending.channel.clone();
                    let was_in = self.ctx.in_channel(&channel);
                    self.ctx.absorb(&channel, pending.data, self.id);
                    if !was_in && self.ctx.in_channel(&channel) {
                        self.ctx.channel_broadcast(Broadcast::Changed { channel, again: false });
                    }
                }
            }
            _ => {
                self.caught_up.remove(&pending.channel);
            }
        }
    }

    /// Gives up on answers that have stopped coming. Called every second
    /// or so.
    pub(super) fn housekeeping(&mut self) {
        let stalled: Vec<RequestId> = self.pending.iter().filter(|(_, p)| p.progress.elapsed() > CATCH_UP_STALL).map(|(id, _)| *id).collect();
        for id in stalled {
            if let Some(pending) = self.pending.remove(&id) {
                self.caught_up.remove(&pending.channel);
            }
        }
    }
}

impl Drop for ChannelWork {
    fn drop(&mut self) {
        let mut connected = self.ctx.connected.locked();
        if let Some(n) = connected.get_mut(&self.remote) {
            *n -= 1;
            if *n == 0 {
                connected.remove(&self.remote);
            }
        }
        drop(connected);
        self.ctx.channel_changed();
    }
}

/// What a front end does with groups.
impl NodeHandle {
    /// Counts up whenever anything about groups changes — a post, a
    /// roster entry, someone connecting — so a front end knows to redraw.
    pub fn channel_updates(&self) -> watch::Receiver<u64> {
        self.ctx.channel_updates.subscribe()
    }

    /// Counts up whenever a message comes from a peer, once it's stored:
    /// so a front end knows at once (a phone asleep may not look again
    /// for a while).
    pub fn message_updates(&self) -> watch::Receiver<u64> {
        self.ctx.message_updates.subscribe()
    }

    /// Whether there's a connection to `key` (a root key) right now.
    pub fn is_connected(&self, key: &[u8; 32]) -> bool {
        self.ctx.connected.locked().contains_key(key)
    }

    /// Makes a new group, with this user in it, and then each of
    /// `contacts` (by name). Returns it even if adding someone failed,
    /// with why.
    pub fn create_group(&self, contacts: &[&str]) -> Result<(ChannelId, Vec<String>)> {
        let me = self.ctx.me();
        let channel = ChannelId::new_group(me);
        let dir = self.ctx.paths.channels();
        channel::create(&dir, &channel)?;
        let first = channel::sign_roster_event(&channel, &self.ctx.identity, Vec::new(), me, RosterAction::Joined);
        channel::append_roster_event(&dir, &channel, &first)?;
        self.ctx.channel_broadcast(Broadcast::Changed { channel: channel.clone(), again: false });
        let failed = contacts.iter().filter_map(|name| self.add_to_group(&channel, name).err().map(|e| format!("{e:#}"))).collect();
        Ok((channel, failed))
    }

    /// Adds contact `name` to `channel`: they're in from now, and see
    /// what's said from then on. They're told as soon as a device of
    /// this user's, or anyone else in it, is connected to them.
    pub fn add_to_group(&self, channel: &ChannelId, name: &str) -> Result<()> {
        let shown = self.label_of(name);
        let reaching = self.reaching(name);
        let key = self.ctx.peers.locked().get(&reaching).map(|e| e.watch.borrow().public_key).ok_or_else(|| self.no_contact(name))?;
        if key == self.ctx.me() {
            anyhow::bail!("that's you");
        }
        if !self.ctx.in_channel(channel) {
            anyhow::bail!("you're not in this group");
        }
        let roster = self.ctx.roster_of(channel);
        if roster.is_member(&key) {
            anyhow::bail!("{shown} is in it already");
        }
        if roster.members.len() >= channel::MAX_MEMBERS {
            anyhow::bail!("a group can have at most {} people", channel::MAX_MEMBERS);
        }
        self.roster_action(channel, key, RosterAction::Joined)
    }

    /// Posts `text` to `channel`: stored here, and passed straight on to
    /// every member connected. Anyone not connected catches up later.
    pub fn post_to_channel(&self, channel: &ChannelId, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            anyhow::bail!("nothing to send");
        }
        if text.len() > crate::posts::MAX_POST_BYTES {
            anyhow::bail!("a message can be at most {} bytes", crate::posts::MAX_POST_BYTES);
        }
        if !self.ctx.in_channel(channel) {
            anyhow::bail!("you're not in this group");
        }
        let name = self.ctx.paths.my_display_name().map(|d| d.name);
        let post = channel::sign_channel_post(channel, &self.ctx.identity, text, name.as_deref());
        channel::merge_posts(&self.ctx.paths.channels(), channel, &self.ctx.me(), std::slice::from_ref(&post)).context("saving the message")?;
        self.ctx.channel_broadcast(Broadcast::Post { channel: channel.clone(), post, from: 0 });
        Ok(())
    }

    /// Signs one roster entry about `subject` as this user and passes it
    /// on.
    fn roster_action(&self, channel: &ChannelId, subject: [u8; 32], action: RosterAction) -> Result<()> {
        let dir = self.ctx.paths.channels();
        let events = channel::load_roster(&dir, channel)?;
        let event = channel::sign_roster_event(channel, &self.ctx.identity, channel::heads(channel, &events), subject, action);
        channel::append_roster_event(&dir, channel, &event)?;
        self.ctx.channel_broadcast(Broadcast::Event { channel: channel.clone(), event, from: 0 });
        Ok(())
    }

    /// Names `channel` (anyone in it can); empty goes back to its
    /// members' names.
    pub fn rename_group(&self, channel: &ChannelId, text: &str) -> Result<()> {
        if !self.ctx.in_channel(channel) {
            anyhow::bail!("you're not in this group");
        }
        let text = text.trim();
        channel::validate_group_name(text).map_err(|e| anyhow::anyhow!(e))?;
        self.roster_action(channel, self.ctx.me(), RosterAction::Named { text: text.to_string() })
    }

    /// Sets how long `channel`'s posts last before they delete themselves
    /// (anyone in it can; 0, for good). Posts already written keep the
    /// timer they were written under.
    pub fn set_channel_timer(&self, channel: &ChannelId, seconds: u64) -> Result<()> {
        if !self.ctx.in_channel(channel) {
            anyhow::bail!("you're not in this group");
        }
        self.roster_action(channel, self.ctx.me(), RosterAction::Timer { seconds })
    }

    /// Leaves `channel` and takes it off the list: the others are told
    /// (now, or as each is met), and what was said goes from this user's
    /// devices. Its roster stays, to tell anyone who hasn't heard. Anyone
    /// in it can add them back. One this user isn't in (never was, fully)
    /// is simply deleted.
    pub fn leave_group(&self, channel: &ChannelId) -> Result<()> {
        let dir = self.ctx.paths.channels();
        let roster = self.ctx.roster_of(channel);
        let me = self.ctx.me();
        if roster.is_member(&me) {
            self.roster_action(channel, me, RosterAction::Left)?;
            channel::delete_posts(&dir, channel)?;
        } else if !roster.ever_in(&me) {
            channel::delete(&dir, channel)?;
        }
        self.ctx.channel_changed();
        Ok(())
    }

    /// Asks `key`, in `channel` with this user but not their contact, to
    /// add them: this user's name and addresses, sealed for them, passed
    /// on through the group. If they add this user, this user's devices
    /// add them back by themselves (`groupintro`).
    pub fn ask_to_add(&self, channel: &ChannelId, key: &[u8; 32]) -> Result<()> {
        let me = self.ctx.me();
        let roster = self.ctx.roster_of(channel);
        if !roster.is_member(&me) {
            anyhow::bail!("you're not in this group");
        }
        if *key == me || !roster.is_member(key) {
            anyhow::bail!("they're not in this group");
        }
        if crate::peers::load(&self.ctx.paths.peers()).unwrap_or_default().iter().any(|p| p.public_key == *key) {
            anyhow::bail!("they're your contact already");
        }
        let manual = self.manual_address().items;
        let addresses: Vec<String> = manual.into_iter().chain(self.ctx.my_addresses.borrow().iter().cloned()).collect();
        if addresses.is_empty() {
            anyhow::bail!("this device has no address yet for them to reach you at");
        }
        let name = self.ctx.paths.my_display_name().map(|d| d.name).unwrap_or_default();
        let code = groupintro::Code { name, addresses: addresses.join(",") };
        let intro = Intro::make(channel, &self.ctx.identity, *key, &code, super::unix_now())?;
        self.ctx.take_intro(intro, 0);
        Ok(())
    }

    /// Catches up on `channel` afresh with every member connected.
    pub fn resync_channel(&self, channel: &ChannelId) {
        self.ctx.channel_broadcast(Broadcast::Changed { channel: channel.clone(), again: true });
    }
}
