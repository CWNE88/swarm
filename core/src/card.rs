//! "Here's where I am": a card for finding a peer again when both have
//! moved while apart, so neither's addresses for the other work. Each
//! hands its peers a card addressed to the one it's lost — its own
//! addresses, signed — and a peer who has the lost one connected passes
//! it on (one hop, no further). The one it's for keeps the addresses
//! only if the author is on its own peers list, then dials them. Nobody
//! learns where anyone is unless they're that person's peer: the carrier
//! is a peer of both, and the card's only shown to the one it names.

use crate::identity::{self, Identity};

/// The most addresses on a card.
pub const MAX_ADDRESSES: usize = crate::addressbook::MAX_PER_PEER;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    /// Who it's from: whose addresses these are, and who signed it.
    pub from: [u8; 32],
    /// The one it's for.
    pub to: [u8; 32],
    /// When it was made (unix secs).
    pub at: u64,
    pub addresses: Vec<String>,
    pub signature: [u8; 64],
}

fn signable(from: &[u8; 32], to: &[u8; 32], at: u64, addresses: &[String]) -> Vec<u8> {
    let mut bytes = b"swarm card 1\0".to_vec();
    bytes.extend_from_slice(from);
    bytes.extend_from_slice(to);
    bytes.extend_from_slice(&at.to_be_bytes());
    bytes.extend_from_slice(addresses.join(",").as_bytes());
    bytes
}

impl Card {
    /// A card from `identity` to `to`, saying it's at `addresses`.
    pub fn new(identity: &Identity, to: [u8; 32], at: u64, addresses: &[String]) -> Card {
        let from = identity.public_key_bytes();
        let addresses: Vec<String> = addresses.iter().take(MAX_ADDRESSES).cloned().collect();
        let signature = identity.sign(&signable(&from, &to, at, &addresses));
        Card { from, to, at, addresses, signature }
    }

    /// Whether `from` really made it, as it is.
    pub fn verifies(&self) -> bool {
        self.addresses.len() <= MAX_ADDRESSES && identity::verify(&self.from, &signable(&self.from, &self.to, self.at, &self.addresses), &self.signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_verifies_only_as_made() {
        let a = Identity::generate();
        let b = Identity::generate();
        let card = Card::new(&a, b.public_key_bytes(), 1_800_000_000, &["2001:db8::1".into(), "192.168.1.7".into()]);
        assert!(card.verifies());
        let mut moved = card.clone();
        moved.addresses[0] = "2001:db8::666".into();
        assert!(!moved.verifies());
        let mut redirected = card.clone();
        redirected.to = [9; 32];
        assert!(!redirected.verifies());
        let mut forged = card;
        forged.from = b.public_key_bytes();
        assert!(!forged.verifies());
    }
}
