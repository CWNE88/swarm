//! A contact line: everything someone needs to add you, in one line —
//! `swarm:<key>?a=<address>,<address>&n=<name>` — and its QR code, shown
//! with your details and read by a camera in Add a peer.
//!
//! Addresses change, so a line's only a snapshot of where you were: good
//! for a first connection, after which Swarm keeps track of its peers.

use std::net::IpAddr;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use swarm_core::video::{Format, Picture};

/// What every contact line starts with.
const SCHEME: &str = "swarm:";
/// The most addresses a line carries: the code stays small enough for a
/// camera to read from a screen.
const MOST_ADDRESSES: usize = 6;
/// A camera's picture is read no bigger than this on its longer side:
/// big enough for a code held up to it, and quick to search.
const MOST_READ_SIDE: usize = 1000;

/// A contact line taken apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    pub key: [u8; 32],
    /// As given: IPs, mostly without ports (Swarm's own is taken), and
    /// whatever its owner set by hand (a port, a host name) first.
    pub addresses: Vec<String>,
    /// Blank if none was given.
    pub name: String,
}

impl Contact {
    /// The key as typed into Add a peer.
    pub fn key_text(&self) -> String {
        STANDARD.encode(self.key)
    }

    /// The addresses as typed into Add a peer: comma-separated, as a
    /// peer's address field has them.
    pub fn address_text(&self) -> String {
        self.addresses.join(",")
    }
}

/// Your contact line: your key, the address you set by hand (`manual`:
/// IPs, with ports, or host names, as typed; first, so it's never the
/// one left out), the addresses you can be reached on (those that work
/// from anywhere first; the rest for someone on the same network), and
/// your name.
pub fn line(key: &[u8; 32], manual: &[String], addresses: &[IpAddr], name: &str) -> String {
    let mut all: Vec<String> = Vec::new();
    for item in manual {
        if !all.contains(item) {
            all.push(item.clone());
        }
    }
    for a in addresses {
        let text = a.to_string();
        if !all.contains(&text) && !a.is_loopback() && !a.is_unspecified() && !is_link_local(a) {
            all.push(text);
        }
    }
    all.truncate(MOST_ADDRESSES);
    let addresses = all;
    let mut line = format!("{SCHEME}{}", STANDARD.encode(key));
    let mut joiner = '?';
    if !addresses.is_empty() {
        line += &format!("{joiner}a={}", addresses.join(","));
        joiner = '&';
    }
    if !name.trim().is_empty() {
        line += &format!("{joiner}n={}", escape(name.trim()));
    }
    line
}

/// A peer's contact line, from their entry: the IPs in their address
/// field (a line carries no ports, and no host names), and their name.
pub fn peer_line(key: &[u8; 32], address_field: &str, name: &str) -> String {
    let addresses: Vec<IpAddr> = swarm_core::peers::literal_addresses(address_field).into_iter().map(|a| a.ip()).collect();
    line(key, &[], &addresses, name)
}

fn is_link_local(a: &IpAddr) -> bool {
    match a {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
    }
}

/// A contact line taken apart, if `text` is one.
pub fn parse(text: &str) -> Option<Contact> {
    let text = text.trim();
    let rest = text.get(..SCHEME.len()).filter(|s| s.eq_ignore_ascii_case(SCHEME)).map(|_| &text[SCHEME.len()..])?;
    let (key, query) = rest.split_once('?').unwrap_or((rest, ""));
    let key: [u8; 32] = STANDARD.decode(key).ok()?.try_into().ok()?;
    let mut contact = Contact { key, addresses: Vec::new(), name: String::new() };
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("a", list)) => contact.addresses = list.split(',').map(str::trim).filter(|a| !a.is_empty()).map(String::from).collect(),
            // Someone else's text: no control or direction characters
            // (an escape a terminal would act on, or text running
            // backwards to pass for another name).
            Some(("n", name)) => contact.name = swarm_core::text::single_line(&unescape(name)?).trim().to_string(),
            _ => {}
        }
    }
    Some(contact)
}

/// A name in a line: anything but letters, digits and `-._~` as `%XX`
/// of its UTF-8.
fn escape(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out += &format!("%{byte:02X}");
        }
    }
    out
}

fn unescape(text: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut rest = text.as_bytes();
    while let Some((&byte, after)) = rest.split_first() {
        if byte == b'%' {
            let hex = std::str::from_utf8(after.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &after[2..];
        } else {
            bytes.push(byte);
            rest = after;
        }
    }
    String::from_utf8(bytes).ok()
}

/// The addresses another of your devices gave in its code (a device's
/// code, or a line of it, under You), for Add address: `user` is your own
/// user key, which every device of yours has. Host names are left out, as
/// Add address takes only IPs. What to say if it won't do.
pub fn own_device_addresses(code: &Contact, user: &[u8; 32]) -> Result<String, &'static str> {
    if code.key != *user {
        return Err("that code is someone else's, not one of your devices': nothing was changed");
    }
    let addresses: Vec<String> = swarm_core::peers::literal_addresses(&code.address_text()).into_iter().map(|a| a.to_string()).collect();
    if addresses.is_empty() {
        return Err("that code has no IP addresses in it");
    }
    Ok(addresses.join(","))
}

/// A QR code of `text`, row by row, true for dark, with no margin.
pub fn qr(text: &str) -> Vec<Vec<bool>> {
    let Ok(code) = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M) else { return Vec::new() };
    let side = code.width();
    let colours = code.to_colors();
    colours.chunks(side).map(|row| row.iter().map(|c| *c == qrcode::Color::Dark).collect()).collect()
}

/// A QR code for a terminal: two rows of it to a line of half blocks,
/// dark as the terminal's text, with the light margin a reader needs.
pub fn qr_lines(text: &str) -> Vec<String> {
    let code = qr(text);
    let margin = 2;
    let side = code.len() + 2 * margin;
    let dark = |x: usize, y: usize| x >= margin && y >= margin && code.get(y - margin).and_then(|row| row.get(x - margin)).copied().unwrap_or(false);
    (0..side.div_ceil(2))
        .map(|line| {
            (0..side)
                .map(|x| match (dark(x, line * 2), dark(x, line * 2 + 1)) {
                    (false, false) => '█',
                    (false, true) => '▀',
                    (true, false) => '▄',
                    (true, true) => ' ',
                })
                .collect()
        })
        .collect()
}

/// The first contact line in a QR code in `picture`, if one's in it.
pub fn read(picture: &Picture) -> Option<Contact> {
    qr_texts(picture).iter().find_map(|text| parse(text))
}

/// What every QR code in `picture` says.
pub fn qr_texts(picture: &Picture) -> Vec<String> {
    let (width, height) = (picture.width, picture.height);
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let step = width.max(height).div_ceil(MOST_READ_SIDE);
    let (w, h) = (width / step, height / step);
    let grey: Vec<u8> = match picture.format {
        Format::I420 => {
            let luma = picture.planes().0;
            (0..h).flat_map(|y| (0..w).map(move |x| (y, x))).map(|(y, x)| luma[y * step * width + x * step]).collect()
        }
        Format::Rgb => (0..h)
            .flat_map(|y| (0..w).map(move |x| (y, x)))
            .map(|(y, x)| {
                let i = (y * step * width + x * step) * 3;
                let d = &picture.data;
                ((d[i] as u16 * 77 + d[i + 1] as u16 * 150 + d[i + 2] as u16 * 29) >> 8) as u8
            })
            .collect(),
    };
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| grey[y * w + x]);
    prepared.detect_grids().into_iter().filter_map(|grid| grid.decode().ok()).map(|(_, text)| text).collect()
}

/// What's pasted into a form's "Their code" field, read: nothing yet, a
/// code (`for_contact`, the one being edited, if any, must be whose it
/// is), or not one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pasted {
    Blank,
    Theirs(Contact),
    NotACode,
    /// A code, but for someone other than the contact being edited.
    SomeoneElse,
}

/// Reads `text`, pasted as a code; `for_contact`, the key of the contact
/// being edited (base64, as their form shows it), when there is one.
pub fn read_pasted(text: &str, for_contact: Option<&str>) -> Pasted {
    if text.trim().is_empty() {
        return Pasted::Blank;
    }
    let Some(contact) = parse(text) else { return Pasted::NotACode };
    match for_contact.map(|key| STANDARD.decode(key.trim()).ok()) {
        Some(theirs) if theirs.as_deref() != Some(&contact.key[..]) => Pasted::SomeoneElse,
        _ => Pasted::Theirs(contact),
    }
}

/// What the "Their code" field says under it about what was pasted.
pub fn pasted_note(pasted: &Pasted, editing: bool) -> &'static str {
    match pasted {
        Pasted::Blank => "",
        Pasted::Theirs(_) if editing => "Their addresses are filled in from it: Save to keep them.",
        Pasted::Theirs(_) => "Filled in from it: check, then Save.",
        Pasted::NotACode => "That isn't a swarm: code. A code starts swarm: and is one line.",
        Pasted::SomeoneElse => "That code is someone else's: its key isn't this contact's. Nothing was changed.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        std::array::from_fn(|i| i as u8 * 7)
    }

    /// A line carries key, addresses and name, and comes back as it went.
    #[test]
    fn a_contact_line_comes_back_as_it_went() {
        let addresses: Vec<IpAddr> = ["2001:db8::5", "203.0.113.7", "192.168.1.20"].iter().map(|a| a.parse().unwrap()).collect();
        let text = line(&key(), &[], &addresses, "Ann & Bob ✓");
        assert!(text.starts_with("swarm:"));
        assert!(!text.contains(' '), "one unbroken line: {text}");
        let back = parse(&text).expect("a contact line");
        assert_eq!(back.key, key());
        assert_eq!(back.addresses, ["2001:db8::5", "203.0.113.7", "192.168.1.20"]);
        assert_eq!(back.name, "Ann & Bob ✓");
        assert_eq!(back.address_text(), "2001:db8::5,203.0.113.7,192.168.1.20");
        assert!(swarm_core::peers::validate_address(&back.address_text()).is_ok(), "taken as it is");
        assert_eq!(peerbook_key(&back.key_text()), key());
    }

    /// A name in someone's code with an escape (which a terminal would act
    /// on) or a direction character (text running backwards, to pass for
    /// another name) comes without them.
    #[test]
    fn a_names_control_and_direction_characters_go() {
        for (sent, read) in [("evil\u{1b}[2Jname", "evil [2Jname"), ("Ann\u{202e}nimda", "Ann nimda"), ("\u{2066}Bob", "Bob")] {
            let back = parse(&line(&key(), &[], &[], sent)).expect("a contact line");
            assert_eq!(back.name, read);
        }
    }

    fn peerbook_key(text: &str) -> [u8; 32] {
        crate::peerbook::decode_key(text).unwrap()
    }

    /// No addresses, or no name: still a line. Loopback, link-local and
    /// repeats are left out, and only so many kept.
    #[test]
    fn a_contact_line_keeps_only_addresses_worth_giving() {
        let bare = line(&key(), &[], &[], "");
        assert_eq!(parse(&bare), Some(Contact { key: key(), addresses: Vec::new(), name: String::new() }));
        let addresses: Vec<IpAddr> = ["127.0.0.1", "fe80::1", "::1", "169.254.3.3", "10.0.0.2", "10.0.0.2"]
            .iter()
            .map(|a| a.parse().unwrap())
            .chain((1..=9).map(|i| format!("2001:db8::{i}").parse().unwrap()))
            .collect();
        let back = parse(&line(&key(), &[], &addresses, "")).unwrap();
        assert_eq!(back.addresses.len(), MOST_ADDRESSES);
        assert_eq!(back.addresses[0], "10.0.0.2");
        assert_eq!(back.addresses[1], "2001:db8::1");
    }

    /// The address set by hand comes first, as typed (port, host name
    /// and all), and is never the one left out; what it repeats isn't
    /// given twice. Read back, it fills in an address field as it is.
    #[test]
    fn the_address_set_by_hand_comes_first() {
        let manual: Vec<String> = vec!["home.example.org:5000".into(), "203.0.113.7".into()];
        let addresses: Vec<IpAddr> = ["203.0.113.7"].iter().map(|a| a.parse().unwrap()).chain((1..=9).map(|i| format!("2001:db8::{i}").parse().unwrap())).collect();
        let back = parse(&line(&key(), &manual, &addresses, "Ann")).unwrap();
        assert_eq!(back.addresses.len(), MOST_ADDRESSES);
        assert_eq!(&back.addresses[..3], ["home.example.org:5000", "203.0.113.7", "2001:db8::1"]);
        assert!(swarm_core::peers::validate_address(&back.address_text()).is_ok());
        let ipv6 = parse(&line(&key(), &["[2001:db8::9]:5000".to_string()], &[], "")).unwrap();
        assert_eq!(ipv6.addresses, ["[2001:db8::9]:5000"]);
    }

    /// A peer's line, from their entry: the IPs in their address field,
    /// without ports (a line has none) or host names (not IPs), and their name.
    #[test]
    fn a_peers_contact_line_comes_from_their_entry() {
        let back = parse(&peer_line(&key(), "2001:db8::5, [2001:db8::6]:9000, 203.0.113.7:9000, home.example", "Ann")).unwrap();
        assert_eq!(back, Contact { key: key(), addresses: vec!["2001:db8::5".into(), "2001:db8::6".into(), "203.0.113.7".into()], name: "Ann".into() });
    }

    /// Anything else isn't taken for one.
    #[test]
    fn what_isnt_a_contact_line_isnt_read_as_one() {
        assert_eq!(parse("2001:db8::5"), None);
        assert_eq!(parse("swarm:notbase64!"), None);
        assert_eq!(parse(&format!("swarm:{}", STANDARD.encode([1u8; 31]))), None, "a key's 32 bytes");
        assert_eq!(parse(&format!("swarm:{}?n=%ZZ", STANDARD.encode(key()))), None);
        let later = parse(&format!("  SWARM:{}?x=1&a=10.0.0.1  ", STANDARD.encode(key()))).expect("unknown parts left alone");
        assert_eq!(later.addresses, ["10.0.0.1"]);
    }

    /// A picture of a code, as a camera gives one.
    fn photographed(text: &str, scale: usize, format: Format) -> Picture {
        let code = qr(text);
        let margin = 4;
        let side = (code.len() + 2 * margin) * scale;
        let (width, height) = (side + 300, side + 100);
        let light = |x: usize, y: usize| {
            let (cx, cy) = (x.checked_sub(150), y.checked_sub(50));
            let (Some(cx), Some(cy)) = (cx, cy) else { return true };
            let (mx, my) = (cx / scale, cy / scale);
            !(mx >= margin && my >= margin && code.get(my - margin).and_then(|r| r.get(mx - margin)).copied().unwrap_or(false))
        };
        let grey: Vec<u8> = (0..height).flat_map(|y| (0..width).map(move |x| if light(x, y) { 220 } else { 30 })).collect();
        match format {
            Format::Rgb => Picture::rgb(width, height, grey.iter().flat_map(|g| [*g, *g, *g]).collect()),
            Format::I420 => {
                let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
                let mut data = grey;
                data.resize(width * height + 2 * cw * ch, 128);
                Picture::i420(width, height, data)
            }
        }
    }

    /// A code in a camera's picture is read, in either of its formats,
    /// a large picture too.
    #[test]
    fn a_contact_lines_code_is_read_from_a_picture() {
        let addresses: Vec<IpAddr> = ["2001:db8:1234:5678:9abc:def0:1234:5678", "203.0.113.7", "192.168.1.20", "fd00::20"].iter().map(|a| a.parse().unwrap()).collect();
        let text = line(&key(), &[], &addresses, "Ann");
        assert_eq!(read(&photographed(&text, 4, Format::Rgb)), parse(&text));
        assert_eq!(read(&photographed(&text, 4, Format::I420)), parse(&text));
        assert_eq!(read(&photographed(&text, 14, Format::I420)), parse(&text), "bigger than it's read at");
        assert_eq!(read(&photographed("hello", 4, Format::Rgb)), None, "a code, but not a contact line");
        assert_eq!(read(&Picture::rgb(64, 48, vec![200; 64 * 48 * 3])), None);
    }

    /// A code for adding a device, with several addresses, is read from
    /// a picture as it was given out.
    #[test]
    fn a_device_codes_qr_code_is_read_from_a_picture() {
        use swarm_core::node::joining::JoinCode;
        let addresses = "[2001:db8:1234:5678:9abc:def0:1234:5678]:1977,[fd00::20]:1977,203.0.113.7:1977,192.168.1.20:1977".to_string();
        let code = JoinCode { user: key(), addresses, secret: std::array::from_fn(|i| i as u8) };
        let read = qr_texts(&photographed(&code.line(), 4, Format::I420));
        assert_eq!(read.iter().find_map(|t| JoinCode::parse(t).ok()), Some(code));
    }

    /// For a terminal: half blocks, two rows of the code to a line.
    #[test]
    fn a_contact_lines_code_is_drawn_for_a_terminal() {
        let text = line(&key(), &[], &["203.0.113.7".parse().unwrap()], "Ann");
        let side = qr(&text).len() + 4;
        let lines = qr_lines(&text);
        assert_eq!(lines.len(), side.div_ceil(2));
        assert!(lines.iter().all(|l| l.chars().count() == side));
        assert!(lines[0].chars().all(|c| c == '█'), "the margin's light");
        let code = qr(&text);
        for (y, row) in code.iter().enumerate() {
            for (x, dark) in row.iter().enumerate() {
                let (cell, top) = (lines[(y + 2) / 2].chars().nth(x + 2).unwrap(), (y + 2) % 2 == 0);
                let drawn_dark = match cell {
                    ' ' => true,
                    '█' => false,
                    '▀' => !top,
                    _ => top,
                };
                assert_eq!(drawn_dark, *dark, "square {x},{y}");
            }
        }
    }

    #[test]
    fn a_device_code_gives_its_addresses_only_to_a_device_of_the_same_user() {
        let code = parse(&line(&key(), &["home.example.org:5000".to_string()], &["203.0.113.7".parse().unwrap()], "Ann")).unwrap();
        assert_eq!(own_device_addresses(&code, &key()).unwrap(), "203.0.113.7:1977");
        assert!(own_device_addresses(&code, &[3; 32]).unwrap_err().contains("someone else's"));
        let bare = parse(&line(&key(), &[], &[], "")).unwrap();
        assert!(own_device_addresses(&bare, &key()).unwrap_err().contains("no IP"));
    }

    #[test]
    fn a_pasted_code_is_read_and_checked_against_the_contact_edited() {
        let line = line(&key(), &[], &["203.0.113.7".parse().unwrap()], "Ann");
        let Pasted::Theirs(read) = read_pasted(&format!("  {line}\n"), None) else { panic!("a code") };
        assert_eq!((read.address_text().as_str(), read.name.as_str()), ("203.0.113.7", "Ann"));
        assert_eq!(read_pasted("", None), Pasted::Blank);
        assert_eq!(read_pasted("203.0.113.7", None), Pasted::NotACode);
        assert!(matches!(read_pasted(&line, Some(&STANDARD.encode(key()))), Pasted::Theirs(_)), "theirs");
        assert_eq!(read_pasted(&line, Some(&STANDARD.encode([3u8; 32]))), Pasted::SomeoneElse);
        assert_eq!(read_pasted(&line, Some("")), Pasted::SomeoneElse, "a contact always has a key");
    }
}
