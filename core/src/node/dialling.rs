//! What every kind of connection here — to a contact's device, to
//! another of the user's own devices, a channel link — goes by when
//! dialling.
//!
//! - **Of two dialled at once, the one kept** is the one dialled by the
//!   side with the lower key, at both ends (`keeps_own_dial`).
//! - **Who answered at an address** is remembered (`AnsweredAt`): a dial
//!   goes to an address, and whichever device is there answers. A manual
//!   address is told by all of a user's devices and forwarded to one,
//!   and one device's old LAN address can be another's now. Dialling
//!   device B where H answers never finds B, and hands H a connection it
//!   doesn't need each time, so for a while that address is left out
//!   when dialling anyone but H.
//! - **With the screen off** (a phone's; `set_screen_on`), whoever keeps
//!   failing is dialled less and less often, up to `DARK_MOST` apart
//!   (`dark_wait`): a dozing phone redialling its devices and contacts
//!   that are off every minute all night kept its radio awake. The
//!   screen coming on puts the waits back as they are with it on, so
//!   anyone due by those is dialled at once. Moving to another network,
//!   a call held for a new connection, and someone dialling in are as
//!   ever.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// The ports of this process's nodes that dial no one (one node, but for
/// tests): a device set to receive connections only. `peer::timeout_dial`
/// refuses every dial for them, whatever asks.
static RECEIVING_ONLY: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

pub(super) fn set_receiving_only(port: u16, on: bool) {
    use crate::lock::Locked;
    let mut ports = RECEIVING_ONLY.locked();
    ports.retain(|p| *p != port);
    if on {
        ports.push(port);
    }
}

pub(super) fn receiving_only_at(port: u16) -> bool {
    use crate::lock::Locked;
    RECEIVING_ONLY.locked().contains(&port)
}

/// Whether the node on `port` holds back from dialling now: set to
/// receive connections only, and neither the screen on (where there is
/// one to say so: a phone's) nor a call under way. A phone behind NAT
/// whose contact has a port forwarded still finds them directly when
/// it's in use, and a call through a relay can still move to a direct
/// connection.
pub(super) fn holds_back(port: u16) -> bool {
    let now = *STATE.borrow();
    receiving_only_at(port) && now.screen != Some(true) && !now.call
}

/// The longest wait between dials with the screen off.
pub(super) const DARK_MOST: Duration = Duration::from_secs(15 * 60);

/// The device's state as its app says: one for the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Awake {
    /// The screen on or off; `None` where nothing says (a computer's,
    /// taken as on).
    screen: Option<bool>,
    /// A call under way here.
    call: bool,
}

static STATE: LazyLock<tokio::sync::watch::Sender<Awake>> = LazyLock::new(|| tokio::sync::watch::channel(Awake { screen: None, call: false }).0);

/// The screen's come on (true) or gone off, as the phone's app says.
pub fn set_screen_on(on: bool) {
    STATE.send_if_modified(|now| now.screen.replace(on) != Some(on));
}

/// A call's begun here (true) or ended, as the phone's app says.
pub fn set_call_underway(on: bool) {
    STATE.send_if_modified(|now| std::mem::replace(&mut now.call, on) != on);
}

pub(super) fn screen_on() -> bool {
    STATE.borrow().screen.unwrap_or(true)
}

/// Each change of the screen or of a call.
pub(super) fn screen_changes() -> tokio::sync::watch::Receiver<Awake> {
    STATE.subscribe()
}

/// The wait before dialling again with the screen off, after `failed`
/// dials in a row haven't been answered: `first`, twice that, and so on
/// up to `DARK_MOST` — never less than `lit`, the wait with it on.
pub(super) fn dark_wait(failed: u32, first: Duration, lit: Duration) -> Duration {
    first.saturating_mul(1 << failed.saturating_sub(1).min(16)).min(DARK_MOST).max(lit)
}

/// Whether, of two connections between this side (`me`) and `them`
/// dialled at once, the one this side dialled is the one kept. The same
/// answer at both ends, so both keep the same one.
pub(super) fn keeps_own_dial(me: &[u8; 32], them: &[u8; 32]) -> bool {
    me < them
}

/// How long who answered at an address is gone by. Who's there can
/// change (the forward moved to another device, say), and after this the
/// address is tried again for everyone; whoever answers then is noted.
pub(super) const ANSWERED_KEPT: Duration = Duration::from_secs(10 * 60);

/// Which device answered at an address, and when.
#[derive(Default)]
pub(super) struct AnsweredAt(HashMap<SocketAddr, ([u8; 32], Instant)>);

impl AnsweredAt {
    pub(super) fn note(&mut self, address: SocketAddr, device: [u8; 32]) {
        self.note_at(address, device, Instant::now());
    }

    fn note_at(&mut self, address: SocketAddr, device: [u8; 32], at: Instant) {
        self.0.insert(crate::transport::canonical(address), (device, at));
    }

    /// This device has moved: a private address is someone else's on
    /// this network. A public one is the same wherever we are.
    pub(super) fn forget_private(&mut self) {
        self.0.retain(|address, _| match address.ip() {
            IpAddr::V4(ip) => !(ip.is_private() || ip.is_link_local()),
            IpAddr::V6(ip) => !(ip.is_unique_local() || ip.is_unicast_link_local()),
        });
    }

    /// `addresses`, without those where a device other than `device`
    /// answered within `ANSWERED_KEPT`.
    pub(super) fn leaving_out(&mut self, device: &[u8; 32], addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
        self.leaving_out_at(device, addresses, Instant::now())
    }

    fn leaving_out_at(&mut self, device: &[u8; 32], mut addresses: Vec<SocketAddr>, now: Instant) -> Vec<SocketAddr> {
        self.0.retain(|_, (_, at)| now.duration_since(*at) < ANSWERED_KEPT);
        addresses.retain(|a| self.0.get(&crate::transport::canonical(*a)).is_none_or(|(there, _)| there == device));
        addresses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_the_screen_off_the_wait_doubles_up_to_the_most() {
        let minute = Duration::from_secs(60);
        let waits: Vec<u64> = (1..=6).map(|n| dark_wait(n, minute, minute).as_secs()).collect();
        assert_eq!(waits, [60, 120, 240, 480, 900, 900]);
        assert_eq!(dark_wait(1, Duration::from_secs(1), Duration::from_secs(300)), Duration::from_secs(300), "never sooner than with it on");
        assert_eq!(dark_wait(u32::MAX, Duration::from_secs(1), Duration::ZERO), DARK_MOST);
    }

    #[test]
    fn the_lower_key_keeps_its_own_dial_and_the_other_side_agrees() {
        let (low, high) = ([1; 32], [2; 32]);
        assert!(keeps_own_dial(&low, &high));
        assert!(!keeps_own_dial(&high, &low));
    }

    #[test]
    fn an_address_another_device_answered_at_is_left_out_for_all_but_that_one() {
        let (forwarded, own): (SocketAddr, SocketAddr) = ("[2001:db8::1]:1977".parse().unwrap(), "[2001:db8::2]:1977".parse().unwrap());
        let (home, other) = ([1; 32], [2; 32]);
        let mut answered = AnsweredAt::default();
        assert_eq!(answered.leaving_out(&other, vec![forwarded, own]), vec![forwarded, own], "nothing's known yet");
        answered.note(forwarded, home);
        assert_eq!(answered.leaving_out(&other, vec![forwarded, own]), vec![own]);
        assert_eq!(answered.leaving_out(&home, vec![forwarded, own]), vec![forwarded, own], "it's still where that device is");
        answered.note(forwarded, other);
        assert_eq!(answered.leaving_out(&other, vec![forwarded, own]), vec![forwarded, own], "the forward now goes to this one");
        assert_eq!(answered.leaving_out(&home, vec![forwarded, own]), vec![own]);
    }

    #[test]
    fn who_answered_is_gone_by_for_a_while_then_tried_again() {
        let forwarded: SocketAddr = "[2001:db8::1]:1977".parse().unwrap();
        let (home, other) = ([1; 32], [2; 32]);
        let mut answered = AnsweredAt::default();
        let t = Instant::now();
        answered.note_at(forwarded, home, t);
        assert_eq!(answered.leaving_out_at(&other, vec![forwarded], t + ANSWERED_KEPT - Duration::from_secs(1)), vec![]);
        assert_eq!(answered.leaving_out_at(&other, vec![forwarded], t + ANSWERED_KEPT), vec![forwarded], "tried again: who's there may have changed");
    }

    #[test]
    fn moving_forgets_who_answered_at_private_addresses_only() {
        let (public, lan, ula): (SocketAddr, SocketAddr, SocketAddr) = ("11.1.0.2:1977".parse().unwrap(), "192.168.1.12:1977".parse().unwrap(), "[fd00::1]:1977".parse().unwrap());
        let mut answered = AnsweredAt::default();
        for address in [public, lan, ula] {
            answered.note(address, [1; 32]);
        }
        answered.forget_private();
        assert_eq!(answered.leaving_out(&[2; 32], vec![public, lan, ula]), vec![lan, ula]);
    }

    #[test]
    fn an_ipv4_address_is_known_however_the_socket_says_it() {
        let (listed, said): (SocketAddr, SocketAddr) = ("11.1.0.2:1977".parse().unwrap(), "[::ffff:11.1.0.2]:1977".parse().unwrap());
        let mut answered = AnsweredAt::default();
        answered.note(said, [1; 32]);
        assert_eq!(answered.leaving_out(&[2; 32], vec![listed]), vec![]);
    }
}
