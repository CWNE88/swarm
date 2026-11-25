//! mDNS packets (RFC 6762) for DNS-SD (RFC 6763): being found on the
//! network a device is on, and finding others there, under
//! `_swarm._udp.local`. Only what that needs: a query for the service,
//! and an answer with its four kinds of record —
//!
//! - **PTR** `_swarm._udp.local` → this device's instance,
//!   `<name> (<token>)._swarm._udp.local`;
//! - **SRV** instance → the port and a host name of its own
//!   (`swarm-<token>.local`, never the computer's, which is the
//!   system's to answer for);
//!
//! The token (`token`) only keeps a user's devices' labels apart, as
//! mDNS needs: a hash of the device key, telling nothing of the device
//! (not its name, not its key). Contacts never see a device's name, and
//! neither does anyone here.
//! - **TXT** instance → `v=1`, `k=<user key, base64>`, `n=<name>`;
//! - **A**/**AAAA** host → this device's addresses on that network.
//!
//! The addresses travel as A/AAAA records, never in the TXT, so it stays
//! small however many a device has. Everything read here comes from
//! anyone on the network: nothing is trusted beyond being parsed safely
//! (`fuzz/fuzz_targets/mdns.rs`), and a key found this way is only ever
//! proven by connecting (Noise), and accepted by its owner.
//!
//! Sockets, interfaces and timing are `swarm_app::discovery`'s.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use base64::{engine::general_purpose::STANDARD, Engine as _};

/// mDNS's port and groups.
pub const PORT: u16 = 5353;
pub const GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
/// How long an answer's records are good for, in seconds: short, so
/// someone who leaves without saying so drops out of the list soon (a
/// browser asks again well before then).
pub const TTL: u32 = 120;
/// The longest packet made or read: mDNS's own limit for one that isn't
/// split (RFC 6762 §17, 9000 less the headers).
pub const MOST_BYTES: usize = 8972;
/// The version written in the TXT (`v=`): one with another is ignored.
const VERSION: &str = "1";

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
/// The top bit of a record's class: "this is all of them" (unique
/// records); of a question's: "answer me directly".
const TOP_BIT: u16 = 0x8000;
/// Most addresses an answer carries, or is read with.
const MOST_ADDRESSES: usize = 8;
/// Most records read from one packet.
const MOST_RECORDS: usize = 64;

/// The service's name.
pub fn service() -> Vec<String> {
    ["_swarm", "_udp", "local"].iter().map(|s| s.to_string()).collect()
}

/// Six hex digits from a one-way hash of `device_key`: the same for a
/// device every time, different between devices, and no use for
/// anything else (it can't be turned back into the key, nor matched
/// with it anywhere else, as it's hashed with a label of its own).
pub fn token(device_key: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::new().chain_update(b"swarm mdns label").chain_update(device_key).finalize();
    hash[..3].iter().map(|b| format!("{b:02x}")).collect()
}

/// What one device says about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advert {
    /// Its instance's label: `<name> (<token>)`, unique among the
    /// user's devices; at most 63 bytes (cut short if longer).
    pub instance: String,
    /// Its host's label (`swarm-<token>`).
    pub host: String,
    pub port: u16,
    /// The user key.
    pub key: [u8; 32],
    /// The user's name ("" none).
    pub name: String,
    /// Where it is on the network the answer goes out on.
    pub addresses: Vec<IpAddr>,
}

impl Advert {
    pub fn instance_name(&self) -> Vec<String> {
        let mut name = vec![cut(&self.instance, 63)];
        name.extend(service());
        name
    }

    pub fn host_name(&self) -> Vec<String> {
        vec![cut(&self.host, 63), "local".to_string()]
    }

    /// Whether a question is about it: the service, its instance or its
    /// host.
    pub fn asked(&self, question: &Question) -> bool {
        let about = |name: &[String], types: &[u16]| same_name(&question.name, name) && (question.kind == TYPE_ANY || types.contains(&question.kind));
        about(&service(), &[TYPE_PTR]) || about(&self.instance_name(), &[TYPE_SRV, TYPE_TXT]) || about(&self.host_name(), &[TYPE_A, TYPE_AAAA])
    }
}

/// Someone found: what one answer said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heard {
    /// Their instance's label, as sent.
    pub instance: String,
    pub key: [u8; 32],
    pub name: String,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
    /// Seconds it's good for; 0: they've gone (a goodbye).
    pub ttl: u32,
}

/// One question in a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub name: Vec<String>,
    pub kind: u16,
    /// To be answered directly, not to the group.
    pub unicast: bool,
}

/// A query for the service.
pub fn query() -> Vec<u8> {
    let mut out = header(0, 1, 0, 0);
    name(&mut out, &service());
    out.extend_from_slice(&TYPE_PTR.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out
}

/// An answer saying all of `advert`, good for `ttl` seconds (0: a
/// goodbye — it's gone).
pub fn answer(advert: &Advert, ttl: u32) -> Vec<u8> {
    let addresses: Vec<&IpAddr> = advert.addresses.iter().take(MOST_ADDRESSES).collect();
    let mut out = header(0x8400, 0, 3 + addresses.len() as u16, 0);
    let instance = advert.instance_name();
    let host = advert.host_name();
    record(&mut out, &service(), TYPE_PTR, false, ttl, |d| name(d, &instance));
    record(&mut out, &instance, TYPE_SRV, true, ttl, |d| {
        d.extend_from_slice(&0u16.to_be_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());
        d.extend_from_slice(&advert.port.to_be_bytes());
        name(d, &host);
    });
    record(&mut out, &instance, TYPE_TXT, true, ttl, |d| {
        for item in [format!("v={VERSION}"), format!("k={}", STANDARD.encode(advert.key)), format!("n={}", cut(&advert.name, 253))] {
            d.push(item.len() as u8);
            d.extend_from_slice(item.as_bytes());
        }
    });
    for a in addresses {
        match a {
            IpAddr::V4(v4) => record(&mut out, &host, TYPE_A, true, ttl, |d| d.extend_from_slice(&v4.octets())),
            IpAddr::V6(v6) => record(&mut out, &host, TYPE_AAAA, true, ttl, |d| d.extend_from_slice(&v6.octets())),
        }
    }
    out
}

/// A query's questions; `None` if it isn't one (or can't be read).
pub fn questions(packet: &[u8]) -> Option<Vec<Question>> {
    let mut r = Reader { bytes: packet, at: 0 };
    let (_, flags, count) = (r.u16()?, r.u16()?, r.u16()?);
    if flags & 0x8000 != 0 {
        return None;
    }
    r.skip(6)?;
    let mut found = Vec::new();
    for _ in 0..count.min(MOST_RECORDS as u16) {
        let name = r.name()?;
        let (kind, class) = (r.u16()?, r.u16()?);
        found.push(Question { name, kind, unicast: class & TOP_BIT != 0 });
    }
    Some(found)
}

/// Everyone an answer tells of, under the service: each instance with a
/// TXT this version reads and a key in it. Anything else (other
/// services, other versions, what can't be read) is left out.
pub fn heard(packet: &[u8]) -> Vec<Heard> {
    read_heard(packet).unwrap_or_default()
}

fn read_heard(packet: &[u8]) -> Option<Vec<Heard>> {
    let mut r = Reader { bytes: packet, at: 0 };
    let (_, flags) = (r.u16()?, r.u16()?);
    if flags & 0x8000 == 0 {
        return None;
    }
    let questions = r.u16()?;
    let records = r.u16()? as usize + r.u16()? as usize + r.u16()? as usize;
    for _ in 0..questions.min(MOST_RECORDS as u16) {
        r.name()?;
        r.skip(4)?;
    }
    let service = service();
    let mut instances: Vec<(String, u32)> = Vec::new();
    let mut srvs: Vec<(Vec<String>, u16, Vec<String>)> = Vec::new();
    let mut txts: Vec<(Vec<String>, Vec<Vec<u8>>)> = Vec::new();
    let mut addresses: Vec<(Vec<String>, IpAddr)> = Vec::new();
    for _ in 0..records.min(MOST_RECORDS) {
        let owner = r.name()?;
        let (kind, _class, ttl, len) = (r.u16()?, r.u16()?, r.u32()?, r.u16()? as usize);
        let start = r.at;
        let end = start.checked_add(len).filter(|e| *e <= packet.len())?;
        match kind {
            TYPE_PTR if same_name(&owner, &service) => {
                let target = r.name()?;
                if target.len() == service.len() + 1 && same_name(&target[1..], &service) && !instances.iter().any(|(i, _)| *i == target[0]) {
                    instances.push((target[0].clone(), ttl));
                }
            }
            TYPE_SRV if len >= 7 => {
                r.skip(4)?;
                let port = r.u16()?;
                srvs.push((owner, port, r.name()?));
            }
            TYPE_TXT => {
                let mut strings = Vec::new();
                let mut at = start;
                while at < end {
                    let n = packet[at] as usize;
                    let s = packet.get(at + 1..at + 1 + n).filter(|_| at + 1 + n <= end)?;
                    strings.push(s.to_vec());
                    at += 1 + n;
                }
                txts.push((owner, strings));
            }
            TYPE_A if len == 4 && addresses.len() < MOST_RECORDS => {
                let o: [u8; 4] = packet[start..end].try_into().ok()?;
                addresses.push((owner, IpAddr::V4(Ipv4Addr::from(o))));
            }
            TYPE_AAAA if len == 16 && addresses.len() < MOST_RECORDS => {
                let o: [u8; 16] = packet[start..end].try_into().ok()?;
                addresses.push((owner, IpAddr::V6(Ipv6Addr::from(o))));
            }
            _ => {}
        }
        r.at = end;
    }
    let mut found = Vec::new();
    for (instance, ttl) in instances {
        let mut full = vec![instance.clone()];
        full.extend(service.iter().cloned());
        let Some((_, strings)) = txts.iter().find(|(owner, _)| same_name(owner, &full)) else { continue };
        let value = |k: &str| {
            strings.iter().find_map(|s| {
                let s = std::str::from_utf8(s).ok()?;
                let (key, value) = s.split_once('=')?;
                key.eq_ignore_ascii_case(k).then(|| value.to_string())
            })
        };
        if value("v").as_deref() != Some(VERSION) {
            continue;
        }
        let Some(key) = value("k").and_then(|k| STANDARD.decode(k).ok()).and_then(|k| <[u8; 32]>::try_from(k).ok()) else { continue };
        let name = crate::text::single_line(&value("n").unwrap_or_default());
        let (port, host) = match srvs.iter().find(|(owner, _, _)| same_name(owner, &full)) {
            Some((_, port, host)) => (*port, Some(host)),
            None => (crate::transport::DEFAULT_PORT, None),
        };
        let mut ips: Vec<IpAddr> = Vec::new();
        for (owner, ip) in &addresses {
            if host.is_some_and(|h| same_name(owner, h)) && !ips.contains(ip) && ips.len() < MOST_ADDRESSES {
                ips.push(*ip);
            }
        }
        found.push(Heard { instance, key, name, port, addresses: ips, ttl });
    }
    Some(found)
}

fn header(flags: u16, questions: u16, answers: u16, additional: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(512);
    for v in [0, flags, questions, answers, 0, additional] {
        out.extend_from_slice(&u16::to_be_bytes(v));
    }
    out
}

/// A name, written whole (no compression: answers are small).
fn name(out: &mut Vec<u8>, labels: &[String]) {
    for label in labels {
        let label = cut(label, 63);
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

fn record(out: &mut Vec<u8>, owner: &[String], kind: u16, unique: bool, ttl: u32, data: impl FnOnce(&mut Vec<u8>)) {
    name(out, owner);
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(CLASS_IN | if unique { TOP_BIT } else { 0 }).to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    let mut d = Vec::new();
    data(&mut d);
    out.extend_from_slice(&(d.len() as u16).to_be_bytes());
    out.extend_from_slice(&d);
}

/// DNS names are the same whatever the case of their ASCII letters.
fn same_name(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// `text` cut to at most `most` bytes, on a character's edge.
fn cut(text: &str, most: usize) -> String {
    let mut end = text.len().min(most);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn skip(&mut self, n: usize) -> Option<()> {
        self.at = self.at.checked_add(n).filter(|e| *e <= self.bytes.len())?;
        Some(())
    }

    fn u16(&mut self) -> Option<u16> {
        let b = self.bytes.get(self.at..self.at + 2)?;
        self.at += 2;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let b = self.bytes.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A name, following compression pointers (only ever backwards, and
    /// a bounded number of them), its labels as text (anything that isn't
    /// UTF-8 made so).
    fn name(&mut self) -> Option<Vec<String>> {
        let mut labels = Vec::new();
        let mut at = self.at;
        let mut jumped = false;
        let mut total = 0usize;
        for _ in 0..128 {
            let len = *self.bytes.get(at)? as usize;
            match len {
                0 => {
                    if !jumped {
                        self.at = at + 1;
                    }
                    return Some(labels);
                }
                1..=63 => {
                    let label = self.bytes.get(at + 1..at + 1 + len)?;
                    total += len + 1;
                    if total > 255 {
                        return None;
                    }
                    labels.push(String::from_utf8_lossy(label).into_owned());
                    at += 1 + len;
                }
                0xC0..=0xFF => {
                    let low = *self.bytes.get(at + 1)? as usize;
                    let to = ((len & 0x3F) << 8) | low;
                    if to >= at {
                        return None;
                    }
                    if !jumped {
                        self.at = at + 2;
                        jumped = true;
                    }
                    at = to;
                }
                _ => return None,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advert() -> Advert {
        Advert {
            instance: "Owen (laptop)".into(),
            host: "swarm-abcd1234".into(),
            port: 1977,
            key: [7; 32],
            name: "Owen".into(),
            addresses: vec!["192.168.1.20".parse().unwrap(), "fe80::1234".parse().unwrap()],
        }
    }

    #[test]
    fn an_answer_is_heard_as_it_was_said() {
        let a = advert();
        let heard = heard(&answer(&a, TTL));
        assert_eq!(heard, [Heard { instance: a.instance.clone(), key: a.key, name: a.name.clone(), port: 1977, addresses: a.addresses.clone(), ttl: TTL }]);
        assert_eq!(super::heard(&answer(&a, 0))[0].ttl, 0, "a goodbye");
    }

    #[test]
    fn small_however_many_addresses() {
        let mut a = advert();
        a.name = "é".repeat(400);
        a.instance = "ü".repeat(100);
        a.addresses = (0..40u8).map(|i| IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, i as u16))).collect();
        let packet = answer(&a, TTL);
        assert!(packet.len() < 1300, "{} bytes", packet.len());
        let h = &heard(&packet)[0];
        assert_eq!(h.addresses.len(), MOST_ADDRESSES);
        assert!(h.name.len() <= 253 && h.name.chars().all(|c| c == 'é'), "cut on a character's edge");
        assert!(h.instance.len() <= 63);
    }

    #[test]
    fn a_token_is_steady_differs_between_devices_and_isnt_the_key() {
        let (a, b) = (token(&[1; 32]), token(&[2; 32]));
        assert_eq!(a, token(&[1; 32]));
        assert_ne!(a, b);
        assert_eq!(a.len(), 6);
        assert!(!"0101010101".contains(&a));
    }

    #[test]
    fn our_query_asks_for_the_service_and_is_answered() {
        let q = questions(&query()).unwrap();
        assert_eq!(q, [Question { name: service(), kind: TYPE_PTR, unicast: false }]);
        assert!(advert().asked(&q[0]));
        assert!(questions(&answer(&advert(), TTL)).is_none(), "an answer isn't a query");
        let other = Question { name: vec!["_http".into(), "_tcp".into(), "local".into()], kind: TYPE_PTR, unicast: false };
        assert!(!advert().asked(&other));
        let host = Question { name: vec!["SWARM-ABCD1234".into(), "local".into()], kind: TYPE_AAAA, unicast: true };
        assert!(advert().asked(&host), "names are the same whatever their case");
    }

    #[test]
    fn compressed_names_are_followed_but_never_forwards_or_round_in_circles() {
        let a = advert();
        let mut out = header(0x8400, 0, 2, 0);
        let ptr_target_at = out.len() + 19 + 8 + 2;
        record(&mut out, &service(), TYPE_PTR, false, TTL, |d| name(d, &a.instance_name()));
        assert_eq!(out[ptr_target_at] as usize, a.instance.len(), "where the instance's name starts");
        out.extend_from_slice(&[0xC0 | (ptr_target_at >> 8) as u8, ptr_target_at as u8]);
        out.extend_from_slice(&TYPE_TXT.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&TTL.to_be_bytes());
        let txt = format!("\x03v=1\x2ek={}\x06n=Owen", STANDARD.encode(a.key));
        out.extend_from_slice(&(txt.len() as u16).to_be_bytes());
        out.extend_from_slice(txt.as_bytes());
        let h = heard(&out);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].key, h[0].name.as_str(), h[0].port), (a.key, "Owen", crate::transport::DEFAULT_PORT));
        let mut looped = header(0x8400, 0, 1, 0);
        let at = looped.len();
        looped.extend_from_slice(&[0xC0, at as u8, 0, 12, 0, 1, 0, 0, 0, 120, 0, 0]);
        assert!(heard(&looped).is_empty());
    }

    #[test]
    fn other_versions_and_keyless_answers_are_left_out() {
        let mut packet = answer(&advert(), TTL);
        let at = packet.windows(3).position(|w| w == b"v=1").unwrap();
        packet[at + 2] = b'2';
        assert!(heard(&packet).is_empty());
        for cut in 0..answer(&advert(), TTL).len() {
            let _ = heard(&answer(&advert(), TTL)[..cut]);
            let _ = questions(&query()[..cut.min(query().len())]);
        }
    }
}
