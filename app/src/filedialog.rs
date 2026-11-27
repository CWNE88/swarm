//! The system's own file dialog, for the window: a file to send, or a
//! folder to save one in. On Linux it's the desktop's (GNOME's, KDE's…),
//! asked for through its portal (`org.freedesktop.portal.FileChooser`,
//! over D-Bus, as Flatpak apps and browsers do); on macOS, AppKit's open
//! panel; on Windows, the common file dialog. Where there's no system
//! dialog, the window uses its own browser instead; the TUI always has
//! its own.
//!
//! Each blocks until the person has chosen: called on a thread of its own.
//!
//! On Linux the portal's asked by a helper: swarm started afresh with
//! `HELPER` (see `helper`). Swarm keeps its memory out of dumps (see
//! `swarm_core::secure::no_dumps`), which also makes its `/proc/<pid>/root`
//! unreadable to the user's other processes — and the portal reads it to
//! know who's asking, so it turned swarm down ("Unable to open
//! /proc/…/root"). A program started afresh can be read again, and the
//! helper never holds a key.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// The window a dialog is for. Given it, the desktop puts the dialog in
/// front of that window, as its own; without, the dialog's a stranger's,
/// and GNOME (keeping a window that wasn't clicked from taking over) left
/// it behind Swarm's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Parent {
    /// None known (Wayland, the phone, a test).
    #[default]
    None,
    /// An X11 window's id.
    X11(u32),
    /// A Windows window's handle.
    Win32(isize),
}

impl Parent {
    /// As the portal takes it (`parent_window`): "x11:" and the id in
    /// hex, or nothing.
    pub fn portal_id(self) -> String {
        match self {
            Parent::X11(window) => format!("x11:{window:x}"),
            Parent::None | Parent::Win32(_) => String::new(),
        }
    }
}

/// A file to send: `Ok(None)` if the dialog was closed without one, an
/// error if there's no dialog here.
pub fn choose_file(title: &str, start: &Path, parent: Parent) -> Result<Option<PathBuf>> {
    choose(title, start, false, parent)
}

/// A folder to save in, as `choose_file`.
pub fn choose_folder(title: &str, start: &Path, parent: Parent) -> Result<Option<PathBuf>> {
    choose(title, start, true, parent)
}

/// The first argument that makes swarm the file dialog's helper.
pub const HELPER: &str = "--file-dialog-helper";

/// Asks the helper (swarm itself, started afresh) to ask the portal.
#[cfg(target_os = "linux")]
fn choose(title: &str, start: &Path, folder: bool, parent: Parent) -> Result<Option<PathBuf>> {
    use anyhow::Context;
    let out = std::process::Command::new(swarm_itself()?)
        .arg(HELPER)
        .arg(if folder { "folder" } else { "file" })
        .arg(title)
        .arg(start)
        .arg(parent.portal_id())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("starting the file dialog's helper")?;
    answer_of(&out.stdout)
}

/// Swarm's own program, to start afresh as the helper: the one running,
/// through `/proc/self/exe`. Its path's no good once it's been replaced
/// (built again, updated) while Swarm runs: Linux has it as
/// "…/swarm (deleted)", which isn't there to start.
#[cfg(target_os = "linux")]
pub(crate) fn swarm_itself() -> Result<PathBuf> {
    Ok(PathBuf::from("/proc/self/exe"))
}

/// Run as the helper (`swarm --file-dialog-helper file|folder <title>
/// <start> <parent>`, the parent as `Parent::portal_id` has it): asks the portal, and writes what was chosen to stdout (see
/// `answer_bytes`). Its exit code.
#[cfg(target_os = "linux")]
pub fn helper(args: &[std::ffi::OsString]) -> i32 {
    use std::io::Write;
    let answer = match args {
        [kind, title, start, parent] => ask_portal(&title.to_string_lossy(), Path::new(start), kind == "folder", &parent.to_string_lossy()),
        _ => Err(anyhow::anyhow!("the file dialog's helper takes file|folder, a title, a folder to start in and the window it's for")),
    };
    if let Err(why) = &answer {
        eprintln!("swarm: no file dialog: {why:#}");
    }
    let mut out = std::io::stdout();
    if out.write_all(&answer_bytes(&answer)).and_then(|()| out.flush()).is_err() {
        return 1;
    }
    0
}

#[cfg(target_os = "linux")]
fn ask_portal(title: &str, start: &Path, folder: bool, parent: &str) -> Result<Option<PathBuf>> {
    use anyhow::{anyhow, bail, Context};
    use std::collections::HashMap;
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::Value;
    const FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
    let conn = Connection::session().context("the desktop's session bus can't be reached")?;
    let chooser = Proxy::new(&conn, crate::portal::DESTINATION, crate::portal::PATH, FILE_CHOOSER)?;
    let version: u32 = chooser.get_property("version").context("the desktop has no file chooser portal")?;
    if folder && version < 3 {
        bail!("the desktop's file chooser can't choose folders");
    }
    let token = crate::portal::token();
    let mut options: HashMap<&str, Value> = HashMap::from([("handle_token", Value::from(token.as_str())), ("modal", Value::from(true)), ("multiple", Value::from(false))]);
    if folder {
        options.insert("directory", Value::from(true));
    }
    if version >= 4 && start.is_dir() {
        options.insert("current_folder", Value::from(folder_bytes(start)));
    }
    let Some(results) = crate::portal::request(&conn, FILE_CHOOSER, "OpenFile", &(parent, title, options), &token)? else { return Ok(None) };
    let uris: Vec<String> = results.get("uris").ok_or_else(|| anyhow!("the file chooser gave nothing"))?.try_clone()?.try_into().map_err(|e| anyhow!("the file chooser's answer: {e}"))?;
    let uri = uris.into_iter().next().ok_or_else(|| anyhow!("the file chooser gave nothing"))?;
    path_of_uri(&uri).map(Some).ok_or_else(|| anyhow!("the file chooser gave {uri}, not a file here"))
}

#[cfg(windows)]
fn choose(title: &str, start: &Path, folder: bool, parent: Parent) -> Result<Option<PathBuf>> {
    use anyhow::Context;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().context("starting COM")?;
        let chosen = (|| -> Result<Option<PathBuf>> {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).context("Windows' file dialog")?;
            let mut options = dialog.GetOptions()? | FOS_FORCEFILESYSTEM;
            if folder {
                options |= FOS_PICKFOLDERS;
            }
            dialog.SetOptions(options)?;
            dialog.SetTitle(&HSTRING::from(title))?;
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start.as_os_str()), None) {
                let _ = dialog.SetFolder(&item);
            }
            let owner = match parent {
                Parent::Win32(window) => Some(HWND(window as *mut _)),
                Parent::None | Parent::X11(_) => None,
            };
            match dialog.Show(owner) {
                Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => return Ok(None),
                shown => shown?,
            }
            let name = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = name.to_string();
            CoTaskMemFree(Some(name.0 as *const _));
            Ok(Some(PathBuf::from(path?)))
        })();
        CoUninitialize();
        chosen
    }
}

/// A copy of what's read from `from`, called `name` (made safe to save,
/// as an offered file's is), in a folder of its own in `dir`, so files of
/// one name kept at once don't meet. Where it is. The phone's: what's
/// chosen there is only lent to swarm, and a transfer reads its file
/// again later, after Swarm's restarted even.
pub fn keep_copy(name: &str, from: &mut impl std::io::Read, dir: &Path) -> Result<PathBuf> {
    use anyhow::Context;
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    let own = (1u64..)
        .map(|n| dir.join(n.to_string()))
        .find(|d| match std::fs::create_dir(d) {
            Ok(()) => true,
            Err(e) => e.kind() != std::io::ErrorKind::AlreadyExists,
        })
        .expect("a free number");
    let safe = swarm_core::transfer::sanitise_name(name);
    let path = own.join(if safe.is_empty() { "file" } else { &safe });
    let mut to = std::fs::File::create(&path).with_context(|| format!("making {}", path.display()))?;
    std::io::copy(from, &mut to).with_context(|| format!("copying into {}", path.display()))?;
    Ok(path)
}

/// The phone's own: its picker (Android's, through the app's Java: a file
/// lent to swarm, and what it's called), and its Download folder, where
/// what's accepted goes, there to be found in its Files app.
#[cfg(target_os = "android")]
pub struct Phone {
    pub pick: fn() -> Result<Option<(std::fs::File, String)>>,
    pub downloads: PathBuf,
}

#[cfg(target_os = "android")]
static PHONE: std::sync::OnceLock<Phone> = std::sync::OnceLock::new();

/// The phone's picker and folder, as the app has them (android/).
#[cfg(target_os = "android")]
pub fn set_phone(phone: Phone) {
    let _ = PHONE.set(phone);
}

/// On the phone: a file chosen in its picker, kept in Swarm's own folder
/// to be sent from (see `keep_copy`); a folder to save in, its Download.
#[cfg(target_os = "android")]
fn choose(_title: &str, _start: &Path, folder: bool, _parent: Parent) -> Result<Option<PathBuf>> {
    let phone = PHONE.get().ok_or_else(|| anyhow::anyhow!("no file picker here"))?;
    if folder {
        return Ok(Some(phone.downloads.clone()));
    }
    match (phone.pick)()? {
        None => Ok(None),
        Some((mut file, name)) => keep_copy(&name, &mut file, &crate::paths::data_dir().join("sending")).map(Some),
    }
}

#[cfg(target_os = "macos")]
fn choose(title: &str, start: &Path, folder: bool, _parent: Parent) -> Result<Option<PathBuf>> {
    use std::ffi::{c_char, CStr, CString, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    unsafe extern "C" {
        fn swarm_macos_choose_path(
            title: *const c_char, start: *const c_char, folder: bool, path: *mut *mut c_char,
        ) -> i32;
    }

    let title = CString::new(title)?;
    let start = CString::new(start.as_os_str().as_bytes())?;
    let mut path = std::ptr::null_mut();
    // The bridge blocks until it has either cancelled or handed back an
    // owned C string. AppKit itself runs on the main thread.
    match unsafe { swarm_macos_choose_path(title.as_ptr(), start.as_ptr(), folder, &mut path) } {
        0 => Ok(None),
        1 if !path.is_null() => {
            let bytes = unsafe { CStr::from_ptr(path) }.to_bytes().to_vec();
            unsafe { libc::free(path.cast()) };
            Ok(Some(PathBuf::from(OsString::from_vec(bytes))))
        }
        _ => anyhow::bail!("no system file dialog here"),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
fn choose(_title: &str, _start: &Path, _folder: bool, _parent: Parent) -> Result<Option<PathBuf>> {
    anyhow::bail!("no system file dialog here")
}

/// The path a portal's `file://` URI names (its bytes percent-encoded).
#[cfg(target_os = "linux")]
fn path_of_uri(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let encoded = uri.strip_prefix("file://")?.as_bytes();
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut at = 0;
    while at < encoded.len() {
        if encoded[at] == b'%' {
            let hex = std::str::from_utf8(encoded.get(at + 1..at + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            bytes.push(encoded[at]);
            at += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// A folder, as the portal takes it: its bytes, then a nul.
#[cfg(target_os = "linux")]
fn folder_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// The helper's answer, as it writes it (see `helper`).
#[cfg(target_os = "linux")]
fn answer_bytes(answer: &Result<Option<PathBuf>>) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    match answer {
        Ok(Some(path)) => [b"C".as_slice(), path.as_os_str().as_bytes()].concat(),
        Ok(None) => b"N".to_vec(),
        Err(why) => format!("E{why}").into_bytes(),
    }
}

/// The helper's answer, as `answer_bytes` wrote it.
#[cfg(target_os = "linux")]
fn answer_of(bytes: &[u8]) -> Result<Option<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    match bytes.split_first() {
        Some((b'C', path)) => Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(path)))),
        Some((b'N', _)) => Ok(None),
        Some((b'E', why)) => Err(anyhow::anyhow!("{}", String::from_utf8_lossy(why))),
        _ => Err(anyhow::anyhow!("the file dialog's helper said nothing")),
    }
}

#[cfg(test)]
mod copy_tests {
    use super::*;

    /// A file chosen on the phone kept whole, by its name, in Swarm's
    /// own folder: two of one name both kept, and a name can't lead out.
    #[test]
    fn a_chosen_file_kept_whole_by_its_name() {
        let dir = std::env::temp_dir().join(format!("swarm-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = keep_copy("Holiday.jpg", &mut &b"first"[..], &dir).unwrap();
        let second = keep_copy("Holiday.jpg", &mut &b"second"[..], &dir).unwrap();
        assert_ne!(first, second);
        for (path, bytes) in [(&first, &b"first"[..]), (&second, &b"second"[..])] {
            assert!(path.starts_with(&dir), "{path:?} in {dir:?}");
            assert_eq!(path.file_name().unwrap(), "Holiday.jpg");
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        let away = keep_copy("../../away.txt", &mut &b"x"[..], &dir).unwrap();
        assert!(away.starts_with(&dir) && away.file_name().unwrap() == "away.txt", "{away:?}");
        let unnamed = keep_copy("", &mut &b"x"[..], &dir).unwrap();
        assert!(unnamed.starts_with(&dir) && !unnamed.file_name().unwrap().is_empty(), "{unnamed:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_portals_answer_is_a_path() {
        assert_eq!(path_of_uri("file:///home/a/My%20File%E2%82%AC.txt"), Some(PathBuf::from("/home/a/My File€.txt")));
        assert_eq!(path_of_uri("file:///tmp/plain"), Some(PathBuf::from("/tmp/plain")));
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(path_of_uri("file:///tmp/%FFx"), Some(PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xffx"))));
        assert_eq!(path_of_uri("https://example.com/x"), None);
        assert_eq!(path_of_uri("file:///tmp/%G1"), None, "not an escape");
    }

    /// The helper's answer comes back as it went: a path (whatever its
    /// bytes), nothing chosen, or why there's no dialog.
    #[test]
    fn the_helpers_answer_comes_back_whole() {
        use std::os::unix::ffi::OsStrExt;
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/a b\n\xffc"));
        assert_eq!(answer_of(&answer_bytes(&Ok(Some(odd.clone())))).unwrap(), Some(odd));
        assert_eq!(answer_of(&answer_bytes(&Ok(None))).unwrap(), None);
        let why = answer_of(&answer_bytes(&Err(anyhow::anyhow!("no portal")))).unwrap_err();
        assert_eq!(format!("{why}"), "no portal");
        assert!(answer_of(b"").is_err(), "a helper that said nothing");
    }

    /// Swarm's program replaced while it runs (built again, updated):
    /// it still starts its helper. Run as a copy of these tests that
    /// replaces itself, then starts itself again (just to list its tests).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_helper_started_with_swarm_replaced() {
        if std::env::var_os("SWARM_REPLACE_ME").is_some() {
            let me = std::env::current_exe().unwrap();
            std::fs::remove_file(&me).unwrap();
            std::fs::write(&me, b"not swarm").unwrap();
            let listed = std::process::Command::new(swarm_itself().unwrap()).arg("--list").output().expect("swarm started again");
            assert!(listed.status.success());
            return;
        }
        let dir = std::env::temp_dir().join(format!("swarm-replaced-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("swarm");
        std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
        let mut tries = 0;
        let out = loop {
            match std::process::Command::new(&copy)
                .args(["--exact", "filedialog::tests::the_helper_started_with_swarm_replaced", "--nocapture"])
                .env("SWARM_REPLACE_ME", "1")
                .output()
            {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => break other.unwrap(),
            }
        };
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }

    /// The window a dialog's for, as the portal has it: X11's id in hex.
    #[test]
    fn the_parent_as_the_portal_takes_it() {
        assert_eq!(Parent::X11(0x3a0_0004).portal_id(), "x11:3a00004");
        assert_eq!(Parent::None.portal_id(), "");
        assert_eq!(Parent::Win32(5).portal_id(), "");
    }

    /// A path's sent to the portal as bytes ending in a nul.
    #[test]
    fn a_folder_goes_to_the_portal_as_bytes() {
        assert_eq!(folder_bytes(Path::new("/home/a b")), b"/home/a b\0".to_vec());
    }
}

/// Opens the folder dialog for someone to close (Esc): run by hand, under
/// Wine on a private display, as Windows' dialog can't be seen otherwise.
#[cfg(all(test, windows))]
mod windows_tests {
    #[test]
    #[ignore]
    fn the_folder_dialog_opens_and_closes_unchosen() {
        let chosen = super::choose_folder("Save in which folder?", &std::env::temp_dir(), super::Parent::None);
        assert!(matches!(chosen, Ok(None)), "{chosen:?}");
    }
}
