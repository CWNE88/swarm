//! This device among others of yours, for the front ends: what it calls
//! itself, keeping its addresses told, and how your devices are
//! described.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use swarm_core::node::devices::{DeviceInfo, DevicesView};
use swarm_core::transport::DEFAULT_PORT;

/// What a device taken off your devices by another of yours says, once
/// it's been wiped.
pub const WIPED: &str = "Another of your devices took this one off your devices, and everything Swarm kept here has been deleted: your identity, contacts and chats. Your other devices still have them. To use Swarm here again, start it again: it asks, as the first time, whether to make a new user or add this device to yours.";

/// A name for this device to start from: its host name, as the system
/// has it.
pub fn default_name() -> String {
    swarm_core::devicelist::default_name()
}

/// What this device is called among your devices, or (not named yet)
/// its host name.
pub fn name_here(node: &swarm_core::node::NodeHandle) -> String {
    node.devices().devices.into_iter().find(|d| d.this).map(|d| d.name).filter(|n| !n.is_empty()).unwrap_or_else(default_name)
}

/// How often this device's addresses are looked at again, for a laptop
/// that moves while Swarm's running — often, as a call carries on over
/// a move only if it's seen within moments (the core holds a call
/// 10 seconds for a new connection); looking costs a file read or two.
pub const ADDRESS_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// Keeps peers (and your other devices) told where this
/// device can be reached, for as long as the app runs: now, and again whenever
/// its addresses change. The IPs of the address you set by hand come
/// first (`myaddress`): where you're reached from anywhere, though no
/// device of yours can see it.
pub async fn keep_addresses(node: swarm_core::node::NodeHandle) {
    let port = node.local_addr().map(|a| a.port()).unwrap_or(DEFAULT_PORT);
    let mut last = (Vec::new(), Vec::new());
    loop {
        let mut now = node.manual_address().literal();
        for found in crate::addrs::for_peers(port) {
            if !now.contains(&found) {
                now.push(found);
            }
        }
        let now = (now, crate::addrs::link_scopes());
        if now != last {
            node.set_my_addresses(now.0.clone(), now.1.clone());
            last = now;
        }
        tokio::time::sleep(ADDRESS_CHECK_EVERY).await;
    }
}

/// A device's name to show: its own, or the start of its key while it
/// has none.
pub fn label(device: &DeviceInfo) -> String {
    if device.name.is_empty() { short_key(&device.key) } else { device.name.clone() }
}

/// Another of your devices, in a call: "phone (yours)".
pub fn in_calls(device: &DeviceInfo) -> String {
    format!("{} (yours)", label(device))
}

pub fn short_key(key: &[u8; 32]) -> String {
    STANDARD.encode(key).chars().take(10).collect()
}

/// One line about `device`: its name, whether it's this one or online,
/// and when it was added.
pub fn describe(device: &DeviceInfo, now: u64) -> String {
    format!("{} ({}) — {}", label(device), short_key(&device.key), standing(device, now))
}

/// How `device` stands: whether it's this one or online, when it was
/// added, and whether it relays.
pub fn standing(device: &DeviceInfo, now: u64) -> String {
    let relays = if device.relay { ", relays for the others" } else { "" };
    let receives = if device.receive_only { ", receives connections only" } else { "" };
    let added = if device.added_at == 0 { added(device, now) } else { format!("added {}", added(device, now)) };
    format!("{}, {added}{relays}{receives}", state(device))
}

/// Whether `device` is this one, or online.
pub fn state(device: &DeviceInfo) -> String {
    if device.this {
        "this device".to_string()
    } else if let Some(relay) = &device.through {
        format!("online through {relay}")
    } else if device.online {
        "online".to_string()
    } else {
        "not connected".to_string()
    }
}

/// The link to `device` in use now, as a contact's is shown (this end,
/// an arrow from whichever dialled, its end), or that there's none;
/// `None` for this device.
pub fn in_use(node: &swarm_core::node::NodeHandle, device: &DeviceInfo) -> Option<String> {
    if device.this {
        return None;
    }
    Some(match node.connection_to(&device.key) {
        Some(link) => crate::peerbook::in_use_text(&link),
        None => "not connected".to_string(),
    })
}

/// When `device` was added, roughly ("3 days ago"), or that it was the
/// first.
pub fn added(device: &DeviceInfo, now: u64) -> String {
    if device.added_at == 0 { "the first device".to_string() } else { format!("{} ago", crate::time::how_long(now.saturating_sub(device.added_at))) }
}

/// Where `device` last said it's at, one address each.
pub fn addresses(device: &DeviceInfo) -> Vec<String> {
    device.address.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect()
}

/// Whether another of your devices (`key`, its own) may control this
/// one's shared screen without asking.
pub fn auto_control(node: &swarm_core::node::NodeHandle, key: &[u8; 32]) -> bool {
    swarm_core::settings::auto_control(&node.paths().settings(), key)
}

/// How calls from another of your devices (`key`, its own device key)
/// are answered here: kept in this device's settings, as a contact's are
/// under theirs, so letting one device in lets no other.
pub fn auto_answer(node: &swarm_core::node::NodeHandle, key: &[u8; 32]) -> swarm_core::settings::AutoAnswer {
    swarm_core::settings::auto_answer(&node.paths().settings(), key)
}

/// Your devices as listed under You: this one first, the rest by name.
pub fn in_list_order(mut devices: Vec<DeviceInfo>) -> Vec<DeviceInfo> {
    devices.sort_by_cached_key(|d| (!d.this, label(d).to_lowercase(), d.key));
    devices
}

/// The devices in `view` matching `typed`: a name, or the start of a key.
pub fn find<'a>(view: &'a DevicesView, typed: &str) -> Result<&'a DeviceInfo, String> {
    let by_name: Vec<&DeviceInfo> = view.devices.iter().filter(|d| d.name == typed).collect();
    let matches = if by_name.is_empty() { view.devices.iter().filter(|d| STANDARD.encode(d.key).starts_with(typed)).collect() } else { by_name };
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no device of yours called '{typed}'")),
        _ => Err(format!("more than one device matches '{typed}' — give the start of its key instead")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, this: bool) -> DeviceInfo {
        DeviceInfo { key: [name.len() as u8; 32], name: name.to_string(), address: String::new(), typed: String::new(), added_at: 0, this, online: false, relay: false, receive_only: false, through: None }
    }

    #[test]
    fn this_device_comes_first_and_the_rest_by_name() {
        let listed = in_list_order(vec![device("server", false), device("phone", true), device("Desktop", false), device("laptop", false)]);
        let names: Vec<&str> = listed.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["phone", "Desktop", "laptop", "server"]);
    }
}
