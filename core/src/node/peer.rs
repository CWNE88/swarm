//! One task per peer (`PeerTask`): keeps a connection to them up, and
//! is the only thing that ever touches it. Each connection it has is a
//! `Session`, with a method for each thing that can happen on it.

use crate::lock::Locked;
use super::channels::{Broadcast, ChannelWork};
use super::callcarry::{CallCarry, CallOn, Sent, CALL_HOLD};
use super::{x25519_of, CoverLive, Ctx, PeerEntry, NOT_ADDED, REMOVED_US};
use crate::call::CallCommand;
use crate::history::{self, Direction};
use crate::peers::Peer;
use crate::transport::{self, AuthenticatedConnection};
use crate::{displayname, outbox, posts, protocol, seen};
use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// How long to wait before redialing a peer that couldn't be reached:
/// this, doubled for each try in a row that's failed, up to
/// `RECONNECT_MOST` — a peer who's off for days isn't dialled at every
/// address they've given every few seconds. One who comes back dials us.
const RECONNECT_FIRST: Duration = Duration::from_secs(1);
const RECONNECT_MOST: Duration = Duration::from_secs(64);

/// The wait before the next dial, after `failed` tries in a row have.
fn reconnect_wait(failed: u32) -> Duration {
    RECONNECT_FIRST.saturating_mul(1 << failed.saturating_sub(1).min(16)).min(RECONNECT_MOST)
}
/// How long to wait before redialing a peer that couldn't be reached,
/// while another of your devices reaches them (`proxy::through`): what's
/// for them goes that way meanwhile, and a phone that can't reach them
/// itself dialled every address of theirs each minute for nothing, its
/// radio woken each time. This device moving still has them tried at
/// once (`wait_after_failure`). Only a phone waits this long: anything else
/// goes on trying as usual, so that a direct path that appears is found
/// within `RECONNECT_MOST`, with nothing having changed at either end.
const REACHED_BY_MINE_RETRY: Duration = Duration::from_secs(5 * 60);
/// Whether a contact another of your devices reaches is dialled seldom.
const SPARE_THE_RADIO: bool = cfg!(target_os = "android");
/// In a call with them through another of your devices, the longest wait
/// before dialling them again, a phone's too: the radio's awake for the
/// call anyway, and a direct path found is one the call moves onto
/// (`proxycall::LinkCalls::go_direct`).
const IN_RELAYED_CALL_RETRY: Duration = Duration::from_secs(20);
/// How often a wait before dialling looks whether such a call has begun.
const RELAYED_CALL_LOOK: Duration = Duration::from_secs(2);

/// The wait before the next dial: `wait`, or no more than
/// `IN_RELAYED_CALL_RETRY` in a call through another device.
fn redial_due(wait: Duration, in_relayed_call: bool) -> Duration {
    if in_relayed_call { wait.min(IN_RELAYED_CALL_RETRY) } else { wait }
}

/// Waits `wait` before the next dial to peer `user` (`dark` while the
/// screen's off: see `dialling::dark_wait`), or no more than
/// `IN_RELAYED_CALL_RETRY` from its start while this device is in a call
/// with them through another of the user's. The screen coming on is
/// seen within `RELAYED_CALL_LOOK`.
async fn wait_to_redial(ctx: Arc<Ctx>, user: [u8; 32], wait: Duration, dark: Duration) {
    let start = Instant::now();
    loop {
        let wait = if super::dialling::screen_on() { wait } else { dark };
        let due = redial_due(wait, super::proxycall::in_call_through(&ctx, &user));
        let left = due.saturating_sub(start.elapsed());
        if left.is_zero() {
            return;
        }
        tokio::time::sleep(left.min(RELAYED_CALL_LOOK)).await;
    }
}
/// The wait before the next dial, after `failed` tries in a row have: soon
/// while a call's held for a new connection, seldom when `seldom`.
fn retry_wait(failed: u32, holding: bool, seldom: bool) -> Duration {
    if holding {
        HELD_RETRY
    } else if seldom {
        reconnect_wait(failed).max(REACHED_BY_MINE_RETRY)
    } else {
        reconnect_wait(failed)
    }
}
/// `retry_wait` with the screen off: growing up to `dialling::DARK_MOST`,
/// but for a call held for a new connection.
fn dark_retry_wait(failed: u32, holding: bool, lit: Duration) -> Duration {
    if holding { lit } else { super::dialling::dark_wait(failed, RECONNECT_FIRST, lit) }
}
/// A floor under how fast a connection that drops straight after
/// connecting is retried (dialing your own address does that).
const CONNECTION_FLAP_COOLDOWN: Duration = Duration::from_millis(250);
/// How often to check back with a peer who hasn't added us — or sooner,
/// when they dial us, which their side does the moment they add us.
const NOT_ADDED_RECHECK: Duration = Duration::from_secs(60);
/// How long a connection to someone who hasn't added us is kept after
/// our introduction is sent, so it gets out before the connection drops.
const INTRODUCE_GRACE: Duration = Duration::from_secs(1);
/// The close code for a connection dropped because both sides dialled
/// at once and the designated dialer keeps its own (see
/// `designated_dialer`): that connection is already there. Whoever gets
/// the code waits this long for it before counting the peer as gone —
/// otherwise each start-up showed them offline and back.
const CROSSED: u32 = 0x5357;
const CROSSED_GRACE: Duration = Duration::from_secs(2);

/// While a call's held, how soon a failed dial is tried again.
const HELD_RETRY: Duration = Duration::from_millis(500);
/// In a call, a connection not heard from for this long, when it went
/// quiet about when this device's addresses changed, is taken as lost
/// with the move, and they're dialled afresh from where this device is
/// now ...
const MOVED_QUIET: Duration = Duration::from_millis(1500);
/// ... about when: this close (a change is seen within a second or so).
const MOVED_NEAR: Duration = Duration::from_secs(5);
/// In a call, the connection's kept over a fresh one from them only if
/// it's been heard from this recently — a call's always sending, so one
/// quiet longer has gone (they've moved and dialled from where they are
/// now).
const CALL_HEARD_WITHIN: Duration = Duration::from_millis(500);

/// Starts `peer`'s task and registers it with the node: the user's own
/// entry, which finds their devices (see `peerdevices`).
pub(super) fn spawn(ctx: &Arc<Ctx>, peer: Peer) {
    spawn_for(ctx, peer, None, None)
}

/// Starts the task of `device`, one of `peer`'s devices, under the name
/// `peerdevices::entry_name` gives it, and registers it;
/// `first`, a connection to it already had, is the one it starts with.
pub(super) fn spawn_device(ctx: &Arc<Ctx>, peer: Peer, device: [u8; 32], first: Option<AuthenticatedConnection>) {
    spawn_for(ctx, peer, Some(device), first)
}

fn spawn_for(ctx: &Arc<Ctx>, peer: Peer, device: Option<[u8; 32]>, first: Option<AuthenticatedConnection>) {
    if device.is_none() {
        log!(ctx, "keeping a connection to {}", peer.name);
    }
    let (requests_tx, requests_rx) = mpsc::channel(8);
    let (watch_tx, watch_rx) = watch::channel(peer.clone());
    let (inbound_tx, inbound_rx) = mpsc::channel(1);
    let online = Arc::new(AtomicBool::new(false));
    let since = Arc::new(AtomicU64::new(crate::lastseen::get(&ctx.paths.lastseen(), &peer.public_key).unwrap_or(0)));
    let not_added = Arc::new(AtomicBool::new(false));
    let removed_us = Arc::new(AtomicBool::new(crate::settings::removed_us(&ctx.paths.settings(), &peer.public_key)));
    let asked = Arc::new(AtomicBool::new(crate::settings::asked(&ctx.paths.settings(), &peer.public_key)));
    let carry = CallCarry::new();
    let call = carry.handle.clone();
    match x25519_of(&peer.public_key) {
        Some(_) if device.is_some() => {}
        Some(key) => {
            ctx.names_by_key.locked().insert(key, peer.handle());
        }
        None => log!(
            ctx,
            "warning: {}'s stored public key isn't a valid Ed25519 identity — connections from them won't be recognised until it's fixed",
            peer.name
        ),
    }
    let handle = match device {
        Some(device) => super::peerdevices::entry_name(&peer.handle(), &device),
        None => peer.handle(),
    };
    ctx.peers.locked().insert(
        handle.clone(),
        PeerEntry {
            requests: requests_tx,
            watch: watch_tx,
            online: online.clone(),
            since: since.clone(),
            not_added: not_added.clone(),
            removed_us: removed_us.clone(),
            asked: asked.clone(),
            inbound: inbound_tx,
            call: call.clone(),
            device,
        },
    );
    let task = PeerTask {
        ctx: ctx.clone(),
        name: handle,
        device,
        peer_watch: watch_rx,
        requests: requests_rx,
        inbound: inbound_rx,
        online_flag: online,
        since,
        not_added_flag: not_added,
        asked,
        removed_us,
        carry,
        online: false,
        last_saved: Instant::now(),
        pending_inbound: first,
        incompatible_noted: false,
        last_good: None,
        dials: 0,
        moved_away: false,
        failed: 0,
        my_addresses_now: ctx.my_addresses.subscribe(),
        search_at: None,
        search_wait: Duration::ZERO,
    };
    tokio::spawn(task.run());
}

/// One request handed to `PeerTask` over its request
/// channel, with a place to hear back what happened — either a message
/// to relay (`swarm send`) or a request for this peer's posts
/// (`swarm posts`). Both go out over whatever connection this peer's
/// task currently has established — see its docs for
/// why that might be one *it* dialed or one the peer dialed instead.
pub(super) enum PeerRequest {
    /// A message already in this peer's outbox (the caller puts it there
    /// first, so it's never lost): send it now, and say once it's acked.
    Send { id: outbox::MessageId, text: String, sent_at: u64, lifetime: u64, reply: oneshot::Sender<Result<()>> },
    /// Their posts created at or after `since`.
    ListPosts { since: u64, reply: oneshot::Sender<Result<posts::Fetched>> },
    /// Something changed in a file transfer with them (offered, answered,
    /// cancelled): send `notice` if there is one, and carry on pulling.
    Files(Option<protocol::Notice>),
    /// A notice to send as it is.
    Notice(protocol::Notice),
    /// The connection's round-trip time, as QUIC measures it (`swarm ping`
    /// with a node running: a connection of its own would displace this
    /// one — see `listener::route_accepted`).
    Rtt { reply: oneshot::Sender<Duration> },
    /// Ask them for a chunk of transfer `id`, for another of the user's
    /// devices taking it through this one (see `proxyfile`): their
    /// answer, as it is.
    Chunk { id: crate::transfer::TransferId, offset: u64, len: u32, reply: oneshot::Sender<protocol::Reply> },
    /// The answer to their request `rid` on connection `conn` (its
    /// `stable_id`), from another of the user's devices (see `proxyfile`).
    Answer { conn: usize, rid: protocol::RequestId, reply: protocol::Reply },
    /// A message the user sent has been deleted (`unsaying`): tell them.
    Unsay,
}

impl PeerRequest {
    fn fail(self, reason: &str) {
        match self {
            PeerRequest::Send { reply, .. } => {
                let _ = reply.send(Err(anyhow::anyhow!("{reason}")));
            }
            PeerRequest::ListPosts { reply, .. } => {
                let _ = reply.send(Err(anyhow::anyhow!("{reason}")));
            }
            PeerRequest::Files(_) => {}
            PeerRequest::Notice(_) => {}
            PeerRequest::Rtt { .. } | PeerRequest::Chunk { .. } => {}
            PeerRequest::Answer { .. } => {}
            PeerRequest::Unsay => {}
        }
    }
}

/// A request a `Session` has sent and not yet had the
/// reply to, keyed by its `protocol::RequestId` — any number can be
/// outstanding, each reply finds its own by id.
enum Awaiting {
    /// A message (from the outbox, which it stays in until acked), and
    /// whoever's waiting to hear it was delivered, if anyone still is.
    Message { id: outbox::MessageId, reply: Option<oneshot::Sender<Result<()>>>, sent: Instant },
    /// A posts fetch, collecting parts until the last.
    ListPosts { reply: oneshot::Sender<Result<posts::Fetched>>, got: posts::Fetched, progress: Instant },
    /// The "what's your display name?" asked once per connection.
    WhoAmI { sent: Instant },
    /// Their status and picture, asked on connecting and when they say
    /// it's changed (see `profile`), a part at a time: what's gathered
    /// so far, and what it's newer than.
    Profile { sent: Instant, have: u64, so_far: Option<crate::profile::ProfilePart> },
    /// A chunk of a file we're receiving (see `files`).
    FileChunk { transfer: crate::transfer::TransferId, offset: u64, sent: Instant },
    /// A chunk for another of the user's devices (see
    /// `PeerRequest::Chunk`).
    Chunk { reply: oneshot::Sender<protocol::Reply>, sent: Instant },
    /// Telling them a message the user sent is deleted (see `unsay`).
    Unsay { line: crate::history::LineRef },
}

impl Awaiting {
    fn fail(self, reason: &str) {
        match self {
            Awaiting::Message { reply: Some(reply), .. } => {
                let _ = reply.send(Err(anyhow::anyhow!("{reason} — it's kept, and will go when they're back")));
            }
            Awaiting::ListPosts { reply, .. } => {
                let _ = reply.send(Err(anyhow::anyhow!("{reason}")));
            }
            Awaiting::Message { reply: None, .. } | Awaiting::WhoAmI { .. } | Awaiting::Profile { .. } | Awaiting::FileChunk { .. } | Awaiting::Chunk { .. } | Awaiting::Unsay { .. } => {}
        }
    }
}

/// How long a local caller waits to hear a message was delivered before
/// being told it's still on its way. The message isn't affected: it
/// stays in flight, and in the outbox, until acked.
pub(super) const SEND_CONFIRM_WAIT: Duration = protocol::REPLY_TIMEOUT;
/// How long a posts fetch (or display name ask) may go without any
/// part of its answer arriving before it's given up on.
const REQUEST_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How many messages may be sent and awaiting their Ack at once.
const MAX_MESSAGES_IN_FLIGHT: usize = 8;
/// How often the outbox is re-read while connected, for anything that
/// was queued without this task being told (a `swarm send` that found
/// it busy, say).
const OUTBOX_RESCAN: Duration = Duration::from_secs(30);
/// Why nothing's dialled, on a device set to receive connections only.
pub(super) const RECEIVING_ONLY: &str = "this device receives connections only";
/// How long to wait before trying a peer again after their swarm turned
/// out to speak a protocol version this one can't.
const INCOMPATIBLE_RECHECK: Duration = Duration::from_secs(300);

/// `transport::dial` has no timeout of its own — left unbounded, a dial
/// against a peer that simply isn't listening yet would hang on QUIC's
/// own internal handshake retry logic, bounded only by the much longer
/// `max_idle_timeout` (10s). Reuses `REPLY_TIMEOUT` rather than
/// inventing another number: it already means "how long is reasonable
/// to wait for a response before giving up," which applies just as well
/// to a handshake reply as to a ping reply.
pub(super) async fn timeout_dial(
    endpoint: &quinn::Endpoint,
    resolved: SocketAddr,
    keys: transport::Keys<'_>,
    public_key: [u8; 32],
) -> Result<AuthenticatedConnection> {
    if endpoint.local_addr().is_ok_and(|a| super::dialling::holds_back(a.port())) {
        return Err(anyhow::anyhow!("{RECEIVING_ONLY}"));
    }
    let result = match tokio::time::timeout(protocol::REPLY_TIMEOUT, transport::dial(endpoint, resolved, keys, public_key)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("no response within {:?}", protocol::REPLY_TIMEOUT)),
    };
    crate::tally::dialled(resolved, result.is_ok());
    result
}

/// How long a dial to one of someone's addresses is given before the
/// next is tried alongside it.
const NEXT_ADDRESS_AFTER: Duration = Duration::from_millis(300);
/// How often a user's own entry, while one of their devices is online,
/// looks again whether any still is (see `PeerTask::find_devices`).
const DEVICES_CHECK: Duration = Duration::from_secs(5);
/// How soon a user's own entry, once one of their devices is online,
/// dials the addresses none of their devices met here is known at, for
/// any other device of theirs (see `PeerTask::search_unmet`); again as
/// soon after one's found ...
const SEARCH_FIRST: Duration = Duration::from_secs(5);
/// ... and, after one finding none, this long, doubling each time ...
const SEARCH_AGAIN: Duration = Duration::from_secs(5 * 60);
/// ... up to this.
const SEARCH_MOST: Duration = Duration::from_secs(60 * 60);
/// How long a user's entry that isn't the designated dialer waits for
/// their dial before dialling them itself (see `find_devices`).
const STAGGER: Duration = Duration::from_secs(1);
/// How often a device that relays looks whether the user's other
/// devices have moved, to tell a contact (sent only when they have).
const DEVICES_TOLD_EVERY: Duration = Duration::from_secs(10);
/// How many of the addresses a peer has given are dialled each time
/// (newest first) ...
const LEARNED_EACH_TIME: usize = 8;
/// ... and every this many tries, all of them.
const ALL_LEARNED_EVERY: u64 = 24;

/// Dials whichever of `addrs` answers: the first straight away, each
/// next one when the ones before haven't finished within
/// `NEXT_ADDRESS_AFTER` (or have failed), keeping the first to complete
/// and dropping the rest. Noise proves the key, so any address that
/// answers is the right person. Failing, the error says why — an
/// incompatible version above anything else, since that's the one worth
/// telling, then something answering to another key, then the last to
/// fail.
/// How much a dial's failure says, for `dial_any`.
fn telling(e: &anyhow::Error) -> u8 {
    if e.downcast_ref::<protocol::IncompatibleVersion>().is_some() {
        2
    } else if e.chain().any(|c| c.is::<transport::NotThatKey>()) {
        1
    } else {
        0
    }
}

pub(super) async fn dial_any(endpoint: &quinn::Endpoint, addrs: &[SocketAddr], keys: transport::Keys<'_>, public_key: [u8; 32]) -> Result<AuthenticatedConnection> {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Poll;
    type Dial<'a> = Pin<Box<dyn Future<Output = Result<AuthenticatedConnection>> + Send + 'a>>;
    let mut pending = addrs.iter().copied();
    let mut running: Vec<Dial> = Vec::new();
    let start = |addr: SocketAddr| -> Dial { Box::pin(timeout_dial(endpoint, addr, keys, public_key)) };
    let mut error: Option<anyhow::Error> = None;
    loop {
        if running.is_empty() {
            match pending.next() {
                Some(addr) => running.push(start(addr)),
                None => return Err(error.unwrap_or_else(|| anyhow::anyhow!("no address to dial"))),
            }
        }
        let more = pending.len() > 0;
        let finished = std::future::poll_fn(|cx| {
            for i in 0..running.len() {
                if let Poll::Ready(result) = running[i].as_mut().poll(cx) {
                    drop(running.swap_remove(i));
                    return Poll::Ready(result);
                }
            }
            Poll::Pending
        });
        tokio::select! {
            result = finished => {
                match result {
                    Ok(conn) => return Ok(conn),
                    Err(e) if error.as_ref().is_none_or(|old| telling(&e) >= telling(old)) => error = Some(e),
                    Err(_) => {}
                }
                if let Some(addr) = pending.next() {
                    running.push(start(addr));
                }
            }
            _ = tokio::time::sleep(NEXT_ADDRESS_AFTER), if more => {
                if let Some(addr) = pending.next() {
                    running.push(start(addr));
                }
            }
        }
    }
}

/// What ends a connection to them, or their task altogether.
enum End {
    /// The connection's dropped, for this reason (for the log; empty
    /// when there's nothing to say).
    Drop(String),
    /// They've been removed (`swarm contact remove`), or `listen` is
    /// shutting down.
    Stop,
}

impl End {
    fn because(why: impl std::fmt::Display) -> End {
        End::Drop(why.to_string())
    }
}

/// What one event on a connection comes to: carry on, or end it.
type Step = Result<(), End>;

/// Their task is to stop (see `End::Stop`).
struct Stop;

/// Keeps one peer connected for the lifetime of `listen`, and is the
/// *only* thing allowed to touch that peer's `AuthenticatedConnection`:
/// Noise's transport mode needs writes/reads issued in a strict
/// per-direction order (an auto-incrementing nonce), so anything racing
/// on the same connection from two different tasks would corrupt it.
/// Everything else — the control socket, the TUI, calls — asks this
/// task to act on its behalf via channels.
///
/// A connection here can come from *either* direction: this task
/// dialing the peer itself, or `listen_forever`'s accept loop handing
/// over one the peer dialed *us* on instead (via `inbound` — see
/// `PeerInbound`'s docs). Either way it becomes the one shared connection
/// for everything, so a peer behind a firewall that only allows
/// outbound connections still gets working comms both ways.
///
/// Requests go out with ids (see `protocol`), any number at once, and
/// each reply is matched to its request by id. A connection is judged
/// alive by whether *anything* arrives from the peer — a frame or a
/// cover slot — within `protocol::LIVENESS_TIMEOUT`, never by how long
/// one reply takes, so a big posts list or a long message crawling
/// through a low cover rate (or a call) can't make a healthy connection
/// look dead. A `Ping` goes every `HEARTBEAT_INTERVAL` so the peer
/// always has something to hear.
///
/// Messages are always in the on-disk outbox (see `swarm_core::outbox`)
/// until the peer acks them, sent from there whenever connected, and
/// carry an id the peer de-duplicates on (`swarm_core::seen`), so a
/// resend after a lost Ack or a reconnect never shows twice.
///
/// `peer_watch` carries live updates to this peer's record; its sender
/// being dropped (`swarm contact remove`) is what tells this task to stop.
///
/// A peer who hasn't added us back answers our connection with
/// `NotPeer` instead of a `CoverHello`. Then nothing is sent — no
/// outbox, no messages, no heartbeat — beyond one introduction (so they
/// see a contact request), and the connection is dropped. Sends fail
/// with `NOT_ADDED` (the caller keeps the message in the outbox), and
/// this task checks back only every `NOT_ADDED_RECHECK` — or straight
/// away when they dial us, which their side does the moment they add us.
/// This holds what lasts from one connection to the next; `Session`,
/// what's for one connection only.
///
/// Only logs on a state transition, so a stable connection is silent.
struct PeerTask {
    ctx: Arc<Ctx>,
    /// Their handle here (`peers::handle_of`, `peerdevices::entry_name`).
    name: String,
    /// The device of theirs this task is for, or `None` for their user's
    /// own entry, which only finds their devices (see `peerdevices`).
    device: Option<[u8; 32]>,
    peer_watch: watch::Receiver<Peer>,
    requests: mpsc::Receiver<PeerRequest>,
    inbound: mpsc::Receiver<AuthenticatedConnection>,
    online_flag: Arc<AtomicBool>,
    /// When `online_flag` last changed.
    since: Arc<AtomicU64>,
    /// Set while they've told us they haven't added us.
    not_added_flag: Arc<AtomicBool>,
    asked: Arc<AtomicBool>,
    /// Set once they've told us they've removed us: they're not dialled
    /// till they dial us (having added us back), or are removed here.
    removed_us: Arc<AtomicBool>,
    /// Calls with them, from one connection to the next.
    carry: CallCarry,
    online: bool,
    last_saved: Instant,
    /// A connection this peer just dialed *us* on, while we were already
    /// connected some other way — adopted at the very top of the next
    /// try, before trying to dial or wait fresh.
    pending_inbound: Option<AuthenticatedConnection>,
    /// Set when their swarm turned out to be incompatible, so that's
    /// said once, not on every retry.
    incompatible_noted: bool,
    /// Of their addresses, the one our last dial got through on.
    last_good: Option<SocketAddr>,
    /// Dials tried, to know when to try every address they've given.
    dials: u64,
    /// The last connection was dropped by this device for having moved:
    /// dial again at once.
    moved_away: bool,
    /// Tries in a row that have failed (see `reconnect_wait`) ...
    failed: u32,
    /// ... and this device's addresses, a change in which has them tried
    /// again at once.
    my_addresses_now: watch::Receiver<Vec<String>>,
    /// For their user's own entry: when next to look for devices of
    /// theirs not met yet, and how long it waited last (see
    /// `search_unmet`).
    search_at: Option<Instant>,
    search_wait: Duration,
}

impl PeerTask {
    /// What the log calls them: the name they have now.
    fn shown(&self) -> String {
        self.peer_watch.borrow().name.clone()
    }

    async fn run(mut self) {
        while self.connect_once().await.is_ok() {}
    }

    /// One connection to them: got (or not), kept until it drops, and
    /// what follows its dropping.
    async fn connect_once(&mut self) -> Result<(), Stop> {
        let peer = self.peer_watch.borrow_and_update().clone();
        if self.removed_us.load(Ordering::Relaxed) && self.pending_inbound.is_none() {
            return self.wait_removed().await;
        }
        if self.pending_inbound.is_none() && super::devices::dials_nobody(&self.ctx) {
            return self.wait_receiving_only().await;
        }
        if self.device.is_none() {
            return self.find_devices(&peer).await;
        }
        let all = self.addresses_to_dial(&peer).await;
        let designated_dialer = super::dialling::keeps_own_dial(&self.ctx.identity.public_key_bytes(), &peer.public_key);
        if all.is_empty() && self.pending_inbound.is_none() {
            return self.wait_unresolvable(&peer).await;
        }
        let resolved = match self.device {
            Some(device) => self.ctx.answered.locked().leaving_out(&device, all),
            None => all,
        };
        if resolved.is_empty() && self.pending_inbound.is_none() {
            return self.wait_after_failure(anyhow::anyhow!("only another of their devices answers where this one has been")).await;
        }
        let (conn, dialed_by_us) = match self.establish(&peer, &resolved, designated_dialer).await {
            Ok(established) => established,
            Err(e) => return self.wait_after_failure(e).await,
        };
        if Some(conn.remote_device) != self.device {
            let at = transport::canonical(conn.connection.remote_address());
            let short = super::peerdevices::entry_name("", &conn.remote_device);
            log!(self.ctx, "{}: dialled at {at}, and their device{short} answered there", self.shown());
            self.ctx.answered.locked().note(at, conn.remote_device);
            super::peerdevices::hand_over(&self.ctx, &peer.public_key, conn);
            return self.wait_after_failure(anyhow::anyhow!("another of their devices answered there")).await;
        }
        self.connected(&peer, &conn, dialed_by_us);
        let mut session = Session::start(self, peer, conn, dialed_by_us, designated_dialer).await;
        match session.run().await {
            End::Stop => Err(Stop),
            End::Drop(reason) => session.finish(reason).await,
        }
    }

    /// Where to dial them: the address that worked last time first, then
    /// where they've said they are (newest first: some of them each
    /// time, all of them every `ALL_LEARNED_EVERY` tries), then what was
    /// typed.
    async fn addresses_to_dial(&mut self, peer: &Peer) -> Vec<SocketAddr> {
        self.dials += 1;
        let learned = crate::addressbook::load(&self.ctx.paths.addresses(), &self.address_key(peer));
        let learned = if self.dials % ALL_LEARNED_EVERY == 1 { &learned[..] } else { &learned[..learned.len().min(LEARNED_EACH_TIME)] };
        let scopes = self.ctx.link_scopes.locked().clone();
        let mut resolved: Vec<SocketAddr> = self.last_good.into_iter().collect();
        let typed = if self.device.is_some() { Vec::new() } else { crate::peers::resolve_address(&peer.address).await.unwrap_or_default() };
        for addr in crate::addressbook::dialable(learned, &scopes).into_iter().chain(typed) {
            if !resolved.contains(&addr) {
                resolved.push(addr);
            }
        }
        resolved
    }

    /// Dials, all at once, each of the user's addresses that no device of
    /// theirs met here is known at (what was typed, what was learned
    /// for them, which includes what their devices told our other
    /// devices), and hands every connection that answers to its
    /// device's entry: devices of theirs not met yet are met, rather
    /// than only the first to answer (`find_devices`), so a call from
    /// any of ours rings any of theirs it can reach. Nothing new is said
    /// to anyone: these are addresses they gave out themselves. Whether
    /// a device not met before answered.
    async fn search_unmet(&mut self, peer: &Peer) -> bool {
        let dir = self.ctx.paths.addresses();
        let scopes = self.ctx.link_scopes.locked().clone();
        let known = super::peerdevices::known(&self.ctx, &peer.public_key);
        let met: std::collections::HashSet<SocketAddr> = known.iter().flat_map(|d| crate::addressbook::dialable(&crate::addressbook::load(&dir, d), &scopes)).collect();
        let learned = crate::addressbook::dialable(&crate::addressbook::load(&dir, &peer.public_key), &scopes);
        let typed = crate::peers::resolve_address(&peer.address).await.unwrap_or_default();
        let mut unmet: Vec<SocketAddr> = Vec::new();
        for addr in learned.into_iter().chain(typed) {
            if !met.contains(&addr) && !unmet.contains(&addr) {
                unmet.push(addr);
            }
        }
        if unmet.is_empty() {
            return false;
        }
        let mut dials = tokio::task::JoinSet::new();
        for addr in unmet {
            let (ctx, key) = (self.ctx.clone(), peer.public_key);
            dials.spawn(async move { timeout_dial(&ctx.endpoint, addr, ctx.keys(), key).await.map(|conn| (addr, conn)) });
        }
        let mut found = false;
        while let Some(done) = dials.join_next().await {
            let Ok(Ok((addr, conn))) = done else { continue };
            let device = conn.remote_device;
            if !known.contains(&device) {
                found = true;
                log!(self.ctx, "{}: another of their devices found, at {addr}", self.shown());
            }
            let _ = crate::addressbook::see(&dir, &device, &[crate::addressbook::form(transport::canonical(addr))]);
            super::peerdevices::hand_over(&self.ctx, &peer.public_key, conn);
        }
        found
    }

    /// Where their addresses are kept: under their device's key, for a
    /// device's entry; under their user's, for the user's own.
    fn address_key(&self, peer: &Peer) -> [u8; 32] {
        self.device.unwrap_or(peer.public_key)
    }

    /// Their user's own entry: while none of their devices is online,
    /// dials what was typed and learned for them, and whichever device
    /// answers gets the connection (see `peerdevices::hand_over`), its
    /// entry made if it's new. Nothing's sent from here: that's for the
    /// devices' entries.
    async fn find_devices(&mut self, peer: &Peer) -> Result<(), Stop> {
        reject_pending(&mut self.requests, "not connected to any of their devices yet");
        if let Some(conn) = self.pending_inbound.take() {
            super::peerdevices::hand_over(&self.ctx, &peer.public_key, conn);
        }
        let online = super::peerdevices::any_online(&self.ctx, &peer.public_key);
        if online || super::peerdevices::any_waiting(&self.ctx, &peer.public_key) {
            if online {
                let at = *self.search_at.get_or_insert_with(|| Instant::now() + SEARCH_FIRST);
                if Instant::now() >= at {
                    let found = self.search_unmet(peer).await;
                    self.search_wait = if found { SEARCH_FIRST } else { (self.search_wait * 2).clamp(SEARCH_AGAIN, SEARCH_MOST) };
                    self.search_at = Some(Instant::now() + self.search_wait);
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(DEVICES_CHECK) => {}
                changed = self.peer_watch.changed() => {
                    if changed.is_err() {
                        return Err(Stop);
                    }
                    self.search_at = Some(Instant::now() + SEARCH_FIRST);
                    self.search_wait = Duration::ZERO;
                }
                Some(conn) = self.inbound.recv() => super::peerdevices::hand_over(&self.ctx, &peer.public_key, conn),
            }
            return Ok(());
        }
        let designated_dialer = super::dialling::keeps_own_dial(&self.ctx.identity.public_key_bytes(), &peer.public_key);
        if !designated_dialer && self.dials == 0 {
            tokio::select! {
                _ = tokio::time::sleep(STAGGER) => {}
                Some(conn) = self.inbound.recv() => {
                    super::peerdevices::hand_over(&self.ctx, &peer.public_key, conn);
                    return Ok(());
                }
                changed = self.peer_watch.changed() => return if changed.is_err() { Err(Stop) } else { Ok(()) },
            }
        }
        let resolved = self.addresses_to_dial(peer).await;
        let direct = if resolved.is_empty() {
            Err(anyhow::anyhow!("nowhere to dial"))
        } else {
            tokio::select! {
                result = dial_any(&self.ctx.endpoint, &resolved, self.ctx.keys(), peer.public_key) => result,
                Some(conn) = self.inbound.recv() => Ok(conn),
                changed = self.peer_watch.changed() => return if changed.is_err() { Err(Stop) } else { Ok(()) },
            }
        };
        let conn = match direct {
            Ok(conn) => conn,
            Err(_) if resolved.is_empty() => return self.wait_unresolvable(peer).await,
            Err(e) => return self.wait_after_failure(e).await,
        };
        self.failed = 0;
        self.incompatible_noted = false;
        let good = transport::canonical(conn.connection.remote_address());
        let _ = crate::addressbook::see(&self.ctx.paths.addresses(), &conn.remote_device, &[crate::addressbook::form(good)]);
        super::peerdevices::hand_over(&self.ctx, &peer.public_key, conn);
        tokio::select! {
            _ = tokio::time::sleep(STAGGER) => {}
            changed = self.peer_watch.changed() => if changed.is_err() { return Err(Stop) },
            Some(conn) = self.inbound.recv() => super::peerdevices::hand_over(&self.ctx, &peer.public_key, conn),
        }
        Ok(())
    }

    /// They've removed us: nothing's sent or dialled. A connection from
    /// them is taken (they've added us back, or not, which it says).
    async fn wait_removed(&mut self) -> Result<(), Stop> {
        self.went_offline("they've removed you");
        tokio::select! {
            changed = self.peer_watch.changed() => if changed.is_err() { return Err(Stop) },
            Some(conn) = self.inbound.recv() => self.pending_inbound = Some(conn),
            request = self.requests.recv() => match request {
                Some(request) => request.fail(&format!("{} {REMOVED_US}", self.shown())),
                None => return Err(Stop),
            },
        }
        Ok(())
    }

    /// This device is set to receive connections only, its screen off:
    /// nothing's dialled, and a connection from them is taken whenever it
    /// comes. Anything
    /// asked of them meanwhile is told so (a message is kept, to go when
    /// they connect).
    async fn wait_receiving_only(&mut self) -> Result<(), Stop> {
        self.went_offline(RECEIVING_ONLY);
        reject_pending(&mut self.requests, RECEIVING_ONLY);
        let mut updates = self.ctx.device_updates.subscribe();
        let mut screen = super::dialling::screen_changes();
        loop {
            tokio::select! {
                changed = self.peer_watch.changed() => return if changed.is_err() { Err(Stop) } else { Ok(()) },
                Ok(()) = screen.changed() => return Ok(()),
                Some(conn) = self.inbound.recv() => {
                    self.pending_inbound = Some(conn);
                    return Ok(());
                }
                Ok(()) = updates.changed() => return Ok(()),
                request = self.requests.recv() => match request {
                    Some(request) => request.fail(RECEIVING_ONLY),
                    None => return Err(Stop),
                },
            }
        }
    }

    /// Nowhere to dial them: a wait before trying again.
    async fn wait_unresolvable(&mut self, peer: &Peer) -> Result<(), Stop> {
        self.went_offline(format!("can't resolve {}", peer.address));
        reject_pending(&mut self.requests, "their address can't be found now");
        tokio::select! {
            _ = tokio::time::sleep(reconnect_wait(1)) => {}
            changed = self.peer_watch.changed() => if changed.is_err() { return Err(Stop) },
            Some(conn) = self.inbound.recv() => self.pending_inbound = Some(conn),
        }
        Ok(())
    }

    /// Gets a connection any way it can: adopts one they already just
    /// dialed us on, or dials out, or takes one they hand us while
    /// that's under way — whichever completes first wins. The
    /// connection, and whether this side dialed it.
    ///
    /// Except when both sides dial each other at once: if each then
    /// adopted the *other's* dial and abandoned its own, both would
    /// be holding a connection its other end had just dropped — and
    /// that can repeat indefinitely. So the one with the lower public
    /// key is the designated dialer: it keeps an incoming connection
    /// that beats its own dial only as a fallback, used only if its
    /// own dial fails (a one-way firewall still works); the other side
    /// adopts an incoming one straight away.
    async fn establish(&mut self, peer: &Peer, resolved: &[SocketAddr], designated_dialer: bool) -> Result<(AuthenticatedConnection, bool)> {
        if let Some(conn) = self.pending_inbound.take() {
            let ours = dialled_here(&conn);
            return Ok((conn, ours));
        }
        let ctx = self.ctx.clone();
        if !designated_dialer {
            return tokio::select! {
                result = dial_any(&ctx.endpoint, resolved, ctx.keys(), peer.public_key) => result.map(|conn| (conn, true)),
                Some(conn) = self.inbound.recv() => { let ours = dialled_here(&conn); Ok((conn, ours)) }
            };
        }
        let dial = dial_any(&ctx.endpoint, resolved, ctx.keys(), peer.public_key);
        tokio::pin!(dial);
        let mut fallback: Option<AuthenticatedConnection> = None;
        loop {
            tokio::select! {
                result = &mut dial => break match (result, fallback.take()) {
                    (Ok(conn), theirs) => {
                        if let Some(theirs) = theirs {
                            theirs.connection.close(CROSSED.into(), b"crossed");
                        }
                        Ok((conn, true))
                    }
                    (Err(_), Some(theirs)) => Ok((theirs, false)),
                    (Err(e), None) => Err(e),
                },
                Some(conn) = self.inbound.recv(), if fallback.is_none() => {
                    if dialled_here(&conn) {
                        break Ok((conn, true));
                    }
                    if self.carry.holding() {
                        break Ok((conn, false));
                    }
                    fallback = Some(conn);
                }
            }
        }
    }

    /// No connection could be had: a wait before trying again.
    async fn wait_after_failure(&mut self, e: anyhow::Error) -> Result<(), Stop> {
        let incompatible = e.downcast_ref::<protocol::IncompatibleVersion>().map(|v| v.to_string());
        reject_pending(&mut self.requests, "they can't be reached now");
        let (wait, dark) = match incompatible {
            Some(why) => {
                if !self.incompatible_noted {
                    log!(self.ctx, "{}: can't connect — {why}", self.shown());
                    self.incompatible_noted = true;
                }
                self.went_offline(&why);
                (INCOMPATIBLE_RECHECK, INCOMPATIBLE_RECHECK)
            }
            None => {
                self.went_offline(&e);
                self.failed = self.failed.saturating_add(1);
                let reached_by_mine = super::proxy::through(&self.ctx, &self.peer_watch.borrow().public_key).is_some();
                let (failed, holding) = (self.failed, self.carry.holding());
                let wait = retry_wait(failed, holding, reached_by_mine && SPARE_THE_RADIO);
                (wait, dark_retry_wait(failed, holding, wait))
            }
        };
        self.my_addresses_now.borrow_and_update();
        let (ctx, user) = (self.ctx.clone(), self.peer_watch.borrow().public_key);
        tokio::select! {
            _ = wait_to_redial(ctx, user, wait, dark) => {}
            Ok(()) = self.my_addresses_now.changed() => self.failed = 0,
            changed = self.peer_watch.changed() => if changed.is_err() { return Err(Stop) },
            Some(conn) = self.inbound.recv() => self.pending_inbound = Some(conn),
        }
        Ok(())
    }

    /// A connection's been had: noted, and where it was had.
    fn connected(&mut self, peer: &Peer, conn: &AuthenticatedConnection, dialed_by_us: bool) {
        self.incompatible_noted = false;
        self.failed = 0;
        if dialed_by_us {
            let good = transport::canonical(conn.connection.remote_address());
            self.ctx.answered.locked().note(good, conn.remote_device);
            self.last_good = Some(good);
            let _ = crate::addressbook::see(&self.ctx.paths.addresses(), &self.address_key(peer), &[crate::addressbook::form(good)]);
            self.dials = 0;
        }
        self.ctx.note_seen_as(&peer.public_key, conn.seen_as);
    }

    fn went_offline(&mut self, why: impl std::fmt::Display) {
        if self.online {
            log!(self.ctx, "{}: offline ({why})", self.shown());
            mark_seen(&self.ctx, &self.since, &self.peer_watch.borrow().public_key);
        }
        self.online = false;
        self.online_flag.store(false, Ordering::Relaxed);
    }
}

/// One connection to them, while it lasts: what it's carrying, and a
/// method for each thing that can happen on it (see `run`).
struct Session<'t> {
    t: &'t mut PeerTask,
    /// Their record, as it was when this connection was had.
    peer: Peer,
    conn: AuthenticatedConnection,
    dialed_by_us: bool,
    designated_dialer: bool,
    /// See `Session::start`.
    cover_ticker: Option<tokio::time::Interval>,
    /// The call on this connection, if any (see `Session::start`).
    call: CallOn,
    /// When this device's addresses last changed, on this connection.
    moved_at: Option<Instant>,
    outbox_dir: std::path::PathBuf,
    outstanding: HashMap<protocol::RequestId, Awaiting>,
    files: super::files::FileWork,
    /// Channels both of us are in: from once the connection's settled.
    chans: Option<ChannelWork>,
    /// Notices waiting their turn: sent one at a time, between
    /// everything else (see `send_queued_notice`).
    queued_notices: std::collections::VecDeque<protocol::Notice>,
    /// The removals from our device list they've been sent (see
    /// `devices::removals`).
    removals_sent: Vec<crate::devicelist::Entry>,
    device_updates: watch::Receiver<u64>,
    my_addresses: watch::Receiver<Vec<String>>,
    my_profile: watch::Receiver<u64>,
    /// This device's receive limit, told to them whenever it changes.
    receive_limit: watch::Receiver<Option<u64>>,
    /// Messages sent on this connection and not yet acked, and ones
    /// waiting their turn to be sent.
    in_flight: std::collections::HashSet<outbox::MessageId>,
    to_send: std::collections::VecDeque<outbox::QueuedMessage>,
    /// Deletions they've been told of and haven't acked (see `unsay`).
    unsays_in_flight: std::collections::HashSet<crate::history::LineRef>,
    sending_rate: Option<u64>,
    raised_by_peer: bool,
    /// Cover settled (see `Session::start`): anything may be sent.
    settled: bool,
    /// Unsettled by then, the connection's given up (see `Session::start`).
    settle_by: tokio::time::Instant,
    /// Set once they've told us they haven't added us: the moment to
    /// drop this connection, once our introduction has had time to go.
    leave_at: Option<tokio::time::Instant>,
    heartbeat: tokio::time::Interval,
    housekeeping: tokio::time::Interval,
    last_rescan: Instant,
    /// What this device, relaying for the user's others, last told them
    /// of where those are, and when it last looked (see `tell_devices`).
    devices_told: Option<Vec<protocol::DeviceAddress>>,
    devices_checked: Instant,
    /// Whether they've been told this device is in a call (with anyone:
    /// `Ctx::in_a_call`), so a file they send it is asked for a few
    /// chunks at a time; and whether they've said they are, so one this
    /// device sends them is (see `pump_files`).
    told_in_call: bool,
    they_are_in_call: bool,
}

impl<'t> Session<'t> {
    async fn start(t: &'t mut PeerTask, peer: Peer, mut conn: AuthenticatedConnection, dialed_by_us: bool, designated_dialer: bool) -> Session<'t> {
        conn.their_receive_limit = t.ctx.their_limits.locked().get(&peer.public_key).copied();
        let cover_bits_per_second = peer.cover_bits_per_second.or(t.ctx.default_cover).map(|rate| cover_under(&conn, rate));
        let cover_ticker: Option<tokio::time::Interval> = cover_bits_per_second.map(|bps| {
            protocol::enable_cover(&mut conn, bps);
            cover_slot_ticker()
        });

        let call = t.carry.begin(t.online, &conn);

        if !t.online {
            if !t.not_added_flag.load(Ordering::Relaxed) {
                log!(t.ctx, "{}: online", t.shown());
            }
            t.online = true;
            t.online_flag.store(true, Ordering::Relaxed);
            mark_seen(&t.ctx, &t.since, &t.peer_watch.borrow().public_key);
            t.last_saved = Instant::now();
        }

        t.ctx.connected(peer.public_key, conn.connection.clone());
        let files = super::files::FileWork::new(t.ctx.clone(), t.device.unwrap_or(peer.public_key), peer.public_key, super::peerdevices::user_name(&t.shown()).to_string());
        let (device_updates, my_addresses, my_profile) = (t.ctx.device_updates.subscribe(), t.ctx.my_addresses.subscribe(), t.ctx.my_profile.subscribe());

        let my_requirement = if peer.cover_required { peer.cover_bits_per_second } else { None };
        let mut receive_limit = t.ctx.receive_limit.subscribe();
        let mine = *receive_limit.borrow_and_update();
        let _ = protocol::send_cover_hello(&mut conn, cover_bits_per_second, my_requirement, mine).await;
        let mut heartbeat = tokio::time::interval(protocol::HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Session {
            outbox_dir: t.ctx.paths.outbox(),
            t,
            peer,
            conn,
            dialed_by_us,
            designated_dialer,
            cover_ticker,
            call,
            moved_at: None,
            outstanding: HashMap::new(),
            files,
            chans: None,
            queued_notices: std::collections::VecDeque::new(),
            removals_sent: Vec::new(),
            device_updates,
            my_addresses,
            my_profile,
            receive_limit,
            in_flight: std::collections::HashSet::new(),
            to_send: std::collections::VecDeque::new(),
            unsays_in_flight: std::collections::HashSet::new(),
            sending_rate: cover_bits_per_second,
            raised_by_peer: false,
            settled: false,
            settle_by: tokio::time::Instant::now() + protocol::LIVENESS_TIMEOUT,
            leave_at: None,
            heartbeat,
            housekeeping: tokio::time::interval(Duration::from_secs(1)),
            last_rescan: Instant::now(),
            devices_told: None,
            devices_checked: Instant::now(),
            told_in_call: false,
            they_are_in_call: false,
        }
    }

    /// Carries the connection until it ends: whatever happens next, by
    /// the method for it.
    async fn run(&mut self) -> End {
        loop {
            let step = tokio::select! {
                _ = tick_if_some(&mut self.cover_ticker) => self.on_cover_slot().await,
                _ = tokio::time::sleep_until(self.settle_by), if !self.settled && self.leave_at.is_none() => {
                    Err(End::because("they said nothing after connecting"))
                }
                _ = tokio::time::sleep_until(self.leave_at.unwrap_or(self.settle_by)), if self.leave_at.is_some() => Err(End::Drop(String::new())),
                Some(command) = self.t.carry.commands.recv(), if self.settled => self.on_call_command(command).await,
                _ = std::future::ready(()), if self.settled && !self.queued_notices.is_empty() => self.send_queued_notice().await,
                b = next_broadcast(&mut self.chans) => self.on_broadcast(b).await,
                _ = self.heartbeat.tick(), if self.settled => self.on_heartbeat().await,
                Ok(()) = self.my_addresses.changed(), if self.settled => self.on_moved().await,
                Ok(()) = self.my_profile.changed(), if self.settled => self.on_profile_changed().await,
                Ok(()) = self.receive_limit.changed(), if self.settled => {
                    let limit = *self.receive_limit.borrow_and_update();
                    protocol::send_notice(&mut self.conn, protocol::Notice::ReceiveLimit { limit, in_call: self.told_in_call }).await.map_err(End::because)
                }
                Ok(()) = self.device_updates.changed() => {
                    if self.settled {
                        self.queue_removals();
                    }
                    Ok(())
                }
                _ = self.housekeeping.tick() => self.on_housekeeping().await,
                _ = std::future::ready(()), if self.settled && self.in_flight.len() < MAX_MESSAGES_IN_FLIGHT && !self.to_send.is_empty() => self.send_next_queued().await,
                maybe_request = self.t.requests.recv(), if self.settled => match maybe_request {
                    Some(request) => self.on_request(request).await,
                    None => Err(End::Stop),
                },
                Some(fresh) = self.t.inbound.recv() => self.on_inbound(fresh),
                incoming = protocol::recv(&mut self.conn) => self.on_incoming(incoming).await,
                changed = self.t.peer_watch.changed() => self.on_peer_changed(changed),
            };
            if let Err(end) = step {
                return end;
            }
        }
    }

    /// Whether there's a call on this connection, whoever holds it still
    /// holding it.
    fn call_live(&self) -> bool {
        self.call.live()
    }

    /// Their cover is in (or it's waited long enough): what's sent on
    /// every new connection, now that anything may be.
    async fn settle(&mut self) {
        self.settled = true;
        self.t.ctx.cover_live.locked().insert(self.t.name.clone(), CoverLive { sends: self.sending_rate, raised_by_peer: self.raised_by_peer });
        self.call.settle(&mut self.conn).await;
        let list = self.my_addresses.borrow_and_update().clone();
        if !list.is_empty() {
            let _ = protocol::send_notice(&mut self.conn, protocol::Notice::Addresses(list)).await;
        }
        let _ = self.tell_devices().await;
        let timer = crate::disappearing::get(&self.t.ctx.paths.settings(), &self.peer.public_key);
        if timer.set_at > 0 {
            let _ = protocol::send_notice(&mut self.conn, protocol::Notice::Timer(timer)).await;
        }
        if let Ok(id) = protocol::request(&mut self.conn, protocol::Request::WhoAmI).await {
            self.outstanding.insert(id, Awaiting::WhoAmI { sent: Instant::now() });
        }
        self.ask_profile().await;
        self.my_profile.borrow_and_update();
        self.rescan_outbox();
        let _ = self.send_unsays().await;
        for offer in self.files.open_offers() {
            let _ = protocol::send_notice(&mut self.conn, offer).await;
        }
        let _ = self.pump_files().await;
        let mut work = ChannelWork::new(self.t.ctx.clone(), self.peer.public_key);
        let _ = work.start(&mut self.conn).await;
        self.chans = Some(work);
        self.removals_sent.clear();
        self.queue_removals();
        if !self.to_send.is_empty() {
            log!(self.t.ctx, "{}: delivering {} queued message(s)", self.t.shown(), self.to_send.len());
        }
    }

    /// Asks for their status and picture if newer than the one kept.
    async fn ask_profile(&mut self) {
        let have = crate::profile::cached(&self.t.ctx.paths.profiles(), &self.peer.public_key).map_or(0, |p| p.at);
        if let Ok(id) = protocol::request(&mut self.conn, protocol::Request::Profile { have, from: 0 }).await {
            self.outstanding.insert(id, Awaiting::Profile { sent: Instant::now(), have, so_far: None });
        }
    }

    /// Asks for the next chunks of the files we're receiving.
    async fn pump_files(&mut self) -> Result<()> {
        let mut asked = Vec::new();
        let in_call = self.call_live() || self.they_are_in_call;
        let pumped = self.files.pump(&mut self.conn, in_call, |request, transfer, offset| asked.push((request, transfer, offset))).await;
        for (request, transfer, offset) in asked {
            self.outstanding.insert(request, Awaiting::FileChunk { transfer, offset, sent: Instant::now() });
        }
        pumped
    }

    /// Whatever's in the outbox for them and not already on its way.
    fn rescan_outbox(&mut self) {
        for queued in outbox::pending(&self.outbox_dir, &self.peer.public_key).unwrap_or_default() {
            if !self.in_flight.contains(&queued.id) && !self.to_send.iter().any(|m| m.id == queued.id) {
                self.to_send.push_back(queued);
            }
        }
    }

    /// Asks them to delete each message the user's deleted that they
    /// haven't heard of yet (see `unsay`) and isn't already asked.
    async fn send_unsays(&mut self) -> Step {
        if !self.settled {
            return Ok(());
        }
        for line in crate::unsaying::all(&self.t.ctx.paths.unsaying(), &self.peer.public_key).unwrap_or_default() {
            if self.unsays_in_flight.contains(&line) {
                continue;
            }
            let request = protocol::Request::Unsay { id: line.id, at: line.at, digest: line.digest };
            let id = protocol::request(&mut self.conn, request).await.map_err(|e| End::because(format!("deletion failed to send: {e}")))?;
            self.unsays_in_flight.insert(line);
            self.outstanding.insert(id, Awaiting::Unsay { line });
        }
        Ok(())
    }

    /// Devices we've taken off since they were last told: the removals,
    /// so they refuse them (see `peerdevices::refuse`).
    fn queue_removals(&mut self) {
        let removals = super::devices::removals(&self.t.ctx);
        if removals != self.removals_sent {
            self.queued_notices.extend(protocol::device_list_notices(removals.clone()));
            self.removals_sent = removals;
        }
    }

    async fn on_cover_slot(&mut self) -> Step {
        protocol::emit_cover_slot(&mut self.conn).await.map_err(|e| End::because(format!("cover slot failed: {e}")))?;
        protocol::update_call_budget(&self.conn, &self.t.carry.handle.budget);
        Ok(())
    }

    async fn on_call_command(&mut self, command: CallCommand) -> Step {
        let t = &mut *self.t;
        match self.call.on_command(&mut t.carry, &mut self.conn, &t.ctx, &t.name, command, Some(&mut t.inbound)).await {
            Ok(Sent::Done) => Ok(()),
            Ok(Sent::Switched(fresh)) => {
                t.pending_inbound = Some(*fresh);
                Err(End::because("call traffic failed: switched to their new connection"))
            }
            Err(e) => Err(End::because(format!("call traffic failed: {e}"))),
        }
    }

    /// The next notice waiting. One at a time, so whatever else
    /// there is goes in between: a big notice's parts can be
    /// megabytes, and sent in one go it left the
    /// connection unread for as long as that took — and they
    /// were taken for gone, though their side was all there.
    async fn send_queued_notice(&mut self) -> Step {
        let notice = self.queued_notices.pop_front().expect("checked");
        protocol::send_notice(&mut self.conn, notice).await.map_err(|e| End::because(format!("notice failed: {e}")))
    }

    async fn on_broadcast(&mut self, b: Option<Broadcast>) -> Step {
        if let Some(work) = self.chans.as_mut() {
            work.on_broadcast(&mut self.conn, b).await.map_err(|e| End::because(format!("channel traffic failed: {e}")))?;
        }
        Ok(())
    }

    async fn on_heartbeat(&mut self) -> Step {
        protocol::request(&mut self.conn, protocol::Request::Ping).await.map_err(End::because)?;
        Ok(())
    }

    /// This device's addresses changed: they're told, and a call
    /// on a connection that can't follow is dialled afresh.
    async fn on_moved(&mut self) -> Step {
        let list = self.my_addresses.borrow_and_update().clone();
        self.moved_at = Some(Instant::now());
        if self.call_live() && stranded(&self.conn, self.dialed_by_us, &list) {
            self.t.moved_away = true;
            return Err(End::because("this device moved"));
        }
        if !list.is_empty() {
            protocol::send_notice(&mut self.conn, protocol::Notice::Addresses(list)).await.map_err(End::because)?;
        }
        Ok(())
    }

    /// While this device relays for the user's others: where they are,
    /// if that's changed since they were last told on this connection
    /// (see `devices::for_contacts`).
    async fn tell_devices(&mut self) -> Result<()> {
        self.devices_checked = Instant::now();
        let Some(now) = super::devices::for_contacts(&self.t.ctx) else { return Ok(()) };
        if self.devices_told.as_ref() == Some(&now) {
            return Ok(());
        }
        protocol::send_notice(&mut self.conn, protocol::Notice::DeviceAddresses(now.clone())).await?;
        self.devices_told = Some(now);
        Ok(())
    }

    /// Our status or picture changed: they're told, and ask.
    async fn on_profile_changed(&mut self) -> Step {
        self.my_profile.borrow_and_update();
        protocol::send_notice(&mut self.conn, protocol::Notice::ProfileChanged).await.map_err(End::because)
    }

    /// Liveness, and requests that have waited too long.
    async fn on_housekeeping(&mut self) -> Step {
        let in_call = self.call_live();
        self.call.housekeeping(&self.t.carry, &mut self.conn, &self.t.ctx, &self.t.name).await;
        if self.call.asked_to_give_up(&self.t.carry) {
            let t = &mut *self.t;
            log!(t.ctx, "{}: another of their devices is taking the call over", t.shown());
            t.carry.hold(&mut self.call, &t.ctx, &t.name);
        }
        if self.settled && self.t.ctx.receive_limit.borrow().is_some() {
            self.pump_files().await.map_err(|e| End::because(format!("file request failed: {e}")))?;
        }
        if !in_call && self.sending_rate.is_some_and(|rate| cover_under(&self.conn, rate) < rate) {
            return Err(End::because("the bandwidth limit, or theirs for what they receive, is lower than this connection's cover"));
        }
        if in_call && lost_with_move(self.t.carry.audio_at, self.moved_at, self.conn.last_heard) {
            self.t.moved_away = true;
            return Err(End::because("this device moved, and nothing's heard on the old way"));
        }
        if self.settled && self.t.ctx.in_a_call() != self.told_in_call {
            self.told_in_call = !self.told_in_call;
            let limit = *self.receive_limit.borrow();
            protocol::send_notice(&mut self.conn, protocol::Notice::ReceiveLimit { limit, in_call: self.told_in_call }).await.map_err(End::because)?;
        }
        if self.settled && self.devices_checked.elapsed() >= DEVICES_TOLD_EVERY {
            self.tell_devices().await.map_err(End::because)?;
        }
        if self.t.online && self.t.last_saved.elapsed() >= crate::lastseen::SAVE_EVERY {
            let key = self.t.peer_watch.borrow().public_key;
            let _ = crate::lastseen::set(&self.t.ctx.paths.lastseen(), &key, super::unix_now());
            self.t.last_saved = Instant::now();
        }
        if self.conn.last_heard.elapsed() > protocol::LIVENESS_TIMEOUT {
            let path = self.conn.connection.stats().path;
            return Err(End::because(format!(
                "nothing heard for {}s (round trip {} ms, window {} kB, {} lost)",
                protocol::LIVENESS_TIMEOUT.as_secs(),
                self.conn.connection.rtt().as_millis(),
                path.cwnd / 1000,
                path.lost_packets
            )));
        }
        let stalled: Vec<protocol::RequestId> = self
            .outstanding
            .iter()
            .filter(|(_, a)| match a {
                Awaiting::ListPosts { progress, .. } => progress.elapsed() > REQUEST_STALL_TIMEOUT,
                Awaiting::WhoAmI { sent } | Awaiting::Profile { sent, .. } => sent.elapsed() > REQUEST_STALL_TIMEOUT,
                Awaiting::FileChunk { sent, .. } | Awaiting::Chunk { sent, .. } => sent.elapsed() > super::files::CHUNK_STALL,
                Awaiting::Message { .. } | Awaiting::Unsay { .. } => false,
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stalled {
            if let Some(awaiting) = self.outstanding.remove(&id) {
                awaiting.fail("no answer from them");
            }
        }
        for awaiting in self.outstanding.values_mut() {
            if let Awaiting::Message { reply, sent, .. } = awaiting
                && sent.elapsed() > SEND_CONFIRM_WAIT
                    && let Some(reply) = reply.take() {
                        let _ = reply.send(Err(anyhow::anyhow!("not confirmed yet — it's kept, and will keep being tried")));
                    }
        }
        if self.settled && self.last_rescan.elapsed() > OUTBOX_RESCAN {
            self.last_rescan = Instant::now();
            self.rescan_outbox();
            self.send_unsays().await?;
        }
        if let Some(work) = self.chans.as_mut() {
            work.housekeeping();
        }
        let stalled = self.files.reset_stalled();
        if !stalled.is_empty() {
            self.outstanding.retain(|_, a| !matches!(a, Awaiting::FileChunk { transfer, .. } if stalled.contains(transfer)));
        }
        if self.settled {
            self.pump_files().await.map_err(|e| End::because(format!("file request failed: {e}")))?;
        }
        Ok(())
    }

    /// Sends the next queued message (see `run`).
    async fn send_next_queued(&mut self) -> Step {
        let msg = self.to_send.pop_front().expect("guarded by !to_send.is_empty()");
        let sent_at = msg.sent_at;
        if msg.lifetime > 0 && sent_at.saturating_add(msg.lifetime) <= super::unix_now() {
            return Ok(());
        }
        match protocol::request(&mut self.conn, protocol::Request::Message { id: msg.id, sent_at, lifetime: msg.lifetime, text: msg.text }).await {
            Ok(id) => {
                self.in_flight.insert(msg.id);
                self.outstanding.insert(id, Awaiting::Message { id: msg.id, reply: None, sent: Instant::now() });
                Ok(())
            }
            Err(e) => Err(End::because(format!("queued message failed to send: {e}"))),
        }
    }

    /// A local request (relayed `swarm send`, posts fetch, ...).
    async fn on_request(&mut self, request: PeerRequest) -> Step {
        match request {
            PeerRequest::Send { id, text, sent_at, lifetime, reply } => {
                if self.in_flight.contains(&id) {
                    for awaiting in self.outstanding.values_mut() {
                        if let Awaiting::Message { id: sent_id, reply: slot, .. } = awaiting
                            && *sent_id == id {
                                *slot = Some(reply);
                                break;
                            }
                    }
                } else {
                    self.to_send.retain(|m| m.id != id);
                    match protocol::request(&mut self.conn, protocol::Request::Message { id, sent_at, lifetime, text }).await {
                        Ok(request_id) => {
                            self.in_flight.insert(id);
                            self.outstanding.insert(request_id, Awaiting::Message { id, reply: Some(reply), sent: Instant::now() });
                        }
                        Err(e) => {
                            let _ = reply.send(Err(anyhow::anyhow!("{e} — it's kept, and will go when they're back")));
                            return Err(End::because(format!("message failed to send: {e}")));
                        }
                    }
                }
            }
            PeerRequest::Files(notice) => {
                self.files.forget();
                if let Some(notice) = notice {
                    protocol::send_notice(&mut self.conn, notice).await.map_err(|e| End::because(format!("file notice failed: {e}")))?;
                }
                self.pump_files().await.map_err(|e| End::because(format!("file request failed: {e}")))?;
            }
            PeerRequest::Notice(notice) => {
                protocol::send_notice(&mut self.conn, notice).await.map_err(|e| End::because(format!("notice failed: {e}")))?;
            }
            PeerRequest::Rtt { reply } => {
                let _ = reply.send(self.conn.connection.rtt());
            }
            PeerRequest::Chunk { id, offset, len, reply } => match protocol::request(&mut self.conn, protocol::Request::FileChunk { id, offset, len }).await {
                Ok(rid) => {
                    self.outstanding.insert(rid, Awaiting::Chunk { reply, sent: Instant::now() });
                }
                Err(e) => return Err(End::because(format!("file request failed: {e}"))),
            },
            PeerRequest::Unsay => self.send_unsays().await?,
            PeerRequest::Answer { conn, rid, reply } => {
                if self.conn.connection.stable_id() == conn {
                    protocol::reply(&mut self.conn, rid, reply).await.map_err(|e| End::because(format!("file reply failed: {e}")))?;
                }
            }
            PeerRequest::ListPosts { since, reply } => match protocol::request(&mut self.conn, protocol::Request::ListPosts { since }).await {
                Ok(id) => {
                    self.outstanding.insert(id, Awaiting::ListPosts { reply, got: posts::Fetched::default(), progress: Instant::now() });
                }
                Err(e) => {
                    let _ = reply.send(Err(anyhow::anyhow!("{e}")));
                    return Err(End::because(format!("posts request failed: {e}")));
                }
            },
        }
        Ok(())
    }

    /// A second connection from them while we already have one.
    /// The same tie-break as `PeerTask::establish` settles which to
    /// keep: the designated dialer keeps a connection it dialed itself
    /// while that's still clearly alive (heard from within a
    /// heartbeat) — otherwise, if both sides finished dialing
    /// at once, each would switch to the other's and drop its
    /// own, which is the very connection the other just
    /// switched to, forever. Anything else — the other side,
    /// or a connection gone quiet (they restarted, say) —
    /// switches to the fresh one.
    fn on_inbound(&mut self, fresh: AuthenticatedConnection) -> Step {
        if dialled_here(&fresh) {
            if self.designated_dialer && !self.dialed_by_us {
                self.t.pending_inbound = Some(fresh);
                return Err(End::because("switched to our own connection"));
            }
            fresh.connection.close(CROSSED.into(), b"crossed");
            return Ok(());
        }
        let in_call = self.call_live() && self.t.carry.audio_at.is_some_and(|at| at.elapsed() < CALL_HOLD);
        let heard_within = if in_call { CALL_HEARD_WITHIN } else { protocol::HEARTBEAT_INTERVAL + Duration::from_secs(1) };
        let moved = in_call && transport::canonical(fresh.connection.remote_address()) != transport::canonical(self.conn.connection.remote_address());
        let keep_ours = self.designated_dialer && self.dialed_by_us && self.leave_at.is_none() && !moved && self.conn.last_heard.elapsed() < heard_within;
        if keep_ours {
            fresh.connection.close(CROSSED.into(), b"crossed");
            Ok(())
        } else {
            self.t.pending_inbound = Some(fresh);
            Err(End::because("switched to their new connection"))
        }
    }

    fn on_peer_changed(&mut self, changed: Result<(), watch::error::RecvError>) -> Step {
        if changed.is_err() {
            return Err(End::Stop);
        }
        let fresh = self.t.peer_watch.borrow_and_update().clone();
        if fresh != self.peer {
            log!(self.t.ctx, "{}: peer info updated, reconnecting", self.t.shown());
            return Err(End::because("peer info updated"));
        }
        Ok(())
    }

    /// Whatever's read from the wire.
    async fn on_incoming(&mut self, incoming: Result<protocol::Incoming>) -> Step {
        let incoming = incoming.map_err(End::because)?;
        protocol::flush(&mut self.conn).await.map_err(End::because)?;
        let opens_connection = matches!(incoming, protocol::Incoming::Notice(protocol::Notice::CoverHello { .. } | protocol::Notice::NotPeer));
        if !self.settled && self.leave_at.is_none() && !opens_connection {
            self.settle().await;
        }
        if self.leave_at.is_some() && matches!(incoming, protocol::Incoming::Notice(protocol::Notice::CoverHello { .. })) {
            return Err(End::Drop(String::new()));
        }
        self.on_wire(incoming).await.map_err(End::because)
    }

    async fn on_wire(&mut self, incoming: protocol::Incoming) -> Result<()> {
        match incoming {
            protocol::Incoming::Notice(protocol::Notice::NotPeer) => self.on_not_peer().await,
            protocol::Incoming::Notice(protocol::Notice::Removed) => {
                self.on_removed();
                Ok(())
            }
            protocol::Incoming::Notice(protocol::Notice::CoverHello { requires, limit, .. }) => {
                self.on_receive_limit(limit);
                self.on_cover_hello(requires).await;
                Ok(())
            }
            protocol::Incoming::Notice(protocol::Notice::Introduce { .. } | protocol::Notice::Carbon { .. } | protocol::Notice::Follow { .. }) => Ok(()),
            protocol::Incoming::Notice(protocol::Notice::ManualAddress { .. } | protocol::Notice::Discoverable { .. }) => Ok(()),
            protocol::Incoming::Notice(protocol::Notice::ReceiveLimit { limit, in_call }) => {
                self.on_receive_limit(limit);
                self.they_are_in_call = in_call;
                Ok(())
            }
            protocol::Incoming::Notice(protocol::Notice::Card(card)) => {
                super::cards::on_card(&self.t.ctx, self.peer.public_key, &self.t.shown(), card);
                Ok(())
            }
            protocol::Incoming::Notice(protocol::Notice::ProfileChanged) => {
                if let Ok(id) = protocol::request(&mut self.conn, protocol::Request::WhoAmI).await {
                    self.outstanding.insert(id, Awaiting::WhoAmI { sent: Instant::now() });
                }
                self.ask_profile().await;
                Ok(())
            }
            protocol::Incoming::Notice(protocol::Notice::Timer(timer)) => {
                super::on_timer(&self.t.ctx, super::peerdevices::user_name(&self.t.shown()), self.peer.public_key, timer);
                Ok(())
            }
            protocol::Incoming::Notice(protocol::Notice::Addresses(list)) => {
                self.on_addresses(list);
                Ok(())
            }
            protocol::Incoming::Notice(protocol::Notice::DeviceAddresses(devices)) => {
                if let Some(from) = self.t.device {
                    super::peerdevices::hear_devices(&self.t.ctx, &self.peer.public_key, &from, &devices);
                }
                Ok(())
            }
            protocol::Incoming::Notice(protocol::Notice::DeviceList { entries }) => {
                super::peerdevices::refuse(&self.t.ctx, &self.peer.public_key, &entries);
                Ok(())
            }
            protocol::Incoming::Notice(
                protocol::Notice::Bundle { .. } | protocol::Notice::PeersBook { .. } | protocol::Notice::RequestsBook { .. } | protocol::Notice::WithdrawalsBook { .. } | protocol::Notice::ReadMarks { .. }
                | protocol::Notice::Reaching(_)
                | protocol::Notice::ReachingDelays(_)
                | protocol::Notice::CallThrough { .. }
                | protocol::Notice::CallRoom { .. }
                | protocol::Notice::FileThrough { .. }
                | protocol::Notice::CarbonFor { .. }
                | protocol::Notice::Delivered { .. }
                | protocol::Notice::Unsaid { .. },
            ) => Ok(()),
            protocol::Incoming::Notice(
                notice @ (protocol::Notice::FileOffer { .. }
                | protocol::Notice::FileAnswer { .. }
                | protocol::Notice::FileCancel { .. }
                | protocol::Notice::FileDone { .. }
                | protocol::Notice::FileClosed { .. }),
            ) => {
                let contact = self.files.peer();
                if super::proxyfile::from_contact(&self.t.ctx, contact, &notice) {
                    return Ok(());
                }
                for back in self.files.on_notice(notice.clone())? {
                    protocol::send_notice(&mut self.conn, back).await?;
                }
                super::proxyfile::after_contact(&self.t.ctx, contact, &notice);
                Ok(())
            }
            protocol::Incoming::Notice(notice @ (protocol::Notice::ChannelPost { .. } | protocol::Notice::ChannelEvent { .. } | protocol::Notice::GroupIntro(_))) => match self.chans.as_mut() {
                Some(work) => work.on_notice(&mut self.conn, notice).await,
                None => Ok(()),
            },
            protocol::Incoming::Request(id, request @ protocol::Request::ChannelCatchUp { .. }) => match self.chans.as_mut() {
                Some(work) => work.on_request(&mut self.conn, id, &request).await.map(|_| ()),
                None => protocol::reply(&mut self.conn, id, protocol::Reply::ChannelError(protocol::NO_SUCH_CHANNEL.to_string())).await,
            },
            protocol::Incoming::Request(id, protocol::Request::FileChunk { id: transfer, offset, len }) => {
                let known = self.files.knows(&transfer);
                match super::proxyfile::serve(&self.t.ctx, self.files.peer(), transfer, offset, len, known) {
                    super::proxyfile::Serve::Here => {
                        let answer = self.files.serve(&transfer, offset, len);
                        protocol::reply(&mut self.conn, id, answer).await
                    }
                    super::proxyfile::Serve::Unanswered => Ok(()),
                    super::proxyfile::Serve::Asked(answer) => {
                        let (ctx, name, conn) = (self.t.ctx.clone(), self.t.name.clone(), self.conn.connection.stable_id());
                        tokio::spawn(async move {
                            let Ok(reply) = answer.await else { return };
                            let requests = ctx.peers.locked().get(&name).map(|e| e.requests.clone());
                            if let Some(requests) = requests {
                                let _ = requests.send(PeerRequest::Answer { conn, rid: id, reply }).await;
                            }
                        });
                        Ok(())
                    }
                }
            }
            protocol::Incoming::Notice(protocol::Notice::Call(signal)) => {
                let t = &mut *self.t;
                if matches!(signal, crate::call::CallSignal::Takeover) {
                    return self.call.take_over(&mut t.carry, &mut self.conn, &t.ctx, &t.name, &self.peer.public_key).await;
                }
                if signal.is_ring()
                    && !self.call.live()
                    && let Some(tx) = super::proxycall::fan(&t.ctx, &t.name, self.peer.public_key, &t.carry.handle, &signal)
                {
                    self.call.take_call(&mut t.carry, tx);
                    return Ok(());
                }
                self.call.on_signal(&mut t.carry, &mut self.conn, &t.ctx, &t.name, signal).await
            }
            protocol::Incoming::Request(id, protocol::Request::Ping) => protocol::reply(&mut self.conn, id, protocol::Reply::Pong).await,
            protocol::Incoming::Request(id, protocol::Request::Message { id: message_id, sent_at, lifetime, text }) => {
                let (ctx, key) = (&self.t.ctx, self.peer.public_key);
                {
                    let _forgetting = ctx.forgetting.locked();
                    if super::just_deleted(ctx, &key) {
                        return Ok(());
                    }
                    if seen::first_time(&ctx.paths.seen(), &key, &message_id).unwrap_or(true) {
                        log!(ctx, "message from {}", self.t.shown());
                        let expires = crate::disappearing::expires(super::unix_now(), lifetime);
                        if let Err(e) = history::append_message(&ctx.paths.history(), &key, Direction::Received, &text, sent_at, expires, Some(message_id)) {
                            log!(ctx, "a message from {} couldn't be kept ({e}): it'll come again", self.t.shown());
                            let _ = seen::forget(&ctx.paths.seen(), &key, &message_id);
                            return Ok(());
                        }
                        ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
                        super::devices::carbon(ctx, key, message_id, false, sent_at, expires, &text);
                    }
                }
                protocol::reply(&mut self.conn, id, protocol::Reply::Ack).await
            }
            protocol::Incoming::Request(id, protocol::Request::ListPosts { since }) => {
                protocol::respond_posts(&mut self.conn, id, posts::asked(&self.t.ctx.paths.posts(), since, true)).await
            }
            protocol::Incoming::Request(id, protocol::Request::WhoAmI) => {
                protocol::reply(&mut self.conn, id, protocol::Reply::DisplayName(self.t.ctx.paths.my_display_name())).await
            }
            protocol::Incoming::Request(id, protocol::Request::Profile { have, from }) => {
                let newer = crate::profile::current(&self.t.ctx.paths.profile()).filter(|p| p.at > have);
                protocol::reply(&mut self.conn, id, protocol::Reply::Profile(newer.map(|p| crate::profile::part(&p, from as usize)))).await
            }
            protocol::Incoming::Request(id, protocol::Request::WithdrawContactRequest { .. }) => protocol::reply(&mut self.conn, id, protocol::Reply::Ack).await,
            protocol::Incoming::Request(id, protocol::Request::Unsay { id: message_id, at, digest }) => {
                super::unsay::taken_back(&self.t.ctx, self.peer.public_key, message_id, at, digest);
                protocol::reply(&mut self.conn, id, protocol::Reply::Ack).await
            }
            protocol::Incoming::Request(id, _) => protocol::reply(&mut self.conn, id, protocol::Reply::Unsupported).await,
            protocol::Incoming::Reply(id, reply) if self.chans.as_ref().is_some_and(|w| w.owns(id)) => {
                if let Some(work) = self.chans.as_mut() {
                    work.on_reply(id, reply);
                }
                Ok(())
            }
            protocol::Incoming::Reply(id, reply) => self.on_reply(id, reply).await,
        }
    }

    /// They haven't added us: an introduction, and the connection's
    /// dropped once it's had time to go (see `PeerTask`).
    async fn on_not_peer(&mut self) -> Result<()> {
        if self.leave_at.is_some() {
            return Ok(());
        }
        if !self.t.not_added_flag.swap(true, Ordering::Relaxed) {
            log!(self.t.ctx, "{}: hasn't added us as a contact yet — introducing ourselves; nothing will be sent until they do", self.t.shown());
        }
        if self.t.online {
            mark_seen(&self.t.ctx, &self.t.since, &self.t.peer_watch.borrow().public_key);
        }
        self.t.online = false;
        self.t.online_flag.store(false, Ordering::Relaxed);
        if !self.t.asked.load(Ordering::Relaxed) {
            super::peerdevices::note_asked(&self.t.ctx, &self.peer.public_key, true);
        }
        self.leave_at = Some(tokio::time::Instant::now() + INTRODUCE_GRACE);
        let introduction = self.t.ctx.paths.my_introduction(&self.t.ctx.identity);
        let sent = protocol::send_introduce(&mut self.conn, &introduction).await;
        if sent.is_ok()
            && let Ok(id) = protocol::request(&mut self.conn, protocol::Request::WhoAmI).await {
                self.outstanding.insert(id, Awaiting::WhoAmI { sent: Instant::now() });
            }
        sent
    }

    /// They've removed us: remembered, and the connection dropped
    /// (see `PeerTask::wait_removed`). Only from someone who's said
    /// they haven't added us (`NotPeer`).
    fn on_removed(&mut self) {
        if self.leave_at.is_none() {
            return;
        }
        if !self.t.removed_us.load(Ordering::Relaxed) {
            log!(self.t.ctx, "{}: {REMOVED_US}", self.t.shown());
            let _ = crate::settings::set_removed_us(&self.t.ctx.paths.settings(), &self.peer.public_key, true);
            self.t.removed_us.store(true, Ordering::Relaxed);
        }
        self.leave_at = Some(tokio::time::Instant::now());
    }

    /// Their receive limit, in their hello or changed since: what's sent
    /// them is held under it — call video from the next second (see
    /// `protocol::set_uncovered_call_budget`), cover from the next
    /// connection (see `on_housekeeping`) — and remembered for that.
    fn on_receive_limit(&mut self, limit: Option<u64>) {
        let key = self.peer.public_key;
        if self.conn.their_receive_limit != limit {
            match limit {
                Some(limit) => log!(self.t.ctx, "{}: asks for no more than {}bit/s", self.t.shown(), super::format_bitrate(limit)),
                None => log!(self.t.ctx, "{}: no longer limits what's sent them", self.t.shown()),
            }
        }
        self.conn.their_receive_limit = limit;
        match limit {
            Some(limit) => self.t.ctx.their_limits.locked().insert(key, limit),
            None => self.t.ctx.their_limits.locked().remove(&key),
        };
    }

    /// Their cover: the rate they require of us, if more than ours.
    async fn on_cover_hello(&mut self, requires: Option<u64>) {
        if self.t.asked.load(Ordering::Relaxed) {
            super::peerdevices::note_asked(&self.t.ctx, &self.peer.public_key, false);
        }
        if self.t.not_added_flag.swap(false, Ordering::Relaxed) {
            log!(self.t.ctx, "{}: has added us as a contact", self.t.shown());
        }
        if self.t.removed_us.swap(false, Ordering::Relaxed) {
            log!(self.t.ctx, "{}: has added you back", self.t.shown());
            let _ = crate::settings::set_removed_us(&self.t.ctx.paths.settings(), &self.peer.public_key, false);
        }
        if let Some(required) = requires.map(|r| cover_under(&self.conn, r))
            && self.sending_rate.is_none_or(|rate| required > rate) {
                if protocol::raise_cover(&mut self.conn, required) {
                    self.cover_ticker = Some(cover_slot_ticker());
                }
                log!(self.t.ctx, "{}: requires {} cover from us — sending at that on this connection", self.t.shown(), super::format_bitrate(required));
                self.sending_rate = Some(required);
                self.raised_by_peer = true;
            }
        if !self.settled {
            self.settle().await;
        }
    }

    /// Where they can be reached now.
    fn on_addresses(&mut self, list: Vec<String>) {
        let addresses = self.t.ctx.paths.addresses();
        let key = self.t.address_key(&self.peer);
        let before = crate::addressbook::told(&addresses, &key);
        let moved = crate::addressbook::hear(&addresses, &key, &list, super::unix_now());
        super::ownpeers::heard(&self.t.ctx, &self.peer.public_key);
        if let Ok(true) = moved {
            log!(self.t.ctx, "{}: can be reached at {} now", self.t.shown(), list.join(", "));
            let routed = |list: &[String]| -> Vec<String> {
                let mut v: Vec<String> = list.iter().filter_map(|a| crate::addressbook::checked(a)).filter(|a| !a.starts_with("fe80:") && !a.starts_with("[fe80:") && !a.starts_with("169.254.")).collect();
                v.sort();
                v
            };
            if before.is_some_and(|b| routed(&b.addresses) != routed(&list)) {
                self.t.ctx.moves.locked().push((super::peerdevices::user_name(&self.t.shown()).to_string(), list.clone()));
            }
        }
    }

    /// They're ringing (or asking us into a conference).
    /// An answer to one of our requests.
    async fn on_reply(&mut self, id: protocol::RequestId, reply: protocol::Reply) -> Result<()> {
        let key = self.peer.public_key;
        match (self.outstanding.remove(&id), reply) {
            (Some(Awaiting::Message { id: message_id, reply, .. }), protocol::Reply::Ack) => {
                self.in_flight.remove(&message_id);
                let _ = outbox::remove(&self.outbox_dir, &key, &message_id);
                super::devices::delivered(&self.t.ctx, key, message_id);
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(()));
                }
            }
            (Some(Awaiting::ListPosts { reply, mut got, .. }), protocol::Reply::Posts { more, posts, deleted }) => {
                got.posts.extend(posts);
                got.deleted.extend(deleted);
                got.posts.truncate(protocol::MAX_POSTS_FETCHED);
                got.deleted.truncate(posts::MAX_DELETIONS);
                if more && got.posts.len() < protocol::MAX_POSTS_FETCHED {
                    self.outstanding.insert(id, Awaiting::ListPosts { reply, got, progress: Instant::now() });
                } else {
                    let _ = reply.send(Ok(got));
                }
            }
            (Some(Awaiting::Profile { have, so_far, .. }), protocol::Reply::Profile(Some(part))) => match crate::profile::gather(so_far, part) {
                crate::profile::Gathered::Whole(profile) => {
                    if let Ok(true) = crate::profile::cache(&self.t.ctx.paths.profiles(), &key, &profile) {
                        log!(self.t.ctx, "{}: status and picture updated", self.t.shown());
                        self.t.ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
                    }
                }
                crate::profile::Gathered::More(so_far) => {
                    let from = so_far.data.len() as u32;
                    if let Ok(id) = protocol::request(&mut self.conn, protocol::Request::Profile { have, from }).await {
                        self.outstanding.insert(id, Awaiting::Profile { sent: Instant::now(), have, so_far: Some(so_far) });
                    }
                }
                crate::profile::Gathered::Wrong => log!(self.t.ctx, "{}: their status and picture came in parts that didn't fit together", self.t.shown()),
            },
            (Some(Awaiting::WhoAmI { .. }), protocol::Reply::DisplayName(Some(claimed))) => {
                let _forgetting = self.t.ctx.forgetting.locked();
                if !super::just_deleted(&self.t.ctx, &key) {
                    let path = self.t.ctx.paths.displaynames();
                    let before = displayname::cached_one(&path, &key).ok().flatten().map(|d| d.name);
                    let _ = displayname::cache_one(&path, key, &claimed);
                    if before.as_deref() != Some(claimed.name.as_str()) && displayname::verify(&claimed, &key) {
                        self.t.ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
                    }
                }
            }
            (Some(Awaiting::Unsay { line }), reply) => {
                self.unsays_in_flight.remove(&line);
                match reply {
                    protocol::Reply::Ack => super::unsay::heard(&self.t.ctx, key, &line),
                    other => log!(self.t.ctx, "{}: unexpected answer to a deletion ({other:?}) — will retry", self.t.shown()),
                }
            }
            (Some(Awaiting::Message { id: message_id, reply, sent }), other) => {
                self.in_flight.remove(&message_id);
                log!(self.t.ctx, "{}: unexpected answer to a message ({other:?}) — will retry", self.t.shown());
                Awaiting::Message { id: message_id, reply, sent }.fail("they didn't accept it");
            }
            (Some(Awaiting::FileChunk { transfer, offset, sent }), reply) => {
                let data = match reply {
                    protocol::Reply::FileData(data) => Ok(data),
                    protocol::Reply::FileError(reason) => Err(reason),
                    other => Err(format!("unexpected answer {other:?}")),
                };
                if let Some(back) = self.files.on_data(transfer, offset, data, sent.elapsed()) {
                    protocol::send_notice(&mut self.conn, back).await?;
                }
                return self.pump_files().await;
            }
            (Some(Awaiting::Chunk { reply: answer, .. }), reply) => {
                let _ = answer.send(reply);
            }
            (Some(awaiting), _) => awaiting.fail("they couldn't answer that"),
            (None, _) => {}
        }
        Ok(())
    }

    /// The connection's gone, for `drop_reason`: what's left of it put
    /// away, and a wait, if there's to be one, before the next.
    async fn finish(mut self, drop_reason: String) -> Result<(), Stop> {
        let t = &mut *self.t;
        t.carry.hold(&mut self.call, &t.ctx, &t.name);
        self.t.ctx.cover_live.locked().remove(&self.t.name);
        self.t.ctx.gone(&self.peer.public_key, &self.conn.connection);
        for (_, awaiting) in self.outstanding.drain() {
            awaiting.fail("the connection dropped");
        }
        if self.t.pending_inbound.is_some() {
            return Ok(());
        }
        if self.leave_at.is_some() {
            if self.t.removed_us.load(Ordering::Relaxed) {
                return Ok(());
            }
            let recheck = tokio::time::sleep(NOT_ADDED_RECHECK);
            tokio::pin!(recheck);
            loop {
                tokio::select! {
                    _ = &mut recheck => break,
                    changed = self.t.peer_watch.changed() => if changed.is_err() { return Err(Stop) } else { break },
                    Some(conn) = self.t.inbound.recv() => {
                        self.t.pending_inbound = Some(conn);
                        break;
                    }
                    request = self.t.requests.recv() => match request {
                        Some(request) => request.fail(&format!("{} {NOT_ADDED}", super::peerdevices::user_name(&self.t.shown()))),
                        None => return Err(Stop),
                    },
                }
            }
            return Ok(());
        }
        let crossed = matches!(self.conn.connection.close_reason(), Some(quinn::ConnectionError::ApplicationClosed(close)) if close.error_code == CROSSED.into());
        if crossed {
            tokio::select! {
                _ = tokio::time::sleep(CROSSED_GRACE) => {}
                Some(theirs) = self.t.inbound.recv() => {
                    self.t.pending_inbound = Some(theirs);
                    return Ok(());
                }
            }
        }
        self.t.went_offline(&drop_reason);
        if std::mem::take(&mut self.t.moved_away) {
            return Ok(());
        }
        tokio::select! {
            _ = tokio::time::sleep(CONNECTION_FLAP_COOLDOWN) => {}
            changed = self.t.peer_watch.changed() => if changed.is_err() { return Err(Stop) },
        }
        Ok(())
    }
}

/// The timer that paces a connection's cover slots. A late tick (the
/// task held up for whatever reason) is skipped, not made up: tokio's
/// default would fire every missed tick back to back, writing several
/// slots at once — which quinn then packs into fewer, *bigger* packets,
/// exactly the kind of burst cover mode exists to hide. One late slot
/// is the lesser tell.
/// The cover sent on `conn` for `rate` asked: under the bandwidth limit
/// (`protocol::covered_rate`) and nine tenths of their receive limit.
fn cover_under(conn: &AuthenticatedConnection, rate: u64) -> u64 {
    let rate = protocol::covered_rate(rate);
    conn.their_receive_limit.map_or(rate, |limit| rate.min(limit / 10 * 9))
}

pub(super) fn cover_slot_ticker() -> tokio::time::Interval {
    let mut ticker = tokio::time::interval(protocol::COVER_SLOT_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker
}

/// Ticks `ticker` if there is one, otherwise never resolves — lets
/// `Session::run`'s `select!` carry an optional periodic
/// branch (the cover-mode slot timer) with no `if` guard needed: a
/// `None` ticker just means this branch sits out every `select!` pass
/// forever, exactly as if it weren't listed at all.
pub(super) async fn tick_if_some(ticker: &mut Option<tokio::time::Interval>) {
    match ticker {
        Some(ticker) => {
            ticker.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// The next channel broadcast for this connection's `ChannelWork`, once
/// it has one; until then, never.
pub(super) async fn next_broadcast(chans: &mut Option<ChannelWork>) -> Option<Broadcast> {
    match chans {
        Some(work) => work.next().await,
        None => std::future::pending().await,
    }
}

/// Whether `ip` is one of `addresses` (this device's, as given to peers).
/// In a call on `conn`, this device's addresses changed to `list`:
/// whether the connection can't follow. On one they dialled, to an
/// address this device no longer has: only the side that dialled a
/// connection can move it, so they're dialled from here. Or on one it
/// dialled, with nothing left of the kind it's on (IPv4 only now, say,
/// on an IPv6 one): QUIC can't move it across.
pub(super) fn stranded(conn: &AuthenticatedConnection, dialed_by_us: bool, list: &[String]) -> bool {
    if dialed_by_us {
        !has_family(list, conn.connection.remote_address())
    } else {
        conn.connection.local_ip().is_some_and(|ip| !has_ip(list, ip))
    }
}

/// In a call (their audio last heard at `audio_at`), a connection quiet
/// since about when this device moved (`moved_at`; last heard at
/// `last_heard`): the move took it with it (onto another kind of
/// address, say, which QUIC can't follow).
pub(super) fn lost_with_move(audio_at: Option<Instant>, moved_at: Option<Instant>, last_heard: Instant) -> bool {
    let near = |a: Instant, b: Instant| a.max(b).duration_since(a.min(b)) < MOVED_NEAR;
    audio_at.is_some_and(|at| near(at, Instant::now())) && moved_at.is_some_and(|at| near(at, last_heard)) && last_heard.elapsed() > MOVED_QUIET
}

fn has_ip(addresses: &[String], ip: std::net::IpAddr) -> bool {
    let ip = transport::canonical(SocketAddr::new(ip, 0)).ip();
    addresses.iter().flat_map(|a| crate::peers::literal_addresses(a)).any(|a| transport::canonical(a).ip() == ip)
}

/// Whether any of `addresses` (this device's) could reach `remote`: one
/// of its family, and link-local just when it is.
fn has_family(addresses: &[String], remote: SocketAddr) -> bool {
    let link_local = |ip: std::net::IpAddr| match ip {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_unicast_link_local(),
    };
    let remote = transport::canonical(remote).ip();
    addresses
        .iter()
        .flat_map(|a| crate::peers::literal_addresses(a))
        .map(|a| transport::canonical(a).ip())
        .any(|ip| ip.is_ipv4() == remote.is_ipv4() && link_local(ip) == link_local(remote))
}

/// Fails every request waiting in the queue — there's no connection to
/// send them on.
fn reject_pending(requests: &mut mpsc::Receiver<PeerRequest>, reason: &str) {
    while let Ok(request) = requests.try_recv() {
        request.fail(reason);
    }
}

/// Whether this side dialled `conn`: a connection handed to a peer's
/// task can be its own, dialled by their user's entry (`peerdevices`).
fn dialled_here(conn: &AuthenticatedConnection) -> bool {
    conn.connection.side().is_client()
}

/// Online or offline from now: notes the time, and saves it as when they
/// were last seen.
fn mark_seen(ctx: &Ctx, since: &AtomicU64, key: &[u8; 32]) {
    let now = super::unix_now();
    since.store(now, Ordering::Relaxed);
    let _ = crate::lastseen::set(&ctx.paths.lastseen(), key, now);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seldom_redial_waits_minutes_unless_a_call_is_held() {
        assert_eq!(retry_wait(1, false, false), RECONNECT_FIRST);
        for failed in [1, 3, 30] {
            assert_eq!(retry_wait(failed, false, true), REACHED_BY_MINE_RETRY);
            assert_eq!(retry_wait(failed, true, true), HELD_RETRY);
        }
    }

    #[test]
    fn in_a_call_through_another_device_they_are_dialled_every_20_seconds_at_most() {
        let phone = retry_wait(5, false, true);
        assert_eq!(redial_due(phone, false), REACHED_BY_MINE_RETRY);
        assert_eq!(redial_due(phone, true), IN_RELAYED_CALL_RETRY);
        assert_eq!(redial_due(retry_wait(9, false, false), true), IN_RELAYED_CALL_RETRY, "a desktop's 64 s too");
        assert_eq!(redial_due(RECONNECT_FIRST, true), RECONNECT_FIRST, "never longer than it would be");
    }

    #[test]
    fn with_the_screen_off_a_contact_is_dialled_up_to_fifteen_minutes_apart_but_a_held_call_soon() {
        let lit = retry_wait(20, false, false);
        assert_eq!(lit, RECONNECT_MOST);
        assert_eq!(dark_retry_wait(20, false, lit), super::super::dialling::DARK_MOST);
        assert_eq!(dark_retry_wait(3, false, retry_wait(3, false, false)), retry_wait(3, false, false), "early on, as with it on");
        assert_eq!(dark_retry_wait(20, true, retry_wait(20, true, false)), HELD_RETRY);
        assert_eq!(dark_retry_wait(1, false, REACHED_BY_MINE_RETRY), REACHED_BY_MINE_RETRY, "never sooner than with it on");
    }

    #[test]
    fn redials_wait_twice_as_long_each_time_up_to_64_seconds_then_stay_there() {
        let waits: Vec<u64> = (1..=10).map(|failed| reconnect_wait(failed).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 64, 64, 64, 64]);
        assert_eq!(reconnect_wait(u32::MAX), RECONNECT_MOST);
    }
}
