//! The user's peers, the same on every one of their devices.
//!
//! Each device keeps a book (`peersbook`): every peer the user has had,
//! by key, with its name and addresses, whether it's a peer now, and
//! when that became so. Changes to the peers list are noticed by
//! comparing it with the book (`note`), so however a peer was added or
//! taken off, it's recorded. The user's devices swap books over their
//! links (`Notice::PeersBook`); for each key, the later change wins
//! (`merge`), and the peers list is brought into step with the book
//! (`apply`).
//!
//! Someone taken off stays in the book, not present, only until every
//! one of the user's devices has that (`settle`): then nothing of them
//! is left (`tidy`).
//!
//! A peer's addresses in the book are what was typed, and, once the peer
//! has told a device where it can be reached, that too (`heard`), so a
//! device it's never reached can dial it: one behind a NAT, with only
//! IPv4, given a peer who connected to another device over IPv6. A device
//! taking in a book keeps those addresses with the ones it's heard itself
//! (`addressbook::see`). What was typed, already in the book's addresses,
//! isn't a change.
//!
//! A gone line is only ever written because the user deleted someone
//! (`forgotten`, `crate::forget::from_book`) or changed the key they have
//! for them. Someone in the book who isn't on the list hasn't been
//! deleted: the list may have been lost or damaged, or a stop may have
//! come part-way through taking a book in. The next book to arrive puts
//! them back (`merge`).

use super::{Ctx, Paths};
use crate::lock::Locked as _;
use crate::protocol::{BookPeer, Notice};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn book_path(paths: &Paths) -> std::path::PathBuf {
    paths.dir().join("peersbook")
}

fn load(ctx: &Ctx) -> BTreeMap<[u8; 32], BookPeer> {
    load_from(&ctx.paths)
}

fn load_from(paths: &Paths) -> BTreeMap<[u8; 32], BookPeer> {
    let text = crate::store::read(book_path(paths)).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let key: [u8; 32] = STANDARD.decode(f.next()?).ok()?.try_into().ok()?;
            let at = f.next()?.parse().ok()?;
            let present = f.next()? == "yes";
            let name = f.next()?.to_string();
            let address = f.next()?.to_string();
            Some((key, BookPeer { key, name, address, at, present }))
        })
        .collect()
}

fn save(ctx: &Ctx, book: &BTreeMap<[u8; 32], BookPeer>) {
    save_to(&ctx.paths, book);
}

fn save_to(paths: &Paths, book: &BTreeMap<[u8; 32], BookPeer>) {
    let text: String = book
        .values()
        .map(|e| format!("{}\t{}\t{}\t{}\t{}\n", STANDARD.encode(e.key), e.at, if e.present { "yes" } else { "no" }, e.name, e.address))
        .collect();
    let _ = crate::store::write_atomic(&book_path(paths), text);
}

/// The peers that are the user's to share: everyone on the list but the
/// user.
fn shared_peers(ctx: &Ctx) -> Option<Vec<crate::peers::Peer>> {
    let me = super::devices::me(ctx);
    Some(crate::peers::load(&ctx.paths.peers()).ok()?.into_iter().filter(|p| p.public_key != me).collect())
}

/// While it's held, a book is being taken in: see the top of this file.
struct Arriving<'a>(&'a Ctx);

impl<'a> Arriving<'a> {
    fn new(ctx: &'a Ctx) -> Self {
        ctx.books_arriving.fetch_add(1, Ordering::SeqCst);
        Arriving(ctx)
    }
}

impl Drop for Arriving<'_> {
    fn drop(&mut self) {
        self.0.books_arriving.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Records in the book whatever's changed on the peers list since it was
/// last looked at; returns the book.
fn note(ctx: &Ctx) -> BTreeMap<[u8; 32], BookPeer> {
    let _guard = crate::store::lock();
    note_held(ctx, None)
}

/// `note`, with the store's lock already held; `added`, a peer the user
/// has just added, is as now rather than found.
fn note_held(ctx: &Ctx, added: Option<&[u8; 32]>) -> BTreeMap<[u8; 32], BookPeer> {
    let mut book = load(ctx);
    let now = super::unix_now();
    let Some(peers) = shared_peers(ctx) else { return book };
    let mut changed = false;
    let arriving = ctx.books_arriving.load(Ordering::SeqCst) > 0;
    for p in &peers {
        let just_added = added == Some(&p.public_key);
        if arriving && !just_added && book.get(&p.public_key).is_some_and(|e| !e.present) {
            continue;
        }
        let same = book.get(&p.public_key).is_some_and(|e| e.present && e.name == p.name && within(&p.address, &e.address) && !(just_added && e.at == 0));
        if !same {
            let address = shared_address(ctx, &p.public_key, &p.address);
            let at = book.get(&p.public_key).map_or(if just_added { now } else { 0 }, |e| now.max(e.at + 1));
            book.insert(p.public_key, BookPeer { key: p.public_key, name: p.name.clone(), address, at, present: true });
            changed = true;
        }
    }
    let others = super::devices::others(ctx);
    let mut settle = ctx.settle.locked();
    book.retain(|key, e| {
        let keep = e.present || !settle.settled(super::settle::Book::Peers, *key, e.at, &others);
        if !keep {
            settle.dropped(super::settle::Book::Peers, key);
            changed = true;
        }
        keep
    });
    drop(settle);
    if changed {
        save(ctx, &book);
    }
    book
}

/// The book brought up to date with the peers list, and what's settled
/// dropped: every half minute, and after a contact's deleted. With no
/// other device there's nobody to share it with, so there's none: it's
/// made afresh from the peers list when there is (`notice`).
pub(super) fn tidy(ctx: &Ctx) {
    if super::devices::has_others(ctx) {
        note(ctx);
    } else {
        let _guard = crate::store::lock();
        let _ = std::fs::remove_file(book_path(&ctx.paths));
    }
}

/// Peer `key` added here just now (by the user, not from another of
/// their devices): in the book at once, as now, so that a deletion
/// elsewhere before it isn't taken as newer.
pub(super) fn added(ctx: &Ctx, key: &[u8; 32]) {
    if !super::devices::has_others(ctx) {
        tidy(ctx);
        return;
    }
    let _guard = crate::store::lock();
    note_held(ctx, Some(key));
}

/// Peer `key`, called `name`, deleted here: a gone line for them, as
/// now, whether or not the book had looked since they came (one taken in
/// from another device's contacts a moment ago may have no line yet, and
/// another device's line for them would bring them back).
pub(super) fn forgotten(ctx: &Ctx, key: &[u8; 32], name: &str) {
    if !super::devices::has_others(ctx) {
        return;
    }
    let _guard = crate::store::lock();
    mark_gone(&ctx.paths, key, name);
}

/// `forgotten`, for a folder no node here is running on. With no book
/// there's only this one device, and nobody to tell.
pub(crate) fn forgotten_on_disk(paths: &Paths, key: &[u8; 32], name: &str) {
    let _guard = crate::store::lock();
    if book_path(paths).exists() {
        mark_gone(paths, key, name);
    }
}

/// The gone line, with the store's lock held.
fn mark_gone(paths: &Paths, key: &[u8; 32], name: &str) {
    let mut book = load_from(paths);
    let now = super::unix_now();
    match book.get_mut(key) {
        Some(entry) if !entry.present => return,
        Some(entry) => {
            entry.present = false;
            entry.at = now.max(entry.at + 1);
        }
        None => {
            book.insert(*key, BookPeer { key: *key, name: name.to_string(), address: String::new(), at: now.max(1), present: false });
        }
    }
    save_to(paths, &book);
}

/// Whether the book has a line for `key`, there or gone. Takes no lock.
pub(super) fn has_line(ctx: &Ctx, key: &[u8; 32]) -> bool {
    load(ctx).contains_key(key)
}

/// Whether `key` was a peer taken off, and not added again: a copy of a
/// message with them that arrives late isn't kept.
pub(super) fn is_gone(ctx: &Ctx, key: &[u8; 32]) -> bool {
    load(ctx).get(key).is_some_and(|e| !e.present)
}

/// Whether every address in list `part` is in list `all`.
fn within(part: &str, all: &str) -> bool {
    crate::peers::address_items(part).all(|a| crate::peers::address_items(all).any(|b| b == a))
}

/// The addresses the book gives peer `key`: where each of their devices
/// last said it can be reached, the newest first, then where they've
/// been reached or seen from (link-local addresses aside, which mean
/// nothing on another device's link), then what was `typed`. A device's
/// are kept under its own key (`PeerTask::address_key`), not the user's.
pub(super) fn shared_address(ctx: &Ctx, key: &[u8; 32], typed: &str) -> String {
    let dir = ctx.paths.addresses();
    let keys: Vec<[u8; 32]> = super::peerdevices::known(ctx, key).into_iter().chain([*key]).collect();
    let mut told: Vec<crate::addressbook::Told> = keys.iter().filter_map(|k| crate::addressbook::told(&dir, k)).collect();
    told.sort_by_key(|t| std::cmp::Reverse(t.at));
    let seen = keys.iter().flat_map(|k| crate::addressbook::load(&dir, k));
    let routed = told.into_iter().flat_map(|t| t.addresses).chain(seen).filter(|a| !a.starts_with("fe80:") && !a.starts_with("[fe80:") && !a.starts_with("169.254."));
    let mut all: Vec<String> = Vec::new();
    for a in routed.chain(crate::peers::address_items(typed).map(str::to_string)) {
        if !all.contains(&a) {
            all.push(a);
        }
    }
    all.truncate(16);
    all.join(",")
}

/// Peer `key` has told this device where it can be reached now: into
/// the book, for the user's other devices. Not as a newer line: only a
/// change of the person's own (adding, renaming, editing, deleting) is
/// that, so an address heard as a contact is deleted on another device
/// can't bring them back. The other devices take the addresses all the
/// same (`merge` sees every present line's).
pub(super) fn heard(ctx: &Ctx, key: &[u8; 32]) {
    if !super::devices::has_others(ctx) || ctx.stopped.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let typed = crate::peers::load(&ctx.paths.peers()).unwrap_or_default().into_iter().find(|p| p.public_key == *key).map(|p| p.address).unwrap_or_default();
    let address = shared_address(ctx, key, &typed);
    let _guard = crate::store::lock();
    let mut book = load(ctx);
    if let Some(entry) = book.get_mut(key).filter(|e| e.present && e.address != address) {
        entry.address = address;
        save(ctx, &book);
    }
}

/// The book, as a notice for another of the user's devices — after
/// taking in any change to the peers list. `None` for a user with one
/// device.
pub(super) fn notice(ctx: &Ctx) -> Option<Notice> {
    super::devices::has_others(ctx).then(|| Notice::PeersBook { entries: note(ctx).into_values().collect() })
}

/// Another of the user's devices' (`device`'s) book: what's newer than
/// ours is taken, and the peers list brought into step.
pub(super) fn merge(ctx: &Arc<Ctx>, device: [u8; 32], entries: Vec<BookPeer>) {
    if !super::devices::has_others(ctx) || ctx.stopped.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    use super::settle::{Book, Held};
    let _arriving = Arriving::new(ctx);
    let held = entries.iter().map(|e| (e.key, if e.present { Held::There } else { Held::Gone(e.at) })).collect();
    ctx.settle.locked().arrived(Book::Peers, device, held);
    let me = super::devices::me(ctx);
    let entries: Vec<BookPeer> = entries.into_iter().filter(|e| e.key != me && crate::peers::validate_name(&e.name).is_ok()).collect();
    for entry in entries.iter().filter(|e| e.present) {
        let items: Vec<String> = crate::peers::address_items(&entry.address).map(str::to_string).collect();
        let _ = crate::addressbook::see(&ctx.paths.addresses(), &entry.key, &items);
    }
    let book = {
        let _guard = crate::store::lock();
        let mut book = note_held(ctx, None);
        let mut changed = false;
        for entry in entries {
            match book.get(&entry.key) {
                None if !entry.present => {}
                Some(ours) if entry.at < ours.at || (entry.at == ours.at && !(ours.present && !entry.present)) => {}
                _ => {
                    book.insert(entry.key, entry);
                    changed = true;
                }
            }
        }
        if changed {
            save(ctx, &book);
        }
        let on_list = crate::peers::load(&ctx.paths.peers()).unwrap_or_default();
        if !changed && book.values().filter(|e| e.present).all(|e| on_list.iter().any(|p| p.public_key == e.key)) {
            return;
        }
        book
    };
    apply(ctx, &book);
}

/// Brings the peers list into step with the book: those taken off
/// first; the book looked at again only
/// once all of it's done (see `NodeHandle::forget_contact_then`).
fn apply(ctx: &Arc<Ctx>, book: &BTreeMap<[u8; 32], BookPeer>) {
    let path = ctx.paths.peers();
    if crate::peers::load(&path).is_err() {
        return;
    }
    let handle = super::NodeHandle { ctx: ctx.clone() };
    for entry in book.values().filter(|e| !e.present) {
        let on_file = crate::peers::load(&path).unwrap_or_default();
        if let Some(p) = on_file.iter().find(|p| p.public_key == entry.key) {
            let _ = crate::peers::remove(&path, &p.handle());
            let blocked = crate::requests::is_blocked(&ctx.paths.requests(), &p.public_key).unwrap_or(false);
            handle.forget_contact_then(&p.handle(), p.public_key, blocked, false);
            log!(ctx, "deleted '{}': another of your devices deleted them", p.name);
        }
    }
    for entry in book.values().filter(|e| e.present) {
        let _forgetting = ctx.forgetting.locked();
        let now_on_file = crate::peers::load(&path).unwrap_or_default();
        if !load(ctx).get(&entry.key).is_some_and(|e| e.present) {
            continue;
        }
        if let Some(p) = now_on_file.iter().find(|p| p.public_key == entry.key) {
            if p.name != entry.name && crate::peers::update(&path, &p.handle(), &entry.name, p.address.clone(), p.public_key).is_ok() {
                log!(ctx, "'{}' is '{}' now: renamed on another of your devices", p.name, entry.name);
                handle.update_peer_live(&p.handle(), crate::peers::Peer { name: entry.name.clone(), ..p.clone() });
                let published = crate::displayname::cached_one(&ctx.paths.displaynames(), &p.public_key).ok().flatten().map(|d| d.name).unwrap_or_default();
                let theirs = !published.trim().is_empty() && crate::peers::label_from(&published, &p.name) == entry.name;
                let _ = crate::settings::set_named_after_them(&ctx.paths.settings(), &p.public_key, theirs);
            }
            continue;
        }
        let name = entry.name.clone();
        let peer = crate::peers::Peer { name: name.clone(), address: entry.address.clone(), public_key: entry.key, cover_bits_per_second: None, cover_required: false };
        if crate::peers::add(&path, &peer).is_ok() {
            let _ = crate::requests::forget(&ctx.paths.requests(), &entry.key);
            log!(ctx, "added '{name}': another of your devices added them");
            handle.start_peer(peer);
        } else {
            log!(ctx, "couldn't add '{name}' from another of your devices");
            let _guard = crate::store::lock();
            let mut book = load(ctx);
            if book.remove(&entry.key).is_some() {
                save(ctx, &book);
            }
        }
    }
    note(ctx);
    super::devices::changed(ctx);
}
