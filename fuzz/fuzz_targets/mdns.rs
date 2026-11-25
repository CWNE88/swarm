//! mDNS packets from anyone on the network (`swarm_core::mdns`): read as
//! a query and as an answer. Never a panic.
#![no_main]

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    let _ = swarm_core::mdns::questions(bytes);
    let _ = swarm_core::mdns::heard(bytes);
});
