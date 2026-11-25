//! Groups between whole nodes, in-process over loopback: someone added
//! learns of it from a contact and catches up from when they were added,
//! posts pass on from member to member (through someone in between for
//! two who aren't contacts), someone away hears when they're back, and
//! leaving, coming back, a name and a timer reach everyone.

use std::sync::Arc;
use std::time::Duration;
use swarm_core::channel::{self, ChannelId};
use swarm_core::identity::Identity;
use swarm_core::node::{self, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
}

impl TestNode {
    fn address(&self) -> String {
        format!("[::1]:{}", self.handle.local_addr().unwrap().port())
    }

    fn key(&self) -> [u8; 32] {
        self.identity.public_key_bytes()
    }

    fn texts(&self, channel: &ChannelId) -> Vec<String> {
        channel::load_posts(&self.paths.channels(), channel).unwrap_or_default().into_iter().map(|p| p.text).collect()
    }

    fn roster(&self, channel: &ChannelId) -> channel::Roster {
        channel::replay(channel, &channel::load_roster(&self.paths.channels(), channel).unwrap_or_default())
    }

    fn sees_in(&self, channel: &ChannelId, who: &TestNode) -> bool {
        self.roster(channel).is_member(&who.key())
    }

    fn in_it(&self, channel: &ChannelId) -> bool {
        self.sees_in(channel, self)
    }

    fn contact(&self, name: &str, of: &TestNode) {
        let peer = Peer { name: name.to_string(), address: of.address(), public_key: of.key(), cover_bits_per_second: None, cover_required: false };
        peers::add(&self.paths.peers(), &peer).unwrap();
        self.handle.add_peer_live(peer);
    }
}

async fn start(label: &str) -> TestNode {
    let dir = std::env::temp_dir().join(format!("swarm-groups-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths::new(&dir);
    let identity = Arc::new(Identity::generate());
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log: Arc::new(|_| {}) };
    let (handle, call_events) = node::start(identity.clone(), config).await.unwrap();
    drop(call_events);
    TestNode { handle, identity, paths }
}

/// `a` and `b` add each other as contacts.
async fn befriend(a: &TestNode, a_name: &str, b: &TestNode, b_name: &str) {
    a.contact(b_name, b);
    b.contact(a_name, a);
    wait_for("them to connect", || a.handle.peer_status(b_name).is_some_and(|s| s.online && !s.not_added) && b.handle.peer_status(a_name).is_some_and(|s| s.online && !s.not_added)).await;
}

async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..150 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Ann adds Bob, who adds Cat; Ann and Cat aren't contacts. Each hears
/// of it from whoever added them, and what each says reaches the others,
/// Ann's and Cat's through Bob.
#[tokio::test(flavor = "multi_thread")]
async fn someone_added_hears_of_it_and_posts_pass_through_someone_in_between() {
    let ann = start("ann").await;
    let bob = start("bob").await;
    let cat = start("cat").await;
    befriend(&ann, "ann", &bob, "bob").await;
    befriend(&bob, "bob", &cat, "cat").await;

    let (group, failed) = ann.handle.create_group(&["bob"]).unwrap();
    assert!(failed.is_empty(), "{failed:?}");
    wait_for("bob to be in it", || bob.in_it(&group)).await;
    assert!(ann.handle.add_to_group(&group, "cat").is_err(), "cat isn't ann's contact");
    bob.handle.add_to_group(&group, "cat").unwrap();
    wait_for("cat to be in it, and ann to know", || cat.in_it(&group) && ann.sees_in(&group, &cat)).await;
    assert!(cat.sees_in(&group, &ann), "cat knows who else is in it");

    ann.handle.post_to_channel(&group, "hello from ann").unwrap();
    wait_for("ann's post to reach cat through bob", || cat.texts(&group) == ["hello from ann"]).await;
    cat.handle.post_to_channel(&group, "hi ann, from cat").unwrap();
    wait_for("cat's post to reach ann", || ann.texts(&group).len() == 2).await;
    assert_eq!(bob.texts(&group).len(), 2);
    assert!(!ann.handle.is_connected(&cat.key()), "ann and cat never connect");
    assert!(bob.handle.add_to_group(&group, "ann").is_err(), "she's in already");
}

/// Someone added sees what's said from then on, nothing before.
#[tokio::test(flavor = "multi_thread")]
async fn history_starts_from_when_you_were_added() {
    let ann = start("ann-history").await;
    let bob = start("bob-history").await;
    befriend(&ann, "ann", &bob, "bob").await;
    let (group, _) = ann.handle.create_group(&[]).unwrap();
    ann.handle.post_to_channel(&group, "before bob").unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    ann.handle.add_to_group(&group, "bob").unwrap();
    wait_for("bob to be in it", || bob.in_it(&group)).await;
    ann.handle.post_to_channel(&group, "after").unwrap();
    wait_for("the later post to reach bob", || bob.texts(&group) == ["after"]).await;
    ann.handle.resync_channel(&group);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(bob.texts(&group), ["after"], "nothing from before, even catching up again");
}

/// Added while away: told, and caught up, once back.
#[tokio::test(flavor = "multi_thread")]
async fn someone_added_while_away_hears_when_they_are_back() {
    let ann = start("ann-away").await;
    let bob = start("bob-away").await;
    befriend(&ann, "ann", &bob, "bob").await;
    bob.handle.remove_peer_live("ann");
    wait_for("ann to see bob gone", || !ann.handle.peer_status("bob").is_some_and(|s| s.online)).await;
    let (group, failed) = ann.handle.create_group(&["bob"]).unwrap();
    assert!(failed.is_empty(), "{failed:?}");
    ann.handle.post_to_channel(&group, "for when you're back").unwrap();
    assert!(!channel::exists(&bob.paths.channels(), &group));

    let back = peers::load(&bob.paths.peers()).unwrap().into_iter().find(|p| p.name == "ann").unwrap();
    bob.handle.add_peer_live(back);
    wait_for("bob to be in it and caught up", || bob.in_it(&group) && bob.texts(&group) == ["for when you're back"]).await;
}

/// Leaving tells the others, takes what was said away, and nothing more
/// comes; added back, it's there again from then.
#[tokio::test(flavor = "multi_thread")]
async fn leaving_tells_the_others_and_anyone_can_add_you_back() {
    let ann = start("ann-leave").await;
    let bob = start("bob-leave").await;
    befriend(&ann, "ann", &bob, "bob").await;
    let (group, _) = ann.handle.create_group(&["bob"]).unwrap();
    wait_for("bob to be in it", || bob.in_it(&group)).await;
    ann.handle.post_to_channel(&group, "one").unwrap();
    wait_for("bob to have it", || bob.texts(&group) == ["one"]).await;

    bob.handle.leave_group(&group).unwrap();
    wait_for("ann to hear bob left", || !ann.sees_in(&group, &bob)).await;
    assert!(bob.texts(&group).is_empty(), "what was said goes from his device");
    assert!(bob.handle.post_to_channel(&group, "still here?").is_err());
    ann.handle.post_to_channel(&group, "bye bob").unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(bob.texts(&group).is_empty(), "nothing more reaches someone who left");

    tokio::time::sleep(Duration::from_millis(1100)).await;
    ann.handle.add_to_group(&group, "bob").unwrap();
    wait_for("bob to be back in it", || bob.in_it(&group)).await;
    ann.handle.post_to_channel(&group, "welcome back").unwrap();
    wait_for("only what's said from then", || bob.texts(&group) == ["welcome back"]).await;
    ann.handle.leave_group(&group).unwrap();
    wait_for("bob to hear ann left", || !bob.sees_in(&group, &ann)).await;
    assert!(bob.in_it(&group));
}

/// A name and a timer, set by anyone in it, reach everyone.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_and_a_timer_set_by_anyone_reach_everyone() {
    let ann = start("ann-name").await;
    let bob = start("bob-name").await;
    befriend(&ann, "ann", &bob, "bob").await;
    let (group, _) = ann.handle.create_group(&["bob"]).unwrap();
    wait_for("bob to be in it", || bob.in_it(&group)).await;
    bob.handle.rename_group(&group, "Lunch").unwrap();
    wait_for("ann to see the name", || ann.roster(&group).name.is_some_and(|n| n.text == "Lunch")).await;
    ann.handle.set_channel_timer(&group, 3600).unwrap();
    wait_for("bob to see the timer", || bob.roster(&group).timer() == 3600).await;
    assert!(ann.handle.rename_group(&group, "two\nlines").is_err());
}

/// Nobody hears of a group they're not in, and asking about one gets
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_contact_not_in_it_hears_nothing_of_it() {
    let ann = start("ann-private").await;
    let bob = start("bob-private").await;
    let cat = start("cat-private").await;
    befriend(&ann, "ann", &bob, "bob").await;
    befriend(&ann, "ann", &cat, "cat").await;
    let (group, _) = ann.handle.create_group(&["bob"]).unwrap();
    wait_for("bob to be in it", || bob.in_it(&group)).await;
    ann.handle.post_to_channel(&group, "just us").unwrap();
    wait_for("bob to have it", || bob.texts(&group).len() == 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!channel::exists(&cat.paths.channels(), &group), "cat knows nothing of it");
}

/// Ann and Cat, in a group through Bob but not contacts: Ann asks Cat to
/// add her. It reaches Cat through Bob, who can't read it, as an
/// ordinary contact request with Ann's address; Cat adds her, and Ann's
/// side adds Cat back by itself.
#[tokio::test(flavor = "multi_thread")]
async fn someone_seen_in_a_group_is_asked_to_add_you_and_added_back() {
    let ann = start("ann-ask").await;
    let bob = start("bob-ask").await;
    let cat = start("cat-ask").await;
    befriend(&ann, "ann", &bob, "bob").await;
    befriend(&bob, "bob", &cat, "cat").await;
    let (group, _) = ann.handle.create_group(&["bob"]).unwrap();
    wait_for("bob to be in it", || bob.in_it(&group)).await;
    bob.handle.add_to_group(&group, "cat").unwrap();
    wait_for("cat to be in it, and ann to know", || cat.in_it(&group) && ann.sees_in(&group, &cat)).await;
    assert!(ann.handle.ask_to_add(&group, &bob.key()).is_err(), "bob's her contact already");

    ann.handle.set_my_addresses(vec![ann.address()], Vec::new());
    ann.handle.ask_to_add(&group, &cat.key()).unwrap();
    let asking = || swarm_core::requests::pending(&cat.paths.requests()).unwrap().into_iter().find(|r| r.public_key == ann.key());
    wait_for("cat to see ann's request", || asking().is_some()).await;
    assert_eq!(asking().unwrap().address, ann.address());
    assert!(swarm_core::requests::pending(&bob.paths.requests()).unwrap().is_empty(), "nothing for bob");

    cat.contact("ann", &ann);
    wait_for("ann to have cat as a contact, and both connected", || {
        ann.handle.peer_status(&peers::handle_of(&cat.key())).is_some_and(|s| s.online && !s.not_added) && cat.handle.peer_status("ann").is_some_and(|s| s.online && !s.not_added)
    })
    .await;
    assert!(swarm_core::groupintro::asked(&ann.paths.group_asks(), swarm_core::unix_now()).is_empty(), "done with");
}
