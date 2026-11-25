//! Finding a peer again when both have moved while apart (see `card`):
//! cards for peers we've lost, handed to every peer who's connected;
//! cards from a peer for someone else, passed on if that someone is
//! connected here; cards for us, taken in.

use crate::lock::Locked;
use super::{peer::PeerRequest, unix_now, Ctx, NodeHandle};
use crate::card::Card;
use crate::protocol::Notice;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A peer not reached for this long is asked around for.
const LOST_AFTER: u64 = 10 * 60;
/// How often cards for a lost peer go out: every 10 minutes for their
/// first hour lost, then every hour.
const SEND_EVERY: Duration = Duration::from_secs(10 * 60);
const SEND_EVERY_LATER: Duration = Duration::from_secs(60 * 60);
/// How often the lost are looked for.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// A card is passed on at most this often for the same two people.
const PASS_ON_EVERY: Duration = Duration::from_secs(5 * 60);
/// A card older than this is ignored.
const CARD_LIFETIME: u64 = 24 * 60 * 60;

/// Cards passed on and taken in lately.
#[derive(Default)]
pub(super) struct CardState {
    passed: HashMap<([u8; 32], [u8; 32]), Instant>,
    /// The newest card taken in from each peer.
    heard: HashMap<[u8; 32], u64>,
    /// When cards last went out for each lost peer.
    sent: HashMap<[u8; 32], Instant>,
}

/// The peers connected now (name, key, where to hand them something).
fn connected(ctx: &Ctx) -> Vec<(String, [u8; 32], tokio::sync::mpsc::Sender<PeerRequest>)> {
    ctx.peers
        .locked()
        .iter()
        .filter(|(_, e)| e.online.load(Ordering::Relaxed))
        .map(|(_, e)| (e.watch.borrow().name.clone(), e.watch.borrow().public_key, e.requests.clone()))
        .collect()
}

/// Peers we can't reach now and have reached before: (name, key, last
/// seen). One for each user, on none of whose devices we're connected.
fn lost(ctx: &Ctx) -> Vec<(String, [u8; 32], u64)> {
    let lastseen = ctx.paths.lastseen();
    let users: Vec<(String, [u8; 32])> = ctx.peers.locked().iter().filter(|(_, e)| e.device.is_none() && !e.online.load(Ordering::Relaxed)).map(|(_, e)| (e.watch.borrow().name.clone(), e.watch.borrow().public_key)).collect();
    users
        .into_iter()
        .filter(|(_, key)| !super::peerdevices::any_online(ctx, key))
        .filter_map(|(name, key)| crate::lastseen::get(&lastseen, &key).map(|at| (name, key, at)))
        .collect()
}

/// Hands every connected peer a card for each lost peer in `which` (all
/// of them, or those due).
fn send_cards(ctx: &Ctx, only_due: bool) -> usize {
    let addresses = ctx.my_addresses.borrow().clone();
    if addresses.is_empty() {
        return 0;
    }
    let now = unix_now();
    let carriers = connected(ctx);
    let mut sent = 0;
    for (name, key, last_seen) in lost(ctx) {
        let lost_for = now.saturating_sub(last_seen);
        if only_due {
            let every = if lost_for < 60 * 60 + LOST_AFTER { SEND_EVERY } else { SEND_EVERY_LATER };
            let due = lost_for >= LOST_AFTER && ctx.cards.locked().sent.get(&key).is_none_or(|at| at.elapsed() >= every);
            if !due {
                continue;
            }
        }
        let card = Card::new(&ctx.identity, key, now, &addresses);
        let mut told = 0;
        for (_, _, requests) in carriers.iter().filter(|(_, k, _)| *k != key) {
            if requests.try_send(PeerRequest::Notice(Notice::Card(card.clone()))).is_ok() {
                told += 1;
            }
        }
        ctx.cards.locked().sent.insert(key, Instant::now());
        if told > 0 {
            log!(ctx, "{name}: can't reach them — asked {told} contact(s) to tell them where we are");
            sent += 1;
        }
    }
    sent
}

/// A card that came from peer `via` (on its connection).
pub(super) fn on_card(ctx: &Ctx, via: [u8; 32], via_name: &str, card: Card) {
    if !card.verifies() || unix_now().saturating_sub(card.at) > CARD_LIFETIME {
        return;
    }
    let me = ctx.identity.public_key_bytes();
    if card.to == me {
        take_in(ctx, via_name, card);
        return;
    }
    if card.from != via {
        return;
    }
    let Some((name, _, requests)) = connected(ctx).into_iter().find(|(_, key, _)| *key == card.to) else { return };
    {
        let mut state = ctx.cards.locked();
        if state.passed.get(&(card.from, card.to)).is_some_and(|at| at.elapsed() < PASS_ON_EVERY) {
            return;
        }
        state.passed.retain(|_, at| at.elapsed() < PASS_ON_EVERY);
        state.passed.insert((card.from, card.to), Instant::now());
    }
    if requests.try_send(PeerRequest::Notice(Notice::Card(card))).is_ok() {
        log!(ctx, "passed {via_name}'s whereabouts on to {name}, who it lost touch with");
    }
}

/// A card for us: kept only from someone on our peers list, and only if
/// it's newer than the last.
fn take_in(ctx: &Ctx, via_name: &str, card: Card) {
    let Some(peer) = crate::peers::load(&ctx.paths.peers()).unwrap_or_default().into_iter().find(|p| p.public_key == card.from) else { return };
    {
        let mut state = ctx.cards.locked();
        if state.heard.get(&card.from).is_some_and(|at| *at >= card.at) {
            return;
        }
        state.heard.insert(card.from, card.at);
    }
    let dir = ctx.paths.addresses();
    let newer = crate::addressbook::told(&dir, &card.from).is_none_or(|t| t.at < card.at);
    let kept = if newer { crate::addressbook::hear(&dir, &card.from, &card.addresses, card.at) } else { crate::addressbook::see(&dir, &card.from, &card.addresses) };
    if kept.is_ok() {
        log!(ctx, "{}: heard where they are now, through {via_name}", peer.name);
    }
}

/// Sends cards for lost peers as they fall due, for as long as the node
/// runs.
pub(super) async fn keep_sending(ctx: Arc<Ctx>) {
    while !ctx.stopped.load(Ordering::Relaxed) {
        tokio::time::sleep(CHECK_EVERY).await;
        send_cards(&ctx, true);
    }
}

impl NodeHandle {
    /// Asks every connected peer to tell each peer we can't reach now
    /// where we are — at once, not waiting for them to count as lost.
    /// How many lost peers it went out for.
    pub fn ask_around_now(&self) -> usize {
        send_cards(&self.ctx, false)
    }
}
