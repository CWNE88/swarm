//! Local cache of another peer's posts, built up purely from what this
//! device has actually fetched (`swarm posts` / "view someone's
//! posts") — never pushed or gossiped. This is the survivability half
//! of posts: the author is still the only one who *originates* one,
//! but once a peer has pulled it, they keep their own copy, so it
//! isn't lost the moment the author goes offline or drops off the
//! network for good — only anything posted *after* the last fetch is.
//!
//! One flat file per author, named by their public key (see
//! `peerstore` for why never by name, same as `outbox`/`history`), same on-disk shape as `posts`'s own format —
//! including the author's signature on each line, not just visibility
//! and text — so a cached copy stays just as independently verifiable
//! as a freshly fetched one (see `posts`'s module docs on why that
//! signature exists at all). Merging a fresh fetch in is just "append
//! what's new."

use crate::posts::Post;
use std::fs;
use std::path::Path;

use crate::peerstore;

/// The most posts kept per author: the newest.
pub const MAX_CACHED: usize = 5000;

/// A post dated further ahead than this is taken to come from a clock
/// that was wrong.
pub const CLOCK_SLACK: u64 = 24 * 60 * 60;

/// Where the next fetch of `author`'s posts starts: the newest cached
/// post's time (0 for none). One dated in the future doesn't count — a
/// post made while the author's clock was years ahead would otherwise
/// keep every later post from ever being asked for.
pub fn newest(dir: &Path, author: &[u8; 32], now: u64) -> u64 {
    cached(dir, author).unwrap_or_default().iter().map(|p| p.created_at).filter(|at| *at <= now.saturating_add(CLOCK_SLACK)).max().unwrap_or(0)
}

/// Everything currently cached for `author`, oldest first. No cache yet
/// reads as empty, not an error.
pub fn cached(dir: &Path, author: &[u8; 32]) -> std::io::Result<Vec<Post>> {
    let path = peerstore::file_for(dir, author);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = crate::store::read(path)?;
    Ok(text.lines().filter_map(parse_line).collect())
}

/// Drops `author`'s cache (someone no longer followed).
pub fn forget(dir: &Path, author: &[u8; 32]) -> std::io::Result<()> {
    let _guard = crate::store::lock();
    match fs::remove_file(peerstore::file_for(dir, author)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Merges a freshly fetched set of posts into `author`'s cache,
/// skipping anything already stored (matched by the whole `Post` — it
/// has no separate id, so its full contents, signature included, are
/// the only identity it has) and keeping the result sorted oldest-
/// first, same order the wire format already uses.
pub fn merge(dir: &Path, author: &[u8; 32], fetched: &[Post]) -> std::io::Result<()> {
    merge_if(dir, author, fetched, || true).map(|_| ())
}

/// Drops the cached posts of `author`'s that `deleted` says they took
/// back (each checked against `authors`, the keys they sign with).
/// Whether any went.
pub fn drop_deleted(dir: &Path, author: &[u8; 32], deleted: &[crate::posts::Deletion], authors: &[[u8; 32]]) -> std::io::Result<bool> {
    let gone: Vec<&[u8; 64]> = deleted.iter().filter(|d| authors.iter().any(|a| crate::posts::verify_deletion(d, a))).map(|d| &d.post_signature).collect();
    if gone.is_empty() {
        return Ok(false);
    }
    let _guard = crate::store::lock();
    let all = cached(dir, author)?;
    let kept: Vec<&Post> = all.iter().filter(|p| !gone.contains(&&p.signature)).collect();
    if kept.len() == all.len() {
        return Ok(false);
    }
    crate::store::write_atomic(&peerstore::file_for(dir, author), kept.into_iter().map(format_line).collect::<String>())?;
    Ok(true)
}

/// `merge`, if `wanted` still says so once nothing else can change the
/// cache (`forget` included) — for a fetch that took a while, whose
/// author may have stopped being followed meanwhile. Whether it was.
pub fn merge_if(dir: &Path, author: &[u8; 32], fetched: &[Post], wanted: impl FnOnce() -> bool) -> std::io::Result<bool> {
    let _guard = crate::store::lock();
    if !wanted() {
        return Ok(false);
    }
    let mut all = cached(dir, author)?;
    let mut have: std::collections::HashSet<Post> = all.iter().cloned().collect();
    for post in fetched {
        if have.insert(post.clone()) {
            all.push(post.clone());
        }
    }
    all.sort_by_key(|post| post.created_at);
    if all.len() > MAX_CACHED {
        all.drain(..all.len() - MAX_CACHED);
    }
    let contents: String = all.iter().map(format_line).collect();
    crate::store::write_atomic(&peerstore::file_for(dir, author), contents)?;
    Ok(true)
}

fn format_line(post: &Post) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    format!("{}\t{}\t{}\t{}\n", post.created_at, post.visibility.as_str(), STANDARD.encode(post.signature), crate::text::to_field(&post.text))
}

fn parse_line(line: &str) -> Option<Post> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use crate::posts::Visibility;

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
    use crate::posts::Visibility;
    use std::path::PathBuf;

    const BOB: [u8; 32] = [1; 32];
    const NOBODY: [u8; 32] = [2; 32];

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("swarm-postcache-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// The exact signature bytes don't matter to this module — it never
    /// verifies, only stores and round-trips whatever it's handed (see
    /// `posts`'s own tests for signature *correctness*) — so a fixed
    /// dummy value distinguishable per call is all these tests need.
    fn post(created_at: u64, visibility: Visibility, text: &str) -> Post {
        Post { created_at, visibility, signature: [created_at as u8; 64], text: text.to_string() }
    }

    #[test]
    fn merge_then_cached_round_trips_and_dedupes() {
        let dir = scratch_dir("merge");
        merge(&dir, &BOB, &[post(100, Visibility::Public, "first"), post(200, Visibility::Private, "second")]).unwrap();
        merge(
            &dir,
            &BOB,
            &[post(100, Visibility::Public, "first"), post(200, Visibility::Private, "second"), post(300, Visibility::Public, "third")],
        )
        .unwrap();

        let all = cached(&dir, &BOB).unwrap();
        assert_eq!(all, vec![post(100, Visibility::Public, "first"), post(200, Visibility::Private, "second"), post(300, Visibility::Public, "third")]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_cache_keeps_the_newest_posts_up_to_its_limit() {
        let dir = scratch_dir("limit");
        let posts: Vec<Post> = (0..MAX_CACHED as u64 + 10).map(|i| post(i, Visibility::Public, "p")).collect();
        merge(&dir, &BOB, &posts).unwrap();
        let all = cached(&dir, &BOB).unwrap();
        assert_eq!(all.len(), MAX_CACHED);
        assert_eq!(all[0].created_at, 10, "the oldest went");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_post_from_a_clock_years_ahead_doesnt_hold_back_the_next_fetch() {
        let dir = scratch_dir("future");
        let now = 1_800_000_000;
        assert_eq!(newest(&dir, &BOB, now), 0);
        merge(&dir, &BOB, &[post(now - 50, Visibility::Public, "today"), post(now + 10 * 365 * 86400, Visibility::Public, "2066")]).unwrap();
        assert_eq!(newest(&dir, &BOB, now), now - 50);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cached_on_missing_file_is_empty_not_an_error() {
        let dir = scratch_dir("missing");
        assert_eq!(cached(&dir, &NOBODY).unwrap(), Vec::new());
    }
}
