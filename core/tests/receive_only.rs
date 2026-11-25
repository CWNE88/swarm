//! A device set to receive connections only (`NodeHandle::set_receive_only`)
//! dials no one with its screen off (or no screen, as here): a contact
//! it's added hears nothing from it till they dial it, and then
//! everything goes over their connection. The screen on, it dials.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use swarm_core::identity::Identity;
use swarm_core::node::{self, InUse, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
    log: Arc<Mutex<Vec<String>>>,
}

fn dir(label: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("swarm-receive-only-test-{label}-{}", std::process::id()));
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
    for _ in 0..300 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_receiving_only_dials_no_one_and_takes_their_connection() {
    let phone = start("phone").await;
    let desk = start("desk").await;
    assert!(phone.handle.set_receive_only(true).unwrap());
    assert!(phone.handle.receiving_only());
    assert!(!phone.handle.set_receive_only(true).unwrap(), "already so");
    assert!(phone.handle.devices().devices.iter().all(|d| d.receive_only == d.this), "said of this device only");
    let desk_key = desk.identity.public_key_bytes();
    let heard_before = desk.log.lock().unwrap().len();
    add(&phone, &desk, "desk");
    phone.handle.send_message("desk", "waits for them").await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(phone.handle.connection_to(&desk_key).is_none(), "nothing dialled");
    assert_eq!(desk.log.lock().unwrap().len(), heard_before, "the desk heard nothing: {:?}", desk.log.lock().unwrap());

    add(&desk, &phone, "phone");
    wait_for("the desk's connection", || matches!(phone.handle.connection_to(&desk_key), Some(InUse::Direct { dialled: false, .. }))).await;
    let phone_key = phone.identity.public_key_bytes();
    let desk_has = |text: &str| swarm_core::history::all(&desk.paths.history(), &phone_key).unwrap_or_default().iter().any(|e| e.text == text);
    wait_for("the waiting message", || desk_has("waits for them")).await;
    phone.handle.send_message("desk", "and this one").await.unwrap();
    wait_for("a message sent over it", || desk_has("and this one")).await;

    let shop = start("shop").await;
    assert!(shop.handle.set_receive_only(true).unwrap());
    let shop_key = shop.identity.public_key_bytes();
    add(&phone, &shop, "shop");
    add(&shop, &phone, "phone");
    node::set_screen_on(false);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(phone.handle.connection_to(&shop_key).is_none(), "neither dials with the screen off");
    node::set_screen_on(true);
    wait_for("the screen coming on to have them dialled", || phone.handle.connection_to(&shop_key).is_some()).await;
    node::set_screen_on(false);

    assert!(phone.handle.set_receive_only(false).unwrap());
    assert!(!phone.handle.receiving_only());
    assert!(phone.log.lock().unwrap().iter().any(|l| l.contains("this device dials again")) || {
        tokio::time::sleep(Duration::from_secs(2)).await;
        phone.log.lock().unwrap().iter().any(|l| l.contains("this device dials again"))
    });
}
