//! Long-term identity: an Ed25519 keypair as the root, with the X25519
//! keypair used for the Noise handshake *derived* from it rather than
//! generated independently.
//!
//! Why derive rather than generate two unrelated keypairs: signing only
//! becomes meaningful once content might be verified by someone who
//! never directly connected to its author — a channel message relayed
//! through someone else, say, unlike a DM or a post fetch, which are
//! already authenticated by the live Noise handshake and need no
//! separate signature. That means a signing key has to exist
//! *alongside* the X25519 identity already in use everywhere, and a
//! peer who already trusts your X25519 public key needs to be able to
//! trust your signing key too, without a separate exchange or
//! trust-on-first-use step for it.
//!
//! Deriving one from the other is the standard way to get that for
//! free — but only in one direction: an Ed25519 seed can safely derive
//! a matching X25519 keypair (same technique as `libsodium`'s
//! `crypto_sign_ed25519_sk_to_curve25519`/`..._pk_to_curve25519`), while
//! the reverse doesn't work. So Ed25519 has to be the root: `keygen`
//! generates an Ed25519 seed, and the X25519 keypair used for Noise
//! (`x25519_private_key_bytes`/`x25519_public_key_bytes`) is derived
//! from it. A peer who only knows someone's Ed25519 public key (what
//! `swarm pubkey` prints, what goes in the peers file) can independently
//! compute the matching X25519 public key via
//! `ed25519_public_key_to_x25519` — provably the same identity, with
//! nothing extra to exchange or pin.
//!
//! Underneath both is the *recovery seed*: 16 random bytes, which is
//! what's kept and what's shown to the person as 12 words (BIP39's
//! English list; `recovery_words`, `from_words`). The 32-byte Ed25519
//! seed is HKDF-SHA256 of it ("swarm identity 1"), so the words alone
//! bring the whole identity back. 128 bits is the strength Ed25519
//! itself gives; fewer words wouldn't be safe, since anyone can check a
//! guess against the public key, offline and without limit.

use crate::lock::Locked;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha512};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

pub struct Identity {
    recovery_seed: [u8; RECOVERY_SEED_LEN],
    signing_key: SigningKey,
    x25519_secret: X25519Secret,
}

/// What the identity file starts with: it's sealed under a passphrase.
const FILE_HEADER: &str = "swarm identity 2";

/// Argon2id's settings for the passphrase: 64 MiB, 3 passes, one lane —
/// about half a second on a laptop, a few on a Pi 3. Each guess at the
/// passphrase costs the same.
const ARGON2_MIB: u32 = 64;
const ARGON2_PASSES: u32 = 3;

/// Why the identity file didn't open.
#[derive(Debug, PartialEq, Eq)]
pub enum UnlockError {
    /// The passphrase isn't this file's.
    WrongPassphrase,
    /// It isn't an identity file (damaged, or something else).
    NotAnIdentityFile,
}

impl std::fmt::Display for UnlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnlockError::WrongPassphrase => write!(f, "that isn't the passphrase"),
            UnlockError::NotAnIdentityFile => write!(f, "this isn't an identity file this version of Swarm can read"),
        }
    }
}

impl std::error::Error for UnlockError {}

fn unlock_key(passphrase: &str, salt: &[u8], mib: u32, passes: u32) -> Option<zeroize::Zeroizing<[u8; 32]>> {
    let params = argon2::Params::new(mib * 1024, passes, 1, Some(32)).ok()?;
    let mut key = zeroize::Zeroizing::new([0u8; 32]);
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params).hash_password_into(passphrase.as_bytes(), salt, &mut *key).ok()?;
    Some(key)
}

/// The list the recovery words come from (BIP39's English 2048).
pub fn word_list() -> &'static [&'static str; 2048] {
    bip39::Language::English.word_list()
}

/// The recovery seed's length: 16 bytes, 12 words.
pub const RECOVERY_SEED_LEN: usize = 16;

/// How many recovery words there are.
pub const RECOVERY_WORDS: usize = 12;

#[derive(Debug)]
pub enum IdentityError {
    InvalidEncoding,
    InvalidLength,
}

/// Why typed recovery words weren't taken, said so the person can fix it.
#[derive(Debug, PartialEq, Eq)]
pub enum WordsError {
    /// Not 12 words: how many there were.
    WrongCount(usize),
    /// This word (1-based) isn't one of the list's, nor the start of one.
    Unknown(usize, String),
    /// Each word's a real one, but they don't go together: one's wrong,
    /// or two have changed places.
    Checksum,
}

impl std::fmt::Display for WordsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WordsError::WrongCount(n) => write!(f, "that's {n} word{}: there should be {RECOVERY_WORDS}", if *n == 1 { "" } else { "s" }),
            WordsError::Unknown(i, word) => write!(f, "word {i}, \"{word}\", isn't one of the recovery words"),
            WordsError::Checksum => write!(f, "those words don't go together: one of them is wrong, or two have changed places"),
        }
    }
}

impl std::error::Error for WordsError {}

impl Drop for Identity {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.recovery_seed);
    }
}

impl Identity {
    /// A new identity, from 16 fresh random bytes.
    pub fn generate() -> Self {
        let mut seed = [0u8; RECOVERY_SEED_LEN];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
        Self::from_recovery_seed(seed)
    }

    /// The identity these 16 bytes make: the Ed25519 seed is HKDF-SHA256
    /// of them, and the X25519 key comes from that.
    pub fn from_recovery_seed(recovery_seed: [u8; RECOVERY_SEED_LEN]) -> Self {
        let mut ed25519_seed = zeroize::Zeroizing::new([0u8; 32]);
        hkdf::Hkdf::<sha2::Sha256>::new(None, &recovery_seed)
            .expand(b"swarm identity 1", &mut *ed25519_seed)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        let signing_key = SigningKey::from_bytes(&ed25519_seed);
        let x25519_secret = derive_x25519_secret(signing_key.as_bytes());
        Identity { recovery_seed, signing_key, x25519_secret }
    }

    /// From the identity file's text: the recovery seed, base64. Any
    /// other length is refused (`InvalidLength`).
    pub fn from_private_key_base64(s: &str) -> Result<Self, IdentityError> {
        let bytes = STANDARD.decode(s.trim()).map_err(|_| IdentityError::InvalidEncoding)?;
        let seed: [u8; RECOVERY_SEED_LEN] = bytes.try_into().map_err(|_| IdentityError::InvalidLength)?;
        Ok(Self::from_recovery_seed(seed))
    }

    /// From the 12 recovery words, as typed: any case, any spacing, and
    /// each word may be cut to its first four letters (enough to tell
    /// every word on the list apart). The last word carries a checksum,
    /// so a wrong word, or two swapped, is caught rather than giving
    /// someone else's identity.
    pub fn from_words(typed: &str) -> Result<Self, WordsError> {
        let english = bip39::Language::English;
        let typed: Vec<String> = typed.split_whitespace().map(|w| w.to_lowercase()).collect();
        if typed.len() != RECOVERY_WORDS {
            return Err(WordsError::WrongCount(typed.len()));
        }
        let mut words = Vec::with_capacity(RECOVERY_WORDS);
        for (i, word) in typed.iter().enumerate() {
            let found = english.find_word(word).map(|n| english.word_list()[n as usize]).or_else(|| {
                let prefix: String = word.chars().take(4).collect();
                match english.words_by_prefix(&prefix) {
                    [only] if prefix.chars().count() == 4 => Some(*only),
                    _ => None,
                }
            });
            words.push(found.ok_or_else(|| WordsError::Unknown(i + 1, word.clone()))?);
        }
        let mnemonic = bip39::Mnemonic::parse_in_normalized(english, &words.join(" ")).map_err(|_| WordsError::Checksum)?;
        let seed: [u8; RECOVERY_SEED_LEN] = mnemonic.to_entropy().try_into().map_err(|_| WordsError::WrongCount(mnemonic.word_count()))?;
        Ok(Self::from_recovery_seed(seed))
    }

    /// The 12 recovery words: everything needed to bring this identity
    /// back.
    pub fn recovery_words(&self) -> Vec<&'static str> {
        bip39::Mnemonic::from_entropy(&self.recovery_seed).expect("16 bytes is a valid BIP39 entropy length").words().collect()
    }

    /// The recovery seed: what's written to the identity file.
    pub fn private_key_bytes(&self) -> [u8; RECOVERY_SEED_LEN] {
        self.recovery_seed
    }

    /// The identity file's text: the recovery seed sealed under
    /// `passphrase` (Argon2id to a key, XChaCha20-Poly1305 with it; the
    /// settings and salt bound in), with nothing readable but what it is.
    pub fn sealed(&self, passphrase: &str) -> String {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        let mut salt = [0u8; 16];
        let mut nonce = [0u8; 24];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
        let settings = format!("argon2id {ARGON2_MIB} {ARGON2_PASSES} {}", STANDARD.encode(salt));
        let key = unlock_key(passphrase, &salt, ARGON2_MIB, ARGON2_PASSES).expect("fixed Argon2 settings are valid");
        let sealed = chacha20poly1305::XChaCha20Poly1305::new((&*key).into())
            .encrypt(chacha20poly1305::XNonce::from_slice(&nonce), Payload { msg: &self.recovery_seed, aad: format!("{FILE_HEADER}\n{settings}").as_bytes() })
            .expect("sealing 16 bytes can't fail");
        format!("{FILE_HEADER}\n{settings}\n{}\n", STANDARD.encode([nonce.as_slice(), &sealed].concat()))
    }

    /// Opens an identity file's text (`sealed`) with `passphrase`.
    pub fn unsealed(text: &str, passphrase: &str) -> Result<Self, UnlockError> {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        let bad = UnlockError::NotAnIdentityFile;
        let mut lines = text.lines();
        if lines.next() != Some(FILE_HEADER) {
            return Err(bad);
        }
        let settings = lines.next().ok_or(UnlockError::NotAnIdentityFile)?;
        let fields: Vec<&str> = settings.split(' ').collect();
        let [kind, mib, passes, salt] = fields[..] else { return Err(bad) };
        let (Ok(mib), Ok(passes), Ok(salt)) = (mib.parse::<u32>(), passes.parse::<u32>(), STANDARD.decode(salt)) else { return Err(bad) };
        if kind != "argon2id" || mib < ARGON2_MIB || passes < ARGON2_PASSES || mib > 4096 || passes > 64 || salt.len() < 16 {
            return Err(bad);
        }
        let bytes = lines.next().and_then(|l| STANDARD.decode(l).ok()).filter(|b| b.len() > 24).ok_or(UnlockError::NotAnIdentityFile)?;
        let (nonce, sealed) = bytes.split_at(24);
        let key = unlock_key(passphrase, &salt, mib, passes).ok_or(UnlockError::NotAnIdentityFile)?;
        let opened = zeroize::Zeroizing::new(
            chacha20poly1305::XChaCha20Poly1305::new((&*key).into())
                .decrypt(chacha20poly1305::XNonce::from_slice(nonce), Payload { msg: sealed, aad: format!("{FILE_HEADER}\n{settings}").as_bytes() })
                .map_err(|_| UnlockError::WrongPassphrase)?,
        );
        let seed: [u8; RECOVERY_SEED_LEN] = opened.as_slice().try_into().map_err(|_| UnlockError::NotAnIdentityFile)?;
        Ok(Self::from_recovery_seed(seed))
    }

    /// The keys that seal what's kept on disk for this identity (see
    /// `store`): from the identity seed, so the 12 words bring them back.
    pub fn storage_keys(&self) -> crate::store::Keys {
        let hkdf = hkdf::Hkdf::<sha2::Sha256>::new(None, self.signing_key.as_bytes());
        let mut seal = zeroize::Zeroizing::new([0u8; 32]);
        let mut names = zeroize::Zeroizing::new([0u8; 32]);
        hkdf.expand(b"swarm storage 1", &mut *seal).expect("32 bytes is a valid HKDF-SHA256 output length");
        hkdf.expand(b"swarm names 1", &mut *names).expect("32 bytes is a valid HKDF-SHA256 output length");
        crate::store::Keys { seal, names }
    }

    /// This identity's root public key (Ed25519) — what `swarm pubkey`
    /// prints and what goes in a peer list entry.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn private_key_base64(&self) -> String {
        STANDARD.encode(self.private_key_bytes())
    }

    pub fn public_key_base64(&self) -> String {
        STANDARD.encode(self.public_key_bytes())
    }

    /// The X25519 private key derived from this identity's Ed25519
    /// seed — used only for the Noise handshake (see `transport::dial`/
    /// `transport::accept`), never exposed as "the" private key anywhere
    /// else.
    pub fn x25519_private_key_bytes(&self) -> [u8; 32] {
        self.x25519_secret.to_bytes()
    }

    /// The X25519 public key derived from this identity — equal to
    /// `ed25519_public_key_to_x25519(&self.public_key_bytes())`, proven
    /// equal by `derived_x25519_matches_the_conversion_of_the_public_key`
    /// below; exposed directly here since anyone holding the private key
    /// doesn't need to go via the public conversion path to get it.
    pub fn x25519_public_key_bytes(&self) -> [u8; 32] {
        *X25519PublicKey::from(&self.x25519_secret).as_bytes()
    }

    /// Signs `message` with this identity's root Ed25519 key — for
    /// content that might be verified by someone who never directly
    /// connected to this device (see the module docs). A DM or a post
    /// fetch doesn't need this: the live Noise handshake already proves
    /// authorship at delivery time.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing_key.sign(message).to_bytes()
    }
}

/// Verifies `signature` over `message` against `public_key` (someone's
/// Ed25519 identity, e.g. from a peer list entry) — the counterpart to
/// `Identity::sign`, usable without holding the corresponding private
/// identity. `false` for a malformed public key or signature, same as
/// for one that just doesn't match — the caller only ever needs to know
/// whether to trust the message, not why not.
///
/// What's been found good is remembered (by a hash of all three): the
/// same signatures are checked again and again — a channel's roster on
/// every redraw, every cached name each time the list is made — and a
/// check costs some forty times the hash.
pub fn verify(public_key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static GOOD: Mutex<Option<HashSet<[u8; 32]>>> = Mutex::new(None);
    const MOST_REMEMBERED: usize = 50_000;
    let seen: [u8; 32] = {
        let mut h = sha2::Sha256::new();
        h.update(public_key);
        h.update(signature);
        h.update(message);
        h.finalize().into()
    };
    if GOOD.locked().as_ref().is_some_and(|good| good.contains(&seen)) {
        return true;
    }
    // Strict: a weak key (of small order), for which signatures can be
    // made without its secret, and signatures written in more than one
    // way, are refused.
    let Ok(verifying_key) = VerifyingKey::from_bytes(public_key) else { return false };
    let ok = !verifying_key.is_weak() && verifying_key.verify_strict(message, &Signature::from_bytes(signature)).is_ok();
    if ok {
        let mut good = GOOD.locked();
        let good = good.get_or_insert_with(HashSet::new);
        if good.len() >= MOST_REMEMBERED {
            good.clear();
        }
        good.insert(seen);
    }
    ok
}

/// Derives the X25519 public key that corresponds to `ed25519_public_key`
/// — what `Identity::x25519_public_key_bytes` returns for the identity
/// that public key belongs to, computable by anyone who only has the
/// public key. This is the whole point of rooting the identity in
/// Ed25519 (see the module docs): a peer list entry only ever needs to
/// store one public key, and both signature verification and the Noise
/// handshake can derive what they need from it.
///
/// Errs if `ed25519_public_key` isn't a valid compressed Edwards point
/// (a mistyped or made-up key, about half the time), or is one of the
/// few weak ones (of small order): no key pair has one, and a handshake
/// with it would give a shared secret anyone could work out.
pub fn ed25519_public_key_to_x25519(ed25519_public_key: &[u8; 32]) -> Result<[u8; 32], IdentityError> {
    CompressedEdwardsY(*ed25519_public_key)
        .decompress()
        .filter(|point| !point.is_small_order())
        .map(|point| point.to_montgomery().to_bytes())
        .ok_or(IdentityError::InvalidEncoding)
}

/// The standard Ed25519-seed-to-X25519-private-key conversion: hash the
/// seed, keep the first half — the same computation Ed25519 itself does
/// internally to derive its signing scalar (`x25519_dalek::StaticSecret`
/// applies the required clamping automatically when constructed from
/// raw bytes, same as it always has).
fn derive_x25519_secret(ed25519_seed: &[u8; 32]) -> X25519Secret {
    let hash = Sha512::digest(ed25519_seed);
    let mut scalar_bytes = [0u8; 32];
    scalar_bytes.copy_from_slice(&hash[..32]);
    X25519Secret::from(scalar_bytes)
}

/// What a user's key signs to make a key one of its devices: every
/// device of a user holds the user's key, and any of them signs a new
/// one in.
const DEVICE_CERT: &[u8] = b"swarm device of a user\0";
/// What a device signs to prove, on one connection, that it holds its
/// key: the connection's handshake hash, after this.
const DEVICE_PROOF: &[u8] = b"swarm device on this connection\0";

/// `user`'s signature making `device` (a public key) one of its devices.
pub fn device_cert(user: &Identity, device: &[u8; 32]) -> [u8; 64] {
    user.sign(&[DEVICE_CERT, device].concat())
}

/// Whether `cert` is `user`'s, making `device` one of its devices.
pub fn verify_device_cert(user: &[u8; 32], device: &[u8; 32], cert: &[u8; 64]) -> bool {
    verify(user, &[DEVICE_CERT, device].concat(), cert)
}

/// `device`'s proof that it's at this end of the connection whose
/// handshake hash is `handshake`.
pub fn device_proof(device: &Identity, handshake: &[u8; 32]) -> [u8; 64] {
    device.sign(&[DEVICE_PROOF, handshake].concat())
}

/// Whether `proof` is `device`'s, for the connection whose handshake
/// hash is `handshake`.
pub fn verify_device_proof(device: &[u8; 32], handshake: &[u8; 32], proof: &[u8; 64]) -> bool {
    verify(device, &[DEVICE_PROOF, handshake].concat(), proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device is one of a user's only by that user's signature, and
    /// proves itself only for the connection it signed for.
    #[test]
    fn a_device_cert_and_proof_hold_only_for_what_they_were_made_for() {
        let (user, other, device) = (Identity::generate(), Identity::generate(), Identity::generate());
        let cert = device_cert(&user, &device.public_key_bytes());
        assert!(verify_device_cert(&user.public_key_bytes(), &device.public_key_bytes(), &cert));
        assert!(!verify_device_cert(&other.public_key_bytes(), &device.public_key_bytes(), &cert), "another user's");
        assert!(!verify_device_cert(&user.public_key_bytes(), &other.public_key_bytes(), &cert), "another device");
        let proof = device_proof(&device, &[7; 32]);
        assert!(verify_device_proof(&device.public_key_bytes(), &[7; 32], &proof));
        assert!(!verify_device_proof(&device.public_key_bytes(), &[8; 32], &proof), "another connection");
        assert!(!verify_device_proof(&user.public_key_bytes(), &device.public_key_bytes(), &cert));
    }

    #[test]
    fn generate_roundtrips_through_base64() {
        let identity = Identity::generate();
        let restored = Identity::from_private_key_base64(&identity.private_key_base64()).unwrap();
        assert_eq!(identity.public_key_bytes(), restored.public_key_bytes());
        assert_eq!(identity.x25519_public_key_bytes(), restored.x25519_public_key_bytes());
    }

    #[test]
    fn rejects_wrong_length_key() {
        let bad = STANDARD.encode([0u8; 32]);
        assert!(matches!(
            Identity::from_private_key_base64(&bad),
            Err(IdentityError::InvalidLength)
        ));
    }

    #[test]
    fn the_twelve_words_bring_the_identity_back() {
        let identity = Identity::generate();
        let words = identity.recovery_words();
        assert_eq!(words.len(), RECOVERY_WORDS);
        let restored = Identity::from_words(&words.join(" ")).unwrap();
        assert_eq!(restored.public_key_bytes(), identity.public_key_bytes());
        assert_eq!(restored.x25519_public_key_bytes(), identity.x25519_public_key_bytes());
        assert_eq!(restored.private_key_bytes(), identity.private_key_bytes());
    }

    /// BIP39's own test vector, so the words are the standard ones any
    /// other BIP39 tool would give for the same bytes; and the identity
    /// they make is fixed, so a change to how it's worked out (which
    /// would give everyone a different ID from the same words) fails.
    #[test]
    fn the_words_are_bip39s_and_the_identity_they_make_is_fixed() {
        let identity = Identity::from_recovery_seed([0u8; 16]);
        assert_eq!(identity.recovery_words().join(" "), "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about");
        let again = Identity::from_recovery_seed([0u8; 16]);
        assert_eq!(identity.public_key_bytes(), again.public_key_bytes());
        assert_eq!(identity.public_key_base64(), "L4KW5oEODKOwYLz9K8O8ZDmXktdDyX3afxvPIfvnTt4=");
    }

    #[test]
    fn words_can_be_typed_loosely() {
        let identity = Identity::generate();
        let words = identity.recovery_words();
        let mut typed: Vec<String> = words.iter().map(|w| w.chars().take(4).collect::<String>().to_uppercase()).collect();
        if let Some(i) = words.iter().position(|w| w.len() >= 4) {
            typed[i] = format!("{}xyz", &words[i][..4]);
        }
        let typed = format!("  {}\n{}  ", typed[..6].join("   "), typed[6..].join(" "));
        assert_eq!(Identity::from_words(&typed).unwrap().public_key_bytes(), identity.public_key_bytes());
    }

    #[test]
    fn wrong_words_are_caught_and_said_which() {
        let words: Vec<String> = Identity::generate().recovery_words().iter().map(|w| w.to_string()).collect();
        assert_eq!(Identity::from_words(&words[..11].join(" ")).err(), Some(WordsError::WrongCount(11)));
        let mut unknown = words.clone();
        unknown[4] = "qwerty".into();
        assert_eq!(Identity::from_words(&unknown.join(" ")).err(), Some(WordsError::Unknown(5, "qwerty".into())));
        let mut short = words.clone();
        short[0] = "zzz".into();
        assert!(matches!(Identity::from_words(&short.join(" ")), Err(WordsError::Unknown(1, _))));
        let caught = (0..2048u16).filter(|&n| {
            let mut changed = words.clone();
            changed[2] = bip39::Language::English.word_list()[n as usize].to_string();
            changed[2] != words[2] && matches!(Identity::from_words(&changed.join(" ")), Err(WordsError::Checksum))
        });
        assert!(caught.count() > 1700, "most wrong words are caught");
    }

    #[test]
    fn the_identity_file_opens_only_with_its_passphrase() {
        let identity = Identity::generate();
        let text = identity.sealed("correct horse battery staple");
        assert!(!text.contains(&identity.private_key_base64()), "the seed isn't readable in the file");
        let opened = Identity::unsealed(&text, "correct horse battery staple").unwrap();
        assert_eq!(opened.public_key_bytes(), identity.public_key_bytes());
        assert_eq!(Identity::unsealed(&text, "correct horse battery stapler").err(), Some(UnlockError::WrongPassphrase));
        assert_ne!(identity.sealed("p"), identity.sealed("p"));
        let cheaper = text.replace(&format!("argon2id {ARGON2_MIB} {ARGON2_PASSES}"), "argon2id 1 1");
        assert_eq!(Identity::unsealed(&cheaper, "correct horse battery staple").err(), Some(UnlockError::NotAnIdentityFile));
        assert_eq!(Identity::unsealed(&identity.private_key_base64(), "x").err(), Some(UnlockError::NotAnIdentityFile));
        let other = text.replace(&format!("argon2id {ARGON2_MIB} {ARGON2_PASSES}"), &format!("argon2id {ARGON2_MIB} {}", ARGON2_PASSES + 1));
        assert_eq!(Identity::unsealed(&other, "correct horse battery staple").err(), Some(UnlockError::WrongPassphrase));
    }

    #[test]
    fn storage_keys_come_from_the_identity_and_differ_between_people() {
        let identity = Identity::generate();
        let again = Identity::from_words(&identity.recovery_words().join(" ")).unwrap();
        assert_eq!(*identity.storage_keys().seal, *again.storage_keys().seal);
        assert_eq!(*identity.storage_keys().names, *again.storage_keys().names);
        assert_ne!(*identity.storage_keys().seal, *identity.storage_keys().names);
        assert_ne!(*identity.storage_keys().seal, *Identity::generate().storage_keys().seal);
    }

    #[test]
    fn sign_then_verify_round_trips_and_rejects_tampering() {
        let identity = Identity::generate();
        let signature = identity.sign(b"a signed post");
        assert!(verify(&identity.public_key_bytes(), b"a signed post", &signature));
        assert!(!verify(&identity.public_key_bytes(), b"a different message", &signature));

        let stranger = Identity::generate();
        assert!(!verify(&stranger.public_key_bytes(), b"a signed post", &signature));
    }

    /// The property the whole derive-don't-generate design depends on:
    /// converting someone's Ed25519 *public* key gives the exact same
    /// X25519 public key their own identity derives from its *private*
    /// key — proven here empirically, not just asserted in the docs.
    #[test]
    fn derived_x25519_matches_the_conversion_of_the_public_key() {
        let identity = Identity::generate();
        let converted = ed25519_public_key_to_x25519(&identity.public_key_bytes()).unwrap();
        assert_eq!(converted, identity.x25519_public_key_bytes());
    }

    #[test]
    fn a_weak_key_signs_nothing_and_shakes_no_hands() {
        // The neutral point as the key: with R the same and s zero, a
        // non-strict check takes that "signature" for any message at all.
        let mut weak = [0u8; 32];
        weak[0] = 1;
        let mut forged = [0u8; 64];
        forged[0] = 1;
        assert!(!verify(&weak, b"anything", &forged));
        assert!(!verify(&weak, b"something else", &forged));
        assert!(ed25519_public_key_to_x25519(&weak).is_err());
        // An ordinary key still works.
        let identity = Identity::generate();
        assert!(verify(&identity.public_key_bytes(), b"hello", &identity.sign(b"hello")));
    }

    #[test]
    fn ed25519_public_key_to_x25519_rejects_an_invalid_point() {
        use rand::RngCore;
        let mut rng = rand::thread_rng();
        let found_invalid = (0..32).any(|_| {
            let mut bytes = [0u8; 32];
            rng.fill_bytes(&mut bytes);
            ed25519_public_key_to_x25519(&bytes).is_err()
        });
        assert!(found_invalid, "expected at least one of 32 random candidates to be an invalid compressed point");
    }
}
