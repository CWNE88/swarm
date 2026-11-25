//! Sending and receiving frames on a connection: sealed on the stream
//! or queued for cover, new keys every so often, the `Hello` both sides
//! open with, and the requests, replies and notices that come in.

use super::*;

/// Sends one frame — immediately, wire-visible the instant it's called,
/// unless `conn.cover_out` is configured (see `enable_cover`), in which
/// case this just queues the frame's bytes and returns right away: the
/// actual wire write happens later, on `emit_cover_slot`'s own fixed
/// schedule, same as every other frame sharing that connection's cover
/// queue. Every frame is bounded by `framing::MAX_FRAME` either way.
pub(super) async fn send_frame(conn: &mut AuthenticatedConnection, frame: Frame) -> Result<()> {
    let lane = frame.cover_lane();
    send_encoded(conn, lane, frame.encode()).await
}

impl Frame {
    /// Which cover stream this frame rides when cover is on: messages and
    /// the replies that can run to many parts are bulk, everything else
    /// — call signals, pings, small requests and replies — urgent, so it
    /// never waits behind them.
    fn cover_lane(&self) -> crate::cover::Lane {
        match self {
            Frame::Request(_, Request::Message { .. } | Request::PassOn { .. })
            | Frame::Reply(_, Reply::Posts { .. } | Reply::ChannelData { .. } | Reply::Profile(Some(_)))
            | Frame::Notice(Notice::ChannelPost { .. } | Notice::ChannelEvent { .. }) => crate::cover::Lane::Bulk,
            Frame::Reply(_, Reply::FileData(_)) => crate::cover::Lane::Background,
            _ => crate::cover::Lane::Urgent,
        }
    }
}

/// How long a connection's keys last: then each side, for what it sends,
/// moves the stream on to Noise's next key and picks a new datagram key
/// (`Frame::Rekey`), so a connection up for days doesn't keep one key
/// throughout.
pub const REKEY_EVERY: Duration = Duration::from_secs(60 * 60);

/// The most a connection's cover streams hold, waiting to go: several
/// times what any honest asking keeps in flight (a file's most chunks
/// ahead is 8 MB, `transfer::MAX_WINDOW`).
pub const MAX_COVER_QUEUE: usize = 32 * 1024 * 1024;

/// How long after telling the other side its new datagram key this side
/// starts sealing with it: time for the telling to arrive first.
const NEW_DATAGRAM_KEY_AFTER: Duration = Duration::from_secs(30);

pub(super) async fn send_encoded(conn: &mut AuthenticatedConnection, lane: crate::cover::Lane, bytes: Vec<u8>) -> Result<()> {
    if conn.peer_version != 0 && conn.keys_since.elapsed() >= conn.rekey_every {
        conn.keys_since = Instant::now();
        conn.rekeys += 1;
        if let Some((_, waiting)) = conn.next_sealer.take() {
            conn.datagram_sealer = Some(waiting);
        }
        let datagram_key: [u8; 32] = rand::random();
        let on_the_stream = conn.cover_out.is_none();
        send_sealed(conn, crate::cover::Lane::Urgent, Frame::Rekey { datagram_key }.encode()).await?;
        conn.next_sealer_acked = conn.cover_out.as_ref().map_or(0, |cover| cover.urgent_end());
        if on_the_stream {
            conn.session.rekey_outgoing();
        }
        conn.next_sealer = Some((Instant::now() + NEW_DATAGRAM_KEY_AFTER, crate::datagram::Sealer::new(&datagram_key)));
    }
    send_sealed(conn, lane, bytes).await
}

async fn send_sealed(conn: &mut AuthenticatedConnection, lane: crate::cover::Lane, bytes: Vec<u8>) -> Result<()> {
    if bytes.len() + 16 > framing::MAX_FRAME {
        bail!("a {} byte frame is over the {} byte limit", bytes.len(), framing::MAX_FRAME);
    }
    let tag = bytes.first().copied().unwrap_or(0);
    match &mut conn.cover_out {
        Some(cover) if cover.queued() > MAX_COVER_QUEUE => bail!("over {MAX_COVER_QUEUE} bytes waiting for cover: the other side asks for more than it carries"),
        Some(cover) => {
            let mut queued = Vec::with_capacity(2 + bytes.len());
            queued.extend((bytes.len() as u16).to_be_bytes());
            queued.extend(bytes);
            crate::tally::sent(tag, queued.len());
            cover.queue_bytes(lane, &queued);
            Ok(())
        }
        None => {
            let sealed = conn.session.seal(&bytes)?;
            conn.written += 2 + sealed.len() as u64;
            crate::tally::sent(tag, 2 + sealed.len());
            let started = Instant::now();
            let written = framing::write_frame(&mut conn.send, &sealed).await;
            conn.write_wait += started.elapsed();
            written
        }
    }
}

/// Reads one sealed frame off the stream and opens it, refreshing
/// `conn.last_heard`.
async fn read_opened(conn: &mut AuthenticatedConnection) -> Result<Vec<u8>> {
    let sealed = conn.frame_reader.read(&mut conn.recv).await?;
    let opened = conn.session.open(&sealed)?;
    conn.last_heard = Instant::now();
    crate::tally::received(opened.first().copied().unwrap_or(0), 2 + sealed.len());
    Ok(opened)
}

/// Something read off the connection: a sealed stream frame, or a
/// datagram (a cover slot).
enum Arrival {
    Frame(Vec<u8>),
    Datagram(bytes::Bytes),
}

/// Waits for the next stream frame or datagram. Cancel-safe: both reads
/// are (see `framing::FrameReader`; quinn keeps undelivered datagrams),
/// and nothing after them awaits — which is why nothing in `recv` ever
/// writes: anything it needs to send goes into `conn.pending_out` for
/// `flush`. `PeerTask` races `recv` against
/// its cover tick, heartbeat and sends, so a write cut off part-way
/// would corrupt the stream.
async fn next_arrival(conn: &mut AuthenticatedConnection) -> Result<Arrival> {
    let AuthenticatedConnection { frame_reader, recv, connection, .. } = conn;
    tokio::select! {
        sealed = frame_reader.read(recv) => Ok(Arrival::Frame(sealed?)),
        datagram = connection.read_datagram() => Ok(Arrival::Datagram(datagram?)),
    }
}

/// Reads one frame, taking cover slots (datagrams) in along the way —
/// whether or not *this* side's own cover is on: the peer's outbound
/// shape is its own choice. Everything read refreshes `conn.last_heard`.
async fn recv_frame(conn: &mut AuthenticatedConnection) -> Result<Frame> {
    loop {
        while let Some(bytes) = conn.voice_ready.pop_front() {
            crate::tally::received(bytes.first().copied().unwrap_or(0), bytes.len());
            if let Ok(frame @ Frame::Notice(Notice::Call(CallSignal::Audio(_)))) = Frame::decode(&bytes) {
                return Ok(frame);
            }
        }
        if let Some(bytes) = conn.cover_in.pop_ready() {
            crate::tally::received(bytes.first().copied().unwrap_or(0), bytes.len());
            match Frame::decode(&bytes)? {
                Frame::Rekey { datagram_key } => {
                    if let Some(opener) = &mut conn.datagram_opener {
                        opener.rotate(&datagram_key);
                    }
                    continue;
                }
                frame => return Ok(frame),
            }
        }
        match next_arrival(conn).await? {
            Arrival::Frame(sealed) => {
                let opened = conn.session.open(&sealed)?;
                conn.last_heard = Instant::now();
                crate::tally::received(opened.first().copied().unwrap_or(0), 2 + sealed.len());
                match Frame::decode(&opened)? {
                    Frame::Rekey { datagram_key } => {
                        conn.session.rekey_incoming();
                        if let Some(opener) = &mut conn.datagram_opener {
                            opener.rotate(&datagram_key);
                        }
                        continue;
                    }
                    frame => return Ok(frame),
                }
            }
            Arrival::Datagram(datagram) => {
                crate::tally::slot_received(datagram.len());
                let Some(opener) = &mut conn.datagram_opener else { continue };
                let Ok(body) = opener.open(&datagram) else { continue };
                conn.last_heard = Instant::now();
                if body.first() == Some(&super::calls::VOICE_DATAGRAM) {
                    super::calls::take_voice_datagram(conn, &body);
                    continue;
                }
                let acks = conn.cover_in.feed(&body)?;
                match &mut conn.cover_out {
                    Some(cover) => cover.on_ack(acks),
                    None => {
                        if conn.cover_in.take_ack_due() {
                            let (urgent, bulk, background) = conn.cover_in.received();
                            conn.pending_out.push_back(Frame::CoverAck(urgent, bulk, background).encode());
                        }
                    }
                }
            }
        }
    }
}

/// Sends whatever `recv` set aside to send (see `next_arrival`): call
/// after every `recv`, outside any `select!`.
pub async fn flush(conn: &mut AuthenticatedConnection) -> Result<()> {
    while let Some(bytes) = conn.pending_out.pop_front() {
        send_encoded(conn, crate::cover::Lane::Urgent, bytes).await?;
    }
    Ok(())
}

/// Both sides' first frame on every connection: exchanges `Hello`s and
/// fails with `IncompatibleVersion` if the two can't talk. Run by
/// `transport::dial` and `transport::accept_incoming`, so every
/// connection either side has is already known compatible.
pub async fn greet(conn: &mut AuthenticatedConnection, keys: crate::transport::Keys<'_>) -> Result<()> {
    let datagram_key: [u8; 32] = rand::random();
    conn.datagram_sealer = Some(crate::datagram::Sealer::new(&datagram_key));
    let seen_as = crate::transport::canonical(conn.connection.remote_address()).to_string();
    let me = DeviceProof::mine(keys, &conn.session.hash);
    send_frame(conn, Frame::Hello { version: PROTOCOL_VERSION, min_version: MIN_PROTOCOL_VERSION, datagram_key, seen_as, me }).await?;
    let opened = read_opened(conn).await?;
    match Frame::decode(&opened) {
        Ok(Frame::Hello { version, min_version, datagram_key, seen_as, me }) => {
            if version < MIN_PROTOCOL_VERSION || min_version > PROTOCOL_VERSION {
                return Err(IncompatibleVersion { theirs: Some(version) }.into());
            }
            if let Some(why) = me.fails(&conn.remote_public_key, &conn.session.hash) {
                return Err(DeviceUnproved(why).into());
            }
            conn.remote_user = me.user;
            conn.remote_device = me.device;
            conn.peer_version = version;
            conn.seen_as = seen_as.parse().ok();
            conn.datagram_opener = Some(crate::datagram::Opener::new(&datagram_key));
            Ok(())
        }
        _ => Err(IncompatibleVersion { theirs: None }.into()),
    }
}

/// One frame off the wire: a request to answer (with `reply`, echoing
/// its id), a reply to one of ours (matched by id), or a notice.
#[derive(Debug)]
pub enum Incoming {
    Request(RequestId, Request),
    Reply(RequestId, Reply),
    Notice(Notice),
}

/// Confirms `claimed_root_key` (a requester's self-reported *root*
/// Ed25519 identity) actually corresponds to `conn.remote_public_key`,
/// the X25519 identity the Noise handshake already proved, by running
/// the Ed25519-to-X25519 conversion (always well-defined) and comparing.
/// `None` means the claim doesn't match — a lie, or a stale key — and
/// the request should be refused, not treated as a protocol error.
pub fn verified_requester(conn: &AuthenticatedConnection, claimed_root_key: [u8; 32]) -> Option<[u8; 32]> {
    let claimed_x25519 = crate::identity::ed25519_public_key_to_x25519(&claimed_root_key).ok()?;
    (claimed_x25519 == conn.remote_public_key).then_some(claimed_root_key)
}

/// Reads the next request, reply or notice. Cancel-safe, and never
/// writes (see `next_arrival`) — call `flush` after it. A request of a
/// type this build doesn't know is answered `Unsupported` (by that
/// `flush`) and skipped, as is an unknown reply or notice.
pub async fn recv(conn: &mut AuthenticatedConnection) -> Result<Incoming> {
    loop {
        match recv_frame(conn).await? {
            Frame::Request(id, request) => return Ok(Incoming::Request(id, request)),
            Frame::Reply(id, reply) => return Ok(Incoming::Reply(id, reply)),
            Frame::Notice(notice) => return Ok(Incoming::Notice(notice)),
            Frame::UnknownRequest(id) => conn.pending_out.push_back(Frame::Reply(id, Reply::Unsupported).encode()),
            Frame::CoverAck(urgent, bulk, background) => {
                if let Some(cover) = &mut conn.cover_out {
                    cover.on_ack((urgent, bulk, background));
                }
            }
            Frame::Hello { .. } | Frame::Rekey { .. } | Frame::Unknown => {}
        }
    }
}

/// Sends `request` under a fresh id, returned so the caller can match
/// the reply.
pub async fn request(conn: &mut AuthenticatedConnection, request: Request) -> Result<RequestId> {
    conn.next_request_id = conn.next_request_id.wrapping_add(1);
    let id = conn.next_request_id;
    send_frame(conn, Frame::Request(id, request)).await?;
    Ok(id)
}

/// Answers request `id`. For replies that can be large, use
/// `respond_posts`/`respond_channel_data`, which split them.
pub async fn reply(conn: &mut AuthenticatedConnection, id: RequestId, reply: Reply) -> Result<()> {
    send_frame(conn, Frame::Reply(id, reply)).await
}

pub async fn send_notice(conn: &mut AuthenticatedConnection, notice: Notice) -> Result<()> {
    send_frame(conn, Frame::Notice(notice)).await
}
