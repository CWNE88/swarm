//! A running swarm node: everything that happens without anyone at the
//! keyboard — keeping a connection to every peer, answering whoever
//! connects, delivering queued messages, carrying calls — behind one
//! handle (`NodeHandle`) that any front end can drive. The CLI and TUI
//! in `swarm-cli` are one such front end; a desktop or phone app would be
//! another, with nothing here to rewrite. So nothing in this module
//! prints, reads a terminal, or assumes a platform: log lines go to the
//! `Logger` the front end supplies, files live under the `Paths` it
//! chooses, and audio stays with the front end (calls hand it a
//! `call::CallLink`).
//!
//! - `start` binds the endpoint, starts a task per peer (`peer.rs`) and
//!   the accept loop (`listener.rs`), and returns the handle.
//! - `NodeHandle` sends messages, fetches posts, reports each peer's
//!   state, hands out call links, and takes peer-list changes live.
//! - `oneshot` does the same jobs with no node running: a single
//!   connection, made and dropped (`swarm send`/`posts`/`call`/`channel`).

use crate::lock::Locked;
use crate::call::{CallCommand, CallLink};
use crate::displayname::{self, DisplayName};
use crate::identity::{self, Identity};
use crate::outbox;
use crate::peers::Peer;
use crate::posts::Post;
use crate::protocol;
use crate::transport::AuthenticatedConnection;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot as reply, watch};

/// Where the node's log lines go — the front end decides (stderr, a
/// file, a panel on screen).
pub type Logger = Arc<dyn Fn(String) + Send + Sync>;

macro_rules! log {
    ($ctx:expr, $($arg:tt)*) => {
        ($ctx.log)(format!($($arg)*))
    };
}

mod callcarry;
mod cards;
mod channels;
mod dialling;
pub use dialling::{set_call_underway, set_screen_on};
pub mod devices;
mod files;
mod follow;
mod ownlook;
mod ownpeers;
pub(crate) use ownpeers::forgotten_on_disk;
mod ownread;
mod ownrequests;
mod unsay;
mod withdraw;
mod settle;
pub mod joining;
mod naming;
mod peerdevices;
mod proxy;
mod proxycall;
mod proxyfile;
pub use peerdevices::{user_name, user_of_device};
mod limits;
mod listener;
pub mod oneshot;
mod peer;

/// The text a send or fetch fails with when the other side hasn't added
/// us as a peer — see `protocol::NotAPeer`.
pub const NOT_ADDED: &str = "hasn't added you as a contact yet";

/// The text a send fails with when the other side has removed us — see
/// `protocol::Notice::Removed`.
pub const REMOVED_US: &str = "has removed you";

/// Every file a node reads or writes, under one directory (normally
/// `~/.swarm`).
#[derive(Clone, Debug)]
pub struct Paths {
    dir: PathBuf,
    peers_file: Option<PathBuf>,
}

impl Paths {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Paths { dir: dir.into(), peers_file: None }
    }

    /// The same, but with the peers list somewhere other than `peers` in
    /// the directory.
    pub fn with_peers_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.peers_file = Some(path.into());
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn identity(&self) -> PathBuf {
        self.dir.join("identity")
    }

    /// This device's own key (see `device_identity`), sealed in the
    /// vault; the identity is the user's, the same on all of them.
    pub fn device_key(&self) -> PathBuf {
        self.dir.join("device-key")
    }

    /// The user's devices (see `crate::devicelist`), sealed in the vault.
    pub fn device_list(&self) -> PathBuf {
        self.dir.join("device-list")
    }

    pub fn peers(&self) -> PathBuf {
        self.peers_file.clone().unwrap_or_else(|| self.dir.join("peers"))
    }

    pub fn posts(&self) -> PathBuf {
        self.dir.join("posts")
    }

    /// Where each peer has said it can be reached (see `addressbook`).
    pub fn addresses(&self) -> PathBuf {
        self.dir.join("addresses")
    }

    /// What the user typed for each of their other devices (see
    /// `typedaddress`).
    pub fn typed_addresses(&self) -> PathBuf {
        self.dir.join("typed-addresses")
    }

    /// Whose public posts this device follows (see `following`).
    pub fn following(&self) -> PathBuf {
        self.dir.join("following")
    }

    /// Who follows this device's (see `followers`).
    pub fn followers(&self) -> PathBuf {
        self.dir.join("followers")
    }

    pub fn outbox(&self) -> PathBuf {
        self.dir.join("outbox")
    }

    pub fn history(&self) -> PathBuf {
        self.dir.join("history")
    }

    /// Messages sent that none of the contact's devices has yet (see
    /// `crate::waiting`).
    pub fn waiting(&self) -> PathBuf {
        self.dir.join("waiting")
    }

    /// Messages sent that the user has deleted and the contact hasn't
    /// yet heard are gone (see `crate::unsaying`).
    pub fn unsaying(&self) -> PathBuf {
        self.dir.join("unsaying")
    }

    pub fn seen(&self) -> PathBuf {
        self.dir.join("seen")
    }

    pub fn postcache(&self) -> PathBuf {
        self.dir.join("postcache")
    }

    /// Groups (`crate::channel`).
    pub fn channels(&self) -> PathBuf {
        self.dir.join("groups")
    }

    /// Asks this user has made, through groups, to be added
    /// (`crate::groupintro`).
    pub fn group_asks(&self) -> PathBuf {
        self.dir.join("group-asks")
    }

    pub fn displayname(&self) -> PathBuf {
        self.dir.join("displayname")
    }

    pub fn displaynames(&self) -> PathBuf {
        self.dir.join("displaynames")
    }

    /// This device's own status and picture (see `crate::profile`).
    pub fn profile(&self) -> PathBuf {
        self.dir.join("profile")
    }

    /// The address the user set by hand (see `myaddress`).
    pub fn manual_address(&self) -> PathBuf {
        self.dir.join("manual-address")
    }

    /// Whether the user is discoverable on a network (see `discoverable`).
    pub fn discoverable(&self) -> PathBuf {
        self.dir.join("discoverable")
    }

    /// Peers' statuses and pictures, one file per peer.
    pub fn profiles(&self) -> PathBuf {
        self.dir.join("profiles")
    }

    pub fn requests(&self) -> PathBuf {
        self.dir.join("requests")
    }

    pub fn transfers(&self) -> PathBuf {
        self.dir.join("transfers")
    }

    pub fn settings(&self) -> PathBuf {
        self.dir.join("settings")
    }

    pub fn lastseen(&self) -> PathBuf {
        self.dir.join("lastseen")
    }

    /// What's been taken back, and who's still to be told.
    pub fn withdrawals(&self) -> PathBuf {
        self.dir.join("withdrawals")
    }

    /// How far each chat has been read, on any of the user's devices.
    pub fn read_marks(&self) -> PathBuf {
        self.dir.join("readmarks")
    }

    /// This device's own published display name, if it has one.
    pub fn my_display_name(&self) -> Option<DisplayName> {
        displayname::current(&self.displayname()).ok().flatten()
    }

    /// Who we are, for an introduction to someone who hasn't added us.
    pub fn my_introduction(&self, identity: &Identity) -> protocol::Introduction {
        protocol::Introduction { root_key: identity.public_key_bytes(), name: self.my_display_name() }
    }
}

pub struct NodeConfig {
    pub paths: Paths,
    /// The UDP port to listen on (normally `transport::DEFAULT_PORT`).
    pub port: u16,
    /// The cover rate for every peer without one of its own (`swarm
    /// --cover`), decided once for the node's whole run.
    pub cover_bits_per_second: Option<u64>,
    pub log: Logger,
}

/// What a peer's connection is sending at right now — which can be more
/// than configured, when the peer requires a rate of us (see
/// `protocol::send_cover_hello`).
#[derive(Clone, Copy, Debug)]
pub struct CoverLive {
    pub sends: Option<u64>,
    /// `sends` was raised to what the peer requires, above our own
    /// setting for them.
    pub raised_by_peer: bool,
}

/// One peer's state as the node sees it.
#[derive(Clone, Copy, Debug)]
pub struct PeerStatus {
    pub online: bool,
    /// They've told us they haven't added us as a contact.
    pub not_added: bool,
    /// They've told us they've removed us (and aren't dialled).
    pub removed_us: bool,
    /// They were last heard to say they haven't added us, whether or not
    /// they can be reached now: there's a request out, to be cancelled by
    /// deleting them.
    pub asked: bool,
    /// Set while connected, once cover is settled.
    pub cover: Option<CoverLive>,
    /// Online since, or offline since (when last connected) — unix secs.
    /// `None`: never seen.
    pub since: Option<u64>,
}

/// How to reach a peer's connection task for a call (calls ride that
/// connection in-band — see `call`), plus what it keeps current of what
/// a call can send (`call::CallBudget`).
#[derive(Clone)]
pub struct PeerCallHandle {
    pub commands: mpsc::UnboundedSender<CallCommand>,
    pub budget: Arc<crate::call::CallBudget>,
    /// What another connection of the same user needs to take a call
    /// over from this one.
    share: Arc<callcarry::CallShare>,
}

/// Something a front end has to react to about a call. Whoever receives
/// one takes over the `CallLink` it carries completely.
pub enum CallEvent {
    /// A peer is ringing. `from` is their peer-list name; `video`, they
    /// offer a video call. `conference`: it's into a conference (see
    /// `call::CallSignal::ConferenceInvite`) — or, if we're in that one
    /// already, the link between us in it.
    /// `at`: when it started ringing (unix seconds), the same on all the
    /// user's devices where the caller says.
    Incoming { from: String, link: CallLink, video: bool, conference: Option<crate::conference::Roster>, at: u64 },
    /// A member of a conference asks for the link between us in it (see
    /// `call::CallSignal::ConferenceLink`): taken only if we're in that
    /// one, else turned down. Never rung.
    LinkAsked { from: String, link: CallLink, roster: crate::conference::Roster },
    /// A call placed from here was answered and is live — `video`, with
    /// video both ways.
    Accepted { peer_name: String, link: CallLink, video: bool },
    /// A call placed from here was refused, or nobody answered within
    /// `call::RING_TIMEOUT`.
    Declined { peer_name: String },
    /// A call placed from here never got as far as ringing.
    Failed { peer_name: String, reason: String },
    /// A live call finished, for any reason.
    Ended { peer_name: String },
    /// The other side's pictures have started coming on a call that had
    /// no video (see `call::CallLink::video_later`).
    /// `via`: someone else's, passed on along this link as this id (see
    /// `call::CallSignal::Via`) — theirs, not the link's own.
    TheirVideo { peer_name: String, via: Option<u8> },
    /// What a peer in a conference with us knows of its roster (see
    /// `call::CallSignal::Roster`), checked.
    Roster { peer_name: String, roster: crate::conference::Roster },
    /// The rest of a conference's own business from a peer in one with
    /// us: `call::CallSignal::Hearing` or `call::CallSignal::Forward`.
    Conference { peer_name: String, signal: crate::call::CallSignal },
    /// Control of a shared screen, from a peer in a call with us:
    /// `call::CallSignal::Control` or `call::CallSignal::Input`.
    Control { peer_name: String, signal: crate::call::CallSignal },
}

/// What happened to a message.
#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The peer acknowledged it.
    Delivered,
    /// Not confirmed yet (they're offline, or the ack hasn't come back):
    /// it's in the outbox, and goes — exactly once — when it can.
    Queued,
    /// They haven't added us as a contact: it's in the outbox, not sent, and
    /// goes once they do.
    HeldUntilAdded,
    /// This device isn't connected to them, but another of the user's
    /// devices that relays is (`proxy`): it's in the outbox, and goes to
    /// them through that device — not "offline".
    Relayed,
}

/// A peer's posts: freshly fetched (and merged into the local cache), or,
/// when they couldn't be reached, whatever the cache had.
#[derive(Debug)]
pub enum PostsResult {
    Live(Vec<Post>),
    Cached { posts: Vec<Post>, reason: String },
}

/// Where a connection to a peer runs (`NodeHandle::connection_to`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InUse {
    /// Straight between the two: this end's address `here` (if it can be
    /// told) and `port`, theirs `there`, and whether this end dialled.
    Direct { here: Option<std::net::IpAddr>, port: u16, there: SocketAddr, dialled: bool },
    /// None of this device's own: what's for them goes through another
    /// of the user's devices, called this, which is connected to them
    /// (see `proxy`).
    Through(String),
}

/// A connection (its `stable_id`), where it went to, and the address
/// this end goes out from (see `Ctx::sent_from`).
type SentFrom = (usize, SocketAddr, Option<std::net::IpAddr>);

/// Shared by everything the node runs.
pub(crate) struct Ctx {
    /// The user's key: what peers know us by, and Noise runs with.
    identity: Arc<Identity>,
    /// This device's own key.
    device: Arc<Identity>,
    paths: Paths,
    log: Logger,
    endpoint: quinn::Endpoint,
    /// Each peer's connections now, by root key: a contact's (user key:
    /// one for each of their devices connected, the newest last) or
    /// another of this user's devices' (device key). What a relay device
    /// does things over (see `proxy`), and what
    /// `NodeHandle::connection_to` tells of.
    live: Mutex<HashMap<[u8; 32], Vec<quinn::Connection>>>,
    default_cover: Option<u64>,
    call_events: mpsc::UnboundedSender<CallEvent>,
    /// The other end of `call_events`, until `start` hands it out.
    call_events_rx: Mutex<Option<mpsc::UnboundedReceiver<CallEvent>>>,
    peers: Mutex<HashMap<String, PeerEntry>>,
    /// Peer names by the *X25519* form of their key — what an accepted
    /// connection's `remote_public_key` holds (see `transport`).
    names_by_key: Mutex<HashMap<[u8; 32], String>>,
    cover_live: Mutex<HashMap<String, CoverLive>>,
    /// How far into each outgoing transfer the other side has asked for
    /// — its progress, as far as we can tell.
    transfer_sent: Mutex<HashMap<crate::transfer::TransferId, u64>>,
    /// New channel posts and roster entries, for every connection to
    /// pass on (see `channels`).
    channel_tx: tokio::sync::broadcast::Sender<channels::Broadcast>,
    /// Counts up on any channel change, for the front end.
    channel_updates: watch::Sender<u64>,
    /// Asks to be added, passed on through groups (`groupintro`), by id:
    /// each passed on once, and to whoever connects while it lasts.
    intros: Mutex<HashMap<[u8; 32], crate::groupintro::Intro>>,
    /// Books from the user's other devices being taken in now
    /// (`ownpeers::Arriving`).
    books_arriving: std::sync::atomic::AtomicUsize,
    /// Something new for `withdraw::keep_telling` to try.
    withdrawals_wake: tokio::sync::Notify,
    /// Counts up on each message received from a peer, for the front end.
    message_updates: watch::Sender<u64>,
    /// Counts up when this device's status or picture changes, for every
    /// peer's connection to tell them (see `profile`).
    my_profile: watch::Sender<u64>,
    /// The address the user set by hand, as last looked at (see
    /// `myaddress`); counts up whenever it changes, for the user's other
    /// devices to be told.
    manual_address: watch::Sender<crate::myaddress::Manual>,
    /// Whether the user is discoverable (see `discoverable`), as last
    /// looked at; changes are told to the user's other devices.
    discoverable: watch::Sender<crate::discoverable::Discoverable>,
    next_connection_id: std::sync::atomic::AtomicU64,
    /// Root keys with a connection carrying channels right now, and how
    /// many.
    connected: Mutex<HashMap<[u8; 32], usize>>,
    /// Where each peer or channel member (root key) last saw our
    /// connection come from — our outside address, behind NAT.
    seen_as: Mutex<HashMap<[u8; 32], SocketAddr>>,
    /// The address this end of each peer's connection (root key) goes
    /// out from, as worked out for that connection (its `stable_id`) and
    /// where it went to (see `NodeHandle::connection_to`).
    sent_from: Mutex<HashMap<[u8; 32], SentFrom>>,
    /// A code for adding a device given out here, while it's good (see
    /// `joining`).
    join_offer: Mutex<Option<joining::Offer>>,
    /// What to tell the person about their devices, not yet collected.
    device_news: Mutex<Vec<String>>,
    /// A copy of a message has been queued for the user's other devices
    /// (see `devices::carbon`).
    carbon_tx: tokio::sync::broadcast::Sender<()>,
    /// The messages last known delivered (see `devices::delivered`), the
    /// newest last.
    delivered: Mutex<std::collections::VecDeque<outbox::MessageId>>,
    /// Links to the user's other devices, by device key.
    siblings: Mutex<HashMap<[u8; 32], devices::SiblingEntry>>,
    /// Calls with the user's other devices, by device key (see
    /// `devices::DeviceCall`).
    device_calls: Mutex<HashMap<[u8; 32], devices::DeviceCall>>,
    /// The peers (user keys) each of the user's other devices that's let
    /// relay says it's connected to directly, by device key (see
    /// `proxy`): what this device can do through it.
    reached: Mutex<HashMap<[u8; 32], Vec<[u8; 32]>>>,
    /// How long each of those devices says a round trip takes from it to
    /// each one it reaches, by device key (`proxy::through` chooses by it).
    reach_delays: Mutex<HashMap<[u8; 32], proxy::Delays>>,
    /// The relay chosen for each peer or device it's reaching for this
    /// one (see `proxy::through`).
    relay_choices: Mutex<HashMap<[u8; 32], proxy::RelayChoice>>,
    /// The way onto each link to another of the user's devices for calls
    /// through it (see `proxycall`), by device.
    link_calls: Mutex<HashMap<[u8; 32], mpsc::UnboundedSender<proxycall::Through>>>,
    /// Calls whose link to a device has gone, waiting for a new one.
    held_link_calls: Mutex<proxycall::HeldLinkCalls>,
    /// The devices whose links have calls through them: a new connection
    /// from one is where it is now, as for a call on the link itself.
    links_in_calls: Mutex<std::collections::HashSet<[u8; 32]>>,
    /// Calls with peers through another of the user's devices, by their
    /// user key (see `proxycall::handle`).
    through_calls: Mutex<HashMap<[u8; 32], Arc<proxycall::ThroughCall>>>,
    /// The way onto each link to another of the user's devices for
    /// transfers through it (see `proxyfile`), by device.
    link_files: Mutex<HashMap<[u8; 32], mpsc::UnboundedSender<proxyfile::LinkFile>>>,
    /// Transfers this device passes on between a peer and another of the
    /// user's devices (see `proxyfile`), in memory only.
    through_files: Mutex<HashMap<crate::transfer::TransferId, proxyfile::Passing>>,
    /// Counts up on any change to devices, for the front end.
    device_updates: watch::Sender<u64>,
    /// Where this device can be reached now, for peers (see
    /// `addressbook`); set by the app, which knows the interfaces.
    my_addresses: watch::Sender<Vec<String>>,
    /// The interfaces an IPv6 link-local address is tried on.
    link_scopes: Mutex<Vec<u32>>,
    /// Which device answered at an address, contacts' and our own (see
    /// `dialling`).
    answered: Mutex<dialling::AnsweredAt>,
    /// Cards for finding lost peers (see `cards`).
    cards: Mutex<cards::CardState>,
    /// Peers who've said they're somewhere new, and where, waiting for
    /// the app to tell the person (`NodeHandle::take_moves`).
    moves: Mutex<Vec<(String, Vec<String>)>>,
    /// What the user's other devices' books have, for dropping gone
    /// lines once they all do (see `settle`).
    settle: Mutex<settle::Settle>,
    /// Contacts deleted while this has been running, and when: told
    /// they're removed when they dial, for `TOLD_DELETED_FOR`, so their
    /// side stops instead of asking at once to be added. Nothing of it is
    /// written down.
    deleted: Mutex<HashMap<[u8; 32], Instant>>,
    /// Held while a contact's forgotten (`forget_here`), and by what
    /// keeps a message with someone (one from them, a copy from another
    /// of our devices) across its look at whether they're deleted and
    /// its write: else a copy landing part-way through a deletion is
    /// written after their history's gone, and kept.
    forgetting: Mutex<()>,
    /// This device's receive limit (see `settings::receive_limit`), told
    /// to every contact connected whenever it changes.
    receive_limit: watch::Sender<Option<u64>>,
    /// Each contact's receive limit (user key), as they last told us: what
    /// we send them is held under it (see `peer::Session`). Remembered for
    /// their next connection, whose cover has to be under it from the start.
    their_limits: Mutex<HashMap<[u8; 32], u64>>,
    /// Set by `NodeHandle::shutdown`: the node-wide loops end, so nothing
    /// of a stopped node goes on writing to its files.
    stopped: AtomicBool,
    /// This device was taken off the user's devices, and what was theirs
    /// on it wiped (see `devices::taken_off`).
    wiped: AtomicBool,
    /// When a call's sound or pictures last went or came, any call this
    /// device is in or passes on (see `in_a_call`).
    call_media_at: Mutex<Option<Instant>>,
}

/// A call's media this recent: the device is in a call (see
/// `Ctx::in_a_call`). Voice goes every 20 ms; a muted one's pictures
/// still go, so seconds without either is a call that's ended.
const IN_A_CALL_FOR: Duration = Duration::from_secs(3);

impl Ctx {
    /// A call's sound or a picture went or came just now.
    pub(crate) fn call_media(&self) {
        *self.call_media_at.locked() = Some(Instant::now());
    }

    /// Whether this device is in a call, with anyone, or passing one on:
    /// files then give it the link, asked for a few chunks at a time
    /// (`files::FileWork::wants`), as is any file it's sending, from the
    /// side asking (`Notice::ReceiveLimit`'s `in_call`).
    pub(crate) fn in_a_call(&self) -> bool {
        self.call_media_at.locked().is_some_and(|at| at.elapsed() < IN_A_CALL_FOR)
    }

    /// What the log calls the entry `handle`: the name they have now
    /// (with the device, for one of their devices).
    pub(super) fn shown(&self, handle: &str) -> String {
        self.peers.locked().get(handle).map_or_else(|| handle.to_string(), |e| e.watch.borrow().name.clone())
    }

    /// `key` is connected on `conn` now, the newest of theirs (with
    /// those of their other devices still going).
    pub(crate) fn connected(&self, key: [u8; 32], conn: quinn::Connection) {
        let mut live = self.live.locked();
        let conns = live.entry(key).or_default();
        conns.retain(|c| c.close_reason().is_none() && c.stable_id() != conn.stable_id());
        conns.push(conn);
    }

    /// `key`'s newest connection still open, if there is one.
    pub(crate) fn live(&self, key: &[u8; 32]) -> Option<quinn::Connection> {
        self.live.locked().get(key)?.iter().rev().find(|c| c.close_reason().is_none()).cloned()
    }

    /// The shortest round trip on any live connection to `key`.
    pub(crate) fn least_rtt(&self, key: &[u8; 32]) -> Option<Duration> {
        self.live.locked().get(key)?.iter().filter(|c| c.close_reason().is_none()).map(|c| c.rtt()).min()
    }

    /// `key`'s connection `conn` has gone: any other of theirs (another
    /// of their devices') is still theirs.
    pub(crate) fn gone(&self, key: &[u8; 32], conn: &quinn::Connection) {
        let mut live = self.live.locked();
        if let Some(conns) = live.get_mut(key) {
            conns.retain(|c| c.stable_id() != conn.stable_id());
            if conns.is_empty() {
                live.remove(key);
            }
        }
    }

    /// Who this end is, for a connection.
    pub(crate) fn keys(&self) -> crate::transport::Keys<'_> {
        crate::transport::Keys { user: &self.identity, device: &self.device }
    }

    /// Keeps what `key`'s side said it saw our connection come from.
    pub(crate) fn note_seen_as(&self, key: &[u8; 32], seen_as: Option<SocketAddr>) {
        let Some(addr) = seen_as else { return };
        let before = self.seen_as.locked().insert(*key, addr);
        if before.map(|b| b.ip()) != Some(addr.ip()) {
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            let who: String = STANDARD.encode(key).chars().take(10).collect();
            log!(self, "{who} sees this device at {}", addr.ip());
        }
    }
}

/// Everything the node keeps for one peer while its task runs. Dropping
/// the entry (its `watch` sender in particular) is what stops the task.
struct PeerEntry {
    requests: mpsc::Sender<peer::PeerRequest>,
    watch: watch::Sender<Peer>,
    online: Arc<AtomicBool>,
    /// When `online` last changed (unix secs): online since, or offline
    /// since. 0 when they've never been seen.
    since: Arc<AtomicU64>,
    not_added: Arc<AtomicBool>,
    removed_us: Arc<AtomicBool>,
    /// See `PeerStatus::asked`.
    asked: Arc<AtomicBool>,
    /// Where the accept loop hands a connection this peer dialed to us —
    /// see `peer::PeerTask`. Buffered to one.
    inbound: mpsc::Sender<AuthenticatedConnection>,
    call: PeerCallHandle,
    /// The device of theirs this entry is for; `None` for their user's
    /// own entry (see `peerdevices`).
    device: Option<[u8; 32]>,
}

/// A running node. Cheap to clone; every clone drives the same node.
#[derive(Clone)]
pub struct NodeHandle {
    ctx: Arc<Ctx>,
}

/// Starts a node: binds `[::]:<port>` (IPv4 too — see `transport::bind`), starts a task for every peer in
/// the peers file and the accept loop, and returns at once. The receiver
/// is for call events (see `CallEvent`); drop it to refuse every ring.
/// Has to be called inside a tokio runtime.
pub async fn start(identity: Arc<Identity>, config: NodeConfig) -> Result<(NodeHandle, mpsc::UnboundedReceiver<CallEvent>)> {
    crate::secure::keep_in_memory(&*identity);
    crate::store::open_vault(config.paths.dir(), &identity.storage_keys());
    crate::store::check_format(config.paths.dir())?;
    let bind_addr: SocketAddr = format!("[::]:{}", config.port).parse()?;
    let endpoint = match crate::transport::bind(bind_addr) {
        Err(e) if !e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)) => {
            crate::transport::bind(format!("0.0.0.0:{}", config.port).parse()?).map_err(|_| e)
        }
        other => other,
    }
    .context("binding QUIC endpoint")?;
    let device = Arc::new(device_identity(&config.paths).context("this device's key")?);
    crate::transport::set_bandwidth_limit(crate::settings::bandwidth_limit(&config.paths.settings()));
    crate::secure::keep_in_memory(&*device);
    crate::quinnlog::forward(config.log.clone());
    let ctx = new_ctx(identity, device, config.paths, config.log, endpoint, config.cover_bits_per_second)?;
    let families = if ctx.endpoint.local_addr().is_ok_and(|a| a.is_ipv4()) { "IPv4 only: this system has no IPv6" } else { "IPv6 and IPv4" };
    log!(ctx, "listening on port {} ({families}), identity {}", config.port, ctx.identity.public_key_base64());
    devices::sync_receiving_only(&ctx);
    let handle = run(&ctx)?;
    tokio::spawn(listener::accept_loop(ctx.clone(), ctx.endpoint.clone()));
    let call_events_rx = ctx_call_events_rx(&ctx);
    Ok((handle, call_events_rx))
}

/// This device's own key, kept in the vault at `paths` (open, so after
/// the passphrase): made the first time it's asked for. The user's key
/// is the same on all of a user's devices; this one tells them apart.
pub fn device_identity(paths: &Paths) -> std::io::Result<Identity> {
    let _guard = crate::store::lock();
    let path = paths.device_key();
    match crate::store::read(&path) {
        Ok(text) => Identity::from_private_key_base64(text.trim()).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e:?}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let device = Identity::generate();
            crate::store::write_atomic(&path, device.private_key_base64() + "\n")?;
            Ok(device)
        }
        Err(e) => Err(e),
    }
}

/// Everything a node keeps, before anything runs.
fn new_ctx(identity: Arc<Identity>, device: Arc<Identity>, paths: Paths, log: Logger, endpoint: quinn::Endpoint, default_cover: Option<u64>) -> Result<Arc<Ctx>> {
    let manual = crate::myaddress::load(&paths.manual_address());
    let receive_limit = crate::settings::receive_limit(&paths.settings());
    let discoverable = crate::discoverable::load(&paths.discoverable());
    let (call_events, call_events_rx) = mpsc::unbounded_channel();
    Ok(Arc::new(Ctx {
        identity,
        device,
        paths,
        log,
        endpoint,
        live: Mutex::new(HashMap::new()),
        default_cover,
        call_events,
        call_events_rx: Mutex::new(Some(call_events_rx)),
        peers: Mutex::new(HashMap::new()),
        names_by_key: Mutex::new(HashMap::new()),
        cover_live: Mutex::new(HashMap::new()),
        transfer_sent: Mutex::new(HashMap::new()),
        channel_tx: tokio::sync::broadcast::channel(channels::BROADCAST_CAPACITY).0,
        channel_updates: watch::channel(0).0,
        intros: Mutex::new(HashMap::new()),
        books_arriving: std::sync::atomic::AtomicUsize::new(0),
        withdrawals_wake: tokio::sync::Notify::new(),
        message_updates: watch::channel(0).0,
        my_profile: watch::channel(0).0,
        manual_address: watch::channel(manual).0,
        discoverable: watch::channel(discoverable).0,
        next_connection_id: std::sync::atomic::AtomicU64::new(0),
        connected: Mutex::new(HashMap::new()),
        seen_as: Mutex::new(HashMap::new()),
        sent_from: Mutex::new(HashMap::new()),
        join_offer: Mutex::new(None),
        device_news: Mutex::new(Vec::new()),
        carbon_tx: tokio::sync::broadcast::channel(256).0,
        delivered: Mutex::new(std::collections::VecDeque::new()),
        siblings: Mutex::new(HashMap::new()),
        device_calls: Mutex::new(HashMap::new()),
        reached: Mutex::new(HashMap::new()),
        reach_delays: Mutex::new(HashMap::new()),
        relay_choices: Mutex::new(HashMap::new()),
        link_calls: Mutex::new(HashMap::new()),
        held_link_calls: Mutex::new(HashMap::new()),
        links_in_calls: Mutex::new(std::collections::HashSet::new()),
        through_calls: Mutex::new(HashMap::new()),
        link_files: Mutex::new(HashMap::new()),
        through_files: Mutex::new(HashMap::new()),
        device_updates: watch::channel(0).0,
        my_addresses: watch::channel(Vec::new()).0,
        link_scopes: Mutex::new(Vec::new()),
        answered: Mutex::default(),
        cards: Mutex::new(Default::default()),
        moves: Mutex::new(Vec::new()),
        settle: Mutex::new(settle::Settle::default()),
        deleted: Mutex::new(HashMap::new()),
        forgetting: Mutex::new(()),
        receive_limit: watch::channel(receive_limit).0,
        their_limits: Mutex::new(HashMap::new()),
        stopped: AtomicBool::new(false),
        wiped: AtomicBool::new(false),
        call_media_at: Mutex::new(None),
    }))
}

fn ctx_call_events_rx(ctx: &Ctx) -> mpsc::UnboundedReceiver<CallEvent> {
    ctx.call_events_rx.locked().take().expect("taken once, when the node starts")
}

/// Starts what a node runs: a task per peer and the node-wide loops.
/// (The listener accepts on its endpoint.)
fn run(ctx: &Arc<Ctx>) -> Result<NodeHandle> {
    let known = crate::peers::load(&ctx.paths.peers()).with_context(|| format!("loading {}", ctx.paths.peers().display()))?;
    if known.is_empty() {
        log!(ctx, "no contacts in {} yet", ctx.paths.peers().display());
    }
    let handle = NodeHandle { ctx: ctx.clone() };
    for peer in known {
        handle.start_peer(peer);
    }
    ownpeers::tidy(ctx);
    if let Err(e) = joining::list_me(ctx, None) {
        log!(ctx, "couldn't put this device on your devices' list: {e:#}");
    }
    tokio::spawn(devices::keep_siblings(ctx.clone()));
    tokio::spawn(follow::keep_following(ctx.clone()));
    tokio::spawn(naming::keep_names(ctx.clone()));
    tokio::spawn(cards::keep_sending(ctx.clone()));
    tokio::spawn(sweep_expired(ctx.clone()));
    tokio::spawn(withdraw::keep_telling(ctx.clone()));
    tokio::spawn(tell_tally(ctx.clone()));
    tokio::spawn(limits::watch(ctx.clone()));
    Ok(handle)
}

impl NodeHandle {
    pub fn identity(&self) -> &Arc<Identity> {
        &self.ctx.identity
    }

    /// This device's own key (see `device_identity`).
    pub fn device(&self) -> &Arc<Identity> {
        &self.ctx.device
    }

    pub fn paths(&self) -> &Paths {
        &self.ctx.paths
    }

    /// Where peers and channel members have seen this device's
    /// connections come from (root key, address) — what
    /// this device looks like from outside.
    pub fn seen_as(&self) -> Vec<([u8; 32], SocketAddr)> {
        self.ctx.seen_as.locked().iter().map(|(k, a)| (*k, *a)).collect()
    }

    /// Where contact `user` (any device of theirs) has seen this
    /// device's connections come from: "They see you at".
    pub fn seen_by(&self, user: &[u8; 32]) -> Vec<std::net::IpAddr> {
        let keys: Vec<[u8; 32]> = peerdevices::known(&self.ctx, user).into_iter().chain([*user]).collect();
        let mut ips: Vec<std::net::IpAddr> = Vec::new();
        for (key, addr) in self.ctx.seen_as.locked().iter() {
            let ip = crate::transport::canonical(*addr).ip();
            if keys.contains(key) && !ips.contains(&ip) {
                ips.push(ip);
            }
        }
        ips
    }

    /// The address the user set by hand (see `myaddress`), as kept now:
    /// looked at afresh, so one set by `swarm address` while this runs
    /// is noticed, and the user's other devices told.
    pub fn manual_address(&self) -> crate::myaddress::Manual {
        let now = crate::myaddress::load(&self.ctx.paths.manual_address());
        self.ctx.manual_address.send_if_modified(|kept| {
            let changed = *kept != now;
            if changed {
                *kept = now.clone();
            }
            changed
        });
        now
    }

    /// Sets the address the user set by hand to `text` (comma-separated;
    /// blank clears it), on this device and, over their links, all of the
    /// user's devices. What's wrong with it, if anything.
    pub fn set_manual_address(&self, text: &str) -> Result<crate::myaddress::Manual, String> {
        let items = crate::myaddress::check(text)?;
        let manual = {
            let _guard = crate::store::lock();
            crate::myaddress::set(&self.ctx.paths.manual_address(), items, unix_now()).map_err(|e| format!("couldn't keep it: {e}"))?
        };
        self.ctx.manual_address.send_replace(manual.clone());
        Ok(manual)
    }

    /// Whether the user is discoverable on the network each device is on
    /// (see `discoverable`), as kept now: looked at afresh, so one set by
    /// `swarm discoverable` while this runs is noticed, and the user's
    /// other devices told.
    pub fn discoverable(&self) -> crate::discoverable::Discoverable {
        let now = crate::discoverable::load(&self.ctx.paths.discoverable());
        self.ctx.discoverable.send_if_modified(|kept| {
            let changed = *kept != now;
            if changed {
                *kept = now;
            }
            changed
        });
        now
    }

    /// Makes the user discoverable or not, on this device and, over their
    /// links, all of the user's devices.
    pub fn set_discoverable(&self, on: bool) -> Result<crate::discoverable::Discoverable, String> {
        let kept = {
            let _guard = crate::store::lock();
            crate::discoverable::set(&self.ctx.paths.discoverable(), on, unix_now()).map_err(|e| format!("couldn't keep it: {e}"))?
        };
        self.ctx.discoverable.send_replace(kept);
        Ok(kept)
    }

    /// Changes whenever whether the user is discoverable does, here or
    /// from another of their devices.
    pub fn discoverable_updates(&self) -> watch::Receiver<crate::discoverable::Discoverable> {
        self.ctx.discoverable.subscribe()
    }

    /// Where this device can be reached now (`addresses`, most useful
    /// first), told to every peer when they connect and again whenever it
    /// changes; and the interfaces (`link_scopes`, by index) a peer's
    /// IPv6 link-local address is tried on.
    pub fn set_my_addresses(&self, addresses: Vec<String>, link_scopes: Vec<u32>) {
        *self.ctx.link_scopes.locked() = link_scopes;
        let moved = self.ctx.my_addresses.send_if_modified(|current| {
            let changed = *current != addresses;
            *current = addresses;
            changed
        });
        if moved {
            self.ctx.answered.locked().forget_private();
        }
    }

    /// Where peer `key` has been seen or said it can be reached, most
    /// recent first (see `addressbook`).
    /// For a user, their devices' too (see `peerdevices`), each device's
    /// newest first.
    pub fn learned_addresses(&self, key: &[u8; 32]) -> Vec<String> {
        let mut all = Vec::new();
        for of in peerdevices::known(&self.ctx, key).iter().chain([key]) {
            for address in crate::addressbook::load(&self.ctx.paths.addresses(), of) {
                if !all.contains(&address) {
                    all.push(address);
                }
            }
        }
        all
    }

    /// Peers who've said they've moved since this was last asked, each
    /// with where they say they are now.
    pub fn take_moves(&self) -> Vec<(String, Vec<String>)> {
        std::mem::take(&mut *self.ctx.moves.locked())
    }

    /// Where peer `key` last said it can be reached, and when.
    /// For a user, the newest any of their devices told.
    pub fn told_addresses(&self, key: &[u8; 32]) -> Option<crate::addressbook::Told> {
        peerdevices::known(&self.ctx, key).iter().chain([key]).filter_map(|of| crate::addressbook::told(&self.ctx.paths.addresses(), of)).max_by_key(|t| t.at)
    }

    /// Whether a call goes through a link to another of the user's
    /// devices: this one relaying it, or in it through another.
    pub fn in_call_through_links(&self) -> bool {
        !self.ctx.links_in_calls.locked().is_empty()
    }

    /// The connection to peer `key` now, if there is one: where it runs.
    pub fn connection_to(&self, key: &[u8; 32]) -> Option<InUse> {
        let Some(conn) = self.ctx.live(key) else {
            return proxy::through_name(&self.ctx, key).map(InUse::Through);
        };
        let there = crate::transport::canonical(conn.remote_address());
        let port = self.ctx.endpoint.local_addr().map(|a| a.port()).unwrap_or(crate::transport::DEFAULT_PORT);
        let mut sent_from = self.ctx.sent_from.locked();
        let ip = match sent_from.get(key) {
            Some((id, to, ip)) if *id == conn.stable_id() && *to == there => *ip,
            _ => {
                let ip = conn.local_ip().map(|ip| ip.to_canonical()).or_else(|| crate::transport::source_for(there));
                sent_from.insert(*key, (conn.stable_id(), there, ip));
                ip
            }
        };
        Some(InUse::Direct { here: ip, port, there, dialled: conn.side().is_client() })
    }

    /// A line in the node's log (`NodeConfig::log`), for parts of the
    /// app that run beside it (finding each other on a network, say).
    pub fn log(&self, line: String) {
        (self.ctx.log)(line)
    }

    /// Where the node is listening.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.ctx.endpoint.local_addr()?)
    }

    /// The cover rate for peers without one of their own.
    pub fn default_cover(&self) -> Option<u64> {
        self.ctx.default_cover
    }

    /// The handle of the entry someone means by `who`: `who` itself if
    /// it's one; or the handle of the only peer with that name, for a
    /// name as typed. Two with the name: nobody is meant, and whatever
    /// asked finds no such entry (`peers::find` says why, for commands).
    pub fn resolve(&self, who: &str) -> String {
        let peers = self.ctx.peers.locked();
        if peers.contains_key(who) {
            return who.to_string();
        }
        let users = || peers.iter().filter(|(_, e)| e.device.is_none());
        let mut named = users().filter(|(_, e)| e.watch.borrow().name == who).map(|(handle, _)| handle);
        if let (Some(one), None) = (named.next(), named.next()) {
            return one.clone();
        }
        if let Some((name, start)) = who.rsplit_once('~') {
            let mut told = users().filter(|(handle, e)| e.watch.borrow().name == name && handle.starts_with(start)).map(|(handle, _)| handle);
            if let (Some(one), None) = (told.next(), told.next()) {
                return one.clone();
            }
        }
        who.to_string()
    }

    /// Why `who` didn't lead to a contact.
    pub(crate) fn no_contact(&self, who: &str) -> anyhow::Error {
        match crate::peers::load(&self.ctx.paths.peers()).ok().map(|all| crate::peers::find(&all, who).map(|_| ())) {
            Some(Err(several @ crate::peers::Lookup::Several(..))) => anyhow::anyhow!("{several}"),
            _ => anyhow::anyhow!("no contact named '{who}'"),
        }
    }

    /// Notes in peer `name`'s chat a call from them, started ringing at
    /// `at`, that nobody answered (`history::Direction::Missed`): each of
    /// the user's devices that rang notes it, at the same time, so it's
    /// one line on each and read on one is read on all. Not for a call
    /// from another of the user's own devices, which has no chat.
    pub fn note_missed_call(&self, name: &str, video: bool, at: u64) {
        if devices::call_device(name).is_some() {
            return;
        }
        let name = &*self.resolve(name);
        let Some(key) = self.ctx.peers.locked().get(name).map(|e| e.watch.borrow().public_key) else { return };
        let _forgetting = self.ctx.forgetting.locked();
        if ownpeers::is_gone(&self.ctx, &key) || just_deleted(&self.ctx, &key) {
            return;
        }
        let expires = crate::disappearing::expires(at, crate::disappearing::get(&self.ctx.paths.settings(), &key).seconds);
        let what = if video { "video call" } else { "call" };
        if let Ok(true) = crate::history::append_new(&self.ctx.paths.history(), &key, crate::history::Direction::Missed, what, at, expires, None) {
            self.ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
        }
    }

    /// What peer `handle` is called, for showing: their name, whichever
    /// of their entries that's the handle of.
    pub fn label_of(&self, handle: &str) -> String {
        peerdevices::user_name(&self.ctx.shown(handle)).to_string()
    }

    pub fn knows(&self, name: &str) -> bool {
        let name = &*self.resolve(name);
        self.ctx.peers.locked().contains_key(name)
    }

    /// Peer `name`'s status: for a user whose devices have been met,
    /// theirs together — online if any is, and since when it came on. For
    /// another of your devices (`devices::call_name`), whether it's
    /// linked.
    pub fn peer_status(&self, name: &str) -> Option<PeerStatus> {
        let name = &*self.resolve(name);
        if let Some(device) = devices::call_device(name) {
            devices::call_handle(&self.ctx, &device)?;
            let online = devices::linked_to(&self.ctx, &device) || devices::reached_through(&self.ctx, &device).is_some();
            return Some(PeerStatus { online, not_added: false, removed_us: false, asked: false, cover: None, since: None });
        }
        let user = self.ctx.peers.locked().get(name).filter(|e| e.device.is_none()).map(|e| e.watch.borrow().public_key);
        let devices = user.map(|user| peerdevices::entries(&self.ctx, &user)).unwrap_or_default();
        if !devices.is_empty() {
            let all: Vec<PeerStatus> = devices.iter().filter_map(|d| self.one_status(d)).collect();
            let online = all.iter().find(|s| s.online);
            return Some(PeerStatus {
                online: online.is_some() || user.is_some_and(|u| proxy::through(&self.ctx, &u).is_some()),
                not_added: all.iter().all(|s| s.not_added),
                removed_us: all.iter().any(|s| s.removed_us),
                asked: all.iter().any(|s| s.asked),
                cover: online.or(all.first()).and_then(|s| s.cover),
                since: online.map_or_else(|| all.iter().filter_map(|s| s.since).max(), |s| s.since),
            });
        }
        let mut status = self.one_status(name)?;
        let user = user.or_else(|| {
            let user = self.ctx.peers.locked().get(name).map(|e| e.watch.borrow().public_key)?;
            (proxycall::standing_name(&self.ctx, &user).as_deref() == Some(name)).then_some(user)
        });
        if !status.online && user.is_some_and(|u| proxy::through(&self.ctx, &u).is_some()) {
            status.online = true;
        }
        Some(status)
    }

    fn one_status(&self, name: &str) -> Option<PeerStatus> {
        let peers = self.ctx.peers.locked();
        let entry = peers.get(name)?;
        Some(PeerStatus {
            online: entry.online.load(Ordering::Relaxed),
            not_added: entry.not_added.load(Ordering::Relaxed),
            removed_us: entry.removed_us.load(Ordering::Relaxed),
            asked: entry.asked.load(Ordering::Relaxed),
            cover: self.ctx.cover_live.locked().get(name).copied(),
            since: Some(entry.since.load(Ordering::Relaxed)).filter(|&t| t > 0),
        })
    }

    /// Closes every connection at once, for quitting: peers see us go
    /// offline straight away, not after their liveness timeout. Waits
    /// up to half a second for the closes to go out.
    pub async fn shutdown(&self) {
        self.ctx.stopped.store(true, Ordering::Relaxed);
        self.ctx.withdrawals_wake.notify_one();
        self.ctx.endpoint.close(0u32.into(), b"");
        let _ = tokio::time::timeout(Duration::from_millis(500), self.ctx.endpoint.wait_idle()).await;
    }

    /// Peer `name`'s calls: on one of their devices that's online, for a
    /// user whose devices have been met (see `reaching`); or another of
    /// your devices', named by `devices::call_name`.
    pub fn call_handle(&self, name: &str) -> Option<PeerCallHandle> {
        let name = &*self.resolve(name);
        if let Some(device) = devices::call_device(name) {
            return devices::call_handle(&self.ctx, &device);
        }
        let name = self.reaching(name);
        let (user, call) = self.ctx.peers.locked().get(&name).map(|e| (e.watch.borrow().public_key, e.call.clone()))?;
        if proxycall::goes_through(&self.ctx, &user) {
            return Some(proxycall::handle(&self.ctx, user));
        }
        Some(call)
    }

    /// Whether calls with peer `name` go through another of your devices
    /// (see `proxycall`): one call then, whichever of their entries it's
    /// placed on, since that device rings each of theirs it reaches.
    pub fn calls_go_through(&self, name: &str) -> bool {
        let name = &*self.resolve(name);
        let user = self.ctx.peers.locked().get(name).map(|e| e.watch.borrow().public_key);
        user.is_some_and(|user| proxycall::goes_through(&self.ctx, &user))
    }

    /// For a front end to report the outcome of a call it placed, or the
    /// end of one, alongside the node's own call events.
    pub fn call_events(&self) -> mpsc::UnboundedSender<CallEvent> {
        self.ctx.call_events.clone()
    }

    /// Starts keeping a connection to `peer` (already in the peers file).
    /// A no-op if this node already has a peer by that name, or if it's
    /// one of this account's own devices (those have links of their own).
    /// Added by the user just now: in the peers book at once, as now
    /// (`ownpeers::added`).
    pub fn add_peer_live(&self, peer: Peer) {
        let key = peer.public_key;
        self.keep_request(&key);
        self.start_peer(peer);
        ownpeers::added(&self.ctx, &key);
    }

    /// `add_peer_live` but for the book: one already on the list, or
    /// taken in from another of the user's devices.
    pub(super) fn start_peer(&self, peer: Peer) {
        self.ctx.deleted.locked().remove(&peer.public_key);
        let _ = crate::requests::remove(&self.ctx.paths.requests(), &peer.public_key);
        if self.knows(&peer.handle()) {
            return;
        }
        peer::spawn(&self.ctx, peer.clone());
        peerdevices::spawn_known(&self.ctx, &peer);
    }

    /// `name`'s record changed (address, key, or a rename to
    /// `peer.name`): its connection is re-established with the new one.
    pub fn update_peer_live(&self, name: &str, mut peer: Peer) {
        let name = &*self.resolve(name);
        let current = self.ctx.peers.locked().get(name).map(|e| e.watch.borrow().clone());
        let Some(current) = current else {
            self.add_peer_live(peer);
            return;
        };
        peer.cover_bits_per_second = current.cover_bits_per_second;
        peer.cover_required = current.cover_required;
        if peer.public_key != current.public_key {
            ownpeers::forgotten(&self.ctx, &current.public_key, &current.name);
            self.remove_peer_live(name);
            self.add_peer_live(peer);
        } else {
            if let Some(entry) = self.ctx.peers.locked().get(name) {
                let _ = entry.watch.send(peer.clone());
            }
            peerdevices::update_all(&self.ctx, &peer);
        }
    }

    /// `name`'s cover setting changed.
    pub fn set_cover_live(&self, name: &str, bits_per_second: Option<u64>, required: bool) {
        let name = &*self.resolve(name);
        let updated = self.ctx.peers.locked().get(name).map(|entry| {
            entry.watch.send_modify(|peer| {
                peer.cover_bits_per_second = bits_per_second;
                peer.cover_required = required && bits_per_second.is_some();
            });
            entry.watch.borrow().clone()
        });
        if let Some(peer) = updated {
            peerdevices::update_all(&self.ctx, &peer);
        }
    }

    /// Stops keeping a connection to `name`.
    pub fn remove_peer_live(&self, name: &str) {
        let name = &*self.resolve(name);
        let removed = self.ctx.peers.locked().remove(name);
        if let Some(entry) = removed {
            if entry.device.is_none() {
                peerdevices::stop_all(&self.ctx, &entry.watch.borrow().public_key);
            }
            self.ctx.names_by_key.locked().retain(|_, existing| existing != name);
            self.ctx.cover_live.locked().remove(name);
        }
    }

    /// Contact `name`, key `key`, deleted (or blocked), and already off
    /// the peers list: transfers with them cancelled, their connections
    /// ended, and everything kept about them deleted
    /// (`crate::forget::contact`), here and, through the peers book, on
    /// the user's other devices. Told they're removed if they dial soon.
    /// If they hadn't accepted our request, they're told it's cancelled.
    pub fn forget_contact(&self, name: &str, key: [u8; 32], block: bool) {
        self.forget_contact_then(name, key, block, true);
    }

    /// `forget_contact`; `tidy`: the book brought up to date with the
    /// peers list after. Not while another device's book is being taken
    /// in (`ownpeers::apply`): part-way through, a peer it adds isn't on
    /// the list yet, and would be recorded as deleted here.
    pub(super) fn forget_contact_then(&self, name: &str, key: [u8; 32], block: bool, tidy: bool) {
        let name = &*self.resolve(name);
        let typed = self.ctx.peers.locked().get(name).map(|e| e.watch.borrow().address.clone()).unwrap_or_default();
        self.cancel_request_if_asked(&key, &typed);
        let devices = peerdevices::known(&self.ctx, &key);
        for t in crate::transfer::load(&self.ctx.paths.transfers()) {
            if !t.state.is_finished() && (t.peer == key || devices.contains(&t.peer)) {
                let _ = self.cancel_transfer(&t.id);
            }
        }
        let shown = self.label_of(name);
        self.remove_peer_live(name);
        forget_here(&self.ctx, key, &shown, block, tidy);
        self.ctx.withdrawals_wake.notify_one();
    }

    /// Sends `text` to peer `name`. It goes into their outbox first and
    /// leaves only once they've acknowledged it, so whatever this
    /// returns, it's never lost and never arrives twice.
    /// Someone on several devices is sent it on all of them (see
    /// `send_to_all`).
    pub async fn send_message(&self, name: &str, text: &str) -> Result<Delivery> {
        let name = &*self.resolve(name);
        let all = self.same_person(name);
        match &all[..] {
            [one] => self.send_one(one, text).await,
            _ => self.send_to_all(&all, text).await,
        }
    }

    /// Every peer entry that's the same person as peer `name`: for a user
    /// whose devices have been met (by name, or by one of theirs), their
    /// devices' entries, which are what's connected (see `peerdevices`);
    /// else `name` alone.
    pub fn same_person(&self, name: &str) -> Vec<String> {
        let name = &*self.resolve(name);
        let user = self.ctx.peers.locked().get(name).map(|e| e.watch.borrow().public_key);
        if let Some(user) = user {
            let devices = peerdevices::entries(&self.ctx, &user);
            if !devices.is_empty() {
                return devices;
            }
        }
        vec![name.to_string()]
    }

    /// The entry to reach peer `name` through, for something that goes
    /// over one connection: a device's entry, itself; else one of their
    /// devices that's online, if any is; else the first of `same_person`.
    pub(crate) fn reaching(&self, name: &str) -> String {
        let name = &*self.resolve(name);
        if self.ctx.peers.locked().get(name).is_some_and(|e| e.device.is_some()) {
            return name.to_string();
        }
        let all = self.same_person(name);
        let peers = self.ctx.peers.locked();
        all.iter().find(|n| peers.get(*n).is_some_and(|e| e.online.load(Ordering::Relaxed))).or(all.first()).cloned().unwrap_or_else(|| name.to_string())
    }

    async fn send_one(&self, name: &str, text: &str) -> Result<Delivery> {
        let shown = self.label_of(name);
        let text = check_message(text)?;
        let (peer, requests, online, not_added) = {
            let peers = self.ctx.peers.locked();
            let entry = peers.get(name).ok_or_else(|| self.no_contact(name))?;
            if entry.removed_us.load(Ordering::Relaxed) {
                anyhow::bail!("{shown} {REMOVED_US}");
            }
            let peer = entry.watch.borrow().clone();
            (peer, entry.requests.clone(), entry.online.load(Ordering::Relaxed), entry.not_added.load(Ordering::Relaxed))
        };
        let (id, sent_at, lifetime) = queue_message(&self.ctx.paths, &peer.public_key, &text)?;
        devices::carbon(&self.ctx, peer.public_key, id, true, sent_at, crate::disappearing::expires(sent_at, lifetime), &text);
        if not_added {
            return Ok(Delivery::HeldUntilAdded);
        }
        if !online {
            return Ok(self.unconnected(&peer.public_key));
        }
        let (reply_tx, reply_rx) = reply::channel();
        if requests.send(peer::PeerRequest::Send { id, text, sent_at, lifetime, reply: reply_tx }).await.is_err() {
            return Ok(Delivery::Queued);
        }
        match tokio::time::timeout(peer::SEND_CONFIRM_WAIT + Duration::from_secs(2), reply_rx).await {
            Ok(Ok(Ok(()))) => Ok(Delivery::Delivered),
            Ok(Ok(Err(e))) if e.to_string().contains(NOT_ADDED) => Ok(Delivery::HeldUntilAdded),
            _ => Ok(Delivery::Queued),
        }
    }

    /// Sends `text` to someone on several devices — peers `names`, all one
    /// person: the same message (one id) goes into each one's outbox and
    /// to each that's online, each acknowledging it for itself, so a
    /// device that's off gets it when it's back. It's in the history
    /// once, with the first. Delivered if any device has it.
    pub async fn send_to_all(&self, names: &[String], text: &str) -> Result<Delivery> {
        if names.len() < 2 {
            return self.send_one(names.first().map(String::as_str).unwrap_or_default(), text).await;
        }
        let text = check_message(text)?;
        let id = outbox::new_message_id();
        let sent_at = unix_now();
        let mut targets = Vec::new();
        for name in names {
            let peers = self.ctx.peers.locked();
            let Some(entry) = peers.get(name) else { continue };
            let key = entry.watch.borrow().public_key;
            targets.push((key, entry.requests.clone(), entry.online.load(Ordering::Relaxed), entry.not_added.load(Ordering::Relaxed)));
        }
        let Some(first) = targets.first().map(|t| t.0) else { anyhow::bail!("none of them is a contact") };
        let lifetime = crate::disappearing::get(&self.ctx.paths.settings(), &first).seconds;
        let expires = crate::disappearing::expires(sent_at, lifetime);
        let mut queued: Vec<[u8; 32]> = Vec::new();
        for (key, ..) in &targets {
            if !queued.contains(key) {
                outbox::enqueue(&self.ctx.paths.outbox(), key, id, &text, sent_at, lifetime).context("queuing message")?;
                queued.push(*key);
            }
        }
        let _ = crate::waiting::add(&self.ctx.paths.waiting(), &first, id, sent_at, expires, &text);
        let _ = crate::history::append_message(&self.ctx.paths.history(), &first, crate::history::Direction::Sent, &text, sent_at, expires, Some(id));
        devices::carbon(&self.ctx, first, id, true, sent_at, expires, &text);
        let mut waiting = Vec::new();
        for (_, requests, online, not_added) in &targets {
            if *online && !*not_added {
                let (reply_tx, reply_rx) = reply::channel();
                if requests.send(peer::PeerRequest::Send { id, text: text.clone(), sent_at, lifetime, reply: reply_tx }).await.is_ok() {
                    waiting.push(reply_rx);
                }
            }
        }
        let mut delivered = false;
        for reply_rx in waiting {
            if let Ok(Ok(Ok(()))) = tokio::time::timeout(peer::SEND_CONFIRM_WAIT + Duration::from_secs(2), reply_rx).await {
                delivered = true;
            }
        }
        Ok(if delivered {
            Delivery::Delivered
        } else if targets.iter().all(|t| t.3) {
            Delivery::HeldUntilAdded
        } else if targets.iter().any(|t| t.2) {
            Delivery::Queued
        } else {
            self.unconnected(&first)
        })
    }

    /// What became of a message to peer `user`, with no connection of
    /// this device's own to them: on its way through another of the
    /// user's devices that reaches them, or waiting for them to be back.
    fn unconnected(&self, user: &[u8; 32]) -> Delivery {
        if proxy::through(&self.ctx, user).is_some() {
            Delivery::Relayed
        } else {
            Delivery::Queued
        }
    }

    /// The round-trip time to peer `name` over their live connection.
    pub async fn ping(&self, name: &str) -> Result<Duration> {
        let name = &*self.resolve(name);
        let shown = self.label_of(name);
        let reaching = self.reaching(name);
        let requests = {
            let peers = self.ctx.peers.locked();
            let entry = peers.get(&reaching).ok_or_else(|| self.no_contact(name))?;
            if entry.not_added.load(Ordering::Relaxed) {
                anyhow::bail!("{shown} {NOT_ADDED}");
            }
            if !entry.online.load(Ordering::Relaxed) {
                anyhow::bail!("{shown} is offline");
            }
            entry.requests.clone()
        };
        let (reply_tx, reply_rx) = reply::channel();
        requests.send(peer::PeerRequest::Rtt { reply: reply_tx }).await.map_err(|_| anyhow::anyhow!("{shown}'s connection task is gone"))?;
        tokio::time::timeout(protocol::REPLY_TIMEOUT, reply_rx)
            .await
            .map_err(|_| anyhow::anyhow!("no answer from {shown}'s connection"))?
            .map_err(|_| anyhow::anyhow!("{shown}'s connection dropped"))
    }

    /// Peer `name`'s posts: what's new since the newest one cached is
    /// fetched over their connection and merged into the cache; if they
    /// can't be reached, the cache is what comes back.
    pub async fn fetch_posts(&self, name: &str) -> Result<PostsResult> {
        let name = &*self.resolve(name);
        let shown = self.label_of(name);
        let reaching = self.reaching(name);
        let (peer, requests, online, not_added) = {
            let peers = self.ctx.peers.locked();
            let entry = peers.get(&reaching).ok_or_else(|| self.no_contact(name))?;
            let peer = entry.watch.borrow().clone();
            (peer, entry.requests.clone(), entry.online.load(Ordering::Relaxed), entry.not_added.load(Ordering::Relaxed))
        };
        let fetched = if not_added {
            Err(anyhow::anyhow!("{shown} {NOT_ADDED}"))
        } else if !online {
            Err(anyhow::anyhow!("{shown} is offline"))
        } else {
            let since = newest_cached(&self.ctx.paths, &peer.public_key);
            let (reply_tx, reply_rx) = reply::channel();
            match requests.send(peer::PeerRequest::ListPosts { since, reply: reply_tx }).await {
                Ok(()) => reply_rx.await.unwrap_or_else(|_| Err(anyhow::anyhow!("{shown}'s connection dropped"))),
                Err(_) => Err(anyhow::anyhow!("{shown}'s connection task is gone")),
            }
        };
        posts_result(&self.ctx.paths, name, &peer.public_key, &[peer.public_key], fetched, &*self.ctx.log, || true)
    }
}

/// Your status and picture, for peers only (see `profile`).
impl NodeHandle {
    /// Publishes this device's status ("" for none) and picture (a JPEG
    /// as `profile` allows, empty for none), and tells every peer
    /// connected now; the rest ask when they next connect.
    pub fn set_profile(&self, status: &str, avatar: &[u8]) -> std::io::Result<()> {
        crate::profile::set(&self.ctx.paths.profile(), &self.ctx.identity, status, avatar)?;
        self.ctx.my_profile.send_modify(|n| *n = n.wrapping_add(1));
        Ok(())
    }
}

/// The user's devices — see `devices`.
impl NodeHandle {
    /// The user's devices, this one among them, as the list has them.
    pub fn devices(&self) -> devices::DevicesView {
        devices::view(&self.ctx)
    }

    /// A code for adding a device to this user, good once and for
    /// `joining::CODE_LIFETIME` (see `joining`).
    pub fn add_device_code(&self) -> Result<joining::JoinCode> {
        joining::offer(&self.ctx)
    }

    /// Counts up whenever anything about devices changes.
    pub fn device_updates(&self) -> watch::Receiver<u64> {
        self.ctx.device_updates.subscribe()
    }

    /// The timer on the conversation with peer `name` (see
    /// `disappearing`): off unless one's been set.
    pub fn timer(&self, name: &str) -> crate::disappearing::Timer {
        let name = &*self.resolve(name);
        let Some(key) = self.ctx.peers.locked().get(name).map(|e| e.watch.borrow().public_key) else { return Default::default() };
        crate::disappearing::get(&self.ctx.paths.settings(), &key)
    }

    /// Sets the timer on the conversation with peer `name` (and their
    /// other devices) to `seconds` (0, off), and tells them — now if
    /// they're connected, else when they are. Messages sent from now on
    /// delete themselves after that long; ones already sent keep theirs.
    pub fn set_timer(&self, name: &str, seconds: u64) -> Result<()> {
        let name = &*self.resolve(name);
        let names = self.same_person(name);
        let peers = self.ctx.peers.locked();
        let now = unix_now();
        let latest = names
            .iter()
            .filter_map(|n| peers.get(n))
            .map(|e| crate::disappearing::get(&self.ctx.paths.settings(), &e.watch.borrow().public_key))
            .filter(|t| t.believable(now))
            .map(|t| t.set_at)
            .max()
            .unwrap_or(0);
        let timer = crate::disappearing::Timer { seconds, set_at: now.max(latest + 1), by: self.ctx.identity.public_key_bytes() };
        let mut any = false;
        for name in &names {
            let Some(entry) = peers.get(name) else { continue };
            any = true;
            crate::disappearing::merge(&self.ctx.paths.settings(), &entry.watch.borrow().public_key, timer)?;
            let _ = entry.requests.try_send(peer::PeerRequest::Notice(protocol::Notice::Timer(timer)));
        }
        if !any {
            return Err(self.no_contact(name));
        }
        Ok(())
    }

    /// Whether this device has been taken off the user's devices and
    /// its folder wiped (see `devices::taken_off`): the node's stopped.
    pub fn wiped(&self) -> bool {
        self.ctx.wiped.load(Ordering::Relaxed)
    }

    /// What's happened with the user's devices since last asked, and
    /// timers on conversations set by others.
    pub fn device_news(&self) -> Vec<String> {
        std::mem::take(&mut *self.ctx.device_news.locked())
    }

    /// Takes `key` off the user's devices, for good (not this device:
    /// another of theirs does). It wipes itself when it hears, and the
    /// user's peers refuse it (see `devices::remove`).
    pub fn remove_device(&self, key: [u8; 32]) -> Result<()> {
        devices::remove(&self.ctx, key)
    }

    /// Lets `key`, one of the user's devices, relay for the others —
    /// peers who can't reach one of them directly go through it — or
    /// stops it. Whether anything changed. Every device is told.
    pub fn allow_relay(&self, key: [u8; 32], allowed: bool) -> Result<bool> {
        devices::allow_relay(&self.ctx, key, allowed)
    }

    /// Sets this device to dial no one and only take connections made to
    /// it, or to dial again (its own setting: see
    /// `devices::set_receive_only`). Whether anything changed.
    pub fn set_receive_only(&self, on: bool) -> Result<bool> {
        devices::set_receive_only(&self.ctx, on)
    }

    /// Sets this device's bandwidth limit (`None`: none), kept for next
    /// time and in force at once (see `transport::set_bandwidth_limit`):
    /// connections whose cover is over it start again under it.
    pub fn set_bandwidth_limit(&self, limit: Option<u64>) -> Result<()> {
        crate::settings::set_bandwidth_limit(&self.ctx.paths.settings(), limit)?;
        crate::transport::set_bandwidth_limit(limit);
        match limit {
            Some(limit) => log!(self.ctx, "bandwidth limit: {}bit/s", format_bitrate(limit)),
            None => log!(self.ctx, "bandwidth limit: none"),
        }
        Ok(())
    }

    /// This device's bandwidth limit, if it has one.
    pub fn bandwidth_limit(&self) -> Option<u64> {
        crate::transport::bandwidth_limit()
    }

    /// Sets this device's receive limit (`None`: none), kept for next time:
    /// every contact connected is told at once and keeps what it sends
    /// under it (call video straight away, cover from its next
    /// connection), and files from them are asked for no faster.
    pub fn set_receive_limit(&self, limit: Option<u64>) -> Result<()> {
        crate::settings::set_receive_limit(&self.ctx.paths.settings(), limit)?;
        self.ctx.receive_limit.send_replace(limit);
        match limit {
            Some(limit) => log!(self.ctx, "receive limit: {}bit/s", format_bitrate(limit)),
            None => log!(self.ctx, "receive limit: none"),
        }
        Ok(())
    }

    /// This device's receive limit, if it has one.
    pub fn receive_limit(&self) -> Option<u64> {
        *self.ctx.receive_limit.borrow()
    }

    /// Whether this device is set to receive connections only.
    pub fn receiving_only(&self) -> bool {
        devices::receiving_only(&self.ctx)
    }

    /// Sets the address typed for `key`, another of the user's devices
    /// (IPs or host names, each with a port if it isn't the usual one,
    /// separated by commas; empty clears it), and tries it now. The
    /// items, as kept.
    pub fn set_device_address(&self, key: [u8; 32], text: &str) -> Result<Vec<String>> {
        devices::set_address(&self.ctx, key, text)
    }

    /// Renames this device among the user's devices. Whether anything
    /// changed.
    pub fn rename_this_device(&self, name: &str) -> Result<bool> {
        devices::rename_me(&self.ctx, name)
    }
}

/// One transfer as a front end shows it.
#[derive(Clone, Debug)]
pub struct TransferStatus {
    pub transfer: crate::transfer::Transfer,
    /// The peer's name, if they're still on the peers list.
    pub peer_name: Option<String>,
    /// Their key as a person, if they're still on the peers list: the
    /// transfer's own key is the device's it went to or came from.
    pub peer_key: Option<[u8; 32]>,
    /// How much has arrived (incoming) or been asked for (outgoing).
    pub done_bytes: u64,
}

impl TransferStatus {
    /// Whether it's with the peer whose key (as a person) is `key`, on
    /// any of their devices.
    pub fn is_with(&self, key: &[u8; 32]) -> bool {
        self.transfer.peer == *key || self.peer_key == Some(*key)
    }
}

impl NodeHandle {
    /// The entry a transfer's key (`transfer::Transfer::peer`) is with: a
    /// device's, by its key, or anyone else's by theirs.
    fn entry_for_transfer(&self, key: &[u8; 32]) -> Option<String> {
        let peers = self.ctx.peers.locked();
        peers
            .iter()
            .find(|(_, e)| e.device == Some(*key))
            .or_else(|| peers.iter().find(|(_, e)| e.device.is_none() && e.watch.borrow().public_key == *key))
            .or_else(|| peers.iter().filter(|(_, e)| e.watch.borrow().public_key == *key).min_by_key(|(n, _)| (*n).clone()))
            .map(|(n, _)| n.clone())
    }

    /// The same, and their key as a person.
    fn peer_for(&self, key: &[u8; 32]) -> Option<(String, [u8; 32])> {
        let name = self.entry_for_transfer(key)?;
        let user = self.ctx.peers.locked().get(&name).map(|e| e.watch.borrow().public_key)?;
        Some((peerdevices::user_name(&name).to_string(), user))
    }

    /// Tells `peer`'s task something changed in a transfer with them —
    /// `notice` goes now if they're connected, and the store makes sure
    /// it's dealt with next time if they aren't.
    /// Not connected to them, but reaching them through another of the
    /// user's devices: it goes through that (see `proxyfile`).
    fn poke_transfer(&self, peer: &[u8; 32], notice: Option<protocol::Notice>) {
        if let Some(notice) = notice.clone() {
            proxyfile::poke(&self.ctx, peer, notice);
        }
        if let Some(name) = self.entry_for_transfer(peer)
            && let Some(entry) = self.ctx.peers.locked().get(&name) {
                let _ = entry.requests.try_send(peer::PeerRequest::Files(notice));
            }
    }

    /// Offers `path` to peer `name` — to every device of someone on
    /// several, the first to accept taking it (the offer's withdrawn from
    /// the rest). Reading it through for its checksum can take a while
    /// for a big file — done off the async threads. Returns the first
    /// offer's id.
    pub async fn offer_file(&self, name: &str, path: &Path) -> Result<crate::transfer::TransferId> {
        let name = &*self.resolve(name);
        let devices = self.same_person(name);
        let peers: Vec<(Peer, [u8; 32])> = {
            let peers = self.ctx.peers.locked();
            peers.get(name).ok_or_else(|| self.no_contact(name))?;
            devices.iter().filter_map(|n| peers.get(n).map(|e| { let p = e.watch.borrow().clone(); let key = e.device.unwrap_or(p.public_key); (p, key) })).collect::<Vec<_>>()
        };
        let path = path.canonicalize().with_context(|| format!("finding {}", path.display()))?;
        if !path.is_file() {
            anyhow::bail!("{} isn't a file", path.display());
        }
        if !crate::text::is_single_line(&path.to_string_lossy()) {
            anyhow::bail!("can't send a file whose path has control characters in it");
        }
        let hashed = path.clone();
        let (size, sha256) = tokio::task::spawn_blocking(move || crate::transfer::hash_file(&hashed)).await??;
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let created = unix_now();
        let mut first = None;
        for (peer, key) in peers {
            let t = crate::transfer::Transfer {
                id: crate::transfer::new_id(),
                direction: crate::transfer::Direction::Out,
                peer: key,
                state: crate::transfer::State::Offered,
                size,
                sha256,
                created,
                name: crate::transfer::sanitise_name(&file_name),
                path: path.clone(),
                note: String::new(),
                cleared: false,
                settled: false,
            };
            crate::transfer::add(&self.ctx.paths.transfers(), t.clone())?;
            log!(self.ctx, "offered {} ({} bytes) to {}", t.name, t.size, peer.name);
            self.poke_transfer(&key, Some(protocol::Notice::FileOffer { id: t.id, name: t.name, size: t.size, sha256: t.sha256 }));
            first.get_or_insert(t.id);
        }
        first.ok_or_else(|| self.no_contact(name))
    }

    /// Outgoing offer `accepted` was taken: the same offer to the
    /// person's other devices is withdrawn.
    pub(crate) fn withdraw_other_offers(&self, accepted: &crate::transfer::Transfer) {
        use crate::transfer::{Direction, State};
        let person: Vec<[u8; 32]> = peerdevices::user_of(&self.ctx, &accepted.peer).map(|user| peerdevices::known(&self.ctx, &user)).unwrap_or_default();
        let same = |t: &crate::transfer::Transfer| {
            t.id != accepted.id
                && t.direction == Direction::Out
                && t.state == State::Offered
                && person.contains(&t.peer)
                && t.sha256 == accepted.sha256
                && t.created == accepted.created
                && t.name == accepted.name
        };
        let reason = crate::transfer::TAKEN_ELSEWHERE;
        for t in crate::transfer::load(&self.ctx.paths.transfers()).into_iter().filter(same) {
            let _ = crate::transfer::update(&self.ctx.paths.transfers(), &t.id, |t| {
                t.state = State::Cancelled;
                t.note = "accepted on another of their devices".to_string();
                t.cleared = true;
            });
            self.poke_transfer(&t.peer, Some(protocol::Notice::FileCancel { id: t.id, reason: reason.to_string() }));
        }
    }

    /// Accepts offered transfer `id`, to be saved in `folder` (created if
    /// need be).
    pub fn accept_transfer(&self, id: &crate::transfer::TransferId, folder: &Path) -> Result<()> {
        std::fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
        let folder = folder.canonicalize()?;
        let store = self.ctx.paths.transfers();
        if let Some(t) = crate::transfer::get(&store, id)
            && let Some(why) = crate::transfer::no_room(&store, &folder, t.size) {
                anyhow::bail!("{why}");
            }
        let t = crate::transfer::update(&store, id, |t| {
            if t.direction == crate::transfer::Direction::In && t.state == crate::transfer::State::Offered {
                t.state = crate::transfer::State::Active;
                t.path = folder.clone();
            }
        })?
        .ok_or_else(|| anyhow::anyhow!("no such transfer"))?;
        if t.state != crate::transfer::State::Active || t.path != folder {
            anyhow::bail!("that offer has already been answered");
        }
        self.poke_transfer(&t.peer, Some(protocol::Notice::FileAnswer { id: *id, accepted: true }));
        proxyfile::withdraw(&self.ctx, &t, crate::transfer::TAKEN_ELSEWHERE);
        Ok(())
    }

    /// Declines offered transfer `id`.
    pub fn decline_transfer(&self, id: &crate::transfer::TransferId) -> Result<()> {
        let t = crate::transfer::update(&self.ctx.paths.transfers(), id, |t| {
            if t.direction == crate::transfer::Direction::In && t.state == crate::transfer::State::Offered {
                t.state = crate::transfer::State::Declined;
            }
        })?
        .ok_or_else(|| anyhow::anyhow!("no such transfer"))?;
        self.poke_transfer(&t.peer, Some(protocol::Notice::FileAnswer { id: *id, accepted: false }));
        proxyfile::withdraw(&self.ctx, &t, "declined on another of your devices");
        Ok(())
    }

    /// Gives up on transfer `id`, either way round. A partly received
    /// file is deleted.
    pub fn cancel_transfer(&self, id: &crate::transfer::TransferId) -> Result<()> {
        let t = crate::transfer::update(&self.ctx.paths.transfers(), id, |t| {
            if !t.state.is_finished() {
                t.state = crate::transfer::State::Cancelled;
                t.note = "cancelled here".to_string();
            }
        })?
        .ok_or_else(|| anyhow::anyhow!("no such transfer"))?;
        crate::transfer::discard_part(&t);
        self.poke_transfer(&t.peer, Some(protocol::Notice::FileCancel { id: *id, reason: String::new() }));
        proxyfile::withdraw(&self.ctx, &t, "cancelled on another of your devices");
        Ok(())
    }

    /// Clears finished transfers `ids` from the list, here only (open
    /// ones are left). Returns how many were cleared.
    pub fn clear_transfers(&self, ids: &[crate::transfer::TransferId]) -> Result<usize> {
        let n = crate::transfer::clear(&self.ctx.paths.transfers(), ids)?;
        if n > 0 {
            self.ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
        }
        Ok(n)
    }

    /// Every transfer, newest first, but those cleared.
    pub fn transfers(&self) -> Vec<TransferStatus> {
        let sent = self.ctx.transfer_sent.locked().clone();
        let mut all: Vec<TransferStatus> = crate::transfer::load(&self.ctx.paths.transfers())
            .into_iter()
            .filter(|t| !t.cleared)
            .map(|t| {
                let done_bytes = match t.direction {
                    crate::transfer::Direction::In => crate::transfer::received(&t),
                    crate::transfer::Direction::Out if t.state == crate::transfer::State::Done => t.size,
                    crate::transfer::Direction::Out => sent.get(&t.id).copied().unwrap_or(0),
                }
                .min(t.size);
                let (peer_name, peer_key) = self.peer_for(&t.peer).map(|(name, key)| (self.label_of(&name), key)).unzip();
                TransferStatus { peer_name, peer_key, done_bytes, transfer: t }
            })
            .collect();
        all.reverse();
        all
    }
}

/// A message as it can be sent: not empty, not too long, one line.
fn check_message(text: &str) -> Result<String> {
    if text.trim().is_empty() {
        anyhow::bail!("nothing to send");
    }
    if text.len() > protocol::MAX_MESSAGE_BYTES {
        anyhow::bail!("a message can be at most {} bytes", protocol::MAX_MESSAGE_BYTES);
    }
    Ok(crate::text::single_line(text))
}

/// Puts a new message in `public_key`'s outbox and history, under the
/// conversation's timer (see `disappearing`); returns its id, send time
/// and lifetime.
fn queue_message(paths: &Paths, public_key: &[u8; 32], text: &str) -> Result<(outbox::MessageId, u64, u64)> {
    let id = outbox::new_message_id();
    let sent_at = unix_now();
    let lifetime = crate::disappearing::get(&paths.settings(), public_key).seconds;
    outbox::enqueue(&paths.outbox(), public_key, id, text, sent_at, lifetime).context("queuing message")?;
    let expires = crate::disappearing::expires(sent_at, lifetime);
    let _ = crate::waiting::add(&paths.waiting(), public_key, id, sent_at, expires, text);
    let _ = crate::history::append_message(&paths.history(), public_key, crate::history::Direction::Sent, text, sent_at, expires, Some(id));
    Ok((id, sent_at, lifetime))
}

/// A peer's timer for our conversation (see `disappearing`): kept if
/// it's newer than ours, and the person told who set what. Only one set
/// by them or by us counts.
fn on_timer(ctx: &Ctx, name: &str, peer: [u8; 32], timer: crate::disappearing::Timer) {
    let me = ctx.identity.public_key_bytes();
    if timer.by != peer && timer.by != me {
        return;
    }
    if !timer.believable(unix_now()) {
        log!(ctx, "{name}: a timer said to be set {}s from now — their clock's wrong? Not taken", timer.set_at.saturating_sub(unix_now()));
        return;
    }
    if let Ok(true) = crate::disappearing::merge(&ctx.paths.settings(), &peer, timer) {
        devices::news(ctx, match (timer.by == me, timer.seconds) {
            (true, 0) => format!("you turned off deleting messages with {name} (on another device)"),
            (true, s) => format!("you set messages with {name} to delete themselves after {} (on another device)", crate::disappearing::describe(s)),
            (false, 0) => format!("{name} turned off deleting your messages with them: they're kept from now on"),
            (false, s) => format!("{name} set your messages with them to delete themselves after {}, on both sides, from now on", crate::disappearing::describe(s)),
        });
    }
}

/// How long a contact deleted is told so when they dial (see
/// `Ctx::deleted`): long enough for each of their devices connected
/// then to dial again, not so long that their adding the user back
/// soon after isn't a request.
const TOLD_DELETED_FOR: Duration = Duration::from_secs(10 * 60);

/// Contact `key`, off the peers list, deleted from this device's disk
/// (or blocked), and the books tidied (the peers book only if `tidy`:
/// see `NodeHandle::forget_contact_then`).
pub(super) fn forget_here(ctx: &Arc<Ctx>, key: [u8; 32], name: &str, block: bool, tidy: bool) {
    let _forgetting = ctx.forgetting.locked();
    ctx.deleted.locked().insert(key, Instant::now());
    for again in crate::peers::load(&ctx.paths.peers()).unwrap_or_default().into_iter().filter(|p| p.public_key == key) {
        let _ = crate::peers::remove(&ctx.paths.peers(), &again.handle());
        NodeHandle { ctx: ctx.clone() }.remove_peer_live(&again.handle());
    }
    ownpeers::forgotten(ctx, &key, name);
    crate::forget::contact(&ctx.paths, &key, block);
    if tidy {
        ownpeers::tidy(ctx);
    }
    ownrequests::tidy(ctx);
    ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
}

/// Whether `key` was deleted here a moment ago (`TOLD_DELETED_FOR`).
pub(super) fn just_deleted(ctx: &Ctx, key: &[u8; 32]) -> bool {
    let mut deleted = ctx.deleted.locked();
    deleted.retain(|_, at| at.elapsed() < TOLD_DELETED_FOR);
    deleted.contains_key(key)
}

/// Once an hour, and first after `tally::FIRST`: what was sent and
/// received, by kind, and the dials made (see `tally`), in the log.
async fn tell_tally(ctx: Arc<Ctx>) {
    let mut wait = crate::tally::FIRST;
    loop {
        tokio::time::sleep(wait).await;
        if ctx.stopped.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        for line in crate::tally::report() {
            log!(ctx, "the last {} min, {line}", wait.as_secs() / 60);
        }
        wait = crate::tally::EVERY;
    }
}

/// Every half minute: deletes messages whose time is up (see
/// `disappearing`), from every conversation and from the outbox; and
/// drops gone lines every one of the user's devices has (`settle`).
async fn sweep_expired(ctx: Arc<Ctx>) {
    let mut every = tokio::time::interval(Duration::from_secs(30));
    loop {
        every.tick().await;
        if ctx.stopped.load(Ordering::Relaxed) {
            return;
        }
        ownpeers::tidy(&ctx);
        ownrequests::tidy(&ctx);
        let now = unix_now();
        let _ = crate::withdrawals::sweep(&ctx.paths.withdrawals(), now);
        let _ = crate::waiting::sweep(&ctx.paths.waiting(), now);
        let _ = crate::unsaying::sweep(&ctx.paths.unsaying(), now);
        let _ = crate::posts::sweep_deletions(&ctx.paths.posts(), now);
        let gone = crate::history::sweep(&ctx.paths.history(), now).unwrap_or(0)
            + crate::outbox::sweep(&ctx.paths.outbox(), now).unwrap_or(0)
            + crate::channel::sweep(&ctx.paths.channels(), now).unwrap_or(0);
        if gone > 0 {
            log!(ctx, "deleted {gone} message(s) whose time was up");
        }
    }
}

/// Posts are append-only, so only what's newer than the newest one
/// already cached needs to travel — from that same second on, in case
/// more than one was posted in it (the merge drops repeats).
fn newest_cached(paths: &Paths, public_key: &[u8; 32]) -> u64 {
    crate::postcache::newest(&paths.postcache(), public_key, unix_now())
}

/// Turns a fetch's outcome into what the caller sees: on success the
/// verified new posts are merged into the cache, those the author's
/// deleted (verified too) taken out, and the whole cache comes back; on
/// failure the cache, if there's anything in it.
/// `wanted` is asked, as the posts are kept, whether they still are.
fn posts_result(
    paths: &Paths,
    name: &str,
    public_key: &[u8; 32],
    authors: &[[u8; 32]],
    fetched: Result<crate::posts::Fetched>,
    log: &dyn Fn(String),
    wanted: impl FnOnce() -> bool,
) -> Result<PostsResult> {
    let cache = paths.postcache();
    match fetched {
        Ok(crate::posts::Fetched { posts, deleted }) => {
            let deleted: Vec<crate::posts::Deletion> = deleted.into_iter().filter(|d| authors.iter().any(|a| crate::posts::verify_deletion(d, a))).collect();
            let verified: Vec<Post> = posts
                .into_iter()
                .filter(|post| !deleted.iter().any(|d| d.post_signature == post.signature))
                .filter(|post| {
                    let ok = authors.iter().any(|a| crate::posts::verify(post, a));
                    if !ok {
                        log(format!("dropped a post claiming to be from {name} that failed signature verification"));
                    }
                    ok
                })
                .collect();
            if let Ok(false) = crate::postcache::merge_if(&cache, public_key, &verified, wanted) {
                anyhow::bail!("not following them any more");
            }
            let _ = crate::postcache::drop_deleted(&cache, public_key, &deleted, authors);
            Ok(PostsResult::Live(crate::postcache::cached(&cache, public_key).unwrap_or(verified)))
        }
        Err(e) => match crate::postcache::cached(&cache, public_key) {
            Ok(posts) if !posts.is_empty() => Ok(PostsResult::Cached { posts, reason: e.to_string() }),
            _ => Err(e),
        },
    }
}

/// The X25519 form of a peer's root key, for matching an accepted
/// connection to its peer — `None` for a key that isn't a valid Ed25519
/// identity.
fn x25519_of(root_key: &[u8; 32]) -> Option<[u8; 32]> {
    identity::ed25519_public_key_to_x25519(root_key).ok()
}

pub use crate::unix_now;

/// A cover rate as typed: a plain number of bits/second, or with a
/// `k`/`m` suffix (`64k`, `64kbit`, `1m`, `2Mb/s`), up to the most cover
/// there is.
pub fn parse_bitrate(input: &str) -> Result<u64, String> {
    parse_bitrate_up_to(input, protocol::MAX_COVER_BITS_PER_SECOND)
}

/// The most a bandwidth limit can be set to (above it, there's no limit to speak of).
pub const MOST_BANDWIDTH_LIMIT: u64 = 10_000_000_000;

/// A bandwidth limit as typed (see `transport::set_bandwidth_limit`):
/// as `parse_bitrate`, from `transport::LEAST_BANDWIDTH_LIMIT`.
pub fn parse_bandwidth_limit(input: &str) -> Result<u64, String> {
    let limit = parse_bitrate_up_to(input, MOST_BANDWIDTH_LIMIT)?;
    if limit < crate::transport::LEAST_BANDWIDTH_LIMIT {
        return Err(format!("a limit has to be at least {}: below that, not even a call's voice fits", format_bitrate(crate::transport::LEAST_BANDWIDTH_LIMIT)));
    }
    Ok(limit)
}

fn parse_bitrate_up_to(input: &str, most_bits_per_second: u64) -> Result<u64, String> {
    let lower = input.trim().to_ascii_lowercase().replace(' ', "");
    let lower = lower.strip_suffix("/s").map_or(lower.clone(), |l| format!("{l}ps"));
    let (digits, multiplier) = if let Some(rest) = lower.strip_suffix('k').or_else(|| lower.strip_suffix("kbit")).or_else(|| lower.strip_suffix("kbps")).or_else(|| lower.strip_suffix("kbitps")) {
        (rest, 1_000)
    } else if let Some(rest) = lower.strip_suffix('m').or_else(|| lower.strip_suffix("mbit")).or_else(|| lower.strip_suffix("mbps")).or_else(|| lower.strip_suffix("mbitps")) {
        (rest, 1_000_000)
    } else if let Some(rest) = lower.strip_suffix('g').or_else(|| lower.strip_suffix("gbit")).or_else(|| lower.strip_suffix("gbps")).or_else(|| lower.strip_suffix("gbitps")) {
        (rest, 1_000_000_000)
    } else {
        (lower.strip_suffix("bitps").or_else(|| lower.strip_suffix("bit")).or_else(|| lower.strip_suffix("bps")).unwrap_or(&lower), 1)
    };
    let value: u64 = digits.trim().parse().map_err(|_| format!("'{input}' isn't a bitrate — try a plain number, or e.g. 64k or 2m"))?;
    let most = most_bits_per_second;
    match value.checked_mul(multiplier) {
        Some(0) => Err("a rate of 0 sends nothing — use off for no cover".to_string()),
        Some(rate) if rate <= most => Ok(rate),
        _ => Err(format!("'{input}' is more than the most there is, {}", format_bitrate(most))),
    }
}

/// The inverse of `parse_bitrate`, for display — `64000` back to
/// `"64k"`, `1_000_000` back to `"1m"`, anything else as a plain number.
pub fn format_bitrate(bits_per_second: u64) -> String {
    if bits_per_second != 0 && bits_per_second.is_multiple_of(1_000_000) {
        format!("{}m", bits_per_second / 1_000_000)
    } else if bits_per_second != 0 && bits_per_second.is_multiple_of(1_000) {
        format!("{}k", bits_per_second / 1_000)
    } else {
        bits_per_second.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bitrate_is_read_as_typed_within_what_there_is() {
        assert_eq!(parse_bitrate("64k"), Ok(64_000));
        assert_eq!(parse_bitrate("64kbit"), Ok(64_000));
        assert_eq!(parse_bitrate("2m"), Ok(2_000_000));
        assert_eq!(parse_bitrate("9600"), Ok(9600));
        assert!(parse_bitrate("0").is_err(), "nothing at all: that's off");
        assert!(parse_bitrate("10g").is_err());
        assert!(parse_bitrate("999999m").is_err(), "more than the most there is");
        assert!(parse_bitrate("lots").is_err());
        assert_eq!(parse_bitrate("2Mb/s"), Ok(2_000_000));
        assert_eq!(parse_bandwidth_limit("2 Mbit/s"), Ok(2_000_000));
        assert_eq!(parse_bandwidth_limit("1g"), Ok(1_000_000_000));
        assert!(parse_bandwidth_limit("10k").is_err(), "too little for anything");
    }
}
