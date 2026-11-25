//! What links with nothing to do send (`tally`): two of one user's
//! devices and a contact, left alone. Not a pass or fail but a
//! measurement, so ignored; run with
//! `cargo test -p swarm-core --test idle_traffic -- --ignored --nocapture`
//! (`IDLE_SECS` for how long, 120 by default).

use std::sync::Arc;
use std::time::Duration;
use swarm_core::identity::Identity;
use swarm_core::node::{self, joining, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
}

fn dir(label: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("swarm-idle-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Paths::new(&dir)
}

async fn start_as(label: &str, identity: Arc<Identity>, paths: Paths) -> TestNode {
    let label = label.to_string();
    let log: node::Logger = Arc::new(move |line| eprintln!("[{label}] {line}"));
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log };
    let (handle, _calls) = node::start(identity.clone(), config).await.unwrap();
    handle.set_my_addresses(vec![format!("[::1]:{}", handle.local_addr().unwrap().port())], Vec::new());
    TestNode { handle, identity, paths }
}

async fn second_device(first: &TestNode, label: &str) -> TestNode {
    let code = first.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, label).await.unwrap();
    let paths = dir(label);
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    start_as(label, Arc::new(joined.user), paths).await
}

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

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn what_idle_links_send() {
    let desk = start_as("desk", Arc::new(Identity::generate()), dir("desk")).await;
    let phone = second_device(&desk, "phone").await;
    let laptop = start_as("laptop", Arc::new(Identity::generate()), dir("laptop")).await;
    add(&desk, &laptop, "alice");
    add(&laptop, &desk, "owen");
    for _ in 0..300 {
        if phone.handle.knows("alice") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(15)).await;
    let _ = swarm_core::tally::report();
    let secs: u64 = std::env::var("IDLE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
    tokio::time::sleep(Duration::from_secs(secs)).await;
    println!("\n{secs} s idle, three nodes (two devices of one user, one contact), all of them together:");
    for line in swarm_core::tally::report() {
        println!("  {line}");
    }
}
