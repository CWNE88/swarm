//! A user's own posts: a local, append-only log they publish nothing
//! from — nothing is ever sent anywhere just because a post was made.
//! Anyone who wants to see them has to actively ask (see `protocol`'s
//! `ListPosts`/`Posts` frames), and only gets an answer once the
//! responder decides what to hand back — see `visible`.
//!
//! Every post is signed by its author (see `Post::signature`) with
//! their root Ed25519 identity key, over exactly what `signable_bytes`
//! returns, and checked (`verify`) when it's fetched, before it's
//! cached. Today a post is only ever fetched from its author, over a
//! connection that already proves who they are; the signature is what
//! would let one be handed on by someone else and still be checked
//! against its real author.
//!
//! One flat file (`~/.swarm/posts`), one post per line:
//! `<unix_secs>\t<public|private>\t<base64_signature>\t<text>` — same
//! spirit as `outbox`'s format.
//!
//! A post deleted (`delete`) goes from the file, and a `Deletion` of it,
//! signed like a post, is kept beside it (`posts-deleted`) for
//! `DELETIONS_KEPT` (at most `MAX_DELETIONS`): it goes out with the posts
//! asked for (`Reply::Posts`), so whoever has a copy (the user's other
//! devices, contacts and followers, `postcache`) drops it too. One asking
//! again only after that keeps theirs.

use crate::identity::{self, Identity};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// Handed to anyone who completes the Noise handshake, known peer
    /// or not — the same open-by-default model direct messages already
    /// use (see `deliver_message`/`listen_forever`'s accept loop in
    /// `swarm-cli`, which never checks the peer list before accepting
    /// a connection either).
    Public,
    /// Only handed to a requester whose authenticated key matches
    /// something on the responder's local peers list — enforced by the
    /// caller (see `visible`), not by this module, since "is this
    /// requester a known peer" depends on the peers list, which lives
    /// in `swarm-cli`, not here.
    Private,
}

impl Visibility {
    /// Same tag used on disk (this file's format), on the wire
    /// (`protocol::Frame::Posts`), and in `swarm-cli`'s control-socket
    /// protocol — one canonical string form, not several that could
    /// drift apart. `pub`, not `pub(crate)`: `swarm-cli` needs it too.
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
        }
    }

    /// The inverse of `as_str` — defaults to `Public` for anything that
    /// isn't exactly `"private"`, same fallback `parse_line` already
    /// used, so a malformed or unexpected tag fails open to the more
    /// permissive visibility rather than silently hiding a post.
    pub fn parse(s: &str) -> Visibility {
        match s {
            "private" => Visibility::Private,
            _ => Visibility::Public,
        }
    }
}

/// The longest post accepted, in bytes — small enough that any one post
/// fits a single protocol frame with room to spare.
pub const MAX_POST_BYTES: usize = 2000;

/// Whether `text` is acceptable as a post: not empty, not too long, and
/// no control characters but line breaks (see `crate::text`). Part of `verify`, so a fetched post
/// that fails this is dropped like one with a bad signature.
pub fn is_valid_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_POST_BYTES && crate::text::is_message_text(text)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Post {
    pub created_at: u64,
    pub visibility: Visibility,
    /// The author's signature over `signable_bytes(created_at,
    /// visibility, text)` — see this module's docs for what it's for
    /// and why nothing checks it yet.
    pub signature: [u8; 64],
    pub text: String,
}

/// How long a post's deletion is kept to pass on, in seconds.
pub const DELETIONS_KEPT: u64 = 30 * 24 * 60 * 60;

/// The most deletions kept: past it, the oldest go.
pub const MAX_DELETIONS: usize = 1000;

/// How far before the time asked from a deletion is still passed on: a
/// clock (another of the user's devices') that's behind.
pub const DELETION_SLACK: u64 = 24 * 60 * 60;

/// A post taken back by its author: which (its time and signature, which
/// is unique to it), when, and the author's signature over that
/// (`deletion_bytes`), so only they can.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Deletion {
    pub post_at: u64,
    pub post_signature: [u8; 64],
    pub deleted_at: u64,
    pub signature: [u8; 64],
    /// Whether the post was for anyone, as far as this device knows: only
    /// those are passed on to someone not a contact. Not sent (or signed);
    /// one that comes from elsewhere is taken as not, unless the post it
    /// takes back was here and was.
    pub public: bool,
}

/// What a posts fetch brings: posts, and deletions of ones that may have
/// been fetched before.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    pub posts: Vec<Post>,
    pub deleted: Vec<Deletion>,
}

fn deletion_bytes(post_at: u64, post_signature: &[u8; 64], deleted_at: u64) -> Vec<u8> {
    format!("swarm post deleted\t{post_at}\t{}\t{deleted_at}", STANDARD.encode(post_signature)).into_bytes()
}

/// Whether `deletion` was made by whoever holds `author_public_key`.
pub fn verify_deletion(deletion: &Deletion, author_public_key: &[u8; 32]) -> bool {
    identity::verify(author_public_key, &deletion_bytes(deletion.post_at, &deletion.post_signature, deletion.deleted_at), &deletion.signature)
}

/// Where the deletions of the posts in `posts` are kept.
fn deletions_path(posts: &Path) -> std::path::PathBuf {
    posts.with_file_name("posts-deleted")
}

/// Every deletion kept, oldest first.
pub fn deletions(posts: &Path) -> Vec<Deletion> {
    let path = deletions_path(posts);
    if !path.exists() {
        return Vec::new();
    }
    crate::store::read(&path)
        .map(|text| {
            text.lines()
                .filter_map(|line| {
                    let mut f = line.split('\t');
                    let deleted_at = f.next()?.parse().ok()?;
                    let post_at = f.next()?.parse().ok()?;
                    let post_signature = STANDARD.decode(f.next()?).ok()?.try_into().ok()?;
                    let signature = STANDARD.decode(f.next()?).ok()?.try_into().ok()?;
                    let public = f.next() == Some("public");
                    Some(Deletion { post_at, post_signature, deleted_at, signature, public })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The deletions to pass on to someone asking for posts from `since`:
/// those made then or since (give or take `DELETION_SLACK`), of posts
/// they'd be given (`include_private`, as `visible`).
pub fn deletions_since(posts: &Path, since: u64, include_private: bool) -> Vec<Deletion> {
    deletions(posts).into_iter().filter(|d| d.deleted_at.saturating_add(DELETION_SLACK) >= since && (include_private || d.public)).collect()
}

/// The answer to someone asking for posts from `since`: those they may
/// see (`visible`), and the deletions since.
pub fn asked(path: &Path, since: u64, include_private: bool) -> Fetched {
    let posts = visible(path, include_private).unwrap_or_default().into_iter().filter(|p| p.created_at >= since).collect();
    Fetched { posts, deleted: deletions_since(path, since, include_private) }
}

/// Saves `all`, less those older than `DELETIONS_KEPT` by `now`, within
/// `MAX_DELETIONS`.
fn save_deletions(posts: &Path, mut all: Vec<Deletion>, now: u64) -> std::io::Result<()> {
    all.retain(|d| d.deleted_at.saturating_add(DELETIONS_KEPT) > now);
    all.sort_by_key(|d| d.deleted_at);
    let excess = all.len().saturating_sub(MAX_DELETIONS);
    all.drain(..excess);
    let text: String = all
        .iter()
        .map(|d| format!("{}\t{}\t{}\t{}\t{}\n", d.deleted_at, d.post_at, STANDARD.encode(d.post_signature), STANDARD.encode(d.signature), if d.public { "public" } else { "private" }))
        .collect();
    crate::store::write_atomic(&deletions_path(posts), text)
}

/// Lets go of deletions older than `DELETIONS_KEPT` by `now`. Whether
/// any went.
pub fn sweep_deletions(posts: &Path, now: u64) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    let all = deletions(posts);
    let before = all.len();
    if !all.iter().any(|d| d.deleted_at.saturating_add(DELETIONS_KEPT) <= now) {
        return Ok(false);
    }
    save_deletions(posts, all, now)?;
    Ok(deletions(posts).len() < before)
}

/// Writes `posts` in time order.
fn save(path: &Path, posts: &mut [Post]) -> std::io::Result<()> {
    posts.sort_by_key(|p| p.created_at);
    let text: String = posts.iter().map(|p| format!("{}\t{}\t{}\t{}\n", p.created_at, p.visibility.as_str(), STANDARD.encode(p.signature), crate::text::to_field(&p.text))).collect();
    crate::store::write_atomic(path, text)
}

/// Deletes the user's post whose signature is `post_signature`, signing
/// its deletion with `identity` to pass on. Whether it was there.
pub fn delete(path: &Path, identity: &Identity, post_signature: &[u8; 64], now: u64) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    let mut mine = all(path)?;
    let Some(at) = mine.iter().position(|p| p.signature == *post_signature) else { return Ok(false) };
    let post = mine.remove(at);
    save(path, &mut mine)?;
    let signature = identity.sign(&deletion_bytes(post.created_at, post_signature, now));
    let mut kept = deletions(path);
    kept.push(Deletion { post_at: post.created_at, post_signature: *post_signature, deleted_at: now, signature, public: post.visibility == Visibility::Public });
    save_deletions(path, kept, now)?;
    Ok(true)
}

/// Takes in deletions from another of this account's devices: those
/// signed by one of `authors` go from the posts here, and are kept to
/// pass on in turn. Returns how many posts went.
pub fn take_deletions(path: &Path, fetched: &[Deletion], authors: &[[u8; 32]], now: u64) -> std::io::Result<usize> {
    let _guard = crate::store::lock();
    let mut kept = deletions(path);
    let new: Vec<&Deletion> = fetched
        .iter()
        .filter(|d| d.deleted_at.saturating_add(DELETIONS_KEPT) > now && authors.iter().any(|a| verify_deletion(d, a)) && !kept.iter().any(|k| k.post_signature == d.post_signature))
        .collect();
    if new.is_empty() {
        return Ok(0);
    }
    let mut mine = all(path)?;
    let before = mine.len();
    let public: Vec<[u8; 64]> = mine.iter().filter(|p| p.visibility == Visibility::Public).map(|p| p.signature).collect();
    mine.retain(|p| !new.iter().any(|d| d.post_signature == p.signature));
    let gone = before - mine.len();
    if gone > 0 {
        save(path, &mut mine)?;
    }
    kept.extend(new.into_iter().map(|d| Deletion { public: d.public || public.contains(&d.post_signature), ..d.clone() }));
    save_deletions(path, kept, now)?;
    Ok(gone)
}

/// The exact bytes a post's signature covers — shared by `add` (signs
/// this) and `verify` (checks it) so the two can never drift apart.
/// Deliberately the same tab-joined shape the on-disk/wire format
/// already uses, just without the signature itself (which obviously
/// can't be part of what it signs).
fn signable_bytes(created_at: u64, visibility: Visibility, text: &str) -> Vec<u8> {
    format!("{created_at}\t{}\t{text}", visibility.as_str()).into_bytes()
}

/// Appends one post, signed with `identity`'s root key, creating
/// `path`'s parent directory as needed.
pub fn add(path: &Path, identity: &Identity, text: &str, visibility: Visibility) -> std::io::Result<()> {
    let text = &crate::text::message_text(text);
    if !is_valid_text(text) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("a post has to be 1 to {MAX_POST_BYTES} bytes"),
        ));
    }
    let created_at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let signature = identity.sign(&signable_bytes(created_at, visibility, text));
    let _guard = crate::store::lock();
    crate::store::append_line(path, &format!("{created_at}\t{}\t{}\t{}", visibility.as_str(), STANDARD.encode(signature), crate::text::to_field(text)))
}

/// Whether `post` was genuinely signed by whoever holds the private key
/// for `author_public_key` — the actual point of `Post::signature`
/// existing (see this module's docs). `author_public_key` has to come
/// from somewhere the caller already trusts (a peer-list entry, e.g.) —
/// this only checks the signature is valid *for that key*, not that the
/// key itself is who you think it is.
pub fn verify(post: &Post, author_public_key: &[u8; 32]) -> bool {
    is_valid_text(&post.text)
        && identity::verify(author_public_key, &signable_bytes(post.created_at, post.visibility, &post.text), &post.signature)
}
/// Every post ever made, oldest first, regardless of visibility — for
/// local use (e.g. the author reviewing their own full history).
pub fn all(path: &Path) -> std::io::Result<Vec<Post>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = crate::store::read(path)?;
    Ok(text.lines().filter_map(parse_line).collect())
}

/// The posts a requester is allowed to see: every `Public` one, plus
/// `Private` ones too when `include_private` is true. Deciding that
/// bool is the caller's job — `serve_connection`'s `on_list_posts`
/// callback in `swarm-cli` passes `true` only once it already knows
/// the requester's authenticated key matches something on the local
/// peers list, the same check `listen_forever`'s accept loop already
/// does to label an incoming connection by name.
pub fn visible(path: &Path, include_private: bool) -> std::io::Result<Vec<Post>> {
    Ok(all(path)?.into_iter().filter(|p| include_private || p.visibility == Visibility::Public).collect())
}

/// Takes in posts from another of this account's devices: those signed
/// by one of `authors` (the account's devices) that aren't here yet. The
/// file is kept in time order. Returns how many were new.
pub fn merge(path: &Path, fetched: &[Post], authors: &[[u8; 32]]) -> std::io::Result<usize> {
    let _guard = crate::store::lock();
    let mut mine = all(path)?;
    let deleted = deletions(path);
    let before = mine.len();
    for post in fetched {
        if !mine.iter().any(|p| p.signature == post.signature) && !deleted.iter().any(|d| d.post_signature == post.signature) && authors.iter().any(|a| verify(post, a)) {
            mine.push(post.clone());
        }
    }
    let added = mine.len() - before;
    if added > 0 {
        save(path, &mut mine)?;
    }
    Ok(added)
}

fn parse_line(line: &str) -> Option<Post> {
    let mut parts = line.splitn(4, '\t');
    let created_at = parts.next()?.parse().ok()?;
    let visibility = Visibility::parse(parts.next()?);
    let signature: [u8; 64] = STANDARD.decode(parts.next()?).ok()?.try_into().ok()?;
    let text = crate::text::from_field(parts.next()?);
    Some(Post { created_at, visibility, signature, text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use crate::identity::Identity;

    /// A post deleted goes, and its deletion is kept, signed, to pass on:
    /// another device takes it in (and won't take the post back), only
    /// from the author, and it's let go after 30 days.
    #[test]
    fn a_post_deleted_goes_everywhere_its_deletion_reaches() {
        let dir = std::env::temp_dir().join(format!("swarm-posts-test-delete-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (here, there) = (dir.join("here").join("posts"), dir.join("there").join("posts"));
        let me = Identity::generate();
        add(&here, &me, "keep this", Visibility::Public).unwrap();
        add(&here, &me, "regret this", Visibility::Private).unwrap();
        let both = all(&here).unwrap();
        merge(&there, &both, &[me.public_key_bytes()]).unwrap();
        let regret = both[1].signature;

        assert!(delete(&here, &me, &regret, 1_000).unwrap());
        assert!(!delete(&here, &me, &regret, 1_000).unwrap(), "once");
        assert_eq!(all(&here).unwrap().iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["keep this"]);
        let deleted = deletions_since(&here, 900, true);
        assert_eq!(deleted.len(), 1);
        assert!(deletions_since(&here, 900, false).is_empty(), "a private post's deletion isn't for strangers");
        assert!(verify_deletion(&deleted[0], &me.public_key_bytes()));
        assert!(!verify_deletion(&deleted[0], &Identity::generate().public_key_bytes()));
        assert!(deletions_since(&here, 1_000 + DELETION_SLACK + 1, true).is_empty(), "not to one who asked from after it");

        assert_eq!(take_deletions(&there, &deleted, &[Identity::generate().public_key_bytes()], 1_000).unwrap(), 0, "only the author's");
        assert_eq!(take_deletions(&there, &deleted, &[me.public_key_bytes()], 1_000).unwrap(), 1);
        assert_eq!(merge(&there, &both, &[me.public_key_bytes()]).unwrap(), 0, "not taken back from a device that hasn't heard");
        assert_eq!(all(&there).unwrap().len(), 1);
        assert_eq!(deletions(&there).len(), 1, "kept there to pass on too");
        assert!(!deletions(&there)[0].public, "and still not for strangers");

        assert!(!sweep_deletions(&here, 1_000 + DELETIONS_KEPT - 1).unwrap());
        assert!(sweep_deletions(&here, 1_000 + DELETIONS_KEPT).unwrap());
        assert!(deletions(&here).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_then_all_round_trips_visibility_and_order() {
        let dir = std::env::temp_dir().join(format!("swarm-posts-test-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("posts");
        let identity = Identity::generate();
        add(&path, &identity, "first, public", Visibility::Public).unwrap();
        add(&path, &identity, "second, private", Visibility::Private).unwrap();

        let posts = all(&path).unwrap();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].text, "first, public");
        assert_eq!(posts[0].visibility, Visibility::Public);
        assert_eq!(posts[1].text, "second, private");
        assert_eq!(posts[1].visibility, Visibility::Private);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn visible_hides_private_posts_unless_included() {
        let dir = std::env::temp_dir().join(format!("swarm-posts-test-visible-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("posts");
        let identity = Identity::generate();
        add(&path, &identity, "public one", Visibility::Public).unwrap();
        add(&path, &identity, "private one", Visibility::Private).unwrap();

        let stranger_view = visible(&path, false).unwrap();
        assert_eq!(stranger_view.len(), 1);
        assert_eq!(stranger_view[0].text, "public one");

        let peer_view = visible(&path, true).unwrap();
        assert_eq!(peer_view.len(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_genuine_post_verifies_against_its_real_authors_key() {
        let dir = std::env::temp_dir().join(format!("swarm-posts-test-verify-genuine-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("posts");
        let author = Identity::generate();
        add(&path, &author, "hello, signed world", Visibility::Public).unwrap();

        let post = &all(&path).unwrap()[0];
        assert!(verify(post, &author.public_key_bytes()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn verify_rejects_the_wrong_authors_key() {
        let dir = std::env::temp_dir().join(format!("swarm-posts-test-verify-wrong-key-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("posts");
        let author = Identity::generate();
        let impostor = Identity::generate();
        add(&path, &author, "actually mine", Visibility::Public).unwrap();

        let post = &all(&path).unwrap()[0];
        assert!(!verify(post, &impostor.public_key_bytes()));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A post relayed (or just corrupted) with its text, visibility, or
    /// timestamp altered after signing has to fail verification — this
    /// is the entire reason `signature` exists (see the module docs on
    /// trusting a post from someone other than its direct author).
    #[test]
    fn verify_rejects_any_tampered_field() {
        let dir = std::env::temp_dir().join(format!("swarm-posts-test-verify-tamper-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("posts");
        let author = Identity::generate();
        add(&path, &author, "the original text", Visibility::Public).unwrap();
        let genuine = all(&path).unwrap().into_iter().next().unwrap();
        let author_key = author.public_key_bytes();
        assert!(verify(&genuine, &author_key));

        let mut tampered_text = genuine.clone();
        tampered_text.text = "not what was signed".to_string();
        assert!(!verify(&tampered_text, &author_key));

        let mut tampered_visibility = genuine.clone();
        tampered_visibility.visibility = Visibility::Private;
        assert!(!verify(&tampered_visibility, &author_key));

        let mut tampered_time = genuine;
        tampered_time.created_at += 1;
        assert!(!verify(&tampered_time, &author_key));
        fs::remove_dir_all(&dir).unwrap();
    }
}
