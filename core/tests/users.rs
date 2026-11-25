//! A user on several devices, as a peer of
//! theirs sees them: one peer, reached on every device.

use std::sync::Arc;
use std::time::Duration;
use swarm_core::identity::Identity;
use swarm_core::node::{self, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
    calls: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>>>,
}

fn dir(label: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("swarm-users-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Paths::new(&dir)
}

/// A device of `identity`'s (the user's key), with its own folder.
async fn start_as(label: &str, identity: Arc<Identity>, paths: Paths) -> TestNode {
    let label = label.to_string();
    let log: node::Logger = if std::env::var_os("SWARM_TEST_LOG").is_some() { Arc::new(move |line| eprintln!("[{label}] {line}")) } else { Arc::new(|_| {}) };
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log };
    let (handle, call_events) = node::start(identity.clone(), config).await.unwrap();
    handle.set_my_addresses(vec![format!("[::1]:{}", handle.local_addr().unwrap().port())], Vec::new());
    TestNode { handle, identity, paths, calls: std::sync::Mutex::new(Some(call_events)) }
}

async fn start(label: &str) -> TestNode {
    start_as(label, Arc::new(Identity::generate()), dir(label)).await
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

async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..150 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

fn said(node: &TestNode, with: &[u8; 32]) -> Vec<String> {
    swarm_core::history::all(&node.paths.history(), with).unwrap_or_default().into_iter().map(|e| e.text).collect()
}

/// Both online, and each of `node`'s entries for `name` connected.
fn all_online(node: &TestNode, name: &str, devices: usize) -> bool {
    let entries = node.handle.same_person(name);
    entries.len() == devices && entries.iter().all(|n| node.handle.peer_status(n).is_some_and(|s| s.online))
}

/// Alice adds Owen by his desktop's address alone; his phone, holding
/// the same user key, dials her too. She has one Owen, reached on both,
/// with one chat: what she sends reaches both, and what either sends is
/// in it. Started again, she knows both devices at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_reaches_both_of_a_users_devices_as_one() {
    let alice = start("alice").await;
    let owen = Arc::new(Identity::generate());
    let desk = start_as("desk", owen.clone(), dir("desk")).await;
    let phone = start_as("phone", owen.clone(), dir("phone")).await;
    assert_ne!(desk.handle.device().public_key_bytes(), phone.handle.device().public_key_bytes(), "a key of its own each");
    add(&alice, &desk, "owen");
    add(&desk, &alice, "alice");
    add(&phone, &alice, "alice");
    wait_for("alice to reach both of owen's devices", || all_online(&alice, "owen", 2)).await;
    assert!(alice.handle.peer_status("owen").is_some_and(|s| s.online));
    assert_eq!(peers::load(&alice.paths.peers()).unwrap().len(), 1, "one owen on her list");

    let alice_key = alice.identity.public_key_bytes();
    let paul_key = owen.public_key_bytes();
    alice.handle.send_message("owen", "hello both").await.unwrap();
    wait_for("both devices to have it", || said(&desk, &alice_key).contains(&"hello both".to_string()) && said(&phone, &alice_key).contains(&"hello both".to_string())).await;
    desk.handle.send_message("alice", "from the desk").await.unwrap();
    phone.handle.send_message("alice", "from the phone").await.unwrap();
    wait_for("one chat with both in it", || {
        let all = said(&alice, &paul_key);
        all.contains(&"from the desk".to_string()) && all.contains(&"from the phone".to_string())
    })
    .await;
    assert_eq!(said(&alice, &paul_key).iter().filter(|t| *t == "hello both").count(), 1, "what she sent, once");

    let (alice_identity, alice_paths) = (alice.identity.clone(), alice.paths.clone());
    alice.handle.shutdown().await;
    let alice = start_as("alice", alice_identity, alice_paths).await;
    assert_eq!(alice.handle.same_person("owen").len(), 2);
    wait_for("alice to reach both again", || all_online(&alice, "owen", 2)).await;
}

/// Owen's desk and then his phone connect to Alice; the phone goes. His
/// desk's connection is still the one in use, not "not connected": each
/// device's connection is his, not only the newest.
#[tokio::test(flavor = "multi_thread")]
async fn a_users_older_connection_is_in_use_when_their_newer_one_goes() {
    use swarm_core::node::InUse;
    let alice = start("alice-older").await;
    let owen = Arc::new(Identity::generate());
    let paul_key = owen.public_key_bytes();
    let desk = start_as("desk-older", owen.clone(), dir("desk-older")).await;
    add(&alice, &desk, "owen");
    add(&desk, &alice, "alice");
    wait_for("alice to reach the desk", || all_online(&alice, "owen", 1)).await;
    let phone = start_as("phone-older", owen.clone(), dir("phone-older")).await;
    add(&phone, &alice, "alice");
    wait_for("alice to reach the phone too", || all_online(&alice, "owen", 2)).await;
    let desk_port = desk.handle.local_addr().unwrap().port();
    phone.handle.shutdown().await;
    let online = |n: &TestNode| n.handle.same_person("owen").iter().filter(|e| n.handle.peer_status(e).is_some_and(|s| s.online)).count();
    wait_for("alice to see the phone gone", || online(&alice) == 1).await;
    match alice.handle.connection_to(&paul_key) {
        Some(InUse::Direct { there, .. }) => assert_eq!(there.port(), desk_port, "the desk's"),
        other => panic!("in use: {other:?}"),
    }
}

/// Alice rings each of Owen's devices' entries: each rings its own
/// device, both of them, not one twice.
#[tokio::test(flavor = "multi_thread")]
async fn each_of_a_users_devices_is_rung_on_its_own_connection() {
    use swarm_core::call::{self, CallLink};
    let alice = start("alice-rings").await;
    let owen = Arc::new(Identity::generate());
    let desk = start_as("desk-rings", owen.clone(), dir("desk-rings")).await;
    let phone = start_as("phone-rings", owen.clone(), dir("phone-rings")).await;
    add(&alice, &desk, "owen");
    add(&desk, &alice, "alice");
    add(&phone, &alice, "alice");
    wait_for("alice to reach both of owen's devices", || all_online(&alice, "owen", 2)).await;
    for entry in alice.handle.same_person("owen") {
        let handle = alice.handle.call_handle(&entry).unwrap();
        let mut link = CallLink::attach(handle.commands, handle.budget).unwrap();
        tokio::spawn(async move {
            let _ = call::ring(&mut link, false).await;
            link
        });
    }
    for (device, label) in [(&desk, "the desktop"), (&phone, "the phone")] {
        let mut rings = device.calls.lock().unwrap().take().unwrap();
        match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await {
            Ok(Some(node::CallEvent::Incoming { .. })) => {}
            _ => panic!("{label} wasn't rung"),
        }
    }
}

/// Taken off her list, Owen's devices' entries go with him.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_removed_takes_their_devices_with_them() {
    let alice = start("alice2").await;
    let owen = Arc::new(Identity::generate());
    let desk = start_as("desk2", owen.clone(), dir("desk2")).await;
    add(&alice, &desk, "owen");
    add(&desk, &alice, "alice");
    wait_for("alice to reach owen's desktop", || all_online(&alice, "owen", 1)).await;
    peers::remove(&alice.paths.peers(), "owen").unwrap();
    alice.handle.remove_peer_live("owen");
    assert!(alice.handle.peer_status("owen").is_none());
    assert_eq!(alice.handle.same_person("owen"), vec!["owen".to_string()], "nothing left of him");
}

/// Owen's desktop gives out a code; his phone uses it, and is given his
/// user key and the list with both on it. The code works once.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_is_added_with_a_code_from_another() {
    use swarm_core::node::joining;
    let desk = start("desk3").await;
    let paul_key = desk.identity.public_key_bytes();
    assert_eq!(desk.handle.devices().devices.len(), 1, "the first device lists itself");
    let code = desk.handle.add_device_code().unwrap();
    let code = joining::JoinCode::parse(&code.line()).unwrap();
    assert_eq!(code.user, paul_key);

    let device = swarm_core::identity::Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    assert_eq!(joined.user.public_key_bytes(), paul_key, "his user key");
    let again = joining::join(&code, &swarm_core::identity::Identity::generate(), "thief").await;
    assert!(again.is_err(), "a code works once");

    let paths = dir("phone3");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone3", Arc::new(joined.user), paths).await;
    assert_eq!(phone.handle.device().public_key_bytes(), device.public_key_bytes(), "the key it joined with");
    for (node, this) in [(&desk, "desk"), (&phone, "phone")] {
        let view = node.handle.devices();
        assert_eq!(view.account, paul_key);
        assert_eq!(view.devices.len(), 2, "{this} has both");
        assert!(view.devices.iter().any(|d| d.name == "phone" && d.key == device.public_key_bytes()));
    }
    assert!(phone.handle.rename_this_device("pixel").unwrap());
    assert!(phone.handle.devices().devices.iter().any(|d| d.this && d.name == "pixel"));
}

/// The 12 words make the user key on a new device, which puts itself on
/// the list.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_set_up_from_the_words_lists_itself() {
    let owen = Identity::generate();
    let words = owen.recovery_words().join(" ");
    let laptop = start_as("laptop4", Arc::new(Identity::from_words(&words).unwrap()), dir("laptop4")).await;
    let view = laptop.handle.devices();
    assert_eq!(view.account, owen.public_key_bytes());
    assert_eq!(view.devices.len(), 1);
    assert!(view.devices[0].this && !view.devices[0].name.is_empty());
}

/// Owen's phone, added with his desktop's code, links to it: each lists
/// the other as online, the phone is given his peers and chats, a message
/// the desktop sends afterwards is copied to it, and a rename on either
/// reaches the other.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_added_keeps_in_step_with_the_other() {
    use swarm_core::node::joining;
    let alice = start("alice5").await;
    let desk = start("desk5").await;
    let alice_key = alice.identity.public_key_bytes();
    add(&desk, &alice, "alice");
    add(&alice, &desk, "owen");
    wait_for("owen's desktop to reach alice", || all_online(&desk, "alice", 1)).await;
    desk.handle.send_message("alice", "before the phone").await.unwrap();

    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone5");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone5", Arc::new(joined.user), paths).await;
    let both_online = |node: &TestNode| {
        let view = node.handle.devices();
        view.devices.len() == 2 && view.devices.iter().all(|d| d.online)
    };
    wait_for("the desktop and phone to link", || both_online(&desk) && both_online(&phone)).await;
    let link = |from: &TestNode, to: &TestNode| match from.handle.connection_to(&to.handle.device().public_key_bytes()) {
        Some(node::InUse::Direct { dialled, .. }) => dialled,
        other => panic!("no direct link: {other:?}"),
    };
    assert_ne!(link(&desk, &phone), link(&phone, &desk), "one dialled");
    wait_for("the phone to have alice", || peers::load(&phone.paths.peers()).unwrap().iter().any(|p| p.public_key == alice_key)).await;
    wait_for("the phone to have the chat", || said(&phone, &alice_key).contains(&"before the phone".to_string())).await;

    desk.handle.send_message("alice", "after the phone").await.unwrap();
    wait_for("the phone to have the copy", || said(&phone, &alice_key).contains(&"after the phone".to_string())).await;
    assert_eq!(said(&phone, &alice_key).iter().filter(|t| *t == "after the phone").count(), 1, "kept once");

    assert!(phone.handle.rename_this_device("pixel").unwrap());
    wait_for("the desktop to hear of the rename", || desk.handle.devices().devices.iter().any(|d| !d.this && d.name == "pixel")).await;
}

/// A message Owen sends from his desktop while Alice can't be reached
/// waits, on the desktop and on his phone, which has its copy; once she
/// has it, it's delivered on both.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_waits_on_every_device_until_one_of_theirs_has_it() {
    use swarm_core::node::joining;
    let alice = start("alice40").await;
    let desk = start("desk40").await;
    let alice_key = alice.identity.public_key_bytes();
    let nowhere = Peer { name: "alice".to_string(), address: "[::1]:9".to_string(), public_key: alice_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);

    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone40");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone40", Arc::new(joined.user), paths).await;
    wait_for("the phone to have alice", || phone.handle.knows("alice")).await;

    desk.handle.send_message("alice", "are you there?").await.unwrap();
    let waiting = |node: &TestNode| swarm_core::waiting::all(&node.paths.waiting(), &alice_key).unwrap().into_iter().map(|w| w.text).collect::<Vec<_>>();
    assert_eq!(waiting(&desk), ["are you there?"]);
    assert_eq!(swarm_core::outbox::pending(&desk.paths.outbox(), &alice_key).unwrap().len(), 1, "in the outbox");
    wait_for("the phone to have the copy, waiting", || waiting(&phone) == ["are you there?"]).await;

    add(&alice, &desk, "owen");
    wait_for("alice to have it", || said(&alice, &desk.identity.public_key_bytes()).contains(&"are you there?".to_string())).await;
    wait_for("delivered on the desktop", || waiting(&desk).is_empty()).await;
    wait_for("delivered on the phone", || waiting(&phone).is_empty()).await;
}

/// A call that nobody answered is noted once in the caller's chat, at
/// the time it started ringing, however many of the user's devices note
/// it, and counts as unread; one from another of the user's own devices
/// isn't noted.
#[tokio::test(flavor = "multi_thread")]
async fn a_missed_call_is_noted_once_in_their_chat() {
    let alice = start("alice41").await;
    let desk = start("desk41").await;
    let alice_key = alice.identity.public_key_bytes();
    add(&desk, &alice, "alice");
    desk.handle.note_missed_call("alice", false, 1_700_000_000);
    desk.handle.note_missed_call("alice", false, 1_700_000_000);
    desk.handle.note_missed_call("#device AAAA", false, 1_700_000_100);
    let all = swarm_core::history::all(&desk.paths.history(), &alice_key).unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!((all[0].direction, all[0].at, all[0].text.as_str()), (swarm_core::history::Direction::Missed, 1_700_000_000, "call"));
    let summary = swarm_core::history::summary(&desk.paths.history(), &alice_key).unwrap();
    assert_eq!((summary.received, summary.last_received), (1, 1_700_000_000));
}

/// Owen's desktop and phone have both moved, so neither knows where the
/// other is now: an address typed for the desktop on the phone links them
/// (a host name, looked up as it's dialled), and it can't be for this
/// device. Set again, it replaces what was typed; cleared, it's gone.
#[tokio::test(flavor = "multi_thread")]
async fn an_address_typed_for_a_device_links_to_it() {
    use swarm_core::node::joining;
    let desk = start("desk30").await;
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone30");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let _ = std::fs::remove_dir_all(paths.addresses());
    let phone = start_as("phone30", Arc::new(joined.user), paths).await;
    let desk_key = desk.handle.device().public_key_bytes();
    let linked = |node: &TestNode| {
        let view = node.handle.devices();
        view.devices.len() == 2 && view.devices.iter().all(|d| d.online)
    };
    let typed = |node: &TestNode| node.handle.devices().devices.into_iter().find(|d| d.key == desk_key).map(|d| d.typed).unwrap_or_default();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!linked(&phone), "nowhere to dial yet");

    assert!(phone.handle.set_device_address(desk_key, "not an address!").is_err(), "not an address");
    assert!(phone.handle.set_device_address(phone.handle.device().public_key_bytes(), "::1").is_err(), "not this device");
    assert!(phone.handle.set_device_address([7; 32], "::1").is_err(), "not one of theirs");

    let kept = phone.handle.set_device_address(desk_key, "2001:db8::1, swarm.example.org").unwrap();
    assert_eq!(kept, ["2001:db8::1", "swarm.example.org"]);
    let port = desk.handle.local_addr().unwrap().port();
    let kept = phone.handle.set_device_address(desk_key, &format!("localhost:{port}")).unwrap();
    assert_eq!(kept, [format!("localhost:{port}")]);
    assert_eq!(typed(&phone), format!("localhost:{port}"), "in place of what was typed before");
    wait_for("the phone and desktop to link", || linked(&phone) && linked(&desk)).await;
    assert_eq!(phone.handle.set_device_address(desk_key, " , ").unwrap(), Vec::<String>::new());
    assert_eq!(typed(&phone), "", "cleared");
}

/// Tim added Owen's desktop and dials it; the address Owen has for Tim
/// is no good, so the desktop knows where Tim is only from what Tim's
/// device told it (kept under that device's key). Owen's phone, added
/// afterwards, is given those addresses with Tim, reaches him itself,
/// and Tim then has both of Owen's devices, so a call rings both.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_added_reaches_a_peer_only_known_from_where_they_said() {
    use swarm_core::node::joining;
    let tim = start("tim7").await;
    let desk = start("desk7").await;
    let tim_key = tim.identity.public_key_bytes();
    let wrong = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &wrong).unwrap();
    desk.handle.add_peer_live(wrong);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;
    wait_for("the desktop to be told where tim is", || desk.handle.told_addresses(&tim_key).is_some()).await;

    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone7");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone7", Arc::new(joined.user), paths).await;
    wait_for("the phone to have tim", || peers::load(&phone.paths.peers()).unwrap().iter().any(|p| p.public_key == tim_key)).await;
    wait_for("the phone to reach tim", || all_online(&phone, "tim", 1)).await;
    wait_for("tim to have both of owen's devices", || all_online(&tim, "owen", 2)).await;
}

/// Tim can be reached by nobody but Owen's desktop (see `node::proxy`):
/// the phone, which can't reach Tim, has him online through the
/// desktop, which is let relay; its message goes to the desktop, which delivers it as Owen's,
/// and is out of the phone's outbox; Tim's reply reaches the phone as a
/// copy from the desktop. Tim never has the phone. Relaying turned off,
/// he's offline on the phone.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_sends_through_another_that_reaches_a_peer() {
    use swarm_core::node::InUse;
    let tim = start("tim20").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let desk = start("desk20").await;
    let (tim_key, paul_key) = (tim.identity.public_key_bytes(), desk.identity.public_key_bytes());
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;

    let phone = join_named_with_no_address(&desk, "phone", "phone20").await;
    wait_for("the phone to have tim", || phone.handle.knows("tim")).await;
    assert!(!phone.handle.peer_status("tim").unwrap().online, "not reached before the desktop's let relay");
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;
    assert!(matches!(phone.handle.connection_to(&tim_key), Some(InUse::Through(_))), "{:?}", phone.handle.connection_to(&tim_key));

    assert_eq!(phone.handle.send_message("tim", "from the phone").await.unwrap(), swarm_core::node::Delivery::Relayed);
    wait_for("tim to have it", || said(&tim, &paul_key).contains(&"from the phone".to_string())).await;
    wait_for("it out of the phone's outbox", || swarm_core::outbox::pending(&phone.paths.outbox(), &tim_key).unwrap().is_empty()).await;
    wait_for("the phone to know tim has it", || swarm_core::waiting::all(&phone.paths.waiting(), &tim_key).unwrap().is_empty()).await;
    tim.handle.send_message("owen", "to the desktop").await.unwrap();
    wait_for("the phone to have it", || said(&phone, &tim_key).contains(&"to the desktop".to_string())).await;
    assert_eq!(said(&tim, &paul_key).iter().filter(|t| *t == "from the phone").count(), 1, "once");
    assert!(all_online(&tim, "owen", 1), "tim has only the desktop");

    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), false).unwrap());
    wait_for("tim offline on the phone", || phone.handle.peer_status("tim").is_some_and(|s| !s.online)).await;
}

/// Tim knows only where Owen's desktop is, and Owen's phone can't reach
/// him; but Tim can reach the phone. With nothing relaying, Tim has only
/// the desktop. With the desktop let relay, it tells Tim where Owen's
/// other devices are, and Tim connects to the phone
/// directly: it's online on the phone over a connection of its own, not
/// through the desktop.
#[tokio::test(flavor = "multi_thread")]
async fn a_relaying_device_tells_a_contact_where_the_others_are_and_they_connect_directly() {
    use swarm_core::node::{joining, InUse};
    let tim = start("tim32").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let desk = start("desk32").await;
    let tim_key = tim.identity.public_key_bytes();
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;

    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone32");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone32", Arc::new(joined.user), paths).await;
    wait_for("the phone to have tim", || phone.handle.knows("tim")).await;
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert!(all_online(&tim, "owen", 1), "nothing relays: tim has only the desktop");
    assert!(!phone.handle.peer_status("tim").unwrap().online);

    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for_long("tim to have both of owen's devices", || all_online(&tim, "owen", 2)).await;
    wait_for("tim online on the phone, directly", || matches!(phone.handle.connection_to(&tim_key), Some(InUse::Direct { .. }))).await;
}

/// As above, for what isn't a message: Tim's status and picture come to
/// the phone from the desktop, and the phone can add him to a group it's
/// only just made: the desktop, told of it, passes it on to him.
#[tokio::test(flavor = "multi_thread")]
async fn a_picture_and_a_group_go_through_another_device() {
    use swarm_core::channel;
    let tim = start("tim-invite").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let mut avatar = vec![0xFF, 0xD8, 0xFF, 0xC0, 0, 17, 8, 2, 128, 2, 128, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, 0xFF, 0xDA, 0, 2];
    avatar.resize(swarm_core::profile::MAX_AVATAR_BYTES - 2, 0x55);
    avatar.extend_from_slice(&[0xFF, 0xD9]);
    tim.handle.set_profile("at the beach", &avatar).unwrap();
    let desk = start("desk-invite").await;
    let tim_key = tim.identity.public_key_bytes();
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;

    let phone = join_named_with_no_address(&desk, "phone", "phone-invite").await;
    wait_for("the phone to have tim", || phone.handle.knows("tim")).await;
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;

    wait_for_long("tim's picture on the phone", || {
        swarm_core::profile::cached(&phone.paths.profiles(), &tim_key).is_some_and(|p| p.status == "at the beach" && p.avatar == avatar)
    })
    .await;

    let (group, failed) = phone.handle.create_group(&["tim"]).unwrap();
    assert!(failed.is_empty(), "{failed:?}");
    let in_it = |paths: &swarm_core::node::Paths, key: &[u8; 32]| channel::replay(&group, &channel::load_roster(&paths.channels(), &group).unwrap_or_default()).is_member(key);
    wait_for_long("tim to be in it", || in_it(&tim.paths, &tim_key)).await;
    phone.handle.post_to_channel(&group, "from the phone").unwrap();
    wait_for_long("the post to reach tim", || channel::load_posts(&tim.paths.channels(), &group).unwrap_or_default().len() == 1).await;
}

/// A chat read on one device is read on the other, whichever it was read
/// on; the later mark stands.
#[tokio::test(flavor = "multi_thread")]
async fn how_far_a_chat_is_read_is_the_same_on_every_device() {
    use swarm_core::node::joining;
    use swarm_core::readmarks::Chat;
    let desk = start("desk-read").await;
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone-read");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone-read", Arc::new(joined.user), paths).await;

    let bob = Chat::Contact([7; 32]);
    let general = Chat::Channel(swarm_core::channel::ChannelId { name: "general".into(), owner: [8; 32] });
    let told = desk.handle.message_updates();
    phone.handle.read_up_to(bob.clone(), 1000);
    wait_for_long("the desktop to have the phone's mark", || desk.handle.read_marks().get(&bob) == Some(&1000)).await;
    assert!(told.has_changed().unwrap(), "so that it recounts");
    desk.handle.read_up_to(general.clone(), 2000);
    desk.handle.read_up_to(bob.clone(), 1100);
    wait_for_long("the phone to have the desktop's", || phone.handle.read_marks().get(&general) == Some(&2000) && phone.handle.read_marks().get(&bob) == Some(&1100)).await;
    phone.handle.read_up_to(bob.clone(), 900);
    assert_eq!(phone.handle.read_marks().get(&bob), Some(&1100), "never back");
    desk.handle.read_up_to(bob.clone(), u64::MAX);
    assert!(desk.handle.read_marks()[&bob] < u64::MAX / 2, "no further ahead than a wrong clock can be");
}

/// A request cancelled on one device reaches them from whichever device
/// had made it, and a finished line says so on every device.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_cancelled_on_one_device_goes_from_the_other() {
    let desk = start("desk-cancel").await;
    let phone = join_from(&desk, "phone-cancel").await;
    let alice = start("alice-cancel").await;
    add(&desk, &alice, "alice");
    let requests = alice.paths.requests();
    let asking = || swarm_core::requests::pending(&requests).unwrap().len();
    wait_for_long("alice to see the request", || asking() == 1).await;
    wait_for_long("the phone to have her too", || peers::load(&phone.paths.peers()).unwrap().iter().any(|p| p.name == "alice")).await;

    peers::remove(&phone.paths.peers(), "alice").unwrap();
    phone.handle.forget_contact("alice", alice.identity.public_key_bytes(), false);
    wait_for_long("the desktop to delete her", || desk.handle.peer_status("alice").is_none()).await;
    wait_for_long("the request to go", || asking() == 0).await;
    wait_for_long("everyone to be done", || desk.handle.withdrawals_waiting().is_empty() && phone.handle.withdrawals_waiting().is_empty()).await;
}

/// A group made on one device is on the other, and leaving it on one
/// leaves it on both.
#[tokio::test(flavor = "multi_thread")]
async fn a_group_is_on_every_device_and_leaving_it_on_one_leaves_it_on_all() {
    let desk = start("desk-group").await;
    let phone = join_from(&desk, "phone-group").await;
    let bob = start("bob-group").await;
    add(&desk, &bob, "bob");
    add(&bob, &desk, "owen");
    wait_for_long("the desktop to reach bob", || desk.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added)).await;
    let me = desk.identity.public_key_bytes();
    let (group, _) = desk.handle.create_group(&["bob"]).unwrap();
    let roster = |paths: &swarm_core::node::Paths| swarm_core::channel::replay(&group, &swarm_core::channel::load_roster(&paths.channels(), &group).unwrap_or_default());
    wait_for_long("the phone to be in it", || roster(&phone.paths).is_member(&me)).await;
    bob.handle.post_to_channel(&group, "hi owen").unwrap();
    wait_for_long("the phone to have bob's post", || swarm_core::channel::load_posts(&phone.paths.channels(), &group).unwrap_or_default().len() == 1).await;

    phone.handle.leave_group(&group).unwrap();
    wait_for_long("the desktop to have left too", || !roster(&desk.paths).is_member(&me)).await;
    wait_for_long("and bob to know", || !roster(&bob.paths).is_member(&me)).await;
    assert!(swarm_core::channel::load_posts(&desk.paths.channels(), &group).unwrap_or_default().is_empty(), "what was said goes from both");
}

/// Tim's picture, on Owen's desktop, comes to a phone added after Tim's
/// gone offline, with no device let relay.
#[tokio::test(flavor = "multi_thread")]
async fn a_picture_kept_on_one_device_comes_to_another_with_the_contact_offline() {
    use swarm_core::node::joining;
    let tim = start("tim-offline").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    tim.handle.set_profile("at the beach", &[]).unwrap();
    let desk = start("desk-offline").await;
    let tim_key = tim.identity.public_key_bytes();
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    wait_for("the desktop to have tim's status", || swarm_core::profile::cached(&desk.paths.profiles(), &tim_key).is_some()).await;
    tim.handle.shutdown().await;
    wait_for_long("tim offline on the desktop", || desk.handle.peer_status("tim").is_some_and(|s| !s.online)).await;

    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone-offline");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone-offline", Arc::new(joined.user), paths).await;
    wait_for_long("tim's status on the phone", || swarm_core::profile::cached(&phone.paths.profiles(), &tim_key).is_some_and(|p| p.status == "at the beach")).await;
    assert!(!phone.handle.peer_status("tim").unwrap().online, "tim's offline");
}

/// A call's voice, both ways, counted: what's heard, and a way to hang
/// up.
struct Talking {
    heard: Arc<std::sync::atomic::AtomicU64>,
    hang_up: Option<tokio::sync::oneshot::Sender<()>>,
    running: tokio::task::JoinHandle<()>,
}

fn talk(link: swarm_core::call::CallLink) -> Talking {
    use std::sync::atomic::Ordering;
    use swarm_core::call;
    let (mic_tx, mic_rx) = tokio::sync::mpsc::unbounded_channel();
    let (speaker_tx, mut speaker_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<f32>>();
    let (hang_up, hangup_rx) = tokio::sync::oneshot::channel();
    let heard = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let count = heard.clone();
    tokio::spawn(async move {
        while speaker_rx.recv().await.is_some() {
            count.fetch_add(1, Ordering::Relaxed);
        }
    });
    tokio::spawn(async move {
        let tone: Vec<f32> = (0..call::FRAME_SAMPLES).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        while mic_tx.send(tone.clone()).is_ok() {
            tokio::time::sleep(call::FRAME_DURATION).await;
        }
    });
    let running = tokio::spawn(async move {
        let _ = call::run(link, mic_rx, speaker_tx, hangup_rx, None).await;
    });
    Talking { heard, hang_up: Some(hang_up), running }
}

/// Both hear more of the other, within a few seconds.
async fn both_hear(a: &Talking, b: &Talking, what: &str) {
    use std::sync::atomic::Ordering;
    let (a0, b0) = (a.heard.load(Ordering::Relaxed), b.heard.load(Ordering::Relaxed));
    wait_for(what, || a.heard.load(Ordering::Relaxed) > a0 + 25 && b.heard.load(Ordering::Relaxed) > b0 + 25).await;
}

/// The next ring on `node`, within ten seconds: who from, and its link.
async fn rung(node: &TestNode, rings: &mut tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>, label: &str) -> (String, swarm_core::call::CallLink) {
    let _ = node;
    match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await {
        Ok(Some(node::CallEvent::Incoming { from, link, .. })) => (from, link),
        _ => panic!("{label} wasn't rung"),
    }
}

/// As above, Tim can only be reached by Owen's desktop, which is let
/// relay. Tim calls Owen: the desktop rings, and passes the ring on to
/// the phone, which answers; the desktop's told it was answered
/// elsewhere, and passes the call on, both ways. Then the phone calls Tim,
/// through the desktop.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_goes_through_another_device_that_reaches_a_peer() {
    use swarm_core::call::{self, CallLink};
    let tim = start("tim21").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let desk = start("desk21").await;
    let tim_key = tim.identity.public_key_bytes();
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;
    let phone = join_named_with_no_address(&desk, "phone", "phone21").await;
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;

    let (mut desk_rings, mut phone_rings, mut tim_rings) =
        (desk.calls.lock().unwrap().take().unwrap(), phone.calls.lock().unwrap().take().unwrap(), tim.calls.lock().unwrap().take().unwrap());
    let handle = tim.handle.call_handle("owen").unwrap();
    let mut to_paul = CallLink::attach(handle.commands, handle.budget).unwrap();
    let answered = tokio::spawn(async move { call::ring(&mut to_paul, false).await.unwrap().map(|_| to_paul) });
    let (_, mut on_desk) = rung(&desk, &mut desk_rings, "the desktop").await;
    let (from, on_phone) = rung(&phone, &mut phone_rings, "the phone").await;
    assert_eq!(phone.handle.label_of(&from), "tim");
    on_phone.accept(false).unwrap();
    let to_paul = tokio::time::timeout(Duration::from_secs(10), answered).await.unwrap().unwrap().expect("answered");
    let (mut at_tim, at_phone) = (talk(to_paul), talk(on_phone));
    both_hear(&at_tim, &at_phone, "tim and the phone to hear each other").await;
    wait_for("the desktop's ring to stop", || on_desk.caller_gave_up()).await;
    assert!(on_desk.answered_elsewhere(), "told it was answered elsewhere");
    at_tim.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), at_phone.running).await.expect("the phone's side ends").unwrap();

    let handle = phone.handle.call_handle("tim").expect("a way to call him");
    let mut to_tim = CallLink::attach(handle.commands, handle.budget).unwrap();
    let answered = tokio::spawn(async move { call::ring(&mut to_tim, false).await.unwrap().map(|_| to_tim) });
    let (from, on_tim) = rung(&tim, &mut tim_rings, "tim").await;
    assert_eq!(tim.handle.label_of(&from), "owen");
    on_tim.accept(false).unwrap();
    let to_tim = tokio::time::timeout(Duration::from_secs(10), answered).await.unwrap().unwrap().expect("answered");
    let (mut at_phone, at_tim) = (talk(to_tim), talk(on_tim));
    both_hear(&at_phone, &at_tim, "the phone and tim to hear each other").await;
    assert!(desk_rings.try_recv().is_err(), "the desktop isn't rung for a call it passes on");
    at_phone.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), at_tim.running).await.expect("tim's side ends").unwrap();
    assert!(all_online(&tim, "owen", 1), "tim has only the desktop");
}

/// A call through another device moves onto a direct connection once
/// there is one (`proxycall`, `LinkCalls::go_direct`). As above, the
/// phone calls Tim through the desktop. Then Tim can be reached where he
/// is, and says so: the phone, told by the desktop, connects to him
/// itself, and the call goes on over that, with no more than a moment's
/// gap; the desktop's let go of it. Hanging up then ends it on both.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_through_another_device_moves_onto_a_direct_connection() {
    use swarm_core::call::{self, CallLink};
    use swarm_core::node::InUse;
    let tim = start("tim42").await;
    let tim_at = format!("[::1]:{}", tim.handle.local_addr().unwrap().port());
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let desk = start("desk42").await;
    let tim_key = tim.identity.public_key_bytes();
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;
    let phone = join_named_with_no_address(&desk, "phone", "phone42").await;
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;
    assert!(matches!(phone.handle.connection_to(&tim_key), Some(InUse::Through(_))));

    let mut tim_rings = tim.calls.lock().unwrap().take().unwrap();
    let handle = phone.handle.call_handle("tim").expect("a way to call him");
    let mut to_tim = CallLink::attach(handle.commands, handle.budget).unwrap();
    let answered = tokio::spawn(async move { call::ring(&mut to_tim, false).await.unwrap().map(|_| to_tim) });
    let (_, on_tim) = rung(&tim, &mut tim_rings, "tim").await;
    on_tim.accept(false).unwrap();
    let to_tim = tokio::time::timeout(Duration::from_secs(10), answered).await.unwrap().unwrap().expect("answered");
    let (mut at_phone, at_tim) = (talk(to_tim), talk(on_tim));
    both_hear(&at_phone, &at_tim, "the phone and tim to hear each other through the desktop").await;

    tim.handle.set_my_addresses(vec![tim_at], Vec::new());
    let mut direct = false;
    for _ in 0..900 {
        if matches!(phone.handle.connection_to(&tim_key), Some(InUse::Direct { .. })) {
            direct = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(direct, "the phone connected to tim itself: {:?}", phone.handle.connection_to(&tim_key));
    wait_for("tim to have both of owen's devices", || all_online(&tim, "owen", 2)).await;
    wait_for("the desktop to let go of the call", || !desk.handle.in_call_through_links()).await;
    both_hear(&at_phone, &at_tim, "the phone and tim to hear each other directly").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    both_hear(&at_phone, &at_tim, "and to go on hearing each other").await;

    at_phone.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), at_tim.running).await.expect("tim's side ends").unwrap();
}

/// The next ring on a node, within ten seconds: who from, its link, and
/// the conference it rings into, if it does.
async fn rung_into(rings: &mut tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>, label: &str) -> (String, swarm_core::call::CallLink, Option<swarm_core::conference::Roster>) {
    match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await {
        Ok(Some(node::CallEvent::Incoming { from, link, conference, .. })) => (from, link, conference),
        _ => panic!("{label} wasn't rung"),
    }
}

/// The next conference event on a node, within ten seconds, other than
/// rings: a roster, or who hears whom.
async fn conference_event(rings: &mut tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>, label: &str) -> node::CallEvent {
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(until, rings.recv()).await {
            Ok(Some(event @ (node::CallEvent::Roster { .. } | node::CallEvent::Conference { .. }))) => return event,
            Ok(Some(_)) => {}
            _ => panic!("{label} had no conference event"),
        }
    }
}

/// Conferences through another device. As above, Tim can only
/// be reached by Owen's desktop, which is let relay. The phone rings Tim
/// into a conference through the desktop: he's rung with its roster, and
/// rosters and who hears whom pass both ways, to each side's front end,
/// none to the desktop's. Then Tim rings Owen into his: both of Owen's
/// devices are rung with its roster, the phone answers, and the same
/// passes both ways.
#[tokio::test(flavor = "multi_thread")]
async fn a_conference_goes_through_another_device_that_reaches_a_peer() {
    use swarm_core::call::{self, CallCommand, CallLink, CallSignal};
    use swarm_core::conference::Roster;
    let tim = start("tim23").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let desk = start("desk23").await;
    let (tim_key, paul_key) = (tim.identity.public_key_bytes(), desk.identity.public_key_bytes());
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;
    let phone = join_named_with_no_address(&desk, "phone", "phone23").await;
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;
    let (mut desk_rings, mut phone_rings, mut tim_rings) =
        (desk.calls.lock().unwrap().take().unwrap(), phone.calls.lock().unwrap().take().unwrap(), tim.calls.lock().unwrap().take().unwrap());
    let carol = Identity::generate();

    let mut roster = Roster::start(&phone.identity);
    roster.add(&phone.identity, tim_key);
    let handle = phone.handle.call_handle("tim").expect("a way to call him");
    let mut to_tim = CallLink::attach(handle.commands, handle.budget).unwrap();
    let sent = roster.clone();
    let answered = tokio::spawn(async move { call::ring_into(&mut to_tim, sent).await.unwrap().then_some(to_tim) });
    let (from, on_tim, got) = rung_into(&mut tim_rings, "tim").await;
    assert_eq!(tim.handle.label_of(&from), "owen");
    assert_eq!(got.as_ref(), Some(&roster), "rung with the conference's roster");
    on_tim.accept(false).unwrap();
    let to_tim = tokio::time::timeout(Duration::from_secs(10), answered).await.unwrap().unwrap().expect("joined");
    let (phone_says, tim_says) = (to_tim.commands(), on_tim.commands());
    let (mut at_phone, at_tim) = (talk(to_tim), talk(on_tim));
    both_hear(&at_phone, &at_tim, "the phone and tim to hear each other").await;
    roster.add(&phone.identity, carol.public_key_bytes());
    phone_says.send(CallCommand::Signal(CallSignal::Roster(roster.clone()))).unwrap();
    match conference_event(&mut tim_rings, "tim").await {
        node::CallEvent::Roster { peer_name, roster: got } => {
            assert_eq!(tim.handle.label_of(&peer_name), "owen");
            assert_eq!(got, roster);
        }
        _ => panic!("expected the roster"),
    }
    tim_says.send(CallCommand::Signal(CallSignal::Hearing(vec![paul_key]))).unwrap();
    match conference_event(&mut phone_rings, "the phone").await {
        node::CallEvent::Conference { peer_name, signal: CallSignal::Hearing(keys) } => {
            assert_eq!(phone.handle.label_of(&peer_name), "tim");
            assert_eq!(keys, vec![paul_key]);
        }
        _ => panic!("expected who tim hears"),
    }
    both_hear(&at_phone, &at_tim, "the call to carry on").await;
    assert!(desk_rings.try_recv().is_err(), "the desktop has nothing of a call it passes on");
    at_phone.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), at_tim.running).await.expect("tim's side ends").unwrap();

    let mut roster = Roster::start(&tim.identity);
    roster.add(&tim.identity, paul_key);
    let handle = tim.handle.call_handle("owen").unwrap();
    let mut to_paul = CallLink::attach(handle.commands, handle.budget).unwrap();
    let sent = roster.clone();
    let answered = tokio::spawn(async move { call::ring_into(&mut to_paul, sent).await.unwrap().then_some(to_paul) });
    let (_, mut on_desk, got) = rung_into(&mut desk_rings, "the desktop").await;
    assert_eq!(got.as_ref(), Some(&roster), "the desktop rung into the conference");
    let (from, on_phone, got) = rung_into(&mut phone_rings, "the phone").await;
    assert_eq!(phone.handle.label_of(&from), "tim");
    assert_eq!(got.as_ref(), Some(&roster), "the phone rung into the conference");
    on_phone.accept(false).unwrap();
    let to_paul = tokio::time::timeout(Duration::from_secs(10), answered).await.unwrap().unwrap().expect("joined");
    let (tim_says, phone_says) = (to_paul.commands(), on_phone.commands());
    let (mut at_tim, at_phone) = (talk(to_paul), talk(on_phone));
    both_hear(&at_tim, &at_phone, "tim and the phone to hear each other").await;
    wait_for("the desktop's ring to stop", || on_desk.caller_gave_up()).await;
    assert!(on_desk.answered_elsewhere(), "told it was answered elsewhere");
    roster.add(&tim.identity, carol.public_key_bytes());
    tim_says.send(CallCommand::Signal(CallSignal::Roster(roster.clone()))).unwrap();
    match conference_event(&mut phone_rings, "the phone").await {
        node::CallEvent::Roster { peer_name, roster: got } => {
            assert_eq!(phone.handle.label_of(&peer_name), "tim");
            assert_eq!(got, roster);
        }
        _ => panic!("expected the roster"),
    }
    phone_says.send(CallCommand::Signal(CallSignal::Hearing(vec![tim_key]))).unwrap();
    match conference_event(&mut tim_rings, "tim").await {
        node::CallEvent::Conference { peer_name, signal: CallSignal::Hearing(keys) } => {
            assert_eq!(tim.handle.label_of(&peer_name), "owen");
            assert_eq!(keys, vec![tim_key]);
        }
        _ => panic!("expected who the phone hears"),
    }
    both_hear(&at_tim, &at_phone, "the call to carry on").await;
    assert!(desk_rings.try_recv().is_err(), "the desktop has nothing of the call once it's answered elsewhere");
    at_tim.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), at_phone.running).await.expect("the phone's side ends").unwrap();
}

/// The next event on a node within ten seconds, a member asking for
/// our link: who from, its link and roster. A ring instead fails: asking
/// for a link never rings.
async fn asked(rings: &mut tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>, label: &str) -> (String, swarm_core::call::CallLink, swarm_core::conference::Roster) {
    match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await {
        Ok(Some(node::CallEvent::LinkAsked { from, link, roster })) => (from, link, roster),
        Ok(Some(node::CallEvent::Incoming { .. })) => panic!("{label} was rung"),
        _ => panic!("{label} wasn't asked"),
    }
}

/// A member of a conference asks for the link between them and Owen,
/// who's on two devices, one of them in the conference
///. Carol reaches both, and asks each: both are
/// asked, neither rung; the desktop, in the conference, takes it, and the
/// phone turns it down. Tim reaches only the desktop, let relay, and the
/// phone's the one in his conference: the desktop passes his asking on,
/// as asking, to the phone, which takes it, while the desktop turns its
/// own down.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_asked_for_in_a_conference_reaches_only_the_device_in_it() {
    use swarm_core::call::{self, CallLink};
    use swarm_core::conference::Roster;
    let carol = start("carol24").await;
    let tim = start("tim24").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let desk = start("desk24").await;
    let paul_key = desk.identity.public_key_bytes();
    let tim_key = tim.identity.public_key_bytes();
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    add(&carol, &desk, "owen");
    add(&desk, &carol, "carol");
    wait_for("tim and carol to reach owen's desktop", || all_online(&desk, "tim", 1) && all_online(&desk, "carol", 1)).await;
    let phone = join_named_with_no_address(&desk, "phone24", "phone24").await;
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for_long("carol to reach both of owen's devices", || all_online(&carol, "owen", 2)).await;
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;
    let (mut desk_rings, mut phone_rings) = (desk.calls.lock().unwrap().take().unwrap(), phone.calls.lock().unwrap().take().unwrap());

    let mut roster = Roster::start(&carol.identity);
    roster.add(&carol.identity, paul_key);
    let mut asking = Vec::new();
    for entry in carol.handle.same_person("owen") {
        let handle = carol.handle.call_handle(&entry).unwrap();
        let mut link = CallLink::attach(handle.commands, handle.budget).unwrap();
        let sent = roster.clone();
        asking.push((entry.clone(), tokio::spawn(async move { call::ask_link(&mut link, sent).await.unwrap().then_some(link) })));
    }
    let (_, on_phone, got) = asked(&mut phone_rings, "the phone").await;
    assert_eq!(got, roster);
    on_phone.reject().unwrap();
    let (from, on_desk, got) = asked(&mut desk_rings, "the desktop").await;
    assert_eq!(desk.handle.label_of(&from), "carol");
    assert_eq!(got, roster);
    on_desk.accept(false).unwrap();
    let mut taken = Vec::new();
    for (entry, ask) in asking {
        let answered = tokio::time::timeout(Duration::from_secs(10), ask).await.unwrap_or_else(|_| panic!("carol's ask of {entry} not answered"));
        if let Some(link) = answered.unwrap() {
            taken.push(link);
        }
    }
    assert_eq!(taken.len(), 1, "taken on one device, turned down on the other");
    let (mut at_carol, at_desk) = (talk(taken.pop().unwrap()), talk(on_desk));
    both_hear(&at_carol, &at_desk, "carol and the desktop to hear each other").await;
    assert!(phone_rings.try_recv().is_err(), "nothing more on the phone");
    at_carol.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), at_desk.running).await.expect("the desktop's side ends").unwrap();
    while desk_rings.try_recv().is_ok() {}

    let mut roster = Roster::start(&tim.identity);
    roster.add(&tim.identity, paul_key);
    let handle = tim.handle.call_handle("owen").unwrap();
    let mut to_paul = CallLink::attach(handle.commands, handle.budget).unwrap();
    let sent = roster.clone();
    let ask = tokio::spawn(async move { call::ask_link(&mut to_paul, sent).await.unwrap().then_some(to_paul) });
    let (_, on_desk, got) = asked(&mut desk_rings, "the desktop").await;
    assert_eq!(got, roster);
    on_desk.reject().unwrap();
    let (from, on_phone, got) = asked(&mut phone_rings, "the phone").await;
    assert_eq!(phone.handle.label_of(&from), "tim");
    assert_eq!(got, roster, "passed on as it was");
    on_phone.accept(false).unwrap();
    let to_paul = tokio::time::timeout(Duration::from_secs(10), ask).await.unwrap().unwrap().expect("taken on the phone");
    let (mut at_tim, at_phone) = (talk(to_paul), talk(on_phone));
    both_hear(&at_tim, &at_phone, "tim and the phone to hear each other").await;
    at_tim.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), at_phone.running).await.expect("the phone's side ends").unwrap();
}

/// Tim, whom only Owen's desktop can reach, and the desktop, let relay,
/// as for a call through it. Tim sends Owen a file: the desktop has the
/// offer, and passes it on to the phone, which takes it; it's off the
/// desktop's list, and comes to the phone through the desktop a chunk at
/// a time. Then the phone sends Tim a file, through the desktop, which
/// keeps no record of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_goes_through_another_device_that_reaches_a_peer() {
    use swarm_core::transfer::{Direction, State};
    let tim = start("tim22").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let desk = start("desk22").await;
    let tim_key = tim.identity.public_key_bytes();
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;
    let phone = join_named_with_no_address(&desk, "phone", "phone22").await;
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;

    let content: Vec<u8> = (0..150_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
    let file = tim.paths.dir().join("photo.jpg");
    std::fs::write(&file, &content).unwrap();
    tim.handle.offer_file("owen", &file).await.unwrap();
    let offered = |node: &TestNode, name: &str| node.handle.transfers().into_iter().find(|t| t.transfer.direction == Direction::In && t.transfer.state == State::Offered && t.transfer.name == name);
    wait_for("the desktop to have tim's offer", || offered(&desk, "photo.jpg").is_some()).await;
    wait_for("the phone to have it too, passed on", || offered(&phone, "photo.jpg").is_some()).await;
    let on_phone = offered(&phone, "photo.jpg").unwrap();
    assert_eq!(on_phone.peer_name.as_deref(), Some("tim"));
    let id = on_phone.transfer.id;
    let saves = phone.paths.dir().join("saved");
    phone.handle.accept_transfer(&id, &saves).unwrap();
    let state = |node: &TestNode, id| node.handle.transfers().into_iter().find(|t| t.transfer.id == id).map(|t| t.transfer.state);
    wait_for_long("the phone to have the file, through the desktop", || state(&phone, id) == Some(State::Done)).await;
    assert_eq!(std::fs::read(saves.join("photo.jpg")).unwrap(), content);
    assert_eq!(state(&desk, id), None, "taken on the phone: off the desktop's list");
    wait_for("tim to hear it's done", || tim.handle.transfers().iter().any(|t| t.transfer.id == id && t.transfer.state == State::Done)).await;

    let back: Vec<u8> = (0..90_000u32).map(|i| (i * 13 + 5) as u8).collect();
    let file = phone.paths.dir().join("reply.pdf");
    std::fs::write(&file, &back).unwrap();
    let out = phone.handle.offer_file("tim", &file).await.unwrap();
    wait_for("tim to have the phone's offer, from owen", || offered(&tim, "reply.pdf").is_some()).await;
    let at_tim = offered(&tim, "reply.pdf").unwrap();
    assert!(at_tim.is_with(&phone.identity.public_key_bytes()), "from owen");
    assert_eq!(at_tim.transfer.id, out, "the phone's own offer");
    let tim_saves = tim.paths.dir().join("saved");
    tim.handle.accept_transfer(&out, &tim_saves).unwrap();
    wait_for_long("tim to have the file, through the desktop", || state(&tim, out) == Some(State::Done)).await;
    assert_eq!(std::fs::read(tim_saves.join("reply.pdf")).unwrap(), back);
    wait_for("the phone to hear it's done", || state(&phone, out) == Some(State::Done)).await;
    assert_eq!(state(&desk, out), None, "the desktop keeps no record of it");
    assert!(all_online(&tim, "owen", 1), "tim has only the desktop");
}

/// Owen's server and phone are both added from his desktop, so each
/// knows only where the desktop is: the desktop tells each where the
/// other is, and they link. With the desktop gone, Tim, whom nobody can
/// dial and who dials only the server, is reached from the phone through
/// the server.
#[tokio::test(flavor = "multi_thread")]
async fn two_devices_added_from_a_third_find_each_other() {
    use swarm_core::node::joining;
    let desk = start("desk9").await;
    let mut added = Vec::new();
    for name in ["server9", "phone9"] {
        let code = desk.handle.add_device_code().unwrap();
        let device = Identity::generate();
        let joined = joining::join(&code, &device, name).await.unwrap();
        let paths = dir(name);
        swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
        joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
        added.push(start_as(name, Arc::new(joined.user), paths).await);
    }
    let (server, phone) = (&added[0], &added[1]);
    let all_linked = |node: &TestNode| node.handle.devices().devices.len() == 3 && node.handle.devices().devices.iter().all(|d| d.online);
    wait_for("the server and phone to link", || all_linked(server) && all_linked(phone)).await;
    desk.handle.shutdown().await;

    let tim = start("tim9").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let tim_key = tim.identity.public_key_bytes();
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&server.paths.peers(), &nowhere).unwrap();
    server.handle.add_peer_live(nowhere);
    add(&tim, server, "owen");
    assert!(server.handle.allow_relay(server.handle.device().public_key_bytes(), true).unwrap());
    wait_for("tim to reach owen's server", || all_online(server, "tim", 1)).await;
    wait_for("the phone to have tim", || peers::load(&phone.paths.peers()).unwrap().iter().any(|p| p.public_key == tim_key)).await;
    wait_for("the phone to reach tim through the server", || all_online(phone, "tim", 1)).await;
    phone.handle.send_message("tim", "from the phone").await.unwrap();
    wait_for("tim to have it", || said(&tim, &phone.identity.public_key_bytes()).contains(&"from the phone".to_string())).await;
}

/// Owen's desk starts again while his phone is away and his server is
/// up. The phone comes first on the list and has many addresses on file,
/// none answering (several seconds to wait out), yet the server is
/// linked at once: each device is dialled on its own, not one after
/// another.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_away_never_holds_up_linking_the_rest() {
    use swarm_core::node::joining;
    let desk = start("desk13").await;
    let mut keys = [Identity::generate(), Identity::generate()];
    keys.sort_by_key(|k| k.public_key_bytes());
    let [phone_device, server_device] = keys;
    let mut added = Vec::new();
    for (name, device) in [("phone13", &phone_device), ("server13", &server_device)] {
        let code = desk.handle.add_device_code().unwrap();
        let joined = joining::join(&code, device, name).await.unwrap();
        let paths = dir(name);
        swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
        joining::settle(&paths, &joined.user, device, &joined.list, &joined.by).unwrap();
        added.push(start_as(name, Arc::new(joined.user), paths).await);
    }
    let server = added.pop().unwrap();
    let phone = added.pop().unwrap();
    let all_linked = |node: &TestNode| node.handle.devices().devices.len() == 3 && node.handle.devices().devices.iter().all(|d| d.online);
    wait_for("all three to link", || all_linked(&desk) && all_linked(&server) && all_linked(&phone)).await;

    phone.handle.shutdown().await;
    let (identity, paths) = (desk.identity.clone(), desk.paths.clone());
    desk.handle.shutdown().await;
    let nowhere: Vec<String> = (0..20).map(|i| format!("[::1]:{}", 9 + i)).collect();
    swarm_core::addressbook::see(&paths.addresses(), &phone_device.public_key_bytes(), &nowhere).unwrap();
    let desk = start_as("desk13", identity, paths).await;
    let server_key = server_device.public_key_bytes();
    let started = std::time::Instant::now();
    wait_for("the desk to link the server again", || desk.handle.devices().devices.iter().any(|d| d.key == server_key && d.online)).await;
    assert!(started.elapsed() < Duration::from_secs(2), "the server linked after {:?}, held up by the phone", started.elapsed());
}

/// Owen's desktop takes his phone off his devices: the phone, told,
/// wipes itself, and Alice, sent the removal, drops the phone's entry and
/// refuses it from then on.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_taken_off_is_wiped_and_refused() {
    use swarm_core::node::joining;
    let alice = start("alice6").await;
    let desk = start("desk6").await;
    add(&desk, &alice, "alice");
    add(&alice, &desk, "owen");
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone6");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone6", Arc::new(joined.user), paths).await;
    wait_for("the desktop and phone to link", || phone.handle.devices().devices.iter().all(|d| d.online) && phone.handle.devices().devices.len() == 2).await;
    wait_for("alice to reach both of owen's devices", || all_online(&alice, "owen", 2)).await;

    desk.handle.remove_device(device.public_key_bytes()).unwrap();
    wait_for("the phone to wipe itself", || phone.handle.wiped()).await;
    wait_for("the phone's folder to be empty", || std::fs::read_dir(phone.paths.dir()).map(|mut d| d.next().is_none()).unwrap_or(false)).await;
    wait_for("alice to drop the phone", || alice.handle.same_person("owen").len() == 1).await;
    assert_eq!(desk.handle.devices().devices.len(), 1, "the desktop alone");
}

/// A file offered to Owen goes to each of his devices, each transfer
/// under that device's key; all of them are his, by his key as a person
/// (what a contact's Transfers shows), and no one else's.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_to_a_users_devices_is_with_them() {
    let alice = start("alice-files").await;
    let owen = Arc::new(Identity::generate());
    let desk = start_as("desk-files", owen.clone(), dir("desk-files")).await;
    let phone = start_as("phone-files", owen.clone(), dir("phone-files")).await;
    add(&alice, &desk, "owen");
    add(&desk, &alice, "alice");
    add(&phone, &alice, "alice");
    wait_for("alice to reach both of owen's devices", || all_online(&alice, "owen", 2)).await;
    let file = alice.paths.dir().join("hello.txt");
    std::fs::write(&file, "hello").unwrap();
    alice.handle.offer_file("owen", &file).await.unwrap();
    let all = alice.handle.transfers();
    assert_eq!(all.len(), 2, "one to each device");
    let paul_key = owen.public_key_bytes();
    assert!(all.iter().all(|t| t.transfer.peer != paul_key), "each under its device's key");
    assert!(all.iter().all(|t| t.is_with(&paul_key)), "both his");
    assert!(all.iter().all(|t| t.peer_name.as_deref() == Some("owen")));
    assert!(!all.iter().any(|t| t.is_with(&alice.identity.public_key_bytes())));

    use swarm_core::transfer::State;
    let offered = |node: &TestNode| node.handle.transfers().iter().any(|t| t.transfer.state == State::Offered);
    wait_for("both devices to be offered it", || offered(&desk) && offered(&phone)).await;
    let id = phone.handle.transfers()[0].transfer.id;
    phone.handle.accept_transfer(&id, &phone.paths.dir().join("saves")).unwrap();
    let done = |node: &TestNode| node.handle.transfers().iter().any(|t| t.transfer.id == id && t.transfer.state == State::Done);
    wait_for("the phone to have it", || done(&phone)).await;
    wait_for("alice to see it delivered", || done(&alice)).await;
    wait_for("the desk's offer to go", || swarm_core::transfer::load(&desk.paths.transfers()).is_empty()).await;
    assert_eq!(swarm_core::transfer::load(&alice.paths.transfers()).len(), 1, "alice keeps only the one taken");

    let saved = phone.handle.transfers()[0].transfer.path.clone();
    assert_eq!(alice.handle.clear_transfers(&[id]).unwrap(), 1);
    assert!(swarm_core::transfer::load(&alice.paths.transfers()).is_empty());
    let settled = || swarm_core::transfer::load(&phone.paths.transfers()).iter().any(|t| t.id == id && t.settled);
    wait_for("alice to tell the phone she knows", settled).await;
    assert_eq!(phone.handle.clear_transfers(&[id]).unwrap(), 1);
    assert!(swarm_core::transfer::load(&phone.paths.transfers()).is_empty());
    assert_eq!(std::fs::read_to_string(saved).unwrap(), "hello");
}

/// Owen's desktop and phone are linked; Tim, a stranger, has added Owen
/// at the desktop's address alone (one port forwarded to it, say), so
/// only the desktop hears him introduce himself. The request is on the
/// phone too, under the name he gave and where he was, and dismissing it
/// there dismisses it on the desktop. Asking again, it's back on both;
/// added from the phone, he's no longer asking on either.
#[tokio::test(flavor = "multi_thread")]
async fn a_contact_request_to_one_device_is_on_every_one() {
    use swarm_core::node::joining;
    use swarm_core::requests;
    let desk = start("desk11").await;
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone11");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone11", Arc::new(joined.user), paths).await;
    wait_for("the desktop and phone to link", || desk.handle.devices().devices.iter().all(|d| d.online) && desk.handle.devices().devices.len() == 2).await;

    let tim = start("tim11").await;
    let tim_key = tim.identity.public_key_bytes();
    swarm_core::displayname::set(&tim.paths.displayname(), &tim.identity, "Tim").unwrap();
    add(&tim, &desk, "owen");
    let asking = |node: &TestNode| requests::pending(&node.paths.requests()).unwrap_or_default().iter().any(|r| r.public_key == tim_key);
    wait_for("the desktop to hear tim ask", || asking(&desk)).await;
    wait_for("the phone to have tim's request", || asking(&phone)).await;
    let on_phone = requests::pending(&phone.paths.requests()).unwrap().into_iter().find(|r| r.public_key == tim_key).unwrap();
    assert_eq!(on_phone.address, format!("[::1]:{}", tim.handle.local_addr().unwrap().port()));
    assert_eq!(swarm_core::displayname::cached_one(&phone.paths.displaynames(), &tim_key).unwrap().map(|n| n.name).as_deref(), Some("Tim"));
    requests::remove(&phone.paths.requests(), &tim_key).unwrap();
    wait_for("the desktop to have it dismissed", || !asking(&desk)).await;
    requests::record(&desk.paths.requests(), tim_key, &on_phone.address, swarm_core::unix_now() + 5).unwrap();
    wait_for("the phone to have it back", || asking(&phone)).await;

    let tim_peer = Peer { name: "tim".to_string(), address: on_phone.address.clone(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&phone.paths.peers(), &tim_peer).unwrap();
    phone.handle.add_peer_live(tim_peer);
    let _ = requests::forget(&phone.paths.requests(), &tim_key);
    wait_for("tim to reach owen", || tim.handle.peer_status("owen").is_some_and(|s| s.online)).await;
    wait_for("tim asking on neither", || !asking(&desk) && !asking(&phone)).await;
}

/// Deleting a contact on one device deletes everything about them on the
/// others too, and once both have that, nothing of them is left on
/// either; blocking keeps only the key, on both.
#[tokio::test(flavor = "multi_thread")]
async fn a_contact_deleted_on_one_device_leaves_nothing_on_any() {
    use swarm_core::node::joining;
    use swarm_core::requests;
    let desk = start("desk12").await;
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone12");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone12", Arc::new(joined.user), paths).await;
    wait_for("the desktop and phone to link", || desk.handle.devices().devices.iter().all(|d| d.online) && desk.handle.devices().devices.len() == 2).await;

    let tim = start("tim12").await;
    let tim_key = tim.identity.public_key_bytes();
    swarm_core::displayname::set(&tim.paths.displayname(), &tim.identity, "Tim").unwrap();
    add(&desk, &tim, "tim");
    add(&tim, &desk, "owen");
    let has_tim = |node: &TestNode| peers::load(&node.paths.peers()).unwrap_or_default().iter().any(|p| p.public_key == tim_key);
    wait_for("the phone to have tim too", || has_tim(&phone)).await;
    wait_for("tim to reach owen", || tim.handle.peer_status("owen").is_some_and(|s| s.online)).await;
    tim.handle.send_message("owen", "hello").await.unwrap();
    let chat = |node: &TestNode| swarm_core::history::all(&node.paths.history(), &tim_key).unwrap_or_default().len();
    wait_for("both to have tim's message", || chat(&desk) == 1 && chat(&phone) == 1).await;

    desk.handle.read_up_to(swarm_core::readmarks::Chat::Contact(tim_key), 1);
    peers::remove(&phone.paths.peers(), "tim").unwrap();
    phone.handle.forget_contact("tim", tim_key, false);
    let tim_port = tim.handle.local_addr().unwrap().port();
    tim.handle.set_my_addresses(vec![format!("[::1]:{tim_port}"), "[::1]:7".to_string()], Vec::new());
    for _ in 0..150 {
        if !has_tim(&desk) && chat(&desk) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!has_tim(&desk) && chat(&desk) == 0, "the desktop to delete tim too: still has him {}, {} said", has_tim(&desk), chat(&desk));
    assert!(!has_tim(&phone), "not brought back on the phone");
    assert_eq!(chat(&phone), 0);
    let key_text = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, tim_key);
    let in_book = |node: &TestNode| swarm_core::store::read(node.paths.dir().join("peersbook")).unwrap_or_default().contains(&key_text);
    wait_for_long("tim to go from both books", || !in_book(&desk) && !in_book(&phone)).await;
    for node in [&desk, &phone] {
        assert!(requests::all(&node.paths.requests()).unwrap().is_empty());
        assert!(swarm_core::displayname::cached_one(&node.paths.displaynames(), &tim_key).unwrap().is_none(), "his name's gone too");
        assert!(!node.handle.read_marks().contains_key(&swarm_core::readmarks::Chat::Contact(tim_key)), "how far he'd been read's gone too");
    }

    add(&desk, &tim, "tim");
    wait_for("the phone to have tim again", || has_tim(&phone)).await;
    wait_for("tim to reach owen again", || tim.handle.peer_status("owen").is_some_and(|s| s.online)).await;
    let named_tim = |node: &TestNode| swarm_core::displayname::cached_one(&node.paths.displaynames(), &tim_key).ok().flatten().is_some_and(|n| n.name == "Tim");
    wait_for("the desktop to know tim's name again", || named_tim(&desk)).await;
    peers::remove(&desk.paths.peers(), "tim").unwrap();
    desk.handle.forget_contact("tim", tim_key, true);
    wait_for("the phone to block tim too", || !has_tim(&phone) && requests::is_blocked(&phone.paths.requests(), &tim_key).unwrap_or(false)).await;
    wait_for("tim's name on the phone, for the list of blocked keys", || named_tim(&phone)).await;
    assert!(named_tim(&desk));
    assert_eq!(chat(&desk) + chat(&phone), 0);
}

/// A contact that came with the desktop's contacts and chats (`bundle`),
/// deleted on the phone the moment it's there, before the phone's book
/// has looked: deleted on both, and not brought back on the phone by the
/// desktop's line for them.
#[tokio::test(flavor = "multi_thread")]
async fn a_contact_deleted_as_soon_as_it_came_stays_deleted() {
    use swarm_core::node::joining;
    let desk = start("desk27").await;
    let tim = start("tim27").await;
    let tim_key = tim.identity.public_key_bytes();
    add(&desk, &tim, "tim");
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone27");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone27", Arc::new(joined.user), paths).await;
    let has_tim = |node: &TestNode| peers::load(&node.paths.peers()).unwrap_or_default().iter().any(|p| p.public_key == tim_key);
    for _ in 0..1000 {
        if has_tim(&phone) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(has_tim(&phone), "tim came with the desktop's contacts");
    peers::remove(&phone.paths.peers(), "tim").unwrap();
    phone.handle.forget_contact("tim", tim_key, false);
    let key_text = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, tim_key);
    let line = |node: &TestNode| swarm_core::store::read(node.paths.dir().join("peersbook")).unwrap_or_default().lines().filter(|l| l.starts_with(&key_text)).map(|l| l.split('\t').take(4).collect::<Vec<_>>().join(" ")).collect::<String>();
    for _ in 0..150 {
        if !has_tim(&desk) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!has_tim(&desk), "the desktop to delete tim too: desktop's line {:?}, phone's {:?}, phone has him {}", line(&desk), line(&phone), has_tim(&phone));
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!has_tim(&phone) && !has_tim(&desk), "not brought back");
}

/// `wait_for`, for something on the half-minute tidy.
async fn wait_for_long(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..900 {
        if check() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Another device of `from`'s user that never says where it can be
/// reached: its links to `from` are all it has, so two of these have no
/// way to each other but through `from`.
async fn join_with_no_address(from: &TestNode, label: &str) -> TestNode {
    join_named_with_no_address(from, label, label).await
}

/// As `join_with_no_address`, the device called `name`: for a test of
/// what goes through a device that relays, whose contact must have no
/// way to it but that (a relaying device tells contacts where the
/// user's others are, and they'd connect to it directly).
async fn join_named_with_no_address(from: &TestNode, name: &str, label: &str) -> TestNode {
    use swarm_core::node::joining;
    let code = from.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, name).await.unwrap();
    let paths = dir(label);
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let identity = Arc::new(joined.user);
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log: Arc::new(|_| {}) };
    let (handle, call_events) = node::start(identity.clone(), config).await.unwrap();
    TestNode { handle, identity, paths, calls: std::sync::Mutex::new(Some(call_events)) }
}

/// Owen's phone and laptop have no way to each other, only each to the
/// desktop. With the desktop let relay, each shows the other online
/// through it; a copy of a message waiting since before then goes through
/// it, as does one sent after; and the two can call each other, the
/// desktop passing the call on and never ringing itself.
#[tokio::test(flavor = "multi_thread")]
async fn two_devices_with_no_way_to_each_other_go_through_a_third() {
    use swarm_core::call::{self, CallLink};
    use swarm_core::node::devices::call_name;
    let alice = start("alice31").await;
    let desk = start("desk31").await;
    let alice_key = alice.identity.public_key_bytes();
    add(&desk, &alice, "alice");
    add(&alice, &desk, "owen");
    wait_for("the desktop to reach alice", || all_online(&desk, "alice", 1)).await;
    let phone = join_with_no_address(&desk, "phone31").await;
    let laptop = join_with_no_address(&desk, "laptop31").await;
    let (phone_key, laptop_key) = (phone.handle.device().public_key_bytes(), laptop.handle.device().public_key_bytes());
    let device_of = |node: &TestNode, key: [u8; 32]| node.handle.devices().devices.into_iter().find(|d| d.key == key);
    wait_for("both to link to the desktop", || {
        [&phone, &laptop].iter().all(|n| device_of(n, desk.handle.device().public_key_bytes()).is_some_and(|d| d.online)) && device_of(&phone, laptop_key).is_some() && device_of(&laptop, phone_key).is_some()
    })
    .await;
    wait_for("the phone to have alice", || peers::load(&phone.paths.peers()).unwrap().iter().any(|p| p.public_key == alice_key)).await;
    wait_for("the phone to reach alice", || phone.handle.peer_status("alice").is_some_and(|s| s.online)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!device_of(&phone, laptop_key).unwrap().online, "no way to the laptop yet");

    phone.handle.send_message("alice", "before the relay").await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!said(&laptop, &alice_key).contains(&"before the relay".to_string()), "nothing's carried it yet");

    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for("each to show the other through the desktop", || {
        device_of(&phone, laptop_key).is_some_and(|d| d.online && d.through.is_some()) && device_of(&laptop, phone_key).is_some_and(|d| d.online && d.through.is_some())
    })
    .await;
    assert!(matches!(phone.handle.connection_to(&laptop_key), Some(node::InUse::Through(_))), "in use: through the desktop");
    wait_for("the waiting copy to reach the laptop", || said(&laptop, &alice_key).contains(&"before the relay".to_string())).await;
    phone.handle.send_message("alice", "after the relay").await.unwrap();
    wait_for("the new copy to reach the laptop", || said(&laptop, &alice_key).contains(&"after the relay".to_string())).await;
    assert_eq!(said(&laptop, &alice_key).iter().filter(|t| *t == "after the relay").count(), 1, "kept once");

    let (mut desk_rings, mut laptop_rings) = (desk.calls.lock().unwrap().take().unwrap(), laptop.calls.lock().unwrap().take().unwrap());
    let name = call_name(&laptop_key);
    assert!(phone.handle.peer_status(&name).is_some_and(|s| s.online), "callable");
    let handle = phone.handle.call_handle(&name).expect("a way to call it");
    let mut to_laptop = CallLink::attach(handle.commands, handle.budget).unwrap();
    let answered = tokio::spawn(async move { call::ring(&mut to_laptop, false).await.unwrap().map(|_| to_laptop) });
    let (from, on_laptop) = rung(&laptop, &mut laptop_rings, "the laptop").await;
    assert_eq!(from, call_name(&phone_key), "the phone calling, by its own name");
    on_laptop.accept(false).unwrap();
    let to_laptop = tokio::time::timeout(Duration::from_secs(10), answered).await.unwrap().unwrap().expect("answered");
    let (mut at_phone, at_laptop) = (talk(to_laptop), talk(on_laptop));
    both_hear(&at_phone, &at_laptop, "the phone and laptop to hear each other").await;
    assert!(desk_rings.try_recv().is_err(), "the desktop isn't rung for a call it passes on");
    at_phone.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), at_laptop.running).await.expect("the laptop's side ends").unwrap();

    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), false).unwrap());
    wait_for("the phone to show the laptop offline", || device_of(&phone, laptop_key).is_some_and(|d| !d.online && d.through.is_none())).await;
}

/// Another device of `from`'s user, added with a code from it.
async fn join_from(from: &TestNode, label: &str) -> TestNode {
    use swarm_core::node::joining;
    let code = from.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, label).await.unwrap();
    let paths = dir(label);
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    start_as(label, Arc::new(joined.user), paths).await
}

/// Owen (desktop, phone) and Dave (laptop, tablet), brought in in an
/// order that leaves a pair unmet: the desktop and laptop add each
/// other; Dave's tablet, added next, finds Owen at the desktop; Owen's
/// phone, added last, finds Dave at whichever of his answers first.
/// Neither side stops at the first device that answers: every device of
/// each reaches every device of the other, so a call from either rings
/// all of the other's.
#[tokio::test(flavor = "multi_thread")]
async fn every_device_of_a_user_meets_every_device_of_a_contact() {
    let desk = start("desk10").await;
    let laptop = start("laptop10").await;
    add(&desk, &laptop, "dave");
    add(&laptop, &desk, "owen");
    wait_for("the desktop and laptop to meet", || all_online(&desk, "dave", 1) && all_online(&laptop, "owen", 1)).await;
    let tablet = join_from(&laptop, "tablet10").await;
    wait_for("the tablet to reach owen", || all_online(&tablet, "owen", 1)).await;
    let phone = join_from(&desk, "phone10").await;
    wait_for("the phone to reach dave", || tablet.handle.same_person("owen").len() + laptop.handle.same_person("owen").len() >= 3).await;
    for (node, name, label) in [(&desk, "dave", "desktop"), (&phone, "dave", "phone"), (&laptop, "owen", "laptop"), (&tablet, "owen", "tablet")] {
        wait_for_long(&format!("the {label} to reach both of {name}'s devices"), || all_online(node, name, 2)).await;
    }
}

/// Voices a conference passes on along a call through another device
/// share the room of that device's connection to the peer, as they would
/// on a direct one. Tim's reached only by
/// Owen's desktop, let relay, which covers its connection to him; the
/// phone calls Tim through it. While the phone says it passes on two
/// others' voices, the desktop makes room for three on its connection,
/// and the phone's told it may send less for each; when it stops, the
/// room's back. The phone takes the call as covered, though its link
/// to the desktop isn't (step 7).
#[tokio::test(flavor = "multi_thread")]
async fn voices_passed_on_through_another_device_share_its_room() {
    use std::sync::atomic::Ordering;
    use swarm_core::call::{self, CallLink};
    let tim = start("tim25").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let desk = start("desk25").await;
    let tim_key = tim.identity.public_key_bytes();
    let covered = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: Some(48_000), cover_required: false };
    peers::add(&desk.paths.peers(), &covered).unwrap();
    desk.handle.add_peer_live(covered);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;
    let phone = join_named_with_no_address(&desk, "phone", "phone25").await;
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;
    let mut tim_rings = tim.calls.lock().unwrap().take().unwrap();

    let handle = phone.handle.call_handle("tim").expect("a way to call him");
    let mut to_tim = CallLink::attach(handle.commands, handle.budget).unwrap();
    let budget = to_tim.budget();
    let answered = tokio::spawn(async move { call::ring(&mut to_tim, false).await.unwrap().is_some().then_some(to_tim) });
    let (_, on_tim) = rung(&tim, &mut tim_rings, "tim").await;
    on_tim.accept(false).unwrap();
    let to_tim = tokio::time::timeout(Duration::from_secs(10), answered).await.unwrap().unwrap().expect("answered");
    let (at_phone, at_tim) = (talk(to_tim), talk(on_tim));
    both_hear(&at_phone, &at_tim, "the phone and tim to hear each other").await;
    let audio = || budget.audio.load(Ordering::Relaxed);
    wait_for("the desktop's cover to bound the phone's voice", || audio() < call::MAX_AUDIO_BYTES).await;
    let alone = audio();
    assert!(budget.covered.load(Ordering::Relaxed), "the desktop's cover counts on the phone");
    budget.relayed.store(2, Ordering::Relaxed);
    wait_for("less for each of three voices", || audio() < alone).await;
    let shared = audio();
    assert!(shared >= call::MIN_AUDIO_BYTES, "{shared}");
    budget.relayed.store(0, Ordering::Relaxed);
    wait_for("the room back once none's passed on", || audio() == alone).await;
    both_hear(&at_phone, &at_tim, "the call to carry on").await;
}

/// `from` rings the contact it calls `name` into the conference `roster`
/// is of; they're rung with it on `rings`, and answer. The link at each
/// end.
async fn rung_in(
    from: &TestNode,
    name: &str,
    rings: &mut tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>,
    roster: &swarm_core::conference::Roster,
) -> (swarm_core::call::CallLink, swarm_core::call::CallLink) {
    use swarm_core::call::{self, CallLink};
    let handle = from.handle.call_handle(name).unwrap_or_else(|| panic!("a way to call {name}"));
    let mut link = CallLink::attach(handle.commands, handle.budget).unwrap();
    let sent = roster.clone();
    let answered = tokio::spawn(async move { call::ring_into(&mut link, sent).await.unwrap().then_some(link) });
    let (_, there, got) = rung_into(rings, name).await;
    assert_eq!(got.as_ref(), Some(roster), "{name} rung with the conference's roster");
    there.accept(false).unwrap();
    let here = tokio::time::timeout(Duration::from_secs(10), answered).await.unwrap().unwrap().expect("joined");
    (here, there)
}

/// As `rung_in`, a member asking the contact it calls `name` for the
/// link between them: taken without ringing.
async fn linked(
    from: &TestNode,
    name: &str,
    rings: &mut tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>,
    roster: &swarm_core::conference::Roster,
) -> (swarm_core::call::CallLink, swarm_core::call::CallLink) {
    use swarm_core::call::{self, CallLink};
    let handle = from.handle.call_handle(name).unwrap();
    let mut link = CallLink::attach(handle.commands, handle.budget).unwrap();
    let sent = roster.clone();
    let taken = tokio::spawn(async move { call::ask_link(&mut link, sent).await.unwrap().then_some(link) });
    let (_, there, got) = asked(rings, name).await;
    assert_eq!(&got, roster);
    there.accept(false).unwrap();
    let here = tokio::time::timeout(Duration::from_secs(10), taken).await.unwrap().unwrap().expect("taken");
    (here, there)
}

/// The next roster on a node, within ten seconds.
async fn next_roster(rings: &mut tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>, label: &str) -> swarm_core::conference::Roster {
    match conference_event(rings, label).await {
        node::CallEvent::Roster { roster, .. } => roster,
        _ => panic!("{label} expected a roster"),
    }
}

/// Ends within `secs` seconds.
async fn ends(talking: Talking, secs: u64, what: &str) {
    tokio::time::timeout(Duration::from_secs(secs), talking.running).await.unwrap_or_else(|_| panic!("{what}")).unwrap();
}

/// Leaving, removing, and losing someone, in a conference through another
/// of your devices. Tim is reached only
/// by Owen's desktop, let relay; Rupert, a contact of both, by the phone
/// too. The phone has a conference with both, Tim's link through the
/// desktop, and Tim and Rupert linked between them. The phone removes
/// Tim: he's told so, his link ends, and Owen and Rupert go on; added
/// again, he's rung again. Then Owen leaves: Tim and Rupert go on. Last,
/// a conference where the phone passes each one's voice on to the other
/// (they're not linked), asked and sent through the desktop; the
/// desktop loses Tim partway: the phone's link to him ends (shown as
/// lost, not declined: any end of an answered link is), and Owen and
/// Rupert go on.
#[tokio::test(flavor = "multi_thread")]
async fn leaving_removing_and_losing_someone_through_another_device() {
    use swarm_core::call::{CallCommand, CallSignal};
    use swarm_core::conference::Roster;
    let tim = start("tim26").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let rupert = start("stefan26").await;
    let desk = start("desk26").await;
    let (tim_key, stefan_key, paul_key) = (tim.identity.public_key_bytes(), rupert.identity.public_key_bytes(), desk.identity.public_key_bytes());
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    add(&desk, &rupert, "rupert");
    add(&rupert, &desk, "owen");
    add(&tim, &rupert, "rupert");
    add(&rupert, &tim, "tim");
    wait_for("tim and rupert to reach owen's desktop", || all_online(&desk, "tim", 1) && all_online(&desk, "rupert", 1)).await;
    let phone = join_named_with_no_address(&desk, "phone26", "phone26").await;
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for_long("rupert to reach both of owen's devices", || all_online(&rupert, "owen", 2)).await;
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;
    wait_for("tim and rupert to reach each other", || all_online(&tim, "rupert", 1)).await;
    wait_for("tim through the desktop, rupert direct", || phone.handle.calls_go_through("tim") && !phone.handle.calls_go_through("rupert")).await;
    let (mut desk_rings, mut phone_rings, mut tim_rings, mut stefan_rings) = (
        desk.calls.lock().unwrap().take().unwrap(),
        phone.calls.lock().unwrap().take().unwrap(),
        tim.calls.lock().unwrap().take().unwrap(),
        rupert.calls.lock().unwrap().take().unwrap(),
    );
    let drain = |rings: &mut tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>| while rings.try_recv().is_ok() {};

    let mut roster = Roster::start(&phone.identity);
    roster.add(&phone.identity, tim_key);
    roster.add(&phone.identity, stefan_key);
    let (to_tim, tim_paul) = rung_in(&phone, "tim", &mut tim_rings, &roster).await;
    let (to_stefan, stefan_paul) = rung_in(&phone, "rupert", &mut stefan_rings, &roster).await;
    let (tim_stefan, stefan_tim) = linked(&tim, "rupert", &mut stefan_rings, &roster).await;
    let (phone_tim, phone_stefan) = (to_tim.commands(), to_stefan.commands());
    let (mut at_phone_tim, at_tim_paul, at_phone_stefan, at_stefan_paul, mut at_tim_stefan, at_stefan_tim) =
        (talk(to_tim), talk(tim_paul), talk(to_stefan), talk(stefan_paul), talk(tim_stefan), talk(stefan_tim));
    both_hear(&at_phone_tim, &at_tim_paul, "the phone and tim to hear each other").await;
    both_hear(&at_phone_stefan, &at_stefan_paul, "the phone and rupert to hear each other").await;
    both_hear(&at_tim_stefan, &at_stefan_tim, "tim and rupert to hear each other").await;

    assert!(roster.remove(&phone.identity, tim_key));
    for to in [&phone_tim, &phone_stefan] {
        to.send(CallCommand::Signal(CallSignal::Roster(roster.clone()))).unwrap();
    }
    at_phone_tim.hang_up.take().unwrap().send(()).unwrap();
    let told = next_roster(&mut tim_rings, "tim").await;
    assert_eq!(told.removed_by(&tim_key), Some(paul_key), "tim told owen removed him");
    ends(at_tim_paul, 10, "tim's link to the phone to end").await;
    assert_eq!(next_roster(&mut stefan_rings, "rupert").await.removed_by(&tim_key), Some(paul_key));
    at_tim_stefan.hang_up.take().unwrap().send(()).unwrap();
    ends(at_stefan_tim, 10, "rupert's link to tim to end").await;
    both_hear(&at_phone_stefan, &at_stefan_paul, "owen and rupert to go on").await;

    roster.add(&phone.identity, tim_key);
    phone_stefan.send(CallCommand::Signal(CallSignal::Roster(roster.clone()))).unwrap();
    assert!(next_roster(&mut stefan_rings, "rupert").await.is_member(&tim_key), "rupert told tim's back");
    drain(&mut tim_rings);
    let (to_tim, tim_paul) = rung_in(&phone, "tim", &mut tim_rings, &roster).await;
    let (tim_stefan, stefan_tim) = linked(&tim, "rupert", &mut stefan_rings, &roster).await;
    let phone_tim = to_tim.commands();
    let (mut at_phone_tim, at_tim_paul, at_tim_stefan, at_stefan_tim) = (talk(to_tim), talk(tim_paul), talk(tim_stefan), talk(stefan_tim));
    both_hear(&at_phone_tim, &at_tim_paul, "the phone and tim, added again, to hear each other").await;
    both_hear(&at_tim_stefan, &at_stefan_tim, "tim and rupert to hear each other again").await;

    let mut at_phone_stefan = at_phone_stefan;
    roster.leave(&phone.identity);
    for to in [&phone_tim, &phone_stefan] {
        to.send(CallCommand::Signal(CallSignal::Roster(roster.clone()))).unwrap();
    }
    at_phone_tim.hang_up.take().unwrap().send(()).unwrap();
    at_phone_stefan.hang_up.take().unwrap().send(()).unwrap();
    assert!(next_roster(&mut tim_rings, "tim").await.has_left(&paul_key), "tim told owen left");
    assert!(next_roster(&mut stefan_rings, "rupert").await.has_left(&paul_key), "rupert told owen left");
    ends(at_tim_paul, 10, "tim's link to the phone to end").await;
    ends(at_stefan_paul, 10, "rupert's link to the phone to end").await;
    both_hear(&at_tim_stefan, &at_stefan_tim, "tim and rupert to go on").await;
    let mut at_tim_stefan = at_tim_stefan;
    at_tim_stefan.hang_up.take().unwrap().send(()).unwrap();
    ends(at_stefan_tim, 10, "tim and rupert's call to end").await;

    drain(&mut phone_rings);
    drain(&mut tim_rings);
    drain(&mut stefan_rings);
    let mut roster = Roster::start(&phone.identity);
    roster.add(&phone.identity, tim_key);
    roster.add(&phone.identity, stefan_key);
    let (to_tim, mut tim_paul) = rung_in(&phone, "tim", &mut tim_rings, &roster).await;
    let (to_stefan, stefan_paul) = rung_in(&phone, "rupert", &mut stefan_rings, &roster).await;
    let (phone_tim, tim_phone) = (to_tim.commands(), tim_paul.commands());
    let (relayed_tx, mut relayed_rx) = tokio::sync::mpsc::unbounded_channel();
    tim_paul.relay_to(relayed_tx);
    let (at_phone_tim, at_tim_paul, at_phone_stefan, at_stefan_paul) = (talk(to_tim), talk(tim_paul), talk(to_stefan), talk(stefan_paul));
    both_hear(&at_phone_tim, &at_tim_paul, "the phone and tim to hear each other").await;
    tim_phone.send(CallCommand::Signal(CallSignal::Forward { from: stefan_key, id: 1, on: true })).unwrap();
    match conference_event(&mut phone_rings, "the phone").await {
        node::CallEvent::Conference { peer_name, signal: CallSignal::Forward { from, id: 1, on: true } } => {
            assert_eq!(phone.handle.label_of(&peer_name), "tim");
            assert_eq!(from, stefan_key);
        }
        _ => panic!("expected tim asking for rupert's voice"),
    }
    let passing = tokio::spawn({
        let phone_tim = phone_tim.clone();
        async move {
            while phone_tim.send(CallCommand::Signal(CallSignal::Relayed { id: 1, opus: vec![0xF8, 0xFF, 0xFE] })).is_ok() {
                tokio::time::sleep(swarm_core::call::FRAME_DURATION).await;
            }
        }
    });
    for _ in 0..25 {
        match tokio::time::timeout(Duration::from_secs(10), relayed_rx.recv()).await {
            Ok(Some((1, _))) => {}
            _ => panic!("rupert's voice not passed on to tim through the desktop"),
        }
    }

    assert!(desk_rings.try_recv().is_err(), "the desktop has nothing of a call it passes on");
    tim.handle.shutdown().await;
    ends(at_phone_tim, 20, "the phone's link to tim to end once the desktop's lost him").await;
    passing.abort();
    both_hear(&at_phone_stefan, &at_stefan_paul, "owen and rupert to go on").await;
    assert!(desk_rings.try_recv().is_err(), "nothing on the desktop");
    let _ = phone_tim;
}

/// Video through another of your devices. Tim's
/// reached only by Owen's desktop, let relay. The phone calls him
/// through it with voice; each side turns its camera on in turn and the
/// other's shown it, so pictures, full ones asked for and the boxes
/// they're shown in go both ways through the desktop, within its
/// connection's room. In a conference, what's passed on of another
/// member's pictures (`Via`) goes through as it is, and what's asked of
/// them comes back.
#[tokio::test(flavor = "multi_thread")]
async fn video_goes_through_another_device_that_reaches_a_peer() {
    use std::sync::atomic::Ordering;
    use swarm_core::call::{self, CallCommand, CallLink, CallSignal};
    use swarm_core::video::Picture;
    use tokio::sync::{mpsc, watch};
    let tim = start("tim28").await;
    tim.handle.set_my_addresses(vec!["[::1]:9".to_string()], Vec::new());
    let desk = start("desk28").await;
    let tim_key = tim.identity.public_key_bytes();
    let nowhere = Peer { name: "tim".to_string(), address: "[::1]:9".to_string(), public_key: tim_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &nowhere).unwrap();
    desk.handle.add_peer_live(nowhere);
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen's desktop", || all_online(&desk, "tim", 1)).await;
    let phone = join_named_with_no_address(&desk, "phone28", "phone28").await;
    assert!(desk.handle.allow_relay(desk.handle.device().public_key_bytes(), true).unwrap());
    wait_for("tim online on the phone, through the desktop", || phone.handle.peer_status("tim").is_some_and(|s| s.online)).await;
    assert!(phone.handle.calls_go_through("tim"));
    let mut tim_rings = tim.calls.lock().unwrap().take().unwrap();

    let handle = phone.handle.call_handle("tim").unwrap();
    let mut to_tim = CallLink::attach(handle.commands, handle.budget).unwrap();
    let budget = to_tim.budget();
    let answered = tokio::spawn(async move { call::ring(&mut to_tim, false).await.unwrap().is_some().then_some(to_tim) });
    let (_, mut on_tim) = rung(&tim, &mut tim_rings, "tim").await;
    on_tim.accept(false).unwrap();
    let mut to_tim = tokio::time::timeout(Duration::from_secs(10), answered).await.unwrap().unwrap().expect("answered");
    let (phone_video, phone_attach) = mpsc::unbounded_channel();
    let (phone_sees, mut tims_started) = mpsc::unbounded_channel();
    to_tim.video_later(phone_attach, phone_sees);
    let (tim_video, tim_attach) = mpsc::unbounded_channel();
    let (tim_sees, mut phones_started) = mpsc::unbounded_channel();
    on_tim.video_later(tim_attach, tim_sees);
    let (phone_vias_tx, mut phone_vias) = mpsc::unbounded_channel();
    to_tim.vias_to(phone_vias_tx);
    let (tim_vias_tx, mut tim_vias) = mpsc::unbounded_channel();
    on_tim.vias_to(tim_vias_tx);
    let (phone_says, tim_says) = (to_tim.commands(), on_tim.commands());
    let (at_phone, at_tim) = (talk(to_tim), talk(on_tim));
    both_hear(&at_phone, &at_tim, "the phone and tim to hear each other").await;
    wait_for("room for video through the desktop", || budget.video_bps.load(Ordering::Relaxed) > 0).await;
    assert!(!budget.covered.load(Ordering::Relaxed), "nothing covered on the way");

    let picture = |shade: u8| Arc::new(Picture::rgb(320, 240, (0..320 * 240).flat_map(|i| [(i % 251) as u8, shade, 200]).collect()));
    let (phone_camera_tx, camera) = watch::channel(Some(picture(80)));
    let (phone_display, mut phone_shown) = watch::channel(None);
    phone_video.send(call::VideoIo { camera, display: phone_display, small: watch::channel(None).0, stats: Arc::default() }).unwrap();
    tokio::time::timeout(Duration::from_secs(10), phones_started.recv()).await.expect("tim told the phone's video started").unwrap();
    let (tim_camera_tx, tim_camera) = watch::channel(None);
    let (tim_display, mut tim_shown) = watch::channel(None);
    tim_video.send(call::VideoIo { camera: tim_camera, display: tim_display, small: watch::channel(None).0, stats: Arc::default() }).unwrap();
    tokio::time::timeout(Duration::from_secs(10), tim_shown.wait_for(|p| p.is_some())).await.expect("the phone's picture shown to tim").unwrap();

    tim_camera_tx.send(Some(picture(160))).unwrap();
    let _ = tims_started.try_recv();
    tokio::time::timeout(Duration::from_secs(10), phone_shown.wait_for(|p| p.is_some())).await.expect("tim's picture shown on the phone").unwrap();

    phone_says.send(CallCommand::Signal(CallSignal::Via { id: 1, signal: Box::new(CallSignal::Screen(true)) })).unwrap();
    match tokio::time::timeout(Duration::from_secs(10), tim_vias.recv()).await {
        Ok(Some((1, CallSignal::Screen(true)))) => {}
        _ => panic!("what's passed on not through to tim"),
    }
    tim_says.send(CallCommand::Signal(CallSignal::Via { id: 1, signal: Box::new(CallSignal::KeyframeRequest { small: true }) })).unwrap();
    tim_says.send(CallCommand::Signal(CallSignal::Via { id: 1, signal: Box::new(CallSignal::ShownIn { width: 640, height: 360, small_width: 160, small_height: 90 }) })).unwrap();
    let mut asked = Vec::new();
    while asked.len() < 2 {
        match tokio::time::timeout(Duration::from_secs(10), phone_vias.recv()).await {
            Ok(Some((1, signal @ (CallSignal::KeyframeRequest { .. } | CallSignal::ShownIn { .. })))) => asked.push(signal),
            Ok(Some(_)) => {}
            _ => panic!("what's asked not back to the phone"),
        }
    }
    assert!(matches!(asked[0], CallSignal::KeyframeRequest { small: true }) && matches!(asked[1], CallSignal::ShownIn { width: 640, .. }));
    both_hear(&at_phone, &at_tim, "the call to carry on").await;
    drop((phone_camera_tx, tim_camera_tx));
}

/// Simon makes an identity, Owen adds it
/// on his phone as "Simon" (Simon never adds him back), deletes it, and
/// straight away adds Simon's new identity, at the same address, under
/// the same name. Both of Owen's devices end with the new Simon alone,
/// by that name; Simon is asked both times; and once he adds Owen back,
/// both of Owen's devices reach him. Run with the new key sorting
/// before the old one and after it (a book is gone through in key order).
async fn a_contact_deleted_and_added_again_under_a_new_key(label: &str, new_sorts_first: bool) {
    use swarm_core::node::joining;
    use swarm_core::requests;
    let desk = start(&format!("desk-{label}")).await;
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir(&format!("phone-{label}"));
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as(&format!("phone-{label}"), Arc::new(joined.user), paths).await;
    wait_for("the desktop and phone to link", || desk.handle.devices().devices.iter().all(|d| d.online) && desk.handle.devices().devices.len() == 2).await;
    let paul_key = desk.identity.public_key_bytes();

    let (old_id, new_id) = loop {
        let (a, b) = (Identity::generate(), Identity::generate());
        if (b.public_key_bytes() < a.public_key_bytes()) == new_sorts_first {
            break (a, b);
        }
    };
    let (old_key, new_key) = (old_id.public_key_bytes(), new_id.public_key_bytes());
    let old = start_as(&format!("steve1-{label}"), Arc::new(old_id), dir(&format!("steve1-{label}"))).await;
    swarm_core::displayname::set(&old.paths.displayname(), &old.identity, "Simon").unwrap();
    let asked = |node: &TestNode| requests::pending(&node.paths.requests()).unwrap_or_default().iter().any(|r| r.public_key == paul_key);
    let with_key = |node: &TestNode, key: [u8; 32]| peers::load(&node.paths.peers()).unwrap_or_default().into_iter().filter(|p| p.public_key == key).map(|p| p.name).collect::<Vec<_>>();
    let all = |node: &TestNode| peers::load(&node.paths.peers()).unwrap_or_default().into_iter().map(|p| format!("{} {}", p.name, &base64::Engine::encode(&base64::engine::general_purpose::STANDARD, p.public_key)[..6])).collect::<Vec<_>>();

    add(&phone, &old, "Simon");
    wait_for("the desktop to have old Simon too", || with_key(&desk, old_key).len() == 1).await;
    wait_for("old Simon to be asked", || asked(&old)).await;

    peers::remove(&phone.paths.peers(), "Simon").unwrap();
    phone.handle.forget_contact("Simon", old_key, false);
    old.handle.shutdown().await;
    drop(old);
    let new = start_as(&format!("steve2-{label}"), Arc::new(new_id), dir(&format!("steve2-{label}"))).await;
    swarm_core::displayname::set(&new.paths.displayname(), &new.identity, "Simon").unwrap();
    add(&phone, &new, "Simon");

    let settled = |n: &TestNode| with_key(n, old_key).is_empty() && with_key(n, new_key).len() == 1;
    for _ in 0..150 {
        if settled(&desk) && settled(&phone) && asked(&new) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let report = format!(
        "phone has {:?}; desktop has {:?}; new Simon asked: {}; old key on desktop {:?}, on phone {:?}; new key on desktop {:?}, on phone {:?}",
        all(&phone), all(&desk), asked(&new), with_key(&desk, old_key), with_key(&phone, old_key), with_key(&desk, new_key), with_key(&phone, new_key)
    );
    eprintln!("[{label}] after re-adding: {report}");
    assert!(settled(&desk) && settled(&phone), "only the new Simon on both: {report}");
    assert_eq!(with_key(&phone, new_key), ["Simon"], "{report}");
    assert_eq!(with_key(&desk, new_key), ["Simon"], "the same name on the desktop: {report}");
    assert!(asked(&new), "new Simon to be asked: {report}");

    add(&new, &desk, "owen");
    wait_for("new Simon to reach owen", || new.handle.peer_status("owen").is_some_and(|s| s.online)).await;
    wait_for_long("the phone to reach new Simon", || phone.handle.peer_status("Simon").is_some_and(|s| s.online)).await;
    wait_for("the desktop to reach new Simon", || desk.handle.peer_status("Simon").is_some_and(|s| s.online)).await;
    phone.handle.send_message("Simon", "hello again").await.unwrap();
    wait_for("new Simon to have it", || said(&new, &paul_key).iter().any(|t| t == "hello again")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_contact_deleted_and_added_again_under_a_new_key_sorting_first() {
    a_contact_deleted_and_added_again_under_a_new_key("readd1", true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_contact_deleted_and_added_again_under_a_new_key_sorting_last() {
    a_contact_deleted_and_added_again_under_a_new_key("readd2", false).await;
}

/// The address set by hand (`myaddress`) is the user's: set on one
/// device it's on the other, the later change wins whichever device
/// made it, and clearing it clears it on both. A device that starts
/// up later takes what changed meanwhile. And a contact's "they see
/// you at" is where their connection saw this device's come from.
#[tokio::test(flavor = "multi_thread")]
async fn the_manual_address_is_the_same_on_every_device() {
    let desk = start("desk-manual").await;
    let phone = join_from(&desk, "phone-manual").await;
    wait_for("the desktop and phone to link", || desk.handle.devices().devices.iter().all(|d| d.online) && desk.handle.devices().devices.len() == 2).await;
    let items = |node: &TestNode| node.handle.manual_address().items;

    desk.handle.set_manual_address("203.0.113.7, home.example.org:5000").unwrap();
    wait_for("the phone to have it", || items(&phone) == ["203.0.113.7", "home.example.org:5000"]).await;
    assert!(desk.handle.set_manual_address("not an address").is_err());
    assert_eq!(items(&desk), ["203.0.113.7", "home.example.org:5000"], "a wrong one changes nothing");

    phone.handle.set_manual_address("198.51.100.2").unwrap();
    wait_for("the desktop to have the phone's", || items(&desk) == ["198.51.100.2"]).await;
    phone.handle.set_manual_address("").unwrap();
    wait_for("cleared on the desktop too", || items(&desk).is_empty()).await;

    let at = phone.handle.manual_address().at;
    swarm_core::myaddress::set(&phone.paths.manual_address(), vec!["192.0.2.1".into()], at).unwrap();
    assert_eq!(items(&phone), ["192.0.2.1"]);
    wait_for("the desktop to have the one set from the command line", || items(&desk) == ["192.0.2.1"]).await;

    let tim = start("tim-manual").await;
    let tim_key = tim.identity.public_key_bytes();
    add(&desk, &tim, "tim");
    add(&tim, &desk, "owen");
    wait_for("tim to reach owen", || desk.handle.peer_status("tim").is_some_and(|s| s.online)).await;
    wait_for("tim to have seen the desktop", || !desk.handle.seen_by(&tim_key).is_empty()).await;
    assert_eq!(desk.handle.seen_by(&tim_key), ["::1".parse::<std::net::IpAddr>().unwrap()]);
    assert!(desk.handle.seen_by(&[9; 32]).is_empty(), "nobody else's");
}

/// A contact arriving in another device's book isn't on this device's
/// list until it's been added. Anything looking at the book meanwhile
/// mustn't take that for this device having deleted them: the deletion
/// would be the newest word on them, and go to every device.
#[tokio::test(flavor = "multi_thread")]
async fn a_contact_on_its_way_from_another_device_isnt_taken_for_deleted() {
    use swarm_core::node::joining;
    let desk = start("desk-arriving").await;
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone-arriving");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone-arriving", Arc::new(joined.user), paths).await;
    wait_for("the desktop and phone to link", || desk.handle.devices().devices.iter().all(|d| d.online) && desk.handle.devices().devices.len() == 2).await;

    let tim = start("tim-arriving").await;
    let tim_key = tim.identity.public_key_bytes();
    add(&desk, &tim, "tim");
    let has_tim = |node: &TestNode| peers::load(&node.paths.peers()).unwrap_or_default().iter().any(|p| p.public_key == tim_key);
    for _ in 0..500 {
        if has_tim(&phone) || !has_tim(&desk) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(has_tim(&desk), "the desktop lost tim: the phone took him for deleted");
    assert!(has_tim(&phone), "the phone never got tim");
}

/// A name is a label: two contacts can share it, each is reached by
/// their key alone, and deleting one leaves the other.
#[tokio::test(flavor = "multi_thread")]
async fn two_contacts_of_one_name_are_two_people() {
    let alice = start("alice-same").await;
    let (first, second) = (start("bob-first").await, start("bob-second").await);
    add(&alice, &first, "bob");
    add(&alice, &second, "bob");
    add(&first, &alice, "alice");
    add(&second, &alice, "alice");
    let (one, two) = (first.identity.public_key_bytes(), second.identity.public_key_bytes());
    assert_eq!(peers::load(&alice.paths.peers()).unwrap().len(), 2);
    let (to_one, to_two) = (peers::handle_of(&one), peers::handle_of(&two));
    wait_for("both bobs online", || alice.handle.peer_status(&to_one).is_some_and(|s| s.online) && alice.handle.peer_status(&to_two).is_some_and(|s| s.online)).await;
    assert_eq!(alice.handle.label_of(&to_one), "bob");
    assert!(alice.handle.send_message("bob", "which?").await.is_err(), "never a guess");

    alice.handle.send_message(&to_one, "to the first").await.unwrap();
    alice.handle.send_message(&to_two, "to the second").await.unwrap();
    let alice_key = alice.identity.public_key_bytes();
    wait_for("each to have their own", || said(&first, &alice_key) == ["to the first"] && said(&second, &alice_key) == ["to the second"]).await;
    assert_eq!(said(&alice, &one), ["to the first"]);
    assert_eq!(said(&alice, &two), ["to the second"]);

    peers::remove(&alice.paths.peers(), &to_one).unwrap();
    alice.handle.forget_contact(&to_one, one, false);
    assert!(alice.handle.peer_status(&to_one).is_none());
    assert!(alice.handle.peer_status(&to_two).is_some_and(|s| s.online), "the other bob is untouched");
    assert_eq!(said(&alice, &two), ["to the second"]);
    assert_eq!(alice.handle.resolve("bob"), to_two, "only one now");
}

/// Two of a name reach the user's other devices as they are, and a
/// rename on one device reaches the rest.
#[tokio::test(flavor = "multi_thread")]
async fn names_are_shared_between_a_users_devices_as_labels() {
    use swarm_core::node::joining;
    let desk = start("desk-labels").await;
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone-labels");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone-labels", Arc::new(joined.user), paths).await;
    wait_for("the devices to link", || desk.handle.devices().devices.iter().all(|d| d.online) && desk.handle.devices().devices.len() == 2).await;

    let (one, two) = ([1u8; 32], [2u8; 32]);
    let bob = |key: [u8; 32]| Peer { name: "bob".to_string(), address: "[::1]:9".to_string(), public_key: key, cover_bits_per_second: None, cover_required: false };
    for key in [one, two] {
        peers::add(&desk.paths.peers(), &bob(key)).unwrap();
        desk.handle.add_peer_live(bob(key));
    }
    let name_of = |node: &TestNode, key: [u8; 32]| peers::load(&node.paths.peers()).unwrap_or_default().into_iter().find(|p| p.public_key == key).map(|p| p.name);
    wait_for("the phone to have both, as bob", || name_of(&phone, one).as_deref() == Some("bob") && name_of(&phone, two).as_deref() == Some("bob")).await;

    let handle = peers::handle_of(&two);
    peers::update(&desk.paths.peers(), &handle, "robert", "[::1]:9".to_string(), two).unwrap();
    desk.handle.update_peer_live(&handle, Peer { name: "robert".to_string(), ..bob(two) });
    wait_for("the rename to reach the phone", || name_of(&phone, two).as_deref() == Some("robert")).await;
    assert_eq!(name_of(&phone, one).as_deref(), Some("bob"));
    assert_eq!(phone.handle.label_of(&handle), "robert", "the running node has it too");
}

/// A rename on one device isn't undone by another that has the contact
/// named after themselves (added there without a name): that one takes
/// the new name and stops following theirs, rather than putting theirs
/// back, which would then have gone back to the first. Given what they
/// call themselves again on one device, the other follows it too.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_on_one_device_isnt_undone_by_another_following_their_name() {
    use swarm_core::node::joining;
    let desk = start("desk-follows").await;
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir("phone-follows");
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as("phone-follows", Arc::new(joined.user), paths).await;
    wait_for("the devices to link", || desk.handle.devices().devices.iter().all(|d| d.online) && desk.handle.devices().devices.len() == 2).await;

    let bob_id = Identity::generate();
    let key = bob_id.public_key_bytes();
    let bob = |name: &str| Peer { name: name.to_string(), address: "[::1]:9".to_string(), public_key: key, cover_bits_per_second: None, cover_required: false };
    peers::add(&desk.paths.peers(), &bob("bob")).unwrap();
    desk.handle.add_peer_live(bob("bob"));
    let name_of = |node: &TestNode| peers::load(&node.paths.peers()).unwrap_or_default().into_iter().find(|p| p.public_key == key).map(|p| p.name);
    wait_for("the phone to have bob", || name_of(&phone).as_deref() == Some("bob")).await;

    let published = phone.paths.dir().join("bob-says");
    swarm_core::displayname::set(&published, &bob_id, "Bobby").unwrap();
    let says = swarm_core::displayname::current(&published).unwrap().unwrap();
    swarm_core::displayname::cache_one(&phone.paths.displaynames(), key, &says).unwrap();
    swarm_core::settings::set_named_after_them(&phone.paths.settings(), &key, true).unwrap();
    wait_for("both to call him Bobby", || name_of(&phone).as_deref() == Some("Bobby") && name_of(&desk).as_deref() == Some("Bobby")).await;

    let handle = peers::handle_of(&key);
    peers::update(&desk.paths.peers(), &handle, "robert", "[::1]:9".to_string(), key).unwrap();
    desk.handle.update_peer_live(&handle, bob("robert"));
    wait_for("the rename to reach the phone", || name_of(&phone).as_deref() == Some("robert")).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(name_of(&phone).as_deref(), Some("robert"), "not put back to Bobby on the phone");
    assert_eq!(name_of(&desk).as_deref(), Some("robert"), "nor on the desktop");
    assert!(!swarm_core::settings::named_after_them(&phone.paths.settings(), &key), "named by hand now");

    peers::update(&desk.paths.peers(), &handle, "Bobby", "[::1]:9".to_string(), key).unwrap();
    desk.handle.update_peer_live(&handle, bob("Bobby"));
    wait_for("the phone to call him Bobby, and follow it", || name_of(&phone).as_deref() == Some("Bobby") && swarm_core::settings::named_after_them(&phone.paths.settings(), &key)).await;
}
