//! Replies split into parts, and one-shot requests on a connection
//! made for them alone.

use super::*;

/// Splits `items` into runs whose encoded size stays within
/// `REPLY_PART_BUDGET` — at least one item per run, and always at least
/// one (possibly empty) run.
pub(super) fn split_by_size<T>(items: Vec<T>, size: impl Fn(&T) -> usize) -> Vec<Vec<T>> {
    let mut parts = vec![Vec::new()];
    let mut used = 0;
    for item in items {
        let n = size(&item);
        if used + n > REPLY_PART_BUDGET && !parts.last().expect("never empty").is_empty() {
            parts.push(Vec::new());
            used = 0;
        }
        used += n;
        parts.last_mut().expect("never empty").push(item);
    }
    parts
}

pub(super) fn encoded_size<T>(item: &T, write: fn(&mut Writer, &T)) -> usize {
    let mut w = Writer::default();
    write(&mut w, item);
    w.len()
}

/// What a device gives another of the user's (see `bundle`), as notices of about
/// 4000 bytes each.
pub fn bundle_notices(bytes: &[u8]) -> Vec<Notice> {
    let mut chunks: Vec<&[u8]> = bytes.chunks(REPLY_PART_BUDGET).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let last = chunks.len() - 1;
    chunks.into_iter().enumerate().map(|(i, chunk)| Notice::Bundle { more: i < last, bytes: chunk.to_vec() }).collect()
}

/// A user's device list entries, as notices of a fitting size.
pub fn device_list_notices(entries: Vec<crate::devicelist::Entry>) -> Vec<Notice> {
    split_by_size(entries, |e| encoded_size(e, write_list_entry)).into_iter().map(|entries| Notice::DeviceList { entries }).collect()
}

/// Answers a `ListPosts` with `posts`, in as many parts as it takes.
pub async fn respond_posts(conn: &mut AuthenticatedConnection, id: RequestId, fetched: Fetched) -> Result<()> {
    let mut parts: Vec<(Vec<Post>, Vec<Deletion>)> = split_by_size(fetched.posts, |p| encoded_size(p, write_post)).into_iter().map(|p| (p, Vec::new())).collect();
    if !fetched.deleted.is_empty() {
        parts.extend(split_by_size(fetched.deleted, |d| encoded_size(d, write_deletion)).into_iter().map(|d| (Vec::new(), d)));
    }
    let last = parts.len() - 1;
    for (i, (posts, deleted)) in parts.into_iter().enumerate() {
        reply(conn, id, Reply::Posts { more: i < last, posts, deleted }).await?;
    }
    Ok(())
}

/// Answers `NotPeer`: tells someone who hasn't added us who we are, so
/// they can (see `requests`). Fire-and-forget — nothing comes back.
pub async fn send_introduce(conn: &mut AuthenticatedConnection, me: &Introduction) -> Result<()> {
    send_notice(conn, Notice::Introduce { root_key: me.root_key, name: me.name.clone() }).await
}

/// Waits (up to `REPLY_TIMEOUT` between frames) for the reply to the
/// one-shot request `id`, answering or skipping anything unsolicited
/// that arrives meanwhile. A one-shot dial to a peer who has us on
/// their list gets adopted by their own connection task, which opens
/// with a `CoverHello` and its own requests. Unsolicited requests that
/// need state this caller doesn't have (a message, a posts or channel
/// ask) are left unanswered: their sender gives up on them and tries
/// again later, nothing is lost. `on_not_peer` is told when the other
/// side says we're not on their list.
pub(super) async fn await_reply(conn: &mut AuthenticatedConnection, id: RequestId, mut on_not_peer: impl FnMut()) -> Result<Reply> {
    let wait = async {
        loop {
            let incoming = recv(conn).await?;
            flush(conn).await?;
            match incoming {
                Incoming::Reply(reply_id, reply) if reply_id == id => return Ok(reply),
                Incoming::Request(their_id, Request::Ping) => send_frame(conn, Frame::Reply(their_id, Reply::Pong)).await?,
                Incoming::Request(their_id, Request::WhoAmI) => send_frame(conn, Frame::Reply(their_id, Reply::DisplayName(None))).await?,
                Incoming::Notice(Notice::Call(ring)) if ring.is_ring() => send_call(conn, CallSignal::Reject).await?,
                Incoming::Notice(Notice::NotPeer) => on_not_peer(),
                _ => {}
            }
        }
    };
    tokio::time::timeout(REPLY_TIMEOUT, wait).await.map_err(|_| anyhow::anyhow!("no reply within {REPLY_TIMEOUT:?}"))?
}

/// One-shot ping on a connection that's only ever going to be used for
/// this (e.g. `swarm ping`'s standalone dial). Not used by `swarm-cli`'s
/// per-peer connection task, which drives `recv` itself.
pub async fn ping(conn: &mut AuthenticatedConnection) -> Result<Duration> {
    let sent_at = Instant::now();
    let id = request(conn, Request::Ping).await?;
    match await_reply(conn, id, || {}).await? {
        Reply::Pong => Ok(sent_at.elapsed()),
        _ => bail!("expected a Pong reply"),
    }
}

/// Asks to be added to the user at the other end, as a device called
/// `name` (see `node::joining`): `UserKey`, or why not.
pub async fn join_user_and_wait(conn: &mut AuthenticatedConnection, name: &str) -> Result<Reply> {
    let id = request(conn, Request::JoinUser { name: name.to_string() }).await?;
    await_reply(conn, id, || {}).await
}

/// One-shot message send, see `ping` — except that it first waits for
/// the other side's opening frame, so a message never leaves this side
/// for someone who hasn't added us: a peer's connection opens with
/// `CoverHello`, anyone else's with `NotPeer`; neither in time, and it
/// fails, the message unsent. On `NotPeer` this
/// introduces us (`me`) and fails with `NotAPeer`, message unsent.
pub async fn send_message_and_wait(
    conn: &mut AuthenticatedConnection,
    message_id: MessageId,
    sent_at: u64,
    lifetime: u64,
    text: &str,
    me: &Introduction,
) -> Result<()> {
    let opening = async {
        loop {
            let incoming = recv(conn).await?;
            flush(conn).await?;
            match incoming {
                Incoming::Notice(Notice::NotPeer) => return Ok::<bool, anyhow::Error>(false),
                Incoming::Notice(Notice::CoverHello { .. }) => return Ok(true),
                Incoming::Request(their_id, Request::Ping) => send_frame(conn, Frame::Reply(their_id, Reply::Pong)).await?,
                Incoming::Request(their_id, Request::WhoAmI) => send_frame(conn, Frame::Reply(their_id, Reply::DisplayName(None))).await?,
                Incoming::Notice(Notice::Call(ring)) if ring.is_ring() => send_call(conn, CallSignal::Reject).await?,
                _ => {}
            }
        }
    };
    let is_peer = tokio::time::timeout(REPLY_TIMEOUT, opening).await.map_err(|_| anyhow::anyhow!("they didn't say hello within {REPLY_TIMEOUT:?}"))??;
    if !is_peer {
        send_introduce(conn, me).await?;
        let _ = tokio::time::timeout(Duration::from_millis(300), recv(conn)).await;
        return Err(NotAPeer.into());
    }
    let id = request(conn, Request::Message { id: message_id, sent_at, lifetime, text: text.to_string() }).await?;
    let mut refused = false;
    let outcome = await_reply(conn, id, || refused = true).await;
    match outcome {
        Ok(Reply::Ack) => Ok(()),
        _ if refused => Err(NotAPeer.into()),
        Ok(_) => bail!("expected an Ack reply"),
        Err(e) => Err(e),
    }
}

/// The most posts one fetch takes (about 10 MB at most): whoever answers
/// could otherwise send parts for ever. What's left comes next time,
/// asked for from the newest of these.
pub const MAX_POSTS_FETCHED: usize = 5000;

/// One-shot posts fetch, see `ping`: posts created at or after
/// `since`, oldest first. Collects every part, up to
/// `MAX_POSTS_FETCHED`.
pub async fn list_posts_and_wait(conn: &mut AuthenticatedConnection, since: u64) -> Result<Fetched> {
    let id = request(conn, Request::ListPosts { since }).await?;
    let mut all = Fetched::default();
    loop {
        match await_reply(conn, id, || {}).await? {
            Reply::Posts { more, posts, deleted } => {
                all.posts.extend(posts);
                all.deleted.extend(deleted);
                all.deleted.truncate(crate::posts::MAX_DELETIONS);
                if all.posts.len() >= MAX_POSTS_FETCHED {
                    all.posts.truncate(MAX_POSTS_FETCHED);
                    return Ok(all);
                }
                if !more {
                    return Ok(all);
                }
            }
            _ => bail!("expected a Posts reply"),
        }
    }
}

/// One-shot display name ask, see `ping`.
pub async fn who_am_i_and_wait(conn: &mut AuthenticatedConnection) -> Result<Option<DisplayName>> {
    let id = request(conn, Request::WhoAmI).await?;
    match await_reply(conn, id, || {}).await? {
        Reply::DisplayName(claimed) => Ok(claimed),
        _ => bail!("expected a display-name reply"),
    }
}
