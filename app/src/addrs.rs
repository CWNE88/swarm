//! This device's addresses, IPv6 and IPv4 — the ones worth giving to
//! someone as where to reach you. Where the system reports their state,
//! temporary ("privacy") IPv6 addresses, deprecated ones and addresses
//! that are not yet usable are left out. Interfaces that are down or
//! have nothing plugged in are skipped.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long what the system says of this device's addresses is taken as
/// it stands: whatever asks meanwhile (the window's every refresh, the
/// address check each second) gets the same. Asking it each time — a
/// socket opened and closed for every interface, on a phone — took most
/// of Swarm's time in a call there. A move to another network is still
/// seen within `devices::ADDRESS_CHECK_EVERY` and this.
const LOOKED_UP_FOR: Duration = Duration::from_secs(1);

/// What the system said, and when.
type LookedUp<T> = Mutex<Option<(Instant, Vec<T>)>>;

static IPV6: LookedUp<(Ipv6Addr, u32)> = Mutex::new(None);
static IPV4: LookedUp<Ipv4Addr> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    /// How many times this thread's asked the system (tests).
    static LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// What `cache` holds, if it was looked up within `LOOKED_UP_FOR`, else
/// `look_up` asked afresh (and kept).
fn fresh<T: Clone>(cache: &Mutex<Option<(Instant, T)>>, look_up: fn() -> T) -> T {
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, value)) = cache.as_ref().filter(|(at, _)| at.elapsed() < LOOKED_UP_FOR) {
        return value.clone();
    }
    #[cfg(test)]
    LOOKUPS.with(|n| n.set(n.get() + 1));
    let value = look_up();
    *cache = Some((Instant::now(), value.clone()));
    value
}

/// Every usable IPv6 address configured here, and its interface's index
/// (see `look_up_ipv6`).
fn host_ipv6_with_interface() -> Vec<(Ipv6Addr, u32)> {
    fresh(&IPV6, look_up_ipv6)
}

/// Every IPv4 address on an interface that's up (see `look_up_ipv4`).
pub fn host_ipv4_addresses() -> Vec<Ipv4Addr> {
    fresh(&IPV4, look_up_ipv4)
}

/// Every usable IPv6 address configured here.
pub fn host_ipv6_addresses() -> Vec<Ipv6Addr> {
    host_ipv6_with_interface().into_iter().map(|(a, _)| a).collect()
}

/// Every stable IPv6 address configured here, and its interface's index:
/// temporary ones, ones on their way out or not yet usable, and (on
/// Android) the carrier's own tunnels left out.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn look_up_ipv6() -> Vec<(Ipv6Addr, u32)> {
    let found = ipv6_from_netlink().unwrap_or_default();
    #[cfg(target_os = "android")]
    let found = {
        let tunnels = carrier_tunnels();
        found.into_iter().filter(|(_, index)| !tunnels.contains(index)).collect()
    };
    found
}

/// The stable IPv6 addresses, asked of the kernel over netlink (an
/// address dump, `RTM_GETADDR`), the socket left unbound — as Android's
/// own `getifaddrs` asks: an Android app may neither read
/// `/proc/net/if_inet6` nor bind a netlink socket, but may ask this.
/// Unlike `getifaddrs`, it says which addresses are temporary.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn ipv6_from_netlink() -> std::io::Result<Vec<(Ipv6Addr, u32)>> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    const TEMPORARY: u32 = 0x01;
    const DAD_FAILED: u32 = 0x08;
    const DEPRECATED: u32 = 0x20;
    const TENTATIVE: u32 = 0x40;
    const ADDRESS: u16 = 1;
    const FLAGS: u16 = 8;
    let align = |n: usize| (n + 3) & !3;

    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let wait = libc::timeval { tv_sec: 2, tv_usec: 0 };
    unsafe { libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVTIMEO, &wait as *const _ as *const libc::c_void, std::mem::size_of::<libc::timeval>() as libc::socklen_t) };

    let mut request = Vec::with_capacity(24);
    request.extend_from_slice(&24u32.to_ne_bytes());
    request.extend_from_slice(&libc::RTM_GETADDR.to_ne_bytes());
    request.extend_from_slice(&((libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16).to_ne_bytes());
    request.extend_from_slice(&[0; 8]);
    request.extend_from_slice(&[libc::AF_INET6 as u8, 0, 0, 0, 0, 0, 0, 0]);
    let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    let sent = unsafe {
        libc::sendto(fd.as_raw_fd(), request.as_ptr() as *const libc::c_void, request.len(), 0, &kernel as *const _ as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t)
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut found = Vec::new();
    let mut buffer = vec![0u8; 65536];
    loop {
        let got = unsafe { libc::recv(fd.as_raw_fd(), buffer.as_mut_ptr() as *mut libc::c_void, buffer.len(), 0) };
        if got < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut messages = &buffer[..got as usize];
        while messages.len() >= 16 {
            let length = u32::from_ne_bytes(messages[0..4].try_into().unwrap()) as usize;
            let kind = u16::from_ne_bytes(messages[4..6].try_into().unwrap());
            if length < 16 || length > messages.len() {
                return Err(std::io::Error::other("a netlink message cut short"));
            }
            if kind == libc::NLMSG_DONE as u16 {
                return Ok(found);
            }
            if kind == libc::NLMSG_ERROR as u16 {
                let code = messages.get(16..20).map(|b| i32::from_ne_bytes(b.try_into().unwrap())).unwrap_or(0);
                return Err(std::io::Error::from_raw_os_error(-code));
            }
            let message = &messages[16..length];
            if kind == libc::RTM_NEWADDR && message.len() >= 8 && message[0] == libc::AF_INET6 as u8 {
                let index = u32::from_ne_bytes(message[4..8].try_into().unwrap());
                let mut flags = message[2] as u32;
                let mut address = None;
                let mut attributes = &message[8..];
                while attributes.len() >= 4 {
                    let size = u16::from_ne_bytes(attributes[0..2].try_into().unwrap()) as usize;
                    let kind = u16::from_ne_bytes(attributes[2..4].try_into().unwrap());
                    if size < 4 || size > attributes.len() {
                        break;
                    }
                    let value = &attributes[4..size];
                    if kind == ADDRESS && value.len() == 16 {
                        address = Some(Ipv6Addr::from(<[u8; 16]>::try_from(value).unwrap()));
                    } else if kind == FLAGS && value.len() == 4 {
                        flags = u32::from_ne_bytes(value.try_into().unwrap());
                    }
                    attributes = &attributes[align(size).min(attributes.len())..];
                }
                if let Some(address) = address {
                    if flags & (TEMPORARY | DAD_FAILED | DEPRECATED | TENTATIVE) == 0 {
                        found.push((address, index));
                    }
                }
            }
            messages = &messages[align(length).min(messages.len())..];
        }
    }
}

/// The carrier's own tunnels on an Android phone (`ipsec…`: calls over
/// Wi-Fi, and its other services), by interface index. They have
/// addresses of their own, global IPv6 ones too, that only the carrier
/// can reach: no use to a peer.
#[cfg(target_os = "android")]
fn carrier_tunnels() -> Vec<u32> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return found;
    }
    let mut at = list;
    while !at.is_null() {
        let entry = unsafe { &*at };
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) };
        if is_carrier_tunnel(name.to_bytes()) {
            let index = unsafe { libc::if_nametoindex(entry.ifa_name) };
            if index != 0 && !found.contains(&index) {
                found.push(index);
            }
        }
        at = entry.ifa_next;
    }
    unsafe { libc::freeifaddrs(list) };
    found
}

#[cfg(target_os = "android")]
fn is_carrier_tunnel(name: &[u8]) -> bool {
    name.starts_with(b"ipsec")
}

/// A phone's mobile data (`rmnet…` on Qualcomm, `ccmni…` on MediaTek,
/// `seth_lte…` on Unisoc). Its IPv4 address is always behind the
/// carrier's NAT, a new one each time it connects: nothing a peer can
/// reach, and a peer given each would fill its list with them. Its IPv6
/// ones are kept — the one way in to a phone on mobile data.
#[cfg(any(target_os = "android", test))]
fn is_mobile_data(name: &[u8]) -> bool {
    [&b"rmnet"[..], b"ccmni", b"seth_lte"].iter().any(|p| name.starts_with(p))
}

/// Every stable IPv6 address configured here, and its interface's index.
#[cfg(windows)]
fn look_up_ipv6() -> Vec<(Ipv6Addr, u32)> {
    windows_addresses().into_iter().filter_map(|(a, index)| match a {
        IpAddr::V6(v6) => Some((v6, index)),
        IpAddr::V4(_) => None,
    }).collect()
}

/// Every IPv4 address configured here, from `GetAdaptersAddresses`.
#[cfg(windows)]
fn look_up_ipv4() -> Vec<Ipv4Addr> {
    windows_addresses().into_iter().filter_map(|(a, _)| match a {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(_) => None,
    }).collect()
}

/// Both kinds, from adapters that are up, each with its adapter's IPv6
/// interface index.
#[cfg(windows)]
fn windows_addresses() -> Vec<(IpAddr, u32)> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{IpDadStatePreferred, IpSuffixOriginRandom, AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6};

    let mut size: u32 = 16 * 1024;
    let mut buffer: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buffer = vec![0u64; (size as usize).div_ceil(8)];
        let result = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC as u32,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                std::ptr::null(),
                buffer.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            )
        };
        const ERROR_BUFFER_OVERFLOW: u32 = 111;
        match result {
            0 => break,
            ERROR_BUFFER_OVERFLOW => continue,
            _ => return Vec::new(),
        }
    }

    let mut found = Vec::new();
    let mut adapter = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !adapter.is_null() {
        let a = unsafe { &*adapter };
        if a.OperStatus == IfOperStatusUp {
            let mut unicast = a.FirstUnicastAddress;
            while !unicast.is_null() {
                let u = unsafe { &*unicast };
                let sockaddr = u.Address.lpSockaddr;
                let preferred = u.DadState == IpDadStatePreferred;
                let usable = u.SuffixOrigin != IpSuffixOriginRandom && preferred;
                let family = if sockaddr.is_null() { 0 } else { unsafe { (*sockaddr).sa_family } };
                if family == AF_INET6 {
                    let v6 = unsafe { &*(sockaddr as *const SOCKADDR_IN6) };
                    let ip = Ipv6Addr::from(unsafe { v6.sin6_addr.u.Byte });
                    if usable || (preferred && ip.is_unicast_link_local()) {
                        found.push((IpAddr::V6(ip), a.Ipv6IfIndex));
                    }
                } else if usable && family == AF_INET {
                    let v4 = unsafe { &*(sockaddr as *const SOCKADDR_IN) };
                    found.push((IpAddr::V4(Ipv4Addr::from(u32::from_be(unsafe { v4.sin_addr.S_un.S_addr }))), a.Ipv6IfIndex));
                }
                unicast = u.Next;
            }
        }
        adapter = a.Next;
    }
    found
}

#[cfg(target_os = "macos")]
fn look_up_ipv6() -> Vec<(Ipv6Addr, u32)> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::os::fd::AsRawFd;

    let Ok(socket) = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)) else {
        return Vec::new();
    };
    let mut list = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Vec::new();
    }

    let mut found = Vec::new();
    let mut at = list;
    let up = (libc::IFF_UP | libc::IFF_RUNNING) as u32;
    while !at.is_null() {
        let entry = unsafe { &*at };
        at = entry.ifa_next;
        if entry.ifa_flags & up != up
            || entry.ifa_addr.is_null()
            || unsafe { (*entry.ifa_addr).sa_family } as i32 != libc::AF_INET6
        {
            continue;
        }
        let addr = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in6>() };
        if !macos_stable_ipv6(socket.as_raw_fd(), entry, addr) {
            continue;
        }
        let mut bytes = addr.sin6_addr.s6_addr;
        if Ipv6Addr::from(bytes).is_unicast_link_local() {
            // Darwin's kernel can store the zone in the address. The
            // interface index travels separately, never in the address.
            bytes[2..4].fill(0);
        }
        let index = if addr.sin6_scope_id != 0 {
            addr.sin6_scope_id
        } else {
            unsafe { libc::if_nametoindex(entry.ifa_name) }
        };
        found.push((Ipv6Addr::from(bytes), index));
    }
    unsafe { libc::freeifaddrs(list) };
    found
}

/// Darwin's getifaddrs has interface flags, but only SIOCGIFAFLAG_IN6
/// reports whether the particular IPv6 address is temporary or ready.
#[cfg(target_os = "macos")]
fn macos_stable_ipv6(fd: libc::c_int, entry: &libc::ifaddrs, addr: &libc::sockaddr_in6) -> bool {
    // Darwin's <netinet6/in6_var.h>. icmp6_ifstat is the biggest union
    // member in in6_ifreq (34 uint64_t counters on both macOS targets).
    #[repr(C)]
    union AddressData {
        addr: libc::sockaddr_in6,
        flags: libc::c_int,
        storage: [u64; 34],
    }
    #[repr(C)]
    struct AddressRequest {
        name: [libc::c_char; libc::IFNAMSIZ as usize],
        data: AddressData,
    }
    // _IOWR('i', 73, struct in6_ifreq)
    const GET_FLAGS: libc::c_ulong = 0xc000_0000
        | ((std::mem::size_of::<AddressRequest>() as libc::c_ulong) << 16)
        | ((b'i' as libc::c_ulong) << 8)
        | 73;
    const TENTATIVE: libc::c_int = 0x0002;
    const DUPLICATED: libc::c_int = 0x0004;
    const DETACHED: libc::c_int = 0x0008;
    const DEPRECATED: libc::c_int = 0x0010;
    const TEMPORARY: libc::c_int = 0x0080;
    const OPTIMISTIC: libc::c_int = 0x0200;
    let mut request = AddressRequest {
        name: [0; libc::IFNAMSIZ as usize],
        data: AddressData { storage: [0; 34] },
    };
    unsafe {
        libc::strncpy(request.name.as_mut_ptr(), entry.ifa_name, request.name.len() - 1);
        request.data.addr = *addr;
        libc::ioctl(fd, GET_FLAGS, &mut request as *mut AddressRequest) == 0
            && request.data.flags & (TENTATIVE | DUPLICATED | DETACHED | DEPRECATED | TEMPORARY | OPTIMISTIC) == 0
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
fn look_up_ipv6() -> Vec<(Ipv6Addr, u32)> {
    Vec::new()
}

/// This device's IPv6 link-local addresses (`fe80::/10`), each with its
/// interface's index — they work only on the link itself, and only with
/// the interface named — on interfaces another device can share a link
/// with (`neighbour_links`).
pub fn link_local_ipv6() -> Vec<(Ipv6Addr, u32)> {
    let links = neighbour_links();
    host_ipv6_with_interface()
        .into_iter()
        .filter(|(a, index)| a.is_unicast_link_local() && *index != 0 && links.as_ref().is_none_or(|l| l.contains(index)))
        .collect()
}

static LINKS: Mutex<Option<(Instant, Option<Vec<u32>>)>> = Mutex::new(None);

/// The interfaces (by index) another device can be on the same link as:
/// up, with a link, and multicast, and neither loopback nor
/// point-to-point. A tunnel (a VPN's `tun0`, WireGuard's) or Android's
/// `dummy0` has a link-local address too, but nobody at the other end of
/// it: a peer's link-local address dialled there never answers, and one
/// of ours told from there is no use to anyone. `None` where it isn't
/// known: every interface, then.
fn neighbour_links() -> Option<Vec<u32>> {
    fresh(&LINKS, look_up_links)
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn look_up_links() -> Option<Vec<u32>> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut at = list;
    while !at.is_null() {
        let entry = unsafe { &*at };
        if is_neighbour_link(entry.ifa_flags) {
            let index = unsafe { libc::if_nametoindex(entry.ifa_name) };
            if index != 0 && !found.contains(&index) {
                found.push(index);
            }
        }
        at = entry.ifa_next;
    }
    unsafe { libc::freeifaddrs(list) };
    Some(found)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn look_up_links() -> Option<Vec<u32>> {
    None
}

/// Whether an interface with these flags (`IFF_*`) can have neighbours.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn is_neighbour_link(flags: u32) -> bool {
    let has = |flag: libc::c_int| flags & flag as u32 != 0;
    has(libc::IFF_UP) && has(libc::IFF_RUNNING) && has(libc::IFF_MULTICAST) && !has(libc::IFF_LOOPBACK) && !has(libc::IFF_POINTOPOINT)
}

/// Where peers can reach this device, for them to keep (see
/// `swarm_core::addressbook`): every global address, every local one
/// (unique-local IPv6, private and shared IPv4 — a big LAN or VPN is
/// still a way through), then link-local ones (IPv4 `169.254/16`, IPv6
/// `fe80::/10` without its interface), each with `port` when it isn't
/// the default. Peers are people you know; there's no reason to keep any
/// of these from them.
pub fn for_peers(port: u16) -> Vec<String> {
    let link_v4 = host_ipv4_addresses().into_iter().filter(|a| a.is_link_local()).map(IpAddr::V4);
    let link_v6 = link_local_ipv6().into_iter().map(|(a, _)| IpAddr::V6(a));
    let mut all: Vec<String> = Vec::new();
    for ip in global_addresses().into_iter().chain(local_addresses()).chain(link_v4).chain(link_v6) {
        let text = swarm_core::peers::address_for(std::net::SocketAddr::new(ip, port));
        if !all.contains(&text) {
            all.push(text);
        }
    }
    all
}

/// The interfaces a peer's link-local address is tried on: each one with
/// a link-local address of its own.
pub fn link_scopes() -> Vec<u32> {
    let mut scopes: Vec<u32> = link_local_ipv6().into_iter().map(|(_, index)| index).collect();
    scopes.sort();
    scopes.dedup();
    scopes
}

/// Every IPv4 address on an interface that's up and has a link, loopback
/// (and, on Android, the carrier's tunnels and mobile data) aside, from
/// `getifaddrs`.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn look_up_ipv4() -> Vec<Ipv4Addr> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return found;
    }
    let mut at = list;
    while !at.is_null() {
        let entry = unsafe { &*at };
        let flags = entry.ifa_flags;
        let up = flags & libc::IFF_UP as u32 != 0 && flags & libc::IFF_RUNNING as u32 != 0 && flags & libc::IFF_LOOPBACK as u32 == 0;
        #[cfg(target_os = "android")]
        let up = up && {
            let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }.to_bytes();
            !is_carrier_tunnel(name) && !is_mobile_data(name)
        };
        if up && !entry.ifa_addr.is_null() && unsafe { (*entry.ifa_addr).sa_family } as i32 == libc::AF_INET {
            let v4 = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
            found.push(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr)));
        }
        at = entry.ifa_next;
    }
    unsafe { libc::freeifaddrs(list) };
    found
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
fn look_up_ipv4() -> Vec<Ipv4Addr> {
    Vec::new()
}

/// Whether an IPv4 address can be reached from anywhere: not private,
/// shared (CGNAT), loopback, link-local or otherwise set aside.
pub fn is_public_ipv4(a: Ipv4Addr) -> bool {
    let [b0, b1, ..] = a.octets();
    !(a.is_private()
        || a.is_loopback()
        || a.is_link_local()
        || a.is_unspecified()
        || a.is_broadcast()
        || a.is_multicast()
        || a.is_documentation()
        || b0 == 0
        || b0 >= 240
        || (b0 == 100 && (64..128).contains(&b1))
        || (b0 == 198 && (18..20).contains(&b1)))
}

/// Whether an IPv4 address works within its own network (a LAN, or an
/// overlay such as a VPN): private, or shared (`100.64/10`, which VPNs
/// like Tailscale use too).
pub fn is_local_ipv4(a: Ipv4Addr) -> bool {
    let [b0, b1, ..] = a.octets();
    a.is_private() || (b0 == 100 && (64..128).contains(&b1))
}

/// This host's global-unicast (`2000::/3`) IPv6 addresses — the ones
/// worth handing to someone anywhere as a contact address.
///
/// Deliberately filters by the address range itself rather than
/// trusting the kernel's reported scope: Linux labels unique-local
/// addresses (`fc00::/7`) as scope 0 ("global") right alongside real
/// global-unicast ones, even though a ULA only works on its own network
/// — those are listed separately, by `unique_local_ipv6_addresses`.
pub fn global_ipv6_addresses() -> Vec<std::net::Ipv6Addr> {
    host_ipv6_addresses().into_iter().filter(|a| a.octets()[0] & 0xE0 == 0x20).collect()
}

/// This host's unique-local (`fc00::/7`, in practice `fd00::/8`) IPv6
/// addresses: reachable by peers on the same network or VPN, not from
/// the wider internet — shown apart from the global ones so nobody hands
/// a ULA to someone elsewhere by mistake, but shown, since on a LAN or
/// overlay network it's exactly the address to use.
pub fn unique_local_ipv6_addresses() -> Vec<std::net::Ipv6Addr> {
    host_ipv6_addresses().into_iter().filter(|a| a.octets()[0] & 0xFE == 0xFC).collect()
}

/// This device's addresses that work from anywhere: global IPv6, then
/// public IPv4.
pub fn global_addresses() -> Vec<IpAddr> {
    let v6 = global_ipv6_addresses().into_iter().map(IpAddr::V6);
    let v4 = host_ipv4_addresses().into_iter().filter(|a| is_public_ipv4(*a)).map(IpAddr::V4);
    v6.chain(v4).collect()
}

/// This device's addresses that work on its own network or VPN:
/// unique-local IPv6, then private IPv4.
pub fn local_addresses() -> Vec<IpAddr> {
    let v6 = unique_local_ipv6_addresses().into_iter().map(IpAddr::V6);
    let v4 = host_ipv4_addresses().into_iter().filter(|a| is_local_ipv4(*a)).map(IpAddr::V4);
    v6.chain(v4).collect()
}

/// The addresses to offer others, as one comma-separated address field:
/// every global one (a channel might have members anywhere), then every
/// local one (members on the same LAN or VPN reach you there, and on a
/// LAN-only network that's all there is). Tried in this order. Empty
/// when there are none.
pub fn detected_address() -> String {
    global_addresses().into_iter().chain(local_addresses()).map(|a| a.to_string()).collect::<Vec<_>>().join(",")
}

/// What peers and channel members have seen this device's connections
/// come from, where that isn't one of its own addresses — its outside
/// address behind NAT — each with who saw it (`peers`: the names you gave
/// them; anyone else by their published name). Ports are left out: a NAT
/// picks its own for each connection, so they say nothing about where a
/// forwarded port would be.
pub fn seen_from_outside(node: &swarm_core::node::NodeHandle, peers: &std::collections::HashMap<[u8; 32], String>) -> Vec<(IpAddr, Vec<String>)> {
    let own: Vec<IpAddr> = host_ipv6_addresses().into_iter().map(IpAddr::V6).chain(host_ipv4_addresses().into_iter().map(IpAddr::V4)).collect();
    let mut seen: Vec<(IpAddr, Vec<String>)> = Vec::new();
    for (key, addr) in node.seen_as() {
        let ip = addr.ip();
        if own.contains(&ip) || ip.is_loopback() {
            continue;
        }
        let who = peers.get(&key).cloned().unwrap_or_else(|| crate::names::published_or_key(&key));
        match seen.iter_mut().find(|(a, _)| *a == ip) {
            Some((_, names)) => names.push(who),
            None => seen.push((ip, vec![who])),
        }
    }
    for (_, names) in &mut seen {
        names.sort();
    }
    seen.sort();
    seen
}

#[cfg(test)]
mod tests {
    /// The phone's interfaces as `ip link` showed them: only Wi-Fi (and a
    /// hotspot's, flagged as Wi-Fi is) has neighbours.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn only_a_link_with_neighbours_is_one_for_link_local_addresses() {
        use libc::{IFF_BROADCAST, IFF_LOOPBACK, IFF_MULTICAST, IFF_NOARP, IFF_POINTOPOINT, IFF_RUNNING, IFF_UP};
        let flags = |all: &[libc::c_int]| all.iter().fold(0u32, |f, x| f | *x as u32);
        assert!(super::is_neighbour_link(flags(&[IFF_BROADCAST, IFF_MULTICAST, IFF_UP, IFF_RUNNING])), "wlan0");
        assert!(!super::is_neighbour_link(flags(&[IFF_BROADCAST, IFF_NOARP, IFF_UP, IFF_RUNNING])), "dummy0");
        assert!(!super::is_neighbour_link(flags(&[IFF_POINTOPOINT, IFF_UP, IFF_RUNNING])), "tun0");
        assert!(!super::is_neighbour_link(flags(&[IFF_POINTOPOINT, IFF_MULTICAST, IFF_NOARP, IFF_UP, IFF_RUNNING])), "a WireGuard interface");
        assert!(!super::is_neighbour_link(flags(&[IFF_NOARP, IFF_UP, IFF_RUNNING])), "ipsec");
        assert!(!super::is_neighbour_link(flags(&[IFF_LOOPBACK, IFF_UP, IFF_RUNNING])), "lo");
        assert!(!super::is_neighbour_link(flags(&[IFF_BROADCAST, IFF_MULTICAST])), "wlan1, down");
    }

    /// A phone's mobile data, by each maker's name for it — and nothing
    /// else, a VPN's `tun0` and Wi-Fi among them.
    #[test]
    fn mobile_data_is_told_by_its_name() {
        for name in ["rmnet1", "rmnet_data0", "rmnet_ipa0", "ccmni0", "seth_lte0"] {
            assert!(super::is_mobile_data(name.as_bytes()), "{name}");
        }
        for name in ["wlan0", "tun0", "eth0", "ipsec573", "v4-wlan0", "dummy0"] {
            assert!(!super::is_mobile_data(name.as_bytes()), "{name}");
        }
    }

    /// However many ask — the window's refresh, several lists each — the
    /// system's asked for this device's addresses at most once a second
    /// of each kind, and asked again once that's past.
    #[test]
    fn the_system_is_asked_for_addresses_at_most_once_a_second() {
        let asked = || super::LOOKUPS.with(|n| n.get());
        std::thread::sleep(super::LOOKED_UP_FOR);
        let before = asked();
        for _ in 0..20 {
            let _ = (super::detected_address(), super::global_addresses(), super::local_addresses(), super::for_peers(1977), super::link_scopes());
        }
        let after = asked();
        assert!(after - before <= 3, "asked {} times", after - before);
        for _ in 0..5 {
            std::thread::sleep(super::LOOKED_UP_FOR);
            let _ = super::detected_address();
            if asked() > after {
                return;
            }
        }
        panic!("not asked again a second on");
    }

    use super::*;

    /// The kernel, asked over netlink (as an Android app can), gives the
    /// same stable IPv6 addresses as `/proc/net/if_inet6` (which an
    /// Android app can't read): temporary, deprecated, tentative and
    /// failed ones left out alike.
    #[cfg(target_os = "linux")]
    #[test]
    fn netlink_finds_the_ipv6_addresses_proc_lists() {
        const LEFT_OUT: u32 = 0x01 | 0x08 | 0x20 | 0x40;
        let text = std::fs::read_to_string("/proc/net/if_inet6").unwrap_or_default();
        let mut from_proc: Vec<(Ipv6Addr, u32)> = text
            .lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let flags = u32::from_str_radix(fields.get(4)?, 16).ok()?;
                if flags & LEFT_OUT != 0 {
                    return None;
                }
                let hex = fields.first()?;
                let mut bytes = [0u8; 16];
                for (i, byte) in bytes.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
                }
                Some((Ipv6Addr::from(bytes), u32::from_str_radix(fields.get(1)?, 16).ok()?))
            })
            .collect();
        let mut from_netlink = ipv6_from_netlink().expect("asking the kernel");
        from_proc.sort();
        from_netlink.sort();
        assert!(!from_proc.is_empty(), "this machine has IPv6 addresses (::1 at least)");
        assert_eq!(from_netlink, from_proc);
    }

    #[test]
    fn sorts_ipv4_into_public_and_local() {
        let v4 = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert!(is_public_ipv4(v4("81.2.69.160")));
        for local in ["192.168.1.20", "10.0.0.1", "172.16.5.4", "100.64.0.1"] {
            assert!(!is_public_ipv4(v4(local)) && is_local_ipv4(v4(local)), "{local}");
        }
        for neither in ["127.0.0.1", "169.254.1.1", "0.0.0.0", "198.18.0.1", "224.0.0.1"] {
            assert!(!is_public_ipv4(v4(neither)) && !is_local_ipv4(v4(neither)), "{neither}");
        }
    }
}
