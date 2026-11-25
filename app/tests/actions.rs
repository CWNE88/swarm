//! The verbs both front ends share (`swarm_app::actions`), on a real
//! node of their own: what each does to the files, and what it says.

use std::sync::Arc;
use swarm_app::actions;
use swarm_app::notes::Kind;
use swarm_app::sidebar::{PeerRow, RequestRow};
use swarm_core::identity::Identity;
use swarm_core::node::{self, NodeConfig, NodeHandle, Paths};
use swarm_core::peers::{self, Peer};
use swarm_core::settings::AutoAnswer;

async fn start(label: &str) -> NodeHandle {
    let dir = std::env::temp_dir().join(format!("swarm-actions-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = NodeConfig { paths: Paths::new(&dir), port: 0, cover_bits_per_second: None, log: Arc::new(|_| {}) };
    let (handle, call_events) = node::start(Arc::new(Identity::generate()), config).await.unwrap();
    drop(call_events);
    handle
}

/// A peer called `name` on `node`'s list, as the sidebar shows them.
fn peer(node: &NodeHandle, name: &str, label: &str) -> PeerRow {
    let public_key = Identity::generate().public_key_bytes();
    let peer = Peer { name: name.to_string(), address: "[::1]:9".to_string(), public_key, cover_bits_per_second: None, cover_required: false };
    peers::add(&node.paths().peers(), &peer).unwrap();
    node.add_peer_live(peer);
    let handle = peers::handle_of(&public_key);
    PeerRow {
        entries: vec![handle.clone()],
        handle,
        label: label.to_string(),
        list_label: label.to_string(),
        address: "[::1]:9".to_string(),
        public_key,
        online: false,
        not_added: false,
        removed_us: false,
        asked: false,
        unread: 0,
        since: None,
        cover_bits_per_second: None,
        cover_required: false,
        cover_tag: None,
        auto_answer: AutoAnswer::Off,
        auto_control: false,
        folder: node.paths().clone(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_timer_steps_on_and_names_the_person_as_shown() {
    let node = start("timer").await;
    let row = peer(&node, "bob", "Bob, bob-laptop");
    let first = actions::next_timer(&node, &row);
    assert_eq!(first.kind, Kind::Info);
    assert!(first.text.starts_with("messages with Bob, bob-laptop delete themselves after"), "{}", first.text);
    assert!(node.timer("bob").seconds > 0);
    let mut last = first;
    for _ in 0..20 {
        if node.timer("bob").seconds == 0 {
            break;
        }
        last = actions::next_timer(&node, &row);
    }
    assert_eq!(node.timer("bob").seconds, 0);
    assert_eq!(last.text, "messages with Bob, bob-laptop are kept again (from now on; they're told)");
}

#[tokio::test(flavor = "multi_thread")]
async fn answering_by_itself_steps_on_from_what_is_saved_not_what_the_row_last_read() {
    let node = start("answer").await;
    let row = peer(&node, "bob", "Bob");
    let settings = node.paths().settings();
    swarm_core::settings::set_auto_answer(&settings, &row.public_key, AutoAnswer::Voice).unwrap();
    let note = actions::cycle_auto_answer(&row);
    assert_eq!(swarm_core::settings::auto_answer(&settings, &row.public_key), AutoAnswer::VoiceAndVideo);
    assert!(note.text.starts_with("calls from Bob: "), "{}", note.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_dismissed_goes_and_one_blocked_stays_away() {
    let node = start("requests").await;
    let path = node.paths().requests();
    let (one, two) = (Identity::generate().public_key_bytes(), Identity::generate().public_key_bytes());
    swarm_core::requests::record(&path, one, "[::1]:9", 1).unwrap();
    swarm_core::requests::record(&path, two, "[::1]:9", 1).unwrap();
    let row = RequestRow { public_key: one, address: "[::1]:9".to_string(), name: Some("Eve".to_string()), folder: node.paths().clone() };

    let note = actions::dismiss_request(&row);
    assert_eq!(note.text, "dismissed Eve — it comes back if they try again");
    let pending: Vec<_> = swarm_core::requests::pending(&path).unwrap().into_iter().map(|r| r.public_key).collect();
    assert_eq!(pending, vec![two]);

    assert_eq!(actions::block_question(&row), "block Eve? They'll never show up here again.");
    let mallory = RequestRow { public_key: two, name: Some("Mallory".to_string()), ..row.clone() };
    assert_eq!(actions::block_request(&mallory).kind, Kind::Success);
    assert!(swarm_core::requests::is_blocked(&path, &two).unwrap());
    assert!(swarm_core::requests::pending(&path).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_keys_are_listed_with_their_names_and_unblocked() {
    let node = start("blocked").await;
    let path = node.paths().requests();
    let (one, two) = (Identity::generate(), Identity::generate().public_key_bytes());
    let theirs = node.paths().dir().join("eve-name");
    swarm_core::displayname::set(&theirs, &one, "Eve").unwrap();
    let named = swarm_core::displayname::current(&theirs).unwrap().unwrap();
    swarm_core::displayname::cache_one(&node.paths().displaynames(), one.public_key_bytes(), &named).unwrap();
    let one = one.public_key_bytes();
    swarm_core::requests::block(&path, one, 1).unwrap();
    swarm_core::requests::block(&path, two, 2).unwrap();
    let listed: Vec<_> = actions::blocked(&node).into_iter().map(|r| (r.public_key, r.name)).collect();
    assert_eq!(listed, vec![(two, None), (one, Some("Eve".to_string()))]);
    let row = actions::blocked(&node).pop().unwrap();
    assert_eq!(row.label(), "Eve");

    let note = actions::unblock(&row);
    assert_eq!(note.kind, Kind::Success);
    assert!(note.text.ends_with("can ask to be added again"), "{}", note.text);
    assert_eq!(actions::blocked(&node).len(), 1);
    assert!(swarm_core::requests::record(&path, one, "[::1]:9", swarm_core::unix_now() + 10).unwrap(), "asks again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_saved_with_a_cover_rate_not_understood_is_saved_and_says_so() {
    let node = start("cover").await;
    let row = peer(&node, "bob", "Bob");
    let note = actions::saved_peer(&node, Ok("saved bob".to_string()), &row.handle, "fast please");
    assert_eq!(note.kind, Kind::Error);
    assert!(note.text.starts_with("saved bob, but the cover rate wasn't understood"), "{}", note.text);
    assert_eq!(actions::saved_peer(&node, Ok("saved bob".to_string()), &row.handle, " ").kind, Kind::Success);
    assert_eq!(actions::saved_peer(&node, Ok("saved bob".to_string()), &row.handle, "64k").kind, Kind::Success);
    let rate = |node: &NodeHandle| peers::load(&node.paths().peers()).unwrap().into_iter().find(|p| p.name == "bob").unwrap().cover_bits_per_second;
    assert_eq!(rate(&node), Some(64_000));
    assert_eq!(actions::set_cover(&node, "bob", "").kind, Kind::Success);
    assert_eq!(rate(&node), None);
    assert_eq!(actions::saved_peer(&node, Err(anyhow::anyhow!("no such peer")), "carol", "64k"), swarm_app::notes::Note::error("no such peer"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_blank_or_unchanged_says_nothing() {
    let node = start("name").await;
    assert_eq!(actions::set_name(&node, "  "), None);
    let set = actions::set_name(&node, " Ann ").expect("a new name is said");
    assert_eq!(set.text, "you're Ann now — others see it beside your key");
    assert_eq!(actions::set_name(&node, "Ann"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transfer_that_isnt_there_says_why_it_couldnt_be_declined() {
    let node = start("transfer").await;
    let transfer = swarm_core::transfer::Transfer {
        id: swarm_core::transfer::new_id(),
        direction: swarm_core::transfer::Direction::In,
        peer: [7; 32],
        state: swarm_core::transfer::State::Offered,
        size: 10,
        sha256: [0; 32],
        created: 1,
        name: format!("{}.pdf", "minutes".repeat(10)),
        path: Default::default(),
        note: String::new(),
        cleared: false,
        settled: false,
    };
    let note = actions::decline_transfer(&node, &transfer);
    assert_eq!(note.kind, Kind::Error);
    assert!(note.text.starts_with("couldn't decline: "), "{}", note.text);
    assert!(actions::transfer_left(&transfer).text.starts_with("minutesminutes"));
    assert!(actions::transfer_left(&transfer).text.contains("….pdf left for later"), "{}", actions::transfer_left(&transfer).text);
}

/// An offer put to the person: open while it waits; taken on another
/// of their devices (off the list here) or cancelled, its question goes,
/// with a note only for the first (a cancel has news of its own).
#[tokio::test(flavor = "multi_thread")]
async fn an_offer_answered_elsewhere_is_asked_no_more() {
    use swarm_app::prompts::{offer_now, OfferNow};
    use swarm_core::transfer::{self, Direction, State};
    let node = start("offer-gone").await;
    let store = node.paths().transfers();
    let t = transfer::Transfer {
        id: transfer::new_id(),
        direction: Direction::In,
        peer: [7; 32],
        state: State::Offered,
        size: 10,
        sha256: [0; 32],
        created: 1,
        name: "minutes.pdf".to_string(),
        path: Default::default(),
        note: String::new(),
        cleared: false,
        settled: false,
    };
    transfer::add(&store, t.clone()).unwrap();
    let offer = node.transfers().into_iter().find(|s| s.transfer.id == t.id).unwrap();
    assert!(matches!(offer_now(&node, &offer), OfferNow::Open));
    transfer::update(&store, &t.id, |t| t.state = State::Cancelled).unwrap();
    assert!(matches!(offer_now(&node, &offer), OfferNow::Gone(None)), "cancelled: its news says so");
    transfer::remove(&store, &t.id).unwrap();
    match offer_now(&node, &offer) {
        OfferNow::Gone(Some(note)) => assert_eq!(note.text, "minutes.pdf was taken on another of your devices"),
        _ => panic!("taken elsewhere: said so"),
    }
}

/// Someone met on two devices has an entry for each besides their own,
/// all with their key: their chat is shown once, under their name.
#[tokio::test(flavor = "multi_thread")]
async fn a_contact_on_two_devices_has_one_chat() {
    use swarm_app::chat::{self, ChatWhat};
    use swarm_core::history::{self, Direction};
    let node = start("two-devices").await;
    let mut row = peer(&node, "bob", "bob");
    row.entries = vec!["bob".to_string(), "bob #Wgxhz1".to_string(), "bob #Kp3rT9".to_string()];
    let history = node.paths().history();
    history::append_expiring(&history, &row.public_key, Direction::Sent, "hello", 100, 0).unwrap();
    history::append_expiring(&history, &row.public_key, Direction::Received, "hi", 200, 0).unwrap();
    let said: Vec<(String, String)> = chat::peer_chat(&row)
        .into_iter()
        .filter_map(|i| match i.what {
            ChatWhat::Said { name, text, .. } => Some((name, text)),
            _ => None,
        })
        .collect();
    assert_eq!(said.len(), 2, "{said:?}");
    assert_eq!(said[1], ("bob".to_string(), "hi".to_string()));
}

/// A message sent shows ◷ until one of their devices has it, then ✓;
/// theirs shows neither; a missed call is one line, however many of the
/// user's devices noted it.
#[tokio::test(flavor = "multi_thread")]
async fn sent_messages_say_whether_theyve_arrived_and_a_missed_call_is_one_line() {
    use swarm_app::chat::{self, ChatWhat, Sent};
    use swarm_core::history::{self, Direction};
    let node = start("marks").await;
    let row = peer(&node, "bob", "Bob");
    let (history, waiting) = (node.paths().history(), node.paths().waiting());
    history::append_message(&history, &row.public_key, Direction::Sent, "there yet?", 100, 0, Some([2; 16])).unwrap();
    history::append_message(&history, &row.public_key, Direction::Sent, "and this?", 101, 0, Some([1; 16])).unwrap();
    swarm_core::waiting::add(&waiting, &row.public_key, [1; 16], 101, 0, "and this?").unwrap();
    history::append_expiring(&history, &row.public_key, Direction::Missed, "call", 200, 0).unwrap();
    history::append_expiring(&history, &row.public_key, Direction::Missed, "call", 202, 0).unwrap();
    history::append_expiring(&history, &row.public_key, Direction::Received, "yes", 300, 0).unwrap();
    let items = chat::peer_chat(&row);
    let shown: Vec<(Sent, String)> = items
        .iter()
        .map(|i| match &i.what {
            ChatWhat::Said { text, .. } => (i.sent, text.clone()),
            ChatWhat::Missed { name } => (i.sent, chat::missed_call(name)),
            _ => (i.sent, String::new()),
        })
        .collect();
    assert_eq!(
        shown,
        [
            (Sent::Delivered, "there yet?".to_string()),
            (Sent::Waiting, "and this?".to_string()),
            (Sent::NotOurs, "Missed call from Bob".to_string()),
            (Sent::NotOurs, "yes".to_string()),
        ]
    );
    swarm_core::waiting::remove(&waiting, &row.public_key, &[1; 16]).unwrap();
    assert_eq!(chat::peer_chat(&row)[1].sent, Sent::Delivered);
}

/// Deleted: their message and a missed call (every device's note of it)
/// go at once, as does one the user sent that hadn't reached them; one
/// that had stays, faint (`Deleting`), till they've heard it's gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_line_deleted_goes_and_one_sent_waits_for_them_to_hear() {
    use swarm_app::chat::{self, Sent};
    use swarm_core::history::{self, Direction};
    let node = start("deleting").await;
    let row = peer(&node, "bob", "Bob");
    let history = node.paths().history();
    history::append_message(&history, &row.public_key, Direction::Sent, "regret this", 100, 0, Some([1; 16])).unwrap();
    history::append_expiring(&history, &row.public_key, Direction::Missed, "call", 200, 0).unwrap();
    history::append_expiring(&history, &row.public_key, Direction::Missed, "call", 230, 0).unwrap();
    history::append_message(&history, &row.public_key, Direction::Received, "and this", 300, 0, Some([2; 16])).unwrap();
    history::append_message(&history, &row.public_key, Direction::Sent, "not gone yet", 400, 0, Some([3; 16])).unwrap();
    swarm_core::waiting::add(&node.paths().waiting(), &row.public_key, [3; 16], 400, 0, "not gone yet").unwrap();
    let items = chat::peer_chat(&row);
    assert_eq!(items.len(), 4);
    assert_eq!(items[3].sent, Sent::Waiting);
    assert_eq!(items[1].lines.len(), 2, "a missed call noted twice is one line, and both go");
    for item in &items {
        assert_eq!(swarm_app::actions::delete_line(&node, &row, &item.lines).map(|n| n.text), None, "said nothing");
    }
    let items = chat::peer_chat(&row);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].sent, Sent::Deleting, "till Bob's heard");
    assert_eq!(items[0].sent.mark(), "◷");
    assert_eq!(swarm_core::unsaying::shown(&node.paths().unsaying(), &row.public_key).unwrap(), [items[0].lines[0]]);
    assert_eq!(swarm_core::unsaying::all(&node.paths().unsaying(), &row.public_key).unwrap().len(), 2, "the one not delivered, still to tell them");
    assert!(swarm_core::waiting::all(&node.paths().waiting(), &row.public_key).unwrap().is_empty(), "and no longer waiting to go");
}

#[tokio::test(flavor = "multi_thread")]
async fn someone_who_cant_be_rung_says_why_and_the_call_buttons_follow() {
    let node = start("callable").await;
    let calls = swarm_app::calls::Calls::default();
    let offline = peer(&node, "bob", "Bob");
    let why = calls.why_not_call(&node, &offline, false).unwrap();
    assert!(why.contains("isn't connected"), "{why}");
    assert!(calls.why_not_call(&node, &offline, true).is_some());
    let voice_only = PeerRow { online: true, cover_bits_per_second: Some(64_000), ..peer(&node, "carol", "Carol") };
    assert_eq!(calls.why_not_call(&node, &voice_only, false), None);
    let why = calls.why_not_call(&node, &voice_only, true).unwrap();
    assert!(why.contains("too low for video"), "{why}");
    let starved = PeerRow { online: true, cover_bits_per_second: Some(4_000), ..peer(&node, "dave", "Dave") };
    assert!(calls.why_not_call(&node, &starved, false).unwrap().contains("too low to carry voice"));
    let fine = PeerRow { online: true, ..peer(&node, "erin", "Erin") };
    assert_eq!(calls.why_not_call(&node, &fine, false), None);
    assert_eq!(calls.why_not_call(&node, &fine, true), None);
    assert!(calls.why_not_add(&node, &fine).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_cant_be_rung_says_why() {
    let node = start("device-callable").await;
    let calls = swarm_app::calls::Calls::default();
    let this = node.devices().devices.into_iter().find(|d| d.this).unwrap();
    assert_eq!(calls.why_not_call_device(&node, &this, false).as_deref(), Some("that's this device"));
    let other = swarm_core::node::devices::DeviceInfo { key: Identity::generate().public_key_bytes(), name: "phone".to_string(), this: false, online: false, relay: false, receive_only: false, through: None, address: String::new(), typed: String::new(), added_at: 1 };
    let why = calls.why_not_call_device(&node, &other, true).unwrap();
    assert!(why.starts_with("phone isn't connected"), "{why}");
    assert_eq!(swarm_app::devices::in_calls(&other), "phone (yours)");
}

#[tokio::test(flavor = "multi_thread")]
async fn another_of_your_devices_can_be_answered_automatically() {
    let node = start("device-answer").await;
    let this = node.devices().devices.into_iter().find(|d| d.this).unwrap();
    let other = swarm_core::node::devices::DeviceInfo { key: Identity::generate().public_key_bytes(), name: "desktop".to_string(), this: false, online: false, relay: false, receive_only: false, through: None, address: String::new(), typed: String::new(), added_at: 1 };
    assert_eq!(swarm_app::devices::auto_answer(&node, &other.key), AutoAnswer::Off, "rings unless set");
    assert_eq!(actions::device_answer(&node, &other, None).text, "calls from desktop: answered automatically (voice only)");
    assert_eq!(actions::device_answer(&node, &other, None).text, "calls from desktop: answered automatically, video too");
    assert_eq!(swarm_app::devices::auto_answer(&node, &other.key), AutoAnswer::VoiceAndVideo);
    assert_eq!(swarm_app::devices::auto_answer(&node, &this.key), AutoAnswer::Off, "no other device let in");
    assert_eq!(swarm_core::settings::auto_answer(&node.paths().settings(), &node.identity().public_key_bytes()), AutoAnswer::Off, "nor your user");
    actions::device_answer(&node, &other, Some(AutoAnswer::Off));
    assert_eq!(swarm_app::devices::auto_answer(&node, &other.key), AutoAnswer::Off);
    assert_eq!(actions::device_answer(&node, &this, Some(AutoAnswer::Voice)).kind, swarm_app::notes::Kind::Error);
}

/// What's been read elsewhere stops counting as unread here, and a chat
/// only counts as read everywhere when someone's really looking at it.
#[tokio::test(flavor = "multi_thread")]
async fn read_on_another_device_is_read_here() {
    use swarm_app::sidebar::{Entry, Sidebar};
    use swarm_core::history::{self, Direction};
    use swarm_core::readmarks::Chat;
    let node = start("unread").await;
    let row = peer(&node, "bob", "bob");
    let (key, history) = (row.public_key, row.folder.history());
    let got = |text: &str, at: u64| history::append(&history, &key, Direction::Received, text, at).unwrap();
    let mut sidebar = Sidebar::new();
    sidebar.all = vec![Entry::Peer(row)];
    let unread = |sidebar: &Sidebar| match &sidebar.all[0] {
        Entry::Peer(row) => row.unread,
        _ => panic!(),
    };
    let chat = Chat::Contact(key);
    got("before", 1000);
    sidebar.mark_read(&node, None, true);
    got("one", 1001);
    got("two", 1002);
    sidebar.mark_read(&node, None, true);
    assert_eq!(unread(&sidebar), 2);
    node.read_up_to(chat.clone(), 1001);
    sidebar.mark_read(&node, None, true);
    assert_eq!(unread(&sidebar), 1, "one read elsewhere");
    node.read_up_to(chat.clone(), 1002);
    sidebar.mark_read(&node, None, true);
    assert_eq!(unread(&sidebar), 0);

    let id = sidebar.all[0].id();
    got("three", 1003);
    sidebar.mark_read(&node, Some(&id), false);
    assert_eq!(node.read_marks().get(&chat), Some(&1002), "left open: not read everywhere");
    sidebar.mark_read(&node, Some(&id), true);
    assert_eq!(node.read_marks().get(&chat), Some(&1003), "being looked at");

    got("late", 1001);
    sidebar.mark_read(&node, None, true);
    assert_eq!(unread(&sidebar), 1, "a late arrival isn't read just for being old");
}

/// A contact's address set alone (Set address): an IP or a host name,
/// their name and key kept; their own code gives its addresses, someone
/// else's is refused, and so is nothing or nonsense.
#[tokio::test(flavor = "multi_thread")]
async fn a_contacts_address_is_set_alone() {
    let node = start("contact-address").await;
    let row = peer(&node, "frank", "Frank");
    let on_file = || peers::load(&node.paths().peers()).unwrap().into_iter().find(|p| p.public_key == row.public_key).unwrap();
    let set = actions::set_contact_address(&node, &row, "home.example.net:5000, 203.0.113.7");
    assert_eq!(set.kind, Kind::Success, "{}", set.text);
    assert_eq!(on_file().address, "home.example.net:5000,203.0.113.7");
    assert_eq!(on_file().name, "frank", "name kept");
    assert_eq!(actions::set_contact_address(&node, &row, "").kind, Kind::Error, "a contact needs an address");
    assert_eq!(actions::set_contact_address(&node, &row, "not an address!").kind, Kind::Error);
    let theirs = swarm_app::contact::line(&row.public_key, &[], &["198.51.100.4".parse().unwrap()], "Frank");
    assert_eq!(actions::set_contact_address(&node, &row, &theirs).kind, Kind::Success);
    assert_eq!(on_file().address, "198.51.100.4", "from their code");
    let someone_else = swarm_app::contact::line(&[9; 32], &[], &["198.51.100.5".parse().unwrap()], "Mallory");
    assert_eq!(actions::set_contact_address(&node, &row, &someone_else).kind, Kind::Error);
    assert_eq!(on_file().address, "198.51.100.4", "unchanged");
}

#[tokio::test(flavor = "multi_thread")]
async fn control_without_asking_is_per_contact_and_per_device_and_off_unless_set() {
    let node = start("auto-control").await;
    let (bob, carol) = (peer(&node, "bob", "Bob"), peer(&node, "carol", "Carol"));
    let settings = node.paths().settings();
    assert!(!swarm_core::settings::auto_control(&settings, &bob.public_key), "asked each time unless set");
    let note = actions::set_auto_control(&bob, None);
    assert_eq!(note.text, "Bob may now control your shared screen without asking");
    assert!(swarm_core::settings::auto_control(&settings, &bob.public_key));
    assert!(!swarm_core::settings::auto_control(&settings, &carol.public_key), "no one else let in");
    actions::set_auto_control(&bob, Some(false));
    assert!(!swarm_core::settings::auto_control(&settings, &bob.public_key));
    let this = node.devices().devices.into_iter().find(|d| d.this).unwrap();
    let other = swarm_core::node::devices::DeviceInfo { key: Identity::generate().public_key_bytes(), name: "laptop".to_string(), this: false, online: false, relay: false, receive_only: false, through: None, address: String::new(), typed: String::new(), added_at: 1 };
    actions::device_control(&node, &other, Some(true));
    assert!(swarm_app::devices::auto_control(&node, &other.key));
    assert_eq!(actions::device_control(&node, &this, Some(true)).kind, swarm_app::notes::Kind::Error);
    assert!(!swarm_app::devices::auto_control(&node, &this.key));
}
