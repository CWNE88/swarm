//! The accept loop: every incoming connection goes to its peer's task,
//! or — for someone not on the peers list — to a stranger responder,
//! within limits.

use crate::lock::Locked;
use super::{unix_now, Ctx};
use crate::transport::{self, AuthenticatedConnection};
use crate::{displayname, posts, protocol, requests};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many incoming connections may be part-way through their
/// handshake at once.
const MAX_HANDSHAKES_IN_PROGRESS: usize = 64;
/// How many connections from people not on our peers list are served
/// at once, in all — and from any one /64 (one household or host).
/// Peers' connections don't count against either.
const MAX_STRANGER_CONNECTIONS: usize = 32;
const MAX_STRANGER_CONNECTIONS_PER_PREFIX: usize = 4;
/// The longest one stranger connection is served. Everything a stranger
/// can do (read posts, introduce themselves)
/// takes seconds; this just stops a slot being held for good.
const STRANGER_CONNECTION_LIFETIME: Duration = Duration::from_secs(300);

/// Accepts connections until `endpoint` closes. Each finishes its
/// handshake on a task of its own, so one that stalls part-way (or is
/// deliberately slow) can't hold up anyone else's — with a cap on how
/// many may be part-way at once, past which new ones are turned away.
pub(super) async fn accept_loop(ctx: Arc<Ctx>, endpoint: quinn::Endpoint) {
    let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_HANDSHAKES_IN_PROGRESS));
    let strangers = StrangerLimits::default();
    let incompatible_noted: Arc<Mutex<HashSet<IpAddr>>> = Arc::default();
    while let Some(incoming) = endpoint.accept().await {
        let Ok(permit) = handshakes.clone().try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        let ctx = ctx.clone();
        let strangers = strangers.clone();
        let incompatible_noted = incompatible_noted.clone();
        tokio::spawn(async move {
            let accepted = transport::accept_incoming(incoming, ctx.keys()).await;
            drop(permit);
            match accepted {
                Ok(conn) => route_accepted(&ctx, conn, &strangers),
                Err(err)
                    if err.reason.is::<protocol::IncompatibleVersion>()
                        && !err.remote.is_some_and(|r| incompatible_noted.locked().insert(r.ip())) => {}
                Err(err) => match err.remote.and_then(|remote| peer_name_at_address(&ctx, remote)) {
                    Some(name) => log!(ctx, "accept failed: {name} ({}): {:#}", err.remote.expect("matched above"), err.reason),
                    None => log!(ctx, "accept failed: {err}"),
                },
            }
        });
    }
    log!(ctx, "stopped accepting connections: the endpoint closed");
}

/// Tracks stranger connections against the caps above.
#[derive(Clone, Default)]
struct StrangerLimits {
    by_prefix: Arc<Mutex<HashMap<[u8; 8], usize>>>,
}

/// A stranger connection's place under `StrangerLimits`, given back when
/// it's dropped.
struct StrangerSlot {
    limits: StrangerLimits,
    prefix: [u8; 8],
}

impl StrangerLimits {
    fn admit(&self, remote: SocketAddr) -> Option<StrangerSlot> {
        let prefix: [u8; 8] = match remote.ip() {
            IpAddr::V6(ip) => ip.octets()[..8].try_into().expect("8 of 16 bytes"),
            IpAddr::V4(ip) => {
                let mut p = [0u8; 8];
                p[..4].copy_from_slice(&ip.octets());
                p
            }
        };
        let mut by_prefix = self.by_prefix.locked();
        let total: usize = by_prefix.values().sum();
        let here = by_prefix.get(&prefix).copied().unwrap_or(0);
        if total >= MAX_STRANGER_CONNECTIONS || here >= MAX_STRANGER_CONNECTIONS_PER_PREFIX {
            return None;
        }
        by_prefix.insert(prefix, here + 1);
        Some(StrangerSlot { limits: self.clone(), prefix })
    }
}

impl Drop for StrangerSlot {
    fn drop(&mut self) {
        let mut by_prefix = self.limits.by_prefix.locked();
        if let Some(count) = by_prefix.get_mut(&self.prefix) {
            *count -= 1;
            if *count == 0 {
                by_prefix.remove(&self.prefix);
            }
        }
    }
}

/// Best-effort: which peer's address on file is `remote`, if any — for
/// naming who an accept failure came from, and for noticing a peer entry
/// whose key looks stale.
fn peer_name_at_address(ctx: &Ctx, remote: SocketAddr) -> Option<String> {
    ctx.peers.locked().values().find_map(|entry| {
        let peer = entry.watch.borrow();
        crate::peers::literal_addresses(&peer.address).contains(&remote).then(|| peer.name.clone())
    })
}

/// Where a freshly accepted, authenticated connection goes: to its
/// peer's task if it's from someone on our list, otherwise to a stranger
/// responder — if there's room for one.
fn route_accepted(ctx: &Arc<Ctx>, conn: AuthenticatedConnection, strangers: &StrangerLimits) {
    let remote = transport::canonical(conn.connection.remote_address());
    let peer_key = conn.remote_public_key;
    if super::joining::is_offered(ctx, &conn.remote_user) {
        log!(ctx, "connection from {remote}: a device being added");
        super::joining::serve(ctx.clone(), conn);
        return;
    }
    if conn.remote_user == ctx.identity.public_key_bytes() {
        super::devices::accept(ctx, conn);
        return;
    }
    let known_name = ctx.names_by_key.locked().get(&peer_key).cloned();
    let label = known_name.clone().unwrap_or_else(|| STANDARD.encode(peer_key));
    if known_name.is_some() {
        let user = conn.remote_user;
        super::peerdevices::hand_over(ctx, &user, conn);
        return;
    }
    let Some(slot) = strangers.admit(remote) else {
        conn.connection.close(0u32.into(), b"busy");
        return;
    };
    log!(ctx, "connection from {remote} ({label})");
    if let Some(stale_name) = peer_name_at_address(ctx, remote) {
        log!(
            ctx,
            "note: {remote} matches the address on file for '{stale_name}', but presented a different key — \
             that entry looks stale; consider `swarm peer update {stale_name} {} {label}`",
            remote.ip(),
        );
    }
    spawn_stranger_responder(ctx.clone(), conn, remote, slot);
}

/// Someone not on our peers list introducing themselves: they've
/// added us, so they're shown as a contact request — unless blocked, or
/// deleted as a contact a moment ago, when true back has them told
/// they're removed; or added here meanwhile.
pub(super) fn contact_request(ctx: &Arc<Ctx>, root_key: [u8; 32], name: Option<displayname::DisplayName>, remote: SocketAddr) -> bool {
    let path = ctx.paths.requests();
    if requests::is_blocked(&path, &root_key).unwrap_or(false) {
        log!(ctx, "{remote} is blocked — telling it it's removed");
        return true;
    }
    if super::just_deleted(ctx, &root_key) {
        log!(ctx, "{remote} was deleted as a contact just now — telling it it's removed");
        return true;
    }
    let is_peer = || crate::peers::load(&ctx.paths.peers()).unwrap_or_default().iter().any(|p| p.public_key == root_key);
    if is_peer() {
        return false;
    }
    let verified_name = name.filter(|claimed| displayname::verify(claimed, &root_key));
    if let Some(claimed) = &verified_name {
        let _ = displayname::cache_one(&ctx.paths.displaynames(), root_key, claimed);
    }
    if crate::groupintro::asked(&ctx.paths.group_asks(), unix_now()).iter().any(|i| i.to == root_key) {
        added_back(ctx, root_key, verified_name.as_ref(), remote);
        return false;
    }
    if let Ok(true) = requests::record(&path, root_key, &crate::peers::address_for(remote), unix_now()) {
        if is_peer() {
            let _ = requests::remove(&path, &root_key);
            return false;
        }
        let who = verified_name.map(|claimed| claimed.name).unwrap_or_else(|| STANDARD.encode(root_key));
        log!(ctx, "contact request from {who} at {remote} — they've added you as a contact");
    }
    false
}

/// Someone this user asked, through a group, to add them, who has: added
/// as a contact, named after themselves, at the address they came from.
fn added_back(ctx: &Arc<Ctx>, key: [u8; 32], name: Option<&displayname::DisplayName>, remote: SocketAddr) {
    let name = name.map(|d| d.name.clone()).filter(|n| crate::peers::validate_name(n).is_ok()).unwrap_or_else(|| STANDARD.encode(key).chars().take(10).collect());
    let peer = crate::peers::Peer { name: name.clone(), address: crate::peers::address_for(remote), public_key: key, cover_bits_per_second: None, cover_required: false };
    if crate::peers::add(&ctx.paths.peers(), &peer).is_ok() {
        let _ = crate::settings::set_named_after_them(&ctx.paths.settings(), &key, true);
        let _ = crate::groupintro::done(&ctx.paths.group_asks(), &key, unix_now());
        super::NodeHandle { ctx: ctx.clone() }.add_peer_live(peer);
        log!(ctx, "{name} added you, as you asked through a group: they're your contact now");
    }
}

/// Answers whatever a connection from someone *not* on our peers list
/// asks, for as long as they keep it open (up to
/// `STRANGER_CONNECTION_LIFETIME`). Public posts only. Their messages
/// are refused; their introduction becomes a contact request (see
/// `requests`) unless they're blocked.
fn spawn_stranger_responder(ctx: Arc<Ctx>, mut conn: AuthenticatedConnection, remote: SocketAddr, slot: StrangerSlot) {
    tokio::spawn(async move {
        let _slot = slot;
        let paths = ctx.paths.clone();
        let serve = protocol::serve_connection(
            &mut conn,
            |root_key, name| contact_request(&ctx, root_key, name, remote),
            |since| posts::asked(&paths.posts(), since, false),
            || paths.my_display_name(),
            |root_key, following| {
                if !requests::is_blocked(&paths.requests(), &root_key).unwrap_or(false) {
                    let _ = crate::followers::record(&paths.followers(), root_key, following, unix_now());
                }
            },
            |root_key| super::withdraw::request_cancelled(&ctx, &root_key),
        );
        let result = tokio::time::timeout(STRANGER_CONNECTION_LIFETIME, serve).await.unwrap_or(Ok(()));
        if let Err(e) = result {
            log!(ctx, "connection from {remote} ended: {e}");
        }
    });
}
