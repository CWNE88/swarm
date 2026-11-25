//! What people are called. Your peers are always the name *you* gave
//! them; anyone else goes by the name they published (signed by their
//! key), shown with a bit of the key wherever that's needed to tell them
//! apart; with no name, the start of their key.

use crate::paths;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::collections::{HashMap, HashSet};
use swarm_core::channel::{self, ChannelId};
use swarm_core::displayname;

/// The start of a key — enough to tell people apart at a glance.
pub fn short_key(key: &[u8; 32]) -> String {
    STANDARD.encode(key).chars().take(10).collect()
}

/// Your own display name, if you've set one.
pub fn my_name() -> Option<String> {
    displayname::current(&paths().displayname()).ok().flatten().map(|d| d.name)
}

/// Someone's published name with a little of their key (`Rupert~UGhT`),
/// or the start of their key.
pub fn published_or_key(key: &[u8; 32]) -> String {
    let short = short_key(key);
    match displayname::cached_one(&paths().displaynames(), key).ok().flatten() {
        Some(d) => format!("{}~{}", d.name, &short[..4]),
        None => short,
    }
}

/// What a group is called: the name a member gave it, or else the other
/// members' names (yours too when you're alone in it), in the order they
/// were added.
pub fn group_name(peers: &HashMap<[u8; 32], String>, me: [u8; 32], id: &ChannelId, roster: &channel::Roster) -> String {
    if let Some(name) = &roster.name {
        return name.text.clone();
    }
    let names = ChannelNames::for_channel(peers.clone(), me, id);
    let mut others: Vec<(&[u8; 32], &channel::Member)> = roster.members.iter().filter(|(key, _)| **key != me).collect();
    others.sort_by_key(|(key, m)| (m.joined_at, **key));
    match others.as_slice() {
        [] => names.current(&me),
        others => others.iter().map(|(key, _)| names.current(key)).collect::<Vec<_>>().join(", "),
    }
}

/// What everyone in a group is called, here and now.
///
/// - **Your peers:** always the name *you* gave them.
/// - **You:** your own name.
/// - **Anyone else:** the name they signed each message with (IRC-style:
///   a line keeps the name it was said under), or for the member list
///   the newest name known for them — from their messages or the names
///   passed round when catching up. No name: the start of their key.
/// - A claimed name that someone else here also goes by (another member,
///   one of your peers, you) gets a little of their key added,
///   `Rupert~UGhT`, so two people can't pass as one.
pub struct ChannelNames {
    me: [u8; 32],
    my_name: Option<String>,
    peers: HashMap<[u8; 32], String>,
    /// The newest name known for each non-peer.
    latest: HashMap<[u8; 32], String>,
    /// Who goes by each name (lower-cased).
    taken: HashMap<String, HashSet<[u8; 32]>>,
}

impl ChannelNames {
    /// From your peers (key → the name you gave them), your own key, and
    /// the channel's messages.
    pub fn load(peers: HashMap<[u8; 32], String>, me: [u8; 32], posts: &[channel::ChannelPost]) -> ChannelNames {
        let my_name = my_name();
        let mut newest: HashMap<[u8; 32], (u64, String)> = HashMap::new();
        for post in posts {
            if let Some(name) = &post.name {
                let entry = newest.entry(post.author).or_insert((0, String::new()));
                if post.created_at >= entry.0 {
                    *entry = (post.created_at, name.clone());
                }
            }
        }
        for (key, claimed) in displayname::cached_all(&paths().displaynames()).unwrap_or_default() {
            if let Some(entry) = newest.get_mut(&key) {
                if claimed.at > entry.0 {
                    *entry = (claimed.at, claimed.name);
                }
            } else {
                newest.insert(key, (claimed.at, claimed.name));
            }
        }
        let latest: HashMap<[u8; 32], String> = newest.into_iter().map(|(k, (_, n))| (k, n)).collect();
        let mut taken: HashMap<String, HashSet<[u8; 32]>> = HashMap::new();
        let mut claim = |name: &str, key: [u8; 32]| {
            taken.entry(name.to_lowercase()).or_default().insert(key);
        };
        for post in posts {
            if let Some(name) = &post.name {
                claim(name, post.author);
            }
        }
        for (key, name) in latest.iter().chain(peers.iter()) {
            claim(name, *key);
        }
        if let Some(name) = &my_name {
            claim(name, me);
        }
        ChannelNames { me, my_name, peers, latest, taken }
    }

    /// The same, reading the channel's messages itself.
    pub fn for_channel(peers: HashMap<[u8; 32], String>, me: [u8; 32], id: &ChannelId) -> ChannelNames {
        ChannelNames::load(peers, me, &channel::load_posts(&paths().channels(), id).unwrap_or_default())
    }

    /// Whether `key` is one of your peers.
    pub fn is_peer(&self, key: &[u8; 32]) -> bool {
        self.peers.contains_key(key)
    }

    /// `name` as someone who isn't a peer claims it: as it is, or with a
    /// little of their key if someone else here goes by it too.
    pub fn claimed(&self, key: &[u8; 32], name: Option<&str>) -> String {
        match name {
            Some(name) if self.taken.get(&name.to_lowercase()).is_some_and(|keys| keys.iter().any(|k| k != key)) => {
                format!("{name}~{}", &short_key(key)[..4])
            }
            Some(name) => name.to_string(),
            None => short_key(key),
        }
    }

    /// What they're called now — the member list, events, commands.
    pub fn current(&self, key: &[u8; 32]) -> String {
        if *key == self.me {
            return self.my_name.clone().unwrap_or_else(|| "you".to_string());
        }
        if let Some(name) = self.peers.get(key) {
            return name.clone();
        }
        self.claimed(key, self.latest.get(key).map(String::as_str))
    }

    /// What they were called when they said `post`.
    pub fn said_as(&self, post: &channel::ChannelPost) -> String {
        if let Some(name) = self.peers.get(&post.author) {
            return name.clone();
        }
        if post.author == self.me {
            return post.name.clone().or_else(|| self.my_name.clone()).unwrap_or_else(|| "you".to_string());
        }
        match &post.name {
            Some(name) => self.claimed(&post.author, Some(name)),
            None => self.current(&post.author),
        }
    }
}
