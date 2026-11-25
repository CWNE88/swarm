//! Calls between whole nodes: one carried on over a new connection when
//! a side moves.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use swarm_core::call::{self, CallLink, FRAME_SAMPLES};
use swarm_core::identity::Identity;
use swarm_core::node::{self, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};
use tokio::sync::{mpsc, oneshot};

struct TestNode {
    handle: NodeHandle,
    identity: Arc<Identity>,
    paths: Paths,
    calls: Mutex<Option<mpsc::UnboundedReceiver<node::CallEvent>>>,
    /// Everything it's logged.
    log: Arc<Mutex<Vec<String>>>,
}

impl TestNode {
    fn key(&self) -> [u8; 32] {
        self.identity.public_key_bytes()
    }

    fn address(&self) -> String {
        format!("[::1]:{}", self.handle.local_addr().unwrap().port())
    }

    fn logged(&self, what: &str) -> bool {
        self.logged_since(0, what)
    }

    /// How many lines are logged so far, for `logged_since`.
    fn lines(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// Whether `what` was logged after the first `from` lines.
    fn logged_since(&self, from: usize, what: &str) -> bool {
        self.log.lock().unwrap().iter().skip(from).any(|l| l.contains(what))
    }
}

fn dir(label: &str) -> Paths {
    let dir = std::env::temp_dir().join(format!("swarm-calls-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Paths::new(&dir)
}

async fn start(label: &str) -> TestNode {
    start_as(label, Arc::new(Identity::generate()), dir(label)).await
}

/// A node with this identity (a user's key, for one of their devices)
/// and folder.
async fn start_as(label: &str, identity: Arc<Identity>, paths: Paths) -> TestNode {
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let (kept, label) = (log.clone(), label.to_string());
    let logger: node::Logger = Arc::new(move |line: String| {
        if std::env::var_os("SWARM_TEST_LOG").is_some() {
            eprintln!("[{label}] {line}");
        }
        kept.lock().unwrap().push(line);
    });
    let config = NodeConfig { paths: paths.clone(), port: 0, cover_bits_per_second: None, log: logger };
    let (handle, call_events) = node::start(identity.clone(), config).await.unwrap();
    TestNode { handle, identity, paths, calls: Mutex::new(Some(call_events)), log }
}

fn add(a: &TestNode, b: &TestNode, name: &str) {
    let peer = Peer { name: name.to_string(), address: b.address(), public_key: b.key(), cover_bits_per_second: None, cover_required: false };
    peers::add(&a.paths.peers(), &peer).unwrap();
    a.handle.add_peer_live(peer);
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

/// Another of `first`'s user's devices, added with a code from it and
/// started, saying where it is.
async fn add_device(first: &TestNode, label: &str) -> TestNode {
    use swarm_core::node::joining;
    let code = first.handle.add_device_code().unwrap();
    let device = Identity::generate();
    let joined = joining::join(&code, &device, label).await.unwrap();
    let paths = dir(label);
    swarm_core::store::open_vault(paths.dir(), &joined.user.storage_keys());
    joining::settle(&paths, &joined.user, &device, &joined.list, &joined.by).unwrap();
    let node = start_as(label, Arc::new(joined.user), paths).await;
    node.handle.set_my_addresses(vec![node.address()], Vec::new());
    node
}

/// One side of a call, running: a steady tone in, and a count of what's
/// heard.
struct Side {
    heard: Arc<AtomicU64>,
    hang_up: Option<oneshot::Sender<()>>,
    running: tokio::task::JoinHandle<()>,
}

fn talk(link: CallLink) -> Side {
    let (mic_tx, mic_rx) = mpsc::unbounded_channel();
    let (speaker_tx, mut speaker_rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let (hang_up, hangup_rx) = oneshot::channel();
    let heard = Arc::new(AtomicU64::new(0));
    let count = heard.clone();
    tokio::spawn(async move {
        while speaker_rx.recv().await.is_some() {
            count.fetch_add(1, Ordering::Relaxed);
        }
    });
    tokio::spawn(async move {
        let tone: Vec<f32> = (0..FRAME_SAMPLES).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        while mic_tx.send(tone.clone()).is_ok() {
            tokio::time::sleep(call::FRAME_DURATION).await;
        }
    });
    let running = tokio::spawn(async move {
        let _ = call::run(link, mic_rx, speaker_tx, hangup_rx, None).await;
    });
    Side { heard, hang_up: Some(hang_up), running }
}

/// Both hear more of the other, within a few seconds.
async fn both_hear(a: &Side, b: &Side, what: &str) {
    let (a0, b0) = (a.heard.load(Ordering::Relaxed), b.heard.load(Ordering::Relaxed));
    wait_for(what, || a.heard.load(Ordering::Relaxed) > a0 + 25 && b.heard.load(Ordering::Relaxed) > b0 + 25).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_carries_on_when_a_side_moves() {
    let alice = start("alice").await;
    let bob = start("bob").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("both online", || {
        alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added) && bob.handle.peer_status("alice").is_some_and(|s| s.online && !s.not_added)
    })
    .await;

    let handle = alice.handle.call_handle("bob").unwrap();
    let mut to_bob = CallLink::attach(handle.commands, handle.budget).unwrap();
    let answered = tokio::spawn(async move { call::ring(&mut to_bob, false).await.unwrap().map(|_| to_bob) });
    let mut rings = bob.calls.lock().unwrap().take().unwrap();
    let from_alice = match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await.unwrap() {
        Some(node::CallEvent::Incoming { link, .. }) => link,
        _ => panic!("expected a ring"),
    };
    from_alice.accept(false).unwrap();
    let to_bob = answered.await.unwrap().expect("answered");
    let (mut a, b) = (talk(to_bob), talk(from_alice));
    both_hear(&a, &b, "the call to start").await;

    let elsewhere = vec!["[2001:db8::99]:1977".to_string()];
    alice.handle.set_my_addresses(elsewhere.clone(), Vec::new());
    bob.handle.set_my_addresses(elsewhere, Vec::new());
    wait_for("the call to be carried on", || alice.logged("call carried on") && bob.logged("call carried on")).await;
    assert!(alice.logged("this device moved") || bob.logged("this device moved"), "one of them dialled afresh");
    both_hear(&a, &b, "the call to go on").await;
    assert!(!a.running.is_finished() && !b.running.is_finished(), "neither call ended");

    a.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), b.running).await.expect("bob's side ends").unwrap();
}

/// A conference's signals between whole nodes: a ring into one arrives
/// with its roster, and a roster sent on a call's link reaches the other
/// side's front end, not the call.
#[tokio::test(flavor = "multi_thread")]
async fn a_conference_ring_and_its_roster_reach_the_front_end() {
    use swarm_core::conference::Roster;
    let alice = start("conf-alice").await;
    let bob = start("conf-bob").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("both online", || {
        alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added) && bob.handle.peer_status("alice").is_some_and(|s| s.online && !s.not_added)
    })
    .await;
    let handle = alice.handle.call_handle("bob").unwrap();
    let mut roster = Roster::start(&alice.identity);
    roster.add(&alice.identity, bob.key());
    let mut to_bob = CallLink::attach(handle.commands, handle.budget).unwrap();
    let sent = roster.clone();
    let answered = tokio::spawn(async move { call::ring_into(&mut to_bob, sent).await.unwrap().then_some(to_bob) });
    let mut rings = bob.calls.lock().unwrap().take().unwrap();
    let from_alice = match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await.unwrap() {
        Some(node::CallEvent::Incoming { link, video: false, conference: Some(got), .. }) => {
            assert_eq!(got, roster);
            link
        }
        _ => panic!("expected a ring into the conference"),
    };
    from_alice.accept(false).unwrap();
    let to_bob = answered.await.unwrap().expect("joined");
    let commands = to_bob.commands();
    let (a, b) = (talk(to_bob), talk(from_alice));
    both_hear(&a, &b, "the call to start").await;
    let carol = Identity::generate();
    roster.add(&alice.identity, carol.public_key_bytes());
    commands.send(call::CallCommand::Signal(call::CallSignal::Roster(roster.clone()))).unwrap();
    match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await.unwrap() {
        Some(node::CallEvent::Roster { peer_name, roster: got }) => {
            assert_eq!(bob.handle.label_of(&peer_name), "alice");
            assert_eq!(got, roster);
        }
        _ => panic!("expected the roster"),
    }
    both_hear(&a, &b, "the call to carry on").await;
    assert!(roster.remove(&alice.identity, carol.public_key_bytes()));
    commands.send(call::CallCommand::Signal(call::CallSignal::Roster(roster.clone()))).unwrap();
    match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await.unwrap() {
        Some(node::CallEvent::Roster { roster: got, .. }) => {
            assert_eq!(got.removed_by(&carol.public_key_bytes()), Some(alice.key()));
            assert_eq!(got.members(), vec![alice.key(), bob.key()]);
        }
        _ => panic!("expected the roster"),
    }
}

/// Control of a shared screen between whole nodes: what's asked and
/// answered, and the pointer and keys, reach the other side's front end
/// (not the call), every one and in the order sent, while the call goes
/// on.
#[tokio::test(flavor = "multi_thread")]
async fn control_of_a_screen_reaches_the_front_end_in_order() {
    use swarm_core::remote::{Control, Input};
    let alice = start("control-alice").await;
    let bob = start("control-bob").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("both online", || {
        alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added) && bob.handle.peer_status("alice").is_some_and(|s| s.online && !s.not_added)
    })
    .await;
    let handle = alice.handle.call_handle("bob").unwrap();
    let mut to_bob = CallLink::attach(handle.commands, handle.budget).unwrap();
    let answered = tokio::spawn(async move { call::ring(&mut to_bob, false).await.unwrap().map(|_| to_bob) });
    let mut rings = bob.calls.lock().unwrap().take().unwrap();
    let from_alice = match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await.unwrap() {
        Some(node::CallEvent::Incoming { link, .. }) => link,
        _ => panic!("expected a ring"),
    };
    from_alice.accept(false).unwrap();
    let to_bob = answered.await.unwrap().expect("answered");
    let commands = to_bob.commands();
    let (a, b) = (talk(to_bob), talk(from_alice));
    both_hear(&a, &b, "the call to start").await;

    let mut sent = vec![call::CallSignal::Control(Control::Ask)];
    for i in 0..300u16 {
        sent.push(call::CallSignal::Input(Input::Pointer { x: i * 200, y: 65535 - i * 200 }));
    }
    sent.push(call::CallSignal::Input(Input::Button { button: 0, down: true }));
    sent.push(call::CallSignal::Input(Input::Button { button: 0, down: false }));
    sent.push(call::CallSignal::Input(Input::Text("héllo".into())));
    sent.push(call::CallSignal::Control(Control::GiveBack));
    for signal in &sent {
        commands.send(call::CallCommand::Signal(signal.clone())).unwrap();
    }
    let mut got = Vec::new();
    while got.len() < sent.len() {
        match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await.expect("all of it arrives") {
            Some(node::CallEvent::Control { peer_name, signal }) => {
                assert_eq!(bob.handle.label_of(&peer_name), "alice");
                got.push(signal);
            }
            Some(_) => {}
            None => panic!("events ended"),
        }
    }
    assert_eq!(got, sent);
    both_hear(&a, &b, "the call to carry on").await;
}

/// A UDP relay in front of a node: what comes to `front` goes on to the
/// node from a socket of its own, and what the node sends back goes to
/// whoever last came to `front`, each way after `delay` (half a round
/// trip over the internet, say). While `down`, nothing goes either way —
/// a Wi-Fi roam, the address unchanged.
async fn relay(node: std::net::SocketAddr, delay: Duration, down: Arc<std::sync::atomic::AtomicBool>) -> std::net::SocketAddr {
    let front = Arc::new(tokio::net::UdpSocket::bind("[::1]:0").await.unwrap());
    let back = Arc::new(tokio::net::UdpSocket::bind("[::1]:0").await.unwrap());
    let at = front.local_addr().unwrap();
    let client = Arc::new(Mutex::new(None));
    let later = |socket: Arc<tokio::net::UdpSocket>| {
        let (tx, mut rx) = mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>, std::net::SocketAddr)>();
        tokio::spawn(async move {
            while let Some((at, packet, to)) = rx.recv().await {
                tokio::time::sleep_until(at).await;
                let _ = socket.send_to(&packet, to).await;
            }
        });
        tx
    };
    let (to_node, to_client) = (later(back.clone()), later(front.clone()));
    let (f, c, d) = (front.clone(), client.clone(), down.clone());
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((n, from)) = f.recv_from(&mut buf).await {
            *c.lock().unwrap() = Some(from);
            if !d.load(Ordering::Relaxed) {
                let _ = to_node.send((tokio::time::Instant::now() + delay, buf[..n].to_vec(), node));
            }
        }
    });
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((n, _)) = back.recv_from(&mut buf).await {
            let to = *client.lock().unwrap();
            if let (Some(to), false) = (to, down.load(Ordering::Relaxed)) {
                let _ = to_client.send((tokio::time::Instant::now() + delay, buf[..n].to_vec(), to));
            }
        }
    });
    at
}

/// A Wi-Fi roam: the link gone entirely for a moment, then back as it
/// was. Not a poor line: a video call sending at a high rate goes on at
/// that rate as soon as it's back, on the same connection — not cut as
/// though the link had become slow, and climbing back for a minute —
/// with a full picture first. Over a 40 ms round trip, gone for a second
/// and a half, then for four.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_goes_on_at_its_rate_after_the_link_drops_for_a_moment() {
    use swarm_core::call::{CallSignal, UNCOVERED_VIDEO_START};
    use swarm_core::video::{VideoPiece, UNCOVERED_PIECE};
    let alice = start("roam-alice").await;
    let bob = start("roam-bob").await;
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let delay = Duration::from_millis(20);
    let to_alice = relay(alice.address().parse().unwrap(), delay, down.clone()).await;
    let to_bob = relay(bob.address().parse().unwrap(), delay, down.clone()).await;
    alice.handle.set_my_addresses(vec![to_alice.to_string()], Vec::new());
    bob.handle.set_my_addresses(vec![to_bob.to_string()], Vec::new());
    for (a, b, name, at) in [(&alice, &bob, "bob", to_bob), (&bob, &alice, "alice", to_alice)] {
        let peer = Peer { name: name.to_string(), address: at.to_string(), public_key: b.key(), cover_bits_per_second: None, cover_required: false };
        peers::add(&a.paths.peers(), &peer).unwrap();
        a.handle.add_peer_live(peer);
    }
    wait_for("both online", || {
        alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added) && bob.handle.peer_status("alice").is_some_and(|s| s.online && !s.not_added)
    })
    .await;

    let handle = alice.handle.call_handle("bob").unwrap();
    let mut to_bob = CallLink::attach(handle.commands, handle.budget).unwrap();
    let answered = tokio::spawn(async move { call::ring(&mut to_bob, true).await.unwrap().map(|_| to_bob) });
    let mut rings = bob.calls.lock().unwrap().take().unwrap();
    let from_alice = match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await.unwrap() {
        Some(node::CallEvent::Incoming { link, .. }) => link,
        _ => panic!("expected a ring"),
    };
    from_alice.accept(true).unwrap();
    let to_bob = answered.await.unwrap().expect("answered");

    let budget = to_bob.budget();
    budget.video_on.store(true, Ordering::Relaxed);
    let received = from_alice.budget();
    for link in [to_bob.commands(), from_alice.commands()] {
        tokio::spawn(async move {
            while link.send(call::CallCommand::Signal(CallSignal::Audio(vec![7; 60]))).is_ok() {
                tokio::time::sleep(call::FRAME_DURATION).await;
            }
        });
    }
    let (from_bob, mut asked) = mpsc::unbounded_channel();
    to_bob.commands().send(call::CallCommand::Attach(from_bob)).unwrap();
    let full = Arc::new(AtomicU64::new(0));
    let (commands, rate, made) = (to_bob.commands(), budget.clone(), full.clone());
    tokio::spawn(async move {
        let mut frame = 0u32;
        loop {
            let mut key = false;
            while let Ok(signal) = asked.try_recv() {
                key |= signal == CallSignal::KeyframeRequest { small: false };
            }
            made.fetch_add(key as u64, Ordering::Relaxed);
            let bytes = rate.video_bps.load(Ordering::Relaxed) / 8 / 25;
            let count = bytes.div_ceil(UNCOVERED_PIECE).max(1);
            for index in 0..count {
                let piece = VideoPiece { frame, index: index as u16, count: count as u16, key, h265: false, small: false, data: vec![9; UNCOVERED_PIECE] };
                if commands.send(call::CallCommand::Signal(CallSignal::Video(piece))).is_err() {
                    return;
                }
            }
            frame += 1;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    });
    let bps = || budget.video_bps.load(Ordering::Relaxed) as u32;
    let started = std::time::Instant::now();
    while bps() < 8 * UNCOVERED_VIDEO_START && started.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let before = bps();
    for gone in [Duration::from_millis(1500), Duration::from_secs(4)] {
        let full_before = full.load(Ordering::Relaxed);
        let (alice_from, bob_from) = (alice.lines(), bob.lines());
        down.store(true, Ordering::Relaxed);
        tokio::time::sleep(gone).await;
        let during = bps();
        down.store(false, Ordering::Relaxed);
        let back = std::time::Instant::now();
        let got = received.received.load(Ordering::Relaxed);
        let mut seen = Vec::new();
        let mut first_second = 0;
        while back.elapsed() < Duration::from_secs(3) {
            tokio::time::sleep(Duration::from_millis(250)).await;
            seen.push(bps() / 1000);
            if seen.len() == 4 {
                first_second = (received.received.load(Ordering::Relaxed) - got) * 8;
            }
        }
        eprintln!(
            "gone {gone:?}: before {} kbit/s, during {} kbit/s, after (kbit/s, every quarter second): {seen:?}; {} kbit/s arrived in the first second back",
            before / 1000,
            during / 1000,
            first_second / 1000
        );
        assert!(
            !alice.logged_since(alice_from, "call traffic failed") && !bob.logged_since(bob_from, "call traffic failed") && !alice.logged_since(alice_from, "carried on"),
            "the same connection carries on"
        );
        assert!(seen.iter().all(|&kbps| kbps * 1000 >= before / 10 * 9), "the rate stays where it was: {} kbit/s before, then {seen:?}", before / 1000);
        assert!(first_second >= before as u64 / 10 * 8, "and it arrives at that rate at once: {} kbit/s of {}", first_second / 1000, before / 1000);
        assert!(full.load(Ordering::Relaxed) > full_before, "a full picture first, once it's back");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn video_added_to_a_voice_call_partway_is_seen_on_the_other_side() {
    use swarm_core::video::Picture;
    use tokio::sync::watch;
    let alice = start("alice-later").await;
    let bob = start("bob-later").await;
    add(&alice, &bob, "bob");
    add(&bob, &alice, "alice");
    wait_for("both online", || {
        alice.handle.peer_status("bob").is_some_and(|s| s.online && !s.not_added) && bob.handle.peer_status("alice").is_some_and(|s| s.online && !s.not_added)
    })
    .await;

    let handle = alice.handle.call_handle("bob").unwrap();
    let mut to_bob = CallLink::attach(handle.commands, handle.budget).unwrap();
    let answered = tokio::spawn(async move { call::ring(&mut to_bob, false).await.unwrap().map(|_| to_bob) });
    let mut rings = bob.calls.lock().unwrap().take().unwrap();
    let mut from_alice = match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await.unwrap() {
        Some(node::CallEvent::Incoming { link, video: false, .. }) => link,
        _ => panic!("expected a voice call ringing"),
    };
    from_alice.accept(false).unwrap();
    let mut to_bob = answered.await.unwrap().expect("answered");
    let (alice_video, alice_attach) = mpsc::unbounded_channel();
    let (alice_sees, _) = mpsc::unbounded_channel();
    to_bob.video_later(alice_attach, alice_sees);
    let (bob_video, bob_attach) = mpsc::unbounded_channel();
    let (bob_sees, mut theirs_started) = mpsc::unbounded_channel();
    from_alice.video_later(bob_attach, bob_sees);
    let (a, b) = (talk(to_bob), talk(from_alice));
    both_hear(&a, &b, "the call to start").await;

    let picture = Arc::new(Picture::rgb(320, 240, (0..320 * 240).flat_map(|i| [(i % 251) as u8, 80, 200]).collect()));
    let (camera_tx, camera) = watch::channel(Some(picture));
    let (alice_display, _alice_shown) = watch::channel(None);
    alice_video.send(call::VideoIo { camera, display: alice_display, small: watch::channel(None).0, stats: Arc::default() }).unwrap();

    tokio::time::timeout(Duration::from_secs(10), theirs_started.recv()).await.expect("bob told alice's video started").unwrap();
    let (_bob_camera_tx, bob_camera) = watch::channel(None);
    let (bob_display, mut bob_shown) = watch::channel(None);
    bob_video.send(call::VideoIo { camera: bob_camera, display: bob_display, small: watch::channel(None).0, stats: Arc::default() }).unwrap();
    tokio::time::timeout(Duration::from_secs(10), bob_shown.wait_for(|p| p.is_some())).await.expect("alice's picture shown to bob").unwrap();
    drop(camera_tx);
}

/// One of a user's devices calls another, over the link between them:
/// only that one rings, named as the device calling; both hear each
/// other; the call carries on when both move; hanging up ends both.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_calls_another_of_their_devices() {
    let desk = start("desk").await;
    desk.handle.set_my_addresses(vec![desk.address()], Vec::new());
    let phone = add_device(&desk, "phone").await;
    let laptop = add_device(&desk, "laptop").await;
    let (desk_key, phone_key) = (desk.handle.device().public_key_bytes(), phone.handle.device().public_key_bytes());
    let linked = |node: &TestNode| node.handle.devices().devices.iter().all(|d| d.online || d.this);
    wait_for("the three to link", || linked(&desk) && linked(&phone) && linked(&laptop)).await;

    let phone_call = node::devices::call_name(&phone_key);
    assert!(desk.handle.peer_status(&phone_call).is_some_and(|s| s.online));
    let handle = desk.handle.call_handle(&phone_call).unwrap();
    let mut to_phone = CallLink::attach(handle.commands, handle.budget).unwrap();
    let answered = tokio::spawn(async move { call::ring(&mut to_phone, false).await.unwrap().map(|_| to_phone) });
    let mut rings = phone.calls.lock().unwrap().take().unwrap();
    let from_desk = match tokio::time::timeout(Duration::from_secs(10), rings.recv()).await.unwrap() {
        Some(node::CallEvent::Incoming { from, link, .. }) => {
            assert_eq!(node::devices::call_device(&from), Some(desk_key), "named as the desktop");
            link
        }
        _ => panic!("expected a ring"),
    };
    from_desk.accept(false).unwrap();
    let to_phone = answered.await.unwrap().expect("answered");
    let (mut a, b) = (talk(to_phone), talk(from_desk));
    both_hear(&a, &b, "the call to start").await;
    assert!(laptop.calls.lock().unwrap().as_mut().unwrap().try_recv().is_err(), "the laptop wasn't rung");

    let elsewhere = vec!["[2001:db8::99]:1977".to_string()];
    desk.handle.set_my_addresses(elsewhere.clone(), Vec::new());
    phone.handle.set_my_addresses(elsewhere, Vec::new());
    wait_for("the call to be carried on", || desk.logged("call carried on") && phone.logged("call carried on")).await;
    both_hear(&a, &b, "the call to go on").await;
    assert!(!a.running.is_finished() && !b.running.is_finished(), "neither call ended");

    a.hang_up.take().unwrap().send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), b.running).await.expect("the phone's side ends").unwrap();
}
