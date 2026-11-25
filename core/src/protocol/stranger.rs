//! A connection from someone who isn't a contact.

use super::*;

/// Answers whatever requests arrive forever, never sending one of its
/// own — for a connection with no per-peer task to hand off to (an
/// accepted connection from someone not on the local peers list; see
/// `listen_forever`'s accept loop). Opens by telling them so
/// (`Notice::NotPeer`), and refuses their messages the same way: until
/// they're added as a peer, all they can do is read public posts and
/// introduce themselves — handed to `on_introduce` with a
/// root key already checked against the connection (the display name,
/// if any, still needs `displayname::verify`), once per connection;
/// true back, and they're told they've been removed (`Notice::Removed`).
/// A contact request they've made, they can cancel: `on_request_cancelled`,
/// with their key checked the same way.
pub async fn serve_connection<F1, F2, F4, F5, F6>(
    conn: &mut AuthenticatedConnection,
    mut on_introduce: F1,
    mut on_list_posts: F2,
    mut on_who_am_i: F4,
    mut on_follow: F5,
    mut on_request_cancelled: F6,
) -> Result<()>
where
    F1: FnMut([u8; 32], Option<DisplayName>) -> bool,
    F2: FnMut(u64) -> crate::posts::Fetched,
    F4: FnMut() -> Option<DisplayName>,
    F5: FnMut([u8; 32], bool),
    F6: FnMut([u8; 32]),
{
    send_notice(conn, Notice::NotPeer).await?;
    let mut introduced = false;
    let mut following_said: Option<bool> = None;
    let mut follows_left = 4;
    loop {
        let Ok(incoming) = recv(conn).await else { return Ok(()) };
        flush(conn).await?;
        match incoming {
            Incoming::Request(id, Request::Ping) => reply(conn, id, Reply::Pong).await?,
            Incoming::Request(id, Request::WithdrawContactRequest { root_key }) => match verified_requester(conn, root_key) {
                Some(root_key) => {
                    on_request_cancelled(root_key);
                    reply(conn, id, Reply::Ack).await?;
                }
                None => reply(conn, id, Reply::Refused("claimed identity doesn't match this connection".to_string())).await?,
            },
            Incoming::Request(_, Request::Message { .. }) => send_notice(conn, Notice::NotPeer).await?,
            Incoming::Request(id, Request::Unsay { .. }) => reply(conn, id, Reply::Ack).await?,
            Incoming::Request(id, Request::ListPosts { since }) => {
                let posts = on_list_posts(since);
                respond_posts(conn, id, posts).await?;
            }
            Incoming::Request(id, Request::WhoAmI) => {
                let claimed = on_who_am_i();
                reply(conn, id, Reply::DisplayName(claimed)).await?;
            }
            Incoming::Request(id, Request::FileChunk { .. }) => reply(conn, id, Reply::FileError("not a contact".to_string())).await?,
            Incoming::Request(id, Request::FileChunkThrough { .. }) => reply(conn, id, Reply::Refused("not a contact".to_string())).await?,
            Incoming::Request(id, Request::Profile { .. }) => reply(conn, id, Reply::Profile(None)).await?,
            Incoming::Request(id, Request::PassOn { .. } | Request::ProfileThrough { .. }) => reply(conn, id, Reply::Refused("not a contact".to_string())).await?,
            Incoming::Request(id, Request::JoinUser { .. }) => reply(conn, id, Reply::Refused("no device is being added".to_string())).await?,
            Incoming::Request(id, Request::ChannelCatchUp { .. }) => reply(conn, id, Reply::ChannelError(NO_SUCH_CHANNEL.to_string())).await?,
            Incoming::Notice(Notice::Introduce { root_key, name }) => {
                if let Some(root_key) = verified_requester(conn, root_key).filter(|_| !introduced) {
                    introduced = true;
                    if on_introduce(root_key, name) {
                        send_notice(conn, Notice::Removed).await?;
                    }
                }
            }
            Incoming::Notice(Notice::Follow { root_key, following }) => {
                if following_said != Some(following)
                    && follows_left > 0
                    && let Some(root_key) = verified_requester(conn, root_key) {
                        following_said = Some(following);
                        follows_left -= 1;
                        on_follow(root_key, following);
                    }
            }
            Incoming::Notice(Notice::Call(ring)) if ring.is_ring() => send_call(conn, CallSignal::Reject).await?,
            Incoming::Notice(_) | Incoming::Reply(..) => {}
        }
    }
}
