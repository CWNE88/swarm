//! Noise handshake and the resulting encrypted `Session`.
//!
//! Pattern: `Noise_IK`, same as `phone`'s call setup — the initiator
//! (dialer) already knows the responder's static key (it came from the
//! peer list), while the initiator's own static key rides inside the
//! first handshake message, authenticated but not pre-shared. So the
//! responder doesn't need the dialer in its own peer list for the crypto
//! to work; it learns (and can verify) the dialer's claimed identity as
//! part of the handshake.
//!
//! This is the layer that actually enforces "pubkey is identity" (see
//! `projectoverview`). QUIC's own TLS is deliberately left unverified
//! (see `transport.rs`) — it's there for multiplexing, loss recovery and
//! opportunistic wire encryption, not for authentication. Real peer
//! authentication happens here, over the QUIC stream, bound to the same
//! X25519 identity used everywhere else in this project.
//!
//! Unlike `phone`'s version, this uses `snow`'s ordinary `TransportState`
//! (auto-incrementing nonce) rather than `StatelessTransportState`: a
//! QUIC stream is already ordered and reliable, so there's no need to
//! carry an explicit sequence number the way out-of-order UDP audio
//! packets require.

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, TransportState};

const NOISE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// Comfortably larger than any Noise handshake message this pattern
/// produces (an ephemeral key, an encrypted static key, and a couple of
/// 16-byte tags — well under 200 bytes).
const MAX_HANDSHAKE_MESSAGE: usize = 512;

fn params() -> NoiseParams {
    NOISE_PATTERN
        .parse()
        .expect("NOISE_PATTERN is a valid, statically-known pattern string")
}

pub struct Handshake {
    state: HandshakeState,
}

impl Handshake {
    /// Dialer side: we already know the remote peer's public key from
    /// the peer list.
    pub fn initiator(
        local_private_key: &[u8; 32],
        remote_public_key: &[u8; 32],
    ) -> Result<Self, snow::Error> {
        let state = Builder::new(params())
            .local_private_key(local_private_key)
            .remote_public_key(remote_public_key)
            .build_initiator()?;
        Ok(Handshake { state })
    }

    /// Listener side: no remote key needed in advance — `IK` delivers it
    /// (authenticated) in the first message.
    pub fn responder(local_private_key: &[u8; 32]) -> Result<Self, snow::Error> {
        let state = Builder::new(params())
            .local_private_key(local_private_key)
            .build_responder()?;
        Ok(Handshake { state })
    }

    pub fn write_message(&mut self) -> Result<Vec<u8>, snow::Error> {
        let mut buf = [0u8; MAX_HANDSHAKE_MESSAGE];
        let len = self.state.write_message(&[], &mut buf)?;
        Ok(buf[..len].to_vec())
    }

    pub fn read_message(&mut self, message: &[u8]) -> Result<(), snow::Error> {
        let mut buf = [0u8; MAX_HANDSHAKE_MESSAGE];
        self.state.read_message(message, &mut buf)?;
        Ok(())
    }

    /// The peer's static public key, once the handshake has progressed
    /// far enough to reveal it. For the responder in `IK`, available as
    /// soon as the first message has been read.
    pub fn remote_public_key(&self) -> Option<[u8; 32]> {
        self.state.get_remote_static().and_then(|s| s.try_into().ok())
    }

    /// Finish the handshake and produce the encrypted session used for
    /// everything after (heartbeats now, log-sync later).
    pub fn into_session(self) -> Result<Session, snow::Error> {
        let hash = self.state.get_handshake_hash().try_into().unwrap_or([0; 32]);
        let transport = self.state.into_transport_mode()?;
        Ok(Session { transport, hash })
    }
}

pub struct Session {
    transport: TransportState,
    /// The handshake's hash: the same at both ends of this one
    /// connection and no other, so something signed over it can't be
    /// replayed on another (see `identity::device_proof`).
    pub hash: [u8; 32],
}

impl Session {
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, snow::Error> {
        let mut buf = vec![0u8; plaintext.len() + 16];
        let len = self.transport.write_message(plaintext, &mut buf)?;
        buf.truncate(len);
        Ok(buf)
    }

    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, snow::Error> {
        let mut buf = vec![0u8; ciphertext.len()];
        let len = self.transport.read_message(ciphertext, &mut buf)?;
        buf.truncate(len);
        Ok(buf)
    }

    /// Moves what this side sends on to a new key (Noise's `Rekey`: the
    /// old key, through itself, so nothing new is exchanged). Done right
    /// after sending a `Frame::Rekey`, so the other side does the same for
    /// what it receives in step (see `protocol::send_encoded`).
    pub fn rekey_outgoing(&mut self) {
        self.transport.rekey_outgoing();
    }

    /// The other side's `rekey_outgoing`, for what's received.
    pub fn rekey_incoming(&mut self) {
        self.transport.rekey_incoming();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    /// After a rekey, only a side that's rekeyed in step opens what's
    /// sent: the old key's gone.
    #[test]
    fn a_rekey_on_one_side_needs_the_other_to_follow() {
        let (dialer, listener) = (Identity::generate(), Identity::generate());
        let mut initiator = Handshake::initiator(&dialer.x25519_private_key_bytes(), &listener.x25519_public_key_bytes()).unwrap();
        let mut responder = Handshake::responder(&listener.x25519_private_key_bytes()).unwrap();
        responder.read_message(&initiator.write_message().unwrap()).unwrap();
        initiator.read_message(&responder.write_message().unwrap()).unwrap();
        let (mut a, mut b) = (initiator.into_session().unwrap(), responder.into_session().unwrap());
        assert_eq!(b.open(&a.seal(b"before").unwrap()).unwrap(), b"before");
        a.rekey_outgoing();
        let after = a.seal(b"after").unwrap();
        assert!(b.open(&after).is_err(), "not with the old key");
        b.rekey_incoming();
        assert_eq!(b.open(&after).unwrap(), b"after");
    }

    #[test]
    fn full_handshake_yields_working_sessions_both_ways() {
        let dialer = Identity::generate();
        let listener = Identity::generate();

        let mut initiator =
            Handshake::initiator(&dialer.x25519_private_key_bytes(), &listener.x25519_public_key_bytes()).unwrap();
        let mut responder = Handshake::responder(&listener.x25519_private_key_bytes()).unwrap();

        let msg1 = initiator.write_message().unwrap();
        responder.read_message(&msg1).unwrap();
        assert_eq!(responder.remote_public_key().unwrap(), dialer.x25519_public_key_bytes());

        let msg2 = responder.write_message().unwrap();
        initiator.read_message(&msg2).unwrap();
        assert_eq!(initiator.remote_public_key().unwrap(), listener.x25519_public_key_bytes());

        let mut dialer_session = initiator.into_session().unwrap();
        let mut listener_session = responder.into_session().unwrap();

        let sealed = dialer_session.seal(b"PING").unwrap();
        assert_eq!(listener_session.open(&sealed).unwrap(), b"PING");

        let reply = listener_session.seal(b"PONG").unwrap();
        assert_eq!(dialer_session.open(&reply).unwrap(), b"PONG");
    }

    #[test]
    fn tampered_ciphertext_fails_to_open() {
        let dialer = Identity::generate();
        let listener = Identity::generate();
        let mut initiator =
            Handshake::initiator(&dialer.x25519_private_key_bytes(), &listener.x25519_public_key_bytes()).unwrap();
        let mut responder = Handshake::responder(&listener.x25519_private_key_bytes()).unwrap();
        responder.read_message(&initiator.write_message().unwrap()).unwrap();
        initiator.read_message(&responder.write_message().unwrap()).unwrap();

        let mut dialer_session = initiator.into_session().unwrap();
        let mut listener_session = responder.into_session().unwrap();

        let mut sealed = dialer_session.seal(b"PING").unwrap();
        *sealed.last_mut().unwrap() ^= 0xFF;
        assert!(listener_session.open(&sealed).is_err());
    }
}
