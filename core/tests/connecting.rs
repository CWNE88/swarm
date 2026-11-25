//! How connections to a contact are made and kept: dialling one of their devices where
//! another of theirs answers.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use swarm_core::identity::Identity;
use swarm_core::node::{self, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    log: Arc<Mutex<Vec<String>>>,
    paths: Paths,
    _calls: tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>,
}

fn dir(label: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("swarm-connecting-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Paths::new(&dir)
}

/// A device of `identity`'s, its log kept for looking at.
async fn start_as(label: &str, identity: Arc<Identity>) -> TestNode {
    let paths = dir(label);
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let (kept, label, shown) = (lines.clone(), label.to_string(), std::env::var_os("SWARM_TEST_LOG").is_some());
    let log: node::Logger = Arc::new(move |line| {
        if shown {
            eprintln!("[{label}] {line}");
        }
        kept.lock().unwrap().push(line.to_string());
    });
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log };
    let (handle, _calls) = node::start(identity.clone(), config).await.unwrap();
    handle.set_my_addresses(vec![address(&handle)], Vec::new());
    TestNode { handle, identity, log: lines, paths, _calls }
}

fn address(handle: &NodeHandle) -> String {
    format!("[::1]:{}", handle.local_addr().unwrap().port())
}

/// `from` adds `to`'s user as `name`, at `to`'s address.
fn add(from: &TestNode, to: &TestNode, name: &str) {
    let peer = Peer { name: name.to_string(), address: address(&to.handle), public_key: to.identity.public_key_bytes(), cover_bits_per_second: None, cover_required: false };
    peers::add(&from.paths.peers(), &peer).unwrap();
    from.handle.add_peer_live(peer);
}

async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

fn online_entries(node: &TestNode, name: &str) -> Vec<String> {
    node.handle.same_person(name).into_iter().filter(|e| node.handle.peer_status(e).is_some_and(|s| s.online)).collect()
}

/// Owen's laptop says it can be reached at his desktop's address too (a
/// manual address forwarded to the desktop), and goes off. Alice's entry
/// for the laptop goes on looking for it, and the desktop answers there:
/// that's noted once, and the address is left out when dialling the
/// laptop after that, so the desktop isn't dialled over and over, and
/// stays online all the while.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_isnt_dialled_over_and_over_where_another_answers() {
    let alice = start_as("alice", Arc::new(Identity::generate())).await;
    let owen = Arc::new(Identity::generate());
    let desk = start_as("desk", owen.clone()).await;
    let laptop = start_as("laptop", owen.clone()).await;
    add(&alice, &desk, "owen");
    add(&desk, &alice, "alice");
    add(&laptop, &alice, "alice");
    wait_for("alice to reach both of owen's devices", || online_entries(&alice, "owen").len() == 2).await;
    let desk_entry = {
        let desk_device = swarm_core::peers::handle_of(&desk.handle.device().public_key_bytes());
        let short: String = desk_device.chars().filter(char::is_ascii_alphanumeric).take(6).collect();
        alice.handle.same_person("owen").into_iter().find(|e| e.ends_with(&format!("#{short}"))).expect("alice has an entry for the desk")
    };

    laptop.handle.set_my_addresses(vec![address(&laptop.handle), address(&desk.handle)], Vec::new());
    tokio::time::sleep(Duration::from_secs(2)).await;
    laptop.handle.shutdown().await;
    wait_for("alice to see the laptop gone", || online_entries(&alice, "owen") == vec![desk_entry.clone()]).await;

    let since = alice.handle.peer_status(&desk_entry).map(|s| s.since);
    alice.log.lock().unwrap().clear();
    let until = Instant::now() + Duration::from_secs(40);
    while Instant::now() < until {
        assert!(alice.handle.peer_status(&desk_entry).is_some_and(|s| s.online), "the desktop's connection dropped");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let misdials = alice.log.lock().unwrap().iter().filter(|l| l.contains("answered there")).count();
    assert!(misdials <= 1, "the desktop was dialled for the laptop {misdials} times in 40 s");
    assert_eq!(alice.handle.peer_status(&desk_entry).map(|s| s.since), since, "the desktop went offline and back");
}
