//! Swarm's window: laid out like the terminal UI, on the shared app
//! layer, opened by the `swarm` command (`swarm --tui` for the terminal
//! UI instead). The window (Slint, on the main thread) only draws; the
//! controller (`controller.rs`, on a tokio runtime) holds the node and the
//! app's state, takes the person's actions, and sends back what to show
//! (`snapshot.rs`).
//!
//! `offscreen` draws the window into an image file instead, for checking
//! the layout with no screen (`swarm --snapshot`).

slint::include_modules!();

mod controller;
mod help;
pub mod keys;
use swarm_app::notify;
mod offscreen;
mod snapshot;

use controller::Action;
use swarm_core::lock::Locked;
use slint::ComponentHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub use notify::Notice;
pub use offscreen::snapshot_to_file;
/// A window drawn in memory: for `--snapshot`, and the tests'.
pub use offscreen::Offscreen;
#[cfg(test)]
use offscreen::Clipped;

/// swarm in sight or not, as the phone's app says (`set_in_sight`):
/// always in sight elsewhere.
static IN_SIGHT: AtomicBool = AtomicBool::new(true);
/// The controller's actions, for what comes from outside the window.
static ACTIONS: OnceLock<mpsc::UnboundedSender<Action>> = OnceLock::new();
/// What shows notifications: the phone's app's (`set_notifier`); none
/// elsewhere.
static NOTIFIER: OnceLock<Box<dyn Fn(Notice) + Send + Sync>> = OnceLock::new();

/// Swarm's come into sight (true) or gone out of it: the phone's app,
/// to the background or with its screen off. Out of it, nothing counts
/// as read.
pub fn set_in_sight(yes: bool) {
    IN_SIGHT.store(yes, Ordering::Relaxed);
    if let Some(actions) = ACTIONS.get() {
        let _ = actions.send(Action::InSight(yes));
    }
}

fn in_sight() -> bool {
    IN_SIGHT.load(Ordering::Relaxed)
}

/// The window's the one in use (has the keyboard), as last drawn: on a
/// computer, a message's sound is only made while it isn't.
static IN_USE: AtomicBool = AtomicBool::new(true);

fn window_in_use() -> bool {
    IN_USE.load(Ordering::Relaxed)
}

/// Notes whether `window`'s the one in use (see `IN_USE`).
fn note_in_use(window: &slint::Window) {
    IN_USE.store(i_slint_core::window::WindowInner::from_pub(window).active(), Ordering::Relaxed);
}

/// When a key was last pressed anywhere in the window (in a box being
/// typed in too), for locking when left: typing a long message, or
/// reading with the arrow keys, is using it.
static KEYED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

fn note_keyed() {
    *KEYED.locked() = Some(std::time::Instant::now());
}

fn last_keyed() -> Option<std::time::Instant> {
    *KEYED.locked()
}

/// Where notifications of messages go (once, before the window opens).
pub fn set_notifier(notifier: impl Fn(Notice) + Send + Sync + 'static) {
    let _ = NOTIFIER.set(Box::new(notifier));
}

/// What's told when a call starts or ends, when the camera's on in one
/// or not, and when it has video: the phone's app (its service says it
/// uses the microphone during one, and the camera; the screen's kept on
/// for its video); nothing elsewhere.
static CALL_HOOK: OnceLock<Box<dyn Fn(CallState) + Send + Sync>> = OnceLock::new();

/// A call, as the phone's app is told of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallState {
    /// Placed, ringing them, or under way.
    pub underway: bool,
    /// Our camera's on in it.
    pub camera: bool,
    /// It has video, either way: there are pictures to watch, so the
    /// screen's kept on.
    pub video: bool,
}

/// What to tell when a call starts or ends, or its camera or video goes
/// on or off (once, before the window opens).
pub fn set_call_hook(hook: impl Fn(CallState) + Send + Sync + 'static) {
    let _ = CALL_HOOK.set(Box::new(hook));
}

fn call_underway(call: CallState) {
    if let Some(hook) = CALL_HOOK.get() {
        hook(call);
    }
}

fn notifies() -> bool {
    NOTIFIER.get().is_some()
}

fn notify(notice: Notice) {
    if let Some(notifier) = NOTIFIER.get() {
        notifier(notice);
    }
}
use tokio::sync::{mpsc, oneshot};

/// Opens the window and runs until it's closed. `cover` is the default
/// cover rate (`swarm --cover`).
pub fn run(cover: Option<u64>) -> anyhow::Result<()> {
    control_bar::ON_DESKTOP.store(true, std::sync::atomic::Ordering::Relaxed);
    let ui = AppWindow::new()?;
    #[cfg(not(target_os = "android"))]
    wheel_sizes_text(&ui);
    run_in(ui, cover, true)
}

/// Ctrl and the mouse wheel make the text a size larger (up) or smaller
/// (down), as Ctrl + and Ctrl − do. Taken from the window's events before
/// Slint has them: by then whatever list or page is under the mouse
/// would have scrolled instead. Whether Ctrl's held is asked of the X
/// server as the wheel turns, on X11: winit's ModifiersChanged there, and
/// even its Ctrl key events (an input method, IBus, has the keys first),
/// were wrong on the user's GNOME desktop, from the start and again once
/// the wheel had scrolled something. Elsewhere (Wayland, Windows) winit's
/// ModifiersChanged, which comes from the compositor or Windows itself.
#[cfg(not(target_os = "android"))]
fn wheel_sizes_text(ui: &AppWindow) {
    use slint::winit_030::winit::event::{MouseScrollDelta, WindowEvent};
    use slint::winit_030::{EventResult, WinitWindowAccessor};
    let weak = ui.as_weak();
    let mut modifiers_control = false;
    let mut steps = WheelSteps::default();
    let mut server: Option<Option<HeldKeys>> = None;
    ui.window().on_winit_window_event(move |window, event| match event {
        WindowEvent::ModifiersChanged(m) => {
            modifiers_control = m.state().control_key();
            EventResult::Propagate
        }
        WindowEvent::MouseWheel { delta, .. } => {
            let server = server.get_or_insert_with(|| HeldKeys::of(window));
            let held = match server {
                Some(keys) => keys.control().unwrap_or(modifiers_control),
                None => modifiers_control,
            };
            if !held {
                steps = WheelSteps::default();
                return EventResult::Propagate;
            }
            let step = match delta {
                MouseScrollDelta::LineDelta(_, y) => steps.lines(*y),
                MouseScrollDelta::PixelDelta(p) => steps.pixels(p.y),
            };
            if step != 0 {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<App>().invoke_text_size(step);
                }
            }
            EventResult::PreventDefault
        }
        _ => EventResult::Propagate,
    });
}

/// Asks the X server which keys are held, for a window on X11.
#[cfg(not(target_os = "android"))]
struct HeldKeys {
    #[cfg(target_os = "linux")]
    x11: swarm_app::remote_x11::HeldKeys,
}

#[cfg(not(target_os = "android"))]
impl HeldKeys {
    /// For `window`, if it's on X11 and the server answers.
    fn of(window: &slint::Window) -> Option<HeldKeys> {
        #[cfg(target_os = "linux")]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let on_x11 = matches!(window.window_handle().window_handle().map(|h| h.as_raw()), Ok(RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_)));
            if !on_x11 {
                return None;
            }
            match swarm_app::remote_x11::HeldKeys::open(None) {
                Ok(x11) => Some(HeldKeys { x11 }),
                Err(e) => {
                    eprintln!("{} asking the X server about Ctrl: {e:#}", swarm_app::unix_now());
                    None
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = window;
            None
        }
    }

    fn control(&self) -> anyhow::Result<bool> {
        #[cfg(target_os = "linux")]
        return self.x11.control();
        #[cfg(not(target_os = "linux"))]
        unreachable!("only made on X11")
    }
}

/// A wheel's turning, as steps of the text size: one a notch, and with a
/// touchpad's (or a smooth wheel's) pixels, one each `WheelSteps::PIXELS`
/// of them, up larger. Never more than one step an event, so one fast
/// flick doesn't go from smallest to largest.
#[cfg(not(target_os = "android"))]
#[derive(Default)]
struct WheelSteps {
    /// Turned towards the next step, not yet a step.
    part: f64,
}

#[cfg(not(target_os = "android"))]
impl WheelSteps {
    const PIXELS: f64 = 60.0;

    fn lines(&mut self, lines: f32) -> i32 {
        self.add(lines as f64)
    }

    fn pixels(&mut self, pixels: f64) -> i32 {
        self.add(pixels / Self::PIXELS)
    }

    fn add(&mut self, notches: f64) -> i32 {
        if self.part * notches < 0.0 {
            self.part = 0.0;
        }
        self.part += notches;
        if self.part.abs() < 1.0 {
            return 0;
        }
        let step = self.part.signum() as i32;
        self.part = 0.0;
        step
    }
}

/// While someone's controlling our screen, a bar saying so, with Stop,
/// over everything else on the desktop: Swarm's own window may well be
/// behind what's shared. Only on a desktop's real screen (not the phone,
/// nor a snapshot, which have one window).
pub(crate) mod control_bar {
    use super::{Action, ControlBar, ACTIONS};
    use slint::ComponentHandle;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(crate) static ON_DESKTOP: AtomicBool = AtomicBool::new(false);

    /// See `swarm_app::remote_x11::keep_above`.
    #[cfg(target_os = "linux")]
    fn keep_above(bar: slint::Weak<ControlBar>) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Some(bar) = bar.upgrade() else { return };
        let window = match bar.window().window_handle().window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Xlib(h)) => h.window as u32,
            Ok(RawWindowHandle::Xcb(h)) => h.window.get(),
            _ => return,
        };
        if let Err(e) = swarm_app::remote_x11::keep_above(window) {
            eprintln!("{} keeping the bar on top: {e:#}", swarm_app::unix_now());
        }
    }

    thread_local! {
        static BAR: RefCell<Option<ControlBar>> = const { RefCell::new(None) };
    }

    /// Shows `text` on the bar, or (`None`) hides it. On the window's
    /// thread.
    pub(crate) fn show(text: Option<String>) {
        if !ON_DESKTOP.load(Ordering::Relaxed) {
            return;
        }
        BAR.with_borrow_mut(|bar| match text {
            Some(text) => {
                if bar.is_none() {
                    let Ok(new) = ControlBar::new() else { return };
                    new.on_stop(|| {
                        if let Some(tx) = ACTIONS.get() {
                            let _ = tx.send(Action::StopControl);
                        }
                    });
                    new.set_text(text.clone().into());
                    if new.show().is_err() {
                        return;
                    }
                    if let Some((x, y, width, _)) = swarm_app::screen::shared_area() {
                        let size = new.window().size();
                        new.window().set_position(slint::PhysicalPosition::new(x + (width as i32 - size.width as i32).max(0) / 2, y));
                    }
                    #[cfg(target_os = "linux")]
                    {
                        let weak = new.as_weak();
                        slint::Timer::single_shot(std::time::Duration::from_millis(300), move || keep_above(weak));
                    }
                    *bar = Some(new);
                }
                if let Some(bar) = bar.as_ref() {
                    bar.set_text(text.into());
                }
            }
            None => {
                if let Some(bar) = bar.take() {
                    let _ = bar.hide();
                }
            }
        });
    }
}

/// The window's keys (see `keys`): what one pressed does, and the hints
/// made again when the window changes a mode of its own.
pub fn wire_keys<W: ComponentHandle + 'static>(ui: &W)
where
    for<'a> App<'a>: slint::Global<'a, W>,
{
    let weak = ui.as_weak();
    ui.global::<App>().on_command_for(move |text| weak.upgrade().map_or(KeyCommand::None, |ui| keys::command_for(&ui.global::<App>(), &text)));
    ui.global::<App>().on_read_code(|text, key| read_code(&text, &key));
    ui.global::<App>().on_complete_name(|text| snapshot::complete_name(&text));
    let weak = ui.as_weak();
    ui.global::<App>().on_chat_text(move |from, to| {
        let Some(ui) = weak.upgrade() else { return Default::default() };
        let lines: Vec<ChatLine> = slint::Model::iter(&ui.global::<App>().get_lines()).collect();
        snapshot::chat_text(&lines, from, to).into()
    });
    let weak = ui.as_weak();
    ui.global::<App>().on_chat_day_of(move |i| {
        let Some(ui) = weak.upgrade() else { return -1 };
        let lines = ui.global::<App>().get_lines();
        (0..=i).rev().find(|&at| slint::Model::row_data(&lines, at as usize).is_some_and(|l| l.kind == LineKind::Day)).unwrap_or(-1)
    });
    let weak = ui.as_weak();
    ui.global::<App>().on_chat_selection(move |first, from, last, to| {
        let Some(ui) = weak.upgrade() else { return Default::default() };
        let lines: Vec<ChatLine> = slint::Model::iter(&ui.global::<App>().get_lines()).collect();
        snapshot::chat_selection(&lines, first, from, last, to).into()
    });
    let weak = ui.as_weak();
    ui.global::<App>().on_text_span_at(move |x, y, unit| {
        let none = TextSpan { start: -1, end: -1 };
        let Some(ui) = weak.upgrade() else { return none };
        let Some((text, at)) = text_at(ui.window(), x, y) else { return none };
        let (start, end) = snapshot::span_around(&text, at, unit);
        TextSpan { start: start as i32, end: end as i32 }
    });
    let weak = ui.as_weak();
    ui.global::<App>().on_keys_changed(move || {
        if let Some(ui) = weak.upgrade() {
            snapshot::show_hints(&ui.global::<App>());
        }
    });
}

/// The read-only text (a chat line's) under the point (`x`, `y`) in the
/// window, and the byte in it there, as the text itself would find it
/// were it clicked: what's under a drag that started on another line,
/// which only that one hears. Only what's been made is looked through
/// (a chat's lines on show, not all of them).
fn text_at(window: &slint::Window, x: f32, y: f32) -> Option<(String, usize)> {
    use i_slint_core::item_tree::ItemRc;
    use i_slint_core::items::TextInput;
    use i_slint_core::lengths::LogicalPoint;
    let inner = i_slint_core::window::WindowInner::from_pub(window);
    let adapter = inner.window_adapter();
    let point = LogicalPoint::new(x, y);
    let mut found = None;
    let mut stack = vec![ItemRc::new(inner.try_component()?, 0)];
    while let Some(item) = stack.pop() {
        if let Some(next) = item.next_sibling() {
            stack.push(next);
        }
        if let Some(child) = item.first_child() {
            stack.push(child);
        }
        let Some(input) = item.downcast::<TextInput>() else { continue };
        let input = input.as_pin_ref();
        if !input.read_only() || input.single_line() || !item.is_visible() {
            continue;
        }
        let geometry = item.geometry();
        let origin = item.map_to_window(geometry.origin);
        let local = LogicalPoint::new(point.x - origin.x, point.y - origin.y);
        if local.x >= 0.0 && local.y >= 0.0 && local.x < geometry.size.width && local.y < geometry.size.height {
            let (at, _) = input.byte_offset_for_position(local, &adapter, &item);
            found = Some((input.text().to_string(), at));
        }
    }
    found
}

/// Runs the controller with `ui` (the desktop's window or the phone's)
/// until it's closed. `chat_shown`: whether the chat selected shows from
/// the start (the phone's opens on its list).
fn run_in<W: ComponentHandle + 'static>(ui: W, cover: Option<u64>, chat_shown: bool) -> anyhow::Result<()>
where
    for<'a> App<'a>: slint::Global<'a, W>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let (tx, rx) = mpsc::unbounded_channel();
    wire(&ui.global::<App>(), tx.clone());
    wire_keys(&ui);
    help::wire(&ui);
    let _ = ACTIONS.set(tx.clone());
    runtime.spawn(controller::run(Ui::of(&ui), rx, cover, chat_shown));
    ui.run()?;

    let (done_tx, done_rx) = oneshot::channel();
    if tx.send(Action::Quit(done_tx)).is_ok() {
        let _ = runtime.block_on(async { tokio::time::timeout(Duration::from_secs(3), done_rx).await });
    }
    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}

/// The phone's window, as the Android app (android/): in the activity
/// Android started, Swarm's folder in the app's own storage.
#[cfg(target_os = "android")]
pub fn run_android(app: slint::android::AndroidApp) -> anyhow::Result<()> {
    if let Some(dir) = app.internal_data_path() {
        std::env::set_var("HOME", dir);
    }
    std::env::set_var("SWARM_LOG", "/proc/self/fd/2");
    std::env::set_var("SWARM_RATE_DEBUG", "1");
    slint::android::init(app).map_err(|e| anyhow::anyhow!("{e}"))?;
    run_in(PhoneWindow::new()?, None, false)
}

/// The window the controller drives, whichever it is: what it shows is
/// handed over on the window's own thread.
#[derive(Clone)]
pub(crate) struct Ui(Arc<dyn Fn(OnWindow) + Send + Sync>);

/// Something to do on the window's thread, with the window.
type OnWindow = Box<dyn FnOnce(&App, &slint::Window) + Send>;

impl Ui {
    fn of<W: ComponentHandle + 'static>(window: &W) -> Ui
    where
        for<'a> App<'a>: slint::Global<'a, W>,
    {
        let weak = window.as_weak();
        Ui(Arc::new(move |show| {
            let _ = weak.upgrade_in_event_loop(move |window| show(&window.global::<App>(), window.window()));
        }))
    }

    pub(crate) fn show(&self, show: impl FnOnce(&App, &slint::Window) + Send + 'static) {
        (self.0)(Box::new(show))
    }
}

/// The window, as the system's file dialog takes it, to be put in front
/// of it (see `swarm_app::filedialog::Parent`). Asked of the window's
/// thread from another: none if it doesn't answer at once.
pub(crate) fn dialog_parent(ui: &Ui) -> swarm_app::filedialog::Parent {
    let (tx, rx) = std::sync::mpsc::channel();
    ui.show(move |_, window| {
        let _ = tx.send(parent_of(window));
    });
    rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default()
}

#[cfg(not(target_os = "android"))]
fn parent_of(window: &slint::Window) -> swarm_app::filedialog::Parent {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use swarm_app::filedialog::Parent;
    match window.window_handle().window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Xlib(h)) => Parent::X11(h.window as u32),
        Ok(RawWindowHandle::Xcb(h)) => Parent::X11(h.window.get()),
        Ok(RawWindowHandle::Win32(h)) => Parent::Win32(h.hwnd.get()),
        _ => Parent::None,
    }
}

#[cfg(target_os = "android")]
fn parent_of(_window: &slint::Window) -> swarm_app::filedialog::Parent {
    swarm_app::filedialog::Parent::None
}

/// Every button and box in the window sends its action to the controller.
fn wire(ui: &App, tx: mpsc::UnboundedSender<Action>) {
    macro_rules! on {
        ($setter:ident, |$($arg:ident),*| $action:expr) => {{
            let tx = tx.clone();
            ui.$setter(move |$($arg),*| {
                let _ = tx.send($action);
            });
        }};
    }
    on!(on_select, |i| Action::Select(i as usize));
    on!(on_move, |delta| Action::Move(delta));
    on!(on_escape, | | Action::Escape);
    on!(on_new_post, | | Action::NewPost);
    on!(on_text_size, |step| Action::TextSize(step));
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
    ui.on_used(note_keyed);
    ui.on_open_link(|url| swarm_app::links::open(&url));
    on!(on_show_view, |v| Action::ShowView(v));
    on!(on_narrow_list, |narrow| Action::NarrowList(narrow));
    on!(on_set_list_width, |width| Action::ListWidth(width.round().max(0.0) as u32));
    on!(on_hide_members, |hidden| Action::HideMembers(hidden));
    on!(on_set_members_width, |width| Action::MembersWidth(width.round().max(0.0) as u32));
    on!(on_set_tab, |t| Action::SetTab(t));
    on!(on_send, |text| Action::Send(text.to_string()));
    on!(on_delete_line, |refs| Action::DeleteLine(refs.to_string()));
    on!(on_delete_post, |refs| Action::DeletePost(refs.to_string()));
    on!(on_post, |text, public| Action::Post(text.to_string(), public));
    on!(on_refresh, | | Action::Refresh);
    on!(on_place_call, |video| Action::Call(video));
    on!(on_add_named, |name| Action::AddToCall(name.to_string()));
    on!(on_pick, |kind, typed| Action::Pick(kind, typed.to_string()));
    on!(on_open_chat, |name| Action::OpenChat(name.to_string()));
    on!(on_hang_up, | | Action::HangUp);
    on!(on_remove_from_call, |number| Action::RemoveFromCall(number));
    on!(on_toggle_camera, | | Action::ToggleCamera);
    on!(on_use_camera, |index| Action::UseCamera(index));
    on!(on_toggle_screen, | | Action::ToggleScreen);
    on!(on_toggle_control, | | Action::ToggleControl);
    on!(on_answer_control, |allow| Action::AnswerControl(allow));
    on!(on_stop_control, | | Action::StopControl);
    on!(on_control_pointer, |x, y| Action::ControlPointer(x, y));
    on!(on_control_button, |button, down| Action::ControlButton(button, down));
    on!(on_control_wheel, |dx, dy| Action::ControlWheel(dx, dy));
    on!(on_control_key, |text, down| Action::ControlKey(text.to_string(), down));
    on!(on_toggle_mute, | | Action::ToggleMute);
    on!(on_preview, |open| Action::Preview(open));
    on!(on_answer, |video| Action::Answer(video));
    on!(on_reject_call, | | Action::Reject);
    on!(on_offer, |choice| Action::Offer(choice));
    on!(on_confirm, |yes| Action::Confirm(yes));
    on!(on_confirm_block, | | Action::ConfirmBlock);
    on!(on_send_file, | | Action::SendFile);
    on!(on_next_timer, | | Action::NextTimer);
    on!(on_add_peer, |n, a, k, c| Action::AddPeer(n.to_string(), a.to_string(), k.to_string(), c.to_string()));
    on!(on_update_peer, |n, a, k, c| Action::UpdatePeer(n.to_string(), a.to_string(), k.to_string(), c.to_string()));
    on!(on_remove_peer, | | Action::RemovePeer);
    on!(on_follow, |a, k| Action::Follow(a.to_string(), k.to_string()));
    on!(on_unfollow, | | Action::Unfollow);
    on!(on_set_receive, |k, f| Action::SetReceive(k.to_string(), f.to_string()));
    on!(on_cycle_answer, | | Action::CycleAnswer);
    on!(on_set_answer, |at| Action::SetAnswer(at.max(0) as usize));
    on!(on_set_control, |on| Action::SetControl(on));
    on!(on_set_timer, |at| Action::SetTimer(at.max(0) as usize));
    on!(on_request, |choice| Action::Request(choice));
    on!(on_add_request, |name| Action::AddRequest(name.to_string()));
    on!(on_new_group, | | Action::NewGroup);
    on!(on_group_with, |name| Action::GroupWith(name.to_string()));
    on!(on_rename_group, |name| Action::RenameGroup(name.to_string()));
    on!(on_transfer, |i, what| Action::Transfer(i as usize, what));
    on!(on_clear_transfers, | | Action::ClearTransfers);
    on!(on_set_name, |name| Action::SetName(name.to_string()));
    on!(on_set_says, |says| Action::SetSays(says.to_string()));
    on!(on_set_manual_address, |text| Action::SetManualAddress(text.to_string()));
    on!(on_set_discoverable, |on| Action::SetDiscoverable(on));
    on!(on_browse, |on| Action::Browse(on));
    on!(on_avatar, |what| Action::Avatar(what));
    on!(on_scan, |on| Action::Scan(on));
    on!(on_scan_device, | | Action::ScanDevice);
    on!(on_sound, |what| Action::Sound(what));
    on!(on_ringtone, |what| Action::Ringtone(what));
    on!(on_set_setting, |which, value| Action::SetSetting(which, value));
    on!(on_first_run, |name, pass, again| Action::FirstRun(name.to_string(), pass.to_string(), again.to_string()));
    on!(on_restore_identity, |words, name, pass, again| Action::RestoreIdentity(words.to_string(), name.to_string(), pass.to_string(), again.to_string()));
    on!(on_join_user, |code, name, pass, again| Action::JoinUser(code.to_string(), name.to_string(), pass.to_string(), again.to_string()));
    on!(on_unlock, |pass| Action::Unlock(pass.to_string()));
    on!(on_reset_passphrase, |words, pass, again| Action::ResetPassphrase(words.to_string(), pass.to_string(), again.to_string()));
    on!(on_suggest_passphrase, | | Action::SuggestPassphrase);
    on!(on_words_written, | | Action::WordsWritten);
    on!(on_device_rename, |name| Action::DeviceRename(name.to_string()));
    on!(on_show_device_code, |on| Action::DeviceCode(on));
    on!(on_device_remove, | | Action::DeviceRemove);
    on!(on_device_relay, |yes| Action::DeviceRelay(yes));
    on!(on_device_receive_only, |yes| Action::DeviceReceiveOnly(yes));
    on!(on_device_bandwidth_limit, |text| Action::DeviceBandwidthLimit(text.to_string()));
    on!(on_device_receive_limit, |text| Action::DeviceReceiveLimit(text.to_string()));
    on!(on_device_address, |text| Action::DeviceAddress(text.to_string()));
    on!(on_contact_address, |text| Action::ContactAddress(text.to_string()));
    on!(on_unblock, |i| Action::Unblock(i as usize));
    on!(on_chat_shown, |shown| Action::ChatShown(shown));
}

/// A code pasted into Add a contact or Edit: what it fills in, and what
/// to say about it; `key`, the contact edited ("" adding).
fn read_code(text: &str, key: &str) -> CodeRead {
    use swarm_app::contact::{pasted_note, read_pasted, Pasted};
    let pasted = read_pasted(text, (!key.is_empty()).then_some(key));
    let note = pasted_note(&pasted, !key.is_empty()).into();
    match pasted {
        Pasted::Theirs(c) => CodeRead { ok: true, name: c.name.as_str().into(), address: c.address_text().into(), key: c.key_text().into(), note },
        _ => CodeRead { note, ..Default::default() },
    }
}

#[cfg(test)]
mod tests {
    /// Ctrl and the wheel: a step of the text size a notch, or each 60
    /// of a touchpad's pixels; one at most an event; turning back starts
    /// again.
    #[test]
    fn the_wheel_steps_the_text_size() {
        let mut steps = WheelSteps::default();
        assert_eq!((steps.lines(1.0), steps.lines(-1.0), steps.lines(5.0)), (1, -1, 1));
        let pixels: Vec<i32> = [20.0, 20.0, 20.0, 20.0, 400.0].iter().map(|p| steps.pixels(*p)).collect();
        assert_eq!(pixels, [0, 0, 1, 0, 1]);
        assert_eq!((steps.pixels(40.0), steps.pixels(-40.0), steps.pixels(-40.0)), (0, 0, -1));
    }

    /// A page of the manual longer than the window once its text wraps
    /// (here, at the largest text size) scrolls to its end.
    #[test]
    fn a_long_help_page_at_large_text_scrolls_to_its_end() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
        use slint::platform::WindowEvent;
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::default() })).unwrap();
        let ui = AppWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(1100, 700));
        ui.show().unwrap();
        let draw = || {
            slint::platform::update_timers_and_animations();
            window.request_redraw();
            window.draw_if_needed(|renderer| { renderer.render(&mut vec![Rgb565Pixel::default(); 1100 * 700], 1100); });
        };
        help::wire(&ui);
        let app = ui.global::<App>();
        app.set_view(Pane::Help);
        let scale = 1.75;
        ui.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: scale });
        ui.window().dispatch_event(WindowEvent::Resized { size: slint::LogicalSize::new(1100.0 / scale, 700.0 / scale) });
        app.invoke_help_link("2.7".into());
        draw();
        let next = || {
            use i_slint_backend_testing::ElementRoot;
            ui.root_element().query_descendants().match_predicate(|e| e.accessible_label().is_some_and(|l| l.ends_with(" ›"))).find_first()
        };
        let bottom = 700.0 / scale;
        assert!(next().is_none_or(|b| b.absolute_position().y > bottom), "the page runs off the window");
        let over = slint::LogicalPosition::new(400.0 / scale, 400.0 / scale);
        ui.window().dispatch_event(WindowEvent::PointerMoved { position: over });
        for _ in 0..100 {
            ui.window().dispatch_event(WindowEvent::PointerScrolled { position: over, delta_x: 0.0, delta_y: -60.0 });
            draw();
        }
        let next = next().expect("the button to the next page");
        assert!(next.absolute_position().y + next.size().height <= bottom, "scrolled down to it: {:?}", next.absolute_position());
    }

    /// The line between the list and the rest: dragged, the list's
    /// width is kept; dragged nearly to the edge, or its ‹ clicked, the
    /// list narrows to pictures, each one's name on the mouse.
    #[test]
    fn the_list_is_dragged_wider_and_narrowed_to_pictures() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
        use slint::platform::{PointerEventButton, WindowEvent};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::default() })).unwrap();
        let ui = AppWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(1100, 700));
        ui.show().unwrap();
        let draw = || {
            slint::platform::update_timers_and_animations();
            window.request_redraw();
            window.draw_if_needed(|renderer| { renderer.render(&mut vec![Rgb565Pixel::default(); 1100 * 700], 1100); });
        };
        let app = ui.global::<App>();
        let heading = Row { kind: RowKind::Heading, mark: "▾".into(), name: "CONTACTS".into(), ..Default::default() };
        let ann = Row { kind: RowKind::Peer, name: "Ann".into(), letter: "A".into(), unread: 3, ..Default::default() };
        app.set_rows(slint::ModelRc::new(slint::VecModel::from(vec![heading, ann])));
        app.set_view(Pane::Peer);
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        app.on_narrow_list({
            let asked = asked.clone();
            move |narrow| asked.borrow_mut().push(format!("narrow {narrow}"))
        });
        app.on_set_list_width({
            let asked = asked.clone();
            move |width| asked.borrow_mut().push(format!("width {width}"))
        });
        draw();
        let named = |name: &'static str| {
            use i_slint_backend_testing::ElementRoot;
            ui.root_element().query_descendants().match_predicate(move |e| e.accessible_label().as_deref() == Some(name)).find_first()
        };
        assert!(named("Ann").is_some(), "named in the list");
        let at = |x: f32| slint::LogicalPosition::new(x, 200.0);
        let drag = |from: f32, to: &[f32]| {
            ui.window().dispatch_event(WindowEvent::PointerMoved { position: at(from) });
            ui.window().dispatch_event(WindowEvent::PointerPressed { position: at(from), button: PointerEventButton::Left });
            for x in to {
                ui.window().dispatch_event(WindowEvent::PointerMoved { position: at(*x) });
            }
            ui.window().dispatch_event(WindowEvent::PointerReleased { position: at(*to.last().unwrap()), button: PointerEventButton::Left });
            draw();
        };
        drag(240.0, &[260.0, 300.0, 340.0]);
        assert_eq!(*asked.borrow(), ["width 340", "narrow false"]);
        asked.borrow_mut().clear();
        drag(240.0, &[600.0, 900.0]);
        assert_eq!(*asked.borrow(), ["width 550", "narrow false"]);
        asked.borrow_mut().clear();
        drag(240.0, &[200.0, 120.0, 40.0]);
        assert_eq!(*asked.borrow(), ["narrow true"]);
        app.set_list_narrow(true);
        draw();
        assert!(named("Ann").is_none(), "no names narrowed");
        ui.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(32.0, 80.0) });
        draw();
        assert_eq!(ui.get_tip(), "Ann", "the name on the mouse");
        asked.borrow_mut().clear();
        let grip = slint::LogicalPosition::new(64.0, 39.0 + (700.0 - 39.0 - 23.0) / 2.0);
        click(ui.window(), grip);
        assert_eq!(*asked.borrow(), ["narrow false"]);
    }

    /// The line between a channel's chat and its members: dragged, their
    /// width is kept (never more than half beside the list); dragged
    /// nearly to the edge, or its › clicked, they fold away, and its ‹,
    /// left at the edge, brings them back.
    #[test]
    fn a_channels_members_are_dragged_wider_and_folded_away() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
        use slint::platform::{PointerEventButton, WindowEvent};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::default() })).unwrap();
        let ui = AppWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(1100, 700));
        ui.show().unwrap();
        let draw = || {
            slint::platform::update_timers_and_animations();
            window.request_redraw();
            window.draw_if_needed(|renderer| { renderer.render(&mut vec![Rgb565Pixel::default(); 1100 * 700], 1100); });
        };
        let app = ui.global::<App>();
        app.set_view(Pane::Channel);
        let member = |name: &str| Member { name: name.into(), connected: true, ..Default::default() };
        app.set_members(slint::ModelRc::new(slint::VecModel::from(vec![member("Dave"), member("Rupert")])));
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        app.on_hide_members({
            let asked = asked.clone();
            move |hidden| asked.borrow_mut().push(format!("hidden {hidden}"))
        });
        app.on_set_members_width({
            let asked = asked.clone();
            move |width| asked.borrow_mut().push(format!("width {width}"))
        });
        draw();
        let named = |name: &'static str| {
            use i_slint_backend_testing::ElementRoot;
            ui.root_element().query_descendants().match_predicate(move |e| e.accessible_label().as_deref() == Some(name)).find_first()
        };
        assert!(named("Rupert").is_some(), "the members shown");
        let grip = middle(&named("›").expect("the line's ›"));
        let at = |x: f32| slint::LogicalPosition::new(x, 200.0);
        let drag = |from: f32, to: &[f32]| {
            ui.window().dispatch_event(WindowEvent::PointerMoved { position: at(from) });
            ui.window().dispatch_event(WindowEvent::PointerPressed { position: at(from), button: PointerEventButton::Left });
            for x in to {
                ui.window().dispatch_event(WindowEvent::PointerMoved { position: at(*x) });
            }
            ui.window().dispatch_event(WindowEvent::PointerReleased { position: at(*to.last().unwrap()), button: PointerEventButton::Left });
            draw();
        };
        let line = grip.x;
        drag(line, &[line - 20.0, line - 60.0]);
        assert_eq!(*asked.borrow(), ["width 270", "hidden false"]);
        asked.borrow_mut().clear();
        drag(line, &[line - 500.0, line - 700.0]);
        let width: f32 = asked.borrow()[0].strip_prefix("width ").unwrap().parse().unwrap();
        assert!((400.0..450.0).contains(&width), "about half of 1100 less the list: {width}");
        asked.borrow_mut().clear();
        drag(line, &[line + 100.0, line + 200.0]);
        assert_eq!(*asked.borrow(), ["hidden true"]);
        app.set_members_hidden(true);
        draw();
        assert!(named("Rupert").is_none(), "folded away");
        asked.borrow_mut().clear();
        let rightmost = {
            use i_slint_backend_testing::ElementRoot;
            ui.root_element().query_descendants().match_predicate(|e| e.accessible_label().as_deref() == Some("‹")).find_all().into_iter().max_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x))
        };
        let back = rightmost.expect("the ‹");
        assert!(back.absolute_position().x > 1000.0, "at the right edge: {:?}", back.absolute_position());
        click(ui.window(), middle(&back));
        assert_eq!(*asked.borrow(), ["hidden false"]);
        app.set_members_hidden(false);
        draw();
        asked.borrow_mut().clear();
        click(ui.window(), middle(&named("›").expect("the ›")));
        assert_eq!(*asked.borrow(), ["hidden true"]);
    }

    use super::*;

    /// The window can always be made as big as the screen: nothing in it
    /// may cap its size (a hidden measure once held it at the tabs' width,
    /// so it couldn't be dragged wider).
    #[test]
    fn nothing_caps_the_window() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Offscreen { window: window.clone() })).unwrap();
        let ui = AppWindow::new().unwrap();
        for view in [Pane::Mine, Pane::Peers, Pane::Channels, Pane::Transfers, Pane::Settings, Pane::Help, Pane::FirstRun] {
            ui.global::<App>().set_view(view);
            for call in ["", "on a call with someone who has a long name  00:05"] {
                { let app = ui.global::<App>(); app.set_call(crate::CallShown { text: call.into(), ..app.get_call() }); }
                assert!(ui.get_layout_max_width() > 10_000.0, "view {view:?}: at most {} wide", ui.get_layout_max_width());
                assert!(ui.get_layout_max_height() > 10_000.0, "view {view:?}: at most {} high", ui.get_layout_max_height());
                assert!(ui.get_layout_min_width() < 1000.0, "view {view:?}: at least {} wide", ui.get_layout_min_width());
            }
        }
    }

    /// A chat's line, said by Ann; `refs` what deletes it ("" for none).
    fn ann_says(text: &str, refs: &str) -> ChatLine {
        ChatLine { kind: LineKind::Said, time: "12:00".into(), name: "Ann".into(), role: 1, text: text.into(), mark: "".into(), for_me: false, linked: false, rich: Default::default(), refs: refs.into(), deleting: false, first: true }
    }

    /// A window on a chat, with one line in it, and what it copies.
    fn chat_window(text: &str) -> (AppWindow, std::rc::Rc<std::cell::RefCell<Option<String>>>) {
        chat_window_with(ann_says(text, ""))
    }

    /// `chat_window`, with `line` in it.
    fn chat_window_with(line: ChatLine) -> (AppWindow, std::rc::Rc<std::cell::RefCell<Option<String>>>) {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        let clipboard = std::rc::Rc::default();
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::clone(&clipboard) })).unwrap();
        DRAWN.with(|drawn| *drawn.borrow_mut() = Some(window.clone()));
        let ui = AppWindow::new().unwrap();
        wire_keys(&ui);
        window.set_size(slint::PhysicalSize::new(1280, 820));
        ui.show().unwrap();
        let app = ui.global::<App>();
        app.set_view(Pane::Peer);
        app.set_lines(slint::ModelRc::new(slint::VecModel::from(vec![snapshot::with_links(line)])));
        window.draw_if_needed(|renderer| {
            let mut pixels = vec![slint::platform::software_renderer::Rgb565Pixel::default(); 1280 * 820];
            renderer.render(&mut pixels, 1280);
        });
        (ui, clipboard)
    }

    thread_local! {
        /// The window `chat_window` made, to draw it again.
        static DRAWN: std::cell::RefCell<Option<std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>>> = const { std::cell::RefCell::new(None) };
    }

    /// Draws `chat_window`'s window again, as a screen would after a change.
    fn redraw() {
        DRAWN.with(|drawn| {
            if let Some(window) = drawn.borrow().as_ref() {
                let size = window.size();
                window.request_redraw();
                window.draw_if_needed(|renderer| { renderer.render(&mut vec![slint::platform::software_renderer::Rgb565Pixel::default(); (size.width * size.height) as usize], size.width as usize); });
            }
        });
    }

    /// Where a line's text is on the window.
    fn said_on(ui: &AppWindow, said: &str) -> i_slint_backend_testing::ElementHandle {
        use i_slint_backend_testing::ElementRoot;
        let said = said.to_string();
        let shown = move |e: &i_slint_backend_testing::ElementHandle| [e.accessible_label(), e.accessible_value()].contains(&Some(said.as_str().into()));
        ui.root_element().query_descendants().match_predicate(shown).find_first().expect("the line, on the window")
    }

    /// A chat's line can be picked out with the mouse and copied with
    /// Ctrl+C, as text anywhere else can.
    #[test]
    fn a_chat_line_dragged_over_is_copied() {
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        let said = "meet at the bridge at six";
        let (ui, clipboard) = chat_window(said);
        let line = said_on(&ui, said);
        let (at, size) = (line.absolute_position(), line.size());
        let y = at.y + size.height / 2.0;
        let window = ui.window();
        window.dispatch_event(WindowEvent::PointerPressed { position: slint::LogicalPosition::new(at.x + 1.0, y), button: PointerEventButton::Left });
        window.dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(at.x + size.width - 1.0, y) });
        window.dispatch_event(WindowEvent::PointerReleased { position: slint::LogicalPosition::new(at.x + size.width - 1.0, y), button: PointerEventButton::Left });
        let control: slint::SharedString = Key::Control.into();
        window.dispatch_event(WindowEvent::KeyPressed { text: control.clone() });
        window.dispatch_event(WindowEvent::KeyPressed { text: "c".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: "c".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: control });
        assert_eq!(clipboard.borrow().as_deref(), Some(said));
    }

    /// Clicks along a line's text, a few pixels at a time from its
    /// start, until a link's opened: where that was, and what it opened.
    fn click_along_for_a_link(ui: &AppWindow, line: &i_slint_backend_testing::ElementHandle, at_y: Option<f32>) -> Option<(f32, String)> {
        let opened = std::rc::Rc::new(std::cell::RefCell::new(None));
        ui.global::<App>().on_open_link({ let opened = opened.clone(); move |url| *opened.borrow_mut() = Some(url.to_string()) });
        let (at, size) = (line.absolute_position(), line.size());
        let y = at_y.unwrap_or(at.y + size.height / 2.0);
        let mut x = at.x + 1.0;
        while x < at.x + size.width {
            click(ui.window(), slint::LogicalPosition::new(x, y));
            if let Some(url) = opened.borrow_mut().take() {
                return Some((x - at.x, url));
            }
            x += 3.0;
        }
        None
    }

    /// A web address in a chat line opens when it's clicked, and only
    /// that: the words before it don't.
    #[test]
    fn a_web_address_in_a_chat_line_opens_when_clicked() {
        let said = "the plan is at https://example.org/plan.";
        let (ui, _) = chat_window(said);
        let (x, url) = click_along_for_a_link(&ui, &said_on(&ui, said), None).expect("a link clicked");
        assert_eq!(url, "https://example.org/plan");
        assert!(x > 40.0, "{x}px in: the words before it opened it");
        let plain = "nothing to open here, example.org";
        let line = ChatLine { kind: LineKind::Said, time: "12:00".into(), name: "Ann".into(), role: 1, text: plain.into(), mark: "".into(), for_me: false, linked: false, rich: Default::default(), refs: "".into(), deleting: false, first: true };
        ui.global::<App>().set_lines(slint::ModelRc::new(slint::VecModel::from(vec![snapshot::with_links(line)])));
        redraw();
        assert_eq!(click_along_for_a_link(&ui, &said_on(&ui, plain), None), None);
    }

    /// A line with a web address in it is still picked out with the mouse
    /// and copied whole, the link and all.
    #[test]
    fn a_chat_line_with_a_link_dragged_over_is_copied() {
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        let said = "the plan is at https://example.org/plan, see you";
        let (ui, clipboard) = chat_window(said);
        let opened = std::rc::Rc::new(std::cell::Cell::new(false));
        ui.global::<App>().on_open_link({ let opened = opened.clone(); move |_| opened.set(true) });
        let line = said_on(&ui, said);
        let (at, size) = (line.absolute_position(), line.size());
        let y = at.y + size.height / 2.0;
        let window = ui.window();
        window.dispatch_event(WindowEvent::PointerPressed { position: slint::LogicalPosition::new(at.x + 1.0, y), button: PointerEventButton::Left });
        window.dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(at.x + size.width - 1.0, y) });
        window.dispatch_event(WindowEvent::PointerReleased { position: slint::LogicalPosition::new(at.x + size.width - 1.0, y), button: PointerEventButton::Left });
        let control: slint::SharedString = Key::Control.into();
        window.dispatch_event(WindowEvent::KeyPressed { text: control.clone() });
        window.dispatch_event(WindowEvent::KeyPressed { text: "c".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: "c".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: control });
        assert_eq!(clipboard.borrow().as_deref(), Some(said));
        assert!(!opened.get(), "dragging isn't clicking");
    }

    fn right_click(window: &slint::Window, at: slint::LogicalPosition) {
        use slint::platform::{PointerEventButton, WindowEvent};
        window.dispatch_event(WindowEvent::PointerPressed { position: at, button: PointerEventButton::Right });
        window.dispatch_event(WindowEvent::PointerReleased { position: at, button: PointerEventButton::Right });
    }

    /// A right-click on a line of a contact's chat offers Delete, which
    /// deletes it at once; a line that can't be deleted, or is being
    /// deleted already, offers nothing.
    #[test]
    fn a_chat_line_right_clicked_is_deleted() {
        let said = "I take that back";
        let (ui, _) = chat_window_with(ann_says(said, ">:-:5:00"));
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        ui.global::<App>().on_delete_line({ let deleted = deleted.clone(); move |refs| deleted.borrow_mut().push(refs.to_string()) });
        let at = middle(&said_on(&ui, said));
        right_click(ui.window(), at);
        redraw();
        let delete = labelled(&ui, "Delete").expect("Delete, offered");
        let (inside, size) = (delete.absolute_position(), delete.size());
        click(ui.window(), slint::LogicalPosition::new(at.x + inside.x + size.width / 2.0, at.y + inside.y + size.height / 2.0));
        redraw();
        assert_eq!(*deleted.borrow(), [">:-:5:00"]);
        assert!(labelled(&ui, "Delete").is_none(), "the menu's gone");

        for line in [ann_says(said, ""), ChatLine { deleting: true, ..ann_says(said, ">:-:5:00") }] {
            ui.global::<App>().set_lines(slint::ModelRc::new(slint::VecModel::from(vec![line])));
            redraw();
            right_click(ui.window(), middle(&said_on(&ui, said)));
            redraw();
            assert!(labelled(&ui, "Delete").is_none(), "nothing to offer");
        }
        assert_eq!(deleted.borrow().len(), 1);
    }

    fn says(name: &str, time: &str, text: &str) -> ChatLine {
        ChatLine { name: name.into(), time: time.into(), ..ann_says(text, "") }
    }

    fn day(text: &str) -> ChatLine {
        ChatLine { kind: LineKind::Day, time: "".into(), name: "".into(), text: text.into(), ..ann_says("", "") }
    }

    /// Lines copied whole: the day they're in heads them, each with its
    /// time and who, and picked either way round.
    #[test]
    fn chat_lines_are_copied_with_their_day_times_and_names() {
        let lines = [
            day("Tuesday 24 November 2026"),
            says("Harrison", "21:03", "evening"),
            says("Owen", "21:04", "hi, see https://example.org/x"),
            ChatLine { kind: LineKind::Did, name: "*".into(), ..says("", "21:05", "Owen waves") },
            day("Wednesday 25 November 2026"),
            ChatLine { kind: LineKind::Missed, name: "".into(), ..says("", "08:00", "missed call from Harrison") },
        ];
        assert_eq!(snapshot::chat_text(&lines, 2, 1), "--- Tuesday 24 November 2026 ---\n21:03 Harrison: evening\n21:04 Owen: hi, see https://example.org/x");
        assert_eq!(
            snapshot::chat_text(&lines, 3, 5),
            "--- Tuesday 24 November 2026 ---\n21:05 * Owen waves\n--- Wednesday 25 November 2026 ---\n08:00 missed call from Harrison"
        );
        assert_eq!(snapshot::chat_text(&lines, 4, 4), "--- Wednesday 25 November 2026 ---");
        assert_eq!(snapshot::chat_text(&lines, -1, 0), "");
        assert_eq!(snapshot::chat_text(&lines, 5, 99), "--- Wednesday 25 November 2026 ---\n08:00 missed call from Harrison");
    }

    /// A window with Ann's, Bob's (with a link) and Cat's lines under a
    /// day's heading.
    fn three_lines() -> (AppWindow, std::rc::Rc<std::cell::RefCell<Option<String>>>) {
        let (ui, clipboard) = chat_window("");
        let lines = vec![day("Tuesday 24 November 2026"), says("Ann", "21:03", "good evening all"), snapshot::with_links(says("Bob", "21:04", "see https://example.org/x for it")), says("Cat", "21:05", "night then")];
        ui.global::<App>().set_lines(slint::ModelRc::new(slint::VecModel::from(lines)));
        redraw();
        (ui, clipboard)
    }

    /// Drags from `from` to `to` with the left button, a step at a time,
    /// drawing between (as a screen would).
    fn drag(window: &slint::Window, from: slint::LogicalPosition, to: slint::LogicalPosition) {
        use slint::platform::{PointerEventButton, WindowEvent};
        window.dispatch_event(WindowEvent::PointerPressed { position: from, button: PointerEventButton::Left });
        redraw();
        for step in 1..=8 {
            let f = step as f32 / 8.0;
            window.dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(from.x + (to.x - from.x) * f, from.y + (to.y - from.y) * f) });
            redraw();
        }
        window.dispatch_event(WindowEvent::PointerReleased { position: to, button: PointerEventButton::Left });
        redraw();
    }

    fn ctrl_c(window: &slint::Window) {
        use slint::platform::{Key, WindowEvent};
        let control: slint::SharedString = Key::Control.into();
        window.dispatch_event(WindowEvent::KeyPressed { text: control.clone() });
        window.dispatch_event(WindowEvent::KeyPressed { text: "c".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: "c".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: control });
    }

    /// A point `chars` characters into `said`'s text (by its width, the
    /// text being near enough even).
    fn into_text(ui: &AppWindow, said: &str, chars: usize) -> slint::LogicalPosition {
        let line = said_on(ui, said);
        let (at, size) = (line.absolute_position(), line.size());
        let width = text_width(ui, said);
        slint::LogicalPosition::new(at.x + width * chars as f32 / said.chars().count() as f32, at.y + size.height / 2.0)
    }

    /// How wide `said` is drawn: where its last character is found.
    fn text_width(ui: &AppWindow, said: &str) -> f32 {
        let line = said_on(ui, said);
        let (at, size) = (line.absolute_position(), line.size());
        let y = at.y + size.height / 2.0;
        let mut x = at.x;
        while x < at.x + size.width && text_at(ui.window(), x, y).is_some_and(|(_, n)| n < said.len()) {
            x += 1.0;
        }
        x - at.x
    }

    /// Dragged from inside one message to inside another, as in a text
    /// editor: from the character pressed on to the one let go on, the
    /// messages alone, the one with a link in between whole.
    #[test]
    fn text_dragged_over_several_messages_is_copied_from_where_it_started() {
        let (ui, clipboard) = three_lines();
        let start = into_text(&ui, "good evening all", 5);
        let from = text_at(ui.window(), start.x, start.y).expect("text there").1;
        assert!(from > 2 && from < 8, "{from}");
        drag(ui.window(), start, into_text(&ui, "night then", 5));
        ctrl_c(ui.window());
        let got = clipboard.borrow().clone().expect("copied");
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 3, "{got:?}");
        assert_eq!(lines[0], &"good evening all"[from..]);
        assert_eq!(lines[1], "see https://example.org/x for it");
        assert!(lines[2].starts_with("ni") && "night then".starts_with(lines[2]) && lines[2].len() < 9, "{got:?}");
        *clipboard.borrow_mut() = None;
        drag(ui.window(), into_text(&ui, "night then", 5), start);
        ctrl_c(ui.window());
        assert_eq!(clipboard.borrow().as_deref(), Some(got.as_str()));
    }

    /// Dragged from the names, whole lines: their day, times and names.
    #[test]
    fn lines_dragged_over_by_their_names_are_copied_whole() {
        let (ui, clipboard) = three_lines();
        drag(ui.window(), middle(&labelled(&ui, "Ann").expect("Ann")), middle(&labelled(&ui, "Bob").expect("Bob")));
        ctrl_c(ui.window());
        assert_eq!(clipboard.borrow().as_deref(), Some("--- Tuesday 24 November 2026 ---\n21:03 Ann: good evening all\n21:04 Bob: see https://example.org/x for it"));
        *clipboard.borrow_mut() = None;
        click(ui.window(), into_text(&ui, "night then", 2));
        redraw();
        ctrl_c(ui.window());
        assert_eq!(*clipboard.borrow(), None);
    }

    /// A double click selects the word, a triple the whole message.
    #[test]
    fn a_double_click_selects_a_word_and_a_triple_the_message() {
        let (ui, clipboard) = three_lines();
        let at = into_text(&ui, "good evening all", 7);
        click(ui.window(), at);
        click(ui.window(), at);
        redraw();
        ctrl_c(ui.window());
        assert_eq!(clipboard.borrow().as_deref(), Some("evening"));
        click(ui.window(), at);
        redraw();
        ctrl_c(ui.window());
        assert_eq!(clipboard.borrow().as_deref(), Some("good evening all"));
    }

    #[test]
    fn a_word_is_a_run_of_letters_spaces_or_marks() {
        let text = "it's ok... see https://x.org";
        let word = |at: usize| { let (a, b) = snapshot::span_around(text, at, 2); &text[a..b] };
        assert_eq!(word(1), "it's");
        assert_eq!(word(7), "...");
        assert_eq!(word(text.len()), "org");
        assert_eq!(snapshot::span_around(text, 3, 3), (0, text.len()));
        assert_eq!(snapshot::span_around(text, 3, 1), (3, 3));
        let lines = [day("Tuesday"), says("Ann", "21:03", "h\u{e9}llo there"), says("Bob", "21:04", "bye")];
        assert_eq!(snapshot::chat_selection(&lines, 1, 2, 2, 2), "llo there\nby", "from the middle of a character, the next whole one");
        assert_eq!(snapshot::chat_selection(&lines, 0, 0, 1, 1), "h");
    }

    /// A right-click on any line offers Copy (its text, a link and all)
    /// and Copy with time and name, though it can't be deleted.
    #[test]
    fn a_chat_line_right_clicked_is_copied() {
        let said = "the plan is at https://example.org/plan, see you";
        let (ui, clipboard) = chat_window(said);
        for (item, copied) in [("Copy", said.to_string()), ("Copy with time and name", format!("12:00 Ann: {said}"))] {
            let at = middle(&said_on(&ui, said));
            right_click(ui.window(), at);
            redraw();
            assert!(labelled(&ui, "Delete").is_none(), "not one to delete");
            let copy = labelled(&ui, item).expect("offered");
            let (inside, size) = (copy.absolute_position(), copy.size());
            click(ui.window(), slint::LogicalPosition::new(at.x + inside.x + size.width / 2.0, at.y + inside.y + size.height / 2.0));
            redraw();
            assert_eq!(clipboard.borrow().as_deref(), Some(copied.as_str()));
        }
    }

    /// A window on the user's own posts, with `post` the one for anyone.
    fn posts_window(text: &str, refs: &str) -> (AppWindow, std::rc::Rc<std::cell::RefCell<Option<String>>>) {
        let (ui, clipboard) = chat_window("");
        let app = ui.global::<App>();
        app.set_view(Pane::Mine);
        app.set_my_public(slint::ModelRc::new(slint::VecModel::from(vec![PostItem { refs: refs.into(), ..snapshot::post_item("1 Nov 12:00".into(), text) }])));
        redraw();
        (ui, clipboard)
    }

    /// A post is picked out with the mouse and copied, as a chat's line
    /// is, and a web address in it opens when clicked.
    #[test]
    fn a_post_is_dragged_over_to_copy_and_its_link_opens() {
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        let said = "slides are up at https://example.org/talk, enjoy";
        let (ui, clipboard) = posts_window(said, "");
        let line = said_on(&ui, said);
        let (_, url) = click_along_for_a_link(&ui, &line, None).expect("a link");
        assert_eq!(url, "https://example.org/talk");
        let (at, size) = (line.absolute_position(), line.size());
        let y = at.y + size.height / 2.0;
        let window = ui.window();
        window.dispatch_event(WindowEvent::PointerPressed { position: slint::LogicalPosition::new(at.x + 1.0, y), button: PointerEventButton::Left });
        window.dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(at.x + size.width - 1.0, y) });
        window.dispatch_event(WindowEvent::PointerReleased { position: slint::LogicalPosition::new(at.x + size.width - 1.0, y), button: PointerEventButton::Left });
        let control: slint::SharedString = Key::Control.into();
        window.dispatch_event(WindowEvent::KeyPressed { text: control.clone() });
        window.dispatch_event(WindowEvent::KeyPressed { text: "c".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: "c".into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: control });
        assert_eq!(clipboard.borrow().as_deref(), Some(said));
    }

    /// One of the user's own posts right-clicked offers Delete, which
    /// deletes it; someone else's offers nothing.
    #[test]
    fn ones_own_post_right_clicked_is_deleted() {
        let said = "posted in haste";
        let (ui, _) = posts_window(said, "c2lnbmF0dXJl");
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        ui.global::<App>().on_delete_post({ let deleted = deleted.clone(); move |refs| deleted.borrow_mut().push(refs.to_string()) });
        let at = middle(&said_on(&ui, said));
        right_click(ui.window(), at);
        redraw();
        let delete = labelled(&ui, "Delete").expect("Delete, offered");
        let (inside, size) = (delete.absolute_position(), delete.size());
        click(ui.window(), slint::LogicalPosition::new(at.x + inside.x + size.width / 2.0, at.y + inside.y + size.height / 2.0));
        redraw();
        assert_eq!(*deleted.borrow(), ["c2lnbmF0dXJl"]);

        let app = ui.global::<App>();
        app.set_my_public(slint::ModelRc::new(slint::VecModel::from(vec![snapshot::post_item("1 Nov 12:00".into(), said)])));
        redraw();
        right_click(ui.window(), middle(&said_on(&ui, said)));
        redraw();
        assert!(labelled(&ui, "Delete").is_none(), "not one's own: nothing to offer");
    }

    /// A right-click on a link in a line offers the line's Delete too.
    #[test]
    fn a_link_right_clicked_offers_its_lines_delete() {
        let said = "see https://example.org/plan";
        let (ui, _) = chat_window_with(ann_says(said, ">:-:5:00"));
        let line = said_on(&ui, said);
        let (x, _) = click_along_for_a_link(&ui, &line, None).expect("a link");
        right_click(ui.window(), slint::LogicalPosition::new(line.absolute_position().x + x, middle(&line).y));
        redraw();
        assert!(labelled(&ui, "Delete").is_some());
    }

    fn click(window: &slint::Window, at: slint::LogicalPosition) {
        use slint::platform::{PointerEventButton, WindowEvent};
        window.dispatch_event(WindowEvent::PointerPressed { position: at, button: PointerEventButton::Left });
        window.dispatch_event(WindowEvent::PointerReleased { position: at, button: PointerEventButton::Left });
    }

    fn middle(e: &i_slint_backend_testing::ElementHandle) -> slint::LogicalPosition {
        let (at, size) = (e.absolute_position(), e.size());
        slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0)
    }

    /// Clicking a chat's line (to copy from it) leaves the window's keys
    /// working: Up and Down still move along the list, and the letters
    /// still do what they do.
    #[test]
    fn the_keys_still_work_after_clicking_a_chat_line() {
        let said = "meet at the bridge at six";
        let (ui, _) = chat_window(said);
        let moved = std::rc::Rc::new(std::cell::Cell::new(0));
        let refreshed = std::rc::Rc::new(std::cell::Cell::new(false));
        let app = ui.global::<App>();
        app.on_move({ let moved = moved.clone(); move |by| moved.set(moved.get() + by) });
        app.on_refresh({ let refreshed = refreshed.clone(); move || refreshed.set(true) });
        click(ui.window(), middle(&said_on(&ui, said)));
        for key in [slint::platform::Key::DownArrow.into(), slint::platform::Key::DownArrow.into(), slint::SharedString::from("r")] {
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: key.clone() });
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: key });
        }
        assert_eq!(moved.get(), 2);
        assert!(refreshed.get());
    }

    /// On the phone, a heading's tapped where it is: it's opened or
    /// closed, and the list stays showing; a row under it opens its chat.
    #[test]
    fn a_heading_on_the_phone_opens_or_closes_in_the_list() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::default() })).unwrap();
        let ui = PhoneWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(412, 915));
        ui.show().unwrap();
        let draw = || {
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| { renderer.render(&mut vec![Rgb565Pixel::default(); 412 * 915], 412); });
        };
        let app = ui.global::<App>();
        let heading = Row { kind: RowKind::Heading, mark: "▾".into(), name: "CONTACTS".into(), ..Default::default() };
        let ann = Row { kind: RowKind::Peer, name: "Ann".into(), letter: "A".into(), ..Default::default() };
        app.set_rows(slint::ModelRc::new(slint::VecModel::from(vec![heading, ann])));
        let chosen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        app.on_select({
            let chosen = chosen.clone();
            move |i| chosen.borrow_mut().push(i)
        });
        draw();
        let find = |label: &'static str| {
            use i_slint_backend_testing::ElementRoot;
            ui.root_element().query_descendants().match_predicate(move |e| e.accessible_label().as_deref() == Some(label)).find_first().expect("listed")
        };
        click(ui.window(), middle(&find("CONTACTS")));
        draw();
        assert_eq!(*chosen.borrow(), vec![0]);
        assert!(!ui.get_open(), "the list stays");
        click(ui.window(), middle(&find("Ann")));
        draw();
        assert_eq!(*chosen.borrow(), vec![0, 1]);
        assert!(ui.get_open());
    }

    /// On the phone, Help is a tab: its contents fill the screen, a
    /// chapter tapped opens over them, and Back goes back to them.
    #[test]
    fn the_phones_help_tab_opens_a_page_and_back_returns() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::default() })).unwrap();
        let ui = PhoneWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(412, 915));
        ui.show().unwrap();
        let draw = || {
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| { renderer.render(&mut vec![Rgb565Pixel::default(); 412 * 915], 412); });
        };
        help::wire(&ui);
        let app = ui.global::<App>();
        app.set_view(Pane::Help);
        draw();
        let find = |label: &'static str| {
            use i_slint_backend_testing::ElementRoot;
            ui.root_element().query_descendants().match_predicate(move |e| e.accessible_label().as_deref() == Some(label)).find_first()
        };
        assert!(find("Help").is_some(), "a tab");
        click(ui.window(), middle(&find("Getting started").expect("listed")));
        draw();
        assert!(ui.get_open());
        assert_eq!(app.get_help_title(), "2 Getting started");
        assert!(find("2.1 Initial use").is_some(), "its page");
        press(ui.window(), &[slint::platform::Key::Escape.into()]);
        draw();
        assert!(!ui.get_open(), "back to the contents");
        assert!(find("Initial use").is_some(), "its sections listed under it");
    }

    /// On the phone, a long press on a chat's line copies all of it.
    #[test]
    fn a_long_press_on_the_phone_copies_a_chat_line() {
        let said = "meet at the bridge at six";
        assert_eq!(long_pressed_on_the_phone(said, middle), said);
    }

    /// On the phone, a long press on a line with a link in it, away from
    /// the link, still copies all of it; a tap on the link opens it.
    #[test]
    fn on_the_phone_a_link_opens_and_its_line_still_copies() {
        let said = "plan: https://example.org/p";
        assert_eq!(long_pressed_on_the_phone(said, |line| slint::LogicalPosition::new(line.absolute_position().x + line.size().width - 3.0, middle(line).y)), said);
    }

    /// The phone, on a chat with `line` in it: the window, what it draws
    /// on, and what it copies.
    #[allow(clippy::type_complexity)]
    fn phone_chat(line: ChatLine) -> (PhoneWindow, std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>, std::rc::Rc<std::cell::RefCell<Option<String>>>) {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        let clipboard = std::rc::Rc::default();
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::clone(&clipboard) })).unwrap();
        let ui = PhoneWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(412, 915));
        ui.show().unwrap();
        let app = ui.global::<App>();
        app.set_view(Pane::Peer);
        let ann = Row { kind: RowKind::You, name: "Ann".into(), ..Default::default() };
        app.set_rows(slint::ModelRc::new(slint::VecModel::from(vec![ann])));
        app.set_lines(slint::ModelRc::new(slint::VecModel::from(vec![snapshot::with_links(line)])));
        draw_phone(&window);
        {
            use i_slint_backend_testing::ElementRoot;
            let row = ui.root_element().query_descendants().match_predicate(|e| e.accessible_label().as_deref() == Some("Ann")).find_first().expect("Ann, listed");
            click(ui.window(), middle(&row));
        }
        draw_phone(&window);
        (ui, window, clipboard)
    }

    fn draw_phone(window: &slint::platform::software_renderer::MinimalSoftwareWindow) {
        slint::platform::update_timers_and_animations();
        window.draw_if_needed(|renderer| { renderer.render(&mut vec![slint::platform::software_renderer::Rgb565Pixel::default(); 412 * 915], 412); });
    }

    /// Holds a finger on the phone at `at` long enough to be a long press.
    fn long_press(ui: &PhoneWindow, window: &slint::platform::software_renderer::MinimalSoftwareWindow, at: slint::LogicalPosition) {
        use slint::platform::{PointerEventButton, WindowEvent};
        ui.window().dispatch_event(WindowEvent::PointerPressed { position: at, button: PointerEventButton::Left });
        let pressed = std::time::Instant::now();
        while pressed.elapsed() < Duration::from_millis(700) {
            std::thread::sleep(Duration::from_millis(20));
            draw_phone(window);
        }
        ui.window().dispatch_event(WindowEvent::PointerReleased { position: at, button: PointerEventButton::Left });
        draw_phone(window);
    }

    /// On the phone, a long press on a line that can be deleted asks Copy
    /// or Delete, and each does what it says.
    #[test]
    fn on_the_phone_a_long_press_copies_or_deletes() {
        let said = "I take that back";
        let (ui, window, clipboard) = phone_chat(ann_says(said, ">:-:5:00"));
        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        ui.global::<App>().on_delete_line({ let deleted = deleted.clone(); move |refs| deleted.borrow_mut().push(refs.to_string()) });
        let at = middle(&said_on_phone(&ui, said));
        let choose = |what: &str| {
            long_press(&ui, &window, at);
            let item = labelled(&ui, what).unwrap_or_else(|| panic!("{what}, offered"));
            let (inside, size) = (item.absolute_position(), item.size());
            click(ui.window(), slint::LogicalPosition::new(at.x + inside.x + size.width / 2.0, at.y + inside.y + size.height / 2.0));
            draw_phone(&window);
        };
        choose("Copy");
        assert_eq!(clipboard.borrow().as_deref(), Some(said));
        assert!(deleted.borrow().is_empty());
        choose("Delete");
        assert_eq!(*deleted.borrow(), [">:-:5:00"]);
    }

    /// On the phone, a long press on a post copies it; on one of the
    /// user's own, it asks Copy or Delete.
    #[test]
    fn on_the_phone_a_post_long_pressed_copies_or_deletes_ones_own() {
        let said = "a post from the phone";
        let (ui, window, clipboard) = phone_chat(ann_says("", ""));
        let app = ui.global::<App>();
        let show = |refs: &str| {
            app.set_view(Pane::Mine);
            app.set_my_public(slint::ModelRc::new(slint::VecModel::from(vec![PostItem { refs: refs.into(), ..snapshot::post_item("1 Nov 12:00".into(), said) }])));
            draw_phone(&window);
        };
        show("");
        let at = middle(&said_on_phone(&ui, said));
        long_press(&ui, &window, at);
        assert_eq!(clipboard.borrow().as_deref(), Some(said));
        assert!(labelled(&ui, "Delete").is_none(), "not one to delete");

        let deleted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        app.on_delete_post({ let deleted = deleted.clone(); move |refs| deleted.borrow_mut().push(refs.to_string()) });
        show("c2lnbmF0dXJl");
        long_press(&ui, &window, at);
        let item = labelled(&ui, "Delete").expect("Delete, offered");
        let (inside, size) = (item.absolute_position(), item.size());
        click(ui.window(), slint::LogicalPosition::new(at.x + inside.x + size.width / 2.0, at.y + inside.y + size.height / 2.0));
        draw_phone(&window);
        assert_eq!(*deleted.borrow(), ["c2lnbmF0dXJl"]);
    }

    /// A long press on the phone at `at` on a chat line saying `said`: what
    /// it copied; then a tap along it must find its link and open it.
    fn long_pressed_on_the_phone(said: &str, at: impl Fn(&i_slint_backend_testing::ElementHandle) -> slint::LogicalPosition) -> String {
        let line = ann_says(said, "");
        let linked = snapshot::with_links(line.clone()).linked;
        let (ui, window, clipboard) = phone_chat(line);
        let app = ui.global::<App>();
        let line = said_on_phone(&ui, said);
        let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        app.on_open_link({ let opened = opened.clone(); move |url| opened.borrow_mut().push(url.to_string()) });
        let at = at(&line);
        long_press(&ui, &window, at);
        assert!(opened.borrow().is_empty(), "a long press away from a link opened {:?}", opened.borrow());
        if linked {
            let (start, size) = (line.absolute_position(), line.size());
            let mut x = start.x + 1.0;
            while opened.borrow().is_empty() && x < start.x + size.width {
                click(ui.window(), slint::LogicalPosition::new(x, at.y));
                x += 3.0;
            }
            assert_eq!(*opened.borrow(), ["https://example.org/p"], "a tap on the link opens it");
        }
        let copied = clipboard.borrow().clone().unwrap_or_default();
        copied
    }

    fn said_on_phone(ui: &PhoneWindow, said: &str) -> i_slint_backend_testing::ElementHandle {
        use i_slint_backend_testing::ElementRoot;
        let said = said.to_string();
        ui.root_element().query_descendants().match_predicate(move |e| e.accessible_label().as_deref() == Some(said.as_str())).find_first().expect("the line, on the phone")
    }

    fn press(window: &slint::Window, keys: &[slint::SharedString]) {
        for key in keys {
            window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: key.clone() });
            window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: key.clone() });
        }
    }

    fn typed(window: &slint::Window, text: &str) {
        let keys: Vec<slint::SharedString> = text.chars().map(|c| c.to_string().into()).collect();
        press(window, &keys);
    }

    /// Tab moves on to a form's next field, and Shift+Tab back.
    #[test]
    fn tab_moves_between_a_forms_fields() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::Key;
        let (ui, _) = chat_window("");
        ui.global::<App>().set_view(Pane::Peers);
        typed(ui.window(), "a");
        let name = ElementHandle::find_by_element_id(&ui, "Labelled::edit").nth(1).expect("the form's name, after their code");
        click(ui.window(), middle(&name));
        typed(ui.window(), "Ann");
        press(ui.window(), &[slint::platform::Key::Tab.into()]);
        typed(ui.window(), "10.0.0.1");
        press(ui.window(), &[slint::platform::Key::Tab.into()]);
        typed(ui.window(), "key");
        press(ui.window(), &[Key::Backtab.into(), Key::End.into()]);
        typed(ui.window(), ":4000");
        assert_eq!(ui.get_form_filled(), "Ann|10.0.0.1:4000|key");
    }

    /// Add a contact has a field for their code, which fills the rest in
    /// as it's pasted and says when it isn't one; Edit takes only the
    /// addresses from one, and only one of that contact's.
    #[test]
    fn a_pasted_code_fills_in_add_and_edit() {
        use base64::Engine as _;
        use i_slint_backend_testing::ElementHandle;
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        app.set_view(Pane::Peers);
        typed(ui.window(), "a");
        redraw();
        let code = ElementHandle::find_by_element_id(&ui, "Labelled::edit").next().expect("their code, first");
        assert!(labelled(&ui, "Their code (paste a swarm: code)").is_some());
        click(ui.window(), middle(&code));
        typed(ui.window(), "203.0.113.7");
        redraw();
        assert!(labelled(&ui, "That isn't a swarm: code. A code starts swarm: and is one line.").is_some());
        code.set_accessible_value("");
        let line = swarm_app::contact::line(&[9; 32], &[], &["203.0.113.7".parse().unwrap()], "Ann");
        code.set_accessible_value(&line);
        redraw();
        assert_eq!(ui.get_form_filled(), format!("Ann|203.0.113.7|{}", base64::engine::general_purpose::STANDARD.encode([9u8; 32])));
        assert!(labelled(&ui, "Filled in from it: check, then Save.").is_some());
        press(ui.window(), &[slint::platform::Key::Escape.into()]);

        let bob = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        app.set_pd(crate::PeerDetails { shown: true, name: "Bobby".into(), address: "198.51.100.1".into(), key: bob.as_str().into(), ..app.get_pd() });
        redraw();
        click(ui.window(), middle(&labelled(&ui, "Edit…").expect("Edit…")));
        redraw();
        let code = ElementHandle::find_by_element_id(&ui, "Labelled::edit").next().expect("their new code, first");
        code.set_accessible_value(&line);
        redraw();
        assert!(labelled(&ui, "That code is someone else's: its key isn't this contact's. Nothing was changed.").is_some());
        assert_eq!(ui.get_form_filled(), format!("Bobby|198.51.100.1|{bob}"));
        code.set_accessible_value(swarm_app::contact::line(&[7; 32], &[], &["203.0.113.9".parse().unwrap()], "Bob"));
        redraw();
        assert_eq!(ui.get_form_filled(), format!("Bobby|203.0.113.9|{bob}"), "his name for him kept");
    }

    /// Whether something with this label (a button's text, say) is on the window.
    fn labelled(ui: &impl i_slint_backend_testing::ElementRoot, label: &str) -> Option<i_slint_backend_testing::ElementHandle> {
        let label = label.to_string();
        ui.root_element().query_descendants().match_predicate(move |e| e.accessible_label().as_deref() == Some(label.as_str())).find_first()
    }

    /// Every element labelled `label`, top to bottom.
    fn all_labelled(ui: &impl i_slint_backend_testing::ElementRoot, label: &str) -> Vec<i_slint_backend_testing::ElementHandle> {
        let label = label.to_string();
        let mut all = ui.root_element().query_descendants().match_predicate(move |e| e.accessible_label().as_deref() == Some(label.as_str())).find_all();
        all.sort_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y));
        all
    }

    /// A picture, as the window has one: a square of one colour.
    fn a_picture() -> slint::Image {
        let side = swarm_app::avatar::SHOWN_SIDE as u32;
        slint::Image::from_rgb8(slint::SharedPixelBuffer::<slint::Rgb8Pixel>::clone_from_slice(&vec![200u8; (side * side * 3) as usize], side, side))
    }

    /// Your status, under You in Contacts: typed and set (Return or
    /// Set), and cleared.
    #[test]
    fn a_status_typed_under_you_is_set_and_cleared() {
        use i_slint_backend_testing::ElementHandle;
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let said = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        app.on_set_says({ let said = said.clone(); move |s| said.borrow_mut().push(s.to_string()) });
        app.set_view(Pane::Peers);
        app.set_pd(crate::PeerDetails { me: true, ..Default::default() });
        redraw();
        assert!(labelled(&ui, "Your name").is_some() && labelled(&ui, "Your picture").is_some());
        let edit = ElementHandle::find_by_element_id(&ui, "YouEditor::says-edit").next().expect("the status field");
        click(ui.window(), middle(&edit));
        typed(ui.window(), "out walking");
        press(ui.window(), &[slint::platform::Key::Return.into()]);
        assert_eq!(*said.borrow(), ["out walking"]);
        assert!(labelled(&ui, "Clear").is_none(), "nothing to clear while there's no status");
        app.set_me(crate::YouShown { says: "out walking".into(), ..app.get_me() });
        redraw();
        click(ui.window(), middle(&labelled(&ui, "Clear").expect("Clear, once there's a status")));
        assert_eq!(*said.borrow(), ["out walking", ""]);
    }

    /// Taking your picture, under You in Contacts: the camera's shown as
    /// the square kept, with Take it and Cancel; and in Settings, not as
    /// the Video section's preview (which would scroll the page down to
    /// it).
    #[test]
    fn taking_a_picture_shows_the_square_kept_not_the_preview() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<AvatarStep>::new()));
        app.on_avatar({ let asked = asked.clone(); move |what| asked.borrow_mut().push(what) });
        ui.window().set_size(slint::PhysicalSize::new(1280, 3000));
        app.set_view(Pane::Peers);
        app.set_pd(crate::PeerDetails { me: true, ..Default::default() });
        app.set_preview_info("the camera, 1280×720".into());
        redraw();
        assert!(labelled(&ui, "Take one").is_some());
        app.set_view(Pane::Settings);
        redraw();
        assert!(labelled(&ui, "Take one").is_none(), "not in Settings any more");
        app.set_view(Pane::Peers);
        redraw();
        click(ui.window(), middle(&labelled(&ui, "Take one").expect("Take one")));
        assert_eq!(*asked.borrow(), [AvatarStep::OpenCamera]);
        app.set_preview_shown(true);
        app.set_me(crate::YouShown { avatar_taking: true, ..app.get_me() });
        redraw();
        assert!(labelled(&ui, "From a file…").is_none());
        click(ui.window(), middle(&labelled(&ui, "Take it").expect("Take it")));
        click(ui.window(), middle(&labelled(&ui, "Cancel").expect("Cancel")));
        assert_eq!(*asked.borrow(), [AvatarStep::OpenCamera, AvatarStep::Take, AvatarStep::Cancel]);
        app.set_view(Pane::Settings);
        redraw();
        assert!(labelled(&ui, "Close preview").is_some(), "the Video section's drawn, up to date");
        assert!(labelled(&ui, "the camera, 1280×720").is_none(), "the Video section's preview isn't open for it");
        app.set_me(crate::YouShown { avatar_taking: false, ..app.get_me() });
        redraw();
        assert!(labelled(&ui, "the camera, 1280×720").is_some());
    }

    /// In a call with your camera off, your picture's where your video
    /// would be, and theirs where theirs would be, in place of "camera
    /// off"; without one, it still says so.
    #[test]
    fn a_picture_stands_in_for_a_camera_thats_off() {
        use i_slint_backend_testing::ElementHandle;
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        app.set_call(crate::CallShown { video_shown: true, local_on: false, remote_note: "Ann's camera is off".into(), ..app.get_call() });
        redraw();
        let avatars = || ElementHandle::find_by_element_type_name(&ui, "Avatar").count();
        assert!(labelled(&ui, "camera off").is_some());
        assert_eq!(avatars(), 0);
        app.set_my_avatar(a_picture());
        app.set_me(crate::YouShown { pictured: true, ..app.get_me() });
        redraw();
        assert!(labelled(&ui, "camera off").is_none(), "your picture in its place");
        assert_eq!(avatars(), 1);
        app.set_remote_avatar(a_picture());
        app.set_call(crate::CallShown { remote_pictured: true, ..app.get_call() });
        redraw();
        assert_eq!(avatars(), 2, "theirs too");
        assert!(labelled(&ui, "Ann's camera is off").is_some(), "under it, why");
    }

    /// Someone asking to control our screen: asked in a dialog, Allow (or
    /// a) allows it; while they control it, Take back control at the
    /// top takes it back.
    #[test]
    fn control_of_your_screen_is_asked_and_taken_back() {
        use slint::platform::Key;
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let answers = std::rc::Rc::new(std::cell::RefCell::new(Vec::<bool>::new()));
        let stops = std::rc::Rc::new(std::cell::Cell::new(0));
        app.on_answer_control({ let answers = answers.clone(); move |allow| answers.borrow_mut().push(allow) });
        app.on_stop_control({ let stops = stops.clone(); move || stops.set(stops.get() + 1) });
        app.set_call(crate::CallShown { video_shown: true, sharing: true, control_asking: "Ann".into(), ..app.get_call() });
        redraw();
        assert!(labelled(&ui, "Ann asks to control your screen: your mouse and keyboard, as if they were sitting at it.").is_some());
        click(ui.window(), middle(&labelled(&ui, "Allow").expect("Allow")));
        typed(ui.window(), "r");
        assert_eq!(*answers.borrow(), [true, false]);
        app.set_call(crate::CallShown { control_asking: "".into(), controlled_by: "Ann".into(), ..app.get_call() });
        redraw();
        assert!(labelled(&ui, "Ann is controlling your screen").is_some(), "said at the top");
        click(ui.window(), middle(&labelled(&ui, "Take back control").expect("Take back control")));
        assert_eq!(stops.get(), 1);
        press(ui.window(), &[Key::Control.into(), Key::Alt.into(), Key::Escape.into()]);
        assert_eq!(stops.get(), 1, "no hotkey: only the button");
    }

    /// Controlling their screen, in the video-only view: a click is sent
    /// as the point of their picture it's on (the picture fitted in the
    /// window, bars either side), and every key is theirs, pressed and
    /// let go: h doesn't hang up, Esc doesn't leave the view.
    #[test]
    fn controlling_their_screen_sends_the_mouse_and_every_key() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let points = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(f32, f32)>::new()));
        let buttons = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(i32, bool)>::new()));
        let keys = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(String, bool)>::new()));
        let hung_up = std::rc::Rc::new(std::cell::Cell::new(false));
        app.on_control_pointer({ let points = points.clone(); move |x, y| points.borrow_mut().push((x, y)) });
        app.on_control_button({ let buttons = buttons.clone(); move |b, down| buttons.borrow_mut().push((b, down)) });
        app.on_control_key({ let keys = keys.clone(); move |k, down| keys.borrow_mut().push((k.to_string(), down)) });
        app.on_hang_up({ let hung_up = hung_up.clone(); move || hung_up.set(true) });
        app.set_call(crate::CallShown { video_shown: true, can_control: true, ..app.get_call() });
        app.set_remote_frame(slint::Image::from_rgb8(slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(160, 90)));
        typed(ui.window(), "v");
        redraw();
        assert!(labelled(&ui, "Control").is_some(), "offered full screen");
        assert!(labelled(&ui, "Full screen").is_some());
        app.set_call(crate::CallShown { control_state: Controlling::Yes, controlling: "Ann".into(), ..app.get_call() });
        redraw();
        assert!(labelled(&ui, "controlling Ann's screen").is_some());
        for button in ["Control", "Microphone", "Camera", "Share screen", "Full screen", "End call"] {
            assert!(labelled(&ui, button).is_some(), "{button} there while controlling");
        }
        assert!(labelled(&ui, "Preview").is_none(), "only those that matter");
        click(ui.window(), slint::LogicalPosition::new(640.0, 50.0 + 180.0));
        let (x, y) = *points.borrow().last().expect("the point sent");
        assert!((x - 0.5).abs() < 0.01 && (y - 0.25).abs() < 0.01, "{x} {y}");
        assert_eq!(*buttons.borrow(), [(0, true), (0, false)]);
        typed(ui.window(), "h");
        press(ui.window(), &[slint::platform::Key::Escape.into()]);
        assert!(!hung_up.get());
        assert_eq!(*keys.borrow(), [("h".to_string(), true), ("h".to_string(), false), ("\u{1b}".to_string(), true), ("\u{1b}".to_string(), false)]);
        assert!(labelled(&ui, "controlling Ann's screen").is_some(), "still in the view");
        let presses = buttons.borrow().len();
        let hang_up = i_slint_backend_testing::ElementRoot::root_element(&ui).query_descendants().match_predicate(|e| e.accessible_label().as_deref() == Some("End call")).find_all().into_iter().find(|e| e.absolute_position().y < 50.0);
        click(ui.window(), middle(&hang_up.expect("End call at the top")));
        assert!(hung_up.get(), "hung up from the buttons while controlling");
        assert_eq!(buttons.borrow().len(), presses, "the click not sent to them");
    }

    /// Full screen has only the call's buttons, no line of keys under
    /// them: the mouse over one shows its key.
    #[test]
    fn full_screen_shows_a_buttons_key_to_the_mouse() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        app.set_call(crate::CallShown { live: true, video_shown: true, ..app.get_call() });
        app.set_remote_frame(slint::Image::from_rgb8(slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(160, 90)));
        typed(ui.window(), "v");
        redraw();
        assert!(labelled(&ui, "Preview").is_some(), "full screen");
        let texts: Vec<String> = i_slint_backend_testing::ElementRoot::root_element(&ui).query_descendants().find_all().into_iter().filter_map(|e| e.accessible_label().map(|l| l.to_string())).collect();
        assert!(!texts.iter().any(|t| t.contains("m microphone") || t.contains("Esc leave")), "no keys under the buttons: {texts:?}");
        let lowest = |label: &str| i_slint_backend_testing::ElementRoot::root_element(&ui).query_descendants().match_predicate({ let label = label.to_string(); move |e| e.accessible_label().as_deref() == Some(label.as_str()) }).find_all().into_iter().max_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y)).unwrap_or_else(|| panic!("{label}"));
        for (button, key) in [("Microphone", "m"), ("Camera", "w"), ("Share screen", "e"), ("Preview", "s"), ("Full screen", "Esc"), ("End call", "h")] {
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position: middle(&lowest(button)) });
            redraw();
            assert_eq!(ui.get_tip(), key, "{button}");
        }
        app.set_call(crate::CallShown { live: false, video_shown: false, ..app.get_call() });
        redraw();
        assert_eq!(ui.get_tip(), "", "no key left over the window");
    }

    /// In a group, a contact among its members opens their chat with a
    /// click (someone else is asked to add you); Add people… asks for someone by
    /// name, and Enter adds the first that matches. In a contact's chat,
    /// Add people… starts a group with them and whoever's chosen.
    #[test]
    fn a_group_adds_people_by_name_and_opens_a_members_chat() {
        use std::{cell::RefCell, rc::Rc};
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let picked = Rc::new(RefCell::new(Vec::<(i32, String)>::new()));
        let sent = Rc::new(RefCell::new(Vec::<String>::new()));
        let opened = Rc::new(RefCell::new(Vec::<String>::new()));
        let grouped = Rc::new(RefCell::new(Vec::<String>::new()));
        app.on_pick({ let picked = picked.clone(); move |kind, typed| picked.borrow_mut().push((kind, typed.to_string())) });
        app.on_send({ let sent = sent.clone(); move |text| sent.borrow_mut().push(text.to_string()) });
        app.on_open_chat({ let opened = opened.clone(); move |name| opened.borrow_mut().push(name.to_string()) });
        app.on_group_with({ let grouped = grouped.clone(); move |name| grouped.borrow_mut().push(name.to_string()) });
        app.set_view(Pane::Channel);
        app.set_channel(crate::ChannelShown { name: "Ann, zed".into(), detail: "3 people".into() });
        let member = |name: &str, contact: &str| crate::Member { name: name.into(), contact: contact.into(), summary: "online since 09:00\nin use: fd99::2".into(), connected: true, ..Default::default() };
        app.set_members(Rc::new(slint::VecModel::from(vec![member("Ann", "Ann"), member("zed", "")])).into());
        redraw();
        let member_named = |name: &str| {
            let name = name.to_string();
            i_slint_backend_testing::ElementRoot::root_element(&ui).query_descendants().match_predicate(move |e| e.accessible_label().as_deref() == Some(name.as_str())).find_all().into_iter().max_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x)).expect("a member")
        };
        click(ui.window(), middle(&member_named("zed")));
        click(ui.window(), middle(&member_named("Ann")));
        assert_eq!(*opened.borrow(), ["Ann"]);
        assert_eq!(*sent.borrow(), ["/ask zed"], "zed isn't a contact: asked to add you");
        click(ui.window(), middle(&labelled(&ui, "Add people…").expect("Add people…")));
        redraw();
        assert!(labelled(&ui, "Add to Ann, zed").is_some());
        app.set_picks(Rc::new(slint::VecModel::from(vec![slint::SharedString::from("Annie")])).into());
        typed(ui.window(), "An");
        press(ui.window(), &[slint::platform::Key::Return.into()]);
        redraw();
        assert_eq!(*picked.borrow(), [(2, String::new()), (2, "A".to_string()), (2, "An".to_string()), (0, String::new())]);
        assert_eq!(*sent.borrow(), ["/ask zed", "/add Annie"]);
        assert!(labelled(&ui, "Add to Ann, zed").is_none(), "closed");

        app.set_view(Pane::Peer);
        app.set_peer(crate::PeerShown { name: "Bob".into(), state: crate::PeerState::Online, ..Default::default() });
        redraw();
        click(ui.window(), middle(&labelled(&ui, "Add people…").expect("Add people… by Bob")));
        redraw();
        assert!(labelled(&ui, "Start a group: who else?").is_some());
        typed(ui.window(), "An");
        press(ui.window(), &[slint::platform::Key::Return.into()]);
        redraw();
        assert_eq!(*grouped.borrow(), ["Annie"]);
        assert_eq!(picked.borrow()[4], (3, String::new()));
    }

    /// Your details show your contact line's QR code, and the line.
    #[test]
    fn your_details_show_your_qr_code() {
        use i_slint_backend_testing::ElementHandle;
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        app.set_view(Pane::Peers);
        app.set_pd(crate::PeerDetails { me: true, ..app.get_pd() });
        let line = swarm_app::contact::line(&[7; 32], &[], &["203.0.113.7".parse().unwrap()], "Ann");
        app.set_my_qr(snapshot::qr_image(&line));
        app.set_me(crate::YouShown { contact: line.as_str().into(), ..app.get_me() });
        redraw();
        let code = ElementHandle::find_by_element_type_name(&ui, "ContactCode").next().expect("the code, with your details");
        assert!(code.size().height > 200.0, "drawn its full size");
        assert_eq!(app.get_my_qr().size().width as usize, swarm_app::contact::qr(&line).len() + 8, "a pixel a square, and the margin");
    }

    /// Your QR code, tapped, fills the window as far as its shorter side
    /// allows (never cropped on a wide one); a tap or Esc puts it away.
    #[test]
    fn a_tapped_qr_code_fills_the_window_and_a_tap_closes_it() {
        use i_slint_backend_testing::ElementHandle;
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        app.set_view(Pane::Peers);
        app.set_pd(crate::PeerDetails { me: true, ..app.get_pd() });
        let line = swarm_app::contact::line(&[7; 32], &[], &["203.0.113.7".parse().unwrap()], "Ann");
        app.set_my_qr(snapshot::qr_image(&line));
        app.set_me(crate::YouShown { contact: line.as_str().into(), ..app.get_me() });
        ui.window().set_size(slint::LogicalSize::new(1400.0, 700.0));
        redraw();
        assert!(ElementHandle::find_by_element_type_name(&ui, "QrBig").next().is_none());
        let small = ElementHandle::find_by_element_type_name(&ui, "ContactCode").next().expect("the code");
        let image = ElementHandle::find_by_element_id(&ui, "ContactCode::image").next().unwrap_or(small);
        click(ui.window(), middle(&image));
        redraw();
        assert_eq!(app.get_qr_big(), 1, "yours, full window");
        let big = ElementHandle::find_by_element_type_name(&ui, "QrBig").next().expect("shown over everything");
        assert_eq!((big.size().width, big.size().height), (1400.0, 700.0));
        click(ui.window(), middle(&big));
        redraw();
        assert_eq!(app.get_qr_big(), 0, "a tap closes it");
        app.set_qr_big(1);
        redraw();
        press(ui.window(), &[slint::platform::Key::Escape.into()]);
        assert_eq!(app.get_qr_big(), 0, "as does Esc");
    }

    /// A peer's details show their contact line's QR code too.
    #[test]
    fn a_peers_details_show_their_qr_code() {
        use i_slint_backend_testing::ElementHandle;
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        app.set_view(Pane::Peers);
        app.set_pd(crate::PeerDetails { shown: true, ..app.get_pd() });
        let line = swarm_app::contact::line(&[9; 32], &[], &["203.0.113.8".parse().unwrap()], "Bob");
        app.set_pd_qr(snapshot::qr_image(&line));
        app.set_pd(crate::PeerDetails { contact: line.as_str().into(), ..app.get_pd() });
        redraw();
        let code = ElementHandle::find_by_element_type_name(&ui, "ContactCode").next().expect("their code, with their details");
        assert!(code.size().height > 200.0, "drawn its full size");
        assert!(labelled(&ui, "Or all of it at once: this code, scanned in their Add a contact, or the line under it.").is_none(), "not your words");
    }

    /// Add a peer reads a QR code with the camera: Scan opens it, Stop
    /// closes it, and a code read fills the form, which Save adds.
    #[test]
    fn add_a_peer_fills_itself_from_a_qr_code() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let scans = std::rc::Rc::new(std::cell::RefCell::new(Vec::<bool>::new()));
        app.on_scan({ let scans = scans.clone(); move |on| scans.borrow_mut().push(on) });
        let added = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        app.on_add_peer({ let added = added.clone(); move |name, address, key, _| added.borrow_mut().extend([name.to_string(), address.to_string(), key.to_string()]) });
        app.set_view(Pane::Peers);
        redraw();
        press(ui.window(), &["a".into()]);
        redraw();
        click(ui.window(), middle(&labelled(&ui, "Scan a QR code").expect("Scan, in Add a contact")));
        app.set_me(crate::YouShown { scanning: true, ..app.get_me() });
        redraw();
        assert!(labelled(&ui, "Scan a QR code").is_none());
        click(ui.window(), middle(&labelled(&ui, "Stop").expect("Stop, while scanning")));
        assert_eq!(*scans.borrow(), [true, false]);
        app.set_me(crate::YouShown { scanned_name: "Ann".into(), ..app.get_me() });
        app.set_me(crate::YouShown { scanned_address: "203.0.113.7".into(), ..app.get_me() });
        app.set_me(crate::YouShown { scanned_key: "BwcH".into(), ..app.get_me() });
        app.set_me(crate::YouShown { scanned_count: 1, ..app.get_me() });
        app.set_me(crate::YouShown { scanning: false, ..app.get_me() });
        redraw();
        click(ui.window(), middle(&labelled(&ui, "Save").expect("Save")));
        assert_eq!(*added.borrow(), ["Ann", "203.0.113.7", "BwcH"]);
    }

    /// A file can be sent from a call, with a button under it: in the
    /// window, among the call's; on the phone, below the call, not over it.
    #[test]
    fn a_file_can_be_sent_from_a_call() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let sent = std::rc::Rc::new(std::cell::Cell::new(0));
        app.on_send_file({ let sent = sent.clone(); move || sent.set(sent.get() + 1) });
        app.set_call(crate::CallShown { video_shown: true, ..app.get_call() });
        redraw();
        click(ui.window(), middle(&labelled(&ui, "File").expect("File, in a video call")));
        app.set_call(crate::CallShown { video_shown: false, text: "on a call with Ann".into(), live: true, ..app.get_call() });
        redraw();
        click(ui.window(), middle(&labelled(&ui, "File").expect("File, in a voice call")));
        assert_eq!(sent.get(), 2);
    }

    /// The phone's, below its video call, not over it.
    #[test]
    fn on_the_phone_a_file_can_be_sent_from_a_video_call() {
        use i_slint_backend_testing::{ElementHandle, ElementRoot};
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        let sent = std::rc::Rc::new(std::cell::Cell::new(0));
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::default() })).unwrap();
        let phone = PhoneWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(412, 915));
        phone.show().unwrap();
        let app = phone.global::<App>();
        app.on_send_file({ let sent = sent.clone(); move || sent.set(sent.get() + 1) });
        app.set_call(crate::CallShown { local_on: true, ..app.get_call() });
        app.set_call_cameras(std::rc::Rc::new(slint::VecModel::from(vec![slint::SharedString::from("Selfie"), "Back".into()])).into());
        app.set_addable(std::rc::Rc::new(slint::VecModel::from(vec![slint::SharedString::from("Bob")])).into());
        app.set_call(crate::CallShown { video_shown: true, ..app.get_call() });
        window.request_redraw();
        window.draw_if_needed(|renderer| { renderer.render(&mut vec![slint::platform::software_renderer::Rgb565Pixel::default(); 412 * 915], 412); });
        let button = |label: &str| {
            phone.root_element().query_descendants().match_predicate({ let label = label.to_string(); move |e| e.accessible_label().as_deref() == Some(label.as_str()) }).find_first().unwrap_or_else(|| panic!("{label}"))
        };
        let file = button("File");
        let row = ["Microphone", "Camera", "Invite…", "File", "End call"].map(button);
        assert!(phone.root_element().query_descendants().match_predicate(|e| e.accessible_label().as_deref() == Some("Switch")).find_first().is_none(), "no Switch");
        for b in &row {
            assert_eq!(b.absolute_position().y, file.absolute_position().y, "one row");
            assert_eq!(b.size().height, file.size().height, "one style");
            assert!(b.absolute_position().x >= 0.0 && b.absolute_position().x + b.size().width <= 412.0, "on the screen: {:?} {:?}", b.accessible_label(), row.iter().map(|b| (b.absolute_position().x, b.size().width)).collect::<Vec<_>>());
        }
        assert!(row[0].absolute_position().x < file.absolute_position().x && file.absolute_position().x < row[4].absolute_position().x, "in the middle");
        let stage = ElementHandle::find_by_element_type_name(&phone, "CallStage").next().expect("the call");
        assert!(stage.absolute_position().y + stage.size().height >= 915.0 - 1.0, "the call to the bottom");
        click(phone.window(), middle(&file));
        assert_eq!(sent.get(), 1);
    }

    /// Everyone in the list has a ringed circle beside their name, with
    /// their picture in it if they have one (a letter if not); and a
    /// picture's in their chat's heading with what they say of themselves.
    #[test]
    fn a_peers_picture_and_status_are_by_their_name() {
        use i_slint_backend_testing::ElementHandle;
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let rows = vec![Row { kind: RowKind::Peer, name: "Ann".into(), ..Default::default() }, Row { kind: RowKind::Peer, name: "Bob".into(), letter: "B".into(), ..Default::default() }];
        app.set_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        let pictures = vec![Pictured { picture: a_picture(), shown: true }, Pictured::default()];
        app.set_row_pictures(slint::ModelRc::new(slint::VecModel::from(pictures)));
        app.set_peer(crate::PeerShown { name: "Ann".into(), says: "at the beach".into(), ..app.get_peer() });
        redraw();
        let avatars = || ElementHandle::find_by_element_type_name(&ui, "Avatar").count();
        assert_eq!(ElementHandle::find_by_element_type_name(&ui, "RingAvatar").count(), 2, "both in the list");
        assert!(labelled(&ui, "B").is_some(), "Bob's letter, having no picture");
        assert_eq!(avatars(), 0, "no heading picture yet");
        app.set_peer_avatar(a_picture());
        app.set_peer(crate::PeerShown { pictured: true, ..app.get_peer() });
        redraw();
        assert_eq!(avatars(), 1, "and in her chat's heading");
        assert!(labelled(&ui, "“at the beach”").is_some());
    }

    /// The phone too: their picture and status at the top of their chat.
    #[test]
    fn on_the_phone_a_peers_picture_and_status_head_their_chat() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::default() })).unwrap();
        let ui = PhoneWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(412, 915));
        ui.show().unwrap();
        let draw = || {
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| { renderer.render(&mut vec![slint::platform::software_renderer::Rgb565Pixel::default(); 412 * 915], 412); });
        };
        let app = ui.global::<App>();
        app.set_view(Pane::Peer);
        app.set_rows(slint::ModelRc::new(slint::VecModel::from(vec![Row { kind: RowKind::Peer, name: "Ann".into(), ..Default::default() }])));
        app.set_row_pictures(slint::ModelRc::new(slint::VecModel::from(vec![Pictured { picture: a_picture(), shown: true }])));
        draw();
        let avatars = || ElementHandle::find_by_element_type_name(&ui, "RingAvatar").count();
        assert_eq!(avatars(), 1, "in the list");
        click(ui.window(), middle(&labelled(&ui, "Ann").expect("Ann, listed")));
        draw();
        app.set_peer(crate::PeerShown { name: "Ann".into(), says: "at the beach".into(), pictured: true, ..app.get_peer() });
        app.set_peer_avatar(a_picture());
        draw();
        assert!(labelled(&ui, "“at the beach”").is_some());
        assert!(avatars() >= 1 && ElementHandle::find_by_element_type_name(&ui, "RingAvatar").any(|a| a.size().width >= 36.0), "at the top of her chat");
    }

    /// Settings on a computer: when a message makes a sound, and which;
    /// chosen, played, and back to Swarm's own.
    #[test]
    fn the_message_sound_is_chosen_in_settings() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<SoundStep>::new()));
        app.on_sound({ let asked = asked.clone(); move |what| asked.borrow_mut().push(what) });
        app.set_view(Pane::Settings);
        redraw();
        assert!(labelled(&ui, "Message sound").is_some());
        assert_eq!(all_labelled(&ui, "Swarm's own").len(), 2, "Swarm's own, till one's chosen, and the ringtone's");
        click(ui.window(), middle(all_labelled(&ui, "Choose…").last().expect("Choose…")));
        click(ui.window(), middle(all_labelled(&ui, "Play").last().expect("Play")));
        app.set_settings(crate::SettingsShown { sound_name: "bell.ogg".into(), ..app.get_settings() });
        redraw();
        assert!(labelled(&ui, "bell.ogg").is_some());
        click(ui.window(), middle(all_labelled(&ui, "Swarm's own").last().expect("back to Swarm's own")));
        assert_eq!(*asked.borrow(), [SoundStep::Choose, SoundStep::Play, SoundStep::BuiltIn]);
    }

    /// Settings › General: the ringtone, chosen, played, and back to
    /// Swarm's own, apart from the message sound.
    #[test]
    fn the_ringtone_is_chosen_in_general() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<SoundStep>::new()));
        app.on_ringtone({ let asked = asked.clone(); move |what| asked.borrow_mut().push(what) });
        app.on_sound(|_| panic!("the message sound's, not the ringtone's"));
        app.set_view(Pane::Settings);
        redraw();
        let ringtone = labelled(&ui, "Ringtone").expect("Ringtone");
        assert!(ringtone.absolute_position().y < labelled(&ui, "Colours").unwrap().absolute_position().y + 200.0, "in General, at the top");
        assert!(ringtone.absolute_position().y < labelled(&ui, "Message sound").unwrap().absolute_position().y);
        click(ui.window(), middle(&all_labelled(&ui, "Choose…")[0]));
        click(ui.window(), middle(&all_labelled(&ui, "Play")[0]));
        app.set_settings(crate::SettingsShown { ringtone_name: "phone.ogg".into(), ..app.get_settings() });
        redraw();
        assert!(labelled(&ui, "phone.ogg").is_some());
        click(ui.window(), middle(&all_labelled(&ui, "Swarm's own")[0]));
        assert_eq!(*asked.borrow(), [SoundStep::Choose, SoundStep::Play, SoundStep::BuiltIn]);
    }

    /// One of your devices, chosen under You: Remove for another (never
    /// this one), relaying once there are two; Add a device's code shows
    /// under You until Done.
    #[test]
    fn one_of_your_devices_is_shown_removed_and_added_to() {
        let (ui, _) = chat_window("");
        ui.window().set_size(slint::PhysicalSize::new(1280, 2400));
        let app = ui.global::<App>();
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        app.on_device_remove({ let asked = asked.clone(); move || asked.borrow_mut().push("remove".to_string()) });
        app.on_device_relay({ let asked = asked.clone(); move |yes| asked.borrow_mut().push(format!("relay {yes}")) });
        app.on_show_device_code({ let asked = asked.clone(); move |on| asked.borrow_mut().push(format!("code {on}")) });
        app.set_view(Pane::Peers);
        let phone = crate::DeviceDetails { shown: true, name: "phone".into(), status: "online".into(), added: "added 2 minutes ago".into(), key: "abc".into(), this: false, relay: false, linked: true, ..Default::default() };
        app.set_dd(phone.clone());
        redraw();
        assert!(labelled(&ui, "phone").is_some() && labelled(&ui, "Your device").is_some());
        assert!(labelled(&ui, "Call").is_some() && labelled(&ui, "Video call").is_some());
        assert!(labelled(&ui, "Set address").is_some(), "where to try it");
        let typed_for = |typed: &str| crate::DeviceDetails { typed: typed.into(), ..phone.clone() };
        app.set_dd(typed_for("home.example.net:5000"));
        redraw();
        use i_slint_backend_testing::ElementRoot as _;
        let shown = ui.root_element().query_descendants().match_predicate(|e| e.accessible_value().as_deref() == Some("home.example.net:5000")).find_all();
        assert!(!shown.is_empty(), "the address set");
        app.set_dd(phone.clone());
        redraw();
        assert!(labelled(&ui, "Scan its code").is_some(), "or read from its code");
        click(ui.window(), middle(&labelled(&ui, "Remove…").expect("Remove…")));
        click(ui.window(), middle(&labelled(&ui, "Relays for your other devices").expect("relaying")));
        app.set_dd(crate::DeviceDetails { name: "desktop".into(), this: true, linked: false, ..phone });
        redraw();
        assert!(labelled(&ui, "This device").is_some() && labelled(&ui, "Rename").is_some());
        assert!(labelled(&ui, "Remove…").is_none() && labelled(&ui, "Relays for your other devices").is_none() && labelled(&ui, "Set address").is_none() && labelled(&ui, "Scan its code").is_none());
        app.set_dd(crate::DeviceDetails::default());
        app.set_pd(crate::PeerDetails { me: true, ..Default::default() });
        app.set_device_code("swarm-device:abc?a=fd99::1&s=xyz".into());
        redraw();
        click(ui.window(), middle(&labelled(&ui, "Done").expect("Done")));
        assert_eq!(*asked.borrow(), ["remove", "relay true", "code false"]);
    }

    /// The first run asks one thing: a new user, or this device added
    /// to yours with a code from another of yours.
    #[test]
    fn the_first_run_asks_new_user_or_this_device_added() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let joined = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        app.on_join_user({ let joined = joined.clone(); move |code, name, pass, again| joined.borrow_mut().push(format!("{code}|{name}|{pass}|{again}")) });
        app.set_view(Pane::FirstRun);
        redraw();
        assert!(labelled(&ui, "New user").is_some());
        assert!(labelled(&ui, "Create identity").is_none(), "nothing asked for till it's answered");
        click(ui.window(), middle(&labelled(&ui, "Add this device to my user").expect("the other answer")));
        redraw();
        assert!(labelled(&ui, "Create identity").is_none(), "no new user's button");
        typed(ui.window(), "swarm-device:abc");
        press(ui.window(), &[slint::platform::Key::Tab.into(), slint::platform::Key::Tab.into(), slint::platform::Key::Tab.into()]);
        typed(ui.window(), "pw");
        press(ui.window(), &[slint::platform::Key::Tab.into()]);
        typed(ui.window(), "pw");
        click(ui.window(), middle(&labelled(&ui, "Add this device").expect("Add this device")));
        assert_eq!(*joined.borrow(), ["swarm-device:abc||pw|pw"]);
        click(ui.window(), middle(&labelled(&ui, "Back").expect("Back")));
        redraw();
        click(ui.window(), middle(&labelled(&ui, "New user").expect("asked again")));
        redraw();
        assert!(labelled(&ui, "Create identity").is_some());
    }

    /// Adding this device, the camera reads the other's code: Scan opens
    /// it, the picture shows with Stop, and a code read is put in the
    /// code's box.
    #[test]
    fn the_first_run_scans_another_devices_code() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let scans = std::rc::Rc::new(std::cell::RefCell::new(Vec::<bool>::new()));
        app.on_scan({ let scans = scans.clone(); move |on| scans.borrow_mut().push(on) });
        let joined = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        app.on_join_user({ let joined = joined.clone(); move |code, _, _, _| joined.borrow_mut().push(code.to_string()) });
        app.set_view(Pane::FirstRun);
        redraw();
        click(ui.window(), middle(&labelled(&ui, "Add this device to my user").expect("the question")));
        redraw();
        click(ui.window(), middle(&labelled(&ui, "Scan a QR code").expect("Scan, by the code's box")));
        assert_eq!(*scans.borrow(), [true]);
        app.set_first_run_scanning(true);
        redraw();
        assert!(labelled(&ui, "Hold your other device's code up to the camera.").is_some());
        assert!(labelled(&ui, "Scan a QR code").is_none());
        click(ui.window(), middle(&labelled(&ui, "Stop").expect("Stop, while scanning")));
        assert_eq!(*scans.borrow(), [true, false]);
        let snap = crate::snapshot::Snapshot { first_run_code: Some("swarm-device:read".to_string()), ..crate::snapshot::Snapshot::first_run() };
        app.set_first_run_scanning(snap.first_run_scanning);
        app.set_first_run_code(snap.first_run_code.unwrap().into());
        redraw();
        assert!(labelled(&ui, "Scan a QR code").is_some(), "the camera's closed");
        press(ui.window(), &[slint::platform::Key::Tab.into(), slint::platform::Key::Tab.into(), slint::platform::Key::Tab.into()]);
        typed(ui.window(), "pw");
        press(ui.window(), &[slint::platform::Key::Tab.into()]);
        typed(ui.window(), "pw");
        click(ui.window(), middle(&labelled(&ui, "Add this device").expect("Add this device")));
        assert_eq!(*joined.borrow(), ["swarm-device:read"], "the code read is the one taken");
    }

    /// A device taken off by another of yours says so, and not as Swarm
    /// failing to start.
    #[test]
    fn a_wiped_device_says_it_was_taken_off() {
        let (ui, _) = chat_window("");
        let app = ui.global::<App>();
        let snap = crate::snapshot::Snapshot::wiped();
        app.set_fatal(snap.fatal.clone().into());
        app.set_wiped(snap.wiped);
        app.set_view(snap.view);
        redraw();
        assert!(labelled(&ui, "This device has been taken off").is_some());
        assert!(labelled(&ui, "Swarm couldn't start").is_none());
        assert!(labelled(&ui, swarm_app::devices::WIPED).is_some());
    }

    /// The phone has its notifications, not a sound of Swarm's.
    #[test]
    fn the_phone_has_no_message_sound() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::default() })).unwrap();
        let ui = PhoneWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(412, 3000));
        ui.show().unwrap();
        ui.global::<App>().set_view(Pane::Settings);
        window.request_redraw();
        window.draw_if_needed(|renderer| { renderer.render(&mut vec![slint::platform::software_renderer::Rgb565Pixel::default(); 412 * 3000], 412); });
        assert!(labelled(&ui, "Notifications").is_some(), "the phone's own, drawn");
        assert!(labelled(&ui, "Message sound").is_none());
        assert!(labelled(&ui, "Ringtone").is_some(), "but a ringtone");
    }

    /// The window's top line has no badge with Swarm's name, and doesn't
    /// say who you are: you're first in the list just below it.
    #[test]
    fn the_top_line_leaves_out_the_badge_and_who_you_are() {
        let (ui, _) = chat_window("");
        { let app = ui.global::<App>(); app.set_top(crate::TopLine { name: "Ann (laptop)".into(), ..app.get_top() }); }
        redraw();
        assert!(labelled(&ui, "swarm").is_none() && labelled(&ui, "Swarm").is_none(), "no badge");
        assert!(labelled(&ui, "Ann (laptop)").is_none(), "your name's in the list");
    }

    /// The phone's top bar has no badge either: the page's title starts it.
    #[test]
    fn the_phones_top_bar_has_no_badge() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Clipped { window: window.clone(), clipboard: std::rc::Rc::default() })).unwrap();
        let ui = PhoneWindow::new().unwrap();
        window.set_size(slint::PhysicalSize::new(412, 900));
        ui.show().unwrap();
        ui.global::<App>().set_view(Pane::Peers);
        window.request_redraw();
        window.draw_if_needed(|renderer| { renderer.render(&mut vec![slint::platform::software_renderer::Rgb565Pixel::default(); 412 * 900], 412); });
        assert!(labelled(&ui, "Contacts").is_some(), "the page's title, drawn");
        assert!(labelled(&ui, "swarm").is_none() && labelled(&ui, "Swarm").is_none(), "no badge");
    }

    /// A window that keeps what each field asks of the keyboard: whether
    /// it may correct what's typed, and whether swarm says it offers its
    /// own completions (to Android, the keyboard's own suggestions then
    /// aren't shown).
    struct Keyboarded {
        window: slint::Window,
        renderer: slint::platform::software_renderer::SoftwareRenderer,
        size: std::cell::Cell<slint::PhysicalSize>,
        asked: std::cell::RefCell<Vec<i_slint_core::items::InputMethodHints>>,
    }

    impl slint::platform::WindowAdapter for Keyboarded {
        fn window(&self) -> &slint::Window {
            &self.window
        }
        fn size(&self) -> slint::PhysicalSize {
            self.size.get()
        }
        fn set_size(&self, size: slint::WindowSize) {
            self.size.set(size.to_physical(1.0));
        }
        fn renderer(&self) -> &dyn slint::platform::Renderer {
            &self.renderer
        }
        fn internal(&self, _: i_slint_core::InternalToken) -> Option<&dyn i_slint_core::window::WindowAdapterInternal> {
            Some(self)
        }
    }

    impl i_slint_core::window::WindowAdapterInternal for Keyboarded {
        fn input_method_request(&self, request: i_slint_core::window::InputMethodRequest) {
            use i_slint_core::window::InputMethodRequest::{Enable, Update};
            if let Enable(props) | Update(props) = request {
                self.asked.borrow_mut().push(props.input_method_hints);
            }
        }
    }

    struct KeyboardPlatform(std::rc::Rc<Keyboarded>);

    impl slint::platform::Platform for KeyboardPlatform {
        fn create_window_adapter(&self) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    fn keyboarded() -> (AppWindow, std::rc::Rc<Keyboarded>) {
        let adapter = std::rc::Rc::new_cyclic(|me: &std::rc::Weak<Keyboarded>| Keyboarded {
            window: slint::Window::new(me.clone()),
            renderer: Default::default(),
            size: std::cell::Cell::new(slint::PhysicalSize::new(1280, 820)),
            asked: Default::default(),
        });
        slint::platform::set_platform(Box::new(KeyboardPlatform(adapter.clone()))).unwrap();
        let ui = AppWindow::new().unwrap();
        wire_keys(&ui);
        ui.show().unwrap();
        (ui, adapter)
    }

    /// What the field pressed on asked of the keyboard: (may correct, may
    /// suggest).
    fn asked_of(ui: &AppWindow, adapter: &Keyboarded, field: &i_slint_backend_testing::ElementHandle) -> (bool, bool) {
        adapter.asked.borrow_mut().clear();
        click(ui.window(), middle(field));
        let asked = adapter.asked.borrow();
        let hints = asked.last().expect("the keyboard asked for");
        (hints.auto_correct, !hints.auto_complete)
    }

    /// A message is typed with the keyboard's suggestions and corrections,
    /// as in any other messaging app; an address or a key is typed as it
    /// is, nothing corrected.
    #[test]
    fn the_keyboard_suggests_words_for_messages_and_leaves_keys_alone() {
        use i_slint_backend_testing::ElementHandle;
        let (ui, adapter) = keyboarded();
        let app = ui.global::<App>();
        app.set_view(Pane::Peer);
        let message = ElementHandle::find_by_element_id(&ui, "TypingBox::edit").next().expect("the message box");
        assert_eq!(asked_of(&ui, &adapter, &message), (true, true), "the message box: (corrects, suggests)");
        ui.invoke_focus_keys();
        app.set_view(Pane::Peers);
        typed(ui.window(), "a");
        let fields: Vec<_> = ElementHandle::find_by_element_id(&ui, "Labelled::edit").collect();
        assert_eq!(asked_of(&ui, &adapter, &fields[0]), (false, true), "a code: not corrected");
        assert_eq!(asked_of(&ui, &adapter, &fields[1]), (false, true), "a name: suggested, not corrected");
        assert_eq!(asked_of(&ui, &adapter, &fields[2]), (false, true), "an address: not corrected");
        assert_eq!(asked_of(&ui, &adapter, &fields[3]), (false, true), "a key: not corrected");
    }
}
