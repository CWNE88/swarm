//! Cover-traffic slots: fixed-size, fixed-interval filler so a
//! connection's wire traffic looks the same whether real application
//! data is moving through it or not.
//!
//! Each slot goes out as one QUIC datagram (see `protocol`), not on the
//! QUIC stream, so QUIC never retransmits one on its own — a lost slot
//! would otherwise come back as an extra packet outside the schedule,
//! exactly the kind of burst cover exists to hide. Reliability for what
//! needs it is done here instead, *inside* the slots: three byte streams
//! (urgent — call signals, pings, small replies; bulk — messages, posts,
//! channel data; background — file data; see `Lane`) are numbered by
//! byte offset, the
//! other side acknowledges how far it has received each in every slot it
//! sends (or, if it sends none, in a small ack of its own — see
//! `protocol`), and anything not acknowledged in time is sent again in
//! later slots. Voice goes in a separate priority lane that's never
//! resent: late voice is worse than a gap.
//!
//! Deliberately byte-oriented, with no notion of `protocol`'s frames —
//! the same relationship `framing.rs` has to them, one layer further in.
//!
//! A slot body is: the sender's three acks (`u64` each: how many urgent,
//! bulk, then background stream bytes it has received from us, in
//! order), then
//! chunks, then zero padding. A chunk is `[lane: u8][len: u16]`, plus
//! `[offset: u64]` on a stream lane, then `len` bytes; a zero lane byte
//! ends the slot.

use anyhow::{bail, Result};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How often a slot is emitted. Fixed, not derived from the requested
/// rate — the rate is expressed by varying slot *size* at this constant
/// cadence instead (see `slot_payload_len_for`), since a fixed interval
/// is also what lets the slot cadence double as a liveness signal. 40ms
/// because live audio rides these slots too (see `protocol::send_call`):
/// it bounds how long a voice frame can wait, and holds exactly two 20ms
/// Opus frames.
pub const SLOT_INTERVAL: Duration = Duration::from_millis(40);

/// The three acks at the start of every slot body (see `Lane`).
const SLOT_HEADER: usize = 24;
const CHUNK_HEADER: usize = 3;
const STREAM_CHUNK_HEADER: usize = CHUNK_HEADER + 8;
const LANE_END: u8 = 0;
/// A piece of the urgent stream, at a given offset.
const LANE_URGENT: u8 = 1;
/// One whole priority item (see `queue_priority`), never split or resent.
const LANE_PRIORITY: u8 = 2;
/// A piece of the bulk stream, at a given offset.
const LANE_BULK: u8 = 3;
/// A piece of the background stream (file data), at a given offset.
const LANE_BACKGROUND: u8 = 4;

/// Which of the two reliable streams bytes go on. Both are resent until
/// acknowledged; urgent goes out ahead of bulk, so a call signal or a
/// ping never waits behind a long posts list on its way through a slow
/// cover rate — bulk is only guaranteed its share (`stream_reserve`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lane {
    Urgent,
    Bulk,
    /// File data: whatever room everything else leaves.
    Background,
}

/// Below this, a slot has no room for the header plus one stream byte —
/// a rate that computes a smaller slot than this is clamped up to it.
pub const MIN_SLOT_PAYLOAD: usize = SLOT_HEADER + STREAM_CHUNK_HEADER + 1;

/// How long sent stream bytes wait for their ack before they're sent
/// again — doubled on every miss up to `MAX_RESEND_AFTER`, and back to
/// this as soon as anything is acknowledged. Long enough for a slot to
/// cross a WAN link and the ack to ride back in one of the other side's
/// slots.
const RESEND_AFTER: Duration = Duration::from_millis(400);
const MAX_RESEND_AFTER: Duration = Duration::from_millis(3200);

/// One reliable byte stream's sending side.
struct SendStream {
    /// Bytes from `acked` onward: sent but unacknowledged, then not yet
    /// sent.
    buf: VecDeque<u8>,
    /// The stream offset of `buf[0]` — everything before it is acked.
    acked: u64,
    /// The next offset to send; `acked..next` is in flight.
    next: u64,
    /// When the oldest in-flight bytes were sent (or last re-sent).
    in_flight_since: Instant,
    resend_after: Duration,
}

impl SendStream {
    fn new() -> Self {
        SendStream { buf: VecDeque::new(), acked: 0, next: 0, in_flight_since: Instant::now(), resend_after: RESEND_AFTER }
    }

    fn on_ack(&mut self, offset: u64) {
        let end = self.acked + self.buf.len() as u64;
        if offset <= self.acked || offset > end {
            return;
        }
        self.buf.drain(..(offset - self.acked) as usize);
        self.acked = offset;
        self.next = self.next.max(offset);
        self.in_flight_since = Instant::now();
        self.resend_after = RESEND_AFTER;
    }

    /// Bytes not yet sent (after going back to resend, if it's time).
    fn unsent(&mut self, now: Instant) -> usize {
        if self.next > self.acked && now.duration_since(self.in_flight_since) >= self.resend_after {
            self.next = self.acked;
            self.resend_after = (self.resend_after * 2).min(MAX_RESEND_AFTER);
        }
        (self.acked + self.buf.len() as u64 - self.next) as usize
    }

    /// Writes the next chunk, of up to `room` bytes all told, into
    /// `body` at `pos` on `lane`; returns the new `pos`.
    fn write_chunk(&mut self, body: &mut [u8], pos: usize, room: usize, lane: u8, now: Instant) -> usize {
        let unsent = self.unsent(now);
        if unsent == 0 || room <= STREAM_CHUNK_HEADER {
            return pos;
        }
        let take = (room - STREAM_CHUNK_HEADER).min(unsent);
        let start = (self.next - self.acked) as usize;
        body[pos] = lane;
        body[pos + 1..pos + 3].copy_from_slice(&(take as u16).to_be_bytes());
        body[pos + 3..pos + 11].copy_from_slice(&self.next.to_be_bytes());
        for (i, byte) in self.buf.range(start..start + take).enumerate() {
            body[pos + STREAM_CHUNK_HEADER + i] = *byte;
        }
        if self.next == self.acked {
            self.in_flight_since = now;
        }
        self.next += take as u64;
        pos + STREAM_CHUNK_HEADER + take
    }
}

/// The outbound side of cover mode for one connection: the two reliable
/// streams, and whole priority items that go out ahead of both — so a
/// voice frame never waits behind half of a large message.
pub struct Outbound {
    urgent: SendStream,
    bulk: SendStream,
    background: SendStream,
    priority: VecDeque<(Vec<u8>, Instant)>,
    /// Video pieces: like `priority` (whole, never resent, dropped once
    /// stale), but after voice and the urgent stream — see `queue_video`.
    video: VecDeque<(Vec<u8>, Instant)>,
    /// Total bytes per slot body — always `>= MIN_SLOT_PAYLOAD`.
    slot_payload_len: usize,
}

impl Outbound {
    pub fn new(slot_payload_len: usize) -> Self {
        Outbound {
            urgent: SendStream::new(),
            bulk: SendStream::new(),
            background: SendStream::new(),
            priority: VecDeque::new(),
            video: VecDeque::new(),
            slot_payload_len: slot_payload_len.max(MIN_SLOT_PAYLOAD),
        }
    }

    /// Queues one video piece to go out whole after voice and the urgent
    /// stream, until `expires`. Size pieces with `video_room` so one fits
    /// a slot; a piece too big for any slot is dropped.
    pub fn queue_video(&mut self, item: Vec<u8>, expires: Instant) {
        if SLOT_HEADER + CHUNK_HEADER + item.len() <= self.slot_payload_len {
            self.video.push_back((item, expires));
        }
    }

    /// How many bytes of each slot are left for video, after voice
    /// (`audio_items` items of `audio_item` bytes each), a small allowance
    /// for the urgent stream, and the bulk stream's guaranteed quarter.
    pub fn video_room(&self, audio_item: usize, audio_items: usize) -> usize {
        const URGENT_ALLOWANCE: usize = 32;
        self.slot_payload_len
            .saturating_sub(SLOT_HEADER + audio_items * (audio_item + CHUNK_HEADER) + URGENT_ALLOWANCE + self.slot_payload_len / 4 + CHUNK_HEADER)
    }

    /// Queues raw bytes onto one of the streams — the caller decides what
    /// they mean and how to delimit them (see `protocol::send_frame`,
    /// which length-prefixes each frame).
    pub fn queue_bytes(&mut self, lane: Lane, bytes: &[u8]) {
        match lane {
            Lane::Urgent => self.urgent.buf.extend(bytes),
            Lane::Bulk => self.bulk.buf.extend(bytes),
            Lane::Background => self.background.buf.extend(bytes),
        }
    }

    /// Queues one item to go out whole, ahead of the streams, and only
    /// until `expires` — after that it's dropped unsent rather than sent
    /// late. An item too big for any single slot can't go out whole, so
    /// it joins the urgent stream instead, length-prefixed like
    /// everything else there.
    pub fn queue_priority(&mut self, item: Vec<u8>, expires: Instant) {
        if SLOT_HEADER + CHUNK_HEADER + item.len() > self.slot_payload_len {
            self.urgent.buf.extend((item.len() as u16).to_be_bytes());
            self.urgent.buf.extend(item);
        } else {
            self.priority.push_back((item, expires));
        }
    }

    /// Grows every slot from now on to `slot_payload_len`, keeping
    /// whatever's queued; a no-op if they're already at least that big
    /// (see `protocol::raise_cover`).
    pub fn grow_to(&mut self, slot_payload_len: usize) {
        self.slot_payload_len = self.slot_payload_len.max(slot_payload_len);
    }

    pub fn slot_payload_len(&self) -> usize {
        self.slot_payload_len
    }

    /// The other side has received our streams up to these offsets:
    /// those bytes are done with. Anything out of range is ignored.
    pub fn on_ack(&mut self, (urgent, bulk, background): (u64, u64, u64)) {
        self.urgent.on_ack(urgent);
        self.bulk.on_ack(bulk);
        self.background.on_ack(background);
    }

    /// Where the urgent stream's queued bytes end now, and how far the
    /// other side has acknowledged it (see `protocol::send_encoded`).
    pub fn urgent_end(&self) -> u64 {
        self.urgent.acked + self.urgent.buf.len() as u64
    }

    pub fn urgent_acked(&self) -> u64 {
        self.urgent.acked
    }

    /// Bytes waiting in the three streams, sent or not, unacknowledged.
    pub fn queued(&self) -> usize {
        self.urgent.buf.len() + self.bulk.buf.len() + self.background.buf.len()
    }

    /// Whether either stream has anything unsent or unacknowledged.
    pub fn stream_pending(&self) -> bool {
        !self.urgent.buf.is_empty() || !self.bulk.buf.is_empty()
    }

    /// How much of each slot the streams are guaranteed while they have
    /// anything waiting, so priority traffic can slow them but never
    /// starve them.
    fn stream_reserve(&self) -> usize {
        if self.stream_pending() { self.slot_payload_len / 4 } else { 0 }
    }

    /// The largest priority item that fits if `items_per_slot` of them
    /// share each slot alongside whatever the streams are currently
    /// owed — what a voice encoder should size its frames to (see
    /// `protocol::call_audio_budget`).
    pub fn priority_item_budget(&self, items_per_slot: usize) -> usize {
        let available = self.slot_payload_len.saturating_sub(SLOT_HEADER + self.stream_reserve());
        (available / items_per_slot.max(1)).saturating_sub(CHUNK_HEADER)
    }

    /// Builds exactly one slot body, always `slot_payload_len` bytes,
    /// carrying `acks` (how much of the other side's urgent and bulk
    /// streams we've received): priority items first (dropping any past
    /// their expiry), then the urgent stream, then the bulk stream —
    /// each going back to resend from its oldest unacknowledged byte if
    /// that's waited too long — then zeros. Bulk is guaranteed a quarter
    /// of the slot while it has anything waiting, urgent or not.
    pub fn build_slot_body(&mut self, acks: (u64, u64, u64)) -> Vec<u8> {
        self.build_slot_body_at(Instant::now(), acks)
    }

    fn build_slot_body_at(&mut self, now: Instant, (urgent_ack, bulk_ack, background_ack): (u64, u64, u64)) -> Vec<u8> {
        let mut body = vec![0u8; self.slot_payload_len];
        body[..8].copy_from_slice(&urgent_ack.to_be_bytes());
        body[8..16].copy_from_slice(&bulk_ack.to_be_bytes());
        body[16..24].copy_from_slice(&background_ack.to_be_bytes());
        let mut pos = SLOT_HEADER;

        let priority_limit = self.slot_payload_len - self.stream_reserve();
        while let Some((item, expires)) = self.priority.front() {
            if *expires < now {
                self.priority.pop_front();
                continue;
            }
            if pos + CHUNK_HEADER + item.len() > priority_limit {
                break;
            }
            let (item, _) = self.priority.pop_front().expect("just peeked");
            body[pos] = LANE_PRIORITY;
            body[pos + 1..pos + 3].copy_from_slice(&(item.len() as u16).to_be_bytes());
            body[pos + 3..pos + 3 + item.len()].copy_from_slice(&item);
            pos += CHUNK_HEADER + item.len();
        }

        let bulk_reserve = if self.bulk.buf.is_empty() { 0 } else { self.slot_payload_len / 4 };
        let urgent_room = (self.slot_payload_len - pos).saturating_sub(bulk_reserve);
        pos = self.urgent.write_chunk(&mut body, pos, urgent_room, LANE_URGENT, now);
        let video_limit = self.slot_payload_len - bulk_reserve;
        while let Some((item, expires)) = self.video.front() {
            if *expires < now {
                self.video.pop_front();
                continue;
            }
            if pos + CHUNK_HEADER + item.len() > video_limit {
                break;
            }
            let (item, _) = self.video.pop_front().expect("just peeked");
            body[pos] = LANE_PRIORITY;
            body[pos + 1..pos + 3].copy_from_slice(&(item.len() as u16).to_be_bytes());
            body[pos + 3..pos + 3 + item.len()].copy_from_slice(&item);
            pos += CHUNK_HEADER + item.len();
        }
        let bulk_room = self.slot_payload_len - pos;
        pos = self.bulk.write_chunk(&mut body, pos, bulk_room, LANE_BULK, now);
        let background_room = self.slot_payload_len - pos;
        self.background.write_chunk(&mut body, pos, background_room, LANE_BACKGROUND, now);
        body
    }
}

/// One reliable byte stream's receiving side: takes bytes in order
/// (dropping anything that skips ahead — the sender will resend from
/// where we got to) and splits them into length-prefixed items.
#[derive(Default)]
struct RecvStream {
    /// How many bytes have been received in order — our ack.
    received: u64,
    buf: Vec<u8>,
    /// Whether `received` moved since the last `take_ack_due`.
    ack_due: bool,
}

impl RecvStream {
    fn feed(&mut self, offset: u64, data: &[u8], ready: &mut VecDeque<Vec<u8>>) {
        let Some(end) = offset.checked_add(data.len() as u64) else { return };
        if offset > self.received || end <= self.received {
            return;
        }
        let fresh = &data[(self.received - offset) as usize..];
        self.received = end;
        self.ack_due = true;
        self.buf.extend_from_slice(fresh);
        loop {
            if self.buf.len() < 2 {
                break;
            }
            let len = u16::from_be_bytes([self.buf[0], self.buf[1]]) as usize;
            if self.buf.len() < 2 + len {
                break;
            }
            ready.push_back(self.buf.drain(..2 + len).skip(2).collect());
        }
    }
}

/// The inbound side of cover mode for one connection: both reliable
/// streams, and priority items as they arrive. Items from the two
/// streams can come out in a different order from the one they were
/// queued in on the other side — only order within each stream holds.
#[derive(Default)]
pub struct Inbound {
    urgent: RecvStream,
    bulk: RecvStream,
    background: RecvStream,
    ready: VecDeque<Vec<u8>>,
}

impl Inbound {
    /// Feeds one whole slot body. Returns the other side's acks (how
    /// much of *our* urgent and bulk streams they've received). Errors
    /// on a malformed slot.
    pub fn feed(&mut self, body: &[u8]) -> Result<(u64, u64, u64)> {
        if body.len() < SLOT_HEADER {
            bail!("cover slot too short");
        }
        let urgent_ack = u64::from_be_bytes(body[..8].try_into().expect("8 bytes"));
        let bulk_ack = u64::from_be_bytes(body[8..16].try_into().expect("8 bytes"));
        let background_ack = u64::from_be_bytes(body[16..24].try_into().expect("8 bytes"));
        let mut pos = SLOT_HEADER;
        while pos + CHUNK_HEADER <= body.len() {
            let lane = body[pos];
            if lane == LANE_END {
                break;
            }
            let len = u16::from_be_bytes([body[pos + 1], body[pos + 2]]) as usize;
            match lane {
                LANE_URGENT | LANE_BULK | LANE_BACKGROUND => {
                    let Some(offset) = body.get(pos + 3..pos + 11).map(|b| u64::from_be_bytes(b.try_into().expect("8 bytes"))) else {
                        bail!("cover slot stream chunk is cut short");
                    };
                    let Some(data) = body.get(pos + STREAM_CHUNK_HEADER..pos + STREAM_CHUNK_HEADER + len) else {
                        bail!("cover slot chunk claims {len} bytes but the slot ends first");
                    };
                    let stream = match lane {
                        LANE_URGENT => &mut self.urgent,
                        LANE_BULK => &mut self.bulk,
                        _ => &mut self.background,
                    };
                    stream.feed(offset, data, &mut self.ready);
                    pos += STREAM_CHUNK_HEADER + len;
                }
                LANE_PRIORITY => {
                    let Some(data) = body.get(pos + CHUNK_HEADER..pos + CHUNK_HEADER + len) else {
                        bail!("cover slot chunk claims {len} bytes but the slot ends first");
                    };
                    self.ready.push_back(data.to_vec());
                    pos += CHUNK_HEADER + len;
                }
                other => bail!("cover slot has a chunk on unknown lane {other}"),
            }
        }
        Ok((urgent_ack, bulk_ack, background_ack))
    }

    /// How many of the other side's urgent and bulk stream bytes have
    /// arrived in order — our acks.
    pub fn received(&self) -> (u64, u64, u64) {
        (self.urgent.received, self.bulk.received, self.background.received)
    }

    /// True once each time either ack has moved — when a side with no
    /// slots of its own to carry its acks has to send them separately.
    pub fn take_ack_due(&mut self) -> bool {
        std::mem::take(&mut self.urgent.ack_due) | std::mem::take(&mut self.bulk.ack_due) | std::mem::take(&mut self.background.ack_due)
    }

    /// Takes the next fully-reassembled item, if one's ready.
    pub fn pop_ready(&mut self) -> Option<Vec<u8>> {
        self.ready.pop_front()
    }
}

/// Derives a slot size from a target bitrate at the fixed
/// `SLOT_INTERVAL` cadence, accounting for `overhead_per_slot` bytes the
/// caller adds around this module's own slot body (see
/// `protocol::enable_cover`).
pub fn slot_payload_len_for(bits_per_second: u64, overhead_per_slot: usize) -> usize {
    let wire_bytes_per_interval = ((bits_per_second as f64) * SLOT_INTERVAL.as_secs_f64() / 8.0).round();
    let plaintext_len = (wire_bytes_per_interval as i64 - overhead_per_slot as i64).max(0) as usize;
    plaintext_len.max(MIN_SLOT_PAYLOAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer's slot can say anything: odd offsets (right up to the
    /// largest there is), lengths past the end, unknown lanes. Each is
    /// refused or ignored, never a panic.
    #[test]
    fn no_garbled_slot_panics() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(1977);
        let mut slot = vec![0u8; SLOT_HEADER];
        slot.push(LANE_URGENT);
        slot.extend_from_slice(&10u16.to_be_bytes());
        slot.extend_from_slice(&(u64::MAX - 3).to_be_bytes());
        slot.extend_from_slice(&[7; 10]);
        let _ = Inbound::default().feed(&slot);
        for _ in 0..50_000 {
            let mut inbound = Inbound::default();
            for _ in 0..3 {
                let len = rng.gen_range(0..120);
                let mut body: Vec<u8> = (0..len).map(|_| rng.r#gen()).collect();
                if body.len() > SLOT_HEADER && rng.gen_bool(0.7) {
                    body[SLOT_HEADER] = rng.gen_range(0..6);
                }
                let _ = inbound.feed(&body);
            }
        }
    }

    /// Carries one slot across, and its acks back.
    fn deliver(out: &mut Outbound, inb: &mut Inbound) {
        inb.feed(&out.build_slot_body((0, 0, 0))).unwrap();
        out.on_ack(inb.received());
    }

    fn stream_item(out: &mut Outbound, lane: Lane, item: &[u8]) {
        out.queue_bytes(lane, &(item.len() as u16).to_be_bytes());
        out.queue_bytes(lane, item);
    }

    #[test]
    fn a_slot_with_nothing_queued_is_the_acks_and_padding() {
        let mut out = Outbound::new(40);
        let body = out.build_slot_body((1234, 99, 7));
        assert_eq!(body.len(), 40);
        let mut inb = Inbound::default();
        assert_eq!(inb.feed(&body).unwrap(), (1234, 99, 7));
        assert!(body[24..].iter().all(|&b| b == 0));
        assert_eq!(inb.pop_ready(), None);
    }

    #[test]
    fn every_slot_is_the_same_size_whatever_is_queued() {
        let mut out = Outbound::new(60);
        assert_eq!(out.build_slot_body((0, 0, 0)).len(), 60);
        stream_item(&mut out, Lane::Bulk, &[7; 100]);
        stream_item(&mut out, Lane::Urgent, &[6; 100]);
        out.queue_priority(vec![1; 10], Instant::now() + Duration::from_secs(1));
        assert_eq!(out.build_slot_body((0, 0, 0)).len(), 60);
        assert_eq!(out.build_slot_body((0, 0, 0)).len(), 60);
    }

    #[test]
    fn an_item_bigger_than_one_slot_spans_several() {
        let mut out = Outbound::new(MIN_SLOT_PAYLOAD + 5);
        let payload = b"a rather long message that needs several slots";
        stream_item(&mut out, Lane::Bulk, payload);
        let mut inb = Inbound::default();
        let mut slots = 0;
        let got = loop {
            deliver(&mut out, &mut inb);
            slots += 1;
            if let Some(item) = inb.pop_ready() {
                break item;
            }
        };
        assert_eq!(got, payload.to_vec());
        assert!(slots > 5);
        assert!(!out.stream_pending(), "all acknowledged");
    }

    #[test]
    fn two_small_items_can_share_one_slot() {
        let mut out = Outbound::new(64);
        stream_item(&mut out, Lane::Urgent, b"one");
        stream_item(&mut out, Lane::Urgent, b"two");
        let mut inb = Inbound::default();
        deliver(&mut out, &mut inb);
        assert_eq!(inb.pop_ready(), Some(b"one".to_vec()));
        assert_eq!(inb.pop_ready(), Some(b"two".to_vec()));
        assert_eq!(inb.pop_ready(), None);
    }

    /// The reason for the two streams: a call signal mustn't wait behind
    /// a long posts list on its way through a slow cover rate.
    #[test]
    fn urgent_items_overtake_a_long_bulk_transfer_which_still_gets_through() {
        let mut out = Outbound::new(100);
        let posts = vec![3u8; 5000];
        stream_item(&mut out, Lane::Bulk, &posts);
        let mut inb = Inbound::default();
        deliver(&mut out, &mut inb);
        stream_item(&mut out, Lane::Urgent, b"invite");
        deliver(&mut out, &mut inb);
        assert_eq!(inb.pop_ready(), Some(b"invite".to_vec()), "in the very next slot");
        let got = loop {
            deliver(&mut out, &mut inb);
            if let Some(item) = inb.pop_ready() {
                break item;
            }
        };
        assert_eq!(got, posts);
    }

    #[test]
    fn a_steady_urgent_flow_cannot_starve_bulk() {
        let mut out = Outbound::new(100);
        stream_item(&mut out, Lane::Bulk, b"bulk item");
        let mut inb = Inbound::default();
        for _ in 0..2 {
            stream_item(&mut out, Lane::Urgent, &[1; 200]);
            deliver(&mut out, &mut inb);
        }
        let items: Vec<Vec<u8>> = std::iter::from_fn(|| inb.pop_ready()).collect();
        assert!(items.contains(&b"bulk item".to_vec()));
    }

    /// The reason for doing reliability here: a lost slot's stream bytes
    /// come back in later slots, never as anything extra.
    #[test]
    fn a_lost_slot_is_resent_in_later_slots_once_it_has_waited_long_enough() {
        let mut out = Outbound::new(40);
        let payload = vec![9u8; 50];
        stream_item(&mut out, Lane::Bulk, &payload);
        let mut inb = Inbound::default();
        let t0 = Instant::now();
        drop(out.build_slot_body_at(t0, (0, 0, 0)));
        let mut now = t0;
        for _ in 0..3 {
            now += SLOT_INTERVAL;
            inb.feed(&out.build_slot_body_at(now, (0, 0, 0))).unwrap();
        }
        assert_eq!(inb.received(), (0, 0, 0));
        let got = loop {
            now += SLOT_INTERVAL;
            inb.feed(&out.build_slot_body_at(now, (0, 0, 0))).unwrap();
            out.on_ack(inb.received());
            if let Some(item) = inb.pop_ready() {
                break item;
            }
            assert!(now - t0 < Duration::from_secs(5), "should have been resent by now");
        };
        assert_eq!(got, payload);
        assert!(now - t0 >= RESEND_AFTER);
    }

    #[test]
    fn duplicated_or_reordered_stream_bytes_are_taken_once() {
        let mut out = Outbound::new(40);
        stream_item(&mut out, Lane::Urgent, b"exactly once, in order");
        let first = out.build_slot_body((0, 0, 0));
        let second = out.build_slot_body((0, 0, 0));
        let mut inb = Inbound::default();
        inb.feed(&second).unwrap();
        inb.feed(&first).unwrap();
        inb.feed(&first).unwrap();
        inb.feed(&second).unwrap();
        let mut rest = out.build_slot_body((0, 0, 0));
        while inb.pop_ready().is_none() {
            inb.feed(&rest).unwrap();
            rest = out.build_slot_body((0, 0, 0));
        }
        assert_eq!(inb.pop_ready(), None);
    }

    #[test]
    fn priority_items_jump_ahead_of_a_half_sent_stream_item() {
        let mut out = Outbound::new(80);
        let big = vec![9u8; 500];
        stream_item(&mut out, Lane::Bulk, &big);
        let mut inb = Inbound::default();
        deliver(&mut out, &mut inb);
        let later = Instant::now() + Duration::from_secs(1);
        out.queue_priority(b"voice-1".to_vec(), later);
        out.queue_priority(b"voice-2".to_vec(), later);
        deliver(&mut out, &mut inb);
        assert_eq!(inb.pop_ready(), Some(b"voice-1".to_vec()));
        assert_eq!(inb.pop_ready(), Some(b"voice-2".to_vec()));
        let got = loop {
            deliver(&mut out, &mut inb);
            if let Some(item) = inb.pop_ready() {
                break item;
            }
        };
        assert_eq!(got, big);
    }

    #[test]
    fn priority_traffic_slows_the_streams_but_never_starves_them() {
        let mut out = Outbound::new(120);
        stream_item(&mut out, Lane::Bulk, b"message");
        let mut inb = Inbound::default();
        let later = Instant::now() + Duration::from_secs(1);
        for _ in 0..20 {
            out.queue_priority(vec![1; 40], later);
        }
        deliver(&mut out, &mut inb);
        deliver(&mut out, &mut inb);
        let items: Vec<Vec<u8>> = std::iter::from_fn(|| inb.pop_ready()).collect();
        assert!(items.contains(&b"message".to_vec()), "the message should get through within two slots");
    }

    #[test]
    fn expired_priority_items_are_dropped_not_sent_late() {
        let mut out = Outbound::new(64);
        let now = Instant::now();
        out.queue_priority(b"stale".to_vec(), now);
        out.queue_priority(b"fresh".to_vec(), now + Duration::from_secs(1));
        let mut inb = Inbound::default();
        inb.feed(&out.build_slot_body_at(now + Duration::from_millis(1), (0, 0, 0))).unwrap();
        assert_eq!(inb.pop_ready(), Some(b"fresh".to_vec()));
        assert_eq!(inb.pop_ready(), None);
    }

    #[test]
    fn a_priority_item_too_big_for_a_slot_still_gets_through_via_the_urgent_stream() {
        let mut out = Outbound::new(MIN_SLOT_PAYLOAD + 8);
        out.queue_priority(vec![5; 40], Instant::now() + Duration::from_secs(1));
        let mut inb = Inbound::default();
        let got = loop {
            deliver(&mut out, &mut inb);
            if let Some(item) = inb.pop_ready() {
                break item;
            }
        };
        assert_eq!(got, vec![5; 40]);
    }

    #[test]
    fn priority_budget_leaves_room_for_a_waiting_stream() {
        let mut out = Outbound::new(302);
        assert_eq!(out.priority_item_budget(2), (302 - 24) / 2 - 3);
        stream_item(&mut out, Lane::Bulk, b"waiting");
        assert_eq!(out.priority_item_budget(2), (302 - 24 - 75) / 2 - 3);
    }

    #[test]
    fn growing_keeps_whats_queued_and_never_shrinks() {
        let mut out = Outbound::new(40);
        stream_item(&mut out, Lane::Urgent, b"queued before");
        out.grow_to(64);
        out.grow_to(20);
        let body = out.build_slot_body((0, 0, 0));
        assert_eq!(body.len(), 64);
        let mut inb = Inbound::default();
        inb.feed(&body).unwrap();
        assert_eq!(inb.pop_ready(), Some(b"queued before".to_vec()));
    }

    #[test]
    fn a_malformed_slot_is_an_error_not_a_panic() {
        let mut inb = Inbound::default();
        assert!(inb.feed(&[0, 0, 0]).is_err());
        let mut body = vec![0u8; 24];
        body.extend_from_slice(&[LANE_BULK, 0, 50, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2]);
        assert!(inb.feed(&body).is_err());
        let mut body = vec![0u8; 24];
        body.extend_from_slice(&[9, 0, 1, 1]);
        assert!(inb.feed(&body).is_err());
    }

    #[test]
    fn video_goes_after_voice_and_urgent_and_never_starves_bulk() {
        let mut out = Outbound::new(600);
        let later = Instant::now() + Duration::from_secs(1);
        stream_item(&mut out, Lane::Bulk, &[4; 2000]);
        out.queue_priority(vec![1; 60], later);
        out.queue_priority(vec![1; 60], later);
        stream_item(&mut out, Lane::Urgent, b"hangup");
        let room = out.video_room(60, 2);
        assert!(room > 200, "128k-sized slots leave room for video, got {room}");
        out.queue_video(vec![2; room], later);
        out.queue_video(vec![3; room], later);
        let mut inb = Inbound::default();
        inb.feed(&out.build_slot_body((0, 0, 0))).unwrap();
        let items: Vec<Vec<u8>> = std::iter::from_fn(|| inb.pop_ready()).collect();
        assert_eq!(items.iter().filter(|i| i.len() == 60).count(), 2, "both voice frames");
        assert!(items.contains(&b"hangup".to_vec()));
        assert_eq!(items.iter().filter(|i| i.len() == room).count(), 1, "one video piece fits, the next waits");
        assert!(out.bulk.acked < out.bulk.next, "bulk still got its share");
    }

    /// File data takes only what's left: a message queued behind a long
    /// file goes in the very next slot, and the file still gets through.
    #[test]
    fn background_takes_what_everything_else_leaves() {
        let mut out = Outbound::new(200);
        let file = vec![8u8; 3000];
        stream_item(&mut out, Lane::Background, &file);
        let mut inb = Inbound::default();
        deliver(&mut out, &mut inb);
        stream_item(&mut out, Lane::Bulk, b"a message");
        deliver(&mut out, &mut inb);
        assert_eq!(inb.pop_ready(), Some(b"a message".to_vec()));
        let got = loop {
            deliver(&mut out, &mut inb);
            if let Some(item) = inb.pop_ready() {
                break item;
            }
        };
        assert_eq!(got, file);
    }

    #[test]
    fn a_very_low_bitrate_still_gets_a_usable_slot() {
        assert_eq!(slot_payload_len_for(1, 18), MIN_SLOT_PAYLOAD);
    }

    #[test]
    fn slot_size_scales_with_bitrate() {
        assert_eq!(slot_payload_len_for(64_000, 24), 320 - 24);
    }
}
