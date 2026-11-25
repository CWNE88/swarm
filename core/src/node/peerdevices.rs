//! A peer's devices: every device of a
//! user holds the user's key, so a connection from any of them is from
//! that user, and its `Hello` says which device it is.
//!
//! The peers file holds users. Each device of a peer's that's been met
//! has an entry of the node's own, with a task as any peer has, under
//! `<user's handle> #<device>` (`entry_name`), and called
//! `<user's name> #<device>` in the log: its `Peer`
//! has the user's key, so their history, outbox and settings are one,
//! and its task the device's key besides (`PeerTask::device`). The
//! user's own entry only finds their devices: it dials what was typed
//! for them while none is online, and hands what answers to the
//! device's entry (`hand_over`), made if it's new and kept in
//! `peer-device-keys` for the next start (`known`).
//!
//! A device the user takes off their devices is refused from then on:
//! their devices send the removal, signed with the user key, and it's
//! kept in `refused-devices` (`refuse`).

use super::{Ctx, PeerEntry};
use crate::lock::Locked;
use crate::peers::Peer;
use crate::transport::AuthenticatedConnection;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// What comes between a user's handle or name and their device's in a
/// device's entry. Neither has spaces, so it's never part of one.
const DEVICE_MARK: &str = " #";

/// `device`'s entry, one of `user_name`'s devices: given the user's
/// handle, its place in the node's table; given their name, what the
/// log calls it.
pub(super) fn entry_name(user_name: &str, device: &[u8; 32]) -> String {
    let short: String = STANDARD.encode(device).chars().filter(char::is_ascii_alphanumeric).take(6).collect();
    format!("{user_name}{DEVICE_MARK}{short}")
}

/// The user's part of an entry's handle or log name: a device's without
/// the device; anyone else's as it is.
pub fn user_name(name: &str) -> &str {
    name.split_once(DEVICE_MARK).map_or(name, |(user, _)| user)
}

fn file(ctx: &Ctx) -> std::path::PathBuf {
    ctx.paths.dir().join("peer-device-keys")
}

/// The devices of user `user` met so far.
pub(super) fn known(ctx: &Ctx, user: &[u8; 32]) -> Vec<[u8; 32]> {
    let text = crate::store::read(file(ctx)).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let (u, d) = line.split_once('\t')?;
            let u: [u8; 32] = STANDARD.decode(u).ok()?.try_into().ok()?;
            let d: [u8; 32] = STANDARD.decode(d).ok()?.try_into().ok()?;
            (u == *user).then_some(d)
        })
        .collect()
}

/// The user whose device `device` is, if it's one met.
pub(super) fn user_of(ctx: &Ctx, device: &[u8; 32]) -> Option<[u8; 32]> {
    user_of_device(&ctx.paths, device)
}

/// `user_of` from the files alone, without a running node: for a
/// command that reads them itself (`swarm transfers`, whose records go
/// by the device's key).
pub fn user_of_device(paths: &super::Paths, device: &[u8; 32]) -> Option<[u8; 32]> {
    let text = crate::store::read(paths.dir().join("peer-device-keys")).unwrap_or_default();
    text.lines().find_map(|line| {
        let (u, d) = line.split_once('\t')?;
        let d: [u8; 32] = STANDARD.decode(d).ok()?.try_into().ok()?;
        if d != *device {
            return None;
        }
        STANDARD.decode(u).ok()?.try_into().ok()
    })
}

/// Keeps `device` as one of `user`'s devices.
fn note(ctx: &Ctx, user: &[u8; 32], device: &[u8; 32]) {
    let _guard = crate::store::lock();
    if known(ctx, user).contains(device) {
        return;
    }
    let _ = crate::store::append_line(&file(ctx), &format!("{}\t{}", STANDARD.encode(user), STANDARD.encode(device)));
}

/// User `user`'s own entry here (its name and record), if they're a peer.
fn user_entry(ctx: &Ctx, user: &[u8; 32]) -> Option<Peer> {
    ctx.peers.locked().values().find(|e| e.device.is_none() && e.watch.borrow().public_key == *user).map(|e| e.watch.borrow().clone())
}

/// The record a device's entry has: its user's, under the entry's name.
fn device_record(user: &Peer, device: &[u8; 32]) -> Peer {
    Peer { name: entry_name(&user.name, device), ..user.clone() }
}

/// Starts an entry for each device of `user`'s met before. For when
/// their user's entry starts.
pub(super) fn spawn_known(ctx: &Arc<Ctx>, user: &Peer) {
    for device in known(ctx, &user.public_key) {
        let record = device_record(user, &device);
        if !ctx.peers.locked().contains_key(&entry_name(&user.handle(), &device)) {
            super::peer::spawn_device(ctx, record, device, None);
        }
    }
}

/// `conn`, from one of user `user`'s devices, to that device's entry:
/// made if it's a device not met before. Dropped if they're no peer, or
/// they've taken that device off.
pub(super) fn hand_over(ctx: &Arc<Ctx>, user: &[u8; 32], conn: AuthenticatedConnection) {
    let Some(record) = user_entry(ctx, user) else { return };
    let device = conn.remote_device;
    if refused(ctx, user, &device) {
        log!(ctx, "{}: refused a device they've taken off", record.name);
        conn.connection.close(0u32.into(), b"refused");
        return;
    }
    let name = entry_name(&record.handle(), &device);
    if !ctx.peers.locked().contains_key(&name) {
        log!(ctx, "{}: a device not met before", record.name);
        note(ctx, user, &device);
        super::peer::spawn_device(ctx, device_record(&record, &device), device, Some(conn));
        return;
    }
    if let Some(entry) = ctx.peers.locked().get(&name) {
        let _ = entry.inbound.try_send(conn);
    }
}

fn refused_file(ctx: &Ctx) -> std::path::PathBuf {
    ctx.paths.dir().join("refused-devices")
}

/// Whether user `user` has taken `device` off their devices.
pub(super) fn refused(ctx: &Ctx, user: &[u8; 32], device: &[u8; 32]) -> bool {
    let line = format!("{}\t{}", STANDARD.encode(user), STANDARD.encode(device));
    crate::store::read(refused_file(ctx)).unwrap_or_default().lines().any(|l| l == line)
}

/// Removals from user `user`'s device list, sent by one of their devices:
/// each that's signed with their key is refused from now on — forgotten
/// as one of theirs, and its entry stopped. Returns the devices newly
/// refused.
pub(super) fn refuse(ctx: &Ctx, user: &[u8; 32], removals: &[crate::devicelist::Entry]) -> Vec<[u8; 32]> {
    let mut newly = Vec::new();
    for e in removals.iter().filter(|e| e.change == crate::devicelist::Change::Removed && crate::devicelist::verify(user, e)) {
        if newly.contains(&e.device) || refused(ctx, user, &e.device) {
            continue;
        }
        let _guard = crate::store::lock();
        let _ = crate::store::append_line(&refused_file(ctx), &format!("{}\t{}", STANDARD.encode(user), STANDARD.encode(e.device)));
        let gone = format!("{}\t{}", STANDARD.encode(user), STANDARD.encode(e.device));
        let kept: String = crate::store::read(file(ctx)).unwrap_or_default().lines().filter(|l| *l != gone).map(|l| format!("{l}\n")).collect();
        let _ = crate::store::write_atomic(&file(ctx), kept);
        newly.push(e.device);
    }
    if !newly.is_empty() {
        let mut peers = ctx.peers.locked();
        let names: Vec<String> = peers.iter().filter(|(_, e)| e.device.is_some_and(|d| newly.contains(&d)) && e.watch.borrow().public_key == *user).map(|(n, _)| n.clone()).collect();
        for name in names {
            if let Some(gone) = peers.remove(&name) {
                log!(ctx, "{}: they've taken a device off, refused from now on", user_name(&gone.watch.borrow().name));
            }
        }
    }
    newly
}

/// The entries of user `user`'s devices (by their entries' names).
pub(super) fn entries(ctx: &Ctx, user: &[u8; 32]) -> Vec<String> {
    let mut names: Vec<String> = ctx.peers.locked().iter().filter(|(_, e)| e.device.is_some() && e.watch.borrow().public_key == *user).map(|(n, _)| n.clone()).collect();
    names.sort();
    names
}

/// Whether any of user `user`'s devices' entries has been told they
/// haven't added us, or have removed us: it checks back itself.
pub(super) fn any_waiting(ctx: &Ctx, user: &[u8; 32]) -> bool {
    ctx.peers
        .locked()
        .values()
        .any(|e| e.device.is_some() && e.watch.borrow().public_key == *user && (e.not_added.load(Ordering::Relaxed) || e.removed_us.load(Ordering::Relaxed)))
}

/// Whether there's a request out to user `user` (`PeerStatus::asked`):
/// remembered, and the same for all of their entries.
pub(super) fn note_asked(ctx: &Ctx, user: &[u8; 32], on: bool) {
    let _ = crate::settings::set_asked(&ctx.paths.settings(), user, on);
    for entry in ctx.peers.locked().values().filter(|e| e.watch.borrow().public_key == *user) {
        entry.asked.store(on, Ordering::Relaxed);
    }
}

/// Whether any of user `user`'s devices is connected now.
pub(super) fn any_online(ctx: &Ctx, user: &[u8; 32]) -> bool {
    ctx.peers.locked().values().any(|e| e.device.is_some() && e.watch.borrow().public_key == *user && e.online.load(Ordering::Relaxed))
}

/// Stops the entries of user `user`'s devices (their user's has gone,
/// or changed).
pub(super) fn stop_all(ctx: &Ctx, user: &[u8; 32]) {
    let gone: Vec<String> = entries(ctx, user);
    let mut peers = ctx.peers.locked();
    for name in gone {
        let _: Option<PeerEntry> = peers.remove(&name);
    }
}

/// User `user`'s record changed (not their key: their name or cover,
/// say): their devices' entries take it too.
pub(super) fn update_all(ctx: &Ctx, user: &Peer) {
    for entry in ctx.peers.locked().values() {
        if let Some(device) = entry.device.filter(|_| entry.watch.borrow().public_key == user.public_key) {
            let _ = entry.watch.send(device_record(user, &device));
        }
    }
}

/// Where user `user`'s other devices are, as one of theirs (`from`) that
/// relays for the rest passes on (see `devices::for_contacts`): each
/// device's own addresses, as it told its own devices. A device of
/// theirs met before, and not connected now, has them kept as what it
/// last said (if they're newer) and is dialled there at once; one not
/// met, under their user, for the user's entry to look for it there
/// (`PeerTask::search_unmet`), straight away. So a contact who can reach
/// only one of their devices finds the others directly, wherever it can
/// reach them, and each gets a connection of its own.
pub(super) fn hear_devices(ctx: &Ctx, user: &[u8; 32], from: &[u8; 32], devices: &[crate::protocol::DeviceAddress]) {
    let dir = ctx.paths.addresses();
    let met = known(ctx, user);
    let mut search = false;
    for d in devices.iter().take(crate::devicelist::MAX_DEVICES) {
        if d.device == *from || refused(ctx, user, &d.device) {
            continue;
        }
        let addresses: Vec<String> = d.addresses.iter().filter_map(|a| crate::addressbook::checked(a)).take(crate::addressbook::MAX_PER_PEER).collect();
        if addresses.is_empty() {
            continue;
        }
        if met.contains(&d.device) {
            let woken = {
                let peers = ctx.peers.locked();
                let Some(entry) = peers.values().find(|e| e.device == Some(d.device) && e.watch.borrow().public_key == *user) else { continue };
                if entry.online.load(Ordering::Relaxed) || crate::addressbook::told(&dir, &d.device).is_some_and(|t| t.at >= d.at) {
                    continue;
                }
                match crate::addressbook::hear(&dir, &d.device, &addresses, d.at) {
                    Ok(true) => {
                        entry.watch.send_modify(|_| {});
                        Some(user_name(&entry.watch.borrow().name).to_string())
                    }
                    _ => None,
                }
            };
            if let Some(name) = woken {
                log!(ctx, "{name}: told where another of their devices is: {}", addresses.join(", "));
            }
        } else if let Ok(true) = crate::addressbook::see(&dir, user, &addresses) {
            search = true;
        }
    }
    if search {
        let woken = ctx.peers.locked().values().find(|e| e.device.is_none() && e.watch.borrow().public_key == *user).map(|entry| {
            entry.watch.send_modify(|_| {});
            entry.watch.borrow().name.clone()
        });
        if let Some(name) = woken {
            log!(ctx, "{name}: told where a device of theirs not met yet may be");
        }
    }
}
