//! Things to tell the person: what just happened, or what went wrong.
//! Each has a kind, which says how it's shown and how long it stays.

use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Something happened; nothing to do.
    Info,
    /// Something asked for worked.
    Success,
    /// Needs attention, or is waiting on an answer.
    Warning,
    /// Something went wrong.
    Error,
}

impl Kind {
    /// How long it stays before it clears itself. A warning stays: it's
    /// usually a question waiting on an answer.
    pub fn lasts(self) -> Option<Duration> {
        match self {
            Kind::Info | Kind::Success => Some(Duration::from_secs(8)),
            Kind::Error => Some(Duration::from_secs(20)),
            Kind::Warning => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub kind: Kind,
    pub text: String,
}

impl Note {
    pub fn info(text: impl Into<String>) -> Note {
        Note { kind: Kind::Info, text: text.into() }
    }
    pub fn success(text: impl Into<String>) -> Note {
        Note { kind: Kind::Success, text: text.into() }
    }
    pub fn warning(text: impl Into<String>) -> Note {
        Note { kind: Kind::Warning, text: text.into() }
    }
    pub fn error(text: impl Into<String>) -> Note {
        Note { kind: Kind::Error, text: text.into() }
    }
}
