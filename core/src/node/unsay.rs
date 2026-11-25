//! Deleting lines of a conversation with a contact, on all the user's
//! devices.
//!
//! Their messages and missed calls go at once, and the user's other
//! devices are told (`Notice::Unsaid`, done). A message the user sent goes
//! from the contact's side too. One delivered is noted as being deleted
//! (`unsaying`, on every device of the user's, `Notice::Unsaid` not done);
//! each device connected to one of the contact's asks it to go
//! (`Request::Unsay`), and the first to hear `Ack` takes it out of the
//! history and tells the others. Till then the chat shows it faint, `◷`.
//! One not yet delivered goes at once, on every device of the user's, and
//! from every outbox, so it never is; only this device keeps a note of it
//! (hidden, for 30 days) to tell them in case a copy got there after all.
//!
//! The contact's device takes the line out, notes the message's id as
//! seen (so a copy still on its way isn't kept), and tells their other
//! devices in turn. They're told nothing else: it's just gone.

use super::{Ctx, NodeHandle};
use crate::history::{self, Direction, LineRef};
use crate::lock::Locked as _;
use std::sync::atomic::Ordering;

impl NodeHandle {
    /// Deletes `lines` of the conversation with contact `key` (their user
    /// key), on all the user's devices; one the user sent, from theirs too.
    pub fn delete_lines(&self, key: [u8; 32], lines: &[LineRef]) -> std::io::Result<()> {
        let ctx = &self.ctx;
        for line in lines {
            if line.direction == Direction::Sent {
                let line = with_id(ctx, &key, *line);
                if delivered(ctx, &key, &line) {
                    begin(ctx, key, &line, false)?;
                    super::devices::unsaid(ctx, key, &line, false);
                } else {
                    begin(ctx, key, &line, true)?;
                    let _ = history::remove(&ctx.paths.history(), &key, &line);
                    if let Some(id) = &line.id {
                        let _ = crate::seen::first_time(&ctx.paths.seen(), &key, id);
                    }
                    super::devices::unsaid(ctx, key, &line, true);
                }
            } else {
                gone(ctx, key, line);
                super::devices::unsaid(ctx, key, line, true);
            }
        }
        wake(ctx, &key);
        Ok(())
    }
}

/// A line sent before history kept ids: its id, if the message is still
/// waiting to be delivered (waiting and the outbox have it), so it's
/// still found when it arrives.
fn with_id(ctx: &Ctx, key: &[u8; 32], line: LineRef) -> LineRef {
    if line.id.is_some() {
        return line;
    }
    let waiting = crate::waiting::all(&ctx.paths.waiting(), key).unwrap_or_default().into_iter().map(|w| (w.id, w.at, w.text));
    let queued = crate::outbox::pending(&ctx.paths.outbox(), key).unwrap_or_default().into_iter().map(|m| (m.id, m.sent_at, m.text));
    let id = waiting.chain(queued).find(|(_, at, text)| *at == line.at && history::digest(text) == line.digest).map(|(id, _, _)| id);
    LineRef { id, ..line }
}

/// Whether a message the user sent to `key` has reached one of their
/// devices: it's not waiting, nor in this device's outbox. One from
/// before ids were kept, not found waiting, has.
fn delivered(ctx: &Ctx, key: &[u8; 32], line: &LineRef) -> bool {
    let Some(id) = line.id else { return true };
    let waiting = crate::waiting::all(&ctx.paths.waiting(), key).unwrap_or_default().iter().any(|w| w.id == id);
    let queued = crate::outbox::pending(&ctx.paths.outbox(), key).unwrap_or_default().iter().any(|m| m.id == id);
    !waiting && !queued
}

/// A message the user sent to `key` is being deleted: it waits to be
/// heard of (`unsaying`; `hidden`, one that hadn't been delivered and is
/// gone from the chat already), and isn't sent if it hasn't been.
fn begin(ctx: &Ctx, key: [u8; 32], line: &LineRef, hidden: bool) -> std::io::Result<()> {
    crate::unsaying::add(&ctx.paths.unsaying(), &key, line, super::unix_now(), hidden)?;
    if let Some(id) = &line.id {
        let _ = crate::outbox::remove(&ctx.paths.outbox(), &key, id);
        let _ = crate::waiting::remove(&ctx.paths.waiting(), &key, id);
    }
    ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
    Ok(())
}

/// `line` of the conversation with `key` is gone: out of the history,
/// no longer waiting to be heard of, and its id noted as seen, so a copy
/// of it still on its way (from them, or another of the user's devices)
/// isn't kept.
fn gone(ctx: &Ctx, key: [u8; 32], line: &LineRef) {
    let _ = history::remove(&ctx.paths.history(), &key, line);
    if line.direction == Direction::Sent {
        let _ = crate::unsaying::remove(&ctx.paths.unsaying(), &key, line);
    }
    if let Some(id) = &line.id {
        let _ = crate::seen::first_time(&ctx.paths.seen(), &key, id);
        if line.direction == Direction::Sent {
            let _ = crate::outbox::remove(&ctx.paths.outbox(), &key, id);
            let _ = crate::waiting::remove(&ctx.paths.waiting(), &key, id);
        }
    }
    ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
}

/// Asks every connection to one of `key`'s devices to tell them what's
/// been deleted, now rather than at its next look.
fn wake(ctx: &Ctx, key: &[u8; 32]) {
    for entry in ctx.peers.locked().values().filter(|e| e.online.load(Ordering::Relaxed) && e.watch.borrow().public_key == *key) {
        let _ = entry.requests.try_send(super::peer::PeerRequest::Unsay);
    }
}

/// One of `key`'s devices has heard `line`, a message the user sent, is
/// deleted: it goes here, and the user's other devices are told.
pub(super) fn heard(ctx: &Ctx, key: [u8; 32], line: &LineRef) {
    gone(ctx, key, line);
    super::devices::unsaid(ctx, key, line, true);
}

/// Contact `key` (their user key, checked against the connection) has
/// deleted a message they sent: it goes from the conversation with them,
/// on this device and the user's others.
pub(super) fn taken_back(ctx: &Ctx, key: [u8; 32], id: Option<crate::outbox::MessageId>, at: u64, digest: [u8; 16]) {
    let _forgetting = ctx.forgetting.locked();
    if super::just_deleted(ctx, &key) {
        return;
    }
    let line = LineRef { direction: Direction::Received, id, at, digest };
    gone(ctx, key, &line);
    super::devices::unsaid(ctx, key, &line, true);
}

/// Another of the user's devices says `line` of the conversation with
/// `peer` is deleted: gone (`done`), or sent by the user and still to be
/// heard of.
pub(super) fn from_another_device(ctx: &Ctx, peer: [u8; 32], line: LineRef, done: bool) {
    {
        let _forgetting = ctx.forgetting.locked();
        if super::ownpeers::is_gone(ctx, &peer) || super::just_deleted(ctx, &peer) {
            return;
        }
        if done {
            gone(ctx, peer, &line);
            return;
        }
        if line.direction != Direction::Sent {
            return;
        }
        let _ = begin(ctx, peer, &line, false);
    }
    wake(ctx, &peer);
}
