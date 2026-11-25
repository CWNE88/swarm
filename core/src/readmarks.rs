//! How far each chat has been read (`~/.swarm/readmarks`, one per line),
//! the same on all of a user's devices (`node::ownread`).
//!
//! A mark is the send time of the newest message read, which is the same
//! on every device (a history `Entry`'s `at`, a channel post's
//! `created_at`), where a count wouldn't be. The later time wins.

use crate::channel::{self, ChannelId};
use crate::store;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::collections::HashMap;
use std::path::Path;

/// The most marks kept; past it, those read longest ago go.
pub const MAX_MARKS: usize = 4096;

/// A chat that can be read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Chat {
    /// With a contact (their user key).
    Contact([u8; 32]),
    Channel(ChannelId),
}

pub type Marks = HashMap<Chat, u64>;

fn format_line(chat: &Chat, at: u64) -> String {
    match chat {
        Chat::Contact(key) => format!("{at}\tc\t{}\n", STANDARD.encode(key)),
        Chat::Channel(id) => format!("{at}\th\t{}\t{}\n", STANDARD.encode(id.owner), id.name),
    }
}

fn parse_line(line: &str) -> Option<(Chat, u64)> {
    let mut parts = line.splitn(4, '\t');
    let at = parts.next()?.parse().ok()?;
    let chat = match parts.next()? {
        "c" => Chat::Contact(STANDARD.decode(parts.next()?).ok()?.try_into().ok()?),
        "h" => {
            let owner: [u8; 32] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
            let name = parts.next()?.to_string();
            channel::validate_name(&name).ok()?;
            Chat::Channel(ChannelId { name, owner })
        }
        _ => return None,
    };
    Some((chat, at))
}

/// Every mark.
pub fn load(path: &Path) -> Marks {
    store::read(path).map(|s| s.lines().filter_map(parse_line).collect()).unwrap_or_default()
}

fn save(path: &Path, marks: &Marks) -> std::io::Result<()> {
    let mut lines: Vec<(&Chat, &u64)> = marks.iter().collect();
    lines.sort_by_key(|(_, at)| std::cmp::Reverse(**at));
    lines.truncate(MAX_MARKS);
    store::write_atomic(path, lines.into_iter().map(|(chat, at)| format_line(chat, *at)).collect::<String>())
}

/// Takes in `theirs`, each only if later than what's here. Returns
/// whether anything changed.
pub fn merge(path: &Path, theirs: impl IntoIterator<Item = (Chat, u64)>) -> std::io::Result<bool> {
    let _guard = store::lock();
    let mut marks = load(path);
    let mut changed = false;
    for (chat, at) in theirs {
        if let Chat::Channel(id) = &chat
            && channel::validate_name(&id.name).is_err()
        {
            continue;
        }
        if at > marks.get(&chat).copied().unwrap_or(0) {
            marks.insert(chat, at);
            changed = true;
        }
    }
    if changed {
        save(path, &marks)?;
    }
    Ok(changed)
}

/// Drops `chat`'s mark, if it has one.
pub fn forget(path: &Path, chat: &Chat) -> std::io::Result<()> {
    let _guard = store::lock();
    let mut marks = load(path);
    if marks.remove(chat).is_some() {
        save(path, &marks)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_kept_and_only_move_forward() {
        let path = std::env::temp_dir().join(format!("swarm-readmarks-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let general = Chat::Channel(ChannelId { name: "general chat".into(), owner: [1; 32] });
        let bob = Chat::Contact([2; 32]);
        assert!(merge(&path, [(general.clone(), 100), (bob.clone(), 50)]).unwrap());
        assert!(!merge(&path, [(general.clone(), 100), (bob.clone(), 49)]).unwrap(), "nothing later");
        assert!(merge(&path, [(bob.clone(), 60)]).unwrap());
        let marks = load(&path);
        assert_eq!((marks[&general], marks[&bob]), (100, 60));
        let _ = std::fs::remove_file(&path);
    }
}
