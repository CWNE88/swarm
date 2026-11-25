//! Voice calls: a ring/answer exchange followed by continuous Opus
//! audio both ways, carried *in-band* on the peer's one persistent
//! connection — as ordinary `protocol` frames (`Frame::Call`), not on a
//! stream of their own. That's what lets cover mode cover calls: on a
//! connection with cover on, call signals and audio ride the same
//! fixed-size, fixed-interval slots as everything else (audio in the
//! slots' priority lane — see `cover::Outbound::queue_priority`), so a
//! call looks exactly like an idle connection on the wire. An earlier
//! version gave each call its own QUIC stream, which bypassed the slots
//! entirely: a call showed up as a jump from ~40 to ~170 packets/s.
//!
//! The connection itself is driven by one task per peer (`swarm-cli`'s
//! `node::peer::PeerTask` — the only thing allowed to touch its Noise
//! session; see `protocol`'s module docs). So a call never touches the
//! connection directly: it holds a `CallLink` — a channel *into* that
//! task (`CallCommand`s: signals and audio to send) and a channel back
//! *out* of it (`CallSignal`s the peer sent). Signals are fire-and-
//! forget, never part of `protocol`'s one-request-outstanding
//! turn-taking, so a call and ordinary messages share the connection
//! without either stalling the other.
//!
//! Each direction picks its own shape, same as cover mode generally: a
//! side with cover on sends its audio in slots, a side without sends it
//! as plain frames; the receiving side doesn't care which.
//!
//! The encoder sizes every frame to fit what the slots currently have
//! room for (`CallLink::audio_budget`, kept up to date by the connection
//! task) — Opus can change bitrate frame to frame with no renegotiation,
//! and a hard byte cap per frame means it can never overrun the cover
//! rate. Without cover it's capped at `MAX_AUDIO_BYTES` (~40kbps,
//! comfortably above where speech stops improving).
//!
//! No loss concealment or FEC: the connection is a reliable QUIC stream,
//! so nothing is lost, only (rarely) late. Playback keeps a small
//! jitter buffer instead (see `swarm-cli`'s `audio` module), since slots
//! deliver audio in 40ms bursts rather than a steady 20ms trickle.
//!
//! Placing a call needs the peer's connection to already be up — a call
//! that can't be answered right now isn't something to queue for later.
//! Only peers on the local peer list can ring in: the connection task
//! that receives a ring exists only for known peers.

use crate::video::{Picture, VideoDecoder, VideoEncoder, VideoPiece};
use anyhow::{bail, Result};
use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Bitrate, Channels, SampleRate};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

/// Opus's own native rate — using anything else would mean resampling
/// somewhere, which this v1 deliberately doesn't do.
pub const SAMPLE_RATE: u32 = 48_000;
/// 20ms is Opus's own recommended default frame size: long enough to
/// encode efficiently, short enough that a late frame is a
/// barely-noticeable click rather than an audible gap.
pub const FRAME_DURATION: Duration = Duration::from_millis(20);
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * 20;

/// How long a placed call rings before giving up on an answer — either
/// because nobody's running a TUI to show the prompt at all (headless
/// `listen` refuses every ring straight away) or because a human just
/// hasn't picked up yet.
pub const RING_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on one encoded audio frame: 100 bytes per 20ms is
/// 40kbps, comfortably above where Opus speech stops getting better.
pub const MAX_AUDIO_BYTES: usize = 100;
/// Lower bound: 15 bytes per 20ms (6kbps) is about the least Opus can
/// still make intelligible speech out of. A cover rate that can't fit
/// even this (see `protocol::cover_rate_carries_voice`) can't carry a
/// call at all.
pub const MIN_AUDIO_BYTES: usize = 15;
/// Voice's cap while video is on (24kbps): good speech, and the rest of
/// the slots for the picture.
pub const AUDIO_BYTES_WITH_VIDEO: usize = 60;

/// Without cover, how many bytes each audio frame may take. Every frame
/// goes on the connection's one reliable stream, so a call sending faster
/// than the link carries builds a queue until packets are lost, and then
/// nothing gets through at all and the connection is given up. An
/// uncovered call takes about 80 kbit/s each way at the most, so that's
/// any link slower than that. So it's lowered while the round trip shows
/// a queue building (a third a second), and raised again slowly once the
/// queue has gone. Fed the round trip about once a second during a call.
pub struct UncoveredAudio {
    /// The least round trip seen: the link with nothing queued. It's let
    /// creep up, so a route that's become longer isn't taken for a queue.
    least_rtt: Option<Duration>,
    bytes: usize,
    /// How far the round trip has been over the least, the last second
    /// and the one before (see `next`): a queue.
    queued: Duration,
    /// How far the last round trip was over the least.
    over_before: Duration,
}

/// A round trip this much over the least: a queue is building.
const QUEUE_BUILDING: Duration = Duration::from_millis(150);
/// Less than this over it: no queue to speak of.
const QUEUE_GONE: Duration = Duration::from_millis(50);

impl UncoveredAudio {
    pub fn new() -> UncoveredAudio {
        UncoveredAudio { least_rtt: None, bytes: MAX_AUDIO_BYTES, queued: Duration::ZERO, over_before: Duration::ZERO }
    }

    /// Bytes per frame from now on, given the round trip now.
    pub fn next(&mut self, rtt: Duration) -> usize {
        let least = self.least_rtt.map_or(rtt, |least| (least + least / 50).min(rtt));
        self.least_rtt = Some(least);
        let over = rtt.saturating_sub(least);
        let queued = over.min(self.over_before);
        self.over_before = over;
        self.queued = queued;
        if queued > QUEUE_BUILDING {
            self.bytes = (self.bytes * 2 / 3).max(MIN_AUDIO_BYTES);
        } else if over < QUEUE_GONE {
            self.bytes = (self.bytes + 5).min(MAX_AUDIO_BYTES);
        }
        self.bytes
    }
}

impl Default for UncoveredAudio {
    fn default() -> Self {
        Self::new()
    }
}

/// Without cover, video's bitrate. Video can take far more than a link
/// carries, and a queue can build in two places: on the path, which the
/// round trip shows (as for `UncoveredAudio`), and inside QUIC, which
/// takes whatever's written and sends it as fast as the link allows —
/// the round trip doesn't show that one at all, but bytes written
/// running ahead of bytes sent does. Either cuts the rate at once — to a
/// little under what QUIC managed to send, when it's backed up, since
/// that's what the link carries. With neither for a while, it's raised a
/// tenth a second, up to `UNCOVERED_VIDEO_MAX`: slowly, since each time
/// it passes what the link carries, the voice behind it waits — and
/// never far past what's actually being sent: a still picture takes
/// little, and that proves nothing about the link. Except as far as the
/// next bigger shape needs: a still picture made small by a slow spell
/// would otherwise stay small for good — and one shape up is a probe the link soon
/// answers, a shape's bitrate being capped.
pub struct UncoveredVideo {
    bps: u32,
    /// Bytes written that QUIC hasn't sent yet, as near as can be told.
    backlog: u64,
    /// Totals written and sent when last asked.
    last: Option<(u64, u64)>,
    /// Seconds to hold before raising again, after a cut.
    hold: u32,
    /// What the next bigger shape needs (see `UncoveredVideo::next`).
    bigger: u32,
    /// What the last `next` did, and why: for `SWARM_RATE_DEBUG`.
    why: &'static str,
    /// What the link's been seen to carry, as near as can be told: what
    /// QUIC managed when it backed up, or a little under a rate that
    /// built a queue, or the most it's been raised to since. Below it,
    /// the rate climbs back quickly after a cut (a quarter a second);
    /// above it, a tenth a second.
    good: u32,
    /// The call's start: the rate doubles each second, rather than
    /// rising a tenth, until anything at all says it's enough (a queue,
    /// QUIC backed up, the link stopping, the round trip not near its
    /// least) — so a call on a fast link is at its picture's size in
    /// seconds, not half a minute, and the camera's opened once at it
    /// (see `UncoveredCall::starting_at`).
    starting: bool,
    /// This second lost more than `LOSSY` of its packets (`UncoveredCall::lossy`):
    /// what QUIC carried in it was cut short by resending, and says
    /// little of what the link can carry.
    lossy: bool,
}

/// Nothing at all from the other side for this long, in a call: the
/// link's stopped for a moment (see `AuthenticatedConnection::quiet`). A
/// call's always sending — silence too, while muted — and QUIC
/// acknowledges it within 80 ms (`transport`'s ACK delay), so this is no
/// slow link, and what QUIC did or didn't send meanwhile says nothing of
/// what it carries: the rate's left as it was (see `UncoveredCall::link_gone`).
pub const LINK_STOPPED: Duration = Duration::from_millis(150);
/// ... and this long: gone (a Wi-Fi roam, say). Pictures aren't sent
/// into it to arrive late, and once it's back a full one goes first.
pub const LINK_GONE: Duration = Duration::from_millis(300);

/// Where video starts on a connection without cover.
pub const UNCOVERED_VIDEO_START: u32 = 1_000_000;
/// The most it's raised to: plenty for 4K.
pub const UNCOVERED_VIDEO_MAX: u32 = 16_000_000;
/// The most video goes at without cover: `UNCOVERED_VIDEO_MAX`, or three
/// quarters of the bandwidth limit (`transport::bandwidth_limit`),
/// leaving the rest to voice, the small pictures and QUIC's own packets.
pub fn uncovered_video_most() -> u32 {
    crate::transport::bandwidth_limit().map_or(UNCOVERED_VIDEO_MAX, |limit| (limit / 4 * 3).min(UNCOVERED_VIDEO_MAX as u64) as u32)
}

/// Video below this is turned off, leaving the link to voice.
const UNCOVERED_VIDEO_LEAST: u32 = 8_000;
/// More than this much of a second's video waiting inside QUIC: a queue.
const BACKLOG_SHARE: u64 = 8;
/// Writing to QUIC waiting this long, all told, in a second: a queue
/// before it (see `transport::AuthenticatedConnection::write_wait`).
const WRITE_WAIT: Duration = Duration::from_millis(125);
/// A queue on the path this long: cut by half, not a fifth — a link far
/// slower than the call (or a deep buffer filling) would otherwise
/// leave a queue of seconds, voice waiting in it.
const BIG_QUEUE: Duration = Duration::from_millis(250);
/// More of the packets sent than this lost since the last asking: the
/// link's lossy. Nothing heard for a while then is QUIC waiting out its
/// losses with a window of a few packets, not the link stopped, and is
/// no reason to hold the rates (see `UncoveredCall::lossy`).
pub const LOSSY: f64 = 0.02;
/// A round trip this much over the least: a queue on the path. Less than
/// voice allows (`QUEUE_BUILDING`): video is what's filling it.
const VIDEO_QUEUE: Duration = Duration::from_millis(60);
/// The rate isn't raised once it's more than this much over what's being
/// sent (a quarter): beyond that the link's unproven, and a picture that
/// suddenly takes all it's allowed would swamp a slow one. (An easy
/// picture still gets big: it takes little at any size; and the next
/// shape up may always be tried, see `allow_bigger`.) Allowed four
/// times, a picture held at 640×360's ceiling on a 1 Mbit/s link could
/// be allowed 4.6, and the next shape up take it all at once: seconds of
/// queue, voice gone meanwhile.
/// A quarter over, each step's a probe the link answers while its queue
/// is small. Each second's use is set against the rate it had, so a
/// picture taking all it's given (the start's doubling too) climbs on.
const UNPROVEN: (u64, u64) = (5, 4);

impl UncoveredVideo {
    pub fn new() -> UncoveredVideo {
        UncoveredVideo { bps: UNCOVERED_VIDEO_START.min(uncovered_video_most()), backlog: 0, last: None, hold: 0, bigger: 0, why: "start", good: 0, starting: true, lossy: false }
    }

    /// Bits a second from now on, given how far the round trip is over
    /// the least (`UncoveredAudio::queued`) and the connection's totals of
    /// bytes written and bytes QUIC has sent. Asked about once a second.
    /// The bitrate the next bigger shape needs (0: none): it may always be
    /// raised that far, however little the picture takes.
    pub fn allow_bigger(&mut self, needs: u32) {
        self.bigger = needs;
    }

    pub fn next(&mut self, queued: Duration, written: u64, sent: u64) -> u32 {
        self.next_given(queued, queued, written, sent, Duration::ZERO)
    }

    /// `next`, given as well how far over the least this second's round
    /// trip was (`queued` being how far it's stayed over, see
    /// `UncoveredAudio::next`): raised only while that's low too.
    fn next_given(&mut self, queued: Duration, now: Duration, written: u64, sent: u64, write_wait: Duration) -> u32 {
        let (mut used, mut carried) = (None, None);
        if let Some((written_before, sent_before)) = self.last {
            let (wrote, went) = (written.saturating_sub(written_before), sent.saturating_sub(sent_before));
            self.backlog = (self.backlog + wrote).saturating_sub(went);
            used = Some(wrote * 8);
            carried = Some(went * 8);
        }
        self.last = Some((written, sent));
        let backed_up = self.backlog * 8 * BACKLOG_SHARE > self.bps as u64 || (used.is_some() && write_wait > WRITE_WAIT);
        let link = carried.map_or(self.bps, |c| c.min(u32::MAX as u64) as u32);
        if backed_up || queued > VIDEO_QUEUE || now >= QUEUE_GONE {
            self.starting = false;
        }
        if backed_up && self.bps > link / 10 * 9 {
            let to = self.bps.min(link) / 5 * 4;
            self.bps = (if self.lossy { to.max(self.bps / 2) } else { to }).max(UNCOVERED_VIDEO_LEAST);
            self.good = self.bps;
            self.hold = 2;
            self.why = if self.lossy { "cut: QUIC backed up, lossy" } else { "cut: QUIC backed up" };
        } else if backed_up {
            self.hold = 2;
            self.why = "held: QUIC backed up";
        } else if queued > VIDEO_QUEUE {
            self.good = self.bps / 10 * 9;
            self.bps = (if queued > BIG_QUEUE { self.bps / 2 } else { self.bps / 5 * 4 }).max(UNCOVERED_VIDEO_LEAST);
            self.hold = 2;
            self.why = "cut: round trip over the least";
        } else if self.hold > 0 {
            self.hold -= 1;
            self.why = "held after a cut";
        } else if now < QUEUE_GONE && (used.is_some_and(|used| used * UNPROVEN.0 >= self.bps as u64 * UNPROVEN.1) || self.bps < self.bigger) {
            if self.starting {
                self.bps = self.bps.saturating_mul(2).min(UNCOVERED_VIDEO_MAX);
                self.good = self.bps;
                self.starting = self.bps < UNCOVERED_VIDEO_MAX;
                self.why = "raised: doubled, starting";
            } else if self.bps < self.good {
                self.bps = (self.bps / 4 * 5).min(self.good);
                self.why = "raised: back towards what the link carried";
            } else {
                self.bps = (self.bps / 10 * 11).min(UNCOVERED_VIDEO_MAX);
                self.good = self.bps;
                self.why = "raised";
            }
        } else if now >= QUEUE_GONE {
            self.why = "kept: round trip not near the least";
        } else {
            self.why = "kept: not using what it has";
            if used.is_some() {
                self.starting = false;
            }
        }
        self.bps = self.bps.min(uncovered_video_most());
        self.bps
    }
}

impl Default for UncoveredVideo {
    fn default() -> Self {
        Self::new()
    }
}

/// Without cover, what a call may send: voice's bytes per frame and
/// video's bitrate, from what the link's been seen to carry.
#[derive(Default)]
pub struct UncoveredCall {
    audio: UncoveredAudio,
    video: UncoveredVideo,
    /// Seconds left, after the link stopped (`link_gone`), before the
    /// round trip and what QUIC sent are trusted again: the round trip
    /// takes in the wait, and what piled up goes all at once.
    settle: u32,
    /// QUIC's counts of packets lost and sent when last asked (`lossy`).
    losses: Option<(u64, u64)>,
}

/// How long after the link stopped a call's rates are held, the round
/// trip and what QUIC sends being thrown by it (see `UncoveredCall`).
const SETTLE_AFTER_STOP: u32 = 2;

impl UncoveredCall {
    pub fn new() -> UncoveredCall {
        UncoveredCall::default()
    }

    /// Voice's bytes per frame and video's bits a second from now on,
    /// given the round trip now, the connection's totals of bytes written
    /// and sent, and what the next bigger shape needs (see
    /// `UncoveredVideo`).
    pub fn next(&mut self, rtt: Duration, written: u64, sent: u64, bigger: u32, write_wait: Duration) -> (usize, u32) {
        if self.settle > 0 {
            self.settle -= 1;
            self.audio.over_before = Duration::ZERO;
            self.video.last = Some((written, sent));
            self.video.backlog = 0;
            self.video.why = "kept: settling after the link stopped";
            return (self.audio.bytes, self.video.bps.min(uncovered_video_most()));
        }
        let audio = self.audio.next(rtt);
        self.video.allow_bigger(bigger);
        (audio, self.video.next_given(self.audio.queued, self.audio.over_before, written, sent, write_wait))
    }

    /// The link's stopped (`LINK_STOPPED`) at some point since last asked:
    /// what was and wasn't sent meanwhile says nothing of what it carries,
    /// so the rates stay as they were, and what's measured starts afresh.
    /// Whether more than `LOSSY` of the packets sent since this was last
    /// asked were lost, given QUIC's counts of packets lost and sent.
    pub fn lossy(&mut self, lost: u64, sent: u64) -> bool {
        let lossy = self.losses.is_some_and(|(lost_before, sent_before)| {
            let sent = sent.saturating_sub(sent_before);
            sent > 0 && lost.saturating_sub(lost_before) as f64 > sent as f64 * LOSSY
        });
        self.losses = Some((lost, sent));
        self.video.lossy = lossy;
        lossy
    }

    pub fn link_gone(&mut self) {
        self.video.last = None;
        self.video.backlog = 0;
        self.settle = SETTLE_AFTER_STOP;
        self.video.starting = false;
    }

    /// A call starting at four fifths of `carried` (bits a second, what
    /// the last call on this path was seen to carry: see `carried`), or,
    /// with none, where any starts; doubling from there until something
    /// says it's enough. Its picture's size is then right from the
    /// start, and the camera's opened at it once, not reopened as the
    /// rate creeps up.
    pub fn starting_at(carried: Option<u32>) -> UncoveredCall {
        let mut call = UncoveredCall::new();
        if let Some(carried) = carried.filter(|&c| c > UNCOVERED_VIDEO_START) {
            call.video.bps = (carried / 5 * 4).max(UNCOVERED_VIDEO_START).min(uncovered_video_most());
            call.video.good = call.video.bps;
        }
        call
    }

    /// What the link's been seen to carry for video, bits a second: for
    /// the next call on this path to start at (`starting_at`). `None`
    /// until it's been measured past the start.
    pub fn carried(&self) -> Option<u32> {
        (self.video.good > UNCOVERED_VIDEO_START && !self.video.starting).then_some(self.video.good)
    }

    /// The least round trip seen, how far the last was over it, QUIC's
    /// backlog as near as can be told, and what the last `next` did to
    /// video and why: for `SWARM_RATE_DEBUG`.
    pub fn explain(&self) -> (Duration, Duration, u64, &'static str) {
        (self.audio.least_rtt.unwrap_or_default(), self.audio.queued, self.video.backlog, self.video.why)
    }
}

/// What a call sends and receives. Travels as `protocol::Frame::Call`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallSignal {
    /// "I'm calling you" — opens a ring on the receiving side. `video`:
    /// a video call is offered.
    /// `at`: when the caller started ringing (unix seconds), the same on
    /// every device of the callee's, so a
    /// call none of them answered is noted at one time on all of them.
    Invite { video: bool, at: u64 },
    /// `video`: answered with video (only ever if it was offered).
    Accept { video: bool },
    Reject,
    /// Either side ending the call — or, before it's answered, the
    /// caller giving up on the ring.
    Hangup,
    /// One encoded 20ms Opus frame.
    Audio(Vec<u8>),
    /// One piece of one video frame (see `video`).
    Video(VideoPiece),
    /// "Send a full frame" of the small pictures (`small`) or the large:
    /// something was lost, or the large are to be shown from now (see
    /// `video`, `ShownIn`).
    KeyframeRequest { small: bool },
    /// To a ringing device of someone on several: another of their
    /// devices has answered, so stop ringing.
    AnsweredElsewhere,
    /// "Our call carries on here": first on a new connection, from a side
    /// that held the call when the last one went (one of us moved to
    /// another network, say). Answered with `Resume` by a side that still
    /// has it, `Hangup` by one that doesn't. Never reaches a `CallLink`:
    /// the connection task deals with it.
    Resume,
    /// "The call you have with another of my devices carries on here":
    /// the connection that call went by is lost for good (a relay gone),
    /// and this one, from the same user, is taking it over. Sent again
    /// each second until it's answered with `Resume`, by the device of
    /// theirs that has the call; the others say nothing. Never reaches a
    /// `CallLink`: the connection task deals with it.
    Takeover,
    /// "My camera's off": no pictures are coming, so the last one isn't
    /// shown standing still. Sent when it goes off, and every
    /// `CAMERA_OFF_EVERY` while it stays off (one lost, or sent while a
    /// call's held, isn't then the last word).
    CameraOff,
    /// "What I'm sending is my screen" (or, `false`, my camera again): so
    /// it's shown as a screen. Sent when it changes, and every
    /// `CAMERA_OFF_EVERY` while a screen's shared (one lost isn't then
    /// the last word).
    Screen(bool),
    /// "I decode H.265" (or, `false`, not — Settings says H.264 only, the
    /// chip doesn't, or it failed): video's sent to this side in H.265
    /// only while it's said so. Sent when a video call starts, when it
    /// changes, and every `DECODES_EVERY`.
    Decodes { h265: bool },
    /// "I show your pictures large in a box this size, and small in a
    /// strip's tile this size", in this side's own pixels (0 by 0: not
    /// known; the small, none wanted): pictures are sent no bigger than
    /// drawn there (see `drawn_long`), the small and the large each a
    /// stream of its own, both all the time — so whoever's shown large
    /// next is there to be shown at once, from the next full frame (see
    /// `KeyframeRequest`). Sent when it changes, and every
    /// `DECODES_EVERY`.
    ShownIn { width: u16, height: u16, small_width: u16, small_height: u16 },
    /// Something to or from a conference's member whose pictures are
    /// passed on by whoever's between us (see `Forward`): `id`'s the one
    /// that member's voice comes as (`Relayed`). Coming to us, their
    /// pictures (`Video`, `CameraOff`, `Screen`) as they sent them; going
    /// back, what's asked of them (`KeyframeRequest`, `ShownIn`). The one
    /// between passes each on, and on to the other link.
    Via { id: u8, signal: Box<CallSignal> },
    /// "Join our conference": a ring like `Invite` (voice only), into the
    /// conference this roster is of — or, from someone already in the
    /// same one as us, the link between us two, answered without asking
    /// (see `conference`). The roster has the one rung added already.
    ConferenceInvite(crate::conference::Roster),
    /// "Link to me in our conference": from a member who's our peer, and
    /// the one of us two to ask (`conference::asks`), sent to each of our
    /// devices online. Only the one in that conference (same roster id)
    /// takes it, answered without asking; any other turns it down and
    /// never rings (`node::CallEvent::LinkAsked`). Older builds ignore it.
    ConferenceLink(crate::conference::Roster),
    /// What this side now knows of the conference's roster: sent on every
    /// link whenever it changes, and passed on by whoever learns something
    /// new from it. Never reaches a `CallLink`: the connection task hands
    /// it to the front end (`node::CallEvent::Roster`).
    Roster(crate::conference::Roster),
    /// "These are the conference's members I hear directly": sent on every
    /// link whenever that changes, so a member who can't hear one of them
    /// knows whom to ask to pass their voice on (`Forward`). To the front
    /// end, as `node::CallEvent::Conference`.
    Hearing(Vec<[u8; 32]>),
    /// "Pass `from`'s voice on to me, as `Relayed` with this `id`" (or,
    /// `on` false, stop): to a member who hears both of us, from one who
    /// can't hear `from` directly. To the front end, as
    /// `node::CallEvent::Conference`.
    Forward { from: [u8; 32], id: u8, on: bool },
    /// One 20ms Opus frame of someone else's voice, passed on as asked by
    /// `Forward` (and encoded afresh, to fit this link). Goes to the
    /// call's relay tap (see `CallLink::relay_to`).
    Relayed { id: u8, opus: Vec<u8> },
    /// Asking for, giving and ending control of a shared screen (see
    /// `remote`). Only ever on the link between the two, never `Via`.
    /// Never reaches a `CallLink`: the connection task hands it to the
    /// front end (`node::CallEvent::Control`).
    Control(crate::remote::Control),
    /// The pointer and keys over a shared screen's picture, from whoever's
    /// been let control it (see `remote`). To the front end, as
    /// `node::CallEvent::Control`.
    Input(crate::remote::Input),
}

impl CallSignal {
    /// Whether this starts a call: a ring (into a conference, or not), or
    /// a member asking for the link between us.
    pub fn is_ring(&self) -> bool {
        matches!(self, CallSignal::Invite { .. } | CallSignal::ConferenceInvite(_) | CallSignal::ConferenceLink(_))
    }

    /// The bytes of voice or pictures this carries, as they go on the
    /// wire (0 for one that only steers the call): what's shown as a
    /// call's traffic (see `CallBudget::sent`).
    pub fn media_bytes(&self) -> usize {
        match self {
            CallSignal::Audio(opus) | CallSignal::Relayed { opus, .. } => opus.len(),
            CallSignal::Video(piece) => piece.data.len() + crate::video::PIECE_OVERHEAD,
            CallSignal::Via { signal, .. } => signal.media_bytes(),
            _ => 0,
        }
    }
}

/// While the camera's off, how often the other side's told so (and while
/// a screen's shared, that it is).
const CAMERA_OFF_EVERY: Duration = Duration::from_secs(2);
/// A shared screen that's changed is sent again, the same, at its pace
/// for this long after, then only now and then: the graphics chip
/// sharpens a still picture over the pictures after it (an xterm at
/// 1080p, 15 a second: 38 dB at first, 42 after one second, 51 after
/// two), where a screen otherwise sent only when it changes would stay
/// blurred. OpenH264 sends next to nothing for a picture that's the same.
const SCREEN_SHARPENS_FOR: Duration = Duration::from_secs(2);
/// How often the other side's told whether H.265 is decoded here, when
/// nothing's changed: should a telling go missing while the call's held.
const DECODES_EVERY: Duration = Duration::from_secs(10);

/// What a call can send right now, kept current by the connection task
/// after every cover slot (see `protocol::update_call_budget`): Opus's
/// bytes per frame, and video's bitrate and largest piece — `video_bps`
/// 0 when there's no room for video at all. `video_on` is set by `run`
/// while a call has video, which caps voice to make room for it.
pub struct CallBudget {
    pub audio: AtomicUsize,
    pub video_bps: AtomicUsize,
    pub video_piece: AtomicUsize,
    pub video_on: AtomicBool,
    /// The bitrate the next bigger shape needs, 0 if there's none to
    /// take (see `UncoveredVideo`). Set by `send_video`.
    pub video_bigger: AtomicUsize,
    /// How many others' voices a conference passes on along this link
    /// (see `CallSignal::Relayed`): the slots' room for voice is shared
    /// between them and ours.
    pub relayed: AtomicUsize,
    /// The connection has cover: everything on it fits its slots, so
    /// others' pictures aren't passed on along it (see `CallSignal::Via`).
    pub covered: AtomicBool,
    /// A call's voice and pictures sent on this link, and received, in
    /// bytes (see `CallSignal::media_bytes`): counted by the connection
    /// task, for showing how much a call takes.
    pub sent: AtomicU64,
    pub received: AtomicU64,
    /// The link's round trip, as QUIC measures it, in microseconds (0:
    /// not yet): kept about once a second, for showing.
    pub round_trip: AtomicU64,
}

impl CallBudget {
    pub fn new() -> Arc<CallBudget> {
        Arc::new(CallBudget {
            audio: AtomicUsize::new(MAX_AUDIO_BYTES),
            video_bps: AtomicUsize::new(0),
            video_piece: AtomicUsize::new(0),
            video_on: AtomicBool::new(false),
            video_bigger: AtomicUsize::new(0),
            covered: AtomicBool::new(false),
            relayed: AtomicUsize::new(0),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            round_trip: AtomicU64::new(0),
        })
    }
}

/// What a `CallLink` asks the peer's connection task to do.
pub enum CallCommand {
    /// Route the peer's incoming `CallSignal`s here from now on — sent
    /// once by whoever places a call, before ringing.
    Attach(mpsc::UnboundedSender<CallSignal>),
    /// As `Attach`, for a call another of the user's devices is in, this
    /// one passing it on (`node::proxycall`): every signal goes to it as
    /// it is, a conference's and a shared screen's business too, for the
    /// front end at the far end.
    Pass(mpsc::UnboundedSender<CallSignal>),
    /// Send this to the peer.
    Signal(CallSignal),
}

/// One call's handle on a peer's connection — see the module docs.
pub struct CallLink {
    commands: mpsc::UnboundedSender<CallCommand>,
    inbound: mpsc::UnboundedReceiver<CallSignal>,
    budget: Arc<CallBudget>,
    /// The ring ended because another of our devices answered it.
    elsewhere: bool,
    /// Where others' voices passed on along this link go (see
    /// `CallLink::relay_to`).
    relays: Option<mpsc::UnboundedSender<(u8, Vec<u8>)>>,
    /// Where the other side's pictures are handed on, for passing on to
    /// whoever's asked to see them through us (see `CallLink::pictures_to`).
    pictures: Option<mpsc::UnboundedSender<CallSignal>>,
    /// Where what comes inside `CallSignal::Via` goes (see
    /// `CallLink::vias_to`).
    vias: Option<mpsc::UnboundedSender<(u8, CallSignal)>>,
    /// Video given to the call once it's going (see
    /// `CallLink::video_later`).
    later: Option<VideoLater>,
}

/// Video for a call already going: what's given it, and where it's said
/// that the other side's pictures have started coming while there's none.
struct VideoLater {
    given: mpsc::UnboundedReceiver<VideoIo>,
    theirs: mpsc::UnboundedSender<()>,
}

impl CallLink {
    /// For whoever *receives* a ring: the connection task already routes
    /// this call's signals into `inbound`.
    pub fn new(
        commands: mpsc::UnboundedSender<CallCommand>,
        inbound: mpsc::UnboundedReceiver<CallSignal>,
        budget: Arc<CallBudget>,
    ) -> Self {
        CallLink { commands, inbound, budget, elsewhere: false, relays: None, pictures: None, vias: None, later: None }
    }

    /// For whoever *places* a call: asks the connection task to route
    /// the peer's call signals here. Errors if that task is gone.
    pub fn attach(commands: mpsc::UnboundedSender<CallCommand>, budget: Arc<CallBudget>) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        if commands.send(CallCommand::Attach(tx)).is_err() {
            bail!("not connected");
        }
        Ok(CallLink { commands, inbound: rx, budget, elsewhere: false, relays: None, pictures: None, vias: None, later: None })
    }

    fn send(&self, signal: CallSignal) -> Result<()> {
        if self.commands.send(CallCommand::Signal(signal)).is_err() {
            bail!("connection closed");
        }
        Ok(())
    }

    /// Answers; `video` with video, if it was offered.
    pub fn accept(&self, video: bool) -> Result<()> {
        self.send(CallSignal::Accept { video })
    }

    pub fn reject(&self) -> Result<()> {
        self.send(CallSignal::Reject)
    }

    pub fn hang_up(&self) -> Result<()> {
        self.send(CallSignal::Hangup)
    }

    /// After `caller_gave_up`: it was because another of our devices
    /// answered.
    pub fn answered_elsewhere(&self) -> bool {
        self.elsewhere
    }

    /// Sends any signal.
    pub fn signal(&self, signal: CallSignal) -> Result<()> {
        self.send(signal)
    }

    /// Where to send signals on this link: for a conference's roster,
    /// which goes on a link while `run` has it.
    pub fn commands(&self) -> mpsc::UnboundedSender<CallCommand> {
        self.commands.clone()
    }

    /// What a call can send on this link right now.
    pub fn budget(&self) -> Arc<CallBudget> {
        self.budget.clone()
    }

    /// In a conference: where `run` puts others' voices passed on along
    /// this link (`CallSignal::Relayed`), still encoded, with their ids.
    pub fn relay_to(&mut self, relays: mpsc::UnboundedSender<(u8, Vec<u8>)>) {
        self.relays = Some(relays);
    }

    /// In a conference: where `run` hands on the other side's pictures as
    /// they come (`Video`, `CameraOff`, `Screen`), for passing on to
    /// whoever's asked to see them through us. Still shown here as well.
    pub fn pictures_to(&mut self, pictures: mpsc::UnboundedSender<CallSignal>) {
        self.pictures = Some(pictures);
    }

    /// In a conference: where `run` puts what comes inside `Via`, with its
    /// id — another's pictures passed on to us, or what's asked of those
    /// we pass on.
    pub fn vias_to(&mut self, vias: mpsc::UnboundedSender<(u8, CallSignal)>) {
        self.vias = Some(vias);
    }

    /// Video can be given to the call once it's going, on `given` (our
    /// camera turned on in a voice call, or theirs to be shown): `run`
    /// starts sending and showing then. While it has none, the other
    /// side's pictures coming are said on `theirs`, once.
    pub fn video_later(&mut self, given: mpsc::UnboundedReceiver<VideoIo>, theirs: mpsc::UnboundedSender<()>) {
        self.later = Some(VideoLater { given, theirs });
    }

    /// Callee side, while ringing: has the caller given up? Checked by
    /// polling (the TUI's tick), not awaited, so the link stays whole
    /// for `run` if the call is answered. A closed channel — the
    /// connection went away — counts as giving up too.
    pub fn caller_gave_up(&mut self) -> bool {
        loop {
            match self.inbound.try_recv() {
                Ok(CallSignal::Hangup) | Ok(CallSignal::Reject) => return true,
                Ok(CallSignal::AnsweredElsewhere) => {
                    self.elsewhere = true;
                    return true;
                }
                Ok(_) => continue,
                Err(mpsc::error::TryRecvError::Empty) => return false,
                Err(mpsc::error::TryRecvError::Disconnected) => return true,
            }
        }
    }
}

/// Caller side: ring (offering video if `video`), and wait for an
/// answer. `Ok(Some(video))` means accepted and ready for `run`, `video`
/// saying whether they took it with video; `Ok(None)` covers rejection,
/// the connection dropping, and nobody answering within `RING_TIMEOUT` —
/// in which case the callee is told the ring is over, so their prompt
/// clears.
pub async fn ring(link: &mut CallLink, video: bool) -> Result<Option<bool>> {
    ring_with(link, CallSignal::Invite { video, at: crate::unix_now() }, video).await
}

/// Caller side, for a conference: rings them into it (voice only), with
/// `roster` (which has them added). `true` once they've joined.
pub async fn ring_into(link: &mut CallLink, roster: crate::conference::Roster) -> Result<bool> {
    Ok(ring_with(link, CallSignal::ConferenceInvite(roster), false).await?.is_some())
}

/// A member's side: asks for the link between us in the conference
/// `roster` is of. `true` once it's taken.
pub async fn ask_link(link: &mut CallLink, roster: crate::conference::Roster) -> Result<bool> {
    Ok(ring_with(link, CallSignal::ConferenceLink(roster), false).await?.is_some())
}

async fn ring_with(link: &mut CallLink, invite: CallSignal, video: bool) -> Result<Option<bool>> {
    link.send(invite)?;
    let answer = tokio::time::timeout(RING_TIMEOUT, async {
        loop {
            match link.inbound.recv().await {
                Some(CallSignal::Accept { video: theirs }) => return Some(video && theirs),
                Some(CallSignal::Reject) | Some(CallSignal::Hangup) | None => return None,
                Some(_) => continue,
            }
        }
    })
    .await;
    match answer {
        Ok(answered) => Ok(answered),
        Err(_) => {
            let _ = link.hang_up();
            Ok(None)
        }
    }
}

/// Wraps `audiopus`'s encoder with this module's fixed format (48kHz
/// mono, tuned for voice) so nothing outside this file has to know Opus
/// construction details.
pub struct AudioEncoder(Encoder);

impl AudioEncoder {
    pub fn new() -> Result<Self> {
        Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .map(AudioEncoder)
            .map_err(|e| anyhow::anyhow!("opus encoder init failed: {e:?}"))
    }

    /// Encodes one frame into at most `max_bytes` (clamped to
    /// `MIN_AUDIO_BYTES..=MAX_AUDIO_BYTES`). The bitrate is retargeted to
    /// that size first, and the output buffer is cut to it as a hard
    /// cap — Opus guarantees never to write more than it's given room
    /// for, so a frame can never outgrow its slot. `pcm` must be exactly
    /// `FRAME_SAMPLES` long.
    pub fn encode(&mut self, pcm: &[f32], max_bytes: usize) -> Result<Vec<u8>> {
        let max_bytes = max_bytes.clamp(MIN_AUDIO_BYTES, MAX_AUDIO_BYTES);
        let bits_per_second = (max_bytes * 8 * 1000 / FRAME_DURATION.as_millis() as usize) as i32;
        self.0.set_bitrate(Bitrate::BitsPerSecond(bits_per_second)).map_err(|e| anyhow::anyhow!("opus set bitrate failed: {e:?}"))?;
        let mut out = vec![0u8; max_bytes];
        let len = self.0.encode_float(pcm, &mut out).map_err(|e| anyhow::anyhow!("opus encode failed: {e:?}"))?;
        out.truncate(len);
        Ok(out)
    }
}

pub struct AudioDecoder(Decoder);

impl AudioDecoder {
    pub fn new() -> Result<Self> {
        Decoder::new(SampleRate::Hz48000, Channels::Mono).map(AudioDecoder).map_err(|e| anyhow::anyhow!("opus decoder init failed: {e:?}"))
    }

    /// Always returns `FRAME_SAMPLES` samples. An empty frame is one that
    /// was lost (voice in datagrams, `protocol::send_call`): Opus fills
    /// it in from what came before, rather than a click of silence.
    pub fn decode(&mut self, opus: &[u8]) -> Result<Vec<f32>> {
        let mut out = vec![0f32; FRAME_SAMPLES];
        let packet = (!opus.is_empty()).then_some(opus);
        let len = self.0.decode_float(packet, out.as_mut_slice(), false).map_err(|e| anyhow::anyhow!("opus decode failed: {e:?}"))?;
        out.truncate(len);
        Ok(out)
    }
}

/// A video call's picture plumbing, from the front end: the camera's
/// newest picture (`None` while it's off, or there isn't one — nothing is
/// sent then), where the other side's pictures go — large, and small for
/// a strip's tile (see `CallSignal::ShownIn`) — and counts for showing
/// how it's going.
pub struct VideoIo {
    pub camera: watch::Receiver<Option<Arc<Picture>>>,
    pub display: watch::Sender<Option<Arc<Picture>>>,
    pub small: watch::Sender<Option<Arc<Picture>>>,
    pub stats: Arc<VideoStats>,
}

#[derive(Default)]
pub struct VideoStats {
    pub frames_sent: AtomicU64,
    /// Of those, full frames.
    pub keyframes_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub frames_shown: AtomicU64,
    /// Full frames asked of the other side: something of theirs was lost.
    pub keyframes_asked: AtomicU64,
    /// The encoder's bitrate right now.
    pub bitrate: AtomicU64,
    /// What the link offers video right now: the encoder's given it, and
    /// uses it up to what the shape's worth (`bitrate`).
    pub offered: AtomicU64,
    /// The shape pictures are sent in now (see `VideoStats::sending`), and
    /// would be with big enough pictures (`VideoStats::wanted`).
    shape: AtomicU64,
    wanted: AtomicU64,
    /// Microseconds a picture takes to encode, and to decode (and turn
    /// into RGB), on average — where a slow computer's time goes.
    pub encode_micros: AtomicU64,
    pub decode_micros: AtomicU64,
    /// They've said their camera's off (`CallSignal::CameraOff`), and no
    /// picture has come since.
    pub their_camera_off: AtomicBool,
    /// What's sent is a shared screen, not the camera: set by the front
    /// end, while it feeds the call its screen.
    pub screen: AtomicBool,
    /// They've said what they're sending is their screen
    /// (`CallSignal::Screen`).
    pub their_screen: AtomicBool,
    /// What's sent is H.265, and what's received.
    pub sending_h265: AtomicBool,
    pub receiving_h265: AtomicBool,
    /// The hardware encoding and decoding, when it's not OpenH264 (see
    /// `video::Hardware`).
    pub encoding_on: std::sync::Mutex<Option<String>>,
    pub decoding_on: std::sync::Mutex<Option<String>>,
    /// The boxes the other side's pictures are shown in here, large and
    /// small, in this side's own pixels, packed (see `show_in`); 0, not
    /// known.
    shown_in: AtomicU64,
    /// In a conference, the biggest boxes their pictures are shown in by
    /// those we pass them on to (packed as `shown_in`; 0, none), and
    /// whether any of them doesn't decode H.265: what's asked of them is
    /// enough for those too (see `send_video`).
    passed_on_in: AtomicU64,
    /// Their large pictures aren't wanted now: set by the front end for
    /// those shown small and not about to be shown large. They're passed
    /// over as they come, and the last one's taken down (`VideoIo::display`
    /// to `None`); wanted again, a full frame's asked for, and they're
    /// shown from it. Wanted to begin with.
    pub large_off: AtomicBool,
    pub passed_on_no_h265: AtomicBool,
    /// Microseconds each picture's decoding is made to take longer, as
    /// a slow phone's does (tests only).
    #[cfg(test)]
    pub decode_delay: AtomicU64,
}

impl VideoStats {
    /// " on the graphics chip" after encoding or decoding times, for
    /// whichever the hardware's doing.
    pub fn on_chip(&self) -> (&'static str, &'static str) {
        let on = |m: &std::sync::Mutex<Option<String>>| if m.lock().map(|o| o.is_some()).unwrap_or(false) { " on the graphics chip" } else { "" };
        (on(&self.encoding_on), on(&self.decoding_on))
    }

    /// What's sent in, and what's shown: "H.264" or "H.265".
    pub fn codecs(&self) -> (&'static str, &'static str) {
        let name = |h265: &AtomicBool| if h265.load(Ordering::Relaxed) { "H.265" } else { "H.264" };
        (name(&self.sending_h265), name(&self.receiving_h265))
    }

    /// The size and pace pictures are sent at now, once any have been.
    pub fn sending(&self) -> Option<crate::video::Shape> {
        unpack(self.shape.load(Ordering::Relaxed))
    }

    /// The shape that would be sent if the camera gave big enough pictures
    /// (see `video::VideoEncoder::wanted`).
    pub fn wanted(&self) -> Option<crate::video::Shape> {
        unpack(self.wanted.load(Ordering::Relaxed))
    }

    /// Notes the boxes the other side's pictures are shown in, large and
    /// small (a strip's tile), in this side's own pixels (0 by 0: not
    /// known; the small, none: nobody's shown small), to tell them: each
    /// is sent no bigger than its box needs. The front end's job.
    pub fn show_in(&self, large: (usize, usize), small: (usize, usize)) {
        self.shown_in.store(Boxes::new(large, small).pack(), Ordering::Relaxed);
    }

    fn shown_in(&self) -> Boxes {
        Boxes::unpack(self.shown_in.load(Ordering::Relaxed))
    }

    /// The large box noted (see `show_in`): (0, 0), none yet.
    pub fn shown_large_in(&self) -> (usize, usize) {
        let large = self.shown_in().large;
        (large.0 as usize, large.1 as usize)
    }

    /// Notes the biggest boxes their pictures are shown in by those we
    /// pass them on to (0 by 0: none).
    pub fn show_passed_on_in(&self, large: (usize, usize), small: (usize, usize)) {
        self.passed_on_in.store(Boxes::new(large, small).pack(), Ordering::Relaxed);
    }

    /// The boxes the other side's told their pictures are shown in: ours,
    /// each grown to take in those we pass them on to. Ours not known:
    /// none.
    fn shown_in_for_all(&self) -> Boxes {
        let (ours, passed) = (self.shown_in(), Boxes::unpack(self.passed_on_in.load(Ordering::Relaxed)));
        if ours.large.0 == 0 || ours.large.1 == 0 {
            return ours;
        }
        let grown = |a: (u16, u16), b: (u16, u16)| (a.0.max(b.0), a.1.max(b.1));
        Boxes { large: grown(ours.large, passed.large), small: grown(ours.small, passed.small) }
    }

    /// Notes what's being sent and what's wanted — the sender's job (see
    /// `send_video`); public for front ends' tests.
    pub fn set_sending(&self, shape: crate::video::Shape, wanted: crate::video::Shape) {
        self.shape.store(pack(shape), Ordering::Relaxed);
        self.wanted.store(pack(wanted), Ordering::Relaxed);
    }
}

/// The boxes someone's pictures are shown in, large and small (a strip's
/// tile), in the shower's own pixels (see `CallSignal::ShownIn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Boxes {
    large: (u16, u16),
    small: (u16, u16),
}

impl Boxes {
    fn new(large: (usize, usize), small: (usize, usize)) -> Boxes {
        let side = |n: usize| n.min(u16::MAX as usize) as u16;
        Boxes { large: (side(large.0), side(large.1)), small: (side(small.0), side(small.1)) }
    }

    /// In one number, for an atomic.
    fn pack(self) -> u64 {
        (self.large.0 as u64) << 48 | (self.large.1 as u64) << 32 | (self.small.0 as u64) << 16 | self.small.1 as u64
    }

    fn unpack(packed: u64) -> Boxes {
        Boxes { large: ((packed >> 48) as u16, (packed >> 32) as u16), small: ((packed >> 16) as u16, packed as u16) }
    }

    fn signal(self) -> CallSignal {
        CallSignal::ShownIn { width: self.large.0, height: self.large.1, small_width: self.small.0, small_height: self.small.1 }
    }

    /// A small box to send to: one that's known.
    fn wants_small(self) -> bool {
        self.small.0 != 0 && self.small.1 != 0
    }
}

/// A shape in one number, for an atomic.
fn pack(shape: crate::video::Shape) -> u64 {
    (shape.width as u64) << 32 | (shape.height as u64) << 16 | shape.fps as u64
}

fn unpack(packed: u64) -> Option<crate::video::Shape> {
    (packed != 0).then_some(crate::video::Shape { width: (packed >> 32) as usize, height: (packed >> 16 & 0xffff) as usize, fps: (packed & 0xffff) as u32 })
}

/// Drives one live call to completion: encodes+sends whatever
/// `FRAME_SAMPLES`-sized chunks arrive on `mic_rx` (captured audio, fed
/// by `swarm-cli`'s `audio` module) and decodes+forwards whatever
/// arrives from the peer onto `speaker_tx`, until either side hangs up
/// (`hangup_rx` fired locally, or a `Hangup` from the peer) or the
/// connection goes away. Run identically by caller and callee once a
/// call is accepted. With `video`, pictures go both ways too — a video
/// failure ends the video, never the call.
///
/// `mic_rx` closing counts as a local hangup — so a caller with no
/// microphone has to keep its sender alive (see `audio::start`), not
/// drop it.
pub async fn run(
    link: CallLink,
    mut mic_rx: mpsc::UnboundedReceiver<Vec<f32>>,
    speaker_tx: mpsc::UnboundedSender<Vec<f32>>,
    mut hangup_rx: oneshot::Receiver<()>,
    video: Option<VideoIo>,
) -> Result<()> {
    let CallLink { commands, mut inbound, budget, relays, pictures, vias, later, .. } = link;
    let keyframe_wanted: Arc<[AtomicBool; 2]> = Arc::default();
    let their_box = Arc::new(AtomicU64::new(0));
    let they_decode_h265 = Arc::new(AtomicBool::new(false));
    let h265_failed = Arc::new(AtomicBool::new(false));
    let video_side = {
        let (commands, budget, keyframe_wanted, they_decode_h265, h265_failed, their_box) =
            (commands.clone(), budget.clone(), keyframe_wanted.clone(), they_decode_h265.clone(), h265_failed.clone(), their_box.clone());
        move |io: VideoIo| -> VideoSide {
            budget.video_on.store(true, Ordering::Relaxed);
            let h265 = H265 { theirs: they_decode_h265.clone(), failed_here: h265_failed.clone() };
            let sending = Stops(tokio::spawn({
                let (commands, budget, camera, stats, keyframe_wanted, their_box) =
                    (commands.clone(), budget.clone(), io.camera.clone(), io.stats.clone(), keyframe_wanted.clone(), their_box.clone());
                async move {
                    let _ = send_video(commands, budget, camera, stats, keyframe_wanted, h265, their_box).await;
                }
            }));
            let shower = Shower::start(io.display, io.small, io.stats.clone(), commands.clone(), h265_failed.clone());
            VideoSide { _sending: sending, shower, stats: io.stats }
        }
    };
    let mut video = video.map(&video_side);
    let (mut given, theirs) = match later {
        Some(VideoLater { given, theirs }) => (Some(given), Some(theirs)),
        None => (None, None),
    };

    let send_commands = commands.clone();
    let send_budget = budget.clone();
    let mut send_task = tokio::spawn(async move {
        let mut encoder = AudioEncoder::new()?;
        loop {
            tokio::select! {
                _ = &mut hangup_rx => break,
                maybe_pcm = mic_rx.recv() => {
                    let Some(pcm) = maybe_pcm else { break };
                    let opus = encoder.encode(&pcm, send_budget.audio.load(Ordering::Relaxed))?;
                    if send_commands.send(CallCommand::Signal(CallSignal::Audio(opus))).is_err() {
                        return Ok(());
                    }
                }
            }
        }
        let _ = send_commands.send(CallCommand::Signal(CallSignal::Hangup));
        Ok::<(), anyhow::Error>(())
    });

    let mut recv_task = tokio::spawn(async move {
        let mut decoder = AudioDecoder::new()?;
        let mut theirs_told = false;
        loop {
            let signal = tokio::select! {
                signal = inbound.recv() => signal,
                Some(io) = async { given.as_mut()?.recv().await }, if video.is_none() => {
                    video = Some(video_side(io));
                    continue;
                }
            };
            if let (Some(pictures), Some(signal @ (CallSignal::Video(_) | CallSignal::CameraOff | CallSignal::Screen(_)))) = (&pictures, &signal) {
                let _ = pictures.send(signal.clone());
            }
            match signal {
                Some(CallSignal::Via { id, signal }) => {
                    if let Some(vias) = &vias {
                        let _ = vias.send((id, *signal));
                    }
                }
                Some(CallSignal::Audio(opus)) => {
                    if let Ok(pcm) = decoder.decode(&opus) {
                        let _ = speaker_tx.send(pcm);
                    }
                }
                Some(CallSignal::Video(piece)) => match &video {
                    Some(video) => {
                        let _ = video.shower.piece(piece);
                    }
                    None => {
                        if let Some(theirs) = theirs.as_ref().filter(|_| !theirs_told) {
                            theirs_told = true;
                            let _ = theirs.send(());
                        }
                    }
                },
                Some(CallSignal::CameraOff) => {
                    if let Some(video) = &video {
                        video.shower.camera_off();
                    }
                }
                Some(CallSignal::Screen(screen)) => {
                    if let Some(video) = &video {
                        video.stats.their_screen.store(screen, Ordering::Relaxed);
                    }
                }
                Some(CallSignal::KeyframeRequest { small }) => keyframe_wanted[small as usize].store(true, Ordering::Relaxed),
                Some(CallSignal::Decodes { h265 }) => they_decode_h265.store(h265, Ordering::Relaxed),
                Some(CallSignal::ShownIn { width, height, small_width, small_height }) => {
                    their_box.store(Boxes { large: (width, height), small: (small_width, small_height) }.pack(), Ordering::Relaxed)
                }
                Some(CallSignal::Relayed { id, opus }) => {
                    if let Some(relays) = &relays {
                        let _ = relays.send((id, opus));
                    }
                }
                Some(CallSignal::Hangup) | Some(CallSignal::Reject) | None => return Ok(()),
                Some(_) => {}
            }
        }
    });

    let result: Result<()> = tokio::select! {
        r = &mut send_task => r.unwrap_or(Ok(())),
        r = &mut recv_task => r.unwrap_or(Ok(())),
    };
    send_task.abort();
    recv_task.abort();
    budget.video_on.store(false, Ordering::Relaxed);
    result
}

/// A call's video while it has some: our pictures being sent (stopped
/// with it), and where theirs go to be shown.
struct VideoSide {
    _sending: Stops,
    shower: Shower,
    stats: Arc<VideoStats>,
}

/// Where someone's pictures are decoded and shown: their large on a
/// thread of its own, their small on another (see `show_video`).
struct Shower {
    large: std::sync::mpsc::Sender<ToShow>,
    small: std::sync::mpsc::Sender<ToShow>,
}

impl Shower {
    fn start(
        display: watch::Sender<Option<Arc<Picture>>>,
        small: watch::Sender<Option<Arc<Picture>>>,
        stats: Arc<VideoStats>,
        commands: mpsc::UnboundedSender<CallCommand>,
        h265_failed: Arc<AtomicBool>,
    ) -> Shower {
        let (large_tx, large_rx) = std::sync::mpsc::channel();
        let (small_tx, small_rx) = std::sync::mpsc::channel();
        let (stats_small, commands_small, h265_failed_small) = (stats.clone(), commands.clone(), h265_failed.clone());
        std::thread::spawn(move || show_video(large_rx, display, Stream::Large, Some(stats), commands, h265_failed));
        std::thread::spawn(move || show_video(small_rx, small, Stream::Small, Some(stats_small), commands_small, h265_failed_small));
        Shower { large: large_tx, small: small_tx }
    }

    /// Whether the threads are still there to take what comes.
    fn piece(&self, piece: VideoPiece) -> bool {
        let to = if piece.small { &self.small } else { &self.large };
        to.send(ToShow::Piece(piece)).is_ok()
    }

    fn camera_off(&self) {
        let _ = self.large.send(ToShow::CameraOff);
        let _ = self.small.send(ToShow::CameraOff);
    }
}

/// Which of someone's pictures a `show_video` shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stream {
    Large,
    Small,
}

/// What the receiving side hands its video thread (`show_video`), in the
/// order it came.
enum ToShow {
    Piece(VideoPiece),
    CameraOff,
}

/// Pieces of more frames than this waiting to be decoded, and decoding's
/// fallen behind: what's waiting is passed over, back to its newest full
/// frame.
const DECODING_BEHIND: usize = 2;

/// How often a full frame's asked for again while the large pictures,
/// wanted once more, wait for one (see `VideoStats::large_off`).
const ASK_KEY_AGAIN: Duration = Duration::from_millis(500);

/// Decodes the other side's pictures of one `stream` as they come, and
/// shows them. When it falls behind (a phone given more than it decodes
/// in time), what's waiting is passed over, from its newest full frame
/// on, or, with none, all of it (the decoder then finds a frame missing,
/// and asks for a full one): a picture late is a picture dropped, never
/// a growing delay. The large, while they're not wanted (see
/// `VideoStats::large_off`), are passed over, and shown again from a full
/// frame. Ends when the call does. The large are what's counted.
fn show_video(
    pieces: std::sync::mpsc::Receiver<ToShow>,
    display: watch::Sender<Option<Arc<Picture>>>,
    stream: Stream,
    stats: Option<Arc<VideoStats>>,
    commands: mpsc::UnboundedSender<CallCommand>,
    h265_failed: Arc<AtomicBool>,
) {
    let small = stream == Stream::Small;
    let video_stats = stats.as_ref().filter(|_| !small);
    let mut video_decoder: Option<VideoDecoder> = None;
    let mut until_key: Option<Option<std::time::Instant>> = None;
    let ask_key = || {
        let _ = commands.send(CallCommand::Signal(CallSignal::KeyframeRequest { small }));
        if let Some(stats) = video_stats {
            stats.keyframes_asked.fetch_add(1, Ordering::Relaxed);
        }
    };
    while let Ok(first) = pieces.recv() {
        let mut waiting: Vec<ToShow> = std::iter::once(first).chain(pieces.try_iter()).collect();
        let mut frames: Vec<u32> = waiting.iter().filter_map(|w| if let ToShow::Piece(p) = w { Some(p.frame) } else { None }).collect();
        frames.dedup();
        if frames.len() > DECODING_BEHIND {
            let newest_key = waiting.iter().rev().find_map(|w| if let ToShow::Piece(p) = w { p.key.then_some(p.frame) } else { None });
            let from = newest_key.and_then(|key| waiting.iter().position(|w| matches!(w, ToShow::Piece(p) if p.frame == key))).unwrap_or(waiting.len());
            let mut at = 0;
            waiting.retain(|w| {
                at += 1;
                at > from || !matches!(w, ToShow::Piece(_))
            });
        }
        for item in waiting {
            match item {
                ToShow::Piece(piece) => {
                    if !small && stats.as_ref().is_some_and(|s| s.large_off.load(Ordering::Relaxed)) {
                        until_key = Some(None);
                        display.send_if_modified(|shown| shown.take().is_some());
                        continue;
                    }
                    if let Some(asked) = until_key {
                        if !(piece.key && piece.index == 0) {
                            if asked.is_none_or(|at| at.elapsed() >= ASK_KEY_AGAIN) {
                                until_key = Some(Some(std::time::Instant::now()));
                                ask_key();
                            }
                            continue;
                        }
                        until_key = None;
                    }
                    if video_decoder.is_none() {
                        video_decoder = VideoDecoder::new().ok();
                    }
                    let Some(video_decoder) = &mut video_decoder else { continue };
                    let started = std::time::Instant::now();
                    let received = video_decoder.push(piece);
                    #[cfg(test)]
                    if received.picture.is_some()
                        && let Some(stats) = &video_stats
                    {
                        std::thread::sleep(Duration::from_micros(stats.decode_delay.load(Ordering::Relaxed)));
                    }
                    if received.picture.is_some()
                        && let Some(stats) = &video_stats {
                            let took = started.elapsed().as_micros() as u64;
                            let was = stats.decode_micros.load(Ordering::Relaxed);
                            stats.decode_micros.store(if was == 0 { took } else { (was * 24 + took) / 25 }, Ordering::Relaxed);
                            if let Ok(mut on) = stats.decoding_on.lock() {
                                *on = video_decoder.hardware();
                            }
                        }
                    if received.no_h265 && !video_decoder.decodes_h265() && !h265_failed.swap(true, Ordering::Relaxed) {
                        let _ = commands.send(CallCommand::Signal(CallSignal::Decodes { h265: false }));
                    }
                    if let Some(stats) = &video_stats {
                        stats.receiving_h265.store(video_decoder.codec() == crate::video::Codec::H265, Ordering::Relaxed);
                    }
                    if received.want_keyframe {
                        ask_key();
                    }
                    if let Some(picture) = received.picture {
                        let _ = display.send(Some(video_decoder.share(picture)));
                        if let Some(stats) = &video_stats {
                            stats.frames_shown.fetch_add(1, Ordering::Relaxed);
                        }
                        if let Some(stats) = &stats {
                            stats.their_camera_off.store(false, Ordering::Relaxed);
                        }
                    }
                }
                ToShow::CameraOff => {
                    if let Some(stats) = &stats {
                        stats.their_camera_off.store(true, Ordering::Relaxed);
                        stats.their_screen.store(false, Ordering::Relaxed);
                    }
                    display.send_if_modified(|shown| shown.take().is_some());
                }
            }
        }
    }
}

/// Shows a conference member's pictures passed on to us by another
/// member (`CallSignal::Via`) as a link's own are (`show_video`): what's
/// handed in — their `Video`, `CameraOff` and `Screen` — is shown in
/// `display` (large) and `small`, counted in `stats`. What's to be asked
/// of them comes out of the other end, for the one passing them on to
/// ask them: a full picture (`KeyframeRequest`), H.265 not decoded here
/// (`Decodes`), and the boxes they're shown in (`ShownIn`, as `stats` has
/// them: when they change, and every `DECODES_EVERY`). Ends when what's
/// handed in stops.
pub fn show_relayed(
    display: watch::Sender<Option<Arc<Picture>>>,
    small: watch::Sender<Option<Arc<Picture>>>,
    stats: Arc<VideoStats>,
) -> (mpsc::UnboundedSender<CallSignal>, mpsc::UnboundedReceiver<CallSignal>) {
    let (to_show, mut handed) = mpsc::unbounded_channel::<CallSignal>();
    let (back_tx, back) = mpsc::unbounded_channel();
    let (commands_tx, mut commands) = mpsc::unbounded_channel::<CallCommand>();
    let shower = Shower::start(display, small, stats.clone(), commands_tx, Arc::new(AtomicBool::new(false)));
    tokio::spawn(async move {
        let mut told: Option<(Boxes, tokio::time::Instant)> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                signal = handed.recv() => match signal {
                    Some(CallSignal::Video(piece)) => {
                        if !shower.piece(piece) {
                            break;
                        }
                    }
                    Some(CallSignal::CameraOff) => shower.camera_off(),
                    Some(CallSignal::Screen(screen)) => stats.their_screen.store(screen, Ordering::Relaxed),
                    Some(_) => {}
                    None => break,
                },
                Some(CallCommand::Signal(signal)) = commands.recv() => {
                    if back_tx.send(signal).is_err() {
                        break;
                    }
                }
                _ = tick.tick() => {
                    let shown_in = stats.shown_in();
                    if told.is_none_or(|(was, at)| was != shown_in || at.elapsed() >= DECODES_EVERY) {
                        told = Some((shown_in, tokio::time::Instant::now()));
                        if back_tx.send(shown_in.signal()).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    (to_show, back)
}

/// How much faster than video's bitrate a frame's pieces are sent.
const PACE: f64 = 1.5;

/// Windows' timers keep to 15.6 ms unless asked for finer, and a tokio
/// sleep with them: pictures due every 40 ms went at 31 or 47, unevenly,
/// and the other side's picture stuttered. Asked for 1 ms while it's
/// held, as players do while they play.
struct FineTimer;

impl FineTimer {
    fn new() -> FineTimer {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        FineTimer
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

/// Whether H.265's decoded at either end of a call.
struct H265 {
    /// They've said they decode it.
    theirs: Arc<AtomicBool>,
    /// This side's decoder failed at it.
    failed_here: Arc<AtomicBool>,
}

/// Of video's bitrate, at most this share goes to the small pictures
/// (see `CallSignal::ShownIn`), the rest to the large.
const SMALL_SHARE_AT_MOST: u32 = 4;

/// Sends each new picture from the camera, at the bitrate the slots
/// have room for (or, without cover, the link carries), in pieces that
/// fit them — no more often than the shape that bitrate suits allows (see
/// `video::Shape`). While the other side shows us in a strip's tile too,
/// small pictures for it go as well, a stream of their own, given what
/// their size is worth (`video::worth_drawn`) of the bitrate, a quarter
/// at most. Nothing is sent while the camera's off, or while there's no
/// room.
async fn send_video(
    commands: mpsc::UnboundedSender<CallCommand>,
    budget: Arc<CallBudget>,
    mut camera: watch::Receiver<Option<Arc<Picture>>>,
    stats: Arc<VideoStats>,
    keyframe_wanted: Arc<[AtomicBool; 2]>,
    h265: H265,
    their_box: Arc<AtomicU64>,
) -> Result<()> {
    let _fine = FineTimer::new();
    let mut decodes_told: Option<(bool, tokio::time::Instant)> = None;
    let mut shown_told: Option<(Boxes, tokio::time::Instant)> = None;
    let mut encoder: Option<VideoEncoder> = None;
    let mut small_encoder: Option<VideoEncoder> = None;
    let mut small_next = tokio::time::Instant::now();
    let mut next = tokio::time::Instant::now();
    let mut last_sent: Option<tokio::time::Instant> = None;
    let mut last_changed: Option<tokio::time::Instant> = None;
    let mut off_told: Option<tokio::time::Instant> = None;
    let mut screen_told: Option<(bool, tokio::time::Instant)> = None;
    let (paced, waiting) = mpsc::channel(1);
    let _pacer = Stops(tokio::spawn(pace_video(commands.clone(), waiting, stats.clone())));
    loop {
        tokio::time::sleep_until(next).await;
        let fps = encoder.as_ref().map_or(crate::video::MAX_FPS, |e| e.shape().fps);
        next += Duration::from_secs(1) / fps.max(1);
        let now = tokio::time::Instant::now();
        if next < now {
            next = now;
        }
        let decodes = crate::video::decodes_h265() && !h265.failed_here.load(Ordering::Relaxed) && !stats.passed_on_no_h265.load(Ordering::Relaxed);
        if decodes_told.is_none_or(|(told, at)| told != decodes || at.elapsed() >= DECODES_EVERY) {
            decodes_told = Some((decodes, tokio::time::Instant::now()));
            if commands.send(CallCommand::Signal(CallSignal::Decodes { h265: decodes })).is_err() {
                return Ok(());
            }
        }
        let shown_in = stats.shown_in_for_all();
        if shown_told.is_none_or(|(told, at)| told != shown_in || at.elapsed() >= DECODES_EVERY) {
            shown_told = Some((shown_in, tokio::time::Instant::now()));
            if commands.send(CallCommand::Signal(shown_in.signal())).is_err() {
                return Ok(());
            }
        }
        let bitrate = budget.video_bps.load(Ordering::Relaxed) as u32;
        stats.offered.store(bitrate as u64, Ordering::Relaxed);
        let piece_size = budget.video_piece.load(Ordering::Relaxed);
        let changed = camera.has_changed().unwrap_or(false);
        if changed {
            last_changed = Some(tokio::time::Instant::now());
        }
        let fresh = changed || keyframe_wanted.iter().any(|k| k.load(Ordering::Relaxed)) || sent_again(last_sent, last_changed, stats.screen.load(Ordering::Relaxed));
        let Some(picture) = camera.borrow_and_update().clone() else {
            if off_told.is_none_or(|t| t.elapsed() >= CAMERA_OFF_EVERY) {
                off_told = Some(tokio::time::Instant::now());
                if commands.send(CallCommand::Signal(CallSignal::CameraOff)).is_err() {
                    return Ok(());
                }
            }
            continue;
        };
        if off_told.take().is_some() {
            for wanted in keyframe_wanted.iter() {
                wanted.store(true, Ordering::Relaxed);
            }
        }
        let screen = stats.screen.load(Ordering::Relaxed);
        let tell = match screen_told {
            None => screen,
            Some((told, at)) => told != screen || (screen && at.elapsed() >= CAMERA_OFF_EVERY),
        };
        if tell {
            screen_told = Some((screen, tokio::time::Instant::now()));
            if commands.send(CallCommand::Signal(CallSignal::Screen(screen))).is_err() {
                return Ok(());
            }
        }
        if bitrate == 0 || piece_size == 0 || !fresh {
            continue;
        }
        let keyframe = keyframe_wanted[0].swap(false, Ordering::Relaxed);
        let previous = encoder.take();
        let shaped = stats.clone();
        let bigger = budget.clone();
        let theirs = h265.theirs.load(Ordering::Relaxed);
        let boxes = Boxes::unpack(their_box.load(Ordering::Relaxed));
        let drawn = |(width, height): (u16, u16)| drawn_long((picture.width, picture.height), (width as usize, height as usize));
        let shown_long = drawn(boxes.large);
        let small_long = drawn(boxes.small);
        let small_rate = if boxes.wants_small() { crate::video::worth_drawn(small_long, screen).min(bitrate / SMALL_SHARE_AT_MOST) } else { 0 };
        if !boxes.wants_small() {
            small_encoder = None;
        }
        let small = if boxes.wants_small() && tokio::time::Instant::now() >= small_next {
            let fps = small_encoder.as_ref().map_or(crate::video::MAX_FPS, |e| e.shape().fps);
            small_next = (small_next + Duration::from_secs(1) / fps.max(1)).max(tokio::time::Instant::now());
            Some((small_encoder.take(), keyframe_wanted[1].swap(false, Ordering::Relaxed)))
        } else {
            None
        };
        let encoded = tokio::task::spawn_blocking(move || -> Result<(VideoEncoder, Option<VideoEncoder>, Result<Vec<VideoPiece>>)> {
            let mut enc = match previous {
                Some(encoder) => encoder,
                None => VideoEncoder::new(bitrate - small_rate)?,
            };
            enc.set_screen(screen)?;
            enc.set_h265(theirs);
            enc.fit_picture(picture.width.max(picture.height));
            enc.fit_shown(shown_long);
            enc.set_bitrate(bitrate - small_rate)?;
            if keyframe {
                enc.request_keyframe();
            }
            shaped.bitrate.store(enc.bitrate() as u64, Ordering::Relaxed);
            shaped.set_sending(enc.shape(), enc.wanted());
            bigger.video_bigger.store(enc.bigger_needs().unwrap_or(0) as usize, Ordering::Relaxed);
            let mut pieces = enc.encode_shared(&picture, piece_size);
            #[cfg(test)]
            std::thread::sleep(Duration::from_micros(tests::ENCODE_DELAY.load(Ordering::Relaxed)));
            shaped.sending_h265.store(enc.codec() == crate::video::Codec::H265, Ordering::Relaxed);
            if let Ok(mut on) = shaped.encoding_on.lock()
                && on.is_some() != enc.hardware().is_some()
            {
                *on = enc.hardware();
            }
            let small_enc = match small {
                Some((previous, keyframe)) => {
                    let mut small_enc = match previous {
                        Some(encoder) => encoder,
                        None => VideoEncoder::new(small_rate)?,
                    };
                    small_enc.set_screen(screen)?;
                    small_enc.set_h265(theirs);
                    small_enc.fit_picture(picture.width.max(picture.height));
                    small_enc.fit_shown(small_long);
                    small_enc.set_bitrate(small_rate)?;
                    if keyframe {
                        small_enc.request_keyframe();
                    }
                    if let Ok(large) = &mut pieces {
                        large.extend(small_enc.encode_shared(&picture, piece_size)?.into_iter().map(|piece| VideoPiece { small: true, ..piece }));
                    }
                    Some(small_enc)
                }
                None => None,
            };
            Ok((enc, small_enc, pieces))
        })
        .await??;
        let (enc, small_enc, pieces) = encoded;
        if let Some(took) = enc.encode_time() {
            stats.encode_micros.store(took.as_micros() as u64, Ordering::Relaxed);
        }
        encoder = Some(enc);
        if small_enc.is_some() {
            small_encoder = small_enc;
        }
        last_sent = Some(tokio::time::Instant::now());
        let pieces = pieces?;
        if pieces.first().is_some_and(|p| p.key) {
            stats.keyframes_sent.fetch_add(1, Ordering::Relaxed);
        }
        if paced.send((pieces, bitrate)).await.is_err() {
            return Ok(());
        }
    }
}

/// How long a picture shaped as `picture` has its longer side drawn in a
/// box `into` big (`image-fit: contain`): what the other side sees of it.
/// Not capped at the picture's own size, which is only the camera's now
/// (the encoder never sends bigger than it gives: `fit_picture`). 0 while
/// the box isn't known.
fn drawn_long((width, height): (usize, usize), into: (usize, usize)) -> usize {
    match into {
        (0, _) | (_, 0) => 0,
        (w, h) if width == 0 || height == 0 => w.max(h),
        (w, h) => {
            let scale = (w as f64 / width as f64).min(h as f64 / height as f64);
            (width.max(height) as f64 * scale).round() as usize
        }
    }
}

/// Whether a picture that's the same as the last one sent is sent again
/// (for `send_video`): once a second, to show the camera's still there,
/// and a screen's at its pace while it sharpens (`SCREEN_SHARPENS_FOR`
/// since it `last_changed`).
fn sent_again(last_sent: Option<tokio::time::Instant>, last_changed: Option<tokio::time::Instant>, screen: bool) -> bool {
    last_sent.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) || (screen && last_changed.is_some_and(|t| t.elapsed() < SCREEN_SHARPENS_FOR))
}

/// A task stopped with whatever holds it: `send_video`'s pacer, when
/// the call's over.
struct Stops(tokio::task::JoinHandle<()>);

impl Drop for Stops {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Sends each picture's pieces from `send_video`, paced, a little faster
/// than the bitrate, not all at once: without cover, voice shares the one
/// stream, and a full frame written in one go would hold up the voice
/// behind it for as long as the link takes to carry it. Each piece is due
/// once the ones before it would have gone at that pace; slept for only
/// when well ahead, as a timer can't wake for the millisecond or two a
/// piece takes on a fast link.
async fn pace_video(commands: mpsc::UnboundedSender<CallCommand>, mut pictures: mpsc::Receiver<(Vec<VideoPiece>, u32)>, stats: Arc<VideoStats>) {
    while let Some((pieces, bitrate)) = pictures.recv().await {
        let started = tokio::time::Instant::now();
        let mut bits_before = 0.0;
        for piece in pieces {
            let due = started + Duration::from_secs_f64(bits_before / (bitrate as f64 * PACE));
            if due > tokio::time::Instant::now() + Duration::from_millis(4) {
                tokio::time::sleep_until(due).await;
            }
            let bytes = piece.data.len();
            bits_before += ((bytes + crate::video::PIECE_OVERHEAD) * 8) as f64;
            stats.bytes_sent.fetch_add(bytes as u64, Ordering::Relaxed);
            if commands.send(CallCommand::Signal(CallSignal::Video(piece))).is_err() {
                return;
            }
        }
        stats.frames_sent.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a call's said to be sending and receiving is its voice and
    /// pictures as they go on the wire, nothing that only steers it.
    #[test]
    fn a_calls_traffic_is_its_voice_and_pictures() {
        assert_eq!(CallSignal::Audio(vec![0; 80]).media_bytes(), 80);
        assert_eq!(CallSignal::Relayed { id: 1, opus: vec![0; 60] }.media_bytes(), 60);
        let piece = crate::video::VideoPiece { frame: 1, index: 0, count: 1, key: true, h265: false, small: false, data: vec![0; 1000] };
        assert_eq!(CallSignal::Video(piece.clone()).media_bytes(), 1000 + crate::video::PIECE_OVERHEAD);
        assert_eq!(CallSignal::Via { id: 2, signal: Box::new(CallSignal::Video(piece)) }.media_bytes(), 1000 + crate::video::PIECE_OVERHEAD);
        assert_eq!(CallSignal::Via { id: 2, signal: Box::new(CallSignal::KeyframeRequest { small: false }) }.media_bytes(), 0);
        assert_eq!(CallSignal::KeyframeRequest { small: true }.media_bytes(), 0);
        assert_eq!(CallSignal::Decodes { h265: true }.media_bytes(), 0);
    }

    /// Two `CallLink`s wired straight to each other, standing in for the
    /// two peers' connection tasks — whatever one side sends arrives as
    /// the other side's inbound signal.
    fn linked_pair() -> (CallLink, CallLink) {
        let budget = CallBudget::new();
        let (a_cmd_tx, a_cmd_rx) = mpsc::unbounded_channel();
        let (b_cmd_tx, b_cmd_rx) = mpsc::unbounded_channel();
        let (a_in_tx, a_in_rx) = mpsc::unbounded_channel();
        let (b_in_tx, b_in_rx) = mpsc::unbounded_channel();
        tokio::spawn(relay(a_cmd_rx, b_in_tx));
        tokio::spawn(relay(b_cmd_rx, a_in_tx));
        (CallLink::new(a_cmd_tx, a_in_rx, budget.clone()), CallLink::new(b_cmd_tx, b_in_rx, budget))
    }

    async fn relay(mut commands: mpsc::UnboundedReceiver<CallCommand>, to: mpsc::UnboundedSender<CallSignal>) {
        while let Some(command) = commands.recv().await {
            if let CallCommand::Signal(signal) = command {
                let _ = to.send(signal);
            }
        }
    }

    #[tokio::test]
    async fn a_ring_answered_on_another_device_says_so() {
        let (caller, mut callee) = linked_pair();
        caller.signal(CallSignal::Invite { video: false, at: 0 }).unwrap();
        tokio::task::yield_now().await;
        assert!(!callee.caller_gave_up());
        caller.signal(CallSignal::AnsweredElsewhere).unwrap();
        for _ in 0..50 {
            if callee.caller_gave_up() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(callee.answered_elsewhere());
    }

    #[tokio::test]
    async fn ring_gets_accepted_and_rejected_correctly() {
        let (mut caller, mut callee) = linked_pair();
        let ring_task = tokio::spawn(async move { ring(&mut caller, false).await.unwrap() });
        assert!(matches!(callee.inbound.recv().await, Some(CallSignal::Invite { video: false, at }) if at > 0));
        callee.accept(false).unwrap();
        assert_eq!(ring_task.await.unwrap(), Some(false));

        let (mut caller, mut callee) = linked_pair();
        let ring_task = tokio::spawn(async move { ring(&mut caller, false).await.unwrap() });
        assert!(matches!(callee.inbound.recv().await, Some(CallSignal::Invite { video: false, at }) if at > 0));
        callee.reject().unwrap();
        assert_eq!(ring_task.await.unwrap(), None);

        let (mut caller, mut callee) = linked_pair();
        let ring_task = tokio::spawn(async move { ring(&mut caller, true).await.unwrap() });
        assert!(matches!(callee.inbound.recv().await, Some(CallSignal::Invite { video: true, at }) if at > 0));
        callee.accept(false).unwrap();
        assert_eq!(ring_task.await.unwrap(), Some(false));
    }

    #[tokio::test]
    async fn a_callee_notices_the_caller_giving_up() {
        let (caller, mut callee) = linked_pair();
        caller.send(CallSignal::Invite { video: false, at: 0 }).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!callee.caller_gave_up(), "the invite alone isn't giving up");
        caller.hang_up().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(callee.caller_gave_up());
    }

    /// A link slower than the call: the round trip grows, and the call
    /// comes down to what it carries; once the queue's gone it comes back.
    #[test]
    fn an_uncovered_call_slows_down_while_a_queue_builds() {
        let mut audio = UncoveredAudio::new();
        let base = Duration::from_millis(40);
        for _ in 0..5 {
            assert_eq!(audio.next(base), MAX_AUDIO_BYTES);
        }
        let mut rtt = base;
        for _ in 0..8 {
            rtt += Duration::from_millis(400);
            audio.next(rtt);
        }
        assert_eq!(audio.next(rtt), MIN_AUDIO_BYTES, "a queue that keeps growing brings it to the least");
        let mut last = MIN_AUDIO_BYTES;
        for _ in 0..30 {
            let now = audio.next(base);
            assert!(now >= last);
            last = now;
        }
        assert_eq!(last, MAX_AUDIO_BYTES, "with the queue gone it comes back");
    }

    /// A route that's become longer for good isn't a queue for ever.
    #[test]
    fn a_longer_route_is_taken_in_time() {
        let mut audio = UncoveredAudio::new();
        audio.next(Duration::from_millis(20));
        for _ in 0..200 {
            audio.next(Duration::from_millis(300));
        }
        assert_eq!(audio.next(Duration::from_millis(300)), MAX_AUDIO_BYTES);
    }

    /// A fast link: no queue anywhere, and everything written is sent
    /// (with QUIC's headers on top) — video climbs to its most.
    #[test]
    fn uncovered_video_climbs_on_a_link_that_keeps_up() {
        let mut video = UncoveredVideo::new();
        let mut written = 0u64;
        let mut last = 0;
        for _ in 0..40 {
            written += last.max(UNCOVERED_VIDEO_START) as u64 / 8;
            let sent = written + written / 20;
            let now = video.next(Duration::ZERO, written, sent);
            assert!(now >= last);
            last = now;
        }
        assert_eq!(last, UNCOVERED_VIDEO_MAX);
    }

    /// QUIC sending less than is written — a link slower than the video
    /// — cuts it at once, though the round trip shows nothing; it holds
    /// a while once the queue's gone, then climbs again.
    #[test]
    fn uncovered_video_comes_down_to_a_link_slower_than_it() {
        let mut video = UncoveredVideo::new();
        let (mut written, mut sent) = (0u64, 0u64);
        video.next(Duration::ZERO, written, sent);
        let mut bps = UNCOVERED_VIDEO_START;
        let mut seen = Vec::new();
        for _ in 0..40 {
            written += bps as u64 / 8;
            sent = (sent + 50_000).min(written + 1000);
            bps = video.next(Duration::ZERO, written, sent);
            seen.push(bps);
        }
        assert!(seen[1] < 400_000, "came down below what the link carries at once: {seen:?}");
        let settled = &seen[10..];
        assert!(settled.iter().all(|&b| (240_000..=480_000).contains(&b)), "{settled:?}");
        let mut video = UncoveredVideo::new();
        let (mut written, mut sent) = (0u64, 0u64);
        let mut next = |video: &mut UncoveredVideo, queued: Duration, bps: u32| {
            written += bps as u64 / 8;
            sent = written;
            video.next(queued, written, sent)
        };
        let cut = next(&mut video, Duration::from_millis(100), UNCOVERED_VIDEO_START);
        assert!(cut < UNCOVERED_VIDEO_START);
        assert_eq!(next(&mut video, Duration::ZERO, cut), cut, "held a while after a cut");
        let mut bps = cut;
        for _ in 0..45 {
            bps = next(&mut video, Duration::ZERO, bps);
        }
        assert_eq!(bps, UNCOVERED_VIDEO_MAX);
    }

    /// A still picture made small by a slow spell, the link fast again:
    /// the rate rises as far as the next bigger shape needs, though the
    /// picture takes little — it doesn't stay small for good.
    #[test]
    fn uncovered_video_can_always_try_the_next_shape_up() {
        let mut video = UncoveredVideo::new();
        let mut written = 0u64;
        video.next(Duration::ZERO, 0, 0);
        written += 400_000;
        let low = video.next(Duration::ZERO, written, 50_000);
        assert!(low < 400_000);
        video.allow_bigger(500_000);
        let mut bps = low;
        for _ in 0..30 {
            written += 75_000 / 8;
            bps = video.next(Duration::ZERO, written, written + 50_000 + 1000);
        }
        assert!(bps >= 500_000, "{bps}");
        assert!(bps < 700_000, "but not on and on: {bps}");
    }

    /// Video taking far less than it may (a still picture) proves
    /// nothing about the link: the rate isn't raised on the strength of it.
    #[test]
    fn uncovered_video_isnt_raised_past_what_it_uses() {
        let mut video = UncoveredVideo::new();
        let mut written = 0u64;
        let mut bps = 0;
        for _ in 0..20 {
            written += 100_000 / 8;
            let sent = written + 1000;
            bps = video.next(Duration::ZERO, written, sent);
        }
        assert_eq!(bps, UNCOVERED_VIDEO_START, "not raised");
    }

    /// A link that keeps up, as `UncoveredCall` sees it each second:
    /// video writes what it's allowed (`used` of it, 1.0 all), QUIC
    /// sends it all, and the round trip's `rtt` (the least 15 ms).
    struct Steady {
        call: UncoveredCall,
        written: u64,
        bps: u32,
    }

    impl Steady {
        fn new() -> Steady {
            Steady { call: UncoveredCall::new(), written: 0, bps: UNCOVERED_VIDEO_START }
        }

        fn second(&mut self, rtt_ms: u64) -> u32 {
            self.written += self.bps as u64 / 8;
            let sent = self.written + self.written / 20;
            self.bps = self.call.next(Duration::from_millis(rtt_ms), self.written, sent, 0, Duration::ZERO).1;
            self.bps
        }

        /// A second in which the link stopped (`LINK_STOPPED`): nothing
        /// went, the round trip took in the wait.
        fn stopped(&mut self) {
            self.call.link_gone();
        }
    }

    /// A phone's Wi-Fi off its channel a second in every five (scanning,
    /// dozing): each stop seen, the round trip high for a second or two
    /// after. The call's video isn't cut for it, and goes on climbing.
    #[test]
    fn uncovered_video_rides_out_a_link_that_stops_now_and_then() {
        let mut link = Steady::new();
        for _ in 0..5 {
            link.second(15);
        }
        let mut least = u32::MAX;
        for round in 0..20 {
            link.stopped();
            for rtt in [900, 250, 60, 15] {
                let bps = link.second(rtt);
                if round > 0 {
                    least = least.min(bps);
                }
            }
        }
        assert!(least >= UNCOVERED_VIDEO_START, "never cut: {least}");
        assert_eq!(link.bps, UNCOVERED_VIDEO_MAX, "climbed as ever");
    }

    /// The round trip spiking for a second now and then, with no stop
    /// seen (shorter than `LINK_STOPPED`, or between askings): one
    /// second's spike isn't a queue, for voice or video.
    #[test]
    fn a_spike_of_one_second_is_no_queue() {
        let mut link = Steady::new();
        let mut voice = MAX_AUDIO_BYTES;
        for second in 0..60 {
            link.second(if second % 3 == 2 { 300 } else { 15 });
            voice = voice.min(link.call.audio.bytes);
        }
        assert_eq!(voice, MAX_AUDIO_BYTES);
        assert_eq!(link.bps, UNCOVERED_VIDEO_MAX);
    }

    /// A queue that lasts (the round trip staying up) still cuts, a
    /// second later than before; voice too.
    #[test]
    fn a_lasting_queue_still_cuts() {
        let mut link = Steady::new();
        for _ in 0..10 {
            link.second(15);
        }
        let before = link.bps;
        assert_eq!(link.second(200), before, "one second: not yet");
        assert!(link.second(220) < before, "two: a queue");
        assert!(link.call.audio.bytes < MAX_AUDIO_BYTES);
    }

    /// After a cut, video climbs back quickly as far as the link was
    /// seen to carry, then carefully beyond it: a link slower than it was
    /// still brings it down, and it doesn't swing far over.
    #[test]
    fn uncovered_video_climbs_back_quickly_after_a_cut() {
        let mut link = Steady::new();
        while link.bps < 8_000_000 {
            link.second(15);
        }
        let at = link.bps;
        link.second(200);
        let cut = link.second(200);
        assert!(cut < at);
        let mut seconds = 0;
        while link.second(15) < at / 10 * 9 {
            seconds += 1;
        }
        assert!(seconds <= 4, "back near it in {seconds} s");
        let mut video = UncoveredVideo::new();
        video.bps = 8_000_000;
        video.good = 8_000_000;
        let (mut written, mut sent) = (0u64, 0u64);
        video.next(Duration::ZERO, 0, 0);
        let mut seen = Vec::new();
        let mut bps = video.bps;
        for _ in 0..40 {
            written += bps as u64 / 8;
            sent = (sent + 250_000).min(written + 1000);
            bps = video.next(Duration::ZERO, written, sent);
            seen.push(bps);
        }
        let settled = &seen[10..];
        assert!(settled.iter().all(|&b| (1_200_000..=2_400_000).contains(&b)), "{settled:?}");
    }

    /// Backed up in a second that lost a lot to resending (Wi-Fi's bursts
    /// of loss): cut by half at most, not to the little QUIC carried in
    /// it; backed up in a clean second, a slower link, to what it carried.
    #[test]
    fn a_lossy_second_cuts_by_half_at_most() {
        for lossy in [true, false] {
            let mut call = UncoveredCall::new();
            call.video.bps = 4_000_000;
            call.video.starting = false;
            let rtt = Duration::from_millis(15);
            call.lossy(0, 1000);
            call.next(rtt, 0, 0, 0, Duration::ZERO);
            call.lossy(if lossy { 50 } else { 0 }, 2000);
            let (_, bps) = call.next(rtt, 500_000, 40_000, 0, Duration::ZERO);
            if lossy {
                assert_eq!(bps, 2_000_000);
            } else {
                assert!(bps <= 320_000, "{bps}");
            }
        }
    }

    /// Writes to QUIC waiting (its window a few packets, on a lossy
    /// link): a queue before QUIC, which its own backlog never shows —
    /// cut to a little under what QUIC sent.
    #[test]
    fn writes_waiting_on_quic_cut_video() {
        let mut call = UncoveredCall::new();
        let rtt = Duration::from_millis(15);
        let (_, bps) = call.next(rtt, 0, 0, 0, Duration::from_secs(3));
        assert_eq!(bps, UNCOVERED_VIDEO_START);
        let (_, bps) = call.next(rtt, 125_000, 50_000, 0, Duration::from_millis(600));
        assert!(bps <= 400_000, "{bps}");
        let mut call = UncoveredCall::new();
        call.next(rtt, 0, 0, 0, Duration::ZERO);
        let (_, bps) = call.next(rtt, 125_000, 131_000, 0, Duration::from_millis(40));
        assert!(bps >= UNCOVERED_VIDEO_START, "{bps}");
    }

    /// Packets lost past `LOSSY` since last asked: a lossy link.
    #[test]
    fn lossy_is_the_share_lost_since_last_asked() {
        let mut call = UncoveredCall::new();
        assert!(!call.lossy(50, 1000), "nothing to compare with yet");
        assert!(!call.lossy(51, 1100), "1%");
        assert!(call.lossy(56, 1200), "5%");
        assert!(!call.lossy(56, 1200), "nothing sent since");
    }

    /// A queue of a quarter of a second or more halves video.
    #[test]
    fn a_big_queue_halves_video() {
        let mut video = UncoveredVideo::new();
        let mut written = 0;
        video.next(Duration::ZERO, 0, 0);
        written += 125_000;
        let bps = video.next(Duration::from_millis(400), written, written);
        assert_eq!(bps, UNCOVERED_VIDEO_START / 2);
    }

    /// A call starts doubling each second on a link that keeps up: at
    /// its most in a few seconds, not half a minute.
    #[test]
    fn a_call_doubles_at_the_start() {
        let mut link = Steady::new();
        let mut seconds = 0;
        while link.second(15) < UNCOVERED_VIDEO_MAX {
            seconds += 1;
            assert!(seconds < 8, "{}", link.bps);
        }
        assert_eq!(link.call.carried(), Some(UNCOVERED_VIDEO_MAX), "measured: the next call may start near it");
    }

    /// The doubling stops at the first sign of enough, and the careful
    /// climb takes over: a queue, here.
    #[test]
    fn the_doubling_stops_at_a_queue() {
        let mut link = Steady::new();
        link.second(15);
        link.second(15);
        let at = link.bps;
        link.second(200);
        link.second(220);
        for _ in 0..3 {
            link.second(15);
        }
        let mut last = link.bps;
        for _ in 0..10 {
            let now = link.second(15);
            assert!(now <= (last as u64 * 5 / 4) as u32 + 1, "no more doubling: {last} to {now}");
            last = now;
        }
        assert!(at > 0);
    }

    /// A call on a path that's carried 6 Mbit/s starts at four fifths of
    /// it; with nothing known, or little, where any starts.
    #[test]
    fn a_call_starts_near_what_the_last_carried() {
        let call = UncoveredCall::starting_at(Some(6_000_000));
        assert_eq!(call.video.bps, 4_800_000);
        assert_eq!(UncoveredCall::starting_at(None).video.bps, UNCOVERED_VIDEO_START);
        assert_eq!(UncoveredCall::starting_at(Some(500_000)).video.bps, UNCOVERED_VIDEO_START);
        let mut video = UncoveredCall::starting_at(Some(6_000_000)).video;
        video.next(Duration::ZERO, 0, 0);
        let bps = video.next(Duration::ZERO, 600_000, 125_000);
        assert!(bps <= 1_000_000, "{bps}");
    }

    #[test]
    fn encoded_frames_never_exceed_their_budget() {
        let mut encoder = AudioEncoder::new().unwrap();
        let noise: Vec<f32> = (0..FRAME_SAMPLES).map(|i| (((i * 7919) % 1000) as f32 / 500.0 - 1.0) * 0.8).collect();
        for budget in [MIN_AUDIO_BYTES, 30, 60, MAX_AUDIO_BYTES, 1000] {
            for _ in 0..20 {
                let frame = encoder.encode(&noise, budget).unwrap();
                assert!(frame.len() <= budget.clamp(MIN_AUDIO_BYTES, MAX_AUDIO_BYTES), "budget {budget}: got {} bytes", frame.len());
            }
        }
    }

    /// A synthetic tone in on one side comes back out the other, having
    /// genuinely round-tripped through Opus encode/decode — only the
    /// real mic/speaker device I/O (`swarm-cli`'s `audio`) and the
    /// connection task's transport are left out.
    #[tokio::test]
    async fn run_carries_real_audio_both_ways_and_hangup_ends_both() {
        let (a, b) = linked_pair();
        let tone: Vec<f32> = (0..FRAME_SAMPLES).map(|i| ((i as f32) * 0.05).sin() * 0.5).collect();

        let (a_mic_tx, a_mic_rx) = mpsc::unbounded_channel();
        let (a_speaker_tx, mut a_speaker_rx) = mpsc::unbounded_channel();
        let (a_hangup_tx, a_hangup_rx) = oneshot::channel();
        let (b_mic_tx, b_mic_rx) = mpsc::unbounded_channel();
        let (b_speaker_tx, mut b_speaker_rx) = mpsc::unbounded_channel();
        let (_b_hangup_tx, b_hangup_rx) = oneshot::channel();

        let a_run = tokio::spawn(run(a, a_mic_rx, a_speaker_tx, a_hangup_rx, None));
        let b_run = tokio::spawn(run(b, b_mic_rx, b_speaker_tx, b_hangup_rx, None));

        a_mic_tx.send(tone.clone()).unwrap();
        b_mic_tx.send(vec![0f32; FRAME_SAMPLES]).unwrap();
        let b_heard = tokio::time::timeout(Duration::from_secs(5), b_speaker_rx.recv()).await.unwrap().unwrap();
        let a_heard = tokio::time::timeout(Duration::from_secs(5), a_speaker_rx.recv()).await.unwrap().unwrap();
        assert_eq!(b_heard.len(), FRAME_SAMPLES);
        assert_eq!(a_heard.len(), FRAME_SAMPLES);
        let max_in = tone.iter().cloned().fold(0f32, f32::max);
        let max_out = b_heard.iter().cloned().fold(0f32, f32::max);
        assert!(max_out > max_in * 0.5, "decoded tone lost most of its amplitude: in={max_in} out={max_out}");

        a_hangup_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), a_run).await.unwrap().unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), b_run).await.unwrap().unwrap().unwrap();
        drop((a_mic_tx, b_mic_tx));
    }

    /// A packet that won't decode is skipped, not the end of the call.
    #[tokio::test]
    async fn audio_that_wont_decode_is_skipped() {
        let (a, b) = linked_pair();
        let (_b_mic_tx, b_mic_rx) = mpsc::unbounded_channel();
        let (b_speaker_tx, mut b_speaker_rx) = mpsc::unbounded_channel();
        let (_b_hangup_tx, b_hangup_rx) = oneshot::channel();
        let b_run = tokio::spawn(run(b, b_mic_rx, b_speaker_tx, b_hangup_rx, None));
        a.signal(CallSignal::Audio(Vec::new())).unwrap();
        a.signal(CallSignal::Audio(vec![0xff; 3])).unwrap();
        let good = AudioEncoder::new().unwrap().encode(&vec![0.1f32; FRAME_SAMPLES], MAX_AUDIO_BYTES).unwrap();
        a.signal(CallSignal::Audio(good)).unwrap();
        let heard = tokio::time::timeout(Duration::from_secs(5), b_speaker_rx.recv()).await.unwrap();
        assert!(heard.is_some_and(|pcm| pcm.len() == FRAME_SAMPLES), "the good one's heard");
        assert!(!b_run.is_finished(), "the call goes on");
    }

    /// An empty voice frame is one lost on the way: filled in from what
    /// came before (Opus's concealment), a frame's worth.
    #[test]
    fn a_lost_frame_is_filled_in() {
        let mut encoder = AudioEncoder::new().unwrap();
        let mut decoder = AudioDecoder::new().unwrap();
        let tone: Vec<f32> = (0..FRAME_SAMPLES).map(|i| ((i as f32) * 0.05).sin() * 0.5).collect();
        for _ in 0..5 {
            decoder.decode(&encoder.encode(&tone, MAX_AUDIO_BYTES).unwrap()).unwrap();
        }
        let filled = decoder.decode(&[]).unwrap();
        assert_eq!(filled.len(), FRAME_SAMPLES);
        assert!(filled.iter().any(|s| s.abs() > 0.01), "not silence: carried on from before");
    }

    /// In a conference, a link's call hands on what's to be passed on:
    /// the other side's pictures as they came (for whoever's asked to see
    /// them through us), and what comes inside `Via` (another's pictures
    /// passed on to us, or what's asked of them) — with or without video
    /// of its own, and still shown here as ever.
    #[tokio::test]
    async fn a_link_hands_on_their_pictures_and_whats_passed_on() {
        let (mut a, b) = linked_pair();
        let (pictures_tx, mut pictures) = mpsc::unbounded_channel();
        let (vias_tx, mut vias) = mpsc::unbounded_channel();
        a.pictures_to(pictures_tx);
        a.vias_to(vias_tx);
        let (_a_mic_tx, a_mic_rx) = mpsc::unbounded_channel();
        let (a_speaker_tx, _a_speaker_rx) = mpsc::unbounded_channel();
        let (_a_hangup_tx, a_hangup_rx) = oneshot::channel();
        let _a_run = tokio::spawn(run(a, a_mic_rx, a_speaker_tx, a_hangup_rx, None));
        let piece = crate::video::VideoPiece { frame: 1, index: 0, count: 1, key: true, h265: false, small: false, data: vec![3; 10] };
        for signal in [CallSignal::Video(piece.clone()), CallSignal::CameraOff, CallSignal::Screen(true)] {
            b.signal(signal.clone()).unwrap();
            assert_eq!(tokio::time::timeout(Duration::from_secs(5), pictures.recv()).await.unwrap(), Some(signal));
        }
        let via = |signal| CallSignal::Via { id: 3, signal: Box::new(signal) };
        b.signal(via(CallSignal::Video(piece))).unwrap();
        b.signal(via(CallSignal::KeyframeRequest { small: false })).unwrap();
        for _ in 0..2 {
            let (id, _) = tokio::time::timeout(Duration::from_secs(5), vias.recv()).await.unwrap().unwrap();
            assert_eq!(id, 3);
        }
        assert!(pictures.try_recv().is_err(), "what's passed on to us isn't handed on again");
    }

    /// A member's pictures passed on to us are shown as a link's own are:
    /// decoded into their display, what they're sending said (a screen,
    /// their camera off) — and what's asked of them goes back, for the
    /// one passing them on to ask them: a full picture when one's needed
    /// (the first that comes isn't), and the box they're shown in.
    #[tokio::test(flavor = "multi_thread")]
    async fn pictures_passed_on_to_us_are_shown_and_whats_asked_goes_back() {
        let (display, mut shown) = watch::channel(None);
        let (small, _) = watch::channel(None);
        let stats = Arc::new(VideoStats::default());
        let (to_show, mut back) = show_relayed(display, small, stats.clone());
        let picture = Picture::rgb(320, 180, (0..320 * 180).flat_map(|i| [(i % 251) as u8, 80, 200]).collect());
        let mut encoder = crate::video::VideoEncoder::new(300_000).unwrap();
        encoder.fit_picture(320);
        encoder.set_bitrate(300_000).unwrap();
        let first = encoder.encode(&picture, 1000).unwrap();
        let second = encoder.encode(&picture, 1000).unwrap();
        for piece in second {
            to_show.send(CallSignal::Video(piece)).unwrap();
        }
        let asked = async {
            loop {
                match back.recv().await {
                    Some(CallSignal::ShownIn { .. }) => {}
                    other => return other,
                }
            }
        };
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), asked).await.unwrap(), Some(CallSignal::KeyframeRequest { small: false }));
        for piece in first {
            to_show.send(CallSignal::Video(piece)).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), shown.wait_for(|p| p.is_some())).await.unwrap().unwrap();
        to_show.send(CallSignal::Screen(true)).unwrap();
        to_show.send(CallSignal::CameraOff).unwrap();
        tokio::time::timeout(Duration::from_secs(5), shown.wait_for(|p| p.is_none())).await.unwrap().unwrap();
        assert!(stats.their_camera_off.load(Ordering::Relaxed));
        stats.show_in((1920, 1080), (480, 270));
        let told = async {
            loop {
                if let Some(CallSignal::ShownIn { width: 1920, height: 1080, small_width: 480, small_height: 270 }) = back.recv().await {
                    return;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), told).await.unwrap();
    }

    /// Shown in a strip's tile as well as large, our pictures go both
    /// sizes, each a stream of its own: the small no bigger than the tile,
    /// given a quarter of the bitrate at most, the large the rest. Shown
    /// large only, none small.
    #[tokio::test(flavor = "multi_thread")]
    async fn pictures_go_small_as_well_as_large_while_theyre_shown_small_too() {
        let budget = CallBudget::new();
        budget.video_bps.store(4_000_000, Ordering::Relaxed);
        budget.video_piece.store(1200, Ordering::Relaxed);
        let picture = Arc::new(Picture::rgb(1920, 1080, (0..1920 * 1080).flat_map(|i| [(i % 251) as u8, (i / 1920 % 199) as u8, 90]).collect()));
        let (_camera_tx, camera_rx) = watch::channel(Some(picture));
        let their_box = Arc::new(AtomicU64::new(Boxes { large: (1920, 1080), small: (480, 270) }.pack()));
        let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
        let stats = Arc::new(VideoStats::default());
        let h265 = H265 { theirs: Arc::new(AtomicBool::new(false)), failed_here: Arc::new(AtomicBool::new(false)) };
        let sender = tokio::spawn(send_video(commands_tx, budget, camera_rx, stats.clone(), Arc::default(), h265, their_box.clone()));
        let mut decoders = [crate::video::VideoDecoder::new().unwrap(), crate::video::VideoDecoder::new().unwrap()];
        let mut sizes: [Option<(usize, usize)>; 2] = [None, None];
        let mut bytes = [0usize; 2];
        let started = tokio::time::Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            let Ok(Some(CallCommand::Signal(CallSignal::Video(piece)))) = tokio::time::timeout(Duration::from_secs(2), commands_rx.recv()).await else { continue };
            let small = piece.small as usize;
            bytes[small] += piece.data.len();
            if let Some(picture) = decoders[small].push(piece).picture {
                sizes[small] = Some((picture.width, picture.height));
            }
        }
        let (large, small) = (sizes[0].expect("large pictures"), sizes[1].expect("small pictures"));
        assert!(small.0 <= 480 && small.1 <= 270, "small pictures {small:?}");
        assert!(large.0 > 480, "large pictures {large:?}");
        assert!(bytes[1] * 4 <= bytes[0] + bytes[1], "small took {} of {} bytes", bytes[1], bytes[0] + bytes[1]);
        their_box.store(Boxes { large: (1920, 1080), small: (0, 0) }.pack(), Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(200)).await;
        while commands_rx.try_recv().is_ok() {}
        let started = tokio::time::Instant::now();
        let mut small_since = 0;
        while started.elapsed() < Duration::from_millis(500) {
            if let Ok(Some(CallCommand::Signal(CallSignal::Video(piece)))) = tokio::time::timeout(Duration::from_millis(500), commands_rx.recv()).await {
                small_since += piece.small as usize;
            }
        }
        sender.abort();
        assert_eq!(small_since, 0, "small pictures sent with no tile to show them");
    }

    /// Someone's large pictures, while they're not wanted, are passed
    /// over and taken down, their small still shown; wanted again, a full
    /// frame of the large is asked for, and they're shown from it, not
    /// before.
    #[tokio::test(flavor = "multi_thread")]
    async fn large_pictures_not_wanted_are_passed_over_and_shown_again_from_a_full_frame() {
        let (display, mut shown) = watch::channel(None);
        let (small_display, mut small_shown) = watch::channel(None);
        let stats = Arc::new(VideoStats::default());
        let (commands_tx, mut commands) = mpsc::unbounded_channel();
        let shower = Shower::start(display, small_display, stats.clone(), commands_tx, Arc::new(AtomicBool::new(false)));
        let picture = Picture::rgb(320, 180, (0..320 * 180).flat_map(|i| [(i % 251) as u8, 80, 200]).collect());
        let mut large = crate::video::VideoEncoder::new(300_000).unwrap();
        let mut small = crate::video::VideoEncoder::new(100_000).unwrap();
        for encoder in [&mut large, &mut small] {
            encoder.fit_picture(320);
            encoder.set_bitrate(300_000).unwrap();
        }
        let send = |encoder: &mut crate::video::VideoEncoder, is_small: bool| {
            for piece in encoder.encode(&picture, 1000).unwrap() {
                assert!(shower.piece(VideoPiece { small: is_small, ..piece }));
            }
        };
        send(&mut large, false);
        send(&mut small, true);
        tokio::time::timeout(Duration::from_secs(5), shown.wait_for(|p| p.is_some())).await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), small_shown.wait_for(|p| p.is_some())).await.unwrap().unwrap();
        stats.large_off.store(true, Ordering::Relaxed);
        send(&mut large, false);
        tokio::time::timeout(Duration::from_secs(5), shown.wait_for(|p| p.is_none())).await.unwrap().unwrap();
        small_shown.mark_unchanged();
        send(&mut small, true);
        tokio::time::timeout(Duration::from_secs(5), small_shown.changed()).await.unwrap().unwrap();
        while commands.try_recv().is_ok() {}
        stats.large_off.store(false, Ordering::Relaxed);
        send(&mut large, false);
        let asked = tokio::time::timeout(Duration::from_secs(5), commands.recv()).await.unwrap();
        assert!(matches!(asked, Some(CallCommand::Signal(CallSignal::KeyframeRequest { small: false }))));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(shown.borrow().is_none(), "shown from a frame that isn't a full one");
        large.request_keyframe();
        send(&mut large, false);
        tokio::time::timeout(Duration::from_secs(5), shown.wait_for(|p| p.is_some())).await.unwrap().unwrap();
    }

    /// Added to each picture's encoding in `send_video`, to stand in for
    /// a slow encoder (in microseconds).
    pub(super) static ENCODE_DELAY: AtomicU64 = AtomicU64::new(0);

    /// A picture that takes 20 ms to encode (4K H.265 on an AMD Radeon
    /// 820M) and whose pieces take most of the time between pictures to
    /// pace out: both fit in the 40 ms, so all 25 a second are sent.
    #[tokio::test(flavor = "multi_thread")]
    async fn slow_encoding_and_pacing_both_fit_in_a_picture() {
        let mut last = String::new();
        for _ in 0..5 {
            match encode_and_pace_once().await {
                Ok(()) => return,
                Err(why) => last = why,
            }
        }
        panic!("{last}");
    }

    async fn encode_and_pace_once() -> Result<(), String> {
        let budget = CallBudget::new();
        budget.video_bps.store(400_000, Ordering::Relaxed);
        budget.video_piece.store(100, Ordering::Relaxed);
        let fps = crate::video::Shape::for_bitrate(400_000).fps;
        let (width, height) = (640, 360);
        let noise = move |seed: u32| {
            let mut n = seed.wrapping_mul(2_654_435_761) | 1;
            let at = move |i: usize| {
                n ^= n << 13;
                n ^= n >> 17;
                n ^= n << 5;
                let (x, y, faint) = (i % width, i / width, (n & 7) as usize);
                [(x / 3 + faint) as u8, (y / 2 + faint) as u8, ((x + y) / 5 + faint) as u8]
            };
            Picture::rgb(width, height, (0..width * height).flat_map(at).collect())
        };
        let pictures: Vec<_> = (0..16).map(|seed| Arc::new(noise(seed))).collect();
        let (camera_tx, camera_rx) = watch::channel(Some(pictures[0].clone()));
        let feeder = tokio::spawn(async move {
            for picture in pictures.iter().cycle().skip(1) {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if camera_tx.send(Some(picture.clone())).is_err() {
                    break;
                }
            }
        });
        let mut encoder = crate::video::VideoEncoder::new(400_000).unwrap();
        encoder.fit_picture(width);
        encoder.set_bitrate(400_000).unwrap();
        let took = (0..8).map(|seed| {
            let started = std::time::Instant::now();
            encoder.encode(&noise(seed), 100).unwrap();
            started.elapsed()
        });
        let took = took.skip(2).max().unwrap();
        ENCODE_DELAY.store(Duration::from_millis(20).saturating_sub(took).as_micros() as u64, Ordering::Relaxed);
        let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
        let stats = Arc::new(VideoStats::default());
        let h265 = H265 { theirs: Arc::new(AtomicBool::new(false)), failed_here: Arc::new(AtomicBool::new(false)) };
        let sender = tokio::spawn(send_video(commands_tx, budget, camera_rx, stats.clone(), Arc::default(), h265, Arc::default()));
        let started = tokio::time::Instant::now();
        let (mut frames, mut bits, mut last) = (0u32, 0usize, None);
        while let Some(CallCommand::Signal(signal)) = commands_rx.recv().await {
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_millis(1500) {
                break;
            }
            if let CallSignal::Video(piece) = signal
                && elapsed >= Duration::from_millis(500)
            {
                bits += piece.data.len() * 8;
                if last != Some(piece.frame) {
                    last = Some(piece.frame);
                    frames += 1;
                }
            }
        }
        ENCODE_DELAY.store(0, Ordering::Relaxed);
        sender.abort();
        feeder.abort();
        let (per_second, kbits) = (frames as f32, bits / 1000);
        let pacing = kbits as f32 * 1000.0 / (400_000.0 * PACE as f32);
        if pacing <= 0.5 {
            return Err(format!("pacing takes {pacing:.2} of the time ({kbits} kbit/s)"));
        }
        if per_second < fps as f32 * 0.9 {
            return Err(format!("{per_second:.1} a second sent, of {fps}; encoding {} us", stats.encode_micros.load(Ordering::Relaxed)));
        }
        Ok(())
    }

    /// A screen the same as the last picture sent goes again at its pace
    /// for a while after it changed, for the chip to sharpen it; a camera
    /// only once a second, as a screen does after that while.
    #[test]
    fn a_changed_screen_is_sent_again_while_it_sharpens() {
        let ago = |ms: u64| Some(tokio::time::Instant::now() - Duration::from_millis(ms));
        assert!(sent_again(ago(40), ago(140), true), "a screen, just changed");
        assert!(!sent_again(ago(40), ago(140), false), "a camera");
        let sharpened = SCREEN_SHARPENS_FOR.as_millis() as u64 + 100;
        assert!(!sent_again(ago(40), ago(sharpened), true), "a screen, sharpened");
        assert!(sent_again(ago(1040), ago(sharpened), true) && sent_again(ago(1040), ago(sharpened), false), "a second since: either");
    }

    /// A shared screen's shape at 60 kbit/s, of a picture 320 wide (see
    /// `video::screen_shapes`): its own size, 15 a second.
    const SCREEN_SHAPE_AT_60K: (usize, usize, u32) = (320, 180, 15);

    /// A video call: A's camera pictures arrive at B as pictures, and B
    /// (with no camera) still sees them and sends nothing.
    /// Video slower to decode than it comes (a phone given a desktop's)
    /// holds up nothing else: their voice is heard at once, and their
    /// hanging up ends the call at once. Decoded in turn with the rest,
    /// pictures queued up faster than they went, and everything behind
    /// them waited longer and longer — on the phone, sound broke up and
    /// lagged, and the call didn't end when the desktop hung up.
    #[tokio::test(flavor = "multi_thread")]
    async fn video_slow_to_decode_holds_up_nothing_else() {
        let (a, b) = linked_pair();
        for budget in [&a.budget, &b.budget] {
            budget.video_bps.store(400_000, Ordering::Relaxed);
            budget.video_piece.store(1200, Ordering::Relaxed);
        }
        let picture = Arc::new(Picture::rgb(640, 360, (0..640 * 360).flat_map(|i| [(i % 251) as u8, 80, 200]).collect()));
        let (a_camera_tx, a_camera_rx) = watch::channel(Some(picture.clone()));
        tokio::spawn(async move {
            while a_camera_tx.send(Some(picture.clone())).is_ok() {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let (_b_camera_tx, b_camera_rx) = watch::channel(None);
        let (a_display_tx, _a_display_rx) = watch::channel(None);
        let (b_display_tx, mut b_display_rx) = watch::channel(None);
        let b_stats = Arc::new(VideoStats::default());
        b_stats.decode_delay.store(200_000, Ordering::Relaxed);
        let a_stats = Arc::new(VideoStats::default());
        let a_video = VideoIo { camera: a_camera_rx, display: a_display_tx, small: watch::channel(None).0, stats: a_stats.clone() };
        let b_video = VideoIo { camera: b_camera_rx, display: b_display_tx, small: watch::channel(None).0, stats: b_stats.clone() };
        let (a_mic_tx, a_mic_rx) = mpsc::unbounded_channel();
        let (_b_mic_tx, b_mic_rx) = mpsc::unbounded_channel();
        let (a_speaker_tx, _a_speaker_rx) = mpsc::unbounded_channel();
        let (b_speaker_tx, mut b_speaker_rx) = mpsc::unbounded_channel();
        let (a_hangup_tx, a_hangup_rx) = oneshot::channel();
        let (_b_hangup_tx, b_hangup_rx) = oneshot::channel();
        let a_run = tokio::spawn(run(a, a_mic_rx, a_speaker_tx, a_hangup_rx, Some(a_video)));
        let b_run = tokio::spawn(run(b, b_mic_rx, b_speaker_tx, b_hangup_rx, Some(b_video)));

        tokio::time::timeout(Duration::from_secs(5), b_display_rx.wait_for(|p| p.is_some())).await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        a_mic_tx.send(vec![0.1f32; FRAME_SAMPLES]).unwrap();
        let heard = tokio::time::timeout(Duration::from_millis(500), b_speaker_rx.recv()).await;
        assert!(heard.is_ok_and(|h| h.is_some()), "A's voice not heard within half a second");
        assert!(b_stats.frames_shown.load(Ordering::Relaxed) >= 2, "{} shown", b_stats.frames_shown.load(Ordering::Relaxed));
        a_hangup_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), a_run).await.unwrap().unwrap().unwrap();
        let ended = tokio::time::timeout(Duration::from_millis(800), b_run).await;
        assert!(ended.is_ok(), "B's call didn't end within 0.8 s of A hanging up");
    }

    /// How long a picture of our picture's shape has its longer side
    /// drawn on the other side, in a box that size (`image-fit:
    /// contain`): a 16:9 picture on a phone held upright is as wide as
    /// the screen, an upright one in a wide window as tall as it. Not
    /// capped at our picture's own size: that's the camera's now, and a
    /// camera made smaller as a call settled could then never be asked
    /// for bigger again (a phone's stayed at 720p for a 4K screen).
    #[test]
    fn a_picture_is_drawn_as_big_as_its_box_lets_it() {
        for ((w, h), into, long) in [
            ((1920, 1080), (1080, 2400), 1080),
            ((1080, 1920), (1920, 1080), 1080),
            ((3840, 2160), (1920, 1080), 1920),
            ((3840, 2160), (480, 270), 480),
            ((640, 360), (1920, 1080), 1920),
            ((1280, 720), (3840, 2160), 3840),
            ((1920, 1080), (0, 0), 0),
        ] {
            assert_eq!(drawn_long((w, h), into), long, "{w}×{h} in {into:?}");
        }
    }

    /// The other side says the box it shows our pictures in, and no
    /// bigger are sent than drawn there, whatever the link has room for:
    /// sized by bandwidth alone, a desktop sent a phone 4K it took 153 ms
    /// a picture to decode, and the phone saw about one a second.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_bigger_pictures_are_sent_than_the_other_side_shows() {
        let (a, b) = linked_pair();
        for budget in [&a.budget, &b.budget] {
            budget.video_bps.store(4_000_000, Ordering::Relaxed);
            budget.video_piece.store(1200, Ordering::Relaxed);
        }
        let picture = Arc::new(Picture::rgb(1280, 720, (0..1280 * 720).flat_map(|i| [(i % 251) as u8, 80, 200]).collect()));
        let (_a_camera_tx, a_camera_rx) = watch::channel(Some(picture));
        let (_b_camera_tx, b_camera_rx) = watch::channel(None);
        let (a_display_tx, _a_display_rx) = watch::channel(None);
        let (b_display_tx, mut b_display_rx) = watch::channel(None);
        let (a_stats, b_stats) = (Arc::new(VideoStats::default()), Arc::new(VideoStats::default()));
        let a_video = VideoIo { camera: a_camera_rx, display: a_display_tx, small: watch::channel(None).0, stats: a_stats.clone() };
        let b_video = VideoIo { camera: b_camera_rx, display: b_display_tx, small: watch::channel(None).0, stats: b_stats.clone() };
        let (_a_mic_tx, a_mic_rx) = mpsc::unbounded_channel();
        let (_b_mic_tx, b_mic_rx) = mpsc::unbounded_channel();
        let (a_speaker_tx, _a_speaker_rx) = mpsc::unbounded_channel();
        let (b_speaker_tx, _b_speaker_rx) = mpsc::unbounded_channel();
        let (a_hangup_tx, a_hangup_rx) = oneshot::channel();
        let (_b_hangup_tx, b_hangup_rx) = oneshot::channel();
        let a_run = tokio::spawn(run(a, a_mic_rx, a_speaker_tx, a_hangup_rx, Some(a_video)));
        let b_run = tokio::spawn(run(b, b_mic_rx, b_speaker_tx, b_hangup_rx, Some(b_video)));

        let width = |w: usize| move |p: &Option<Arc<Picture>>| p.as_ref().is_some_and(|p| p.width == w);
        tokio::time::timeout(Duration::from_secs(10), b_display_rx.wait_for(width(1280))).await.unwrap().unwrap();
        b_stats.show_in((480, 270), (0, 0));
        tokio::time::timeout(Duration::from_secs(10), b_display_rx.wait_for(width(480))).await.unwrap().unwrap();
        assert_eq!(a_stats.sending().map(|s| s.width), Some(480));
        assert_eq!(a_stats.wanted().map(|s| s.width), Some(480), "the camera's asked for no more");
        b_stats.show_passed_on_in((640, 360), (0, 0));
        tokio::time::timeout(Duration::from_secs(10), b_display_rx.wait_for(width(640))).await.unwrap().unwrap();
        b_stats.show_passed_on_in((0, 0), (0, 0));
        tokio::time::timeout(Duration::from_secs(10), b_display_rx.wait_for(width(480))).await.unwrap().unwrap();
        b_stats.show_in((960, 540), (0, 0));
        tokio::time::timeout(Duration::from_secs(10), b_display_rx.wait_for(width(960))).await.unwrap().unwrap();
        b_stats.show_in((2560, 1440), (0, 0));
        tokio::time::timeout(Duration::from_secs(10), b_display_rx.wait_for(width(1280))).await.unwrap().unwrap();
        let wanted = || a_stats.wanted().map(|s| s.width);
        let start = std::time::Instant::now();
        while wanted() != Some(1920) && start.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(wanted(), Some(1920), "the camera's asked for bigger");

        a_hangup_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), a_run).await.unwrap().unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), b_run).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test]
    async fn run_carries_video_from_a_camera_to_the_other_side() {
        let (a, b) = linked_pair();
        for budget in [&a.budget, &b.budget] {
            budget.video_bps.store(60_000, Ordering::Relaxed);
            budget.video_piece.store(300, Ordering::Relaxed);
        }
        let picture = Picture::rgb(320, 240, (0..320 * 240).flat_map(|i| [(i % 251) as u8, 80, 200]).collect());
        let picture = Arc::new(picture);
        let (a_camera_tx, a_camera_rx) = watch::channel(Some(picture.clone()));
        let (_b_camera_tx, b_camera_rx) = watch::channel(None);
        let (a_display_tx, _a_display_rx) = watch::channel(None);
        let (b_display_tx, mut b_display_rx) = watch::channel(None);
        let a_stats = Arc::new(VideoStats::default());
        let b_stats = Arc::new(VideoStats::default());
        let a_video = VideoIo { camera: a_camera_rx, display: a_display_tx, small: watch::channel(None).0, stats: a_stats.clone() };
        let b_video = VideoIo { camera: b_camera_rx, display: b_display_tx, small: watch::channel(None).0, stats: b_stats.clone() };

        let (_a_mic_tx, a_mic_rx) = mpsc::unbounded_channel();
        let (_b_mic_tx, b_mic_rx) = mpsc::unbounded_channel();
        let (a_speaker_tx, _a_speaker_rx) = mpsc::unbounded_channel();
        let (b_speaker_tx, _b_speaker_rx) = mpsc::unbounded_channel();
        let (a_hangup_tx, a_hangup_rx) = oneshot::channel();
        let (_b_hangup_tx, b_hangup_rx) = oneshot::channel();
        let a_run = tokio::spawn(run(a, a_mic_rx, a_speaker_tx, a_hangup_rx, Some(a_video)));
        let b_run = tokio::spawn(run(b, b_mic_rx, b_speaker_tx, b_hangup_rx, Some(b_video)));

        tokio::time::timeout(Duration::from_secs(5), b_display_rx.wait_for(|p| p.is_some())).await.unwrap().unwrap();
        let shown = b_display_rx.borrow().clone().unwrap();
        let shape = crate::video::Shape::for_bitrate(60_000);
        assert_eq!((shown.width, shown.height), (shape.width, shape.height));
        assert!(a_stats.frames_sent.load(Ordering::Relaxed) > 0);
        assert_eq!(b_stats.frames_sent.load(Ordering::Relaxed), 0, "no camera, nothing sent");
        assert!(a_stats.their_camera_off.load(Ordering::Relaxed), "B's no camera is said");

        a_camera_tx.send(None).unwrap();
        tokio::time::timeout(Duration::from_secs(5), b_display_rx.wait_for(|p| p.is_none())).await.unwrap().unwrap();
        assert!(b_stats.their_camera_off.load(Ordering::Relaxed));
        a_camera_tx.send(Some(picture)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), b_display_rx.wait_for(|p| p.is_some())).await.unwrap().unwrap();
        assert!(!b_stats.their_camera_off.load(Ordering::Relaxed));

        assert!(!b_stats.their_screen.load(Ordering::Relaxed));
        a_stats.screen.store(true, Ordering::Relaxed);
        let screen_shape = SCREEN_SHAPE_AT_60K;
        tokio::time::timeout(Duration::from_secs(5), b_display_rx.wait_for(|p| p.as_ref().is_some_and(|p| p.width == screen_shape.0))).await.unwrap().unwrap();
        assert!(b_stats.their_screen.load(Ordering::Relaxed), "B's told it's a screen");
        assert_eq!(a_stats.sending().map(|s| (s.width, s.height, s.fps)), Some(screen_shape));
        a_stats.screen.store(false, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(5), b_display_rx.wait_for(|p| p.as_ref().is_some_and(|p| p.width == shape.width))).await.unwrap().unwrap();
        assert!(!b_stats.their_screen.load(Ordering::Relaxed));

        a_hangup_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), a_run).await.unwrap().unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), b_run).await.unwrap().unwrap().unwrap();
    }
}
