//! Whole nodes, in-process, talking to each other over loopback through
//! the same `NodeHandle` API every front end uses.

use std::sync::Arc;
use std::time::Duration;
use swarm_core::identity::Identity;
use swarm_core::node::{self, Delivery, NodeConfig, NodeHandle, Paths, PostsResult};
use swarm_core::peers::{self, Peer};
use swarm_core::posts::{self, Visibility};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
}

async fn start(label: &str) -> TestNode {
    let dir = std::env::temp_dir().join(format!("swarm-node-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths::new(&dir);
    let identity = Arc::new(Identity::generate());
    start_as(identity, paths).await
}

/// A node with this identity and these files (again, after a restart),
/// on a new port — as if it had moved — telling peers where it is.
async fn start_as(identity: Arc<Identity>, paths: Paths) -> TestNode {
    let label = paths.dir().file_name().unwrap().to_string_lossy().split('-').nth(3).unwrap_or("?").to_string();
    let log: node::Logger = if std::env::var_os("SWARM_TEST_LOG").is_some() { Arc::new(move |line| eprintln!("[{label}] {line}")) } else { Arc::new(|_| {}) };
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log };
    let (handle, call_events) = node::start(identity.clone(), config).await.unwrap();
    drop(call_events);
    handle.set_my_addresses(vec![format!("[::1]:{}", handle.local_addr().unwrap().port())], Vec::new());
    TestNode { handle, identity, paths }
}

/// `from` adds `to` as a peer called `name`.
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

/// Both connected, each having added the other: waiting on one side
/// alone leaves the other still holding what it sends till it hears it's
/// been added, and two dialling at once may still settle on one link.
async fn both_up(a: &TestNode, b_as: &str, b: &TestNode, a_as: &str) {
    let up = |node: &TestNode, name: &str| node.handle.peer_status(name).is_some_and(|s| s.online && !s.not_added);
    wait_for("both to see each other online", || up(a, b_as) && up(b, a_as)).await;
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
async fn two_nodes_exchange_a_message_and_posts() {
    let alice = start("alice").await;
    let bob = start("bob").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("alice to see bob online", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;

    let mut messages = bob.handle.message_updates();
    assert_eq!(alice.handle.send_message("bob", "hi bob").await.unwrap(), Delivery::Delivered);
    tokio::time::timeout(Duration::from_secs(5), messages.changed()).await.expect("bob told of the message").unwrap();
    let history = swarm_core::history::all(&bob.paths.history(), &alice.identity.public_key_bytes()).unwrap();
    assert_eq!(history.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), vec!["hi bob"]);
    assert!(swarm_core::outbox::pending(&alice.paths.outbox(), &bob.identity.public_key_bytes()).unwrap().is_empty());

    posts::add(&bob.paths.posts(), &bob.identity, "bob's post", Visibility::Private).unwrap();
    match alice.handle.fetch_posts("bob").await.unwrap() {
        PostsResult::Live(posts) => assert_eq!(posts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), vec!["bob's post"]),
        PostsResult::Cached { reason, .. } => panic!("expected a live fetch, got the cache: {reason}"),
    }
}

/// Everything a node keeps is sealed: after a message, a post and a
/// name, no file under either node's folder shows any of them, or either
/// peer's name or key (in hex or base64) — in its contents or its name.
/// And with the other's keys, nothing opens.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_node_keeps_is_sealed_and_names_nobody() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let alice = start("alice-sealed").await;
    let bob = start("bob-sealed").await;
    add(&alice, &bob, "bobby");
    add(&bob, &alice, "alicia");
    both_up(&alice, "bobby", &bob, "alicia").await;
    assert_eq!(alice.handle.send_message("bobby", "the gate code is 4471").await.unwrap(), Delivery::Delivered);
    posts::add(&bob.paths.posts(), &bob.identity, "a private thought", Visibility::Private).unwrap();
    swarm_core::displayname::set(&alice.paths.displayname(), &alice.identity, "Alice Smith").unwrap();
    alice.handle.set_profile("walking the dog", &[]).unwrap();
    assert!(matches!(alice.handle.fetch_posts("bobby").await.unwrap(), PostsResult::Live(_)));
    let alice_key = alice.identity.public_key_bytes();
    wait_for("bob to have alice's status", || swarm_core::profile::cached(&bob.paths.profiles(), &alice_key).is_some()).await;
    let hex = |k: [u8; 32]| k.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let secrets = [
        "gate code".to_string(),
        "private thought".to_string(),
        "Alice Smith".to_string(),
        "walking the dog".to_string(),
        "bobby".to_string(),
        "alicia".to_string(),
        hex(alice.identity.public_key_bytes()),
        hex(bob.identity.public_key_bytes()),
        STANDARD.encode(alice.identity.public_key_bytes()),
        STANDARD.encode(bob.identity.public_key_bytes()),
    ];
    let mut files = 0;
    for node in [&alice, &bob] {
        let mut dirs = vec![node.paths.dir().to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                for secret in &secrets {
                    assert!(!name.contains(secret.as_str()), "{} names {secret}", path.display());
                }
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                files += 1;
                for secret in &secrets {
                    assert!(!text.contains(secret.as_str()), "{} holds {secret} readably", path.display());
                }
            }
        }
    }
    assert!(files >= 6, "only {files} files: was anything kept?");
    let history = swarm_core::history::all(&bob.paths.history(), &alice.identity.public_key_bytes()).unwrap();
    assert_eq!(history.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), vec!["the gate code is 4471"]);
    swarm_core::store::open_vault(bob.paths.dir(), &alice.identity.storage_keys());
    assert!(swarm_core::peers::load(&bob.paths.peers()).unwrap().is_empty());
    swarm_core::store::open_vault(bob.paths.dir(), &bob.identity.storage_keys());
}

/// A status and picture go to peers, and only to peers: someone who
/// hasn't been added gets nothing until they are; a change while
/// connected reaches them straight away.
/// What `profile` reads of a picture — a baseline JPEG's headers, 640
/// square — with filler for the rest: as big as one can be, so it goes
/// in many parts.
fn picture() -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8, 0xFF, 0xC0, 0, 17, 8, 2, 128, 2, 128, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 0xFF, 0xDA, 0, 2];
    j.resize(swarm_core::profile::MAX_AVATAR_BYTES - 2, 0x55);
    j.extend_from_slice(&[0xFF, 0xD9]);
    j
}

#[tokio::test(flavor = "multi_thread")]
async fn a_status_and_picture_reach_peers_and_nobody_else() {
    let alice = start("alice-profile").await;
    let bob = start("bob-profile").await;
    let carol = start("carol-profile").await;
    let avatar = picture();
    alice.handle.set_profile("at the beach", &avatar).unwrap();
    let alice_key = alice.identity.public_key_bytes();
    let theirs = |node: &TestNode| swarm_core::profile::cached(&node.paths.profiles(), &alice_key);

    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    add(&carol, &alice, "alice");
    wait_for("bob to have alice's profile", || theirs(&bob).is_some()).await;
    let got = theirs(&bob).unwrap();
    assert_eq!((got.status.as_str(), got.avatar.as_slice()), ("at the beach", avatar.as_slice()));
    wait_for("carol to learn alice hasn't added her", || carol.handle.peer_status("alice").is_some_and(|s| s.not_added)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(theirs(&carol), None, "a stranger gets nothing");

    alice.handle.set_profile("back home", &[]).unwrap();
    wait_for("bob to have alice's new status", || theirs(&bob).is_some_and(|p| p.status == "back home")).await;
    assert!(theirs(&bob).unwrap().avatar.is_empty());

    add(&alice, &carol, "carol");
    wait_for("carol to have alice's profile", || theirs(&carol).is_some_and(|p| p.status == "back home")).await;
}

/// Messages that delete themselves: alice sets the timer, bob hears who
/// set it; a message sent under it carries it to both sides and goes
/// from both when its time's up; one sent before keeps going for good,
/// and turning the timer off later doesn't keep the one sent under it.
#[tokio::test(flavor = "multi_thread")]
async fn a_timer_set_by_either_side_deletes_messages_on_both() {
    let alice = start("alice-timer").await;
    let bob = start("bob-timer").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    both_up(&alice, "bob", &bob, "alice").await;
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert_eq!(alice.handle.send_message("bob", "before the timer").await.unwrap(), Delivery::Delivered);
    alice.handle.set_timer("bob", 3600).unwrap();
    wait_for("bob to have alice's timer", || bob.handle.timer("alice").seconds == 3600).await;
    assert_eq!(bob.handle.timer("alice").by, alice.identity.public_key_bytes());
    assert!(bob.handle.device_news().iter().any(|n| n.contains("alice set your messages with them to delete themselves after an hour")), "bob's told who set it");
    assert_eq!(alice.handle.send_message("bob", "under the timer").await.unwrap(), Delivery::Delivered);
    bob.handle.set_timer("alice", 0).unwrap();
    wait_for("alice to have bob's timer off", || alice.handle.timer("bob").seconds == 0 && alice.handle.timer("bob").set_at > 0).await;
    assert_eq!(alice.handle.send_message("bob", "after it's off").await.unwrap(), Delivery::Delivered);
    let history = |node: &TestNode, of: &TestNode| swarm_core::history::all(&node.paths.history(), &of.identity.public_key_bytes()).unwrap();
    for (node, of) in [(&alice, &bob), (&bob, &alice)] {
        let all = history(node, of);
        assert_eq!(all.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["before the timer", "under the timer", "after it's off"]);
        assert_eq!(all[0].expires, 0);
        assert!(all[1].expires >= now() + 3590 && all[1].expires <= now() + 3600, "an hour from now: {}", all[1].expires);
        assert_eq!(all[2].expires, 0);
    }
    for (node, of) in [(&alice, &bob), (&bob, &alice)] {
        assert_eq!(swarm_core::history::sweep(&node.paths.history(), now() + 3601).unwrap(), 1);
        assert_eq!(history(node, of).iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["before the timer", "after it's off"]);
    }
}

/// A timer said to be set impossibly far ahead (a peer's clock gone wrong,
/// or worse) isn't taken, and one kept from before can't stop either side
/// setting theirs: it once overflowed, panicking (debug) or never winning
/// again (release).
#[tokio::test(flavor = "multi_thread")]
async fn a_timer_from_far_ahead_isnt_taken_or_kept_over_a_new_one() {
    use swarm_core::disappearing::{merge, Timer};
    let alice = start("alice-ahead").await;
    let bob = start("bob-ahead").await;
    let (a, b) = (alice.identity.public_key_bytes(), bob.identity.public_key_bytes());
    merge(&bob.paths.settings(), &a, Timer { seconds: 60, set_at: u64::MAX, by: b }).unwrap();
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("alice to see bob online", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(alice.handle.timer("bob").seconds, 0, "not taken");
    alice.handle.set_timer("bob", 3600).unwrap();
    wait_for("bob to have alice's timer", || bob.handle.timer("alice").seconds == 3600).await;
    assert_eq!(alice.handle.timer("bob").seconds, 3600);
}

/// `swarm ping` with a node running goes over the connection that's up,
/// not one of its own (which the other side would take as the peer
/// connection, knocking it over).
#[tokio::test(flavor = "multi_thread")]
async fn a_ping_goes_over_the_live_connection() {
    let alice = start("ping-alice").await;
    let bob = start("ping-bob").await;
    add(&alice, &bob, "bob");
    assert!(alice.handle.ping("bob").await.is_err(), "bob isn't connected yet");
    add(&bob, &alice, "alice");
    both_up(&alice, "bob", &bob, "alice").await;
    let since = alice.handle.peer_status("bob").unwrap().since;
    for _ in 0..3 {
        let rtt = alice.handle.ping("bob").await.unwrap();
        assert!(rtt < Duration::from_secs(1), "loopback rtt {rtt:?}");
    }
    assert!(alice.handle.ping("nobody").await.is_err());
    let status = alice.handle.peer_status("bob").unwrap();
    assert!(status.online);
    assert_eq!(status.since, since);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_to_someone_who_has_not_added_us_is_held_and_they_see_a_request() {
    let alice = start("alice2").await;
    let carol = start("carol").await;
    add(&carol, &alice, "alice");
    wait_for("carol to learn alice hasn't added her", || carol.handle.peer_status("alice").is_some_and(|s| s.not_added)).await;

    assert_eq!(carol.handle.send_message("alice", "hello?").await.unwrap(), Delivery::HeldUntilAdded);
    assert!(swarm_core::history::all(&alice.paths.history(), &carol.identity.public_key_bytes()).unwrap().is_empty());
    let requested = || swarm_core::requests::pending(&alice.paths.requests()).unwrap().iter().map(|r| r.public_key).collect::<Vec<_>>();
    wait_for("alice to see carol's request", || requested() == vec![carol.identity.public_key_bytes()]).await;

    add(&alice, &carol, "carol");
    let carol_key = carol.identity.public_key_bytes();
    wait_for("the held message to arrive", || {
        !swarm_core::history::all(&alice.paths.history(), &carol_key).unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(swarm_core::history::all(&alice.paths.history(), &carol_key).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_contact_deleted_leaves_nothing_is_told_so_and_hears_when_added_back() {
    let alice = start("alice-removed").await;
    let bob = start("bob-removed").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("bob to see alice online", || bob.handle.peer_status("alice").is_some_and(|s| s.online)).await;
    bob.handle.send_message("alice", "hello").await.unwrap();
    let bob_key = bob.identity.public_key_bytes();
    wait_for("alice to have bob's message", || !swarm_core::history::all(&alice.paths.history(), &bob_key).unwrap().is_empty()).await;

    peers::remove(&alice.paths.peers(), "bob").unwrap();
    alice.handle.forget_contact("bob", bob_key, false);
    assert!(swarm_core::history::all(&alice.paths.history(), &bob_key).unwrap().is_empty(), "the chat's gone");
    assert!(swarm_core::requests::all(&alice.paths.requests()).unwrap().is_empty(), "nothing kept of him");
    let book = std::fs::read_to_string(alice.paths.dir().join("peersbook")).unwrap_or_default();
    assert!(book.lines().all(|l| l.trim().is_empty()), "nor in the book, with no other devices: {book:?}");
    wait_for("bob to be told alice removed him", || bob.handle.peer_status("alice").is_some_and(|s| s.removed_us && !s.online)).await;
    assert!(swarm_core::settings::removed_us(&bob.paths.settings(), &alice.identity.public_key_bytes()));
    assert!(swarm_core::requests::pending(&alice.paths.requests()).unwrap().is_empty(), "no request from him");
    let err = bob.handle.send_message("alice", "why?").await.unwrap_err().to_string();
    assert!(err.contains(node::REMOVED_US), "{err}");

    add(&alice, &bob, "bob");
    wait_for("bob to see alice again", || bob.handle.peer_status("alice").is_some_and(|s| s.online && !s.removed_us)).await;
    wait_for("bob's settings to say so", || !swarm_core::settings::removed_us(&bob.paths.settings(), &alice.identity.public_key_bytes())).await;

    peers::remove(&alice.paths.peers(), "bob").unwrap();
    alice.handle.forget_contact("bob", bob_key, true);
    let left = swarm_core::requests::all(&alice.paths.requests()).unwrap();
    assert_eq!(left.iter().map(|r| (r.public_key, r.status)).collect::<Vec<_>>(), vec![(bob_key, swarm_core::requests::Status::Blocked)]);
    wait_for("bob to be told again", || bob.handle.peer_status("alice").is_some_and(|s| s.removed_us && !s.online)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nodes_reach_each_other_over_ipv4_past_an_address_that_does_not_answer() {
    let alice = start("alice-v4").await;
    let bob = start("bob-v4").await;
    let port = |n: &TestNode| n.handle.local_addr().unwrap().port();
    let peer = |name: &str, address: String, to: &TestNode| Peer {
        name: name.to_string(),
        address,
        public_key: to.identity.public_key_bytes(),
        cover_bits_per_second: None,
        cover_required: false,
    };
    let for_bob = peer("bob", format!("[::1]:9,127.0.0.1:{}", port(&bob)), &bob);
    peers::add(&alice.paths.peers(), &for_bob).unwrap();
    alice.handle.add_peer_live(for_bob);
    let for_alice = peer("alice", format!("127.0.0.1:{}", port(&alice)), &alice);
    peers::add(&bob.paths.peers(), &for_alice).unwrap();
    bob.handle.add_peer_live(for_alice);
    wait_for("alice to see bob online", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;
    assert_eq!(alice.handle.send_message("bob", "over IPv4").await.unwrap(), Delivery::Delivered);
    let seen: Vec<_> = alice.handle.seen_as().into_iter().map(|(_, a)| a.ip()).collect();
    assert_eq!(seen, vec![std::net::IpAddr::from([127, 0, 0, 1])]);
}

fn transfer_state(node: &TestNode, id: &swarm_core::transfer::TransferId) -> swarm_core::transfer::State {
    node.handle.transfers().into_iter().find(|t| t.transfer.id == *id).expect("on record").transfer.state
}

async fn offered(node: &TestNode) -> swarm_core::transfer::TransferId {
    for _ in 0..100 {
        if let Some(t) = node.handle.transfers().into_iter().find(|t| t.transfer.direction == swarm_core::transfer::Direction::In) {
            return t.transfer.id;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no offer arrived");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_offered_and_accepted_arrives_whole_and_a_declined_one_doesnt() {
    use swarm_core::transfer::State;
    let alice = start("alice3").await;
    let bob = start("bob3").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("connection", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;

    let source = alice.paths.dir().join("photo.jpg");
    let data: Vec<u8> = (0..50_000u32).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(&source, &data).unwrap();
    let out_id = alice.handle.offer_file("bob", &source).await.unwrap();
    let in_id = offered(&bob).await;
    assert_eq!(in_id, out_id);
    assert_eq!(transfer_state(&bob, &in_id), State::Offered, "nothing moves until accepted");

    let downloads = bob.paths.dir().join("downloads");
    bob.handle.accept_transfer(&in_id, &downloads).unwrap();
    wait_for("the file to arrive", || transfer_state(&bob, &in_id) == State::Done).await;
    assert_eq!(std::fs::read(downloads.join("photo.jpg")).unwrap(), data);
    wait_for("the sender to hear it's done", || transfer_state(&alice, &out_id) == State::Done).await;

    let second = alice.handle.offer_file("bob", &source).await.unwrap();
    wait_for("the second offer", || bob.handle.transfers().iter().any(|t| t.transfer.id == second)).await;
    bob.handle.decline_transfer(&second).unwrap();
    wait_for("the sender to hear it's declined", || transfer_state(&alice, &second) == State::Declined).await;
    assert!(!downloads.join("photo (1).jpg").exists());
}

/// Files going each way at once, over the one connection (on the phone, a
/// screenshot sent to the desktop while a PDF came from it): both arrive
/// whole.
#[tokio::test(flavor = "multi_thread")]
async fn files_each_way_at_once_both_arrive_whole() {
    use swarm_core::transfer::{Direction, State};
    let alice = start("alice-both").await;
    let bob = start("bob-both").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    both_up(&alice, "bob", &bob, "alice").await;

    let made = |node: &TestNode, name: &str, size: u32, step: u32| {
        let path = node.paths.dir().join(name);
        let data: Vec<u8> = (0..size).map(|i| (i.wrapping_mul(step) >> 7) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        (path, data)
    };
    let (shot, shot_data) = made(&alice, "screenshot.png", 1_247_525, 2_654_435_761);
    let (pdf, pdf_data) = made(&bob, "4kwebcam.pdf", 334_616, 40_503);
    let to_bob = alice.handle.offer_file("bob", &shot).await.unwrap();
    let to_alice = bob.handle.offer_file("alice", &pdf).await.unwrap();
    let incoming = |node: &TestNode, id| node.handle.transfers().iter().any(|t| t.transfer.id == id && t.transfer.direction == Direction::In);
    wait_for("both offers", || incoming(&bob, to_bob) && incoming(&alice, to_alice)).await;

    let (bob_saves, alice_saves) = (bob.paths.dir().join("downloads"), alice.paths.dir().join("downloads"));
    bob.handle.accept_transfer(&to_bob, &bob_saves).unwrap();
    alice.handle.accept_transfer(&to_alice, &alice_saves).unwrap();
    let over = |s: State| matches!(s, State::Done | State::Failed | State::Declined | State::Cancelled);
    wait_for("both to finish", || over(transfer_state(&bob, &to_bob)) && over(transfer_state(&alice, &to_alice))).await;
    let note = |node: &TestNode, id| node.handle.transfers().into_iter().find(|t| t.transfer.id == id).unwrap().transfer.note;
    assert_eq!(transfer_state(&bob, &to_bob), State::Done, "the screenshot: {}", note(&bob, to_bob));
    assert_eq!(transfer_state(&alice, &to_alice), State::Done, "the PDF: {}", note(&alice, to_alice));
    assert!(std::fs::read(bob_saves.join("screenshot.png")).unwrap() == shot_data);
    assert!(std::fs::read(alice_saves.join("4kwebcam.pdf")).unwrap() == pdf_data);
}

#[tokio::test(flavor = "multi_thread")]
async fn files_from_a_peer_set_to_be_received_arrive_without_an_answer() {
    use swarm_core::transfer::State;
    let alice = start("alice5").await;
    let bob = start("bob5").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    let inbox = bob.paths.dir().join("inbox");
    swarm_core::settings::set_receive_to(&bob.paths.settings(), &alice.identity.public_key_bytes(), Some(&inbox)).unwrap();
    wait_for("connection", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;

    let source = alice.paths.dir().join("backup.tar");
    let data: Vec<u8> = (0..40_000u32).map(|i| (i * 7 % 253) as u8).collect();
    std::fs::write(&source, &data).unwrap();
    let id = alice.handle.offer_file("bob", &source).await.unwrap();
    wait_for("the file to arrive unasked", || bob.handle.transfers().iter().any(|t| t.transfer.id == id && t.transfer.state == State::Done)).await;
    assert_eq!(std::fs::read(inbox.join("backup.tar")).unwrap(), data);

    swarm_core::settings::set_receive_to(&bob.paths.settings(), &alice.identity.public_key_bytes(), None).unwrap();
    let second = alice.handle.offer_file("bob", &source).await.unwrap();
    wait_for("the second offer", || bob.handle.transfers().iter().any(|t| t.transfer.id == second)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(transfer_state(&bob, &second), State::Offered);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transfer_cut_off_part_way_carries_on_from_where_it_stopped() {
    use swarm_core::transfer::{self, State};
    let alice = start("alice4").await;
    let bob = start("bob4").await;
    let mut to_bob = Peer {
        name: "bob".into(),
        address: format!("[::1]:{}", bob.handle.local_addr().unwrap().port()),
        public_key: bob.identity.public_key_bytes(),
        cover_bits_per_second: Some(64_000),
        cover_required: false,
    };
    peers::add(&alice.paths.peers(), &to_bob).unwrap();
    alice.handle.add_peer_live(to_bob.clone());
    add(&bob, &alice, "alice");
    wait_for("connection", || {
        bob.handle.peer_status("alice").is_some_and(|s| s.online && !s.not_added) && alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)
    })
    .await;

    let source = alice.paths.dir().join("big.bin");
    let data: Vec<u8> = (0..40_000u32).map(|i| (i * 13 % 253) as u8).collect();
    std::fs::write(&source, &data).unwrap();
    alice.handle.offer_file("bob", &source).await.unwrap();
    let id = offered(&bob).await;
    let downloads = bob.paths.dir().join("downloads");
    bob.handle.accept_transfer(&id, &downloads).unwrap();
    wait_for("some of it", || bob.handle.transfers().iter().any(|t| t.transfer.id == id && t.done_bytes > 4000)).await;

    bob.handle.remove_peer_live("alice");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t = transfer::get(&bob.paths.transfers(), &id).unwrap();
    let partial = transfer::received(&t);
    assert!(partial > 0 && partial < data.len() as u64, "part way: {partial}");

    to_bob.cover_bits_per_second = None;
    let alice_entry = peers::load(&bob.paths.peers()).unwrap().into_iter().find(|p| p.name == "alice").unwrap();
    bob.handle.add_peer_live(alice_entry);
    wait_for("the rest", || transfer_state(&bob, &id) == State::Done).await;
    assert_eq!(std::fs::read(downloads.join("big.bin")).unwrap(), data);
}

#[tokio::test(flavor = "multi_thread")]
async fn peers_who_move_one_at_a_time_keep_finding_each_other() {
    let alice = start("alice6").await;
    let bob = start("bob6").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("connection", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;

    bob.handle.shutdown().await;
    let bob = start_as(bob.identity.clone(), bob.paths.clone()).await;
    let bob_now = format!("[::1]:{}", bob.handle.local_addr().unwrap().port());
    wait_for("alice to learn where bob is now", || alice.handle.learned_addresses(&bob.identity.public_key_bytes()).first() == Some(&bob_now)).await;

    alice.handle.shutdown().await;
    let alice = start_as(alice.identity.clone(), alice.paths.clone()).await;
    wait_for("alice to reach bob again", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;
    let alice_now = format!("[::1]:{}", alice.handle.local_addr().unwrap().port());
    wait_for("bob to learn where alice is now", || bob.handle.learned_addresses(&alice.identity.public_key_bytes()).first() == Some(&alice_now)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn peers_who_both_moved_find_each_other_through_a_peer_they_share() {
    let alice = start("alice7").await;
    let bob = start("bob7").await;
    let carol = start("carol7").await;
    for (a, b, name) in [(&alice, &bob, "bob"), (&bob, &alice, "alice"), (&alice, &carol, "carol"), (&carol, &alice, "alice"), (&bob, &carol, "carol"), (&carol, &bob, "bob")] {
        add(a, b, name);
    }
    wait_for("alice and bob to connect", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;
    swarm_core::lastseen::set(&alice.paths.lastseen(), &bob.identity.public_key_bytes(), 1).unwrap();
    swarm_core::lastseen::set(&bob.paths.lastseen(), &alice.identity.public_key_bytes(), 1).unwrap();

    alice.handle.shutdown().await;
    bob.handle.shutdown().await;
    for node in [&alice, &bob] {
        let _ = std::fs::remove_dir_all(node.paths.addresses());
    }
    let alice = start_as(alice.identity.clone(), alice.paths.clone()).await;
    let bob = start_as(bob.identity.clone(), bob.paths.clone()).await;
    wait_for("both to reach carol", || {
        alice.handle.peer_status("carol").is_some_and(|s| s.online && !s.not_added)
            && bob.handle.peer_status("carol").is_some_and(|s| s.online && !s.not_added)
            && carol.handle.peer_status("alice").is_some_and(|s| s.online && !s.not_added)
            && carol.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)
    })
    .await;
    assert!(!alice.handle.peer_status("bob").is_some_and(|s| s.online));

    assert_eq!(alice.handle.ask_around_now(), 1);
    wait_for("bob to find alice", || bob.handle.peer_status("alice").is_some_and(|s| s.online && !s.not_added)).await;
    wait_for("alice to see bob", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;
}

/// Two peers starting at the same moment dial each other at once; one
/// of the two connections is kept. Neither should ever see the other go
/// offline for it.
#[tokio::test(flavor = "multi_thread")]
async fn peers_starting_together_never_see_each_other_go_offline() {
    for round in 0..8 {
        let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let mut nodes = Vec::new();
        for who in ["a", "b"] {
            let dir = std::env::temp_dir().join(format!("swarm-node-test-together{round}{who}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let paths = Paths::new(&dir);
            let identity = Arc::new(Identity::generate());
            let seen = lines.clone();
            let log: node::Logger = Arc::new(move |line| seen.lock().unwrap().push(format!("[{who}] {line}")));
            let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log };
            let (handle, call_events) = node::start(identity.clone(), config).await.unwrap();
            drop(call_events);
            nodes.push(TestNode { handle, identity, paths });
        }
        let peer = |to: &TestNode, name: &str| Peer {
            name: name.to_string(),
            address: format!("[::1]:{}", to.handle.local_addr().unwrap().port()),
            public_key: to.identity.public_key_bytes(),
            cover_bits_per_second: None,
            cover_required: false,
        };
        let (a_to_b, b_to_a) = (peer(&nodes[1], "b"), peer(&nodes[0], "a"));
        peers::add(&nodes[0].paths.peers(), &a_to_b).unwrap();
        peers::add(&nodes[1].paths.peers(), &b_to_a).unwrap();
        nodes[0].handle.add_peer_live(a_to_b);
        nodes[1].handle.add_peer_live(b_to_a);
        wait_for("both online", || nodes[0].handle.peer_status("b").is_some_and(|s| s.online && !s.not_added) && nodes[1].handle.peer_status("a").is_some_and(|s| s.online && !s.not_added)).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let lines = lines.lock().unwrap().clone();
        assert!(!lines.iter().any(|l| l.contains("offline")), "round {round}:\n{}", lines.join("\n"));
        for node in nodes {
            node.handle.shutdown().await;
        }
    }
}

/// Whoever we offer a file can ask for any part of it; asking far past
/// its end gets nothing, and mustn't count as progress (the TUI drew a
/// bar from it, and a huge enough one brought it down).
#[tokio::test(flavor = "multi_thread")]
async fn asking_for_a_file_far_past_its_end_doesnt_count_as_progress() {
    use swarm_core::protocol::{self, Incoming, Reply, Request};
    let alice = start("alice8").await;
    let bob = Arc::new(Identity::generate());
    let bob_endpoint = swarm_core::transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let to_bob = Peer {
        name: "bob".into(),
        address: format!("[::1]:{}", bob_endpoint.local_addr().unwrap().port()),
        public_key: bob.public_key_bytes(),
        cover_bits_per_second: None,
        cover_required: false,
    };
    peers::add(&alice.paths.peers(), &to_bob).unwrap();
    alice.handle.add_peer_live(to_bob);
    let alice_addr: std::net::SocketAddr = format!("[::1]:{}", alice.handle.local_addr().unwrap().port()).parse().unwrap();
    let mut conn = swarm_core::transport::dial(&bob_endpoint, alice_addr, swarm_core::transport::Keys::new_device(&bob), alice.identity.public_key_bytes()).await.unwrap();
    wait_for("alice to take bob's connection", || alice.handle.peer_status("bob").is_some_and(|s| s.online)).await;

    let source = alice.paths.dir().join("small.txt");
    std::fs::write(&source, "five!").unwrap();
    let id = alice.handle.offer_file("bob", &source).await.unwrap();
    for offset in [1_000_000, 1u64 << 40, 1u64 << 50, u64::MAX - 2] {
        let asked = protocol::request(&mut conn, Request::FileChunk { id, offset, len: 4000 }).await.unwrap();
        loop {
            match protocol::recv(&mut conn).await.unwrap() {
                Incoming::Reply(rid, Reply::FileData(data)) if rid == asked => {
                    assert!(data.is_empty());
                    break;
                }
                Incoming::Reply(rid, Reply::FileError(_)) if rid == asked => break,
                Incoming::Reply(rid, other) if rid == asked => panic!("unexpected {other:?}"),
                _ => {}
            }
        }
    }
    let status = alice.handle.transfers().into_iter().find(|t| t.transfer.id == id).unwrap();
    assert!(status.done_bytes <= status.transfer.size, "{} of {} bytes", status.done_bytes, status.transfer.size);
}

/// When each new QUIC connection (an Initial with a connection ID not
/// seen before) arrives at a socket that never answers.
fn dials_seen(socket: std::net::UdpSocket) -> Arc<std::sync::Mutex<Vec<std::time::Instant>>> {
    let seen: Arc<std::sync::Mutex<Vec<std::time::Instant>>> = Arc::default();
    let record = seen.clone();
    std::thread::spawn(move || {
        let mut ids = std::collections::HashSet::new();
        let mut buf = [0u8; 2048];
        while let Ok(n) = socket.recv(&mut buf) {
            if n < 6 || buf[0] & 0x80 == 0 || n < 6 + buf[5] as usize {
                continue;
            }
            if ids.insert(buf[6..6 + buf[5] as usize].to_vec()) {
                record.lock().unwrap().push(std::time::Instant::now());
            }
        }
    });
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_never_answers_is_tried_less_and_less_often_and_at_once_after_a_move() {
    let alice = start("alice-backoff").await;
    let nobody = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let address = format!("[::1]:{}", nobody.local_addr().unwrap().port());
    let seen = dials_seen(nobody);
    let ghost = Peer {
        name: "ghost".to_string(),
        address,
        public_key: Identity::generate().public_key_bytes(),
        cover_bits_per_second: None,
        cover_required: false,
    };
    peers::add(&alice.paths.peers(), &ghost).unwrap();
    alice.handle.add_peer_live(ghost);

    let started = std::time::Instant::now();
    while seen.lock().unwrap().len() < 4 && started.elapsed() < Duration::from_secs(30) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let times = seen.lock().unwrap().clone();
    assert_eq!(times.len(), 4, "dials seen: {}", times.len());
    let gaps: Vec<f64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs_f64()).collect();
    for (gap, want) in gaps.iter().zip([4.0, 5.0, 7.0]) {
        assert!((gap - want).abs() < 0.6, "gaps between dials {gaps:.1?}, wanted about 4, 5, 7");
    }

    tokio::time::sleep(Duration::from_millis(3500)).await;
    let moved = std::time::Instant::now();
    alice.handle.set_my_addresses(vec!["[::1]:1".to_string()], Vec::new());
    while seen.lock().unwrap().len() < 5 && moved.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let after = seen.lock().unwrap().get(4).map(|t| (*t - moved).as_secs_f64());
    assert!(after.is_some_and(|s| s < 1.0), "tried again {after:?} s after this device moved");
}

/// Each side tells the connection between them as it runs: its own
/// address and port, the other's, and which of them dialled (one, not
/// both) — and none before there's one.
#[tokio::test(flavor = "multi_thread")]
async fn each_side_tells_the_connection_in_use() {
    use node::InUse;
    let alice = start("alice-inuse").await;
    let bob = start("bob-inuse").await;
    let (alice_key, bob_key) = (alice.identity.public_key_bytes(), bob.identity.public_key_bytes());
    assert_eq!(alice.handle.connection_to(&bob_key), None);
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("alice to see bob online", || alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;
    wait_for("bob to see alice online", || bob.handle.peer_status("alice").is_some_and(|s| s.online && !s.not_added)).await;
    let at = |n: &TestNode| std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, n.handle.local_addr().unwrap().port()));
    let dialled = |from: &TestNode, to: &TestNode, key: &[u8; 32]| match from.handle.connection_to(key) {
        Some(InUse::Direct { here, port, there, dialled }) if here == Some(std::net::Ipv6Addr::LOCALHOST.into()) && port == at(from).port() && there == at(to) => Some(dialled),
        _ => None,
    };
    wait_for("each side to tell the one connection, dialled by one of them", || {
        matches!((dialled(&alice, &bob, &bob_key), dialled(&bob, &alice, &alice_key)), (Some(a), Some(b)) if a != b)
    })
    .await;
}

/// A contact request that's gone out can be taken back: it goes from
/// their list, and they can be asked again.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_request_takes_it_off_their_list() {
    let alice = start("alice-cancel").await;
    let carol = start("carol-cancel").await;
    add(&carol, &alice, "alice");
    let asking = || swarm_core::requests::pending(&alice.paths.requests()).unwrap().len();
    wait_for("alice to see carol's request", || asking() == 1).await;
    wait_for("carol to know it's out", || carol.handle.peer_status("alice").is_some_and(|s| s.asked)).await;

    peers::remove(&carol.paths.peers(), "alice").unwrap();
    carol.handle.forget_contact("alice", alice.identity.public_key_bytes(), false);
    wait_for("the request to go", || asking() == 0).await;
    wait_for("carol to be done telling her", || carol.handle.withdrawals_waiting().is_empty()).await;

    add(&carol, &alice, "alice");
    wait_for("asking again to show as new", || asking() == 1).await;
}

/// Someone away when a request is cancelled hears when they're back, on
/// the same address, though they're no longer a contact.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_cancelled_while_they_are_away_goes_when_they_are_back() {
    let alice = start("alice-away").await;
    let carol = start("carol-away").await;
    add(&carol, &alice, "alice");
    let requests = alice.paths.requests();
    wait_for("alice to see carol's request", || swarm_core::requests::pending(&requests).unwrap().len() == 1).await;
    wait_for("carol to know it's out", || carol.handle.peer_status("alice").is_some_and(|s| s.asked)).await;
    alice.handle.shutdown().await;

    peers::remove(&carol.paths.peers(), "alice").unwrap();
    carol.handle.forget_contact("alice", alice.identity.public_key_bytes(), false);
    wait_for("carol's try to fail", || carol.handle.peer_status("alice").is_none()).await;
    assert_eq!(carol.handle.withdrawals_waiting().len(), 1);
    assert_eq!(swarm_core::requests::pending(&requests).unwrap().len(), 1, "she hasn't heard yet");

    let back = start_as(alice.identity.clone(), alice.paths.clone()).await;
    let address = format!("[::1]:{}", back.handle.local_addr().unwrap().port());
    swarm_core::withdrawals::add(&carol.paths.withdrawals(), swarm_core::withdrawals::What::ContactRequest, alice.identity.public_key_bytes(), &address, swarm_core::unix_now()).unwrap();
    wait_for("carol to reach her", || {
        carol.handle.try_withdrawals_now();
        swarm_core::requests::pending(&requests).unwrap().is_empty()
    })
    .await;
    wait_for("carol to be done", || carol.handle.withdrawals_waiting().is_empty()).await;
}

/// Adding them again before they've heard: there's nothing to cancel any
/// more, and the new request must stand.
#[tokio::test(flavor = "multi_thread")]
async fn adding_them_again_keeps_the_new_request() {
    let alice = start("alice-again").await;
    let carol = start("carol-again").await;
    add(&carol, &alice, "alice");
    wait_for("carol to know it's out", || carol.handle.peer_status("alice").is_some_and(|s| s.asked)).await;
    alice.handle.shutdown().await;
    peers::remove(&carol.paths.peers(), "alice").unwrap();
    carol.handle.forget_contact("alice", alice.identity.public_key_bytes(), false);
    assert_eq!(carol.handle.withdrawals_waiting().len(), 1);

    let peer = Peer { name: "alice".to_string(), address: "[::1]:9".to_string(), public_key: alice.identity.public_key_bytes(), cover_bits_per_second: None, cover_required: false };
    peers::add(&carol.paths.peers(), &peer).unwrap();
    carol.handle.add_peer_live(peer);
    assert!(carol.handle.withdrawals_waiting().is_empty());
}
