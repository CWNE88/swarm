//! Cover slots, one after another on one connection, as the other side
//! might send them: never a panic, and whatever comes out whole is read as
//! the frame it'd be taken for.
#![no_main]
use swarm_core::cover::Inbound;

libfuzzer_sys::fuzz_target!(|slots: Vec<Vec<u8>>| {
    let mut inbound = Inbound::default();
    for slot in &slots {
        let _ = inbound.feed(slot);
        let _ = inbound.received();
        let _ = inbound.take_ack_due();
        while let Some(item) = inbound.pop_ready() {
            swarm_core::protocol::fuzz_frame(&item);
        }
    }
});
