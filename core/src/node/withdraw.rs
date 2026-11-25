//! Telling someone that the contact request they were given has been
//! taken back (`withdrawals`). Kept the same on all of the user's devices,
//! and whichever reaches them first tells them. They're no longer a
//! contact, so nothing connects to them any more: `keep_telling` dials
//! them for it, until they answer.

use super::{unix_now, Ctx, NodeHandle};
use crate::lock::Locked as _;
use crate::protocol::{self, Incoming, Notice, Reply, Request};
use crate::withdrawals::{self, What, Withdrawal};
use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// How often someone not answering is tried again.
const RETRY: Duration = Duration::from_secs(5 * 60);

/// The longest one attempt gets, however it's going.
const ATTEMPT: Duration = Duration::from_secs(30);

/// The lines as notices for another of the user's devices, in as many
/// parts as it takes. None with nothing to tell.
pub(super) fn book(ctx: &Ctx) -> Option<Vec<Notice>> {
    /// Well inside a frame, with its header.
    const PART_BYTES: usize = 6000;
    let all = withdrawals::load(&ctx.paths.withdrawals());
    if all.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let (mut part, mut size) = (Vec::new(), 0);
    for line in all {
        let bytes = 32 + 8 + 1 + 1 + 2 + line.address.len();
        if size + bytes > PART_BYTES {
            parts.push(Notice::WithdrawalsBook { entries: std::mem::take(&mut part) });
            size = 0;
        }
        size += bytes;
        part.push(line);
    }
    parts.push(Notice::WithdrawalsBook { entries: part });
    Some(parts)
}

/// Another of the user's devices' lines, taken in; anyone to be told is
/// tried straight away.
pub(super) fn merge_book(ctx: &Ctx, entries: Vec<Withdrawal>) {
    if matches!(withdrawals::merge(&ctx.paths.withdrawals(), entries), Ok(true)) {
        ctx.withdrawals_wake.notify_one();
    }
}

/// `to` has been told `what`.
pub(super) fn told(ctx: &Ctx, what: &What, to: &[u8; 32]) {
    let _ = withdrawals::finish(&ctx.paths.withdrawals(), what, to, unix_now());
}

/// Keeps trying everyone whose contact request has been cancelled and who
/// hasn't heard yet. Asleep with nobody to tell.
pub(super) async fn keep_telling(ctx: Arc<Ctx>) {
    while !ctx.stopped.load(Ordering::Relaxed) {
        let now = unix_now();
        let due: Vec<Withdrawal> = withdrawals::load(&ctx.paths.withdrawals()).into_iter().filter(|w| w.what == What::ContactRequest && w.is_due(now)).collect();
        let mut waiting = false;
        for line in due {
            if ctx.peers.locked().values().any(|e| e.watch.borrow().public_key == line.to) {
                waiting = true;
                continue;
            }
            match tokio::time::timeout(ATTEMPT, tell_request_cancelled(&ctx, &line)).await {
                Ok(Ok(true)) => told(&ctx, &What::ContactRequest, &line.to),
                _ => waiting = true,
            }
        }
        if waiting {
            let _ = tokio::time::timeout(RETRY, ctx.withdrawals_wake.notified()).await;
        } else {
            ctx.withdrawals_wake.notified().await;
        }
    }
}

/// Dials the one this line is about and tells them. Whether they've taken
/// it in: a build too old to know how answers `Unsupported`, and is tried
/// again in case it's updated.
async fn tell_request_cancelled(ctx: &Ctx, line: &Withdrawal) -> Result<bool> {
    let mut conn = dial(ctx, line.to, &line.address).await?;
    let id = protocol::request(&mut conn, Request::WithdrawContactRequest { root_key: ctx.identity.public_key_bytes() }).await?;
    let answer = loop {
        let incoming = protocol::recv(&mut conn).await?;
        protocol::flush(&mut conn).await?;
        if let Incoming::Reply(answering, reply) = incoming
            && answering == id
        {
            break reply;
        }
    };
    conn.connection.close(0u32.into(), b"done");
    Ok(matches!(answer, Reply::Ack))
}

/// `root_key`, already checked against the connection, has cancelled the
/// contact request they made. The line is dismissed even if there's none
/// here: the request may have reached another of the user's devices, and
/// this way the book takes it off there too.
pub(super) fn request_cancelled(ctx: &Ctx, root_key: &[u8; 32]) {
    if let Ok(true) = crate::requests::cancelled(&ctx.paths.requests(), root_key, unix_now()) {
        log!(ctx, "a contact request was cancelled by the one who made it");
    }
}

/// Dials `key` at `address`, for a connection of its own.
async fn dial(ctx: &Ctx, key: [u8; 32], address: &str) -> Result<crate::transport::AuthenticatedConnection> {
    let resolved = crate::peers::resolve_address(address).await?;
    super::peer::dial_any(&ctx.endpoint, &resolved, ctx.keys(), key).await
}

impl NodeHandle {
    /// Tries everyone still to be told straight away, instead of at the
    /// next retry.
    pub fn try_withdrawals_now(&self) {
        self.ctx.withdrawals_wake.notify_one();
    }

    /// Who's still to be told about a cancelled contact request, by key.
    pub fn withdrawals_waiting(&self) -> Vec<Withdrawal> {
        let now = unix_now();
        withdrawals::load(&self.ctx.paths.withdrawals()).into_iter().filter(|w| w.is_due(now)).collect()
    }

    /// Everything else the deletion of contact `key` needs done first, if
    /// this user's request to them hasn't been accepted: they're to be told
    /// it's cancelled. `typed`: the address on the list.
    pub(super) fn cancel_request_if_asked(&self, key: &[u8; 32], typed: &str) {
        let asked = self.ctx.peers.locked().values().any(|e| e.watch.borrow().public_key == *key && e.asked.load(Ordering::Relaxed));
        if !asked {
            return;
        }
        let address = super::ownpeers::shared_address(&self.ctx, key, typed);
        let _ = withdrawals::add(&self.ctx.paths.withdrawals(), What::ContactRequest, *key, &address, unix_now());
    }

    /// Added again: a cancellation not yet delivered would take back the
    /// new request.
    pub(super) fn keep_request(&self, key: &[u8; 32]) {
        told(&self.ctx, &What::ContactRequest, key);
    }
}
