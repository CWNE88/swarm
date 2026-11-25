//! A contact is deleted when, and only when, the user deletes them: on
//! every one of their devices, for good, and whatever else goes wrong,
//! nobody else is.

use std::sync::Arc;
use std::time::Duration;
use swarm_core::identity::Identity;
use swarm_core::node::{self, joining, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
    _calls: tokio::sync::mpsc::UnboundedReceiver<node::CallEvent>,
}

fn dir(label: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("swarm-deleting-test-{label}-{}", std::process::id()));
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
    TestNode { handle, identity, paths, _calls }
}

async fn start(label: &str) -> TestNode {
    start_as(label, Arc::new(Identity::generate()), dir(label)).await
}

fn linked(node: &TestNode) -> bool {
    let devices = node.handle.devices().devices;
    devices.len() == 2 && devices.iter().all(|d| d.online)
}

/// One user's two devices, linked.
async fn two_devices(label: &str) -> (TestNode, TestNode) {
    let desk = start(&format!("{label}-desk")).await;
    let code = desk.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, "phone").await.unwrap();
    let paths = dir(&format!("{label}-phone"));
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let phone = start_as(&format!("{label}-phone"), Arc::new(joined.user), paths).await;
    wait_for("the devices to link", || linked(&desk) && linked(&phone)).await;
    (desk, phone)
}

/// A device started again on the folder it had.
async fn restart(label: &str, node: TestNode) -> TestNode {
    let (identity, paths) = (node.identity.clone(), node.paths.clone());
    node.handle.shutdown().await;
    start_as(label, identity, paths).await
}

fn someone(name: &str, key: [u8; 32]) -> Peer {
    Peer { name: name.to_string(), address: "[::1]:9".to_string(), public_key: key, cover_bits_per_second: None, cover_required: false }
}

fn add(node: &TestNode, peer: &Peer) {
    peers::add(&node.paths.peers(), peer).unwrap();
    node.handle.add_peer_live(peer.clone());
}

fn delete(node: &TestNode, name: &str, key: [u8; 32]) {
    peers::remove(&node.paths.peers(), name).unwrap();
    node.handle.forget_contact(name, key, false);
}

fn has(node: &TestNode, key: &[u8; 32]) -> bool {
    peers::load(&node.paths.peers()).unwrap_or_default().iter().any(|p| p.public_key == *key)
}

fn name_of(node: &TestNode, key: &[u8; 32]) -> Option<String> {
    peers::load(&node.paths.peers()).unwrap_or_default().into_iter().find(|p| p.public_key == *key).map(|p| p.name)
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

/// Long enough for several of the links' two-second looks at the book.
async fn a_few_looks() {
    tokio::time::sleep(Duration::from_secs(7)).await;
}

/// `who`, known to both devices.
async fn shared_contact(desk: &TestNode, phone: &TestNode, name: &str) -> [u8; 32] {
    let key = Identity::generate().public_key_bytes();
    add(desk, &someone(name, key));
    wait_for("the phone to have the contact", || has(phone, &key)).await;
    key
}

/// The contacts list damaged in some way on the desktop: nobody's been
/// deleted, on either device, and starting again gets them back.
async fn damaged_list_deletes_nobody(label: &str, damage: impl FnOnce(&std::path::Path)) {
    let (desk, phone) = two_devices(label).await;
    let key = shared_contact(&desk, &phone, "tim").await;
    a_few_looks().await;
    damage(&desk.paths.peers());
    a_few_looks().await;
    assert!(has(&phone, &key), "the phone deleted tim because the desktop's list was damaged");
    let desk = restart(&format!("{label}-desk"), desk).await;
    wait_for("the devices to link again", || linked(&desk) && linked(&phone)).await;
    a_few_looks().await;
    assert!(has(&phone, &key), "the phone deleted tim after the desktop started again");
    wait_for("the desktop to have tim again", || has(&desk, &key)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_contacts_file_deletes_nobody() {
    damaged_list_deletes_nobody("missing", |path| std::fs::remove_file(path).unwrap()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_contacts_file_deletes_nobody() {
    damaged_list_deletes_nobody("empty", |path| std::fs::write(path, "").unwrap()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_contacts_file_that_wont_open_deletes_nobody() {
    damaged_list_deletes_nobody("garbled", |path| std::fs::write(path, "bm90IGEgc2VhbGVkIGxpbmU=\n").unwrap()).await;
}

/// Stopped after saving a book's line and before adding its contact to
/// the list. The next start mustn't read that as this device having
/// deleted them; it finishes the job.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_part_way_through_a_book_deletes_nobody() {
    let (desk, phone) = two_devices("stopped").await;
    let (phone_identity, phone_paths) = (phone.identity.clone(), phone.paths.clone());
    phone.handle.shutdown().await;
    let key = Identity::generate().public_key_bytes();
    add(&desk, &someone("sam", key));
    let text = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key);
    let line = |paths: &Paths| swarm_core::store::read(paths.dir().join("peersbook")).unwrap_or_default().lines().find(|l| l.starts_with(&text)).map(str::to_string);
    wait_for("the desktop's book to have sam", || line(&desk.paths).is_some()).await;
    let mut book = swarm_core::store::read(phone_paths.dir().join("peersbook")).unwrap_or_default();
    book.push_str(&line(&desk.paths).unwrap());
    book.push('\n');
    swarm_core::store::write_atomic(&phone_paths.dir().join("peersbook"), book).unwrap();

    let phone = start_as("stopped-phone", phone_identity, phone_paths).await;
    wait_for("the devices to link again", || linked(&desk) && linked(&phone)).await;
    a_few_looks().await;
    assert!(has(&desk, &key), "the desktop deleted sam: the phone took him for deleted");
    wait_for("the phone to finish adding sam", || has(&phone, &key)).await;
}

/// A device off for a while, and contacts added meanwhile: none of
/// them is lost.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_coming_back_deletes_nobody() {
    let (desk, phone) = two_devices("back").await;
    let older = shared_contact(&desk, &phone, "tim").await;
    let (phone_identity, phone_paths) = (phone.identity.clone(), phone.paths.clone());
    phone.handle.shutdown().await;
    let newer: Vec<[u8; 32]> = (0..5).map(|_| Identity::generate().public_key_bytes()).collect();
    for (i, key) in newer.iter().enumerate() {
        add(&desk, &someone(&format!("new{i}"), *key));
    }
    a_few_looks().await;
    let phone = start_as("back-phone", phone_identity, phone_paths).await;
    wait_for("the devices to link again", || linked(&desk) && linked(&phone)).await;
    for key in &newer {
        wait_for("the phone to have the new contacts", || has(&phone, key)).await;
    }
    a_few_looks().await;
    for key in newer.iter().chain([&older]) {
        assert!(has(&desk, key) && has(&phone, key), "lost a contact: on the desktop {}, on the phone {}", has(&desk, key), has(&phone, key));
    }
}

/// Deleted while another device is off: gone from that one too when it
/// comes back, and it doesn't bring them back.
#[tokio::test(flavor = "multi_thread")]
async fn deleted_while_a_device_was_off_stays_deleted() {
    let (desk, phone) = two_devices("off").await;
    let key = shared_contact(&desk, &phone, "tim").await;
    a_few_looks().await;
    let (phone_identity, phone_paths) = (phone.identity.clone(), phone.paths.clone());
    phone.handle.shutdown().await;
    delete(&desk, "tim", key);
    let phone = start_as("off-phone", phone_identity, phone_paths).await;
    wait_for("the phone to delete tim too", || !has(&phone, &key)).await;
    a_few_looks().await;
    assert!(!has(&desk, &key) && !has(&phone, &key), "brought back: on the desktop {}, on the phone {}", has(&desk, &key), has(&phone, &key));
}

/// Deleted on one device while another is off, and added again on that
/// other later on: the later word stands when they next meet.
#[tokio::test(flavor = "multi_thread")]
async fn added_again_after_a_deletion_elsewhere_stays_added() {
    use base64::Engine as _;
    let (desk, phone) = two_devices("again").await;
    let key = shared_contact(&desk, &phone, "tim").await;
    a_few_looks().await;
    let (phone_identity, phone_paths) = (phone.identity.clone(), phone.paths.clone());
    phone.handle.shutdown().await;
    delete(&desk, "tim", key);
    let text = base64::engine::general_purpose::STANDARD.encode(key);
    let gone_at = |paths: &Paths| -> Option<u64> {
        let book = swarm_core::store::read(paths.dir().join("peersbook")).ok()?;
        let line = book.lines().find(|l| l.starts_with(&text))?.to_string();
        let mut fields = line.split('\t');
        fields.next();
        fields.next()?.parse().ok()
    };
    wait_for("the desktop's deletion to be in its book", || gone_at(&desk.paths).is_some()).await;
    let later = gone_at(&desk.paths).unwrap() + 10;
    let book = swarm_core::store::read(phone_paths.dir().join("peersbook")).unwrap();
    let book: String = book.lines().map(|l| if l.starts_with(&text) { format!("{text}\t{later}\tyes\ttim\t[::1]:9\n") } else { format!("{l}\n") }).collect();
    swarm_core::store::write_atomic(&phone_paths.dir().join("peersbook"), book).unwrap();

    let phone = start_as("again-phone", phone_identity, phone_paths).await;
    wait_for("the devices to link again", || linked(&desk) && linked(&phone)).await;
    wait_for("the desktop to have tim again", || has(&desk, &key)).await;
    a_few_looks().await;
    assert!(has(&desk, &key) && has(&phone, &key), "the older deletion won: on the desktop {}, on the phone {}", has(&desk, &key), has(&phone, &key));
}

/// A deletion and additions going on at the same moment on two devices
/// touch only who they're about.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_one_while_another_device_adds_others() {
    let (desk, phone) = two_devices("race").await;
    let doomed: Vec<[u8; 32]> = (0..6).map(|_| Identity::generate().public_key_bytes()).collect();
    for (i, key) in doomed.iter().enumerate() {
        add(&desk, &someone(&format!("old{i}"), *key));
    }
    for key in &doomed {
        wait_for("the phone to have them", || has(&phone, key)).await;
    }
    a_few_looks().await;
    let added: Vec<[u8; 32]> = (0..6).map(|_| Identity::generate().public_key_bytes()).collect();
    for (i, (old, new)) in doomed.iter().zip(&added).enumerate() {
        delete(&phone, &format!("old{i}"), *old);
        add(&desk, &someone(&format!("new{i}"), *new));
    }
    for (old, new) in doomed.iter().zip(&added) {
        wait_for("the deletions and additions to reach both", || !has(&desk, old) && !has(&phone, old) && has(&desk, new) && has(&phone, new)).await;
    }
    a_few_looks().await;
    for (old, new) in doomed.iter().zip(&added) {
        assert!(!has(&desk, old) && !has(&phone, old), "a deleted contact came back");
        assert!(has(&desk, new) && has(&phone, new), "a new contact went");
    }
}

/// A deletion arriving doesn't get undone by the device's own look at
/// its list part-way through taking it in.
#[tokio::test(flavor = "multi_thread")]
async fn many_deletions_at_once_all_stay_deleted() {
    let (desk, phone) = two_devices("many").await;
    let keys: Vec<[u8; 32]> = (0..40).map(|_| Identity::generate().public_key_bytes()).collect();
    for (i, key) in keys.iter().enumerate() {
        add(&desk, &someone(&format!("c{i}"), *key));
    }
    for key in &keys {
        wait_for("the phone to have them all", || has(&phone, key)).await;
    }
    a_few_looks().await;
    for (i, key) in keys.iter().enumerate() {
        delete(&desk, &format!("c{i}"), *key);
    }
    for key in &keys {
        wait_for("the phone to delete them all", || !has(&phone, key)).await;
    }
    a_few_looks().await;
    assert!(keys.iter().all(|k| !has(&desk, k) && !has(&phone, k)), "someone deleted came back");
}

/// Deleted with Swarm not running here (`swarm contact remove`): the
/// other devices are told when it next runs.
#[tokio::test(flavor = "multi_thread")]
async fn deleted_with_nothing_running_reaches_the_other_devices() {
    let (desk, phone) = two_devices("cold").await;
    let key = shared_contact(&desk, &phone, "tim").await;
    a_few_looks().await;
    let (phone_identity, phone_paths) = (phone.identity.clone(), phone.paths.clone());
    phone.handle.shutdown().await;
    peers::remove(&phone_paths.peers(), "tim").unwrap();
    swarm_core::forget::contact(&phone_paths, &key, false);
    swarm_core::forget::from_book(&phone_paths, &key, "tim");
    let phone = start_as("cold-phone", phone_identity, phone_paths).await;
    wait_for("the desktop to delete tim too", || !has(&desk, &key)).await;
    a_few_looks().await;
    assert!(!has(&phone, &key), "brought back on the phone");
}

/// A contact given a new key is one person gone and another come, on
/// both devices, and nobody else is touched. A rename is neither.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_key_replaces_only_that_contact_and_a_rename_deletes_nobody() {
    let (desk, phone) = two_devices("rekey").await;
    let tim = shared_contact(&desk, &phone, "tim").await;
    let sam = shared_contact(&desk, &phone, "sam").await;
    a_few_looks().await;

    peers::update(&desk.paths.peers(), "sam", "samuel", "[::1]:9".to_string(), sam).unwrap();
    desk.handle.update_peer_live("sam", peers::load(&desk.paths.peers()).unwrap().into_iter().find(|p| p.public_key == sam).unwrap());
    assert_eq!(name_of(&desk, &sam).as_deref(), Some("samuel"));
    a_few_looks().await;
    assert!(has(&desk, &sam) && has(&phone, &sam) && has(&desk, &tim) && has(&phone, &tim), "a rename deleted someone");

    let new = Identity::generate().public_key_bytes();
    peers::update(&desk.paths.peers(), "tim", "tim", "[::1]:9".to_string(), new).unwrap();
    desk.handle.update_peer_live("tim", peers::load(&desk.paths.peers()).unwrap().into_iter().find(|p| p.public_key == new).unwrap());
    wait_for("the phone to swap the key", || has(&phone, &new) && !has(&phone, &tim)).await;
    a_few_looks().await;
    assert!(has(&desk, &new) && !has(&desk, &tim), "the old key's back on the desktop, or the new one went");
    assert!(has(&phone, &new) && !has(&phone, &tim), "the old key's back on the phone, or the new one went");
    assert!(has(&desk, &sam) && has(&phone, &sam), "someone else was touched");
}
