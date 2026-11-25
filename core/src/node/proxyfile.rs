//! Files through another of the user's devices (see `proxy`).
//! The device that's let relay and connected to the peer passes a
//! transfer's notices and chunks on between its connection to them and
//! its link to the user's device that has the file, or is taking it. The
//! file is never kept on the way: each chunk goes on as it's asked for,
//! and its answer as it comes.
//!
//! - **A file in** (`after_contact`): the peer offers this device a file,
//!   and it's let relay. It's offered here, and passed on to each
//!   of the user's other devices it's linked to; one connected to the peer
//!   itself takes no notice (they offer it the file directly). The first
//!   to take it has it: it's off the list here, the others' copies are
//!   withdrawn, and the chunks it asks for over the link
//!   (`Request::FileChunkThrough`) are asked for of the peer.
//! - **A file out** (`LinkFiles::look`): a device not connected to the
//!   peer, which one that's let relay reaches, offers it through that one,
//!   which offers it to them as the user's and asks the device for each
//!   chunk they ask for.
//!
//! Which transfers go through this device is kept in memory
//! (`Ctx::through_files`): the device at the other end says again what it
//! has going through it every `RESAY`, so after a restart, or a new link,
//! they carry on.

use super::files::FileWork;
use super::peer::PeerRequest;
use super::Ctx;
use crate::lock::Locked;
use crate::protocol::{self, Notice, Reply, Request, RequestId};
use crate::transfer::{self, Direction, State, TransferId};
use crate::transport::AuthenticatedConnection;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// How often a link looks for transfers to go through it.
const LOOK: Duration = Duration::from_secs(2);
/// How often what's going through a link is said again.
const RESAY: Duration = Duration::from_secs(20);

/// A transfer this device passes on: over the link to which of the
/// user's devices, with which peer (the key it goes by here, and the one
/// it goes by on that device: a device of theirs, or them as a user),
/// and whether the file's going out to them.
#[derive(Clone, Copy, Debug)]
pub(super) struct Passing {
    link: [u8; 32],
    here: [u8; 32],
    there: [u8; 32],
    out: bool,
}

/// What goes on a link about a transfer through it: one of its notices
/// (`onward`, for the device at the other end to pass on to the peer;
/// otherwise from them), or a chunk to ask it for, and where its answer
/// goes.
pub(super) enum LinkFile {
    Notice { contact: [u8; 32], onward: bool, notice: Box<Notice> },
    Chunk { contact: [u8; 32], id: TransferId, offset: u64, len: u32, reply: oneshot::Sender<Reply> },
}

fn id_of(notice: &Notice) -> Option<TransferId> {
    match notice {
        Notice::FileOffer { id, .. } | Notice::FileAnswer { id, .. } | Notice::FileCancel { id, .. } | Notice::FileDone { id } => Some(*id),
        _ => None,
    }
}

/// Whether nothing more goes through for a transfer after `notice`.
fn finishes(notice: &Notice) -> bool {
    matches!(notice, Notice::FileAnswer { accepted: false, .. } | Notice::FileCancel { .. } | Notice::FileDone { .. })
}

fn to_link(ctx: &Ctx, link: [u8; 32], file: LinkFile) -> bool {
    ctx.link_files.locked().get(&link).is_some_and(|tx| tx.send(file).is_ok())
}

/// The user's devices this one's linked to now.
fn links(ctx: &Ctx) -> Vec<[u8; 32]> {
    let siblings = ctx.siblings.locked();
    let mut links: Vec<[u8; 32]> = ctx.link_files.locked().keys().filter(|d| siblings.contains_key(*d)).copied().collect();
    links.sort();
    links
}

/// A peer entry here, as transfers need it.
struct Entry {
    name: String,
    /// Their user key.
    user: [u8; 32],
    /// The key transfers with this entry go by here.
    key: [u8; 32],
    online: bool,
    requests: mpsc::Sender<PeerRequest>,
}

/// The peer entries here for `key`, as another device names the peer to
/// it: the entry transfers go by that key with first, if there is one;
/// then the user's entries, by name (one device may know a peer by their
/// devices, another by them as a user).
fn entries(ctx: &Ctx, key: &[u8; 32]) -> Vec<Entry> {
    let user = super::peerdevices::user_of(ctx, key).unwrap_or(*key);
    let peers = ctx.peers.locked();
    let mut all: Vec<(u8, &String, Entry)> = peers
        .iter()
        .filter_map(|(name, e)| {
            let pk = e.watch.borrow().public_key;
            let own = e.device.unwrap_or(pk);
            let rank = if own == *key {
                0
            } else if pk == user {
                1
            } else {
                return None;
            };
            Some((rank, name, Entry { name: name.clone(), user: pk, key: own, online: e.online.load(Ordering::Relaxed), requests: e.requests.clone() }))
        })
        .collect();
    all.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    all.into_iter().map(|(_, _, e)| e).collect()
}

fn entry(ctx: &Ctx, key: &[u8; 32]) -> Option<Entry> {
    entries(ctx, key).into_iter().next()
}

/// Peer `key`'s user key, which the user's other devices all know them
/// by (one may know only them, not their devices): how a transfer's peer
/// is named on a link.
fn as_user(ctx: &Ctx, key: &[u8; 32]) -> [u8; 32] {
    entry(ctx, key).map_or(*key, |e| e.user)
}

/// The entry here for peer `key` (as another device names them) that's
/// connected directly now, on a connection of its own.
fn connected(ctx: &Ctx, key: &[u8; 32]) -> Option<Entry> {
    entries(ctx, key).into_iter().find(|e| e.online && super::proxy::direct(ctx, &e.user))
}

/// A transfer notice from peer `contact` (the key transfers with them go
/// by) about one this device passes on: passed on, and true.
pub(super) fn from_contact(ctx: &Ctx, contact: [u8; 32], notice: &Notice) -> bool {
    let Some(id) = id_of(notice) else { return false };
    let passing = {
        let mut through = ctx.through_files.locked();
        let passing = through.get(&id).copied().filter(|p| p.here == contact);
        if passing.is_some() && finishes(notice) {
            through.remove(&id);
        }
        passing
    };
    let Some(passing) = passing else { return false };
    to_link(ctx, passing.link, LinkFile::Notice { contact: passing.there, onward: false, notice: Box::new(notice.clone()) });
    true
}

/// Once peer `contact`'s notice has been dealt with here, on a device
/// that's let relay: an offer waiting for an answer is passed on to the
/// user's other devices, and one they've cancelled is withdrawn from them.
pub(super) fn after_contact(ctx: &Ctx, contact: [u8; 32], notice: &Notice) {
    if !matches!(notice, Notice::FileOffer { .. } | Notice::FileCancel { .. }) || !super::devices::relaying_here(ctx) {
        return;
    }
    let Some(id) = id_of(notice) else { return };
    let Some(t) = transfer::get(&ctx.paths.transfers(), &id).filter(|t| t.direction == Direction::In && t.peer == contact) else { return };
    let passed = match (notice, t.state) {
        (Notice::FileOffer { .. }, State::Offered) => notice.clone(),
        (Notice::FileCancel { .. }, State::Cancelled) => notice.clone(),
        _ => return,
    };
    let user = as_user(ctx, &contact);
    for link in links(ctx) {
        to_link(ctx, link, LinkFile::Notice { contact: user, onward: false, notice: Box::new(passed.clone()) });
    }
}

/// Offer `t`, made to this device directly, was answered or given up on
/// here (`why`): the copies passed on to the user's other devices go.
pub(super) fn withdraw(ctx: &Ctx, t: &transfer::Transfer, why: &str) {
    if t.direction != Direction::In || !super::devices::relaying_here(ctx) || connected(ctx, &t.peer).is_none() {
        return;
    }
    let user = as_user(ctx, &t.peer);
    for link in links(ctx) {
        let notice = Notice::FileCancel { id: t.id, reason: why.to_string() };
        to_link(ctx, link, LinkFile::Notice { contact: user, onward: false, notice: Box::new(notice) });
    }
}

/// Something changed here in a transfer with `key`, who this device
/// isn't connected to: `notice` goes through the device that reaches
/// them, if one does.
pub(super) fn poke(ctx: &Ctx, key: &[u8; 32], notice: Notice) {
    let Some(e) = entry(ctx, key) else { return };
    if e.online || super::proxy::online_here(ctx, &e.user) {
        return;
    }
    if let Some(device) = super::proxy::through(ctx, &e.user) {
        to_link(ctx, device, LinkFile::Notice { contact: *key, onward: true, notice: Box::new(notice) });
    }
}

/// How a chunk peer `contact` asks for is answered.
pub(super) enum Serve {
    /// From what's here.
    Here,
    /// By the device the file's on, through this one: its answer, when
    /// it comes.
    Asked(oneshot::Receiver<Reply>),
    /// Not at all for now: it's going through this device, but the device
    /// with the file isn't linked just now, or hasn't said so since this
    /// one started. They ask again when it stalls.
    Unanswered,
}

/// Peer `contact` asks for a chunk of transfer `id` (`known_here`: one
/// this device has a record of).
pub(super) fn serve(ctx: &Ctx, contact: [u8; 32], id: TransferId, offset: u64, len: u32, known_here: bool) -> Serve {
    let passing = ctx.through_files.locked().get(&id).copied();
    match passing {
        Some(p) if p.here == contact && p.out => {
            let (reply, answer) = oneshot::channel();
            if to_link(ctx, p.link, LinkFile::Chunk { contact: p.there, id, offset, len, reply }) {
                Serve::Asked(answer)
            } else {
                Serve::Unanswered
            }
        }
        None if !known_here && super::devices::relaying_here(ctx) => Serve::Unanswered,
        _ => Serve::Here,
    }
}

/// The transfers through one link to another of the user's devices,
/// kept by that link's task (`devices::run`).
pub(super) struct LinkFiles {
    ctx: Arc<Ctx>,
    remote: [u8; 32],
    tx: mpsc::UnboundedSender<LinkFile>,
    rx: mpsc::UnboundedReceiver<LinkFile>,
    /// Answers from a peer to chunks asked for over the link, to go back.
    answers_tx: mpsc::UnboundedSender<(RequestId, Option<Reply>)>,
    answers_rx: mpsc::UnboundedReceiver<(RequestId, Option<Reply>)>,
    /// Chunks a peer asked for, asked for of the device with the file.
    asked: HashMap<RequestId, oneshot::Sender<Reply>>,
    /// Chunks of a file from a peer asked for through it: whose, which,
    /// where, and when.
    pulls: HashMap<RequestId, ([u8; 32], TransferId, u64, Instant)>,
    /// This device's transfers with peers reached through it, by the key
    /// each goes by.
    works: HashMap<[u8; 32], FileWork>,
    /// When each transfer through it was last said.
    said: HashMap<TransferId, Instant>,
    /// Transfers whose chunks couldn't be passed on: not asked for again
    /// until the next look.
    held: HashSet<TransferId>,
    look: tokio::time::Interval,
}

/// What wakes a link for the transfers through it.
pub(super) enum Wake {
    Send(LinkFile),
    Answer(RequestId, Option<Reply>),
    Look,
}

impl LinkFiles {
    pub fn new(ctx: &Arc<Ctx>, remote: [u8; 32]) -> LinkFiles {
        let (tx, rx) = mpsc::unbounded_channel();
        let (answers_tx, answers_rx) = mpsc::unbounded_channel();
        ctx.link_files.locked().insert(remote, tx.clone());
        LinkFiles {
            ctx: ctx.clone(),
            remote,
            tx,
            rx,
            answers_tx,
            answers_rx,
            asked: HashMap::new(),
            pulls: HashMap::new(),
            works: HashMap::new(),
            said: HashMap::new(),
            held: HashSet::new(),
            look: tokio::time::interval(LOOK),
        }
    }

    /// Cancel-safe.
    pub async fn next(&mut self) -> Wake {
        tokio::select! {
            Some(file) = self.rx.recv() => Wake::Send(file),
            Some((rid, reply)) = self.answers_rx.recv() => Wake::Answer(rid, reply),
            _ = self.look.tick() => Wake::Look,
        }
    }

    pub async fn on_wake(&mut self, conn: &mut AuthenticatedConnection, wake: Wake) -> Result<()> {
        match wake {
            Wake::Send(LinkFile::Notice { contact, onward, notice }) => {
                let pull = onward && matches!(*notice, Notice::FileAnswer { accepted: true, .. });
                protocol::send_notice(conn, Notice::FileThrough { contact, onward, notice }).await?;
                if pull {
                    self.works.entry(contact).or_insert_with(|| work(&self.ctx, contact)).forget();
                    self.pump(conn, contact).await?;
                }
                Ok(())
            }
            Wake::Send(LinkFile::Chunk { contact, id, offset, len, reply }) => {
                let rid = protocol::request(conn, Request::FileChunkThrough { contact, id, offset, len }).await?;
                self.asked.insert(rid, reply);
                Ok(())
            }
            Wake::Answer(rid, reply) => protocol::reply(conn, rid, reply.unwrap_or_else(|| Reply::Refused("not connected to them just now".to_string()))).await,
            Wake::Look => self.on_look(conn).await,
        }
    }

    /// What's going through it, said again now and then, and more of what's
    /// coming through it asked for.
    async fn on_look(&mut self, conn: &mut AuthenticatedConnection) -> Result<()> {
        self.held.clear();
        for work in self.works.values_mut() {
            for id in work.reset_stalled() {
                self.pulls.retain(|_, p| p.1 != id);
            }
        }
        self.said.retain(|_, at| at.elapsed() < RESAY);
        let mut contacts = HashSet::new();
        for t in transfer::load(&self.ctx.paths.transfers()) {
            let notice = match (t.direction, t.state) {
                (Direction::Out, State::Offered | State::Active) => Notice::FileOffer { id: t.id, name: t.name.clone(), size: t.size, sha256: t.sha256 },
                (Direction::In, State::Active) => Notice::FileAnswer { id: t.id, accepted: true },
                _ => continue,
            };
            if !self.goes_through(&t.peer, t.direction == Direction::Out) {
                continue;
            }
            if t.direction == Direction::In {
                contacts.insert(t.peer);
            }
            if self.said.contains_key(&t.id) {
                continue;
            }
            self.said.insert(t.id, Instant::now());
            protocol::send_notice(conn, Notice::FileThrough { contact: t.peer, onward: true, notice: Box::new(notice) }).await?;
        }
        for contact in contacts {
            self.pump(conn, contact).await?;
        }
        Ok(())
    }

    /// Whether this device's transfers with `key` go through this link:
    /// the device at the other end is the one that reaches them, and this
    /// one isn't connected to them itself (`out`: on any of its entries
    /// for them; otherwise, one coming in: on the one it goes by, which
    /// would take it from here).
    fn goes_through(&self, key: &[u8; 32], out: bool) -> bool {
        let all = entries(&self.ctx, key);
        let Some(user) = all.first().map(|e| e.user) else { return false };
        let here = if out { super::proxy::online_here(&self.ctx, &user) } else { all.iter().any(|e| e.key == *key && e.online) };
        !here && super::proxy::through(&self.ctx, &user) == Some(self.remote)
    }

    /// Asks for more of what's coming from `contact` through it.
    async fn pump(&mut self, conn: &mut AuthenticatedConnection, contact: [u8; 32]) -> Result<()> {
        if !self.goes_through(&contact, false) {
            return Ok(());
        }
        let ctx = &self.ctx;
        let work = self.works.entry(contact).or_insert_with(|| work(ctx, contact));
        for (id, offset, len) in work.wants(false) {
            if self.held.contains(&id) {
                work.restart(&id);
                continue;
            }
            let rid = protocol::request(conn, Request::FileChunkThrough { contact, id, offset, len }).await?;
            self.pulls.insert(rid, (contact, id, offset, Instant::now()));
        }
        Ok(())
    }

    /// Whether reply `rid` is for a chunk asked for over it.
    pub fn owns(&self, rid: RequestId) -> bool {
        self.asked.contains_key(&rid) || self.pulls.contains_key(&rid)
    }

    pub async fn on_reply(&mut self, conn: &mut AuthenticatedConnection, rid: RequestId, reply: Reply) -> Result<()> {
        if let Some(answer) = self.asked.remove(&rid) {
            let _ = answer.send(reply);
            return Ok(());
        }
        let Some((contact, id, offset, sent)) = self.pulls.remove(&rid) else { return Ok(()) };
        let Some(work) = self.works.get_mut(&contact) else { return Ok(()) };
        let data = match reply {
            Reply::FileData(data) => Ok(data),
            Reply::FileError(why) => Err(why),
            _ => {
                work.restart(&id);
                self.pulls.retain(|_, p| p.1 != id);
                self.held.insert(id);
                return Ok(());
            }
        };
        if let Some(notice) = work.on_data(id, offset, data, sent.elapsed()) {
            protocol::send_notice(conn, Notice::FileThrough { contact, onward: true, notice: Box::new(notice) }).await?;
        }
        self.pump(conn, contact).await
    }

    /// It asks for a chunk of transfer `id` with peer `contact`: of them,
    /// if it's taking a file from them through this device; otherwise
    /// from this device's file, which it's passing on to them.
    pub async fn on_chunk(&mut self, conn: &mut AuthenticatedConnection, rid: RequestId, contact: [u8; 32], id: TransferId, offset: u64, len: u32) -> Result<()> {
        let passing = self.ctx.through_files.locked().get(&id).copied();
        if let Some(p) = passing {
            if p.link != self.remote || p.there != contact || p.out {
                return protocol::reply(conn, rid, Reply::Refused("not one passed on for you".to_string())).await;
            }
            let (reply, answer) = oneshot::channel();
            let asked = connected(&self.ctx, &p.here).is_some_and(|e| e.key == p.here && e.requests.try_send(PeerRequest::Chunk { id, offset, len, reply }).is_ok());
            if !asked {
                return protocol::reply(conn, rid, Reply::Refused("not connected to them just now".to_string())).await;
            }
            let answers = self.answers_tx.clone();
            tokio::spawn(async move {
                let _ = answers.send((rid, answer.await.ok()));
            });
            return Ok(());
        }
        let ctx = self.ctx.clone();
        let reply = self.works.entry(contact).or_insert_with(|| work(&ctx, contact)).serve(&id, offset, len);
        protocol::reply(conn, rid, reply).await
    }

    /// One of a transfer's notices, about peer `contact`: for this device
    /// to pass on to them (`onward`), or from them, through it.
    pub async fn on_notice(&mut self, conn: &mut AuthenticatedConnection, contact: [u8; 32], onward: bool, notice: Notice) -> Result<()> {
        if onward {
            return self.pass_on(conn, contact, notice).await;
        }
        let Some(e) = entry(&self.ctx, &contact) else { return Ok(()) };
        if matches!(notice, Notice::FileOffer { .. }) && super::proxy::online_here(&self.ctx, &e.user) {
            return Ok(());
        }
        let ctx = self.ctx.clone();
        let back = self.works.entry(contact).or_insert_with(|| work(&ctx, contact)).on_notice(notice)?;
        for notice in back {
            protocol::send_notice(conn, Notice::FileThrough { contact, onward: true, notice: Box::new(notice) }).await?;
        }
        self.pump(conn, contact).await
    }

    /// A notice it has for peer `contact`, for this device to pass on.
    async fn pass_on(&mut self, conn: &mut AuthenticatedConnection, contact: [u8; 32], notice: Notice) -> Result<()> {
        let ctx = self.ctx.clone();
        if !super::devices::relaying_here(&ctx) {
            return Ok(());
        }
        let Some(id) = id_of(&notice) else { return Ok(()) };
        let passing = ctx.through_files.locked().get(&id).copied();
        let on = |key: &[u8; 32]| entries(&ctx, key).into_iter().find(|e| e.key == *key && e.online);
        let found = match passing {
            Some(p) => on(&p.here),
            None => match transfer::get(&ctx.paths.transfers(), &id) {
                Some(t) if t.direction == Direction::In && as_user(&ctx, &t.peer) == as_user(&ctx, &contact) => on(&t.peer),
                _ => connected(&ctx, &contact),
            },
        };
        let Some(Entry { key: here, requests, .. }) = found else { return Ok(()) };
        let store = ctx.paths.transfers();
        match passing {
            Some(p) if p.link == self.remote && p.there == contact => {
                if finishes(&notice) {
                    ctx.through_files.locked().remove(&id);
                }
                let _ = requests.try_send(PeerRequest::Notice(notice));
            }
            Some(p) if !p.out => {
                if matches!(notice, Notice::FileAnswer { accepted: true, .. }) {
                    let notice = Notice::FileCancel { id, reason: transfer::TAKEN_ELSEWHERE.to_string() };
                    protocol::send_notice(conn, Notice::FileThrough { contact, onward: false, notice: Box::new(notice) }).await?;
                }
            }
            Some(_) => {}
            None => match transfer::get(&store, &id) {
                None => {
                    if matches!(notice, Notice::FileOffer { .. }) {
                        ctx.through_files.locked().insert(id, Passing { link: self.remote, here, there: contact, out: true });
                        let _ = requests.try_send(PeerRequest::Notice(notice));
                    }
                }
                Some(t) if t.direction == Direction::In && t.peer == here => {
                    if t.state != State::Offered {
                        if matches!(notice, Notice::FileAnswer { accepted: true, .. }) {
                            let notice = Notice::FileCancel { id, reason: transfer::TAKEN_ELSEWHERE.to_string() };
                            protocol::send_notice(conn, Notice::FileThrough { contact, onward: false, notice: Box::new(notice) }).await?;
                        }
                        return Ok(());
                    }
                    let who = super::devices::label(&ctx, &self.remote);
                    let why = match &notice {
                        Notice::FileAnswer { accepted: true, .. } | Notice::FileDone { .. } => {
                            if !finishes(&notice) {
                                ctx.through_files.locked().insert(id, Passing { link: self.remote, here, there: contact, out: false });
                            }
                            let _ = transfer::remove(&store, &id);
                            log!(ctx, "a file ({} bytes): taken on your device {who}", t.size);
                            transfer::TAKEN_ELSEWHERE
                        }
                        Notice::FileAnswer { accepted: false, .. } => {
                            let _ = transfer::update(&store, &id, |t| t.state = State::Declined);
                            "declined on another of your devices"
                        }
                        Notice::FileCancel { .. } => {
                            let _ = transfer::update(&store, &id, |t| {
                                t.state = State::Cancelled;
                                t.note = format!("cancelled on your device {who}");
                            });
                            "cancelled on another of your devices"
                        }
                        _ => return Ok(()),
                    };
                    ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
                    let _ = requests.try_send(PeerRequest::Notice(notice));
                    for link in links(&ctx).into_iter().filter(|l| *l != self.remote) {
                        let notice = Notice::FileCancel { id, reason: why.to_string() };
                        to_link(&ctx, link, LinkFile::Notice { contact, onward: false, notice: Box::new(notice) });
                    }
                }
                Some(_) => {}
            },
        }
        Ok(())
    }

    /// The link's gone: chunks asked for over it won't be answered, and
    /// what was coming through it is asked for again over the next.
    pub fn gone(&self) {
        let mut links = self.ctx.link_files.locked();
        if links.get(&self.remote).is_some_and(|tx| tx.same_channel(&self.tx)) {
            links.remove(&self.remote);
        }
    }
}

/// This device's transfers with `key`, as a `FileWork`.
fn work(ctx: &Arc<Ctx>, key: [u8; 32]) -> FileWork {
    let (name, user) = entry(ctx, &key).map(|e| (super::peerdevices::user_name(&ctx.shown(&e.name)).to_string(), e.user)).unwrap_or_default();
    FileWork::new(ctx.clone(), key, user, name)
}
