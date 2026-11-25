//! H.264 from a contact, read as the graphics chip's decoders read it
//! before handing it over (`video_va`'s): its units, the SPS, PPS and
//! each slice's header. Never a panic; and taking out the bytes that stop
//! a start code appearing undoes putting them in.
#![no_main]
use std::collections::HashMap;
use swarm_media::h264::{escape, nal_units, unescape, Pps, SliceHeader, Sps};

libfuzzer_sys::fuzz_target!(|stream: &[u8]| {
    let (mut spss, mut ppss) = (HashMap::new(), HashMap::new());
    for nal in nal_units(stream) {
        match nal.first().map(|b| b & 0x1f) {
            Some(7) => {
                if let Some(sps) = Sps::parse(nal) {
                    let _ = sps.shown();
                    let _ = sps.height_in_mbs();
                    let _ = sps.max_frame_num();
                    spss.insert(sps.id, sps);
                }
            }
            Some(8) => {
                if let Some(pps) = Pps::parse(nal) {
                    ppss.insert(pps.id, pps);
                }
            }
            Some(1 | 5) => {
                let _ = SliceHeader::parse(nal, &spss, &ppss);
            }
            _ => {}
        }
        assert_eq!(unescape(&escape(nal)), nal, "escaping, then unescaping, gives back what was escaped");
    }
    let _ = swarm_media::common::parameter_sets(stream);
    let _ = swarm_media::common::sps_size(stream);
    let _ = swarm_core::video::is_key(stream, swarm_core::video::Codec::H264);
});
