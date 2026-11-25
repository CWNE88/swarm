//! Contact requests, the same on every one of the user's devices.
//!
//! A stranger who's added the user reaches whichever device answers at
//! the address they were given — with IPv4 and one port forwarded, the
//! same one every time — and introduces themselves to it alone. So each
//! device gives the others its requests file (`requests::all`) over their
//! links, with the name each stranger gave, and takes theirs in
//! (`requests::merge`, the line changed last winning): the request is on
//! every device, and dismissing, blocking or unblocking one on one
//! device does so on all. A dismissed line goes once every device has it
//! (`tidy`, `settle`).

use super::Ctx;
use crate::lock::Locked as _;
use crate::protocol::{BookRequest, Notice};
use base64::{engine::general_purpose::STANDARD, Engine as _};

/// Roughly how many bytes of entries go in one notice, well under
/// `framing::MAX_FRAME` with the frame's own header.
const PART_BYTES: usize = 6000;

/// The requests file as notices for another of the user's devices, in
/// as many parts as it takes. None for a user with one device.
pub(super) fn notices(ctx: &Ctx) -> Option<Vec<Notice>> {
    if !super::devices::has_others(ctx) {
        return None;
    }
    tidy(ctx);
    let names = crate::displayname::cached_all(&ctx.paths.displaynames()).unwrap_or_default();
    let mut parts = Vec::new();
    let mut part = Vec::new();
    let mut size = 0;
    for request in crate::requests::all(&ctx.paths.requests()).unwrap_or_default() {
        let name = (request.status != crate::requests::Status::Dismissed).then(|| names.get(&request.public_key).cloned()).flatten();
        let bytes = 32 + 1 + 2 + request.address.len() + 16 + 1 + name.as_ref().map_or(0, |n| 8 + 64 + 2 + n.name.len());
        if size + bytes > PART_BYTES && !part.is_empty() {
            parts.push(Notice::RequestsBook { entries: std::mem::take(&mut part), more: true });
            size = 0;
        }
        size += bytes;
        part.push(BookRequest { request, name });
    }
    parts.push(Notice::RequestsBook { entries: part, more: false });
    Some(parts)
}

/// Drops each dismissed line every one of the user's other devices has:
/// every half minute, and when the book goes to one of them. (`settle`
/// is let go before the file's locked: `ownpeers::note` takes them the
/// other way round.)
pub(super) fn tidy(ctx: &Ctx) {
    use super::settle::Book;
    use crate::requests::Status;
    let path = ctx.paths.requests();
    let others = super::devices::others(ctx);
    let settled: Vec<([u8; 32], u64)> = {
        let mut settle = ctx.settle.locked();
        crate::requests::all(&path)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.status == Status::Dismissed && settle.settled(Book::Requests, r.public_key, r.last_seen, &others))
            .map(|r| (r.public_key, r.last_seen))
            .collect()
    };
    if settled.is_empty() {
        return;
    }
    if let Ok(true) = crate::requests::prune(&path, |r| settled.contains(&(r.public_key, r.last_seen))) {
        let mut settle = ctx.settle.locked();
        for (key, _) in &settled {
            settle.dropped(Book::Requests, key);
        }
    }
}

/// Another of the user's devices' (`device`'s) whole requests book:
/// what's newer than ours is taken. Not the user, nor a request from
/// anyone who's a peer here (added on another device meanwhile: the
/// peers book has them) — though a block is, as the peers book is about
/// to take them off.
pub(super) fn merge(ctx: &Ctx, device: [u8; 32], entries: Vec<BookRequest>) {
    if !super::devices::has_others(ctx) || ctx.stopped.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    use super::settle::{Book, Held};
    use crate::requests::Status;
    let held = entries.iter().map(|e| (e.request.public_key, if e.request.status == Status::Dismissed { Held::Gone(e.request.last_seen) } else { Held::There })).collect();
    ctx.settle.locked().arrived(Book::Requests, device, held);
    let me = super::devices::me(ctx);
    let peers: Vec<[u8; 32]> = crate::peers::load(&ctx.paths.peers()).unwrap_or_default().into_iter().map(|p| p.public_key).collect();
    let mut lines = Vec::with_capacity(entries.len());
    for BookRequest { request, name } in entries {
        if request.public_key == me || (request.status != Status::Blocked && peers.contains(&request.public_key)) {
            continue;
        }
        let wanted = request.status == Status::Blocked || !super::just_deleted(ctx, &request.public_key);
        if let Some(claimed) = name.filter(|claimed| wanted && crate::displayname::verify(claimed, &request.public_key)) {
            let _ = crate::displayname::cache_one(&ctx.paths.displaynames(), request.public_key, &claimed);
        }
        lines.push(request);
    }
    let path = ctx.paths.requests();
    let asking = || crate::requests::pending(&path).unwrap_or_default().into_iter().map(|r| r.public_key).collect::<Vec<_>>();
    let before = asking();
    match crate::requests::merge(&path, lines, |_| false) {
        Ok(true) => {
            for key in asking().into_iter().filter(|k| !before.contains(k)) {
                let who = crate::displayname::cached_one(&ctx.paths.displaynames(), &key).ok().flatten().map(|n| n.name).unwrap_or_else(|| STANDARD.encode(key));
                log!(ctx, "contact request from {who}, passed on by another of your devices");
            }
        }
        Ok(false) => {}
        Err(e) => log!(ctx, "couldn't take in contact requests from your other devices: {e}"),
    }
}
