//! The user's name and picture, the same on every one of their devices
//!.
//!
//! Each device publishes them as its display name and profile, signed by
//! the user key, as any peer does. The user's devices ask each other for
//! theirs over their links (see `devices`), as peers do, and are told
//! when they change. A device that has another's newer than its own, and
//! different, takes it on, signed afresh (`adopt`), and its own peers are
//! told in turn. Two changed at once, the later wins.

use super::Ctx;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// The user's name and picture as their other devices have them, taken
/// on where one's newer than this device's own and different.
pub(super) fn adopt(ctx: &Arc<Ctx>) {
    if !super::devices::has_others(ctx) || ctx.stopped.load(Ordering::Relaxed) {
        return;
    }
    let others = [super::devices::me(ctx)];
    let mut taken = false;

    let names = crate::displayname::cached_all(&ctx.paths.displaynames()).unwrap_or_default();
    let mine = ctx.paths.my_display_name();
    if let Some(newest) = others.iter().filter_map(|k| names.get(k)).max_by_key(|d| d.at)
        && mine.as_ref().is_none_or(|m| newest.at > m.at && newest.name != m.name)
    {
        match crate::displayname::set(&ctx.paths.displayname(), &ctx.identity, &newest.name) {
            Ok(()) => {
                log!(ctx, "you're called {} now: another of your devices renamed you", newest.name);
                taken = true;
            }
            Err(e) => log!(ctx, "couldn't take on your new name: {e}"),
        }
    }

    let mine = crate::profile::current(&ctx.paths.profile());
    let newest = others.iter().filter_map(|k| crate::profile::cached(&ctx.paths.profiles(), k)).max_by_key(|p| p.at);
    if let Some(newest) = newest
        && mine.as_ref().is_none_or(|m| newest.at > m.at && (newest.status != m.status || newest.avatar != m.avatar))
    {
        match crate::profile::set(&ctx.paths.profile(), &ctx.identity, &newest.status, &newest.avatar) {
            Ok(_) => {
                log!(ctx, "your picture changed on another of your devices");
                taken = true;
            }
            Err(e) => log!(ctx, "couldn't take on your new picture: {e}"),
        }
    }

    if taken {
        told(ctx);
    }
}

/// The user's name or picture changed here: peers and the user's other
/// devices are told, and the front ends look again.
pub(super) fn told(ctx: &Ctx) {
    ctx.my_profile.send_modify(|n| *n = n.wrapping_add(1));
    ctx.message_updates.send_modify(|n| *n = n.wrapping_add(1));
    ctx.device_updates.send_modify(|n| *n = n.wrapping_add(1));
}

/// On a link to another of the user's devices: asking for its name and
/// picture, and telling it when ours change.
pub(super) struct Look {
    ctx: Arc<Ctx>,
    remote: [u8; 32],
    /// Counts up when the user's name or picture changes here.
    pub(super) mine: tokio::sync::watch::Receiver<u64>,
    name_asked: Option<crate::protocol::RequestId>,
    /// Asked for, from what's kept, and the picture so far.
    profile_asked: Option<(crate::protocol::RequestId, u64, Option<crate::profile::ProfilePart>)>,
}

impl Look {
    pub(super) fn new(ctx: &Arc<Ctx>, remote: [u8; 32]) -> Look {
        let mut mine = ctx.my_profile.subscribe();
        mine.borrow_and_update();
        Look { ctx: ctx.clone(), remote, mine, name_asked: None, profile_asked: None }
    }

    /// Asks for their name, and their picture if newer than the one kept.
    pub(super) async fn ask(&mut self, conn: &mut crate::transport::AuthenticatedConnection) -> anyhow::Result<()> {
        self.name_asked = Some(crate::protocol::request(conn, crate::protocol::Request::WhoAmI).await?);
        let have = crate::profile::cached(&self.ctx.paths.profiles(), &self.remote).map_or(0, |p| p.at);
        let id = crate::protocol::request(conn, crate::protocol::Request::Profile { have, from: 0 }).await?;
        self.profile_asked = Some((id, have, None));
        Ok(())
    }

    /// Ours changed: they're told, and ask.
    pub(super) async fn tell(&mut self, conn: &mut crate::transport::AuthenticatedConnection) -> anyhow::Result<()> {
        self.mine.borrow_and_update();
        crate::protocol::send_notice(conn, crate::protocol::Notice::ProfileChanged).await
    }

    /// Whether reply `id` is to one of these asks.
    pub(super) fn owns(&self, id: crate::protocol::RequestId) -> bool {
        self.name_asked == Some(id) || self.profile_asked.as_ref().is_some_and(|(asked, _, _)| *asked == id)
    }

    pub(super) async fn on_reply(&mut self, conn: &mut crate::transport::AuthenticatedConnection, id: crate::protocol::RequestId, reply: crate::protocol::Reply) -> anyhow::Result<()> {
        use crate::protocol::Reply;
        if self.name_asked == Some(id) {
            self.name_asked = None;
            if let Reply::DisplayName(Some(claimed)) = reply {
                let _ = crate::displayname::cache_one(&self.ctx.paths.displaynames(), self.remote, &claimed);
                adopt(&self.ctx);
            }
            return Ok(());
        }
        let Some((_, have, so_far)) = self.profile_asked.take() else { return Ok(()) };
        let Reply::Profile(Some(part)) = reply else { return Ok(()) };
        match crate::profile::gather(so_far, part) {
            crate::profile::Gathered::Whole(profile) => {
                if let Ok(true) = crate::profile::cache(&self.ctx.paths.profiles(), &self.remote, &profile) {
                    adopt(&self.ctx);
                }
            }
            crate::profile::Gathered::More(so_far) => {
                let from = so_far.data.len() as u32;
                let id = crate::protocol::request(conn, crate::protocol::Request::Profile { have, from }).await?;
                self.profile_asked = Some((id, have, Some(so_far)));
            }
            crate::profile::Gathered::Wrong => {}
        }
        Ok(())
    }
}

/// The answer to another of the user's devices asking for this one's picture: the part
/// from `from`, if it's newer than the one they have.
pub(super) fn answer(ctx: &Ctx, have: u64, from: u32) -> crate::protocol::Reply {
    let newer = crate::profile::current(&ctx.paths.profile()).filter(|p| p.at > have);
    crate::protocol::Reply::Profile(newer.map(|p| crate::profile::part(&p, from as usize)))
}
