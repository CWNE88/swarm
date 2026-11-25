//! A frame from the wire: any bytes decode to a frame or an error, never
//! a panic, and what decodes encodes back the same (`protocol::fuzz_frame`).
#![no_main]
libfuzzer_sys::fuzz_target!(|bytes: &[u8]| swarm_core::protocol::fuzz_frame(bytes));
