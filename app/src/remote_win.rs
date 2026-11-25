//! Being controlled on Windows (see `remote`): what's sent is done with
//! `SendInput`, on the primary monitor (the one shared: `screen`'s
//! `gdi`).
//!
//! - Characters typed go as Unicode (`KEYEVENTF_UNICODE`), whatever the
//!   keyboard here: a UK `£` comes out as `£` on a US one. Keys held and
//!   shortcuts go as the keys this layout has for them.
//! - Your own mouse and keys are told from ours by low-level hooks on a
//!   thread of their own: everything we send is marked
//!   (`dwExtraInfo`), and anything not marked is yours.
//! - Like every program that isn't elevated, it can't reach a window
//!   that is (run as administrator), nor UAC's prompts: Windows drops
//!   what's sent to them, without saying.

use swarm_core::lock::Locked;
use crate::remote::Injector;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use swarm_core::remote::{keys, BUTTON_BACK, BUTTON_FORWARD, BUTTON_LEFT, BUTTON_MIDDLE, BUTTON_RIGHT};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_QUIT, XBUTTON1, XBUTTON2,
};

/// Marks what we send, for the hooks (`dwExtraInfo`): "swrm".
const OURS: usize = 0x7377_726d;

/// When our own mouse or keys were last used, for the injector.
type Seen = Arc<Mutex<Option<Instant>>>;

thread_local! {
    /// The hook thread's: where its hooks say they've seen you.
    static HOOKED: std::cell::RefCell<Option<Seen>> = const { std::cell::RefCell::new(None) };
}

pub struct WinInjector {
    /// The keys pressed for them, by what was sent, and the key pressed
    /// for each (let go the same, whatever's changed since).
    pressed: HashMap<char, Key>,
    buttons: Vec<u8>,
    seen: Seen,
    /// The hook thread, and its id (to tell it to end).
    hooks: Option<(std::thread::JoinHandle<()>, u32)>,
}

/// A key as Windows has it: a virtual key, or (`unicode`) a character
/// this layout has no key for.
#[derive(Clone, Copy)]
struct Key {
    vk: u16,
    unicode: Option<u16>,
}

impl WinInjector {
    pub fn open() -> Result<WinInjector> {
        let seen = Seen::default();
        let id = Arc::new(AtomicU32::new(0));
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let (thread_seen, thread_id) = (seen.clone(), id.clone());
        let thread = std::thread::Builder::new().name("windows-control".into()).spawn(move || {
            // SAFETY: plain Win32 calls; the hooks are taken off before
            // the thread ends.
            unsafe {
                thread_id.store(GetCurrentThreadId(), Ordering::Relaxed);
                HOOKED.with_borrow_mut(|h| *h = Some(thread_seen));
                let module = GetModuleHandleW(None).ok().map(|m| m.into());
                let mouse = if under_wine() { Err(windows::core::Error::empty()) } else { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) };
                let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0);
                if (mouse.is_err() && !under_wine()) || keyboard.is_err() {
                    eprintln!("{} couldn't watch for your own input: your own mouse won't pause whoever's controlling", crate::unix_now());
                }
                let _ = ready_tx.send(());
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {}
                for hook in [mouse, keyboard].into_iter().flatten() {
                    let _ = UnhookWindowsHookEx(hook);
                }
            }
        })?;
        let _ = ready.recv();
        Ok(WinInjector { pressed: HashMap::new(), buttons: Vec::new(), seen, hooks: Some((thread, id.load(Ordering::Relaxed))) })
    }

    fn send(&self, inputs: Vec<INPUT>) -> Result<()> {
        // SAFETY: a slice of whole INPUTs, and their size.
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if (sent as usize) < inputs.len() {
            bail!("Windows took {sent} of {} things sent: {}", inputs.len(), windows::core::Error::from_thread());
        }
        Ok(())
    }
}

/// Whether this is Wine, not Windows.
fn under_wine() -> bool {
    // SAFETY: plain lookups.
    unsafe { GetModuleHandleW(windows::core::w!("ntdll.dll")).is_ok_and(|ntdll| GetProcAddress(ntdll, windows::core::s!("wine_get_version")).is_some()) }
}

/// The mouse moved, a button or the wheel, marked as ours.
fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: OURS } },
    }
}

/// A key pressed or let go, marked as ours.
fn keyboard(key: Key, down: bool) -> INPUT {
    let up = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    let ki = match key.unicode {
        Some(unit) => KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: unit, dwFlags: KEYEVENTF_UNICODE | up, time: 0, dwExtraInfo: OURS },
        None => {
            let vk = VIRTUAL_KEY(key.vk);
            // SAFETY: a plain lookup.
            let scan = unsafe { MapVirtualKeyW(key.vk as u32, MAPVK_VK_TO_VSC) } as u16;
            let extended = if extended(vk) { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) };
            KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: extended | up, time: 0, dwExtraInfo: OURS }
        }
    };
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } }
}

/// Keys that are on the extended part of a keyboard (an E0 scan code):
/// without saying so, the arrows come out as the number pad's.
fn extended(vk: VIRTUAL_KEY) -> bool {
    matches!(
        vk,
        VK_RCONTROL | VK_RMENU | VK_INSERT | VK_DELETE | VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_LWIN | VK_RWIN | VK_APPS | VK_SNAPSHOT
    )
}

/// The virtual key of a key that types nothing (see `keys`), or `None`.
fn named(key: char) -> Option<VIRTUAL_KEY> {
    Some(match key {
        keys::BACKSPACE => VK_BACK,
        keys::TAB => VK_TAB,
        keys::RETURN => VK_RETURN,
        keys::ESCAPE => VK_ESCAPE,
        keys::DELETE => VK_DELETE,
        keys::SHIFT => VK_LSHIFT,
        keys::SHIFT_R => VK_RSHIFT,
        keys::CONTROL => VK_LCONTROL,
        keys::CONTROL_R => VK_RCONTROL,
        keys::ALT => VK_LMENU,
        keys::ALT_GR => VK_RMENU,
        keys::CAPS_LOCK => VK_CAPITAL,
        keys::META => VK_LWIN,
        keys::META_R => VK_RWIN,
        keys::SPACE => VK_SPACE,
        keys::UP => VK_UP,
        keys::DOWN => VK_DOWN,
        keys::LEFT => VK_LEFT,
        keys::RIGHT => VK_RIGHT,
        keys::INSERT => VK_INSERT,
        keys::HOME => VK_HOME,
        keys::END => VK_END,
        keys::PAGE_UP => VK_PRIOR,
        keys::PAGE_DOWN => VK_NEXT,
        keys::SCROLL_LOCK => VK_SCROLL,
        keys::PAUSE => VK_PAUSE,
        keys::PRINT_SCREEN => VK_SNAPSHOT,
        keys::MENU => VK_APPS,
        k if (keys::F1..=keys::F24).contains(&k) => VIRTUAL_KEY(VK_F1.0 + (k as u32 - keys::F1 as u32) as u16),
        _ => return None,
    })
}

/// The key for `key`: a named one's, the key this layout types it with
/// (Shift and the rest are sent as keys of their own), or as Unicode.
fn key_for(key: char) -> Key {
    if let Some(vk) = named(key) {
        return Key { vk: vk.0, unicode: None };
    }
    let mut units = [0u16; 2];
    let units = key.encode_utf16(&mut units);
    if units.len() == 1 {
        // SAFETY: a plain lookup. Low byte the key, -1 none.
        let found = unsafe { VkKeyScanW(units[0]) };
        if found != -1 {
            return Key { vk: (found as u16) & 0xff, unicode: None };
        }
    }
    Key { vk: 0, unicode: Some(units[0]) }
}

impl Injector for WinInjector {
    fn pointer(&mut self, x: f64, y: f64) -> Result<()> {
        let (x, y) = ((x * 65535.0).round() as i32, (y * 65535.0).round() as i32);
        self.send(vec![mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, x, y, 0)])
    }

    fn button(&mut self, button: u8, down: bool) -> Result<()> {
        let (press, release, data) = match button {
            BUTTON_LEFT => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
            BUTTON_MIDDLE => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
            BUTTON_RIGHT => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
            BUTTON_BACK => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1 as i32),
            BUTTON_FORWARD => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2 as i32),
            _ => return Ok(()),
        };
        if down == self.buttons.contains(&button) {
            return Ok(());
        }
        if down {
            self.buttons.push(button);
        } else {
            self.buttons.retain(|b| *b != button);
        }
        self.send(vec![mouse(if down { press } else { release }, 0, 0, data)])
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        let mut inputs = Vec::new();
        if dy != 0 {
            inputs.push(mouse(MOUSEEVENTF_WHEEL, 0, 0, dy));
        }
        if dx != 0 {
            inputs.push(mouse(MOUSEEVENTF_HWHEEL, 0, 0, dx));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        self.send(inputs)
    }

    fn key(&mut self, key: char, down: bool) -> Result<()> {
        if down {
            if self.pressed.contains_key(&key) {
                return Ok(());
            }
            let found = key_for(key);
            self.pressed.insert(key, found);
            self.send(vec![keyboard(found, true)])
        } else if let Some(found) = self.pressed.remove(&key) {
            self.send(vec![keyboard(found, false)])
        } else {
            Ok(())
        }
    }

    fn text(&mut self, text: &str) -> Result<()> {
        let mut inputs = Vec::new();
        for c in text.chars().filter(|c| !c.is_control()) {
            let mut units = [0u16; 2];
            for &unit in c.encode_utf16(&mut units).iter() {
                let key = Key { vk: 0, unicode: Some(unit) };
                inputs.extend([keyboard(key, true), keyboard(key, false)]);
            }
        }
        if inputs.is_empty() {
            return Ok(());
        }
        self.send(inputs)
    }

    fn local_input_at(&self) -> Option<Instant> {
        *self.seen.locked()
    }
}

impl Drop for WinInjector {
    fn drop(&mut self) {
        let mut inputs: Vec<INPUT> = std::mem::take(&mut self.pressed).into_values().map(|key| keyboard(key, false)).collect();
        for button in std::mem::take(&mut self.buttons) {
            let (release, data) = match button {
                BUTTON_LEFT => (MOUSEEVENTF_LEFTUP, 0),
                BUTTON_MIDDLE => (MOUSEEVENTF_MIDDLEUP, 0),
                BUTTON_RIGHT => (MOUSEEVENTF_RIGHTUP, 0),
                BUTTON_BACK => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
                _ => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
            };
            inputs.push(mouse(release, 0, 0, data));
        }
        if !inputs.is_empty() {
            let _ = self.send(inputs);
        }
        if let Some((thread, id)) = self.hooks.take() {
            // SAFETY: a message to the hook thread, which ends on it.
            if unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) }.is_ok() {
                let _ = thread.join();
            }
        }
    }
}

/// Anything not marked as ours is yours.
fn seen_yours(extra: usize) {
    if extra != OURS {
        HOOKED.with_borrow(|seen| {
            if let Some(seen) = seen {
                *seen.locked() = Some(Instant::now());
            }
        });
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_MOUSE_LL, lparam is an MSLLHOOKSTRUCT.
        seen_yours(unsafe { (*(lparam.0 as *const MSLLHOOKSTRUCT)).dwExtraInfo });
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL, lparam is a KBDLLHOOKSTRUCT.
        seen_yours(unsafe { (*(lparam.0 as *const KBDLLHOOKSTRUCT)).dwExtraInfo });
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::time::Duration;
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetSystemMetrics, PeekMessageW, RegisterClassW, SetForegroundWindow, ShowWindow, PM_REMOVE,
        SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, TranslateMessage, WINDOW_EX_STYLE, WM_CHAR, WM_RBUTTONDOWN, WNDCLASSW, WS_POPUP, WS_VISIBLE,
    };

    thread_local! {
        /// What the window's been sent: characters, and `R` a right click.
        static GOT: RefCell<String> = const { RefCell::new(String::new()) };
    }

    unsafe extern "system" fn proc(window: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_CHAR => GOT.with_borrow_mut(|got| got.extend(char::decode_utf16([wparam.0 as u16]).flatten())),
            WM_RBUTTONDOWN => GOT.with_borrow_mut(|got| got.push('R')),
            _ => {}
        }
        unsafe { DefWindowProcW(window, msg, wparam, lparam) }
    }

    /// The window's messages handled for a while; what it got, taken.
    fn got() -> String {
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            let mut msg = MSG::default();
            // SAFETY: this thread's own messages.
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        GOT.with_borrow_mut(std::mem::take)
    }

    /// Run under Wine (on a display of its own: never the one in use);
    /// skipped elsewhere if there's no screen.
    #[test]
    fn what_is_sent_is_done_on_a_windows_screen_and_not_taken_for_your_own() {
        // SAFETY: plain Win32 calls on this thread's own window.
        let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if width <= 0 {
            eprintln!("no screen here: skipped");
            return;
        }
        let window = unsafe {
            let class = WNDCLASSW { lpfnWndProc: Some(proc), lpszClassName: w!("swarm-control-test"), ..Default::default() };
            RegisterClassW(&class);
            let window = CreateWindowExW(WINDOW_EX_STYLE(0), w!("swarm-control-test"), w!("test"), WS_POPUP | WS_VISIBLE, 0, 0, width, height, None, None, None, None).unwrap();
            let _ = ShowWindow(window, SW_SHOW);
            let _ = SetForegroundWindow(window);
            window
        };
        got();
        let mut injector = WinInjector::open().unwrap();

        injector.pointer(0.5, 0.25).unwrap();
        got();
        let mut at = POINT::default();
        unsafe { GetCursorPos(&mut at) }.unwrap();
        assert!((at.x - width / 2).abs() <= 1 && (at.y - height / 4).abs() <= 1, "pointer at {at:?} on {width}×{height}");
        injector.button(BUTTON_RIGHT, true).unwrap();
        injector.button(BUTTON_RIGHT, false).unwrap();
        assert_eq!(got(), "R");

        injector.text("aB £1!").unwrap();
        assert_eq!(got(), "aB £1!");
        injector.key('q', true).unwrap();
        injector.key('q', false).unwrap();
        assert_eq!(got(), "q");
        assert!(injector.local_input_at().is_none(), "our own input taken for theirs");

        let mut key = [keyboard(Key { vk: VK_SHIFT.0, unicode: None }, true), keyboard(Key { vk: VK_SHIFT.0, unicode: None }, false)];
        key[0].Anonymous.ki.dwExtraInfo = 0;
        key[1].Anonymous.ki.dwExtraInfo = 0;
        unsafe { SendInput(&key, std::mem::size_of::<INPUT>() as i32) };
        got();
        assert!(injector.local_input_at().is_some(), "your own keys not seen");
        if !under_wine() {
            *injector.seen.locked() = None;
            let mut moved = [mouse(MOUSEEVENTF_MOVE, 1, 0, 0)];
            moved[0].Anonymous.mi.dwExtraInfo = 0;
            unsafe { SendInput(&moved, std::mem::size_of::<INPUT>() as i32) };
            got();
            assert!(injector.local_input_at().is_some(), "your own mouse not seen");
        }

        drop(injector);
        unsafe { DestroyWindow(window) }.unwrap();
    }
}
