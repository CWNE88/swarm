//! A node's jobs with no node running: one connection, made for the job
//! and dropped after — `swarm send`/`posts`/`ping`/`call` when
//! nothing else on this device is keeping connections. Messages still
//! go through the outbox, so one that can't be delivered now goes the
//! next time a node runs.

use super::{check_message, newest_cached, posts_result, queue_message, Delivery, Paths, PostsResult};
use crate::call::{CallCommand, CallLink, CallSignal};
use crate::peers::Peer;
use crate::transport::{self, AuthenticatedConnection};
use crate::{outbox, protocol};
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Dials `address` (one address or a comma-separated list), expecting
/// `public_key` there, from a fresh endpoint on an ephemeral port — each
/// address bounded by `protocol::REPLY_TIMEOUT`.
pub async fn dial(keys: transport::Keys<'_>, address: &str, public_key: [u8; 32]) -> Result<AuthenticatedConnection> {
    let resolved: Vec<SocketAddr> = crate::peers::resolve_address(address).await.with_context(|| format!("resolving {address}"))?;
    if resolved.is_empty() {
        anyhow::bail!("could not resolve {address}");
    }
    let endpoint = transport::bind("[::]:0".parse()?).or_else(|_| transport::bind("0.0.0.0:0".parse()?)).context("binding QUIC endpoint")?;
    super::peer::dial_any(&endpoint, &resolved, keys, public_key).await.with_context(|| format!("connecting to {address}"))
}

pub async fn ping(keys: transport::Keys<'_>, peer: &Peer) -> Result<Duration> {
    let mut conn = dial(keys, &peer.address, peer.public_key).await?;
    protocol::ping(&mut conn).await
}

/// Sends `text` to `peer` over a connection of its own. It's in the
/// outbox until they acknowledge it, so if this can't deliver it, a node
/// run later will — and they'll never see it twice.
pub async fn send_message(keys: transport::Keys<'_>, paths: &Paths, peer: &Peer, text: &str) -> Result<Delivery> {
    let text = check_message(text)?;
    let (id, sent_at, lifetime) = queue_message(paths, &peer.public_key, &text)?;
    let attempt = async {
        let mut conn = dial(keys, &peer.address, peer.public_key).await?;
        protocol::send_message_and_wait(&mut conn, id, sent_at, lifetime, &text, &paths.my_introduction(keys.user)).await
    };
    let result = tokio::time::timeout(protocol::REPLY_TIMEOUT * 3, attempt).await.unwrap_or_else(|_| Err(anyhow::anyhow!("no answer")));
    match result {
        Ok(()) => {
            let _ = outbox::remove(&paths.outbox(), &peer.public_key, &id);
            let _ = crate::waiting::remove(&paths.waiting(), &peer.public_key, &id);
            Ok(Delivery::Delivered)
        }
        Err(e) if e.is::<protocol::NotAPeer>() => Ok(Delivery::HeldUntilAdded),
        Err(_) => Ok(Delivery::Queued),
    }
}

/// `peer`'s posts, fetched over a connection of its own — see
/// `NodeHandle::fetch_posts`.
pub async fn fetch_posts(keys: transport::Keys<'_>, paths: &Paths, peer: &Peer, log: &dyn Fn(String)) -> Result<PostsResult> {
    let since = newest_cached(paths, &peer.public_key);
    let attempt = async {
        let mut conn = dial(keys, &peer.address, peer.public_key).await?;
        protocol::list_posts_and_wait(&mut conn, since).await.context("requesting posts")
    };
    let fetched = tokio::time::timeout(protocol::REPLY_TIMEOUT * 3, attempt).await.unwrap_or_else(|_| Err(anyhow::anyhow!("no answer")));
    posts_result(paths, &peer.name, &peer.public_key, &[peer.public_key], fetched, log, || true)
}

/// Dials `peer` and gets a call ready to ring: the call rides this one
/// connection in-band (see `call`), driven by a task of its own, at the
/// peer's own cover rate if they have one. Ring with `call::ring`.
pub async fn place_call(keys: transport::Keys<'_>, paths: &Paths, peer: &Peer) -> Result<CallLink> {
    if let Some(rate) = peer.cover_bits_per_second
        && !protocol::cover_rate_carries_voice(rate) {
            anyhow::bail!("{}'s cover rate {} is too low to carry voice (needs about 18k or more)", peer.name, super::format_bitrate(rate));
        }
    let mut conn = dial(keys, &peer.address, peer.public_key).await?;
    let cover_ticker = peer.cover_bits_per_second.map(|bps| {
        protocol::enable_cover(&mut conn, bps);
        super::peer::cover_slot_ticker()
    });
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let budget = crate::call::CallBudget::new();
    protocol::update_call_budget(&conn, &budget);
    tokio::spawn(drive_call_connection(conn, cover_ticker, commands_rx, budget.clone(), peer.cover_bits_per_second, paths.clone()));
    CallLink::attach(commands_tx, budget)
}

/// Drives a one-shot call's connection: cover slots, call traffic both
/// ways, and polite answers to anything else the peer asks meanwhile.
async fn drive_call_connection(
    mut conn: AuthenticatedConnection,
    mut cover_ticker: Option<tokio::time::Interval>,
    mut commands: mpsc::UnboundedReceiver<CallCommand>,
    budget: Arc<crate::call::CallBudget>,
    sends: Option<u64>,
    paths: Paths,
) {
    let _ = protocol::send_cover_hello(&mut conn, sends, None, crate::settings::receive_limit(&paths.settings())).await;
    let mut call_in: Option<mpsc::UnboundedSender<CallSignal>> = None;
    let mut uncovered = crate::call::UncoveredCall::new();
    let mut each_second = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        let outcome = tokio::select! {
            _ = super::peer::tick_if_some(&mut cover_ticker) => {
                let sent = protocol::emit_cover_slot(&mut conn).await;
                protocol::update_call_budget(&conn, &budget);
                sent
            }
            _ = each_second.tick(), if conn.cover_out.is_none() => {
                protocol::set_uncovered_call_budget(&budget, &mut uncovered, &mut conn);
                Ok(())
            }
            Some(command) = commands.recv() => match command {
                CallCommand::Attach(tx) | CallCommand::Pass(tx) => {
                    call_in = Some(tx);
                    Ok(())
                }
                CallCommand::Signal(signal) => protocol::send_call(&mut conn, signal).await,
            },
            incoming = protocol::recv(&mut conn) => match protocol::flush(&mut conn).await.and(incoming) {
                Ok(protocol::Incoming::Request(id, protocol::Request::Ping)) => protocol::reply(&mut conn, id, protocol::Reply::Pong).await,
                Ok(protocol::Incoming::Request(id, protocol::Request::WhoAmI)) => {
                    protocol::reply(&mut conn, id, protocol::Reply::DisplayName(paths.my_display_name())).await
                }
                Ok(protocol::Incoming::Request(..)) => Ok(()),
                Ok(protocol::Incoming::Notice(protocol::Notice::Call(signal))) => {
                    if let Some(tx) = &call_in {
                        let _ = tx.send(signal);
                    }
                    Ok(())
                }
                Ok(protocol::Incoming::Notice(protocol::Notice::CoverHello { requires: Some(required), .. })) => {
                    if protocol::raise_cover(&mut conn, required) {
                        cover_ticker = Some(super::peer::cover_slot_ticker());
                    }
                    protocol::update_call_budget(&conn, &budget);
                    Ok(())
                }
                Ok(_) => Ok(()),
                Err(e) => Err(e),
            },
        };
        if outcome.is_err() {
            return;
        }
    }
}
