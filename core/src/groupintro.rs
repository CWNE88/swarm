//! Asking to add someone seen in a group.
//!
//! Two people in a group who aren't contacts never connect and don't
//! know each other's addresses. One asks to add the other with an
//! `Intro`: their code (name and addresses), sealed so only the one asked
//! can open it, signed by the asker, and passed on through the group as
//! its messages are, by people who can't read it. The one asked sees an
//! ordinary contact request; adding them back dials the asker, whose
//! devices remember asking (`asked`) and add them in turn, so neither
//! has to ask twice.
//!
//! The seal is a one-way Noise handshake (`Noise_N`): an ephemeral key
//! and the recipient's static key make the key it's sealed under; the
//! group and both keys are its prologue, so it opens only there.

use crate::channel::ChannelId;
use crate::identity::{self, Identity};
use crate::store;
use crate::wire::{hex, Reader, Writer};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use sha2::{Digest, Sha256};
use std::path::Path;

const SEAL_PATTERN: &str = "Noise_N_25519_ChaChaPoly_BLAKE2s";

/// How long an ask is passed on, and remembered by the one who made it
/// (seconds).
pub const LASTS: u64 = 7 * 24 * 60 * 60;

/// The most bytes a sealed code may have: a name and some addresses.
const MOST_SEALED: usize = 2048;

/// Asking `to` to add `from`, through `channel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intro {
    pub channel: ChannelId,
    pub from: [u8; 32],
    pub to: [u8; 32],
    pub at: u64,
    /// `from`'s name and addresses, sealed for `to`.
    pub sealed: Vec<u8>,
    pub signature: [u8; 64],
}

/// What's sealed in an `Intro`: how to reach whoever asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    pub name: String,
    pub addresses: String,
}

fn prologue(channel: &ChannelId, from: &[u8; 32], to: &[u8; 32]) -> Vec<u8> {
    format!("group intro\t{}\t{}\t{}\t{}", channel.name, STANDARD.encode(channel.owner), STANDARD.encode(from), STANDARD.encode(to)).into_bytes()
}

fn signable(intro: &Intro) -> Vec<u8> {
    let sealed: [u8; 32] = Sha256::digest(&intro.sealed).into();
    let mut bytes = prologue(&intro.channel, &intro.from, &intro.to);
    bytes.extend(format!("\t{}\t{}", intro.at, hex(&sealed)).into_bytes());
    bytes
}

impl Intro {
    /// `identity` asking `to` to add them, through `channel`, with how to
    /// reach them (`code`).
    pub fn make(channel: &ChannelId, identity: &Identity, to: [u8; 32], code: &Code, at: u64) -> Result<Intro> {
        let from = identity.public_key_bytes();
        let their_key = identity::ed25519_public_key_to_x25519(&to).map_err(|_| anyhow::anyhow!("not a key that can be sealed for"))?;
        let mut state = snow::Builder::new(SEAL_PATTERN.parse().expect("a known pattern"))
            .prologue(&prologue(channel, &from, &to))
            .remote_public_key(&their_key)
            .build_initiator()
            .context("sealing")?;
        let mut plain = Writer::new(0);
        plain.str(&crate::text::single_line(&code.name)).str(&crate::text::single_line(&code.addresses));
        let mut sealed = vec![0u8; 65535];
        let n = state.write_message(&plain.into_bytes()[1..], &mut sealed).context("sealing")?;
        sealed.truncate(n);
        let mut intro = Intro { channel: channel.clone(), from, to, at, sealed, signature: [0; 64] };
        intro.signature = identity.sign(&signable(&intro));
        Ok(intro)
    }

    /// Whether it's whole and really from `from` (anyone can check).
    pub fn verify(&self) -> bool {
        self.sealed.len() <= MOST_SEALED && crate::channel::validate_name(&self.channel.name).is_ok() && identity::verify(&self.from, &signable(self), &self.signature)
    }

    /// Opened by `identity`, the one asked: how to reach whoever asked.
    pub fn open(&self, identity: &Identity) -> Result<Code> {
        if identity.public_key_bytes() != self.to {
            bail!("not for this user");
        }
        let mut state = snow::Builder::new(SEAL_PATTERN.parse().expect("a known pattern"))
            .prologue(&prologue(&self.channel, &self.from, &self.to))
            .local_private_key(&identity.x25519_private_key_bytes())
            .build_responder()
            .context("opening")?;
        let mut plain = vec![0u8; 65535];
        let n = state.read_message(&self.sealed, &mut plain).context("opening")?;
        let mut r = Reader::new(&plain[..n]);
        Ok(Code { name: crate::text::single_line(&r.str()?), addresses: crate::text::single_line(&r.str()?) })
    }

    /// Its identity, for knowing it's been seen.
    pub fn id(&self) -> [u8; 32] {
        Sha256::digest(self.signature).into()
    }

    /// Whether it's still passed on, as of `now`.
    pub fn live(&self, now: u64) -> bool {
        now < self.at.saturating_add(LASTS) && self.at <= now.saturating_add(crate::postcache::CLOCK_SLACK)
    }

    /// As it goes on the wire, after a frame's tag.
    pub fn write(&self, w: &mut Writer) {
        w.str(&self.channel.name).array(&self.channel.owner).array(&self.from).array(&self.to).u64(self.at).bytes(&self.sealed).array(&self.signature);
    }

    pub fn read(r: &mut Reader) -> Result<Intro> {
        let channel = ChannelId { name: r.str()?, owner: r.array()? };
        let (from, to, at) = (r.array()?, r.array()?, r.u64()?);
        let sealed = r.bytes()?.to_vec();
        if sealed.len() > MOST_SEALED {
            bail!("a sealed code that's too long");
        }
        Ok(Intro { channel, from, to, at, sealed, signature: r.array()? })
    }
}

/// The asks this user has made, kept (`~/.swarm/group-asks`, an `Intro`
/// a line, in hex) so they're passed on again after a restart, and so
/// whoever was asked is added straight back when they add this user.
/// Kept `LASTS`.
pub fn asked(path: &Path, now: u64) -> Vec<Intro> {
    store::read(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let bytes = (0..line.len() / 2).map(|i| u8::from_str_radix(line.get(i * 2..i * 2 + 2)?, 16).ok()).collect::<Option<Vec<u8>>>()?;
            Intro::read(&mut Reader::new(&bytes)).ok()
        })
        .filter(|intro| intro.live(now))
        .collect()
}

/// Keeps `intro`, one this user made (here or on another of their
/// devices). Returns whether it was new here.
pub fn keep(path: &Path, intro: &Intro, now: u64) -> std::io::Result<bool> {
    let _guard = store::lock();
    let mut all = asked(path, now);
    if all.iter().any(|i| i.id() == intro.id()) {
        return Ok(false);
    }
    all.retain(|i| i.to != intro.to);
    all.push(intro.clone());
    let lines: String = all
        .iter()
        .map(|i| {
            let mut w = Writer::new(0);
            i.write(&mut w);
            format!("{}\n", hex(&w.into_bytes()[1..]))
        })
        .collect();
    store::write_atomic(path, lines)?;
    Ok(true)
}

/// Forgets the ask of `to` (they've been added).
pub fn done(path: &Path, to: &[u8; 32], now: u64) -> std::io::Result<()> {
    let _guard = store::lock();
    let all: Vec<Intro> = asked(path, now).into_iter().filter(|i| i.to != *to).collect();
    let lines: String = all
        .iter()
        .map(|i| {
            let mut w = Writer::new(0);
            i.write(&mut w);
            format!("{}\n", hex(&w.into_bytes()[1..]))
        })
        .collect();
    store::write_atomic(path, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_one_asked_opens_it_and_anyone_can_check_who_asked() {
        let (ann, cat, bob) = (Identity::generate(), Identity::generate(), Identity::generate());
        let group = ChannelId::new_group(bob.public_key_bytes());
        let code = Code { name: "Ann".into(), addresses: "2001:db8::1,192.0.2.1".into() };
        let intro = Intro::make(&group, &ann, cat.public_key_bytes(), &code, 1000).unwrap();
        assert!(intro.verify());
        assert_eq!(intro.open(&cat).unwrap(), code);
        assert!(intro.open(&bob).is_err(), "bob passes it on, but can't read it");
        let mut moved = intro.clone();
        moved.channel = ChannelId::new_group(bob.public_key_bytes());
        assert!(!moved.verify(), "signed for its group");
        let mut forged = intro.clone();
        forged.from = bob.public_key_bytes();
        assert!(!forged.verify());
        let mut w = Writer::new(0);
        intro.write(&mut w);
        assert_eq!(Intro::read(&mut Reader::new(&w.into_bytes()[1..])).unwrap(), intro);
        assert!(intro.live(1000 + LASTS - 1) && !intro.live(1000 + LASTS));
    }
}
