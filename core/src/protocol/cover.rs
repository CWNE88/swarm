//! Cover's slots on a connection: how a rate becomes datagrams, and
//! sending them.

use super::*;

/// The largest datagram this connection can send, or `None` if the
/// other side doesn't take datagrams at all.
fn max_datagram(conn: &AuthenticatedConnection) -> Option<usize> {
    conn.connection.max_datagram_size()
}

/// The most cover sent, whatever's asked: a peer can require any rate of
/// us, and anything near this is more than calls with video need.
pub const MAX_COVER_BITS_PER_SECOND: u64 = 20_000_000;

/// The cover rate actually sent for `bits_per_second` asked: never more
/// than `MAX_COVER_BITS_PER_SECOND`, nor than nine tenths of the
/// bandwidth limit (`transport::bandwidth_limit`), which QUIC's own
/// packets have to fit under too.
pub fn covered_rate(bits_per_second: u64) -> u64 {
    let most = crate::transport::bandwidth_limit().map_or(MAX_COVER_BITS_PER_SECOND, |limit| (limit / 10 * 9).min(MAX_COVER_BITS_PER_SECOND));
    bits_per_second.min(most)
}

/// How a rate becomes slots: the wire bytes due every interval, split
/// into as few equal datagrams as fit the path, and the slot body size
/// of each.
fn slot_plan(conn: &AuthenticatedConnection, bits_per_second: u64) -> (usize, usize) {
    let bits_per_second = covered_rate(bits_per_second);
    let wire = crate::cover::slot_payload_len_for(bits_per_second, 0);
    let max = max_datagram(conn).unwrap_or(1200).max(crate::cover::MIN_SLOT_PAYLOAD + COVER_OVERHEAD_PER_SLOT);
    let per_tick = wire.div_ceil(max).max(1);
    let body = (wire / per_tick).saturating_sub(COVER_OVERHEAD_PER_SLOT).max(crate::cover::MIN_SLOT_PAYLOAD);
    (per_tick, body)
}

/// Turns on cover-mode sending for `conn` at `bits_per_second` — every
/// frame sent afterward queues into fixed-size, fixed-interval slots
/// instead of going straight to the wire; the caller is responsible for
/// actually ticking `emit_cover_slot` every `COVER_SLOT_INTERVAL` (see
/// `node::peer::PeerTask`). Not meant to be called a
/// second time on the same live connection (see `raise_cover` for
/// growing it): doing so would replace the queue, dropping whatever was
/// still waiting in it.
pub fn enable_cover(conn: &mut AuthenticatedConnection, bits_per_second: u64) {
    let (per_tick, body) = slot_plan(conn, bits_per_second);
    conn.cover_datagrams_per_tick = per_tick;
    conn.cover_out = Some(crate::cover::Outbound::new(body));
}

/// Emits one interval's cover — one datagram, or a few equal ones at a
/// rate too high for one — on `conn`'s outbound direction; a no-op if
/// `enable_cover` was never called for it. Filled with whatever's
/// queued, padded if there's nothing; always exactly the same size
/// either way, which is the entire point.
pub async fn emit_cover_slot(conn: &mut AuthenticatedConnection) -> Result<()> {
    let acks = conn.cover_in.received();
    let told = conn.cover_out.as_ref().is_none_or(|cover| cover.urgent_acked() >= conn.next_sealer_acked);
    if told && conn.next_sealer.as_ref().is_some_and(|(from, _)| Instant::now() >= *from) {
        conn.datagram_sealer = conn.next_sealer.take().map(|(_, sealer)| sealer);
    }
    let (Some(cover), Some(sealer)) = (&mut conn.cover_out, &mut conn.datagram_sealer) else { return Ok(()) };
    for _ in 0..conn.cover_datagrams_per_tick {
        let datagram = sealer.seal(&cover.build_slot_body(acks));
        crate::tally::slot_sent(datagram.len());
        conn.connection.send_datagram(datagram.into())?;
    }
    Ok(())
}

/// The first thing either side sends on a persistent connection after
/// `greet`: the cover rate it's sending at, and the rate it requires of
/// the other direction (`peers::Peer::cover_required`), if any. The
/// receiving side raises its own outbound cover to at least the
/// required rate (`raise_cover`) before sending anything else — so
/// "require" holds even against a peer who left cover off.
pub async fn send_cover_hello(conn: &mut AuthenticatedConnection, sends: Option<u64>, requires: Option<u64>, limit: Option<u64>) -> Result<()> {
    send_frame(conn, Frame::Notice(Notice::CoverHello { sends, requires, limit })).await
}

/// Makes this connection's outbound cover at least `bits_per_second`:
/// turns cover on if it was off, or grows the slots if they're smaller,
/// keeping anything already queued; never lowers it. Returns true when
/// cover was off until now — the caller then has to start ticking
/// `emit_cover_slot`.
pub fn raise_cover(conn: &mut AuthenticatedConnection, bits_per_second: u64) -> bool {
    let (per_tick, body) = slot_plan(conn, bits_per_second);
    match &mut conn.cover_out {
        Some(cover) => {
            if per_tick * body > conn.cover_datagrams_per_tick * cover.slot_payload_len() {
                if per_tick > conn.cover_datagrams_per_tick {
                    conn.cover_datagrams_per_tick = per_tick;
                }
                cover.grow_to(body);
            }
            false
        }
        None => {
            enable_cover(conn, bits_per_second);
            true
        }
    }
}
