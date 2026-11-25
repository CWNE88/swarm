//! Peers added without a name (`settings::named_after_them`): their
//! entry takes the name they publish once it's known, and follows it
//! when they change it, until it's renamed by hand. (Your own name and
//! picture, changed on another of your devices, come by `ownlook`.)

use super::{Ctx, NodeHandle};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const CHECK_EVERY: Duration = Duration::from_secs(3);

/// Renames each such peer to their published name where it differs.
fn rename_to_published(ctx: &Arc<Ctx>) {
    let settings = ctx.paths.settings();
    let peers = crate::peers::load(&ctx.paths.peers()).unwrap_or_default();
    for peer in &peers {
        if !crate::settings::named_after_them(&settings, &peer.public_key) {
            continue;
        }
        let Some(published) = crate::displayname::cached_one(&ctx.paths.displaynames(), &peer.public_key).ok().flatten() else { continue };
        let name = crate::peers::label_from(&published.name, &peer.name);
        if name == peer.name {
            continue;
        }
        if crate::peers::update(&ctx.paths.peers(), &peer.handle(), &name, peer.address.clone(), peer.public_key).is_ok() {
            log!(ctx, "'{}' is '{name}' now: the name they go by", peer.name);
            let renamed = crate::peers::Peer { name: name.clone(), ..peer.clone() };
            NodeHandle { ctx: ctx.clone() }.update_peer_live(&peer.handle(), renamed);
        }
        return;
    }
}

pub(super) async fn keep_names(ctx: Arc<Ctx>) {
    while !ctx.stopped.load(Ordering::Relaxed) {
        rename_to_published(&ctx);
        super::ownlook::adopt(&ctx);
        tokio::time::sleep(CHECK_EVERY).await;
    }
}
