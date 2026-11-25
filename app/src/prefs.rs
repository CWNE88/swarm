//! The display choices people make (Settings): times in chats, local
//! time or UTC, joins and parts, the order peers are listed in, light or
//! dark, (for the window) text size, how your voice is cleaned up in
//! calls, and the camera and video size. Kept in `~/.swarm/settings`,
//! so the window and the terminal UI share them. Video always uses the
//! graphics chip where it can, and H.265 where both ends' chips have it:
//! neither is a setting (see `crate::hardware`).

use std::path::Path;
use swarm_core::settings;

use crate::audio::Voice;
use crate::time::zone_name;

/// The order peers are listed in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PeerOrder {
    /// As they were added.
    Added,
    Name,
    /// Online first, then everyone else; each group by latest message.
    Recent,
}

impl PeerOrder {
    pub fn label(self) -> &'static str {
        match self {
            PeerOrder::Added => "as added",
            PeerOrder::Name => "by name",
            PeerOrder::Recent => "online first, then latest message",
        }
    }
    fn key(self) -> &'static str {
        match self {
            PeerOrder::Added => "added",
            PeerOrder::Name => "name",
            PeerOrder::Recent => "recent",
        }
    }
    fn next(self) -> Self {
        match self {
            PeerOrder::Added => PeerOrder::Name,
            PeerOrder::Name => PeerOrder::Recent,
            PeerOrder::Recent => PeerOrder::Added,
        }
    }
}

/// Light or dark.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    /// The window follows the system's choice; the terminal UI uses the
    /// terminal's own colours.
    System,
    Light,
    Dark,
}

impl Theme {
    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
    /// 0 system, 1 light, 2 dark — for front ends that number choices.
    pub fn index(self) -> i32 {
        match self {
            Theme::System => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        }
    }
    pub fn from_index(i: i32) -> Theme {
        match i {
            1 => Theme::Light,
            2 => Theme::Dark,
            _ => Theme::System,
        }
    }
    fn next(self) -> Self {
        Theme::from_index((self.index() + 1) % 3)
    }
}

/// The window's text sizes, in percent of normal.
pub const TEXT_SIZES: [u32; 7] = [90, 100, 115, 130, 150, 175, 200];

/// The display choices kept in `~/.swarm/settings`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Prefs {
    /// A time beside each chat line.
    pub show_times: bool,
    /// Times in UTC rather than this device's local time.
    pub utc: bool,
    /// Who added whom, and who left, shown in groups (a group's name and
    /// its timer always are).
    pub show_joins: bool,
    pub peer_order: PeerOrder,
    pub theme: Theme,
    /// The window's text size, percent of normal (one of `TEXT_SIZES`).
    pub text_size: u32,
    /// How your voice is cleaned up in calls (see `crate::audio`): each
    /// part on unless turned off.
    pub voice: Voice,
    /// The width a camera's pictures are always sent at (one of
    /// `VIDEO_WIDTHS`), or `None`, the biggest the link carries.
    pub video_width: Option<usize>,
    /// Minutes left alone before the TUI or the window locks (asks for
    /// the passphrase again); 0, never — the default, for a device
    /// that's always on.
    pub lock_after: u32,
    /// A call's details shown while it's on: what it's sending and
    /// receiving, and a video call's sizes, coding and timings. Hidden
    /// unless turned on.
    pub call_info: bool,
    /// Messages coming shown in notifications (the phone's): the first
    /// from each unless another's chosen.
    pub notify: Notify,
    /// The list's sections closed under their headings: none unless
    /// closed.
    pub closed: crate::sidebar::Closed,
    /// The window's list narrowed to a strip of pictures: not unless
    /// narrowed.
    pub list_narrow: bool,
    /// The width the window's list was dragged to, in logical pixels;
    /// 0, by the window's width.
    pub list_width: u32,
    /// A channel's members, on the window's right, folded away: not
    /// unless folded.
    pub members_hidden: bool,
    /// The width a channel's members were dragged to, in logical pixels;
    /// 0, the usual.
    pub members_width: u32,
}

/// When the phone notifies of a message (who it's from, never what it
/// says): never; the first from someone with nothing of theirs unread
/// (more aren't, until it's read); or each one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    Off,
    First,
    Every,
}

impl Notify {
    pub fn from_index(i: i32) -> Notify {
        match i {
            1 => Notify::First,
            2 => Notify::Every,
            _ => Notify::Off,
        }
    }

    pub fn index(self) -> i32 {
        match self {
            Notify::Off => 0,
            Notify::First => 1,
            Notify::Every => 2,
        }
    }
}

/// The choices for `video_width`: automatic, then the camera sizes.
pub const VIDEO_WIDTHS: [Option<usize>; 5] = [None, Some(3840), Some(2560), Some(1920), Some(1280)];

/// How a `video_width` choice is shown.
pub fn video_width_label(width: Option<usize>) -> &'static str {
    match width {
        None => "automatic: as big as the link carries",
        Some(3840) => "3840×2160 (4K)",
        Some(2560) => "2560×1440",
        Some(1920) => "1920×1080",
        _ => "1280×720",
    }
}

/// The choices for `lock_after`, in minutes (0: never).
pub const LOCK_AFTER: [u32; 5] = [0, 5, 15, 30, 60];

/// One line of the Settings view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Setting {
    Name,
    /// Your status, for peers (see swarm_core's profile.rs).
    Status,
    /// Your picture, for peers (the TUI's; the window has its own).
    Picture,
    /// When a message makes a sound (the TUI's; the window has its own
    /// rows, the phone its notifications): as `Notify`.
    Sound,
    /// Which sound (see `crate::sound`).
    SoundFile,
    /// The sound a call rings with (see `crate::ringtone`).
    Ringtone,
    Times,
    Zone,
    Joins,
    Order,
    Theme,
    Echo,
    Noise,
    Level,
    /// Which microphone and speaker (kept by `crate::audio_devices`,
    /// not here).
    Microphone,
    Speaker,
    /// Which camera (kept by `crate::camera`, not here).
    Camera,
    /// The size a camera's pictures are sent at, or automatic.
    VideoSize,
    /// Locking when left alone, and after how long.
    Lock,
    /// Unblocking a key: the TUI's prompt for it, under You (not listed
    /// among the settings; see `actions::blocked`).
    Unblock,
    /// The address you set by hand: the TUI's prompt for it, under You
    /// (not listed among the settings; see swarm_core's myaddress.rs).
    ManualAddress,
    /// A call's details shown, or not.
    CallInfo,
    /// The camera's picture, to see how you look (the TUI's; the window
    /// has its own beside the camera).
    Preview,
}

/// Settings in the order shown, by section (see `Setting::section`).
/// Your name, status and picture aren't among them: they're set under
/// You, in Contacts.
pub const SETTINGS: [Setting; 18] = [
    Setting::Theme,
    Setting::Lock,
    Setting::CallInfo,
    Setting::Ringtone,
    Setting::Times,
    Setting::Zone,
    Setting::Joins,
    Setting::Order,
    Setting::Sound,
    Setting::SoundFile,
    Setting::Microphone,
    Setting::Speaker,
    Setting::Echo,
    Setting::Noise,
    Setting::Level,
    Setting::Camera,
    Setting::Preview,
    Setting::VideoSize,
];

impl Setting {
    pub fn label(self) -> &'static str {
        match self {
            Setting::Name => "Your name",
            Setting::Status => "Your status",
            Setting::Picture => "Your picture",
            Setting::Sound => "Message sound",
            Setting::SoundFile => "Sound",
            Setting::Ringtone => "Ringtone",
            Setting::Times => "Times",
            Setting::Zone => "Times shown in",
            Setting::Joins => "People added and leaving",
            Setting::Order => "Contacts listed",
            Setting::Theme => "Colours",
            Setting::Echo => "Echo",
            Setting::Noise => "Background noise",
            Setting::Level => "Your voice's level",
            Setting::Microphone => "Microphone",
            Setting::Speaker => "Speaker",
            Setting::Camera => "Camera",
            Setting::VideoSize => "Video size",
            Setting::Lock => "Lock when left",
            Setting::Unblock => "Unblock",
            Setting::ManualAddress => "Manually set IP address",
            Setting::CallInfo => "Call details",
            Setting::Preview => "Camera preview",
        }
    }

    /// The heading it's shown under.
    pub fn section(self) -> &'static str {
        match self {
            Setting::Name | Setting::Status | Setting::Picture | Setting::Theme | Setting::Lock | Setting::CallInfo | Setting::Ringtone | Setting::Unblock | Setting::ManualAddress => "General",
            Setting::Times | Setting::Zone | Setting::Joins | Setting::Order | Setting::Sound | Setting::SoundFile => "Chats",
            Setting::Microphone | Setting::Speaker | Setting::Echo | Setting::Noise | Setting::Level => "Audio",
            Setting::Camera | Setting::Preview | Setting::VideoSize => "Video",
        }
    }

    /// What it does, in a line (the TUI's, under the one selected).
    pub fn help(self) -> &'static str {
        match self {
            Setting::Name => "shown to others beside your key",
            Setting::Status => "a few words beside your name, for your contacts only",
            Setting::Picture => "what your contacts see beside your name",
            Setting::Sound => "when a message comes while the terminal isn't the one in use: the first from each until read, or every one; never in a call",
            Setting::SoundFile => "a sound file (WAV, Ogg, FLAC, MP3 or M4A; its first 10 seconds), or blank for Swarm's own",
            Setting::Ringtone => "what a call rings with, over and over until it's answered: a sound file (WAV, Ogg, FLAC, MP3 or M4A; its first 30 seconds), or blank for Swarm's own",
            Setting::Theme => "light or dark, or as the system has it; the window's too",
            Setting::Lock => "asks for your passphrase again; Swarm runs on meanwhile",
            Setting::CallInfo => "in a call: the round trip, what it sends and receives, and video's size and coding",
            Setting::Unblock => "the name or key of someone blocked: they can ask to be added again",
            Setting::ManualAddress => "where you can be reached that this device can't see for itself (your router's public address with a port forwarded, or a host name; :port if it isn't 1977; several separated by commas): first in your code, on all your devices; blank clears it",
            Setting::Times => "the time beside each line of a chat",
            Setting::Zone => "your own time, or UTC",
            Setting::Joins => "in a group's chat, who added whom, and who left",
            Setting::Order => "the order of the list on the left",
            Setting::Echo => "so the other side won't hear themselves from your speakers; not needed with headphones",
            Setting::Noise => "steady noise such as a fan, made quieter",
            Setting::Level => "a quiet or loud microphone brought to an even level",
            Setting::Microphone => "for calls, at once, even in one; the system's own follows your sound settings",
            Setting::Speaker => "for calls, at once, even in one; the system's own follows your sound settings",
            Setting::Camera => "for calls and the preview, from the next time it's turned on",
            Setting::Preview => "see how you look, before a call",
            Setting::VideoSize => "a size: sent at it from the start, sharper as the link allows",
        }
    }
}

impl Prefs {
    pub fn load(path: &Path) -> Prefs {
        let get = |key: &str| settings::get(path, key);
        Prefs {
            show_times: get("show_times").as_deref() != Some("no"),
            utc: get("times").as_deref() == Some("utc"),
            show_joins: get("show_joins").as_deref() != Some("no"),
            peer_order: match get("peer_order").as_deref() {
                Some("name") => PeerOrder::Name,
                Some("recent") => PeerOrder::Recent,
                _ => PeerOrder::Added,
            },
            theme: match get("theme").as_deref() {
                Some("light") => Theme::Light,
                Some("dark") => Theme::Dark,
                _ => Theme::System,
            },
            text_size: get("text_size").and_then(|v| v.parse().ok()).filter(|v| TEXT_SIZES.contains(v)).unwrap_or(100),
            voice: voice(path),
            video_width: video_width(path),
            lock_after: get("lock_after").and_then(|v| v.parse().ok()).filter(|v| LOCK_AFTER.contains(v)).unwrap_or(0),
            call_info: get("call_info").as_deref() == Some("yes"),
            notify: match get("notify").as_deref() {
                Some("off") => Notify::Off,
                Some("every") => Notify::Every,
                _ => Notify::First,
            },
            closed: crate::sidebar::Closed::from_text(&get("closed_sections").unwrap_or_default()),
            list_narrow: get("list_narrow").as_deref() == Some("yes"),
            list_width: get("list_width").and_then(|v| v.parse().ok()).unwrap_or(0),
            members_hidden: get("members_hidden").as_deref() == Some("yes"),
            members_width: get("members_width").and_then(|v| v.parse().ok()).unwrap_or(0),
        }
    }

    /// What `setting` is set to, as shown in Settings (not the name,
    /// which isn't kept here).
    pub fn value(&self, setting: Setting) -> String {
        match setting {
            Setting::Name | Setting::Status | Setting::Picture | Setting::Unblock | Setting::ManualAddress => String::new(),
            Setting::Sound => ["off", "the first message from each", "every message"][self.notify.index() as usize].to_string(),
            Setting::SoundFile => Some(crate::sound::chosen_name(&crate::paths().settings())).filter(|n| !n.is_empty()).unwrap_or_else(|| "Swarm's own".to_string()),
            Setting::Ringtone => Some(crate::ringtone::chosen_name(&crate::paths().settings())).filter(|n| !n.is_empty()).unwrap_or_else(|| "Swarm's own".to_string()),
            Setting::Preview => "Enter to see it".to_string(),
            Setting::CallInfo => if self.call_info { "shown" } else { "hidden" }.to_string(),
            Setting::Times => if self.show_times { "shown" } else { "hidden" }.to_string(),
            Setting::Zone if self.utc => "UTC".to_string(),
            Setting::Zone => format!("local time ({})", zone_name()),
            Setting::Joins => if self.show_joins { "shown" } else { "hidden" }.to_string(),
            Setting::Order => self.peer_order.label().to_string(),
            Setting::Theme => self.theme.label().to_string(),
            Setting::Echo => if self.voice.cancel_echo { "cancelled" } else { "not cancelled (for headphones)" }.to_string(),
            Setting::Noise => if self.voice.suppress_noise { "turned down" } else { "left as it is" }.to_string(),
            Setting::Level => if self.voice.steady_level { "kept even" } else { "as the microphone gives it" }.to_string(),
            Setting::Microphone => crate::audio_devices::choice_label(crate::audio_devices::Way::Microphone),
            Setting::Speaker => crate::audio_devices::choice_label(crate::audio_devices::Way::Speaker),
            Setting::Camera => crate::camera::choice_label(),
            Setting::VideoSize => video_width_label(self.video_width).to_string(),
            Setting::Lock if self.lock_after == 0 => "never".to_string(),
            Setting::Lock => format!("after {} minutes alone", self.lock_after),
        }
    }

    /// Moves `setting` on to its next choice and saves it.
    pub fn cycle(&mut self, path: &Path, setting: Setting) -> std::io::Result<()> {
        let yes_no = |on: bool| if on { "yes" } else { "no" };
        match setting {
            Setting::Name | Setting::Status | Setting::Picture | Setting::SoundFile | Setting::Ringtone | Setting::Unblock | Setting::ManualAddress | Setting::Preview => Ok(()),
            Setting::Sound => self.set_notify(path, Notify::from_index((self.notify.index() + 1) % 3)),
            Setting::CallInfo => self.set_call_info(path, !self.call_info),
            Setting::Times => {
                self.show_times = !self.show_times;
                settings::set(path, "show_times", yes_no(self.show_times))
            }
            Setting::Zone => {
                self.utc = !self.utc;
                settings::set(path, "times", if self.utc { "utc" } else { "local" })
            }
            Setting::Joins => {
                self.show_joins = !self.show_joins;
                settings::set(path, "show_joins", yes_no(self.show_joins))
            }
            Setting::Order => {
                self.peer_order = self.peer_order.next();
                settings::set(path, "peer_order", self.peer_order.key())
            }
            Setting::Theme => self.set_theme(path, self.theme.next()),
            Setting::Echo | Setting::Noise | Setting::Level => self.set_voice(path, setting, !self.voice_part(setting)),
            Setting::VideoSize => {
                let at = VIDEO_WIDTHS.iter().position(|w| *w == self.video_width).unwrap_or(0);
                self.set_video_width(path, VIDEO_WIDTHS[(at + 1) % VIDEO_WIDTHS.len()])
            }
            Setting::Microphone => crate::audio_devices::choose_next(path, crate::audio_devices::Way::Microphone),
            Setting::Speaker => crate::audio_devices::choose_next(path, crate::audio_devices::Way::Speaker),
            Setting::Camera => crate::camera::choose_next(path),
            Setting::Lock => {
                let at = LOCK_AFTER.iter().position(|m| *m == self.lock_after).unwrap_or(0);
                self.set_lock_after(path, LOCK_AFTER[(at + 1) % LOCK_AFTER.len()])
            }
        }
    }

    /// Whether `part` (`Echo`, `Noise` or `Level`) of cleaning up your
    /// voice is on.
    pub fn voice_part(&self, part: Setting) -> bool {
        match part {
            Setting::Echo => self.voice.cancel_echo,
            Setting::Noise => self.voice.suppress_noise,
            Setting::Level => self.voice.steady_level,
            _ => false,
        }
    }

    /// Turns `part` (`Echo`, `Noise` or `Level`) of cleaning up your
    /// voice on or off — in a call already going, too.
    pub fn set_voice(&mut self, path: &Path, part: Setting, on: bool) -> std::io::Result<()> {
        let key = match part {
            Setting::Echo => {
                self.voice.cancel_echo = on;
                "cancel_echo"
            }
            Setting::Noise => {
                self.voice.suppress_noise = on;
                "suppress_noise"
            }
            Setting::Level => {
                self.voice.steady_level = on;
                "steady_level"
            }
            _ => return Ok(()),
        };
        crate::audio::set_voice(self.voice);
        settings::set(path, key, if on { "yes" } else { "no" })
    }

    /// The size a camera's pictures are always sent at (`None`: the
    /// biggest the link carries) — from a call's next picture, and the
    /// camera's next opening.
    pub fn set_video_width(&mut self, path: &Path, width: Option<usize>) -> std::io::Result<()> {
        self.video_width = width;
        swarm_core::video::set_video_width(width);
        settings::set(path, "video_size", &width.map_or("auto".to_string(), |w| w.to_string()))
    }

    /// Minutes left alone before locking (one of `LOCK_AFTER`; 0 never).
    pub fn set_lock_after(&mut self, path: &Path, minutes: u32) -> std::io::Result<()> {
        self.lock_after = if LOCK_AFTER.contains(&minutes) { minutes } else { 0 };
        settings::set(path, "lock_after", &self.lock_after.to_string())
    }

    /// The list's sections closed now.
    pub fn set_closed(&mut self, path: &Path, closed: crate::sidebar::Closed) -> std::io::Result<()> {
        self.closed = closed;
        settings::set(path, "closed_sections", &closed.to_text())
    }

    /// A call's details shown while it's on, or not.
    pub fn set_call_info(&mut self, path: &Path, on: bool) -> std::io::Result<()> {
        self.call_info = on;
        settings::set(path, "call_info", if on { "yes" } else { "no" })
    }

    pub fn set_notify(&mut self, path: &Path, notify: Notify) -> std::io::Result<()> {
        self.notify = notify;
        settings::set(path, "notify", ["off", "first", "every"][notify.index() as usize])
    }

    pub fn set_theme(&mut self, path: &Path, theme: Theme) -> std::io::Result<()> {
        self.theme = theme;
        settings::set(path, "theme", theme.label())
    }

    /// The window's list narrowed to pictures, or not.
    pub fn set_list_narrow(&mut self, path: &Path, narrow: bool) -> std::io::Result<()> {
        self.list_narrow = narrow;
        settings::set(path, "list_narrow", if narrow { "yes" } else { "no" })
    }

    /// The width the window's list was dragged to.
    pub fn set_list_width(&mut self, path: &Path, width: u32) -> std::io::Result<()> {
        self.list_width = width;
        settings::set(path, "list_width", &width.to_string())
    }

    /// A channel's members folded away in the window, or not.
    pub fn set_members_hidden(&mut self, path: &Path, hidden: bool) -> std::io::Result<()> {
        self.members_hidden = hidden;
        settings::set(path, "members_hidden", if hidden { "yes" } else { "no" })
    }

    /// The width a channel's members were dragged to.
    pub fn set_members_width(&mut self, path: &Path, width: u32) -> std::io::Result<()> {
        self.members_width = width;
        settings::set(path, "members_width", &width.to_string())
    }

    /// One size larger (1), smaller (-1), or back to normal (0).
    pub fn step_text_size(&mut self, path: &Path, step: i32) -> std::io::Result<()> {
        let at = TEXT_SIZES.iter().position(|s| *s == self.text_size).unwrap_or(1) as i32;
        let next = if step == 0 { 1 } else { (at + step).clamp(0, TEXT_SIZES.len() as i32 - 1) };
        self.text_size = TEXT_SIZES[next as usize];
        settings::set(path, "text_size", &self.text_size.to_string())
    }
}

/// How your voice is to be cleaned up in calls, as kept in `path`.
pub fn voice(path: &Path) -> Voice {
    let on = |key: &str| settings::get(path, key).as_deref() != Some("no");
    Voice { cancel_echo: on("cancel_echo"), suppress_noise: on("suppress_noise"), steady_level: on("steady_level") }
}

/// The size a camera's pictures are always sent at, as kept in `path`
/// (`None`: automatic).
pub fn video_width(path: &Path) -> Option<usize> {
    settings::get(path, "video_size").and_then(|v| v.parse().ok()).filter(|w| VIDEO_WIDTHS.contains(&Some(*w)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings come in sections, each in one piece and in order, every
    /// line with a word of help.
    #[test]
    fn settings_come_in_sections() {
        let mut seen: Vec<&str> = Vec::new();
        for setting in SETTINGS {
            if seen.last() != Some(&setting.section()) {
                assert!(!seen.contains(&setting.section()), "{:?}: {} is split", setting, setting.section());
                seen.push(setting.section());
            }
            assert!(!setting.help().is_empty(), "{setting:?} has no help");
        }
        assert_eq!(seen, ["General", "Chats", "Audio", "Video"]);
        assert!(SETTINGS.contains(&Setting::CallInfo) && SETTINGS.contains(&Setting::Preview));
    }

    /// Notifications of messages are of the first from each unless
    /// another's chosen, and whichever's chosen is kept.
    #[test]
    fn notifications_are_kept() {
        let dir = std::env::temp_dir().join(format!("swarm-prefs-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        swarm_core::store::open_vault(&dir, &swarm_core::store::Keys { seal: [7; 32].into(), names: [8; 32].into() });
        let path = dir.join("settings");
        let mut prefs = Prefs::load(&path);
        assert_eq!(prefs.notify, Notify::First);
        for notify in [Notify::Off, Notify::Every, Notify::First, Notify::Off] {
            prefs.set_notify(&path, notify).unwrap();
            assert_eq!(Prefs::load(&path).notify, notify);
        }
        assert_eq!(Notify::from_index(1), Notify::First);
        assert_eq!(Notify::Every.index(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The window's list: full width, by the window's, until narrowed or
    /// dragged, and either kept.
    #[test]
    fn the_lists_width_is_kept() {
        let dir = std::env::temp_dir().join(format!("swarm-prefs-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        swarm_core::store::open_vault(&dir, &swarm_core::store::Keys { seal: [9; 32].into(), names: [10; 32].into() });
        let path = dir.join("settings");
        let mut prefs = Prefs::load(&path);
        assert_eq!((prefs.list_narrow, prefs.list_width), (false, 0));
        prefs.set_list_width(&path, 320).unwrap();
        prefs.set_list_narrow(&path, true).unwrap();
        assert_eq!((Prefs::load(&path).list_narrow, Prefs::load(&path).list_width), (true, 320));
        prefs.set_list_narrow(&path, false).unwrap();
        assert!(!Prefs::load(&path).list_narrow);
        assert_eq!((prefs.members_hidden, prefs.members_width), (false, 0));
        prefs.set_members_width(&path, 260).unwrap();
        prefs.set_members_hidden(&path, true).unwrap();
        assert_eq!((Prefs::load(&path).members_hidden, Prefs::load(&path).members_width), (true, 260));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A call's details are hidden unless asked for, and kept.
    #[test]
    fn call_details_are_kept() {
        let dir = std::env::temp_dir().join(format!("swarm-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        swarm_core::store::open_vault(&dir, &swarm_core::store::Keys { seal: [5; 32].into(), names: [6; 32].into() });
        let path = dir.join("settings");
        let mut prefs = Prefs::load(&path);
        assert!(!prefs.call_info);
        assert_eq!(prefs.value(Setting::CallInfo), "hidden");
        prefs.cycle(&path, Setting::CallInfo).unwrap();
        assert!(Prefs::load(&path).call_info);
        assert_eq!(Prefs::load(&path).value(Setting::CallInfo), "shown");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
