//! A call carried on a connection: a peer's (`peer`), or a link to
//! another of the user's devices (`devices`). `CallCarry` is what lasts
//! from one connection to the next (the call's way in from the front
//! end, and a call held while a new connection comes); `CallOn`, what's
//! for one connection only.

use crate::lock::Locked;
use super::{CallEvent, Ctx, PeerCallHandle};
use crate::call::{CallCommand, CallLink, CallSignal};
use crate::protocol;
use crate::transport::AuthenticatedConnection;
use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A call whose connection goes is held this long for a new connection
/// to the same peer to carry it on (see `CallSignal::Resume`) — one side
/// moving to another network, say — before it ends.
pub(super) const CALL_HOLD: Duration = Duration::from_secs(10);
/// Carried onto a new connection, a call waits this long for the other
/// side to say it still has it too, and ends if it doesn't.
const RESUME_WAIT: Duration = Duration::from_secs(6);
/// In a call, how long without hearing them a write to the connection
/// may wait before the connection's taken as gone.
const CALL_STALL: Duration = Duration::from_secs(2);

/// A call whose connection went, waiting for the next one to carry it
/// on (`CALL_HOLD`): which hold this is — a timer drops it, ending the
/// call, if it's still the same hold then — and its signals' way in.
type HeldCall = Arc<std::sync::Mutex<Option<(u64, mpsc::UnboundedSender<CallSignal>)>>>;

/// How long a connection's asked to let go of a call counts for (see
/// `CallOn::take_over`): it's looked at once a second.
const GIVE_UP_ASKED: Duration = Duration::from_secs(2);

/// What another connection of the same user needs to take a call over
/// from this one (`CallOn::take_over`).
#[derive(Default)]
pub(super) struct CallShare {
    held: HeldCall,
    /// Of the held call: whether it's another of the user's devices'.
    passed_on: std::sync::atomic::AtomicBool,
    /// Where the call's commands go, once it's been taken over.
    moved: std::sync::Mutex<Option<mpsc::UnboundedSender<CallCommand>>>,
    /// When the connection with the call was asked to let go of it.
    give_up_asked: std::sync::Mutex<Option<Instant>>,
}

/// Sends a connection's call commands on to the connection that's taken
/// the call over, while it's that call; a new one is this connection's.
async fn route_commands(mut commands: mpsc::UnboundedReceiver<CallCommand>, own: mpsc::UnboundedSender<CallCommand>, share: Arc<CallShare>) {
    while let Some(command) = commands.recv().await {
        let moved = share.moved.locked().clone().filter(|to| !to.is_closed());
        match (moved, &command) {
            (Some(to), CallCommand::Signal(signal)) => {
                if matches!(signal, CallSignal::Hangup | CallSignal::Reject | CallSignal::AnsweredElsewhere) {
                    *share.moved.locked() = None;
                }
                let _ = to.send(command);
            }
            (moved, _) => {
                if moved.is_some() {
                    *share.moved.locked() = None;
                }
                let _ = own.send(command);
            }
        }
    }
}

/// Keeps a call's budget up to date from the connection it's gone to
/// (`from`), and that connection told what the call wants, until the
/// call's over.
pub(super) fn mirror_budget(from: Arc<crate::call::CallBudget>, to: Arc<crate::call::CallBudget>, call_in: mpsc::UnboundedSender<CallSignal>) {
    tokio::spawn(async move {
        let (mut sent, mut received) = (from.sent.load(Ordering::Relaxed), from.received.load(Ordering::Relaxed));
        while !call_in.is_closed() {
            to.audio.store(from.audio.load(Ordering::Relaxed), Ordering::Relaxed);
            to.video_bps.store(from.video_bps.load(Ordering::Relaxed), Ordering::Relaxed);
            to.video_piece.store(from.video_piece.load(Ordering::Relaxed), Ordering::Relaxed);
            to.covered.store(from.covered.load(Ordering::Relaxed), Ordering::Relaxed);
            to.round_trip.store(from.round_trip.load(Ordering::Relaxed), Ordering::Relaxed);
            from.video_on.store(to.video_on.load(Ordering::Relaxed), Ordering::Relaxed);
            from.video_bigger.store(to.video_bigger.load(Ordering::Relaxed), Ordering::Relaxed);
            from.relayed.store(to.relayed.load(Ordering::Relaxed), Ordering::Relaxed);
            let (now_sent, now_received) = (from.sent.load(Ordering::Relaxed), from.received.load(Ordering::Relaxed));
            to.sent.fetch_add(now_sent - sent, Ordering::Relaxed);
            to.received.fetch_add(now_received - received, Ordering::Relaxed);
            (sent, received) = (now_sent, now_received);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

/// What lasts of calls with one other end from one connection to the
/// next.
pub(super) struct CallCarry {
    /// Ours, for handing to an incoming call's `CallLink`.
    pub handle: PeerCallHandle,
    pub commands: mpsc::UnboundedReceiver<CallCommand>,
    held: HeldCall,
    holds: u64,
    /// When their call audio was last heard — on this connection or the
    /// last, for a held call — and `None` from a new call until it's
    /// answered. A call only counts as under way, for the rules that
    /// judge a connection by how recently it's been heard, while this is
    /// recent: ringing sends nothing.
    pub audio_at: Option<Instant>,
    /// The call's another of the user's devices', passed on by this one
    /// (`proxycall`): all its signals go to it as they are.
    passed_on: bool,
    /// What the last call's video, without cover, was seen to carry, and
    /// on what path (the two ends' addresses): the next call on the same
    /// path starts near it (`call::UncoveredCall::starting_at`). Only
    /// while Swarm runs: nothing's kept of who was called.
    carried: std::sync::Mutex<Option<(CallPath, u32)>>,
}

/// A connection's two ends' addresses: another network, at either end,
/// is another path, which the last call's rate says nothing of.
type CallPath = (std::net::IpAddr, Option<std::net::IpAddr>);

fn call_path(conn: &AuthenticatedConnection) -> CallPath {
    (conn.connection.remote_address().ip(), conn.connection.local_ip())
}

impl CallCarry {
    pub fn new() -> CallCarry {
        let (own, commands) = mpsc::unbounded_channel();
        let share = Arc::new(CallShare::default());
        let (commands_tx, routed) = mpsc::unbounded_channel();
        tokio::spawn(route_commands(routed, own, share.clone()));
        let held = share.held.clone();
        let handle = PeerCallHandle { commands: commands_tx, budget: crate::call::CallBudget::new(), share };
        CallCarry { handle, commands, held, holds: 0, audio_at: None, passed_on: false, carried: std::sync::Mutex::new(None) }
    }

    /// A call's held for the next connection.
    pub fn holding(&self) -> bool {
        self.held.locked().is_some()
    }

    /// A new connection's: a call held from the last carries on on it,
    /// once the other side says it still has it too (`Resume`). Call
    /// commands from while there was none (`was_online` false) are
    /// stale, so they're dropped — before they're shown online: a call
    /// placed the moment they are (a ring) is this connection's. Not on
    /// a switch straight to one they've just made (they stay online):
    /// one placed meanwhile is too. A held call's audio and pictures from
    /// while there was no connection are stale all the same — but a
    /// hang-up meanwhile ends it.
    pub fn begin(&mut self, was_online: bool, conn: &AuthenticatedConnection) -> CallOn {
        let mut call_in: Option<mpsc::UnboundedSender<CallSignal>> = self.held.locked().take().map(|(_, tx)| tx).filter(|tx| !tx.is_closed());
        while let Some(command) = if was_online { None } else { self.commands.try_recv().ok() } {
            if matches!(command, CallCommand::Signal(CallSignal::Hangup | CallSignal::Reject)) {
                call_in = None;
            }
        }
        let resume_by = call_in.as_ref().map(|_| tokio::time::Instant::now() + RESUME_WAIT);
        protocol::update_call_budget(conn, &self.handle.budget);
        CallOn {
            call_in,
            resume_by,
            uncovered: crate::call::UncoveredCall::starting_at(self.carried.locked().filter(|(on, _)| *on == call_path(conn)).map(|(_, carried)| carried)),
            pictures_held: [false; 2],
            gap: Duration::ZERO,
            asked_logged: None,
            were_asked_logged: None,
        }
    }

    /// The connection's gone: a call on it is held for the next one
    /// (`CALL_HOLD`); if none comes in time it ends — its `CallLink` sees
    /// the channel close.
    pub fn hold(&mut self, on: &mut CallOn, ctx: &Arc<Ctx>, name: &str) {
        let Some(tx) = on.call_in.take().filter(|tx| !tx.is_closed()) else { return };
        log!(ctx, "{name}: connection lost in a call — holding it for a new one", name = ctx.shown(name));
        self.holds += 1;
        self.handle.share.passed_on.store(self.passed_on, Ordering::Relaxed);
        *self.held.locked() = Some((self.holds, tx));
        let (held, hold, ctx, name) = (self.held.clone(), self.holds, ctx.clone(), name.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(CALL_HOLD).await;
            let mut held = held.locked();
            if held.as_ref().is_some_and(|(h, _)| *h == hold) {
                *held = None;
                log!(ctx, "{name}: no new connection in time — the call's over", name = ctx.shown(&name));
            }
        });
    }
}

/// What a call command came to: sent, or the other side's dialled
/// afresh meanwhile (that connection, for whoever was waiting for one).
pub(super) enum Sent {
    Done,
    Switched(Box<AuthenticatedConnection>),
}

/// A call on one connection.
pub(super) struct CallOn {
    /// Where the other side's call signals go: the one live (or ringing)
    /// call on this connection, if any. It's dropped with the
    /// connection, which ends that call's `CallLink` too.
    call_in: Option<mpsc::UnboundedSender<CallSignal>>,
    /// Set while a carried-on call waits for the other side's `Resume`.
    resume_by: Option<tokio::time::Instant>,
    /// Without cover, a call's voice and video follow what the link carries.
    uncovered: crate::call::UncoveredCall,
    /// Without cover, the link gone for a moment in a call: pictures
    /// aren't sent until a full one, once it's back (see `call::LINK_GONE`):
    /// the large pictures' stream, and the small's.
    pictures_held: [bool; 2],
    /// For the log, so a call's hiccups can be told apart afterwards:
    /// the longest it's been quiet, in a gap still going (see
    /// `call::LINK_STOPPED`), and when a full picture was last asked for
    /// each way (once a second at most).
    gap: Duration,
    asked_logged: Option<Instant>,
    were_asked_logged: Option<Instant>,
}

impl CallOn {
    /// Whether there's a call on this connection, whoever holds it still
    /// holding it.
    pub fn live(&self) -> bool {
        self.call_in.as_ref().is_some_and(|tx| !tx.is_closed())
    }

    /// A call on this connection that something else answers for (a
    /// ring passed on to the user's other devices, see `proxycall::fan`):
    /// its signals from the other side go to `tx`.
    pub fn take_call(&mut self, carry: &mut CallCarry, tx: mpsc::UnboundedSender<CallSignal>) {
        self.call_in = Some(tx);
        carry.audio_at = None;
        carry.passed_on = true;
    }

    /// The connection's settled: a call carried on from the last one is
    /// said so, first.
    pub async fn settle(&self, conn: &mut AuthenticatedConnection) {
        if self.resume_by.is_some() {
            let _ = protocol::send_call(conn, CallSignal::Resume).await;
        }
    }

    /// Something from our side of the call: a call attached to this
    /// connection, or a signal to send. `inbound`: where a fresh
    /// connection from them would come, which means they've moved (in a
    /// call, a write waits no longer for this one then).
    pub async fn on_command(
        &mut self,
        carry: &mut CallCarry,
        conn: &mut AuthenticatedConnection,
        ctx: &Arc<Ctx>,
        name: &str,
        command: CallCommand,
        inbound: Option<&mut mpsc::Receiver<AuthenticatedConnection>>,
    ) -> Result<Sent> {
        match command {
            CallCommand::Attach(tx) => {
                self.call_in = Some(tx);
                carry.audio_at = None;
                carry.passed_on = false;
                Ok(Sent::Done)
            }
            CallCommand::Pass(tx) => {
                self.call_in = Some(tx);
                carry.audio_at = None;
                carry.passed_on = true;
                Ok(Sent::Done)
            }
            CallCommand::Signal(signal) => self.send(carry, conn, ctx, name, signal, inbound).await,
        }
    }

    /// One signal from our side of the call, sent.
    async fn send(
        &mut self,
        carry: &mut CallCarry,
        conn: &mut AuthenticatedConnection,
        ctx: &Arc<Ctx>,
        name: &str,
        signal: CallSignal,
        mut inbound: Option<&mut mpsc::Receiver<AuthenticatedConnection>>,
    ) -> Result<Sent> {
        if signal.media_bytes() > 0 {
            ctx.call_media();
        }
        let ends_call = matches!(signal, CallSignal::Hangup | CallSignal::Reject | CallSignal::AnsweredElsewhere);
        let heard = conn.last_heard;
        let quiet = conn.quiet();
        if quiet > crate::call::LINK_STOPPED {
            self.gap = self.gap.max(quiet);
        } else if !self.gap.is_zero() {
            log!(ctx, "{name}: nothing heard for {:.1}s in a call, then back", self.gap.as_secs_f64(), name = ctx.shown(name));
            self.gap = Duration::ZERO;
        }
        if let CallSignal::KeyframeRequest { small } = signal
            && self.asked_logged.is_none_or(|at: Instant| at.elapsed() > Duration::from_secs(1))
        {
            log!(ctx, "{name}: asked for a full picture of their {}", if small { "small" } else { "large" }, name = ctx.shown(name));
            self.asked_logged = Some(Instant::now());
        }
        if let (None, CallSignal::Video(piece)) = (&conn.cover_out, &signal) {
            if quiet > crate::call::LINK_GONE {
                self.pictures_held = [true; 2];
                return Ok(Sent::Done);
            }
            let held = &mut self.pictures_held[piece.small as usize];
            if *held && !(piece.key && piece.index == 0) {
                if let Some(tx) = &self.call_in {
                    let _ = tx.send(CallSignal::KeyframeRequest { small: piece.small });
                }
                return Ok(Sent::Done);
            }
            *held = false;
        }
        carry.handle.budget.sent.fetch_add(signal.media_bytes() as u64, Ordering::Relaxed);
        let in_call = carry.audio_at.is_some();
        let send = protocol::send_call(conn, signal);
        tokio::pin!(send);
        let sent = loop {
            tokio::select! {
                sent = &mut send => break sent.map(|()| Sent::Done),
                Some(fresh) = async { match inbound.as_mut() { Some(inbound) => inbound.recv().await, None => std::future::pending().await } }, if in_call => {
                    break Ok(Sent::Switched(Box::new(fresh)));
                }
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if in_call && heard.elapsed() > CALL_STALL {
                        break Err(anyhow::anyhow!("nothing heard for {}s, and nothing more can be sent", CALL_STALL.as_secs()));
                    }
                }
            }
        };
        if ends_call {
            self.call_in = None;
        }
        sent
    }

    /// A call signal from the other side, `name` here: a ring, a call
    /// carried on, a conference's or a shared screen's business for the
    /// front end, or the rest of the call's, for our side of it.
    pub async fn on_signal(&mut self, carry: &mut CallCarry, conn: &mut AuthenticatedConnection, ctx: &Arc<Ctx>, name: &str, signal: CallSignal) -> Result<()> {
        if signal.media_bytes() > 0 {
            ctx.call_media();
        }
        match signal {
            invite if invite.is_ring() => self.on_invite(carry, conn, ctx, name, invite).await,
            CallSignal::Resume if !carry.passed_on || self.resume_by.is_some() => self.on_resume(conn, ctx, name).await,
            CallSignal::Takeover => Ok(()),
            signal if carry.passed_on && self.live() => {
                self.on_call_signal(carry, ctx, name, signal);
                Ok(())
            }
            signal => {
                if let Some(signal) = front_end_takes(ctx, name, signal) {
                    self.on_call_signal(carry, ctx, name, signal);
                }
                Ok(())
            }
        }
    }

    async fn on_invite(&mut self, carry: &mut CallCarry, conn: &mut AuthenticatedConnection, ctx: &Arc<Ctx>, name: &str, invite: CallSignal) -> Result<()> {
        if self.live() {
            return protocol::send_call(conn, CallSignal::Reject).await;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let link = CallLink::new(carry.handle.commands.clone(), rx, carry.handle.budget.clone());
        let event = ring_event(name.to_string(), link, invite).expect("a ring");
        match ctx.call_events.send(event) {
            Ok(()) => {
                self.call_in = Some(tx);
                carry.audio_at = None;
                carry.passed_on = false;
                Ok(())
            }
            Err(_) => protocol::send_call(conn, CallSignal::Reject).await,
        }
    }

    /// Another device of `user`'s says this connection's taking over the
    /// call another of theirs had: if one of their connections here is
    /// holding it, it carries on on this one, and they're told. One
    /// still in it (its way to them hasn't been missed yet, or the relay
    /// at the other end is still there with nobody to pass it to) is
    /// asked to let go, for the next time they say so.
    pub async fn take_over(&mut self, carry: &mut CallCarry, conn: &mut AuthenticatedConnection, ctx: &Arc<Ctx>, name: &str, user: &[u8; 32]) -> Result<()> {
        if self.live() {
            return Ok(());
        }
        let others: Vec<PeerCallHandle> = ctx.peers.locked().values().filter(|e| e.watch.borrow().public_key == *user && !Arc::ptr_eq(&e.call.share, &carry.handle.share)).map(|e| e.call.clone()).collect();
        let found = others.iter().find_map(|other| {
            let tx = other.share.held.locked().take().map(|(_, tx)| tx).filter(|tx| !tx.is_closed())?;
            Some((other, tx))
        });
        let Some((other, tx)) = found else {
            for other in &others {
                *other.share.give_up_asked.locked() = Some(Instant::now());
            }
            return Ok(());
        };
        log!(ctx, "{name}: call carried on by another of their devices", name = ctx.shown(name));
        *other.share.moved.locked() = Some(carry.handle.commands.clone());
        mirror_budget(carry.handle.budget.clone(), other.budget.clone(), tx.clone());
        carry.passed_on = other.share.passed_on.load(Ordering::Relaxed);
        carry.audio_at = Some(Instant::now());
        let _ = tx.send(CallSignal::KeyframeRequest { small: false });
        let _ = tx.send(CallSignal::KeyframeRequest { small: true });
        self.call_in = Some(tx);
        protocol::send_call(conn, CallSignal::Resume).await?;
        protocol::send_call(conn, CallSignal::KeyframeRequest { small: false }).await?;
        protocol::send_call(conn, CallSignal::KeyframeRequest { small: true }).await
    }

    /// Whether this connection's been asked to let go of its call, for
    /// another of theirs to take it over (see `take_over`).
    pub fn asked_to_give_up(&self, carry: &CallCarry) -> bool {
        let asked = carry.handle.share.give_up_asked.locked().take().is_some_and(|at| at.elapsed() < GIVE_UP_ASKED);
        asked && self.live()
    }

    /// They've carried a call on from the last connection.
    async fn on_resume(&mut self, conn: &mut AuthenticatedConnection, ctx: &Arc<Ctx>, name: &str) -> Result<()> {
        if !self.live() {
            return protocol::send_call(conn, CallSignal::Hangup).await;
        }
        if self.resume_by.take().is_none() {
            return Ok(());
        }
        log!(ctx, "{name}: call carried on over a new connection", name = ctx.shown(name));
        if let Some(tx) = &self.call_in {
            let _ = tx.send(CallSignal::KeyframeRequest { small: false });
            let _ = tx.send(CallSignal::KeyframeRequest { small: true });
        }
        protocol::send_call(conn, CallSignal::KeyframeRequest { small: false }).await?;
        protocol::send_call(conn, CallSignal::KeyframeRequest { small: true }).await
    }

    /// The rest of their call's signals, for our side of it.
    fn on_call_signal(&mut self, carry: &mut CallCarry, ctx: &Arc<Ctx>, name: &str, signal: CallSignal) {
        if matches!(signal, CallSignal::Audio(_)) {
            carry.audio_at = Some(Instant::now());
        }
        if let CallSignal::KeyframeRequest { small } = signal
            && self.were_asked_logged.is_none_or(|at: Instant| at.elapsed() > Duration::from_secs(1))
        {
            log!(ctx, "{name}: asked us for a full picture of our {}", if small { "small" } else { "large" }, name = ctx.shown(name));
            self.were_asked_logged = Some(Instant::now());
        }
        carry.handle.budget.received.fetch_add(signal.media_bytes() as u64, Ordering::Relaxed);
        let ends_call = matches!(signal, CallSignal::Hangup | CallSignal::Reject | CallSignal::AnsweredElsewhere);
        if let Some(tx) = &self.call_in {
            let _ = tx.send(signal);
        }
        if ends_call {
            self.call_in = None;
        }
    }

    /// Once a second: a call carried on that the other side never said
    /// it still has ends; and without cover, what the call may send
    /// follows what the link's carrying.
    pub async fn housekeeping(&mut self, carry: &CallCarry, conn: &mut AuthenticatedConnection, ctx: &Arc<Ctx>, name: &str) {
        let in_call = self.live();
        if !in_call {
            self.resume_by = None;
        }
        if self.resume_by.is_some_and(|by| tokio::time::Instant::now() >= by) {
            log!(ctx, "{name}: the call couldn't be carried on — they didn't answer for it", name = ctx.shown(name));
            self.resume_by = None;
            self.call_in = None;
            let _ = protocol::send_call(conn, CallSignal::Hangup).await;
        }
        if conn.cover_out.is_none() {
            if self.live() {
                let budget = &carry.handle.budget;
                let was = budget.video_bps.load(Ordering::Relaxed);
                protocol::set_uncovered_call_budget(budget, &mut self.uncovered, conn);
                if budget.video_on.load(Ordering::Relaxed)
                    && let Some(carried) = self.uncovered.carried()
                {
                    *carry.carried.locked() = Some((call_path(conn), carried));
                }
                let now = budget.video_bps.load(Ordering::Relaxed);
                if budget.video_on.load(Ordering::Relaxed) && now < was / 5 * 4 {
                    log!(ctx, "{name}: video in this call cut to {} (was {})", super::format_bitrate(now as u64), super::format_bitrate(was as u64), name = ctx.shown(name));
                }
            } else {
                let path = call_path(conn);
                let carried = carry.carried.locked().filter(|(on, _)| *on == path).map(|(_, carried)| carried);
                self.uncovered = crate::call::UncoveredCall::starting_at(carried);
            }
        }
    }
}

/// A signal of a call from peer `name`, at the end that's in it: a
/// conference's business goes to the front end as its own event, since
/// the front end keeps the conference, and so does control of a shared
/// screen, since the front end has the screen. The rest is the call's,
/// given back.
/// What the front end's told of `ring` (a ring, into a conference or
/// not, or a member asking for our link) from `from`, on `link`; `None`
/// if it's no ring.
pub(super) fn ring_event(from: String, link: CallLink, ring: CallSignal) -> Option<CallEvent> {
    Some(match ring {
        CallSignal::Invite { video, at } => CallEvent::Incoming { from, link, video, conference: None, at: if at == 0 { super::unix_now() } else { at } },
        CallSignal::ConferenceInvite(roster) => CallEvent::Incoming { from, link, video: false, conference: Some(roster), at: super::unix_now() },
        CallSignal::ConferenceLink(roster) => CallEvent::LinkAsked { from, link, roster },
        _ => return None,
    })
}

pub(super) fn front_end_takes(ctx: &Ctx, name: &str, signal: CallSignal) -> Option<CallSignal> {
    let peer_name = name.to_string();
    let event = match signal {
        CallSignal::Roster(roster) => CallEvent::Roster { peer_name, roster },
        signal @ (CallSignal::Hearing(_) | CallSignal::Forward { .. }) => CallEvent::Conference { peer_name, signal },
        signal @ (CallSignal::Control(_) | CallSignal::Input(_)) => CallEvent::Control { peer_name, signal },
        signal => return Some(signal),
    };
    let _ = ctx.call_events.send(event);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_taken_over_calls_commands_go_to_its_new_connection_until_it_ends() {
        let (own, mut at_own) = mpsc::unbounded_channel();
        let (commands, routed) = mpsc::unbounded_channel();
        let share = Arc::new(CallShare::default());
        tokio::spawn(route_commands(routed, own, share.clone()));
        let (to, mut at_new) = mpsc::unbounded_channel();

        commands.send(CallCommand::Signal(CallSignal::CameraOff)).unwrap();
        assert!(matches!(at_own.recv().await, Some(CallCommand::Signal(CallSignal::CameraOff))));

        *share.moved.locked() = Some(to);
        commands.send(CallCommand::Signal(CallSignal::CameraOff)).unwrap();
        assert!(matches!(at_new.recv().await, Some(CallCommand::Signal(CallSignal::CameraOff))));
        commands.send(CallCommand::Signal(CallSignal::Hangup)).unwrap();
        assert!(matches!(at_new.recv().await, Some(CallCommand::Signal(CallSignal::Hangup))));

        commands.send(CallCommand::Signal(CallSignal::CameraOff)).unwrap();
        assert!(matches!(at_own.recv().await, Some(CallCommand::Signal(CallSignal::CameraOff))));
        assert!(at_new.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_new_call_on_a_connection_isnt_sent_after_the_one_taken_over() {
        let (own, mut at_own) = mpsc::unbounded_channel();
        let (commands, routed) = mpsc::unbounded_channel();
        let share = Arc::new(CallShare::default());
        tokio::spawn(route_commands(routed, own, share.clone()));
        let (to, mut at_new) = mpsc::unbounded_channel();
        *share.moved.locked() = Some(to);

        let (call_in, _kept) = mpsc::unbounded_channel();
        commands.send(CallCommand::Attach(call_in)).unwrap();
        assert!(matches!(at_own.recv().await, Some(CallCommand::Attach(_))));
        assert!(at_new.try_recv().is_err());
        assert!(share.moved.locked().is_none());
    }
}
