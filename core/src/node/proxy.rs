//! Doing things through another of the user's devices, at the level of
//! what's done: a device with no connection of its own to a peer, while
//! another of the user's devices that's let relay has one, sends what it
//! has for that peer over its link to that device, and the other device
//! passes it on over its own connection to them, as the user's. The peer
//! only ever talks to "the user", on whichever of their devices it
//! reaches. Nothing goes inside anything else: each hop is an ordinary
//! connection with its own cover.
//!
//! - **What's reached** (`Proxy::tell`): a device that's let relay tells
//!   each of the others which peers it's connected to directly
//!   (`Notice::Reaching`), whenever that changes. What the others have said is kept in `Ctx::reached`
//!   while their links last, and makes a peer online here "through" that
//!   device (`through`).
//! - **Messages out** (`Proxy::pass`): what's in the outbox for a peer
//!   this device isn't connected to goes to the device that reaches them
//!   (`Request::PassOn`), which puts it in its own outbox, under the same
//!   id, and sends it to them; once it's taken there, it's out of the
//!   outbox here. Messages in need nothing new: the peer sends to every
//!   device it's connected to, and the user's devices give each other
//!   copies (`devices::carbon`).
//! - **Status and picture** (`Proxy::ask_profile`): a peer this device
//!   isn't connected to has theirs copied from another of the user's
//!   devices (`Request::ProfileThrough`), signed by the peer, with
//!   the peer offline or not and whether or not that device relays: it's
//!   only what that device already keeps.

use super::Ctx;
use crate::lock::Locked;
use crate::outbox::{self, MessageId};
use crate::protocol::{self, Notice, Reply, Request, RequestId};
use crate::transport::AuthenticatedConnection;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often what's reached, and the outbox, are looked at.
const CHECK: Duration = Duration::from_secs(2);

/// How long after a first choice of relay a better one can still take its place:
/// links come up in no particular order just after starting or moving.
const SETTLE: Duration = Duration::from_secs(5);

/// How long a chosen relay can stop reaching someone, with its link still
/// up, before what goes through it moves to another.
const GRACE: Duration = Duration::from_secs(3);

/// The soonest delays are told again when only they have changed.
const DELAYS_EVERY: Duration = Duration::from_secs(30);

/// By how much a delay has to change to be told again: this much, and at
/// least a quarter of what it was.
const DELAY_CHANGE: Duration = Duration::from_millis(20);

/// How often a relay is asked for the status and picture of someone it
/// reaches: little to ask, and nothing comes back unless there's a newer one.
const PROFILE_EVERY: Duration = Duration::from_secs(60);

/// How often another of the user's devices is asked for the status and
/// picture of someone it doesn't reach: they only change while that
/// device, or another, is connected to them.
const PROFILE_KEPT_EVERY: Duration = Duration::from_secs(10 * 60);

/// What this device can reach directly now, for telling the user's other
/// devices: those of theirs it has a link to, by device key (so two with
/// no way to each other can go through it), then the peers it's
/// connected to, by user key. Nothing, unless it's let relay.
fn reaching(ctx: &Ctx) -> Vec<[u8; 32]> {
    if !super::devices::relaying_here(ctx) {
        return Vec::new();
    }
    let mut mine: Vec<[u8; 32]> = ctx.siblings.locked().keys().copied().collect();
    mine.sort();
    let users: Vec<[u8; 32]> = ctx.peers.locked().values().filter(|e| e.device.is_none()).map(|e| e.watch.borrow().public_key).collect();
    let mut direct: Vec<[u8; 32]> = users.into_iter().filter(|user| direct(ctx, user)).collect();
    direct.sort();
    direct.dedup();
    mine.extend(direct);
    mine.truncate(protocol::MAX_REACHING);
    mine
}

/// The user's own devices in `reached`.
fn own_among(ctx: &Ctx, reached: &[[u8; 32]]) -> Vec<[u8; 32]> {
    reached.iter().filter(|key| super::devices::is_mine(ctx, key)).copied().collect()
}

/// Whether this device's own connection to peer `user` (their user key)
/// is up.
pub(super) fn direct(ctx: &Ctx, user: &[u8; 32]) -> bool {
    ctx.live(user).is_some()
}

/// Whether this device is connected to peer `user` (their user key) now,
/// on any of its own entries for them.
pub(super) fn online_here(ctx: &Ctx, user: &[u8; 32]) -> bool {
    ctx.peers.locked().values().any(|e| e.online.load(Ordering::Relaxed) && e.watch.borrow().public_key == *user)
}

/// The relay that peer `user` (their user key, or another of this user's
/// devices by its key) is reached through, for a device not connected to
/// them itself: of the devices that are linked, let relay and connected to
/// them, the one with the lowest delay when the choice was made, and
/// that one from then on, however the delays change (`choose`).
pub(super) fn through(ctx: &Ctx, user: &[u8; 32]) -> Option<[u8; 32]> {
    let devices: Vec<[u8; 32]> = ctx.reached.locked().iter().filter(|(_, users)| users.contains(user)).map(|(device, _)| *device).collect();
    if devices.is_empty() && !ctx.relay_choices.locked().contains_key(user) {
        return None;
    }
    let linked: HashSet<[u8; 32]> = ctx.siblings.locked().keys().copied().collect();
    if linked.contains(user) || *user == super::devices::this_device(ctx) {
        return None;
    }
    let list = super::devices::own_list(ctx).devices();
    let delays = ctx.reach_delays.locked();
    let mut relays: Vec<Relay> = devices
        .into_iter()
        .filter(|d| linked.contains(d))
        .filter_map(|device| {
            let on = list.get(&device).filter(|d| d.relay)?;
            let onward = delays.get(&device).and_then(|d| d.get(user)).copied();
            Some(Relay { delay: onward.zip(ctx.least_rtt(&device)).map(|(a, b)| a + b), name: on.name.clone(), device })
        })
        .collect();
    drop(delays);
    relays.sort_by(|a, b| (a.delay.is_none(), a.delay, &a.name).cmp(&(b.delay.is_none(), b.delay, &b.name)));
    if online_here(ctx, user) {
        ctx.relay_choices.locked().remove(user);
        return relays.first().map(|r| r.device);
    }
    let (chosen, changed) = {
        let mut choices = ctx.relay_choices.locked();
        let mut choice = choices.remove(user);
        let was = choice.as_ref().map(|c| c.device);
        let chosen = choose(&mut choice, &relays, |d| linked.contains(d), Instant::now());
        let changed = chosen.is_some() && chosen != was;
        if let Some(choice) = choice {
            choices.insert(*user, choice);
        }
        (chosen, changed)
    };
    if changed && let Some(relay) = relays.iter().find(|r| Some(r.device) == chosen) {
        let who = super::devices::peer_name(ctx, user).map_or_else(|| super::devices::label(ctx, user), |name| ctx.shown(&name));
        let delay = relay.delay.map_or("delay unknown".to_string(), |d| format!("{} ms", d.as_millis()));
        log!(ctx, "{who}: going through your device {} ({delay}, of {} that can)", relay.name, relays.len());
    }
    chosen
}

/// A device that could relay to someone.
struct Relay {
    device: [u8; 32],
    name: String,
    /// There and on to them, if it's said; those that haven't come last.
    delay: Option<Duration>,
}

/// How long a round trip takes to each of those reached, by their key.
pub(super) type Delays = HashMap<[u8; 32], Duration>;

/// The relay chosen for someone (`through`).
pub(super) struct RelayChoice {
    device: [u8; 32],
    /// Till when a better one can take its place: a first choice only,
    /// so one made because the last stopped working isn't undone when
    /// that one returns.
    settle: Option<Instant>,
    /// Since when it hasn't said it reaches them.
    missing: Option<Instant>,
}

/// Which relay to use, of `relays` (the best first), given what was
/// chosen before. Once settled a choice is kept for as long as
/// it still reaches them: a faster one is no reason to move. One that
/// stops is given `GRACE` while its link to this device is up, if there's
/// another to move to, and is then left for good; a link that's gone has
/// already had its time being noticed.
fn choose(choice: &mut Option<RelayChoice>, relays: &[Relay], linked: impl Fn(&[u8; 32]) -> bool, now: Instant) -> Option<[u8; 32]> {
    let best = relays.first().map(|r| r.device);
    if let Some(c) = choice {
        if relays.iter().any(|r| r.device == c.device) {
            c.missing = None;
            if c.settle.is_some_and(|until| now < until)
                && let Some(best) = best
            {
                c.device = best;
            }
            return Some(c.device);
        }
        let since = *c.missing.get_or_insert(now);
        let graced = now.duration_since(since) < GRACE;
        if best.is_none() {
            if !graced {
                *choice = None;
            }
            return None;
        }
        if graced && linked(&c.device) {
            return Some(c.device);
        }
    }
    let settle = choice.is_none().then_some(now + SETTLE);
    *choice = best.map(|device| RelayChoice { device, settle, missing: None });
    best
}

/// The device peer `user` is reached through, by name, for one not
/// connected here (see `through`).
pub(super) fn through_name(ctx: &Ctx, user: &[u8; 32]) -> Option<String> {
    if online_here(ctx, user) {
        return None;
    }
    through(ctx, user).map(|device| super::devices::label(ctx, &device))
}

/// What a link to another of the user's devices (`devices::run`) does
/// for doing things through it, or for it.
pub(super) struct Proxy {
    ctx: Arc<Ctx>,
    /// The device at the other end.
    remote: [u8; 32],
    who: String,
    check: tokio::time::Interval,
    /// Something new in an outbox here, maybe (see `devices::carbon`).
    carbons: tokio::sync::broadcast::Receiver<()>,
    /// What it was last told this device reaches.
    told: Option<Vec<[u8; 32]>>,
    /// The delays it was last told, and when.
    told_delays: Option<(Delays, Instant)>,
    /// Messages handed to it and not yet answered: whose, and which.
    passing: HashMap<RequestId, ([u8; 32], MessageId)>,
    /// Peers it's refused messages for, each said once.
    refused: HashSet<[u8; 32]>,
    /// When it was last asked for each peer's status and picture.
    profiles_asked: HashMap<[u8; 32], Instant>,
    /// The one being asked about: request, whose, the time of the one
    /// kept, and the picture so far.
    profile_asking: Option<(RequestId, [u8; 32], u64, Option<crate::profile::ProfilePart>)>,
}

impl Proxy {
    pub fn new(ctx: &Arc<Ctx>, remote: [u8; 32]) -> Proxy {
        Proxy {
            ctx: ctx.clone(),
            remote,
            who: super::devices::label(ctx, &remote),
            check: tokio::time::interval(CHECK),
            carbons: ctx.carbon_tx.subscribe(),
            told: None,
            told_delays: None,
            passing: HashMap::new(),
            refused: HashSet::new(),
            profiles_asked: HashMap::new(),
            profile_asking: None,
        }
    }

    /// Waits for the next time to look. Cancel-safe.
    pub async fn next(&mut self) {
        tokio::select! {
            _ = self.check.tick() => {}
            carbon = self.carbons.recv() => {
                if let Err(tokio::sync::broadcast::error::RecvError::Closed) = carbon {
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    /// Tells it what this device reaches, if that's changed, and hands it
    /// what's waiting for the peers reached only through it.
    pub async fn act(&mut self, conn: &mut AuthenticatedConnection) -> Result<()> {
        self.tell(conn).await?;
        self.pass(conn).await?;
        self.ask_profile(conn).await
    }

    /// Asks for the status and picture of one peer this device isn't
    /// connected to, if it's time to: a newer one than kept here, if it
    /// has it. Often for one reached through it, seldom for any other.
    async fn ask_profile(&mut self, conn: &mut AuthenticatedConnection) -> Result<()> {
        if self.profile_asking.is_some() {
            return Ok(());
        }
        let users: Vec<[u8; 32]> = self.ctx.peers.locked().values().filter(|e| e.device.is_none()).map(|e| e.watch.borrow().public_key).collect();
        let due = users.into_iter().find(|user| {
            let waited = self.profiles_asked.get(user).map(|at| at.elapsed());
            waited.is_none_or(|waited| waited >= PROFILE_KEPT_EVERY || (waited >= PROFILE_EVERY && through(&self.ctx, user) == Some(self.remote))) && !online_here(&self.ctx, user)
        });
        let Some(contact) = due else { return Ok(()) };
        self.profiles_asked.insert(contact, Instant::now());
        let have = crate::profile::cached(&self.ctx.paths.profiles(), &contact).map_or(0, |p| p.at);
        let rid = protocol::request(conn, Request::ProfileThrough { contact, have, from: 0 }).await?;
        self.profile_asking = Some((rid, contact, have, None));
        Ok(())
    }

    /// Whether reply `rid` is about a peer's status and picture.
    pub fn owns_profile(&self, rid: RequestId) -> bool {
        self.profile_asking.as_ref().is_some_and(|(asked, ..)| *asked == rid)
    }

    /// A part of the status and picture asked for.
    pub async fn on_profile(&mut self, conn: &mut AuthenticatedConnection, reply: Reply) -> Result<()> {
        let Some((_, contact, have, so_far)) = self.profile_asking.take() else { return Ok(()) };
        let Reply::Profile(Some(part)) = reply else { return Ok(()) };
        match crate::profile::gather(so_far, part) {
            crate::profile::Gathered::Whole(profile) => {
                if let Ok(true) = crate::profile::cache(&self.ctx.paths.profiles(), &contact, &profile) {
                    let name = self.ctx.shown(&super::devices::peer_name(&self.ctx, &contact).unwrap_or_default());
                    log!(self.ctx, "{name}: status and picture updated, from your device {}", self.who);
                    self.ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
                }
            }
            crate::profile::Gathered::More(so_far) => {
                let from = so_far.data.len() as u32;
                let rid = protocol::request(conn, Request::ProfileThrough { contact, have, from }).await?;
                self.profile_asking = Some((rid, contact, have, Some(so_far)));
            }
            crate::profile::Gathered::Wrong => {}
        }
        Ok(())
    }

    /// Another of the user's devices asks for what this one has of peer
    /// `contact`'s status and picture.
    pub async fn on_profile_through(&self, conn: &mut AuthenticatedConnection, rid: RequestId, contact: [u8; 32], have: u64, from: u32) -> Result<()> {
        let reply = if super::devices::peer_name(&self.ctx, &contact).is_none() {
            Reply::Refused("they aren't a contact there".to_string())
        } else {
            let newer = crate::profile::cached(&self.ctx.paths.profiles(), &contact).filter(|p| p.at > have);
            Reply::Profile(newer.map(|p| crate::profile::part(&p, from as usize)))
        };
        protocol::reply(conn, rid, reply).await
    }

    async fn tell(&mut self, conn: &mut AuthenticatedConnection) -> Result<()> {
        let now = reaching(&self.ctx);
        let same = self.told.as_ref() == Some(&now);
        self.tell_delays(conn, &now, !same).await?;
        if same || (self.told.is_none() && now.is_empty()) {
            return Ok(());
        }
        protocol::send_notice(conn, Notice::Reaching(now.clone())).await?;
        self.told = Some(now);
        Ok(())
    }

    /// Tells it the delays to those in `reached`, first of all with a
    /// change to who's reached (`list_changed`), or every so often if
    /// they've moved by much. They only matter when a relay's being chosen.
    async fn tell_delays(&mut self, conn: &mut AuthenticatedConnection, reached: &[[u8; 32]], list_changed: bool) -> Result<()> {
        if reached.is_empty() {
            return Ok(());
        }
        let now: Delays = reached.iter().filter_map(|key| self.ctx.least_rtt(key).map(|rtt| (*key, rtt))).collect();
        if !list_changed && let Some((told, at)) = &self.told_delays
            && (at.elapsed() < DELAYS_EVERY || !delays_moved(told, &now))
        {
            return Ok(());
        }
        let list = now.iter().map(|(key, rtt)| (*key, rtt.as_millis().min(u16::MAX as u128) as u16)).collect();
        protocol::send_notice(conn, Notice::ReachingDelays(list)).await?;
        self.told_delays = Some((now, Instant::now()));
        Ok(())
    }

    async fn pass(&mut self, conn: &mut AuthenticatedConnection) -> Result<()> {
        let users = self.ctx.reached.locked().get(&self.remote).cloned().unwrap_or_default();
        let dir = self.ctx.paths.outbox();
        for user in users {
            if online_here(&self.ctx, &user) || through(&self.ctx, &user) != Some(self.remote) {
                continue;
            }
            for message in outbox::pending(&dir, &user).unwrap_or_default() {
                if self.passing.values().any(|(_, id)| *id == message.id) {
                    continue;
                }
                let request = Request::PassOn { contact: user, id: message.id, sent_at: message.sent_at, lifetime: message.lifetime, text: message.text };
                let rid = protocol::request(conn, request).await?;
                self.passing.insert(rid, (user, message.id));
            }
        }
        Ok(())
    }

    /// What it says it reaches now.
    pub fn on_reaching(&mut self, users: Vec<[u8; 32]>) {
        let was = self.ctx.reached.locked().insert(self.remote, users.clone());
        if was.as_ref() != Some(&users) {
            let names: Vec<String> = users.iter().filter_map(|u| super::devices::peer_name(&self.ctx, u)).map(|name| self.ctx.shown(&name)).collect();
            if !names.is_empty() {
                log!(self.ctx, "your device {} reaches {}", self.who, names.join(", "));
            }
            self.ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
            if own_among(&self.ctx, was.as_deref().unwrap_or_default()) != own_among(&self.ctx, &users) {
                super::devices::changed(&self.ctx);
            }
        }
        self.check.reset_immediately();
    }

    /// The delays it says it has to those it reaches.
    pub fn on_delays(&mut self, delays: Vec<([u8; 32], u16)>) {
        let delays = delays.into_iter().map(|(key, ms)| (key, Duration::from_millis(ms.into()))).collect();
        self.ctx.reach_delays.locked().insert(self.remote, delays);
    }

    /// Whether reply `rid` is for a message it was handed.
    pub fn owns(&self, rid: RequestId) -> bool {
        self.passing.contains_key(&rid)
    }

    /// Its answer about a message it was handed: taken, so it's out of the
    /// outbox here; or not, and it waits here.
    pub fn on_reply(&mut self, rid: RequestId, reply: Reply) {
        let Some((user, id)) = self.passing.remove(&rid) else { return };
        match reply {
            Reply::Ack => {
                let _ = outbox::remove(&self.ctx.paths.outbox(), &user, &id);
                self.refused.remove(&user);
            }
            other => {
                if self.refused.insert(user) {
                    let why = match other {
                        Reply::Refused(why) => why,
                        _ => "it can't".to_string(),
                    };
                    let name = self.ctx.shown(&super::devices::peer_name(&self.ctx, &user).unwrap_or_default());
                    log!(self.ctx, "your device {} won't pass messages on to {name}: {why}", self.who);
                }
            }
        }
    }

    /// It asks this device to deliver a message of its to a peer
    /// (`Request::PassOn`).
    pub async fn on_pass_on(&mut self, conn: &mut AuthenticatedConnection, rid: RequestId, request: Request) -> Result<()> {
        let Request::PassOn { contact, id, sent_at, lifetime, text } = request else { return protocol::reply(conn, rid, Reply::Unsupported).await };
        let reply = match take(&self.ctx, contact, id, sent_at, lifetime, &text) {
            Ok(()) => Reply::Ack,
            Err(why) => Reply::Refused(why.to_string()),
        };
        protocol::reply(conn, rid, reply).await
    }

    /// Its link has gone: nothing's reached through it.
    pub fn gone(&self) {
        self.ctx.reach_delays.locked().remove(&self.remote);
        let was = self.ctx.reached.locked().remove(&self.remote);
        if was.is_some_and(|users| !users.is_empty()) {
            self.ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
        }
    }
}

/// Whether any delay in `now` is new or far enough from what `told` said.
fn delays_moved(told: &Delays, now: &Delays) -> bool {
    now.iter().any(|(key, rtt)| {
        told.get(key).is_none_or(|was| {
            let moved = rtt.abs_diff(*was);
            moved >= DELAY_CHANGE && moved >= *was / 4
        })
    })
}

/// Another of the user's devices' message to peer `contact`, to go from
/// this one: into the outbox here under its own id (once), and to them
/// now if they're connected.
fn take(ctx: &Ctx, contact: [u8; 32], id: MessageId, sent_at: u64, lifetime: u64, text: &str) -> std::result::Result<(), &'static str> {
    if !super::devices::relaying_here(ctx) {
        return Err("this device isn't let relay");
    }
    if super::devices::peer_name(ctx, &contact).is_none() {
        return Err("they aren't a contact there");
    }
    let dir = ctx.paths.outbox();
    if !outbox::pending(&dir, &contact).unwrap_or_default().iter().any(|m| m.id == id) {
        outbox::enqueue(&dir, &contact, id, text, sent_at, lifetime).map_err(|_| "it couldn't keep it")?;
    }
    let requests: Vec<_> =
        ctx.peers.locked().values().filter(|e| e.online.load(Ordering::Relaxed) && e.watch.borrow().public_key == contact).map(|e| e.requests.clone()).collect();
    for requests in requests {
        let (reply, _) = tokio::sync::oneshot::channel();
        let _ = requests.try_send(super::peer::PeerRequest::Send { id, text: text.to_string(), sent_at, lifetime, reply });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(n: u8, ms: Option<u64>) -> Relay {
        Relay { device: [n; 32], name: format!("{n}"), delay: ms.map(Duration::from_millis) }
    }

    #[test]
    fn the_lowest_delay_is_chosen_and_kept_when_another_gets_faster() {
        let t = Instant::now();
        let mut choice = None;
        assert_eq!(choose(&mut choice, &[relay(1, Some(10)), relay(2, Some(50))], |_| true, t), Some([1; 32]));
        let later = t + SETTLE + Duration::from_secs(1);
        assert_eq!(choose(&mut choice, &[relay(2, Some(5)), relay(1, Some(90))], |_| true, later), Some([1; 32]));
    }

    #[test]
    fn a_better_relay_that_turns_up_while_settling_takes_over() {
        let t = Instant::now();
        let mut choice = None;
        assert_eq!(choose(&mut choice, &[relay(2, None)], |_| true, t), Some([2; 32]));
        assert_eq!(choose(&mut choice, &[relay(1, Some(9)), relay(2, None)], |_| true, t + Duration::from_secs(2)), Some([1; 32]));
    }

    #[test]
    fn a_relay_that_stops_reaching_is_given_grace_and_then_left_for_good() {
        let t = Instant::now();
        let mut choice = None;
        choose(&mut choice, &[relay(1, Some(1)), relay(2, Some(9))], |_| true, t);
        let late = t + SETTLE * 2;
        let others = [relay(2, Some(9))];
        assert_eq!(choose(&mut choice, &others, |_| true, late), Some([1; 32]), "a brief drop moves nothing");
        assert_eq!(choose(&mut choice, &others, |_| true, late + GRACE), Some([2; 32]));
        let back = [relay(1, Some(1)), relay(2, Some(9))];
        assert_eq!(choose(&mut choice, &back, |_| true, late + GRACE * 8), Some([2; 32]), "and it isn't gone back to");
    }

    #[test]
    fn a_relay_moved_to_isnt_undone_when_the_faster_one_returns_at_once() {
        let t = Instant::now();
        let mut choice = None;
        choose(&mut choice, &[relay(1, Some(1)), relay(2, Some(9))], |_| true, t);
        let late = t + SETTLE * 2;
        assert_eq!(choose(&mut choice, &[relay(2, Some(9))], |d| *d != [1; 32], late), Some([2; 32]));
        let back = [relay(1, Some(1)), relay(2, Some(9))];
        assert_eq!(choose(&mut choice, &back, |_| true, late + Duration::from_secs(1)), Some([2; 32]));
    }

    #[test]
    fn a_brief_drop_of_the_chosen_relay_keeps_it() {
        let t = Instant::now();
        let mut choice = None;
        choose(&mut choice, &[relay(1, Some(1)), relay(2, Some(9))], |_| true, t);
        let late = t + SETTLE * 2;
        choose(&mut choice, &[relay(2, Some(9))], |_| true, late);
        let back = [relay(2, Some(1)), relay(1, Some(50))];
        assert_eq!(choose(&mut choice, &back, |_| true, late + Duration::from_secs(1)), Some([1; 32]));
    }

    #[test]
    fn a_relay_whose_link_has_gone_is_left_at_once() {
        let t = Instant::now();
        let mut choice = None;
        choose(&mut choice, &[relay(1, Some(1))], |_| true, t);
        assert_eq!(choose(&mut choice, &[relay(2, Some(9))], |d| *d != [1; 32], t + SETTLE * 2), Some([2; 32]));
    }

    #[test]
    fn with_no_relay_left_nothing_is_said_to_reach_them() {
        let t = Instant::now();
        let mut choice = None;
        choose(&mut choice, &[relay(1, Some(1))], |_| true, t);
        assert_eq!(choose(&mut choice, &[], |_| true, t + SETTLE), None);
        assert_eq!(choose(&mut choice, &[], |_| true, t + SETTLE + GRACE), None);
        assert!(choice.is_none());
    }

    #[test]
    fn delays_are_told_again_only_after_a_real_change() {
        let was = HashMap::from([([1; 32], Duration::from_millis(100))]);
        let by = |ms| HashMap::from([([1; 32], Duration::from_millis(ms))]);
        assert!(!delays_moved(&was, &by(115)));
        assert!(!delays_moved(&was, &by(80 + 1)));
        assert!(delays_moved(&was, &by(130)));
        assert!(delays_moved(&was, &HashMap::from([([2; 32], Duration::from_millis(1))])));
    }
}
