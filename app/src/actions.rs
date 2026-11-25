//! What the person does, for every front end: each verb does one thing
//! and says how it went as a `Note`, which the front end only shows. So
//! the TUI and the window can't drift apart in what a key or a button
//! does, or in how it's put.
//!
//! Files are the node's own (`node.paths()`), the same folder the front
//! ends otherwise reach through `paths()`.

use crate::notes::Note;
use crate::peerbook;
use crate::sidebar::{FollowRow, PeerRow, RequestRow};
use crate::text::shorten_middle;
use swarm_core::channel::ChannelId;
use swarm_core::node::devices::DeviceInfo;
use swarm_core::node::NodeHandle;
use swarm_core::transfer::Transfer;

/// How much of a file's name a note shows.
const NAME_CHARS: usize = 32;

/// The next disappearing-message timer with this peer (off, then each
/// length in turn), set on both sides.
pub fn next_timer(node: &NodeHandle, row: &PeerRow) -> Note {
    set_timer(node, row, swarm_core::disappearing::next(node.timer(&row.handle).seconds))
}

/// The disappearing-message timer with this peer: messages delete
/// themselves after `seconds` (0: kept), on both sides.
pub fn set_timer(node: &NodeHandle, row: &PeerRow, seconds: u64) -> Note {
    match node.set_timer(&row.handle, seconds) {
        Ok(()) if seconds == 0 => Note::info(format!("messages with {} are kept again (from now on; they're told)", row.label)),
        Ok(()) => Note::info(format!(
            "messages with {} delete themselves after {} from now on, on both sides (they're told)",
            row.label,
            swarm_core::disappearing::describe(seconds)
        )),
        Err(e) => Note::error(format!("couldn't set that: {e:#}")),
    }
}

/// Deletes a line of the chat with this contact (a `ChatItem`'s
/// `lines`), on all the user's devices, at once and without asking: a
/// message they sent, from theirs too, as soon as one of their devices
/// can be told (it stays, faint, till then). Nothing's said unless it
/// couldn't be done.
pub fn delete_line(node: &NodeHandle, row: &PeerRow, lines: &[swarm_core::history::LineRef]) -> Option<Note> {
    match node.delete_lines(row.public_key, lines) {
        Ok(()) => None,
        Err(e) => Some(Note::error(format!("couldn't delete that: {e:#}"))),
    }
}

/// Deletes one of the user's own posts (by its signature), at once and
/// without asking: from all their devices as they next swap posts, and
/// from the copies contacts and followers keep as they next fetch them
/// (within 30 days; see `posts::Deletion`). Nothing's said unless it
/// couldn't be done.
pub fn delete_post(node: &NodeHandle, signature: &[u8; 64]) -> Option<Note> {
    match swarm_core::posts::delete(&node.paths().posts(), node.identity(), signature, crate::unix_now()) {
        Ok(_) => None,
        Err(e) => Some(Note::error(format!("couldn't delete that post: {e:#}"))),
    }
}

/// The next way this peer's calls are answered: asked, then answered
/// for voice, then for video too.
pub fn cycle_auto_answer(row: &PeerRow) -> Note {
    set_auto_answer(row, swarm_core::settings::auto_answer(&row.folder.settings(), &row.public_key).next())
}

/// How this peer's calls are answered.
pub fn set_auto_answer(row: &PeerRow, next: swarm_core::settings::AutoAnswer) -> Note {
    let path = row.folder.settings();
    match swarm_core::settings::set_auto_answer(&path, &row.public_key, next) {
        Ok(()) => Note::info(format!("calls from {}: {}", row.label, crate::sidebar::auto_answer_text(next))),
        Err(e) => Note::error(format!("couldn't save that: {e:#}")),
    }
}

/// How calls from `device`, another of yours, are answered here: as
/// `answer`, or (None) the next way after how they are now.
pub fn device_answer(node: &NodeHandle, device: &DeviceInfo, answer: Option<swarm_core::settings::AutoAnswer>) -> Note {
    let label = crate::devices::label(device);
    if device.this {
        return Note::error("that's this device — its own calls don't come to it");
    }
    let path = node.paths().settings();
    let answer = answer.unwrap_or_else(|| swarm_core::settings::auto_answer(&path, &device.key).next());
    match swarm_core::settings::set_auto_answer(&path, &device.key, answer) {
        Ok(()) => Note::info(format!("calls from {label}: {}", crate::sidebar::auto_answer_text(answer))),
        Err(e) => Note::error(format!("couldn't save that: {e:#}")),
    }
}

/// Whether this peer may control your shared screen without asking:
/// `on`, or (None) the other way from now.
pub fn set_auto_control(row: &PeerRow, on: Option<bool>) -> Note {
    let path = row.folder.settings();
    let on = on.unwrap_or_else(|| !swarm_core::settings::auto_control(&path, &row.public_key));
    match swarm_core::settings::set_auto_control(&path, &row.public_key, on) {
        Ok(()) => control_note(&row.label, on),
        Err(e) => Note::error(format!("couldn't save that: {e:#}")),
    }
}

/// Whether `device`, another of yours, may control this one's shared
/// screen without asking: `on`, or (None) the other way from now.
pub fn device_control(node: &NodeHandle, device: &DeviceInfo, on: Option<bool>) -> Note {
    if device.this {
        return Note::error("that's this device — it can't control its own screen");
    }
    let path = node.paths().settings();
    let on = on.unwrap_or_else(|| !swarm_core::settings::auto_control(&path, &device.key));
    match swarm_core::settings::set_auto_control(&path, &device.key, on) {
        Ok(()) => control_note(&crate::devices::label(device), on),
        Err(e) => Note::error(format!("couldn't save that: {e:#}")),
    }
}

fn control_note(label: &str, on: bool) -> Note {
    if on {
        Note::warning(format!("{label} may now control your shared screen without asking"))
    } else {
        Note::info(format!("{label} must ask each time to control your screen"))
    }
}

/// Turns down a file offered.
pub fn decline_transfer(node: &NodeHandle, transfer: &Transfer) -> Note {
    match node.decline_transfer(&transfer.id) {
        Ok(()) => Note::info(format!("declined {}", shorten_middle(&transfer.name, NAME_CHARS))),
        Err(e) => Note::error(format!("couldn't decline: {e:#}")),
    }
}

/// Stops a file coming or going.
pub fn cancel_transfer(node: &NodeHandle, transfer: &Transfer) -> Note {
    match node.cancel_transfer(&transfer.id) {
        Ok(()) => Note::info(format!("cancelled {}", shorten_middle(&transfer.name, NAME_CHARS))),
        Err(e) => Note::error(format!("couldn't cancel: {e:#}")),
    }
}

/// Takes finished transfers off the list (here only; a file received
/// stays where it was saved). One: by name; more: how many.
pub fn clear_transfers(node: &NodeHandle, transfers: &[Transfer]) -> Note {
    let ids: Vec<_> = transfers.iter().filter(|t| t.state.is_finished()).map(|t| t.id).collect();
    match node.clear_transfers(&ids) {
        Ok(0) => Note::info("nothing finished to clear"),
        Ok(1) if transfers.len() == 1 => Note::info(format!("cleared {}", shorten_middle(&transfers[0].name, NAME_CHARS))),
        Ok(1) => Note::info("cleared 1 finished transfer"),
        Ok(n) => Note::info(format!("cleared {n} finished transfers")),
        Err(e) => Note::error(format!("couldn't clear: {e:#}")),
    }
}

/// A file offered, left to answer later.
pub fn transfer_left(transfer: &Transfer) -> Note {
    Note::info(format!("{} left for later — it's in Transfers", shorten_middle(&transfer.name, NAME_CHARS)))
}

/// Takes a contact request off the list, until they ask again.
pub fn dismiss_request(row: &RequestRow) -> Note {
    match swarm_core::requests::remove(&row.folder.requests(), &row.public_key) {
        Ok(()) => Note::info(format!("dismissed {} — it comes back if they try again", row.label())),
        Err(e) => Note::error(format!("couldn't dismiss: {e:#}")),
    }
}

/// What's asked before blocking someone who asked to be a peer.
pub fn block_question(row: &RequestRow) -> String {
    format!("block {}? They'll never show up here again.", row.label())
}

/// Blocks someone who asked to be a peer: they're never shown again.
pub fn block_request(row: &RequestRow) -> Note {
    let label = row.label();
    match swarm_core::requests::block(&row.folder.requests(), row.public_key, crate::unix_now()) {
        Ok(()) => Note::success(format!("blocked {label}")),
        Err(e) => Note::error(format!("couldn't block {label}: {e:#}")),
    }
}

/// A blocked key, as You lists them to be unblocked.
#[derive(Clone, Debug)]
pub struct BlockedRow {
    pub public_key: [u8; 32],
    /// The name they last gave, if any.
    pub name: Option<String>,
    pub folder: swarm_core::node::Paths,
}

impl BlockedRow {
    /// Their name, or a short form of their key.
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| crate::names::short_key(&self.public_key))
    }

    /// Their whole key, as it's typed.
    pub fn key(&self) -> String {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        STANDARD.encode(self.public_key)
    }
}

/// Every blocked key, most recent first.
pub fn blocked(node: &NodeHandle) -> Vec<BlockedRow> {
    let folder = node.paths().clone();
    let published = swarm_core::displayname::cached_all(&folder.displaynames()).unwrap_or_default();
    swarm_core::requests::blocked(&folder.requests())
        .unwrap_or_default()
        .into_iter()
        .map(|r| BlockedRow { public_key: r.public_key, name: published.get(&r.public_key).map(|dn| dn.name.clone()), folder: folder.clone() })
        .collect()
}

/// Unblocks someone: they can ask to be added again.
pub fn unblock(row: &BlockedRow) -> Note {
    match swarm_core::requests::forget(&row.folder.requests(), &row.public_key) {
        Ok(()) => Note::success(format!("unblocked {} — they can ask to be added again", row.label())),
        Err(e) => Note::error(format!("couldn't unblock {}: {e:#}", row.label())),
    }
}

/// What's asked before deleting a contact: deleted, or blocked too.
pub fn remove_peer_question(name: &str, asked: bool) -> String {
    if asked {
        return format!(
            "cancel your contact request to {name}? It goes from their list, as soon as one of your devices can tell them, and {name} comes off yours, on all your devices. Block: and they can't ask you either."
        );
    }
    format!(
        "delete {name}? Everything about them goes, on all your devices: your chat, their posts, name and picture. Files they sent you stay. Delete: they can ask to be added again. Block: they can't, and only their key is kept, to unblock under You."
    )
}

/// Deletes a contact, or (`block`) blocks them too.
pub fn remove_peer(node: &NodeHandle, name: &str, block: bool) -> Note {
    match peerbook::remove(&node.paths().peers(), name, block, Some(node)) {
        Ok(note) => Note::success(note),
        Err(e) => Note::error(format!("couldn't delete {}: {e:#}", node.label_of(name))),
    }
}

/// After a peer's added or edited (`result`, from `peerbook`): their
/// cover rate too, if one was typed ("64k", "64k require", "off"; blank
/// leaves it as it was).
pub fn saved_peer(node: &NodeHandle, result: anyhow::Result<String>, key: &str, cover: &str) -> Note {
    let who = peerbook::decode_key(key).map_or_else(|_| key.to_string(), |key| swarm_core::peers::handle_of(&key));
    let name = who.as_str();
    let note = match result {
        Ok(note) => note,
        Err(e) => return Note::error(format!("{e:#}")),
    };
    if cover.trim().is_empty() {
        return Note::success(note);
    }
    match peerbook::parse_cover_setting(cover) {
        Ok((rate, required)) => match peerbook::set_cover(&node.paths().peers(), name, rate, required, Some(node)) {
            Ok(_) => Note::success(note),
            Err(e) => Note::error(format!("{note}, but the cover rate didn't save: {e:#}")),
        },
        Err(e) => Note::error(format!("{note}, but the cover rate wasn't understood ({e})")),
    }
}

/// A peer's own cover rate alone; blank or `off` puts them back on the
/// default.
pub fn set_cover(node: &NodeHandle, name: &str, cover: &str) -> Note {
    let parsed = if cover.trim().is_empty() { Ok((None, false)) } else { peerbook::parse_cover_setting(cover) };
    match parsed {
        Ok((rate, required)) => match peerbook::set_cover(&node.paths().peers(), name, rate, required, Some(node)) {
            Ok(note) => Note::success(note),
            Err(e) => Note::error(format!("the cover rate didn't save: {e:#}")),
        },
        Err(e) => Note::error(e),
    }
}

/// Where files from this peer are saved (blank: asked each time).
pub fn set_receive(row: &PeerRow, folder: &str) -> Note {
    match peerbook::set_receive(row, folder) {
        Ok(note) => Note::success(note),
        Err(e) => Note::error(format!("couldn't set that: {e:#}")),
    }
}

/// What this device is called among your devices.
pub fn rename_device(node: &NodeHandle, name: &str) -> Note {
    match node.rename_this_device(name.trim()) {
        Ok(true) => Note::success(format!("this device is called {} now", name.trim())),
        Ok(false) => Note::info(format!("this device is called {} already", name.trim())),
        Err(e) => Note::error(format!("{e:#}")),
    }
}

/// Lets `device`, one of yours, relay for the others, or stops it.
pub fn allow_relay(node: &NodeHandle, device: &DeviceInfo, yes: bool) -> Note {
    let label = crate::devices::label(device);
    match node.allow_relay(device.key, yes) {
        Ok(_) if yes => Note::success(format!("{label} relays for your other devices — contacts who can't reach them directly go through it")),
        Ok(_) => Note::info(format!("{label} no longer relays for your other devices")),
        Err(e) => Note::error(format!("couldn't set that: {e:#}")),
    }
}

/// Sets this device to receive connections only — it dials no one — or
/// to dial again.
pub fn receive_only(node: &NodeHandle, yes: bool) -> Note {
    match node.set_receive_only(yes) {
        Ok(_) if yes => Note::success("this device receives connections only: it dials no one, and your other devices and contacts connect to it"),
        Ok(_) => Note::info("this device dials your other devices and contacts again"),
        Err(e) => Note::error(format!("couldn't set that: {e:#}")),
    }
}

/// Sets this device's bandwidth limit from what was typed (`2m`, `500k`,
/// `2 Mbit/s`; empty or `none`: no limit).
pub fn bandwidth_limit(node: &NodeHandle, typed: &str) -> Note {
    let typed = typed.trim();
    let limit = if typed.is_empty() || typed.eq_ignore_ascii_case("none") || typed.eq_ignore_ascii_case("off") {
        None
    } else {
        match swarm_core::node::parse_bandwidth_limit(typed) {
            Ok(limit) => Some(limit),
            Err(e) => return Note::error(e),
        }
    };
    match node.set_bandwidth_limit(limit) {
        Ok(()) => match limit {
            Some(limit) => Note::success(format!("this device sends at most {}bit/s now", swarm_core::node::format_bitrate(limit))),
            None => Note::info("this device's bandwidth limit is off"),
        },
        Err(e) => Note::error(format!("couldn't set that: {e:#}")),
    }
}

/// Sets this device's receive limit from what was typed (as
/// `bandwidth_limit`): contacts are asked to keep under it.
pub fn receive_limit(node: &NodeHandle, typed: &str) -> Note {
    let typed = typed.trim();
    let limit = if typed.is_empty() || typed.eq_ignore_ascii_case("none") || typed.eq_ignore_ascii_case("off") {
        None
    } else {
        match swarm_core::node::parse_bandwidth_limit(typed) {
            Ok(limit) => Some(limit),
            Err(e) => return Note::error(e),
        }
    };
    match node.set_receive_limit(limit) {
        Ok(()) => match limit {
            Some(limit) => Note::success(format!("contacts are asked to send this device at most {}bit/s", swarm_core::node::format_bitrate(limit))),
            None => Note::info("this device's receive limit is off"),
        },
        Err(e) => Note::error(format!("couldn't set that: {e:#}")),
    }
}

/// The receive limit as it's shown and typed (`1m`; empty: none).
pub fn receive_limit_text(node: &NodeHandle) -> String {
    node.receive_limit().map(swarm_core::node::format_bitrate).unwrap_or_default()
}

/// The bandwidth limit as it's shown and typed (`2m`; empty: none).
pub fn bandwidth_limit_text(node: &NodeHandle) -> String {
    node.bandwidth_limit().map(swarm_core::node::format_bitrate).unwrap_or_default()
}

/// Sets the address you typed for `device`, another of yours (IPs or host
/// names, comma-separated; empty clears it), and tries it. That device's
/// code, pasted, gives the IPs in it.
pub fn set_device_address(node: &NodeHandle, device: &DeviceInfo, text: &str) -> Note {
    if let Some(code) = crate::contact::parse(text) {
        return add_device_code(node, device, &code);
    }
    set_typed(node, device, text)
}

/// Sets the IPs in the code of `device`, another of yours, scanned or
/// pasted, as its address, and tries them.
pub fn add_device_code(node: &NodeHandle, device: &DeviceInfo, code: &crate::contact::Contact) -> Note {
    match crate::contact::own_device_addresses(code, &node.identity().public_key_bytes()) {
        Ok(addresses) => set_typed(node, device, &addresses),
        Err(why) => Note::error(why),
    }
}

fn set_typed(node: &NodeHandle, device: &DeviceInfo, text: &str) -> Note {
    let label = crate::devices::label(device);
    match node.set_device_address(device.key, text) {
        Ok(kept) if kept.is_empty() => Note::info(format!("no address set for {label}: it's dialled where it's been")),
        Ok(kept) => Note::success(format!("trying {label} at {}: kept, and tried with where it's been whenever it isn't connected", kept.join(", "))),
        Err(e) => Note::error(format!("{e:#}")),
    }
}

/// Sets the address of contact `row` (IPs or host names, comma-separated,
/// as when they were added), keeping their name, key and the rest; their
/// code, pasted, gives the addresses in it.
pub fn set_contact_address(node: &NodeHandle, row: &crate::sidebar::PeerRow, text: &str) -> Note {
    let text = match crate::contact::parse(text) {
        Some(code) if code.key != row.public_key => return Note::error("that code is someone else's: nothing was changed"),
        Some(code) if code.addresses.is_empty() => return Note::error("that code has no addresses in it"),
        Some(code) => code.address_text(),
        None => text.trim().to_string(),
    };
    let items = match swarm_core::myaddress::check(&text) {
        Ok(items) if items.is_empty() => return Note::error(format!("{} needs an address", row.label)),
        Ok(items) => items,
        Err(e) => return Note::error(e),
    };
    let address = items.join(",");
    if address == row.address {
        return Note::info(format!("{} is already at {address}", row.label));
    }
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let path = node.paths().peers();
    let name = swarm_core::peers::load(&path).unwrap_or_default().into_iter().find(|p| p.public_key == row.public_key).map(|p| p.name);
    let Some(name) = name else { return Note::error(format!("{} isn't a contact any more", row.label)) };
    match peerbook::update(&path, &row.handle, &name, &address, &STANDARD.encode(row.public_key), Some(node)) {
        Ok(_) => Note::success(format!("trying {} at {address}: kept, and looked up afresh at each try", row.label)),
        Err(e) => Note::error(format!("{e:#}")),
    }
}

/// Asked before `device` is taken off.
pub fn remove_device_question(device: &DeviceInfo) -> String {
    format!(
        "take {} off your devices? It deletes everything Swarm keeps on it once it hears, and your contacts won't talk to it again. It can't be undone: to have it back, add it again as a new device",
        crate::devices::label(device)
    )
}

/// Takes `device`, another of yours, off your devices, for good.
pub fn remove_device(node: &NodeHandle, device: &DeviceInfo) -> Note {
    let label = crate::devices::label(device);
    if device.this {
        return Note::error("that's this device — another of yours takes it off");
    }
    let removed = node.remove_device(device.key);
    if removed.is_ok() {
        let _ = swarm_core::settings::forget_peer(&node.paths().settings(), &device.key);
    }
    match removed {
        Ok(()) if device.online => Note::success(format!("{label} is no longer one of your devices, and is deleting what Swarm kept on it")),
        Ok(()) => Note::success(format!("{label} is no longer one of your devices; it deletes what Swarm kept on it when it next connects")),
        Err(e) => Note::error(format!("couldn't take {label} off: {e:#}")),
    }
}

/// A code for adding a device, as a line (for the QR code), or why
/// there isn't one.
pub fn device_code(node: &NodeHandle) -> Result<String, Note> {
    node.add_device_code().map(|code| code.line()).map_err(|e| Note::error(format!("couldn't make a code: {e:#}")))
}

/// Follows someone's public posts. Their key when it worked, so their
/// posts aren't fetched again straight away (following fetches them).
pub fn follow(node: &NodeHandle, key: &str, address: &str) -> (Note, Option<[u8; 32]>) {
    match peerbook::decode_key(key).and_then(|key| node.follow(key, address.trim()).map(|()| key)) {
        Ok(key) => (Note::success("following them — their public posts are under FOLLOWING"), Some(key)),
        Err(e) => (Note::error(format!("couldn't follow them: {e:#}")), None),
    }
}

/// Stops following someone.
pub fn unfollow(node: &NodeHandle, row: &FollowRow) -> Note {
    match node.unfollow(&row.public_key) {
        Ok(_) => Note::info(format!("stopped following {}", row.label)),
        Err(e) => Note::error(format!("couldn't stop following {}: {e:#}", row.label)),
    }
}

/// Starts a group of you and `contacts` (by name): the group, to open,
/// and what's to be said about it.
pub fn new_group(node: &NodeHandle, contacts: &[&str]) -> (Option<ChannelId>, Note) {
    match node.create_group(contacts) {
        Ok((group, failed)) if failed.is_empty() && contacts.is_empty() => (Some(group), Note::success("started a group — Add people… puts your contacts in it")),
        Ok((group, failed)) if failed.is_empty() => (Some(group), Note::success(format!("started a group with {}", contacts.iter().map(|c| node.label_of(c)).collect::<Vec<_>>().join(", ")))),
        Ok((group, failed)) => (Some(group), Note::warning(format!("started a group, but {}", failed.join("; ")))),
        Err(e) => (None, Note::error(format!("couldn't start a group: {e:#}"))),
    }
}

/// Adds contact `name` to group `id`.
pub fn add_to_group(node: &NodeHandle, id: &ChannelId, name: &str) -> Note {
    match node.add_to_group(id, name) {
        Ok(()) => Note::success(format!("added {} — they'll see what's said from now on", node.label_of(name))),
        Err(e) => Note::error(format!("couldn't add {}: {e:#}", node.label_of(name))),
    }
}

/// Renames group `id` (blank: back to its members' names).
pub fn rename_group(node: &NodeHandle, id: &ChannelId, name: &str) -> Note {
    match node.rename_group(id, name) {
        Ok(()) if name.trim().is_empty() => Note::success("the group goes by its members' names again"),
        Ok(()) => Note::success(format!("named the group {}", name.trim())),
        Err(e) => Note::error(format!("couldn't rename the group: {e:#}")),
    }
}

/// Your name, as others see it beside your key. Nothing when it's
/// blank or as it was.
pub fn set_name(node: &NodeHandle, name: &str) -> Option<Note> {
    let name = name.trim();
    let path = node.paths().displayname();
    if name.is_empty() || swarm_core::displayname::current(&path).ok().flatten().is_some_and(|d| d.name == name) {
        return None;
    }
    Some(match swarm_core::displayname::set(&path, node.identity(), name) {
        Ok(()) => Note::success(format!("you're {name} now — others see it beside your key")),
        Err(e) => Note::error(format!("couldn't set your name: {e:#}")),
    })
}

/// The address you set by hand (blank clears it): first in your code,
/// and its IPs told to your contacts, on all your devices.
pub fn set_manual_address(node: &NodeHandle, text: &str) -> Note {
    if swarm_core::myaddress::check(text).is_ok_and(|items| items == node.manual_address().items) {
        return Note::info("your manually set address is as it was");
    }
    match node.set_manual_address(text) {
        Ok(manual) if manual.items.is_empty() => Note::success("your manually set address is cleared"),
        Ok(manual) => Note::success(format!("your manually set address is {} — it's first in your code now, on all your devices", manual.text())),
        Err(e) => Note::error(e),
    }
}

/// Makes you discoverable on the network each of your devices is on, or
/// not (see `discovery`), on all your devices.
pub fn set_discoverable(node: &NodeHandle, on: bool) -> Note {
    match node.set_discoverable(on) {
        Ok(_) if on => Note::success("you're discoverable: anyone on the same network as one of your devices can find you under Add a contact"),
        Ok(_) => Note::success("you're no longer discoverable on any network"),
        Err(e) => Note::error(e),
    }
}

/// Your status, as your peers see it (blank clears it); your picture
/// stays as it is.
pub fn set_status(node: &NodeHandle, says: &str) -> Note {
    let says = says.trim();
    let avatar = swarm_core::profile::current(&node.paths().profile()).map(|p| p.avatar).unwrap_or_default();
    let set = node.set_profile(says, &avatar);
    crate::avatar::changed_own();
    match set {
        Ok(()) if says.is_empty() => Note::success("your status is cleared"),
        Ok(()) => Note::success("your status is set — your contacts see it"),
        Err(e) => Note::error(format!("couldn't set your status: {e:#}")),
    }
}
