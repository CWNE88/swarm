//! Sharing a Wayland screen: Wayland lets a program see the screen only
//! through the desktop's portal (`org.freedesktop.portal.ScreenCast`,
//! over D-Bus), which asks the user what to share in a dialog of its own,
//! then hands over a PipeWire stream of it. The stream's pictures are
//! read here, through PipeWire's own library — loaded when sharing
//! starts, not linked, so swarm builds and runs where there's
//! no PipeWire (and only this says why it can't).
//!
//! The choice made in the dialog is remembered (the portal's "restore
//! token", kept in the settings file), so it's only asked once — unless
//! the desktop forgets it.
//!
//! The portal's asked by a helper process (`portal_helper`), which holds
//! the session: the portal turns swarm itself down.
//!
//! `SWARM_SCREEN=pipewire:<node id>` reads that PipeWire node straight
//! from the PipeWire this user runs, with no portal — for labs.

use swarm_core::lock::Locked;
use super::{Grabber, ScreenInfo};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use swarm_core::video::Picture;

/// How long the stream may take to give its first picture.
const FIRST_PICTURE_WITHIN: Duration = Duration::from_secs(10);
/// Pictures are taken from the stream no more often than this.
const EVERY: Duration = Duration::from_millis(1000 / super::MAX_FPS as u64);
/// A stream that's been going, and has been paused this long, has had
/// its screen taken away: stopped from the desktop's side (GNOME's "stop
/// sharing" button), which PipeWire says only by pausing it.
const PAUSED_FOR_GOOD: Duration = Duration::from_secs(2);

/// A screen shared through the portal (or, for labs, a PipeWire node).
pub struct WaylandGrabber {
    stream: Stream,
    info: ScreenInfo,
    /// Holds the portal's session: let go after the stream.
    _helper: Option<crate::portal_helper::Helper>,
}

impl WaylandGrabber {
    /// Asks the desktop's portal for a screen (its dialog, unless a
    /// choice it remembers is still good), and starts reading it.
    /// Blocks until the user has chosen, or turned it down.
    pub fn open() -> Result<WaylandGrabber> {
        let lib = Lib::get()?;
        let (fd, node, helper) = match std::env::var("SWARM_SCREEN").ok().and_then(|v| v.strip_prefix("pipewire:").map(str::to_string)) {
            Some(node) => (None, node.parse().context("SWARM_SCREEN=pipewire:<node id>")?, None),
            None => {
                let restore = swarm_core::settings::get(&crate::paths().settings(), portal::RESTORE_KEY);
                let mut helper = crate::portal_helper::Helper::start(crate::portal_helper::Kind::Screen, restore.as_deref(), None)?;
                let answer = helper.answer()?;
                if let Some(restore) = &answer.restore {
                    let _ = swarm_core::settings::set(&crate::paths().settings(), portal::RESTORE_KEY, restore);
                }
                let fd = answer.fd.ok_or_else(|| anyhow!("the desktop's portal gave no stream"))?;
                (Some(fd), answer.node, Some(helper))
            }
        };
        let stream = Stream::start(lib, fd, node)?;
        let started = Instant::now();
        loop {
            let size = stream.shared.latest.locked().as_ref().map(|p| (p.width, p.height));
            if let Some((width, height)) = size {
                let info = ScreenInfo { name: format!("screen {width}×{height}"), width: width as u32, height: height as u32 };
                return Ok(WaylandGrabber { stream, info, _helper: helper });
            }
            if let Some(why) = stream.shared.failed.locked().clone() {
                bail!("the screen's stream failed: {why}");
            }
            if started.elapsed() > FIRST_PICTURE_WITHIN {
                bail!("the screen's stream gave no picture");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Grabber for WaylandGrabber {
    fn info(&self) -> ScreenInfo {
        self.info.clone()
    }

    fn grab(&mut self) -> Result<Picture> {
        if let Some(why) = self.ended() {
            bail!("the screen's stream ended: {why}");
        }
        self.stream.shared.latest.locked().clone().ok_or_else(|| anyhow!("no picture yet"))
    }

    fn ended(&self) -> Option<String> {
        let shared = &self.stream.shared;
        if let Some(why) = shared.failed.locked().clone() {
            return Some(why);
        }
        let paused = *shared.paused_since.locked();
        paused.filter(|since| since.elapsed() >= PAUSED_FOR_GOOD).map(|_| "the desktop stopped sharing it".to_string())
    }
}

/// The portal's side: asking for a screen (by the portal helper).
pub(crate) mod portal {
    use super::*;
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::{ObjectPath, OwnedFd, OwnedValue, Value};

    use crate::portal::{token, DESTINATION, PATH};
    const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
    /// Monitors (not windows): a window can be moved off, or covered.
    const MONITOR: u32 = 1;
    /// The pointer drawn into the pictures.
    const POINTER_IN_PICTURE: u32 = 2;
    /// The choice kept until it's revoked.
    const PERSIST: u32 = 2;
    /// Where the portal's restore token is kept.
    pub(super) const RESTORE_KEY: &str = "screen_restore";

    /// A screen-cast call's answer; cancelled, nothing to share.
    fn request(conn: &Connection, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType), token: &str) -> Result<HashMap<String, OwnedValue>> {
        crate::portal::request(conn, SCREEN_CAST, method, body, token)?.ok_or_else(|| anyhow!("nothing was chosen to share"))
    }

    /// Asks for a monitor to share, offering the choice remembered
    /// (`restore`): a PipeWire connection to it, its node there, and the
    /// choice to remember now. The session lasts as long as `conn`.
    pub fn ask(conn: &Connection, restore: Option<&str>) -> Result<(std::os::fd::OwnedFd, u32, Option<String>)> {
        let conn = conn.clone();
        let screen_cast = Proxy::new(&conn, DESTINATION, PATH, SCREEN_CAST)?;
        let version: u32 = screen_cast.get_property("version").unwrap_or(1);
        let pointers: u32 = screen_cast.get_property("AvailableCursorModes").unwrap_or(0);

        let token_now = token();
        let session_token = token();
        let options: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token_now.as_str())), ("session_handle_token", Value::from(session_token.as_str()))]);
        let created = request(&conn, "CreateSession", &(options,), &token_now)?;
        let session: String = created
            .get("session_handle")
            .and_then(|v| v.downcast_ref::<String>().ok().or_else(|| v.downcast_ref::<ObjectPath>().ok().map(|p| p.to_string())))
            .ok_or_else(|| anyhow!("the desktop's portal gave no session"))?;
        let session = ObjectPath::try_from(session.as_str())?;

        let token_now = token();
        let mut options: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token_now.as_str())), ("types", Value::from(MONITOR)), ("multiple", Value::from(false))]);
        if pointers & POINTER_IN_PICTURE != 0 {
            options.insert("cursor_mode", Value::from(POINTER_IN_PICTURE));
        }
        if version >= 4 {
            options.insert("persist_mode", Value::from(PERSIST));
            if let Some(restore) = restore {
                options.insert("restore_token", Value::from(restore));
            }
        }
        request(&conn, "SelectSources", &(&session, options), &token_now)?;

        let token_now = token();
        let options: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token_now.as_str()))]);
        let started = request(&conn, "Start", &(&session, "", options), &token_now)?;
        let restore = started.get("restore_token").and_then(|v| v.downcast_ref::<String>().ok());
        let streams: Vec<(u32, HashMap<String, OwnedValue>)> = started
            .get("streams")
            .ok_or_else(|| anyhow!("the desktop's portal gave no screen"))?
            .try_clone()?
            .try_into()
            .map_err(|e| anyhow!("the desktop's portal's answer: {e}"))?;
        let (node, _) = streams.into_iter().next().ok_or_else(|| anyhow!("the desktop's portal gave no screen"))?;

        let options: HashMap<&str, Value> = HashMap::new();
        let fd: OwnedFd = screen_cast.call("OpenPipeWireRemote", &(&session, options)).context("opening the screen's stream")?;
        Ok((fd.into(), node, restore))
    }
}

/// The parts of PipeWire's library used here, loaded once.
struct Lib {
    _lib: libloading::Library,
    pw_init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    pw_thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    pw_thread_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pw_thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    pw_thread_loop_stop: unsafe extern "C" fn(*mut c_void),
    pw_thread_loop_lock: unsafe extern "C" fn(*mut c_void),
    pw_thread_loop_unlock: unsafe extern "C" fn(*mut c_void),
    pw_thread_loop_destroy: unsafe extern "C" fn(*mut c_void),
    pw_context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    pw_context_connect: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    pw_context_connect_fd: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    pw_context_destroy: unsafe extern "C" fn(*mut c_void),
    pw_core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    pw_properties_new_string: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    pw_stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void,
    pw_stream_add_listener: unsafe extern "C" fn(*mut c_void, *mut SpaHook, *const StreamEvents, *mut c_void),
    pw_stream_connect: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const c_void, u32) -> c_int,
    pw_stream_dequeue_buffer: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    pw_stream_queue_buffer: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    pw_stream_destroy: unsafe extern "C" fn(*mut c_void),
}

impl Lib {
    fn get() -> Result<&'static Lib> {
        static LIB: OnceLock<std::result::Result<Lib, String>> = OnceLock::new();
        LIB.get_or_init(|| Lib::load().map_err(|e| format!("{e:#}"))).as_ref().map_err(|e| anyhow!("{e}"))
    }

    fn load() -> Result<Lib> {
        unsafe {
            let lib = libloading::Library::new("libpipewire-0.3.so.0").context("sharing a Wayland screen needs PipeWire (libpipewire-0.3), which wasn't found")?;
            macro_rules! f {
                ($name:ident) => {
                    *lib.get(concat!(stringify!($name), "\0").as_bytes()).with_context(|| format!("PipeWire's library has no {}", stringify!($name)))?
                };
            }
            let made = Lib {
                pw_init: f!(pw_init),
                pw_thread_loop_new: f!(pw_thread_loop_new),
                pw_thread_loop_get_loop: f!(pw_thread_loop_get_loop),
                pw_thread_loop_start: f!(pw_thread_loop_start),
                pw_thread_loop_stop: f!(pw_thread_loop_stop),
                pw_thread_loop_lock: f!(pw_thread_loop_lock),
                pw_thread_loop_unlock: f!(pw_thread_loop_unlock),
                pw_thread_loop_destroy: f!(pw_thread_loop_destroy),
                pw_context_new: f!(pw_context_new),
                pw_context_connect: f!(pw_context_connect),
                pw_context_connect_fd: f!(pw_context_connect_fd),
                pw_context_destroy: f!(pw_context_destroy),
                pw_core_disconnect: f!(pw_core_disconnect),
                pw_properties_new_string: f!(pw_properties_new_string),
                pw_stream_new: f!(pw_stream_new),
                pw_stream_add_listener: f!(pw_stream_add_listener),
                pw_stream_connect: f!(pw_stream_connect),
                pw_stream_dequeue_buffer: f!(pw_stream_dequeue_buffer),
                pw_stream_queue_buffer: f!(pw_stream_queue_buffer),
                pw_stream_destroy: f!(pw_stream_destroy),
                _lib: lib,
            };
            (made.pw_init)(std::ptr::null_mut(), std::ptr::null_mut());
            Ok(made)
        }
    }
}

/// `struct spa_hook`: a listener's place in a list, which PipeWire fills
/// in. Six pointers; room for more.
#[repr(C)]
struct SpaHook([usize; 8]);

/// `struct pw_stream_events`, version 2.
#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn()>,
    io_changed: Option<unsafe extern "C" fn()>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const u32)>,
    add_buffer: Option<unsafe extern "C" fn()>,
    remove_buffer: Option<unsafe extern "C" fn()>,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Option<unsafe extern "C" fn()>,
    command: Option<unsafe extern "C" fn()>,
    trigger_done: Option<unsafe extern "C" fn()>,
}

/// `struct pw_buffer` (the part read here).
#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
}

/// `struct spa_buffer`.
#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut c_void,
    datas: *mut SpaData,
}

/// `struct spa_data`.
#[repr(C)]
struct SpaData {
    kind: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

/// `struct spa_chunk`.
#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_RECTANGLE: u32 = 10;
const SPA_TYPE_FRACTION: u32 = 11;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_CHOICE: u32 = 19;
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_PARAM_FORMAT: u32 = 4;
const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_VIDEO_FORMAT: u32 = 0x20001;
const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;
const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const SPA_FORMAT_VIDEO_MAX_FRAMERATE: u32 = 0x20005;
const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
const SPA_CHOICE_RANGE: u32 = 1;
const SPA_CHOICE_ENUM: u32 = 3;
const PW_DIRECTION_INPUT: u32 = 0;
const PW_ID_ANY: u32 = 0xffff_ffff;
const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
const PW_STREAM_STATE_ERROR: c_int = -1;
const PW_STREAM_STATE_UNCONNECTED: c_int = 0;
const PW_STREAM_STATE_PAUSED: c_int = 2;
const PW_STREAM_STATE_STREAMING: c_int = 3;

/// The pixel layouts taken, in `spa_video_format`'s numbering: four
/// bytes a pixel (what screens give), or three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Rgbx = 7,
    Bgrx = 8,
    Rgba = 11,
    Bgra = 12,
    Rgb = 15,
    Bgr = 16,
}

impl Layout {
    const ALL: [Layout; 6] = [Layout::Bgrx, Layout::Rgbx, Layout::Bgra, Layout::Rgba, Layout::Rgb, Layout::Bgr];

    fn from_id(id: u32) -> Option<Layout> {
        Layout::ALL.into_iter().find(|l| *l as u32 == id)
    }

    fn bytes(self) -> usize {
        match self {
            Layout::Rgb | Layout::Bgr => 3,
            _ => 4,
        }
    }

    /// Where red, green and blue are in a pixel.
    fn order(self) -> [usize; 3] {
        match self {
            Layout::Rgbx | Layout::Rgba | Layout::Rgb => [0, 1, 2],
            Layout::Bgrx | Layout::Bgra | Layout::Bgr => [2, 1, 0],
        }
    }
}

/// The formats asked for, as an SPA pod (`SPA_PARAM_EnumFormat`): raw
/// video in any of `Layout`'s layouts, any size, at up to `MAX_FPS`
/// pictures a second (no more are sent, so none are wanted).
fn enum_format() -> Vec<u32> {
    let mut props: Vec<u32> = Vec::new();
    let mut id = |key: u32, value: u32| props.extend([key, 0, 4, SPA_TYPE_ID, value, 0]);
    id(SPA_FORMAT_MEDIA_TYPE, SPA_MEDIA_TYPE_VIDEO);
    id(SPA_FORMAT_MEDIA_SUBTYPE, SPA_MEDIA_SUBTYPE_RAW);
    let mut layouts: Vec<u32> = vec![Layout::ALL[0] as u32];
    layouts.extend(Layout::ALL.iter().map(|l| *l as u32));
    let body = 16 + 4 * layouts.len() as u32;
    props.extend([SPA_FORMAT_VIDEO_FORMAT, 0, body, SPA_TYPE_CHOICE, SPA_CHOICE_ENUM, 0, 4, SPA_TYPE_ID]);
    props.extend(&layouts);
    if !body.is_multiple_of(8) {
        props.push(0);
    }
    props.extend([SPA_FORMAT_VIDEO_SIZE, 0, 40, SPA_TYPE_CHOICE, SPA_CHOICE_RANGE, 0, 8, SPA_TYPE_RECTANGLE, 1920, 1080, 1, 1, 16384, 16384]);
    props.extend([SPA_FORMAT_VIDEO_FRAMERATE, 0, 40, SPA_TYPE_CHOICE, SPA_CHOICE_RANGE, 0, 8, SPA_TYPE_FRACTION, super::MAX_FPS, 1, 0, 1, 1000, 1]);
    props.extend([SPA_FORMAT_VIDEO_MAX_FRAMERATE, 0, 40, SPA_TYPE_CHOICE, SPA_CHOICE_RANGE, 0, 8, SPA_TYPE_FRACTION, super::MAX_FPS, 1, 1, 1, super::MAX_FPS, 1]);
    let mut pod = vec![(8 + props.len() * 4) as u32, SPA_TYPE_OBJECT, SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT];
    pod.extend(props);
    pod
}

/// The layout and size in a format the stream settled on (an SPA pod
/// object, as 32-bit words), if it's one of ours.
fn settled_format(pod: &[u32]) -> Option<(Layout, usize, usize)> {
    if pod.len() < 4 || pod[1] != SPA_TYPE_OBJECT {
        return None;
    }
    let end = (2 + pod[0] as usize / 4).min(pod.len());
    let (mut layout, mut size) = (None, None);
    let mut at = 4;
    while at + 4 <= end {
        let (key, value_size, value_type) = (pod[at], pod[at + 2] as usize, pod[at + 3]);
        let body = at + 4;
        let (value_type, value) = if value_type == SPA_TYPE_CHOICE && body + 4 <= end { (pod[body + 3], body + 4) } else { (value_type, body) };
        match (key, value_type) {
            (SPA_FORMAT_VIDEO_FORMAT, SPA_TYPE_ID) if value < end => layout = Layout::from_id(pod[value]),
            (SPA_FORMAT_VIDEO_SIZE, SPA_TYPE_RECTANGLE) if value + 1 < end => size = Some((pod[value] as usize, pod[value + 1] as usize)),
            _ => {}
        }
        at = body + value_size.div_ceil(8) * 2;
    }
    let (width, height) = size?;
    Some((layout?, width, height))
}

/// One picture out of a buffer: `width`×`height` in `layout`, rows
/// `stride` bytes apart, as RGB.
fn picture_from(bytes: &[u8], layout: Layout, width: usize, height: usize, stride: usize) -> Option<Picture> {
    let per = layout.bytes();
    if width == 0 || height == 0 || stride < width * per || bytes.len() < stride * (height - 1) + width * per {
        return None;
    }
    let [r, g, b] = layout.order();
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in 0..height {
        for px in bytes[row * stride..row * stride + width * per].chunks_exact(per) {
            rgb.extend_from_slice(&[px[r], px[g], px[b]]);
        }
    }
    Some(Picture::rgb(width, height, rgb))
}

/// What the stream's callbacks (on PipeWire's thread) share with us.
struct Shared {
    lib: &'static Lib,
    stream: Mutex<usize>,
    format: Mutex<Option<(Layout, usize, usize)>>,
    latest: Mutex<Option<Picture>>,
    taken: Mutex<Option<Instant>>,
    failed: Mutex<Option<String>>,
    /// It's streamed, and since when it's been paused again, if it has.
    streamed: Mutex<bool>,
    paused_since: Mutex<Option<Instant>>,
}

/// A PipeWire stream of a screen, read on PipeWire's own thread.
struct Stream {
    lib: &'static Lib,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    _hook: Box<SpaHook>,
    _events: Box<StreamEvents>,
    /// The callbacks hold a pointer to what this holds.
    shared: Arc<Shared>,
    _fd: Option<std::os::fd::OwnedFd>,
}

unsafe impl Send for Stream {}

unsafe extern "C" fn on_state(data: *mut c_void, _old: c_int, state: c_int, error: *const c_char) {
    let shared = unsafe { &*(data as *const Shared) };
    if state == PW_STREAM_STATE_ERROR || state == PW_STREAM_STATE_UNCONNECTED {
        let why = if error.is_null() { "it was stopped".to_string() } else { unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned() };
        *shared.failed.locked() = Some(why);
    }
    let mut streamed = shared.streamed.locked();
    *shared.paused_since.locked() = match state {
        PW_STREAM_STATE_STREAMING => {
            *streamed = true;
            None
        }
        PW_STREAM_STATE_PAUSED if *streamed => Some(Instant::now()),
        _ => None,
    };
}

unsafe extern "C" fn on_param(data: *mut c_void, id: u32, param: *const u32) {
    let shared = unsafe { &*(data as *const Shared) };
    if id != SPA_PARAM_FORMAT || param.is_null() {
        return;
    }
    let words = 2 + unsafe { *param } as usize / 4;
    let pod = unsafe { std::slice::from_raw_parts(param, words) };
    *shared.format.locked() = settled_format(pod);
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let shared = unsafe { &*(data as *const Shared) };
    let stream = *shared.stream.locked() as *mut c_void;
    if stream.is_null() {
        return;
    }
    let mut newest: *mut PwBuffer = std::ptr::null_mut();
    loop {
        let next = unsafe { (shared.lib.pw_stream_dequeue_buffer)(stream) };
        if next.is_null() {
            break;
        }
        if !newest.is_null() {
            unsafe { (shared.lib.pw_stream_queue_buffer)(stream, newest) };
        }
        newest = next;
    }
    if newest.is_null() {
        return;
    }
    let due = shared.taken.locked().is_none_or(|t| t.elapsed() >= EVERY);
    let format = *shared.format.locked();
    if let (true, Some((layout, width, height))) = (due, format) {
        let buffer = unsafe { &*(*newest).buffer };
        if buffer.n_datas > 0 && !buffer.datas.is_null() {
            let data = unsafe { &*buffer.datas };
            if !data.data.is_null() && !data.chunk.is_null() {
                let chunk = unsafe { &*data.chunk };
                let offset = (chunk.offset % data.maxsize.max(1)) as usize;
                let size = (chunk.size.min(data.maxsize) as usize).min(data.maxsize as usize - offset);
                let stride = if chunk.stride > 0 { chunk.stride as usize } else { width * layout.bytes() };
                let bytes = unsafe { std::slice::from_raw_parts((data.data as *const u8).add(offset), size) };
                if let Some(picture) = picture_from(bytes, layout, width, height, stride) {
                    *shared.latest.locked() = Some(picture);
                    *shared.taken.locked() = Some(Instant::now());
                }
            }
        }
    }
    unsafe { (shared.lib.pw_stream_queue_buffer)(stream, newest) };
}

impl Stream {
    /// Connects to PipeWire (through `fd`, the portal's, or else the
    /// user's own) and starts reading node `node`.
    fn start(lib: &'static Lib, fd: Option<std::os::fd::OwnedFd>, node: u32) -> Result<Stream> {
        use std::os::fd::AsRawFd;
        let shared = Arc::new(Shared {
            lib,
            stream: Mutex::new(0),
            format: Mutex::new(None),
            latest: Mutex::new(None),
            taken: Mutex::new(None),
            failed: Mutex::new(None),
            streamed: Mutex::new(false),
            paused_since: Mutex::new(None),
        });
        let events = Box::new(StreamEvents {
            version: 2,
            destroy: None,
            state_changed: Some(on_state),
            control_info: None,
            io_changed: None,
            param_changed: Some(on_param),
            add_buffer: None,
            remove_buffer: None,
            process: Some(on_process),
            drained: None,
            command: None,
            trigger_done: None,
        });
        let hook = Box::new(SpaHook([0; 8]));
        unsafe {
            let thread_loop = (lib.pw_thread_loop_new)(c"swarm-screen".as_ptr(), std::ptr::null());
            if thread_loop.is_null() {
                bail!("couldn't start PipeWire's loop");
            }
            let mut made = Stream {
                lib,
                thread_loop,
                context: std::ptr::null_mut(),
                core: std::ptr::null_mut(),
                stream: std::ptr::null_mut(),
                _hook: hook,
                _events: events,
                shared,
                _fd: None,
            };
            made.context = (lib.pw_context_new)((lib.pw_thread_loop_get_loop)(thread_loop), std::ptr::null_mut(), 0);
            if made.context.is_null() {
                bail!("couldn't start PipeWire");
            }
            if (lib.pw_thread_loop_start)(thread_loop) < 0 {
                bail!("couldn't start PipeWire's loop");
            }
            (lib.pw_thread_loop_lock)(thread_loop);
            let connected = (|| -> Result<()> {
                made.core = match &fd {
                    Some(fd) => (lib.pw_context_connect_fd)(made.context, libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3), std::ptr::null_mut(), 0),
                    None => (lib.pw_context_connect)(made.context, std::ptr::null_mut(), 0),
                };
                if made.core.is_null() {
                    bail!("couldn't connect to PipeWire");
                }
                let props = (lib.pw_properties_new_string)(c"media.type=Video media.category=Capture media.role=Screen".as_ptr());
                made.stream = (lib.pw_stream_new)(made.core, c"swarm screen".as_ptr(), props);
                if made.stream.is_null() {
                    bail!("couldn't make a PipeWire stream");
                }
                *made.shared.stream.locked() = made.stream as usize;
                let hook_ptr: *mut SpaHook = &mut *made._hook;
                (lib.pw_stream_add_listener)(made.stream, hook_ptr, &*made._events, Arc::as_ptr(&made.shared) as *mut c_void);
                let format = enum_format();
                let mut params = [format.as_ptr() as *const c_void];
                let code = (lib.pw_stream_connect)(made.stream, PW_DIRECTION_INPUT, if node == 0 { PW_ID_ANY } else { node }, PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS, params.as_mut_ptr(), 1);
                if code < 0 {
                    bail!("couldn't connect to the screen's stream (error {code})");
                }
                Ok(())
            })();
            (lib.pw_thread_loop_unlock)(thread_loop);
            made._fd = fd;
            connected?;
            Ok(made)
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            (self.lib.pw_thread_loop_stop)(self.thread_loop);
            *self.shared.stream.locked() = 0;
            if !self.stream.is_null() {
                (self.lib.pw_stream_destroy)(self.stream);
            }
            if !self.core.is_null() {
                (self.lib.pw_core_disconnect)(self.core);
            }
            if !self.context.is_null() {
                (self.lib.pw_context_destroy)(self.context);
            }
            (self.lib.pw_thread_loop_destroy)(self.thread_loop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The format asked for reads back as a well-formed object: the
    /// property walk ends exactly at its end, and finds its size.
    #[test]
    fn the_format_asked_for_is_a_well_formed_pod() {
        let pod = enum_format();
        assert_eq!(pod[0] as usize, pod.len() * 4 - 8, "the size covers the body");
        assert_eq!(settled_format(&pod), Some((Layout::Bgrx, 1920, 1080)), "its defaults read back");
        let mut at = 4;
        while at < pod.len() {
            at += 4 + (pod[at + 2] as usize).div_ceil(8) * 2;
        }
        assert_eq!(at, pod.len());
    }

    #[test]
    fn a_settled_format_is_read() {
        let mut pod = vec![0, SPA_TYPE_OBJECT, SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_FORMAT];
        pod.extend([SPA_FORMAT_MEDIA_TYPE, 0, 4, SPA_TYPE_ID, SPA_MEDIA_TYPE_VIDEO, 0]);
        pod.extend([SPA_FORMAT_MEDIA_SUBTYPE, 0, 4, SPA_TYPE_ID, SPA_MEDIA_SUBTYPE_RAW, 0]);
        pod.extend([SPA_FORMAT_VIDEO_FORMAT, 0, 4, SPA_TYPE_ID, Layout::Rgbx as u32, 0]);
        pod.extend([SPA_FORMAT_VIDEO_SIZE, 0, 8, SPA_TYPE_RECTANGLE, 1366, 768]);
        pod[0] = (pod.len() * 4 - 8) as u32;
        assert_eq!(settled_format(&pod), Some((Layout::Rgbx, 1366, 768)));
        pod[4 + 12 + 4] = 2;
        assert_eq!(settled_format(&pod), None);
    }

    #[test]
    fn pixels_come_out_as_rgb_whatever_their_order_and_padding() {
        let bytes = [3, 2, 1, 0, 6, 5, 4, 0, 9, 9, 9, 9, 13, 12, 11, 0, 16, 15, 14, 0, 9, 9, 9, 9];
        let picture = picture_from(&bytes, Layout::Bgrx, 2, 2, 12).unwrap();
        assert_eq!(picture.data, vec![1, 2, 3, 4, 5, 6, 11, 12, 13, 14, 15, 16]);
        let picture = picture_from(&[1, 2, 3, 4, 5, 6], Layout::Rgb, 2, 1, 6).unwrap();
        assert_eq!(picture.data, vec![1, 2, 3, 4, 5, 6]);
        assert!(picture_from(&[0; 10], Layout::Rgbx, 2, 2, 8).is_none(), "too short");
    }
}
