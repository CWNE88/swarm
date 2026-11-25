//! The app behind the window: owns the node and the shared app layer's
//! state (`swarm_app`), takes the person's actions from the window, and
//! after anything changes sends the window a fresh `Snapshot` of what
//! to show. Runs as a task on the tokio runtime; the window runs on the
//! main thread and only draws.

use swarm_core::lock::Locked;
use crate::snapshot::{self, Snapshot};
use crate::{Answer, AvatarStep, Pane, RequestAnswer, Setting, SoundStep, TransferAnswer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use swarm_app::calls::Calls;
use swarm_core::identity::Identity;
use swarm_app::actions;
use swarm_app::text::capitalised;
use swarm_app::commands::{self, Confirm};
use swarm_app::jobs::{self, Done, Jobs, Timelines};
use swarm_app::notes::Note;
use swarm_app::prefs::Prefs;
use swarm_app::prompts::{self, Prompts};
use swarm_app::sidebar::{Entry, EntryId, Sidebar};
use swarm_app::{paths, peerbook};
use swarm_core::channel::ChannelId;
use swarm_core::node::{CallEvent, NodeHandle, TransferStatus};
use swarm_core::posts::{self, Visibility};
use swarm_core::transfer::TransferId;
use tokio::sync::{mpsc, oneshot};

/// Something the person did in the window.
pub enum Action {
    /// swarm in sight, or not (the phone's app gone to the background,
    /// its screen off): out of it, nothing counts as read.
    InSight(bool),
    /// The phone's window: a chat open over the list (true), or the list
    /// showing; the chat selected is only read while it's open.
    ChatShown(bool),
    Select(usize),
    /// Up (-1) or down (1) the list.
    Move(i32),
    /// Back to the chats.
    Escape,
    /// To your own posts, to write one.
    NewPost,
    /// Text one size larger (1), smaller (-1), or normal (0).
    TextSize(i32),
    ShowView(Pane),
    /// The list narrowed to pictures, or full width again.
    NarrowList(bool),
    /// The list dragged to this width (logical pixels).
    ListWidth(u32),
    /// A channel's members folded away, or shown again.
    HideMembers(bool),
    /// A channel's members dragged to this width (logical pixels).
    MembersWidth(u32),
    SetTab(i32),
    Send(String),
    /// Deletes a line of the open contact's chat: its `ChatLine::refs`.
    DeleteLine(String),
    /// Deletes one of the user's posts: its `PostItem::refs`.
    DeletePost(String),
    Post(String, bool),
    Refresh,
    Call(bool),
    HangUp,
    /// Removes whoever has this number from the conference.
    RemoveFromCall(i32),
    ToggleCamera,
    UseCamera(i32),
    ToggleScreen,
    /// Ask to control the screen shown large, or give it back.
    ToggleControl,
    /// Whoever's asking to control our screen: allowed, or refused.
    AnswerControl(bool),
    /// Whoever's controlling our screen: stopped.
    StopControl,
    /// Over their picture, while we control their screen: the pointer (0
    /// to 1 across and down), a button (see `App.control-button`), the
    /// wheel (in pixels), a key (as the window gives it).
    ControlPointer(f32, f32),
    ControlButton(i32, bool),
    ControlWheel(f32, f32),
    ControlKey(String, bool),
    ToggleMute,
    Preview(bool),
    Answer(bool),
    Reject,
    Offer(Answer),
    Confirm(bool),
    /// Deleting a contact, asked: block them too.
    ConfirmBlock,
    SendFile,
    /// The selected peer's conversation: on to the next timer (see
    /// `disappearing`).
    NextTimer,
    /// The selected peer's timer, by its place in `disappearing::CHOICES`.
    SetTimer(usize),
    AddPeer(String, String, String, String),
    UpdatePeer(String, String, String, String),
    RemovePeer,
    /// Follow someone's public posts: address, key.
    Follow(String, String),
    Unfollow,
    /// Where a peer's files go without asking: their key, the folder.
    SetReceive(String, String),
    CycleAnswer,
    /// How the selected peer's or device's calls are answered: off,
    /// voice, voice and video.
    SetAnswer(usize),
    /// The contact or device chosen may control your shared screen
    /// without asking, or not.
    SetControl(bool),
    Request(RequestAnswer),
    AddRequest(String),
    /// Adds the peer with this name to the call (the phone's Add, in a
    /// video call).
    AddToCall(String),
    /// Someone being picked, to add to the call (1) or invite into the
    /// channel open (2), by what's typed; 0: no longer.
    Pick(i32, String),
    /// A contact's chat, by their name (a channel's member clicked).
    OpenChat(String),
    /// A new group, of you alone (people are added after).
    NewGroup,
    /// A group of the contact selected and this one.
    GroupWith(String),
    /// The group selected renamed ("": its members' names).
    RenameGroup(String),
    Transfer(usize, TransferAnswer),
    ClearTransfers,
    SetName(String),
    /// Your status ("" for none).
    SetSays(String),
    /// The address you set by hand (blank clears it).
    SetManualAddress(String),
    /// Discoverable on the network each of your devices is on, or not.
    SetDiscoverable(bool),
    /// Add a contact or Follow someone opened (finding who's on this
    /// network) or closed.
    Browse(bool),
    Avatar(AvatarStep),
    /// Add a peer's camera reading a QR code: on, or off.
    Scan(bool),
    /// The code of the device chosen, read with the camera: for Add address.
    ScanDevice,
    Sound(SoundStep),
    /// The ringtone: chosen, played, or Swarm's own.
    Ringtone(SoundStep),
    /// A setting, and its value: on (1) or off (0), or the place chosen
    /// in its list.
    SetSetting(Setting, i32),
    /// Make an identity: a name (or ""), the passphrase, and it again.
    FirstRun(String, String, String),
    /// Bring an identity back: its 12 words, a name (or ""), the
    /// passphrase, and it again.
    RestoreIdentity(String, String, String, String),
    /// Add this device to your user: a code from another of yours, a
    /// name for this one (or ""), the passphrase, and it again.
    JoinUser(String, String, String, String),
    /// Unlock with this passphrase.
    Unlock(String),
    /// A forgotten passphrase: the 12 words, a new one, and it again.
    ResetPassphrase(String, String, String),
    /// Offer a passphrase.
    SuggestPassphrase,
    /// The new identity's words have been written down.
    WordsWritten,
    /// This device's name among your devices.
    DeviceRename(String),
    /// A code for adding a device: shown, or put away.
    DeviceCode(bool),
    /// The device chosen under You taken off (asked first).
    DeviceRemove,
    /// The device chosen under You let relay for the others, or not.
    DeviceRelay(bool),
    /// This device dials no one (true), or dials again.
    DeviceReceiveOnly(bool),
    DeviceBandwidthLimit(String),
    DeviceReceiveLimit(String),
    /// Where to try the device chosen under You.
    DeviceAddress(String),
    ContactAddress(String),
    /// Unblock the `n`th blocked key, as You lists them.
    Unblock(usize),
    /// Hand over a snapshot now (for `--snapshot`).
    Snapshot(oneshot::Sender<Snapshot>),
    /// The call's and the preview's newest pictures (for `--snapshot`, which has no event loop for them to be sent through).
    Frames(oneshot::Sender<snapshot::Frames>),
    /// The meters' levels now (for `--snapshot`, likewise).
    Levels(oneshot::Sender<snapshot::Levels>),
    Quit(oneshot::Sender<()>),
}

/// The main panel's views: what's selected in the list (`Chats`), or one
/// chosen in the top bar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Chats,
    Peers,
    Channels,
    Transfers,
    Settings,
    /// The manual (its pages are the window's own: see `crate::help`).
    Help,
}

impl View {
    /// Whether choosing someone in the list leaves the view as it is:
    /// Peers shows who's chosen, and Transfers their transfers. Anywhere
    /// else, their chat opens.
    pub fn stays_on_select(self) -> bool {
        matches!(self, View::Peers | View::Channels | View::Transfers)
    }
}

/// Something waiting on a yes or no.
pub enum Asking {
    Channel(ChannelId, Confirm),
    RemovePeer(String, bool),
    RemoveDevice([u8; 32]),
    Block(Box<swarm_app::sidebar::RequestRow>),
}

/// A code for adding a device, as shown: its line, until when it's good,
/// and how many devices you had when it was made (one more, and it's
/// been used).
pub struct DeviceCode {
    pub line: String,
    pub until: Instant,
    pub devices: usize,
}

/// What the system's file dialog is open for.
pub enum Browsing {
    /// A file to offer this peer.
    SendTo(String),
    /// The folder to save offered transfer `id` in.
    SaveFor(TransferId),
    /// A picture for your profile.
    Avatar,
    /// The sound a message makes.
    Sound,
    /// The sound a call rings with.
    Ringtone,
}

/// What the system's file dialog was opened for (see `State::choose`),
/// and what was chosen in it: an error where there's no such dialog.
pub struct Chosen {
    purpose: Browsing,
    start: PathBuf,
    path: anyhow::Result<Option<PathBuf>>,
}

/// Everything the app holds.
pub struct State {
    pub node: NodeHandle,
    pub jobs: Jobs,
    pub sidebar: Sidebar,
    pub calls: Calls,
    pub timelines: Timelines,
    pub prompts: Prompts,
    /// How fast the files under way are going, for the top line.
    pub progress: swarm_app::progress::Meter,
    pub prefs: Prefs,
    pub view: View,
    pub selected: Option<EntryId>,
    pub peer_tab: i32,
    /// See `Action::InSight` and `Action::ChatShown`.
    pub in_sight: bool,
    pub chat_shown: bool,
    /// A call under way, and its camera on, as last told (see
    /// `crate::call_underway`).
    pub call_told: crate::CallState,
    /// What was unread when last notified of (see `crate::notify`).
    pub unread: crate::notify::Unread,
    pub offer: Option<TransferStatus>,
    pub asking: Option<(Asking, String)>,
    /// The system's file dialog is open (see `State::choose`), and where
    /// what's chosen in it goes.
    pub dialog_open: bool,
    pub dialogs: mpsc::UnboundedSender<Chosen>,
    /// The window, for the dialog to be put in front of.
    pub ui: crate::Ui,
    /// The camera's preview is open to take your picture (see
    /// `Action::Avatar`), not to see how you look.
    pub avatar_taking: bool,
    /// The camera's preview is open to read a contact line's QR code
    /// (see `Action::Scan`).
    pub scanning: bool,
    /// What's being scanned is the code of the device chosen.
    scanning_device: bool,
    /// Someone being picked (`Pick`): for the call (1) or the channel
    /// open (2), and what's typed so far.
    pub picking: Option<(i32, String)>,
    /// The last contact line read, and how many have been: Add a peer
    /// fills its fields with it as the count goes up.
    pub scanned: (i32, Option<swarm_app::contact::Contact>),
    /// A code for adding a device, while it's shown under You.
    pub device_code: Option<DeviceCode>,
    pub status: Option<(Note, Instant)>,
    /// The list as last shown: which entry each row of the window's list
    /// is (headings are none).
    pub row_map: Vec<Option<usize>>,
    /// The transfers as last shown, each row's (see `transfer_shown`).
    pub transfer_ids: Vec<TransferId>,
    last_fetch: Instant,
    /// When the list and its counts were last read from disk (`refresh`).
    last_refreshed: Instant,
    /// When each person followed was last fetched, this session.
    followed_fetched: jobs::Followed,
    /// When the window was last used: for locking when left (Settings).
    last_used: Instant,
    /// Locked after being left, and why the last passphrase didn't open
    /// it. Nothing's shown meanwhile; the node runs on.
    pub locked: Option<String>,
    /// Rings while a call's ringing here (see `ring`).
    pub ringer: swarm_app::ringtone::Ringer,
}

/// What's showing before there's an identity to run with.
enum Before {
    /// No identity yet: making one, or bringing one back. Why the last
    /// try wasn't taken, if it wasn't.
    FirstRun(String),
    /// A new identity's words, waiting to be written down.
    Words(Vec<String>),
    /// Asking for the passphrase, and why the last one didn't open it.
    Unlock(String),
    /// Can't go on: why.
    Fatal(String),
}

impl Before {
    fn snapshot(&self, suggested: &str) -> Snapshot {
        let mut snap = match self {
            Before::FirstRun(why) => Snapshot::first_run_error(why.clone()),
            Before::Words(words) => Snapshot::recovery_words(words.clone()),
            Before::Unlock(why) => Snapshot::unlock(why.clone()),
            Before::Fatal(why) => Snapshot::fatal(why.clone()),
        };
        snap.suggested = suggested.to_string();
        snap
    }
}

/// How often the first run's camera's picture is looked at for another
/// device's code.
const SCAN_EVERY: Duration = Duration::from_millis(100);

/// The identity to run with: unlocked with its passphrase, or made (or
/// brought back from its words) the first time. `None` if the window's
/// closed first. While adding this device, the camera can read the
/// other's code: its pictures go to `frame`, mirrored or not.
async fn unlock_or_make(identity_path: &std::path::Path, actions: &mut mpsc::UnboundedReceiver<Action>, push: &impl Fn(Snapshot), frame: &impl Fn(Arc<swarm_core::video::Picture>, bool)) -> Option<Identity> {
    use swarm_app::launch::{self, UnlockFailed};
    let mut showing = if launch::has_identity(identity_path) { Before::Unlock(String::new()) } else { Before::FirstRun(String::new()) };
    let mut suggested = String::new();
    let mut made: Option<Identity> = None;
    let mut scanner: Option<swarm_app::camera::Camera> = None;
    let mut looked_at: Option<Arc<swarm_core::video::Picture>> = None;
    let close = |scanner: &mut Option<swarm_app::camera::Camera>| {
        let camera = scanner.take();
        tokio::task::block_in_place(|| drop(camera));
    };
    push(showing.snapshot(&suggested));
    loop {
        let path = identity_path.to_path_buf();
        let slow = |f: Box<dyn FnOnce() -> Result<Identity, String> + Send>| async move { tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| Err(e.to_string())) };
        let named = |identity: &Identity, name: &str| {
            let name = name.trim();
            if !name.is_empty() {
                let _ = swarm_core::displayname::set(&paths().displayname(), identity, name);
            }
        };
        let action = if scanner.is_some() {
            tokio::select! {
                action = actions.recv() => action?,
                () = tokio::time::sleep(SCAN_EVERY) => {
                    let Some(camera) = &scanner else { continue };
                    let Some(picture) = camera.latest().filter(|p| looked_at.as_ref().is_none_or(|l| !Arc::ptr_eq(l, p))) else { continue };
                    looked_at = Some(picture.clone());
                    frame(picture.clone(), !camera.info().faces_away);
                    let read = tokio::task::block_in_place(|| swarm_app::contact::qr_texts(&picture));
                    let Some(code) = read.into_iter().find(|t| swarm_core::node::joining::JoinCode::parse(t).is_ok()) else { continue };
                    close(&mut scanner);
                    showing = Before::FirstRun(String::new());
                    let mut snap = showing.snapshot(&suggested);
                    snap.first_run_code = Some(code.trim().to_string());
                    push(snap);
                    continue;
                }
            }
        } else {
            actions.recv().await?
        };
        if matches!(action, Action::FirstRun(..) | Action::RestoreIdentity(..) | Action::JoinUser(..)) {
            close(&mut scanner);
        }
        match action {
            Action::Scan(on) if matches!(showing, Before::FirstRun(_)) => {
                close(&mut scanner);
                if on {
                    match tokio::task::block_in_place(swarm_app::camera::Camera::open_for_reading) {
                        Ok(camera) => {
                            scanner = Some(camera);
                            looked_at = None;
                            showing = Before::FirstRun(String::new());
                        }
                        Err(e) => showing = Before::FirstRun(format!("couldn't open the camera: {e:#}")),
                    }
                }
            }
            Action::FirstRun(name, pass, again) if matches!(showing, Before::FirstRun(_)) => {
                if let Err(why) = launch::check_new_passphrase(&pass, &again) {
                    showing = Before::FirstRun(why);
                } else {
                    match slow(Box::new(move || launch::create_identity(&path, &pass).map_err(|e| format!("{e:#}")))).await {
                        Ok(identity) => {
                            named(&identity, &name);
                            showing = Before::Words(identity.recovery_words().iter().map(|w| w.to_string()).collect());
                            made = Some(identity);
                        }
                        Err(e) => showing = Before::Fatal(format!("couldn't make your identity: {e}")),
                    }
                }
            }
            Action::RestoreIdentity(words, name, pass, again) if matches!(showing, Before::FirstRun(_)) => {
                let checked = Identity::from_words(&words).map_err(|e| e.to_string()).and_then(|identity| launch::check_new_passphrase(&pass, &again).map(|()| identity));
                match checked {
                    Err(why) => showing = Before::FirstRun(why),
                    Ok(identity) => match slow(Box::new(move || launch::save_new_identity(&path, identity, &pass).map_err(|e| format!("{e:#}")))).await {
                        Ok(identity) => {
                            named(&identity, &name);
                            return Some(identity);
                        }
                        Err(e) => showing = Before::Fatal(format!("couldn't save your identity: {e}")),
                    },
                }
            }
            Action::JoinUser(code, name, pass, again) if matches!(showing, Before::FirstRun(_)) => {
                use swarm_core::node::joining;
                let checked = joining::JoinCode::parse(&code).map_err(|e| format!("{e:#}")).and_then(|code| launch::check_new_passphrase(&pass, &again).map(|()| code));
                match checked {
                    Err(why) => showing = Before::FirstRun(why),
                    Ok(code) => {
                        let mut asking = showing.snapshot(&suggested);
                        (asking.first_run_error, asking.first_run_busy) = (String::new(), true);
                        push(asking);
                        let name = if name.trim().is_empty() { swarm_app::devices::default_name() } else { name.trim().to_string() };
                        let device = Identity::generate();
                        match joining::join(&code, &device, &name).await {
                            Err(e) => showing = Before::FirstRun(format!("{e:#}")),
                            Ok(joined) => match slow(Box::new(move || launch::save_joined_identity(&path, joined, &device, &pass).map_err(|e| format!("{e:#}")))).await {
                                Ok(identity) => return Some(identity),
                                Err(e) => showing = Before::Fatal(format!("couldn't save your identity: {e}")),
                            },
                        }
                    }
                }
            }
            Action::WordsWritten => {
                if let Some(identity) = made.take() {
                    return Some(identity);
                }
            }
            Action::Unlock(pass) if matches!(showing, Before::Unlock(_)) => {
                let tried = tokio::task::spawn_blocking(move || launch::unlock(&path, &pass)).await;
                match tried {
                    Ok(Ok(identity)) => return Some(identity),
                    Ok(Err(UnlockFailed::WrongPassphrase)) => showing = Before::Unlock("that isn't the passphrase".into()),
                    Ok(Err(UnlockFailed::Other(e))) => showing = Before::Fatal(format!("{e:#} — it's been left as it is")),
                    Err(e) => showing = Before::Fatal(e.to_string()),
                }
            }
            Action::ResetPassphrase(words, pass, again) if matches!(showing, Before::Unlock(_)) => {
                if let Err(why) = launch::check_new_passphrase(&pass, &again) {
                    showing = Before::Unlock(why);
                } else {
                    match slow(Box::new(move || launch::reset_passphrase(&path, &words, &pass).map_err(|e| format!("{e:#}")))).await {
                        Ok(identity) => return Some(identity),
                        Err(why) => showing = Before::Unlock(why),
                    }
                }
            }
            Action::SuggestPassphrase => suggested = launch::suggest_passphrase(),
            Action::Snapshot(reply) => {
                let mut snap = showing.snapshot(&suggested);
                snap.first_run_scanning = scanner.is_some();
                let _ = reply.send(snap);
                continue;
            }
            Action::Quit(done) => {
                close(&mut scanner);
                let _ = done.send(());
                return None;
            }
            _ => continue,
        }
        let mut snap = showing.snapshot(&suggested);
        snap.first_run_scanning = scanner.is_some();
        push(snap);
    }
}

/// Nothing more to do but be closed (a snapshot's still answered).
async fn until_quit(actions: &mut mpsc::UnboundedReceiver<Action>, showing: impl Fn() -> Snapshot) {
    while let Some(action) = actions.recv().await {
        match action {
            Action::Quit(done) => {
                let _ = done.send(());
                return;
            }
            Action::Snapshot(reply) => {
                let _ = reply.send(showing());
            }
            _ => {}
        }
    }
}

/// How often the window's list (who's online, unread, transfers) is read
/// from disk again with nothing having said it changed.
const REFRESH_EVERY: Duration = Duration::from_secs(1);
/// How often the meters are brought up to date: smooth to the eye.
const METERS_EVERY: Duration = Duration::from_millis(50);

/// Starts the app: an identity first (unlocked, or made the first time),
/// then the node, then the loop.
pub async fn run(ui: crate::Ui, mut actions: mpsc::UnboundedReceiver<Action>, cover: Option<u64>, chat_shown: bool) {
    let push = |snap: Snapshot| {
        ui.show(move |app, window| snapshot::apply(app, window, snap));
    };
    let identity_path = swarm_app::launch::identity_path();
    let frame = |picture: Arc<swarm_core::video::Picture>, mirror: bool| {
        ui.show(move |app, window| snapshot::first_run_preview(app, window, &picture, mirror));
    };
    let Some(identity) = unlock_or_make(&identity_path, &mut actions, &push, &frame).await else { return };
    let log: swarm_core::node::Logger = match std::env::var_os("SWARM_LOG").and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()) {
        Some(file) => {
            let file = std::sync::Mutex::new(file);
            std::sync::Arc::new(move |line: String| {
                use std::io::Write;
                let _ = writeln!(file.locked(), "{line}");
            })
        }
        None => std::sync::Arc::new(|_| {}),
    };
    let (node, mut call_events) = match swarm_app::launch::start(identity, cover, log).await {
        Ok(started) => started,
        Err(e) => {
            let why = format!("{e:#}");
            push(Snapshot::fatal(why.clone()));
            return until_quit(&mut actions, || Snapshot::fatal(why.clone())).await;
        }
    };
    let (jobs, mut job_results) = Jobs::new();
    let (dialogs, mut chosen) = mpsc::unbounded_channel();
    let mut state = State {
        node: node.clone(),
        jobs,
        sidebar: { let mut sidebar = Sidebar::new(); sidebar.set_tree(true); sidebar },
        calls: Calls::default(),
        timelines: Timelines::default(),
        prompts: Prompts::default(),
        progress: Default::default(),
        prefs: Prefs::load(&paths().settings()),
        view: View::Chats,
        selected: Some("you".to_string()),
        peer_tab: 0,
        in_sight: crate::in_sight(),
        chat_shown,
        call_told: Default::default(),
        unread: Default::default(),
        offer: None,
        asking: None,
        dialog_open: false,
        dialogs,
        ui: ui.clone(),
        avatar_taking: false,
        scanning: false,
        scanning_device: false,
        picking: None,
        scanned: (0, None),
        device_code: None,
        status: None,
        row_map: Vec::new(),
        transfer_ids: Vec::new(),
        last_fetch: Instant::now(),
        last_refreshed: Instant::now(),
        followed_fetched: jobs::Followed::default(),
        last_used: Instant::now(),
        locked: None,
        ringer: swarm_app::ringtone::Ringer::default(),
    };
    state.refresh();

    let mut tick = tokio::time::interval(Duration::from_millis(300));
    let mut meter_tick = tokio::time::interval(METERS_EVERY);
    meter_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut shown = snapshot::PicturesShown::default();
    let mut channel_updates = node.channel_updates();
    let mut message_updates = node.message_updates();
    loop {
        let pictures = snapshot::picture_to_show(snapshot::picture_sources(&state, &shown));
        tokio::select! {
            _ = tick.tick() => {
                if node.wiped() {
                    push(Snapshot::wiped());
                    return until_quit(&mut actions, Snapshot::wiped).await;
                }
                state.check_device_code();
                let now = Instant::now();
                state.last_used = last_used(state.last_used, crate::last_keyed(), state.call_state().underway || state.calls.incoming().is_some(), now);
                if state.locked.is_none() && left_long_enough(state.prefs.lock_after, state.last_used, now) {
                    state.locked = Some(String::new());
                }
                if state.hidden() {
                    state.calls.tick(&node, &state.sidebar);
                    state.keep_listening();
                    state.ring();
                    continue;
                }
                if state.last_refreshed.elapsed() >= REFRESH_EVERY {
                    state.refresh();
                }
                if state.locked.is_none() {
                    let frames = snapshot::frames(&state, &mut shown);
                    ui.show(move |app, window| snapshot::apply_frames(app, window, frames));
                }
                state.read_code();
                let note = state.calls.tick(&node, &state.sidebar);
                state.note(note);
                state.keep_listening();
                let told = state.call_state();
                if told != state.call_told {
                    state.call_told = told;
                    crate::call_underway(told);
                }
                state.check_prompts();
                if state.view == View::Chats && state.last_fetch.elapsed() > Duration::from_secs(5) {
                    state.fetch_posts(true);
                }
            }
            Ok(()) = channel_updates.changed() => {
                state.refresh();
                state.check_prompts();
            }
            Ok(()) = message_updates.changed() => state.refresh(),
            Some(done) = job_results.recv() => state.finished(done),
            Some(chosen) = chosen.recv() => state.chosen(chosen),
            Some(event) = call_events.recv() => {
                let quiet = matches!(&event, CallEvent::Control { signal: swarm_core::call::CallSignal::Input(_), .. });
                state.on_call(event);
                if quiet {
                    continue;
                }
            }
            _ = meter_tick.tick(), if snapshot::levels_shown(&state) => {
                let levels = snapshot::levels(&state);
                ui.show(move |app, _| snapshot::apply_levels(app, levels));
                continue;
            }
            _ = pictures, if state.locked.is_none() => {
                let frames = snapshot::frames(&state, &mut shown);
                ui.show(move |app, window| snapshot::apply_frames(app, window, frames));
                continue;
            }
            action = actions.recv() => match action {
                Some(Action::Quit(done)) => {
                    let _ = state.calls.hang_up(&node);
                    swarm_app::discovery::goodbye();
                    node.shutdown().await;
                    let _ = done.send(());
                    return;
                }
                Some(Action::Snapshot(reply)) => {
                    let _ = reply.send(state.snapshot());
                    continue;
                }
                Some(Action::Frames(reply)) => {
                    let _ = reply.send(snapshot::frames(&state, &mut snapshot::PicturesShown::default()));
                    continue;
                }
                Some(Action::Levels(reply)) => {
                    let _ = reply.send(snapshot::levels(&state));
                    continue;
                }
                Some(Action::Unlock(pass)) if state.locked.is_some() => {
                    let opens = tokio::task::spawn_blocking(move || swarm_app::launch::is_passphrase(&swarm_app::launch::identity_path(), &pass)).await.unwrap_or(false);
                    state.locked = if opens { None } else { Some("that isn't the passphrase".to_string()) };
                    state.last_used = Instant::now();
                }
                Some(_) if state.locked.is_some() => {}
                Some(action @ (Action::ControlPointer(..) | Action::ControlButton(..) | Action::ControlWheel(..) | Action::ControlKey(..))) => {
                    state.last_used = Instant::now();
                    state.act(action);
                    continue;
                }
                Some(action) => {
                    state.last_used = Instant::now();
                    state.act(action);
                }
                None => return,
            },
        }
        state.ring();
        if !state.hidden() {
            push(state.snapshot());
        }
    }
}

impl State {
    /// The ringtone on while a call's ringing here, and off as soon as
    /// it isn't: after anything that might have changed it, and on each
    /// tick (a call rung out).
    fn ring(&mut self) {
        self.ringer.set(self.calls.incoming().is_some(), &paths().settings());
    }

    fn note(&mut self, note: Option<Note>) {
        if let Some(note) = note {
            self.status = Some((note, Instant::now()));
        }
    }

    fn say(&mut self, note: Note) {
        self.note(Some(note));
    }

    /// Swarm's out of sight on the phone (in the background, or its
    /// screen off), with no call under way or ringing: the window isn't
    /// drawn, nor its list read again each second. Messages and channel
    /// news still refresh it, for their notifications. Doing it all
    /// regardless kept a phone asleep at 15% of a core.
    fn hidden(&self) -> bool {
        !self.in_sight && !self.call_state().underway && self.calls.incoming().is_none()
    }

    /// The call, as the phone's app is told of it.
    fn call_state(&self) -> crate::CallState {
        let video = self.calls.active().and_then(|call| call.video.as_ref());
        crate::CallState {
            underway: self.calls.active().is_some() || self.calls.outgoing().is_some(),
            camera: video.is_some_and(|video| video.camera.is_some()),
            video: video.is_some(),
        }
    }

    /// The selected entry, if it's still in the list (shown, or in a
    /// section closed since).
    pub fn selected_entry(&self) -> Option<&Entry> {
        self.sidebar.entry(self.selected.as_ref()?)
    }

    /// The device chosen under You, as the node has it now.
    fn selected_device(&self) -> Option<swarm_core::node::devices::DeviceInfo> {
        let Some(Entry::Device(chosen)) = self.selected_entry() else { return None };
        self.node.devices().devices.into_iter().find(|d| d.key == chosen.key)
    }

    fn selected_peer_name(&self) -> Option<String> {
        match self.selected_entry() {
            Some(Entry::Peer(row)) => Some(row.handle.clone()),
            _ => None,
        }
    }

    /// Whom a file's sent to: in a call, someone in it (see
    /// `Active::file_goes_to`); else the peer selected.
    fn file_goes_to(&self) -> Option<String> {
        let Some(call) = self.calls.active() else { return self.selected_peer_name() };
        let open = match self.selected_entry() {
            Some(Entry::Peer(row)) => vec![row.public_key],
            _ => Vec::new(),
        };
        let key = call.file_goes_to(&open)?;
        self.sidebar.peer_by_key(&key).map(|row| row.handle.clone())
    }

    fn selected_channel(&self) -> Option<ChannelId> {
        match self.selected_entry() {
            Some(Entry::Channel(row)) => Some(row.id.clone()),
            _ => None,
        }
    }

    /// Rebuilds the list (keeping the same entry selected), counts what's
    /// on screen as read, and notifies of what's new (see `notify`).
    /// Settings' microphone meter listens while Settings is in sight
    /// (see `Calls::keep_listening`).
    /// The code for adding a device put away once it's run out, or
    /// been used.
    fn check_device_code(&mut self) {
        let Some(code) = &self.device_code else { return };
        if Instant::now() >= code.until {
            self.device_code = None;
            self.say(Note::info("the code for adding a device has run out — Add a device makes another"));
        } else if self.node.devices().devices.len() > code.devices {
            self.device_code = None;
        }
    }

    pub fn keep_listening(&mut self) {
        let wanted = self.view == View::Settings && self.in_sight && self.locked.is_none();
        self.calls.keep_listening(wanted);
    }

    pub fn refresh(&mut self) {
        self.last_refreshed = Instant::now();
        let on_screen = self.view == View::Chats && self.in_sight && self.chat_shown;
        let open = if on_screen { self.selected.clone() } else { None };
        let looked_at = crate::notifies() || (crate::window_in_use() && self.last_used.elapsed() < swarm_app::sidebar::LOOKED_AT_WITHIN);
        if let Some(news) = self.sidebar.refresh(&self.node, &paths().peers(), &self.prefs, open.as_ref(), looked_at) {
            self.say(Note::info(news));
        }
        if self.selected_entry().is_none() {
            self.selected = Some("you".to_string());
        }
        if let Some(name) = self.selected_peer_name() {
            if self.peer_tab == 1 {
                self.timelines.mark_seen(&name);
            } else {
                self.timelines.baseline(&name);
            }
        }
        self.notify();
    }

    /// Each chat with more unread notified of, as the setting has it,
    /// while Swarm's out of sight, and each one read taken away: on the
    /// phone. Elsewhere nothing shows them: a sound, while the window
    /// isn't the one in use (see `notify::sounds`).
    fn notify(&mut self) {
        let chats: Vec<(String, String, usize)> = self
            .sidebar
            .all
            .iter()
            .filter_map(|entry| match entry {
                Entry::Peer(row) => Some((entry.id(), row.label.clone(), row.unread)),
                Entry::Channel(row) => Some((entry.id(), row.name.clone(), row.noticed())),
                _ => None,
            })
            .collect();
        if !crate::notifies() {
            let in_use = crate::window_in_use() || !self.unread.has_looked();
            let notices = self.unread.notices(&chats, self.prefs.notify, in_use);
            if crate::notify::sounds(&notices, self.calls.active().is_some() || self.calls.incoming().is_some()) {
                swarm_app::sound::play_chosen(&paths().settings());
            }
            return;
        }
        for notice in self.unread.notices(&chats, self.prefs.notify, self.in_sight) {
            crate::notify(notice);
        }
    }

    fn selected_followed(&self) -> Option<swarm_app::sidebar::FollowRow> {
        match self.selected_entry() {
            Some(Entry::Followed(row)) => Some(row.clone()),
            _ => None,
        }
    }

    /// Fetches the selected followed person's posts: `now`, or when they
    /// haven't been fetched in the last minute (each fetch dials them).
    fn fetch_followed(&mut self, now: bool) {
        let Some(row) = self.selected_followed() else { return };
        let note = self.jobs.fetch_followed(&self.node, &mut self.followed_fetched, &row, now);
        self.note(note);
    }

    fn fetch_posts(&mut self, force: bool) {
        self.last_fetch = Instant::now();
        if self.selected_followed().is_some() {
            self.fetch_followed(false);
            return;
        }
        if let Some(name) = self.selected_peer_name() {
            self.timelines.fetch(&self.jobs, &self.node, &self.sidebar, &name, force);
        }
    }

    /// Puts a new file offer to the person, one at a time, and
    /// passes on transfer news.
    fn check_prompts(&mut self) {
        for note in self.prompts.transfer_news(&self.node) {
            self.say(note);
        }
        for note in self.prompts.link_news(&self.node) {
            self.say(note);
        }
        if let Some(prompts::OfferNow::Gone(note)) = self.offer.as_ref().map(|offer| prompts::offer_now(&self.node, offer)) {
            self.offer = None;
            if let Some(note) = note {
                self.say(note);
            }
        }
        if self.offer.is_none() && self.calls.incoming().is_none() && !self.dialog_open {
            if let Some((offer, note)) = self.prompts.next_offer(&self.node, &self.sidebar) {
                self.say(note);
                self.offer = Some(offer);
            }
        }
    }

    /// Starts a group of you and `contacts`, and opens it.
    fn start_group(&mut self, contacts: &[&str]) {
        let (group, note) = actions::new_group(&self.node, contacts);
        self.say(note);
        if let Some(group) = group {
            self.selected = Some(swarm_app::sidebar::channel_entry_id(&group));
            self.view = View::Chats;
        }
        self.refresh();
    }

    fn finished(&mut self, done: Done) {
        let joined = matches!(done, Done::Followed { .. });
        let note = jobs::finished(done, &mut self.timelines, &self.sidebar);
        self.note(note);
        if joined {
            self.refresh();
        }
    }

    fn on_call(&mut self, event: CallEvent) {
        let note = self.calls.on_event(&self.node, &self.sidebar, event);
        self.note(note);
    }

    pub fn snapshot(&mut self) -> Snapshot {
        if let Some(why) = &self.locked {
            let mut snap = Snapshot::unlock(why.clone());
            snap.view = Pane::Locked;
            snap.theme = self.prefs.theme.index();
            return snap;
        }
        if self.status.as_ref().is_some_and(|(note, at)| note.kind.lasts().is_some_and(|d| at.elapsed() > d)) {
            self.status = None;
        }
        snapshot::build(self)
    }

    fn act(&mut self, action: Action) {
        let node = self.node.clone();
        match action {
            Action::InSight(yes) => {
                self.in_sight = yes;
                self.refresh();
            }
            Action::ChatShown(yes) => {
                self.chat_shown = yes;
                self.refresh();
            }
            Action::Select(i) => {
                let Some(Some(index)) = self.row_map.get(i).copied() else { return };
                let Some(entry) = self.sidebar.rows.get(index) else { return };
                if let Entry::Heading(h) = entry {
                    let section = h.section;
                    self.toggle_section(section);
                    return;
                }
                if let Entry::Add(_) = entry {
                    return;
                }
                self.selected = Some(entry.id());
                self.peer_tab = 0;
                if !self.view.stays_on_select() {
                    self.view = View::Chats;
                }
                self.refresh();
                self.fetch_posts(false);
            }
            Action::Move(delta) => {
                let rows = &self.sidebar.rows;
                let selected = self.selected_entry();
                let at = selected
                    .and_then(|entry| rows.iter().position(|e| e.id() == entry.id()).or_else(|| self.sidebar.shown_heading(entry.section())))
                    .unwrap_or(0);
                let step = delta.signum() as i64;
                let (mut next, mut left, mut i) = (at, delta.unsigned_abs(), at as i64 + step);
                while left > 0 && i >= 0 && (i as usize) < rows.len() {
                    if !matches!(rows[i as usize], Entry::Heading(_) | Entry::Add(_)) {
                        next = i as usize;
                        left -= 1;
                    }
                    i += step;
                }
                if next != at {
                    self.selected = Some(rows[next].id());
                    self.peer_tab = 0;
                    self.refresh();
                    self.fetch_posts(false);
                }
            }
            Action::Escape => {
                self.calls.close_preview();
                self.scanning = false;
                self.view = View::Chats;
                self.refresh();
            }
            Action::NewPost => {
                self.selected = Some("you".to_string());
                self.view = View::Chats;
                self.refresh();
            }
            Action::TextSize(step) => {
                if let Err(e) = self.prefs.step_text_size(&paths().settings(), step) {
                    self.say(Note::error(format!("couldn't save the text size: {e}")));
                }
            }
            Action::ShowView(v) => {
                self.calls.close_preview();
                self.scanning = false;
                self.view = match v {
                    Pane::Peers => View::Peers,
                    Pane::Channels => View::Channels,
                    Pane::Transfers => View::Transfers,
                    Pane::Settings => View::Settings,
                    Pane::Help => View::Help,
                    _ => View::Chats,
                };
                self.keep_listening();
                self.refresh();
            }
            Action::NarrowList(narrow) => {
                if let Err(e) = self.prefs.set_list_narrow(&paths().settings(), narrow) {
                    self.say(Note::error(format!("couldn't save the list's width: {e}")));
                }
            }
            Action::ListWidth(width) => {
                if let Err(e) = self.prefs.set_list_width(&paths().settings(), width) {
                    self.say(Note::error(format!("couldn't save the list's width: {e}")));
                }
            }
            Action::HideMembers(hidden) => {
                if let Err(e) = self.prefs.set_members_hidden(&paths().settings(), hidden) {
                    self.say(Note::error(format!("couldn't save the members' width: {e}")));
                }
            }
            Action::MembersWidth(width) => {
                if let Err(e) = self.prefs.set_members_width(&paths().settings(), width) {
                    self.say(Note::error(format!("couldn't save the members' width: {e}")));
                }
            }
            Action::SetTab(tab) => {
                self.peer_tab = tab;
                self.refresh();
            }
            Action::Send(text) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    return;
                }
                match self.selected_entry().cloned() {
                    Some(Entry::Peer(row)) => self.jobs.send(&node, &row, text),
                    Some(Entry::Channel(row)) if text.starts_with('/') && swarm_app::chat::action(&text).is_none() => match commands::run(&node, &self.sidebar, &row.id, &text) {
                        commands::Outcome::Done(note) => self.say(note),
                        commands::Outcome::Ask { what, question } => self.asking = Some((Asking::Channel(row.id, what), question)),
                    },
                    Some(Entry::Channel(row)) => {
                        if let Err(e) = node.post_to_channel(&row.id, &text) {
                            self.say(Note::error(format!("{e:#}")));
                        }
                    }
                    _ => {}
                }
                self.refresh();
            }
            Action::DeleteLine(refs) => {
                let lines: Vec<swarm_core::history::LineRef> = refs.split(',').filter_map(swarm_core::history::LineRef::decode).collect();
                if let (Some(Entry::Peer(row)), false) = (self.selected_entry().cloned(), lines.is_empty()) {
                    let note = swarm_app::actions::delete_line(&node, &row, &lines);
                    self.note(note);
                }
                self.refresh();
            }
            Action::DeletePost(refs) => {
                let signature: Option<[u8; 64]> = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, refs.as_bytes()).ok().and_then(|b| b.try_into().ok());
                if let Some(signature) = signature {
                    let note = swarm_app::actions::delete_post(&node, &signature);
                    self.note(note);
                }
                self.refresh();
            }
            Action::Post(text, public) => {
                let visibility = if public { Visibility::Public } else { Visibility::Private };
                match posts::add(&paths().posts(), node.identity(), &text, visibility) {
                    Ok(()) => self.say(Note::success(format!("posted ({})", if public { "for anyone" } else { "for contacts" }))),
                    Err(e) => self.say(Note::error(format!("couldn't post: {e:#}"))),
                }
            }
            Action::Refresh => {
                if self.selected_followed().is_some() {
                    self.fetch_followed(true);
                    return;
                }
                self.fetch_posts(true);
                if let Some(id) = self.selected_channel() {
                    node.resync_channel(&id);
                    self.say(Note::info("catching up with everyone in it who's connected"));
                }
            }
            Action::Call(video) => {
                if let Some(device) = self.selected_device().filter(|_| self.calls.active().is_none()) {
                    let note = self.calls.place_device(&node, &device, video);
                    self.say(note);
                    return;
                }
                if let Some(Entry::Channel(row)) = self.selected_entry().cloned() {
                    let note = self.calls.call_group(&node, &self.sidebar, &row.name, &swarm_app::calls::group_contacts(&node, &self.sidebar, &row.id));
                    self.say(note);
                    return;
                }
                if let Some(Entry::Peer(row)) = self.selected_entry().cloned() {
                    if self.calls.active().is_some() {
                        if !video && self.calls.can_add(&row) {
                            let note = self.calls.add(&node, &self.sidebar, &row);
                            self.say(note);
                        }
                        return;
                    }
                    let note = self.calls.place(&node, &row, video);
                    self.say(note);
                }
            }
            Action::Pick(kind, typed) => {
                self.picking = (kind != 0).then_some((kind, typed));
            }
            Action::OpenChat(name) => {
                let Some(row) = self.sidebar.peer_by_name(&name) else { return };
                self.selected = Some(Entry::Peer(row.clone()).id());
                self.peer_tab = 0;
                self.view = View::Chats;
                self.refresh();
                self.fetch_posts(false);
            }
            Action::AddToCall(name) => {
                if let Some(row) = self.sidebar.peer_by_name(&node.resolve(&name)).cloned().filter(|row| self.calls.can_add(row)) {
                    let note = self.calls.add(&node, &self.sidebar, &row);
                    self.say(note);
                }
            }
            Action::HangUp => {
                let note = self.calls.hang_up(&node);
                self.note(note);
            }
            Action::RemoveFromCall(number) => {
                let numbered = self.calls.active().map(|c| c.numbered()).unwrap_or_default();
                for (_, _, key, _) in numbered.into_iter().filter(|m| m.3 && m.0 as i32 == number) {
                    let note = self.calls.remove(&node, key);
                    self.say(note);
                }
            }
            Action::ToggleCamera => {
                let note = self.calls.toggle_camera(&node, &self.sidebar);
                self.note(note);
            }
            Action::UseCamera(index) => {
                let note = self.calls.use_camera(&node, &self.sidebar, index.max(0) as usize);
                self.note(note);
            }
            Action::ToggleScreen => {
                if let Some(note) = self.calls.toggle_screen(&node, &self.sidebar) {
                    self.say(note);
                }
            }
            Action::ToggleControl => {
                if let Some(note) = self.calls.toggle_control() {
                    self.say(note);
                }
            }
            Action::AnswerControl(allow) => {
                if let Some(note) = self.calls.answer_control(&node, &self.sidebar, allow) {
                    self.say(note);
                }
            }
            Action::StopControl => {
                if let Some(note) = self.calls.stop_control() {
                    self.say(note);
                }
            }
            Action::ControlPointer(x, y) => {
                if let Some(control) = self.calls.control() {
                    control.pointer(x, y);
                }
            }
            Action::ControlButton(button, down) => {
                if let (Some(control), Ok(button)) = (self.calls.control(), u8::try_from(button)) {
                    control.button(button, down);
                }
            }
            Action::ControlWheel(dx, dy) => {
                if let Some(control) = self.calls.control() {
                    control.wheel((dx * 2.0).round() as i32, (dy * 2.0).round() as i32);
                }
            }
            Action::ControlKey(text, down) => {
                if let Some(control) = self.calls.control() {
                    for key in text.chars() {
                        control.key(key, down);
                    }
                }
            }
            Action::ToggleMute => self.calls.toggle_mute(),
            Action::Preview(open) => {
                self.avatar_taking = false;
                if open {
                    self.calls.open_preview();
                } else {
                    self.calls.close_preview();
                }
            }
            Action::Answer(video) => {
                let note = self.calls.answer(&node, &self.sidebar, video);
                self.note(note);
            }
            Action::Reject => {
                let note = self.calls.reject();
                self.note(note);
            }
            Action::Offer(choice) => {
                let Some(offer) = self.offer.take() else { return };
                match choice {
                    Answer::Yes => self.ask_where_to_save(offer.transfer.id),
                    Answer::No => self.say(actions::decline_transfer(&node, &offer.transfer)),
                    Answer::Later => self.say(actions::transfer_left(&offer.transfer)),
                }
            }
            Action::Confirm(yes) => {
                let Some((asking, _)) = self.asking.take() else { return };
                if !yes {
                    self.say(Note::info("cancelled"));
                    return;
                }
                match asking {
                    Asking::Channel(id, what) => {
                        let note = commands::confirm(&node, &id, what);
                        self.say(note);
                    }
                    Asking::RemovePeer(name, _) => self.say(actions::remove_peer(&node, &name, false)),
                    Asking::RemoveDevice(key) => match node.devices().devices.iter().find(|d| d.key == key) {
                        Some(device) => self.say(actions::remove_device(&node, device)),
                        None => self.say(Note::info("that device was taken off already")),
                    },
                    Asking::Block(row) => self.say(actions::block_request(&row)),
                }
                self.refresh();
            }
            Action::ConfirmBlock => {
                if let Some((Asking::RemovePeer(name, _), _)) = self.asking.take() {
                    self.say(actions::remove_peer(&node, &name, true));
                    self.refresh();
                }
            }
            Action::NextTimer => {
                if let Some(Entry::Peer(row)) = self.selected_entry().cloned() {
                    self.say(actions::next_timer(&node, &row));
                }
            }
            Action::SetTimer(at) => {
                if let (Some(Entry::Peer(row)), Some(seconds)) = (self.selected_entry().cloned(), swarm_core::disappearing::CHOICES.get(at)) {
                    if node.timer(&row.handle).seconds != *seconds {
                        self.say(actions::set_timer(&node, &row, *seconds));
                    }
                }
            }
            Action::SendFile => {
                if let Some(name) = self.file_goes_to() {
                    let start = swarm_core::settings::get(&paths().settings(), "send_from").map(PathBuf::from).unwrap_or_else(swarm_app::paths::home);
                    self.choose(start, Browsing::SendTo(name));
                }
            }
            Action::AddPeer(name, address, key, cover) => {
                let (name, address, key) = match swarm_app::contact::parse(&key).or_else(|| swarm_app::contact::parse(&address)) {
                    Some(c) => (if name.trim().is_empty() { c.name.clone() } else { name }, c.address_text(), c.key_text()),
                    None => (name, address, key),
                };
                let result = peerbook::add(&paths().peers(), name.trim(), address.trim(), key.trim(), Some(&node));
                self.saved_peer(result, key.trim(), &cover);
            }
            Action::UpdatePeer(name, address, key, cover) => {
                let Some(target) = self.selected_peer_name() else { return };
                let result = peerbook::update(&paths().peers(), &target, name.trim(), address.trim(), key.trim(), Some(&node));
                self.saved_peer(result, key.trim(), &cover);
            }
            Action::Follow(address, key) => {
                let (note, key) = actions::follow(&node, &key, &address);
                if let Some(key) = key {
                    self.followed_fetched.fetched(key);
                }
                self.say(note);
                self.refresh();
            }
            Action::SetReceive(key, folder) => {
                if let (Ok(key), Some(Entry::Peer(row))) = (peerbook::decode_key(&key), self.selected_entry().cloned()) {
                    let current = swarm_core::settings::receive_to(&row.folder.settings(), &key).map(|f| f.display().to_string()).unwrap_or_default();
                    if row.public_key == key && folder.trim() != current {
                        self.say(actions::set_receive(&row, &folder));
                    }
                }
            }
            Action::Unfollow => {
                if let Some(row) = self.selected_followed() {
                    self.say(actions::unfollow(&node, &row));
                    self.refresh();
                }
            }
            Action::RemovePeer => {
                if let Some(Entry::Peer(row)) = self.selected_entry().cloned().filter(|e| matches!(e, Entry::Peer(r) if r.removed_us)) {
                    self.say(actions::remove_peer(&node, &row.handle, false));
                    self.refresh();
                } else if let Some(name) = self.selected_peer_name() {
                    let asked = matches!(self.selected_entry(), Some(Entry::Peer(row)) if row.asked);
                    let shown = self.sidebar.peer_by_name(&name).map_or_else(|| name.clone(), |row| row.label.clone());
                    let question = capitalised(&actions::remove_peer_question(&shown, asked));
                    self.asking = Some((Asking::RemovePeer(name, asked), question));
                }
            }
            Action::CycleAnswer => match self.selected_entry().cloned() {
                Some(Entry::Peer(row)) => {
                    self.say(actions::cycle_auto_answer(&row));
                    self.refresh();
                }
                Some(Entry::Device(device)) if !device.this => {
                    self.say(actions::device_answer(&node, &device, None));
                    self.refresh();
                }
                _ => {}
            },
            Action::SetAnswer(at) => {
                let answer = match at {
                    1 => swarm_core::settings::AutoAnswer::Voice,
                    2 => swarm_core::settings::AutoAnswer::VoiceAndVideo,
                    _ => swarm_core::settings::AutoAnswer::Off,
                };
                match self.selected_entry().cloned() {
                    Some(Entry::Peer(row)) => self.say(actions::set_auto_answer(&row, answer)),
                    Some(Entry::Device(device)) if !device.this => self.say(actions::device_answer(&node, &device, Some(answer))),
                    _ => return,
                }
                self.refresh();
            }
            Action::SetControl(on) => {
                match self.selected_entry().cloned() {
                    Some(Entry::Peer(row)) => self.say(actions::set_auto_control(&row, Some(on))),
                    Some(Entry::Device(device)) if !device.this => self.say(actions::device_control(&node, &device, Some(on))),
                    _ => return,
                }
                self.refresh();
            }
            Action::Request(choice) => {
                let Some(Entry::Request(row)) = self.selected_entry().cloned() else { return };
                match choice {
                    RequestAnswer::Dismiss => {
                        self.say(actions::dismiss_request(&row));
                        self.refresh();
                    }
                    RequestAnswer::Block => {
                        let question = capitalised(&actions::block_question(&row));
                        self.asking = Some((Asking::Block(Box::new(row)), question));
                    }
                }
            }
            Action::AddRequest(name) => {
                let Some(Entry::Request(row)) = self.selected_entry().cloned() else { return };
                let typed = if name.trim().is_empty() { row.suggested_peer_name() } else { name.trim().to_string() };
                let name = row.peer_name(&typed);
                let key = base64_key(&row.public_key);
                let result = peerbook::add(&paths().peers(), &name, &row.address, &key, Some(&node));
                self.saved_peer(result, &key, "");
            }
            Action::NewGroup => self.start_group(&[]),
            Action::GroupWith(other) => {
                let Some(Entry::Peer(row)) = self.selected_entry().cloned() else { return };
                self.start_group(&[row.handle.as_str(), &other]);
            }
            Action::RenameGroup(name) => {
                let Some(Entry::Channel(row)) = self.selected_entry().cloned() else { return };
                self.say(actions::rename_group(&node, &row.id, &name));
                self.refresh();
            }
            Action::Transfer(i, what) => {
                let Some(t) = transfer_shown(&self.transfer_ids, node.transfers(), i) else { return };
                match what {
                    TransferAnswer::Accept => self.ask_where_to_save(t.transfer.id),
                    TransferAnswer::Decline => self.say(actions::decline_transfer(&node, &t.transfer)),
                    TransferAnswer::Cancel => self.say(actions::cancel_transfer(&node, &t.transfer)),
                    TransferAnswer::Clear => self.say(actions::clear_transfers(&node, &[t.transfer])),
                }
                self.refresh();
            }
            Action::ClearTransfers => {
                let shown: Vec<_> = node.transfers().into_iter().filter(|t| self.transfer_ids.contains(&t.transfer.id)).map(|t| t.transfer).collect();
                self.say(actions::clear_transfers(&node, &shown));
                self.refresh();
            }
            Action::SetName(name) => {
                let note = actions::set_name(&node, &name);
                self.note(note);
            }
            Action::SetManualAddress(text) => {
                self.say(actions::set_manual_address(&node, &text));
                self.refresh();
            }
            Action::SetDiscoverable(on) => {
                self.say(actions::set_discoverable(&node, on));
                self.refresh();
            }
            Action::Browse(on) => {
                swarm_app::discovery::browse(on);
                self.refresh();
            }
            Action::SetSays(says) => {
                self.say(actions::set_status(&node, &says));
                self.refresh();
            }
            Action::Avatar(what) => self.avatar(what),
            Action::Scan(on) => self.scan(on),
            Action::ScanDevice => {
                self.scan(true);
                self.scanning_device = self.scanning;
            }
            Action::Sound(what) => {
                let settings = paths().settings();
                match what {
                    SoundStep::Choose => {
                        let start = swarm_core::settings::get(&settings, swarm_app::sound::SETTING)
                            .and_then(|p| PathBuf::from(p).parent().map(std::path::Path::to_path_buf))
                            .unwrap_or_else(swarm_app::paths::home);
                        self.choose(start, Browsing::Sound);
                    }
                    SoundStep::Play => swarm_app::sound::play_chosen(&settings),
                    SoundStep::BuiltIn => match swarm_core::settings::set(&settings, swarm_app::sound::SETTING, "") {
                        Ok(()) => {
                            self.say(Note::success("messages make Swarm's own sound"));
                            swarm_app::sound::play(swarm_app::sound::built_in());
                        }
                        Err(e) => self.say(Note::error(format!("couldn't save that: {e}"))),
                    },
                }
                self.refresh();
            }
            Action::Ringtone(what) => {
                let settings = paths().settings();
                match what {
                    SoundStep::Choose => {
                        let start = swarm_core::settings::get(&settings, swarm_app::ringtone::SETTING)
                            .and_then(|p| PathBuf::from(p).parent().map(std::path::Path::to_path_buf))
                            .unwrap_or_else(swarm_app::paths::home);
                        self.choose(start, Browsing::Ringtone);
                    }
                    SoundStep::Play => swarm_app::ringtone::play_once(&settings),
                    SoundStep::BuiltIn => match swarm_app::ringtone::choose_built_in(&settings) {
                        Ok(()) => {
                            self.say(Note::success("calls ring with Swarm's own ringtone"));
                            swarm_app::ringtone::play_once(&settings);
                        }
                        Err(e) => self.say(Note::error(format!("couldn't save that: {e:#}"))),
                    },
                }
                self.refresh();
            }
            Action::SetSetting(which, value) => {
                let path = paths().settings();
                let yes = |on: bool| if on { "yes" } else { "no" };
                let at = |list_len: usize| value.clamp(0, list_len as i32 - 1) as usize;
                let saved = match which {
                    Setting::ShowTimes => swarm_core::settings::set(&path, "show_times", yes(value != 0)),
                    Setting::Utc => swarm_core::settings::set(&path, "times", if value != 0 { "utc" } else { "local" }),
                    Setting::ShowJoins => swarm_core::settings::set(&path, "show_joins", yes(value != 0)),
                    Setting::PeerOrder => swarm_core::settings::set(&path, "peer_order", ["added", "name", "recent"][at(3)]),
                    Setting::Theme => self.prefs.set_theme(&path, swarm_app::prefs::Theme::from_index(value)),
                    Setting::TextSize => swarm_core::settings::set(&path, "text_size", &swarm_app::prefs::TEXT_SIZES[at(swarm_app::prefs::TEXT_SIZES.len())].to_string()),
                    Setting::CancelEcho => self.prefs.set_voice(&path, swarm_app::prefs::Setting::Echo, value != 0),
                    Setting::SuppressNoise => self.prefs.set_voice(&path, swarm_app::prefs::Setting::Noise, value != 0),
                    Setting::SteadyLevel => self.prefs.set_voice(&path, swarm_app::prefs::Setting::Level, value != 0),
                    Setting::LockAfter => self.prefs.set_lock_after(&path, swarm_app::prefs::LOCK_AFTER[at(swarm_app::prefs::LOCK_AFTER.len())]),
                    Setting::Microphone => swarm_app::audio_devices::choose_at(&path, swarm_app::audio_devices::Way::Microphone, value.max(0) as usize),
                    Setting::Speaker => swarm_app::audio_devices::choose_at(&path, swarm_app::audio_devices::Way::Speaker, value.max(0) as usize),
                    Setting::Camera => swarm_app::camera::choose_at(&path, value.max(0) as usize),
                    Setting::VideoSize => self.prefs.set_video_width(&path, swarm_app::prefs::VIDEO_WIDTHS[at(swarm_app::prefs::VIDEO_WIDTHS.len())]),
                    Setting::CallDetails => self.prefs.set_call_info(&path, value != 0),
                    Setting::Notify => self.prefs.set_notify(&path, swarm_app::prefs::Notify::from_index(value)),
                };
                if let Err(e) = saved {
                    self.say(Note::error(format!("couldn't save that setting: {e}")));
                }
                self.prefs = Prefs::load(&path);
                self.refresh();
            }
            Action::DeviceRename(name) => self.say(actions::rename_device(&node, &name)),
            Action::DeviceCode(false) => self.device_code = None,
            Action::DeviceCode(true) => match actions::device_code(&node) {
                Ok(line) => {
                    self.device_code = Some(DeviceCode { line, until: Instant::now() + swarm_core::node::joining::CODE_LIFETIME, devices: node.devices().devices.len() });
                    self.selected = Some(Entry::You.id());
                    self.view = View::Peers;
                }
                Err(note) => self.say(note),
            },
            Action::DeviceRemove => {
                if let Some(device) = self.selected_device().filter(|d| !d.this) {
                    let question = capitalised(&actions::remove_device_question(&device));
                    self.asking = Some((Asking::RemoveDevice(device.key), question));
                }
            }
            Action::DeviceRelay(yes) => {
                if let Some(device) = self.selected_device() {
                    self.say(actions::allow_relay(&node, &device, yes));
                }
            }
            Action::DeviceReceiveOnly(yes) => {
                self.say(actions::receive_only(&node, yes));
                self.refresh();
            }
            Action::DeviceBandwidthLimit(text) => {
                self.say(actions::bandwidth_limit(&node, &text));
                self.refresh();
            }
            Action::DeviceReceiveLimit(text) => {
                self.say(actions::receive_limit(&node, &text));
                self.refresh();
            }
            Action::DeviceAddress(text) => {
                if let Some(device) = self.selected_device() {
                    self.say(actions::set_device_address(&node, &device, &text));
                    self.refresh();
                }
            }
            Action::ContactAddress(text) => {
                if let Some(Entry::Peer(row)) = self.selected_entry().cloned() {
                    self.say(actions::set_contact_address(&node, &row, &text));
                    self.refresh();
                }
            }
            Action::Unblock(i) => {
                if let Some(row) = actions::blocked(&node).get(i) {
                    self.say(actions::unblock(row));
                }
            }
            Action::FirstRun(..) | Action::RestoreIdentity(..) | Action::JoinUser(..) | Action::WordsWritten | Action::Unlock(_) | Action::ResetPassphrase(..) | Action::SuggestPassphrase | Action::Snapshot(_) | Action::Frames(_) | Action::Levels(_) | Action::Quit(_) => {}
        }
    }

    /// After adding or editing a peer: their cover rate too, if one was
    /// given ("64k", "64k require", "off").
    /// Opens or closes a section of the list, and remembers it.
    fn toggle_section(&mut self, section: swarm_app::sidebar::Section) {
        if let Err(e) = self.sidebar.toggle(section, &mut self.prefs, &paths().settings()) {
            self.say(Note::error(format!("couldn't save the list's headings: {e}")));
        }
        self.refresh();
    }

    fn saved_peer(&mut self, result: anyhow::Result<String>, name: &str, cover: &str) {
        let note = actions::saved_peer(&self.node, result, name, cover);
        self.say(note);
        self.refresh();
    }

    fn ask_where_to_save(&mut self, id: TransferId) {
        self.choose(swarm_app::paths::save_folder(), Browsing::SaveFor(id));
    }

    /// Asks where (a file to send, a folder to save in): in the system's
    /// own file dialog, on a thread of its own as it waits for the person.
    fn choose(&mut self, start: PathBuf, purpose: Browsing) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let tx = self.dialogs.clone();
        let ui = self.ui.clone();
        let node = self.node.clone();
        std::thread::spawn(move || {
            let parent = crate::dialog_parent(&ui);
            let path = match &purpose {
                Browsing::SendTo(peer) => swarm_app::filedialog::choose_file(&format!("Send a file to {}", node.label_of(peer)), &start, parent),
                Browsing::SaveFor(_) => swarm_app::filedialog::choose_folder("Save in which folder?", &start, parent),
                Browsing::Avatar => swarm_app::filedialog::choose_file("Choose your picture", &start, parent),
                Browsing::Sound => swarm_app::filedialog::choose_file("Choose the sound a message makes", &start, parent),
                Browsing::Ringtone => swarm_app::filedialog::choose_file("Choose the sound a call rings with", &start, parent),
            };
            let _ = tx.send(Chosen { purpose, start, path });
        });
    }

    /// What was chosen in the system's file dialog.
    fn chosen(&mut self, chosen: Chosen) {
        self.dialog_open = false;
        match (chosen.path, chosen.purpose) {
            (Err(why), Browsing::SendTo(_)) => self.say(Note::error(format!("no file dialog: {why:#} — `swarm send-file` sends one from a terminal"))),
            (Err(why), Browsing::SaveFor(id)) => {
                self.say(Note::warning(format!("no file dialog ({why:#}): saving in {}", chosen.start.display())));
                self.save_in(id, chosen.start);
            }
            (Ok(Some(path)), Browsing::SendTo(peer)) => self.offer_file(peer, path),
            (Ok(Some(dir)), Browsing::SaveFor(id)) => self.save_in(id, dir),
            (Ok(None), Browsing::SaveFor(_)) => self.say(Note::info("not accepted yet — it's in Transfers")),
            (Ok(None), Browsing::SendTo(_) | Browsing::Avatar | Browsing::Sound | Browsing::Ringtone) => {}
            (Err(why), Browsing::Sound | Browsing::Ringtone) => self.say(Note::error(format!("no file dialog: {why:#}"))),
            (Ok(Some(path)), Browsing::Sound) => match tokio::task::block_in_place(|| swarm_app::sound::from_file(&path)) {
                Ok(sound) => {
                    let _ = swarm_core::settings::set(&paths().settings(), swarm_app::sound::SETTING, &path.to_string_lossy());
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.say(Note::success(format!("messages make the sound of {name}")));
                    swarm_app::sound::play(sound);
                }
                Err(e) => self.say(Note::error(format!("couldn't use that sound: {e:#}"))),
            },
            (Ok(Some(path)), Browsing::Ringtone) => match tokio::task::block_in_place(|| swarm_app::ringtone::choose(&paths().settings(), &path)) {
                Ok((_, name)) => {
                    self.say(Note::success(format!("calls ring with {name}")));
                    swarm_app::ringtone::play_once(&paths().settings());
                }
                Err(e) => self.say(Note::error(format!("couldn't use that sound: {e:#}"))),
            },
            (Err(why), Browsing::Avatar) => self.say(Note::error(format!("no file dialog: {why:#}"))),
            (Ok(Some(path)), Browsing::Avatar) => {
                let made = picture_from(&path);
                self.set_avatar(made);
            }
        }
        self.refresh();
    }

    /// Your picture: from a file, the camera opened for it, taken, not
    /// after all, or removed.
    fn avatar(&mut self, what: AvatarStep) {
        match what {
            AvatarStep::FromFile => {
                let start = swarm_core::settings::get(&paths().settings(), "avatar_from").map(PathBuf::from).unwrap_or_else(swarm_app::paths::home);
                self.choose(start, Browsing::Avatar);
            }
            AvatarStep::OpenCamera => {
                if self.calls.active().is_some_and(|c| c.video.is_some()) {
                    self.say(Note::error("the camera's in the call — take your picture after it"));
                    return;
                }
                self.scanning = false;
                self.calls.open_preview();
                self.avatar_taking = true;
            }
            AvatarStep::Take => {
                let picture = self.calls.preview().and_then(|p| p.as_ref().ok()).and_then(|c| c.latest());
                let Some(picture) = picture else {
                    self.say(Note::error("the camera hasn't shown a picture yet"));
                    return;
                };
                let made = tokio::task::block_in_place(|| swarm_app::avatar::from_picture(&picture));
                self.avatar_taking = false;
                self.calls.close_preview();
                self.set_avatar(made);
            }
            AvatarStep::Cancel => {
                self.avatar_taking = false;
                self.calls.close_preview();
            }
            AvatarStep::Remove => self.set_avatar(Ok(Vec::new())),
        }
        self.refresh();
    }

    /// Add a peer's camera, to read a contact line's QR code: opened, or
    /// closed (see `read_code`).
    fn scan(&mut self, on: bool) {
        if on && self.calls.active().is_some_and(|c| c.video.is_some()) {
            self.say(Note::error("the camera's in the call — scan the code after it"));
            return;
        }
        self.avatar_taking = false;
        self.scanning = on;
        self.scanning_device = false;
        if on {
            self.calls.open_reader();
        } else {
            self.calls.close_preview();
        }
        self.refresh();
    }

    /// While scanning, the camera's newest picture searched for a contact
    /// line's QR code; one found fills Add a peer, and the camera's let go.
    fn read_code(&mut self) {
        if !self.scanning {
            return;
        }
        let picture = self.calls.preview().and_then(|p| p.as_ref().ok()).and_then(|c| c.latest());
        let Some(picture) = picture else { return };
        let Some(contact) = tokio::task::block_in_place(|| swarm_app::contact::read(&picture)) else { return };
        if self.scanning_device {
            if let Some(device) = self.selected_device() {
                self.say(swarm_app::actions::add_device_code(&self.node, &device, &contact));
            }
            self.scanning = false;
            self.scanning_device = false;
            self.calls.close_preview();
            self.refresh();
            return;
        }
        let who = if contact.name.is_empty() { "a contact".to_string() } else { contact.name.clone() };
        self.say(Note::success(format!("read {who}'s code — check the details, then Save")));
        self.scanned = (self.scanned.0 + 1, Some(contact));
        self.scanning = false;
        self.calls.close_preview();
        self.refresh();
    }

    /// Publishes `made` as your picture (empty: none), keeping your status.
    fn set_avatar(&mut self, made: anyhow::Result<Vec<u8>>) {
        let says = swarm_core::profile::current(&paths().profile()).map(|p| p.status).unwrap_or_default();
        let set = made.and_then(|jpeg| self.node.set_profile(&says, &jpeg).map(|()| jpeg.is_empty()).map_err(Into::into));
        swarm_app::avatar::changed_own();
        match set {
            Ok(true) => self.say(Note::success("your picture is removed")),
            Ok(false) => self.say(Note::success("your picture is set — your contacts see it")),
            Err(e) => self.say(Note::error(format!("couldn't set your picture: {e:#}"))),
        }
    }

    /// Offers `path` to `peer`, and starts there next time.
    fn offer_file(&mut self, peer: String, path: PathBuf) {
        if let Some(dir) = path.parent() {
            let _ = swarm_core::settings::set(&paths().settings(), "send_from", &dir.to_string_lossy());
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.say(Note::info(format!("offering {name} to {}...", self.node.label_of(&peer))));
        self.jobs.offer(&self.node, peer, path);
    }

    /// Accepts transfer `id`, saving it in `dir` (and offering `dir` first
    /// next time).
    fn save_in(&mut self, id: TransferId, dir: PathBuf) {
        match self.node.accept_transfer(&id, &dir) {
            Ok(()) => {
                let _ = swarm_core::settings::set(&paths().settings(), "downloads", &dir.to_string_lossy());
                self.say(Note::success(format!("accepted — saving in {}", dir.display())));
            }
            Err(e) => self.say(Note::error(format!("couldn't accept: {e:#}"))),
        }
    }
}

/// When the window was last used, for locking when left: the last thing
/// done in it (`acted`), the last key pressed in it (`keyed`, typing in a
/// box included), or now while there's a call (`in_call`: placed, ringing
/// either way, or under way) — a call isn't being left, though nothing's
/// pressed in it, and locking would hide it, Hang up and all, with the
/// microphone still on.
fn last_used(acted: Instant, keyed: Option<Instant>, in_call: bool, now: Instant) -> Instant {
    if in_call {
        return now;
    }
    keyed.map_or(acted, |keyed| keyed.max(acted))
}

/// Whether the window's been left `lock_after` minutes (0: never locks)
/// since it was last used.
fn left_long_enough(lock_after: u32, last_used: Instant, now: Instant) -> bool {
    lock_after > 0 && now.saturating_duration_since(last_used) > Duration::from_secs(60 * u64::from(lock_after))
}

/// The transfer row `i` showed (of those `shown`), as it is now: not
/// whichever's in its place, the list having moved since.
fn transfer_shown(shown: &[TransferId], now: Vec<TransferStatus>, i: usize) -> Option<TransferStatus> {
    let id = shown.get(i)?;
    now.into_iter().find(|t| t.transfer.id == *id)
}

fn base64_key(key: &[u8; 32]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.encode(key)
}

/// The picture in the file at `path` chosen to be yours, made into one (see `avatar::from_file`).
fn picture_from(path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    let made = tokio::task::block_in_place(|| swarm_app::avatar::from_file(path));
    let lent = swarm_app::paths::data_dir().join("sending");
    if let Some(copy) = path.parent().filter(|dir| dir.parent() == Some(lent.as_path())) {
        let _ = std::fs::remove_dir_all(copy);
    } else if let Some(dir) = path.parent() {
        let _ = swarm_core::settings::set(&paths().settings(), "avatar_from", &dir.to_string_lossy());
    }
    made
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::transfer::{Direction, State as TransferState, Transfer};

    /// Someone chosen in the list from Transfers shows their transfers;
    /// Settings still opens the chat.
    #[test]
    fn choosing_someone_keeps_peers_and_transfers() {
        assert!(View::Transfers.stays_on_select());
        assert!(View::Peers.stays_on_select());
        assert!(View::Channels.stays_on_select());
        assert!(!View::Settings.stays_on_select());
        assert!(!View::Chats.stays_on_select());
    }

    fn status(id: u8) -> TransferStatus {
        let transfer = Transfer {
            id: [id; 16],
            direction: Direction::In,
            peer: [2; 32],
            state: TransferState::Offered,
            size: 5,
            sha256: [0; 32],
            created: 0,
            name: format!("{id}.txt"),
            path: PathBuf::new(),
            note: String::new(),
            cleared: false,
            settled: false,
        };
        TransferStatus { transfer, peer_name: None, peer_key: None, done_bytes: 0 }
    }

    /// A transfer's buttons act on the one they were drawn beside, though
    /// another's come since (newest first, it moved the rest down one):
    /// not the one in its place now.
    #[test]
    fn a_transfer_is_acted_on_as_shown_not_where_it_is_now() {
        let shown = [[1; 16], [2; 16]];
        let now = vec![status(3), status(1), status(2)];
        assert_eq!(transfer_shown(&shown, now.clone(), 0).map(|t| t.transfer.id), Some([1; 16]));
        assert_eq!(transfer_shown(&shown, now.clone(), 1).map(|t| t.transfer.id), Some([2; 16]));
        assert!(transfer_shown(&shown, now, 2).is_none(), "not drawn yet");
        assert!(transfer_shown(&shown, vec![status(2)], 0).is_none(), "gone since");
    }

    /// With locking when left on, a quiet call never locks the window
    /// (nothing's pressed in a call, and locking would hide Hang up with
    /// the microphone on). A call is never being left; nor is a window
    /// being typed in.
    #[test]
    fn a_call_or_typing_is_never_leaving_the_window() {
        let start = Instant::now();
        let minutes = |n: u64| Duration::from_secs(60 * n);
        let later = start + minutes(6);
        let used = last_used(start, None, false, later);
        assert!(left_long_enough(5, used, later));
        assert!(!left_long_enough(15, used, later));
        assert!(!left_long_enough(0, used, later));
        let mut used = start;
        for minute in 1..=60 {
            used = last_used(used, None, true, start + minutes(minute));
            assert!(!left_long_enough(5, used, start + minutes(minute)), "minute {minute} of a call");
        }
        let ended = start + minutes(60);
        assert!(!left_long_enough(5, used, ended + minutes(4)));
        assert!(left_long_enough(5, used, ended + minutes(6)));
        let used = last_used(start, Some(later - minutes(2)), false, later);
        assert!(!left_long_enough(5, used, later));
        assert_eq!(last_used(later, Some(start), false, later), later);
    }
}
