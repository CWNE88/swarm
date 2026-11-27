//! Being controlled on a Wayland desktop: Wayland lets no program put in
//! another's mouse or keys, so it's asked of the desktop's portal
//! (`org.freedesktop.portal.RemoteDesktop`, over D-Bus), which asks the
//! one sharing in a dialog of its own, then does what it's sent.
//!
//! The portal turns swarm itself down (see `portal_helper`), so its
//! session is held, and what's sent done, by the portal helper: swarm
//! works out what to press (keysyms, Shift, wheel notches) and sends it
//! there a line at a time.
//!
//! The session is asked for once control's been allowed here
//! (`Injector::start`), on a thread of its own, since its dialog waits
//! for an answer; what's sent before the desktop's allowed it too is
//! dropped. Like the screen's, the choice made in the dialog is
//! remembered (the restore token, kept in the settings file), so it's
//! only asked once, unless the desktop forgets it.
//!
//! The pointer's put where it's sent on a screen-cast stream of the
//! session's own (the portal's only way to place it absolutely), which
//! is never read: the pictures still come from the screen being shared.
//! So the monitor chosen in this dialog should be the one shared.
//!
//! Keys go as keysyms, which the desktop types with its own layout's
//! keys (Shift and all); a character the layout hasn't is lost. The
//! desktop doesn't say which input is whose, so your own mouse doesn't
//! pause the one controlling here. Stopped from the desktop's side (its
//! own "stop" in the top bar), the session closes, and so does the
//! control (`Injector::ended`).

use swarm_core::lock::Locked;
use crate::portal::{token, DESTINATION, PATH};
use crate::portal_helper::{Helper, Kind};
use crate::remote::Injector;
use crate::remote_x11::{keysym, named};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::BufRead;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use swarm_core::remote::{keys, BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT};
use tokio::sync::mpsc;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const SESSION: &str = "org.freedesktop.portal.Session";
const KEYBOARD: u32 = 1;
const POINTER: u32 = 2;
/// A monitor (not a window) for the session's stream.
const MONITOR: u32 = 1;
/// The choice kept until it's revoked.
const PERSIST: u32 = 2;
/// Where the portal's restore token is kept.
const RESTORE_KEY: &str = "control_restore";
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;
const BTN_SIDE: i32 = 0x113;
const BTN_EXTRA: i32 = 0x114;

/// Something to do, sent to the session's thread.
enum Do {
    Pointer(f64, f64),
    Button(u8, bool),
    Wheel(i32, i32),
    Key(char, bool),
    Text(String),
}

/// What the session's thread shares with the injector.
#[derive(Default)]
struct Shared {
    /// The helper, while the desktop's dialog is up: ended from here
    /// (which takes the dialog down) if control ends first.
    asking: AtomicU32,
    /// Why the session's over, once it is.
    ended: Mutex<Option<String>>,
    /// The desktop's allowed it: what's sent from now on is done.
    ready: AtomicBool,
    /// Control's been stopped here: nothing still queued is done.
    stopped: AtomicBool,
}

impl Shared {
    fn end(&self, why: impl Into<String>) {
        self.ended.locked().get_or_insert(why.into());
    }
}

/// Puts what's sent in through the desktop's portal.
pub struct WaylandInjector {
    place: Place,
    tx: Option<mpsc::UnboundedSender<Do>>,
    shared: Arc<Shared>,
}

/// Which bus the portal's on, and where the restore token's kept.
#[derive(Clone)]
struct Place {
    /// `None`: the desktop's session bus, through the portal helper
    /// process; a bus's address, the helper's work on a thread (labs).
    bus: Option<String>,
    settings: PathBuf,
}

impl Place {
    fn connect(&self) -> Result<Connection> {
        let conn = match &self.bus {
            Some(address) => zbus::blocking::connection::Builder::address(address.as_str()).and_then(|b| b.build()),
            None => Connection::session(),
        };
        conn.context("reaching the desktop's session bus")
    }
}

impl WaylandInjector {
    /// Checks that the desktop's portal can do it (no dialog yet: see
    /// `Injector::start`). Reading its properties is allowed swarm
    /// itself.
    pub fn open() -> Result<WaylandInjector> {
        WaylandInjector::open_on(Place { bus: None, settings: crate::paths().settings() })
    }

    fn open_on(place: Place) -> Result<WaylandInjector> {
        let conn = place.connect()?;
        let portal = Proxy::new(&conn, DESTINATION, PATH, REMOTE_DESKTOP)?;
        let devices: u32 = portal.get_property("AvailableDeviceTypes").map_err(|_| anyhow!("the desktop's portal can't hand over the mouse and keys"))?;
        if devices & (KEYBOARD | POINTER) != KEYBOARD | POINTER {
            bail!("the desktop's portal can't hand over both the mouse and the keys");
        }
        Ok(WaylandInjector { place, tx: None, shared: Arc::default() })
    }

    fn send(&mut self, what: Do) -> Result<()> {
        if let Some(tx) = &self.tx {
            let _ = tx.send(what);
        }
        Ok(())
    }
}

impl Injector for WaylandInjector {
    fn start(&mut self) -> Option<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = Some(tx);
        let (shared, place) = (self.shared.clone(), self.place.clone());
        let remembered = swarm_core::settings::get(&place.settings, RESTORE_KEY).is_some();
        let started = std::thread::Builder::new().name("swarm-control".into()).spawn(move || {
            let why = match run(&shared, &place, rx) {
                Ok(why) => why,
                Err(e) => format!("{e:#}"),
            };
            shared.end(why);
        });
        if let Err(e) = started {
            self.shared.end(e.to_string());
        }
        (!remembered).then(|| "allow it in your desktop's dialog too, choosing the screen you're sharing".to_string())
    }

    fn pointer(&mut self, x: f64, y: f64) -> Result<()> {
        self.send(Do::Pointer(x, y))
    }
    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        self.send(Do::Button(button, down))
    }
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.send(Do::Wheel(dx, dy))
    }
    fn key(&mut self, key: char, down: bool) -> Result<()> {
        self.send(Do::Key(key, down))
    }
    fn text(&mut self, text: &str) -> Result<()> {
        self.send(Do::Text(text.to_string()))
    }

    fn ended(&self) -> Option<String> {
        self.shared.ended.locked().clone()
    }
}

impl Drop for WaylandInjector {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        self.tx = None;
        let pid = self.shared.asking.swap(0, Ordering::Relaxed);
        if pid != 0 {
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
}

/// Swarm's side, on the session's thread: asks the helper for the
/// session, then turns what's sent into its lines until control ends;
/// why it ended.
fn run(shared: &Arc<Shared>, place: &Place, mut rx: mpsc::UnboundedReceiver<Do>) -> Result<String> {
    let restore = swarm_core::settings::get(&place.settings, RESTORE_KEY);
    let mut helper = Helper::start(Kind::Control, restore.as_deref(), place.bus.as_deref())?;
    shared.asking.store(helper.pid().unwrap_or(0), Ordering::Relaxed);
    let answer = helper.answer();
    if shared.asking.swap(0, Ordering::Relaxed) == 0 && helper.pid().is_some() {
        return Ok("control ended".to_string());
    }
    let answer = answer?;
    if let Some(restore) = &answer.restore {
        let _ = swarm_core::settings::set(&place.settings, RESTORE_KEY, restore);
    }
    let mut listener = helper.listener()?;
    let heard = shared.clone();
    std::thread::spawn(move || {
        if listener.hear().as_deref() == Some("closed") {
            heard.end("your desktop stopped it");
        }
    });
    while rx.try_recv().is_ok() {}
    shared.ready.store(true, Ordering::Relaxed);
    let mut keys = Keys::default();
    while let Some(what) = rx.blocking_recv() {
        if shared.stopped.load(Ordering::Relaxed) {
            break;
        }
        for line in keys.lines(what) {
            if helper.say(&line).is_err() {
                return Ok("the portal helper ended".to_string());
            }
        }
    }
    for line in keys.release() {
        let _ = helper.say(&line);
    }
    Ok("control ended".to_string())
}

/// What's held for them, and the helper's lines for what's sent.
#[derive(Default)]
struct Keys {
    /// Keys pressed for them, by what was sent, as the keysym pressed.
    pressed: HashMap<char, u32>,
    buttons: Vec<i32>,
    /// Wheel turned short of a notch, in 120ths, across and down.
    wheel: (i32, i32),
}

impl Keys {
    fn lines(&mut self, what: Do) -> Vec<String> {
        let key = |sym: u32, down: bool| format!("k {sym} {}", down as u8);
        match what {
            Do::Pointer(x, y) => vec![format!("p {x} {y}")],
            Do::Button(button, down) => {
                let code = match button {
                    BUTTON_LEFT => BTN_LEFT,
                    BUTTON_MIDDLE => BTN_MIDDLE,
                    BUTTON_RIGHT => BTN_RIGHT,
                    BUTTON_BACK => BTN_SIDE,
                    BUTTON_FORWARD => BTN_EXTRA,
                    _ => return Vec::new(),
                };
                if down == self.buttons.contains(&code) {
                    return Vec::new();
                }
                if down {
                    self.buttons.push(code);
                } else {
                    self.buttons.retain(|b| *b != code);
                }
                vec![format!("b {code} {}", down as u8)]
            }
            Do::Wheel(dx, dy) => {
                self.wheel.0 += dx;
                self.wheel.1 += dy;
                let notches = |turned: &mut i32| {
                    let n = *turned / 120;
                    *turned -= n * 120;
                    n
                };
                let (across, up) = (notches(&mut self.wheel.0), notches(&mut self.wheel.1));
                let mut lines = Vec::new();
                if up != 0 {
                    lines.push(format!("a 0 {}", -up));
                }
                if across != 0 {
                    lines.push(format!("a 1 {across}"));
                }
                lines
            }
            Do::Key(k, down) => {
                if down {
                    if self.pressed.contains_key(&k) {
                        return Vec::new();
                    }
                    let sym = named(k).unwrap_or_else(|| keysym(k));
                    self.pressed.insert(k, sym);
                    vec![key(sym, true)]
                } else {
                    self.pressed.remove(&k).map(|sym| vec![key(sym, false)]).unwrap_or_default()
                }
            }
            Do::Text(text) => {
                let shifts: Vec<u32> = [keys::SHIFT, keys::SHIFT_R].iter().filter_map(|k| self.pressed.get(k).copied()).collect();
                let mut lines: Vec<String> = shifts.iter().map(|&sym| key(sym, false)).collect();
                for c in text.chars().filter(|c| !c.is_control()) {
                    lines.push(key(keysym(c), true));
                    lines.push(key(keysym(c), false));
                }
                lines.extend(shifts.iter().map(|&sym| key(sym, true)));
                lines
            }
        }
    }

    /// Lets go of everything held.
    fn release(&mut self) -> Vec<String> {
        let mut lines: Vec<String> = std::mem::take(&mut self.pressed).into_values().map(|sym| format!("k {sym} 0")).collect();
        lines.extend(std::mem::take(&mut self.buttons).into_iter().map(|code| format!("b {code} 0")));
        lines
    }
}

/// The helper's side (see `portal_helper`): asks for the session, says
/// how that went, then does what swarm sends until swarm's done (when
/// the session's closed) or the desktop closes it (when swarm's told).
pub(crate) fn serve_helper(conn: &Connection, restore: Option<&str>, stream: &UnixStream, reader: std::io::BufReader<UnixStream>) {
    let session = match Session::start(conn, restore) {
        Ok(session) => session,
        Err(e) => {
            let _ = crate::portal_helper::answer_err(stream, &e);
            return;
        }
    };
    if crate::portal_helper::answer_ok(stream, session.restore.as_deref()).is_err() {
        let _ = close(conn, &session.session);
        return;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    if futures_lite::future::block_on(session.serve(rx)) {
        let _ = crate::portal_helper::say_closed(stream);
    }
}

fn close(conn: &Connection, session: &ObjectPath) -> Result<()> {
    Proxy::new(conn, DESTINATION, session, SESSION)?.call::<_, _, ()>("Close", &())?;
    Ok(())
}

/// The portal's session, once the desktop's allowed it.
struct Session {
    conn: Connection,
    session: OwnedObjectPath,
    /// The session's stream, and its size on the desktop.
    stream: u32,
    size: (i32, i32),
    /// The choice to remember.
    restore: Option<String>,
}

/// A portal call's answer; cancelled, not allowed.
fn request(conn: &Connection, interface: &str, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType), token: &str) -> Result<HashMap<String, OwnedValue>> {
    crate::portal::request(conn, interface, method, body, token)?.ok_or_else(|| anyhow!("it wasn't allowed in the desktop's dialog"))
}

impl Session {
    /// Asks the portal for a session with the mouse and keys and a
    /// monitor's stream (its dialog, unless the choice remembered,
    /// `restore`, is still good). Blocks until the one sharing has
    /// answered.
    fn start(conn: &Connection, restore: Option<&str>) -> Result<Session> {
        let conn = conn.clone();
        let portal = Proxy::new(&conn, DESTINATION, PATH, REMOTE_DESKTOP)?;
        let version: u32 = portal.get_property("version").unwrap_or(1);

        let token_now = token();
        let session_token = token();
        let options: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token_now.as_str())), ("session_handle_token", Value::from(session_token.as_str()))]);
        let created = request(&conn, REMOTE_DESKTOP, "CreateSession", &(options,), &token_now)?;
        let session: String = created
            .get("session_handle")
            .and_then(|v| v.downcast_ref::<String>().ok().or_else(|| v.downcast_ref::<ObjectPath>().ok().map(|p| p.to_string())))
            .ok_or_else(|| anyhow!("the desktop's portal gave no session"))?;
        let session = OwnedObjectPath::try_from(session)?;

        let token_now = token();
        let mut options: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token_now.as_str())), ("types", Value::from(KEYBOARD | POINTER))]);
        if version >= 2 {
            options.insert("persist_mode", Value::from(PERSIST));
            if let Some(restore) = restore {
                options.insert("restore_token", Value::from(restore));
            }
        }
        request(&conn, REMOTE_DESKTOP, "SelectDevices", &(&session, options), &token_now)?;

        let token_now = token();
        let options: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token_now.as_str())), ("types", Value::from(MONITOR)), ("multiple", Value::from(false))]);
        request(&conn, SCREEN_CAST, "SelectSources", &(&session, options), &token_now)?;

        let token_now = token();
        let options: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token_now.as_str()))]);
        let started = request(&conn, REMOTE_DESKTOP, "Start", &(&session, "", options), &token_now)?;
        let restore = started.get("restore_token").and_then(|v| v.downcast_ref::<String>().ok());
        let devices = started.get("devices").and_then(|v| v.downcast_ref::<u32>().ok()).unwrap_or(0);
        if devices & (KEYBOARD | POINTER) != KEYBOARD | POINTER {
            bail!("the desktop didn't hand over both the mouse and the keys");
        }
        let streams: Vec<(u32, HashMap<String, OwnedValue>)> = started
            .get("streams")
            .ok_or_else(|| anyhow!("the desktop's portal gave no screen"))?
            .try_clone()?
            .try_into()
            .map_err(|e| anyhow!("the desktop's portal's answer: {e}"))?;
        let (stream, properties) = streams.into_iter().next().ok_or_else(|| anyhow!("the desktop's portal gave no screen"))?;
        let size = properties
            .get("size")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| <(i32, i32)>::try_from(v).ok())
            .filter(|&(w, h)| w > 0 && h > 0)
            .ok_or_else(|| anyhow!("the desktop's portal didn't say how big the screen is"))?;
        Ok(Session { conn, session, stream, size, restore })
    }

    /// Does swarm's lines until it's done (the session closed) or the
    /// desktop closes the session (`true`).
    async fn serve(self, mut rx: mpsc::UnboundedReceiver<String>) -> bool {
        let conn = self.conn.inner().clone();
        let Ok(portal) = zbus::Proxy::new(&conn, DESTINATION, PATH, REMOTE_DESKTOP).await else { return false };
        let Ok(session) = zbus::Proxy::new(&conn, DESTINATION, self.session.clone(), SESSION).await else { return false };
        let Ok(mut closed) = session.receive_signal("Closed").await else { return false };
        loop {
            let next = futures_lite::future::or(async { Some(rx.recv().await) }, async {
                futures_lite::StreamExt::next(&mut closed).await;
                None
            })
            .await;
            match next {
                Some(Some(line)) => {
                    if let Err(e) = self.apply(&portal, &line).await {
                        eprintln!("{} controlling the screen: {e:#}", crate::unix_now());
                    }
                }
                Some(None) => {
                    let _ = close(&self.conn, &self.session);
                    return false;
                }
                None => return true,
            }
        }
    }

    /// Calls one of the portal's `Notify…` methods with `a` and `b`
    /// after the session and its (empty) options.
    async fn call<A, B>(&self, portal: &zbus::Proxy<'_>, method: &str, a: A, b: B) -> Result<()>
    where
        A: serde::Serialize + zbus::zvariant::Type,
        B: serde::Serialize + zbus::zvariant::Type,
    {
        let options: HashMap<&str, Value> = HashMap::new();
        portal.call::<_, _, ()>(method, &(&self.session, options, a, b)).await.with_context(|| method.to_string())
    }

    /// Does one of swarm's lines.
    async fn apply(&self, portal: &zbus::Proxy<'_>, line: &str) -> Result<()> {
        let words: Vec<&str> = line.split(' ').collect();
        let bad = || anyhow!("the portal helper can't do \"{line}\"");
        let int = |i: usize| words.get(i).and_then(|w| w.parse::<i64>().ok()).ok_or_else(bad);
        match words.first().copied() {
            Some("p") => {
                let at = |i: usize| words.get(i).and_then(|w| w.parse::<f64>().ok()).ok_or_else(bad);
                let (width, height) = self.size;
                let (x, y) = ((at(1)?.clamp(0.0, 1.0) * (width - 1) as f64).round(), (at(2)?.clamp(0.0, 1.0) * (height - 1) as f64).round());
                let options: HashMap<&str, Value> = HashMap::new();
                portal.call::<_, _, ()>("NotifyPointerMotionAbsolute", &(&self.session, options, self.stream, x, y)).await.context("NotifyPointerMotionAbsolute")
            }
            Some("b") => self.call(portal, "NotifyPointerButton", int(1)? as i32, int(2)? as u32).await,
            Some("a") => self.call(portal, "NotifyPointerAxisDiscrete", int(1)? as u32, int(2)? as i32).await,
            Some("k") => self.call(portal, "NotifyKeyboardKeysym", int(1)? as i32, int(2)? as u32).await,
            _ => Err(bad()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// A bus of our own (never the desktop's), with `tools/portal-lab.py`
    /// on it standing in for the portal, while it lasts.
    struct Lab {
        bus: Child,
        portal: Option<Child>,
        address: String,
        dir: PathBuf,
    }

    impl Lab {
        /// `None` without dbus-daemon, or Python's D-Bus, here.
        fn start() -> Option<Lab> {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!("swarm-portal-lab-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).ok()?;
            swarm_core::store::open_vault(&dir, &swarm_core::store::Keys { seal: [9; 32].into(), names: [10; 32].into() });
            let mut bus = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
            let mut address = String::new();
            std::io::BufReader::new(bus.stdout.take()?).read_line(&mut address).ok()?;
            let mut lab = Lab { bus, portal: None, address: address.trim().to_string(), dir };
            lab.portal(false)?;
            Some(lab)
        }

        /// (Re)starts the stand-in portal: `cancel`, its dialog cancelled.
        fn portal(&mut self, cancel: bool) -> Option<()> {
            if let Some(mut was) = self.portal.take() {
                let _ = was.kill();
                let _ = was.wait();
            }
            let _ = std::fs::remove_file(self.log());
            let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../tools/portal-lab.py");
            let mut command = Command::new("python3");
            command.arg(script).arg(self.log()).env("DBUS_SESSION_BUS_ADDRESS", &self.address).stdout(Stdio::null()).stderr(Stdio::null());
            if cancel {
                command.arg("cancel");
            }
            self.portal = Some(command.spawn().ok()?);
            self.wait_for("ready").then_some(())
        }

        fn log(&self) -> PathBuf {
            self.dir.join("portal.log")
        }

        fn place(&self) -> Place {
            Place { bus: Some(self.address.clone()), settings: self.dir.join("settings") }
        }

        /// The portal's log, from line `from` on.
        fn lines(&self, from: usize) -> Vec<String> {
            std::fs::read_to_string(self.log()).unwrap_or_default().lines().skip(from).map(str::to_string).collect()
        }

        fn wait_for(&self, line: &str) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if self.lines(0).iter().any(|l| l == line) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        }

        /// Waits for the log to be `count` lines long from `from`.
        fn wait_lines(&self, from: usize, count: usize) -> Vec<String> {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.lines(from).len() < count && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(100));
            self.lines(from)
        }
    }

    impl Drop for Lab {
        fn drop(&mut self) {
            for child in self.portal.iter_mut().chain([&mut self.bus]) {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn ready(injector: &WaylandInjector) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !injector.shared.ready.load(std::sync::atomic::Ordering::Relaxed) {
            assert!(injector.ended().is_none(), "{:?}", injector.ended());
            assert!(Instant::now() < deadline, "the session never started");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn what_is_sent_goes_to_the_portal_and_the_desktop_can_take_it_back() {
        let Some(mut lab) = Lab::start() else {
            eprintln!("no dbus-daemon or Python D-Bus here: skipped");
            return;
        };
        drop(WaylandInjector::open_on(lab.place()).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(lab.lines(1), Vec::<String>::new());

        let mut injector = WaylandInjector::open_on(lab.place()).unwrap();
        assert!(injector.start().is_some_and(|said| said.contains("dialog")), "told the first time");
        ready(&injector);
        assert_eq!(
            lab.lines(1),
            vec![
                "CreateSession",
                "SelectDevices {'persist_mode': 2, 'types': 3}",
                "SelectSources {'multiple': False, 'types': 1}",
                "Start",
            ]
        );
        assert_eq!(swarm_core::settings::get(&lab.place().settings, RESTORE_KEY).as_deref(), Some("restore-1"));

        let from = lab.lines(0).len();
        injector.pointer(0.5, 0.25).unwrap();
        injector.button(BUTTON_RIGHT, true).unwrap();
        injector.button(BUTTON_RIGHT, false).unwrap();
        injector.wheel(0, 120).unwrap();
        injector.wheel(60, 0).unwrap();
        injector.wheel(60, 0).unwrap();
        injector.key(keys::CONTROL, true).unwrap();
        injector.key('c', true).unwrap();
        injector.key('c', false).unwrap();
        injector.key(keys::CONTROL, false).unwrap();
        injector.key(keys::SHIFT, true).unwrap();
        injector.text("aB£").unwrap();
        injector.key(keys::SHIFT, false).unwrap();
        injector.key(keys::UP, true).unwrap();
        drop(injector);
        let done = lab.wait_lines(from, 22);
        let expected = [
            "motion 44 960 270",
            "button 0x111 1",
            "button 0x111 0",
            "axis 0 -1",
            "axis 1 1",
            "keysym 0xffe3 1",
            "keysym 0x63 1",
            "keysym 0x63 0",
            "keysym 0xffe3 0",
            "keysym 0xffe1 1",
            "keysym 0xffe1 0",
            "keysym 0x61 1",
            "keysym 0x61 0",
            "keysym 0x42 1",
            "keysym 0x42 0",
            "keysym 0xa3 1",
            "keysym 0xa3 0",
            "keysym 0xffe1 1",
            "keysym 0xffe1 0",
            "keysym 0xff52 1",
            "keysym 0xff52 0",
            "Close",
        ];
        assert_eq!(done, expected);

        let from = lab.lines(0).len();
        let mut injector = WaylandInjector::open_on(lab.place()).unwrap();
        assert_eq!(injector.start(), None);
        ready(&injector);
        assert!(lab.lines(from).contains(&"SelectDevices {'persist_mode': 2, 'restore_token': 'restore-1', 'types': 3}".to_string()), "{:?}", lab.lines(from));

        let conn = lab.place().connect().unwrap();
        conn.call_method(Some("org.freedesktop.portal.Desktop"), "/lab", Some("lab.Lab"), "CloseAll", &()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while injector.ended().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(injector.ended().as_deref(), Some("your desktop stopped it"));
        drop(injector);

        lab.portal(true).unwrap();
        let mut injector = WaylandInjector::open_on(lab.place()).unwrap();
        injector.start();
        let deadline = Instant::now() + Duration::from_secs(5);
        while injector.ended().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(injector.ended().as_deref(), Some("it wasn't allowed in the desktop's dialog"));
    }

    #[test]
    fn a_screen_is_asked_for_through_the_helper_and_its_stream_handed_over() {
        let Some(lab) = Lab::start() else {
            eprintln!("no dbus-daemon or Python D-Bus here: skipped");
            return;
        };
        let mut helper = Helper::start(Kind::Screen, Some("screen-0"), Some(&lab.address)).unwrap();
        let answer = helper.answer().unwrap();
        assert_eq!((answer.node, answer.restore.as_deref()), (44, Some("screen-1")));
        assert!(answer.fd.is_some(), "the stream's descriptor handed over");
        assert_eq!(
            lab.lines(1),
            vec![
                "ScreenCast.CreateSession",
                "SelectSources {'cursor_mode': 2, 'multiple': False, 'persist_mode': 2, 'restore_token': 'screen-0', 'types': 1}",
                "ScreenCast.Start",
                "OpenPipeWireRemote",
            ]
        );
    }
}
