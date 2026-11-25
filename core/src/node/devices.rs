//! The user's own devices, as each of them keeps in step with the
//! others. Every device holds the user key
//! and a device key of its own; the user's device list
//! (`crate::devicelist`, signed with the user key) says which devices are
//! theirs.
//!
//! - **Device links.** A connection kept to each of the user's other
//!   devices (`keep_siblings`), dialled with the user key at the
//!   addresses that device last gave (kept under its device key in the
//!   address book) and known by the device key its `Hello` proves. If
//!   both ends dial at once, the one dialled by the lower device key is
//!   kept. A connection from a device holding the
//!   user key is taken as one (`accept`), unless the list says it's been
//!   taken off.
//! - **What a link carries** (`run`, `Duties`): the list, both ways, all
//!   of it on connecting and again whenever it changes; where each
//!   device can be reached, and where the others last said they can be,
//!   whenever a device links or moves, looked at again every
//!   `DEVICE_ADDRESSES_EVERY` and sent if it's changed (so two that have
//!   never met, as when both were added from a third, find each other:
//!   `tell_addresses`); copies of the messages each
//!   sends and receives (`carbon`); each other's posts; groups (as on
//!   a contact's connection: `channels`); the user's name and picture (`ownlook`) and
//!   peers (`ownpeers`); and, the first time two devices meet, each
//!   one's peers and chats (`crate::bundle`), so a device just added has
//!   them.
//! - **Removing a device** (`remove`): it's sent the list, which says so,
//!   and wipes itself on reading it (`taken_off`); the user's peers are
//!   sent the removal, signed with the user key, and refuse that device
//!   from then on (`removals`, `peerdevices::refuse`).

use crate::lock::Locked;
use super::callcarry::{CallCarry, CallOn};
use super::{Ctx, PeerCallHandle};
use crate::devicelist::{self, Change};
use crate::protocol::{self, Incoming, Notice, Reply, Request};
use crate::transport::AuthenticatedConnection;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// How often the links needed are worked out (and at once when the list
/// changes).
const RECHECK: Duration = Duration::from_secs(10);
/// How often device links swap posts.
const POSTS_SYNC: Duration = Duration::from_secs(30);
/// How often the user's peers are looked at for a change, to pass on.
const PEERS_BOOK_CHECK: Duration = Duration::from_secs(2);
/// How often what another of our devices was told of where the others
/// are is looked at, and told again if it has changed.
const DEVICE_ADDRESSES_EVERY: Duration = Duration::from_secs(10);
/// How long a device that couldn't be reached is left before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long after moving to another network the devices that can't be
/// reached are tried more often (`RETRY_AFTER_MOVING`): the network may
/// take a moment to work.
const MOVING: Duration = Duration::from_secs(30);
const RETRY_AFTER_MOVING: Duration = Duration::from_secs(5);

/// A link to another of the user's devices, as the node keeps track of
/// it.
pub(super) struct SiblingEntry {
    id: u64,
    dialed_by_us: bool,
    stop: watch::Sender<bool>,
}

/// Calls with another of the user's devices, which ride the link to it
/// (see `callcarry`): their way in from the front end, and what's kept
/// of them from one link to the next — locked by the link that's up, so
/// a new one takes over once the last has let go.
pub(super) struct DeviceCall {
    handle: PeerCallHandle,
    carry: Arc<tokio::sync::Mutex<CallCarry>>,
    /// A call's on the link now: a new link from that device is taken
    /// over the one there, which has likely gone with a move.
    in_call: Arc<AtomicBool>,
}

/// What a call with device `key` is called, for the front end: no
/// peer's name starts with '#'.
pub fn call_name(key: &[u8; 32]) -> String {
    use base64::Engine as _;
    format!("#device {}", base64::engine::general_purpose::STANDARD.encode(key))
}

/// The device a call name (`call_name`) is for.
pub fn call_device(name: &str) -> Option<[u8; 32]> {
    use base64::Engine as _;
    let key = base64::engine::general_purpose::STANDARD.decode(name.strip_prefix("#device ")?).ok()?;
    key.try_into().ok()
}

/// Calls with device `key`, made the first time they're needed.
fn device_call(ctx: &Ctx, key: &[u8; 32]) -> (PeerCallHandle, Arc<tokio::sync::Mutex<CallCarry>>, Arc<AtomicBool>) {
    let mut calls = ctx.device_calls.locked();
    let call = calls.entry(*key).or_insert_with(|| {
        let carry = CallCarry::new();
        DeviceCall { handle: carry.handle.clone(), carry: Arc::new(tokio::sync::Mutex::new(carry)), in_call: Arc::default() }
    });
    (call.handle.clone(), call.carry.clone(), call.in_call.clone())
}

/// How to call another of the user's devices, `key`: over the link to
/// it, or through a device that's let relay when there's no link (see
/// `proxycall`). `None` for one not on the list.
pub(super) fn call_handle(ctx: &Arc<Ctx>, key: &[u8; 32]) -> Option<PeerCallHandle> {
    if !is_mine(ctx, key) || *key == this_device(ctx) {
        return None;
    }
    if reached_through(ctx, key).is_some() {
        return Some(super::proxycall::handle(ctx, *key));
    }
    Some(device_call(ctx, key).0)
}

/// Whether there's a link to device `key` now.
pub(super) fn linked_to(ctx: &Ctx, key: &[u8; 32]) -> bool {
    ctx.siblings.locked().contains_key(key)
}

/// The device let relay that device `key`, with no link from this one, is
/// reached through; `None` if it's linked, or nothing reaches it.
pub(super) fn reached_through(ctx: &Ctx, key: &[u8; 32]) -> Option<[u8; 32]> {
    if linked_to(ctx, key) {
        return None;
    }
    super::proxy::through(ctx, key)
}

/// One of the user's devices, as a front end shows it.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// Its device key.
    pub key: [u8; 32],
    pub name: String,
    /// Where it last said it can be reached (empty for this one).
    pub address: String,
    /// What the user typed for it here, comma-separated (see
    /// `typedaddress`; empty: nothing, and always for this one).
    pub typed: String,
    pub added_at: u64,
    /// This device.
    pub this: bool,
    /// A link to it is up right now, or a device let relay reaches it.
    pub online: bool,
    /// It may relay for the user's other devices.
    pub relay: bool,
    /// It dials no one, only taking connections made to it (known for
    /// this device only: it's set on each, for itself).
    pub receive_only: bool,
    /// The name of the device it's reached through, with no link to it
    /// from this one.
    pub through: Option<String>,
}

/// The user's devices (the user key the account).
#[derive(Clone, Debug)]
pub struct DevicesView {
    pub account: [u8; 32],
    pub devices: Vec<DeviceInfo>,
}

impl DevicesView {
    /// More than this one device.
    pub fn linked(&self) -> bool {
        self.devices.len() > 1
    }
}

/// The user key.
pub(super) fn me(ctx: &Ctx) -> [u8; 32] {
    ctx.identity.public_key_bytes()
}

pub(super) fn this_device(ctx: &Ctx) -> [u8; 32] {
    ctx.device.public_key_bytes()
}

/// The user's device list here.
pub(super) fn own_list(ctx: &Ctx) -> devicelist::List {
    devicelist::load(&ctx.paths.device_list(), me(ctx)).unwrap_or_else(|_| devicelist::List::new(me(ctx)))
}

/// The user's other devices now, by device key.
pub(super) fn others(ctx: &Ctx) -> Vec<[u8; 32]> {
    let this = this_device(ctx);
    own_list(ctx).devices().into_keys().filter(|k| *k != this).collect()
}

/// Whether the user has a device besides this one.
pub(super) fn has_others(ctx: &Ctx) -> bool {
    !others(ctx).is_empty()
}

/// Whether `device` (a device key) is another of the user's devices.
pub(super) fn is_mine(ctx: &Ctx, device: &[u8; 32]) -> bool {
    *device != this_device(ctx) && own_list(ctx).devices().contains_key(device)
}

/// What the person knows device `key` by: its name on the list, or the
/// start of its key.
pub(super) fn label(ctx: &Ctx, key: &[u8; 32]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    own_list(ctx).devices().get(key).map(|d| d.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| STANDARD.encode(key).chars().take(10).collect())
}

/// Something changed about devices: links are rechecked, and carry the
/// list again if it's changed; front ends look again.
pub(super) fn changed(ctx: &Ctx) {
    ctx.device_updates.send_modify(|n| *n += 1);
}

/// Something to tell the person about their devices (the front ends
/// collect these with `NodeHandle::device_news`).
pub(super) fn news(ctx: &Ctx, text: String) {
    log!(ctx, "{text}");
    ctx.device_news.locked().push(text);
    changed(ctx);
}

/// What the front ends show: the user's devices, as their list has them.
pub(super) fn view(ctx: &Ctx) -> DevicesView {
    let this = this_device(ctx);
    let linked: Vec<[u8; 32]> = ctx.siblings.locked().keys().copied().collect();
    let addresses = ctx.paths.addresses();
    let typed = ctx.paths.typed_addresses();
    let receiving_only = receiving_only(ctx);
    let mut devices: Vec<DeviceInfo> = own_list(ctx)
        .devices()
        .into_iter()
        .map(|(key, d)| {
            let through = if key == this { None } else { reached_through(ctx, &key) };
            DeviceInfo {
                key,
                name: d.name,
                address: if key == this { String::new() } else { crate::addressbook::told(&addresses, &key).map(|t| t.addresses.join(",")).unwrap_or_default() },
                typed: if key == this { String::new() } else { crate::typedaddress::load(&typed, &key) },
                added_at: d.added_at,
                this: key == this,
                online: key == this || linked.contains(&key) || through.is_some(),
                relay: d.relay,
                receive_only: key == this && receiving_only,
                through: through.map(|device| label(ctx, &device)),
            }
        })
        .collect();
    devices.sort_by_key(|d| (d.added_at, d.key));
    DevicesView { account: me(ctx), devices }
}

/// Signs `change` to `device` into the user's device list.
fn sign_list(ctx: &Ctx, device: [u8; 32], change: Change) -> Result<()> {
    devicelist::update(&ctx.paths.device_list(), me(ctx), |list| {
        list.sign(&ctx.identity, device, change);
    })?;
    changed(ctx);
    Ok(())
}

/// Renames this device among the user's devices. Whether anything
/// changed. (Its addresses aren't on the list: its links tell the other
/// devices where it is.)
pub(super) fn rename_me(ctx: &Ctx, name: &str) -> Result<bool> {
    devicelist::validate_name(name).map_err(anyhow::Error::msg)?;
    let this = this_device(ctx);
    if own_list(ctx).devices().get(&this).is_some_and(|d| d.name == name) {
        return Ok(false);
    }
    sign_list(ctx, this, Change::Renamed { name: name.to_string() })?;
    Ok(true)
}

/// Lets `key`, one of the user's devices, relay for the others, or
/// stops it. Whether anything changed.
pub(super) fn allow_relay(ctx: &Ctx, key: [u8; 32], allowed: bool) -> Result<bool> {
    let devices = own_list(ctx).devices();
    let Some(device) = devices.get(&key) else { bail!("that isn't one of your devices") };
    if devices.len() < 2 {
        bail!("relaying is for when you have more than one device");
    }
    if device.relay == allowed {
        return Ok(false);
    }
    sign_list(ctx, key, Change::Relay { allowed })?;
    Ok(true)
}

/// The address the person typed for `key`, another of the user's
/// devices (see `typedaddress`), for when it couldn't tell this one
/// itself (both moved to a new network, say), or a host name to look up
/// at each dial: `text` is one or more IPs or host names, each with a
/// port if it isn't the usual one, separated by commas; empty clears it.
/// Kept in place of what was typed before, dialled with where it's been
/// from then on, and tried straight away. The items, as kept.
pub(super) fn set_address(ctx: &Ctx, key: [u8; 32], text: &str) -> Result<Vec<String>> {
    if key == this_device(ctx) {
        bail!("that's this device: where it can be reached is set under You, as your manually set IP address");
    }
    if !is_mine(ctx, &key) {
        bail!("that isn't one of your devices");
    }
    let kept = crate::myaddress::check(text).map_err(|e| anyhow::anyhow!(e))?;
    crate::typedaddress::set(&ctx.paths.typed_addresses(), &key, &kept)?;
    changed(ctx);
    Ok(kept)
}

/// Takes `key`, another of the user's devices, off their list, for good.
/// Its link closes once it's been sent the list, so it learns and wipes
/// itself (`taken_off`); the user's peers are sent the removal and
/// refuse it from then on (`removals`, `peerdevices::refuse`).
pub(super) fn remove(ctx: &Ctx, key: [u8; 32]) -> Result<()> {
    if !own_list(ctx).devices().contains_key(&key) {
        bail!("that isn't one of your devices");
    }
    if key == this_device(ctx) {
        bail!("a device can't take itself off: another of yours does");
    }
    sign_list(ctx, key, Change::Removed)?;
    Ok(())
}

/// Sets this device to dial no one and only take connections made to it,
/// or to dial again: its own setting, kept here and told to no one (the
/// others still dial it). Whether anything changed.
pub(super) fn set_receive_only(ctx: &Ctx, on: bool) -> Result<bool> {
    if receiving_only(ctx) == on {
        return Ok(false);
    }
    crate::settings::set_receive_only(&ctx.paths.settings(), on)?;
    sync_receiving_only(ctx);
    changed(ctx);
    Ok(true)
}

/// Whether this device is set to receive connections only.
pub(super) fn receiving_only(ctx: &Ctx) -> bool {
    crate::settings::receive_only(&ctx.paths.settings())
}

/// Whether this device dials no one now: set to receive connections only,
/// its screen off and no call under way (`dialling::holds_back`; the
/// setting as last synced, cheap enough to ask before each dial).
pub(super) fn dials_nobody(ctx: &Ctx) -> bool {
    ctx.endpoint.local_addr().is_ok_and(|at| super::dialling::holds_back(at.port()))
}

/// Has every dial refused or allowed as the setting says now (see
/// `dialling::set_receiving_only`), and says which.
pub(super) fn sync_receiving_only(ctx: &Ctx) -> bool {
    let on = receiving_only(ctx);
    if let Ok(at) = ctx.endpoint.local_addr() {
        super::dialling::set_receiving_only(at.port(), on);
    }
    on
}

/// Whether this device has been let relay for the user's others (see
/// `proxy`), as the list has it.
pub(super) fn relaying_here(ctx: &Ctx) -> bool {
    own_list(ctx).devices().get(&this_device(ctx)).is_some_and(|d| d.relay)
}

/// The removals on the user's list, for their peers: each signed with
/// the user key, so a peer can check it, and refuse that device.
pub(super) fn removals(ctx: &Ctx) -> Vec<devicelist::Entry> {
    own_list(ctx).entries.into_iter().filter(|e| e.change == Change::Removed).collect()
}

/// This device has been taken off the user's devices, by `who`: the node
/// stops, and everything in its folder goes — the user key, the vault,
/// the lot. Once.
fn taken_off(ctx: &Arc<Ctx>, who: &str) {
    if ctx.wiped.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    news(ctx, format!("{who} took this device off your devices: everything of yours on it is wiped"));
    let ctx = ctx.clone();
    tokio::spawn(async move {
        super::NodeHandle { ctx: ctx.clone() }.shutdown().await;
        match wipe(ctx.paths.dir()) {
            Ok(()) => log!(ctx, "wiped {}", ctx.paths.dir().display()),
            Err(e) => log!(ctx, "couldn't wipe {}: {e}", ctx.paths.dir().display()),
        }
    });
}

/// Deletes everything in `dir`, a device's folder, its vault closed first
/// so nothing's written there meanwhile. Never the root or the home
/// folder, whatever the folder's been set to.
fn wipe(dir: &std::path::Path) -> std::io::Result<()> {
    let dir = dir.canonicalize()?;
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).and_then(|h| std::path::PathBuf::from(h).canonicalize().ok());
    if dir.parent().is_none() || home.as_ref() == Some(&dir) {
        return Err(std::io::Error::other("that folder is too wide to wipe"));
    }
    crate::store::close_vault(&dir);
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Entries of the list from another of the user's devices (`who`), taken
/// in. Whether this device is still on it (if not, it's wiped).
fn take_list(ctx: &Arc<Ctx>, who: &str, entries: &[devicelist::Entry]) -> bool {
    match devicelist::merge_file(&ctx.paths.device_list(), me(ctx), entries) {
        Ok(new) if !new.is_empty() => {
            log!(ctx, "{} change(s) to your devices from {who}", new.len());
            changed(ctx);
        }
        Ok(_) => {}
        Err(e) => log!(ctx, "couldn't save your devices' list from {who}: {e}"),
    }
    if own_list(ctx).removed(&this_device(ctx)) {
        taken_off(ctx, who);
        return false;
    }
    true
}

/// A copy of a message this device just sent to (`sent`) or received
/// from peer `peer`, for the user's other devices. Kept for each of them
/// (`carbons/<device>`) until it's been sent, so one that's off gets it
/// when it's back.
pub(super) fn carbon(ctx: &Ctx, peer: [u8; 32], id: crate::outbox::MessageId, sent: bool, at: u64, expires: u64, text: &str) {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let others = others(ctx);
    if others.is_empty() || peer == me(ctx) {
        return;
    }
    let line = format!("{}\t{}\t{}\t{at}\t{expires}\t{}", STANDARD.encode(peer), crate::outbox::format_id(&id), if sent { ">" } else { "<" }, crate::text::single_line(text));
    let guard = crate::store::lock();
    for device in others {
        let _ = crate::store::append_line(&carbon_queue(ctx, &device), &line);
    }
    drop(guard);
    let _ = ctx.carbon_tx.send(());
}

/// A copy of a message from another of the user's devices (`who`), or
/// word of one delivered, for `device`, which it has no link to: kept
/// here for that device, on the same queue as this one's own copies, and
/// sent on from there.
fn carbon_for(ctx: &Ctx, who: &str, device: &[u8; 32], carbon: Notice) {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    if !relaying_here(ctx) || !is_mine(ctx, device) {
        log!(ctx, "your device {who} sent a copy of a message to pass on, which this device can't");
        return;
    }
    let line = match carbon {
        Notice::Carbon { peer, id, sent, at, expires, text } => {
            format!("{}\t{}\t{}\t{at}\t{expires}\t{}", STANDARD.encode(peer), crate::outbox::format_id(&id), if sent { ">" } else { "<" }, crate::text::single_line(&text))
        }
        Notice::Delivered { peer, id } => delivered_line(&peer, &id),
        Notice::Unsaid { peer, line, done } => unsaid_line(&peer, &line, done),
        _ => return,
    };
    let guard = crate::store::lock();
    let _ = crate::store::append_line(&carbon_queue(ctx, device), &line);
    drop(guard);
    let _ = ctx.carbon_tx.send(());
}

/// A carbon queue's line saying message `id` to `peer` was delivered:
/// `<peer>\t<id>\t=`, the peer first, as on every line (`crate::forget`).
fn delivered_line(peer: &[u8; 32], id: &crate::outbox::MessageId) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    format!("{}\t{}\t=", STANDARD.encode(peer), crate::outbox::format_id(id))
}

/// A carbon queue's line saying `line` of the conversation with `peer` is
/// deleted (`Notice::Unsaid`): `<peer>\t<id>\tx\t<done>\t<line>`, the id
/// there only to keep to every line's shape.
fn unsaid_line(peer: &[u8; 32], line: &crate::history::LineRef, done: bool) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    format!("{}\t{}\tx\t{}\t{}", STANDARD.encode(peer), crate::outbox::format_id(&line.id.unwrap_or_default()), u8::from(done), line.encode())
}

/// Tells the user's other devices `line` of the conversation with `peer`
/// is deleted (see `Notice::Unsaid`), queued as copies of messages are.
pub(super) fn unsaid(ctx: &Ctx, peer: [u8; 32], line: &crate::history::LineRef, done: bool) {
    if peer == me(ctx) {
        return;
    }
    let others = others(ctx);
    let text = unsaid_line(&peer, line, done);
    let guard = crate::store::lock();
    for device in &others {
        let _ = crate::store::append_line(&carbon_queue(ctx, device), &text);
    }
    drop(guard);
    if !others.is_empty() {
        let _ = ctx.carbon_tx.send(());
    }
}

/// Message `id`, sent to peer `peer` (their user key), has reached one of
/// their devices, acknowledged here: it isn't waiting any more, here or
/// on the user's other devices, which are told (see `crate::waiting`).
pub(super) fn delivered(ctx: &Ctx, peer: [u8; 32], id: crate::outbox::MessageId) {
    no_longer_waiting(ctx, peer, id);
    if peer == me(ctx) {
        return;
    }
    let line = delivered_line(&peer, &id);
    let others = others(ctx);
    let guard = crate::store::lock();
    for device in &others {
        let _ = crate::store::append_line(&carbon_queue(ctx, device), &line);
    }
    drop(guard);
    if !others.is_empty() {
        let _ = ctx.carbon_tx.send(());
    }
}

/// Message `id` to `peer` isn't waiting any more, and is remembered a
/// while as delivered, for a copy of it from another device that comes
/// after the word that it was delivered.
fn no_longer_waiting(ctx: &Ctx, peer: [u8; 32], id: crate::outbox::MessageId) {
    let _ = crate::waiting::remove(&ctx.paths.waiting(), &peer, &id);
    let mut recent = ctx.delivered.locked();
    if !recent.contains(&id) {
        if recent.len() >= 1000 {
            recent.pop_front();
        }
        recent.push_back(id);
    }
    drop(recent);
    ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
}

fn carbon_queue(ctx: &Ctx, device: &[u8; 32]) -> std::path::PathBuf {
    crate::peerstore::file_for(&ctx.paths.dir().join("carbons"), device)
}

/// The copies waiting for `device`, as notices, and the lines of the
/// queue they are.
fn queued_carbons(ctx: &Ctx, device: &[u8; 32]) -> (Vec<Notice>, Vec<String>) {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let text = crate::store::read(carbon_queue(ctx, device)).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let notices = lines
        .iter()
        .filter_map(|line| {
            let mut f = line.splitn(6, '\t');
            let peer: [u8; 32] = STANDARD.decode(f.next()?).ok()?.try_into().ok()?;
            let id = crate::outbox::parse_id(f.next()?)?;
            let way = f.next()?;
            if way == "=" {
                return Some(Notice::Delivered { peer, id });
            }
            if way == "x" {
                let done = f.next()? == "1";
                let line = crate::history::LineRef::decode(f.next()?)?;
                return Some(Notice::Unsaid { peer, line, done });
            }
            let sent = way == ">";
            let at = f.next()?.parse().ok()?;
            let expires = f.next()?.parse().ok()?;
            Some(Notice::Carbon { peer, id, sent, at, expires, text: f.next()?.to_string() })
        })
        .collect();
    (notices, lines)
}

/// Takes `sent` off `device`'s queue. By what they say, not where they
/// were: lines with a contact deleted meanwhile may have gone already
/// (`crate::forget`).
fn drop_carbons(ctx: &Ctx, device: &[u8; 32], sent: Vec<String>) {
    let _guard = crate::store::lock();
    let path = carbon_queue(ctx, device);
    let text = crate::store::read(&path).unwrap_or_default();
    let mut sent: HashMap<String, usize> = sent.into_iter().fold(HashMap::new(), |mut m, l| {
        *m.entry(l).or_default() += 1;
        m
    });
    let rest: String = text
        .lines()
        .filter(|l| match sent.get_mut(*l) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .map(|l| format!("{l}\n"))
        .collect();
    let _ = if rest.is_empty() { std::fs::remove_file(&path) } else { crate::store::write_atomic(&path, rest) };
}

/// A copy from another of our devices: into that conversation, unless
/// it's there already. A message they sent us is counted as received
/// here (`seen`), so when their own copy for this device arrives it's
/// acknowledged but not shown again.
fn on_carbon(ctx: &Ctx, peer: [u8; 32], id: crate::outbox::MessageId, sent: bool, at: u64, expires: u64, text: &str) {
    let _forgetting = ctx.forgetting.locked();
    if super::ownpeers::is_gone(ctx, &peer) || super::just_deleted(ctx, &peer) {
        return;
    }
    if sent {
        if !crate::seen::first_time(&ctx.paths.seen(), &peer, &id).unwrap_or(true) {
            return;
        }
        let _ = crate::history::append_new(&ctx.paths.history(), &peer, crate::history::Direction::Sent, text, at, expires, Some(id));
        if !ctx.delivered.locked().contains(&id) {
            let _ = crate::waiting::add(&ctx.paths.waiting(), &peer, id, at, expires, text);
        }
    } else if crate::seen::first_time(&ctx.paths.seen(), &peer, &id).unwrap_or(true) {
        let _ = crate::history::append_new(&ctx.paths.history(), &peer, crate::history::Direction::Received, text, at, expires, Some(id));
    }
    ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
}

/// The settings key saying this device's peers and chats have gone to
/// `device` (see `Duty::Bundle`).
fn bundle_sent_key(device: &[u8; 32]) -> String {
    format!("device_bundle_sent.{}", crate::peerstore::file_name(device))
}

/// Takes in another of our devices' peers and chats (`crate::bundle`),
/// keeping the node's peers in step; says what came. Not its name and
/// picture: the newer of the two is kept, as `ownlook` does.
fn take_bundle(ctx: &Arc<Ctx>, bundle: &str) -> String {
    let wanted: String = bundle.lines().filter(|l| !l.starts_with("name\t") && !l.starts_with("profile\t")).map(|l| format!("{l}\n")).collect();
    let _forgetting = ctx.forgetting.locked();
    let merged = match crate::bundle::merge(&ctx.paths, &wanted, &[me(ctx)], &|k| super::ownpeers::has_line(ctx, k), &ctx.identity) {
        Ok(merged) => merged,
        Err(e) => return format!("couldn't take in its contacts and chats: {e}"),
    };
    let handle = super::NodeHandle { ctx: ctx.clone() };
    for peer in &merged.peers_added {
        handle.start_peer(peer.clone());
    }
    super::ownpeers::tidy(ctx);
    ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
    format!("{} contact(s), {} conversation(s) with messages", merged.peers_added.len(), merged.conversations)
}

/// Where another of our devices has been: where it last said it is,
/// newest first.
fn addresses_of(ctx: &Ctx, device: &[u8; 32]) -> Vec<std::net::SocketAddr> {
    let learned = crate::addressbook::load(&ctx.paths.addresses(), device);
    let scopes = ctx.link_scopes.locked().clone();
    crate::addressbook::dialable(&learned, &scopes)
}

/// Where to dial another of our devices: where it's been
/// (`addresses_of`), then what the user typed for it, host names looked
/// up now.
async fn dial_list(ctx: &Ctx, device: &[u8; 32], learned: Vec<std::net::SocketAddr>, typed: &str) -> Vec<std::net::SocketAddr> {
    let mut all = learned;
    if !typed.is_empty() {
        for addr in crate::peers::resolve_address(typed).await.unwrap_or_default() {
            if !all.contains(&addr) {
                all.push(addr);
            }
        }
    }
    ctx.answered.locked().leaving_out(device, all)
}

/// Keeps a link to each of the user's other devices, for as long as the
/// node runs (and this device is still one of theirs).
pub(super) async fn keep_siblings(ctx: Arc<Ctx>) {
    let mut updates = ctx.device_updates.subscribe();
    let failed: Arc<Failed> = Arc::default();
    let dialling: Arc<std::sync::Mutex<std::collections::HashSet<[u8; 32]>>> = Arc::default();
    let mut moved = ctx.my_addresses.subscribe();
    let mut moved_at: Option<Instant> = None;
    let mut screen = super::dialling::screen_changes();
    let mut was_receiving_only = false;
    while !ctx.stopped.load(std::sync::atomic::Ordering::Relaxed) {
        let removed = own_list(&ctx).removed(&this_device(&ctx));
        let lit = super::dialling::screen_on();
        let receiving_only = sync_receiving_only(&ctx);
        if receiving_only != was_receiving_only {
            log!(ctx, "{}", if receiving_only { "this device receives connections only: it dials no one while its screen's off" } else { "this device dials again" });
            was_receiving_only = receiving_only;
            changed(&ctx);
        }
        let holding_back = receiving_only && dials_nobody(&ctx);
        for key in others(&ctx).into_iter().filter(|_| !removed && !holding_back) {
            let held = ctx.device_calls.locked().get(&key).is_some_and(|c| c.carry.try_lock().is_ok_and(|carry| carry.holding())) || super::proxycall::held_for(&ctx, &key);
            let waited_out = |&(at, times): &(Instant, u32)| {
                let retry = if moved_at.is_some_and(|at| at.elapsed() < MOVING) {
                    RETRY_AFTER_MOVING
                } else if lit {
                    RETRY_AFTER
                } else {
                    super::dialling::dark_wait(times, RETRY_AFTER, RETRY_AFTER)
                };
                at.elapsed() >= retry
            };
            if ctx.siblings.locked().contains_key(&key) || dialling.locked().contains(&key) || (!held && failed.locked().get(&key).is_some_and(|f| !waited_out(f))) {
                continue;
            }
            let learned = addresses_of(&ctx, &key);
            let typed = crate::typedaddress::load(&ctx.paths.typed_addresses(), &key);
            if learned.is_empty() && typed.is_empty() {
                continue;
            }
            dialling.locked().insert(key);
            let (ctx, failed, dialling) = (ctx.clone(), failed.clone(), dialling.clone());
            let here = ctx.my_addresses.borrow().clone();
            tokio::spawn(async move {
                let addresses = dial_list(&ctx, &key, learned.clone(), &typed).await;
                if addresses.is_empty() {
                    if !typed.is_empty() {
                        note_failed(&failed, key);
                    }
                    dialling.locked().remove(&key);
                    return;
                }
                match super::peer::dial_any(&ctx.endpoint, &addresses, ctx.keys(), me(&ctx)).await {
                    Ok(conn) => {
                        failed.locked().remove(&key);
                        ctx.answered.locked().note(conn.connection.remote_address(), conn.remote_device);
                        start(&ctx, conn, true);
                    }
                    Err(e) => {
                        log!(ctx, "your device {}: can't connect — {e:#}", label(&ctx, &key));
                        let same = addresses_of(&ctx, &key) == learned && crate::typedaddress::load(&ctx.paths.typed_addresses(), &key) == typed;
                        if same && *ctx.my_addresses.borrow() == here {
                            note_failed(&failed, key);
                        } else if !same {
                            dialling.locked().remove(&key);
                            changed(&ctx);
                        }
                    }
                }
                dialling.locked().remove(&key);
            });
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        tokio::select! {
            Ok(()) = updates.changed() => failed.locked().clear(),
            Ok(()) = moved.changed() => {
                failed.locked().clear();
                moved_at = Some(Instant::now());
            }
            Ok(()) = screen.changed() => {}
            _ = tokio::time::sleep(if super::proxycall::any_held(&ctx) || moved_at.is_some_and(|at| at.elapsed() < MOVING) { Duration::ZERO } else { RECHECK }) => {}
        }
    }
}

/// Each device whose last dial wasn't answered: when, and how many in a
/// row.
type Failed = std::sync::Mutex<HashMap<[u8; 32], (Instant, u32)>>;

/// A dial to `key` wasn't answered: when, and once more in a row.
fn note_failed(failed: &Failed, key: [u8; 32]) {
    let mut failed = failed.locked();
    let times = failed.get(&key).map_or(0, |(_, times)| *times);
    failed.insert(key, (Instant::now(), times.saturating_add(1)));
}

/// A connection from a device holding the user key.
pub(super) fn accept(ctx: &Arc<Ctx>, conn: AuthenticatedConnection) {
    start(ctx, conn, false);
}

/// Registers a link to the device at the other end of `conn` and runs it,
/// unless that's this device, one taken off the list, or there's a link
/// already that should be kept instead.
fn start(ctx: &Arc<Ctx>, conn: AuthenticatedConnection, dialed_by_us: bool) {
    let remote = conn.remote_device;
    let this = this_device(ctx);
    if remote == this {
        conn.connection.close(0u32.into(), b"that's me");
        return;
    }
    if own_list(ctx).removed(&this) {
        conn.connection.close(0u32.into(), b"removed");
        return;
    }
    if own_list(ctx).removed(&remote) {
        log!(ctx, "{} connected, though it's been taken off your devices: telling it", label(ctx, &remote));
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut conn = conn;
            let _ = send_list(&ctx, &mut conn, &mut Vec::new()).await;
            tell_and_close(conn).await;
        });
        return;
    }
    let id = ctx.next_connection_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let (stop_tx, stop_rx) = watch::channel(false);
    {
        let mut siblings = ctx.siblings.locked();
        if let Some(existing) = siblings.get(&remote) {
            let in_call = ctx.device_calls.locked().get(&remote).is_some_and(|c| c.in_call.load(Ordering::Relaxed)) || ctx.links_in_calls.locked().contains(&remote);
            let existing_wins = existing.dialed_by_us == super::dialling::keeps_own_dial(&this, &remote) && existing.dialed_by_us != dialed_by_us && !in_call;
            if existing_wins {
                conn.connection.close(0u32.into(), b"have one");
                return;
            }
            let _ = existing.stop.send(true);
        }
        ctx.connected(remote, conn.connection.clone());
        siblings.insert(remote, SiblingEntry { id, dialed_by_us, stop: stop_tx });
    }
    changed(ctx);
    tokio::spawn(run(ctx.clone(), conn, remote, id, dialed_by_us, stop_rx));
}

/// What a link to another of our own devices carries besides the list
/// and addresses: copies of messages, each other's posts (every so often:
/// they're written straight to the file, so there's nothing to be told
/// of), the user's
/// peers, and once, each one's peers and chats.
struct Duties {
    ctx: Arc<Ctx>,
    remote: [u8; 32],
    who: String,
    carbons: tokio::sync::broadcast::Receiver<()>,
    flush_now: bool,
    posts_sync: tokio::time::Interval,
    posts_asked: Option<(protocol::RequestId, crate::posts::Fetched)>,
    /// The newest post they've sent, by their clock: the next ask starts
    /// a little before it. Not by ours — theirs may be slow, and every
    /// post they make would then be older than what we asked for.
    posts_newest: u64,
    /// How often the user's peers book is looked at, and the last one
    /// sent, so it's sent again only when it's changed.
    peers_book_check: tokio::time::Interval,
    peers_book_sent: Option<Notice>,
    requests_sent: Option<Vec<Notice>>,
    withdrawals_sent: Option<Vec<Notice>>,
    /// How far each chat was read, as this link last had it.
    read_sent: HashMap<crate::readmarks::Chat, u64>,
    /// Their requests book so far, coming in parts.
    requests_in: Vec<crate::protocol::BookRequest>,
    /// Whether this device's peers and chats are still to go to them.
    bundle_owed: bool,
    /// Theirs so far, coming in parts.
    bundle_in: Vec<u8>,
}

/// What `Duties::next` woke for.
enum Duty {
    /// Send the copies waiting for that device.
    Carbons,
    Posts,
    PeersBook,
    Bundle,
}

impl Duties {
    fn new(ctx: &Arc<Ctx>, remote: [u8; 32]) -> Duties {
        let bundle_owed = crate::settings::get(&ctx.paths.settings(), &bundle_sent_key(&remote)).is_none();
        Duties {
            ctx: ctx.clone(),
            remote,
            who: label(ctx, &remote),
            carbons: ctx.carbon_tx.subscribe(),
            flush_now: true,
            posts_sync: tokio::time::interval(POSTS_SYNC),
            posts_asked: None,
            posts_newest: 0,
            peers_book_check: tokio::time::interval(PEERS_BOOK_CHECK),
            peers_book_sent: None,
            requests_sent: None,
            withdrawals_sent: None,
            read_sent: HashMap::new(),
            requests_in: Vec::new(),
            bundle_owed,
            bundle_in: Vec::new(),
        }
    }

    /// The next thing to do. Cancel-safe.
    async fn next(&mut self) -> Duty {
        loop {
            tokio::select! {
                _ = std::future::ready(()), if self.bundle_owed => {
                    self.bundle_owed = false;
                    return Duty::Bundle;
                }
                _ = std::future::ready(()), if self.flush_now => {
                    self.flush_now = false;
                    return Duty::Carbons;
                }
                carbon = self.carbons.recv() => match carbon {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return Duty::Carbons,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
                },
                _ = self.posts_sync.tick(), if self.posts_asked.is_none() => return Duty::Posts,
                _ = self.peers_book_check.tick() => return Duty::PeersBook,
            }
        }
    }

    async fn act(&mut self, conn: &mut AuthenticatedConnection, duty: Duty) -> Result<()> {
        match duty {
            Duty::Bundle => {
                for notice in protocol::bundle_notices(crate::bundle::build(&self.ctx.paths, 0).as_bytes()) {
                    protocol::send_notice(conn, notice).await?;
                }
                let _ = crate::settings::set(&self.ctx.paths.settings(), &bundle_sent_key(&self.remote), "yes");
                Ok(())
            }
            Duty::Carbons => {
                let (notices, lines) = queued_carbons(&self.ctx, &self.remote);
                for notice in notices {
                    protocol::send_notice(conn, notice).await?;
                }
                drop_carbons(&self.ctx, &self.remote, lines);
                for device in others(&self.ctx).into_iter().filter(|d| *d != self.remote && reached_through(&self.ctx, d) == Some(self.remote)) {
                    let (notices, lines) = queued_carbons(&self.ctx, &device);
                    for notice in notices {
                        protocol::send_notice(conn, Notice::CarbonFor { device, carbon: Box::new(notice) }).await?;
                    }
                    drop_carbons(&self.ctx, &device, lines);
                }
                Ok(())
            }
            Duty::Posts => {
                let since = self.posts_newest.saturating_sub(POSTS_SYNC.as_secs() * 2);
                let id = protocol::request(conn, Request::ListPosts { since }).await?;
                self.posts_asked = Some((id, crate::posts::Fetched::default()));
                Ok(())
            }
            Duty::PeersBook => {
                let Some(book) = super::ownpeers::notice(&self.ctx) else { return Ok(()) };
                if self.peers_book_sent.as_ref() != Some(&book) {
                    protocol::send_notice(conn, book.clone()).await?;
                    self.peers_book_sent = Some(book);
                }
                if let Some(requests) = super::ownrequests::notices(&self.ctx)
                    && self.requests_sent.as_ref() != Some(&requests)
                {
                    for part in &requests {
                        protocol::send_notice(conn, part.clone()).await?;
                    }
                    self.requests_sent = Some(requests);
                }
                if let Some(withdrawals) = super::withdraw::book(&self.ctx)
                    && self.withdrawals_sent.as_ref() != Some(&withdrawals)
                {
                    for part in &withdrawals {
                        protocol::send_notice(conn, part.clone()).await?;
                    }
                    self.withdrawals_sent = Some(withdrawals);
                }
                for notice in super::ownread::changes(&self.ctx, &mut self.read_sent) {
                    protocol::send_notice(conn, notice).await?;
                }
                Ok(())
            }
        }
    }

    /// Takes a reply that's for these duties (their posts); hands any
    /// other back.
    fn on_reply(&mut self, rid: protocol::RequestId, reply: Reply) -> Option<Reply> {
        let ctx = &self.ctx;
        let who = &self.who;
        match reply {
            Reply::Posts { more, posts, deleted } if self.posts_asked.as_ref().is_some_and(|(id, _)| *id == rid) => {
                let (_, so_far) = self.posts_asked.as_mut().expect("checked");
                so_far.posts.extend(posts);
                so_far.deleted.extend(deleted);
                if !more {
                    let (_, fetched) = self.posts_asked.take().expect("checked");
                    let me = me(ctx);
                    if let Ok(n) = crate::posts::take_deletions(&ctx.paths.posts(), &fetched.deleted, &[me], super::unix_now())
                        && n > 0
                    {
                        log!(ctx, "{n} post(s) deleted on your device {who}");
                    }
                    let got = fetched.posts;
                    let believable = super::unix_now().saturating_add(crate::postcache::CLOCK_SLACK);
                    self.posts_newest = got.iter().filter(|p| p.created_at <= believable && crate::posts::verify(p, &me)).map(|p| p.created_at).fold(self.posts_newest, u64::max);
                    if let Ok(n) = crate::posts::merge(&ctx.paths.posts(), &got, &[me])
                        && n > 0 {
                            log!(ctx, "{n} post(s) from your device {who}");
                        }
                }
                None
            }
            other => Some(other),
        }
    }

    /// Takes a notice that's for these duties (a copy of a message, the
    /// user's peers, their peers and chats); hands any other back.
    fn on_notice(&mut self, notice: Notice) -> Result<Option<Notice>> {
        let ctx = self.ctx.clone();
        let who = &self.who;
        match notice {
            Notice::Carbon { peer, id, sent, at, expires, text } => {
                on_carbon(&ctx, peer, id, sent, at, expires, &text);
                Ok(None)
            }
            Notice::CarbonFor { device, carbon } => {
                carbon_for(&ctx, who, &device, *carbon);
                Ok(None)
            }
            Notice::Delivered { peer, id } => {
                no_longer_waiting(&ctx, peer, id);
                Ok(None)
            }
            Notice::Unsaid { peer, line, done } => {
                super::unsay::from_another_device(&ctx, peer, line, done);
                Ok(None)
            }
            Notice::PeersBook { entries } => {
                super::ownpeers::merge(&ctx, self.remote, entries);
                Ok(None)
            }
            Notice::WithdrawalsBook { entries } => {
                super::withdraw::merge_book(&ctx, entries);
                Ok(None)
            }
            Notice::ReadMarks { entries } => {
                self.read_sent.extend(entries.iter().cloned());
                super::ownread::merge(&ctx, entries);
                Ok(None)
            }
            Notice::RequestsBook { entries, more } => {
                self.requests_in.extend(entries);
                if self.requests_in.len() > 100_000 {
                    self.requests_in.clear();
                } else if !more {
                    super::ownrequests::merge(&ctx, self.remote, std::mem::take(&mut self.requests_in));
                }
                Ok(None)
            }
            Notice::Bundle { more, bytes } => {
                self.bundle_in.extend_from_slice(&bytes);
                if self.bundle_in.len() > crate::bundle::MAX_BYTES {
                    self.bundle_in.clear();
                    log!(ctx, "your device {who} sent more contacts and chats than will be taken");
                } else if !more {
                    let bundle = std::mem::take(&mut self.bundle_in);
                    let said = take_bundle(&ctx, &String::from_utf8_lossy(&bundle));
                    log!(ctx, "from your device {who}: {said}");
                }
                Ok(None)
            }
            other => Ok(Some(other)),
        }
    }
}

async fn run(ctx: Arc<Ctx>, mut conn: AuthenticatedConnection, remote: [u8; 32], id: u64, dialed_by_us: bool, mut stop: watch::Receiver<bool>) {
    let who = label(&ctx, &remote);
    log!(ctx, "link to your device {who}: up");
    ctx.note_seen_as(&remote, conn.seen_as);
    if !dialed_by_us {
        let _ = crate::addressbook::see(&ctx.paths.addresses(), &remote, &crate::addressbook::came_from(conn.connection.remote_address()));
    }
    let cover = ctx.default_cover;
    let mut cover_ticker = cover.map(|bps| {
        protocol::enable_cover(&mut conn, bps);
        super::peer::cover_slot_ticker()
    });
    let mut updates = ctx.device_updates.subscribe();
    let mut my_addresses = ctx.my_addresses.subscribe();
    let mut manual = ctx.manual_address.subscribe();
    let mut discoverable = ctx.discoverable.subscribe();
    let mut duties = Duties::new(&ctx, remote);
    let mut proxy = super::proxy::Proxy::new(&ctx, remote);
    let mut through = super::proxycall::LinkCalls::new(&ctx, remote);
    let mut files = super::proxyfile::LinkFiles::new(&ctx, remote);
    let mut work = super::channels::ChannelWork::new(ctx.clone(), me(&ctx));
    let mut look = super::ownlook::Look::new(&ctx, me(&ctx));
    let mut list_sent: Vec<devicelist::Entry> = Vec::new();
    let name = call_name(&remote);
    let (_, carry, in_call) = device_call(&ctx, &remote);
    let mut carry = tokio::select! {
        carry = carry.lock_owned() => carry,
        _ = stop.changed() => {
            log!(ctx, "link to your device {who}: closed (replaced or no longer needed)");
            return;
        }
    };
    let mut call: CallOn = carry.begin(false, &conn);
    let mut moved_at: Option<Instant> = None;
    let reason: String = 'link: {
        let addresses = my_addresses.borrow_and_update().clone();
        if let Err(e) = protocol::send_notice(&mut conn, Notice::Addresses(addresses)).await {
            break 'link e.to_string();
        }
        if let Err(e) = send_list(&ctx, &mut conn, &mut list_sent).await {
            break 'link e.to_string();
        }
        if let Err(e) = work.start(&mut conn).await {
            break 'link e.to_string();
        }
        if let Err(e) = look.ask(&mut conn).await {
            break 'link e.to_string();
        }
        let kept = manual.borrow_and_update().clone();
        if kept.at > 0
            && let Err(e) = protocol::send_notice(&mut conn, Notice::ManualAddress { at: kept.at, items: kept.items }).await
        {
            break 'link e.to_string();
        }
        let kept = *discoverable.borrow_and_update();
        if kept.at > 0
            && let Err(e) = protocol::send_notice(&mut conn, Notice::Discoverable { at: kept.at, on: kept.on }).await
        {
            break 'link e.to_string();
        }
        call.settle(&mut conn).await;
        let mut heartbeat = tokio::time::interval(protocol::HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut housekeeping = tokio::time::interval(Duration::from_secs(1));
        let mut tell_devices = tokio::time::interval(DEVICE_ADDRESSES_EVERY);
        let mut addresses_told: Option<Vec<protocol::DeviceAddress>> = None;
        loop {
            let outcome: Result<()> = tokio::select! {
                _ = super::peer::tick_if_some(&mut cover_ticker) => match protocol::emit_cover_slot(&mut conn).await {
                    Ok(()) => {
                        protocol::update_call_budget(&conn, &carry.handle.budget);
                        Ok(())
                    }
                    e => e,
                },
                Some(command) = carry.commands.recv() => call.on_command(&mut carry, &mut conn, &ctx, &name, command, None).await.map(|_| ()),
                _ = stop.changed() => break 'link "replaced or no longer needed".to_string(),
                Ok(()) = updates.changed() => {
                    let sent = send_list(&ctx, &mut conn, &mut list_sent).await;
                    if own_list(&ctx).removed(&remote) {
                        break 'link "taken off your devices".to_string();
                    }
                    match sent {
                        Ok(()) => tell_addresses(&ctx, &mut conn, &remote, &mut addresses_told).await,
                        e => e,
                    }
                }
                Ok(()) = my_addresses.changed() => {
                    let addresses = my_addresses.borrow_and_update().clone();
                    if call.live() {
                        moved_at = Some(Instant::now());
                        if super::peer::stranded(&conn, dialed_by_us, &addresses) {
                            break 'link "this device moved, in a call".to_string();
                        }
                    }
                    protocol::send_notice(&mut conn, Notice::Addresses(addresses)).await
                }
                b = work.next() => work.on_broadcast(&mut conn, b).await,
                duty = duties.next() => duties.act(&mut conn, duty).await,
                _ = proxy.next() => proxy.act(&mut conn).await,
                wake = through.next() => through.on_wake(&mut conn, wake).await,
                wake = files.next() => files.on_wake(&mut conn, wake).await,
                Ok(()) = look.mine.changed() => look.tell(&mut conn).await,
                Ok(()) = manual.changed() => {
                    let kept = manual.borrow_and_update().clone();
                    protocol::send_notice(&mut conn, Notice::ManualAddress { at: kept.at, items: kept.items }).await
                }
                Ok(()) = discoverable.changed() => {
                    let kept = *discoverable.borrow_and_update();
                    protocol::send_notice(&mut conn, Notice::Discoverable { at: kept.at, on: kept.on }).await
                }
                _ = heartbeat.tick() => protocol::request(&mut conn, Request::Ping).await.map(|_| ()),
                _ = tell_devices.tick() => tell_addresses(&ctx, &mut conn, &remote, &mut addresses_told).await,
                _ = housekeeping.tick() => {
                    if conn.last_heard.elapsed() > protocol::LIVENESS_TIMEOUT {
                        break 'link format!("nothing heard for {}s", protocol::LIVENESS_TIMEOUT.as_secs());
                    }
                    if call.live() && super::peer::lost_with_move(carry.audio_at, moved_at, conn.last_heard) {
                        break 'link "this device moved, and nothing's heard on the old way".to_string();
                    }
                    call.housekeeping(&carry, &mut conn, &ctx, &name).await;
                    in_call.store(call.live(), Ordering::Relaxed);
                    work.housekeeping();
                    Ok(())
                }
                incoming = protocol::recv(&mut conn) => match protocol::flush(&mut conn).await.and(incoming) {
                    Err(e) => Err(e),
                    Ok(Incoming::Notice(Notice::DeviceList { entries })) => {
                        if !take_list(&ctx, &who, &entries) {
                            break 'link "this device was taken off".to_string();
                        }
                        Ok(())
                    }
                    Ok(Incoming::Notice(Notice::Addresses(list))) => {
                        if let Ok(true) = crate::addressbook::hear(&ctx.paths.addresses(), &remote, &list, super::unix_now()) {
                            log!(ctx, "your device {who} can be reached at {} now", list.join(", "));
                            changed(&ctx);
                        }
                        Ok(())
                    }
                    Ok(Incoming::Notice(Notice::DeviceAddresses(devices))) => {
                        take_device_addresses(&ctx, &who, &remote, &devices);
                        Ok(())
                    }
                    Ok(Incoming::Notice(Notice::ManualAddress { at, items })) => {
                        let theirs = crate::myaddress::Manual { at, items };
                        if let Ok(true) = crate::myaddress::take(&ctx.paths.manual_address(), &theirs) {
                            let kept = crate::myaddress::load(&ctx.paths.manual_address());
                            log!(ctx, "your manually set address is {} now: set on your device {who}", if kept.items.is_empty() { "none".to_string() } else { kept.text() });
                            ctx.manual_address.send_replace(kept);
                            ctx.device_updates.send_modify(|n| *n = n.wrapping_add(1));
                        }
                        Ok(())
                    }
                    Ok(Incoming::Notice(Notice::Discoverable { at, on })) => {
                        let theirs = crate::discoverable::Discoverable { at, on };
                        if let Ok(true) = crate::discoverable::take(&ctx.paths.discoverable(), theirs) {
                            log!(ctx, "you're {} on the network each device is on now: set on your device {who}", if on { "discoverable" } else { "not discoverable" });
                            ctx.discoverable.send_replace(theirs);
                            ctx.device_updates.send_modify(|n| *n = n.wrapping_add(1));
                        }
                        Ok(())
                    }
                    Ok(Incoming::Notice(Notice::Call(signal))) => call.on_signal(&mut carry, &mut conn, &ctx, &name, signal).await,
                    Ok(Incoming::Notice(Notice::CoverHello { requires: Some(required), .. })) => {
                        if protocol::raise_cover(&mut conn, required) {
                            cover_ticker = Some(super::peer::cover_slot_ticker());
                        }
                        Ok(())
                    }
                    Ok(Incoming::Notice(notice @ (Notice::Carbon { .. } | Notice::CarbonFor { .. } | Notice::Delivered { .. } | Notice::Unsaid { .. } | Notice::PeersBook { .. } | Notice::RequestsBook { .. } | Notice::WithdrawalsBook { .. } | Notice::ReadMarks { .. } | Notice::Bundle { .. }))) => {
                        duties.on_notice(notice).map(|_| ())
                    }
                    Ok(Incoming::Notice(Notice::ProfileChanged)) => look.ask(&mut conn).await,
                    Ok(Incoming::Notice(Notice::Reaching(users))) => {
                        proxy.on_reaching(users);
                        duties.flush_now = true;
                        Ok(())
                    }
                    Ok(Incoming::Notice(Notice::ReachingDelays(delays))) => {
                        proxy.on_delays(delays);
                        Ok(())
                    }
                    Ok(Incoming::Notice(Notice::CallThrough { contact, signal })) => through.on_signal(&mut conn, contact, signal).await,
                    Ok(Incoming::Notice(Notice::CallRoom { contact, audio, video_bps, video_piece, video_on, bigger, relayed, covered })) => {
                        through.on_room(contact, (audio, video_bps, video_piece), covered, video_on, bigger, relayed);
                        Ok(())
                    }
                    Ok(Incoming::Reply(rid, reply)) if proxy.owns(rid) => {
                        proxy.on_reply(rid, reply);
                        Ok(())
                    }
                    Ok(Incoming::Notice(Notice::FileThrough { contact, onward, notice })) => files.on_notice(&mut conn, contact, onward, *notice).await,
                    Ok(Incoming::Request(rid, Request::FileChunkThrough { contact, id, offset, len })) => files.on_chunk(&mut conn, rid, contact, id, offset, len).await,
                    Ok(Incoming::Reply(rid, reply)) if files.owns(rid) => files.on_reply(&mut conn, rid, reply).await,
                    Ok(Incoming::Request(rid, request @ Request::PassOn { .. })) => proxy.on_pass_on(&mut conn, rid, request).await,
                    Ok(Incoming::Request(rid, Request::ProfileThrough { contact, have, from })) => proxy.on_profile_through(&mut conn, rid, contact, have, from).await,
                    Ok(Incoming::Reply(rid, reply)) if proxy.owns_profile(rid) => proxy.on_profile(&mut conn, reply).await,
                    Ok(Incoming::Reply(rid, reply)) if look.owns(rid) => look.on_reply(&mut conn, rid, reply).await,
                    Ok(Incoming::Reply(rid, reply @ (Reply::Posts { .. } | Reply::ChannelData { .. } | Reply::ChannelError(_))))
                        if !work.owns(rid) =>
                    {
                        let _ = duties.on_reply(rid, reply);
                        Ok(())
                    }
                    Ok(Incoming::Notice(notice @ (Notice::ChannelPost { .. } | Notice::ChannelEvent { .. } | Notice::GroupIntro(_)))) => work.on_notice(&mut conn, notice).await,
                    Ok(Incoming::Reply(rid, reply)) if work.owns(rid) => {
                        work.on_reply(rid, reply);
                        Ok(())
                    }
                    Ok(Incoming::Notice(_)) | Ok(Incoming::Reply(..)) => Ok(()),
                    Ok(Incoming::Request(rid, Request::Ping)) => protocol::reply(&mut conn, rid, Reply::Pong).await,
                    Ok(Incoming::Request(rid, request @ Request::ChannelCatchUp { .. })) => work.on_request(&mut conn, rid, &request).await.map(|_| ()),
                    Ok(Incoming::Request(rid, Request::ListPosts { since })) => {
                        protocol::respond_posts(&mut conn, rid, crate::posts::asked(&ctx.paths.posts(), since, true)).await
                    }
                    Ok(Incoming::Request(rid, Request::WhoAmI)) => protocol::reply(&mut conn, rid, Reply::DisplayName(ctx.paths.my_display_name())).await,
                    Ok(Incoming::Request(rid, Request::Profile { have, from })) => protocol::reply(&mut conn, rid, super::ownlook::answer(&ctx, have, from)).await,
                    Ok(Incoming::Request(rid, _)) => protocol::reply(&mut conn, rid, Reply::Unsupported).await,
                },
            };
            if let Err(e) = outcome {
                break 'link e.to_string();
            }
        }
    };
    log!(ctx, "link to your device {who}: closed ({reason})");
    proxy.gone();
    through.gone();
    files.gone();
    carry.hold(&mut call, &ctx, &name);
    in_call.store(false, Ordering::Relaxed);
    drop(carry);
    ctx.gone(&remote, &conn.connection);
    {
        let mut siblings = ctx.siblings.locked();
        if siblings.get(&remote).is_some_and(|e| e.id == id) {
            siblings.remove(&remote);
        }
    }
    changed(&ctx);
    if own_list(&ctx).removed(&remote) {
        tell_and_close(conn).await;
    }
}

/// What peer `user` (their user key) is called here, if they're one.
pub(super) fn peer_name(ctx: &Ctx, user: &[u8; 32]) -> Option<String> {
    ctx.peers.locked().iter().find(|(_, e)| e.device.is_none() && e.watch.borrow().public_key == *user).map(|(name, _)| name.clone())
}

/// Where the user's devices besides this one and `to` last said they can
/// be reached, and when, for `to`: what it hasn't heard from them
/// itself.
/// Tells `to` where the user's other devices are, if that's changed since
/// it was last told on this link (`told`): looked at every
/// `DEVICE_ADDRESSES_EVERY`, but sent only changed — each time, it kept a
/// dozing phone's radio awake.
async fn tell_addresses(ctx: &Ctx, conn: &mut AuthenticatedConnection, to: &[u8; 32], told: &mut Option<Vec<protocol::DeviceAddress>>) -> Result<()> {
    let now = device_addresses(ctx, to);
    if told.as_ref() == Some(&now) {
        return Ok(());
    }
    protocol::send_notice(conn, Notice::DeviceAddresses(now.clone())).await?;
    *told = Some(now);
    Ok(())
}

fn device_addresses(ctx: &Ctx, to: &[u8; 32]) -> Vec<protocol::DeviceAddress> {
    let dir = ctx.paths.addresses();
    others(ctx)
        .into_iter()
        .filter(|key| key != to)
        .filter_map(|device| crate::addressbook::told(&dir, &device).map(|t| protocol::DeviceAddress { device, at: t.at, addresses: t.addresses }))
        .collect()
}

/// For a contact, from a device let relay for the others: where each of
/// the user's other devices last said it can be reached (its own
/// addresses, as it told its own devices), so the contact can reach it
/// directly where it can, rather than only through this one. `None`
/// when this device doesn't relay, or there's nothing to tell. See
/// `peerdevices::hear_devices` for their side.
pub(super) fn for_contacts(ctx: &Ctx) -> Option<Vec<protocol::DeviceAddress>> {
    if !relaying_here(ctx) {
        return None;
    }
    let dir = ctx.paths.addresses();
    let list: Vec<protocol::DeviceAddress> = others(ctx)
        .into_iter()
        .filter_map(|device| crate::addressbook::told(&dir, &device).map(|t| protocol::DeviceAddress { device, at: t.at, addresses: t.addresses }))
        .filter(|d| !d.addresses.is_empty())
        .collect();
    (!list.is_empty()).then_some(list)
}

/// Where the user's other devices are, as another of ours (`from`,
/// called `who`) heard: each kept if it's newer than what this device
/// has. Not for this device, `from` (which says where it is itself), one
/// not on the list, or one linked to now (which says so itself). Anything
/// new, and the devices not linked to are dialled again straight away.
fn take_device_addresses(ctx: &Ctx, who: &str, from: &[u8; 32], devices: &[protocol::DeviceAddress]) {
    let dir = ctx.paths.addresses();
    let others = others(ctx);
    let mut learned = false;
    for d in devices {
        if d.device == *from || !others.contains(&d.device) || ctx.siblings.locked().contains_key(&d.device) {
            continue;
        }
        if crate::addressbook::told(&dir, &d.device).is_some_and(|t| t.at >= d.at) {
            continue;
        }
        if let Ok(true) = crate::addressbook::hear(&dir, &d.device, &d.addresses, d.at) {
            log!(ctx, "your device {who} says your device {} can be reached at {}", label(ctx, &d.device), d.addresses.join(", "));
            learned = true;
        }
    }
    if learned {
        changed(ctx);
    }
}

/// A moment for what's been sent to get there (and be read: the other
/// end closes first, as a rule), then the connection's closed.
async fn tell_and_close(conn: AuthenticatedConnection) {
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.connection.closed()).await;
    conn.connection.close(0u32.into(), b"removed");
}

/// The list, to another of our devices, if it's changed since `sent`.
async fn send_list(ctx: &Ctx, conn: &mut AuthenticatedConnection, sent: &mut Vec<devicelist::Entry>) -> Result<()> {
    let entries = own_list(ctx).entries;
    if entries == *sent {
        return Ok(());
    }
    for notice in protocol::device_list_notices(entries.clone()) {
        protocol::send_notice(conn, notice).await?;
    }
    *sent = entries;
    Ok(())
}
