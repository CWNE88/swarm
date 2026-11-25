//! Work an app hands off to run in the background — the screen never
//! waits on the network: sending a message, fetching someone's posts,
//! offering a file. Each reports back on the receiver
//! `Jobs::new` hands out, which the app's loop listens to and passes to
//! `finished`.
//!
//! Also each peer's posts as last fetched (`Timelines`), and when each
//! person followed was last asked for theirs (`Followed`).

use crate::notes::Note;
use crate::paths;
use crate::sidebar::Sidebar;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use swarm_core::node::{Delivery, NodeHandle, PostsResult};
use swarm_core::postcache;
use tokio::sync::mpsc;

/// What a background job came back with.
pub enum Done {
    Sent { peer: String, result: Result<Delivery> },
    Posts { peer: String, result: Result<PostsResult> },
    Offered { peer: String, name: String, result: Result<swarm_core::transfer::TransferId> },
    /// Someone followed, fetched again.
    Followed { label: String, result: Result<PostsResult> },
}

/// Starts background jobs.
#[derive(Clone)]
pub struct Jobs {
    tx: mpsc::UnboundedSender<Done>,
}

impl Jobs {
    /// The starter, and the receiver its results arrive on.
    pub fn new() -> (Jobs, mpsc::UnboundedReceiver<Done>) {
        let (tx, results) = mpsc::unbounded_channel();
        (Jobs { tx }, results)
    }

    /// Sends `text` to `row`'s peer — every device of theirs.
    pub fn send(&self, node: &NodeHandle, row: &crate::sidebar::PeerRow, text: String) {
        let (node, tx) = (node.clone(), self.tx.clone());
        let (peer, names) = (row.label.clone(), row.entries.clone());
        tokio::spawn(async move {
            let result = node.send_to_all(&names, &text).await;
            let _ = tx.send(Done::Sent { peer, result });
        });
    }

    /// Offers the file at `path` to `peer`.
    pub fn offer(&self, node: &NodeHandle, peer: String, path: std::path::PathBuf) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (node, tx) = (node.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = node.offer_file(&peer, &path).await;
            let peer = node.label_of(&peer);
            let _ = tx.send(Done::Offered { peer, name, result });
        });
    }

    /// Fetches the posts of someone followed (`label`: what they're
    /// called), into the cache the sidebar and their view read.
    pub fn refresh_followed(&self, node: &NodeHandle, key: [u8; 32], label: String) {
        let (node, tx) = (node.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = node.fetch_followed(&key).await;
            let _ = tx.send(Done::Followed { label, result });
        });
    }

    /// Fetches a followed person's posts: `now`, or when they haven't
    /// been asked for them in the last minute (they're not a peer: no
    /// connection is kept to them, each fetch dials). Asked for now, it
    /// says so.
    pub fn fetch_followed(&self, node: &NodeHandle, fetched: &mut Followed, row: &crate::sidebar::FollowRow, now: bool) -> Option<Note> {
        if !now && fetched.0.get(&row.public_key).is_some_and(|at| at.elapsed() < Followed::AGAIN) {
            return None;
        }
        fetched.fetched(row.public_key);
        self.refresh_followed(node, row.public_key, row.label.clone());
        now.then(|| Note::info(format!("asking {} for their posts...", row.label)))
    }
}

/// When each person followed was last asked for their posts.
#[derive(Default)]
pub struct Followed(HashMap<[u8; 32], Instant>);

impl Followed {
    /// How long before their posts are asked for again unasked.
    const AGAIN: Duration = Duration::from_secs(60);

    /// Their posts were just asked for (following someone fetches them).
    pub fn fetched(&mut self, key: [u8; 32]) {
        self.0.insert(key, Instant::now());
    }
}

/// Each peer's posts as last fetched (live, or what was saved when they
/// couldn't be reached), and how many had been seen.
#[derive(Default)]
pub struct Timelines {
    pub posts: HashMap<String, PostsResult>,
    fetching: HashSet<String>,
    seen: HashMap<String, usize>,
}

impl Timelines {
    /// Fetches peer `peer`'s posts in the background — unless `force` is
    /// off and they've been fetched already this session, or a fetch is
    /// under way. Someone who hasn't added us isn't asked (it would mean
    /// dialling them every time); their saved posts are shown instead.
    pub fn fetch(&mut self, jobs: &Jobs, node: &NodeHandle, sidebar: &Sidebar, peer: &str, force: bool) {
        let Some(row) = sidebar.peer_by_name(peer) else { return };
        if !force && self.posts.contains_key(peer) {
            return;
        }
        if row.not_added {
            let posts = postcache::cached(&paths().postcache(), &row.public_key).unwrap_or_default();
            self.posts.insert(peer.to_string(), PostsResult::Cached { posts, reason: format!("{} {}", row.label, swarm_core::node::NOT_ADDED) });
            return;
        }
        if !self.fetching.insert(peer.to_string()) {
            return;
        }
        let (node, tx, peer) = (node.clone(), jobs.tx.clone(), peer.to_string());
        tokio::spawn(async move {
            let result = node.fetch_posts(&peer).await;
            let _ = tx.send(Done::Posts { peer, result });
        });
    }

    /// How many posts `peer` has, as far as known.
    pub fn count(&self, peer: &str) -> Option<usize> {
        self.posts.get(peer).map(|r| match r {
            PostsResult::Live(posts) | PostsResult::Cached { posts, .. } => posts.len(),
        })
    }

    /// Whether `peer` has posts not yet seen (since `baseline` or
    /// `mark_seen`).
    pub fn has_new(&self, peer: &str) -> bool {
        self.count(peer).is_some_and(|count| count > self.seen.get(peer).copied().unwrap_or(usize::MAX))
    }

    /// The first count known for `peer` is the starting point, so only
    /// posts that turn up later count as new.
    pub fn baseline(&mut self, peer: &str) {
        if let Some(count) = self.count(peer) {
            self.seen.entry(peer.to_string()).or_insert(count);
        }
    }

    /// `peer`'s posts are on screen: all seen.
    pub fn mark_seen(&mut self, peer: &str) {
        if let Some(count) = self.count(peer) {
            self.seen.insert(peer.to_string(), count);
        }
    }
}

/// Takes in a finished job; returns what to tell the person, if anything.
pub fn finished(done: Done, timelines: &mut Timelines, sidebar: &Sidebar) -> Option<Note> {
    match done {
        Done::Sent { peer, result } => match result {
            Ok(Delivery::Delivered | Delivery::Relayed) => None,
            Ok(Delivery::Queued) => Some(Note::info(format!("{peer} is offline — it goes when they're back"))),
            Ok(Delivery::HeldUntilAdded) => Some(Note::info(format!("{peer} hasn't added you yet — your message waits here until they do"))),
            Err(e) => Some(Note::error(format!("couldn't send: {e:#}"))),
        },
        Done::Posts { peer, result } => {
            timelines.fetching.remove(&peer);
            let result = result.unwrap_or_else(|e| {
                let row = sidebar.peer_by_name(&peer);
                let posts = row.and_then(|r| postcache::cached(&r.folder.postcache(), &r.public_key).ok()).unwrap_or_default();
                PostsResult::Cached { posts, reason: format!("{e:#}") }
            });
            timelines.posts.insert(peer, result);
            None
        }
        Done::Offered { peer, name, result } => Some(match result {
            Ok(_) => Note::success(format!("offered {name} to {peer} — it goes once they accept")),
            Err(e) => Note::error(format!("couldn't offer {name}: {e:#}")),
        }),
        Done::Followed { label, result } => match result {
            Ok(PostsResult::Live(_)) => None,
            Ok(PostsResult::Cached { .. }) => Some(Note::info(format!("couldn't reach {label} — this is what you saw last"))),
            Err(e) => Some(Note::info(format!("nothing from {label} yet: {e:#}"))),
        },
    }
}
