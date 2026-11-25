//! Groups' answers: split into parts and put back together.

use super::*;

/// Answers a `ChannelCatchUp` with the roster and posts, in
/// as many parts as it takes — roster first, so by the time a member
/// sees posts it already knows who may write them.
pub async fn respond_channel_data(conn: &mut AuthenticatedConnection, id: RequestId, data: ChannelData) -> Result<()> {
    enum Item {
        Event(RosterEvent),
        Name(([u8; 32], DisplayName)),
        Post(ChannelPost),
    }
    let items: Vec<Item> = data
        .roster
        .into_iter()
        .map(Item::Event)
        .chain(data.names.into_iter().map(Item::Name))
        .chain(data.posts.into_iter().map(Item::Post))
        .collect();
    let parts = split_by_size(items, |item| match item {
        Item::Event(e) => encoded_size(e, write_roster_event),
        Item::Name(n) => encoded_size(n, write_member_name),
        Item::Post(p) => encoded_size(p, write_channel_post),
    });
    let last = parts.len() - 1;
    for (i, part) in parts.into_iter().enumerate() {
        let (mut roster, mut posts, mut names) = (Vec::new(), Vec::new(), Vec::new());
        for item in part {
            match item {
                Item::Event(e) => roster.push(e),
                Item::Name(n) => names.push(n),
                Item::Post(p) => posts.push(p),
            }
        }
        reply(conn, id, Reply::ChannelData { more: i < last, roster, posts, names }).await?;
    }
    Ok(())
}

/// A group's roster, posts and members' names, as one answer carries
/// them (or several parts of one, put back together).
#[derive(Debug, Default, Clone)]
pub struct ChannelData {
    pub roster: Vec<RosterEvent>,
    pub posts: Vec<ChannelPost>,
    pub names: Vec<([u8; 32], DisplayName)>,
}

/// The most roster entries and members' names a channel answer is put
/// back together from — far beyond an honest one, which has at most
/// `channel::MAX_POSTS` posts too.
const MOST_ROSTER_TAKEN: usize = 50_000;
const MOST_NAMES_TAKEN: usize = 10 * crate::channel::MAX_MEMBERS;

impl ChannelData {
    /// Adds one part of a split answer. False once it's full: whoever
    /// answers could send parts for ever, so the answer's then taken as
    /// complete, and anything more ignored.
    #[must_use]
    pub fn extend(&mut self, roster: Vec<RosterEvent>, posts: Vec<ChannelPost>, names: Vec<([u8; 32], DisplayName)>) -> bool {
        self.roster.extend(roster.into_iter().take(MOST_ROSTER_TAKEN - self.roster.len()));
        self.posts.extend(posts.into_iter().take(crate::channel::MAX_POSTS - self.posts.len()));
        self.names.extend(names.into_iter().take(MOST_NAMES_TAKEN - self.names.len()));
        self.roster.len() < MOST_ROSTER_TAKEN && self.posts.len() < crate::channel::MAX_POSTS && self.names.len() < MOST_NAMES_TAKEN
    }
}

/// What a `ChannelCatchUp` comes back with.
pub enum ChannelReply {
    Data(ChannelData),
    Error(String),
}

/// The one answer to any group request from someone who isn't in that
/// group here, whether or not it exists — so asking can't be used to
/// find out which groups someone is in.
pub const NO_SUCH_CHANNEL: &str = "no such group here";

/// Answers a group request `id` with whatever `answer` is.
pub async fn respond_channel(conn: &mut AuthenticatedConnection, id: RequestId, answer: ChannelReply) -> Result<()> {
    match answer {
        ChannelReply::Data(data) => respond_channel_data(conn, id, data).await,
        ChannelReply::Error(message) => reply(conn, id, Reply::ChannelError(message)).await,
    }
}
