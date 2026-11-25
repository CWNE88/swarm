//! Following someone's public posts without being their peer (see
//! `following`): fetched over a short connection of its own, which tells
//! them it's a follower's (`Notice::Follow`) — counted on their side
//! (`followers`), never a contact request.

use super::{newest_cached, posts_result, Ctx, NodeHandle, PostsResult};
use crate::following::{self, Followed};
use crate::protocol;
use anyhow::{bail, Result};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// How often everyone followed is fetched again while the node runs.
pub const REFRESH_EVERY: Duration = Duration::from_secs(15 * 60);
/// The first round, a little after starting.
const FIRST_REFRESH_AFTER: Duration = Duration::from_secs(20);

impl NodeHandle {
    /// Whose public posts this device follows.
    pub fn following(&self) -> Vec<Followed> {
        following::load(&self.ctx.paths.following()).unwrap_or_default()
    }

    /// Follows `public_key` at `address` and fetches their posts in the
    /// background. Not yourself, your own devices, or a peer (whose posts
    /// you see anyway).
    pub fn follow(&self, public_key: [u8; 32], address: &str) -> Result<()> {
        if public_key == self.ctx.identity.public_key_bytes() || self.devices().devices.iter().any(|d| d.key == public_key) {
            bail!("that's you");
        }
        if let Some(peer) = crate::peers::load(&self.ctx.paths.peers()).unwrap_or_default().into_iter().find(|p| p.public_key == public_key) {
            bail!("that's your contact '{}': you see their posts already", peer.name);
        }
        following::add(&self.ctx.paths.following(), public_key, address)?;
        let ctx = self.ctx.clone();
        let followed = Followed { public_key, address: address.to_string() };
        tokio::spawn(async move {
            let _ = fetch(&ctx, &followed).await;
        });
        Ok(())
    }

    /// Stops following `public_key`, and tells them if they can be
    /// reached (otherwise they stop counting us after a while).
    pub fn unfollow(&self, public_key: &[u8; 32]) -> Result<bool> {
        let Some(followed) = self.following().into_iter().find(|f| &f.public_key == public_key) else { return Ok(false) };
        following::remove(&self.ctx.paths.following(), public_key)?;
        if !crate::peers::load(&self.ctx.paths.peers()).unwrap_or_default().iter().any(|p| &p.public_key == public_key) {
            let _ = crate::postcache::forget(&self.ctx.paths.postcache(), public_key);
        }
        let ctx = self.ctx.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(protocol::REPLY_TIMEOUT * 3, tell(&ctx, &followed, false)).await;
        });
        Ok(true)
    }

    /// Fetches the public posts of someone followed: what's new merged
    /// into the cache, and the whole cache back (or the cache and why,
    /// if they can't be reached).
    pub async fn fetch_followed(&self, public_key: &[u8; 32]) -> Result<PostsResult> {
        let Some(followed) = self.following().into_iter().find(|f| &f.public_key == public_key) else { bail!("not following them") };
        fetch(&self.ctx, &followed).await
    }

    /// How many follow this device's public posts (see `followers`):
    /// heard from in the last 30 days, peers left out.
    pub fn followers(&self) -> usize {
        let peers: Vec<[u8; 32]> = crate::peers::load(&self.ctx.paths.peers()).unwrap_or_default().iter().map(|p| p.public_key).collect();
        crate::followers::count(&self.ctx.paths.followers(), super::unix_now(), &peers)
    }
}

/// Connects to `followed` and says whether we follow them. Kept open
/// for more if we do.
async fn tell(ctx: &Ctx, followed: &Followed, following: bool) -> Result<crate::transport::AuthenticatedConnection> {
    let resolved = crate::peers::resolve_address(&followed.address).await?;
    let mut conn = super::peer::dial_any(&ctx.endpoint, &resolved, ctx.keys(), followed.public_key).await?;
    protocol::send_notice(&mut conn, protocol::Notice::Follow { root_key: ctx.identity.public_key_bytes(), following }).await?;
    if !following {
        protocol::flush(&mut conn).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        conn.connection.close(0u32.into(), b"");
    }
    Ok(conn)
}

async fn fetch(ctx: &Ctx, followed: &Followed) -> Result<PostsResult> {
    let key = followed.public_key;
    let since = newest_cached(&ctx.paths, &key);
    let attempt = async {
        let mut conn = tell(ctx, followed, true).await?;
        let posts = protocol::list_posts_and_wait(&mut conn, since).await?;
        if let Ok(Some(claimed)) = protocol::who_am_i_and_wait(&mut conn).await {
            let _ = crate::displayname::cache_one(&ctx.paths.displaynames(), key, &claimed);
        }
        conn.connection.close(0u32.into(), b"");
        Ok(posts)
    };
    let fetched = tokio::time::timeout(protocol::REPLY_TIMEOUT * 3, attempt).await.unwrap_or_else(|_| Err(anyhow::anyhow!("no answer")));
    let fetched = fetched.map_err(|e: anyhow::Error| anyhow::anyhow!("couldn't reach them: {e:#}"));
    let name = crate::displayname::cached_one(&ctx.paths.displaynames(), &key).ok().flatten().map(|d| d.name).unwrap_or_default();
    let following = ctx.paths.following();
    let public = fetched.map(|f| crate::posts::Fetched { posts: f.posts.into_iter().filter(|p| p.visibility == crate::posts::Visibility::Public).collect(), deleted: f.deleted });
    posts_result(&ctx.paths, &name, &key, &[key], public, &|_| {}, || following::is_followed(&following, &key))
}

/// Fetches everyone followed every `REFRESH_EVERY`, for as long as the
/// node runs.
pub(super) async fn keep_following(ctx: Arc<Ctx>) {
    tokio::time::sleep(FIRST_REFRESH_AFTER).await;
    while !ctx.stopped.load(Ordering::Relaxed) {
        for followed in following::load(&ctx.paths.following()).unwrap_or_default() {
            let _ = fetch(&ctx, &followed).await;
        }
        tokio::time::sleep(REFRESH_EVERY).await;
    }
}
