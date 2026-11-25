//! Being controlled on X11 (see `remote`): what's sent is done through
//! the XTEST extension, on the monitor that's shared (`screen::x11`).
//!
//! - Characters typed are put in with the keys this keyboard has for
//!   them, Shift (and Caps Lock) taken into account. One it hasn't is put
//!   on a key that has nothing, for as long as it's needed, as xdotool
//!   does: so a UK `£` comes out as `£` on a US keyboard.
//! - Your own mouse and keys are told from ours by XInput2: what XTEST
//!   does comes from its own devices ("Virtual core XTEST pointer").
//!
//! All this on a connection of its own, with a thread watching a second
//! one for your input and a changed keyboard layout. And `keep_above`
//! keeps the bar saying you're being controlled over everything else.

use swarm_core::lock::Locked;
use crate::remote::Injector;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use swarm_core::remote::{keys, BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT};
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, Keycode, Keysym, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// How many keys with nothing on them are borrowed for characters this
/// keyboard hasn't, at most.
const SPARES: usize = 8;
/// After a key's given a character, how long before it's pressed: the
/// programs on the desktop read the new layout first.
const REMAPPED_SETTLE: Duration = Duration::from_millis(20);

const SHIFT_L: Keysym = 0xffe1;
const ESCAPE: Keysym = 0xff1b;

/// What the watching thread finds, for the injector.
#[derive(Default)]
struct Seen {
    /// Our own mouse or keys were last used then.
    local_at: Mutex<Option<Instant>>,
    /// The keyboard's layout changed (or we changed it): read again.
    remapped: AtomicBool,
    /// The injector's gone: the thread ends.
    done: AtomicBool,
}

pub struct X11Injector {
    conn: RustConnection,
    root: Window,
    area: (i16, i16, u16, u16),
    first: Keycode,
    per: usize,
    /// Each keycode's keysyms from `first`, `per` each.
    map: Vec<Keysym>,
    /// Keys with nothing on them, ours to borrow, and what we've put on
    /// each, with when it was last used.
    spares: Vec<(Keycode, Keysym, u64)>,
    used: u64,
    /// The keys pressed for them, by what was sent, and the keycode
    /// pressed for each (let go the same, whatever's changed since).
    pressed: HashMap<char, Keycode>,
    buttons: Vec<u8>,
    /// Wheel turned short of a notch, in 120ths, across and down.
    wheel: (i32, i32),
    seen: Arc<Seen>,
    watcher: Option<std::thread::JoinHandle<()>>,
}

impl X11Injector {
    /// On the display `display` names (`None`: `DISPLAY`).
    pub fn open(display: Option<&str>) -> Result<X11Injector> {
        let (conn, screen) = x11rb::connect(display).context("connecting to the X11 display")?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)?.reply().map_err(|_| anyhow!("the X11 display hasn't the XTEST extension"))?;
        let area = crate::screen::x11::shared_area(&conn, screen);
        let (first, last) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let mut injector = X11Injector {
            conn,
            root,
            area,
            first,
            per: 0,
            map: Vec::new(),
            spares: Vec::new(),
            used: 0,
            pressed: HashMap::new(),
            buttons: Vec::new(),
            wheel: (0, 0),
            seen: Arc::new(Seen::default()),
            watcher: None,
        };
        injector.read_map()?;
        injector.spares = (first..=last).rev().filter(|&code| injector.keysyms(code).iter().all(|&s| s == 0)).take(SPARES).map(|code| (code, 0, 0)).collect();
        let (watch, seen) = (x11rb::connect(display).context("connecting to the X11 display")?.0, injector.seen.clone());
        injector.watcher = Some(std::thread::Builder::new().name("x11-control".into()).spawn(move || {
            if let Err(e) = watch_input(watch, screen, &seen) {
                eprintln!("{} watching for your own input: {e:#}", crate::unix_now());
            }
        })?);
        Ok(injector)
    }

    fn read_map(&mut self) -> Result<()> {
        let (first, last) = (self.conn.setup().min_keycode, self.conn.setup().max_keycode);
        let reply = self.conn.get_keyboard_mapping(first, last - first + 1)?.reply()?;
        self.per = reply.keysyms_per_keycode as usize;
        self.map = reply.keysyms;
        Ok(())
    }

    fn keysyms(&self, code: Keycode) -> &[Keysym] {
        let at = (code - self.first) as usize * self.per;
        self.map.get(at..at + self.per).unwrap_or(&[])
    }

    /// The key that types `keysym`, and whether with Shift: the first
    /// group's two levels only.
    fn find(&self, keysym: Keysym) -> Option<(Keycode, bool)> {
        let last = self.conn.setup().max_keycode;
        [false, true].into_iter().find_map(|shift| (self.first..=last).find(|&code| self.keysyms(code).get(shift as usize) == Some(&keysym)).map(|code| (code, shift)))
    }

    /// The key for `keysym`, borrowing a spare if this keyboard has none
    /// (`upper`: its upper case, put with Shift, if it has one).
    fn key_for(&mut self, keysym: Keysym, upper: Option<Keysym>) -> Result<(Keycode, bool)> {
        if self.seen.remapped.swap(false, Ordering::Relaxed) {
            self.read_map()?;
        }
        if let Some(found) = self.find(keysym) {
            return Ok(found);
        }
        self.used += 1;
        let pressed: Vec<Keycode> = self.pressed.values().copied().collect();
        let used = self.used;
        let Some(spare) = self.spares.iter_mut().filter(|(code, _, _)| !pressed.contains(code)).min_by_key(|(_, _, last)| *last) else {
            bail!("no key free to put {keysym:#x} on");
        };
        spare.1 = keysym;
        spare.2 = used;
        let code = spare.0;
        let syms: Vec<Keysym> = (0..self.per).map(|i| if i % 2 == 1 { upper.unwrap_or(keysym) } else { keysym }).collect();
        self.conn.change_keyboard_mapping(1, code, self.per as u8, &syms)?.check()?;
        let at = (code - self.first) as usize * self.per;
        self.map[at..at + self.per].copy_from_slice(&syms);
        std::thread::sleep(REMAPPED_SETTLE);
        Ok((code, false))
    }

    fn fake(&self, kind: u8, detail: u8) -> Result<()> {
        self.conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, 0, 0, 0)?;
        Ok(())
    }

    /// Shift and Caps Lock as they are now.
    fn shift_and_lock(&self) -> Result<(bool, bool)> {
        let mask = u16::from(self.conn.query_pointer(self.root)?.reply()?.mask);
        Ok((mask & u16::from(xproto::KeyButMask::SHIFT) != 0, mask & u16::from(xproto::KeyButMask::LOCK) != 0))
    }

    /// Types one character.
    fn type_char(&mut self, c: char) -> Result<()> {
        let (lower, upper) = (single(c.to_lowercase()), single(c.to_uppercase()));
        let cased = lower != '\0' && upper != '\0' && lower != upper;
        let (code, shifted) = if cased {
            let (code, _) = self.key_for(keysym(lower), Some(keysym(upper)))?;
            (code, c == upper)
        } else {
            self.key_for(keysym(c), None)?
        };
        let (shift, lock) = self.shift_and_lock()?;
        let want = shifted ^ (lock && cased);
        let held: Vec<Keycode> = [keys::SHIFT, keys::SHIFT_R].iter().filter_map(|k| self.pressed.get(k).copied()).collect();
        let (press, let_go) = match (want, shift) {
            (true, false) => (self.find(SHIFT_L).map(|(code, _)| code), Vec::new()),
            (false, true) => (None, held),
            _ => (None, Vec::new()),
        };
        for &code in &let_go {
            self.fake(xproto::KEY_RELEASE_EVENT, code)?;
        }
        if let Some(shift) = press {
            self.fake(xproto::KEY_PRESS_EVENT, shift)?;
        }
        self.fake(xproto::KEY_PRESS_EVENT, code)?;
        self.fake(xproto::KEY_RELEASE_EVENT, code)?;
        if let Some(shift) = press {
            self.fake(xproto::KEY_RELEASE_EVENT, shift)?;
        }
        for &code in &let_go {
            self.fake(xproto::KEY_PRESS_EVENT, code)?;
        }
        Ok(())
    }
}

/// A character's keysym: Latin-1 as it is, the rest Unicode's.
pub(crate) fn keysym(c: char) -> Keysym {
    match c as u32 {
        cp @ (0x20..=0x7e | 0xa0..=0xff) => cp,
        cp => 0x0100_0000 | cp,
    }
}

/// A case change that's one character, or the character as it was.
fn single(mut chars: impl Iterator<Item = char> + Clone) -> char {
    let first = chars.next();
    match (first, chars.next()) {
        (Some(c), None) => c,
        _ => '\0',
    }
}

/// The keysym of a key that types nothing (see `keys`), or `None`.
pub(crate) fn named(key: char) -> Option<Keysym> {
    Some(match key {
        keys::BACKSPACE => 0xff08,
        keys::TAB => 0xff09,
        keys::RETURN => 0xff0d,
        keys::ESCAPE => ESCAPE,
        keys::DELETE => 0xffff,
        keys::SHIFT => SHIFT_L,
        keys::SHIFT_R => 0xffe2,
        keys::CONTROL => 0xffe3,
        keys::CONTROL_R => 0xffe4,
        keys::ALT => 0xffe9,
        keys::META => 0xffeb,
        keys::META_R => 0xffec,
        keys::SPACE => 0x20,
        keys::UP => 0xff52,
        keys::DOWN => 0xff54,
        keys::LEFT => 0xff51,
        keys::RIGHT => 0xff53,
        keys::INSERT => 0xff63,
        keys::HOME => 0xff50,
        keys::END => 0xff57,
        keys::PAGE_UP => 0xff55,
        keys::PAGE_DOWN => 0xff56,
        keys::SCROLL_LOCK => 0xff14,
        keys::PAUSE => 0xff13,
        keys::PRINT_SCREEN => 0xff61,
        keys::MENU => 0xff67,
        k if (keys::F1..=keys::F24).contains(&k) => 0xffbe + (k as u32 - keys::F1 as u32),
        _ => return None,
    })
}

impl Injector for X11Injector {
    fn pointer(&mut self, x: f64, y: f64) -> Result<()> {
        let (left, top, width, height) = self.area;
        let x = left as f64 + (x * (width.max(1) - 1) as f64).round();
        let y = top as f64 + (y * (height.max(1) - 1) as f64).round();
        self.conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 0, x11rb::CURRENT_TIME, self.root, x as i16, y as i16, 0)?;
        self.conn.flush()?;
        Ok(())
    }

    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let detail = match button {
            BUTTON_LEFT => 1,
            BUTTON_MIDDLE => 2,
            BUTTON_RIGHT => 3,
            BUTTON_BACK => 8,
            BUTTON_FORWARD => 9,
            _ => return Ok(()),
        };
        if down == self.buttons.contains(&detail) {
            return Ok(());
        }
        if down {
            self.buttons.push(detail);
        } else {
            self.buttons.retain(|b| *b != detail);
        }
        self.fake(if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT }, detail)?;
        self.conn.flush()?;
        Ok(())
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.wheel.0 += dx;
        self.wheel.1 += dy;
        let notches = |turned: &mut i32| {
            let n = *turned / 120;
            *turned -= n * 120;
            n
        };
        let (across, down) = (notches(&mut self.wheel.0), notches(&mut self.wheel.1));
        for (n, more, less) in [(down, 4, 5), (across, 7, 6)] {
            let button = if n > 0 { more } else { less };
            for _ in 0..n.unsigned_abs() {
                self.fake(xproto::BUTTON_PRESS_EVENT, button)?;
                self.fake(xproto::BUTTON_RELEASE_EVENT, button)?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    fn key(&mut self, key: char, down: bool) -> Result<()> {
        if down {
            if self.pressed.contains_key(&key) {
                return Ok(());
            }
            let (code, _) = match named(key) {
                Some(keysym) => self.key_for(keysym, None)?,
                None => self.key_for(keysym(key), None)?,
            };
            self.pressed.insert(key, code);
            self.fake(xproto::KEY_PRESS_EVENT, code)?;
        } else if let Some(code) = self.pressed.remove(&key) {
            self.fake(xproto::KEY_RELEASE_EVENT, code)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn text(&mut self, text: &str) -> Result<()> {
        for c in text.chars().filter(|c| !c.is_control()) {
            self.type_char(c)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn local_input_at(&self) -> Option<Instant> {
        *self.seen.local_at.locked()
    }
}

impl Drop for X11Injector {
    fn drop(&mut self) {
        for code in std::mem::take(&mut self.pressed).into_values() {
            let _ = self.fake(xproto::KEY_RELEASE_EVENT, code);
        }
        for button in std::mem::take(&mut self.buttons) {
            let _ = self.fake(xproto::BUTTON_RELEASE_EVENT, button);
        }
        let empty = vec![0; self.per];
        for (code, keysym, _) in &self.spares {
            if *keysym != 0 {
                let _ = self.conn.change_keyboard_mapping(1, *code, self.per as u8, &empty);
            }
        }
        let _ = self.conn.sync();
        self.seen.done.store(true, Ordering::Relaxed);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// Watches `conn` until the injector's gone: your own mouse and keys
/// (anything not from XTEST's devices), and layouts changed.
fn watch_input(conn: RustConnection, screen: usize, seen: &Seen) -> Result<()> {
    let root = conn.setup().roots[screen].root;
    let mut ours = Vec::new();
    if conn.xinput_xi_query_version(2, 0)?.reply().is_ok() {
        ours = conn.xinput_xi_query_device(0u16)?.reply()?.infos.into_iter().filter(|d| d.name.windows(5).any(|w| w == b"XTEST")).map(|d| d.deviceid).collect();
        let mask = xinput::XIEventMask::RAW_MOTION | xinput::XIEventMask::RAW_BUTTON_PRESS | xinput::XIEventMask::RAW_KEY_PRESS;
        conn.xinput_xi_select_events(root, &[xinput::EventMask { deviceid: 0, mask: vec![mask] }])?.check()?;
    } else {
        eprintln!("{} the X11 display hasn't XInput2: your own mouse won't pause whoever's controlling", crate::unix_now());
    }
    conn.flush()?;
    let fd = conn.stream().as_raw_fd();
    while !seen.done.load(Ordering::Relaxed) {
        while let Some(event) = conn.poll_for_event()? {
            let source = match &event {
                Event::XinputRawMotion(e) => Some(e.sourceid),
                Event::XinputRawButtonPress(e) => Some(e.sourceid),
                Event::XinputRawKeyPress(e) => Some(e.sourceid),
                Event::MappingNotify(_) => {
                    seen.remapped.store(true, Ordering::Relaxed);
                    None
                }
                _ => None,
            };
            if source.is_some_and(|s| !ours.contains(&s)) {
                *seen.local_at.locked() = Some(Instant::now());
            }
        }
        let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd, ours, for the length of the call.
        unsafe { libc::poll(&mut poll, 1, 100) };
    }
    Ok(())
}

/// Whether Ctrl is held, as the X server has it now: for Ctrl and the
/// wheel in the window. winit's own idea of it (its ModifiersChanged, and
/// its key events) wasn't to be relied on on a GNOME desktop: wrong from
/// the start, and again once the wheel had scrolled something.
pub struct HeldKeys {
    conn: RustConnection,
    root: Window,
}

impl HeldKeys {
    pub fn open(display: Option<&str>) -> Result<HeldKeys> {
        let (conn, screen) = x11rb::connect(display).context("connecting to the X11 display")?;
        let root = conn.setup().roots[screen].root;
        Ok(HeldKeys { conn, root })
    }

    pub fn control(&self) -> Result<bool> {
        let mask = self.conn.query_pointer(self.root)?.reply()?.mask;
        Ok(mask.contains(xproto::KeyButMask::CONTROL))
    }
}

/// Asks the window manager to keep the X11 window `window` above every
/// other, on every workspace, and out of the taskbar. Asked once it's
/// shown: asked as it's mapped (as winit does), GNOME missed it, and the
/// bar went behind whatever was brought forward.
pub fn keep_above(window: u32) -> Result<()> {
    let (conn, screen) = x11rb::connect(None).context("connecting to the X11 display")?;
    let root = conn.setup().roots[screen].root;
    let atom = |name: &str| -> Result<u32> { Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom) };
    let state = atom("_NET_WM_STATE")?;
    for (a, b) in [("_NET_WM_STATE_ABOVE", "_NET_WM_STATE_STICKY"), ("_NET_WM_STATE_SKIP_TASKBAR", "_NET_WM_STATE_SKIP_PAGER")] {
        let event = xproto::ClientMessageEvent::new(32, window, state, [1, atom(a)?, atom(b)?, 1, 0]);
        conn.send_event(false, root, xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY, event)?;
    }
    conn.configure_window(window, &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE))?;
    conn.sync()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::{CreateWindowAux, EventMask, InputFocus, WindowClass};

    /// A display of our own (never the one in use), while it lasts.
    struct Xvfb(std::process::Child, String);

    impl Xvfb {
        /// `None` without Xvfb here.
        fn start() -> Option<Xvfb> {
            use std::io::BufRead;
            let mut child = std::process::Command::new("Xvfb")
                .args(["-displayfd", "1", "-screen", "0", "1280x720x24", "-nolisten", "tcp"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
            let xvfb = Xvfb(child, format!(":{}", line.trim()));
            (!line.trim().is_empty()).then_some(xvfb)
        }
    }

    impl Drop for Xvfb {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// What's typed into a window covering the screen, as characters.
    fn typed(conn: &RustConnection, until: usize) -> String {
        let mut text = String::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while text.chars().count() < until && Instant::now() < deadline {
            match conn.poll_for_event().unwrap() {
                Some(Event::KeyPress(e)) => {
                    let (first, last) = (conn.setup().min_keycode, conn.setup().max_keycode);
                    let map = conn.get_keyboard_mapping(first, last - first + 1).unwrap().reply().unwrap();
                    let at = (e.detail - first) as usize * map.keysyms_per_keycode as usize;
                    let shift = u16::from(e.state) & u16::from(xproto::KeyButMask::SHIFT) != 0;
                    let sym = map.keysyms[at + shift as usize];
                    if let Some(c) = char::from_u32(if sym >= 0x0100_0000 { sym - 0x0100_0000 } else { sym }).filter(|_| !(0xff00..0x0100_0000).contains(&sym)) {
                        text.push(c);
                    }
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        text
    }

    /// Ctrl held, as the server has it, whoever pressed it.
    #[test]
    fn ctrl_held_is_read_from_the_server() {
        let Some(xvfb) = Xvfb::start() else {
            eprintln!("no Xvfb here: skipped");
            return;
        };
        let held = HeldKeys::open(Some(&xvfb.1)).unwrap();
        assert!(!held.control().unwrap());
        let mut injector = X11Injector::open(Some(&xvfb.1)).unwrap();
        injector.key(keys::CONTROL, true).unwrap();
        injector.conn.sync().unwrap();
        assert!(held.control().unwrap(), "held");
        injector.key(keys::CONTROL, false).unwrap();
        injector.conn.sync().unwrap();
        assert!(!held.control().unwrap(), "let go");
    }

    #[test]
    fn what_is_sent_is_done_on_an_x11_screen_and_not_taken_for_your_own() {
        let Some(xvfb) = Xvfb::start() else {
            eprintln!("no Xvfb here: skipped");
            return;
        };
        let (conn, screen) = x11rb::connect(Some(&xvfb.1)).unwrap();
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().unwrap();
        let events = EventMask::KEY_PRESS | EventMask::BUTTON_PRESS;
        conn.create_window(0, window, root, 0, 0, 1280, 720, 0, WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new().event_mask(events)).unwrap();
        conn.map_window(window).unwrap();
        conn.set_input_focus(InputFocus::POINTER_ROOT, window, x11rb::CURRENT_TIME).unwrap();
        conn.sync().unwrap();

        let mut injector = X11Injector::open(Some(&xvfb.1)).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        injector.pointer(0.5, 0.25).unwrap();
        injector.conn.sync().unwrap();
        let at = conn.query_pointer(root).unwrap().reply().unwrap();
        assert_eq!((at.root_x, at.root_y), (640, 180));
        injector.button(BUTTON_RIGHT, true).unwrap();
        injector.button(BUTTON_RIGHT, false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let button = loop {
            match conn.poll_for_event().unwrap() {
                Some(Event::ButtonPress(e)) => break e.detail,
                Some(_) => {}
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                None => panic!("no click"),
            }
        };
        assert_eq!(button, 3);

        injector.text("aB £1!").unwrap();
        assert_eq!(typed(&conn, 6), "aB £1!");
        injector.key(keys::SHIFT, true).unwrap();
        injector.text("x").unwrap();
        injector.text("Y").unwrap();
        injector.key(keys::SHIFT, false).unwrap();
        assert_eq!(typed(&conn, 2), "xY");

        std::thread::sleep(Duration::from_millis(200));
        assert!(injector.local_input_at().is_none(), "our own input taken for theirs");

        drop(injector);
        conn.sync().unwrap();
        let (first, last) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let map = conn.get_keyboard_mapping(first, last - first + 1).unwrap().reply().unwrap();
        assert!(!map.keysyms.contains(&0xa3));
    }
}
