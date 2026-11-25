//! Notices on the wire: one-way frames, all but a call's signals (`calls`).

use super::*;
use crate::readmarks::Chat;

/// One peer in the peers book a user's devices share: who, by what name
/// and where, whether they're a peer (`present`) or were taken off, and
/// when that was so (unix seconds, by the device that did it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookPeer {
    pub key: [u8; 32],
    pub name: String,
    pub address: String,
    pub at: u64,
    pub present: bool,
}

/// One line of the contact requests a user's devices share (see
/// `requests`, `node::ownrequests`), with the name the stranger
/// introduced themselves by, signed, if there was one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookRequest {
    pub request: crate::requests::Request,
    pub name: Option<DisplayName>,
}

/// One of a user's devices, in a `Notice::DeviceAddresses`: where it
/// last said it can be reached (`addresses`), and when (`at`, Unix
/// seconds, by the clock of whichever device it said so to).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAddress {
    pub device: [u8; 32],
    pub at: u64,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Each side's opening frame on a persistent connection: the cover
    /// rate it sends at, and the rate (if any) it requires of the other
    /// side's direction. See `send_cover_hello`.
    ///
    /// `limit`: the most the sender wants sent to it, all told, in bits a
    /// second (its receive limit; see `Notice::ReceiveLimit`).
    CoverHello { sends: Option<u64>, requires: Option<u64>, limit: Option<u64> },
    /// The sender's receive limit, changed since its `CoverHello` (`None`:
    /// none now): what this side sends it is held under it from now on —
    /// call video at once, cover from its next connection (see
    /// `node::peer`). A request: nothing at the receiving end can make it so.
    ///
    /// `in_call`: the sender's in a call (with anyone), or no longer: a
    /// file it's sending this side is asked for a few chunks at a time
    /// meanwhile, so the call has the link (`node::files`). Sent again
    /// each time that changes, the limit as it is. After `limit`; a build
    /// before it reads none, and a build before it reads past it.
    ReceiveLimit { limit: Option<u64>, in_call: bool },
    /// "You're not on my peers list." The first thing a connection from
    /// someone we haven't added hears (see `serve_connection`) — in place
    /// of the `CoverHello` a peer's connection opens with — so their
    /// side knows before sending anything that we won't take messages
    /// from them yet. Also the answer to a `Message` that comes anyway.
    NotPeer,
    /// The answer to `NotPeer`: who's asking (their root key — checked
    /// with `verified_requester` before use) and their display name, if
    /// they've set one (still needs `displayname::verify`), so it can be
    /// shown as a contact request.
    Introduce { root_key: [u8; 32], name: Option<DisplayName> },
    /// The answer to an `Introduce` from someone this side has blocked,
    /// or deleted as a contact just now (`node::Ctx::deleted`): "I've
    /// removed you." Their side stops dialling and shows
    /// it, until they remove us too or we add them back.
    Removed,
    /// Anything to do with a voice call — see `call`'s module docs.
    Call(CallSignal),
    /// "I'd like to send you this file" — see `transfer`. Repeated on
    /// every connection until it's answered (or, once accepted, done).
    FileOffer { id: crate::transfer::TransferId, name: String, size: u64, sha256: [u8; 32] },
    /// The answer to an offer: accepted, or declined (final).
    FileAnswer { id: crate::transfer::TransferId, accepted: bool },
    /// Either side giving up on a transfer (final), and why.
    FileCancel { id: crate::transfer::TransferId, reason: String },
    /// The receiver has the whole file, checked.
    FileDone { id: crate::transfer::TransferId },
    /// The sender's answer to the receiver's last word on a transfer
    /// (done, declined or cancelled): it knows, and won't offer it again,
    /// so the receiver's record can go once cleared (`Transfer::settled`).
    /// Older versions don't send it; theirs go after `transfer::KEPT`.
    FileClosed { id: crate::transfer::TransferId },
    /// A new post in a group both sides are in — passed on from member
    /// to member (see `node::channels`).
    ChannelPost { channel: ChannelId, post: ChannelPost },
    /// A roster entry (someone added or leaving, a name, a timer), passed
    /// on the same way; and to someone added, what tells them they're in.
    ChannelEvent { channel: ChannelId, event: RosterEvent },
    /// Someone in a group asking another in it, who isn't their contact,
    /// to add them: passed on through the group (`groupintro`).
    GroupIntro(crate::groupintro::Intro),
    /// Between a user's devices: entries of their device list (see
    /// `devicelist`), all of them on connecting, in as many notices as it
    /// takes; new ones as they're made.
    DeviceList { entries: Vec<crate::devicelist::Entry> },
    /// Between a user's devices: where the sender's other devices last
    /// said they can be reached, and when (see `node::devices`), so two
    /// that have never met can find each other. At most
    /// `devicelist::MAX_DEVICES` of them.
    DeviceAddresses(Vec<DeviceAddress>),
    /// Part of what a device gives another of the user's the first time
    /// they're linked (see `bundle`), `more` until the last.
    Bundle { more: bool, bytes: Vec<u8> },
    /// Between a user's devices: their peers book (see
    /// `node::ownpeers`), so a peer added or removed on one is on
    /// every one.
    PeersBook { entries: Vec<BookPeer> },
    /// Between a user's devices: contact requests, and what was done
    /// with them (see `node::ownrequests`), so a stranger who reached one
    /// device is asking on every one. Possibly in several parts: `more`
    /// on all but the last, so the whole of it is known (what isn't in
    /// it, the device hasn't got; see `node::settle`).
    RequestsBook { entries: Vec<BookRequest>, more: bool },
    /// Between a user's devices: what's been taken back and who's still
    /// to be told (see `node::withdraw`). Any number of these: each is
    /// merged as it comes.
    WithdrawalsBook { entries: Vec<crate::withdrawals::Withdrawal> },
    /// Between a user's devices: how far chats have been read (see
    /// `readmarks`), those that have moved since the last. At most
    /// `MOST_READ_MARKS`.
    ReadMarks { entries: Vec<(crate::readmarks::Chat, u64)> },
    /// Between one account's devices: a copy of a message this device
    /// sent to (`sent`) or received from peer `peer`, so every device
    /// shows the whole conversation.
    /// `id` is the message's own (see `outbox::MessageId`), so one that
    /// also arrives straight from them isn't shown twice. `expires`: when
    /// it deletes itself (unix seconds; 0, never).
    Carbon { peer: [u8; 32], id: MessageId, sent: bool, at: u64, expires: u64, text: String },
    /// From someone who isn't a peer, before asking for posts: they
    /// follow this device's public posts (`following`), or have stopped.
    /// `root_key` is theirs, checked with `verified_requester` like
    /// `Introduce`'s. Not a contact request; only counted (see
    /// `followers`).
    Follow { root_key: [u8; 32], following: bool },
    /// Between peers: where the sender can be reached now, most useful
    /// first (see `addressbook`). Sent when the connection comes up and
    /// whenever they change; at most `addressbook::MAX_PER_PEER`.
    Addresses(Vec<String>),
    /// Between the user's devices: the address they set by hand, and
    /// when (see `myaddress`); the later wins. At most `myaddress::
    /// MOST_ITEMS`.
    ManualAddress { at: u64, items: Vec<String> },
    /// Between the user's devices: whether they're discoverable on a
    /// network, and when that was set (see `discoverable`); the later
    /// wins.
    Discoverable { at: u64, on: bool },
    /// Between peers: a card for someone the sender has lost touch with,
    /// to be passed on to them if they're connected (see `card`).
    Card(crate::card::Card),
    /// Between peers: the sender's timer for their conversation (see
    /// `disappearing`), sent when the connection comes up and whenever
    /// it's set.
    Timer(crate::disappearing::Timer),
    /// Between peers: the sender's status or picture has changed, to be
    /// asked for again (`Request::Profile`).
    ProfileChanged,
    /// Between a user's devices: the peers (their user keys) the sender
    /// is connected to directly now, and will pass things on to for the
    /// user's other devices (see `node::proxy`); none, if it isn't let
    /// relay. At most `MAX_REACHING`.
    Reaching(Vec<[u8; 32]>),
    /// Between a user's devices: how long a round trip takes from the
    /// sender to each of those it's said it reaches (`Reaching`), in
    /// milliseconds, so that another device can choose between relays
    /// (see `node::proxy`). Sent before the `Reaching` it goes with, and
    /// again if they've changed by much.
    /// At most `MAX_REACHING`.
    ReachingDelays(Vec<([u8; 32], u16)>),
    /// Between a user's devices: one signal of a call with peer `contact`
    /// (their user key) that the device let relay passes on between that
    /// peer's connection and this link, as it is (see `node::proxy`).
    CallThrough { contact: [u8; 32], signal: CallSignal },
    /// Between a user's devices, about a call through one of them with
    /// peer `contact`, once a second: from the one passing it on, what its
    /// connection to them carries (`audio` bytes a frame, `video_bps`,
    /// `video_piece`); from the other, what its call wants (`video_on`,
    /// and `bigger`, what the next bigger shape needs; `relayed`, how many
    /// others' voices a conference passes on along it, which share the
    /// connection's room for voice with ours). From the one passing it on,
    /// too, whether that connection has cover (`covered`), so others' pictures aren't passed on along it (`CallBudget::covered`).
    /// Each side reads only its half.
    CallRoom { contact: [u8; 32], audio: u32, video_bps: u32, video_piece: u32, video_on: bool, bigger: u32, relayed: u32, covered: bool },
    /// Between a user's devices: one of a file transfer's notices
    /// (`FileOffer`, `FileAnswer`, `FileCancel`, `FileDone`) with peer
    /// `contact` (the key the transfer goes by), passed on as it is by the
    /// device let relay (see `node::proxyfile`): `onward`, for it to pass
    /// on to them; otherwise, from them.
    FileThrough { contact: [u8; 32], onward: bool, notice: Box<Notice> },
    /// Between a user's devices: a copy of a message (`Carbon`) for
    /// `device`, a third of theirs that the sender has no link to and the
    /// device let relay has: for it to keep for that device and pass on
    /// (see `node::devices`).
    CarbonFor { device: [u8; 32], carbon: Box<Notice> },
    /// Between a user's devices: message `id`, sent to peer `peer` (their
    /// user key) from one of the user's devices, has reached one of the
    /// peer's devices (see `waiting`). Kept and passed on as copies of
    /// messages are (`Carbon`, `CarbonFor`).
    Delivered { peer: [u8; 32], id: MessageId },
    /// Between a user's devices: `line` of the conversation with peer
    /// `peer` (their user key) is deleted. `done`: it's gone, to take out
    /// of the history (theirs, or one sent that they've heard is gone);
    /// otherwise, sent by the user and deleted, still to tell them
    /// (`unsaying`). Kept and passed on as copies of messages are.
    Unsaid { peer: [u8; 32], line: crate::history::LineRef, done: bool },
}

impl Notice {
    /// The most marks one `ReadMarks` carries: inside a frame, even for
    /// channels with the longest names.
    pub const MOST_READ_MARKS: usize = 25;
}

/// What `Notice::CallThrough` adds to the call signal it carries: its tag
/// and the peer's key.
pub const CALL_THROUGH_OVERHEAD: usize = 33;

/// A `Notice::CallThrough`'s bytes, made without copying the signal.
pub(super) fn call_through_bytes(contact: &[u8; 32], signal: CallSignal) -> Vec<u8> {
    let inner = Frame::Notice(Notice::Call(signal)).encode();
    let mut bytes = Vec::with_capacity(CALL_THROUGH_OVERHEAD + inner.len());
    bytes.push(TAG_CALL_THROUGH);
    bytes.extend_from_slice(contact);
    bytes.extend(inner);
    bytes
}

/// The most peers a `Notice::Reaching` lists (fitting in one frame).
pub const MAX_REACHING: usize = 200;

/// A notice's bytes.
pub(super) fn encode_notice(notice: &Notice) -> Vec<u8> {
    match notice {
        Notice::CoverHello { sends, requires, limit } => {
            let mut w = Writer::new(TAG_COVER_HELLO);
            w.u64(sends.unwrap_or(0)).u64(requires.unwrap_or(0)).u64(limit.unwrap_or(0));
            w.into_bytes()
        }
        Notice::ReceiveLimit { limit, in_call } => {
            let mut w = Writer::new(TAG_RECEIVE_LIMIT);
            w.u64(limit.unwrap_or(0)).u8(*in_call as u8);
            w.into_bytes()
        }
        Notice::NotPeer => vec![TAG_NOT_PEER],
        Notice::Removed => vec![TAG_REMOVED],
        Notice::Introduce { root_key, name } => {
            let mut w = Writer::new(TAG_INTRODUCE);
            w.array(root_key);
            write_display_name(&mut w, name);
            w.into_bytes()
        }
        Notice::FileOffer { id, name, size, sha256 } => {
            let mut w = Writer::new(TAG_FILE_OFFER);
            w.array(id).str(name).u64(*size).array(sha256);
            w.into_bytes()
        }
        Notice::FileAnswer { id, accepted } => {
            let mut w = Writer::new(TAG_FILE_ANSWER);
            w.array(id).bool(*accepted);
            w.into_bytes()
        }
        Notice::FileCancel { id, reason } => {
            let mut w = Writer::new(TAG_FILE_CANCEL);
            w.array(id).str(reason);
            w.into_bytes()
        }
        Notice::FileDone { id } => {
            let mut w = Writer::new(TAG_FILE_DONE);
            w.array(id);
            w.into_bytes()
        }
        Notice::FileClosed { id } => {
            let mut w = Writer::new(TAG_FILE_CLOSED);
            w.array(id);
            w.into_bytes()
        }
        Notice::ChannelPost { channel, post } => {
            let mut w = Writer::new(TAG_CHANNEL_POST);
            write_channel_id(&mut w, channel);
            write_channel_post(&mut w, post);
            w.into_bytes()
        }
        Notice::ChannelEvent { channel, event } => {
            let mut w = Writer::new(TAG_CHANNEL_EVENT);
            write_channel_id(&mut w, channel);
            write_roster_event(&mut w, event);
            w.into_bytes()
        }
        Notice::GroupIntro(intro) => {
            let mut w = Writer::new(TAG_GROUP_INTRO);
            intro.write(&mut w);
            w.into_bytes()
        }
        Notice::DeviceList { entries } => {
            let mut w = Writer::new(TAG_DEVICE_LIST);
            write_list(&mut w, entries, write_list_entry);
            w.into_bytes()
        }
        Notice::Bundle { more, bytes } => {
            let mut w = Writer::new(TAG_BUNDLE);
            w.bool(*more).bytes(bytes);
            w.into_bytes()
        }
        Notice::ProfileChanged => vec![TAG_PROFILE_CHANGED],
        Notice::PeersBook { entries } => {
            let mut w = Writer::new(TAG_PEERS_BOOK);
            write_list(&mut w, entries, |w, e| {
                w.array(&e.key).str(&e.name).str(&e.address).u64(e.at).bool(e.present);
            });
            w.into_bytes()
        }
        Notice::WithdrawalsBook { entries } => {
            let mut w = Writer::new(TAG_WITHDRAWALS_BOOK);
            write_list(&mut w, entries, |w, line| {
                w.array(&line.to).u64(line.changed_at).bool(line.waiting).str(&line.address);
            });
            w.into_bytes()
        }
        Notice::ReadMarks { entries } => {
            let mut w = Writer::new(TAG_READ_MARKS);
            write_list(&mut w, entries, |w, (chat, at)| {
                w.u64(*at);
                match chat {
                    Chat::Contact(key) => {
                        w.u8(0).array(key);
                    }
                    Chat::Channel(id) => {
                        w.u8(1);
                        write_channel_id(w, id);
                    }
                }
            });
            w.into_bytes()
        }
        Notice::RequestsBook { entries, more } => {
            let mut w = Writer::new(TAG_REQUESTS_BOOK);
            w.bool(*more);
            write_list(&mut w, entries, |w, e| {
                let r = &e.request;
                w.array(&r.public_key).u8(request_status_byte(r.status)).str(&r.address).u64(r.first_seen).u64(r.last_seen);
                write_display_name(w, &e.name);
            });
            w.into_bytes()
        }
        Notice::Timer(timer) => {
            let mut w = Writer::new(TAG_TIMER);
            w.u64(timer.seconds).u64(timer.set_at).array(&timer.by);
            w.into_bytes()
        }
        Notice::Card(card) => {
            let mut w = Writer::new(TAG_CARD);
            w.array(&card.from).array(&card.to).u64(card.at).array(&card.signature);
            let list: Vec<&String> = card.addresses.iter().take(crate::card::MAX_ADDRESSES).collect();
            w.u8(list.len() as u8);
            for a in list {
                w.str(a);
            }
            w.into_bytes()
        }
        Notice::DeviceAddresses(devices) => {
            let mut w = Writer::new(TAG_DEVICE_ADDRESSES);
            let devices: Vec<&DeviceAddress> = devices.iter().take(crate::devicelist::MAX_DEVICES).collect();
            w.u8(devices.len() as u8);
            for d in devices {
                let list: Vec<&String> = d.addresses.iter().take(crate::addressbook::MAX_PER_PEER).collect();
                w.array(&d.device).u64(d.at).u8(list.len() as u8);
                for a in list {
                    w.str(a);
                }
            }
            w.into_bytes()
        }
        Notice::Addresses(list) => {
            let mut w = Writer::new(TAG_ADDRESSES);
            let list: Vec<&String> = list.iter().take(crate::addressbook::MAX_PER_PEER).collect();
            w.u8(list.len() as u8);
            for a in list {
                w.str(a);
            }
            w.into_bytes()
        }
        Notice::ManualAddress { at, items } => {
            let mut w = Writer::new(TAG_MANUAL_ADDRESS);
            w.u64(*at);
            let items: Vec<&String> = items.iter().take(crate::myaddress::MOST_ITEMS).collect();
            w.u8(items.len() as u8);
            for a in items {
                w.str(a);
            }
            w.into_bytes()
        }
        Notice::Discoverable { at, on } => {
            let mut w = Writer::new(TAG_DISCOVERABLE);
            w.u64(*at);
            w.bool(*on);
            w.into_bytes()
        }
        Notice::Reaching(contacts) => {
            let mut w = Writer::new(TAG_REACHING);
            let contacts: Vec<&[u8; 32]> = contacts.iter().take(MAX_REACHING).collect();
            w.u16(contacts.len() as u16);
            for c in contacts {
                w.array(c);
            }
            w.into_bytes()
        }
        Notice::ReachingDelays(delays) => {
            let mut w = Writer::new(TAG_REACHING_DELAYS);
            let delays: Vec<&([u8; 32], u16)> = delays.iter().take(MAX_REACHING).collect();
            w.u16(delays.len() as u16);
            for (key, ms) in delays {
                w.array(key).u16(*ms);
            }
            w.into_bytes()
        }
        Notice::CallThrough { contact, signal } => call_through_bytes(contact, signal.clone()),
        Notice::FileThrough { contact, onward, notice } => {
            let inner = Frame::Notice((**notice).clone()).encode();
            let mut bytes = Vec::with_capacity(34 + inner.len());
            bytes.push(TAG_FILE_THROUGH);
            bytes.extend_from_slice(contact);
            bytes.push(u8::from(*onward));
            bytes.extend(inner);
            bytes
        }
        Notice::CarbonFor { device, carbon } => {
            let inner = Frame::Notice((**carbon).clone()).encode();
            let mut bytes = Vec::with_capacity(33 + inner.len());
            bytes.push(TAG_CARBON_FOR);
            bytes.extend_from_slice(device);
            bytes.extend(inner);
            bytes
        }
        Notice::CallRoom { contact, audio, video_bps, video_piece, video_on, bigger, relayed, covered } => {
            let mut w = Writer::new(TAG_CALL_ROOM);
            w.array(contact).u32(*audio).u32(*video_bps).u32(*video_piece).bool(*video_on).u32(*bigger).u32(*relayed).bool(*covered);
            w.into_bytes()
        }
        Notice::Follow { root_key, following } => {
            let mut w = Writer::new(TAG_FOLLOW);
            w.array(root_key).bool(*following);
            w.into_bytes()
        }
        Notice::Delivered { peer, id } => {
            let mut w = Writer::new(TAG_DELIVERED);
            w.array(peer).array(id);
            w.into_bytes()
        }
        Notice::Unsaid { peer, line, done } => {
            let mut w = Writer::new(TAG_UNSAID);
            w.array(peer).u8(line.direction.tag().as_bytes()[0]).array(&line.id.unwrap_or_default()).u64(line.at).array(&line.digest).bool(*done);
            w.into_bytes()
        }
        Notice::Carbon { peer, id, sent, at, expires, text } => {
            let mut w = Writer::new(TAG_CARBON);
            w.array(peer).array(id).bool(*sent).u64(*at).str(text).u64(*expires);
            w.into_bytes()
        }
        Notice::Call(signal) => encode_call(signal),
    }
}

/// A notice (not a call's) with tag `tag`, if it's one this build knows.
pub(super) fn decode_notice(tag: u8, r: &mut Reader) -> Result<Option<Frame>> {
    Ok(Some(match tag {
        TAG_COVER_HELLO => {
            let sends = r.u64()?;
            let requires = r.u64()?;
            let limit = r.u64()?;
            Frame::Notice(Notice::CoverHello { sends: (sends > 0).then_some(sends), requires: (requires > 0).then_some(requires), limit: (limit > 0).then_some(limit) })
        }
        TAG_RECEIVE_LIMIT => {
            let limit = r.u64()?;
            let in_call = r.bool()?;
            Frame::Notice(Notice::ReceiveLimit { limit: (limit > 0).then_some(limit), in_call })
        }
        TAG_NOT_PEER => Frame::Notice(Notice::NotPeer),
        TAG_REMOVED => Frame::Notice(Notice::Removed),
        TAG_INTRODUCE => Frame::Notice(Notice::Introduce { root_key: r.array()?, name: read_display_name(r)? }),
        TAG_FILE_OFFER => Frame::Notice(Notice::FileOffer { id: r.array()?, name: r.str()?, size: r.u64()?, sha256: r.array()? }),
        TAG_FILE_ANSWER => Frame::Notice(Notice::FileAnswer { id: r.array()?, accepted: r.bool()? }),
        TAG_FILE_CANCEL => Frame::Notice(Notice::FileCancel { id: r.array()?, reason: crate::text::single_line(&r.str()?) }),
        TAG_FILE_DONE => Frame::Notice(Notice::FileDone { id: r.array()? }),
        TAG_FILE_CLOSED => Frame::Notice(Notice::FileClosed { id: r.array()? }),
        TAG_CHANNEL_POST => Frame::Notice(Notice::ChannelPost { channel: read_channel_id(r)?, post: read_channel_post(r)? }),
        TAG_CHANNEL_EVENT => Frame::Notice(Notice::ChannelEvent { channel: read_channel_id(r)?, event: read_roster_event(r)? }),
        TAG_GROUP_INTRO => Frame::Notice(Notice::GroupIntro(crate::groupintro::Intro::read(r)?)),
        TAG_DEVICE_LIST => Frame::Notice(Notice::DeviceList { entries: read_list(r, read_list_entry)? }),
        TAG_BUNDLE => Frame::Notice(Notice::Bundle { more: r.bool()?, bytes: r.bytes()?.to_vec() }),
        TAG_PEERS_BOOK => Frame::Notice(Notice::PeersBook {
            entries: read_list(r, |r| {
                Ok(BookPeer {
                    key: r.array()?,
                    name: crate::text::single_line(&r.str()?),
                    address: crate::text::single_line(&r.str()?),
                    at: r.u64()?,
                    present: r.bool()?,
                })
            })?,
        }),
        TAG_WITHDRAWALS_BOOK => Frame::Notice(Notice::WithdrawalsBook {
            entries: read_list(r, |r| {
                let (to, changed_at, waiting, address) = (r.array()?, r.u64()?, r.bool()?, r.str()?);
                Ok(crate::withdrawals::Withdrawal { what: crate::withdrawals::What::ContactRequest, to, address, changed_at, waiting })
            })?,
        }),
        TAG_READ_MARKS => Frame::Notice(Notice::ReadMarks {
            entries: read_list(r, |r| {
                let at = r.u64()?;
                let chat = match r.u8()? {
                    0 => Chat::Contact(r.array()?),
                    1 => Chat::Channel(read_channel_id(r)?),
                    other => bail!("unknown kind of chat {other}"),
                };
                Ok((chat, at))
            })?,
        }),
        TAG_REQUESTS_BOOK => Frame::Notice(Notice::RequestsBook {
            more: r.bool()?,
            entries: read_list(r, |r| {
                let public_key = r.array()?;
                let status = request_status(r.u8()?)?;
                let address = crate::text::single_line(&r.str()?);
                let (first_seen, last_seen) = (r.u64()?, r.u64()?);
                let name = read_display_name(r)?;
                Ok(BookRequest { request: crate::requests::Request { public_key, status, address, first_seen, last_seen }, name })
            })?,
        }),
        TAG_PROFILE_CHANGED => Frame::Notice(Notice::ProfileChanged),
        TAG_CARD => {
            let (from, to, at, signature) = (r.array()?, r.array()?, r.u64()?, r.array()?);
            let addresses = read_short_list(r, crate::card::MAX_ADDRESSES, |r| r.str())?;
            Frame::Notice(Notice::Card(crate::card::Card { from, to, at, addresses, signature }))
        }
        TAG_ADDRESSES => {
            let list = read_short_list(r, crate::addressbook::MAX_PER_PEER, |r| Ok(crate::text::single_line(&r.str()?)))?;
            Frame::Notice(Notice::Addresses(list))
        }
        TAG_MANUAL_ADDRESS => {
            let at = r.u64()?;
            let items = read_short_list(r, crate::myaddress::MOST_ITEMS, |r| Ok(crate::text::single_line(&r.str()?)))?;
            Frame::Notice(Notice::ManualAddress { at, items })
        }
        TAG_DISCOVERABLE => Frame::Notice(Notice::Discoverable { at: r.u64()?, on: r.bool()? }),
        TAG_DEVICE_ADDRESSES => {
            let devices = read_short_list(r, crate::devicelist::MAX_DEVICES, |r| {
                let (device, at) = (r.array()?, r.u64()?);
                let addresses = read_short_list(r, crate::addressbook::MAX_PER_PEER, |r| Ok(crate::text::single_line(&r.str()?)))?;
                Ok(DeviceAddress { device, at, addresses })
            })?;
            Frame::Notice(Notice::DeviceAddresses(devices))
        }
        TAG_REACHING => {
            let count = r.u16()? as usize;
            if count > MAX_REACHING {
                bail!("reaching more than {MAX_REACHING} peers");
            }
            let mut contacts = Vec::with_capacity(count);
            for _ in 0..count {
                contacts.push(r.array()?);
            }
            Frame::Notice(Notice::Reaching(contacts))
        }
        TAG_REACHING_DELAYS => {
            let count = r.u16()? as usize;
            if count > MAX_REACHING {
                bail!("delays to more than {MAX_REACHING} peers");
            }
            let mut delays = Vec::with_capacity(count);
            for _ in 0..count {
                let key = r.array()?;
                delays.push((key, r.u16()?));
            }
            Frame::Notice(Notice::ReachingDelays(delays))
        }
        TAG_CALL_THROUGH => {
            let contact = r.array()?;
            match Frame::decode(r.rest())? {
                Frame::Notice(Notice::Call(signal)) => Frame::Notice(Notice::CallThrough { contact, signal }),
                _ => Frame::Unknown,
            }
        }
        TAG_FILE_THROUGH => {
            let contact = r.array()?;
            let onward = r.bool()?;
            match Frame::decode(r.rest())? {
                Frame::Notice(
                    notice @ (Notice::FileOffer { .. } | Notice::FileAnswer { .. } | Notice::FileCancel { .. } | Notice::FileDone { .. }),
                ) => Frame::Notice(Notice::FileThrough { contact, onward, notice: Box::new(notice) }),
                _ => Frame::Unknown,
            }
        }
        TAG_CARBON_FOR => {
            let device = r.array()?;
            match Frame::decode(r.rest())? {
                Frame::Notice(carbon @ (Notice::Carbon { .. } | Notice::Delivered { .. } | Notice::Unsaid { .. })) => Frame::Notice(Notice::CarbonFor { device, carbon: Box::new(carbon) }),
                _ => Frame::Unknown,
            }
        }
        TAG_CALL_ROOM => Frame::Notice(Notice::CallRoom {
            contact: r.array()?,
            audio: r.u32()?,
            video_bps: r.u32()?,
            video_piece: r.u32()?,
            video_on: r.bool()?,
            bigger: r.u32()?,
            relayed: r.u32()?,
            covered: r.bool()?,
        }),
        TAG_FOLLOW => Frame::Notice(Notice::Follow { root_key: r.array()?, following: r.bool()? }),
        TAG_CARBON => {
            let (peer, id, sent, at, text) = (r.array()?, r.array()?, r.bool()?, r.u64()?, r.str()?);
            if text.len() > MAX_MESSAGE_BYTES {
                bail!("message longer than {MAX_MESSAGE_BYTES} bytes");
            }
            let expires = r.u64()?;
            Frame::Notice(Notice::Carbon { peer, id, sent, at, expires, text: crate::text::single_line(&text) })
        }
        TAG_DELIVERED => Frame::Notice(Notice::Delivered { peer: r.array()?, id: r.array()? }),
        TAG_UNSAID => {
            let peer = r.array()?;
            let Some(direction) = crate::history::Direction::parse(&char::from(r.u8()?).to_string()) else { bail!("not a way a message goes") };
            let id: MessageId = r.array()?;
            let (at, digest, done) = (r.u64()?, r.array()?, r.bool()?);
            Frame::Notice(Notice::Unsaid { peer, line: crate::history::LineRef { direction, id: (id != MessageId::default()).then_some(id), at, digest }, done })
        }
        TAG_TIMER => {
            let (seconds, set_at, by) = (r.u64()?, r.u64()?, r.array()?);
            Frame::Notice(Notice::Timer(crate::disappearing::Timer { seconds, set_at, by }))
        }
        _ => return Ok(None),
    }))
}
