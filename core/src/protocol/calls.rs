//! Call signals on the wire, and sending them: voice and pictures go
//! by their own rules when cover's on, and what a call may send is
//! worked out from the room the connection has.

use super::*;

/// A call signal's bytes, as `Frame::Notice(Notice::Call(..))`.
pub(super) fn encode_call(signal: &CallSignal) -> Vec<u8> {
    match signal {
        CallSignal::Invite { video, at } => {
            let mut bytes = vec![TAG_CALL_INVITE, *video as u8];
            bytes.extend_from_slice(&at.to_be_bytes());
            bytes
        }
        CallSignal::Accept { video } => vec![TAG_CALL_ACCEPT, *video as u8],
        CallSignal::Video(piece) => {
            let mut w = Writer::new(TAG_CALL_VIDEO);
            w.u32(piece.frame).u16(piece.index).u16(piece.count).u8(piece.key as u8 | (piece.h265 as u8) << 1 | (piece.small as u8) << 2).rest(&piece.data);
            w.into_bytes()
        }
        CallSignal::KeyframeRequest { small } => vec![TAG_CALL_KEYFRAME, *small as u8],
        CallSignal::AnsweredElsewhere => vec![TAG_CALL_ELSEWHERE],
        CallSignal::Resume => vec![TAG_CALL_RESUME],
        CallSignal::Takeover => vec![TAG_CALL_TAKEOVER],
        CallSignal::CameraOff => vec![TAG_CALL_CAMERA_OFF],
        CallSignal::Screen(screen) => vec![TAG_CALL_SCREEN, *screen as u8],
        CallSignal::Decodes { h265 } => vec![TAG_CALL_DECODES, *h265 as u8],
        CallSignal::Via { id, signal } => {
            let mut w = Writer::new(TAG_CALL_VIA);
            w.u8(*id).rest(&Frame::Notice(Notice::Call((**signal).clone())).encode());
            w.into_bytes()
        }
        CallSignal::ShownIn { width, height, small_width, small_height } => {
            let mut w = Writer::new(TAG_CALL_SHOWN_IN);
            w.u16(*width).u16(*height).u16(*small_width).u16(*small_height);
            w.into_bytes()
        }
        CallSignal::ConferenceInvite(roster) => {
            let mut w = Writer::new(TAG_CALL_CONFERENCE);
            write_roster(&mut w, roster);
            w.into_bytes()
        }
        CallSignal::ConferenceLink(roster) => {
            let mut w = Writer::new(TAG_CALL_CONFERENCE_LINK);
            write_roster(&mut w, roster);
            w.into_bytes()
        }
        CallSignal::Roster(roster) => {
            let mut w = Writer::new(TAG_CALL_ROSTER);
            write_roster(&mut w, roster);
            w.into_bytes()
        }
        CallSignal::Hearing(keys) => {
            let mut w = Writer::new(TAG_CALL_HEARING);
            write_list(&mut w, keys, |w, k| {
                w.array(k);
            });
            w.into_bytes()
        }
        CallSignal::Forward { from, id, on } => {
            let mut w = Writer::new(TAG_CALL_FORWARD);
            w.array(from).u8(*id).bool(*on);
            w.into_bytes()
        }
        CallSignal::Control(control) => {
            let mut w = Writer::new(TAG_CALL_CONTROL);
            control.write(&mut w);
            w.into_bytes()
        }
        CallSignal::Input(input) => {
            let mut w = Writer::new(TAG_CALL_INPUT);
            input.write(&mut w);
            w.into_bytes()
        }
        CallSignal::Relayed { id, opus } => {
            let mut w = Writer::new(TAG_CALL_RELAYED);
            w.u8(*id).rest(opus);
            w.into_bytes()
        }
        CallSignal::Reject => vec![TAG_CALL_REJECT],
        CallSignal::Hangup => vec![TAG_CALL_HANGUP],
        CallSignal::Audio(opus) => {
            let mut w = Writer::new(TAG_CALL_AUDIO);
            w.rest(opus);
            w.into_bytes()
        }
    }
}

/// A call signal with tag `tag`, if it's one this build knows.
pub(super) fn decode_call(tag: u8, r: &mut Reader) -> Result<Option<Frame>> {
    Ok(Some(match tag {
        TAG_CALL_INVITE => Frame::Notice(Notice::Call(CallSignal::Invite { video: r.bool()?, at: r.u64()? })),
        TAG_CALL_ACCEPT => Frame::Notice(Notice::Call(CallSignal::Accept { video: r.bool()? })),
        TAG_CALL_VIDEO => {
            let (frame, index, count, flags) = (r.u32()?, r.u16()?, r.u16()?, r.u8()?);
            Frame::Notice(Notice::Call(CallSignal::Video(crate::video::VideoPiece { frame, index, count, key: flags & 1 != 0, h265: flags & 2 != 0, small: flags & 4 != 0, data: r.rest().to_vec() })))
        }
        TAG_CALL_KEYFRAME => Frame::Notice(Notice::Call(CallSignal::KeyframeRequest { small: r.u8()? != 0 })),
        TAG_CALL_ELSEWHERE => Frame::Notice(Notice::Call(CallSignal::AnsweredElsewhere)),
        TAG_CALL_RESUME => Frame::Notice(Notice::Call(CallSignal::Resume)),
        TAG_CALL_TAKEOVER => Frame::Notice(Notice::Call(CallSignal::Takeover)),
        TAG_CALL_CAMERA_OFF => Frame::Notice(Notice::Call(CallSignal::CameraOff)),
        TAG_CALL_CONFERENCE => Frame::Notice(Notice::Call(CallSignal::ConferenceInvite(read_roster(r)?.verified()))),
        TAG_CALL_CONFERENCE_LINK => Frame::Notice(Notice::Call(CallSignal::ConferenceLink(read_roster(r)?.verified()))),
        TAG_CALL_ROSTER => Frame::Notice(Notice::Call(CallSignal::Roster(read_roster(r)?.verified()))),
        TAG_CALL_HEARING => {
            let keys = read_list(r, |r| r.array())?;
            if keys.len() > crate::conference::MOST_MEMBERS {
                bail!("hearing too many");
            }
            Frame::Notice(Notice::Call(CallSignal::Hearing(keys)))
        }
        TAG_CALL_FORWARD => Frame::Notice(Notice::Call(CallSignal::Forward { from: r.array()?, id: r.u8()?, on: r.bool()? })),
        TAG_CALL_RELAYED => Frame::Notice(Notice::Call(CallSignal::Relayed { id: r.u8()?, opus: r.rest().to_vec() })),
        TAG_CALL_SCREEN => Frame::Notice(Notice::Call(CallSignal::Screen(r.bool()?))),
        TAG_CALL_DECODES => Frame::Notice(Notice::Call(CallSignal::Decodes { h265: r.bool()? })),
        TAG_CALL_VIA => {
            let id = r.u8()?;
            match Frame::decode(r.rest())? {
                Frame::Notice(Notice::Call(signal)) if !matches!(signal, CallSignal::Via { .. } | CallSignal::Control(_) | CallSignal::Input(_)) => Frame::Notice(Notice::Call(CallSignal::Via { id, signal: Box::new(signal) })),
                _ => bail!("not a call signal inside"),
            }
        }
        TAG_CALL_CONTROL => crate::remote::Control::read(r)?.map_or(Frame::Unknown, |c| Frame::Notice(Notice::Call(CallSignal::Control(c)))),
        TAG_CALL_INPUT => crate::remote::Input::read(r)?.map_or(Frame::Unknown, |i| Frame::Notice(Notice::Call(CallSignal::Input(i)))),
        TAG_CALL_SHOWN_IN => Frame::Notice(Notice::Call(CallSignal::ShownIn { width: r.u16()?, height: r.u16()?, small_width: r.u16()?, small_height: r.u16()? })),
        TAG_CALL_REJECT => Frame::Notice(Notice::Call(CallSignal::Reject)),
        TAG_CALL_HANGUP => Frame::Notice(Notice::Call(CallSignal::Hangup)),
        TAG_CALL_AUDIO => Frame::Notice(Notice::Call(CallSignal::Audio(r.rest().to_vec()))),
        _ => return Ok(None),
    }))
}

/// Sends one call signal (see `call`). Audio on a connection with cover
/// on goes into the slots' priority lane — out within one slot, ahead of
/// any message already part-way through, and dropped rather than sent
/// late if it somehow can't make it in time; everything else (and all
/// audio without cover) goes the same way as any other frame.
/// Sends a signal of a call with peer `contact` on a link to another of
/// the user's devices (`Notice::CallThrough`), the way `send_call` sends
/// one of a call on this connection.
pub async fn send_call_through(conn: &mut AuthenticatedConnection, contact: &[u8; 32], signal: CallSignal) -> Result<()> {
    conn.quiet();
    match (&mut conn.cover_out, &signal) {
        (Some(cover), CallSignal::Audio(_) | CallSignal::Relayed { .. }) => {
            cover.queue_priority(call_through_bytes(contact, signal), Instant::now() + AUDIO_STALE_AFTER);
            Ok(())
        }
        (Some(cover), CallSignal::Video(_)) => {
            cover.queue_video(call_through_bytes(contact, signal), Instant::now() + crate::video::PIECE_STALE_AFTER);
            Ok(())
        }
        _ => send_encoded(conn, crate::cover::Lane::Urgent, call_through_bytes(contact, signal)).await,
    }
}

/// Sending anything of a call asks how long the link's been quiet (see
/// `AuthenticatedConnection::quiet`): voice goes every 20 ms, so a stop
/// of a few hundred milliseconds is seen, wherever the call's carried
/// (`call::LINK_STOPPED`).
pub async fn send_call(conn: &mut AuthenticatedConnection, signal: CallSignal) -> Result<()> {
    conn.quiet();
    if conn.cover_out.is_none() && matches!(signal, CallSignal::Audio(_)) && send_voice_datagram(conn, &signal) {
        return Ok(());
    }
    match (&mut conn.cover_out, &signal) {
        (Some(cover), CallSignal::Audio(_) | CallSignal::Relayed { .. }) => {
            cover.queue_priority(Frame::Notice(Notice::Call(signal)).encode(), Instant::now() + AUDIO_STALE_AFTER);
            Ok(())
        }
        (Some(cover), CallSignal::Video(_)) => {
            cover.queue_video(Frame::Notice(Notice::Call(signal)).encode(), Instant::now() + crate::video::PIECE_STALE_AFTER);
            Ok(())
        }
        _ => send_frame(conn, Frame::Notice(Notice::Call(signal))).await,
    }
}

/// The first byte of a datagram that's a call's voice (`send_voice_datagram`)
/// and not a cover slot: a slot's body starts with its urgent ack, the
/// top byte of which is 0 short of 2^56 bytes on a connection.
pub(super) const VOICE_DATAGRAM: u8 = 0xFF;
/// The most lost voice frames in a row that are concealed (an empty
/// `Audio` each, which the decoder fills in): beyond, it's a gap, which
/// the speaker's buffer deals with.
const MOST_CONCEALED: u32 = 3;

/// Sends a voice frame (`CallSignal::Audio`) as a datagram of its own,
/// sealed as cover's are: `VOICE_DATAGRAM`, its number (4 bytes) and the
/// frame. Without cover, voice on the stream waits behind any video
/// queued there, and behind every lost packet being sent again; QUIC
/// puts datagrams in each packet before stream data, and never sends
/// them again — a lost one is a 20 ms gap, filled in by the decoder.
/// False if it can't go that way (no datagrams on this connection, or
/// not this big), for the stream to take it.
fn send_voice_datagram(conn: &mut AuthenticatedConnection, signal: &CallSignal) -> bool {
    if conn.next_sealer.as_ref().is_some_and(|(from, _)| Instant::now() >= *from) {
        conn.datagram_sealer = conn.next_sealer.take().map(|(_, sealer)| sealer);
    }
    let Some(sealer) = &mut conn.datagram_sealer else { return false };
    let frame = Frame::Notice(Notice::Call(signal.clone())).encode();
    let mut body = Vec::with_capacity(5 + frame.len());
    body.push(VOICE_DATAGRAM);
    body.extend(conn.voice_seq_out.to_be_bytes());
    body.extend(&frame);
    let datagram = sealer.seal(&body);
    if conn.connection.max_datagram_size().is_none_or(|most| datagram.len() > most) {
        return false;
    }
    let len = datagram.len();
    if conn.connection.send_datagram(datagram.into()).is_err() {
        return false;
    }
    conn.voice_seq_out = conn.voice_seq_out.wrapping_add(1);
    conn.written += len as u64;
    crate::tally::sent(frame[0], len);
    true
}

/// A voice datagram's body (see `send_voice_datagram`), opened: its frame
/// queued to be read, after an empty one for each lost since the last
/// (up to `MOST_CONCEALED`); one older than the newest came too late,
/// and is dropped.
pub(super) fn take_voice_datagram(conn: &mut AuthenticatedConnection, body: &[u8]) {
    let Some(seq) = body.get(1..5).map(|b| u32::from_be_bytes(b.try_into().expect("4 bytes"))) else { return };
    if let Some(newest) = conn.voice_seq_in {
        let ahead = seq.wrapping_sub(newest);
        if ahead == 0 || ahead > u32::MAX / 2 {
            return;
        }
        for _ in 0..(ahead - 1).min(MOST_CONCEALED) {
            conn.voice_ready.push_back(Frame::Notice(Notice::Call(CallSignal::Audio(Vec::new()))).encode());
        }
    }
    conn.voice_seq_in = Some(seq);
    conn.voice_ready.push_back(body[5..].to_vec());
}

/// How many slot-priority items one slot (one datagram) has to carry:
/// one per 20ms audio frame in the interval, shared across however many
/// datagrams go out per interval.
pub(super) fn audio_frames_per_slot(datagrams_per_tick: usize) -> usize {
    let per_interval = (COVER_SLOT_INTERVAL.as_millis() / crate::call::FRAME_DURATION.as_millis()).max(1) as usize;
    per_interval.div_ceil(datagrams_per_tick.max(1))
}

/// The most bytes one encoded audio frame should take right now, so a
/// call's audio fits `conn`'s cover slots alongside whatever else is
/// queued (see `cover::Outbound::priority_item_budget`) — re-read for
/// every frame, which is what lets Opus track the room available.
/// Without cover, just `call::MAX_AUDIO_BYTES`.
///
/// `voices`: how many voices share the link — ours, and any others a
/// conference passes on along it (`call::CallSignal::Relayed`) — each
/// getting an equal share of the slots' room.
pub fn call_audio_budget(conn: &AuthenticatedConnection, voices: usize) -> usize {
    match &conn.cover_out {
        Some(cover) => {
            let items = audio_frames_per_slot(conn.cover_datagrams_per_tick) * voices.max(1);
            cover.priority_item_budget(items).saturating_sub(if voices > 1 { 2 } else { 1 }).min(crate::call::MAX_AUDIO_BYTES)
        }
        None => crate::call::MAX_AUDIO_BYTES,
    }
}

/// Video's bits a second, held to three quarters of the other side's
/// receive limit (see `protocol::Notice::ReceiveLimit`), leaving the rest
/// to voice, the small pictures and QUIC's own packets.
fn under_their_limit(conn: &AuthenticatedConnection, video_bps: usize) -> usize {
    conn.their_receive_limit.map_or(video_bps, |limit| video_bps.min((limit / 4 * 3).min(usize::MAX as u64) as usize))
}

/// `SWARM_RATE_DEBUG=1`: each second of a call without cover, the round
/// trip, QUIC's backlog, window and losses, and what video's allowed and
/// why (see `call::UncoveredVideo`), for a report from a network.
fn rate_debugging() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SWARM_RATE_DEBUG").is_ok_and(|v| v == "1"))
}

/// Without cover: what a call may send, from what the link's been seen
/// to carry (see `call::UncoveredCall`) — video off once there's too
/// little room for a picture worth sending. Called about once a second.
pub fn set_uncovered_call_budget(budget: &crate::call::CallBudget, uncovered: &mut crate::call::UncoveredCall, conn: &mut AuthenticatedConnection) {
    use std::sync::atomic::Ordering::Relaxed;
    let quiet = conn.take_longest_quiet();
    let write_wait = std::mem::take(&mut conn.write_wait);
    let path = conn.connection.stats().path;
    let lossy = uncovered.lossy(path.lost_packets, path.sent_packets);
    if quiet > crate::call::LINK_STOPPED && !lossy {
        if rate_debugging() {
            eprintln!("{} rate: the link stopped for {} ms; rates held", crate::unix_now(), quiet.as_millis());
        }
        uncovered.link_gone();
        return;
    }
    let sent = conn.connection.stats().udp_tx.bytes;
    budget.round_trip.store(conn.connection.rtt().as_micros() as u64, Relaxed);
    let bigger = budget.video_bigger.load(Relaxed) as u32;
    let (audio_bytes, video_bps) = uncovered.next(conn.connection.rtt(), conn.written, sent, bigger, write_wait);
    let audio_cap = if budget.video_on.load(Relaxed) { crate::call::AUDIO_BYTES_WITH_VIDEO } else { crate::call::MAX_AUDIO_BYTES };
    budget.audio.store(audio_bytes.min(audio_cap), Relaxed);
    let video_bps = under_their_limit(conn, video_bps as usize);
    let (video_bps, piece) = if (video_bps as u32) < crate::video::MIN_BITRATE { (0, 0) } else { (video_bps, crate::video::UNCOVERED_PIECE) };
    if rate_debugging() {
        let (least, queued, backlog, why) = uncovered.explain();
        let stats = conn.connection.stats();
        eprintln!(
            "{} rate: round trip {} ms (least {}, over it {}), QUIC backlog {} kB, writes waited {} ms, window {} kB, lost {} of {} packets{}; video {} kbit/s ({why}), voice {} bytes",
            crate::unix_now(),
            conn.connection.rtt().as_millis(),
            least.as_millis(),
            queued.as_millis(),
            backlog / 1000,
            write_wait.as_millis(),
            stats.path.cwnd / 1000,
            stats.path.lost_packets,
            stats.path.sent_packets,
            if quiet > crate::call::LINK_STOPPED { format!(" (stopped {} ms, for losses)", quiet.as_millis()) } else { String::new() },
            video_bps / 1000,
            audio_bytes,
        );
    }
    budget.video_bps.store(video_bps, Relaxed);
    budget.video_piece.store(piece, Relaxed);
}

/// Brings `budget` (what a call on `conn` may send — see
/// `call::CallBudget`) up to date: voice's bytes per frame, capped while
/// video is on, and video's bitrate and largest piece, from whatever
/// room the slots have left once voice, the urgent stream and the bulk
/// stream's share are accounted for. Without cover: voice uncapped and
/// video where finding the link's rate starts (see
/// `set_uncovered_call_budget`). Called by the connection task after every slot.
pub fn update_call_budget(conn: &AuthenticatedConnection, budget: &crate::call::CallBudget) {
    use std::sync::atomic::Ordering::Relaxed;
    let video_on = budget.video_on.load(Relaxed);
    let audio_cap = if video_on { crate::call::AUDIO_BYTES_WITH_VIDEO } else { crate::call::MAX_AUDIO_BYTES };
    let audio = call_audio_budget(conn, 1 + budget.relayed.load(Relaxed)).min(audio_cap);
    budget.audio.store(audio, Relaxed);
    budget.covered.store(conn.cover_out.is_some(), Relaxed);
    budget.round_trip.store(conn.connection.rtt().as_micros() as u64, Relaxed);
    let (video_bps, video_piece) = match &conn.cover_out {
        Some(cover) => {
            let per_tick = conn.cover_datagrams_per_tick.max(1);
            let items = audio_frames_per_slot(per_tick);
            let room = cover.video_room(audio + 1, items);
            let piece = room.saturating_sub(crate::video::PIECE_OVERHEAD);
            let per_second = room * per_tick * (1000 / COVER_SLOT_INTERVAL.as_millis() as usize);
            let bps = per_second * 8 * 4 / 5;
            if piece < 64 || (bps as u32) < crate::video::MIN_BITRATE { (0, 0) } else { (bps, piece) }
        }
        None => (crate::call::UNCOVERED_VIDEO_START as usize, crate::video::UNCOVERED_PIECE),
    };
    let video_bps = under_their_limit(conn, video_bps);
    budget.video_bps.store(video_bps, Relaxed);
    budget.video_piece.store(video_piece, Relaxed);
}

/// Whether a connection with cover at `bits_per_second` may carry a
/// video call — see `video::MIN_COVER_FOR_VIDEO`.
pub fn cover_rate_carries_video(bits_per_second: u64) -> bool {
    bits_per_second >= crate::video::MIN_COVER_FOR_VIDEO && cover_rate_carries_voice(bits_per_second)
}

/// Whether a connection with cover at `bits_per_second` has room for
/// even minimal-quality voice (`call::MIN_AUDIO_BYTES` per frame) —
/// checked before placing or answering a call, so a cover rate too low
/// for voice refuses the call up front rather than garbling it.
pub fn cover_rate_carries_voice(bits_per_second: u64) -> bool {
    let slot = crate::cover::Outbound::new(crate::cover::slot_payload_len_for(bits_per_second, COVER_OVERHEAD_PER_SLOT));
    slot.priority_item_budget(audio_frames_per_slot(1)).saturating_sub(1) >= crate::call::MIN_AUDIO_BYTES
}
