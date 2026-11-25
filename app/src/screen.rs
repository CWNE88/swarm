//! Screen capture for sharing your screen in a video call: X11 on Linux
//! (`x11rb`, the whole of the primary monitor), Wayland through the
//! desktop's portal and PipeWire (whichever monitor's chosen in its
//! dialog — see `screen_wayland.rs`), GDI on Windows (the primary
//! monitor), the mouse pointer drawn in on all three.
//!
//! Like the camera (see `camera`), a thread of its own takes pictures and
//! keeps only the newest (`Screen::subscribe`) — at most `MAX_FPS` a
//! second, and only when something on the screen has changed: a screen
//! that stands still costs nothing to send. Each picture is made 16:9,
//! the shape calls are sent in, by black bars at the sides or top and
//! bottom, so nothing is cropped off.
//!
//! `SWARM_SCREEN=test` shares a made-up screen of text, with a pointer
//! moving over it, for trying sharing where there's no screen to share.

#[cfg(target_os = "linux")]
#[path = "screen_wayland.rs"]
mod wayland;

/// The portal asked for a screen to share (by the portal helper): see
/// `screen_wayland.rs`.
#[cfg(target_os = "linux")]
pub(crate) fn wayland_ask(conn: &zbus::blocking::Connection, restore: Option<&str>) -> Result<(std::os::fd::OwnedFd, u32, Option<String>)> {
    wayland::portal::ask(conn, restore)
}

use swarm_core::lock::Locked;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use swarm_core::video::Picture;
use tokio::sync::watch;

/// The most pictures a second taken: as many as video's ever sent at, a
/// camera's or a screen's (see `swarm_core::video`).
const MAX_FPS: u32 = swarm_core::video::MAX_FPS;

/// What's being shared.
#[derive(Clone)]
pub struct ScreenInfo {
    /// "screen 1920×1080", say.
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// A screen being captured, until dropped.
pub struct Screen {
    info: ScreenInfo,
    latest: watch::Sender<Option<Arc<Picture>>>,
    stop: Arc<AtomicBool>,
    /// Why there'll be no more pictures, once there won't.
    ended: Arc<std::sync::Mutex<Option<String>>>,
    /// Pictures taken, and tries that failed (see `counts`).
    taken: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// One way of taking a picture of the screen: made, used and let go on
/// its own thread (see `Screen::start`).
trait Grabber: 'static {
    fn info(&self) -> ScreenInfo;
    /// The screen as it is now, pointer and all, as RGB.
    fn grab(&mut self) -> Result<Picture>;
    /// Why the screen can't be had any more, if it can't (the desktop
    /// stopped sharing it, say).
    fn ended(&self) -> Option<String> {
        None
    }
}

/// Whether this is a Wayland desktop, where the screen's had through the
/// portal (and X11 programs, through XWayland, see only each other).
#[cfg(target_os = "linux")]
pub(crate) fn wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
        || (std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_some())
        || std::env::var("SWARM_SCREEN").is_ok_and(|v| v.starts_with("pipewire:"))
}

/// Where on the desktop what's shared is (its primary monitor), as x, y,
/// width and height in pixels, where that's known (X11 and Windows, so
/// far).
pub fn shared_area() -> Option<(i32, i32, u32, u32)> {
    #[cfg(target_os = "linux")]
    if !wayland() && std::env::var_os("DISPLAY").is_some() {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let (x, y, width, height) = x11::shared_area(&conn, screen);
        return Some((x as i32, y as i32, width as u32, height as u32));
    }
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if width > 0 && height > 0 {
            return Some((0, 0, width as u32, height as u32));
        }
    }
    None
}

impl Screen {
    /// Starts capturing the screen (`SWARM_SCREEN=test`: a made-up one).
    /// Blocks briefly while the first picture's taken, so a screen that
    /// can't be captured fails here rather than sharing nothing.
    pub fn open() -> Result<Screen> {
        if std::env::var("SWARM_SCREEN").is_ok_and(|v| v == "test") {
            return Screen::start(|| Ok(TestScreen::new()));
        }
        #[cfg(target_os = "linux")]
        {
            if wayland() {
                return Screen::start(wayland::WaylandGrabber::open);
            }
            if std::env::var_os("DISPLAY").is_none() {
                anyhow::bail!("no screen to share here (no display)");
            }
            Screen::start(x11::X11Grabber::open)
        }
        #[cfg(windows)]
        {
            Screen::start(gdi::GdiGrabber::open)
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            anyhow::bail!("sharing the screen isn't supported on this system yet")
        }
    }

    /// Opens the screen (`open`) and takes its pictures, all on a thread
    /// of its own: Windows' handles belong to the thread that got them,
    /// and taken on another, every picture after the first failed (so it
    /// froze). Waits for the first, as `open` says.
    fn start<G: Grabber>(open: impl FnOnce() -> Result<G> + Send + 'static) -> Result<Screen> {
        let latest = watch::Sender::new(None);
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(std::sync::Mutex::new(None));
        let (taken, failed) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (opened_tx, opened) = std::sync::mpsc::sync_channel(1);
        let thread = {
            let (latest, stop, ended) = (latest.clone(), stop.clone(), ended.clone());
            let (taken, failed) = (taken.clone(), failed.clone());
            std::thread::spawn(move || {
                let first = open().and_then(|mut grabber| {
                    let first = sixteen_by_nine(&grabber.grab()?);
                    Ok((grabber, first))
                });
                let mut grabber = match first {
                    Ok((grabber, first)) => {
                        latest.send_replace(Some(Arc::new(first)));
                        let _ = opened_tx.send(Ok(grabber.info()));
                        grabber
                    }
                    Err(e) => {
                        let _ = opened_tx.send(Err(e));
                        return;
                    }
                };
                let mut said = false;
                let every = Duration::from_secs(1) / MAX_FPS;
                let mut next = Instant::now() + every;
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(next.saturating_duration_since(Instant::now()));
                    next = (next + every).max(Instant::now());
                    let picture = match grabber.grab() {
                        Ok(picture) => {
                            taken.fetch_add(1, Ordering::Relaxed);
                            picture
                        }
                        Err(e) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            if let Some(why) = grabber.ended() {
                                *ended.locked() = Some(why);
                                break;
                            }
                            if !said {
                                eprintln!("{} screen: a picture couldn't be taken: {e:#}", crate::unix_now());
                                said = true;
                            }
                            continue;
                        }
                    };
                    let picture = sixteen_by_nine(&picture);
                    latest.send_if_modified(|shown| {
                        if shown.as_deref() == Some(&picture) {
                            return false;
                        }
                        *shown = Some(Arc::new(picture));
                        true
                    });
                }
            })
        };
        let info = match opened.recv() {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => return Err(e),
            Err(_) => anyhow::bail!("the screen's thread stopped"),
        };
        Ok(Screen { info, latest, stop, ended, taken, failed, thread: Some(thread) })
    }

    /// Why sharing's ended, if it has from the screen's side (the desktop
    /// stopped it, say).
    pub fn ended(&self) -> Option<String> {
        self.ended.locked().clone()
    }

    pub fn info(&self) -> &ScreenInfo {
        &self.info
    }

    /// Pictures taken since the first, and tries that failed (`swarm
    /// screen-check`).
    pub fn counts(&self) -> (u64, u64) {
        (self.taken.load(Ordering::Relaxed), self.failed.load(Ordering::Relaxed))
    }

    /// The newest picture taken.
    pub fn latest(&self) -> Option<Arc<Picture>> {
        self.latest.borrow().clone()
    }

    /// Follows the newest picture as it changes — for a video call.
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<Picture>>> {
        self.latest.subscribe()
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `picture` with black bars added to make it 16:9 (in the middle, as
/// little added as can be), or as it is if it's that already.
fn sixteen_by_nine(picture: &Picture) -> Picture {
    let (width, height) = (picture.width, picture.height);
    let (out_w, out_h) = if width * 9 >= height * 16 { (width, (width * 9).div_ceil(16)) } else { ((height * 16).div_ceil(9), height) };
    if (out_w, out_h) == (width, height) {
        return picture.clone();
    }
    let (left, top) = ((out_w - width) / 2, (out_h - height) / 2);
    let mut rgb = vec![0u8; out_w * out_h * 3];
    for y in 0..height {
        let to = ((top + y) * out_w + left) * 3;
        rgb[to..to + width * 3].copy_from_slice(&picture.data[y * width * 3..(y + 1) * width * 3]);
    }
    Picture::rgb(out_w, out_h, rgb)
}

/// Blends a pointer's picture (premultiplied ARGB, as both X11 and
/// Windows give it, `width` across) onto `picture` with its top left at
/// (`x`, `y`) — partly or wholly off the edge as may be.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn draw_pointer(picture: &mut Picture, argb: &[u32], width: usize, x: i32, y: i32) {
    if width == 0 {
        return;
    }
    for (row, pixels) in argb.chunks(width).enumerate() {
        let py = y + row as i32;
        if py < 0 || py >= picture.height as i32 {
            continue;
        }
        for (column, &pixel) in pixels.iter().enumerate() {
            let px = x + column as i32;
            if px < 0 || px >= picture.width as i32 {
                continue;
            }
            let alpha = pixel >> 24;
            if alpha == 0 {
                continue;
            }
            let at = (py as usize * picture.width + px as usize) * 3;
            for (channel, shift) in [16, 8, 0].into_iter().enumerate() {
                let over = (pixel >> shift) & 0xff;
                let under = picture.data[at + channel] as u32;
                picture.data[at + channel] = (over + under * (255 - alpha) / 255).min(255) as u8;
            }
        }
    }
}

/// A made-up screen: lines of text-like marks on a light background, a
/// line added now and then, and an arrow pointer drifting over it.
struct TestScreen {
    started: Instant,
}

impl TestScreen {
    const WIDTH: usize = 1920;
    const HEIGHT: usize = 1200;

    fn new() -> TestScreen {
        TestScreen { started: Instant::now() }
    }
}

impl Grabber for TestScreen {
    fn info(&self) -> ScreenInfo {
        ScreenInfo { name: format!("test screen {}×{}", TestScreen::WIDTH, TestScreen::HEIGHT), width: TestScreen::WIDTH as u32, height: TestScreen::HEIGHT as u32 }
    }

    fn grab(&mut self) -> Result<Picture> {
        let (width, height) = (TestScreen::WIDTH, TestScreen::HEIGHT);
        let mut rgb = vec![246u8; width * height * 3];
        let elapsed = self.started.elapsed().as_secs_f32();
        let lines = ((elapsed * 2.0) as usize % (height / 16)) + 1;
        let mut seed = 9u32;
        let mut next = move |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 16) % n
        };
        for line in 0..lines {
            let length = 10 + next(120) as usize;
            for column in 0..length.min(width / 8 - 2) {
                if next(6) == 0 {
                    continue;
                }
                let (x0, y0) = (10 + column * 8, 4 + line * 16);
                for _ in 0..2 + next(2) {
                    let (sx, sy, w, h) = [(0, 0, 1, 10), (5, 0, 1, 10), (0, 0, 6, 1), (0, 5, 6, 1), (0, 9, 6, 1)][next(5) as usize];
                    for y in y0 + sy..y0 + sy + h {
                        let at = (y * width + x0 + sx) * 3;
                        for px in rgb[at..at + w * 3].as_chunks_mut::<3>().0 {
                            px.copy_from_slice(&[30, 30, 40]);
                        }
                    }
                }
            }
        }
        let mut picture = Picture::rgb(width, height, rgb);
        let arrow: Vec<u32> = (0..18)
            .flat_map(|y: usize| (0..12).map(move |x: usize| if x > y * 2 / 3 { 0 } else if x == 0 || x == y * 2 / 3 || y == 17 { 0xff00_0000 } else { 0xffff_ffff }))
            .collect();
        let (x, y) = ((elapsed * 0.7).sin() * 600.0 + 960.0, (elapsed * 0.5).cos() * 400.0 + 600.0);
        draw_pointer(&mut picture, &arrow, 12, x as i32, y as i32);
        Ok(picture)
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod x11 {
    use super::{draw_pointer, Grabber, ScreenInfo};
    use anyhow::{anyhow, bail, Context, Result};
    use swarm_core::video::Picture;
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xfixes::ConnectionExt as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, Window};
    use x11rb::rust_connection::RustConnection;

    /// The primary monitor (or, with none said to be, the first; or with
    /// no monitors known, the whole screen) of the X11 display `DISPLAY`
    /// names.
    pub struct X11Grabber {
        conn: RustConnection,
        root: Window,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        /// The pointer can be drawn in (the XFIXES extension's there).
        pointer: bool,
        /// Pixels are 32 bits, least significant byte first: blue, green,
        /// red, unused — else 24 bits, the same order.
        bytes_per_pixel: usize,
    }

    impl X11Grabber {
        pub fn open() -> Result<X11Grabber> {
            let (conn, screen) = x11rb::connect(None).context("connecting to the X11 display")?;
            let setup = conn.setup();
            let root_screen = &setup.roots[screen];
            let root = root_screen.root;
            if setup.image_byte_order != ImageOrder::LSB_FIRST {
                bail!("this X11 display's pixels are in an order not supported");
            }
            let depth = root_screen.root_depth;
            let bits = setup.pixmap_formats.iter().find(|f| f.depth == depth).map(|f| f.bits_per_pixel).ok_or_else(|| anyhow!("no pixel format for depth {depth}"))?;
            if depth != 24 || !(bits == 32 || bits == 24) {
                bail!("only 24-bit colour screens can be shared (this is {depth}-bit)");
            }
            let (x, y, width, height) = shared_area(&conn, screen);
            let pointer = conn.xfixes_query_version(5, 0).ok().and_then(|c| c.reply().ok()).is_some();
            Ok(X11Grabber { conn, root, x, y, width, height, pointer, bytes_per_pixel: bits as usize / 8 })
        }
    }

    /// Where on the X11 screen numbered `screen` what's shared is: its
    /// primary monitor (or, with none said to be, the first; or with no
    /// monitors known, the whole screen), as x, y, width and height.
    pub(crate) fn shared_area(conn: &RustConnection, screen: usize) -> (i16, i16, u16, u16) {
        let root_screen = &conn.setup().roots[screen];
        let mut area = (0, 0, root_screen.width_in_pixels, root_screen.height_in_pixels);
        if let Ok(monitors) = conn.randr_get_monitors(root_screen.root, true).map_err(anyhow::Error::from).and_then(|c| c.reply().map_err(anyhow::Error::from)) {
            if let Some(m) = monitors.monitors.iter().find(|m| m.primary).or(monitors.monitors.first()) {
                area = (m.x, m.y, m.width, m.height);
            }
        }
        area
    }

    impl Grabber for X11Grabber {
        fn info(&self) -> ScreenInfo {
            ScreenInfo { name: format!("screen {}×{}", self.width, self.height), width: self.width as u32, height: self.height as u32 }
        }

        fn grab(&mut self) -> Result<Picture> {
            let image = self.conn.get_image(ImageFormat::Z_PIXMAP, self.root, self.x, self.y, self.width, self.height, !0)?.reply()?;
            let (width, height) = (self.width as usize, self.height as usize);
            let bpp = self.bytes_per_pixel;
            let stride = (width * bpp).div_ceil(4) * 4;
            if image.data.len() < stride * height {
                bail!("the screen's picture came back short");
            }
            let mut rgb = Vec::with_capacity(width * height * 3);
            for row in image.data.chunks_exact(stride).take(height) {
                for px in row[..width * bpp].chunks_exact(bpp) {
                    rgb.extend_from_slice(&[px[2], px[1], px[0]]);
                }
            }
            let mut picture = Picture::rgb(width, height, rgb);
            if self.pointer {
                if let Ok(cursor) = self.conn.xfixes_get_cursor_image().map_err(anyhow::Error::from).and_then(|c| c.reply().map_err(anyhow::Error::from)) {
                    let x = cursor.x as i32 - cursor.xhot as i32 - self.x as i32;
                    let y = cursor.y as i32 - cursor.yhot as i32 - self.y as i32;
                    draw_pointer(&mut picture, &cursor.cursor_image, cursor.width as usize, x, y);
                }
            }
            Ok(picture)
        }
    }
}

#[cfg(windows)]
mod gdi {
    use super::{Grabber, ScreenInfo};
    use anyhow::{bail, Result};
    use swarm_core::video::Picture;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
        DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
    };
    use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    use windows::Win32::UI::WindowsAndMessaging::{
        DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HICON, ICONINFO, SM_CXSCREEN, SM_CYSCREEN,
    };

    /// The primary monitor, through GDI: copied into a bitmap of our own,
    /// the pointer drawn on, then read out. Made, used and let go on the
    /// one thread (see `Screen::start`): the screen's handle is that
    /// thread's.
    pub struct GdiGrabber {
        screen: HDC,
        memory: HDC,
        bitmap: HBITMAP,
        width: i32,
        height: i32,
    }

    impl GdiGrabber {
        pub fn open() -> Result<GdiGrabber> {
            unsafe {
                let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let (width, height) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
                if width <= 0 || height <= 0 {
                    bail!("couldn't find the screen's size");
                }
                let screen = GetDC(None);
                if screen.is_invalid() {
                    bail!("couldn't reach the screen");
                }
                let memory = CreateCompatibleDC(Some(screen));
                let bitmap = CreateCompatibleBitmap(screen, width, height);
                if memory.is_invalid() || bitmap.is_invalid() {
                    if !bitmap.is_invalid() {
                        let _ = DeleteObject(bitmap.into());
                    }
                    if !memory.is_invalid() {
                        let _ = DeleteDC(memory);
                    }
                    ReleaseDC(None, screen);
                    bail!("couldn't make room for the screen's picture");
                }
                Ok(GdiGrabber { screen, memory, bitmap, width, height })
            }
        }
    }

    impl Grabber for GdiGrabber {
        fn info(&self) -> ScreenInfo {
            ScreenInfo { name: format!("screen {}×{}", self.width, self.height), width: self.width as u32, height: self.height as u32 }
        }

        fn grab(&mut self) -> Result<Picture> {
            let (width, height) = (self.width as usize, self.height as usize);
            let mut bgra = vec![0u8; width * height * 4];
            unsafe {
                let old: HGDIOBJ = SelectObject(self.memory, self.bitmap.into());
                let copied = BitBlt(self.memory, 0, 0, self.width, self.height, Some(self.screen), 0, 0, SRCCOPY | CAPTUREBLT);
                if let Err(e) = copied {
                    SelectObject(self.memory, old);
                    return Err(e.into());
                }
                let mut cursor = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
                if GetCursorInfo(&mut cursor).is_ok() && cursor.flags.0 & CURSOR_SHOWING.0 != 0 && !cursor.hCursor.is_invalid() {
                    let icon = HICON(cursor.hCursor.0);
                    let mut info = ICONINFO::default();
                    let (hot_x, hot_y) = if GetIconInfo(icon, &mut info).is_ok() {
                        if !info.hbmMask.is_invalid() {
                            let _ = DeleteObject(info.hbmMask.into());
                        }
                        if !info.hbmColor.is_invalid() {
                            let _ = DeleteObject(info.hbmColor.into());
                        }
                        (info.xHotspot as i32, info.yHotspot as i32)
                    } else {
                        (0, 0)
                    };
                    let _ = DrawIconEx(self.memory, cursor.ptScreenPos.x - hot_x, cursor.ptScreenPos.y - hot_y, icon, 0, 0, 0, None, DI_NORMAL);
                }
                SelectObject(self.memory, old);
                let mut header = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: self.width,
                        biHeight: -self.height,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let rows = GetDIBits(self.memory, self.bitmap, 0, self.height as u32, Some(bgra.as_mut_ptr().cast()), &mut header, DIB_RGB_COLORS);
                if rows != self.height {
                    bail!("couldn't read the screen's picture");
                }
            }
            let mut rgb = Vec::with_capacity(width * height * 3);
            for px in bgra.as_chunks::<4>().0 {
                rgb.extend_from_slice(&[px[2], px[1], px[0]]);
            }
            Ok(Picture::rgb(width, height, rgb))
        }
    }

    impl Drop for GdiGrabber {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteObject(self.bitmap.into());
                let _ = DeleteDC(self.memory);
                ReleaseDC(None, self.screen);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_of_another_shape_gets_bars_not_cropped() {
        let tall = Picture::rgb(16, 10, vec![200; 16 * 10 * 3]);
        let made = sixteen_by_nine(&tall);
        assert_eq!((made.width, made.height), (18, 10));
        assert_eq!(made.pixel(0, 5), [0, 0, 0], "a bar at the left");
        assert_eq!(made.pixel(1, 5), [200, 200, 200]);
        assert_eq!(made.pixel(16, 5), [200, 200, 200]);
        assert_eq!(made.pixel(17, 5), [0, 0, 0], "and at the right");
        let wide = Picture::rgb(32, 9, vec![200; 32 * 9 * 3]);
        let made = sixteen_by_nine(&wide);
        assert_eq!((made.width, made.height), (32, 18));
        assert_eq!(made.pixel(5, 0), [0, 0, 0]);
        assert_eq!(made.pixel(5, 3), [0, 0, 0]);
        assert_eq!(made.pixel(5, 4), [200, 200, 200]);
        assert_eq!(made.pixel(5, 12), [200, 200, 200]);
        assert_eq!(made.pixel(5, 13), [0, 0, 0]);
        let right = Picture::rgb(32, 18, vec![7; 32 * 18 * 3]);
        assert_eq!(sixteen_by_nine(&right), right);
    }

    #[test]
    fn the_pointer_is_drawn_over_the_screen_and_clipped_at_its_edges() {
        let mut picture = Picture::rgb(4, 4, vec![100; 4 * 4 * 3]);
        let pointer = [0xffff_0000, 0x0000_0000, 0x8080_8080, 0xffff_0000];
        draw_pointer(&mut picture, &pointer, 2, 3, 3);
        assert_eq!(picture.pixel(3, 3), [255, 0, 0]);
        assert_eq!(picture.pixel(2, 3), [100, 100, 100], "nothing outside the pointer");
        draw_pointer(&mut picture, &pointer, 2, -1, 0);
        assert_eq!(picture.pixel(0, 0), [100, 100, 100], "see-through");
        assert_eq!(picture.pixel(0, 1), [255, 0, 0]);
        let mut picture = Picture::rgb(1, 1, vec![100; 3]);
        draw_pointer(&mut picture, &[0x8080_8080], 1, 0, 0);
        assert_eq!(picture.pixel(0, 0), [128 + 49, 128 + 49, 128 + 49]);
    }

    /// A screen that gives three pictures, then is stopped from the
    /// desktop's side.
    struct Stopping(u32);

    impl Grabber for Stopping {
        fn info(&self) -> ScreenInfo {
            ScreenInfo { name: "stopping".into(), width: 16, height: 9 }
        }
        fn grab(&mut self) -> Result<Picture> {
            self.0 += 1;
            anyhow::ensure!(self.0 <= 3, "stopped");
            Ok(Picture::rgb(16, 9, vec![self.0 as u8; 16 * 9 * 3]))
        }
        fn ended(&self) -> Option<String> {
            (self.0 > 3).then(|| "stopped by the desktop".to_string())
        }
    }

    /// A screen that, like Windows' (GDI's handles belong to the thread
    /// that got them), only gives pictures on the thread that opened it:
    /// a new one each time.
    struct ThreadBound(std::thread::ThreadId, u8);

    impl Grabber for ThreadBound {
        fn info(&self) -> ScreenInfo {
            ScreenInfo { name: "thread-bound".into(), width: 16, height: 9 }
        }
        fn grab(&mut self) -> Result<Picture> {
            anyhow::ensure!(std::thread::current().id() == self.0, "not on the thread that opened it");
            self.1 = self.1.wrapping_add(1);
            Ok(Picture::rgb(16, 9, vec![self.1; 16 * 9 * 3]))
        }
    }

    /// Found by the user: sharing a Windows screen showed its first
    /// picture, then froze. The screen was opened on one thread and its
    /// pictures taken on another, every one failing.
    #[test]
    fn a_screen_keeps_giving_pictures_when_it_only_works_where_it_was_opened() {
        let screen = Screen::start(|| Ok(ThreadBound(std::thread::current().id(), 0))).unwrap();
        let first = screen.latest().unwrap().data[0];
        let since = Instant::now();
        while screen.latest().unwrap().data[0] == first && since.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(screen.latest().unwrap().data[0], first, "pictures after the first");
        assert!(screen.counts().0 >= 1 && screen.counts().1 == 0, "{:?}", screen.counts());
    }

    #[test]
    fn a_screen_stopped_from_the_desktops_side_says_so() {
        let screen = Screen::start(|| Ok(Stopping(0))).unwrap();
        let since = Instant::now();
        while screen.ended().is_none() && since.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(screen.ended().as_deref(), Some("stopped by the desktop"));
        assert_eq!(screen.latest().unwrap().data[0], 3, "the last picture stays");
    }

    #[test]
    fn the_test_screen_changes_and_is_shared_whole() {
        std::env::set_var("SWARM_SCREEN", "test");
        let screen = Screen::open().unwrap();
        let first = screen.latest().unwrap();
        assert_eq!((first.width, first.height), (2134, 1200), "16:10 with bars");
        let mut frames = screen.subscribe();
        frames.mark_unchanged();
        let changed = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async { tokio::time::timeout(Duration::from_secs(2), frames.changed()).await });
        assert!(changed.is_ok_and(|r| r.is_ok()), "a new picture as the pointer moves");
    }
}
