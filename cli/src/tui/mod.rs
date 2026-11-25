//! The full-screen terminal UI: tabs along the top (Contacts, Chats,
//! Transfers, Settings, Help; keys 1 to 5), the list of contacts and
//! channels on the left, and whatever's chosen beside it. It shows what
//! the window shows, from the same app layer (`swarm_app`); how it looks
//! is in `look`, the text box is `lineedit`.
//!
//! This module holds the state (`App`) and the loop that draws it and
//! takes keys; `keys` is what each key does, `render` the drawing. The
//! state is read again every `TICK`, and the chosen contact's posts every
//! `TIMELINE_REFRESH` while they're shown. Messages, posts and files go
//! in the background (`swarm_app::jobs`), so the screen doesn't wait on
//! them.

mod keys;
mod render;

use keys::*;
use render::*;
use swarm_core::lock::Locked;
use crate::{lineedit, look};
use swarm_app::addrs::{global_addresses, local_addresses, seen_from_outside};
use swarm_app::calls::{CallVideo, Calls, Incoming};
use swarm_app::chat::{self, ChatItem};
use swarm_app::commands::{self, Confirm};
use swarm_app::jobs::{self, Done, Jobs, Timelines};
use swarm_app::notes::{Kind as StatusKind, Note};
use swarm_app::prompts::{self, is_open_offer, Prompts};
use swarm_app::names::{my_name, short_key};
use swarm_app::sidebar::{Adding, Entry as SidebarEntry, FollowRow, HeadingRow, PeerRow, RequestRow, Sidebar};
use swarm_app::actions;
use swarm_app::peerbook::cover_setting_text;
use swarm_app::{expand_home, paths, peerbook, prefs, time};
use swarm_core::node::{self, CallEvent, NodeHandle, PostsResult};
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use crossterm::event::{Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use swarm_core::channel::ChannelId;
use swarm_core::identity::Identity;
use swarm_core::posts::{self, Visibility};
use tokio::sync::mpsc;

/// A background failure from the `listen_forever` task this TUI is
/// running alongside — most realistically a bind failure (port 1977
/// already in use by another process on this host). Surfaced in the
/// status bar rather than silently swallowed; local data (history,
/// posts, cached timelines) is all still readable even if this is set.
pub type BackendError = Arc<Mutex<Option<String>>>;

const TICK: Duration = Duration::from_millis(300);
/// How often the camera preview is redrawn: about 15 frames a second.
const PREVIEW_FRAME: Duration = Duration::from_millis(66);
/// The meters' redraw, while there are any.
const METER_FRAME: Duration = Duration::from_millis(100);
/// How long a ringing call's border is lit, then unlit.
const RING_FLASH: Duration = Duration::from_millis(500);
const SIDEBAR_WIDTH: u16 = 30;
/// How often the currently-selected peer's timeline re-fetches while
/// they're the one on screen. Everything else here is either local
/// (messages, unread counts — a cheap file re-read every `TICK`) or
/// explicitly pulled on request; a peer's posts are the one thing with
/// no local signal that something changed, so "this contact is selected"
/// has to mean "keep asking," not just "asked once."
const TIMELINE_REFRESH: Duration = Duration::from_secs(5);

/// The width of a channel's member list.
const MEMBERS_WIDTH: u16 = 24;

/// Points the process's real stderr (fd 2) at `/dev/null`, best-effort.
/// Call this *before* the background `listen_forever` task (and
/// anything it spawns) has a chance to run — every status line it
/// prints (`eprintln!`: heartbeat, online/offline, message received,
/// accept failures, ...) has nowhere to go once the TUI's raw-mode
/// screen owns the terminal without corrupting the display, and the
/// same is true of the plain `eprintln!`s in `deliver_message`/
/// `get_posts` and friends, which this UI also calls directly. A
/// `quiet` flag threaded through every one of those functions would
/// work too, but touches far more code for the same result — this is
/// the one central point where "TUI is running" actually needs
/// handling.
///
/// `restore_stderr` (called both on ordinary exit and from a panic
/// hook, see `run`) prefers reopening the controlling terminal, which
/// is correct even if this process's original stderr was something
/// unusual (a pipe, a log file); the fd saved here is only a fallback
/// for the one case that doesn't work — no controlling terminal at
/// all (e.g. a non-interactive sandbox) — so a failure message still
/// reaches *somewhere* instead of vanishing into `/dev/null` forever.
///
/// `SWARM_LOG=<file>` sends it to that file (appended) instead — the
/// background connection log (online/offline, reconnects, cover) is
/// otherwise invisible while the TUI runs.
#[cfg(unix)]
pub fn silence_stderr() {
    let saved = unsafe { dup(2) };
    if saved >= 0 {
        let _ = SAVED_STDERR.set(saved);
    }
    if let Some(log) = std::env::var_os("SWARM_LOG") {
        use std::os::unix::io::IntoRawFd;
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            unsafe {
                dup2(file.into_raw_fd(), 2);
            }
            return;
        }
    }
    redirect_to("/dev/null");
}

#[cfg(unix)]
pub(crate) fn restore_stderr() {
    if redirect_to("/dev/tty") {
        return;
    }
    if let Some(&saved) = SAVED_STDERR.get() {
        unsafe {
            dup2(saved, 2);
        }
    }
}

#[cfg(unix)]
static SAVED_STDERR: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

#[cfg(unix)]
extern "C" {
    fn dup(fd: i32) -> i32;
    fn dup2(oldfd: i32, newfd: i32) -> i32;
}

#[cfg(unix)]
fn redirect_to(path: &str) -> bool {
    use std::os::unix::io::AsRawFd;
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => {
            unsafe {
                dup2(file.as_raw_fd(), 2);
            }
            true
        }
        Err(_) => false,
    }
}

/// Windows: the same, by swapping the process's standard-error handle
/// (which Rust looks up afresh on every write) for the log file or
/// `NUL`, and back.
#[cfg(windows)]
pub fn silence_stderr() {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE};
    let saved = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    let _ = SAVED_STDERR.set(saved as usize);
    let target = std::env::var_os("SWARM_LOG")
        .and_then(|log| std::fs::OpenOptions::new().create(true).append(true).open(log).ok())
        .or_else(|| std::fs::OpenOptions::new().write(true).open("NUL").ok());
    if let Some(file) = target {
        unsafe {
            SetStdHandle(STD_ERROR_HANDLE, file.into_raw_handle() as _);
        }
    }
}

#[cfg(windows)]
pub(crate) fn restore_stderr() {
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE};
    if let Some(&saved) = SAVED_STDERR.get() {
        unsafe {
            SetStdHandle(STD_ERROR_HANDLE, saved as _);
        }
    }
}

#[cfg(windows)]
static SAVED_STDERR: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

pub async fn run(
    node: NodeHandle,
    peers_path: PathBuf,
    backend_error: BackendError,
    call_events: mpsc::UnboundedReceiver<CallEvent>,
) -> Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableFocusChange);
        let _ = ratatui::try_restore();
        restore_stderr();
        default_hook(info);
    }));

    let result = match ratatui::try_init() {
        Ok(mut terminal) => {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableFocusChange);
            run_app(&mut terminal, node, peers_path, backend_error, call_events).await
        }
        Err(e) => Err(e.into()),
    };
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableFocusChange);
    let _ = ratatui::try_restore();
    restore_stderr();
    result
}

async fn run_app(
    terminal: &mut DefaultTerminal,
    node: NodeHandle,
    peers_path: PathBuf,
    backend_error: BackendError,
    mut call_events: mpsc::UnboundedReceiver<CallEvent>,
) -> Result<()> {
    let (jobs, mut ui_rx) = Jobs::new();
    let mut app = App::new(node, peers_path, backend_error, jobs);
    app.refresh_rows();

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<CEvent>();
    std::thread::spawn(move || loop {
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(true) => match crossterm::event::read() {
                Ok(event) => {
                    if input_tx.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });

    let mut tick = tokio::time::interval(TICK);
    let mut preview_tick = tokio::time::interval(PREVIEW_FRAME);
    preview_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut meter_tick = tokio::time::interval(METER_FRAME);
    meter_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut timeline_tick = tokio::time::interval(TIMELINE_REFRESH);
    timeline_tick.reset();
    let mut channel_updates = app.node.channel_updates();

    loop {
        app.list_for_mode();
        let adding = app.peer_form.as_ref().is_some_and(|f| matches!(f.kind, PeerFormKind::Add | PeerFormKind::Follow));
        swarm_app::discovery::browse(adding);
        if !adding {
            app.nearby_pick = None;
        }
        app.ringer.set(app.calls.incoming().is_some(), &paths().settings());
        terminal.draw(|f| ui(f, &app))?;
        if app.should_quit {
            swarm_app::discovery::goodbye();
            app.node.shutdown().await;
            return Ok(());
        }
        if app.node.wiped() {
            anyhow::bail!("{}", swarm_app::devices::WIPED);
        }

        let ring_flip = next_ring_flash(&app);
        tokio::select! {
            _ = tick.tick() => {
                if app.prefs.lock_after > 0 && app.locked.is_none() && app.last_key.elapsed() > Duration::from_secs(60 * u64::from(app.prefs.lock_after)) {
                    app.locked = Some((String::new(), String::new()));
                }
                app.refresh_rows();
                app.check_call_timeouts();
                app.check_transfers();
                app.check_device_news();
                app.calls.keep_listening(app.mode == MainMode::Settings && app.locked.is_none());
            }
            Ok(()) = channel_updates.changed() => {
                app.refresh_rows();
            }
            _ = preview_tick.tick(), if app.calls.preview().is_some() || app.calls.active().is_some_and(|c| c.video.is_some()) => {}
            _ = meter_tick.tick(), if app.calls.active().is_some() || app.mode == MainMode::Settings => {}
            _ = tokio::time::sleep_until(ring_flip.unwrap_or_else(tokio::time::Instant::now)), if ring_flip.is_some() => {}
            Some(done) = ui_rx.recv() => app.handle_done(done),
            maybe_call = call_events.recv() => {
                if let Some(event) = maybe_call {
                    app.handle_call_event(event);
                }
            }
            _ = timeline_tick.tick() => {
                if app.post_compose.is_none() {
                    app.fetch_timeline(true);
                }
            }
            maybe_event = input_rx.recv() => {
                match maybe_event {
                    Some(CEvent::Key(key)) => app.handle_key(key).await,
                    Some(CEvent::FocusGained) => (app.in_use, app.left) = (true, false),
                    Some(CEvent::FocusLost) => (app.in_use, app.left) = (false, true),
                    Some(_) => {}
                    None => return Ok(()),
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MainMode {
    /// Whatever's selected in the sidebar — public posts, private
    /// posts, and (for a real peer) the conversation with them.
    View,
    /// The details of whatever's selected: a peer, a channel, you.
    Peers,
    /// File transfers, both ways (`t`): the selected peer's, or
    /// everyone's with you selected.
    Transfers,
    /// Your name and how things are shown (`s`).
    Settings,
    /// The manual (`?`): its contents in the list's place, a page beside.
    Help,
}

/// What's being typed about one of your devices, or a contact's address.
#[derive(Clone, Copy, PartialEq)]
enum DeviceInput {
    /// A new name for this device.
    Rename,
    /// Where to try another of your devices (its key).
    Address([u8; 32]),
    /// A contact's address (their user key).
    ContactAddress([u8; 32]),
    /// This device's bandwidth limit.
    BandwidthLimit,
    /// This device's receive limit.
    ReceiveLimit,
}

/// What a peer's view shows: your conversation, or their posts (Tab).
#[derive(Clone, Copy, PartialEq)]
enum PeerTab {
    Chat,
    Posts,
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Nav,
    Input,
}

#[derive(Clone, Copy, PartialEq)]
enum PeerFormKind {
    Add,
    Update,
    /// Adding someone from their contact request — an Add, pre-filled
    /// from the request the way Update is from the peer.
    Accept,
    /// Following someone's public posts: their address and key.
    Follow,
}

struct PeerForm {
    kind: PeerFormKind,
    /// The peer being edited, for `Update` — its name as it was when the
    /// form opened, since the Name step can change it.
    target: Option<String>,
    step: u8,
    name: String,
    address: String,
    pubkey: String,
    /// Blank or `off` clears it (back to the session default), as
    /// `actions::saved_peer` reads it.
    cover: String,
    /// For `Accept`: the contact request.
    request: Option<Box<RequestRow>>,
}

/// Starting a group: of you and the contacts typed (`New`), or of the
/// contact whose handle it holds, you and those typed (`With`). See
/// `App::advance_channel_form`.
#[derive(Clone, PartialEq)]
enum ChannelFormKind {
    New,
    With(String),
}

/// The one field: contacts' names, separated by commas.
struct ChannelForm {
    kind: ChannelFormKind,
}

enum PostCompose {
    /// Typing the post text into `App::input`.
    Text,
    /// Text is finalised; waiting on a single y/n keypress for
    /// visibility.
    AskVisibility(String),
}

/// How the camera preview (`w`) is being drawn — the camera itself is
/// `Calls`'s — and frame counts for the rates shown with it.
struct Preview {
    mode: crate::picture::ColourMode,
    /// Frames drawn since it opened.
    drawn: std::cell::Cell<u64>,
    /// When the rates were last worked out, the counts then, and the
    /// rates (captured, drawn per second) — refreshed about once a second.
    rates: std::cell::Cell<(Instant, u64, u64, f32, f32)>,
}

/// What the file browser is open for.
enum Browsing {
    /// A file to offer this peer.
    SendTo(String),
    /// The folder to save offered transfer `id` in.
    SaveFor(swarm_core::transfer::TransferId),
    /// A picture of you, for your peers.
    Avatar,
}

/// One of a screen's buttons: what it says, and the key it stands for
/// (see `buttons`).
struct Button {
    label: &'static str,
    key: KeyCode,
}

struct App {
    identity: Arc<Identity>,
    browser: Option<(crate::browser::Browser, Browsing)>,
    /// A file picked to send, waiting on y/n: peer, path, size.
    confirm_send: Option<(String, PathBuf, u64)>,
    /// An incoming file offer waiting on a/d/l, like a call.
    offer_prompt: Option<node::TransferStatus>,
    /// The transfer selected in the transfers panel (see `picked_at`).
    transfer_selected: Option<swarm_core::transfer::TransferId>,
    /// A group action waiting on y/n.
    channel_confirm: Option<(ChannelId, Confirm)>,
    preview: Option<Preview>,
    node: NodeHandle,
    /// Background work: sends, fetches, offers, joins.
    jobs: Jobs,
    /// Peers' posts as last fetched.
    timelines: Timelines,
    /// File offers and invites put to the person, and transfer news.
    prompts: Prompts,
    /// How fast the files under way are going, for the top line.
    progress: swarm_app::progress::Meter,
    /// Calls and the camera preview's camera.
    calls: Calls,
    /// A video call's rates as last worked out, for its title: when, the
    /// counts then, and frames sent and shown per second, kbit/s sent.
    video_rates: std::cell::Cell<(Instant, u64, u64, u64, f32, f32, f32)>,
    peers_path: PathBuf,
    backend_error: BackendError,
    /// The list down the side — see `swarm_app::sidebar`.
    sidebar: Sidebar,
    selected: usize,
    mode: MainMode,
    focus: Focus,
    input: String,
    peer_form: Option<PeerForm>,
    /// Adding someone: which of those found on this network Tab last
    /// put in the address field (see `swarm_app::discovery`).
    nearby_pick: Option<usize>,
    channel_form: Option<ChannelForm>,
    post_compose: Option<PostCompose>,
    /// A peer name awaiting a y/n confirmation keypress for removal.
    pending_remove: Option<String>,
    /// A contact request awaiting a y/n confirmation to block its key.
    pending_block: Option<RequestRow>,
    /// When each person followed was last fetched, this session.
    followed_fetched: jobs::Followed,
    /// The button chosen with Left/Right, for Enter (see `buttons`);
    /// any other key puts it back.
    button_choice: Option<usize>,
    /// In Transfers, Up/Down moves through the transfers rather than
    /// the list (Tab).
    in_transfers: bool,
    /// The camera's open to take your picture (Settings, Your picture).
    taking_picture: bool,
    /// The folder a peer's files go to without asking, being typed: name, key.
    receive_prompt: Option<PeerRow>,
    /// How many lines back from the newest the open chat is scrolled
    /// (PageUp/PageDown); 0 follows new messages.
    chat_scroll: u16,
    /// A line of the open contact's chat picked with Up while typing, to
    /// delete with Delete: its `ChatItem::lines`.
    picked: Option<Vec<swarm_core::history::LineRef>>,
    /// One of the user's posts picked with Up while writing a post, to
    /// delete with Delete: its signature.
    picked_post: Option<[u8; 64]>,
    /// The open chat's text height and how many lines don't fit, as last
    /// drawn — what PageUp/PageDown page by and stop at.
    chat_view: std::cell::Cell<(u16, u16)>,
    status: String,
    status_kind: StatusKind,
    status_at: Instant,
    /// Where the cursor is in `input`: characters back from its end.
    input_back: usize,
    /// The last Tab's completion of a name (see `chat::complete`).
    completing: Option<chat::Completing>,
    /// How things are shown — see `look::Prefs`.
    prefs: prefs::Prefs,
    /// The open chat fills the screen: no sidebar, members or posts.
    maximised: bool,
    /// A video call fills the whole screen, nothing else on it (`v`).
    video_only: bool,
    /// When the video-only view was last opened (its keys show for a while).
    video_only_since: Instant,
    /// Our own picture left out of a video call (`s` in the video-only view).
    hide_self: bool,
    /// The button chosen on a ringing call's card (Left/Right, then Enter).
    ring_choice: usize,
    peer_tab: PeerTab,
    /// The line selected in Settings.
    setting_selected: usize,
    /// The manual's page open (a section of `swarm_app::manual`), how far
    /// down it's scrolled, and how many lines it last had room for (for
    /// paging); the drawing keeps the scroll inside the page.
    help_at: usize,
    help_scroll: std::cell::Cell<u16>,
    help_height: std::cell::Cell<u16>,
    /// Typing a new display name in Settings.
    /// Your name or status being typed (Settings).
    name_prompt: Option<prefs::Setting>,
    device_input: Option<DeviceInput>,
    /// A code for adding a device, while it's shown: its line, until
    /// when it's good, and how many devices there were when it was made
    /// (one more, and it's been used).
    device_code: Option<(String, Instant, usize)>,
    /// One of your devices waiting on y to be taken off.
    pending_device_remove: Option<[u8; 32]>,
    /// Removing someone from a conference: the number typed so far.
    removing: Option<String>,
    should_quit: bool,
    /// When a key was last pressed: for locking when left (Settings).
    last_key: Instant,
    /// The terminal's the one in use, as it last said (one that doesn't
    /// say counts as not: messages to chats not open make their sound).
    in_use: bool,
    /// The terminal's said it has lost the keyboard (not all of them say).
    left: bool,
    /// How much was unread in each chat, for the message sound.
    heard: swarm_app::notify::Unread,
    /// Rings while a call's ringing here.
    ringer: swarm_app::ringtone::Ringer,
    /// Locked after being left: the passphrase being typed, and why the
    /// last one didn't open it. Everything else is hidden meanwhile,
    /// though the node runs on underneath.
    locked: Option<(String, String)>,
}

impl App {
    fn new(node: NodeHandle, peers_path: PathBuf, backend_error: BackendError, jobs: Jobs) -> Self {
        App {
            identity: node.identity().clone(),
            browser: None,
            confirm_send: None,
            offer_prompt: None,
            transfer_selected: None,
            channel_confirm: None,
            preview: None,
            node,
            jobs,
            timelines: Timelines::default(),
            prompts: Prompts::default(),
            progress: Default::default(),
            calls: Calls::default(),
            video_rates: std::cell::Cell::new((Instant::now(), 0, 0, 0, 0.0, 0.0, 0.0)),
            peers_path,
            backend_error,
            sidebar: Sidebar::new(),
            selected: 1,
            mode: MainMode::View,
            focus: Focus::Nav,
            input: String::new(),
            peer_form: None,
            nearby_pick: None,
            channel_form: None,
            post_compose: None,
            pending_remove: None,
            pending_block: None,
            followed_fetched: jobs::Followed::default(),
            button_choice: None,
            in_transfers: false,
            taking_picture: false,
            receive_prompt: None,
            chat_scroll: 0,
            picked: None,
            picked_post: None,
            chat_view: std::cell::Cell::new((0, 0)),
            status: String::new(),
            status_kind: StatusKind::Info,
            status_at: Instant::now(),
            input_back: 0,
            completing: None,
            prefs: prefs::Prefs::load(&paths().settings()),
            maximised: false,
            video_only: false,
            video_only_since: Instant::now(),
            hide_self: false,
            ring_choice: 0,
            peer_tab: PeerTab::Chat,
            setting_selected: 0,
            help_at: 0,
            help_scroll: std::cell::Cell::new(0),
            help_height: std::cell::Cell::new(20),
            name_prompt: None,
            device_input: None,
            device_code: None,
            pending_device_remove: None,
            removing: None,
            should_quit: false,
            last_key: Instant::now(),
            in_use: false,
            left: false,
            heard: Default::default(),
            ringer: Default::default(),
            locked: None,
        }
    }

    fn set_status(&mut self, kind: StatusKind, msg: impl Into<String>) {
        self.status_kind = kind;
        self.status = msg.into();
        self.status_at = Instant::now();
    }

    /// Asks whom to remove from the conference, by their number: those
    /// who came in after us. `typed`: the number so far.
    fn ask_whom_to_remove(&mut self, typed: &str) {
        let numbered = self.calls.active().map(|c| c.numbered()).unwrap_or_default();
        let whom: Vec<String> = numbered.iter().filter(|m| m.3).map(|(n, name, _, _)| format!("{n} {name}")).collect();
        if whom.is_empty() {
            self.removing = None;
            let you = numbered.iter().find(|m| m.1 == "you").map_or(0, |m| m.0);
            self.set_status(StatusKind::Info, format!("you're number {you}: you can only remove someone who came into the call after you, and nobody has"));
            return;
        }
        self.set_status(StatusKind::Warning, format!("remove whom? {} — type their number{}", whom.join(", "), if typed.is_empty() { String::new() } else { format!(": {typed}") }));
    }

    /// A key while asking whom to remove: digits make up their number,
    /// which removes them once it can't be anyone else's (or on Enter);
    /// anything else cancels.
    fn on_removing_key(&mut self, key: KeyEvent, mut typed: String) {
        let numbered = self.calls.active().map(|c| c.numbered()).unwrap_or_default();
        let removable: Vec<(u32, [u8; 32])> = numbered.iter().filter(|m| m.3).map(|(n, _, k, _)| (*n, *k)).collect();
        match key.code {
            KeyCode::Char(d) if d.is_ascii_digit() => typed.push(d),
            KeyCode::Backspace => {
                typed.pop();
            }
            KeyCode::Enter => {}
            _ => {
                self.set_status(StatusKind::Info, "cancelled");
                return;
            }
        }
        let exact: Vec<[u8; 32]> = removable.iter().filter(|(n, _)| n.to_string() == typed).map(|(_, k)| *k).collect();
        let longer = removable.iter().any(|(n, _)| n.to_string().starts_with(&typed) && n.to_string() != typed);
        if !exact.is_empty() && (!longer || key.code == KeyCode::Enter) {
            for key in exact {
                let note = self.calls.remove(&self.node, key);
                self.note(Some(note));
            }
            return;
        }
        if key.code == KeyCode::Enter || !removable.iter().any(|(n, _)| n.to_string().starts_with(&typed)) {
            self.set_status(StatusKind::Info, format!("nobody you can remove is number {typed}"));
            return;
        }
        self.removing = Some(typed.clone());
        self.ask_whom_to_remove(&typed);
    }

    /// Shows something the shared layer said.
    fn note(&mut self, note: Option<Note>) {
        if let Some(note) = note {
            self.set_status(note.kind, note.text);
        }
    }

    fn say(&mut self, note: Note) {
        self.note(Some(note));
    }

    /// The status line's message, unless it's been up long enough to go.
    fn current_status(&self) -> Option<(&str, StatusKind)> {
        let expired = self.status_kind.lasts().is_some_and(|d| self.status_at.elapsed() > d);
        (!self.status.is_empty() && !expired).then_some((self.status.as_str(), self.status_kind))
    }

    /// In Peers, each section of the list ends with its ways to add to
    /// it; elsewhere it doesn't. The same entry stays selected.
    fn list_for_mode(&mut self) {
        let was_selected = self.sidebar.rows.get(self.selected).map(SidebarEntry::id);
        self.sidebar.set_adding(self.mode == MainMode::Peers);
        if let Some(i) = was_selected.and_then(|id| self.sidebar.position(&id)) {
            self.selected = i;
        }
        self.selected = self.selected.min(self.sidebar.rows.len() - 1);
    }

    /// Rebuilds the sidebar (see `swarm_app::sidebar`), keeping the same
    /// entry selected wherever it's moved to.
    fn refresh_rows(&mut self) {
        let was_selected = self.sidebar.rows.get(self.selected).map(SidebarEntry::id);
        if let Some(news) = self.sidebar.refresh(&self.node, &expand_home(&self.peers_path), &self.prefs, was_selected.as_ref(), self.looked_at()) {
            self.set_status(StatusKind::Info, news);
        }
        if let Some(i) = was_selected.and_then(|id| self.sidebar.position(&id)) {
            self.selected = i;
        }
        if self.selected >= self.sidebar.rows.len() {
            self.selected = self.sidebar.rows.len() - 1;
            self.chat_scroll = 0;
        }
        self.refresh_unread();
        self.sound_for_messages();
    }

    /// A sound for messages come while the terminal isn't the one in use,
    /// as Settings has it (see `swarm_app::notify::sounds`). The chat open
    /// counts as read, so it never makes one.
    fn sound_for_messages(&mut self) {
        let chats: Vec<(String, String, usize)> = self
            .sidebar
            .all
            .iter()
            .filter_map(|entry| match entry {
                SidebarEntry::Peer(row) => Some((entry.id(), row.label.clone(), row.unread)),
                SidebarEntry::Channel(row) => Some((entry.id(), row.name.clone(), row.noticed())),
                _ => None,
            })
            .collect();
        let in_use = self.in_use || !self.heard.has_looked();
        let notices = self.heard.notices(&chats, self.prefs.notify, in_use);
        if swarm_app::notify::sounds(&notices, self.calls.active().is_some() || self.calls.incoming().is_some()) {
            swarm_app::sound::play_chosen(&paths().settings());
        }
    }

    fn selected_request(&self) -> Option<&RequestRow> {
        match self.sidebar.rows.get(self.selected) {
            Some(SidebarEntry::Request(row)) => Some(row),
            _ => None,
        }
    }

    /// Opens the Add form for a contact request, pre-filled from it.
    fn start_accept_form(&mut self, request: RequestRow) {
        let name = request.suggested_peer_name();
        self.peer_form = Some(PeerForm {
            kind: PeerFormKind::Accept,
            target: None,
            step: 0,
            name: name.clone(),
            address: request.address.clone(),
            pubkey: STANDARD.encode(request.public_key),
            cover: String::new(),
            request: Some(Box::new(request)),
        });
        self.input = name;
        self.mode = MainMode::Peers;
        self.focus = Focus::Input;
    }

    /// The selected sidebar entry, if it's a real peer (not `You`) —
    /// what every peer-specific action (messaging, timeline fetch,
    /// peer management) actually operates on.
    fn selected_peer(&self) -> Option<&PeerRow> {
        match self.sidebar.rows.get(self.selected) {
            Some(SidebarEntry::Peer(row)) => Some(row),
            _ => None,
        }
    }

    /// The lines of the open contact's chat that can be picked (see
    /// `picked`): all but those being deleted already.
    fn pickable(&self) -> Vec<ChatItem> {
        let Some(row) = self.selected_peer() else { return Vec::new() };
        chat::peer_chat(row).into_iter().filter(|i| !i.lines.is_empty() && i.sent != chat::Sent::Deleting).collect()
    }

    /// Picks the line before the one picked (`step` -1; the newest, if
    /// none is), or after it (1; past the newest, none: back to typing).
    fn pick(&mut self, step: i32) {
        let items: Vec<Vec<swarm_core::history::LineRef>> = self.pickable().into_iter().map(|i| i.lines).collect();
        self.picked = pick_next(&items, self.picked.as_ref(), step);
        if self.picked.is_some() {
            self.set_status(StatusKind::Info, "Delete: delete it (one you sent, from their chat too) · Up/Down: another · Esc: back to typing");
        }
    }

    /// Picks one of the user's posts as `pick` does a chat's lines: Up
    /// from the newest.
    fn pick_post(&mut self, step: i32) {
        let mine: Vec<[u8; 64]> = posts::all(&paths().posts()).unwrap_or_default().into_iter().map(|p| p.signature).collect();
        self.picked_post = pick_next(&mine, self.picked_post.as_ref(), step);
        if self.picked_post.is_some() {
            self.set_status(StatusKind::Info, "Delete: delete it (from contacts' and followers' copies too) · Up/Down: another · Esc: back to writing");
        }
    }

    /// Deletes the post picked, at once, and picks the one before it.
    fn delete_picked_post(&mut self) {
        let Some(signature) = self.picked_post else { return };
        let mine: Vec<[u8; 64]> = posts::all(&paths().posts()).unwrap_or_default().into_iter().map(|p| p.signature).collect();
        let before = mine.iter().position(|s| *s == signature).and_then(|at| at.checked_sub(1)).map(|at| mine[at]);
        let note = actions::delete_post(&self.node, &signature);
        self.picked_post = before;
        self.note(note);
    }

    /// Deletes the line picked, at once, and picks the one before it.
    fn delete_picked(&mut self) {
        let Some(row) = self.selected_peer().cloned() else { return };
        let items = self.pickable();
        let Some(at) = self.picked.as_ref().and_then(|p| items.iter().position(|i| i.lines == *p)) else {
            self.picked = None;
            return;
        };
        let note = actions::delete_line(&self.node, &row, &items[at].lines);
        self.picked = at.checked_sub(1).map(|n| items[n].lines.clone());
        self.note(note);
    }

    fn selected_channel(&self) -> Option<&ChannelId> {
        match self.sidebar.rows.get(self.selected) {
            Some(SidebarEntry::Channel(row)) => Some(&row.id),
            _ => None,
        }
    }

    /// Whether someone's really here: what's on screen then counts as
    /// read on all of the user's devices.
    fn looked_at(&self) -> bool {
        !self.left && self.last_key.elapsed() < swarm_app::sidebar::LOOKED_AT_WITHIN
    }

    /// Recounts unread, with whatever's on screen counted as read — and
    /// notes how many of the open peer's posts have been seen, for the
    /// Posts tab's mark.
    fn refresh_unread(&mut self) {
        let open = self.sidebar.rows.get(self.selected).map(SidebarEntry::id);
        let looked_at = self.looked_at();
        self.sidebar.mark_read(&self.node, open.as_ref(), looked_at);
        if let Some(name) = self.selected_peer().map(|r| r.handle.clone()) {
            if self.peer_tab == PeerTab::Posts {
                self.timelines.mark_seen(&name);
            } else {
                self.timelines.baseline(&name);
            }
        }
    }

    /// The transfers Transfers shows: the selected peer's, or with
    /// anyone else selected (you), everyone's.
    fn shown_transfers(&self) -> Vec<node::TransferStatus> {
        let mut all = self.node.transfers();
        if let Some(row) = self.selected_peer() {
            all.retain(|t| t.is_with(&row.public_key));
        }
        all
    }

    /// A way to add, at the end of its section in Peers: its form.
    fn start_adding(&mut self, adding: Adding) {
        self.mode = MainMode::Peers;
        if adding == Adding::Device {
            self.show_device_code();
            return;
        }
        self.focus = Focus::Input;
        self.input.clear();
        match adding {
            Adding::Contact | Adding::Follow => {
                self.peer_form = Some(PeerForm {
                    kind: if adding == Adding::Follow { PeerFormKind::Follow } else { PeerFormKind::Add },
                    target: None,
                    step: 1,
                    name: String::new(),
                    address: String::new(),
                    pubkey: String::new(),
                    cover: String::new(),
                    request: None,
                });
            }
            _ => {
                self.channel_form = Some(ChannelForm { kind: ChannelFormKind::New });
            }
        }
    }

    /// Your picture from a file: the browser, where the last one was.
    fn choose_picture_file(&mut self) {
        let start = swarm_core::settings::get(&paths().settings(), "avatar_from").map(PathBuf::from).unwrap_or_else(crate::browser::home);
        self.browser = Some((crate::browser::Browser::new(crate::browser::Purpose::File, &start), Browsing::Avatar));
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

    /// Whether Left/Right and Enter are the buttons': nothing being
    /// answered, typed or chosen.
    fn buttons_take_keys(&self) -> bool {
        self.peer_form.is_none()
            && self.channel_form.is_none()
            && self.post_compose.is_none()
            && self.pending_remove.is_none()
            && self.pending_block.is_none()
            && self.pending_device_remove.is_none()
            && self.removing.is_none()
            && !self.video_only
            && self.calls.preview().is_none()
    }

    /// Call and Video call for `row`, each only while they can be rung
    /// (the window greys them; see `Calls::why_not_call`).
    fn call_buttons(&self, row: &PeerRow) -> Vec<Button> {
        let mut out = Vec::new();
        if self.calls.why_not_call(&self.node, row, false).is_none() {
            out.push(Button { label: "Call", key: KeyCode::Char('c') });
        }
        if self.calls.why_not_call(&self.node, row, true).is_none() {
            out.push(Button { label: "Video call", key: KeyCode::Char('v') });
        }
        out
    }

    /// The buttons on screen now, in order: what had only a key before,
    /// each still that key (see `button_lines`).
    fn buttons(&self) -> Vec<Button> {
        let b = |label, key| Button { label, key };
        let c = KeyCode::Char;
        let selected = self.sidebar.rows.get(self.selected);
        let mut out = Vec::new();
        match self.mode {
            MainMode::Transfers => {
                let all = self.shown_transfers();
                let ids: Vec<_> = all.iter().map(|t| t.transfer.id).collect();
                if let Some(t) = all.get(picked_at(&ids, self.transfer_selected)) {
                    if is_open_offer(t) {
                        out.extend([b("Accept", c('a')), b("Decline", c('d'))]);
                    }
                    if !t.transfer.state.is_finished() {
                        out.push(b("Cancel", c('x')));
                    } else {
                        out.push(b("Clear", c('l')));
                    }
                }
                if all.iter().any(|t| t.transfer.state.is_finished()) {
                    out.push(b("Clear all", c('L')));
                }
                return out;
            }
            MainMode::Settings | MainMode::Help => return out,
            MainMode::View | MainMode::Peers => {}
        }
        let peers = self.mode == MainMode::Peers;
        let call = |out: &mut Vec<Button>| {
            if let Some(call) = self.calls.active() {
                out.extend([b("Microphone", c('m')), b("Camera", c('w'))]);
                if call.video.is_some() {
                    out.push(b("Share screen", c('e')));
                }
                if self.selected_peer().is_some_and(|row| self.calls.why_not_add(&self.node, row).is_none()) {
                    out.push(b("Add to call", c('c')));
                }
                if call.in_conference() {
                    out.push(b("Remove someone", c('K')));
                }
                if call.video.is_some() {
                    out.push(b("Full screen", c('v')));
                }
                out.push(b("End call", c('h')));
                true
            } else if self.calls.outgoing().is_some() {
                out.push(b("End call", c('h')));
                true
            } else {
                false
            }
        };
        match selected {
            Some(SidebarEntry::Request(_)) => out.extend([b("Add them", c('a')), b("Dismiss", c('x')), b("Block", c('b'))]),
            Some(SidebarEntry::Followed(_)) => {
                if peers {
                    out.push(b("Posts", KeyCode::Enter));
                }
                out.extend([b("Refresh", c('r')), b("Stop following", c('x'))]);
            }
            Some(SidebarEntry::You) if !peers => out.push(b("New post", c('n'))),
            Some(SidebarEntry::You) if peers => {
                out.extend([b("Name", c('n')), b("Status", c('u')), b("Picture from a file", c('f')), b("Take a picture", c('e')), b("Manual address", c('i'))]);
                if swarm_app::avatar::pixels(&self.identity.public_key_bytes(), true).is_some() {
                    out.push(b("Remove picture", c('x')));
                }
                if !actions::blocked(&self.node).is_empty() {
                    out.push(b("Unblock", c('b')));
                }
            }
            Some(SidebarEntry::You) | Some(SidebarEntry::Heading(_)) | None => {}
            Some(SidebarEntry::Add(adding)) => out.push(b(adding.text(), KeyCode::Enter)),
            Some(SidebarEntry::Device(device)) => {
                if device.this {
                    out.push(b("Rename", c('n')));
                }
                if self.calls.why_not_call_device(&self.node, device, false).is_none() {
                    out.push(b("Call", c('c')));
                }
                if self.calls.why_not_call_device(&self.node, device, true).is_none() {
                    out.push(b("Video call", c('v')));
                }
                if self.node.devices().linked() {
                    out.push(b(if device.relay { "Stop relaying" } else { "Relay" }, c('l')));
                }
                if device.this {
                    out.push(b(if device.receive_only { "Dial again" } else { "Receive only" }, c('g')));
                    out.push(b("Bandwidth limit", c('z')));
                    out.push(b("Receive limit", c('r')));
                }
                if !device.this {
                    out.extend([b("Set address", c('i')), b("Answer its calls", c('o')), b(control_button(swarm_app::devices::auto_control(&self.node, &device.key)), c('a')), b("Remove", c('d'))]);
                }
            }
            Some(SidebarEntry::Peer(row)) if peers => {
                out.extend([b("Chat", KeyCode::Enter), b("Send a file", c('f'))]);
                if !call(&mut out) {
                    out.extend(self.call_buttons(row));
                }
                out.extend([b("Edit", c('u')), b("Set address", c('i')), b("Answer their calls", c('o')), b(control_button(row.auto_control), c('a')), b("Their files", c('r')), b(if row.asked { "Cancel request" } else { "Delete" }, c('d'))]);
            }
            Some(SidebarEntry::Peer(_)) if self.peer_tab == PeerTab::Posts => out.extend([b("Chat", KeyCode::Tab), b("Refresh", c('r'))]),
            Some(SidebarEntry::Peer(row)) => {
                if !call(&mut out) {
                    out.extend(self.call_buttons(row));
                }
                out.extend([b("Send a file", c('f')), b("Messages deleted after…", c('d')), b("Start a group", c('g')), b("Posts", KeyCode::Tab), b(if self.maximised { "Restore" } else { "Maximise" }, c('x'))]);
            }
            Some(SidebarEntry::Channel(_)) if peers => out.extend([b("Chat", KeyCode::Enter), b("Add people", c('g')), b("Leave", c('l'))]),
            Some(SidebarEntry::Channel(_)) => {
                if !call(&mut out) {
                    out.push(b("Call", c('c')));
                }
                out.extend([b("Add people", c('g')), b("Catch up", c('r')), b("Details", c('i')), b(if self.maximised { "Restore" } else { "Maximise" }, c('x'))]);
            }
        }
        out
    }

    fn submit_post(&mut self, text: String, public: bool) {
        self.focus = Focus::Nav;
        let visibility = if public { Visibility::Public } else { Visibility::Private };
        match posts::add(&paths().posts(), &self.identity, &text, visibility) {
            Ok(()) => self.set_status(StatusKind::Success, format!("posted ({})", if public { "public" } else { "private" })),
            Err(e) => self.set_status(StatusKind::Error, format!("failed to post: {e:#}")),
        }
    }

    /// Advances the currently active peer form by one field, submitting
    /// (`peer_add`/`peer_update`, then `peer_set_cover` if a rate was
    /// given) once the last one's filled in. Both kinds ask
    /// name/address/pubkey/cover. `Update` starts out holding the peer's
    /// current values and pre-fills each step with its own, so Enter
    /// keeps a field — and a field left blank keeps it too, rather than
    /// saving a blank address or an undecodable empty key. The cover
    /// step is optional either way: blank means "don't change it", type
    /// `off` to clear it.
    fn advance_peer_form(&mut self) {
        let Some(form) = self.peer_form.as_ref() else { return };
        let value = std::mem::take(&mut self.input);
        let mut value = value.trim().to_string();
        if form.kind == PeerFormKind::Update && form.step == 1 {
            use swarm_app::contact::{pasted_note, read_pasted, Pasted};
            match read_pasted(&value, Some(&form.pubkey)) {
                Pasted::Theirs(contact) => {
                    value = contact.address_text();
                    self.set_status(StatusKind::Info, "their addresses taken from their code");
                }
                pasted @ Pasted::SomeoneElse => {
                    self.input = form.address.clone();
                    self.set_status(StatusKind::Warning, pasted_note(&pasted, true));
                    return;
                }
                _ => {}
            }
        }
        let Some(form) = self.peer_form.as_mut() else { return };
        let is_update = form.kind != PeerFormKind::Add;
        if form.step > 3 {
            self.peer_form = None;
            self.focus = Focus::Nav;
            return;
        }
        let pasted = form.kind != PeerFormKind::Update && from_contact_line(form, &value);
        if pasted && form.kind != PeerFormKind::Follow {
            self.set_status(StatusKind::Info, "the contact line's name, address and key taken — now the cover rate");
            return;
        }
        if !pasted {
            let field = match form.step {
                0 => &mut form.name,
                1 => &mut form.address,
                2 => &mut form.pubkey,
                _ => &mut form.cover,
            };
            if !(is_update && value.is_empty()) {
                *field = value;
            }
        }
        if form.kind == PeerFormKind::Follow && form.step == 2 {
            let form = self.peer_form.take().expect("just matched Some above");
            self.focus = Focus::Nav;
            let (note, key) = actions::follow(&self.node, &form.pubkey, &form.address);
            if let Some(key) = key {
                self.followed_fetched.fetched(key);
            }
            self.say(note);
            self.refresh_rows();
            return;
        }
        if form.step < 3 {
            form.step += 1;
            if is_update {
                self.input = match form.step {
                    1 => form.address.clone(),
                    2 => form.pubkey.clone(),
                    _ => form.cover.clone(),
                };
            }
            return;
        }

        let form = self.peer_form.take().expect("just matched Some above");
        self.focus = Focus::Nav;
        let name = form.request.as_ref().map_or(form.name.clone(), |r| r.peer_name(&form.name));
        let result = match form.kind {
            PeerFormKind::Add | PeerFormKind::Accept | PeerFormKind::Follow => peerbook::add(&self.peers_path, &name, &form.address, &form.pubkey, Some(&self.node)),
            PeerFormKind::Update => peerbook::update(
                &self.peers_path,
                &form.target.expect("Update always carries a target"),
                &form.name,
                &form.address,
                &form.pubkey,
                Some(&self.node),
            ),
        };
        self.say(actions::saved_peer(&self.node, result, &form.pubkey, &form.cover));
        self.refresh_rows();
    }

    /// The folder browser, to choose where offered transfer `id` is saved
    /// — starting where the last one went.
    fn open_save_browser(&mut self, id: swarm_core::transfer::TransferId) {
        let start = swarm_app::paths::save_folder();
        self.browser = Some((crate::browser::Browser::new(crate::browser::Purpose::Folder, &start), Browsing::SaveFor(id)));
    }

    fn transfer_estimate(&self, peer: &str, size: u64) -> String {
        prompts::transfer_estimate(&self.node, &self.sidebar, peer, size)
    }

    /// Says when a transfer finishes, fails or is turned down, and puts a
    /// new incoming offer to the person.
    fn check_transfers(&mut self) {
        for note in self.prompts.transfer_news(&self.node) {
            self.note(Some(note));
        }
        if let Some(prompts::OfferNow::Gone(note)) = self.offer_prompt.as_ref().map(|offer| prompts::offer_now(&self.node, offer)) {
            self.offer_prompt = None;
            self.note(note);
        }
        if self.offer_prompt.is_some() || self.calls.incoming().is_some() || self.browser.is_some() {
            return;
        }
        if let Some((offer, note)) = self.prompts.next_offer(&self.node, &self.sidebar) {
            self.note(Some(note));
            self.offer_prompt = Some(offer);
        }
    }

    /// Control of a shared screen, either way (see `swarm_app::remote`).
    fn control(&self) -> swarm_app::calls::ControlState {
        self.calls.active().map(|c| c.control_state()).unwrap_or_default()
    }

    /// Says what's happened with your devices, and timers set by others.
    fn check_device_news(&mut self) {
        for note in self.prompts.link_news(&self.node) {
            self.note(Some(note));
        }
        if let Some((_, until, devices)) = &self.device_code {
            if Instant::now() >= *until {
                self.device_code = None;
                self.set_status(StatusKind::Info, "the code for adding a device has run out — Add a device, under DEVICES, makes another");
            } else if self.node.devices().devices.len() > *devices {
                self.device_code = None;
            }
        }
    }

    /// A code for adding a device, shown under You (chosen for it).
    fn show_device_code(&mut self) {
        match actions::device_code(&self.node) {
            Ok(line) => {
                self.device_code = Some((line, Instant::now() + swarm_core::node::joining::CODE_LIFETIME, self.node.devices().devices.len()));
                if let Some(at) = self.sidebar.rows.iter().position(|e| matches!(e, SidebarEntry::You)) {
                    self.selected = at;
                }
            }
            Err(note) => self.say(note),
        }
    }

    /// A `/command` typed in a group (see `swarm_app::commands`).
    fn channel_command(&mut self, id: &ChannelId, line: &str) {
        match commands::run(&self.node, &self.sidebar, id, line) {
            commands::Outcome::Done(note) => self.note(Some(note)),
            commands::Outcome::Ask { what, question } => return self.ask_channel_confirm(id, what, format!("{question} — y/n")),
        }
        self.refresh_rows();
    }

    fn ask_channel_confirm(&mut self, id: &ChannelId, what: Confirm, question: String) {
        self.channel_confirm = Some((id.clone(), what));
        self.focus = Focus::Nav;
        self.set_status(StatusKind::Warning, question);
    }

    /// Opens the camera preview — the camera opens on the spot (briefly
    /// blocking), and one that won't open shows why in the panel instead.
    fn open_preview(&mut self) {
        self.calls.open_preview();
        let now = Instant::now();
        self.preview = Some(Preview {
            mode: crate::picture::ColourMode::detect(),
            drawn: std::cell::Cell::new(0),
            rates: std::cell::Cell::new((now, 0, 0, 0.0, 0.0)),
        });
    }

    /// Sends whatever's in `self.input` to whatever's selected — a
    /// message for a peer, or (new) a post for a channel. A channel
    /// post needs no visibility choice the way `You`'s posts do (see
    /// `PostCompose`), so it's local-only and immediate: append a
    /// signed `channel::ChannelPost`, no network round trip, same
    /// "nothing is sent until someone asks" model `swarm channel post`
    /// already uses.
    async fn send_current_input(&mut self) {
        self.chat_scroll = 0;
        let text = std::mem::take(&mut self.input);
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        match self.sidebar.rows.get(self.selected).cloned() {
            Some(SidebarEntry::Peer(row)) => {
                self.set_status(StatusKind::Info, format!("sending to {}...", row.label));
                self.jobs.send(&self.node, &row, text);
                self.refresh_unread();
            }
            Some(SidebarEntry::Channel(row)) => {
                let id = row.id;
                if text.starts_with('/') && swarm_app::chat::action(&text).is_none() {
                    self.channel_command(&id, &text);
                } else if let Err(e) = self.node.post_to_channel(&id, &text) {
                    self.set_status(StatusKind::Error, format!("{e:#}"));
                }
            }
            _ => {}
        }
    }

    /// Starts the group the form's for, with the contacts typed, and
    /// opens it.
    fn advance_channel_form(&mut self) {
        let Some(form) = self.channel_form.take() else { return };
        self.focus = Focus::Nav;
        let typed = std::mem::take(&mut self.input);
        let mut contacts: Vec<&str> = typed.split(',').map(str::trim).filter(|c| !c.is_empty()).collect();
        if let ChannelFormKind::With(handle) = &form.kind {
            contacts.insert(0, handle);
        }
        let (group, note) = actions::new_group(&self.node, &contacts);
        self.say(note);
        self.refresh_rows();
        if let Some(i) = group.and_then(|g| self.sidebar.position(&swarm_app::sidebar::channel_entry_id(&g))) {
            self.selected = i;
            self.mode = MainMode::View;
            self.refresh_unread();
        }
    }

    /// Starts a group with the contact selected: asks who else.
    pub(super) fn start_group_with(&mut self, handle: String) {
        self.channel_form = Some(ChannelForm { kind: ChannelFormKind::With(handle) });
        self.focus = Focus::Input;
        self.input.clear();
    }

    /// Adding someone to the group open: `/add `, ready for a name.
    pub(super) fn start_adding_to_group(&mut self) {
        self.mode = MainMode::View;
        self.focus = Focus::Input;
        self.input = "/add ".to_string();
    }

    /// Fetches the selected peer's posts in the background (see
    /// `swarm_app::jobs::Timelines`) — unless `force` is off and they're
    /// already fetched this session. Nothing for anything but a peer.
    fn fetch_timeline(&mut self, force: bool) {
        if self.selected_followed().is_some() {
            self.fetch_followed(false);
            return;
        }
        let Some(name) = self.selected_peer().map(|r| r.handle.clone()) else { return };
        self.timelines.fetch(&self.jobs, &self.node, &self.sidebar, &name, force);
    }

    /// The selected person followed, if that's what's selected.
    fn selected_followed(&self) -> Option<&FollowRow> {
        match self.sidebar.rows.get(self.selected) {
            Some(SidebarEntry::Followed(row)) => Some(row),
            _ => None,
        }
    }

    /// Fetches the selected followed person's posts: `now`, or when they
    /// haven't been fetched in the last minute (they're not a peer: no
    /// connection is kept to them, each fetch dials).
    fn fetch_followed(&mut self, now: bool) {
        let Some(row) = self.selected_followed().cloned() else { return };
        let note = self.jobs.fetch_followed(&self.node, &mut self.followed_fetched, &row, now);
        self.note(note);
    }

    /// Takes in what a background job came back with.
    fn handle_done(&mut self, done: Done) {
        let joined = matches!(done, Done::Followed { .. });
        let note = jobs::finished(done, &mut self.timelines, &self.sidebar);
        self.note(note);
        if joined {
            self.refresh_rows();
        }
    }

    /// A ring that's run out of time, or whose caller gave up.
    fn check_call_timeouts(&mut self) {
        let note = self.calls.tick(&self.node, &self.sidebar);
        self.note(note);
    }

    /// The manual's page holding section `at`, from its top.
    fn help_open(&mut self, at: usize) {
        self.help_at = swarm_app::manual::get().page_of(at);
        self.help_scroll.set(0);
    }

    /// A call event from the node: a ring, or the outcome of a call.
    fn handle_call_event(&mut self, event: CallEvent) {
        if matches!(event, CallEvent::Incoming { .. }) {
            self.ring_choice = 0;
        }
        if matches!(event, CallEvent::Incoming { .. }) && self.calls.preview().is_some() {
            self.preview = None;
        }
        let note = self.calls.on_event(&self.node, &self.sidebar, event);
        self.note(note);
    }

}

/// Among the lines of a chat that can be picked (each one's `lines`),
/// the one before `picked` (`step` -1; the newest, if none is), or after
/// it (1; past the newest, none). One picked that's no longer there is as
/// none.
/// The button that turns control without asking on or off.
fn control_button(on: bool) -> &'static str {
    if on {
        "Ask before control"
    } else {
        "Control unasked"
    }
}

fn pick_next<T: PartialEq + Clone>(items: &[T], picked: Option<&T>, step: i32) -> Option<T> {
    let at = picked.and_then(|p| items.iter().position(|i| i == p));
    let next = match (at, step < 0) {
        (None, true) => items.len().checked_sub(1),
        (None, false) => None,
        (Some(at), true) => Some(at.saturating_sub(1)),
        (Some(at), false) => Some(at + 1).filter(|n| *n < items.len()),
    };
    next.map(|n| items[n].clone())
}

#[cfg(test)]
mod tests {

    /// Up picks the newest line, then older ones, stopping at the oldest;
    /// Down goes back, and past the newest picks none (typing again).
    #[test]
    fn up_picks_lines_from_the_newest_and_down_goes_back_to_typing() {
        use swarm_core::history::{digest, Direction, LineRef};
        let line = |at| vec![LineRef { direction: Direction::Sent, id: None, at, digest: digest("x") }];
        let items = [line(1), line(2), line(3)];
        let up = pick_next(&items, None, -1);
        assert_eq!(up, Some(line(3)), "the newest first");
        let up = pick_next(&items, up.as_ref(), -1);
        assert_eq!(up, Some(line(2)));
        assert_eq!(pick_next(&items, Some(&line(1)), -1), Some(line(1)), "the oldest stays picked");
        assert_eq!(pick_next(&items, up.as_ref(), 1), Some(line(3)));
        assert_eq!(pick_next(&items, Some(&line(3)), 1), None, "past the newest: typing again");
        assert_eq!(pick_next(&items, None, 1), None);
        assert_eq!(pick_next(&items, Some(&line(9)), -1), Some(line(3)), "one gone: as none");
        assert_eq!(pick_next::<Vec<LineRef>>(&[], None, -1), None, "nothing to pick");
    }

    /// The link shown changes to one with fewer pictures counted (another
    /// member talking): the rates start afresh. (It panicked, subtracting
    /// below none.)
    #[test]
    fn video_rates_start_afresh_when_the_link_shown_has_counted_less() {
        let rates = std::cell::Cell::new((Instant::now() - Duration::from_secs(2), 500, 400, 900_000, 0.0, 0.0, 0.0));
        let (_, sent, shown, bytes, sent_fps, _, _) = video_rates(&rates, 20, 10, 30_000);
        assert_eq!((sent, shown, bytes, sent_fps), (20, 10, 30_000, 0.0));
    }

    use super::*;

    /// A contact line typed into Add's address takes name, addresses and
    /// key, and goes on to the cover rate; a name typed first is kept.
    #[test]
    fn a_contact_line_fills_the_peer_form() {
        let key = [9u8; 32];
        let line = swarm_app::contact::line(&key, &[], &["203.0.113.7".parse().unwrap()], "Ann");
        let blank = |kind, name: &str| PeerForm { kind, target: None, step: 1, name: name.into(), address: String::new(), pubkey: String::new(), cover: String::new(), request: None };
        let mut form = blank(PeerFormKind::Add, "");
        assert!(from_contact_line(&mut form, &line));
        assert_eq!((form.name.as_str(), form.address.as_str(), form.step), ("Ann", "203.0.113.7", 3));
        assert_eq!(peerbook::decode_key(&form.pubkey).unwrap(), key);
        let mut named = blank(PeerFormKind::Add, "Annie");
        assert!(from_contact_line(&mut named, &line));
        assert_eq!(named.name, "Annie");
        let mut follow = blank(PeerFormKind::Follow, "");
        assert!(from_contact_line(&mut follow, &line));
        assert_eq!((follow.name.as_str(), follow.step), ("", 2), "following's done at the key");
        assert!(!from_contact_line(&mut blank(PeerFormKind::Add, ""), "203.0.113.7"), "an address is an address");
    }

    /// The transfer picked stays picked as others come (newest first,
    /// above it), and moving goes from wherever it is now.
    #[test]
    fn the_transfer_picked_stays_picked_as_others_come() {
        let ids = |list: &[u8]| list.iter().map(|&n| [n; 16]).collect::<Vec<_>>();
        let picked = moved_to(&ids(&[2, 1]), None, 1);
        assert_eq!(picked, Some([1; 16]));
        let now = ids(&[3, 2, 1]);
        assert_eq!(picked_at(&now, picked), 2, "still the one picked");
        assert_eq!(moved_to(&now, picked, -1), Some([2; 16]));
        assert_eq!(moved_to(&now, picked, 1), Some([1; 16]), "the last stays last");
        assert_eq!(picked_at(&ids(&[3, 2]), picked), 0, "gone: back to the top");
        assert_eq!(moved_to(&[], picked, 1), None);
    }

    #[test]
    fn the_selected_setting_is_always_in_sight() {
        assert_eq!(scroll_to_show((1, 4), 31), 0);
        assert_eq!(scroll_to_show((36, 39), 31), 8);
        assert_eq!(scroll_to_show((36, 39), 2), 36);
    }
    /// A button's key is lit in its label where the label has the letter
    /// (either case, but K only as K), and after it where it hasn't.
    #[test]
    fn a_buttons_key_is_shown_in_it_or_after_it() {
        use super::{button_spans, Button};
        use crossterm::event::KeyCode;
        use ratatui::style::Modifier;
        let shown = |label, key| {
            let spans = button_spans(&Button { label, key }, false);
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            let lit: String = spans.iter().filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED)).map(|s| s.content.as_ref()).collect();
            (text, lit)
        };
        assert_eq!(shown("Call", KeyCode::Char('c')), ("[Call]".to_string(), "C".to_string()));
        assert_eq!(shown("Send a file", KeyCode::Char('f')), ("[Send a file]".to_string(), "f".to_string()));
        assert_eq!(shown("Edit", KeyCode::Char('u')), ("[Edit u]".to_string(), "u".to_string()));
        assert_eq!(shown("Remove someone", KeyCode::Char('K')), ("[Remove someone K]".to_string(), "K".to_string()));
        assert_eq!(shown("Chat", KeyCode::Enter), ("[Chat ↵]".to_string(), "↵".to_string()));
    }
}
