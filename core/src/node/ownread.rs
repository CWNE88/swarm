//! What's been read, the same on all of a user's devices: each sends the
//! others the marks that have moved (`readmarks`, `Notice::ReadMarks`), on
//! the same check as the peers book. A device reached only through a third
//! gets them from it: the third has taken them in as its own.

use super::{unix_now, Ctx, NodeHandle};
use crate::protocol::Notice;
use crate::readmarks::{self, Chat, Marks};
use std::collections::HashMap;

/// The marks that differ from what this link has had (`sent`, brought up
/// to date), in as many notices as it takes.
pub(super) fn changes(ctx: &Ctx, sent: &mut HashMap<Chat, u64>) -> Vec<Notice> {
    let changed: Vec<(Chat, u64)> = readmarks::load(&ctx.paths.read_marks()).into_iter().filter(|(chat, at)| sent.get(chat) != Some(at)).collect();
    sent.extend(changed.iter().cloned());
    changed.chunks(Notice::MOST_READ_MARKS).map(|part| Notice::ReadMarks { entries: part.to_vec() }).collect()
}

/// Another device's marks, taken in: the front ends recount if any moved.
pub(super) fn merge(ctx: &Ctx, entries: Vec<(Chat, u64)>) {
    let entries = entries.into_iter().filter(|(chat, _)| !matches!(chat, Chat::Contact(key) if super::just_deleted(ctx, key) || super::ownpeers::is_gone(ctx, key)));
    if matches!(readmarks::merge(&ctx.paths.read_marks(), entries), Ok(true)) {
        ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
    }
}

impl NodeHandle {
    /// How far each chat has been read, on any of the user's devices.
    pub fn read_marks(&self) -> Marks {
        readmarks::load(&self.ctx.paths.read_marks())
    }

    /// The chat has been read up to the message sent at `at`. No further
    /// than `CLOCK_SLACK` ahead: a sender whose clock is wrong mustn't
    /// leave everything sent after it looking read.
    pub fn read_up_to(&self, chat: Chat, at: u64) {
        let at = at.min(unix_now().saturating_add(crate::postcache::CLOCK_SLACK));
        let _ = readmarks::merge(&self.ctx.paths.read_marks(), [(chat, at)]);
    }
}
