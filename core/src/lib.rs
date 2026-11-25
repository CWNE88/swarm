//! Swarm's core: identities and keys, the connections and what goes over
//! them, and what's kept on disk. No screens, cameras or sound: those are
//! `swarm-app`'s.

pub mod call;
pub mod addressbook;
pub mod bundle;
pub mod card;
pub mod channel;
pub mod groupintro;
pub mod conference;
pub mod cover;
pub mod datagram;
pub mod devicelist;
pub mod disappearing;
pub mod discoverable;
pub mod displayname;
pub mod followers;
pub mod following;
pub mod forget;
pub mod framing;
pub mod history;
pub mod identity;
pub mod readmarks;
pub mod lastseen;
pub mod lock;
pub mod mdns;
pub mod myaddress;
mod memo;
pub mod node;
pub mod noise;
pub mod outbox;
pub mod peers;
pub mod peerstore;
pub mod postcache;
pub mod posts;
pub mod profile;
pub mod typedaddress;
pub mod protocol;
pub mod quinnlog;
pub mod remote;
pub mod requests;
pub mod secure;
pub mod seen;
pub mod settings;
pub mod store;
pub mod tally;
pub mod text;
pub mod transfer;
pub mod transport;
pub mod video;
pub mod unsaying;
pub mod waiting;
pub mod wire;
pub mod withdrawals;

/// Unix seconds, now (0 if the clock's before 1970).
pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
