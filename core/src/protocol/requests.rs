//! Requests and their replies on the wire.

use super::*;

/// A request's id, chosen by whoever sends it and echoed in its reply.
pub type RequestId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Ping,
    /// `id` is the message's own identity (see `outbox::MessageId`): the
    /// same on every resend, so the receiver can drop a copy it already
    /// has. `sent_at` is the original send time, so a delayed delivery
    /// still shows when it was written.
    /// `lifetime`: how long it lasts, in seconds, before it deletes
    /// itself (see `disappearing`); 0, for good.
    Message { id: MessageId, sent_at: u64, lifetime: u64, text: String },
    /// Asks for posts created at or after `since` (unix seconds) — the
    /// newest one the asker already has, so a refresh carries only
    /// what's new (0 for everything).
    ListPosts { since: u64 },
    /// Asks whoever's on the other end what display name they're
    /// currently publishing (see `displayname`) — answerable by anyone.
    WhoAmI,
    /// Asks for up to `len` bytes of offered file `id` from `offset` —
    /// see `transfer` for how a transfer goes.
    FileChunk { id: crate::transfer::TransferId, offset: u64, len: u32 },
    /// A member catching up on a group both sides are in: what the asker
    /// holds, summed up (see `channel::digests`). Answered with
    /// `ChannelData`: the whole roster, and the posts the asker lacks from
    /// when they were added.
    ChannelCatchUp { channel: ChannelId, from: u64, digests: Vec<BucketDigest> },
    /// A peer asking for this side's status and picture (see `profile`),
    /// if newer than the one it has (`have`, its time; 0 for none): the
    /// part from `from` in the picture. Only ever answered on a peer's
    /// connection.
    Profile { have: u64, from: u32 },
    /// A device being added to this side's user (see `node::joining`),
    /// called `name`: answered `UserKey`, or `Refused`. Only from the key
    /// a code this side gave out makes.
    JoinUser { name: String },
    /// From someone not on the list: the contact request they made is
    /// cancelled. `root_key` is theirs, checked with `verified_requester`
    /// like `Notice::Introduce`'s. Answered `Ack`, once it's taken in
    /// (see `node::withdraw`).
    WithdrawContactRequest { root_key: [u8; 32] },
    /// Between a user's devices: a message this device sent to peer
    /// `contact` (their user key), which it can't reach itself, for the
    /// other device to deliver over its own connection to them (see
    /// `node::proxy`); as `Message`, otherwise. Answered `Ack` once it's
    /// in the other device's outbox, or `Refused`.
    PassOn { contact: [u8; 32], id: MessageId, sent_at: u64, lifetime: u64, text: String },
    /// Between a user's devices: a chunk of transfer `id` with peer
    /// `contact` (the key it goes by), which the device let relay passes
    /// on between that peer's connection and this link (see
    /// `node::proxyfile`); answered as `FileChunk` is, or `Refused` when
    /// it can't be passed on just now.
    FileChunkThrough { contact: [u8; 32], id: crate::transfer::TransferId, offset: u64, len: u32 },
    /// Between a user's devices: peer `contact`'s status and picture, as
    /// the other device keeps them, otherwise as `Profile` (see
    /// `node::proxy`). They're signed by the peer, so a copy is as good as
    /// the original, and the peer needn't be online. Answered `Profile`,
    /// or `Refused`.
    ProfileThrough { contact: [u8; 32], have: u64, from: u32 },
    /// The message this side sent at `at` is deleted: it's to go from
    /// the conversation with them too, found by `id` (None for one sent
    /// before ids were kept), or else by `at` and its text's `digest`
    /// (`history::LineRef`). Answered `Ack` once it's gone; a build too
    /// old to know it answers `Unsupported`. See `node::unsay`.
    Unsay { id: Option<MessageId>, at: u64, digest: [u8; 16] },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Pong,
    Ack,
    /// One part of the answer to `ListPosts`; `more` until the last.
    /// `deleted`: posts the author has taken back since (see
    /// `posts::Deletion`), after the posts, and only when there are any
    /// (a build from before them reads past it).
    Posts { more: bool, posts: Vec<Post>, deleted: Vec<Deletion> },
    /// One part of the answer to `ChannelCatchUp`. `names` are members'
    /// own signed display names,
    /// passed along (each still checked against its key on arrival).
    ChannelData { more: bool, roster: Vec<RosterEvent>, posts: Vec<ChannelPost>, names: Vec<([u8; 32], DisplayName)> },
    ChannelError(String),
    DisplayName(Option<DisplayName>),
    /// "I don't know that request" — from a newer peer's request type.
    Unsupported,
    /// The bytes a `FileChunk` asked for.
    FileData(Vec<u8>),
    /// A `FileChunk` that can't be answered (cancelled, file gone).
    FileError(String),
    /// The answer to `Profile`: a part of this side's, or none newer than
    /// asked. Once gathered, still needs `profile::verify`
    /// (`profile::cache` does it).
    Profile(Option<crate::profile::ProfilePart>),
    /// A request this side won't do, and why (a `Relay`, say).
    Refused(String),
    /// The answer to `JoinUser`: the user key's recovery seed, and the
    /// user's device list with the new device on it (`devicelist`'s text).
    UserKey { seed: [u8; crate::identity::RECOVERY_SEED_LEN], list: String },
}

/// A request's bytes, under id `id`.
pub(super) fn encode_request(id: &RequestId, request: &Request) -> Vec<u8> {
        let tag = match request {
            Request::Ping => TAG_PING,
            Request::Message { .. } => TAG_MESSAGE,
            Request::ListPosts { .. } => TAG_LIST_POSTS,
            Request::WhoAmI => TAG_WHO_AM_I,
            Request::FileChunk { .. } => TAG_FILE_CHUNK,
            Request::ChannelCatchUp { .. } => TAG_CHANNEL_CATCH_UP,
            Request::Profile { .. } => TAG_PROFILE_ASK,
            Request::JoinUser { .. } => TAG_JOIN_USER,
            Request::WithdrawContactRequest { .. } => TAG_WITHDRAW_CONTACT_REQUEST,
            Request::PassOn { .. } => TAG_PASS_ON,
            Request::FileChunkThrough { .. } => TAG_FILE_CHUNK_THROUGH,
            Request::ProfileThrough { .. } => TAG_PROFILE_THROUGH,
            Request::Unsay { .. } => TAG_UNSAY,
        };
        let mut w = Writer::new(tag);
        w.u32(*id);
        match request {
            Request::Ping | Request::WhoAmI => {}
            Request::WithdrawContactRequest { root_key } => {
                w.array(root_key);
            }
            Request::ListPosts { since } => {
                w.u64(*since);
            }
            Request::Profile { have, from } => {
                w.u64(*have).u32(*from);
            }
            Request::JoinUser { name } => {
                w.str(name);
            }
            Request::Message { id, sent_at, lifetime, text } => {
                w.array(id).u64(*sent_at).str(text).u64(*lifetime);
            }
            Request::Unsay { id, at, digest } => {
                w.array(&id.unwrap_or_default()).u64(*at).array(digest);
            }
            Request::PassOn { contact, id, sent_at, lifetime, text } => {
                w.array(contact).array(id).u64(*sent_at).str(text).u64(*lifetime);
            }
            Request::FileChunk { id, offset, len } => {
                w.array(id).u64(*offset).u32(*len);
            }
            Request::FileChunkThrough { contact, id, offset, len } => {
                w.array(contact).array(id).u64(*offset).u32(*len);
            }
            Request::ProfileThrough { contact, have, from } => {
                w.array(contact).u64(*have).u32(*from);
            }
            Request::ChannelCatchUp { channel, from, digests } => {
                write_channel_id(&mut w, channel);
                w.u64(*from);
                write_list(&mut w, digests, write_digest);
            }
        }
        w.into_bytes()
}

/// A reply's bytes, to request `id`.
pub(super) fn encode_reply(id: &RequestId, reply: &Reply) -> Vec<u8> {
        let tag = match reply {
            Reply::Pong => TAG_PONG,
            Reply::Ack => TAG_ACK,
            Reply::Posts { .. } => TAG_POSTS,
            Reply::ChannelData { .. } => TAG_CHANNEL_DATA,
            Reply::ChannelError(_) => TAG_CHANNEL_ERROR,
            Reply::DisplayName(_) => TAG_DISPLAY_NAME,
            Reply::Unsupported => TAG_UNSUPPORTED,
            Reply::FileData(_) => TAG_FILE_DATA,
            Reply::FileError(_) => TAG_FILE_ERROR,
            Reply::Profile(_) => TAG_PROFILE,
            Reply::Refused(_) => TAG_REFUSED,
            Reply::UserKey { .. } => TAG_USER_KEY,
        };
        let mut w = Writer::new(tag);
        w.u32(*id);
        match reply {
            Reply::Pong | Reply::Ack | Reply::Unsupported => {}
            Reply::Posts { more, posts, deleted } => {
                w.bool(*more);
                write_list(&mut w, posts, write_post);
                write_list(&mut w, deleted, write_deletion);
            }
            Reply::ChannelData { more, roster, posts, names } => {
                w.bool(*more);
                write_list(&mut w, roster, write_roster_event);
                write_list(&mut w, posts, write_channel_post);
                write_list(&mut w, names, write_member_name);
            }
            Reply::ChannelError(message) => {
                w.str(message);
            }
            Reply::DisplayName(name) => write_display_name(&mut w, name),
            Reply::FileData(data) => {
                w.bytes(data);
            }
            Reply::FileError(why) | Reply::Refused(why) => {
                w.str(why);
            }
            Reply::UserKey { seed, list } => {
                w.array(seed).str(list);
            }
            Reply::Profile(None) => {
                w.bool(false);
            }
            Reply::Profile(Some(p)) => {
                w.bool(true).u64(p.at).array(&p.signature).str(&p.status).u32(p.avatar_len).u32(p.from).bytes(&p.data);
            }
        }
        w.into_bytes()
}

/// A request with tag `tag`, if it's one this build knows.
pub(super) fn decode_request(tag: u8, r: &mut Reader) -> Result<Option<Frame>> {
    Ok(Some(match tag {
        TAG_PING | TAG_WITHDRAW_CONTACT_REQUEST | TAG_MESSAGE | TAG_LIST_POSTS | TAG_WHO_AM_I | TAG_FILE_CHUNK | TAG_CHANNEL_CATCH_UP | TAG_PROFILE_ASK | TAG_JOIN_USER | TAG_PASS_ON | TAG_FILE_CHUNK_THROUGH | TAG_PROFILE_THROUGH | TAG_UNSAY => {
            let id = r.u32()?;
            let request = match tag {
                TAG_PING => Request::Ping,
                TAG_MESSAGE => {
                    let message_id = r.array()?;
                    let sent_at = r.u64()?;
                    let text = r.str()?;
                    if text.len() > MAX_MESSAGE_BYTES {
                        bail!("message longer than {MAX_MESSAGE_BYTES} bytes");
                    }
                    let lifetime = r.u64()?;
                    Request::Message { id: message_id, sent_at, lifetime, text: crate::text::single_line(&text) }
                }
                TAG_PASS_ON => {
                    let (contact, message_id, sent_at, text) = (r.array()?, r.array()?, r.u64()?, r.str()?);
                    if text.len() > MAX_MESSAGE_BYTES {
                        bail!("message longer than {MAX_MESSAGE_BYTES} bytes");
                    }
                    Request::PassOn { contact, id: message_id, sent_at, lifetime: r.u64()?, text: crate::text::single_line(&text) }
                }
                TAG_LIST_POSTS => Request::ListPosts { since: r.u64()? },
                TAG_PROFILE_ASK => Request::Profile { have: r.u64()?, from: r.u32()? },
                TAG_JOIN_USER => Request::JoinUser { name: crate::text::single_line(&r.str()?) },
                TAG_WITHDRAW_CONTACT_REQUEST => Request::WithdrawContactRequest { root_key: r.array()? },
                TAG_UNSAY => {
                    let message_id: MessageId = r.array()?;
                    Request::Unsay { id: (message_id != MessageId::default()).then_some(message_id), at: r.u64()?, digest: r.array()? }
                }
                TAG_FILE_CHUNK => Request::FileChunk { id: r.array()?, offset: r.u64()?, len: r.u32()? },
                TAG_FILE_CHUNK_THROUGH => Request::FileChunkThrough { contact: r.array()?, id: r.array()?, offset: r.u64()?, len: r.u32()? },
                TAG_PROFILE_THROUGH => Request::ProfileThrough { contact: r.array()?, have: r.u64()?, from: r.u32()? },
                TAG_CHANNEL_CATCH_UP => {
                    let channel = read_channel_id(r)?;
                    let from = r.u64()?;
                    let digests = read_list(r, read_digest)?;
                    if digests.len() > crate::channel::MAX_DIGESTS {
                        bail!("catch-up with too many digests");
                    }
                    Request::ChannelCatchUp { channel, from, digests }
                }
                _ => Request::WhoAmI,
            };
            Frame::Request(id, request)
        }
        _ => return Ok(None),
    }))
}

/// A reply with tag `tag`, if it's one this build knows.
pub(super) fn decode_reply(tag: u8, r: &mut Reader) -> Result<Option<Frame>> {
    Ok(Some(match tag {
        TAG_PONG | TAG_ACK | TAG_POSTS | TAG_CHANNEL_DATA | TAG_CHANNEL_ERROR | TAG_DISPLAY_NAME | TAG_UNSUPPORTED | TAG_FILE_DATA | TAG_FILE_ERROR | TAG_PROFILE | TAG_REFUSED | TAG_USER_KEY => {
            let id = r.u32()?;
            let reply = match tag {
                TAG_PONG => Reply::Pong,
                TAG_ACK => Reply::Ack,
                TAG_POSTS => Reply::Posts { more: r.bool()?, posts: read_list(r, read_post)?, deleted: read_list(r, read_deletion)? },
                TAG_CHANNEL_DATA => Reply::ChannelData {
                    more: r.bool()?,
                    roster: read_list(r, read_roster_event)?,
                    posts: read_list(r, read_channel_post)?,
                    names: read_list(r, read_member_name)?,
                },
                TAG_CHANNEL_ERROR => Reply::ChannelError(crate::text::single_line(&r.str()?)),
                TAG_DISPLAY_NAME => Reply::DisplayName(read_display_name(r)?),
                TAG_FILE_DATA => Reply::FileData(r.bytes()?.to_vec()),
                TAG_FILE_ERROR => Reply::FileError(crate::text::single_line(&r.str()?)),
                TAG_REFUSED => Reply::Refused(crate::text::single_line(&r.str()?)),
                TAG_USER_KEY => Reply::UserKey { seed: r.array()?, list: r.str()? },
                TAG_PROFILE => Reply::Profile(match r.bool()? {
                    true => Some(crate::profile::ProfilePart {
                        at: r.u64()?,
                        signature: r.array()?,
                        status: r.str()?,
                        avatar_len: r.u32()?,
                        from: r.u32()?,
                        data: r.bytes()?.to_vec(),
                    }),
                    false => None,
                }),
                _ => Reply::Unsupported,
            };
            Frame::Reply(id, reply)
        }
        _ => return Ok(None),
    }))
}
