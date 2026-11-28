//! The window's keys, in one table: for what's showing, each key, what
//! it does (a `KeyCommand`, which app.slint's `run` carries out) and
//! the hint along the bottom that says so. A key pressed and a hint
//! clicked go through the same row, so the hints can't say one thing
//! while the keys do another; a key that works without a hint (one kept
//! from the terminal UI, or a second key for the same thing) is a row
//! of its own with no hint, so every key is here.
//!
//! What's showing is read from the window itself (`Showing::read`), on
//! its own thread: the hints are made again whenever it changes (after
//! each snapshot, and when the window changes a mode of its own), and a
//! key looks up its row as it's pressed.

use crate::{App, Hint, KeyCommand, Pane};
use slint::platform::Key;
use slint::SharedString;

/// What decides the keys: what the window shows.
#[derive(Clone, Debug, Default)]
pub struct Showing {
    pub view: Pane,
    pub peer_tab: i32,
    pub ring_shown: bool,
    pub ring_video: bool,
    /// Someone's asking to control our screen.
    pub control_asking: bool,
    pub offer_shown: bool,
    pub confirm_shown: bool,
    /// The question is deleting a contact: Delete, Block or Cancel.
    pub confirm_delete: bool,
    /// The call and nothing else (the window's own).
    pub video_only: bool,
    pub video_shown: bool,
    pub preview_shown: bool,
    /// A peer's details, in Peers.
    pub pd_shown: bool,
    /// One of your devices, in Peers: whether it's this one, and whether
    /// there are others for it to relay for.
    pub dd_shown: bool,
    pub dd_this: bool,
    pub dd_linked: bool,
    pub dd_relay: bool,
    pub pd_request: bool,
    pub call_live: bool,
    pub calling_out: bool,
    pub muted: bool,
    pub sharing: bool,
    /// The chat open is with someone who can be added to the call.
    pub can_add: bool,
    /// The one chosen can be called now, voice (in a call: added to
    /// it) and video; their Call keys are greyed while not.
    pub can_call: bool,
    pub can_video: bool,
    pub list_narrow: bool,
}

impl Showing {
    pub fn read(app: &App) -> Showing {
        Showing {
            view: app.get_view(),
            peer_tab: app.get_peer().tab,
            ring_shown: app.get_ask().ring_shown,
            ring_video: app.get_ask().ring_video,
            control_asking: !app.get_call().control_asking.is_empty(),
            offer_shown: app.get_ask().offer_shown,
            confirm_shown: app.get_ask().confirm_shown,
            confirm_delete: app.get_ask().confirm_delete,
            video_only: app.get_video_only(),
            video_shown: app.get_call().video_shown,
            preview_shown: app.get_preview_shown(),
            pd_shown: app.get_pd().shown,
            pd_request: app.get_pd().request,
            dd_shown: app.get_dd().shown,
            dd_this: app.get_dd().this,
            dd_linked: app.get_dd().linked,
            dd_relay: app.get_dd().relay,
            call_live: app.get_call().live,
            calling_out: app.get_call().calling_out,
            muted: app.get_call().muted,
            sharing: app.get_call().sharing,
            can_add: !app.get_call().add_to_call.is_empty(),
            can_call: app.get_call().call_why.is_empty(),
            can_video: app.get_call().video_why.is_empty(),
            list_narrow: app.get_list_narrow(),
        }
    }
}

/// One key: the text pressed, how its hint shows it ("" for no hint),
/// what the hint says, and what it does.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub press: SharedString,
    pub shown: &'static str,
    pub what: String,
    pub command: KeyCommand,
    /// Hinted greyed: it can't be done now.
    pub dim: bool,
}

/// The keys for what's showing: the rows, first match first, and what
/// any other key does.
#[derive(Clone, Debug)]
pub struct Keys {
    pub rows: Vec<Row>,
    pub rest: KeyCommand,
}

impl Keys {
    /// What `text` pressed does.
    pub fn command(&self, text: &str) -> KeyCommand {
        self.rows.iter().find(|r| !r.press.is_empty() && r.press == text).map_or(self.rest, |r| r.command)
    }

    /// The hints along the bottom, in order.
    pub fn hints(&self) -> Vec<Hint> {
        self.rows.iter().filter(|r| !r.shown.is_empty()).map(|r| Hint { key: r.shown.into(), what: r.what.as_str().into(), press: r.press.clone(), dim: r.dim }).collect()
    }
}

/// Builds the rows, in order.
#[derive(Default)]
struct Table(Vec<Row>);

impl Table {
    /// A key with its hint, the key shown as typed.
    fn key(&mut self, key: &'static str, what: impl Into<String>, command: KeyCommand) -> &mut Self {
        self.named(key, key, what, command)
    }
    /// A key with its hint, shown as `shown` (Esc, Enter...).
    fn named(&mut self, shown: &'static str, press: impl Into<SharedString>, what: impl Into<String>, command: KeyCommand) -> &mut Self {
        self.0.push(Row { press: press.into(), shown, what: what.into(), command, dim: false });
        self
    }
    /// A key that works with no hint of its own.
    fn quiet(&mut self, press: impl Into<SharedString>, command: KeyCommand) -> &mut Self {
        self.0.push(Row { press: press.into(), shown: "", what: String::new(), command, dim: false });
        self
    }
    /// A hint for keys given rows of their own (Up/Down).
    fn says(&mut self, shown: &'static str, what: &str) -> &mut Self {
        self.0.push(Row { press: SharedString::new(), shown, what: what.to_string(), command: KeyCommand::None, dim: false });
        self
    }
    /// A key with a hint only when `hinted`.
    fn maybe(&mut self, hinted: bool, key: &'static str, what: impl Into<String>, command: KeyCommand) -> &mut Self {
        if hinted { self.key(key, what, command) } else { self.quiet(key, command) }
    }
    /// A key that does `command` while `can`; otherwise hinted greyed,
    /// and taken without doing anything (a Call to someone who can't be
    /// called).
    fn unless_greyed(&mut self, can: bool, key: &'static str, what: impl Into<String>, command: KeyCommand) -> &mut Self {
        if can {
            return self.key(key, what, command);
        }
        self.0.push(Row { press: key.into(), shown: key, what: what.into(), command: KeyCommand::Swallow, dim: true });
        self
    }
    fn done(&mut self, rest: KeyCommand) -> Keys {
        Keys { rows: std::mem::take(&mut self.0), rest }
    }
}

fn esc() -> SharedString {
    Key::Escape.into()
}
fn enter() -> SharedString {
    Key::Return.into()
}

/// The keys for what's showing. Whatever's asking for an answer takes
/// every key; then the video-only view; then the view's own keys, the
/// call's, and those that work everywhere.
pub fn keys(s: &Showing) -> Keys {
    use KeyCommand as C;
    let mut t = Table::default();
    if s.ring_shown {
        t.says("Left/Right", "choose").quiet(Key::LeftArrow, C::RingLeft).quiet(Key::RightArrow, C::RingRight);
        t.named("Enter", enter(), "that", C::RingChosen);
        t.maybe(s.ring_video, "a", "video", C::AnswerVideo).key("s", "audio", C::AnswerVoice).key("r", "reject", C::RejectCall).quiet(esc(), C::RejectCall);
        return t.done(C::Swallow);
    }
    if s.control_asking {
        t.key("a", "allow control", C::AllowControl).key("r", "refuse", C::RefuseControl).quiet(esc(), C::RefuseControl);
        return t.done(C::Swallow);
    }
    if s.offer_shown {
        t.named("Esc", esc(), "later", C::OfferLater).quiet("l", C::OfferLater).key("a", "accept", C::AcceptOffer).key("d", "decline", C::DeclineOffer);
        return t.done(C::Swallow);
    }
    if s.confirm_shown && s.confirm_delete {
        t.key("d", "delete", C::Yes).key("b", "block", C::Block).named("Esc", esc(), "cancel", C::No).quiet("n", C::No);
        return t.done(C::Swallow);
    }
    if s.confirm_shown {
        t.key("y", "yes", C::Yes).key("n", "no", C::No).quiet(esc(), C::No);
        return t.done(C::Swallow);
    }
    if s.video_only && s.video_shown {
        t.named("Esc", esc(), "leave full screen", C::VideoBack).quiet("v", C::LeaveVideoOnly).quiet(Key::F11, C::LeaveVideoOnly);
        t.key("s", "preview", C::HideSelf).key("m", "microphone", C::Mute).key("w", "camera", C::Camera);
        t.key("e", "share screen", C::ShareScreen).key("h", "end call", C::HangUp);
        return t.done(C::Swallow);
    }

    let esc_row = |t: &mut Table, hinted: bool| {
        let command = if s.preview_shown { C::ClosePreview } else { C::Back };
        if hinted {
            t.named("Esc", esc(), if s.preview_shown { "close the preview" } else { "back" }, command);
        } else {
            t.quiet(esc(), command);
        }
    };
    let listing = matches!(s.view, Pane::Peers | Pane::Channels | Pane::Transfers | Pane::Settings | Pane::Help);
    match s.view {
        _ if s.preview_shown => esc_row(&mut t, true),
        Pane::Peers => {
            esc_row(&mut t, true);
            t.key("a", if s.pd_request { "add them" } else { "add" }, C::AddForm).key("f", "follow", C::FollowForm);
            if s.pd_shown {
                t.key("u", "edit", C::EditForm).quiet("c", C::EditForm).key("d", "delete", C::RemovePeer).key("o", "auto-answer", C::CycleAnswer);
            }
            if s.dd_shown {
                if s.dd_linked {
                    t.key("l", if s.dd_relay { "stop relaying" } else { "relay" }, C::ToggleRelay);
                }
                if !s.dd_this {
                    if !s.call_live && !s.calling_out {
                        t.unless_greyed(s.can_call, "c", "call", C::Call).unless_greyed(s.can_video, "v", "video call", C::VideoCall);
                    }
                    t.key("o", "auto-answer", C::CycleAnswer).key("d", "remove", C::RemoveDevice);
                }
            }
        }
        Pane::Channels => {
            esc_row(&mut t, true);
            t.key("g", "add people", C::AddPeople).key("a", "new group", C::NewGroup);
        }
        Pane::Transfers => {
            esc_row(&mut t, true);
            t.says("Up/Down", "select").quiet(Key::UpArrow, C::TransferUp).quiet(Key::DownArrow, C::TransferDown);
            t.key("a", "accept", C::AcceptTransfer).key("d", "decline", C::DeclineTransfer).key("x", "cancel", C::CancelTransfer);
        }
        Pane::Help => {
            esc_row(&mut t, true);
            t.says("Up/Down", "section").quiet(Key::UpArrow, C::HelpUp).quiet(Key::DownArrow, C::HelpDown);
            t.says("PgUp/PgDn", "scroll");
        }
        Pane::Settings => {
            esc_row(&mut t, true);
            if !s.call_live && !s.video_shown {
                t.key("w", "camera preview", C::Preview);
            }
        }
        Pane::Peer if s.peer_tab == 1 => {
            t.named("Tab", Key::Tab, "chat", C::SwitchTab).quiet(Key::Backtab, C::SwitchTab).key("r", "refresh", C::Refresh);
        }
        Pane::Peer => {
            t.named("Enter", enter(), "write", C::Write).named("Tab", Key::Tab, "posts", C::SwitchTab).quiet(Key::Backtab, C::SwitchTab);
            t.key("d", "delete after…", C::NextTimer).key("g", "start a group", C::AddPeople);
        }
        Pane::Channel => {
            t.named("Enter", enter(), "write", C::Write).key("c", "call", C::Call);
            if !s.call_live && !s.calling_out {
                t.key("v", "video call", C::VideoCall);
            }
            t.key("g", "add people", C::AddPeople).key("r", "catch up", C::Refresh);
        }
        Pane::Request => {
            t.key("a", "add", C::AddRequest).quiet(enter(), C::AddRequest).key("x", "dismiss", C::DismissRequest).key("b", "block", C::BlockRequest);
        }
        Pane::Followed => {
            t.key("r", "refresh", C::Refresh).key("x", "stop following", C::Unfollow);
        }
        _ => {
            t.key("n", "new post", C::NewPost);
            if s.view == Pane::Mine {
                t.quiet(enter(), C::PostBox);
            }
        }
    }
    if !listing && !s.preview_shown {
        esc_row(&mut t, false);
    }

    let hinted = !listing;
    if s.video_shown {
        t.maybe(hinted, "v", "full screen", C::VideoOnly).quiet(Key::F11, C::VideoOnly);
    }
    if s.call_live {
        t.maybe(hinted, "h", "end call", C::HangUp).maybe(hinted, "m", "microphone", C::Mute);
        if s.view == Pane::Peer {
            if s.can_add {
                if hinted { t.unless_greyed(s.can_call, "c", "add to the call", C::Call) } else { t.quiet("c", if s.can_call { C::Call } else { C::Swallow }) };
            } else {
                t.quiet("c", C::Call);
            }
        }
        t.maybe(hinted, "w", "camera", C::Camera);
        t.maybe(hinted, "e", if s.sharing { "stop sharing" } else { "share screen" }, C::ShareScreen);
    } else if s.calling_out {
        t.maybe(hinted, "h", "cancel the call", C::HangUp);
    } else if s.view == Pane::Peer {
        t.unless_greyed(s.can_call, "c", "call", C::Call).unless_greyed(s.can_video, "v", "video call", C::VideoCall).key("f", "file", C::SendFile);
    }
    if matches!(s.view, Pane::Peer | Pane::Channel) {
        t.key("x", if s.list_narrow { "widen list" } else { "narrow list" }, C::Narrow);
    }

    t.quiet(Key::UpArrow, C::Up).quiet(Key::DownArrow, C::Down).quiet(enter(), C::Swallow);
    t.quiet(Key::Tab, C::None).quiet(Key::Backtab, C::None);
    t.quiet(Key::PageUp, C::PageUp).quiet(Key::PageDown, C::PageDown);
    t.quiet("n", C::NewPost).quiet("r", C::Refresh).quiet("f", C::SendFile);
    if !s.call_live && !s.video_shown {
        t.quiet("w", C::Preview);
    }
    t.quiet("m", C::Swallow).quiet("h", C::HangUp);
    t.maybe(!listing, "1", "contacts", C::Peers).maybe(!listing, "2", "chats", C::Chats);
    t.maybe(!listing, "3", "transfers", C::Transfers).maybe(!listing, "4", "settings", C::Settings);
    t.quiet("p", C::Peers).quiet("t", C::Transfers).quiet("s", C::Settings);
    t.quiet("i", C::Settings);
    t.maybe(!listing, "5", "help", C::Help).quiet("?", C::Help).quiet(Key::F1, C::Help);
    t.key("q", "quit", C::Quit);
    t.done(C::None)
}

/// What `text` pressed does now.
pub fn command_for(app: &App, text: &str) -> KeyCommand {
    keys(&Showing::read(app)).command(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyCommand as C;

    fn peer() -> Showing {
        Showing { view: Pane::Peer, can_call: true, can_video: true, ..Default::default() }
    }

    #[test]
    fn every_hint_that_can_be_clicked_does_what_it_says() {
        let mut cases = vec![peer(), Showing { view: Pane::Mine, ..Default::default() }];
        for view in [Pane::Peers, Pane::Channels, Pane::Transfers, Pane::Settings, Pane::Help, Pane::Channel, Pane::Request, Pane::Followed] {
            cases.push(Showing { view, pd_shown: true, ..Default::default() });
        }
        cases.push(Showing { call_live: true, video_shown: true, sharing: true, can_add: true, ..peer() });
        cases.push(Showing { calling_out: true, ..peer() });
        cases.push(Showing { video_only: true, video_shown: true, call_live: true, ..peer() });
        for asking in 0..4 {
            let mut s = peer();
            *[&mut s.ring_shown, &mut s.control_asking, &mut s.offer_shown, &mut s.confirm_shown][asking] = true;
            cases.push(s);
        }
        cases.push(Showing { preview_shown: true, view: Pane::Settings, ..Default::default() });
        cases.push(Showing { view: Pane::Peers, dd_shown: true, dd_linked: true, ..Default::default() });
        cases.push(Showing { view: Pane::Peers, dd_shown: true, dd_this: true, ..Default::default() });
        for s in cases {
            let keys = keys(&s);
            let hints = keys.hints();
            assert!(!hints.is_empty(), "{s:?}");
            for hint in hints.iter().filter(|h| !h.press.is_empty()) {
                let row = keys.rows.iter().find(|r| r.press == hint.press).unwrap();
                assert_eq!(row.shown, hint.key.as_str(), "{s:?}: {} is shown as one key but works as another", hint.key);
                assert_ne!(row.command, C::None, "{s:?}: {} hinted, doing nothing", hint.key);
            }
            let mut pressed: Vec<_> = hints.iter().filter(|h| !h.press.is_empty()).map(|h| h.press.clone()).collect();
            pressed.sort();
            let before = pressed.len();
            pressed.dedup();
            assert_eq!(before, pressed.len(), "{s:?}: a key hinted twice");
        }
    }

    #[test]
    fn what_the_hints_used_to_leave_out_works_and_is_here() {
        let offer = keys(&Showing { offer_shown: true, ..peer() });
        assert_eq!(offer.command("l"), C::OfferLater);
        assert_eq!(offer.command(&esc()), C::OfferLater);
        assert_eq!(offer.command("q"), C::Swallow);
        let peers = keys(&Showing { view: Pane::Peers, pd_shown: true, ..Default::default() });
        assert_eq!(peers.command("c"), C::EditForm);
        assert_eq!(peers.command("u"), C::EditForm);
        assert_eq!(keys(&Showing { view: Pane::Peers, ..Default::default() }).command("u"), C::None);
        assert!(keys(&peer()).hints().iter().any(|h| h.key == "3" && h.what == "transfers"));
    }

    #[test]
    fn the_tabs_are_chosen_by_number_from_anywhere() {
        for s in [peer(), Showing { view: Pane::Peers, ..Default::default() }, Showing { view: Pane::Settings, ..Default::default() }] {
            let keys = keys(&s);
            assert_eq!(keys.command("1"), C::Peers, "{s:?}");
            assert_eq!(keys.command("2"), C::Chats, "{s:?}");
            assert_eq!(keys.command("3"), C::Transfers, "{s:?}");
            assert_eq!(keys.command("4"), C::Settings, "{s:?}");
            assert_eq!(keys.command("5"), C::Help, "{s:?}");
            assert_eq!(keys.command("p"), C::Peers, "{s:?}");
            assert_eq!(keys.command("t"), C::Transfers, "{s:?}");
        }
        let hinted: Vec<String> = keys(&peer()).hints().iter().filter(|h| ["1", "2", "3", "4", "5"].contains(&h.key.as_str())).map(|h| h.what.to_string()).collect();
        assert_eq!(hinted, ["contacts", "chats", "transfers", "settings", "help"]);
    }

    #[test]
    fn help_opens_from_anywhere_and_its_arrows_choose_a_section() {
        for s in [peer(), Showing { view: Pane::Settings, ..Default::default() }, Showing { view: Pane::Mine, ..Default::default() }] {
            assert_eq!(keys(&s).command("?"), C::Help, "{s:?}");
            assert_eq!(keys(&s).command(&SharedString::from(Key::F1)), C::Help, "{s:?}");
        }
        let help = keys(&Showing { view: Pane::Help, ..Default::default() });
        assert_eq!(help.command(&SharedString::from(Key::UpArrow)), C::HelpUp);
        assert_eq!(help.command(&SharedString::from(Key::DownArrow)), C::HelpDown);
        assert_eq!(help.command(&SharedString::from(Key::PageDown)), C::PageDown);
        assert_eq!(help.command(&esc()), C::Back);
        assert!(keys(&peer()).hints().iter().any(|h| h.key == "5" && h.what == "help"));
    }

    #[test]
    fn a_call_takes_its_keys_and_v_goes_to_the_call_once_there_is_video() {
        let calling = keys(&Showing { call_live: true, can_add: true, ..peer() });
        assert_eq!(calling.command("c"), C::Call);
        assert_eq!(calling.command("h"), C::HangUp);
        assert_eq!(calling.command("w"), C::Camera);
        let video = keys(&Showing { call_live: true, video_shown: true, ..peer() });
        assert_eq!(video.command("v"), C::VideoOnly);
        assert_eq!(keys(&peer()).command("v"), C::VideoCall);
        assert_eq!(keys(&Showing { view: Pane::Channel, ..Default::default() }).command("v"), C::VideoCall, "a group's video call");
        assert_eq!(keys(&Showing { view: Pane::Channel, call_live: true, video_shown: true, ..Default::default() }).command("v"), C::VideoOnly, "in one, full screen");
        assert_eq!(keys(&Showing { view: Pane::Channels, ..Default::default() }).command("w"), C::Preview);
    }

    #[test]
    fn one_of_your_devices_is_removed_and_let_relay_from_its_keys() {
        let other = keys(&Showing { view: Pane::Peers, dd_shown: true, dd_linked: true, ..Default::default() });
        assert_eq!(other.command("d"), C::RemoveDevice);
        assert_eq!(other.command("l"), C::ToggleRelay);
        assert_eq!(other.command("c"), C::Swallow);
        let up = keys(&Showing { view: Pane::Peers, dd_shown: true, dd_linked: true, can_call: true, can_video: true, ..Default::default() });
        assert_eq!(up.command("c"), C::Call);
        assert_eq!(up.command("v"), C::VideoCall);
        let this = keys(&Showing { view: Pane::Peers, dd_shown: true, dd_this: true, ..Default::default() });
        assert_eq!(this.command("d"), C::None);
        assert_eq!(this.command("l"), C::None);
        assert_eq!(this.command("c"), C::None);
    }

    #[test]
    fn someone_who_cant_be_called_has_their_call_keys_greyed() {
        let hint = |keys: &Keys, key: &str| keys.hints().into_iter().find(|h| h.key == key).unwrap();
        let offline = keys(&Showing { can_call: false, can_video: false, ..peer() });
        assert!(hint(&offline, "c").dim && hint(&offline, "v").dim);
        assert_eq!(offline.command("c"), C::Swallow);
        assert_eq!(offline.command("v"), C::Swallow);
        let voice_only = keys(&Showing { can_video: false, ..peer() });
        assert!(!hint(&voice_only, "c").dim && hint(&voice_only, "v").dim);
        assert_eq!(voice_only.command("c"), C::Call);
        let adding = keys(&Showing { call_live: true, can_add: true, can_call: false, ..peer() });
        assert!(hint(&adding, "c").dim);
        assert_eq!(adding.command("c"), C::Swallow);
        assert!(!hint(&keys(&peer()), "c").dim);
    }
}
