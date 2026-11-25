//! The shared app layer: everything a swarm app needs between the network
//! (`swarm_core`) and the screen. Front ends — the terminal UI in `cli/`,
//! the desktop and phone apps — draw what this works out, and pass on
//! what the person does; none of them decides what to show on its own,
//! so they all show the same thing.

/// When this build was made (local time, to the minute; see
/// `build.rs`): the top of the Help tab, and `swarm --version`.
pub const BUILT: &str = env!("SWARM_BUILT");

/// Swarm's version, one whole number rising with each release, and when
/// this build was made: "1 (built YYYY-MM-DD HH:MM)".
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION_MAJOR"), " (built ", env!("SWARM_BUILT"), ")");

pub mod actions;
pub mod addrs;
pub mod audio;
pub mod autoanswer;
pub mod avatar;
pub mod calls;
pub mod camera;
pub mod chat;
pub mod colours;
pub mod contact;
pub mod commands;
pub mod control;
pub mod devices;
pub mod discovery;
pub mod filedialog;
pub mod hardware;
pub mod ipc;
pub mod jobs;
pub mod launch;
pub mod links;
pub mod manual;
pub mod meter;
pub mod audio_devices;
pub mod names;
pub mod notes;
pub mod notify;
pub mod paths;
pub mod peerbook;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
pub mod portal_helper;
pub mod prefs;
pub mod progress;
pub mod prompts;
pub mod remote;
#[cfg(target_os = "linux")]
pub mod remote_wayland;
#[cfg(target_os = "linux")]
pub mod remote_x11;
#[cfg(windows)]
pub mod remote_win;
pub mod ringtone;
pub mod screen;
pub mod sound;
pub mod sidebar;
pub mod text;
pub mod talking;
pub mod time;

pub use swarm_media::{h264, h265, jpeg};
#[cfg(target_os = "linux")]
pub use swarm_media::{nv_sys, va_sys, video_nv, video_va};

pub use paths::{data_dir, dirs_home, expand_home, paths, unix_now};
