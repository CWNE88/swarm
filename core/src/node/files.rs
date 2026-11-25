//! A peer task's part in file transfers (see `crate::transfer` for how a
//! transfer goes): offering ours, answering theirs as the person decided,
//! pulling what we've accepted a few chunks ahead, and serving chunks of
//! what we offered. One `FileWork` per connection; everything that has to
//! outlast a connection is in the transfers store, so a new connection
//! carries on where the last one stopped.

use crate::lock::Locked;
use super::{unix_now, Ctx};
use crate::protocol::{self, Notice, Reply, Request, RequestId};
use crate::transfer::{self, Direction, State, TransferId};
use crate::transport::AuthenticatedConnection;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A chunk asked for that hasn't come back in this long is asked for
/// again (with the rest of what was outstanding for that transfer).
pub(super) const CHUNK_STALL: Duration = Duration::from_secs(60);

/// How much longer than the quickest a chunk may take to come back
/// before the window stops growing: more than this, and chunks are
/// queueing — on a connection without cover, ahead of messages and
/// everything else (they share one stream).
const QUEUE_TARGET: Duration = Duration::from_millis(250);

/// Every chunk this long coming back slowly, with the window at its
/// least: the path's longer now (a roam, say), and the quickest of them
/// is taken as the quickest from then on. At its least, a transfer
/// queues next to nothing itself, so what they took is the path's, not
/// its own queue's.
const QUICKEST_STALE: Duration = Duration::from_secs(30);

/// How long the transfers store, as read, is taken as it stands. Every
/// change the person makes pokes the peer task (`FileWork::forget`), and
/// this connection's own forget it too, so this only bounds a poke lost.
const STORE_FRESH: Duration = Duration::from_secs(1);

/// An incoming transfer being pulled on this connection.
struct Pull {
    /// The next offset to ask for.
    next: u64,
    /// Chunks asked for and not yet answered.
    asked: usize,
    last_progress: Instant,
    /// How many chunks may be asked for at once (see `window_after`).
    window: usize,
    /// The quickest a chunk has come back on this connection.
    quickest: Option<Duration>,
    /// When the window was last cut (see `window_after`).
    cut_at: Option<Instant>,
    /// Since when every chunk's come back slowly with the window at its
    /// least, and the quickest of them (see `QUICKEST_STALE`).
    slow_since: Option<(Instant, Duration)>,
    /// What's been written hashed as it was, and how far: so finishing
    /// needn't read it all back (see `hashed_after`). None if it wasn't
    /// all seen here (it resumed from what's on disk).
    hashed: Option<(Sha256, u64)>,
    /// Its part file, open from the first chunk to the last (see
    /// `transfer::PartFile`); closed with the `Pull`.
    part: Option<transfer::PartFile>,
}

/// Since when every chunk's come back slowly with the window at its
/// least (`slow`: this one did), and the quickest of them; and, once
/// that's been `QUICKEST_STALE`, the quickest a chunk takes from now on
/// (the path's longer than it was).
fn slow_at_least(slow_since: Option<(Instant, Duration)>, slow: bool, took: Duration, now: Instant) -> (Option<(Instant, Duration)>, Option<Duration>) {
    match slow_since {
        _ if !slow => (None, None),
        Some((since, least)) if now.duration_since(since) >= QUICKEST_STALE => (None, Some(least.min(took))),
        Some((since, least)) => (Some((since, least.min(took))), None),
        None => (Some((now, took)), None),
    }
}

/// `hashed` once `data`, at `offset`, has gone to the part file, which
/// then has `have` bytes: hashed in, if it carries straight on from what
/// was; as it was, for a repeat (`transfer::append` ignores one); given
/// up on otherwise.
fn hashed_after(hashed: Option<(Sha256, u64)>, offset: u64, data: &[u8], have: u64) -> Option<(Sha256, u64)> {
    match hashed? {
        (mut hasher, at) if at == offset && have == offset + data.len() as u64 => {
            hasher.update(data);
            Some((hasher, have))
        }
        (hasher, at) if at == have => Some((hasher, at)),
        _ => None,
    }
}

/// The SHA-256 of all `size` bytes, if `hashed` has had them.
fn digest_of(hashed: Option<(Sha256, u64)>, size: u64) -> Option<[u8; 32]> {
    hashed.filter(|(_, at)| *at == size).map(|(hasher, _)| hasher.finalize().into())
}

/// The window once a chunk has come back in `took`, and whether it was
/// cut: one more if it came back about as quickly as the quickest
/// (nothing's queueing); if not, a quarter fewer, but no more than once a
/// round trip (`took`, against the time `since_cut` the last cut) — so it
/// settles where chunks queue for about `QUEUE_TARGET`. A packet lost
/// holds up every chunk behind it on the one stream, and they all come
/// back slowly together: taken one fewer for each, the window would
/// fall far under what the link carries.
fn window_after(window: usize, quickest: Duration, took: Duration, since_cut: Option<Duration>) -> (usize, bool) {
    if took <= quickest + QUEUE_TARGET {
        ((window + 1).min(transfer::MAX_WINDOW), false)
    } else if since_cut.is_none_or(|since| since >= took) {
        ((window - window / 4).max(transfer::WINDOW), true)
    } else {
        (window, false)
    }
}

pub(super) struct FileWork {
    ctx: Arc<Ctx>,
    peer: [u8; 32],
    user: [u8; 32],
    name: String,
    pulling: HashMap<TransferId, Pull>,
    /// The transfers store as last read, and when — not read again for
    /// every chunk, which with a full store slowed a transfer sixfold.
    known: Option<(Instant, Vec<transfer::Transfer>)>,
    /// Under a receive limit: the bytes that may be asked for now, and when
    /// that was worked out (see `wants`).
    asking: (f64, Instant),
    /// The files being sent them, open while they're asked for (see
    /// `transfer::SourceFile`).
    sources: HashMap<TransferId, transfer::SourceFile>,
}

impl FileWork {
    /// `peer`: what this connection's transfers go by (the device's key,
    /// for one of a user's devices); `user`, whose settings they are.
    pub(super) fn new(ctx: Arc<Ctx>, peer: [u8; 32], user: [u8; 32], name: String) -> Self {
        FileWork { ctx, peer, user, name, pulling: HashMap::new(), known: None, asking: (0.0, Instant::now()), sources: HashMap::new() }
    }

    /// Every transfer on record, as last read (see `STORE_FRESH`). Read
    /// again, the files of any that's stopped (done, cancelled here or
    /// there, failed) are closed.
    fn known(&mut self) -> &[transfer::Transfer] {
        if self.known.as_ref().is_none_or(|(at, _)| at.elapsed() > STORE_FRESH) {
            let all = transfer::load(&self.store());
            let state = |id: &TransferId| all.iter().find(|t| t.id == *id).map(|t| t.state);
            self.pulling.retain(|id, _| state(id) == Some(State::Active));
            self.sources.retain(|id, _| matches!(state(id), Some(State::Offered | State::Active)));
            self.known = Some((Instant::now(), all));
        }
        &self.known.as_ref().expect("just read").1
    }

    /// Whether transfer `id` is on record here.
    pub(super) fn knows(&mut self, id: &TransferId) -> bool {
        self.known_one(id).is_some()
    }

    /// Transfer `id`, from the record as last read, or read again if it
    /// isn't there: one offered a moment after the last read (and
    /// accepted at once) is on record, and isn't refused.
    fn known_one(&mut self, id: &TransferId) -> Option<transfer::Transfer> {
        if let Some(t) = self.known().iter().find(|t| t.id == *id) {
            return Some(t.clone());
        }
        self.forget();
        self.known().iter().find(|t| t.id == *id).cloned()
    }

    /// The store's changed: read it again next time.
    pub(super) fn forget(&mut self) {
        self.known = None;
    }

    fn store(&self) -> std::path::PathBuf {
        self.ctx.paths.transfers()
    }

    /// Ours to this peer that are still open: offered again on every
    /// connection — an offer, or (once accepted) a "how's it going", which
    /// the other side answers with whatever's become of it.
    pub(super) fn open_offers(&mut self) -> Vec<Notice> {
        let peer = self.peer;
        self.known()
            .iter()
            .filter(|t| t.direction == Direction::Out && t.peer == peer && matches!(t.state, State::Offered | State::Active))
            .map(|t| Notice::FileOffer { id: t.id, name: t.name.clone(), size: t.size, sha256: t.sha256 })
            .collect()
    }

    /// Whether a transfer going by `key` is with this peer: one with them,
    /// or with another of their devices, or with them as a user (a file
    /// is for a user; one that went through another of their devices may
    /// carry on from this one, see `proxyfile`).
    fn theirs(&self, key: &[u8; 32]) -> bool {
        *key == self.peer || *key == self.user || super::peerdevices::user_of(&self.ctx, key) == Some(self.user)
    }

    /// The key this peer's transfers go by.
    pub(super) fn peer(&self) -> [u8; 32] {
        self.peer
    }

    /// The chunks to ask for next (transfer, offset, length) of whatever
    /// we're receiving from this peer, up to each transfer's window of
    /// chunks ahead (`transfer::WINDOW` while `in_call`, or while this
    /// device is in a call with anyone: `Ctx::in_a_call`); each counts as
    /// asked for from here on.
    pub(super) fn wants(&mut self, in_call: bool) -> Vec<(TransferId, u64, u32)> {
        let in_call = in_call || self.ctx.in_a_call();
        let peer = self.peer;
        let active: Vec<transfer::Transfer> = self.known().iter().filter(|t| t.direction == Direction::In && t.peer == peer && t.state == State::Active).cloned().collect();
        let mut wants = Vec::new();
        let limit = *self.ctx.receive_limit.borrow();
        let mut allowance = limit.map(|limit| {
            let per_second = limit as f64 / 8.0;
            let (had, at) = self.asking;
            (had + at.elapsed().as_secs_f64() * per_second).min(per_second)
        });
        self.asking.1 = Instant::now();
        for t in active {
            let pull = self.pulling.entry(t.id).or_insert_with(|| {
                let next = transfer::received(&t);
                Pull { next, asked: 0, last_progress: Instant::now(), window: transfer::WINDOW, quickest: None, cut_at: None, slow_since: None, hashed: (next == 0).then(|| (Sha256::new(), 0)), part: None }
            });
            let most = if in_call { transfer::WINDOW } else { pull.window };
            while pull.asked < most && pull.next < t.size {
                let len = (t.size - pull.next).min(transfer::CHUNK as u64) as u32;
                if let Some(allowance) = &mut allowance {
                    if *allowance < len as f64 {
                        break;
                    }
                    *allowance -= len as f64;
                }
                wants.push((t.id, pull.next, len));
                pull.next += len as u64;
                pull.asked += 1;
            }
        }
        self.asking.0 = allowance.unwrap_or(0.0);
        wants
    }

    /// Asks for more of whatever we're receiving from this peer (see
    /// `wants`). Each request goes into `outstanding` as
    /// `Awaiting::FileChunk`.
    pub(super) async fn pump(&mut self, conn: &mut AuthenticatedConnection, in_call: bool, mut ask: impl FnMut(RequestId, TransferId, u64)) -> Result<()> {
        for (id, offset, len) in self.wants(in_call) {
            let rid = protocol::request(conn, Request::FileChunk { id, offset, len }).await?;
            ask(rid, id, offset);
        }
        Ok(())
    }

    /// Transfer `id` is to be asked for again from what's on disk, its
    /// chunks asked for counting no more (they couldn't be passed on).
    pub(super) fn restart(&mut self, id: &TransferId) {
        self.pulling.remove(id);
    }

    /// Transfers whose chunks have stopped coming: forgotten on this
    /// connection, so the next `pump` asks again from what's on disk.
    /// Returns them, so their outstanding requests can be dropped.
    pub(super) fn reset_stalled(&mut self) -> Vec<TransferId> {
        let stalled: Vec<TransferId> =
            self.pulling.iter().filter(|(_, p)| p.asked > 0 && p.last_progress.elapsed() > CHUNK_STALL).map(|(id, _)| *id).collect();
        for id in &stalled {
            self.pulling.remove(id);
        }
        stalled
    }

    /// The folder this peer's files are saved to without asking, if any
    /// (set on the user, whichever of their devices it's from).
    fn receive_to(&self) -> Option<std::path::PathBuf> {
        let settings = self.ctx.paths.settings();
        crate::settings::receive_to(&settings, &self.user)
    }

    /// Accepts offer `id` into `folder` there and then, as the person set
    /// it up for this peer. A folder that can't be made leaves the offer
    /// to be answered by hand.
    fn take_unasked(&mut self, id: TransferId, folder: &std::path::Path) -> Result<Option<Notice>> {
        let folder = match std::fs::create_dir_all(folder).and_then(|()| folder.canonicalize()) {
            Ok(folder) => folder,
            Err(e) => {
                log!(self.ctx, "{} offers a file — couldn't use the folder set for it ({e}), so it waits for an answer", self.name);
                return Ok(None);
            }
        };
        let size = transfer::get(&self.store(), &id).map(|t| t.size).unwrap_or(0);
        if transfer::no_room(&self.store(), &folder, size).is_some() {
            log!(self.ctx, "{} offers a file — there isn't room for it, so it waits for an answer", self.name);
            return Ok(None);
        }
        let taken = transfer::update(&self.store(), &id, |t| {
            if t.state == State::Offered {
                t.state = State::Active;
                t.path = folder.clone();
            }
        })?;
        self.forget();
        if taken.is_some_and(|t| t.state == State::Active) {
            log!(self.ctx, "{} sent a file — saving it where set", self.name);
            return Ok(Some(Notice::FileAnswer { id, accepted: true }));
        }
        Ok(None)
    }

    /// One of this peer's file notices: what's to be sent back.
    pub(super) fn on_notice(&mut self, notice: Notice) -> Result<Vec<Notice>> {
        self.forget();
        let store = self.store();
        let mut back = Vec::new();
        match notice {
            Notice::FileOffer { id, name, size, sha256 } => match transfer::get(&store, &id) {
                Some(t) if t.direction != Direction::In || t.peer != self.peer => {}
                Some(t) => match t.state {
                    State::Offered | State::Active => {}
                    State::Done => back.push(Notice::FileDone { id }),
                    State::Declined => back.push(Notice::FileAnswer { id, accepted: false }),
                    State::Cancelled | State::Failed => back.push(Notice::FileCancel { id, reason: t.note.clone() }),
                },
                None => {
                    let waiting = transfer::load(&store).iter().filter(|t| t.direction == Direction::In && t.peer == self.peer && t.state == State::Offered).count();
                    if waiting >= transfer::MAX_PENDING_OFFERS_PER_PEER {
                        return Ok(back);
                    }
                    let name = transfer::sanitise_name(&name);
                    let t = transfer::Transfer {
                        id,
                        direction: Direction::In,
                        peer: self.peer,
                        state: State::Offered,
                        size,
                        sha256,
                        created: unix_now(),
                        name: name.clone(),
                        path: Default::default(),
                        note: String::new(),
                        cleared: false,
                        settled: false,
                    };
                    if transfer::add(&store, t)? {
                        match self.receive_to() {
                            Some(folder) => back.extend(self.take_unasked(id, &folder)?),
                            None => log!(self.ctx, "{} offers a file ({size} bytes)", self.name),
                        }
                    }
                }
            },
            Notice::FileAnswer { id, accepted } => {
                let changed = transfer::update(&store, &id, |t| {
                    if t.direction == Direction::Out && self.theirs(&t.peer) && t.state == State::Offered {
                        t.state = if accepted { State::Active } else { State::Declined };
                    }
                })?;
                back.extend(self.closed(changed.as_ref()));
                match changed {
                    Some(t) if t.state == State::Declined => log!(self.ctx, "{} declined a file ({} bytes)", self.name, t.size),
                    Some(t) if t.state == State::Active => super::NodeHandle { ctx: self.ctx.clone() }.withdraw_other_offers(&t),
                    _ => {}
                }
            }
            Notice::FileCancel { id, reason } => {
                let changed = transfer::update(&store, &id, |t| {
                    if self.theirs(&t.peer) && !t.state.is_finished() {
                        t.state = State::Cancelled;
                        t.note = format!("{} cancelled it{}", self.name, if reason.is_empty() { String::new() } else { format!(": {reason}") });
                        t.cleared = reason == transfer::TAKEN_ELSEWHERE;
                    }
                    if t.direction == Direction::In && t.peer == self.peer && t.state.is_finished() {
                        t.settled = true;
                    }
                })?;
                back.extend(self.closed(changed.as_ref()));
                if let Some(t) = changed.filter(|t| t.state == State::Cancelled) {
                    self.pulling.remove(&id);
                    self.sources.remove(&id);
                    transfer::discard_part(&t);
                    log!(self.ctx, "{}", t.note);
                }
            }
            Notice::FileDone { id } => {
                let changed = transfer::update(&store, &id, |t| {
                    if t.direction == Direction::Out && self.theirs(&t.peer) && !t.state.is_finished() {
                        t.state = State::Done;
                    }
                })?;
                back.extend(self.closed(changed.as_ref()));
                if let Some(t) = changed.filter(|t| t.state == State::Done && t.direction == Direction::Out) {
                    log!(self.ctx, "{} has the file ({} bytes)", self.name, t.size);
                }
            }
            Notice::FileClosed { id } => {
                transfer::update(&store, &id, |t| {
                    if t.direction == Direction::In && t.peer == self.peer && t.state.is_finished() {
                        t.settled = true;
                    }
                })?;
            }
            _ => {}
        }
        Ok(back)
    }

    /// Word to the receiver, after their last word on `t` (one this side
    /// sent them), that it's ended here too (see `Notice::FileClosed`).
    fn closed(&self, t: Option<&transfer::Transfer>) -> Option<Notice> {
        let t = t?;
        (t.direction == Direction::Out && self.theirs(&t.peer) && t.state.is_finished()).then_some(Notice::FileClosed { id: t.id })
    }

    /// Their request for a chunk of something we offered them.
    pub(super) fn serve(&mut self, id: &TransferId, offset: u64, len: u32) -> Reply {
        let store = self.store();
        let Some(t) = self.known_one(id).filter(|t| t.direction == Direction::Out && self.theirs(&t.peer)) else {
            return Reply::FileError("no such file offered".to_string());
        };
        let t = match t.state {
            State::Offered => {
                self.forget();
                transfer::update(&store, id, |t| t.state = State::Active).ok().flatten().unwrap_or(t)
            }
            State::Active => t,
            State::Done => t,
            _ => return Reply::FileError(if t.note.is_empty() { "cancelled".to_string() } else { t.note }),
        };
        let source = match self.sources.entry(*id) {
            std::collections::hash_map::Entry::Occupied(open) => Ok(open.into_mut()),
            std::collections::hash_map::Entry::Vacant(closed) => transfer::SourceFile::open(&t.path).map(|file| closed.insert(file)),
        };
        let source = match source.map(|source| (!source.changed(&t.path, t.size)).then_some(source)) {
            Ok(Some(source)) => source,
            _ => {
                self.sources.remove(id);
                self.forget();
                let _ = transfer::update(&store, id, |t| {
                    t.state = State::Failed;
                    t.note = "the file changed or went missing while it was being sent".to_string();
                });
                return Reply::FileError("the file changed or went missing while it was being sent".to_string());
            }
        };
        match source.read(offset, len as usize) {
            Ok(data) => {
                if !data.is_empty() {
                    let mut sent = self.ctx.transfer_sent.locked();
                    let seen = sent.entry(*id).or_insert(0);
                    *seen = (*seen).max(offset.saturating_add(data.len() as u64).min(t.size));
                }
                Reply::FileData(data)
            }
            Err(e) => Reply::FileError(format!("couldn't read the file: {e}")),
        }
    }

    /// A chunk (or why not) of something we're receiving, `took` after
    /// it was asked for: what's to be sent back.
    pub(super) fn on_data(&mut self, id: TransferId, offset: u64, data: Result<Vec<u8>, String>, took: Duration) -> Option<Notice> {
        let store = self.store();
        if let Some(pull) = self.pulling.get_mut(&id) {
            pull.asked = pull.asked.saturating_sub(1);
            pull.last_progress = Instant::now();
            let quickest = pull.quickest.map_or(took, |q| q.min(took));
            pull.quickest = Some(quickest);
            let (window, cut) = window_after(pull.window, quickest, took, pull.cut_at.map(|at| at.elapsed()));
            pull.window = window;
            if cut {
                pull.cut_at = Some(Instant::now());
            }
            let (slow_since, rebased) = slow_at_least(pull.slow_since, took > quickest + QUEUE_TARGET && window == transfer::WINDOW, took, Instant::now());
            pull.slow_since = slow_since;
            if rebased.is_some() {
                pull.quickest = rebased;
            }
        }
        let t = self.known_one(&id).filter(|t| t.direction == Direction::In && t.state == State::Active)?;
        let data = match data {
            Ok(data) => data,
            Err(reason) => {
                self.pulling.remove(&id);
                transfer::discard_part(&t);
                let note = format!("{} can't send it: {reason}", self.name);
                self.forget();
                let _ = transfer::update(&store, &id, |t| {
                    t.state = State::Failed;
                    t.note = note.clone();
                });
                log!(self.ctx, "a file ({} bytes): {note}", t.size);
                return None;
            }
        };
        let written = match self.pulling.get_mut(&id) {
            Some(pull) => match &mut pull.part {
                Some(part) => part.append(t.size, offset, &data),
                None => transfer::PartFile::open(&t).and_then(|part| pull.part.insert(part).append(t.size, offset, &data)),
            },
            None => transfer::append(&t, offset, &data),
        };
        let have = match written {
            Ok(have) => have,
            Err(e) => {
                self.pulling.remove(&id);
                transfer::discard_part(&t);
                let note = format!("couldn't save it: {e}");
                self.forget();
                let _ = transfer::update(&store, &id, |t| {
                    t.state = State::Failed;
                    t.note = note.clone();
                });
                log!(self.ctx, "a file ({} bytes): {note}", t.size);
                return Some(Notice::FileCancel { id, reason: note });
            }
        };
        if let Some(pull) = self.pulling.get_mut(&id) {
            pull.hashed = hashed_after(pull.hashed.take(), offset, &data, have);
        }
        if have < t.size {
            if let Some(pull) = self.pulling.get_mut(&id)
                && pull.asked == 0 && pull.next > have {
                    pull.next = have;
                }
            return None;
        }
        let arrived = digest_of(self.pulling.remove(&id).and_then(|pull| pull.hashed), t.size);
        match tokio::task::block_in_place(|| transfer::finish(&t, arrived)) {
            Ok(placed) => {
                self.forget();
                let _ = transfer::update(&store, &id, |t| {
                    t.state = State::Done;
                    t.path = placed.clone();
                });
                log!(self.ctx, "a file ({} bytes) from {} saved", t.size, self.name);
                Some(Notice::FileDone { id })
            }
            Err(e) => {
                let note = format!("{e:#}");
                self.forget();
                let _ = transfer::update(&store, &id, |t| {
                    t.state = State::Failed;
                    t.note = note.clone();
                });
                log!(self.ctx, "a file ({} bytes): {note}", t.size);
                Some(Notice::FileCancel { id, reason: note })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_grows_while_chunks_come_back_promptly_and_shrinks_when_they_queue() {
        let rtt = Duration::from_millis(200);
        let mut window = transfer::WINDOW;
        for _ in 0..10_000 {
            window = window_after(window, rtt, rtt + Duration::from_millis(20), None).0;
        }
        assert_eq!(window, transfer::MAX_WINDOW, "grows to the most");
        for _ in 0..1000 {
            window = window_after(window, rtt, rtt + Duration::from_secs(2), None).0;
        }
        assert_eq!(window, transfer::WINDOW, "back to the least, never below");
    }

    /// Moved to a path 400 ms longer: every chunk's slow, the window at
    /// its least; after `QUICKEST_STALE` of that, the quickest of them is
    /// the quickest, and the window can grow again. A prompt one between
    /// starts it over.
    #[test]
    fn a_longer_path_becomes_the_quickest_after_a_while_at_the_least() {
        let start = Instant::now();
        let mut state = None;
        for (i, took) in [600, 500, 550].into_iter().enumerate() {
            let (s, rebased) = slow_at_least(state, true, Duration::from_millis(took), start + Duration::from_secs(i as u64));
            assert_eq!(rebased, None);
            state = s;
        }
        assert_eq!(slow_at_least(state, false, Duration::from_millis(100), start + QUICKEST_STALE).0, None, "a prompt one: over");
        let (_, rebased) = slow_at_least(state, true, Duration::from_millis(700), start + QUICKEST_STALE);
        assert_eq!(rebased, Some(Duration::from_millis(500)));
        let (window, _) = window_after(transfer::WINDOW, rebased.unwrap(), Duration::from_millis(520), None);
        assert_eq!(window, transfer::WINDOW + 1, "grows again");
    }

    /// A packet lost holds up every chunk behind it, all slow at once:
    /// the window's cut by a quarter for the first, and not again till a
    /// round trip's gone by.
    #[test]
    fn a_run_of_slow_chunks_cuts_the_window_once_a_round_trip() {
        let (rtt, slow) = (Duration::from_millis(70), Duration::from_millis(400));
        let (window, cut) = window_after(200, rtt, slow, None);
        assert_eq!((window, cut), (150, true));
        for _ in 0..100 {
            assert_eq!(window_after(window, rtt, slow, Some(Duration::from_millis(100))), (150, false), "the same hold-up");
        }
        assert_eq!(window_after(150, rtt, slow, Some(slow)), (113, true), "a round trip on: queueing still");
    }

    /// A file asks for no more than a few chunks at a time while this
    /// device is in a call with anyone (not only on this connection):
    /// the call has the link. Out of it, its window again.
    #[tokio::test]
    async fn a_call_with_anyone_holds_a_file_to_a_few_chunks() {
        let dir = std::env::temp_dir().join(format!("swarm-files-call-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = crate::node::Paths::new(&dir);
        let endpoint = crate::transport::bind("[::1]:0".parse().unwrap()).unwrap();
        let me = Arc::new(crate::identity::Identity::generate());
        let ctx = super::super::new_ctx(me.clone(), me, paths, Arc::new(|_| {}), endpoint, None).unwrap();
        let peer = [7; 32];
        let t = transfer::Transfer { id: [1; 16], direction: Direction::In, peer, state: State::Active, size: 100_000_000, sha256: [0; 32], created: 0, name: "big".into(), path: dir.clone(), note: String::new(), cleared: false, settled: false };
        transfer::add(&ctx.paths.transfers(), t).unwrap();
        let mut work = FileWork::new(ctx.clone(), peer, peer, "them".into());
        assert_eq!(work.wants(false).len(), transfer::WINDOW, "a new transfer starts at the least");
        let pull = work.pulling.get_mut(&[1; 16]).unwrap();
        pull.asked = 0;
        pull.window = 100;
        ctx.call_media();
        assert_eq!(work.wants(false).len(), transfer::WINDOW, "in a call with someone else: a few at a time");
        let pull = work.pulling.get_mut(&[1; 16]).unwrap();
        pull.asked = 0;
        *ctx.call_media_at.locked() = Some(Instant::now() - Duration::from_secs(10));
        assert_eq!(work.wants(false).len(), 100, "the call over: its window again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What's received is hashed as it's written, so finishing needn't
    /// read it all back: a repeat (which `transfer::append` ignores)
    /// isn't hashed twice, and anything written but not hashed gives up
    /// on it (the file's read back instead).
    #[test]
    fn whats_received_is_hashed_as_it_comes() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7) as u8).collect();
        let (a, b, c) = (&data[..4000], &data[4000..8000], &data[8000..]);
        let mut hashed = Some((Sha256::new(), 0));
        hashed = hashed_after(hashed, 0, a, 4000);
        hashed = hashed_after(hashed, 0, a, 4000);
        hashed = hashed_after(hashed, 4000, b, 8000);
        assert_eq!(digest_of(hashed.clone(), 10_000), None, "not all of it yet");
        hashed = hashed_after(hashed, 8000, c, 10_000);
        assert_eq!(digest_of(hashed, 10_000), Some(Sha256::digest(&data).into()));
        assert!(hashed_after(Some((Sha256::new(), 0)), 4000, b, 8000).is_none(), "written, but what came before it wasn't hashed");
        assert!(hashed_after(None, 0, a, 4000).is_none(), "resumed from what's on disk: read back at the end");
    }
}
