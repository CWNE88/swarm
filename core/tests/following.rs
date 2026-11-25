//! Following someone's public posts without being their peer: whole
//! nodes, one following the other.

use std::sync::Arc;
use std::time::Duration;
use swarm_core::identity::Identity;
use swarm_core::node::{self, NodeConfig, NodeHandle, Paths, PostsResult};
use swarm_core::posts::{self, Visibility};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
}

impl TestNode {
    fn key(&self) -> [u8; 32] {
        self.identity.public_key_bytes()
    }

    fn address(&self) -> String {
        format!("[::1]:{}", self.handle.local_addr().unwrap().port())
    }
}

async fn start(label: &str) -> TestNode {
    let dir = std::env::temp_dir().join(format!("swarm-following-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths::new(&dir);
    let identity = Arc::new(Identity::generate());
    let label = label.to_string();
    let log: node::Logger = if std::env::var_os("SWARM_TEST_LOG").is_some() { Arc::new(move |line| eprintln!("[{label}] {line}")) } else { Arc::new(|_| {}) };
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log };
    let (handle, _calls) = node::start(identity.clone(), config).await.unwrap();
    TestNode { handle, identity, paths }
}

async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_sees_public_posts_only_and_is_counted_not_asking() {
    let author = start("author").await;
    let reader = start("reader").await;
    posts::add(&author.paths.posts(), &author.identity, "for anyone", Visibility::Public).unwrap();
    posts::add(&author.paths.posts(), &author.identity, "for peers", Visibility::Private).unwrap();
    swarm_core::displayname::set(&author.paths.displayname(), &author.identity, "Ann").unwrap();

    reader.handle.follow(author.key(), &author.address()).unwrap();
    assert_eq!(reader.handle.following().len(), 1);
    let texts = match reader.handle.fetch_followed(&author.key()).await.unwrap() {
        PostsResult::Live(posts) => posts.into_iter().map(|p| p.text).collect::<Vec<_>>(),
        other => panic!("not live: {other:?}"),
    };
    assert_eq!(texts, vec!["for anyone".to_string()]);
    let name = swarm_core::displayname::cached_one(&reader.paths.displaynames(), &author.key()).unwrap().map(|d| d.name);
    assert_eq!(name.as_deref(), Some("Ann"));

    wait_for("the author to count a follower", || author.handle.followers() == 1).await;
    assert!(swarm_core::requests::pending(&author.paths.requests()).unwrap_or_default().is_empty());

    assert!(reader.handle.unfollow(&author.key()).unwrap());
    wait_for("the author to stop counting them", || author.handle.followers() == 0).await;
    assert!(reader.handle.following().is_empty());
    assert!(swarm_core::postcache::cached(&reader.paths.postcache(), &author.key()).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn you_cannot_follow_yourself_or_a_peer() {
    let me = start("me").await;
    let bob = start("bob").await;
    assert!(me.handle.follow(me.key(), "[::1]:1").is_err());
    let peer = swarm_core::peers::Peer { name: "bob".into(), address: bob.address(), public_key: bob.key(), cover_bits_per_second: None, cover_required: false };
    swarm_core::peers::add(&me.paths.peers(), &peer).unwrap();
    assert!(me.handle.follow(bob.key(), &bob.address()).is_err());
}

/// A post its author deletes goes from a follower's copy when they next
/// look; the rest stay.
#[tokio::test(flavor = "multi_thread")]
async fn a_post_deleted_goes_from_a_followers_copy() {
    let author = start("author-deletes").await;
    let reader = start("reader-deletes").await;
    posts::add(&author.paths.posts(), &author.identity, "keep this", Visibility::Public).unwrap();
    posts::add(&author.paths.posts(), &author.identity, "take this back", Visibility::Public).unwrap();
    reader.handle.follow(author.key(), &author.address()).unwrap();
    let texts = |result: PostsResult| match result {
        PostsResult::Live(posts) => posts.into_iter().map(|p| p.text).collect::<Vec<_>>(),
        other => panic!("not live: {other:?}"),
    };
    assert_eq!(texts(reader.handle.fetch_followed(&author.key()).await.unwrap()).len(), 2);

    let gone = posts::all(&author.paths.posts()).unwrap().into_iter().find(|p| p.text == "take this back").unwrap().signature;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(posts::delete(&author.paths.posts(), &author.identity, &gone, now).unwrap());
    assert_eq!(texts(reader.handle.fetch_followed(&author.key()).await.unwrap()), ["keep this"]);
    let cached: Vec<String> = swarm_core::postcache::cached(&reader.paths.postcache(), &author.key()).unwrap().into_iter().map(|p| p.text).collect();
    assert_eq!(cached, ["keep this"], "gone from the copy kept");
}
