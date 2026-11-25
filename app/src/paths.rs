//! Where things live: the data folder (`~/.swarm`, or on Windows
//! `%USERPROFILE%\.swarm`) and the clock.

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

/// The data folder: `~/.swarm`.
pub fn data_dir() -> PathBuf {
    expand_home(Path::new("~/.swarm"))
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
