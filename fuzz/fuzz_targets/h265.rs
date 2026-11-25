//! H.265 from a contact, read as the graphics chip's decoders read it
//! (`video_va`'s): its SPS, PPS and each slice's header. Never a panic.
#![no_main]
use std::collections::HashMap;
use swarm_media::h264::nal_units;
use swarm_media::h265::{self, Pps, SliceHeader, Sps};

libfuzzer_sys::fuzz_target!(|stream: &[u8]| {
    let (mut spss, mut ppss) = (HashMap::new(), HashMap::new());
    for nal in nal_units(stream) {
        match h265::nal_type(nal) {
            Some(h265::SPS) => {
                if let Some(sps) = Sps::parse(nal) {
                    let _ = sps.shown();
                    spss.insert(sps.id, sps);
                }
            }
            Some(h265::PPS) => {
                if let Some(pps) = Pps::parse(nal) {
                    ppss.insert(pps.id, pps);
                }
            }
            Some(kind) if h265::is_slice(kind) => {
                let _ = SliceHeader::parse(nal, &spss, &ppss);
            }
            _ => {}
        }
    }
    let _ = h265::parameter_sets(stream);
    let _ = swarm_core::video::is_key(stream, swarm_core::video::Codec::H265);
});
