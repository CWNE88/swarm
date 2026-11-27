//! Where things live: the data folder (see `data_dir`) and the clock.

use std::path::{Path, PathBuf};
use swarm_core::node::Paths;

/// Your home folder: `HOME`, or on Windows `USERPROFILE`
/// (`C:\Users\<you>`), so `~/.swarm` is `C:\Users\<you>\.swarm` there.
/// (Windows looks at `USERPROFILE` first: tools like Git set `HOME`
/// there, sometimes in a Unix-style form.)
pub fn dirs_home() -> Option<PathBuf> {
    let order = if cfg!(windows) { ["USERPROFILE", "HOME"] } else { ["HOME", "USERPROFILE"] };
    order.iter().find_map(|name| std::env::var_os(name).filter(|v| !v.is_empty())).map(PathBuf::from)
}

/// `path` with a leading `~` replaced by the home folder.
pub fn expand_home(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        if let Some(home) = dirs_home() {
            return rest.components().fold(home, |path, part| path.join(part));
        }
    }
    path.to_path_buf()
}

/// The data folder, everything Swarm keeps:
/// - `SWARM_HOME`, if it's set;
/// - else `~/.swarm`, if there is one (where it always used to be);
/// - else the system's place for it: `~/.config/swarm` on Linux and the
///   like (`$XDG_CONFIG_HOME/swarm` if that's set), `AppData\Local\Swarm`
///   on Windows (not the roaming AppData: a user's keys stay on the
///   machine), `~/Library/Application Support/Swarm` on macOS.
///
/// On Android, the app's own storage (`HOME` there) holds `.swarm`.
pub fn data_dir() -> PathBuf {
    let os = if cfg!(target_os = "android") {
        System::Android
    } else if cfg!(windows) {
        System::Windows
    } else if cfg!(target_os = "macos") {
        System::Mac
    } else {
        System::Unix
    };
    choose_data_dir(os, |name| std::env::var_os(name).filter(|v| !v.is_empty()), dirs_home(), |path| path.exists())
}

#[derive(Clone, Copy, PartialEq)]
enum System {
    Android,
    Windows,
    Mac,
    Unix,
}

/// `data_dir`'s choice, on `os`, given what `var` says each environment
/// variable is, the home folder, and what `exists`.
fn choose_data_dir(os: System, var: impl Fn(&str) -> Option<std::ffi::OsString>, home: Option<PathBuf>, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let in_home = |rest: &Path| home.as_ref().map(|home| rest.components().fold(home.clone(), |path, part| path.join(part)));
    if let Some(dir) = var("SWARM_HOME") {
        let dir = PathBuf::from(dir);
        return dir.strip_prefix("~").ok().and_then(in_home).unwrap_or(dir);
    }
    let old = in_home(Path::new(".swarm")).unwrap_or_else(|| PathBuf::from(".swarm"));
    if os == System::Android || exists(&old) {
        return old;
    }
    let absolute = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let system = match os {
        System::Windows => absolute("LOCALAPPDATA").map(|dir| dir.join("Swarm")),
        System::Mac => in_home(Path::new("Library/Application Support/Swarm")),
        _ => absolute("XDG_CONFIG_HOME").or_else(|| in_home(Path::new(".config"))).map(|dir| dir.join("swarm")),
    };
    system.unwrap_or(old)
}

/// Where everything lives, in the data folder.
pub fn paths() -> Paths {
    Paths::new(data_dir())
}

pub use swarm_core::unix_now;


/// Your home folder, or the top of the file system if there's none.
pub fn home() -> PathBuf {
    dirs_home().unwrap_or_else(|| PathBuf::from("/"))
}

/// Where a file offered to you is first suggested to go: where the last
/// one went, else your Downloads folder, else your home folder.
pub fn save_folder() -> PathBuf {
    swarm_core::settings::get(&paths().settings(), "downloads").map(PathBuf::from).unwrap_or_else(|| {
        let downloads = home().join("Downloads");
        if downloads.is_dir() { downloads } else { home() }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name| pairs.iter().find(|(n, _)| *n == name).map(|(_, v)| OsString::from(v))
    }

    /// A new user's folder is the system's place for it; one who already
    /// has ~/.swarm keeps it; SWARM_HOME says otherwise for anyone.
    #[test]
    fn the_data_folder_is_the_systems_unless_there_is_one_already() {
        let home = || Some(PathBuf::from("/home/ann"));
        let none = |_: &Path| false;
        assert_eq!(choose_data_dir(System::Unix, env(&[]), home(), none), PathBuf::from("/home/ann/.config/swarm"));
        assert_eq!(choose_data_dir(System::Unix, env(&[("XDG_CONFIG_HOME", "/data/cfg")]), home(), none), PathBuf::from("/data/cfg/swarm"));
        assert_eq!(choose_data_dir(System::Unix, env(&[("XDG_CONFIG_HOME", "relative")]), home(), none), PathBuf::from("/home/ann/.config/swarm"), "not a relative one");
        assert_eq!(choose_data_dir(System::Mac, env(&[]), home(), none), PathBuf::from("/home/ann/Library/Application Support/Swarm"));
        assert_eq!(choose_data_dir(System::Windows, env(&[("LOCALAPPDATA", "/c/ann/AppData/Local")]), home(), none), PathBuf::from("/c/ann/AppData/Local/Swarm"));
        // Already there: kept, on every system.
        let old = |path: &Path| path == Path::new("/home/ann/.swarm");
        for os in [System::Unix, System::Mac, System::Windows] {
            assert_eq!(choose_data_dir(os, env(&[("LOCALAPPDATA", "/c/x")]), home(), old), PathBuf::from("/home/ann/.swarm"));
        }
        assert_eq!(choose_data_dir(System::Android, env(&[]), home(), none), PathBuf::from("/home/ann/.swarm"), "the app's own storage, as ever");
        // Said: wherever that is, even with ~/.swarm there.
        assert_eq!(choose_data_dir(System::Unix, env(&[("SWARM_HOME", "/srv/swarm")]), home(), old), PathBuf::from("/srv/swarm"));
        assert_eq!(choose_data_dir(System::Unix, env(&[("SWARM_HOME", "~/elsewhere")]), home(), none), PathBuf::from("/home/ann/elsewhere"));
    }
}
