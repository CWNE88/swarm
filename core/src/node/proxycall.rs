//! Calls through another of the user's devices (see `proxy`).
//! The device that's let relay and connected to the peer is a bridge: it
//! takes the call on its own connection to them, as the user's, and
//! passes each of its signals on, as it is (nothing's decoded), over the
//! link to the device that's in the call (`Notice::CallThrough`).
//!
//! - **A ring in** (`fan`): the peer rings this device, and it's let
//!   relay: it rings here, and passes the ring on to each of the
//!   user's other devices it's linked to. A device connected to the peer
//!   itself refuses one passed on (the peer rings it directly). Whichever
//!   answers first has the call; the rest are told it was answered
//!   elsewhere.
//! - **A call out** (`place`): a device not connected to the peer rings
//!   them through one that is (`handle`, for the front end, rides the
//!   link to it), which rings each of the peer's devices it's connected
//!   to, the first to answer having it.
//! - **What a call may send**: the device in the call is told once a
//!   second what the bridge's connection to the peer carries
//!   (`Notice::CallRoom`), and sends no more than that and its link
//!   carry, and whether it has cover (so others' pictures aren't passed
//!   on along it, as on a covered direct one: `CallBudget::covered`); the
//!   bridge is told whether the call wants video, and how many others'
//!   voices a conference passes on along it, for its own reckoning of
//!   that connection: they share its room for voice there as on a direct
//!   one (`CallBudget::relayed`).
//!
//! - **A conference**: a ring into one goes as any ring, its roster
//!   shown on each device rung, and the rest of a conference's business
//!   (rosters, who hears whom, voices passed on) rides the bridge as it
//!   is. It's the device in the call whose front end has it, as its own
//!   events (`callcarry::front_end_takes`), never the bridge's.
//!
//! - **Between two of the user's devices** with no link to each other
//!   (`LinkCalls::between`): the device let relay that's linked to both
//!   joins their two links. To each, the other device is the "peer" of
//!   the call, by its device key. What each may send is what the bridge's
//!   link to the other carries.
//!
//! - **The link goes in a call** (`LinkCalls::gone`): the calls through
//!   it are kept for `CALL_HOLD`, for a new link to that device to take
//!   on (`LinkCalls::new`), and a full picture's asked for each way, as
//!   for a direct call carried on (`callcarry`). If none comes, they end.
//!   The other side of the bridge sees nothing but a gap. A bridge finds
//!   its links by device, so it goes on over the new one.
//!
//! - **The relay's lost for good** (`move_held`): held past `MOVE_AFTER`
//!   and another way to the peer there, the device in the call goes that
//!   way (direct, or through another device let relay) and says
//!   `CallSignal::Takeover`, which the peer's connection to it answers
//!   for the connection the call was on (`CallOn::take_over`). Calls
//!   with a peer only: a call between two of the user's devices waits
//!   for its relay.
//!
//! - **A direct connection comes up** (`LinkCalls::go_direct`): the
//!   device in a call with a peer through a relay, once it's had its own
//!   connection to them for `DIRECT_STEADY`, carries the call on over it
//!   the same way (`move_to_direct`), while the relay still carries it:
//!   the call goes on through the relay until the peer's device answers
//!   on the direct one (`CallSignal::Resume`), and the relay's then let
//!   go. Should it not answer, the call stays as it was, and it's tried
//!   again after `DIRECT_RETRY`.

use super::{Ctx, PeerCallHandle};
use crate::call::{CallBudget, CallCommand, CallLink, CallSignal};
use crate::lock::Locked;
use crate::protocol::{self, Notice};
use crate::transport::AuthenticatedConnection;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How long a call whose link has gone waits for that device to come
/// back, before going another way if there is one: a brief drop doesn't
/// move it.
const MOVE_AFTER: Duration = Duration::from_secs(3);
/// How long a bridge keeps a call whose link to the device in it has
/// gone. That device may not notice for `LIVENESS_TIMEOUT` after the
/// bridge does, then waits `MOVE_AFTER`, and has to get word to the peer.
const BRIDGE_HOLD: Duration = Duration::from_secs(protocol::LIVENESS_TIMEOUT.as_secs() + MOVE_AFTER.as_secs() + 3);
/// How often a held call looks for another way, after that.
const MOVE_RECHECK: Duration = Duration::from_millis(500);
/// How long this device's own connection to a peer has to have been up
/// before a call with them through a relay moves onto it: one that's
/// only just come, or comes and goes, isn't worth a gap in the call.
const DIRECT_STEADY: Duration = Duration::from_secs(3);
/// After a move onto a direct connection that didn't happen, how long
/// before it's tried again (longer than the move's given, `CALL_HOLD`).
const DIRECT_RETRY: Duration = Duration::from_secs(30);

/// What goes on a link about a call through it, from this device's side:
/// a call's way in (`Attach`: where its signals from the other side go;
/// the bridge's connection's room, if this is the bridge, and whether it
/// is; `keep`, a room to keep up to date from this link, for a bridge
/// joining two links), that room once it's known (`Room`), or a signal
/// to send.
pub(super) enum LinkCall {
    Attach { tx: mpsc::UnboundedSender<CallSignal>, budget: Option<Arc<CallBudget>>, bridge: bool, keep: Option<Arc<CallBudget>> },
    Room(Arc<CallBudget>),
    Signal(CallSignal),
    /// A call from the link this one replaced.
    Adopt(Box<OnLink>),
}

/// A `LinkCall` about the call with peer `contact` (their user key).
pub(super) struct Through {
    pub contact: [u8; 32],
    pub call: LinkCall,
}

/// A call through a link, at this end of it.
pub(super) struct OnLink {
    call_in: mpsc::UnboundedSender<CallSignal>,
    /// The bridge's: its connection to the peer's. Otherwise the call's
    /// here, as the front end reads it.
    budget: Option<Arc<CallBudget>>,
    bridge: bool,
    /// Not the bridge: what it says its connection to the peer carries
    /// (voice's bytes a frame, video's bits a second, a picture's piece),
    /// and whether it has cover.
    room: Option<((u32, u32, u32), bool)>,
    /// A bridge joining two links: what this link carries, kept up to
    /// date here for the device at the end of the other.
    keep: Option<(Arc<CallBudget>, crate::call::UncoveredCall)>,
    /// Not the bridge: since when this device has had its own connection
    /// to the peer, and when the call last tried moving onto it (see
    /// `LinkCalls::go_direct`).
    direct_since: Option<Instant>,
    direct_tried: Option<Instant>,
}

impl OnLink {
    fn new(call_in: mpsc::UnboundedSender<CallSignal>, budget: Option<Arc<CallBudget>>, bridge: bool, keep: Option<(Arc<CallBudget>, crate::call::UncoveredCall)>) -> OnLink {
        OnLink { call_in, budget, bridge, room: None, keep, direct_since: None, direct_tried: None }
    }

    fn live(&self) -> bool {
        !self.call_in.is_closed()
    }
}

/// The bridge's connection to the peer is its own again: no voices of a
/// conference through it to make room for.
impl Drop for OnLink {
    fn drop(&mut self) {
        if let (true, Some(budget)) = (self.bridge, &self.budget) {
            budget.relayed.store(0, Ordering::Relaxed);
        }
    }
}

fn ends_call(signal: &CallSignal) -> bool {
    matches!(signal, CallSignal::Hangup | CallSignal::Reject | CallSignal::AnsweredElsewhere)
}

/// Calls whose link to a device has gone, held for a new one: by device,
/// with which hold it is (a timer ends it, if it's still that one then)
/// and the calls, by their peer.
pub(super) type HeldLinkCalls = HashMap<[u8; 32], (u64, HashMap<[u8; 32], OnLink>)>;

/// The calls through one link to another of the user's devices, kept by
/// that link's task (`devices::run`).
pub(super) struct LinkCalls {
    ctx: Arc<Ctx>,
    remote: [u8; 32],
    who: String,
    tx: mpsc::UnboundedSender<Through>,
    rx: mpsc::UnboundedReceiver<Through>,
    calls: HashMap<[u8; 32], OnLink>,
    room: tokio::time::Interval,
    /// Whether `Ctx::links_in_calls` has this link in it.
    marked: bool,
}

/// What wakes a link for its calls through it.
pub(super) enum Wake {
    Send(Through),
    Room,
}

impl LinkCalls {
    pub fn new(ctx: &Arc<Ctx>, remote: [u8; 32]) -> LinkCalls {
        let (tx, rx) = mpsc::unbounded_channel();
        let held = {
            let mut links = ctx.link_calls.locked();
            links.insert(remote, tx.clone());
            ctx.held_link_calls.locked().remove(&remote)
        };
        let mut link = LinkCalls {
            ctx: ctx.clone(),
            remote,
            who: super::devices::label(ctx, &remote),
            tx,
            rx,
            calls: HashMap::new(),
            room: tokio::time::interval(Duration::from_secs(1)),
            marked: false,
        };
        for (contact, on) in held.map(|(_, calls)| calls).unwrap_or_default() {
            link.adopt(contact, on);
        }
        link
    }

    /// A call from the link before this one. At the end that's in it,
    /// pictures went missing, so a full one each way; or, ended meanwhile,
    /// the bridge is told.
    fn adopt(&mut self, contact: [u8; 32], on: OnLink) {
        let says = |signal: CallSignal| {
            let _ = self.tx.send(Through { contact, call: LinkCall::Signal(signal) });
        };
        if !on.live() {
            if !on.bridge {
                says(CallSignal::Hangup);
            }
            return;
        }
        if !on.bridge {
            log!(self.ctx, "call through your device {}: carried on over a new link", self.who);
            for small in [false, true] {
                let _ = on.call_in.send(CallSignal::KeyframeRequest { small });
                says(CallSignal::KeyframeRequest { small });
            }
        }
        self.calls.insert(contact, on);
        self.mark();
    }

    /// Keeps `Ctx::links_in_calls` right as calls come and go.
    fn mark(&mut self) {
        let in_call = !self.calls.is_empty();
        if in_call != self.marked {
            self.marked = in_call;
            let mut links = self.ctx.links_in_calls.locked();
            if in_call {
                links.insert(self.remote);
            } else {
                links.remove(&self.remote);
            }
        }
    }

    /// Cancel-safe.
    pub async fn next(&mut self) -> Wake {
        tokio::select! {
            Some(through) = self.rx.recv() => Wake::Send(through),
            _ = self.room.tick() => Wake::Room,
        }
    }

    pub async fn on_wake(&mut self, conn: &mut AuthenticatedConnection, wake: Wake) -> Result<()> {
        let result = match wake {
            Wake::Send(Through { contact, call }) => self.on_call(conn, contact, call).await,
            Wake::Room => {
                self.go_direct();
                self.tell_room(conn).await
            }
        };
        self.mark();
        result
    }

    async fn on_call(&mut self, conn: &mut AuthenticatedConnection, contact: [u8; 32], call: LinkCall) -> Result<()> {
        match call {
            LinkCall::Attach { tx, budget, bridge, keep } => {
                if let Some(keep) = &keep {
                    protocol::update_call_budget(conn, keep);
                }
                let keep = keep.map(|keep| (keep, crate::call::UncoveredCall::new()));
                self.calls.insert(contact, OnLink::new(tx, budget, bridge, keep));
                Ok(())
            }
            LinkCall::Room(budget) => {
                if let Some(on) = self.calls.get_mut(&contact) {
                    on.budget = Some(budget);
                }
                Ok(())
            }
            LinkCall::Adopt(on) => {
                self.adopt(contact, *on);
                Ok(())
            }
            LinkCall::Signal(signal) => {
                if signal.media_bytes() > 0 {
                    self.ctx.call_media();
                }
                if let Some(budget) = self.calls.get(&contact).filter(|on| !on.bridge).and_then(|on| on.budget.as_ref()) {
                    budget.sent.fetch_add(signal.media_bytes() as u64, Ordering::Relaxed);
                }
                if ends_call(&signal) {
                    self.calls.remove(&contact);
                }
                protocol::send_call_through(conn, &contact, signal).await
            }
        }
    }

    /// A signal from the other end of the link, of a call with peer
    /// `contact`.
    pub async fn on_signal(&mut self, conn: &mut AuthenticatedConnection, contact: [u8; 32], signal: CallSignal) -> Result<()> {
        if signal.media_bytes() > 0 {
            self.ctx.call_media();
        }
        let result = self.signal_in(conn, contact, signal).await;
        self.mark();
        result
    }

    async fn signal_in(&mut self, conn: &mut AuthenticatedConnection, contact: [u8; 32], signal: CallSignal) -> Result<()> {
        let takeover = matches!(signal, CallSignal::Takeover);
        let ring = signal.is_ring();
        if let Some(on) = self.calls.get(&contact).filter(|on| on.live()) {
            if ring || takeover {
                return protocol::send_call_through(conn, &contact, CallSignal::Reject).await;
            }
            if let Some(budget) = on.budget.as_ref().filter(|_| !on.bridge) {
                budget.received.fetch_add(signal.media_bytes() as u64, Ordering::Relaxed);
            }
            let signal = match (on.bridge, standing_name(&self.ctx, &contact)) {
                (false, Some(name)) => match super::callcarry::front_end_takes(&self.ctx, &name, signal) {
                    Some(signal) => signal,
                    None => return Ok(()),
                },
                _ => signal,
            };
            let ends = ends_call(&signal);
            let _ = on.call_in.send(signal);
            if ends {
                self.calls.remove(&contact);
            }
            return Ok(());
        }
        if takeover {
            let placed = (!super::devices::is_mine(&self.ctx, &contact) && super::proxy::online_here(&self.ctx, &contact) && super::devices::relaying_here(&self.ctx)).then(|| place(&self.ctx, self.remote, contact, signal)).flatten();
            return match placed {
                Some(tx) => {
                    self.calls.insert(contact, OnLink::new(tx, None, true, None));
                    Ok(())
                }
                None => protocol::send_call_through(conn, &contact, CallSignal::Reject).await,
            };
        }
        if !ring {
            return Ok(());
        }
        if super::devices::is_mine(&self.ctx, &contact) && super::devices::linked_to(&self.ctx, &contact) {
            let joined = (super::devices::relaying_here(&self.ctx) && contact != self.remote).then(|| self.between(conn, contact, signal)).flatten();
            return match joined {
                Some(on) => {
                    self.calls.insert(contact, on);
                    Ok(())
                }
                None => protocol::send_call_through(conn, &contact, CallSignal::Reject).await,
            };
        }
        let here = super::proxy::online_here(&self.ctx, &contact);
        if here && super::devices::relaying_here(&self.ctx) {
            return match place(&self.ctx, self.remote, contact, signal) {
                Some(tx) => {
                    self.calls.insert(contact, OnLink::new(tx, None, true, None));
                    Ok(())
                }
                None => protocol::send_call_through(conn, &contact, CallSignal::Reject).await,
            };
        }
        if here {
            return protocol::send_call_through(conn, &contact, CallSignal::Reject).await;
        }
        match self.ring_here(contact, signal) {
            Some(on) => {
                self.calls.insert(contact, on);
                Ok(())
            }
            None => protocol::send_call_through(conn, &contact, CallSignal::Reject).await,
        }
    }

    /// Peer `contact`'s ring (into a conference, or not), passed on by
    /// the device at the other end: rung here, unless there's a call
    /// through with them already.
    fn ring_here(&self, contact: [u8; 32], ring: CallSignal) -> Option<OnLink> {
        if !ring.is_ring() {
            return None;
        }
        let from = standing_name(&self.ctx, &contact)?;
        let through = through_call(&self.ctx, contact);
        if through.current.locked().as_ref().is_some_and(|tx| !tx.is_closed()) {
            return None;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        *through.via.locked() = Some(self.remote);
        *through.current.locked() = Some(tx.clone());
        let link = CallLink::new(through.handle.commands.clone(), rx, through.handle.budget.clone());
        let asked = matches!(ring, CallSignal::ConferenceLink(_));
        self.ctx.call_events.send(super::callcarry::ring_event(from.clone(), link, ring)?).ok()?;
        if asked {
            log!(self.ctx, "{from} asks for our link in a conference, through your device {}", self.who);
        } else {
            log!(self.ctx, "{from} rings, through your device {}", self.who);
        }
        Some(OnLink::new(tx, Some(through.handle.budget.clone()), false, None))
    }

    /// The device at the other end of this link rings `to`, another of
    /// the user's, that it has no link to and this one has: the two links
    /// joined, and `to` rung. This link's side of the call, or `None` if
    /// the link to `to` has just gone.
    fn between(&self, conn: &AuthenticatedConnection, to: [u8; 32], ring: CallSignal) -> Option<OnLink> {
        let far = self.ctx.link_calls.locked().get(&to).cloned()?;
        let (this_room, far_room) = (CallBudget::new(), CallBudget::new());
        protocol::update_call_budget(conn, &this_room);
        let (out, gathered) = mpsc::unbounded_channel();
        let (this_tx, this_rx) = mpsc::unbounded_channel();
        gather(0, this_rx, Some, &out);
        let (far_tx, far_rx) = mpsc::unbounded_channel();
        gather(1, far_rx, Some, &out);
        far.send(Through { contact: self.remote, call: LinkCall::Attach { tx: far_tx, budget: Some(this_room.clone()), bridge: true, keep: Some(far_room.clone()) } }).ok()?;
        let parties = vec![Party::Link(self.ctx.clone(), to, self.remote), Party::Link(self.ctx.clone(), self.remote, to)];
        parties[1].send(ring);
        let target = super::devices::label(&self.ctx, &to);
        log!(self.ctx, "your device {} calls your device {target} through this one", self.who);
        tokio::spawn(bridge(self.ctx.clone(), format!("your device {target}"), parties, gathered, None));
        Some(OnLink::new(this_tx, Some(far_room), true, Some((this_room, crate::call::UncoveredCall::new()))))
    }

    /// What the bridge's connection to the peer carries, and whether it
    /// has cover, given; or what this side's call wants, and how many
    /// others' voices it passes on.
    pub fn on_room(&mut self, contact: [u8; 32], room: (u32, u32, u32), covered: bool, video_on: bool, bigger: u32, relayed: u32) {
        let Some(on) = self.calls.get_mut(&contact) else { return };
        if on.bridge {
            if let Some(budget) = &on.budget {
                budget.video_on.store(video_on, Ordering::Relaxed);
                budget.video_bigger.store(bigger as usize, Ordering::Relaxed);
                budget.relayed.store(relayed as usize, Ordering::Relaxed);
            }
        } else {
            on.room = Some((room, covered));
        }
    }

    /// Once a second, for each call through the link: the bridge says
    /// what its connection carries; the other side takes the least of
    /// that and what the link carries, and says what its call wants.
    async fn tell_room(&mut self, conn: &mut AuthenticatedConnection) -> Result<()> {
        self.calls.retain(|_, on| on.live());
        let mut notices = Vec::new();
        for (contact, on) in &mut self.calls {
            if let Some((keep, uncovered)) = &mut on.keep {
                if conn.cover_out.is_some() {
                    protocol::update_call_budget(conn, keep);
                } else {
                    protocol::set_uncovered_call_budget(keep, uncovered, conn);
                }
            }
            let Some(budget) = &on.budget else { continue };
            let load = |a: &std::sync::atomic::AtomicUsize| a.load(Ordering::Relaxed) as u32;
            if on.bridge {
                let room = (load(&budget.audio), load(&budget.video_bps), load(&budget.video_piece));
                let covered = budget.covered.load(Ordering::Relaxed);
                notices.push(Notice::CallRoom { contact: *contact, audio: room.0, video_bps: room.1, video_piece: room.2, video_on: false, bigger: 0, relayed: 0, covered });
                continue;
            }
            fit(conn, budget, on.room);
            let (video_on, bigger, relayed) = (budget.video_on.load(Ordering::Relaxed), load(&budget.video_bigger), load(&budget.relayed));
            notices.push(Notice::CallRoom { contact: *contact, audio: 0, video_bps: 0, video_piece: 0, video_on, bigger, relayed, covered: false });
        }
        for notice in notices {
            protocol::send_notice(conn, notice).await?;
        }
        Ok(())
    }

    /// Calls here with a peer through this link (not as the bridge, and
    /// not with another of the user's devices) move onto this device's own
    /// connection to them, once it's been up for `DIRECT_STEADY`.
    fn go_direct(&mut self) {
        for (contact, on) in &mut self.calls {
            if on.bridge || !on.live() || !super::proxy::online_here(&self.ctx, contact) || super::devices::is_mine(&self.ctx, contact) {
                on.direct_since = None;
                continue;
            }
            let since = *on.direct_since.get_or_insert_with(Instant::now);
            if since.elapsed() < DIRECT_STEADY || on.direct_tried.is_some_and(|at| at.elapsed() < DIRECT_RETRY) {
                continue;
            }
            on.direct_tried = Some(Instant::now());
            move_to_direct(&self.ctx, *contact, on.call_in.clone(), self.remote);
        }
    }

    /// The link's gone: the calls through it go to the link that's
    /// replaced it, or are held for one.
    pub fn gone(&mut self) {
        self.calls.retain(|_, on| on.live());
        let calls = std::mem::take(&mut self.calls);
        self.mark();
        let mut links = self.ctx.link_calls.locked();
        let newer = match links.get(&self.remote) {
            Some(tx) if tx.same_channel(&self.tx) => {
                links.remove(&self.remote);
                None
            }
            other => other.cloned(),
        };
        if calls.is_empty() {
            return;
        }
        if let Some(link) = newer {
            for (contact, on) in calls {
                let _ = link.send(Through { contact, call: LinkCall::Adopt(Box::new(on)) });
            }
            return;
        }
        log!(self.ctx, "link to your device {} lost in a call through it — holding it for a new one", self.who);
        let hold = self.ctx.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let wait = if calls.values().any(|on| on.bridge) { BRIDGE_HOLD } else { super::callcarry::CALL_HOLD };
        self.ctx.held_link_calls.locked().insert(self.remote, (hold, calls));
        drop(links);
        tokio::spawn(move_held(self.ctx.clone(), self.remote, hold));
        let (ctx, remote, who) = (self.ctx.clone(), self.remote, self.who.clone());
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let over = {
                let mut held = ctx.held_link_calls.locked();
                if held.get(&remote).is_some_and(|(h, _)| *h == hold) { held.remove(&remote) } else { None }
            };
            let Some((_, calls)) = over else { return };
            log!(ctx, "no new link to your device {who} in time — the calls through it are over");
            for (contact, on) in calls {
                if !on.bridge
                    && let Some(through) = ctx.through_calls.locked().get(&contact)
                    && through.via.locked().as_ref() == Some(&remote)
                {
                    *through.current.locked() = None;
                }
            }
        });
    }
}

/// The way a call with peer `user` could go now the link to `lost` has:
/// `Some(None)` direct, `Some(Some(device))` through another device.
fn another_way(ctx: &Ctx, user: &[u8; 32], lost: &[u8; 32]) -> Option<Option<[u8; 32]>> {
    if super::proxy::online_here(ctx, user) {
        return Some(None);
    }
    super::proxy::through(ctx, user).filter(|device| device != lost).map(Some)
}

/// The calls held from the link to `lost` that this device is in go
/// another way, if there is one, once it's had `MOVE_AFTER` to come back.
async fn move_held(ctx: Arc<Ctx>, lost: [u8; 32], hold: u64) {
    tokio::time::sleep(MOVE_AFTER).await;
    loop {
        let contacts: Vec<[u8; 32]> = match ctx.held_link_calls.locked().get(&lost) {
            Some((h, calls)) if *h == hold => calls.iter().filter(|(_, on)| !on.bridge && on.live()).map(|(contact, _)| *contact).collect(),
            _ => return,
        };
        let contacts: Vec<[u8; 32]> = contacts.into_iter().filter(|contact| !super::devices::is_mine(&ctx, contact)).collect();
        if contacts.is_empty() {
            return;
        }
        for contact in contacts {
            if ctx.through_calls.locked().get(&contact).is_some_and(|through| through.moving.load(Ordering::Relaxed)) {
                continue;
            }
            let Some(way) = another_way(&ctx, &contact, &lost) else { continue };
            let on = match ctx.held_link_calls.locked().get_mut(&lost) {
                Some((h, calls)) if *h == hold => calls.remove(&contact),
                _ => return,
            };
            if let Some(on) = on {
                move_call(&ctx, contact, on, way);
            }
        }
        tokio::time::sleep(MOVE_RECHECK).await;
    }
}

/// A held call with peer `user` goes `way` (see `another_way`).
fn move_call(ctx: &Arc<Ctx>, user: [u8; 32], on: OnLink, way: Option<[u8; 32]>) {
    let through = through_call(ctx, user);
    let Some(device) = way else {
        take_over_direct(ctx, user, &on, &through);
        return;
    };
    let link = ctx.link_calls.locked().get(&device).cloned();
    let Some(link) = link else {
        *through.current.locked() = None;
        return;
    };
    log!(ctx, "call through another device: carrying on through your device {}", super::devices::label(ctx, &device));
    *through.via.locked() = Some(device);
    let _ = link.send(Through { contact: user, call: LinkCall::Adopt(Box::new(on)) });
    let _ = link.send(Through { contact: user, call: LinkCall::Signal(CallSignal::Takeover) });
}

/// A held call goes on this device's own connection to peer `user`:
/// passed between the front end's side of the call and whichever of their
/// devices has it.
fn take_over_direct(ctx: &Arc<Ctx>, user: [u8; 32], on: &OnLink, through: &ThroughCall) {
    let Some(name) = standing_name(ctx, &user) else {
        *through.current.locked() = None;
        return;
    };
    let (out, gathered) = mpsc::unbounded_channel();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    gather(0, commands_rx, |c| if let CallCommand::Signal(s) = c { Some(s) } else { None }, &out);
    let mut parties = vec![Party::Here(on.call_in.clone(), ctx.clone(), name.clone())];
    ring_devices(ctx, &user, &CallSignal::Takeover, &out, &mut parties);
    log!(ctx, "call with {name}: carrying on over this device's own connection", name = ctx.shown(&name));
    *through.via.locked() = None;
    *through.direct.locked() = Some(commands_tx);
    tokio::spawn(bridge(ctx.clone(), name, parties, gathered, Some(TakingOver { mirror: Some(through.handle.budget.clone()), carried: None, not_carried: None })));
}

/// A call here with peer `user`, through the link to `relay`, is carried
/// on over this device's own connection to them (`LinkCalls::go_direct`):
/// it goes on through the relay until one of their devices answers on
/// the direct connection, and only then is the relay let go (sent a
/// hang-up, which the peer's old connection, no longer having the call,
/// passes over). Not answered, it stays on the relay.
fn move_to_direct(ctx: &Arc<Ctx>, user: [u8; 32], call_in: mpsc::UnboundedSender<CallSignal>, relay: [u8; 32]) {
    let through = through_call(ctx, user);
    if through.via.locked().as_ref() != Some(&relay) || through.moving.load(Ordering::Relaxed) {
        return;
    }
    let Some(name) = standing_name(ctx, &user) else { return };
    let (out, gathered) = mpsc::unbounded_channel();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    gather(0, commands_rx, |c| if let CallCommand::Signal(s) = c { Some(s) } else { None }, &out);
    let mut parties = vec![Party::Here(call_in, ctx.clone(), name.clone())];
    ring_devices(ctx, &user, &CallSignal::Takeover, &out, &mut parties);
    if parties.len() == 1 {
        return;
    }
    let who = super::devices::label(ctx, &relay);
    log!(ctx, "call with {name}: connected to them directly, so the call through your device {who} moves onto that", name = ctx.shown(&name));
    through.moving.store(true, Ordering::Relaxed);
    let carried = {
        let (ctx, through, who) = (ctx.clone(), through.clone(), who.clone());
        move || {
            through.moving.store(false, Ordering::Relaxed);
            *through.via.locked() = None;
            *through.direct.locked() = Some(commands_tx);
            send_on_link(&ctx, &relay, user, LinkCall::Signal(CallSignal::Hangup));
            log!(ctx, "call with {}: on the direct connection now, no longer through your device {who}", ctx.shown(&standing_name(&ctx, &user).unwrap_or_default()));
        }
    };
    let not_carried = {
        let through = through.clone();
        move || through.moving.store(false, Ordering::Relaxed)
    };
    let taking_over = TakingOver { mirror: Some(through.handle.budget.clone()), carried: Some(Box::new(carried)), not_carried: Some(Box::new(not_carried)) };
    tokio::spawn(bridge(ctx.clone(), name, parties, gathered, Some(taking_over)));
}

/// Whether calls through the link to `device` are held for a new one.
pub(super) fn held_for(ctx: &Ctx, device: &[u8; 32]) -> bool {
    ctx.held_link_calls.locked().contains_key(device)
}

/// Whether any calls through a link are held for a new one.
pub(super) fn any_held(ctx: &Ctx) -> bool {
    !ctx.held_link_calls.locked().is_empty()
}

/// What a call through a link may send: what the link carries, less
/// what going through adds to each signal, and no more than the bridge's
/// connection to the peer carries (`room`), once it's said. Without
/// cover on the link, video follows the bridge's connection alone.
/// Covered if either the link or the bridge's connection is: either
/// way, there's no room past the slots for others' pictures.
fn fit(conn: &AuthenticatedConnection, budget: &CallBudget, room: Option<((u32, u32, u32), bool)>) {
    protocol::update_call_budget(conn, budget);
    let covered = conn.cover_out.is_some();
    let (audio, video_bps, piece) = (budget.audio.load(Ordering::Relaxed), budget.video_bps.load(Ordering::Relaxed), budget.video_piece.load(Ordering::Relaxed));
    let (mut audio, mut video_bps, mut piece) = if covered {
        (audio.saturating_sub(protocol::CALL_THROUGH_OVERHEAD).max(crate::call::MIN_AUDIO_BYTES), video_bps, piece.saturating_sub(protocol::CALL_THROUGH_OVERHEAD))
    } else {
        (audio, video_bps, piece)
    };
    if let Some(((their_audio, their_bps, their_piece), their_cover)) = room {
        if their_cover {
            budget.covered.store(true, Ordering::Relaxed);
        }
        audio = audio.min(their_audio as usize).max(crate::call::MIN_AUDIO_BYTES);
        video_bps = if covered { video_bps.min(their_bps as usize) } else { their_bps as usize };
        piece = piece.min(their_piece as usize);
    }
    if piece == 0 || video_bps == 0 {
        (video_bps, piece) = (0, 0);
    }
    budget.audio.store(audio, Ordering::Relaxed);
    budget.video_bps.store(video_bps, Ordering::Relaxed);
    budget.video_piece.store(piece, Ordering::Relaxed);
}

/// What peer `user`'s calls through another device are called here: the
/// first of their devices' entries, or theirs (see `peer_status`); for
/// another of the user's own devices, its call name.
pub(super) fn standing_name(ctx: &Ctx, user: &[u8; 32]) -> Option<String> {
    if super::devices::is_mine(ctx, user) {
        return Some(super::devices::call_name(user));
    }
    super::peerdevices::entries(ctx, user).into_iter().next().or_else(|| super::devices::peer_name(ctx, user))
}

/// Whether calls with peer `user` go through another of the user's
/// devices: this one isn't connected to them, and one of those reaches
/// them.
pub(super) fn goes_through(ctx: &Ctx, user: &[u8; 32]) -> bool {
    !super::proxy::online_here(ctx, user) && super::proxy::through(ctx, user).is_some()
}

/// Whether this device is in a call with peer `user` (or ringing) through
/// another of the user's devices, not yet moved onto a connection of its
/// own to them.
pub(super) fn in_call_through(ctx: &Ctx, user: &[u8; 32]) -> bool {
    ctx.through_calls.locked().get(user).is_some_and(|through| through.via.locked().is_some() && through.current.locked().as_ref().is_some_and(|tx| !tx.is_closed()))
}

/// Calls with peer `user` through another of the user's devices, at the
/// end that's in the call: the front end's way in, which link the call's
/// on, and the call's way in from that link, while it lasts.
pub(super) struct ThroughCall {
    handle: PeerCallHandle,
    via: Mutex<Option<[u8; 32]>>,
    current: Mutex<Option<mpsc::UnboundedSender<CallSignal>>>,
    /// The call's been carried on over this device's own connection:
    /// where its commands go instead (see `take_over_direct`).
    direct: Mutex<Option<mpsc::UnboundedSender<CallCommand>>>,
    /// It's being moved onto this device's own connection, the relay
    /// still carrying it meanwhile (see `move_to_direct`).
    moving: std::sync::atomic::AtomicBool,
}

fn through_call(ctx: &Arc<Ctx>, user: [u8; 32]) -> Arc<ThroughCall> {
    let mut calls = ctx.through_calls.locked();
    calls
        .entry(user)
        .or_insert_with(|| {
            let (commands, rx) = mpsc::unbounded_channel();
            let call = Arc::new(ThroughCall { handle: PeerCallHandle { commands, budget: CallBudget::new(), share: Arc::default() }, via: Mutex::default(), current: Mutex::default(), direct: Mutex::default(), moving: Default::default() });
            tokio::spawn(pump(Arc::downgrade(ctx), user, Arc::downgrade(&call), rx));
            call
        })
        .clone()
}

/// How the front end calls peer `user` through another of the user's
/// devices.
pub(super) fn handle(ctx: &Arc<Ctx>, user: [u8; 32]) -> PeerCallHandle {
    through_call(ctx, user).handle.clone()
}

/// Takes the front end's call commands for a call through another device
/// to the link it's on: a call placed goes on the link to the first
/// device by name that reaches them (`proxy::through`), and stays on it.
async fn pump(ctx: Weak<Ctx>, user: [u8; 32], call: Weak<ThroughCall>, mut commands: mpsc::UnboundedReceiver<CallCommand>) {
    while let Some(command) = commands.recv().await {
        let (Some(ctx), Some(call)) = (ctx.upgrade(), call.upgrade()) else { return };
        if matches!(command, CallCommand::Signal(_))
            && let Some(direct) = call.direct.locked().clone().filter(|direct| !direct.is_closed())
        {
            let _ = direct.send(command);
            continue;
        }
        let via = match &command {
            CallCommand::Attach(_) | CallCommand::Pass(_) => {
                *call.direct.locked() = None;
                let via = super::proxy::through(&ctx, &user);
                *call.via.locked() = via;
                via
            }
            CallCommand::Signal(_) => *call.via.locked(),
        };
        let Some(link) = via.and_then(|device| ctx.link_calls.locked().get(&device).cloned()) else { continue };
        let call = match command {
            CallCommand::Attach(tx) | CallCommand::Pass(tx) => {
                *call.current.locked() = Some(tx.clone());
                LinkCall::Attach { tx, budget: Some(call.handle.budget.clone()), bridge: false, keep: None }
            }
            CallCommand::Signal(signal) => LinkCall::Signal(signal),
        };
        let _ = link.send(Through { contact: user, call });
    }
}

/// One side of a call a bridge passes on.
enum Party {
    /// A peer's connection: the call's commands there.
    Peer(mpsc::UnboundedSender<CallCommand>, Arc<CallBudget>),
    /// The link to another of the user's devices, about the call with
    /// peer `contact`: whichever link it's on now (it may be replaced in
    /// the call).
    Link(Arc<Ctx>, [u8; 32], [u8; 32]),
    /// This device's front end, the call with peer `name`.
    Here(mpsc::UnboundedSender<CallSignal>, Arc<Ctx>, String),
}

impl Party {
    fn send(&self, signal: CallSignal) {
        match self {
            Party::Peer(commands, _) => {
                let _ = commands.send(CallCommand::Signal(signal));
            }
            Party::Link(ctx, contact, device) => send_on_link(ctx, device, *contact, LinkCall::Signal(signal)),
            Party::Here(tx, ctx, name) => {
                if let Some(signal) = super::callcarry::front_end_takes(ctx, name, signal) {
                    let _ = tx.send(signal);
                }
            }
        }
    }
}

/// Sends on the link to `device`, if there is one just now: a signal
/// sent meanwhile is lost, as it would be on a connection that's gone.
fn send_on_link(ctx: &Ctx, device: &[u8; 32], contact: [u8; 32], call: LinkCall) {
    if let Some(link) = ctx.link_calls.locked().get(device) {
        let _ = link.send(Through { contact, call });
    }
}

/// What comes from each party, into one stream (by its number); `None`
/// once it's gone. Stops once the bridge has.
fn gather<T: Send + 'static>(
    party: usize,
    mut rx: mpsc::UnboundedReceiver<T>,
    take: fn(T) -> Option<CallSignal>,
    out: &mpsc::UnboundedSender<(usize, Option<CallSignal>)>,
) {
    let out = out.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                got = rx.recv() => match got {
                    Some(got) => {
                        if let Some(signal) = take(got)
                            && out.send((party, Some(signal))).is_err()
                        {
                            return;
                        }
                    }
                    None => {
                        let _ = out.send((party, None));
                        return;
                    }
                },
                _ = out.closed() => return,
            }
        }
    });
}

/// Peer `user`, called `name` here, rings this device (`ring`: into a
/// conference, or not), which is let relay: rung here, and passed on to
/// the user's other devices this one is linked to. Where the peer's call
/// signals go from now on; `None` if there's nobody to pass it on to, so
/// it's rung here alone, as ever.
pub(super) fn fan(ctx: &Arc<Ctx>, name: &str, user: [u8; 32], call: &PeerCallHandle, ring: &CallSignal) -> Option<mpsc::UnboundedSender<CallSignal>> {
    if !super::devices::relaying_here(ctx) {
        return None;
    }
    if !ring.is_ring() {
        return None;
    }
    let links: Vec<([u8; 32], mpsc::UnboundedSender<Through>)> = {
        let siblings = ctx.siblings.locked();
        let mut links: Vec<_> = ctx.link_calls.locked().iter().filter(|(d, _)| siblings.contains_key(*d)).map(|(d, tx)| (*d, tx.clone())).collect();
        links.sort_by_key(|(d, _)| *d);
        links
    };
    if links.is_empty() {
        return None;
    }
    let (out, gathered) = mpsc::unbounded_channel();
    let (peer_tx, peer_rx) = mpsc::unbounded_channel();
    gather(0, peer_rx, Some, &out);
    let mut parties = vec![Party::Peer(call.commands.clone(), call.budget.clone())];
    let (here_tx, here_rx) = mpsc::unbounded_channel();
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let link = CallLink::new(commands_tx, here_rx, call.budget.clone());
    let event = super::callcarry::ring_event(name.to_string(), link, ring.clone())?;
    if ctx.call_events.send(event).is_ok() {
        gather(parties.len(), commands_rx, |c| if let CallCommand::Signal(s) = c { Some(s) } else { None }, &out);
        parties.push(Party::Here(here_tx, ctx.clone(), name.to_string()));
    }
    for (device, link) in links {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = link.send(Through { contact: user, call: LinkCall::Attach { tx, budget: Some(call.budget.clone()), bridge: true, keep: None } });
        gather(parties.len(), rx, Some, &out);
        let party = Party::Link(ctx.clone(), user, device);
        party.send(ring.clone());
        parties.push(party);
    }
    let names: Vec<String> = parties.iter().filter_map(|p| if let Party::Link(_, _, d) = p { Some(super::devices::label(ctx, d)) } else { None }).collect();
    log!(ctx, "{name} rings: passed on to your {} {}", if names.len() == 1 { "device" } else { "devices" }, names.join(", "), name = ctx.shown(name));
    tokio::spawn(bridge(ctx.clone(), name.to_string(), parties, gathered, None));
    Some(peer_tx)
}

/// Another of the user's devices, `device`, calls peer `user` through
/// this one, over `link` (`ring`: into a conference, or not): each of
/// their devices this one's connected to is rung. Where that device's
/// signals of the call go from now on; `None` if none can be.
fn place(ctx: &Arc<Ctx>, device: [u8; 32], user: [u8; 32], ring: CallSignal) -> Option<mpsc::UnboundedSender<CallSignal>> {
    let name = super::devices::peer_name(ctx, &user)?;
    let (out, gathered) = mpsc::unbounded_channel();
    let (link_tx, link_rx) = mpsc::unbounded_channel();
    gather(0, link_rx, Some, &out);
    let mut parties = vec![Party::Link(ctx.clone(), user, device)];
    ring_devices(ctx, &user, &ring, &out, &mut parties);
    if parties.len() == 1 {
        return None;
    }
    let taking_over = matches!(ring, CallSignal::Takeover).then_some(TakingOver { mirror: None, carried: None, not_carried: None });
    if taking_over.is_some() {
        log!(ctx, "your device {} carries a call with {name} on through this one", super::devices::label(ctx, &device), name = ctx.shown(&name));
    } else {
        log!(ctx, "your device {} calls {name} through this one", super::devices::label(ctx, &device), name = ctx.shown(&name));
    }
    tokio::spawn(bridge(ctx.clone(), name, parties, gathered, taking_over));
    Some(link_tx)
}

/// Each of peer `user`'s devices this one's connected to is sent `ring`,
/// and becomes a party to the call.
fn ring_devices(ctx: &Ctx, user: &[u8; 32], ring: &CallSignal, out: &mpsc::UnboundedSender<(usize, Option<CallSignal>)>, parties: &mut Vec<Party>) {
    let calls: Vec<PeerCallHandle> = {
        let peers = ctx.peers.locked();
        let theirs: Vec<_> = peers.values().filter(|e| e.watch.borrow().public_key == *user && e.online.load(Ordering::Relaxed)).collect();
        let devices: Vec<_> = theirs.iter().filter(|e| e.device.is_some()).collect();
        if devices.is_empty() { theirs.iter().map(|e| e.call.clone()).collect() } else { devices.iter().map(|e| e.call.clone()).collect() }
    };
    for call in calls {
        let (tx, rx) = mpsc::unbounded_channel();
        if call.commands.send(CallCommand::Pass(tx)).is_err() {
            continue;
        }
        gather(parties.len(), rx, Some, out);
        let party = Party::Peer(call.commands, call.budget);
        party.send(ring.clone());
        parties.push(party);
    }
}

/// A bridge carrying on a call from elsewhere (`CallSignal::Takeover`).
struct TakingOver {
    /// This device's side of the call is here: its budget, to keep up to
    /// date from the connection that answers.
    mirror: Option<Arc<CallBudget>>,
    /// The call's still going the way it was (`move_to_direct`): what's
    /// done once it's carried on here, and what's done if it isn't, which
    /// leaves the call as it was rather than ending it.
    carried: Option<Box<dyn FnOnce() + Send>>,
    not_carried: Option<Box<dyn FnOnce() + Send>>,
}

/// Passes a call on between party 0, who rang, and whichever of the rest
/// answers first; the others are told it was answered elsewhere, or, if
/// none of them will have it, the one who rang is refused. Over when
/// either hangs up or goes.
async fn bridge(ctx: Arc<Ctx>, name: String, parties: Vec<Party>, mut gathered: mpsc::UnboundedReceiver<(usize, Option<CallSignal>)>, mut taking_over: Option<TakingOver>) {
    let mut out = vec![false; parties.len()];
    out[0] = true;
    let mut answered: Option<usize> = None;
    let rest = |parties: &[Party], but: Option<usize>, out: &[bool], signal: CallSignal| {
        for (i, party) in parties.iter().enumerate().skip(1) {
            if Some(i) != but && !out[i] {
                party.send(signal.clone());
            }
        }
    };
    let give_up_at = Instant::now() + super::callcarry::CALL_HOLD;
    let mut again = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(1), Duration::from_secs(1));
    loop {
        let got = tokio::select! {
            got = gathered.recv() => got,
            _ = again.tick(), if taking_over.is_some() && answered.is_none() => {
                if Instant::now() >= give_up_at {
                    if let Some(not_carried) = taking_over.as_mut().and_then(|t| t.not_carried.take()) {
                        log!(ctx, "call with {name}: they didn't take it on over the direct connection, so it stays as it was", name = ctx.shown(&name));
                        rest(&parties, None, &out, CallSignal::Hangup);
                        not_carried();
                        return;
                    }
                    log!(ctx, "call with {name}: nobody answered for it — the call's over", name = ctx.shown(&name));
                    rest(&parties, None, &out, CallSignal::Hangup);
                    parties[0].send(CallSignal::Hangup);
                    return;
                }
                rest(&parties, None, &out, CallSignal::Takeover);
                continue;
            }
        };
        let Some((from, signal)) = got else { return };
        match answered {
            None => match (from, signal) {
                (0, None) => {
                    rest(&parties, None, &out, CallSignal::Hangup);
                    return;
                }
                (0, Some(signal)) if ends_call(&signal) => {
                    rest(&parties, None, &out, signal);
                    return;
                }
                (0, Some(_)) => {}
                (i, Some(CallSignal::Resume)) if taking_over.is_some() => {
                    answered = Some(i);
                    if let (Party::Link(ctx, contact, device), Party::Peer(_, budget)) = (&parties[0], &parties[i]) {
                        send_on_link(ctx, device, *contact, LinkCall::Room(budget.clone()));
                    }
                    if let (Party::Here(call_in, ..), Party::Peer(_, budget), Some(TakingOver { mirror: Some(mirror), .. })) = (&parties[0], &parties[i], &taking_over) {
                        super::callcarry::mirror_budget(budget.clone(), mirror.clone(), call_in.clone());
                    }
                    if let Some(carried) = taking_over.as_mut().and_then(|t| {
                        t.not_carried = None;
                        t.carried.take()
                    }) {
                        carried();
                    }
                    rest(&parties, Some(i), &out, CallSignal::AnsweredElsewhere);
                    log!(ctx, "call with {name}: carried on", name = ctx.shown(&name));
                }
                (i, Some(accept @ CallSignal::Accept { .. })) => {
                    answered = Some(i);
                    if let (Party::Link(ctx, contact, device), Party::Peer(_, budget)) = (&parties[0], &parties[i]) {
                        send_on_link(ctx, device, *contact, LinkCall::Room(budget.clone()));
                    }
                    parties[0].send(accept);
                    rest(&parties, Some(i), &out, CallSignal::AnsweredElsewhere);
                    match &parties[i] {
                        Party::Link(_, _, device) => log!(ctx, "call with {name}: answered on your device {}", super::devices::label(&ctx, device), name = ctx.shown(&name)),
                        Party::Here(..) => log!(ctx, "call with {name}: answered here", name = ctx.shown(&name)),
                        Party::Peer(..) => log!(ctx, "call with {name}: answered, through this device", name = ctx.shown(&name)),
                    }
                }
                (i, None | Some(CallSignal::Reject | CallSignal::Hangup)) => {
                    out[i] = true;
                    if out[1..].iter().all(|o| *o) {
                        if let Some(not_carried) = taking_over.as_mut().and_then(|t| t.not_carried.take()) {
                            log!(ctx, "call with {name}: the direct connection didn't take it on, so it stays as it was", name = ctx.shown(&name));
                            not_carried();
                            return;
                        }
                        parties[0].send(CallSignal::Reject);
                        return;
                    }
                }
                _ => {}
            },
            Some(answered) => match (from, signal) {
                (0, None) => {
                    parties[answered].send(CallSignal::Hangup);
                    return;
                }
                (0, Some(signal)) => {
                    let ends = ends_call(&signal);
                    parties[answered].send(signal);
                    if ends {
                        return;
                    }
                }
                (i, None) if i == answered => {
                    parties[0].send(CallSignal::Hangup);
                    return;
                }
                (i, Some(signal)) if i == answered => {
                    let ends = ends_call(&signal);
                    parties[0].send(signal);
                    if ends {
                        return;
                    }
                }
                (i, Some(CallSignal::Accept { .. })) => parties[i].send(CallSignal::Hangup),
                _ => {}
            },
        }
    }
}
