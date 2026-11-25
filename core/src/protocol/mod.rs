//! The frames exchanged over every established Noise session, and the
//! primitives for driving a connection that can carry traffic *either
//! side* initiated.
//!
//! A Noise/QUIC connection is full-duplex once up, so either side can
//! drive it for outgoing requests *and* answer incoming ones — see
//! `recv`/`request`/`reply` below, and `swarm-cli`'s per-peer connection
//! task, which is the only thing meant to call them (the Noise transport
//! cipher's nonce is a strict per-direction counter, so only one task
//! may ever touch a given connection's send side, and only one its
//! receive side, at a time).
//!
//! Every frame is one of three kinds, told apart by its tag's range:
//!
//! - a **request** (`Request`) carries a `RequestId` its sender chose;
//! - a **reply** (`Reply`) carries the id of the request it answers, so
//!   any number of requests can be outstanding at once in either
//!   direction and nothing is matched up by position — unsolicited
//!   frames arriving first can't be mistaken for a reply;
//! - a **notice** (`Notice`) is one-way: cover settings, "not a contact",
//!   introductions, call signals.
//!
//! Each connection opens with a `Hello` from both sides carrying their
//! protocol version (see `greet`, which `transport` runs as part of
//! every dial and accept), so two incompatible builds say so instead of
//! failing on the first frame one of them doesn't know. Within a
//! compatible version, an unknown request is answered `Unsupported` and
//! an unknown reply or notice is skipped, so new frame types can be
//! added without breaking older peers. Fields are encoded with `wire`.
//!
//! Replies that can grow without bound (a peer's posts, a channel's
//! roster and posts) are split into parts that each fit one frame
//! (`respond_posts`, `respond_channel_data`); the asker collects them
//! until the last one.
//!
//! The parts: the frame and its tags here; `requests` (requests and
//! replies), `notices`, `calls` (call signals, and sending them),
//! `channels` and `fields` (what several kinds of frame are made of) for
//! the bytes; `link` (sending, receiving, the handshake), `cover` (its
//! slots), `asks` (one-shot requests and split replies) and `stranger`
//! (a connection from someone not a contact) for what's done with them.

use crate::call::CallSignal;
use crate::channel::{BucketDigest, ChannelId, ChannelPost, RosterAction, RosterEvent};
use crate::displayname::DisplayName;
use crate::framing;
use crate::outbox::MessageId;
use crate::posts::{Deletion, Fetched, Post, Visibility};
use crate::transport::AuthenticatedConnection;
use crate::wire::{Reader, Writer};
use anyhow::{bail, Result};
use std::time::{Duration, Instant};

mod asks;
mod calls;
mod channels;
mod cover;
mod fields;
mod link;
mod notices;
mod requests;
mod stranger;
#[cfg(test)]
mod tests;

pub use asks::*;
pub use calls::*;
pub use channels::*;
pub use cover::*;
use fields::*;
pub use link::*;
pub use notices::*;
pub use requests::*;
pub use stranger::*;

/// The protocol's version, sent in every `Hello`. It goes up only with
/// a change the build before can't follow.
pub const PROTOCOL_VERSION: u16 = 1;
/// The oldest version this build can still talk to.
pub const MIN_PROTOCOL_VERSION: u16 = 1;

const TAG_PING: u8 = 0x01;
const TAG_MESSAGE: u8 = 0x02;
const TAG_LIST_POSTS: u8 = 0x03;
const TAG_WHO_AM_I: u8 = 0x06;
const TAG_FILE_CHUNK: u8 = 0x07;
const TAG_PROFILE_ASK: u8 = 0x09;
const TAG_JOIN_USER: u8 = 0x0B;
const TAG_PASS_ON: u8 = 0x0C;
const TAG_FILE_CHUNK_THROUGH: u8 = 0x0D;
const TAG_PROFILE_THROUGH: u8 = 0x0F;
const TAG_WITHDRAW_CONTACT_REQUEST: u8 = 0x10;
const TAG_UNSAY: u8 = 0x11;
const TAG_CHANNEL_CATCH_UP: u8 = 0x12;
const TAG_PONG: u8 = 0x40;
const TAG_ACK: u8 = 0x41;
const TAG_POSTS: u8 = 0x42;
const TAG_DISPLAY_NAME: u8 = 0x45;
const TAG_UNSUPPORTED: u8 = 0x46;
const TAG_FILE_DATA: u8 = 0x47;
const TAG_FILE_ERROR: u8 = 0x48;
const TAG_PROFILE: u8 = 0x49;
const TAG_REFUSED: u8 = 0x4A;
const TAG_USER_KEY: u8 = 0x4B;
const TAG_CHANNEL_DATA: u8 = 0x4C;
const TAG_CHANNEL_ERROR: u8 = 0x4D;
const TAG_HELLO: u8 = 0x80;
const TAG_COVER_HELLO: u8 = 0x81;
const TAG_NOT_PEER: u8 = 0x82;
const TAG_INTRODUCE: u8 = 0x83;
const TAG_CALL_INVITE: u8 = 0x84;
const TAG_CALL_ACCEPT: u8 = 0x85;
const TAG_CALL_REJECT: u8 = 0x86;
const TAG_CALL_HANGUP: u8 = 0x87;
const TAG_CALL_AUDIO: u8 = 0x88;
const TAG_COVER_ACK: u8 = 0x89;
const TAG_REKEY: u8 = 0xA6;
const TAG_CALL_VIDEO: u8 = 0x8A;
const TAG_CALL_KEYFRAME: u8 = 0x8B;
const TAG_FILE_OFFER: u8 = 0x8C;
const TAG_FILE_ANSWER: u8 = 0x8D;
const TAG_FILE_CANCEL: u8 = 0x8E;
const TAG_FILE_DONE: u8 = 0x8F;
const TAG_BUNDLE: u8 = 0x96;
const TAG_CARBON: u8 = 0x98;
const TAG_CALL_ELSEWHERE: u8 = 0x99;
const TAG_FOLLOW: u8 = 0x9A;
const TAG_ADDRESSES: u8 = 0x9B;
const TAG_CARD: u8 = 0x9C;
const TAG_CALL_RESUME: u8 = 0x9D;
const TAG_CALL_TAKEOVER: u8 = 0xBB;
const TAG_CALL_CAMERA_OFF: u8 = 0x9E;
const TAG_CALL_SCREEN: u8 = 0x9F;
const TAG_CALL_CONFERENCE: u8 = 0xA0;
const TAG_CALL_ROSTER: u8 = 0xA1;
const TAG_CALL_HEARING: u8 = 0xA2;
const TAG_CALL_FORWARD: u8 = 0xA3;
const TAG_CALL_RELAYED: u8 = 0xA4;
const TAG_TIMER: u8 = 0xA5;
const TAG_CALL_DECODES: u8 = 0xA7;
const TAG_CALL_SHOWN_IN: u8 = 0xA8;
const TAG_CALL_VIA: u8 = 0xA9;
const TAG_PROFILE_CHANGED: u8 = 0xAA;
const TAG_CALL_CONTROL: u8 = 0xAB;
const TAG_CALL_INPUT: u8 = 0xAC;
const TAG_PEERS_BOOK: u8 = 0xAF;
const TAG_REMOVED: u8 = 0xB0;
const TAG_DEVICE_LIST: u8 = 0xB1;
const TAG_DEVICE_ADDRESSES: u8 = 0xB2;
const TAG_REQUESTS_BOOK: u8 = 0xB3;
const TAG_REACHING: u8 = 0xB4;
const TAG_CALL_THROUGH: u8 = 0xB5;
const TAG_CALL_ROOM: u8 = 0xB6;
const TAG_FILE_THROUGH: u8 = 0xB7;
const TAG_CALL_CONFERENCE_LINK: u8 = 0xB8;
const TAG_MANUAL_ADDRESS: u8 = 0xB9;
const TAG_CARBON_FOR: u8 = 0xBA;
const TAG_REACHING_DELAYS: u8 = 0xBC;
const TAG_READ_MARKS: u8 = 0xBE;
const TAG_WITHDRAWALS_BOOK: u8 = 0xAE;
const TAG_DELIVERED: u8 = 0x93;
const TAG_UNSAID: u8 = 0x94;
const TAG_DISCOVERABLE: u8 = 0x95;
const TAG_RECEIVE_LIMIT: u8 = 0x97;
const TAG_CHANNEL_POST: u8 = 0xC1;
const TAG_CHANNEL_EVENT: u8 = 0xC2;
const TAG_GROUP_INTRO: u8 = 0xC3;
const TAG_FILE_CLOSED: u8 = 0xC4;

/// A frame's kind, by its tag (its first byte), for what's counted
/// (`tally`): "?" for one this build doesn't know.
pub(crate) fn tag_name(tag: u8) -> &'static str {
    match tag {
        TAG_PING => "ping",
        TAG_MESSAGE => "message",
        TAG_LIST_POSTS => "list posts",
        TAG_WHO_AM_I => "who am i",
        TAG_FILE_CHUNK => "file chunk",
        TAG_CHANNEL_CATCH_UP => "group catch up",
        TAG_PROFILE_ASK => "profile ask",
        TAG_JOIN_USER => "join user",
        TAG_PASS_ON => "pass on",
        TAG_FILE_CHUNK_THROUGH => "file chunk through",
        TAG_PROFILE_THROUGH => "profile through",
        TAG_WITHDRAW_CONTACT_REQUEST => "withdraw contact request",
        TAG_UNSAY => "unsay",
        TAG_PONG => "pong",
        TAG_ACK => "ack",
        TAG_POSTS => "posts",
        TAG_CHANNEL_DATA => "group data",
        TAG_CHANNEL_ERROR => "group error",
        TAG_DISPLAY_NAME => "display name",
        TAG_UNSUPPORTED => "unsupported",
        TAG_FILE_DATA => "file data",
        TAG_FILE_ERROR => "file error",
        TAG_PROFILE => "profile",
        TAG_REFUSED => "refused",
        TAG_USER_KEY => "user key",
        TAG_HELLO => "hello",
        TAG_COVER_HELLO => "cover hello",
        TAG_NOT_PEER => "not peer",
        TAG_INTRODUCE => "introduce",
        TAG_CALL_INVITE => "call invite",
        TAG_CALL_ACCEPT => "call accept",
        TAG_CALL_REJECT => "call reject",
        TAG_CALL_HANGUP => "call hangup",
        TAG_CALL_AUDIO => "call audio",
        TAG_COVER_ACK => "cover ack",
        TAG_REKEY => "rekey",
        TAG_CALL_VIDEO => "call video",
        TAG_CALL_KEYFRAME => "call keyframe",
        TAG_FILE_OFFER => "file offer",
        TAG_FILE_ANSWER => "file answer",
        TAG_FILE_CANCEL => "file cancel",
        TAG_FILE_DONE => "file done",
        TAG_FILE_CLOSED => "file closed",
        TAG_CHANNEL_POST => "group post",
        TAG_CHANNEL_EVENT => "group roster",
        TAG_GROUP_INTRO => "group intro",
        TAG_BUNDLE => "bundle",
        TAG_CARBON => "carbon",
        TAG_CALL_ELSEWHERE => "call elsewhere",
        TAG_FOLLOW => "follow",
        TAG_ADDRESSES => "addresses",
        TAG_CARD => "card",
        TAG_CALL_RESUME => "call resume",
        TAG_CALL_TAKEOVER => "call takeover",
        TAG_CALL_CAMERA_OFF => "call camera off",
        TAG_CALL_SCREEN => "call screen",
        TAG_CALL_CONFERENCE => "call conference",
        TAG_CALL_ROSTER => "call roster",
        TAG_CALL_HEARING => "call hearing",
        TAG_CALL_FORWARD => "call forward",
        TAG_CALL_RELAYED => "call relayed",
        TAG_TIMER => "timer",
        TAG_CALL_DECODES => "call decodes",
        TAG_CALL_SHOWN_IN => "call shown in",
        TAG_CALL_VIA => "call via",
        TAG_PROFILE_CHANGED => "profile changed",
        TAG_CALL_CONTROL => "call control",
        TAG_CALL_INPUT => "call input",
        TAG_PEERS_BOOK => "peers book",
        TAG_REMOVED => "removed",
        TAG_DEVICE_LIST => "device list",
        TAG_DEVICE_ADDRESSES => "device addresses",
        TAG_REQUESTS_BOOK => "requests book",
        TAG_REACHING => "reaching",
        TAG_CALL_THROUGH => "call through",
        TAG_CALL_ROOM => "call room",
        TAG_FILE_THROUGH => "file through",
        TAG_CALL_CONFERENCE_LINK => "call conference link",
        TAG_MANUAL_ADDRESS => "manual address",
        TAG_CARBON_FOR => "carbon for",
        TAG_REACHING_DELAYS => "reaching delays",
        TAG_READ_MARKS => "read marks",
        TAG_WITHDRAWALS_BOOK => "withdrawals book",
        TAG_DELIVERED => "delivered",
        TAG_UNSAID => "unsaid",
        TAG_DISCOVERABLE => "discoverable",
        TAG_RECEIVE_LIMIT => "receive limit",
        _ => "?",
    }
}

fn is_request_tag(tag: u8) -> bool {
    (0x01..=0x3F).contains(&tag)
}

/// The longest message text accepted, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 4000;

/// How many bytes of items one part of a split reply may carry — leaves
/// room under `framing::MAX_FRAME` for the frame's own header, the Noise
/// tag, and the one item that tips a part over.
const REPLY_PART_BUDGET: usize = 4000;

/// How long a voice frame queued in the cover slots may wait before
/// it's dropped instead of sent — late voice is worse than a gap.
const AUDIO_STALE_AFTER: Duration = Duration::from_millis(200);

/// Per-slot overhead this module adds around `cover::Outbound`'s own
/// slot body: the datagram's sequence number and seal (see `datagram`).
const COVER_OVERHEAD_PER_SLOT: usize = crate::datagram::OVERHEAD;

pub use crate::cover::SLOT_INTERVAL as COVER_SLOT_INTERVAL;

/// How often a persistent connection sends a `Ping` (see `swarm-cli`'s
/// `node::peer::PeerTask`) — so the other side always hears from it
/// at least this often, even with nothing else to say and no cover on.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// How long a one-shot request (and a dial) waits for an answer before
/// giving up.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// A persistent connection we've heard nothing at all on for this long —
/// no frame, no cover slot — is dead. Anything the peer sends counts, so
/// a reply held up behind a long queue of cover slots doesn't make a
/// healthy connection look dead the way waiting on one specific reply
/// did. A peer always sends at least a `Ping` every `HEARTBEAT_INTERVAL`.
pub const LIVENESS_TIMEOUT: Duration = Duration::from_secs(10);

/// Who's at the sending end of a connection, as its `Hello` says: its
/// user key, its device's key and the user's signature on it, and the
/// device's signature over this connection's handshake (see
/// `identity::device_cert`, `device_proof`).
#[derive(Clone, Debug, PartialEq)]
struct DeviceProof {
    user: [u8; 32],
    device: [u8; 32],
    cert: [u8; 64],
    proof: [u8; 64],
}

impl DeviceProof {
    fn mine(keys: crate::transport::Keys<'_>, handshake: &[u8; 32]) -> DeviceProof {
        let device = keys.device.public_key_bytes();
        DeviceProof {
            user: keys.user.public_key_bytes(),
            device,
            cert: crate::identity::device_cert(keys.user, &device),
            proof: crate::identity::device_proof(keys.device, handshake),
        }
    }

    /// Why it doesn't hold for a connection whose Noise handshake (hash
    /// `handshake`) authenticated `noise_key` (X25519), if it doesn't.
    fn fails(&self, noise_key: &[u8; 32], handshake: &[u8; 32]) -> Option<&'static str> {
        if crate::identity::ed25519_public_key_to_x25519(&self.user).ok().as_ref() != Some(noise_key) {
            Some("its user key isn't the key it connected with")
        } else if !crate::identity::verify_device_cert(&self.user, &self.device, &self.cert) {
            Some("its device isn't signed in by its user key")
        } else if !crate::identity::verify_device_proof(&self.device, handshake, &self.proof) {
            Some("its device didn't prove it holds its key")
        } else {
            None
        }
    }
}

/// A connection whose other end didn't prove its device (see
/// `protocol::greet`): not to be used.
#[derive(Debug)]
pub struct DeviceUnproved(pub &'static str);

impl std::fmt::Display for DeviceUnproved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused: {}", self.0)
    }
}

impl std::error::Error for DeviceUnproved {}

/// For `fuzz/` only (`cargo fuzz` builds with `--cfg fuzzing`; no real
/// build has it): `bytes` decoded as a frame from the wire is, and as a
/// bundle's notices. Whatever decodes has to encode to bytes that decode
/// to the same again.
#[cfg(fuzzing)]
pub fn fuzz_frame(bytes: &[u8]) {
    if let Ok(frame) = Frame::decode(bytes)
        && !matches!(frame, Frame::Unknown | Frame::UnknownRequest(_))
    {
        let encoded = frame.encode();
        let again = Frame::decode(&encoded).expect("a frame that decoded, encoded, decodes again");
        assert!(again.encode() == encoded, "a frame that decoded, encoded, decodes to the same again");
    }
    let _ = bundle_notices(bytes);
}

enum Frame {
    /// `datagram_key` is what the sender seals its cover datagrams with
    /// (see `datagram`); `me`, who's sending.
    Hello { version: u16, min_version: u16, datagram_key: [u8; 32], seen_as: String, me: DeviceProof },
    /// How much of the other side's cover streams (urgent, bulk) we've
    /// received — sent on the QUIC stream by a side with no cover slots
    /// of its own to carry it (see `cover`). Handled inside `recv`,
    /// never surfaced.
    CoverAck(u64, u64, u64),
    /// New keys from here on (see `REKEY_EVERY`): what the sender seals
    /// its datagrams with shortly (`datagram_key`), and — if this came on
    /// the stream, not in a cover slot — the stream's next frames from it
    /// are under Noise's next key. Handled inside `recv`, never surfaced.
    Rekey { datagram_key: [u8; 32] },
    Request(RequestId, Request),
    Reply(RequestId, Reply),
    Notice(Notice),
    /// A request of a type this build doesn't know.
    UnknownRequest(RequestId),
    /// A reply or notice of a type this build doesn't know.
    Unknown,
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        match self {
            Frame::Hello { version, min_version, datagram_key, seen_as, me } => {
                let mut w = Writer::new(TAG_HELLO);
                w.u16(*version).u16(*min_version).array(datagram_key).str(seen_as);
                w.array(&me.user).array(&me.device).array(&me.cert).array(&me.proof);
                w.into_bytes()
            }
            Frame::CoverAck(urgent, bulk, background) => {
                let mut w = Writer::new(TAG_COVER_ACK);
                w.u64(*urgent).u64(*bulk).u64(*background);
                w.into_bytes()
            }
            Frame::Rekey { datagram_key } => {
                let mut w = Writer::new(TAG_REKEY);
                w.array(datagram_key);
                w.into_bytes()
            }
            Frame::Request(id, request) => encode_request(id, request),
            Frame::Reply(id, reply) => encode_reply(id, reply),
            Frame::Notice(notice) => encode_notice(notice),
            Frame::UnknownRequest(_) | Frame::Unknown => unreachable!("only ever decoded, never sent"),
        }
    }

    fn decode(bytes: &[u8]) -> Result<Frame> {
        let Some((&tag, body)) = bytes.split_first() else { bail!("empty frame") };
        let mut r = Reader::new(body);
        let frame = match tag {
            TAG_HELLO => Frame::Hello {
                version: r.u16()?,
                min_version: r.u16()?,
                datagram_key: r.array()?,
                seen_as: r.str()?,
                me: DeviceProof { user: r.array()?, device: r.array()?, cert: r.array()?, proof: r.array()? },
            },
            TAG_COVER_ACK => Frame::CoverAck(r.u64()?, r.u64()?, r.u64()?),
            TAG_REKEY => Frame::Rekey { datagram_key: r.array()? },
            _ => {
                if let Some(frame) = decode_request(tag, &mut r)? {
                    frame
                } else if let Some(frame) = decode_reply(tag, &mut r)? {
                    frame
                } else if let Some(frame) = decode_call(tag, &mut r)? {
                    frame
                } else if let Some(frame) = decode_notice(tag, &mut r)? {
                    frame
                } else if is_request_tag(tag) {
                    Frame::UnknownRequest(r.u32()?)
                } else {
                    Frame::Unknown
                }
            }
        };
        Ok(frame)
    }
}

/// Why a message wasn't sent: the other side hasn't added us as a contact
/// (see `Notice::NotPeer`). Callers tell it apart from other failures
/// with `anyhow::Error::is::<NotAPeer>()` — it's not "offline", and
/// retrying won't help until they add us.
#[derive(Debug)]
pub struct NotAPeer;

impl std::fmt::Display for NotAPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "they haven't added you as a contact yet")
    }
}

impl std::error::Error for NotAPeer {}

/// The other side runs a swarm whose protocol this one can't speak (see
/// `greet`). `theirs` is their version, or `None` if they didn't open
/// with a `Hello` this build can read.
#[derive(Debug)]
pub struct IncompatibleVersion {
    pub theirs: Option<u16>,
}

impl std::fmt::Display for IncompatibleVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.theirs {
            Some(v) if v < PROTOCOL_VERSION => write!(f, "they run an older Swarm (protocol {v}, this is {PROTOCOL_VERSION}) — they need to update"),
            Some(v) => write!(f, "they run a newer Swarm (protocol {v}, this is {PROTOCOL_VERSION}) — update this one"),
            None => write!(f, "they run a Swarm this one can't talk to — one of you needs to update"),
        }
    }
}

impl std::error::Error for IncompatibleVersion {}

/// Who we are, for an `Introduce` notice — see `send_introduce`.
#[derive(Clone)]
pub struct Introduction {
    pub root_key: [u8; 32],
    pub name: Option<DisplayName>,
}
