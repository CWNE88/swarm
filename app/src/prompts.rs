//! Things that arrive and need an answer — a file offered to you — each
//! put to the person once; and news about
//! transfers already under way (done, declined, failed).

use crate::notes::Note;
use crate::sidebar::Sidebar;
use crate::text::{human_size, rough_duration};
use std::collections::{HashMap, HashSet};
use swarm_core::node::{format_bitrate, NodeHandle, TransferStatus};
use swarm_core::transfer::{Direction, State, TransferId};

/// Whether `t` is an offer to us still waiting on an answer.
pub fn is_open_offer(t: &TransferStatus) -> bool {
    t.transfer.direction == Direction::In && t.transfer.state == State::Offered
}

/// What's become of an offer put to the person, not yet answered here.
pub enum OfferNow {
    /// Still waiting on an answer.
    Open,
    /// Answered, or given up on, elsewhere: its question goes, with a
    /// note if nothing else will say why (one taken on another of your
    /// devices is off the list here altogether; a cancel or a decline
    /// has news of its own, see `Prompts::transfer_news`).
    Gone(Option<Note>),
}

pub fn offer_now(node: &NodeHandle, offer: &TransferStatus) -> OfferNow {
    match node.transfers().into_iter().find(|t| t.transfer.id == offer.transfer.id) {
        Some(t) if is_open_offer(&t) => OfferNow::Open,
        Some(_) => OfferNow::Gone(None),
        None => OfferNow::Gone(Some(Note::info(format!(
            "{} was taken on another of your devices",
            crate::text::shorten_middle(&offer.transfer.name, 32)
        )))),
    }
}

/// " — about 5 min at 64k", from the peer's cover rate: file data gets
/// what the slots have spare, roughly nine tenths of the rate when
/// nothing else is moving. Nothing without cover (it's quick).
pub fn transfer_estimate(node: &NodeHandle, sidebar: &Sidebar, peer: &str, size: u64) -> String {
    let rate = node
        .peer_status(peer)
        .and_then(|s| s.cover)
        .and_then(|c| c.sends)
        .or_else(|| sidebar.peer_by_name(peer).and_then(|row| row.cover_bits_per_second).or(node.default_cover()));
    let Some(rate) = rate else { return String::new() };
    let bytes_per_second = (rate as f64 / 8.0 * 0.9).max(1.0);
    format!(" — about {} at {}", rough_duration(size as f64 / bytes_per_second), format_bitrate(rate))
}

/// What's been put to the person already, and each transfer's state when
/// last looked at.
#[derive(Default)]
pub struct Prompts {
    transfer_states: HashMap<TransferId, State>,
    offers_seen: HashSet<TransferId>,
}

impl Prompts {
    /// News about transfers whose state has changed since last time.
    pub fn transfer_news(&mut self, node: &NodeHandle) -> Vec<Note> {
        let mut notes = Vec::new();
        for t in node.transfers() {
            let was = self.transfer_states.insert(t.transfer.id, t.transfer.state);
            if was.is_none() || was == Some(t.transfer.state) {
                continue;
            }
            let who = t.peer_name.clone().unwrap_or_else(|| "them".to_string());
            let name = &crate::text::shorten_middle(&t.transfer.name, 32);
            notes.extend(match (t.transfer.direction, t.transfer.state) {
                (Direction::In, State::Done) => Some(Note::success(format!("{name} from {who} saved to {}", t.transfer.path.display()))),
                (Direction::Out, State::Done) => Some(Note::success(format!("{who} has {name}"))),
                (Direction::Out, State::Active) => Some(Note::info(format!("{who} accepted {name}"))),
                (Direction::Out, State::Declined) => Some(Note::warning(format!("{who} declined {name}"))),
                (_, State::Failed | State::Cancelled) if !t.transfer.note.is_empty() && t.transfer.note != "cancelled here" => {
                    Some(Note::error(format!("{name}: {}", t.transfer.note)))
                }
                _ => None,
            });
        }
        notes
    }

    /// A file offered to us that hasn't been put to the person yet — now
    /// counted as put to them — with a sentence about it.
    pub fn next_offer(&mut self, node: &NodeHandle, sidebar: &Sidebar) -> Option<(TransferStatus, Note)> {
        let t = node.transfers().into_iter().find(|t| is_open_offer(t) && !self.offers_seen.contains(&t.transfer.id))?;
        self.offers_seen.insert(t.transfer.id);
        let who = t.peer_name.as_deref().unwrap_or("someone");
        let estimate = transfer_estimate(node, sidebar, who, t.transfer.size);
        let note = Note::warning(format!(
            "{} offers {} ({}){estimate}",
            crate::text::shorten_middle(who, 24),
            crate::text::shorten_middle(&t.transfer.name, 32),
            human_size(t.transfer.size)
        ));
        Some((t, note))
    }

    /// What's happened with your devices, and timers set by others.
    pub fn link_news(&mut self, node: &NodeHandle) -> Vec<Note> {
        node.device_news().into_iter().map(Note::info).collect()
    }
}
