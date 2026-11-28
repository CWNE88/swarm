use super::*;
use crate::identity::Identity;
use crate::transport;

fn post(created_at: u64, visibility: Visibility, text: &str) -> Post {
    Post { created_at, visibility, signature: [0u8; 64], text: text.to_string() }
}

/// A connected pair: (dialer's side, acceptor's side).
async fn pair() -> (AuthenticatedConnection, AuthenticatedConnection) {
    let a_identity = Identity::generate();
    let b_identity = Identity::generate();
    let a_key = a_identity.public_key_bytes();
    let a_endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let a_addr = a_endpoint.local_addr().unwrap();
    let b_endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let accept = tokio::spawn(async move {
        let conn = transport::accept(&a_endpoint, crate::transport::Keys::new_device(&a_identity)).await.unwrap();
        (conn, a_endpoint)
    });
    let dialer = transport::dial(&b_endpoint, a_addr, crate::transport::Keys::new_device(&b_identity), a_key).await.unwrap();
    let (acceptor, a_endpoint) = accept.await.unwrap();
    std::mem::forget(a_endpoint);
    std::mem::forget(b_endpoint);
    (dialer, acceptor)
}

/// Each end of a connection knows the other's user and device keys,
/// as proved in its `Hello`.
#[tokio::test]
async fn each_end_knows_the_others_user_and_device() {
    let (user_a, device_a, user_b, device_b) = (Identity::generate(), Identity::generate(), Identity::generate(), Identity::generate());
    let a_endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let a_addr = a_endpoint.local_addr().unwrap();
    let b_endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let (a_user, a_device) = (user_a.public_key_bytes(), device_a.public_key_bytes());
    let accept = tokio::spawn(async move { transport::accept(&a_endpoint, transport::Keys { user: &user_a, device: &device_a }).await.map(|c| (c.remote_user, c.remote_device)).unwrap() });
    let dialer = transport::dial(&b_endpoint, a_addr, transport::Keys { user: &user_b, device: &device_b }, a_user).await.unwrap();
    assert_eq!((dialer.remote_user, dialer.remote_device), (a_user, a_device));
    assert_eq!(accept.await.unwrap(), (user_b.public_key_bytes(), device_b.public_key_bytes()));
}

#[test]
fn a_device_that_cant_prove_itself_is_refused() {
    let (user, other, device) = (Identity::generate(), Identity::generate(), Identity::generate());
    let noise = user.x25519_public_key_bytes();
    let keys = transport::Keys { user: &user, device: &device };
    let good = DeviceProof::mine(keys, &[1; 32]);
    assert_eq!(good.fails(&noise, &[1; 32]), None);
    assert!(good.fails(&other.x25519_public_key_bytes(), &[1; 32]).is_some(), "another key connected");
    assert!(good.fails(&noise, &[2; 32]).is_some(), "signed for another connection");
    let signed_by_another = DeviceProof { cert: crate::identity::device_cert(&other, &device.public_key_bytes()), ..good.clone() };
    assert!(signed_by_another.fails(&noise, &[1; 32]).is_some(), "not this user's device");
    let not_holding_it = DeviceProof { proof: crate::identity::device_proof(&other, &[1; 32]), ..good };
    assert!(not_holding_it.fails(&noise, &[1; 32]).is_some(), "someone else signed for the device");
}

/// Serves a stranger connection with `serve_connection`, on its own
/// task; returns the dialer's side and what the stranger side saw
/// introduced.
async fn stranger_pair(
    posts: Vec<Post>,
    name: Option<DisplayName>,
) -> (AuthenticatedConnection, Identity, tokio::sync::mpsc::UnboundedReceiver<[u8; 32]>) {
    let listener_identity = Identity::generate();
    let dialer_identity = Identity::generate();
    let listener_key = listener_identity.public_key_bytes();
    let endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut conn = transport::accept(&endpoint, crate::transport::Keys::new_device(&listener_identity)).await.unwrap();
        let _ = serve_connection(
            &mut conn,
            |key, _| {
                let _ = tx.send(key);
                false
            },
            move |since| crate::posts::Fetched { posts: posts.iter().filter(|p| p.created_at >= since).cloned().collect(), deleted: Vec::new() },
            move || name.clone(),
            |_, _| {},
            |_| {},
        )
        .await;
    });
    let dialer_endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let conn = transport::dial(&dialer_endpoint, addr, crate::transport::Keys::new_device(&dialer_identity), listener_key).await.unwrap();
    std::mem::forget(dialer_endpoint);
    (conn, dialer_identity, rx)
}

/// What a peer who *has* added the dialer does with its messages, in
/// miniature (the real thing is `swarm-cli`'s
/// `node::peer::PeerTask`): opens with a `CoverHello`, then acks
/// and passes on every message. Runs until the connection drops.
async fn serve_as_peer(mut conn: AuthenticatedConnection, tx: tokio::sync::mpsc::UnboundedSender<(MessageId, u64, String)>) {
    send_cover_hello(&mut conn, None, None, None).await.unwrap();
    while let Ok(incoming) = recv(&mut conn).await {
        match incoming {
            Incoming::Request(id, Request::Message { id: message_id, sent_at, text, .. }) => {
                let _ = tx.send((message_id, sent_at, text));
                reply(&mut conn, id, Reply::Ack).await.unwrap();
            }
            Incoming::Request(id, Request::Ping) => reply(&mut conn, id, Reply::Pong).await.unwrap(),
            _ => {}
        }
    }
}

fn introduction(identity: &Identity) -> Introduction {
    Introduction { root_key: identity.public_key_bytes(), name: None }
}

#[tokio::test]
async fn every_connection_has_exchanged_versions() {
    let (dialer, acceptor) = pair().await;
    assert_eq!((dialer.peer_version, acceptor.peer_version), (PROTOCOL_VERSION, PROTOCOL_VERSION));
}

#[tokio::test]
async fn message_is_delivered_and_acked() {
    let (mut dialer, acceptor) = pair().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(serve_as_peer(acceptor, tx));
    let me = Identity::generate();
    send_message_and_wait(&mut dialer, [3; 16], 1_700_000_000, 0, "hello swarm", &introduction(&me)).await.unwrap();
    assert_eq!(rx.recv().await.unwrap(), ([3; 16], 1_700_000_000, "hello swarm".to_string()));
}

#[tokio::test]
async fn a_message_keeps_its_line_breaks_and_loses_other_control_characters() {
    let (mut dialer, mut acceptor) = pair().await;
    request(&mut dialer, Request::Message { id: [1; 16], sent_at: 1, lifetime: 0, text: "one\r\ntwo\x1b[2J\tthree".to_string() }).await.unwrap();
    match recv(&mut acceptor).await.unwrap() {
        Incoming::Request(_, Request::Message { text, .. }) => assert_eq!(text, "one\ntwo [2J three"),
        other => panic!("expected a message, got {other:?}"),
    }
}

/// Someone who hasn't added us can't get a message to us at all: the
/// sender learns that from the opening `NotPeer` and never sends it,
/// introducing itself instead — with a key the stranger side has
/// checked against the connection.
#[tokio::test]
async fn a_stranger_is_never_sent_the_message_and_hears_an_introduction_instead() {
    let (mut conn, me, mut introduced) = stranger_pair(Vec::new(), None).await;
    let err = send_message_and_wait(&mut conn, [1; 16], 1_700_000_000, 0, "hello?", &introduction(&me)).await.unwrap_err();
    assert!(err.is::<NotAPeer>(), "got {err:#}");
    assert_eq!(introduced.recv().await.unwrap(), me.public_key_bytes());
    assert!(list_posts_and_wait(&mut conn, 0).await.unwrap().posts.is_empty());
}

/// A claimed key that isn't the connection's own is dropped, not
/// recorded as a request from that key; and only one introduction
/// per connection counts.
#[tokio::test]
async fn an_introduction_claiming_someone_elses_key_is_ignored() {
    let (mut conn, me, mut introduced) = stranger_pair(Vec::new(), None).await;
    let impostor = Introduction { root_key: Identity::generate().public_key_bytes(), name: None };
    send_introduce(&mut conn, &impostor).await.unwrap();
    send_introduce(&mut conn, &introduction(&me)).await.unwrap();
    send_introduce(&mut conn, &introduction(&me)).await.unwrap();
    ping(&mut conn).await.unwrap();
    assert_eq!(introduced.recv().await.unwrap(), me.public_key_bytes(), "only the genuine one gets through");
    assert!(introduced.try_recv().is_err(), "and only once");
}

#[tokio::test]
async fn list_posts_returns_whatever_the_responder_hands_back() {
    let posts = vec![post(100, Visibility::Public, "first"), post(200, Visibility::Private, "second\tsecond")];
    let (mut conn, _, _) = stranger_pair(posts.clone(), None).await;
    assert_eq!(list_posts_and_wait(&mut conn, 0).await.unwrap().posts, posts);
}

#[tokio::test]
async fn a_refresh_asks_only_for_what_is_new() {
    let posts = vec![post(100, Visibility::Public, "old"), post(200, Visibility::Public, "new")];
    let (mut conn, _, _) = stranger_pair(posts.clone(), None).await;
    assert_eq!(list_posts_and_wait(&mut conn, 200).await.unwrap().posts, vec![posts[1].clone()]);
}

/// More posts than fit one frame come back in parts, none over
/// `MAX_FRAME`.
#[tokio::test]
async fn a_posts_list_bigger_than_a_frame_arrives_whole() {
    let posts: Vec<Post> = (0..60).map(|i| post(i, Visibility::Public, &format!("{i:04} {}", "x".repeat(1500)))).collect();
    let (mut conn, _, _) = stranger_pair(posts.clone(), None).await;
    assert_eq!(list_posts_and_wait(&mut conn, 0).await.unwrap().posts, posts);
}

/// Someone followed is a stranger, and could answer a posts ask with
/// parts for ever: one fetch takes at most `MAX_POSTS_FETCHED` (the
/// oldest, as they come), and the next carries on from there.
#[tokio::test]
async fn one_fetch_takes_a_bounded_number_of_posts_and_the_next_carries_on() {
    let posts: Vec<Post> = (0..MAX_POSTS_FETCHED as u64 + 100).map(|i| post(1000 + i, Visibility::Public, "p")).collect();
    let (mut conn, _, _) = stranger_pair(posts.clone(), None).await;
    let first = list_posts_and_wait(&mut conn, 0).await.unwrap().posts;
    assert_eq!(first, posts[..MAX_POSTS_FETCHED]);
    let (mut conn, _, _) = stranger_pair(posts.clone(), None).await;
    let next = list_posts_and_wait(&mut conn, first.last().unwrap().created_at).await.unwrap().posts;
    assert_eq!(next, posts[MAX_POSTS_FETCHED - 1..]);
}

/// Whoever answers a group ask could send parts for ever: what's
/// put back together stops at the most a group keeps, and that's
/// taken as the whole answer.
#[test]
fn a_group_answer_in_endless_parts_is_cut_off() {
    let channel = ChannelId { name: "x".into(), owner: [1; 32] };
    let post = crate::channel::sign_channel_post(&channel, &Identity::generate(), "p", None);
    let mut data = ChannelData::default();
    let mut parts = 0;
    while data.extend(Vec::new(), vec![post.clone(); 20], Vec::new()) {
        parts += 1;
        assert!(parts < 10_000, "never taken as whole");
    }
    assert_eq!(data.posts.len(), crate::channel::MAX_POSTS);
}

/// A stranger repeating a follow notice over and over makes us record
/// it once (each recording rewrites the followers file); a change of
/// mind still counts.
#[tokio::test]
async fn a_follow_repeated_on_one_connection_is_recorded_once() {
    let listener = Identity::generate();
    let me = Identity::generate();
    let endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    let listener_key = listener.public_key_bytes();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut conn = transport::accept(&endpoint, crate::transport::Keys::new_device(&listener)).await.unwrap();
        let _ = serve_connection(&mut conn, |_, _| false, |_| crate::posts::Fetched::default(), || None, move |key, on| {
            let _ = tx.send((key, on));
        }, |_| {})
        .await;
    });
    let dialer = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let mut conn = transport::dial(&dialer, addr, crate::transport::Keys::new_device(&me), listener_key).await.unwrap();
    for _ in 0..50 {
        send_notice(&mut conn, Notice::Follow { root_key: me.public_key_bytes(), following: true }).await.unwrap();
    }
    send_notice(&mut conn, Notice::Follow { root_key: me.public_key_bytes(), following: false }).await.unwrap();
    ping(&mut conn).await.unwrap();
    let mut seen = Vec::new();
    while let Ok(one) = rx.try_recv() {
        seen.push(one.1);
    }
    assert_eq!(seen, [true, false]);
}

/// A peer can require any rate at all of us; what we send is held to
/// `MAX_COVER_BITS_PER_SECOND` (a rate of 10^18 planned trillions of
/// datagrams every slot).
#[tokio::test]
async fn a_required_cover_rate_is_held_to_the_most_there_is() {
    let (mut conn, _other) = pair().await;
    raise_cover(&mut conn, u64::MAX);
    let per_second = conn.cover_datagrams_per_tick as u64 * (conn.cover_out.as_ref().unwrap().slot_payload_len() + COVER_OVERHEAD_PER_SLOT) as u64 * 8 * 1000
        / COVER_SLOT_INTERVAL.as_millis() as u64;
    assert!(per_second <= MAX_COVER_BITS_PER_SECOND * 11 / 10, "{per_second} bits/s");
    let (mut conn, _other) = pair().await;
    enable_cover(&mut conn, 1);
    assert_eq!(conn.cover_datagrams_per_tick, 1);
}

/// A conference's roster goes and comes back whole, and anything in
/// it that doesn't hold up is gone by the time it's decoded.
#[test]
fn conference_frames_round_trip_and_forgeries_are_dropped() {
    use crate::conference::{Added, Left, Roster};
    let (a, b, c, d) = (Identity::generate(), Identity::generate(), Identity::generate(), Identity::generate());
    let mut roster = Roster::start(&a);
    roster.add(&a, b.public_key_bytes());
    roster.add(&b, c.public_key_bytes());
    roster.add(&c, d.public_key_bytes());
    roster.leave(&b);
    assert!(roster.remove(&a, d.public_key_bytes()));
    let hearing = CallSignal::Hearing(vec![a.public_key_bytes(), c.public_key_bytes()]);
    let forward = CallSignal::Forward { from: c.public_key_bytes(), id: 3, on: true };
    let relayed = CallSignal::Relayed { id: 3, opus: vec![1, 2, 3] };
    for signal in [CallSignal::ConferenceInvite(roster.clone()), CallSignal::ConferenceLink(roster.clone()), CallSignal::Roster(roster.clone()), hearing, forward, relayed] {
        match Frame::decode(&Frame::Notice(Notice::Call(signal.clone())).encode()).unwrap() {
            Frame::Notice(Notice::Call(back)) => assert_eq!(back, signal),
            _ => panic!("not a call notice"),
        }
    }
    let mut forged = roster.clone();
    forged.added.push(Added { member: [9; 32], by: a.public_key_bytes(), round: 0, number: 9, signature: [1; 64] });
    forged.left.push(Left { member: c.public_key_bytes(), round: 0, by: d.public_key_bytes(), by_round: 0, signature: [1; 64] });
    match Frame::decode(&Frame::Notice(Notice::Call(CallSignal::Roster(forged))).encode()).unwrap() {
        Frame::Notice(Notice::Call(CallSignal::Roster(back))) => assert_eq!(back, roster),
        _ => panic!("not a roster"),
    }
}

/// Control of a shared screen: every kind decodes back to what was
/// encoded; a kind from a newer build is skipped, not an error; and
/// none of it is ever taken inside `Via`, since it's never passed on.
#[test]
fn screen_control_frames_round_trip() {
    use crate::remote::{keys, Control, Input};
    let signals = [
        CallSignal::Control(Control::Ask),
        CallSignal::Control(Control::GiveBack),
        CallSignal::Control(Control::Allow),
        CallSignal::Control(Control::Refuse("this desktop can't be controlled".into())),
        CallSignal::Control(Control::Stop),
        CallSignal::Control(Control::Paused(true)),
        CallSignal::Control(Control::Paused(false)),
        CallSignal::Input(Input::Pointer { x: 0, y: 65535 }),
        CallSignal::Input(Input::Button { button: crate::remote::BUTTON_RIGHT, down: true }),
        CallSignal::Input(Input::Wheel { dx: -120, dy: 240 }),
        CallSignal::Input(Input::Key { key: keys::F1, down: true }),
        CallSignal::Input(Input::Key { key: 'c', down: false }),
        CallSignal::Input(Input::Text("£5 for ‘Ünïcode’ 🐝".into())),
    ];
    for signal in signals {
        let notice = Notice::Call(signal.clone());
        match Frame::decode(&Frame::Notice(notice.clone()).encode()).unwrap() {
            Frame::Notice(back) => assert_eq!(back, notice),
            _ => panic!("not a notice"),
        }
        let mut via = Frame::Notice(Notice::Call(CallSignal::Via { id: 1, signal: Box::new(CallSignal::CameraOff) })).encode();
        via.truncate(2);
        via.extend(Frame::Notice(notice).encode());
        assert!(Frame::decode(&via).is_err(), "{signal:?} taken inside Via");
    }
    assert!(matches!(Frame::decode(&[TAG_CALL_CONTROL, 99]).unwrap(), Frame::Unknown));
    assert!(matches!(Frame::decode(&[TAG_CALL_INPUT, 99, 1, 2]).unwrap(), Frame::Unknown));
    let mut w = crate::wire::Writer::new(TAG_CALL_INPUT);
    w.u8(4).str("rm -rf\u{1b}[A\x07 ok");
    match Frame::decode(&w.into_bytes()).unwrap() {
        Frame::Notice(Notice::Call(CallSignal::Input(Input::Text(text)))) => assert_eq!(text, "rm -rf[A ok"),
        _ => panic!("not text"),
    }
    let mut w = crate::wire::Writer::new(TAG_CALL_CONTROL);
    w.u8(3).str(&"é".repeat(300));
    match Frame::decode(&w.into_bytes()).unwrap() {
        Frame::Notice(Notice::Call(CallSignal::Control(Control::Refuse(why)))) => assert_eq!(why.len(), crate::remote::MAX_REASON),
        _ => panic!("not a refusal"),
    }
}

/// Devices' frames decode back to what was encoded.
#[test]
fn device_frames_round_trip() {
    let desktop = Identity::generate();
    let notices = [
        Notice::PeersBook { entries: vec![BookPeer { key: [6; 32], name: "alice".into(), address: "[2001:db8::7]:1977".into(), at: 1_700_000_000, present: false }] },
        Notice::RequestsBook {
            entries: vec![
                BookRequest {
                    request: crate::requests::Request { public_key: [7; 32], status: crate::requests::Status::Pending, address: "198.51.100.7:1977".into(), first_seen: 1_700_000_000, last_seen: 1_700_000_060 },
                    name: Some(DisplayName { name: "Tim".into(), at: 1_699_999_000, signature: [5; 64] }),
                },
                BookRequest {
                    request: crate::requests::Request { public_key: [8; 32], status: crate::requests::Status::Dismissed, address: String::new(), first_seen: 1, last_seen: 2 },
                    name: None,
                },
            ],
            more: true,
        },
        Notice::Bundle { more: true, bytes: b"peer\tbob".to_vec() },
        Notice::Carbon { peer: [3; 32], id: [4; 16], sent: true, at: 1_700_000_000, expires: 1_700_003_600, text: "copied".into() },
        Notice::CarbonFor { device: [9; 32], carbon: Box::new(Notice::Carbon { peer: [3; 32], id: [4; 16], sent: false, at: 1, expires: 61, text: "on its way".into() }) },
        Notice::Reaching(vec![[1; 32], [2; 32]]),
        Notice::Reaching(Vec::new()),
        Notice::ReachingDelays(vec![([1; 32], 12), ([2; 32], u16::MAX)]),
        Notice::ReachingDelays(Vec::new()),
        Notice::CallThrough { contact: [5; 32], signal: CallSignal::Invite { video: true, at: 0 } },
        Notice::CallThrough { contact: [5; 32], signal: CallSignal::Audio(vec![1, 2, 3]) },
        Notice::CallThrough { contact: [5; 32], signal: CallSignal::Hangup },
        Notice::FileThrough { contact: [7; 32], onward: true, notice: Box::new(Notice::FileOffer { id: [1; 16], name: "a.txt".into(), size: 9, sha256: [2; 32] }) },
        Notice::FileThrough { contact: [7; 32], onward: false, notice: Box::new(Notice::FileAnswer { id: [1; 16], accepted: true }) },
        Notice::FileThrough { contact: [7; 32], onward: true, notice: Box::new(Notice::FileCancel { id: [1; 16], reason: "no".into() }) },
        Notice::FileThrough { contact: [7; 32], onward: false, notice: Box::new(Notice::FileDone { id: [1; 16] }) },
        Notice::CallRoom { contact: [6; 32], audio: 80, video_bps: 500_000, video_piece: 1100, video_on: true, bigger: 900_000, relayed: 2, covered: true },
        Notice::Timer(crate::disappearing::Timer { seconds: 86_400, set_at: 1_700_000_000, by: [9; 32] }),
        Notice::Call(CallSignal::AnsweredElsewhere),
        Notice::Call(CallSignal::Resume),
        Notice::Call(CallSignal::Takeover),
        Notice::Call(CallSignal::CameraOff),
        Notice::Call(CallSignal::Screen(true)),
        Notice::Call(CallSignal::Screen(false)),
        Notice::Call(CallSignal::Decodes { h265: true }),
        Notice::Call(CallSignal::Decodes { h265: false }),
        Notice::Call(CallSignal::ShownIn { width: 1080, height: 2400, small_width: 0, small_height: 0 }),
        Notice::Call(CallSignal::ShownIn { width: 0, height: 0, small_width: 0, small_height: 0 }),
        Notice::Call(CallSignal::ShownIn { width: 3840, height: 1900, small_width: 480, small_height: 270 }),
        Notice::Call(CallSignal::Via { id: 0, signal: Box::new(CallSignal::ShownIn { width: 1920, height: 1080, small_width: 480, small_height: 270 }) }),
        Notice::Call(CallSignal::KeyframeRequest { small: false }),
        Notice::Call(CallSignal::KeyframeRequest { small: true }),
        Notice::Call(CallSignal::Via { id: 3, signal: Box::new(CallSignal::KeyframeRequest { small: true }) }),
        Notice::Call(CallSignal::Video(crate::video::VideoPiece { frame: 2, index: 1, count: 3, key: true, h265: false, small: true, data: vec![4; 9] })),
        Notice::Call(CallSignal::Video(crate::video::VideoPiece { frame: 3, index: 0, count: 1, key: false, h265: true, small: false, data: vec![5; 9] })),
        Notice::Call(CallSignal::Via { id: 255, signal: Box::new(CallSignal::CameraOff) }),
        Notice::Call(CallSignal::Via { id: 1, signal: Box::new(CallSignal::Screen(true)) }),
        Notice::Follow { root_key: [5; 32], following: true },
        Notice::Follow { root_key: [6; 32], following: false },
        Notice::Addresses(vec!["2001:db8::5".into(), "[fe80::1]:5000".into(), "192.168.1.2".into()]),
        Notice::ManualAddress { at: 7, items: vec!["203.0.113.7".into(), "home.example.org:5000".into()] },
        Notice::ManualAddress { at: 0, items: vec![] },
        Notice::Discoverable { at: 7, on: false },
        Notice::Discoverable { at: 8, on: true },
        Notice::Card(crate::card::Card::new(&desktop, [8; 32], 1_800_000_000, &["2001:db8::5".into()])),
    ];
    for notice in notices {
        match Frame::decode(&Frame::Notice(notice.clone()).encode()).unwrap() {
            Frame::Notice(back) => assert_eq!(back, notice),
            _ => panic!("not a notice"),
        }
    }
}

/// The largest part of the largest profile there can be goes in one
/// frame, and comes back as it went.
#[test]
fn the_largest_profile_part_fits_a_frame_and_round_trips() {
    let profile = crate::profile::Profile {
        at: u64::MAX,
        status: "€".repeat(crate::profile::MAX_STATUS_CHARS),
        avatar: crate::profile::tests::jpeg(0xC0, 640, 3, crate::profile::MAX_AVATAR_BYTES),
        signature: [7; 64],
    };
    let part = crate::profile::part(&profile, crate::profile::PART_BYTES);
    assert_eq!(part.data.len(), crate::profile::PART_BYTES);
    let bytes = Frame::Reply(u32::MAX, Reply::Profile(Some(part.clone()))).encode();
    assert!(bytes.len() + 16 <= framing::MAX_FRAME, "{} bytes", bytes.len());
    match Frame::decode(&bytes).unwrap() {
        Frame::Reply(_, Reply::Profile(Some(back))) => assert_eq!(back, part),
        _ => panic!("not the profile"),
    }
    match Frame::decode(&Frame::Request(3, Request::Profile { have: 9, from: 6144 }).encode()).unwrap() {
        Frame::Request(3, Request::Profile { have: 9, from: 6144 }) => {}
        _ => panic!("not the request"),
    }
}

/// A message's lifetime crosses the wire.
#[test]
fn a_messages_lifetime_round_trips() {
    let message = Request::Message { id: [1; 16], sent_at: 5, lifetime: 3600, text: "gone in an hour".into() };
    match Frame::decode(&Frame::Request(7, message.clone()).encode()).unwrap() {
        Frame::Request(7, back) => assert_eq!(back, message),
        _ => panic!("not the request"),
    }
    let passed = Request::PassOn { contact: [2; 32], id: [1; 16], sent_at: 5, lifetime: 60, text: "through the desktop".into() };
    match Frame::decode(&Frame::Request(8, passed.clone()).encode()).unwrap() {
        Frame::Request(8, back) => assert_eq!(back, passed),
        _ => panic!("not the request"),
    }
}

/// A chunk asked for through another device names the peer, and a
/// transfer notice passed on carries only a transfer's notice.
#[test]
fn file_frames_through_a_device_round_trip() {
    let asked = Request::FileChunkThrough { contact: [4; 32], id: [1; 16], offset: 4000, len: 4000 };
    match Frame::decode(&Frame::Request(9, asked.clone()).encode()).unwrap() {
        Frame::Request(9, back) => assert_eq!(back, asked),
        _ => panic!("not the request"),
    }
    let mut wrong = vec![TAG_FILE_THROUGH];
    wrong.extend_from_slice(&[7; 32]);
    wrong.push(1);
    wrong.extend(Frame::Notice(Notice::Reaching(vec![[1; 32]])).encode());
    assert!(matches!(Frame::decode(&wrong).unwrap(), Frame::Unknown), "only a transfer's notice goes through");
}

/// Whatever arrives — any frame cut short anywhere, any bytes
/// changed, plain noise — decodes to a frame or an error, never a
/// panic (a stranger can send any of it before being checked).
#[test]
fn no_garbled_frame_panics_the_decoder() {
    use crate::channel::{self, BucketDigest, ChannelId};
    use rand::{Rng, SeedableRng};
    let me = Identity::generate();
    let channel = ChannelId { name: "général".to_string(), owner: me.public_key_bytes() };
    let name = DisplayName { name: "Bob".to_string(), at: 5, signature: me.sign(b"5\tBob") };
    let post = Post { created_at: 9, visibility: Visibility::Public, signature: [1; 64], text: "hello".into() };
    let event = channel::sign_roster_event(&channel, &me, vec![[4; 32]], [9; 32], RosterAction::Joined);
    let cpost = channel::sign_channel_post(&channel, &me, "live", Some("Zoë"));
    let frames = vec![
        Frame::Hello { version: 8, min_version: 7, datagram_key: [2; 32], seen_as: "2001:db8::1".into(), me: DeviceProof { user: [3; 32], device: [4; 32], cert: [5; 64], proof: [6; 64] } },
        Frame::CoverAck(1, 2, 3),
        Frame::Request(1, Request::Ping),
        Frame::Request(2, Request::Message { id: [3; 16], sent_at: 4, lifetime: 60, text: "hi".into() }),
        Frame::Request(3, Request::ListPosts { since: 5 }),
        Frame::Request(6, Request::WhoAmI),
        Frame::Request(7, Request::FileChunk { id: [1; 16], offset: 2, len: 3 }),
        Frame::Request(9, Request::FileChunkThrough { contact: [4; 32], id: [1; 16], offset: 2, len: 3 }),
        Frame::Request(8, Request::ChannelCatchUp { channel: channel.clone(), from: 1, digests: vec![BucketDigest { bucket: 1, count: 2, xor: 3 }] }),
        Frame::Reply(1, Reply::Pong),
        Frame::Reply(2, Reply::Posts { more: true, posts: vec![post], deleted: Vec::new() }),
        Frame::Reply(3, Reply::ChannelData { more: false, roster: vec![event.clone()], posts: vec![cpost.clone()], names: vec![([1; 32], name.clone())] }),
        Frame::Reply(4, Reply::ChannelError("no".into())),
        Frame::Reply(5, Reply::DisplayName(Some(name.clone()))),
        Frame::Reply(6, Reply::FileData(vec![1, 2, 3])),
        Frame::Reply(7, Reply::FileError("gone".into())),
        Frame::Request(9, Request::Profile { have: 3, from: 0 }),
        Frame::Reply(8, Reply::Profile(Some(crate::profile::part(&crate::profile::Profile { at: 4, status: "out".into(), avatar: crate::profile::tests::picture(), signature: [5; 64] }, 0)))),
        Frame::Reply(9, Reply::Profile(None)),
        Frame::Reply(10, Reply::Refused("not allowed".into())),
        Frame::Request(11, Request::JoinUser { name: "phone".into() }),
        Frame::Request(12, Request::WithdrawContactRequest { root_key: [4; 32] }),
        Frame::Reply(11, Reply::UserKey { seed: [7; 16], list: "user x\n".into() }),
        Frame::Notice(Notice::ProfileChanged),
        Frame::Notice(Notice::CoverHello { sends: Some(1), requires: None, limit: None }),
        Frame::Notice(Notice::CoverHello { sends: None, requires: None, limit: Some(1_000_000) }),
        Frame::Notice(Notice::ReceiveLimit { limit: Some(500_000), in_call: false }),
        Frame::Notice(Notice::ReceiveLimit { limit: None, in_call: false }),
        Frame::Notice(Notice::ReceiveLimit { limit: None, in_call: true }),
        Frame::Notice(Notice::Introduce { root_key: [1; 32], name: Some(name) }),
        Frame::Notice(Notice::Call(CallSignal::Invite { video: true, at: 0 })),
        Frame::Notice(Notice::Call(CallSignal::Video(crate::video::VideoPiece { frame: 1, index: 0, count: 2, key: true, h265: true, small: true, data: vec![9; 20] }))),
        Frame::Notice(Notice::Call(CallSignal::Audio(vec![1; 30]))),
        Frame::Notice(Notice::Call(CallSignal::Via {
            id: 4,
            signal: Box::new(CallSignal::Video(crate::video::VideoPiece { frame: 9, index: 1, count: 3, key: false, h265: false, small: false, data: vec![7; 40] })),
        })),
        Frame::Notice(Notice::Call(CallSignal::Via { id: 5, signal: Box::new(CallSignal::KeyframeRequest { small: true }) })),
        Frame::Notice(Notice::FileOffer { id: [1; 16], name: "a.txt".into(), size: 3, sha256: [2; 32] }),
        Frame::Notice(Notice::FileAnswer { id: [1; 16], accepted: true }),
        Frame::Notice(Notice::FileCancel { id: [1; 16], reason: "x".into() }),
        Frame::Notice(Notice::FileDone { id: [1; 16] }),
        Frame::Notice(Notice::FileClosed { id: [1; 16] }),
        Frame::Notice(Notice::ChannelPost { channel: channel.clone(), post: cpost }),
        Frame::Notice(Notice::ChannelEvent { channel: channel.clone(), event }),
        Frame::Notice(Notice::Bundle { more: true, bytes: b"peer\tbob".to_vec() }),
        Frame::Notice(Notice::Carbon { peer: [3; 32], id: [4; 16], sent: true, at: 1, expires: 61, text: "c".into() }),
        Frame::Notice(Notice::Follow { root_key: [5; 32], following: true }),
        Frame::Notice(Notice::Addresses(vec!["2001:db8::5".into(), "192.168.1.2".into()])),
        Frame::Notice(Notice::ManualAddress { at: 7, items: vec!["203.0.113.7".into()] }),
        Frame::Notice(Notice::DeviceAddresses(vec![
            DeviceAddress { device: [6; 32], at: 9, addresses: vec!["2001:db8::6".into(), "10.0.0.6".into()] },
            DeviceAddress { device: [7; 32], at: 10, addresses: vec![] },
        ])),
        Frame::Notice(Notice::DeviceList {
            entries: vec![
                crate::devicelist::Entry { at: 5, device: [1; 32], change: crate::devicelist::Change::Added { name: "desk".into() }, signature: [2; 64] },
                crate::devicelist::Entry { at: 6, device: [3; 32], change: crate::devicelist::Change::Relay { allowed: true }, signature: [4; 64] },
                crate::devicelist::Entry { at: 7, device: [3; 32], change: crate::devicelist::Change::Removed, signature: [5; 64] },
            ],
        }),
        Frame::Notice(Notice::Card(crate::card::Card::new(&me, [8; 32], 1, &["2001:db8::5".into()]))),
    ];
    let frames: Vec<Frame> = frames.into_iter().chain(every_frame().into_iter().map(|(_, f)| f)).collect();
    let mut rng = rand::rngs::StdRng::seed_from_u64(1977);
    for frame in &frames {
        let bytes = frame.encode();
        for cut in 0..bytes.len() {
            let _ = Frame::decode(&bytes[..cut]);
        }
        let times = if [TAG_CALL_CONFERENCE, TAG_CALL_CONFERENCE_LINK, TAG_CALL_ROSTER].contains(&bytes[0]) { 100 } else { 2000 };
        for _ in 0..times {
            let mut garbled = bytes.clone();
            for _ in 0..rng.gen_range(1..4) {
                let at = rng.gen_range(0..garbled.len());
                garbled[at] = rng.r#gen();
            }
            if rng.gen_bool(0.3) {
                garbled.truncate(rng.gen_range(0..=garbled.len()));
            }
            let _ = Frame::decode(&garbled);
        }
    }
    for _ in 0..20_000 {
        let len = rng.gen_range(0..300);
        let noise: Vec<u8> = (0..len).map(|_| rng.r#gen()).collect();
        let _ = Frame::decode(&noise);
        let _ = bundle_notices(&noise);
    }
}

#[test]
fn live_channel_frames_round_trip() {
    use crate::channel::{self, BucketDigest, ChannelId};
    let owner = Identity::generate();
    let channel = ChannelId { name: "général".to_string(), owner: owner.public_key_bytes() };
    let left = channel::sign_roster_event(&channel, &owner, vec![[4; 32]], [9; 32], RosterAction::Left);
    let post = channel::sign_channel_post(&channel, &owner, "live", None);
    let named = channel::sign_channel_post(&channel, &owner, "with a name", Some("Zoë"));
    let notices = [
        Notice::ChannelPost { channel: channel.clone(), post },
        Notice::ChannelPost { channel: channel.clone(), post: named },
        Notice::ChannelEvent { channel: channel.clone(), event: left },
    ];
    for notice in notices {
        match Frame::decode(&Frame::Notice(notice.clone()).encode()).unwrap() {
            Frame::Notice(back) => assert_eq!(back, notice),
            _ => panic!("not a notice"),
        }
    }
    let request = Request::ChannelCatchUp { channel, from: 7, digests: vec![BucketDigest { bucket: 470_000, count: 3, xor: 0xdead_beef }] };
    match Frame::decode(&Frame::Request(5, request.clone()).encode()).unwrap() {
        Frame::Request(5, back) => assert_eq!(back, request),
        _ => panic!("not the request"),
    }
}

#[tokio::test]
async fn who_am_i_round_trips_a_published_name_and_none_when_unset() {
    let me = Identity::generate();
    let name = DisplayName { name: "Bob".to_string(), at: 5, signature: me.sign(b"5\tBob") };
    let (mut conn, _, _) = stranger_pair(Vec::new(), Some(name.clone())).await;
    assert_eq!(who_am_i_and_wait(&mut conn).await.unwrap(), Some(name));
    let (mut conn, _, _) = stranger_pair(Vec::new(), None).await;
    assert_eq!(who_am_i_and_wait(&mut conn).await.unwrap(), None);
}

/// Any number of requests can be outstanding each way at once; each
/// reply finds its request by id, whatever order they come back in.
#[tokio::test]
async fn replies_are_matched_by_id_not_by_order() {
    let (mut a, mut b) = pair().await;
    let ping_id = request(&mut a, Request::Ping).await.unwrap();
    let posts_id = request(&mut a, Request::ListPosts { since: 0 }).await.unwrap();
    let b_message = request(&mut b, Request::Message { id: [7; 16], sent_at: 1, lifetime: 0, text: "crossing".to_string() }).await.unwrap();

    let mut asks = Vec::new();
    while asks.len() < 2 {
        match recv(&mut b).await.unwrap() {
            Incoming::Request(id, req) => asks.push((id, req)),
            other => panic!("unexpected {other:?}"),
        }
    }
    respond_posts(&mut b, asks[1].0, crate::posts::Fetched { posts: vec![post(1, Visibility::Public, "p")], deleted: Vec::new() }).await.unwrap();
    reply(&mut b, asks[0].0, Reply::Pong).await.unwrap();

    let mut got = Vec::new();
    while got.len() < 3 {
        match recv(&mut a).await.unwrap() {
            Incoming::Request(id, Request::Message { .. }) => {
                reply(&mut a, id, Reply::Ack).await.unwrap();
                got.push("message");
            }
            Incoming::Reply(id, Reply::Posts { .. }) if id == posts_id => got.push("posts"),
            Incoming::Reply(id, Reply::Pong) if id == ping_id => got.push("pong"),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(got, vec!["message", "posts", "pong"]);
    assert!(matches!(recv(&mut b).await.unwrap(), Incoming::Reply(id, Reply::Ack) if id == b_message));
}

/// A request type from some later version gets `Unsupported` back
/// instead of killing the connection.
#[tokio::test]
async fn an_unknown_request_is_answered_unsupported() {
    let (mut a, mut b) = pair().await;
    let mut raw = vec![0x3F];
    raw.extend_from_slice(&77u32.to_be_bytes());
    raw.extend_from_slice(b"whatever a later version puts here");
    let sealed = a.session.seal(&raw).unwrap();
    framing::write_frame(&mut a.send, &sealed).await.unwrap();
    let sealed = a.session.seal(&[0xBF, 1, 2, 3]).unwrap();
    framing::write_frame(&mut a.send, &sealed).await.unwrap();
    let ping_id = request(&mut a, Request::Ping).await.unwrap();

    let incoming = recv(&mut b).await.unwrap();
    flush(&mut b).await.unwrap();
    match incoming {
        Incoming::Request(id, Request::Ping) => {
            assert_eq!(id, ping_id);
            reply(&mut b, id, Reply::Pong).await.unwrap();
        }
        other => panic!("expected the ping after the unknowns, got {other:?}"),
    }
    assert!(matches!(recv(&mut a).await.unwrap(), Incoming::Reply(77, Reply::Unsupported)));
    assert!(matches!(recv(&mut a).await.unwrap(), Incoming::Reply(id, Reply::Pong) if id == ping_id));
}

/// A peer whose `Hello` says it needs a newer protocol is refused at
/// connect time, with an error saying so.
#[tokio::test]
async fn an_incompatible_version_is_refused_with_a_clear_error() {
    use crate::noise::Handshake;

    let server_identity = Identity::generate();
    let server_key = server_identity.public_key_bytes();
    let endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(async move {
        let connection = endpoint.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = connection.accept_bi().await.unwrap();
        let mut hs = Handshake::responder(&server_identity.x25519_private_key_bytes()).unwrap();
        hs.read_message(&framing::read_frame(&mut recv).await.unwrap()).unwrap();
        framing::write_frame(&mut send, &hs.write_message().unwrap()).await.unwrap();
        let mut session = hs.into_session().unwrap();
        let hello = Frame::Hello { version: PROTOCOL_VERSION + 1, min_version: PROTOCOL_VERSION + 1, datagram_key: [0; 32], seen_as: String::new(), me: DeviceProof { user: [0; 32], device: [0; 32], cert: [0; 64], proof: [0; 64] } }.encode();
        framing::write_frame(&mut send, &session.seal(&hello).unwrap()).await.unwrap();
        let _ = framing::read_frame(&mut recv).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    let client = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let err = match transport::dial(&client, addr, crate::transport::Keys::new_device(&Identity::generate()), server_key).await {
        Ok(_) => panic!("should have been refused"),
        Err(e) => e,
    };
    let version = err.downcast_ref::<IncompatibleVersion>().expect("an IncompatibleVersion error");
    assert_eq!(version.theirs, Some(PROTOCOL_VERSION + 1));
    assert!(err.to_string().contains("update this one"));
}

/// A message queued under cover mode never gets written directly —
/// it has to be drained out through `emit_cover_slot` ticks — but
/// still arrives on the other end exactly as sent, and the plain
/// (non-cover) reply that comes back still decodes.
#[tokio::test]
async fn cover_mode_delivers_a_message_queued_through_slots() {
    let (mut conn, acceptor) = pair().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(serve_as_peer(acceptor, tx));

    enable_cover(&mut conn, 64_000);
    let id = request(&mut conn, Request::Message { id: [2; 16], sent_at: 1_700_000_000, lifetime: 0, text: "hidden in plain sight".to_string() }).await.unwrap();
    for _ in 0..10 {
        emit_cover_slot(&mut conn).await.unwrap();
    }
    assert_eq!(rx.recv().await.unwrap(), ([2; 16], 1_700_000_000, "hidden in plain sight".to_string()));
    assert!(matches!(recv(&mut conn).await.unwrap(), Incoming::Notice(Notice::CoverHello { .. })));
    assert!(matches!(recv(&mut conn).await.unwrap(), Incoming::Reply(r, Reply::Ack) if r == id));
}

/// Keys renewed on every frame, both ways, without cover and with:
/// everything still arrives, in order.
#[tokio::test]
async fn renewing_keys_loses_nothing_with_or_without_cover() {
    let (mut a, mut b) = pair().await;
    a.rekey_every = Duration::ZERO;
    b.rekey_every = Duration::ZERO;
    let note = |i: usize| Notice::Addresses(vec![format!("2001:db8::{i}")]);
    let got = |incoming: Incoming| match incoming {
        Incoming::Notice(Notice::Addresses(list)) => list[0].clone(),
        other => panic!("unexpected {other:?}"),
    };
    for i in 0..20 {
        send_notice(&mut a, note(i)).await.unwrap();
        assert_eq!(got(recv(&mut b).await.unwrap()), format!("2001:db8::{i}"));
        send_notice(&mut b, note(100 + i)).await.unwrap();
        assert_eq!(got(recv(&mut a).await.unwrap()), format!("2001:db8::{}", 100 + i));
    }
    assert!(a.rekeys >= 20 && b.rekeys >= 20, "{} and {} rekeys", a.rekeys, b.rekeys);
    enable_cover(&mut a, 64_000);
    let before = a.rekeys;
    for i in 0..20 {
        send_notice(&mut a, note(200 + i)).await.unwrap();
        for _ in 0..4 {
            emit_cover_slot(&mut a).await.unwrap();
        }
    }
    let mut seen = Vec::new();
    while seen.len() < 20 {
        match tokio::time::timeout(Duration::from_secs(5), recv(&mut b)).await.unwrap().unwrap() {
            Incoming::Notice(Notice::Addresses(list)) => seen.push(list[0].clone()),
            Incoming::Notice(Notice::CoverHello { .. }) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(seen, (0..20).map(|i| format!("2001:db8::{}", 200 + i)).collect::<Vec<_>>());
    assert!(a.rekeys >= before + 20);
    send_notice(&mut b, note(999)).await.unwrap();
    assert_eq!(got(recv(&mut a).await.unwrap()), "2001:db8::999");
}

/// `emit_cover_slot` on a connection nothing ever called
/// `enable_cover` on is a documented no-op, not an error.
#[tokio::test]
async fn emit_cover_slot_without_enable_cover_is_a_harmless_no_op() {
    let (mut conn, _acceptor) = pair().await;
    emit_cover_slot(&mut conn).await.unwrap();
    request(&mut conn, Request::Ping).await.unwrap();
}

/// Call audio queued under cover goes out through the slots' priority
/// lane, arriving before a message that was queued *first* and is
/// still part-way out.
#[tokio::test]
async fn cover_mode_carries_call_audio_ahead_of_a_message_in_flight() {
    let (mut conn, mut listener) = pair().await;
    enable_cover(&mut conn, 64_000);
    let budget = call_audio_budget(&conn, 1);
    assert!(budget >= 60, "64k should leave room for decent voice, got {budget} bytes/frame");

    request(&mut conn, Request::Message { id: [1; 16], sent_at: 1, lifetime: 0, text: "x".repeat(2000) }).await.unwrap();
    emit_cover_slot(&mut conn).await.unwrap();
    send_call(&mut conn, CallSignal::Audio(vec![7; budget])).await.unwrap();
    send_call(&mut conn, CallSignal::Audio(vec![8; budget])).await.unwrap();
    for _ in 0..12 {
        emit_cover_slot(&mut conn).await.unwrap();
    }

    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(match tokio::time::timeout(Duration::from_secs(5), recv(&mut listener)).await.unwrap().unwrap() {
            Incoming::Notice(Notice::Call(_)) => "Call",
            Incoming::Request(_, Request::Message { .. }) => "Message",
            other => panic!("unexpected {other:?}"),
        });
    }
    assert_eq!(got, vec!["Call", "Call", "Message"]);
}

/// A conference passing two others' voices on along a link shares
/// the slots' room three ways: a slot's worth of frames for all three,
/// each at the shared budget, goes out in one slot — cover's never
/// exceeded, and nothing waits.
#[tokio::test]
async fn voices_passed_on_share_the_slots() {
    let (mut conn, mut listener) = pair().await;
    enable_cover(&mut conn, 64_000);
    let (alone, shared) = (call_audio_budget(&conn, 1), call_audio_budget(&conn, 3));
    assert!(shared < alone && shared >= crate::call::MIN_AUDIO_BYTES, "{shared} of {alone}");
    let items = audio_frames_per_slot(conn.cover_datagrams_per_tick) * 3;
    for i in 0..items {
        let signal = if i % 3 == 0 { CallSignal::Audio(vec![7; shared]) } else { CallSignal::Relayed { id: i as u8, opus: vec![8; shared] } };
        send_call(&mut conn, signal).await.unwrap();
    }
    emit_cover_slot(&mut conn).await.unwrap();
    for _ in 0..items {
        match tokio::time::timeout(Duration::from_secs(5), recv(&mut listener)).await.unwrap().unwrap() {
            Incoming::Notice(Notice::Call(CallSignal::Audio(_) | CallSignal::Relayed { .. })) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// The old "expected a Pong reply" failure: a one-shot ping whose
/// connection the peer's own connection task has adopted, which opens
/// with its own `CoverHello`, `WhoAmI` and a ping before answering.
#[tokio::test]
async fn a_one_shot_reply_survives_unsolicited_frames_arriving_first() {
    let (mut conn, mut peer) = pair().await;
    let peer_task = tokio::spawn(async move {
        send_cover_hello(&mut peer, None, Some(64_000), None).await.unwrap();
        let who = request(&mut peer, Request::WhoAmI).await.unwrap();
        let their_ping = request(&mut peer, Request::Ping).await.unwrap();
        let mut replies = Vec::new();
        while replies.len() < 2 {
            match recv(&mut peer).await.unwrap() {
                Incoming::Request(id, Request::Ping) => reply(&mut peer, id, Reply::Pong).await.unwrap(),
                Incoming::Reply(id, Reply::DisplayName(_)) if id == who => replies.push("DisplayName"),
                Incoming::Reply(id, Reply::Pong) if id == their_ping => replies.push("Pong"),
                other => panic!("unexpected {other:?}"),
            }
        }
        replies.sort();
        (replies, peer)
    });
    ping(&mut conn).await.unwrap();
    let (replies, _peer) = peer_task.await.unwrap();
    assert_eq!(replies, vec!["DisplayName", "Pong"]);
}

#[tokio::test]
async fn raise_cover_turns_cover_on_or_grows_it_and_never_lowers_it() {
    let (mut conn, mut peer) = pair().await;
    assert!(conn.cover_out.is_none());
    assert!(raise_cover(&mut conn, 32_000), "cover was off, so the caller has to start ticking");
    let at_32k = call_audio_budget(&conn, 1);
    assert!(!raise_cover(&mut conn, 64_000));
    let at_64k = call_audio_budget(&conn, 1);
    assert!(at_64k > at_32k);
    assert!(!raise_cover(&mut conn, 16_000));
    assert_eq!(call_audio_budget(&conn, 1), at_64k, "never lowered");

    send_cover_hello(&mut peer, Some(8_000), Some(64_000), Some(1_000_000)).await.unwrap();
    match recv(&mut conn).await.unwrap() {
        Incoming::Notice(Notice::CoverHello { sends, requires, limit }) => assert_eq!((sends, requires, limit), (Some(8_000), Some(64_000), Some(1_000_000))),
        other => panic!("expected a CoverHello, got {other:?}"),
    }
}

#[test]
fn voice_needs_a_minimum_cover_rate() {
    assert!(cover_rate_carries_voice(64_000));
    assert!(cover_rate_carries_voice(18_000));
    assert!(!cover_rate_carries_voice(16_000));
    assert!(!cover_rate_carries_voice(4_000));
}

#[tokio::test]
async fn liveness_counts_cover_slots_not_just_frames() {
    let (mut conn, mut peer) = pair().await;
    enable_cover(&mut peer, 16_000);
    let before = conn.last_heard;
    tokio::time::sleep(Duration::from_millis(20)).await;
    emit_cover_slot(&mut peer).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(200), recv(&mut conn)).await;
    assert!(conn.last_heard > before);
}

#[tokio::test]
async fn the_video_budget_follows_the_cover_rate_and_voice_is_capped_for_it() {
    use crate::call::CallBudget;
    use std::sync::atomic::Ordering::Relaxed;
    let (mut conn, _peer) = pair().await;
    let budget = CallBudget::new();

    update_call_budget(&conn, &budget);
    assert_eq!(budget.video_bps.load(Relaxed), crate::call::UNCOVERED_VIDEO_START as usize, "no cover: where the link's rate is found from");

    enable_cover(&mut conn, 128_000);
    budget.video_on.store(true, Relaxed);
    update_call_budget(&conn, &budget);
    let (audio, bps, piece) = (budget.audio.load(Relaxed), budget.video_bps.load(Relaxed), budget.video_piece.load(Relaxed));
    assert!((40..=crate::call::AUDIO_BYTES_WITH_VIDEO).contains(&audio), "voice capped but decent: {audio}");
    assert!(bps >= 40_000, "128k leaves room for a picture: {bps} bps");
    assert!(piece >= 200, "pieces of a useful size: {piece}");

    assert!(cover_rate_carries_video(128_000));
    assert!(!cover_rate_carries_video(64_000));
}

#[tokio::test]
async fn video_signals_cross_a_connection_and_pieces_ride_the_slots() {
    use crate::video::VideoPiece;
    let (mut conn, mut peer) = pair().await;
    send_call(&mut conn, CallSignal::Invite { video: true, at: 0 }).await.unwrap();
    enable_cover(&mut conn, 128_000);
    let piece = VideoPiece { frame: 7, index: 1, count: 3, key: true, h265: false, small: true, data: vec![5; 250] };
    send_call(&mut conn, CallSignal::Video(piece.clone())).await.unwrap();
    send_call(&mut conn, CallSignal::KeyframeRequest { small: true }).await.unwrap();
    for _ in 0..4 {
        emit_cover_slot(&mut conn).await.unwrap();
    }
    let mut got = Vec::new();
    while got.len() < 3 {
        match tokio::time::timeout(Duration::from_secs(5), recv(&mut peer)).await.unwrap().unwrap() {
            Incoming::Notice(Notice::Call(signal)) => got.push(signal),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(got.contains(&CallSignal::Invite { video: true, at: 0 }));
    assert!(got.contains(&CallSignal::Video(piece)));
    assert!(got.contains(&CallSignal::KeyframeRequest { small: true }));
}

/// Every tag there is, for `every_frame_encodes_as_it_always_has`.
const ALL_TAGS: &[u8] = &[
    TAG_PING, TAG_MESSAGE, TAG_LIST_POSTS, TAG_WHO_AM_I, TAG_FILE_CHUNK, TAG_CHANNEL_CATCH_UP, TAG_PROFILE_ASK, TAG_JOIN_USER,
    TAG_PASS_ON, TAG_WITHDRAW_CONTACT_REQUEST, TAG_UNSAY, TAG_UNSAID, TAG_WITHDRAWALS_BOOK, TAG_FILE_CHUNK_THROUGH, TAG_PROFILE_THROUGH, TAG_PONG, TAG_ACK, TAG_POSTS, TAG_CHANNEL_DATA, TAG_CHANNEL_ERROR, TAG_DISPLAY_NAME, TAG_UNSUPPORTED, TAG_FILE_DATA,
    TAG_FILE_ERROR, TAG_PROFILE, TAG_REFUSED, TAG_USER_KEY, TAG_HELLO, TAG_COVER_HELLO, TAG_NOT_PEER, TAG_INTRODUCE, TAG_CALL_INVITE, TAG_CALL_ACCEPT,
    TAG_CALL_REJECT, TAG_CALL_HANGUP, TAG_CALL_AUDIO, TAG_COVER_ACK, TAG_REKEY, TAG_CALL_VIDEO, TAG_CALL_KEYFRAME, TAG_FILE_OFFER, TAG_FILE_ANSWER,
    TAG_FILE_CANCEL, TAG_FILE_DONE, TAG_FILE_CLOSED, TAG_CHANNEL_POST, TAG_CHANNEL_EVENT, TAG_GROUP_INTRO, TAG_BUNDLE, TAG_CARBON, TAG_CARBON_FOR, TAG_DELIVERED, TAG_CALL_ELSEWHERE, TAG_FOLLOW,
    TAG_ADDRESSES, TAG_CARD, TAG_CALL_RESUME, TAG_CALL_CAMERA_OFF, TAG_CALL_SCREEN, TAG_CALL_CONFERENCE, TAG_CALL_ROSTER, TAG_CALL_HEARING, TAG_CALL_FORWARD,
    TAG_CALL_RELAYED, TAG_TIMER, TAG_CALL_DECODES, TAG_CALL_SHOWN_IN, TAG_CALL_VIA, TAG_PROFILE_CHANGED, TAG_CALL_CONTROL, TAG_CALL_INPUT, TAG_PEERS_BOOK,
    TAG_REMOVED, TAG_DEVICE_LIST, TAG_DEVICE_ADDRESSES, TAG_REQUESTS_BOOK, TAG_REACHING, TAG_CALL_THROUGH, TAG_CALL_ROOM, TAG_FILE_THROUGH, TAG_CALL_CONFERENCE_LINK,
    TAG_MANUAL_ADDRESS, TAG_DISCOVERABLE, TAG_RECEIVE_LIMIT, TAG_REACHING_DELAYS,
];

/// One frame of every kind, and of every kind within one (each roster
/// action, device-list change, control and input), made from fixed
/// values only, so its bytes are the same every time.
fn every_frame() -> Vec<(&'static str, Frame)> {
    use crate::channel::{BucketDigest, ChannelId, ChannelPost, RosterAction, RosterEvent};
    use crate::conference::{Added, Left, Roster};
    use crate::devicelist::{Change, Entry};
    use crate::remote::{Control, Input};
    let name = DisplayName { name: "Zoë".into(), at: 5, signature: [9; 64] };
    let channel = ChannelId { name: "général".into(), owner: [1; 32] };
    let event = |action| RosterEvent { at: 7, by: [2; 32], subject: [3; 32], action, prev: vec![[4; 32]], signature: [5; 64] };
    let post = ChannelPost { author: [6; 32], created_at: 8, text: "live".into(), name: Some("Zoë".into()), signature: [7; 64] };
    let roster = Roster {
        id: [8; 16],
        founder: [1; 32],
        added: vec![Added { member: [2; 32], by: [1; 32], round: 0, number: 2, signature: [3; 64] }],
        left: vec![Left { member: [2; 32], round: 0, by: [2; 32], by_round: 1, signature: [4; 64] }],
    };
    let piece = crate::video::VideoPiece { frame: 2, index: 1, count: 3, key: true, h265: true, small: false, data: vec![4; 9] };
    let entry = |change| Entry { at: 6, device: [3; 32], change, signature: [4; 64] };
    let request = |status| crate::requests::Request { public_key: [7; 32], status, address: "198.51.100.7:1977".into(), first_seen: 1, last_seen: 2 };
    let call = |signal| Frame::Notice(Notice::Call(signal));
    vec![
        ("hello", Frame::Hello { version: 1, min_version: 1, datagram_key: [2; 32], seen_as: "[2001:db8::1]:1977".into(), me: DeviceProof { user: [3; 32], device: [4; 32], cert: [5; 64], proof: [6; 64] } }),
        ("cover ack", Frame::CoverAck(1, 2, 3)),
        ("rekey", Frame::Rekey { datagram_key: [7; 32] }),
        ("ping", Frame::Request(1, Request::Ping)),
        ("message", Frame::Request(2, Request::Message { id: [3; 16], sent_at: 4, lifetime: 60, text: "hi".into() })),
        ("list posts", Frame::Request(3, Request::ListPosts { since: 5 })),
        ("read marks", Frame::Notice(Notice::ReadMarks { entries: vec![(crate::readmarks::Chat::Contact([4; 32]), 9), (crate::readmarks::Chat::Channel(ChannelId { name: "general".into(), owner: [5; 32] }), 10)] })),
        ("who am i", Frame::Request(6, Request::WhoAmI)),
        ("file chunk", Frame::Request(7, Request::FileChunk { id: [1; 16], offset: 2, len: 3 })),
        ("channel catch-up", Frame::Request(8, Request::ChannelCatchUp { channel: channel.clone(), from: 1, digests: vec![BucketDigest { bucket: 1, count: 2, xor: 3 }] })),
        ("profile ask", Frame::Request(9, Request::Profile { have: 3, from: 6144 })),
        ("join user", Frame::Request(10, Request::JoinUser { name: "phone".into() })),
        ("withdraw contact request", Frame::Request(11, Request::WithdrawContactRequest { root_key: [4; 32] })),
        ("pass on", Frame::Request(11, Request::PassOn { contact: [2; 32], id: [1; 16], sent_at: 5, lifetime: 0, text: "through".into() })),
        ("file chunk through", Frame::Request(12, Request::FileChunkThrough { contact: [4; 32], id: [1; 16], offset: 4000, len: 4000 })),
        ("profile through", Frame::Request(14, Request::ProfileThrough { contact: [4; 32], have: 9, from: 6144 })),
        ("pong", Frame::Reply(1, Reply::Pong)),
        ("ack", Frame::Reply(2, Reply::Ack)),
        ("posts", Frame::Reply(3, Reply::Posts { more: true, posts: vec![Post { created_at: 9, visibility: Visibility::Private, signature: [1; 64], text: "hello".into() }], deleted: Vec::new() })),
        ("channel data", Frame::Reply(4, Reply::ChannelData { more: false, roster: vec![event(RosterAction::Joined)], posts: vec![post.clone()], names: vec![([1; 32], name.clone())] })),
        ("channel error", Frame::Reply(5, Reply::ChannelError("no".into()))),
        ("display name", Frame::Reply(6, Reply::DisplayName(Some(name.clone())))),
        ("no display name", Frame::Reply(6, Reply::DisplayName(None))),
        ("unsupported", Frame::Reply(7, Reply::Unsupported)),
        ("file data", Frame::Reply(8, Reply::FileData(vec![1, 2, 3]))),
        ("file error", Frame::Reply(9, Reply::FileError("gone".into()))),
        ("profile", Frame::Reply(10, Reply::Profile(Some(crate::profile::ProfilePart { at: 4, signature: [5; 64], status: "out".into(), avatar_len: 9, from: 0, data: vec![6; 9] })))),
        ("no profile", Frame::Reply(10, Reply::Profile(None))),
        ("refused", Frame::Reply(11, Reply::Refused("not allowed".into()))),
        ("user key", Frame::Reply(12, Reply::UserKey { seed: [7; 16], list: "user x\n".into() })),
        ("cover hello", Frame::Notice(Notice::CoverHello { sends: Some(64_000), requires: None, limit: None })),
        ("receive limit", Frame::Notice(Notice::ReceiveLimit { limit: Some(1_000_000), in_call: false })),
        ("receive limit, in a call", Frame::Notice(Notice::ReceiveLimit { limit: None, in_call: true })),
        ("not peer", Frame::Notice(Notice::NotPeer)),
        ("introduce", Frame::Notice(Notice::Introduce { root_key: [1; 32], name: Some(name.clone()) })),
        ("removed", Frame::Notice(Notice::Removed)),
        ("call invite", call(CallSignal::Invite { video: true, at: 0 })),
        ("call invite, with its time", call(CallSignal::Invite { video: true, at: 1_700_000_000 })),
        ("call accept", call(CallSignal::Accept { video: false })),
        ("call reject", call(CallSignal::Reject)),
        ("call hangup", call(CallSignal::Hangup)),
        ("call audio", call(CallSignal::Audio(vec![1; 5]))),
        ("call video", call(CallSignal::Video(piece.clone()))),
        ("call keyframe", call(CallSignal::KeyframeRequest { small: true })),
        ("call elsewhere", call(CallSignal::AnsweredElsewhere)),
        ("call resume", call(CallSignal::Resume)),
        ("call camera off", call(CallSignal::CameraOff)),
        ("call screen", call(CallSignal::Screen(true))),
        ("call decodes", call(CallSignal::Decodes { h265: true })),
        ("call shown in", call(CallSignal::ShownIn { width: 1920, height: 1080, small_width: 480, small_height: 270 })),
        ("call via", call(CallSignal::Via { id: 4, signal: Box::new(CallSignal::Video(piece)) })),
        ("call conference", call(CallSignal::ConferenceInvite(roster.clone()))),
        ("call conference link", call(CallSignal::ConferenceLink(roster.clone()))),
        ("call roster", call(CallSignal::Roster(roster))),
        ("call hearing", call(CallSignal::Hearing(vec![[1; 32], [2; 32]]))),
        ("call forward", call(CallSignal::Forward { from: [3; 32], id: 3, on: true })),
        ("call relayed", call(CallSignal::Relayed { id: 3, opus: vec![1, 2, 3] })),
        ("control ask", call(CallSignal::Control(Control::Ask))),
        ("control give back", call(CallSignal::Control(Control::GiveBack))),
        ("control allow", call(CallSignal::Control(Control::Allow))),
        ("control refuse", call(CallSignal::Control(Control::Refuse("no".into())))),
        ("control stop", call(CallSignal::Control(Control::Stop))),
        ("control paused", call(CallSignal::Control(Control::Paused(true)))),
        ("input pointer", call(CallSignal::Input(Input::Pointer { x: 1, y: 65535 }))),
        ("input button", call(CallSignal::Input(Input::Button { button: crate::remote::BUTTON_RIGHT, down: true }))),
        ("input wheel", call(CallSignal::Input(Input::Wheel { dx: -120, dy: 240 }))),
        ("input key", call(CallSignal::Input(Input::Key { key: 'é', down: false }))),
        ("input text", call(CallSignal::Input(Input::Text("£5 🐝".into())))),
        ("file offer", Frame::Notice(Notice::FileOffer { id: [1; 16], name: "a.txt".into(), size: 3, sha256: [2; 32] })),
        ("file answer", Frame::Notice(Notice::FileAnswer { id: [1; 16], accepted: true })),
        ("file cancel", Frame::Notice(Notice::FileCancel { id: [1; 16], reason: "x".into() })),
        ("file done", Frame::Notice(Notice::FileDone { id: [1; 16] })),
        ("file closed", Frame::Notice(Notice::FileClosed { id: [1; 16] })),
        ("channel post", Frame::Notice(Notice::ChannelPost { channel: channel.clone(), post })),
        ("group joined", Frame::Notice(Notice::ChannelEvent { channel: channel.clone(), event: event(RosterAction::Joined) })),
        ("group left", Frame::Notice(Notice::ChannelEvent { channel: channel.clone(), event: event(RosterAction::Left) })),
        ("group named", Frame::Notice(Notice::ChannelEvent { channel: channel.clone(), event: event(RosterAction::Named { text: "t".into() }) })),
        ("group timer", Frame::Notice(Notice::ChannelEvent { channel: channel.clone(), event: event(RosterAction::Timer { seconds: 60 }) })),
        (
            "group intro",
            Frame::Notice(Notice::GroupIntro(crate::groupintro::Intro { channel: channel.clone(), from: [1; 32], to: [2; 32], at: 9, sealed: vec![3; 5], signature: [4; 64] })),
        ),
        (
            "device list",
            Frame::Notice(Notice::DeviceList {
                entries: vec![entry(Change::Added { name: "desk".into() }), entry(Change::Renamed { name: "desktop".into() }), entry(Change::Relay { allowed: true }), entry(Change::Removed)],
            }),
        ),
        ("device addresses", Frame::Notice(Notice::DeviceAddresses(vec![DeviceAddress { device: [6; 32], at: 9, addresses: vec!["2001:db8::6".into(), "10.0.0.6".into()] }, DeviceAddress { device: [7; 32], at: 10, addresses: vec![] }]))),
        ("bundle", Frame::Notice(Notice::Bundle { more: true, bytes: b"peer\tbob".to_vec() })),
        (
            "withdrawals book",
            Frame::Notice(Notice::WithdrawalsBook {
                entries: vec![
                    crate::withdrawals::Withdrawal { what: crate::withdrawals::What::ContactRequest, to: [2; 32], address: "2001:db8::2".into(), changed_at: 7, waiting: true },
                    crate::withdrawals::Withdrawal { what: crate::withdrawals::What::ContactRequest, to: [3; 32], address: String::new(), changed_at: 8, waiting: false },
                ],
            }),
        ),
        ("peers book", Frame::Notice(Notice::PeersBook { entries: vec![BookPeer { key: [6; 32], name: "alice".into(), address: "[2001:db8::7]:1977".into(), at: 1_700_000_000, present: false }] })),
        (
            "requests book",
            Frame::Notice(Notice::RequestsBook {
                entries: vec![
                    BookRequest { request: request(crate::requests::Status::Pending), name: Some(name) },
                    BookRequest { request: request(crate::requests::Status::Dismissed), name: None },
                    BookRequest { request: request(crate::requests::Status::Blocked), name: None },
                ],
                more: true,
            }),
        ),
        ("carbon", Frame::Notice(Notice::Carbon { peer: [3; 32], id: [4; 16], sent: true, at: 1, expires: 61, text: "c".into() })),
        ("carbon for", Frame::Notice(Notice::CarbonFor { device: [9; 32], carbon: Box::new(Notice::Carbon { peer: [3; 32], id: [4; 16], sent: true, at: 1, expires: 61, text: "c".into() }) })),
        ("follow", Frame::Notice(Notice::Follow { root_key: [5; 32], following: true })),
        ("addresses", Frame::Notice(Notice::Addresses(vec!["2001:db8::5".into(), "192.168.1.2".into()]))),
        ("manual address", Frame::Notice(Notice::ManualAddress { at: 1_700_000_000, items: vec!["203.0.113.7".into(), "home.example.org:5000".into()] })),
        ("discoverable", Frame::Notice(Notice::Discoverable { at: 1_700_000_000, on: false })),
        ("card", Frame::Notice(Notice::Card(crate::card::Card { from: [1; 32], to: [8; 32], at: 1, addresses: vec!["2001:db8::5".into()], signature: [2; 64] }))),
        ("timer", Frame::Notice(Notice::Timer(crate::disappearing::Timer { seconds: 86_400, set_at: 1_700_000_000, by: [9; 32] }))),
        ("profile changed", Frame::Notice(Notice::ProfileChanged)),
        ("reaching", Frame::Notice(Notice::Reaching(vec![[1; 32], [2; 32]]))),
        ("reaching delays", Frame::Notice(Notice::ReachingDelays(vec![([1; 32], 30)]))),
        ("call through", Frame::Notice(Notice::CallThrough { contact: [5; 32], signal: CallSignal::Audio(vec![1, 2, 3]) })),
        ("call room", Frame::Notice(Notice::CallRoom { contact: [6; 32], audio: 80, video_bps: 500_000, video_piece: 1100, video_on: true, bigger: 900_000, relayed: 2, covered: true })),
        ("file through", Frame::Notice(Notice::FileThrough { contact: [7; 32], onward: true, notice: Box::new(Notice::FileOffer { id: [1; 16], name: "a.txt".into(), size: 9, sha256: [2; 32] }) })),
        ("delivered", Frame::Notice(Notice::Delivered { peer: [3; 32], id: [4; 16] })),
        ("delivered, for a device", Frame::Notice(Notice::CarbonFor { device: [9; 32], carbon: Box::new(Notice::Delivered { peer: [3; 32], id: [4; 16] }) })),
        ("unsay", Frame::Request(15, Request::Unsay { id: Some([4; 16]), at: 5, digest: [6; 16] })),
        ("unsay, from before ids", Frame::Request(16, Request::Unsay { id: None, at: 5, digest: [6; 16] })),
        ("unsaid", Frame::Notice(Notice::Unsaid { peer: [3; 32], line: crate::history::LineRef { direction: crate::history::Direction::Sent, id: Some([4; 16]), at: 5, digest: [6; 16] }, done: false })),
        ("unsaid, for a device", Frame::Notice(Notice::CarbonFor { device: [9; 32], carbon: Box::new(Notice::Unsaid { peer: [3; 32], line: crate::history::LineRef { direction: crate::history::Direction::Received, id: None, at: 5, digest: [6; 16] }, done: true }) })),
        ("posts, with deletions", Frame::Reply(3, Reply::Posts { more: false, posts: Vec::new(), deleted: vec![crate::posts::Deletion { post_at: 9, post_signature: [1; 64], deleted_at: 10, signature: [2; 64], public: false }] })),
    ]
}

/// Every kind of frame goes as the same bytes it always has (kept in
/// `protocol/frames.txt`), and decodes to a frame that goes as them
/// again; and every tag is among them. What's sent is what builds
/// already out there read, so a change here is a change to the
/// protocol: `SWARM_BLESS=1` writes the file afresh, for a change
/// that's meant.
#[test]
fn every_frame_encodes_as_it_always_has() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/protocol/frames.txt");
    let frames = every_frame();
    let now: String = frames.iter().map(|(name, frame)| format!("{name}: {}\n", crate::wire::hex(&frame.encode()))).collect();
    if std::env::var_os("SWARM_BLESS").is_some() {
        std::fs::write(path, &now).unwrap();
    }
    let kept = std::fs::read_to_string(path).unwrap();
    for (now, kept) in now.lines().zip(kept.lines()) {
        assert_eq!(now, kept, "a frame's bytes changed");
    }
    assert_eq!(now.lines().count(), kept.lines().count(), "frames added or taken away");
    for (name, frame) in &frames {
        let bytes = frame.encode();
        let again = Frame::decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        if !matches!(again, Frame::Notice(Notice::Call(CallSignal::ConferenceInvite(_) | CallSignal::ConferenceLink(_) | CallSignal::Roster(_)))) {
            assert_eq!(again.encode(), bytes, "{name} didn't decode to itself");
        }
    }
    let seen: std::collections::HashSet<u8> = frames.iter().map(|(_, f)| f.encode()[0]).collect();
    let missing: Vec<String> = ALL_TAGS.iter().filter(|t| !seen.contains(t)).map(|t| format!("{t:#04x}")).collect();
    assert!(missing.is_empty(), "no frame here has tag(s) {missing:?}");
}

/// The frames whose last field is a run of bytes (voice, a piece of a
/// picture, a signal inside another): anything after it is part of it,
/// so a field can never be added to one of these, only a new tag made.
const ENDS_IN_BYTES: &[u8] = &[TAG_CALL_AUDIO, TAG_CALL_VIDEO, TAG_CALL_RELAYED, TAG_CALL_VIA, TAG_CALL_THROUGH];

/// The rules for changing the protocol, each held to.
mod changing_the_protocol {
    use super::*;

    /// Bytes after a frame's last field are a newer build's fields: read
    /// past, so the frame reads as it would without them. Only the frames
    /// that end in a run of bytes can't have more added.
    #[test]
    fn what_a_newer_build_adds_at_the_end_is_read_past() {
        for (name, frame) in every_frame() {
            let bytes = frame.encode();
            if ENDS_IN_BYTES.contains(&bytes[0]) {
                continue;
            }
            let as_it_is = Frame::decode(&bytes).unwrap().encode();
            let mut longer = bytes.clone();
            longer.extend([0xEE, 0x01, 0x02, 0x03]);
            let read = Frame::decode(&longer).unwrap_or_else(|e| panic!("{name} with more at the end: {e}"));
            assert_eq!(read.encode(), as_it_is, "{name} read what came after it");
        }
    }

    /// A list counted in a byte (addresses, a device's addresses) may hold
    /// more than this build takes: it keeps as many as it takes and reads
    /// what follows where it is.
    #[test]
    fn a_list_longer_than_this_build_takes_is_cut_and_whats_after_it_still_read() {
        let most = crate::addressbook::MAX_PER_PEER;
        let mut w = crate::wire::Writer::new(TAG_DEVICE_ADDRESSES);
        w.u8(2);
        w.array(&[6; 32]).u64(9).u8((most + 3) as u8);
        for i in 0..most + 3 {
            w.str(&format!("2001:db8::{i}"));
        }
        w.array(&[7; 32]).u64(10).u8(1).str("10.0.0.7");
        match Frame::decode(&w.into_bytes()).unwrap() {
            Frame::Notice(Notice::DeviceAddresses(devices)) => {
                assert_eq!(devices.len(), 2);
                assert_eq!(devices[0].addresses.len(), most);
                assert_eq!(devices[1], DeviceAddress { device: [7; 32], at: 10, addresses: vec!["10.0.0.7".into()] });
            }
            _ => panic!("not the device addresses"),
        }
        let mut w = crate::wire::Writer::new(TAG_ADDRESSES);
        w.u8((most + 1) as u8);
        for i in 0..most + 1 {
            w.str(&format!("2001:db8::{i}"));
        }
        assert!(matches!(Frame::decode(&w.into_bytes()).unwrap(), Frame::Notice(Notice::Addresses(list)) if list.len() == most));
    }

    /// A frame of a kind this build doesn't know is skipped (a notice or a
    /// reply), or answered `Unsupported` (a request, by its id), however
    /// short: never an error, which would end the connection.
    #[test]
    fn a_kind_this_build_doesnt_know_is_skipped_or_answered() {
        for tag in [0x3F, 0x3E] {
            assert!(matches!(Frame::decode(&[tag, 0, 0, 0, 9]).unwrap(), Frame::UnknownRequest(9)));
        }
        for unknown in [&[0x7F][..], &[0x7F, 1], &[0xFE], &[0xFE, 1, 2, 3], &[0xC0]] {
            assert!(matches!(Frame::decode(unknown).unwrap(), Frame::Unknown), "{unknown:?}");
        }
    }

    /// Every tag is used once, each in the range for its kind.
    #[test]
    fn every_tag_is_used_once_in_its_range() {
        let mut seen = std::collections::HashSet::new();
        for tag in ALL_TAGS {
            assert!(seen.insert(tag), "{tag:#04x} used twice");
        }
        for (name, frame) in every_frame() {
            let tag = frame.encode()[0];
            let kind = match frame {
                Frame::Request(..) => 0x01..=0x3F,
                Frame::Reply(..) => 0x40..=0x7F,
                _ => 0x80..=0xFF,
            };
            assert!(kind.contains(&tag), "{name}'s tag {tag:#04x} is in another kind's range");
        }
    }
}

/// Without cover, voice goes in datagrams of its own, and arrives as
/// voice, in order.
#[tokio::test]
async fn voice_goes_in_datagrams_without_cover() {
    let (mut a, mut b) = pair().await;
    let before = a.connection.stats().frame_tx.datagram;
    for n in 1..=3u8 {
        send_call(&mut a, CallSignal::Audio(vec![n; 40])).await.unwrap();
    }
    for n in 1..=3u8 {
        match recv(&mut b).await.unwrap() {
            Incoming::Notice(Notice::Call(CallSignal::Audio(opus))) => assert_eq!(opus, vec![n; 40]),
            other => panic!("expected voice, got {other:?}"),
        }
    }
    assert_eq!(a.connection.stats().frame_tx.datagram - before, 3, "as datagrams");
}

/// Voice datagrams lost in between are each an empty frame (filled in by
/// the decoder), up to three; one older than the newest is dropped.
#[tokio::test]
async fn lost_voice_is_filled_in_and_late_voice_dropped() {
    let (_a, mut b) = pair().await;
    let body = |seq: u32, n: u8| {
        let mut body = vec![VOICE_DATAGRAM];
        body.extend(seq.to_be_bytes());
        body.extend(Frame::Notice(Notice::Call(CallSignal::Audio(vec![n; 10]))).encode());
        body
    };
    take_voice_datagram(&mut b, &body(10, 1));
    take_voice_datagram(&mut b, &body(12, 2));
    take_voice_datagram(&mut b, &body(11, 9));
    take_voice_datagram(&mut b, &body(20, 3));
    take_voice_datagram(&mut b, &body(u32::MAX, 4));
    let mut heard = Vec::new();
    while let Some(bytes) = b.voice_ready.pop_front() {
        match Frame::decode(&bytes).unwrap() {
            Frame::Notice(Notice::Call(CallSignal::Audio(opus))) => heard.push(opus.first().copied().unwrap_or(0)),
            _ => panic!("not voice"),
        }
    }
    assert_eq!(heard, vec![1, 0, 2, 0, 0, 0, 3]);
}
