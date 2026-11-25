//! Controlling a shared screen, and being controlled (see
//! `swarm_core::remote`): who's asked and who's been let, on the side
//! sharing; what's asked for and the pointer and keys sent, on the side
//! watching.
//!
//! The rules, as agreed:
//! - The one watching asks, and the one sharing allows or refuses, each
//!   time (unless they've set the one asking to be let in unasked: see
//!   `swarm_core::settings::auto_control`). Only over the link between
//!   the two (never passed on by another member of a conference).
//! - Asked while no screen's shared, it's to see the screen as well:
//!   allowing it shares the screen first (the call does that; see
//!   `wants_screen`). The one watching waits for the screen to come
//!   (`SCREEN_WAIT`) before giving up.
//! - One at a time: letting someone else control it stops whoever was.
//! - The one sharing can stop it whenever they like; it stops too when
//!   the sharing does, the link goes, or the call ends — and then every
//!   key and button held down is let go, so nothing's left stuck.
//! - Using your own mouse while being controlled pauses the other side
//!   for `PAUSE_AFTER_LOCAL` (as far as the desktop can tell which is
//!   which: see `Injector::local_input_at`).
//!
//! What's done on the screen goes through an `Injector`: X11's
//! (`remote_x11`), Wayland's portal (`remote_wayland`) or Windows'
//! (`remote_win`), or `SWARM_INPUT=log:<file>`, which writes each
//! thing done to a file, for labs. A desktop with none says why, and
//! whoever asks is told that.

use swarm_core::lock::Locked;
use crate::notes::Note;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use swarm_core::call::CallSignal;
use swarm_core::remote::{keys, Control, Input};

/// How long your own mouse or keys pause whoever's controlling.
pub const PAUSE_AFTER_LOCAL: Duration = Duration::from_secs(1);
/// The pointer's sent no more often than this: newest wins in between,
/// so a fast mouse doesn't flood a slow link (or the cover slots).
pub const POINTER_EVERY: Duration = Duration::from_millis(15);

/// Watching: how long after they allow it we wait for their screen,
/// when we asked before it was shared (they share it as they allow it;
/// a desktop's dialog may come first).
pub const SCREEN_WAIT: Duration = Duration::from_secs(30);

/// Does things on this desktop's screen, as if done here.
pub trait Injector: Send {
    /// The pointer to this point of the screen shared: 0.0 its left or
    /// top edge, 1.0 its right or bottom.
    fn pointer(&mut self, x: f64, y: f64) -> anyhow::Result<()>;
    /// See `swarm_core::remote::BUTTON_LEFT` and the rest.
    fn button(&mut self, button: u8, down: bool) -> anyhow::Result<()>;
    /// In 120ths of a notch (see `Input::Wheel`).
    fn wheel(&mut self, dx: i32, dy: i32) -> anyhow::Result<()>;
    /// A key pressed or let go (see `Input::Key`).
    fn key(&mut self, key: char, down: bool) -> anyhow::Result<()>;
    /// Characters typed.
    fn text(&mut self, text: &str) -> anyhow::Result<()>;
    /// When the mouse or keys here were last used by someone here, not
    /// by us, if the desktop can tell.
    fn local_input_at(&self) -> Option<Instant> {
        None
    }
    /// Control's been allowed: whatever must be asked of the desktop is
    /// asked now (not when an injector's opened only to see whether one
    /// can be). Anything the one sharing should be told.
    fn start(&mut self) -> Option<String> {
        None
    }
    /// The desktop's taken control back itself, and why.
    fn ended(&self) -> Option<String> {
        None
    }
}

/// An injector for this desktop, or why there's none.
pub type Opener = Arc<dyn Fn() -> Result<Box<dyn Injector>, String> + Send + Sync>;

/// This desktop's injector: X11's (`remote_x11`), Wayland's
/// (`remote_wayland`) or Windows' (`remote_win`), or with
/// `SWARM_INPUT=log:<file>`, what would be done written to a file.
pub fn open_injector() -> Result<Box<dyn Injector>, String> {
    if let Some(path) = std::env::var("SWARM_INPUT").ok().and_then(|v| v.strip_prefix("log:").map(str::to_string)) {
        return LogInjector::open(&path).map(|i| Box::new(i) as Box<dyn Injector>).map_err(|e| format!("couldn't open {path}: {e}"));
    }
    #[cfg(target_os = "linux")]
    {
        if crate::screen::wayland() {
            return crate::remote_wayland::WaylandInjector::open().map(|i| Box::new(i) as Box<dyn Injector>).map_err(|e| format!("{e:#}"));
        }
        if std::env::var_os("DISPLAY").is_none() {
            return Err("there's no display here".to_string());
        }
        crate::remote_x11::X11Injector::open(None).map(|i| Box::new(i) as Box<dyn Injector>).map_err(|e| format!("{e:#}"))
    }
    #[cfg(windows)]
    {
        crate::remote_win::WinInjector::open().map(|i| Box::new(i) as Box<dyn Injector>).map_err(|e| format!("{e:#}"))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    Err("controlling this desktop isn't supported yet".to_string())
}

/// Writes each thing it's asked to do as a line of a file.
pub struct LogInjector(std::fs::File);

impl LogInjector {
    pub fn open(path: &str) -> std::io::Result<LogInjector> {
        std::fs::OpenOptions::new().create(true).append(true).open(path).map(LogInjector)
    }

    fn say(&mut self, line: String) -> anyhow::Result<()> {
        use std::io::Write;
        writeln!(self.0, "{line}")?;
        Ok(())
    }
}

impl Injector for LogInjector {
    fn pointer(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
        self.say(format!("pointer {x:.4} {y:.4}"))
    }
    fn button(&mut self, button: u8, down: bool) -> anyhow::Result<()> {
        self.say(format!("button {button} {}", if down { "down" } else { "up" }))
    }
    fn wheel(&mut self, dx: i32, dy: i32) -> anyhow::Result<()> {
        self.say(format!("wheel {dx} {dy}"))
    }
    fn key(&mut self, key: char, down: bool) -> anyhow::Result<()> {
        self.say(format!("key U+{:04X} {}", key as u32, if down { "down" } else { "up" }))
    }
    fn text(&mut self, text: &str) -> anyhow::Result<()> {
        self.say(format!("text {text}"))
    }
}

/// Where a point of the picture sent (`Input::Pointer`, bars and all) is
/// on a screen `width` by `height`: the picture's the screen made 16:9
/// with black bars, evenly either side (see `screen`), so the bars are
/// taken off, and a point on one is the screen's nearest edge.
pub fn on_screen(x: u16, y: u16, width: u32, height: u32) -> (f64, f64) {
    let (x, y) = (x as f64 / 65535.0, y as f64 / 65535.0);
    let (width, height) = (width.max(1) as f64, height.max(1) as f64);
    let aspect = width / height;
    let wide = 16.0 / 9.0;
    let (x, y) = if aspect >= wide {
        let part = wide / aspect;
        (x, (y - (1.0 - part) / 2.0) / part)
    } else {
        let part = aspect / wide;
        ((x - (1.0 - part) / 2.0) / part, y)
    };
    (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
}

/// Sends a signal on the link to the one with this key.
pub type Sender = Arc<dyn Fn([u8; 32], CallSignal) + Send + Sync>;

/// How it stands with the screen we're watching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watching {
    /// We've asked to control it.
    Asking,
    /// We're controlling it.
    Controlling,
    /// We're controlling it, but they're using their own mouse.
    Paused,
}

/// Our side of controlling, or being controlled: one per call.
pub struct RemoteControl {
    send: Sender,
    opener: Opener,
    /// Sharing: who's asked, oldest first, and not been answered.
    asking: Vec<[u8; 32]>,
    /// Sharing: those of `asking` who asked while no screen was shared,
    /// so want to see it as well.
    wants_screen: HashSet<[u8; 32]>,
    /// Sharing: who's controlling our screen.
    controller: Option<Controller>,
    /// Watching: whose screen, and how it stands.
    watching: Option<([u8; 32], Watching)>,
    /// Watching: modifiers we've sent held down.
    held: HashSet<char>,
    /// Watching: their shared screen's been seen since we asked; and
    /// when they allowed it (the screen may still be on its way).
    screen_seen: bool,
    allowed_at: Option<Instant>,
    pointer: Arc<Mutex<Pacer>>,
}

/// Whoever's controlling our screen, and what's held down for them.
struct Controller {
    key: [u8; 32],
    injector: Box<dyn Injector>,
    keys: HashSet<char>,
    buttons: HashSet<u8>,
    /// Paused for our own mouse, until when (see `PAUSE_AFTER_LOCAL`).
    paused_until: Option<Instant>,
}

impl Controller {
    /// Lets go of everything held for them.
    fn release(&mut self) {
        for button in std::mem::take(&mut self.buttons) {
            let _ = self.injector.button(button, false);
        }
        for key in std::mem::take(&mut self.keys) {
            let _ = self.injector.key(key, false);
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.release();
    }
}

/// The pointer, sent at most every `POINTER_EVERY`: newest wins.
#[derive(Default)]
struct Pacer {
    to: Option<[u8; 32]>,
    pending: Option<(u16, u16)>,
    last: Option<Instant>,
    /// A task's waiting to send what's pending.
    waiting: bool,
}

impl RemoteControl {
    pub fn new(send: Sender, opener: Opener) -> RemoteControl {
        RemoteControl { send, opener, asking: Vec::new(), wants_screen: HashSet::new(), controller: None, watching: None, held: HashSet::new(), screen_seen: false, allowed_at: None, pointer: Arc::new(Mutex::new(Pacer::default())) }
    }

    /// Sharing: the first who's asked and not been answered.
    pub fn asking(&self) -> Option<[u8; 32]> {
        self.asking.first().copied()
    }

    /// Sharing: whether `key` has asked and not been answered.
    pub fn has_asked(&self, key: [u8; 32]) -> bool {
        self.asking.contains(&key)
    }

    /// Sharing: whether `key` asked while no screen was shared, so
    /// allowing them shares it first.
    pub fn wants_screen(&self, key: [u8; 32]) -> bool {
        self.wants_screen.contains(&key)
    }

    /// Sharing: who's controlling our screen, and whether they're paused.
    pub fn controller(&self) -> Option<([u8; 32], bool)> {
        self.controller.as_ref().map(|c| (c.key, c.paused_until.is_some()))
    }

    /// Watching: whose screen, and how it stands.
    pub fn watching(&self) -> Option<([u8; 32], Watching)> {
        self.watching
    }

    /// What's come from `key` (called `name`) about control.
    /// `sharing`: our screen's size, while it's shared.
    pub fn on_signal(&mut self, key: [u8; 32], name: &str, signal: CallSignal, sharing: Option<(u32, u32)>) -> Option<Note> {
        match signal {
            CallSignal::Input(input) => {
                self.on_input(key, input, sharing);
                None
            }
            CallSignal::Control(Control::Ask) => {
                if sharing.is_some() && self.controller.as_ref().is_some_and(|c| c.key == key) {
                    (self.send)(key, CallSignal::Control(Control::Allow));
                    return None;
                }
                if let Err(why) = (self.opener)() {
                    (self.send)(key, CallSignal::Control(Control::Refuse(format!("this desktop can't be controlled: {why}"))));
                    return Some(Note::info(format!("{name} asked to control your screen, but {why}")));
                }
                if !self.asking.contains(&key) {
                    self.asking.push(key);
                }
                if sharing.is_none() {
                    self.wants_screen.insert(key);
                    return Some(Note::warning(format!("{name} asks to see and control your screen")));
                }
                self.wants_screen.remove(&key);
                Some(Note::warning(format!("{name} asks to control your screen")))
            }
            CallSignal::Control(Control::GiveBack) => {
                self.asking.retain(|k| *k != key);
                self.wants_screen.remove(&key);
                if self.controller.as_ref().is_some_and(|c| c.key == key) {
                    self.controller = None;
                    return Some(Note::info(format!("{name} has stopped controlling your screen")));
                }
                None
            }
            CallSignal::Control(Control::Allow) => match self.watching {
                Some((k, Watching::Asking)) if k == key => {
                    self.watching = Some((key, Watching::Controlling));
                    self.allowed_at = Some(Instant::now());
                    self.pointer.locked().to = Some(key);
                    Some(Note::success(format!("you're controlling {name}'s screen")))
                }
                Some((k, _)) if k == key => None,
                _ => {
                    (self.send)(key, CallSignal::Control(Control::GiveBack));
                    None
                }
            },
            CallSignal::Control(Control::Refuse(why)) => {
                if self.watching.is_some_and(|(k, _)| k == key) {
                    self.stop_watching();
                    return Some(Note::warning(format!("{name}: {why}")));
                }
                None
            }
            CallSignal::Control(Control::Stop) => {
                if self.watching.is_some_and(|(k, _)| k == key) {
                    self.stop_watching();
                    return Some(Note::info(format!("you're no longer controlling {name}'s screen")));
                }
                None
            }
            CallSignal::Control(Control::Paused(paused)) => {
                if let Some((_, state)) = self.watching.as_mut().filter(|(k, _)| *k == key) {
                    if *state != Watching::Asking {
                        *state = if paused { Watching::Paused } else { Watching::Controlling };
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn on_input(&mut self, key: [u8; 32], input: Input, sharing: Option<(u32, u32)>) {
        let Some((width, height)) = sharing else { return };
        let Some(c) = self.controller.as_mut().filter(|c| c.key == key) else { return };
        let local = c.injector.local_input_at().is_some_and(|at| at.elapsed() < PAUSE_AFTER_LOCAL);
        if local {
            if c.paused_until.is_none() {
                c.release();
                (self.send)(key, CallSignal::Control(Control::Paused(true)));
            }
            c.paused_until = Some(Instant::now() + PAUSE_AFTER_LOCAL);
            return;
        }
        if c.paused_until.is_some() {
            c.paused_until = None;
            (self.send)(key, CallSignal::Control(Control::Paused(false)));
        }
        let done = match input {
            Input::Pointer { x, y } => {
                let (x, y) = on_screen(x, y, width, height);
                c.injector.pointer(x, y)
            }
            Input::Button { button, down } => {
                if down {
                    c.buttons.insert(button);
                } else {
                    c.buttons.remove(&button);
                }
                c.injector.button(button, down)
            }
            Input::Wheel { dx, dy } => c.injector.wheel(dx as i32, dy as i32),
            Input::Key { key, down } => {
                if down {
                    c.keys.insert(key);
                } else {
                    c.keys.remove(&key);
                }
                c.injector.key(key, down)
            }
            Input::Text(text) => c.injector.text(&text),
        };
        if let Err(e) = done {
            eprintln!("{} controlling the screen: {e:#}", crate::unix_now());
        }
    }

    /// Sharing: lets the one who asked control our screen — and stops
    /// whoever was.
    pub fn allow(&mut self, key: [u8; 32], sharing: bool) -> Option<Note> {
        if !self.asking.contains(&key) {
            return None;
        }
        self.asking.retain(|k| *k != key);
        self.wants_screen.remove(&key);
        if !sharing {
            (self.send)(key, CallSignal::Control(Control::Refuse("the screen isn't being shared".to_string())));
            return None;
        }
        let mut injector = match (self.opener)() {
            Ok(injector) => injector,
            Err(why) => {
                (self.send)(key, CallSignal::Control(Control::Refuse(format!("this desktop can't be controlled: {why}"))));
                return Some(Note::error(format!("can't be controlled: {why}")));
            }
        };
        if let Some(was) = self.controller.take() {
            (self.send)(was.key, CallSignal::Control(Control::Stop));
        }
        let said = injector.start();
        self.controller = Some(Controller { key, injector, keys: HashSet::new(), buttons: HashSet::new(), paused_until: None });
        (self.send)(key, CallSignal::Control(Control::Allow));
        said.map(Note::info)
    }

    /// Sharing: refuses the one who asked.
    pub fn refuse(&mut self, key: [u8; 32]) {
        self.refuse_because(key, "refused");
    }

    /// Sharing: refuses the one who asked, saying why.
    pub fn refuse_because(&mut self, key: [u8; 32], why: &str) {
        if self.asking.contains(&key) {
            self.asking.retain(|k| *k != key);
            self.wants_screen.remove(&key);
            (self.send)(key, CallSignal::Control(Control::Refuse(why.to_string())));
        }
    }

    /// Sharing: stops whoever's controlling our screen. `true` if
    /// someone was.
    pub fn stop(&mut self) -> bool {
        match self.controller.take() {
            Some(was) => {
                (self.send)(was.key, CallSignal::Control(Control::Stop));
                true
            }
            None => false,
        }
    }

    /// Our sharing has ended (or isn't going): whoever's controlling is
    /// stopped, and whoever asked while it was shared refused. Those who
    /// asked to see it as well are still waiting for an answer.
    pub fn sharing_ended(&mut self) {
        self.stop();
        let wants_screen = &self.wants_screen;
        let (waiting, refused): (Vec<_>, Vec<_>) = std::mem::take(&mut self.asking).into_iter().partition(|k| wants_screen.contains(k));
        self.asking = waiting;
        for key in refused {
            (self.send)(key, CallSignal::Control(Control::Refuse("the screen isn't being shared any more".to_string())));
        }
    }

    /// The link to `key` has gone: nothing more of theirs, either way.
    pub fn link_gone(&mut self, key: [u8; 32]) {
        self.asking.retain(|k| *k != key);
        self.wants_screen.remove(&key);
        if self.controller.as_ref().is_some_and(|c| c.key == key) {
            self.controller = None;
        }
        if self.watching.is_some_and(|(k, _)| k == key) {
            self.stop_watching();
        }
    }

    /// Kept up with now and then: a pause for our own mouse that's run
    /// out is said to be over; and if the screen we're watching isn't
    /// shared any more (`sharing`: whether the one with this key is
    /// sharing theirs), we stop; and control the desktop's taken back
    /// itself is stopped.
    pub fn tick(&mut self, sharing: impl Fn([u8; 32]) -> bool) -> Option<Note> {
        let mut note = None;
        if let Some(why) = self.controller.as_ref().and_then(|c| c.injector.ended()) {
            self.stop();
            note = Some(Note::info(format!("control of your screen stopped: {why}")));
        }
        if let Some(c) = self.controller.as_mut() {
            if c.paused_until.is_some_and(|until| Instant::now() >= until) && !c.injector.local_input_at().is_some_and(|at| at.elapsed() < PAUSE_AFTER_LOCAL) {
                c.paused_until = None;
                (self.send)(c.key, CallSignal::Control(Control::Paused(false)));
            }
        }
        if let Some((key, _)) = self.watching {
            if sharing(key) {
                self.screen_seen = true;
            } else if self.screen_seen || self.allowed_at.is_some_and(|at| at.elapsed() >= SCREEN_WAIT) {
                self.give_back();
            }
        }
        note
    }

    /// Watching: asks to control `key`'s screen (and gives back any
    /// other's).
    pub fn ask(&mut self, key: [u8; 32]) {
        if self.watching.is_some_and(|(k, _)| k == key) {
            return;
        }
        self.give_back();
        self.watching = Some((key, Watching::Asking));
        (self.send)(key, CallSignal::Control(Control::Ask));
    }

    /// Watching: no longer asking, or controlling — they're told. `true`
    /// if we were.
    pub fn give_back(&mut self) -> bool {
        let Some((key, _)) = self.watching else { return false };
        self.stop_watching();
        (self.send)(key, CallSignal::Control(Control::GiveBack));
        true
    }

    fn stop_watching(&mut self) {
        self.watching = None;
        self.screen_seen = false;
        self.allowed_at = None;
        self.held.clear();
        *self.pointer.locked() = Pacer::default();
    }

    /// Watching: whether what's done over their picture goes to them.
    pub fn controlling(&self) -> Option<[u8; 32]> {
        self.watching.filter(|(_, state)| *state != Watching::Asking).map(|(key, _)| key)
    }

    /// Watching: the pointer's at this point of their picture (0.0 to
    /// 1.0 across and down, bars and all). Sent now, or as soon as
    /// `POINTER_EVERY` allows, unless it's moved again by then.
    pub fn pointer(&mut self, x: f32, y: f32) {
        let Some(to) = self.controlling() else { return };
        let point = ((x.clamp(0.0, 1.0) * 65535.0).round() as u16, (y.clamp(0.0, 1.0) * 65535.0).round() as u16);
        let mut pacer = self.pointer.locked();
        pacer.to = Some(to);
        let now = Instant::now();
        match pacer.last.filter(|last| now.duration_since(*last) < POINTER_EVERY) {
            None => {
                pacer.last = Some(now);
                pacer.pending = None;
                (self.send)(to, CallSignal::Input(Input::Pointer { x: point.0, y: point.1 }));
            }
            Some(last) => {
                pacer.pending = Some(point);
                if !pacer.waiting {
                    pacer.waiting = true;
                    let (pacer, send) = (self.pointer.clone(), self.send.clone());
                    let at = last + POINTER_EVERY;
                    std::thread::spawn(move || {
                        std::thread::sleep(at.saturating_duration_since(Instant::now()));
                        let mut pacer = pacer.locked();
                        pacer.waiting = false;
                        if let (Some(to), Some((x, y))) = (pacer.to, pacer.pending.take()) {
                            pacer.last = Some(Instant::now());
                            send(to, CallSignal::Input(Input::Pointer { x, y }));
                        }
                    });
                }
            }
        }
    }

    /// Sends something other than the pointer, after any pointer still
    /// to go, so it's done where the pointer was meant to be.
    fn send_input(&mut self, input: Input) {
        let Some(to) = self.controlling() else { return };
        let mut pacer = self.pointer.locked();
        if let Some((x, y)) = pacer.pending.take() {
            pacer.last = Some(Instant::now());
            (self.send)(to, CallSignal::Input(Input::Pointer { x, y }));
        }
        (self.send)(to, CallSignal::Input(input));
    }

    /// Watching: a mouse button pressed or let go over their picture.
    pub fn button(&mut self, button: u8, down: bool) {
        self.send_input(Input::Button { button, down });
    }

    /// Watching: the wheel turned over their picture (see `Input::Wheel`).
    pub fn wheel(&mut self, dx: i32, dy: i32) {
        if dx != 0 || dy != 0 {
            let clamp = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            self.send_input(Input::Wheel { dx: clamp(dx), dy: clamp(dy) });
        }
    }

    /// Watching: a key pressed or let go, as the window gives it (`key`:
    /// the character it types, or one of `keys`). Characters go as text,
    /// so they come out as typed whatever the keyboard at the other end;
    /// with Control, Alt or the logo key held, as a key (a shortcut);
    /// keys that type nothing as keys. AltGr and Caps Lock aren't sent:
    /// what they change comes in the characters.
    pub fn key(&mut self, key: char, down: bool) {
        if self.controlling().is_none() || matches!(key, keys::ALT_GR | keys::CAPS_LOCK) {
            return;
        }
        if keys::is_modifier(key) {
            if down {
                self.held.insert(key);
            } else {
                self.held.remove(&key);
            }
            self.send_input(Input::Key { key, down });
        } else if keys::is_named(key) {
            self.send_input(Input::Key { key, down });
        } else if self.held.iter().any(|k| !matches!(*k, keys::SHIFT | keys::SHIFT_R)) {
            self.send_input(Input::Key { key: key.to_lowercase().next().unwrap_or(key), down });
        } else if down {
            self.send_input(Input::Text(key.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANN: [u8; 32] = [1; 32];
    const BOB: [u8; 32] = [2; 32];
    const SCREEN: Option<(u32, u32)> = Some((1920, 1080));

    type Sent = Arc<Mutex<Vec<([u8; 32], CallSignal)>>>;

    /// What each side sent, and what was done on the screen.
    struct Lab {
        sent: Sent,
        done: Arc<Mutex<Vec<String>>>,
        rc: RemoteControl,
    }

    struct Fake(Arc<Mutex<Vec<String>>>, Arc<Mutex<Option<Instant>>>);

    impl Injector for Fake {
        fn pointer(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
            self.0.locked().push(format!("pointer {x:.2} {y:.2}"));
            Ok(())
        }
        fn button(&mut self, button: u8, down: bool) -> anyhow::Result<()> {
            self.0.locked().push(format!("button {button} {down}"));
            Ok(())
        }
        fn wheel(&mut self, dx: i32, dy: i32) -> anyhow::Result<()> {
            self.0.locked().push(format!("wheel {dx} {dy}"));
            Ok(())
        }
        fn key(&mut self, key: char, down: bool) -> anyhow::Result<()> {
            self.0.locked().push(format!("key {key:?} {down}"));
            Ok(())
        }
        fn text(&mut self, text: &str) -> anyhow::Result<()> {
            self.0.locked().push(format!("text {text}"));
            Ok(())
        }
        fn local_input_at(&self) -> Option<Instant> {
            *self.1.locked()
        }
    }

    fn lab(can: bool) -> (Lab, Arc<Mutex<Option<Instant>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(Mutex::new(Vec::new()));
        let local = Arc::new(Mutex::new(None));
        let s = sent.clone();
        let (d, l) = (done.clone(), local.clone());
        let opener: Opener = Arc::new(move || if can { Ok(Box::new(Fake(d.clone(), l.clone())) as Box<dyn Injector>) } else { Err("no way in here".to_string()) });
        let rc = RemoteControl::new(Arc::new(move |k, sig| s.locked().push((k, sig))), opener);
        (Lab { sent, done, rc }, local)
    }

    impl Lab {
        fn sent(&self) -> Vec<([u8; 32], CallSignal)> {
            std::mem::take(&mut self.sent.locked())
        }
        fn done(&self) -> Vec<String> {
            std::mem::take(&mut self.done.locked())
        }
        fn input(&mut self, from: [u8; 32], input: Input) {
            self.rc.on_signal(from, "them", CallSignal::Input(input), SCREEN);
        }
    }

    fn ctl(c: Control) -> CallSignal {
        CallSignal::Control(c)
    }

    #[test]
    fn nothing_is_done_on_the_screen_until_it_is_allowed_and_only_for_the_one_allowed() {
        let (mut lab, _) = lab(true);
        lab.input(ANN, Input::Text("unasked".into()));
        assert!(lab.rc.on_signal(ANN, "Ann", ctl(Control::Ask), SCREEN).is_some());
        assert_eq!(lab.rc.asking(), Some(ANN));
        lab.input(ANN, Input::Text("asked".into()));
        assert!(lab.done().is_empty(), "nothing done before it's allowed");
        lab.rc.allow(ANN, true);
        assert_eq!(lab.sent(), vec![(ANN, ctl(Control::Allow))]);
        lab.input(ANN, Input::Text("allowed".into()));
        lab.input(BOB, Input::Text("not Bob".into()));
        assert_eq!(lab.done(), vec!["text allowed"]);
    }

    #[test]
    fn allowing_another_stops_whoever_was_controlling_and_lets_go_of_what_they_held() {
        let (mut lab, _) = lab(true);
        for k in [ANN, BOB] {
            lab.rc.on_signal(k, "them", ctl(Control::Ask), SCREEN);
        }
        lab.rc.allow(ANN, true);
        lab.input(ANN, Input::Key { key: keys::CONTROL, down: true });
        lab.input(ANN, Input::Button { button: 0, down: true });
        lab.done();
        lab.sent();
        lab.rc.allow(BOB, true);
        assert_eq!(lab.sent(), vec![(ANN, ctl(Control::Stop)), (BOB, ctl(Control::Allow))]);
        let mut released = lab.done();
        released.sort();
        assert_eq!(released, vec!["button 0 false", "key '\\u{11}' false"]);
        lab.input(ANN, Input::Text("Ann".into()));
        lab.input(BOB, Input::Text("Bob".into()));
        assert_eq!(lab.done(), vec!["text Bob"]);
    }

    #[test]
    fn stopping_sharing_stops_the_controller_and_refuses_those_asking() {
        let (mut lab, _) = lab(true);
        lab.rc.on_signal(ANN, "Ann", ctl(Control::Ask), SCREEN);
        lab.rc.on_signal(BOB, "Bob", ctl(Control::Ask), SCREEN);
        lab.rc.allow(ANN, true);
        lab.input(ANN, Input::Key { key: 'x', down: true });
        lab.sent();
        lab.done();
        lab.rc.sharing_ended();
        let sent = lab.sent();
        assert_eq!(sent[0], (ANN, ctl(Control::Stop)));
        assert!(matches!(&sent[1], (k, CallSignal::Control(Control::Refuse(_))) if *k == BOB));
        assert_eq!(lab.done(), vec!["key 'x' false"]);
        assert!(lab.rc.controller().is_none() && lab.rc.asking().is_none());
        let note = lab.rc.on_signal(ANN, "Ann", ctl(Control::Ask), None).unwrap();
        assert_eq!(note.text, "Ann asks to see and control your screen");
        assert!(lab.sent().is_empty());
        assert!(lab.rc.wants_screen(ANN));
        lab.rc.sharing_ended();
        assert!(lab.sent().is_empty() && lab.rc.asking() == Some(ANN));
        lab.rc.allow(ANN, true);
        assert_eq!(lab.sent(), vec![(ANN, ctl(Control::Allow))]);
        assert!(!lab.rc.wants_screen(ANN));
        lab.rc.stop();
        lab.rc.on_signal(BOB, "Bob", ctl(Control::Ask), None);
        lab.sent();
        lab.rc.refuse_because(BOB, "couldn't share the screen");
        assert_eq!(lab.sent(), vec![(BOB, ctl(Control::Refuse("couldn't share the screen".into())))]);
        assert!(lab.rc.asking().is_none() && !lab.rc.wants_screen(BOB));
    }

    #[test]
    fn a_desktop_that_cant_be_controlled_says_so_to_whoever_asks() {
        let (mut lab, _) = lab(false);
        let note = lab.rc.on_signal(ANN, "Ann", ctl(Control::Ask), SCREEN).unwrap();
        assert!(note.text.contains("Ann asked to control your screen"), "{}", note.text);
        match &lab.sent()[..] {
            [(k, CallSignal::Control(Control::Refuse(why)))] => assert!(*k == ANN && why.contains("no way in here")),
            other => panic!("{other:?}"),
        }
        assert!(lab.rc.asking().is_none());
    }

    /// Asks the desktop only once allowed (not when asked, to see if it
    /// can), and the desktop taking control back stops it.
    #[test]
    fn the_desktop_is_asked_only_once_allowed_and_can_take_control_back() {
        struct Asks(Arc<Mutex<Vec<&'static str>>>, Arc<Mutex<Option<String>>>);
        impl Injector for Asks {
            fn pointer(&mut self, _: f64, _: f64) -> anyhow::Result<()> {
                Ok(())
            }
            fn button(&mut self, _: u8, _: bool) -> anyhow::Result<()> {
                Ok(())
            }
            fn wheel(&mut self, _: i32, _: i32) -> anyhow::Result<()> {
                Ok(())
            }
            fn key(&mut self, _: char, _: bool) -> anyhow::Result<()> {
                Ok(())
            }
            fn text(&mut self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn start(&mut self) -> Option<String> {
                self.0.locked().push("start");
                Some("allow it in your desktop's dialog too".to_string())
            }
            fn ended(&self) -> Option<String> {
                self.1.locked().clone()
            }
        }
        let (started, ended) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None)));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (st, en, se) = (started.clone(), ended.clone(), sent.clone());
        let opener: Opener = Arc::new(move || Ok(Box::new(Asks(st.clone(), en.clone())) as Box<dyn Injector>));
        let mut rc = RemoteControl::new(Arc::new(move |k, sig| se.locked().push((k, sig))), opener);
        rc.on_signal(ANN, "Ann", ctl(Control::Ask), SCREEN);
        assert!(started.locked().is_empty(), "the desktop asked before it was allowed here");
        let note = rc.allow(ANN, true).expect("told to allow it in the desktop's dialog");
        assert!(note.text.contains("dialog"), "{}", note.text);
        assert_eq!(*started.locked(), vec!["start"]);
        assert!(rc.tick(|_| false).is_none());
        sent.locked().clear();
        *ended.locked() = Some("your desktop stopped it".to_string());
        let note = rc.tick(|_| false).expect("a note");
        assert!(note.text.contains("your desktop stopped it"), "{}", note.text);
        assert_eq!(*sent.locked(), vec![(ANN, ctl(Control::Stop))]);
        assert!(rc.controller().is_none());
    }

    #[test]
    fn using_your_own_mouse_pauses_the_controller_and_lets_go_of_what_they_held() {
        let (mut lab, local) = lab(true);
        lab.rc.on_signal(ANN, "Ann", ctl(Control::Ask), SCREEN);
        lab.rc.allow(ANN, true);
        lab.input(ANN, Input::Button { button: 0, down: true });
        lab.sent();
        lab.done();
        *local.locked() = Some(Instant::now());
        lab.input(ANN, Input::Pointer { x: 0, y: 0 });
        lab.input(ANN, Input::Text("ignored".into()));
        assert_eq!(lab.sent(), vec![(ANN, ctl(Control::Paused(true)))]);
        assert_eq!(lab.done(), vec!["button 0 false"]);
        assert_eq!(lab.rc.controller(), Some((ANN, true)));
        *local.locked() = Some(Instant::now() - PAUSE_AFTER_LOCAL);
        lab.rc.controller.as_mut().unwrap().paused_until = Some(Instant::now());
        lab.rc.tick(|_| false);
        assert_eq!(lab.sent(), vec![(ANN, ctl(Control::Paused(false)))]);
        lab.input(ANN, Input::Text("back".into()));
        assert_eq!(lab.done(), vec!["text back"]);
    }

    #[test]
    fn the_one_watching_asks_and_sends_only_once_allowed() {
        let (mut lab, _) = lab(true);
        lab.rc.key('a', true);
        lab.rc.ask(ANN);
        lab.rc.key('a', true);
        assert_eq!(lab.sent(), vec![(ANN, ctl(Control::Ask))]);
        assert_eq!(lab.rc.watching(), Some((ANN, Watching::Asking)));
        lab.rc.on_signal(BOB, "Bob", ctl(Control::Allow), None);
        assert_eq!(lab.sent(), vec![(BOB, ctl(Control::GiveBack))]);
        lab.rc.on_signal(ANN, "Ann", ctl(Control::Allow), None);
        assert_eq!(lab.rc.controlling(), Some(ANN));
        lab.rc.key('a', true);
        lab.rc.key('a', false);
        assert_eq!(lab.sent(), vec![(ANN, CallSignal::Input(Input::Text("a".into())))]);
        lab.rc.on_signal(ANN, "Ann", ctl(Control::Stop), None);
        assert_eq!(lab.rc.watching(), None);
        lab.rc.ask(ANN);
        lab.rc.on_signal(ANN, "Ann", ctl(Control::Allow), None);
        lab.sent();
        lab.rc.tick(|_| true);
        lab.rc.tick(|_| false);
        assert_eq!(lab.sent(), vec![(ANN, ctl(Control::GiveBack))]);
        assert_eq!(lab.rc.watching(), None);
    }

    /// Asked before their screen was shared: waited for while they
    /// choose it, and after they allow it, for `SCREEN_WAIT`.
    #[test]
    fn a_screen_not_yet_shared_is_waited_for_then_given_up_on() {
        let (mut lab, _) = lab(true);
        lab.rc.ask(ANN);
        lab.sent();
        lab.rc.tick(|_| false);
        assert_eq!(lab.rc.watching(), Some((ANN, Watching::Asking)), "still asking while it's not shared");
        lab.rc.on_signal(ANN, "Ann", ctl(Control::Allow), None);
        lab.rc.tick(|_| false);
        assert_eq!(lab.rc.controlling(), Some(ANN), "the screen's on its way");
        lab.rc.allowed_at = Some(Instant::now() - SCREEN_WAIT);
        lab.rc.tick(|_| false);
        assert_eq!(lab.sent(), vec![(ANN, ctl(Control::GiveBack))]);
        assert_eq!(lab.rc.watching(), None);
    }

    #[test]
    fn characters_go_as_text_and_shortcuts_and_named_keys_as_keys() {
        let (mut lab, _) = lab(true);
        lab.rc.ask(ANN);
        lab.rc.on_signal(ANN, "Ann", ctl(Control::Allow), None);
        lab.sent();
        let mut press = |k: char| {
            lab.rc.key(k, true);
            lab.rc.key(k, false);
        };
        press('£');
        press(keys::RETURN);
        press(keys::ALT_GR);
        let ins = |rc: &mut RemoteControl| {
            rc.key(keys::SHIFT, true);
            rc.key('A', true);
            rc.key('A', false);
            rc.key(keys::SHIFT, false);
            rc.key(keys::CONTROL, true);
            rc.key('C', true);
            rc.key('C', false);
            rc.key(keys::CONTROL, false);
        };
        ins(&mut lab.rc);
        let k = |key, down| CallSignal::Input(Input::Key { key, down });
        let t = |s: &str| CallSignal::Input(Input::Text(s.into()));
        let sent: Vec<CallSignal> = lab.sent().into_iter().map(|(_, s)| s).collect();
        assert_eq!(
            sent,
            vec![
                t("£"),
                k(keys::RETURN, true),
                k(keys::RETURN, false),
                k(keys::SHIFT, true),
                t("A"),
                k(keys::SHIFT, false),
                k(keys::CONTROL, true),
                k('c', true),
                k('c', false),
                k(keys::CONTROL, false),
            ]
        );
    }

    #[test]
    fn the_pointer_is_paced_newest_wins_and_goes_before_a_click() {
        let (mut lab, _) = lab(true);
        lab.rc.ask(ANN);
        lab.rc.on_signal(ANN, "Ann", ctl(Control::Allow), None);
        lab.sent();
        lab.rc.pointer(0.0, 0.0);
        lab.rc.pointer(0.25, 0.25);
        lab.rc.pointer(0.5, 0.5);
        lab.rc.button(0, true);
        let p = |x, y| CallSignal::Input(Input::Pointer { x, y });
        let sent: Vec<CallSignal> = lab.sent().into_iter().map(|(_, s)| s).collect();
        assert_eq!(sent, vec![p(0, 0), p(32768, 32768), CallSignal::Input(Input::Button { button: 0, down: true })]);
        lab.rc.pointer(1.0, 1.0);
        std::thread::sleep(POINTER_EVERY * 3);
        let sent: Vec<CallSignal> = lab.sent().into_iter().map(|(_, s)| s).collect();
        assert_eq!(sent, vec![p(65535, 65535)]);
    }

    #[test]
    fn a_point_on_the_picture_is_the_same_point_on_the_screen_bars_taken_off() {
        let near = |(x, y): (f64, f64), (ex, ey): (f64, f64)| (x - ex).abs() < 0.001 && (y - ey).abs() < 0.001;
        assert!(near(on_screen(32768, 16384, 1920, 1080), (0.5, 0.25)));
        let bar = (1.0 - (16.0 / 10.0) / (16.0 / 9.0)) / 2.0;
        assert!(near(on_screen((bar * 65535.0) as u16, 0, 1920, 1200), (0.0, 0.0)));
        assert!(near(on_screen(32768, 65535, 1920, 1200), (0.5, 1.0)));
        assert!(near(on_screen(0, 32768, 1920, 1200), (0.0, 0.5)), "a point on a bar is the nearest edge");
        assert!(near(on_screen(65535, 16384, 5120, 1440), (1.0, 0.0)));
        assert!(near(on_screen(0, 32768, 5120, 1440), (0.0, 0.5)));
    }
}
