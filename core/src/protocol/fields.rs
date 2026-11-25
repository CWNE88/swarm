//! What several kinds of frame are made of: names, posts, roster
//! entries, lists.

use super::*;

pub(super) fn write_display_name(w: &mut Writer, name: &Option<DisplayName>) {
    match name {
        Some(dn) => {
            w.bool(true).u64(dn.at).array(&dn.signature).str(&dn.name);
        }
        None => {
            w.bool(false);
        }
    }
}

pub(super) fn request_status_byte(status: crate::requests::Status) -> u8 {
    use crate::requests::Status;
    match status {
        Status::Pending => 0,
        Status::Dismissed => 1,
        Status::Blocked => 2,
    }
}

pub(super) fn request_status(byte: u8) -> Result<crate::requests::Status> {
    use crate::requests::Status;
    Ok(match byte {
        0 => Status::Pending,
        1 => Status::Dismissed,
        2 => Status::Blocked,
        other => bail!("unknown contact request status {other}"),
    })
}

pub(super) fn read_display_name(r: &mut Reader) -> Result<Option<DisplayName>> {
    if !r.bool()? {
        return Ok(None);
    }
    let at = r.u64()?;
    let signature = r.array()?;
    let name = r.str()?;
    Ok(Some(DisplayName { name, at, signature }))
}

pub(super) fn write_post(w: &mut Writer, post: &Post) {
    w.u64(post.created_at).u8(matches!(post.visibility, Visibility::Private) as u8).array(&post.signature).str(&post.text);
}

pub(super) fn read_post(r: &mut Reader) -> Result<Post> {
    let created_at = r.u64()?;
    let visibility = if r.u8()? == 1 { Visibility::Private } else { Visibility::Public };
    let signature = r.array()?;
    let text = r.str()?;
    Ok(Post { created_at, visibility, signature, text })
}

pub(super) fn write_deletion(w: &mut Writer, d: &Deletion) {
    w.u64(d.post_at).array(&d.post_signature).u64(d.deleted_at).array(&d.signature);
}

pub(super) fn read_deletion(r: &mut Reader) -> Result<Deletion> {
    Ok(Deletion { post_at: r.u64()?, post_signature: r.array()?, deleted_at: r.u64()?, signature: r.array()?, public: false })
}

pub(super) fn write_roster_event(w: &mut Writer, event: &RosterEvent) {
    w.u64(event.at).array(&event.by).array(&event.subject);
    match &event.action {
        RosterAction::Joined => {
            w.u8(0);
        }
        RosterAction::Left => {
            w.u8(1);
        }
        RosterAction::Named { text } => {
            w.u8(2).str(text);
        }
        RosterAction::Timer { seconds } => {
            w.u8(3).u64(*seconds);
        }
    }
    w.u8(event.prev.len().min(u8::MAX as usize) as u8);
    for id in event.prev.iter().take(u8::MAX as usize) {
        w.array(id);
    }
    w.array(&event.signature);
}

pub(super) fn read_roster_event(r: &mut Reader) -> Result<RosterEvent> {
    let at = r.u64()?;
    let by = r.array()?;
    let subject = r.array()?;
    let action = match r.u8()? {
        0 => RosterAction::Joined,
        1 => RosterAction::Left,
        2 => RosterAction::Named { text: r.str()? },
        3 => RosterAction::Timer { seconds: r.u64()? },
        other => bail!("unknown roster action {other}"),
    };
    let prev_count = r.u8()? as usize;
    if prev_count > crate::channel::MAX_PREV {
        bail!("roster entry names too many earlier entries");
    }
    let prev = (0..prev_count).map(|_| r.array()).collect::<Result<Vec<_>>>()?;
    let signature = r.array()?;
    Ok(RosterEvent { at, by, subject, action, prev, signature })
}

pub(super) fn write_channel_post(w: &mut Writer, post: &ChannelPost) {
    w.array(&post.author).u64(post.created_at).array(&post.signature).str(&post.text);
    match &post.name {
        Some(name) => w.u8(1).str(name),
        None => w.u8(0),
    };
}

pub(super) fn read_channel_post(r: &mut Reader) -> Result<ChannelPost> {
    let author = r.array()?;
    let created_at = r.u64()?;
    let signature = r.array()?;
    let text = r.str()?;
    let name = match r.u8()? {
        0 => None,
        _ => Some(r.str()?),
    };
    Ok(ChannelPost { author, created_at, text, name, signature })
}

/// A group's id: its name and its owner.
pub(super) fn write_channel_id(w: &mut Writer, channel: &ChannelId) {
    w.str(&channel.name).array(&channel.owner);
}

pub(super) fn read_channel_id(r: &mut Reader) -> Result<ChannelId> {
    Ok(ChannelId { name: r.str()?, owner: r.array()? })
}

pub(super) fn write_digest(w: &mut Writer, d: &BucketDigest) {
    w.u64(d.bucket).u16(d.count).u64(d.xor);
}

pub(super) fn read_digest(r: &mut Reader) -> Result<BucketDigest> {
    Ok(BucketDigest { bucket: r.u64()?, count: r.u16()?, xor: r.u64()? })
}

pub(super) fn write_member_name(w: &mut Writer, (key, name): &([u8; 32], DisplayName)) {
    w.array(key);
    write_display_name(w, &Some(name.clone()));
}

pub(super) fn read_member_name(r: &mut Reader) -> Result<([u8; 32], DisplayName)> {
    let key = r.array()?;
    match read_display_name(r)? {
        Some(name) => Ok((key, name)),
        None => bail!("a member name with no name"),
    }
}

pub(super) fn write_list_entry(w: &mut Writer, entry: &crate::devicelist::Entry) {
    use crate::devicelist::Change;
    w.u64(entry.at).array(&entry.device);
    match &entry.change {
        Change::Added { name } => {
            w.u8(0).str(name);
        }
        Change::Renamed { name } => {
            w.u8(1).str(name);
        }
        Change::Relay { allowed } => {
            w.u8(2).bool(*allowed);
        }
        Change::Removed => {
            w.u8(3);
        }
    }
    w.array(&entry.signature);
}

pub(super) fn read_list_entry(r: &mut Reader) -> Result<crate::devicelist::Entry> {
    use crate::devicelist::Change;
    let (at, device) = (r.u64()?, r.array()?);
    let change = match r.u8()? {
        0 => Change::Added { name: r.str()? },
        1 => Change::Renamed { name: r.str()? },
        2 => Change::Relay { allowed: r.bool()? },
        3 => Change::Removed,
        _ => bail!("unknown device list entry"),
    };
    Ok(crate::devicelist::Entry { at, device, change, signature: r.array()? })
}

/// Encodes a list as a `u16` count and then each item.
pub(super) fn write_list<T>(w: &mut Writer, items: &[T], write: fn(&mut Writer, &T)) {
    w.u16(u16::try_from(items.len()).expect("a reply part is bounded well under u16::MAX items"));
    for item in items {
        write(w, item);
    }
}

pub(super) fn read_list<T>(r: &mut Reader, read: fn(&mut Reader) -> Result<T>) -> Result<Vec<T>> {
    let count = r.u16()? as usize;
    (0..count).map(|_| read(r)).collect()
}

/// A list counted in one byte: every item read, so whatever follows is
/// read where it is, but only the first `most` kept. A newer build may
/// send more than this one takes.
pub(super) fn read_short_list<T>(r: &mut Reader, most: usize, mut read: impl FnMut(&mut Reader) -> Result<T>) -> Result<Vec<T>> {
    let count = r.u8()? as usize;
    let mut kept = Vec::with_capacity(count.min(most));
    for _ in 0..count {
        let item = read(r)?;
        if kept.len() < most {
            kept.push(item);
        }
    }
    Ok(kept)
}

pub(super) fn write_roster(w: &mut Writer, roster: &crate::conference::Roster) {
    w.array(&roster.id).array(&roster.founder);
    write_list(w, &roster.added, |w, a| {
        w.array(&a.member).array(&a.by).u32(a.round).u32(a.number).array(&a.signature);
    });
    write_list(w, &roster.left, |w, l| {
        w.array(&l.member).u32(l.round).array(&l.by).u32(l.by_round).array(&l.signature);
    });
}

pub(super) fn read_roster(r: &mut Reader) -> Result<crate::conference::Roster> {
    use crate::conference::{Added, Left, Roster, MOST_ENTRIES};
    let (id, founder) = (r.array()?, r.array()?);
    let added = read_list(r, |r| Ok(Added { member: r.array()?, by: r.array()?, round: r.u32()?, number: r.u32()?, signature: r.array()? }))?;
    let left = read_list(r, |r| Ok(Left { member: r.array()?, round: r.u32()?, by: r.array()?, by_round: r.u32()?, signature: r.array()? }))?;
    if added.len() > MOST_ENTRIES || left.len() > MOST_ENTRIES {
        bail!("conference roster too long");
    }
    Ok(Roster { id, founder, added, left })
}
