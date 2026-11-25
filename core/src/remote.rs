//! Controlling a shared screen from the other end of a call: what's
//! asked and answered (`Control`), and the pointer and keys sent over
//! (`Input`). Both go as call signals on the link between the two, and
//! only on a direct one: nothing of this is ever passed on by a third
//! member of a conference.
//!
//! The one watching asks, and the one sharing allows or refuses, each
//! time; once allowed, what they do over the picture of the screen is
//! done on it, until either stops. Doing it (and deciding whether to
//! allow) is the front end's: see `swarm_app::remote`.

use crate::wire::{Reader, Writer};
use anyhow::Result;

/// The longest reason a refusal gives, in bytes.
pub const MAX_REASON: usize = 200;
/// The most text one `Input::Text` carries, in bytes.
pub const MAX_TEXT: usize = 400;

/// What's asked and answered about controlling a shared screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Watching: "may I control your screen?"
    Ask,
    /// Watching: "I don't want it any more" — an ask withdrawn, or
    /// control given back.
    GiveBack,
    /// Sharing: "you may: what you do over my screen's picture is done
    /// on it".
    Allow,
    /// Sharing: "no", and why ("refused", or that this desktop can't be
    /// controlled).
    Refuse(String),
    /// Sharing: "you're not controlling it any more" — stopped here, the
    /// sharing over, or someone else given it.
    Stop,
    /// Sharing: what's sent is being ignored for a moment (`true`: the
    /// one sharing is using their own mouse), or no longer (`false`).
    Paused(bool),
}

/// Something done over a shared screen's picture, to be done on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// The pointer's at this point of the picture as it's sent (bars and
    /// all), across and down: 0 the left or top edge, 65535 the right or
    /// bottom.
    Pointer { x: u16, y: u16 },
    /// A mouse button pressed (`down`) or let go: see `BUTTON_LEFT` and
    /// the rest.
    Button { button: u8, down: bool },
    /// The wheel turned, in 120ths of a notch (Windows' own measure):
    /// `dy` positive away from the user (scrolling up), `dx` positive to
    /// the right.
    Wheel { dx: i16, dy: i16 },
    /// A key pressed or let go: one that types nothing (see `keys`), or
    /// one that would, pressed with Control, Alt or the logo key held (a
    /// shortcut), as the character it types without Shift.
    Key { key: char, down: bool },
    /// Characters typed, to be put in as they are, whatever the keyboard
    /// at the other end is laid out as.
    Text(String),
}

pub const BUTTON_LEFT: u8 = 0;
pub const BUTTON_MIDDLE: u8 = 1;
pub const BUTTON_RIGHT: u8 = 2;
pub const BUTTON_BACK: u8 = 3;
pub const BUTTON_FORWARD: u8 = 4;

/// The keys that type nothing, as `Input::Key` carries them: the same
/// characters Slint gives them (control characters, and Unicode's
/// private use area), so a window's keys go as they come.
pub mod keys {
    pub const BACKSPACE: char = '\u{0008}';
    pub const TAB: char = '\u{0009}';
    pub const RETURN: char = '\u{000a}';
    pub const ESCAPE: char = '\u{001b}';
    pub const DELETE: char = '\u{007f}';
    pub const SHIFT: char = '\u{0010}';
    pub const CONTROL: char = '\u{0011}';
    pub const ALT: char = '\u{0012}';
    pub const ALT_GR: char = '\u{0013}';
    pub const CAPS_LOCK: char = '\u{0014}';
    pub const SHIFT_R: char = '\u{0015}';
    pub const CONTROL_R: char = '\u{0016}';
    pub const META: char = '\u{0017}';
    pub const META_R: char = '\u{0018}';
    pub const SPACE: char = ' ';
    pub const UP: char = '\u{F700}';
    pub const DOWN: char = '\u{F701}';
    pub const LEFT: char = '\u{F702}';
    pub const RIGHT: char = '\u{F703}';
    /// F1 to F24 follow on from here.
    pub const F1: char = '\u{F704}';
    pub const F24: char = '\u{F71B}';
    pub const INSERT: char = '\u{F727}';
    pub const HOME: char = '\u{F729}';
    pub const END: char = '\u{F72B}';
    pub const PAGE_UP: char = '\u{F72C}';
    pub const PAGE_DOWN: char = '\u{F72D}';
    pub const SCROLL_LOCK: char = '\u{F72F}';
    pub const PAUSE: char = '\u{F730}';
    pub const PRINT_SCREEN: char = '\u{F731}';
    pub const MENU: char = '\u{F735}';

    /// Shift, Control, Alt or the logo key, either side.
    pub fn is_modifier(key: char) -> bool {
        matches!(key, SHIFT | SHIFT_R | CONTROL | CONTROL_R | ALT | META | META_R)
    }

    /// A key that types nothing: one of the above (Space aside, which
    /// types a space).
    pub fn is_named(key: char) -> bool {
        key != SPACE && (key.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&key))
    }
}

impl Control {
    pub(crate) fn write(&self, w: &mut Writer) {
        match self {
            Control::Ask => w.u8(0),
            Control::GiveBack => w.u8(1),
            Control::Allow => w.u8(2),
            Control::Refuse(why) => w.u8(3).str(why),
            Control::Stop => w.u8(4),
            Control::Paused(paused) => w.u8(5).bool(*paused),
        };
    }

    /// `None`: a kind this build doesn't know (a newer one's), skipped.
    pub(crate) fn read(r: &mut Reader) -> Result<Option<Control>> {
        Ok(Some(match r.u8()? {
            0 => Control::Ask,
            1 => Control::GiveBack,
            2 => Control::Allow,
            3 => {
                let mut why = crate::text::single_line(&r.str()?);
                if why.len() > MAX_REASON {
                    let end = (0..=MAX_REASON).rev().find(|&i| why.is_char_boundary(i)).unwrap_or(0);
                    why.truncate(end);
                }
                Control::Refuse(why)
            }
            4 => Control::Stop,
            5 => Control::Paused(r.bool()?),
            _ => return Ok(None),
        }))
    }
}

impl Input {
    pub(crate) fn write(&self, w: &mut Writer) {
        match self {
            Input::Pointer { x, y } => w.u8(0).u16(*x).u16(*y),
            Input::Button { button, down } => w.u8(1).u8(*button).bool(*down),
            Input::Wheel { dx, dy } => w.u8(2).u16(*dx as u16).u16(*dy as u16),
            Input::Key { key, down } => w.u8(3).u32(*key as u32).bool(*down),
            Input::Text(text) => w.u8(4).str(text),
        };
    }

    /// `None`: a kind this build doesn't know (a newer one's), skipped.
    pub(crate) fn read(r: &mut Reader) -> Result<Option<Input>> {
        Ok(Some(match r.u8()? {
            0 => Input::Pointer { x: r.u16()?, y: r.u16()? },
            1 => Input::Button { button: r.u8()?, down: r.bool()? },
            2 => Input::Wheel { dx: r.u16()? as i16, dy: r.u16()? as i16 },
            3 => {
                let key = char::from_u32(r.u32()?).ok_or_else(|| anyhow::anyhow!("not a key"))?;
                Input::Key { key, down: r.bool()? }
            }
            4 => {
                let text = r.str()?;
                if text.len() > MAX_TEXT {
                    anyhow::bail!("more text than {MAX_TEXT} bytes at once");
                }
                Input::Text(text.chars().filter(|c| !c.is_control()).collect())
            }
            _ => return Ok(None),
        }))
    }
}
