//! A call's video pieces, as a contact might send them: any number, any
//! count and index, sent twice or never, frames out of order. Never a
//! panic, and never more held than `VideoDecoder::push` allows
//! (`check_held`). What comes whole is decoded in OpenH264, as it would be.
#![no_main]
use arbitrary::Arbitrary;
use swarm_core::video::{VideoDecoder, VideoPiece};

#[derive(Arbitrary, Debug)]
struct Piece {
    /// Few frame numbers, so pieces meet; sometimes far ahead or behind.
    frame: u8,
    leap: bool,
    index: u16,
    count: u16,
    key: bool,
    h265: bool,
    small: bool,
    data: Data,
}

#[derive(Arbitrary, Debug)]
enum Data {
    Bytes(Vec<u8>),
    /// A big piece without a big input: what fills the caps.
    Filled(u8, u32),
}

libfuzzer_sys::fuzz_target!(|pieces: Vec<Piece>| {
    let Ok(mut decoder) = VideoDecoder::software() else { return };
    for piece in pieces {
        let data = match piece.data {
            Data::Bytes(bytes) => bytes,
            Data::Filled(byte, len) => vec![byte; len as usize % (3 << 20)],
        };
        let frame = if piece.leap { u32::from(piece.frame).wrapping_mul(0x0100_0001).wrapping_sub(1 << 31) } else { u32::from(piece.frame) };
        let _ = decoder.push(VideoPiece { frame, index: piece.index, count: piece.count, key: piece.key, h265: piece.h265, small: piece.small, data });
        decoder.check_held();
    }
});
