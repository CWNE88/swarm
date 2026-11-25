//! Text someone else made, pasted or read from a QR code: a contact's
//! code, a code for adding a device, a peers list. Never a panic.
#![no_main]

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let text = String::from_utf8_lossy(bytes);
    let _ = swarm_app::contact::parse(&text);
    let _ = swarm_app::contact::read_pasted(&text, Some("AAAA"));
    let _ = swarm_core::node::joining::JoinCode::parse(&text);
    let _ = swarm_core::peers::parse(&text);
});
