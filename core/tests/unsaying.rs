//! Deleting lines of a conversation (`node::unsay`): a message the user
//! sent goes from both sides, on every device, once the other side has
//! heard; anything else, from the user's own devices only.

use std::sync::Arc;
use std::time::Duration;
use swarm_core::history::{self, Direction, LineRef};
use swarm_core::identity::Identity;
use swarm_core::node::{self, joining, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
}

fn dir(label: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("swarm-unsaying-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Paths::new(&dir)
}

async fn start_as(label: &str, identity: Arc<Identity>, paths: Paths) -> TestNode {
    let label = label.to_string();
    let log: node::Logger = if std::env::var_os("SWARM_TEST_LOG").is_some() { Arc::new(move |line| eprintln!("[{label}] {line}")) } else { Arc::new(|_| {}) };
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log };
    let (handle, _calls) = node::start(identity.clone(), config).await.unwrap();
    handle.set_my_addresses(vec![format!("[::1]:{}", handle.local_addr().unwrap().port())], Vec::new());
    TestNode { handle, identity, paths }
}

async fn start(label: &str) -> TestNode {
    start_as(label, Arc::new(Identity::generate()), dir(label)).await
}

/// Another device of `first`'s user, added with a code from it.
async fn second_device(first: &TestNode, label: &str) -> TestNode {
    let code = first.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, label).await.unwrap();
    let paths = dir(label);
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    start_as(label, Arc::new(joined.user), paths).await
}

/// `from` adds `to`'s user as a peer called `name`, at `to`'s address.
fn add(from: &TestNode, to: &TestNode, name: &str) {
    let peer = Peer {
        name: name.to_string(),
        address: format!("[::1]:{}", to.handle.local_addr().unwrap().port()),
        public_key: to.identity.public_key_bytes(),
        cover_bits_per_second: None,
        cover_required: false,
    };
    peers::add(&from.paths.peers(), &peer).unwrap();
    from.handle.add_peer_live(peer);
}

async fn wait_for(what: &str, check: impl FnMut() -> bool) {
    wait_up_to(200, what, check).await
}

/// `wait_for`, for up to `tenths` tenths of a second.
async fn wait_up_to(tenths: u32, what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..tenths {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

fn said(node: &TestNode, with: &[u8; 32]) -> Vec<String> {
    history::all(&node.paths.history(), with).unwrap_or_default().into_iter().map(|e| e.text).collect()
}

fn has(node: &TestNode, with: &[u8; 32], text: &str) -> bool {
    said(node, with).iter().any(|t| t == text)
}

/// The line of `node`'s chat with `with` that says `text`.
fn line(node: &TestNode, with: &[u8; 32], text: &str) -> LineRef {
    history::all(&node.paths.history(), with).unwrap().into_iter().find(|e| e.text == text).unwrap_or_else(|| panic!("no {text:?}")).line_ref()
}

/// Owen (desk and phone) and Alice (laptop and tablet). A message Owen
/// deleted goes from all four, his line too once hers have heard, and a
/// copy that comes after isn't kept; one without an id is found by its
/// time and text. One of hers Alice deletes goes from hers
/// only.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_deleted_goes_from_every_device_on_both_sides() {
    let desk = start("desk").await;
    let phone = second_device(&desk, "phone").await;
    let laptop = start("laptop").await;
    let tablet = second_device(&laptop, "tablet").await;
    let (owen, alice) = (desk.identity.public_key_bytes(), laptop.identity.public_key_bytes());
    history::append(&desk.paths.history(), &alice, Direction::Sent, "from long ago", 1_600_000_000).unwrap();
    history::append(&laptop.paths.history(), &owen, Direction::Received, "from long ago", 1_600_000_000).unwrap();
    add(&desk, &laptop, "alice");
    add(&laptop, &desk, "owen");
    wait_for("each to have the other on both devices", || phone.handle.knows("alice") && tablet.handle.knows("owen")).await;

    desk.handle.send_message("alice", "oops, wrong chat").await.unwrap();
    desk.handle.send_message("alice", "hello Alice").await.unwrap();
    laptop.handle.send_message("owen", "who's this?").await.unwrap();
    let everyone = [&desk, &phone, &laptop, &tablet];
    let chat = |node: &TestNode| if node.identity.public_key_bytes() == owen { alice } else { owen };
    let all_said = ["oops, wrong chat", "hello Alice", "who's this?"];
    wait_up_to(600, "all four to have everything", || everyone.iter().all(|n| all_said.iter().all(|t| has(n, &chat(n), t)))).await;

    let mine = line(&desk, &alice, "oops, wrong chat");
    assert!(mine.id.is_some(), "a message sent now has its id kept");
    desk.handle.delete_lines(alice, &[mine, line(&desk, &alice, "from long ago")]).unwrap();
    wait_for("Owen's message to go from Alice's devices", || [&laptop, &tablet].iter().all(|n| !has(n, &owen, "oops, wrong chat") && !has(n, &owen, "from long ago"))).await;
    wait_for("and then from his", || [&desk, &phone].iter().all(|n| !has(n, &alice, "oops, wrong chat") && !has(n, &alice, "from long ago"))).await;
    wait_for("nothing left to tell her", || [&desk, &phone].iter().all(|n| swarm_core::unsaying::all(&n.paths.unsaying(), &alice).unwrap().is_empty())).await;
    assert!(everyone.iter().all(|n| has(n, &chat(n), "who's this?") && has(n, &chat(n), "hello Alice")), "nothing else goes");

    laptop.handle.delete_lines(owen, &[line(&laptop, &owen, "hello Alice")]).unwrap();
    assert!(!has(&laptop, &owen, "hello Alice"), "at once, here");
    wait_for("and on her tablet", || !has(&tablet, &owen, "hello Alice")).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(has(&desk, &alice, "hello Alice") && has(&phone, &alice, "hello Alice"), "her deleting her copy is hers alone");
}

/// A message deleted before it reached Alice is never sent, and goes
/// from Owen's chat at once; she's told when she's next connected, in
/// case a copy got there after all, and never has it.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_deleted_before_it_reached_them_never_does() {
    let desk = start("desk2").await;
    let laptop = start("laptop2").await;
    let alice = laptop.identity.public_key_bytes();
    let nowhere = Peer { name: "alice".to_string(), address: "[::1]:9".to_string(), public_key: alice, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);

    let _ = desk.handle.send_message("alice", "hasty words").await;
    desk.handle.send_message("alice", "kind words").await.ok();
    let hasty = line(&desk, &alice, "hasty words");
    desk.handle.delete_lines(alice, &[hasty]).unwrap();
    let queued: Vec<String> = swarm_core::outbox::pending(&desk.paths.outbox(), &alice).unwrap().into_iter().map(|m| m.text).collect();
    assert_eq!(queued, ["kind words"], "no longer to be sent");
    assert!(!has(&desk, &alice, "hasty words"), "gone at once: nothing of it to wait for");
    assert_eq!(swarm_core::unsaying::all(&desk.paths.unsaying(), &alice).unwrap(), [hasty], "still to tell her");
    assert!(swarm_core::unsaying::shown(&desk.paths.unsaying(), &alice).unwrap().is_empty(), "but not in the chat");

    add(&laptop, &desk, "owen");
    let owen = desk.identity.public_key_bytes();
    wait_for("the other message to reach her", || has(&laptop, &owen, "kind words")).await;
    wait_for("her to have heard", || swarm_core::unsaying::all(&desk.paths.unsaying(), &alice).unwrap().is_empty()).await;
    assert!(!has(&laptop, &owen, "hasty words"), "she never had it");
}

/// A post Owen deletes goes from his other device as they next swap
/// posts, and from Alice's copy as she next looks (asking either of
/// them, once both have it); his others stay.
#[tokio::test(flavor = "multi_thread")]
async fn a_post_deleted_goes_from_the_users_devices_and_a_contacts_copy() {
    use swarm_core::node::PostsResult;
    use swarm_core::posts::{self, Visibility};
    let desk = start("desk3").await;
    let phone = second_device(&desk, "phone3").await;
    let laptop = start("laptop3").await;
    add(&desk, &laptop, "alice");
    add(&laptop, &desk, "owen");
    posts::add(&desk.paths.posts(), &desk.identity, "news for all", Visibility::Public).unwrap();
    posts::add(&desk.paths.posts(), &desk.identity, "a second thought", Visibility::Private).unwrap();
    let texts = |node: &TestNode| posts::all(&node.paths.posts()).unwrap().into_iter().map(|p| p.text).collect::<Vec<_>>();
    wait_up_to(450, "the phone to have both posts", || texts(&phone).len() == 2).await;
    wait_for("Alice to reach Owen", || laptop.handle.peer_status("owen").is_some_and(|s| s.online)).await;
    let fetched = |result: PostsResult| match result {
        PostsResult::Live(posts) => posts.into_iter().map(|p| p.text).collect::<Vec<_>>(),
        other => panic!("not live: {other:?}"),
    };
    assert_eq!(fetched(laptop.handle.fetch_posts("owen").await.unwrap()).len(), 2);

    let second = posts::all(&desk.paths.posts()).unwrap().into_iter().find(|p| p.text == "a second thought").unwrap().signature;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(posts::delete(&desk.paths.posts(), &desk.identity, &second, now).unwrap());
    wait_up_to(450, "gone from his phone", || texts(&phone) == ["news for all"]).await;
    assert_eq!(fetched(laptop.handle.fetch_posts("owen").await.unwrap()), ["news for all"], "gone from Alice's copy");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(texts(&desk), ["news for all"], "and not brought back from the phone");
}
