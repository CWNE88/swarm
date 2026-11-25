//! QUIC transport, bound on `DEFAULT_PORT` (1977) on one socket that
//! takes both IPv6 and IPv4 (see `bind`).
//!
//! QUIC's own TLS layer is deliberately left **unauthenticated**: the
//! endpoint uses a throwaway self-signed certificate, and the client
//! accepts any server certificate offered. There's no CA in this design,
//! and no permanent server identity to pin a cert to — QUIC is used here
//! purely for what it's good at (stream multiplexing, loss recovery,
//! congestion control, connection-level liveness), not authentication.
//! Real peer authentication is `noise::Handshake`, run over the first
//! stream of every connection — see that module's docs.

use crate::noise::{Handshake, Session};
use crate::{framing, identity::Identity};
use anyhow::{Context, Result};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rand::Rng;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 1977;

/// What the other side may send on a connection's stream before this one
/// has read it (see `shared_transport_config`): the most that can be on
/// its way at once, whatever asks for more (a file's chunks, see
/// `transfer::MAX_WINDOW`).
pub const STREAM_WINDOW: u32 = 8 * 1024 * 1024;

/// QUIC's own connection-idle timeout. Without this, quinn defaults to
/// no idle timeout at all: a connection whose peer vanished without a
/// clean close (killed process, pulled cable) can sit alive at the QUIC
/// layer indefinitely, since nothing ever tells it the peer is gone.
///
/// This isn't just a backstop for the dialer — `protocol::serve_connection`
/// (the listener side) has no timeout of its own, it just blocks on a
/// read forever, so this is the *only* thing that unblocks it when a
/// dialer disappears.
///
/// Derived, not hardcoded, so it can't quietly drift out of sync with
/// the constants it depends on the way a separately-chosen number did
/// once already: one full `HEARTBEAT_INTERVAL` (traffic is expected at
/// least this often) plus one `REPLY_TIMEOUT` (grace for a request
/// already in flight when the interval elapsed) plus a fixed buffer for
/// ordinary network jitter. A healthy, on-schedule heartbeat never comes
/// close to tripping this; a peer that's actually gone is still caught
/// on roughly the same timescale the reply timeout already implies.
fn max_idle_timeout() -> Duration {
    crate::protocol::HEARTBEAT_INTERVAL + crate::protocol::REPLY_TIMEOUT + Duration::from_secs(2)
}

/// A syntactically-valid but otherwise meaningless name for the
/// self-signed server certificate's SAN (see `self_signed_server_config`
/// below) — `rcgen` needs *some* name to generate a cert, and nothing
/// here ever checks it against anything real. Not wire-visible in
/// cleartext (the Certificate message is encrypted under TLS 1.3), so
/// this is just hygiene, not the thing that keeps this off a DPI
/// signature — see `dial`'s use of an IP-literal `ServerName` for that.
fn random_sni() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..8).map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char).collect()
}

/// One QUIC connection, authenticated via a completed Noise_IK
/// handshake. `remote_public_key` is the peer's verified X25519 key —
/// the form Noise itself authenticates, *derived* from their root
/// Ed25519 identity (see `identity`'s module docs) rather than being
/// that identity directly. Known-in-advance for a dial (converted from
/// the peer list entry — see `dial`), learned-and-verified during the
/// handshake for an accept.
pub struct AuthenticatedConnection {
    pub connection: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
    pub session: Session,
    pub remote_public_key: [u8; 32],
    /// The other side's user key (Ed25519, what peers know it by) and
    /// the key of its device at the other end, as its `Hello` gave and
    /// proved them (see `protocol::greet`): the user key the one Noise
    /// authenticated, the device signed in by it, and holding its key
    /// for this connection.
    pub remote_user: [u8; 32],
    pub remote_device: [u8; 32],
    /// Cover-mode outbound pacing for this connection, if `protocol::
    /// enable_cover` has turned it on — `None` (the default, for every
    /// connection unless something opts in) means `protocol::send_frame`
    /// writes immediately, exactly as if cover mode didn't exist.
    pub cover_out: Option<crate::cover::Outbound>,
    /// Cover-mode inbound reassembly for this connection — always
    /// present and always checked (see `protocol::recv_frame`),
    /// regardless of `cover_out`: whether the *peer's* outbound traffic
    /// happens to be cover-shaped is entirely their own choice, not
    /// something this side's own configuration has any say over.
    pub cover_in: crate::cover::Inbound,
    /// Partial-frame buffer for `recv` — see `framing::FrameReader` for
    /// why reads on this connection go through it.
    pub frame_reader: crate::framing::FrameReader,
    /// When anything at all — a frame or a cover slot — last arrived
    /// (see `protocol::LIVENESS_TIMEOUT`).
    pub last_heard: std::time::Instant,
    /// The last `protocol::RequestId` this side used.
    pub next_request_id: u32,
    /// The other side's protocol version, from its `Hello`.
    pub peer_version: u16,
    /// Seals this side's cover datagrams, and opens the other side's
    /// (see `datagram`); set up by `protocol::greet`.
    pub datagram_sealer: Option<crate::datagram::Sealer>,
    pub datagram_opener: Option<crate::datagram::Opener>,
    /// Where the other side saw this connection come from, as its
    /// `Hello` said — this device's outside address, if it's behind NAT.
    pub seen_as: Option<SocketAddr>,
    /// How many datagrams make up one interval's cover (see
    /// `protocol::emit_cover_slot`).
    pub cover_datagrams_per_tick: usize,
    /// Frames `protocol::recv` decided to send, for `protocol::flush`.
    pub pending_out: std::collections::VecDeque<Vec<u8>>,
    /// Bytes handed to `send` so far. Set against what QUIC has actually
    /// sent, it shows a queue building inside QUIC, which the round trip
    /// never does (see `call::UncoveredVideo`).
    pub written: u64,
    /// How long writing to `send` has waited for QUIC to take it, all
    /// told (taken each second by a call without cover): a queue that
    /// builds *before* QUIC, in what's waiting to be written — under
    /// heavy loss QUIC's window is a few packets, and the call's pieces
    /// wait their turn with voice behind them, though nothing's queued
    /// inside QUIC itself (see `call::UncoveredVideo`).
    pub write_wait: std::time::Duration,
    /// The next voice datagram's number, and the newest received.
    pub voice_seq_out: u32,
    pub voice_seq_in: Option<u32>,
    /// Voice from datagrams waiting to be read (frames' bytes), and a
    /// lost frame's empty one before it, for its gap to be filled.
    pub voice_ready: std::collections::VecDeque<Vec<u8>>,
    /// When this side's keys were last renewed (see `protocol::REKEY_EVERY`),
    /// and how often they are; `rekeys` counts how many times.
    pub keys_since: std::time::Instant,
    pub rekey_every: std::time::Duration,
    pub rekeys: u32,
    /// A new datagram key, told to the other side, and when to start
    /// sealing with it.
    pub next_sealer: Option<(std::time::Instant, crate::datagram::Sealer)>,
    /// Told in a cover slot: how far the other side has to have
    /// acknowledged the urgent stream (past the `Rekey`) before
    /// `next_sealer` is used — a slot telling it, resent late, would
    /// otherwise go under the key it tells of (0: no such wait).
    pub next_sealer_acked: u64,
    /// The other side's receive limit (see `protocol::Notice::ReceiveLimit`):
    /// call video sent it is held under it.
    pub their_receive_limit: Option<u64>,
    /// Datagrams QUIC had received when last asked, when that count last
    /// went up, and the longest it's stayed the same since
    /// `take_longest_quiet` (see `quiet`).
    rx_seen: u64,
    rx_at: std::time::Instant,
    longest_quiet: std::time::Duration,
}

impl AuthenticatedConnection {
    fn new(connection: Connection, send: SendStream, recv: RecvStream, session: Session, remote_public_key: [u8; 32]) -> Self {
        AuthenticatedConnection {
            connection,
            send,
            recv,
            session,
            remote_public_key,
            remote_user: [0; 32],
            remote_device: [0; 32],
            cover_out: None,
            cover_in: crate::cover::Inbound::default(),
            frame_reader: crate::framing::FrameReader::default(),
            last_heard: std::time::Instant::now(),
            next_request_id: 0,
            peer_version: 0,
            datagram_sealer: None,
            datagram_opener: None,
            seen_as: None,
            cover_datagrams_per_tick: 1,
            pending_out: std::collections::VecDeque::new(),
            written: 0,
            write_wait: std::time::Duration::ZERO,
            voice_seq_out: 0,
            voice_seq_in: None,
            voice_ready: std::collections::VecDeque::new(),
            keys_since: std::time::Instant::now(),
            rekey_every: std::env::var("SWARM_REKEY_EVERY").ok().and_then(|s| s.parse().ok()).map(std::time::Duration::from_secs).unwrap_or(crate::protocol::REKEY_EVERY),
            rekeys: 0,
            next_sealer: None,
            next_sealer_acked: 0,
            their_receive_limit: None,
            rx_seen: 0,
            rx_at: std::time::Instant::now(),
            longest_quiet: std::time::Duration::ZERO,
        }
    }

    /// How long nothing at all has come from the other side: not a frame,
    /// not even QUIC's acknowledgement of what this side's sent. While
    /// this side's sending, those come back all the time, however slow or
    /// long the path — so a stretch without one is the link gone for a
    /// moment (a Wi-Fi roam), not a poor one (see `call::LINK_STOPPED`).
    pub fn quiet(&mut self) -> std::time::Duration {
        let seen = self.connection.stats().udp_rx.datagrams;
        if seen != self.rx_seen {
            self.rx_seen = seen;
            self.rx_at = std::time::Instant::now();
        }
        let quiet = self.rx_at.elapsed();
        self.longest_quiet = self.longest_quiet.max(quiet);
        quiet
    }

    /// The longest `quiet` seen since this was last asked.
    pub fn take_longest_quiet(&mut self) -> std::time::Duration {
        self.quiet();
        std::mem::take(&mut self.longest_quiet)
    }
}

/// Bind a QUIC endpoint on `addr` that can both accept incoming
/// connections and dial out. One endpoint per process is normal for
/// QUIC — unlike a UDP socket, it's not "used up" by a single peer.
pub fn bind(addr: SocketAddr) -> Result<Endpoint> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let server_config = self_signed_server_config()?;
    let socket = dual_stack_socket(addr).with_context(|| format!("binding QUIC endpoint on {addr}"))?;
    let runtime = quinn::default_runtime().context("no async runtime for QUIC")?;
    let socket = Arc::new(Limited::new(runtime.wrap_udp_socket(socket).with_context(|| format!("binding QUIC endpoint on {addr}"))?));
    let mut endpoint = Endpoint::new_with_abstract_socket(quinn::EndpointConfig::default(), Some(server_config), socket, runtime)
        .with_context(|| format!("binding QUIC endpoint on {addr}"))?;
    endpoint.set_default_client_config(insecure_client_config()?);
    Ok(endpoint)
}

/// The most this device sends, in bits a second, all told (0: no limit):
/// the person's bandwidth limit (see `set_bandwidth_limit`).
static LIMIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Sets the bandwidth limit: nothing this device sends goes faster than
/// `bits_per_second` (`None`: no limit). It's a ceiling, not a rate: below
/// it, everything goes as it would. Every endpoint's socket keeps to it
/// (`Limited`), whatever sends — calls, files, cover, and QUIC's own
/// packets alike — and cover and call video are sized to fit under it
/// (`protocol::slot_plan`, `call::UncoveredVideo`).
pub fn set_bandwidth_limit(bits_per_second: Option<u64>) {
    LIMIT.store(bits_per_second.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
}

/// The bandwidth limit, if one's set.
pub fn bandwidth_limit() -> Option<u64> {
    Some(LIMIT.load(std::sync::atomic::Ordering::Relaxed)).filter(|&l| l > 0)
}

/// The least a bandwidth limit can be: below this, a call's voice and
/// QUIC's own packets wouldn't fit.
pub const LEAST_BANDWIDTH_LIMIT: u64 = 64_000;

/// A socket that keeps to the bandwidth limit: a token bucket, filled at
/// the limit's rate, that each send takes its bytes from. A packet that
/// would take it below nothing is refused (`WouldBlock`), and QUIC keeps
/// it until the bucket's filled again (its `UdpPoller` waits till then).
///
/// Never a handshake's packets (QUIC's long header): they take their bytes
/// all the same, but go at once. Held, a server's handshake packet (a few
/// milliseconds, with the bucket overdrawn by another connection) left
/// quinn, on the connection made, never probing for a last packet lost
/// minutes later: the transfer stopped a few hundred bytes short until the
/// connection timed out (`tests/bandwidth.rs`, one run in six).
#[derive(Debug)]
struct Limited {
    inner: Arc<dyn quinn::AsyncUdpSocket>,
    bucket: std::sync::Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// Bytes that may go now; below nothing, owed.
    tokens: f64,
    filled: std::time::Instant,
    /// A packet was refused: none is tried again before this.
    held_until: Option<std::time::Instant>,
}

impl Limited {
    fn new(inner: Arc<dyn quinn::AsyncUdpSocket>) -> Limited {
        Limited { inner, bucket: std::sync::Mutex::new(Bucket { tokens: f64::MAX, filled: std::time::Instant::now(), held_until: None }) }
    }
}

impl Bucket {
    /// Filled to `now` at `bytes_per_second`, up to a burst of QUIC's (ten
    /// full packets at once) or a twentieth of a second's worth.
    fn fill(&mut self, now: std::time::Instant, bytes_per_second: f64) {
        let most = (bytes_per_second / 20.0).max(15_000.0);
        self.tokens = (self.tokens.min(most) + now.duration_since(self.filled).as_secs_f64() * bytes_per_second).min(most);
        self.filled = now;
    }
}

impl quinn::AsyncUdpSocket for Limited {
    fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn quinn::UdpPoller>> {
        let inner = self.inner.clone().create_io_poller();
        Box::pin(LimitedPoller { socket: self, inner, sleep: None })
    }

    fn try_send(&self, transmit: &quinn::udp::Transmit) -> std::io::Result<()> {
        use crate::lock::Locked;
        let handshake = transmit.contents.first().is_some_and(|b| b & 0x80 != 0);
        if let Some(limit) = bandwidth_limit() {
            let bytes_per_second = limit as f64 / 8.0;
            let now = std::time::Instant::now();
            let mut bucket = self.bucket.locked();
            bucket.fill(now, bytes_per_second);
            if !handshake && bucket.tokens <= 0.0 {
                bucket.held_until = Some(now + Duration::from_secs_f64(-bucket.tokens / bytes_per_second + 0.000_1));
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            drop(bucket);
            self.inner.try_send(transmit)?;
            self.bucket.locked().tokens -= transmit.contents.len() as f64;
            return Ok(());
        }
        self.inner.try_send(transmit)
    }

    fn poll_recv(&self, cx: &mut std::task::Context, bufs: &mut [std::io::IoSliceMut<'_>], meta: &mut [quinn::udp::RecvMeta]) -> std::task::Poll<std::io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta)
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// Writable when the socket is and no packet's being held (see `Limited`).
#[derive(Debug)]
struct LimitedPoller {
    socket: Arc<Limited>,
    inner: std::pin::Pin<Box<dyn quinn::UdpPoller>>,
    sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl quinn::UdpPoller for LimitedPoller {
    fn poll_writable(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context) -> std::task::Poll<std::io::Result<()>> {
        use crate::lock::Locked;
        loop {
            let held = {
                let mut bucket = self.socket.bucket.locked();
                match bucket.held_until {
                    Some(until) if until > std::time::Instant::now() => Some(until),
                    _ => {
                        bucket.held_until = None;
                        None
                    }
                }
            };
            let Some(until) = held else { break };
            let deadline = tokio::time::Instant::from_std(until);
            match &mut self.sleep {
                Some(sleep) => sleep.as_mut().reset(deadline),
                None => self.sleep = Some(Box::pin(tokio::time::sleep_until(deadline))),
            }
            if self.sleep.as_mut().expect("just set").as_mut().poll(cx).is_pending() {
                return std::task::Poll::Pending;
            }
        }
        self.inner.as_mut().poll_writable(cx)
    }
}

use std::future::Future as _;

/// A UDP socket on `addr`. An IPv6 one is made dual-stack, so the same
/// socket (and port) also takes IPv4 — Linux does that by default, but
/// Windows doesn't. Where the system won't allow it, it stays IPv6-only
/// rather than failing.
fn dual_stack_socket(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        let _ = socket.set_only_v6(false);
    }
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// `addr` as people write it: an IPv4 address that arrived on the
/// dual-stack socket as `::ffff:a.b.c.d` becomes plain `a.b.c.d`.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::from((v4, v6.port())),
            None => addr,
        },
        v4 => v4,
    }
}

/// The address the system sends to `to` from: its route there, asked by
/// connecting a UDP socket (nothing is sent). `None` if there's no route.
pub fn source_for(to: SocketAddr) -> Option<std::net::IpAddr> {
    let any: SocketAddr = if to.is_ipv4() { (std::net::Ipv4Addr::UNSPECIFIED, 0).into() } else { (std::net::Ipv6Addr::UNSPECIFIED, 0).into() };
    let socket = std::net::UdpSocket::bind(any).ok()?;
    socket.connect(to).ok()?;
    Some(socket.local_addr().ok()?.ip()).filter(|ip| !ip.is_unspecified())
}

fn shared_transport_config() -> Result<Arc<quinn::TransportConfig>> {
    let mut config = quinn::TransportConfig::default();
    config.max_idle_timeout(Some(
        max_idle_timeout().try_into().context("computed idle timeout out of quinn's representable range")?,
    ));
    config.mtu_discovery_config(None);
    config.max_probe_interval(Some(Duration::from_millis(250)));
    config.persistent_congestion_threshold(200);
    if std::env::var("SWARM_CONGESTION").is_ok_and(|v| v == "bbr") {
        config.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    }
    config.stream_receive_window(STREAM_WINDOW.into());
    config.max_concurrent_bidi_streams(1u32.into());
    config.max_concurrent_uni_streams(0u32.into());
    config.allow_spin(false);
    let mut ack_frequency = quinn::AckFrequencyConfig::default();
    ack_frequency.ack_eliciting_threshold(10u32.into()).max_ack_delay(Some(crate::cover::SLOT_INTERVAL * 2));
    config.ack_frequency_config(Some(ack_frequency));
    Ok(Arc::new(config))
}

fn self_signed_server_config() -> Result<quinn::ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec![random_sni()]).context("generating self-signed cert")?;
    let cert_der = cert.cert.der().clone();
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    let mut server_config = quinn::ServerConfig::with_single_cert(vec![cert_der], key_der)
        .context("building QUIC server config")?;
    server_config.transport_config(shared_transport_config()?);
    Ok(server_config)
}

fn insecure_client_config() -> Result<quinn::ClientConfig> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .context("building QUIC client crypto config")?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    client_config.transport_config(shared_transport_config()?);
    Ok(client_config)
}

/// Accepts any server certificate: real peer authentication happens in
/// `noise::Handshake`, not here. See the module doc above.
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Who this end is: the user's key, which Noise runs with and peers know
/// it by, and this device's own.
#[derive(Clone, Copy)]
pub struct Keys<'a> {
    pub user: &'a Identity,
    pub device: &'a Identity,
}

impl<'a> Keys<'a> {
    /// `user` on a device of its own made just now, whose key lasts as
    /// long as the process: for tests and one-off tools.
    pub fn new_device(user: &'a Identity) -> Keys<'a> {
        Keys { user, device: Box::leak(Box::new(Identity::generate())) }
    }
}

/// Dial a known peer: QUIC-connect, then run Noise_IK as the initiator
/// over the first bidirectional stream, then `protocol::greet`. `remote_public_key` is the
/// peer's root identity (Ed25519 — same value as their peer list entry
/// and what their own `swarm pubkey` prints); converted internally to
/// the X25519 form `IK` actually needs to encrypt the first handshake
/// message, via `identity::ed25519_public_key_to_x25519`.
pub async fn dial(
    endpoint: &Endpoint,
    addr: SocketAddr,
    keys: Keys<'_>,
    remote_public_key: [u8; 32],
) -> Result<AuthenticatedConnection> {
    let x25519_remote_public_key = crate::identity::ed25519_public_key_to_x25519(&remote_public_key)
        .map_err(|_| anyhow::anyhow!("stored public key isn't a valid Ed25519 identity"))?;

    let connecting = endpoint
        .connect(addr, &addr.ip().to_string())
        .with_context(|| format!("starting QUIC connection to {addr}"))?;
    let connection = connecting.await.with_context(|| format!("connecting to {addr}"))?;
    let (mut send, mut recv) = connection.open_bi().await.context("opening handshake stream")?;

    let mut handshake = Handshake::initiator(&keys.user.x25519_private_key_bytes(), &x25519_remote_public_key)
        .context("starting Noise handshake")?;
    let msg1 = handshake.write_message().context("writing Noise message 1")?;
    framing::write_frame(&mut send, &msg1).await?;
    let msg2 = framing::read_frame(&mut recv).await.map_err(|e| anyhow::Error::new(NotThatKey).context(format!("no answer to the handshake: {e}")))?;
    handshake.read_message(&msg2).map_err(|_| anyhow::Error::new(NotThatKey).context("reading Noise message 2 — wrong pubkey on file?"))?;

    let session = handshake.into_session().context("finishing Noise handshake")?;
    let mut conn = AuthenticatedConnection::new(connection, send, recv, session, x25519_remote_public_key);
    crate::protocol::greet(&mut conn, keys).await?;
    Ok(conn)
}

/// Something answered where a peer was dialled, but not to the key
/// dialled: it dropped the handshake, or answered for another key.
#[derive(Debug)]
pub struct NotThatKey;

impl std::fmt::Display for NotThatKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "something answered, but not to that key")
    }
}

impl std::error::Error for NotThatKey {}

/// An `accept` failure, with the remote address attached whenever it
/// was known (as soon as the QUIC connection itself came up — before
/// that, e.g. "endpoint closed", there's nothing to attach). This
/// module has no notion of peer names, so `Display` can only ever show
/// a bare address; a caller that *does* track names (`listen_forever`,
/// via its peer list) can match `remote` against it and show one
/// instead — see its `accept` error-handling for that.
#[derive(Debug)]
pub struct AcceptError {
    pub remote: Option<SocketAddr>,
    pub reason: anyhow::Error,
}

impl std::fmt::Display for AcceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.remote {
            Some(remote) => write!(f, "{remote}: {:#}", self.reason),
            None => write!(f, "{:#}", self.reason),
        }
    }
}

impl std::error::Error for AcceptError {}

/// How long an incoming connection gets to finish everything up to a
/// working Noise session — the QUIC handshake, opening its stream, and
/// the first Noise message. Generous for any real link; the point is
/// that a connection that stalls part-way gives up its place, rather
/// than holding it until QUIC's idle timeout.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Accept one incoming connection and run Noise_IK as the responder.
/// Takes them one at a time, so only for tests and one-off tools — a
/// long-running listener takes each `endpoint.accept()` and hands it to
/// `accept_incoming` on a task of its own, so one stalled handshake
/// can't hold up the next (see `swarm-cli`'s `listen_forever`).
pub async fn accept(endpoint: &Endpoint, keys: Keys<'_>) -> Result<AuthenticatedConnection, AcceptError> {
    let incoming = endpoint.accept().await.ok_or_else(|| AcceptError { remote: None, reason: anyhow::anyhow!("endpoint closed") })?;
    accept_incoming(incoming, keys).await
}

/// Finishes accepting one connection the endpoint has already taken in
/// — QUIC handshake, stream, Noise_IK as the responder — bounded by
/// `HANDSHAKE_TIMEOUT`. The dialer's identity is verified as part of
/// the handshake (`IK`'s first message is authenticated), so a caller
/// not in the peers list isn't a security problem — the caller layer
/// decides what someone unknown may do.
pub async fn accept_incoming(incoming: quinn::Incoming, keys: Keys<'_>) -> Result<AuthenticatedConnection, AcceptError> {
    let remote = Some(canonical(incoming.remote_address()));
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, accept_inner(incoming, keys)).await {
        Ok(result) => result.map_err(|reason| AcceptError { remote, reason }),
        Err(_) => Err(AcceptError { remote, reason: anyhow::anyhow!("didn't finish connecting within {HANDSHAKE_TIMEOUT:?}") }),
    }
}

/// Does the actual work. Every context message here is a complete,
/// plain-language sentence on its own, with no address in it:
/// `AcceptError::fmt` (or a caller that knows names) adds that.
async fn accept_inner(incoming: quinn::Incoming, keys: Keys<'_>) -> Result<AuthenticatedConnection> {
    let connection = incoming.await.context("accepting QUIC connection")?;
    let (mut send, mut recv) = connection.accept_bi().await.context("accepting a stream")?;

    let msg1 = framing::read_frame(&mut recv).await.context("reading handshake data")?;
    let mut handshake = Handshake::responder(&keys.user.x25519_private_key_bytes()).context("starting Noise handshake")?;
    handshake.read_message(&msg1).map_err(|_| anyhow::anyhow!("used the wrong key for this host"))?;
    let remote_public_key = handshake.remote_public_key().context("didn't identify its sender")?;
    let msg2 = handshake.write_message().context("preparing our reply")?;
    framing::write_frame(&mut send, &msg2).await.context("sending our reply")?;

    let session = handshake.into_session().context("completing the handshake")?;
    let mut conn = AuthenticatedConnection::new(connection, send, recv, session, remote_public_key);
    crate::protocol::greet(&mut conn, keys).await?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address sent from to somewhere is the system's route there:
    /// loopback's own, for loopback.
    #[test]
    fn the_source_for_loopback_is_loopback() {
        assert_eq!(source_for("127.0.0.1:1977".parse().unwrap()), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(source_for("[::1]:1977".parse().unwrap()), Some("::1".parse().unwrap()));
    }

    #[test]
    fn random_sni_is_regenerated_and_never_the_old_fixed_value() {
        let a = random_sni();
        let b = random_sni();
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_ne!(a, b, "SNI must be regenerated per call, not fixed");
        assert_ne!(a, "swarm.local");
    }

    /// `dial` passes `addr.ip().to_string()` as the QUIC `server_name`.
    /// This pins down that an IPv6 literal parses to
    /// `ServerName::IpAddress` (not `DnsName`) — the variant rustls
    /// only ever puts in the ClientHello for `DnsName` (see
    /// `client::hs::start_handshake` in the rustls source: the
    /// `server_name` extension is `None` for every other variant) — so
    /// a regression to something that accidentally parses as a
    /// hostname would silently bring the SNI extension back.
    #[test]
    fn dialing_by_ip_produces_an_ip_server_name_not_a_dns_one() {
        use rustls::pki_types::ServerName;
        let addr: SocketAddr = "[2403:5819:c8bf:5f5f:4269:675f:1111:2222]:1977".parse().unwrap();
        let server_name = ServerName::try_from(addr.ip().to_string()).expect("IP literal must parse");
        assert!(matches!(server_name, ServerName::IpAddress(_)), "expected IpAddress, got {server_name:?}");
    }

    /// The way a long-running listener accepts (each `endpoint.accept()`
    /// finished on its own task): someone who connects at the QUIC level
    /// and then does nothing mustn't hold up the next person.
    #[tokio::test]
    async fn a_stalled_connection_does_not_hold_up_the_next_one() {
        use crate::identity::Identity;

        let server_identity = Arc::new(Identity::generate());
        let server_key = server_identity.public_key_bytes();
        let server = bind("[::1]:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(incoming) = server.accept().await {
                let identity = server_identity.clone();
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    if let Ok(conn) = accept_incoming(incoming, Keys::new_device(&identity)).await {
                        let _ = done_tx.send(conn);
                    }
                });
            }
        });

        let staller = bind("[::1]:0".parse().unwrap()).unwrap();
        let _stalled = staller.connect(addr, "::1").unwrap().await.unwrap();

        let client_identity = Identity::generate();
        let client = bind("[::1]:0".parse().unwrap()).unwrap();
        let dialed = tokio::time::timeout(Duration::from_secs(2), dial(&client, addr, Keys::new_device(&client_identity), server_key)).await;
        match &dialed { Ok(Ok(_)) => {}, Ok(Err(e)) => panic!("dial failed: {e:#}"), Err(_) => panic!("dial timed out") }
        assert_eq!(done_rx.recv().await.unwrap().remote_public_key, client_identity.x25519_public_key_bytes());
    }
}
