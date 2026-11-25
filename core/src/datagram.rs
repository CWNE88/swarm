//! Sealing for cover slots sent as QUIC datagrams (see `protocol`'s
//! cover functions). QUIC's own encryption isn't enough on its own — its
//! TLS layer is deliberately unauthenticated (see `transport`) — so each
//! datagram is sealed again, ChaCha20-Poly1305, under a key that only
//! ever travelled inside the authenticated Noise session: each side picks
//! a fresh random key for what *it* sends and tells the other in its
//! `Hello` (see `protocol::greet`). One key per direction, and a sequence
//! number per datagram as the nonce, so a nonce is never reused.
//!
//! Datagrams can be lost, duplicated or reordered, so — unlike the Noise
//! stream — each one carries its sequence number in the clear (inside
//! QUIC's own encryption, so not visible on the wire), and the receiving
//! side refuses any it has already seen or that's too old to tell.

use anyhow::{bail, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};

/// What sealing adds to a datagram: the sequence number and the tag.
pub const OVERHEAD: usize = 8 + 16;

/// How far behind the newest datagram an older one can arrive and still
/// be accepted (it was reordered, not replayed).
const REPLAY_WINDOW: u64 = 64;

fn nonce(seq: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&seq.to_be_bytes());
    Nonce::assume_unique_for_key(bytes)
}

fn key(bytes: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, bytes).expect("32 bytes is the ChaCha20-Poly1305 key size"))
}

/// Seals what this side sends.
pub struct Sealer {
    key: LessSafeKey,
    next_seq: u64,
}

impl Sealer {
    pub fn new(key_bytes: &[u8; 32]) -> Self {
        Sealer { key: key(key_bytes), next_seq: 0 }
    }

    /// `plaintext` sealed as the next datagram: `[seq][ciphertext + tag]`.
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut out = Vec::with_capacity(OVERHEAD + plaintext.len());
        out.extend_from_slice(&seq.to_be_bytes());
        let mut body = plaintext.to_vec();
        self.key.seal_in_place_append_tag(nonce(seq), Aad::empty(), &mut body).expect("sealing can't fail for in-range input");
        out.extend_from_slice(&body);
        out
    }
}

/// Opens what the other side sends, refusing anything replayed.
pub struct Opener {
    key: LessSafeKey,
    /// The key before this one, while datagrams sealed with it may still
    /// be on their way (see `rotate`).
    previous: Option<Box<Opener>>,
    /// The highest sequence number accepted so far, and which of the
    /// `REPLAY_WINDOW` below it have been (bit n = `highest - n`).
    highest: Option<u64>,
    seen: u64,
}

impl Opener {
    pub fn new(key_bytes: &[u8; 32]) -> Self {
        Opener { key: key(key_bytes), previous: None, highest: None, seen: 0 }
    }

    /// The other side's new key (it sends one now and then, and switches
    /// to it a little later): opened with from now on, the one before
    /// kept too, for what was already on its way. Any older one goes.
    pub fn rotate(&mut self, key_bytes: &[u8; 32]) {
        let mut old = std::mem::replace(self, Opener::new(key_bytes));
        old.previous = None;
        self.previous = Some(Box::new(old));
    }

    pub fn open(&mut self, datagram: &[u8]) -> Result<Vec<u8>> {
        match self.open_with_this(datagram) {
            Err(e) => match &mut self.previous {
                Some(previous) => previous.open_with_this(datagram),
                None => Err(e),
            },
            ok => ok,
        }
    }

    fn open_with_this(&mut self, datagram: &[u8]) -> Result<Vec<u8>> {
        if datagram.len() < OVERHEAD {
            bail!("datagram too short");
        }
        let seq = u64::from_be_bytes(datagram[..8].try_into().expect("8 bytes"));
        if let Some(highest) = self.highest
            && seq <= highest && (highest - seq >= REPLAY_WINDOW || self.seen & (1 << (highest - seq)) != 0) {
                bail!("datagram replayed or too old");
            }
        let mut body = datagram[8..].to_vec();
        let plaintext_len = self.key.open_in_place(nonce(seq), Aad::empty(), &mut body).map_err(|_| anyhow::anyhow!("datagram failed to open"))?.len();
        body.truncate(plaintext_len);
        match self.highest {
            Some(highest) if seq <= highest => self.seen |= 1 << (highest - seq),
            Some(highest) => {
                let shift = seq - highest;
                self.seen = if shift >= 64 { 0 } else { self.seen << shift };
                self.seen |= 1;
                self.highest = Some(seq);
            }
            None => {
                self.highest = Some(seq);
                self.seen = 1;
            }
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens_in_any_order_but_never_twice() {
        let k = [7u8; 32];
        let mut s = Sealer::new(&k);
        let mut o = Opener::new(&k);
        let d0 = s.seal(b"zero");
        let d1 = s.seal(b"one");
        let d2 = s.seal(b"two");
        assert_eq!(o.open(&d2).unwrap(), b"two");
        assert_eq!(o.open(&d0).unwrap(), b"zero", "reordered is fine");
        assert!(o.open(&d0).is_err(), "replayed is not");
        assert_eq!(o.open(&d1).unwrap(), b"one");
        assert!(o.open(&d2).is_err());
    }

    /// A new key: what's sealed with it opens, so does what was sealed
    /// with the one before (still on its way), and neither can be replayed;
    /// the key before that no longer opens anything.
    #[test]
    fn a_new_key_opens_and_the_last_one_still_does_for_a_while() {
        let mut first = Sealer::new(&[1u8; 32]);
        let mut o = Opener::new(&[1u8; 32]);
        let late = first.seal(b"late");
        o.open(&first.seal(b"before")).unwrap();
        o.rotate(&[2u8; 32]);
        let mut second = Sealer::new(&[2u8; 32]);
        let d = second.seal(b"after");
        assert_eq!(o.open(&d).unwrap(), b"after");
        assert!(o.open(&d).is_err(), "not twice");
        assert_eq!(o.open(&late).unwrap(), b"late", "one sealed before the switch, still on its way");
        assert!(o.open(&late).is_err(), "not twice either");
        let older = first.seal(b"two keys ago");
        o.rotate(&[3u8; 32]);
        assert!(o.open(&older).is_err());
        assert_eq!(o.open(&second.seal(b"one key ago")).unwrap(), b"one key ago");
    }

    #[test]
    fn a_tampered_or_wrongly_keyed_datagram_is_refused() {
        let mut s = Sealer::new(&[1u8; 32]);
        let mut d = s.seal(b"hello");
        assert!(Opener::new(&[2u8; 32]).open(&d).is_err());
        *d.last_mut().unwrap() ^= 1;
        assert!(Opener::new(&[1u8; 32]).open(&d).is_err());
    }

    #[test]
    fn anything_older_than_the_window_is_refused() {
        let k = [3u8; 32];
        let mut s = Sealer::new(&k);
        let mut o = Opener::new(&k);
        let old = s.seal(b"old");
        for _ in 0..REPLAY_WINDOW {
            o.open(&s.seal(b"newer")).unwrap();
        }
        assert!(o.open(&old).is_err());
    }
}
