//! Finding each other on one network (mDNS, DNS-SD's `_swarm._udp`;
//! the packets are `swarm_core::mdns`'s).
//!
//! - **Being found:** while the user's discoverable (`swarm_core::
//!   discoverable`, on unless turned off, the same on all their
//!   devices), this device says it's here on each of its interfaces:
//!   its user's key and name and its address on that interface (private
//!   IPv4, IPv6 link-local), and nothing else: never the device's name,
//!   nor its key (its labels carry `mdns::token` instead). Once on starting or
//!   moving, and whenever asked; a goodbye when it's turned off or Swarm
//!   stops.
//! - **Finding:** only while Add a contact or Follow someone is open
//!   (`browse`), it asks who's here, and keeps what's said for the form
//!   to show (`nearby`): one row a user, every device of theirs' addresses
//!   in it; their own devices left out, contacts at the end.
//!
//! Anyone on the network can say anything: a row only fills in the form,
//! a key found here is proven by connecting (Noise), and its owner still
//! accepts the request or doesn't.

use std::collections::HashMap;

/// The most devices heard on the LAN kept at once: far more than any
/// office has, few enough that a flood of made-up answers costs nothing.
const MOST_HEARD: usize = 512;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use swarm_core::mdns::{self, Advert};
use swarm_core::node::NodeHandle;
use tokio::net::UdpSocket;

/// How often interfaces and the setting are looked at again.
const LOOK_EVERY: Duration = Duration::from_secs(2);
/// How often what this device says (its name, the setting as kept on
/// disk) is looked at again when nothing else has changed: reading them
/// means opening sealed files, which an idle phone shouldn't do often.
const REREAD_EVERY: Duration = Duration::from_secs(30);
/// How often the setting is read from disk, for `swarm discoverable`
/// run while this runs (one small file).
const SETTING_EVERY: Duration = Duration::from_secs(5);
/// After asking, how long before "nobody's here" is said.
const LOOKED_AFTER: Duration = Duration::from_secs(4);
/// While finding, how often to ask again (well inside `mdns::TTL`).
const ASK_EVERY: Duration = Duration::from_secs(30);
/// The least time between two answers on the same interfaces: someone
/// asking over and over gets no more than this.
const ANSWER_GAP: Duration = Duration::from_millis(900);

/// Someone on this network, as Add a contact shows them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nearby {
    pub key: [u8; 32],
    pub name: String,
    /// In the peers file's form (`[fe80::1]:5000`; no port when it's
    /// 1977), the one each answer came from first.
    pub addresses: Vec<String>,
    /// Already a contact: shown greyed, at the end.
    pub known: bool,
}

impl Nearby {
    /// A contact code for them: what picking them puts in the form.
    pub fn code(&self) -> String {
        crate::contact::line(&self.key, &self.addresses, &[], &self.name)
    }
}

/// A device heard: its user's key, and its instance.
type Device = ([u8; 32], String);
/// What it said: its user's name, its addresses, and until when that's
/// good.
type Said = (String, Vec<String>, Instant);

/// What's shared between the running part and the front ends.
#[derive(Default)]
struct Shared {
    /// Since when it's been finding (Add a contact open); `None`: not.
    browsing: Option<Instant>,
    /// Asked to start finding, not yet asked on the network.
    ask_now: bool,
    /// Each device heard, by user key and instance: what it said, and
    /// until when that's good.
    heard: HashMap<Device, Said>,
    /// What's being said now, and on which interfaces (for a goodbye
    /// when Swarm stops); `None`: nothing.
    saying: Option<Saying>,
}

#[derive(Clone)]
struct Saying {
    advert: Advert,
    v4: Vec<Ipv4Addr>,
    v6: Vec<(Ipv6Addr, u32)>,
    sockets: (Option<std::sync::Arc<std::net::UdpSocket>>, Option<std::sync::Arc<std::net::UdpSocket>>),
}

/// On the phone: told whether multicast is needed now (being found, or
/// finding), so the app holds Wi-Fi's multicast lock only then (without
/// it, Wi-Fi drops what's sent to the group).
static MULTICAST: OnceLock<Box<dyn Fn(bool) + Send + Sync>> = OnceLock::new();

/// The phone's way to hold Wi-Fi's multicast lock, or let it go.
pub fn set_multicast_hold(hold: impl Fn(bool) + Send + Sync + 'static) {
    let _ = MULTICAST.set(Box::new(hold));
}

/// The node's log, once running: what went wrong on the network, each
/// new trouble once (an answer goes out every few seconds), and which
/// interfaces it's on.
static LOG: OnceLock<swarm_core::node::Logger> = OnceLock::new();

fn log(line: String) {
    match LOG.get() {
        Some(log) => log(format!("mDNS: {line}")),
        None => eprintln!("mDNS: {line}"),
    }
}

/// `what` went wrong with `e`: logged unless it's what went wrong last
/// time for the same `what`.
fn trouble(what: &str, e: impl std::fmt::Display) {
    static LAST: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let text = e.to_string();
    let mut last = LAST.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if last.get(what) != Some(&text) {
        log(format!("{what}: {text}"));
        last.insert(what.to_string(), text);
    }
}

/// Wakes the running part at once: Add a contact just opened.
fn wake() -> &'static tokio::sync::Notify {
    static WAKE: OnceLock<tokio::sync::Notify> = OnceLock::new();
    WAKE.get_or_init(tokio::sync::Notify::new)
}

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts or stops finding who's on this network: started when Add a
/// contact or Follow someone opens, stopped when it closes (what was
/// found is forgotten).
pub fn browse(on: bool) {
    let mut s = shared();
    match (on, s.browsing.is_some()) {
        (true, false) => {
            s.browsing = Some(Instant::now());
            s.ask_now = true;
            wake().notify_one();
        }
        (false, true) => {
            s.browsing = None;
            s.heard.clear();
        }
        _ => {}
    }
}

/// Whether it's finding now (Add a contact or Follow someone open).
pub fn browsing() -> bool {
    shared().browsing.is_some()
}

/// Whether it's been finding long enough for an empty list to mean
/// nobody's discoverable here.
pub fn looked() -> bool {
    shared().browsing.is_some_and(|at| at.elapsed() >= LOOKED_AFTER)
}

/// Who's on this network, for Add a contact: one row a user (all their
/// devices' addresses), new ones first by name and contacts at the end;
/// this user and anyone blocked left out.
pub fn nearby(node: &NodeHandle) -> Vec<Nearby> {
    let now = Instant::now();
    let mut users: Vec<Nearby> = Vec::new();
    {
        let mut s = shared();
        s.heard.retain(|_, (_, _, until)| *until > now);
        let mut heard: Vec<_> = s.heard.iter().collect();
        heard.sort_by(|a, b| a.0.cmp(b.0));
        for ((key, _), (name, addresses, _)) in heard {
            match users.iter_mut().find(|u| u.key == *key) {
                Some(user) => {
                    for a in addresses {
                        if !user.addresses.contains(a) {
                            user.addresses.push(a.clone());
                        }
                    }
                }
                None => users.push(Nearby { key: *key, name: name.clone(), addresses: addresses.clone(), known: false }),
            }
        }
    }
    let me = node.identity().public_key_bytes();
    let paths = node.paths().clone();
    let blocked: Vec<[u8; 32]> = swarm_core::requests::blocked(&paths.requests()).unwrap_or_default().iter().map(|r| r.public_key).collect();
    let peers: Vec<[u8; 32]> = swarm_core::peers::load(&paths.peers()).unwrap_or_default().iter().map(|p| p.public_key).collect();
    users.retain(|u| u.key != me && !blocked.contains(&u.key));
    for u in &mut users {
        u.known = peers.contains(&u.key);
    }
    users.sort_by(|a, b| a.known.cmp(&b.known).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    users
}

/// Says goodbye on the network, if this device is saying it's here:
/// for when Swarm stops, so others' lists lose it at once.
pub fn goodbye() {
    let saying = shared().saying.take();
    if let Some(saying) = saying {
        send_all(&saying, 0);
    }
}

/// Runs for as long as the app does: being found and finding (see the
/// top). Nothing at all on the network while the user isn't discoverable
/// and Add a contact isn't open, though the sockets stay open.
pub async fn run(node: NodeHandle) {
    let log_to = node.clone();
    let _ = LOG.set(std::sync::Arc::new(move |line| log_to.log(line)));
    let v4 = open_v4().map_err(|e| log(format!("over IPv4 isn't available: couldn't open port {}: {e}", mdns::PORT))).ok();
    let v6 = open_v6().map_err(|e| log(format!("over IPv6 isn't available: couldn't open port {}: {e}", mdns::PORT))).ok();
    if v4.is_none() && v6.is_none() {
        return;
    }
    let mut joined_v4: Vec<Ipv4Addr> = Vec::new();
    let mut joined_v6: Vec<(Ipv6Addr, u32)> = Vec::new();
    let mut said: Option<Advert> = None;
    let mut last_answer: Option<Instant> = None;
    let mut again_at: Option<Instant> = None;
    let mut asked_at: Option<Instant> = None;
    let mut asks_early = 0u8;
    let mut look = tokio::time::interval(Duration::from_secs(1));
    look.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut looked_at: Option<Instant> = None;
    let mut discoverable = node.discoverable_updates();
    let mut on = node.discoverable().on;
    let mut reread_at = Instant::now();
    let mut setting_at = Instant::now();
    let mut buf_v4 = vec![0u8; mdns::MOST_BYTES];
    let mut buf_v6 = vec![0u8; mdns::MOST_BYTES];
    let mut holding = false;
    let mut said_on: Option<Vec<String>> = None;
    loop {
        tokio::select! {
            _ = wake().notified() => look.reset_immediately(),
            Ok(()) = discoverable.changed() => {
                on = discoverable.borrow_and_update().on;
                looked_at = None;
                look.reset_immediately();
            }
            _ = look.tick() => {
                if looked_at.is_none_or(|at| at.elapsed() >= LOOK_EVERY) {
                    looked_at = Some(Instant::now());
                    let (now_v4, now_v6) = interfaces();
                    if let Some(s) = &v4 {
                        rejoin_v4(s, &mut joined_v4, &now_v4);
                    }
                    if let Some(s) = &v6 {
                        rejoin_v6(s, &mut joined_v6, &now_v6);
                    }
                    let on_now: Vec<String> = joined_v4.iter().map(|a| a.to_string()).chain(joined_v6.iter().map(|(a, i)| format!("{a}%{i}"))).collect();
                    if Some(&on_now) != said_on.as_ref() {
                        log(if on_now.is_empty() { "on no interface (none with a private IPv4 or an IPv6 link-local address)".to_string() } else { format!("on {}", on_now.join(", ")) });
                        said_on = Some(on_now);
                    }
                    let interfaces_moved = said.as_ref().is_some_and(|a| a.addresses != advert_addresses(&joined_v4, &joined_v6));
                    let reread = reread_at.elapsed() >= REREAD_EVERY;
                    if reread {
                        reread_at = Instant::now();
                    }
                    if setting_at.elapsed() >= SETTING_EVERY {
                        setting_at = Instant::now();
                        on = node.discoverable().on;
                    }
                    let advert = if !on {
                        None
                    } else if said.is_none() || interfaces_moved || reread {
                        Some(advert(&node, &joined_v4, &joined_v6))
                    } else {
                        said.clone()
                    };
                    let saying = |advert: Advert| Saying { advert, v4: joined_v4.clone(), v6: joined_v6.clone(), sockets: (std_socket(&v4), std_socket(&v6)) };
                    let moved = shared().saying.as_ref().is_some_and(|s| s.v4 != joined_v4 || s.v6 != joined_v6);
                    if advert != said || moved {
                        if let Some(old) = shared().saying.take() {
                            send_all(&old, 0);
                        }
                        if let Some(advert) = &advert {
                            let new = saying(advert.clone());
                            send_all(&new, mdns::TTL);
                            shared().saying = Some(new);
                            again_at = Some(Instant::now() + Duration::from_secs(1));
                        }
                        said = advert;
                    }
                }
                let want = said.is_some() || shared().browsing.is_some();
                if want != holding {
                    holding = want;
                    if let Some(hold) = MULTICAST.get() {
                        hold(want);
                    }
                }
                let (browsing, ask_now) = {
                    let mut s = shared();
                    (s.browsing.is_some(), std::mem::take(&mut s.ask_now))
                };
                if ask_now {
                    asks_early = 0;
                    asked_at = None;
                }
                if browsing {
                    let due = match asked_at {
                        None => true,
                        Some(at) => at.elapsed() >= match asks_early { 1 => Duration::from_secs(1), 2 => Duration::from_secs(2), _ => ASK_EVERY },
                    };
                    if due {
                        ask(&v4, &v6, &joined_v4, &joined_v6);
                        asked_at = Some(Instant::now());
                        asks_early = asks_early.saturating_add(1);
                    }
                }
                if again_at.is_some_and(|at| Instant::now() >= at) {
                    again_at = None;
                    if let Some(saying) = shared().saying.clone() {
                        send_all(&saying, mdns::TTL);
                    }
                }
            }
            got = recv(&v4, &mut buf_v4) => {
                if let Some((len, from)) = got {
                    on_packet(&buf_v4[..len], from, &said, &mut last_answer);
                }
            }
            got = recv(&v6, &mut buf_v6) => {
                if let Some((len, from)) = got {
                    on_packet(&buf_v6[..len], from, &said, &mut last_answer);
                }
            }
        }
    }
}

/// A packet heard: a query about this device answered (when
/// discoverable, and not too often), an answer kept (when finding).
fn on_packet(packet: &[u8], from: SocketAddr, said: &Option<Advert>, last_answer: &mut Option<Instant>) {
    if let Some(questions) = mdns::questions(packet) {
        let Some(advert) = said else { return };
        if questions.iter().any(|q| advert.asked(q)) && last_answer.is_none_or(|at| at.elapsed() >= ANSWER_GAP) {
            *last_answer = Some(Instant::now());
            if let Some(saying) = shared().saying.clone() {
                send_all(&saying, mdns::TTL);
            }
        }
        return;
    }
    let mut s = shared();
    if s.browsing.is_none() {
        return;
    }
    for heard in mdns::heard(packet) {
        let entry = (heard.key, heard.instance.clone());
        if heard.ttl == 0 {
            s.heard.remove(&entry);
            continue;
        }
        let mut addresses: Vec<String> = Vec::new();
        for ip in std::iter::once(from.ip()).chain(heard.addresses.iter().copied()) {
            let ip = match ip {
                IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
                ip => ip,
            };
            if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
                continue;
            }
            let text = swarm_core::peers::address_for(SocketAddr::new(ip, heard.port));
            if !addresses.contains(&text) {
                addresses.push(text);
            }
        }
        let until = Instant::now() + Duration::from_secs(u64::from(heard.ttl.min(mdns::TTL * 2)));
        let kept = s.heard.get(&entry).map(|(_, a, _)| a.clone()).unwrap_or_default();
        for a in kept {
            if !addresses.contains(&a) {
                addresses.push(a);
            }
        }
        addresses.truncate(8);
        // A LAN full of made-up answers fills no more than this.
        if !s.heard.contains_key(&entry) && s.heard.len() >= MOST_HEARD {
            continue;
        }
        s.heard.insert(entry, (heard.name, addresses, until));
    }
}

/// What this device says of itself, on the interfaces joined.
fn advert(node: &NodeHandle, v4: &[Ipv4Addr], v6: &[(Ipv6Addr, u32)]) -> Advert {
    let devices = node.devices();
    let this = devices.devices.iter().find(|d| d.this);
    let device_key = this.map(|d| d.key).unwrap_or_default();
    let name = crate::names::my_name().unwrap_or_default();
    let token = mdns::token(&device_key);
    let instance = if name.is_empty() { format!("Swarm ({token})") } else { format!("{name} ({token})") };
    let host = format!("swarm-{token}");
    let port = node.local_addr().map(|a| a.port()).unwrap_or(swarm_core::transport::DEFAULT_PORT);
    Advert { instance, host, port, key: node.identity().public_key_bytes(), name, addresses: advert_addresses(v4, v6) }
}

/// The addresses an advert carries, on the interfaces joined.
fn advert_addresses(v4: &[Ipv4Addr], v6: &[(Ipv6Addr, u32)]) -> Vec<IpAddr> {
    v4.iter().map(|a| IpAddr::V4(*a)).chain(v6.iter().map(|(a, _)| IpAddr::V6(*a))).collect()
}

/// The interfaces to be on: each with a private (or link-local) IPv4
/// address, and each with an IPv6 link-local one.
fn interfaces() -> (Vec<Ipv4Addr>, Vec<(Ipv6Addr, u32)>) {
    let mut v4: Vec<Ipv4Addr> = crate::addrs::host_ipv4_addresses().into_iter().filter(|a| crate::addrs::is_local_ipv4(*a) || a.is_link_local()).collect();
    v4.sort();
    v4.dedup();
    let mut v6 = crate::addrs::link_local_ipv6();
    v6.sort();
    v6.dedup_by_key(|(_, index)| *index);
    (v4, v6)
}

/// Each interface's answer carries its own address only.
fn send_all(saying: &Saying, ttl: u32) {
    if let Some(socket) = &saying.sockets.0 {
        for a in &saying.v4 {
            let advert = Advert { addresses: vec![IpAddr::V4(*a)], ..saying.advert.clone() };
            let r = socket2::SockRef::from(socket.as_ref());
            let sent = r.set_multicast_if_v4(a).and_then(|_| socket.send_to(&mdns::answer(&advert, ttl), SocketAddrV4::new(mdns::GROUP_V4, mdns::PORT)));
            if let Err(e) = sent {
                trouble(&format!("saying it's here on {a}"), e);
            }
        }
    }
    if let Some(socket) = &saying.sockets.1 {
        for (a, index) in &saying.v6 {
            let advert = Advert { addresses: vec![IpAddr::V6(*a)], ..saying.advert.clone() };
            let r = socket2::SockRef::from(socket.as_ref());
            let sent = r.set_multicast_if_v6(*index).and_then(|_| socket.send_to(&mdns::answer(&advert, ttl), SocketAddrV6::new(mdns::GROUP_V6, mdns::PORT, 0, *index)));
            if let Err(e) = sent {
                trouble(&format!("saying it's here on {a}%{index}"), e);
            }
        }
    }
}

/// Asks who's here, on every interface.
fn ask(v4: &Option<UdpSocket>, v6: &Option<UdpSocket>, joined_v4: &[Ipv4Addr], joined_v6: &[(Ipv6Addr, u32)]) {
    let query = mdns::query();
    if let Some(socket) = v4 {
        for a in joined_v4 {
            let sent = socket2::SockRef::from(socket).set_multicast_if_v4(a).and_then(|_| socket.try_send_to(&query, SocketAddr::V4(SocketAddrV4::new(mdns::GROUP_V4, mdns::PORT))));
            if let Err(e) = sent {
                trouble(&format!("asking who's here on {a}"), e);
            }
        }
    }
    if let Some(socket) = v6 {
        for (_, index) in joined_v6 {
            let sent = socket2::SockRef::from(socket).set_multicast_if_v6(*index).and_then(|_| socket.try_send_to(&query, SocketAddr::V6(SocketAddrV6::new(mdns::GROUP_V6, mdns::PORT, 0, *index))));
            if let Err(e) = sent {
                trouble(&format!("asking who's here on interface {index}"), e);
            }
        }
    }
}

async fn recv(socket: &Option<UdpSocket>, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
    match socket {
        Some(s) => s.recv_from(buf).await.ok(),
        None => std::future::pending().await,
    }
}

/// A copy of a socket to send on from outside the running part (a
/// goodbye when Swarm stops).
fn std_socket(socket: &Option<UdpSocket>) -> Option<std::sync::Arc<std::net::UdpSocket>> {
    let s = socket.as_ref()?;
    socket2::SockRef::from(s).try_clone().ok().map(|s| std::sync::Arc::new(std::net::UdpSocket::from(s)))
}

/// The group joined on interfaces that have come, left on those gone.
fn rejoin_v4(socket: &UdpSocket, joined: &mut Vec<Ipv4Addr>, now: &[Ipv4Addr]) {
    for gone in joined.iter().filter(|a| !now.contains(a)) {
        let _ = socket.leave_multicast_v4(mdns::GROUP_V4, *gone);
    }
    joined.retain(|a| now.contains(a));
    for a in now {
        if !joined.contains(a) {
            if let Err(e) = socket.join_multicast_v4(mdns::GROUP_V4, *a) {
                trouble(&format!("listening on {a}"), e);
            }
            joined.push(*a);
        }
    }
}

fn rejoin_v6(socket: &UdpSocket, joined: &mut Vec<(Ipv6Addr, u32)>, now: &[(Ipv6Addr, u32)]) {
    for (_, index) in joined.iter().filter(|j| !now.iter().any(|n| n.1 == j.1)) {
        let _ = socket.leave_multicast_v6(&mdns::GROUP_V6, *index);
    }
    joined.retain(|j| now.iter().any(|n| n.1 == j.1));
    for n in now {
        match joined.iter_mut().find(|j| j.1 == n.1) {
            Some(j) => j.0 = n.0,
            None => {
                if let Err(e) = socket.join_multicast_v6(&mdns::GROUP_V6, n.1) {
                    trouble(&format!("listening on interface {}", n.1), e);
                }
                joined.push(*n);
            }
        }
    }
}

/// Port 5353, shared with the system's own mDNS (Avahi, Windows', the
/// phone's).
fn open_v4() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(all(unix, not(target_os = "solaris")))]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, mdns::PORT)).into())?;
    s.set_multicast_ttl_v4(255)?;
    s.set_multicast_loop_v4(true)?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s.into())
}

fn open_v6() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_only_v6(true)?;
    s.set_reuse_address(true)?;
    #[cfg(all(unix, not(target_os = "solaris")))]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, mdns::PORT)).into())?;
    s.set_multicast_hops_v6(255)?;
    s.set_multicast_loop_v6(true)?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_kept_by_user_with_where_it_came_from_first_and_a_goodbye_drops_it() {
        browse(true);
        let advert = Advert { instance: "Ann (phone)".into(), host: "swarm-01".into(), port: 1977, key: [3; 32], name: "Ann".into(), addresses: vec!["192.168.1.9".parse().unwrap()] };
        let from: SocketAddr = "192.168.1.9:5353".parse().unwrap();
        on_packet(&mdns::answer(&advert, mdns::TTL), from, &None, &mut None);
        let laptop = Advert { instance: "Ann (laptop)".into(), addresses: vec!["fe80::9".parse().unwrap()], port: 5000, ..advert.clone() };
        on_packet(&mdns::answer(&laptop, mdns::TTL), "[fe80::9]:5353".parse().unwrap(), &None, &mut None);
        {
            let s = shared();
            let phone = &s.heard[&([3; 32], "Ann (phone)".to_string())];
            assert_eq!(phone.1, ["192.168.1.9"]);
            let laptop = &s.heard[&([3; 32], "Ann (laptop)".to_string())];
            assert_eq!(laptop.1, ["[fe80::9]:5000"]);
        }
        on_packet(&mdns::answer(&advert, 0), from, &None, &mut None);
        assert!(!shared().heard.contains_key(&([3; 32], "Ann (phone)".to_string())));
        browse(false);
        assert!(shared().heard.is_empty(), "forgotten once the form closes");
        on_packet(&mdns::answer(&advert, mdns::TTL), from, &None, &mut None);
        assert!(shared().heard.is_empty());
    }

    #[test]
    fn a_code_for_someone_found_reads_back() {
        let n = Nearby { key: [5; 32], name: "Bo Smith".into(), addresses: vec!["192.168.1.4".into(), "fe80::4".into()], known: false };
        let c = crate::contact::parse(&n.code()).unwrap();
        assert_eq!((c.key, c.name.as_str(), c.addresses.clone()), ([5; 32], "Bo Smith", n.addresses.clone()));
    }
}
