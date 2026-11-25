//! A receive limit (`NodeHandle::set_receive_limit`): the most a device
//! wants sent to it. Set mid-connection, a contact is told at once, and
//! the cover it sends there comes down under it.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use swarm_core::identity::Identity;
use swarm_core::node::{self, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
    log: Arc<Mutex<Vec<String>>>,
}

fn dir(label: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("swarm-receive-limit-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Paths::new(&dir)
}

async fn start(label: &str) -> TestNode {
    let (identity, paths) = (Arc::new(Identity::generate()), dir(label));
    let log = Arc::new(Mutex::new(Vec::new()));
    let (kept, label) = (log.clone(), label.to_string());
    let logger: node::Logger = Arc::new(move |line| {
        if std::env::var_os("SWARM_TEST_LOG").is_some() {
            eprintln!("[{label}] {line}");
        }
        kept.lock().unwrap().push(line);
    });
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log: logger };
    let (handle, _calls) = node::start(identity.clone(), config).await.unwrap();
    handle.set_my_addresses(vec![format!("[::1]:{}", handle.local_addr().unwrap().port())], Vec::new());
    TestNode { handle, identity, paths, log }
}

fn add(from: &TestNode, to: &TestNode, name: &str, cover: Option<u64>) {
    let peer = Peer {
        name: name.to_string(),
        address: format!("[::1]:{}", to.handle.local_addr().unwrap().port()),
        public_key: to.identity.public_key_bytes(),
        cover_bits_per_second: cover,
        cover_required: false,
    };
    peers::add(&from.paths.peers(), &peer).unwrap();
    from.handle.add_peer_live(peer);
}

async fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..300 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_receive_limit_set_mid_connection_brings_a_contacts_cover_under_it() {
    let phone = start("phone").await;
    let desk = start("desk").await;
    add(&phone, &desk, "desk", None);
    add(&desk, &phone, "phone", Some(2_000_000));
    let sends = |n: &TestNode| n.handle.peer_status("phone").and_then(|s| s.cover).and_then(|c| c.sends);
    wait_for("the desk's 2m cover to the phone", || sends(&desk) == Some(2_000_000)).await;

    phone.handle.set_receive_limit(Some(1_000_000)).unwrap();
    assert_eq!(phone.handle.receive_limit(), Some(1_000_000));
    wait_for("the desk to hear of it", || desk.log.lock().unwrap().iter().any(|l| l.contains("asks for no more than 1m"))).await;
    wait_for("the desk's cover under it", || sends(&desk) == Some(900_000)).await;

    phone.handle.set_receive_limit(None).unwrap();
    wait_for("the desk to hear it's gone", || desk.log.lock().unwrap().iter().any(|l| l.contains("no longer limits"))).await;
}
