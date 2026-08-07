//! Dedicated media event loop.
//!
//! The control loop submits infrequent session/topology changes through one
//! [`EventSubmission`]. The media loop owns every mutable packet-plane value
//! and returns only coalesced control-plane observations through
//! [`VoiceEventSubmission`]. Both queues wake their consumer's `mio::Poll`, and
//! consumers swap reusable buffers so draining does not allocate.
//!
//! Besides the UDP media sockets, the loop owns TCP voice-fallback lanes:
//! connections the control loop classified by [`media::VOICE_TCP_MAGIC`] and
//! handed over. Each lane carries length-prefixed sealed media datagrams and is
//! bound to its session by the first frame — a `Bind` that opens under the
//! session's media keys. A bound lane takes precedence over the session's UDP
//! address for downstream media until it closes.

use hashbrown::HashMap;
use mio::{
    Events, Interest, Poll, Token, Waker,
    net::{TcpStream, UdpSocket},
};
use rpc::{
    crypto::AntiReplay,
    evented::{
        MioReady, ReadLimit, Readiness, WriteQueue, is_interrupted_io_error, read_into_buffer,
        write_queue_to,
    },
    frame,
    ids::{RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayloadRef, MediaProtection, VoicePayloadRef},
    recv::RecvBuffer,
};
use std::{
    io,
    net::SocketAddr,
    os::fd::AsRawFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    config::MAX_BIND_ADDRS,
    event_queue::{EventNotifier, VOICE_EVENTS},
};

/// Media sockets take tokens `0..MAX_BIND_ADDRS`, so the probe and the command
/// waker sit immediately above them. Readiness is a bitmask over the same
/// numbering, which is why the bind cap has to stay inside a `u64`.
const UDP_PROBE: Token = Token(MAX_BIND_ADDRS);
const COMMANDS: Token = Token(MAX_BIND_ADDRS + 1);
const PROBE_BIT: u32 = MAX_BIND_ADDRS as u32;
const _: () = assert!(MAX_BIND_ADDRS < u64::BITS as usize);
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const UDP_DRAIN_BUDGET: usize = 64;
const MAX_UDP_DATAGRAM_BYTES: usize = 2048;
const ACTIVITY_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_SESSIONS: usize = super::MAX_CLIENTS;
const SESSION_WORDS: usize = MAX_SESSIONS.div_ceil(u64::BITS as usize);

/// TCP voice lanes take tokens `TCP_CONN_BASE + slot`, above every UDP socket
/// and the command waker.
const TCP_CONN_BASE: usize = MAX_BIND_ADDRS + 2;
const MAX_TCP_VOICE_CONNS: usize = MAX_SESSIONS;
const TCP_CONN_WORDS: usize = MAX_TCP_VOICE_CONNS.div_ceil(u64::BITS as usize);
/// Cap on lanes that have left the control loop's pre-auth budget but have not
/// yet authenticated with a first frame. It covers both handoffs still queued
/// for the relay thread and lanes the relay already owns, so it is the only
/// bound on anonymous holders of a socket once classification releases them.
const MAX_PENDING_TCP_VOICE: usize = 32;
const VOICE_TCP_AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const VOICE_TCP_READ_BUDGET_BYTES: usize = 16 * 1024;
const VOICE_TCP_WRITE_ATTEMPTS: usize = 32;
/// A lane is detached instead of queued beyond this backlog. Keeping the old
/// FIFO would replay stale speech and place control frames behind it.
const VOICE_TCP_WRITE_CAP_BYTES: usize = 32 * 1024;
const VOICE_TCP_MAX_WIRE_FRAME_BYTES: usize =
    frame::LENGTH_PREFIX_LEN + media::VOICE_TCP_MAX_FRAME_BYTES;
const _: () = assert!(VOICE_TCP_WRITE_CAP_BYTES > VOICE_TCP_MAX_WIRE_FRAME_BYTES);
/// A lane is detached once its queue has been continuously occupied this long.
/// The byte cap is a memory bound, not a latency one — 32 KiB is seconds of
/// speech at ordinary bitrates — and a healthy lane empties on every flush, so
/// an occupied queue means the peer has stopped reading.
const VOICE_TCP_STALE_BACKLOG: Duration = Duration::from_millis(250);

/// Sustained per-session voice ingress ceiling, in bytes and packets per
/// second, with one second of burst.
///
/// Sized several times above the legitimate worst case — a 96 kbps encoder at
/// 50 packets per second plus DRED and framing overhead — because this exists
/// to bound what one buggy or hostile session can spend of the relay's fan-out
/// budget, not to shape ordinary speech. TCP ingress is otherwise limited only
/// by how fast the sender can write.
const VOICE_INGRESS_BYTES_PER_SEC: u32 = 48 * 1024;
const VOICE_INGRESS_PACKETS_PER_SEC: u32 = 120;
/// Feedback allowance: clients report once per 500 ms per inbound stream, so
/// this covers a large room and no more.
const FEEDBACK_INGRESS_PACKETS_PER_SEC: u32 = 100;
/// Bind, Ping, and NAT probe allowance. Binds retry at 1 Hz, pings are sparser,
/// and NAT probes come in pairs at each ICE restart.
const SIGNAL_INGRESS_PACKETS_PER_SEC: u32 = 20;

#[cfg(target_os = "linux")]
fn request_realtime_priority() -> io::Result<bool> {
    let mut policy = 0;
    let mut parameters = libc::sched_param { sched_priority: 0 };

    // SAFETY: Both pointers refer to initialized, writable values for the
    // duration of the call, and pthread_self() identifies the calling thread.
    let result =
        unsafe { libc::pthread_getschedparam(libc::pthread_self(), &mut policy, &mut parameters) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    if policy != libc::SCHED_OTHER {
        return Ok(false);
    }

    parameters.sched_priority = 1;
    // SAFETY: parameters is valid for the duration of the call and applies to
    // the calling thread. Linux reports permission failures in `result`.
    let result =
        unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &parameters) };
    if result == 0 {
        Ok(true)
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

#[cfg(not(target_os = "linux"))]
fn request_realtime_priority() -> io::Result<bool> {
    Ok(false)
}

#[cfg(target_os = "linux")]
fn restore_normal_priority(promoted: bool) -> io::Result<()> {
    if !promoted {
        return Ok(());
    }
    let parameters = libc::sched_param { sched_priority: 0 };
    // SAFETY: parameters is valid for the duration of the call and applies to
    // the calling thread. Lowering a thread back to SCHED_OTHER is permitted
    // after a successful promotion.
    let result = unsafe {
        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_OTHER, &parameters)
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

#[cfg(not(target_os = "linux"))]
fn restore_normal_priority(_promoted: bool) -> io::Result<()> {
    Ok(())
}

/// A waker-backed, allocation-reusing event submission queue.
///
/// Producers hold the mutex only long enough to append. The receiving event
/// loop swaps the shared vector with its already-empty local vector, so it does
/// not hold this lock while applying any event.
struct EventSubmission<T> {
    pending: Mutex<Vec<T>>,
    waker: Arc<Waker>,
}

impl<T> EventSubmission<T> {
    fn new(waker: Arc<Waker>) -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            waker,
        }
    }

    fn submit(&self, event: T) {
        self.pending.lock().unwrap().push(event);
        if let Err(error) = self.waker.wake() {
            kvlog::warn!("voice event wake failed", error = %error);
        }
    }

    fn drain_into(&self, events: &mut Vec<T>) {
        debug_assert!(events.is_empty());
        let mut pending = self.pending.lock().unwrap();
        std::mem::swap(&mut *pending, events);
    }
}

/// Shared admission budget for lanes between classification and authentication.
///
/// A permit is taken by the control loop *before* it gives up its own pre-auth
/// slot, and released only when the lane binds to a session or its socket is
/// dropped. Queued handoffs therefore hold a permit while they sit in the
/// command queue, which is what keeps file descriptors bounded across the
/// thread boundary.
struct TcpHandoffBudget {
    held: AtomicUsize,
}

impl TcpHandoffBudget {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            held: AtomicUsize::new(0),
        })
    }

    fn reserve(self: &Arc<Self>) -> Option<TcpHandoffPermit> {
        let mut held = self.held.load(Ordering::Relaxed);
        loop {
            if held >= MAX_PENDING_TCP_VOICE {
                return None;
            }
            match self.held.compare_exchange_weak(
                held,
                held + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(TcpHandoffPermit {
                        budget: Arc::clone(self),
                    });
                }
                Err(current) => held = current,
            }
        }
    }

    fn held(&self) -> usize {
        self.held.load(Ordering::Relaxed)
    }
}

/// One reserved slot of [`TcpHandoffBudget`], released on drop so every
/// rejection, detach, and discarded command returns it without bookkeeping.
pub(super) struct TcpHandoffPermit {
    budget: Arc<TcpHandoffBudget>,
}

impl Drop for TcpHandoffPermit {
    fn drop(&mut self) {
        self.budget.held.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VoiceRoute {
    pub(super) room_id: RoomId,
    pub(super) stream_id: StreamId,
}

pub(super) enum VoiceCommand {
    RegisterSession {
        session_id: SessionId,
        user_id: UserId,
        protection: MediaProtection,
    },
    SetRoute {
        session_id: SessionId,
        route: Option<VoiceRoute>,
    },
    RemoveSession {
        session_id: SessionId,
    },
    /// A TCP voice lane handed over by the control loop, already stripped of
    /// its magic preamble. `read_buf` carries any bytes that arrived with it,
    /// and `permit` the admission slot reserved before the handoff.
    AttachTcp {
        socket: TcpStream,
        addr: SocketAddr,
        read_buf: RecvBuffer,
        permit: TcpHandoffPermit,
    },
    Shutdown,
}

pub(super) struct VoiceActivity {
    pub(super) last_activity: Instant,
    pub(super) reported_rtt_ms: Option<u16>,
    pub(super) rtt_reported_at: Option<Instant>,
}

#[derive(Default)]
pub(super) struct VoiceEventBatch {
    pub(super) udp_bound: HashMap<SessionId, SocketAddr>,
    pub(super) nat_probe: HashMap<(SessionId, u8), SocketAddr>,
    pub(super) activity: HashMap<SessionId, VoiceActivity>,
    pub(super) failure: Option<String>,
}

impl VoiceEventBatch {
    pub(super) fn with_capacity() -> Self {
        Self {
            udp_bound: HashMap::with_capacity(MAX_SESSIONS),
            nat_probe: HashMap::with_capacity(MAX_SESSIONS * 2),
            activity: HashMap::with_capacity(MAX_SESSIONS),
            failure: None,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.udp_bound.is_empty()
            && self.nat_probe.is_empty()
            && self.activity.is_empty()
            && self.failure.is_none()
    }
}

/// Latest-value queue for UDP observations.
///
/// The control loop can legitimately stall behind disk or control-plane work.
/// Keeping only the newest value for each session/probe bounds memory and makes
/// its eventual drain proportional to the live topology, not the stall length.
struct VoiceEventSubmission {
    pending: Mutex<VoiceEventBatch>,
    notifier: Arc<EventNotifier>,
}

impl VoiceEventSubmission {
    fn new(notifier: Arc<EventNotifier>) -> Self {
        Self {
            pending: Mutex::new(VoiceEventBatch::with_capacity()),
            notifier,
        }
    }

    fn submit_all(&self, events: &mut VoiceEventBatch) {
        if events.is_empty() {
            return;
        }
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.is_empty() {
                std::mem::swap(&mut *pending, events);
            } else {
                pending.udp_bound.extend(events.udp_bound.drain());
                pending.nat_probe.extend(events.nat_probe.drain());
                pending.activity.extend(events.activity.drain());
                if events.failure.is_some() {
                    pending.failure = events.failure.take();
                }
            }
        }
        self.notifier.signal(VOICE_EVENTS, "voice");
    }

    fn submit_failure(&self, failure: String) {
        self.pending.lock().unwrap().failure = Some(failure);
        self.notifier.signal(VOICE_EVENTS, "voice");
    }

    fn drain_into(&self, events: &mut VoiceEventBatch) {
        debug_assert!(events.is_empty());
        let mut pending = self.pending.lock().unwrap();
        std::mem::swap(&mut *pending, events);
    }
}

pub(super) struct VoiceRelayHandle {
    commands: Arc<EventSubmission<VoiceCommand>>,
    events: Arc<VoiceEventSubmission>,
    handoff_budget: Arc<TcpHandoffBudget>,
    thread: Option<JoinHandle<()>>,
    udp_local_addrs: Vec<SocketAddr>,
}

impl VoiceRelayHandle {
    pub(super) fn spawn(
        mut udp: Vec<UdpSocket>,
        mut udp_probe: Option<UdpSocket>,
        control_notifier: Arc<EventNotifier>,
        p2p_enabled: bool,
    ) -> io::Result<Self> {
        debug_assert!(!udp.is_empty());
        debug_assert!(udp.len() <= MAX_BIND_ADDRS);
        let mut udp_local_addrs = Vec::with_capacity(udp.len());
        for socket in &udp {
            udp_local_addrs.push(socket.local_addr()?);
        }
        for (socket, local_addr) in udp.iter().zip(&udp_local_addrs) {
            if let Err(error) = rpc::qos::apply_voice_qos(socket.as_raw_fd(), *local_addr) {
                kvlog::warn!(
                    "voice udp qos unavailable",
                    addr = %local_addr,
                    dscp = rpc::qos::VOICE_DSCP,
                    error = %error
                );
            }
        }
        let poll = Poll::new().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to create voice relay poller: {error}"),
            )
        })?;
        for (index, socket) in udp.iter_mut().enumerate() {
            let addr = udp_local_addrs[index];
            poll.registry()
                .register(socket, Token(index), Interest::READABLE)
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "failed to register voice udp bind {} ({addr}): {error}",
                            index + 1
                        ),
                    )
                })?;
        }
        if let Some(probe) = udp_probe.as_mut() {
            poll.registry()
                .register(probe, UDP_PROBE, Interest::READABLE)
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("failed to register voice udp probe: {error}"),
                    )
                })?;
        }
        let command_waker = Arc::new(Waker::new(poll.registry(), COMMANDS).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to create voice relay command waker: {error}"),
            )
        })?);
        let commands = Arc::new(EventSubmission::new(command_waker));
        let events = Arc::new(VoiceEventSubmission::new(control_notifier));
        let handoff_budget = TcpHandoffBudget::new();
        let loop_commands = Arc::clone(&commands);
        let loop_events = Arc::clone(&events);
        let loop_budget = Arc::clone(&handoff_budget);
        let thread = thread::Builder::new()
            .name("chatt-voice-relay".to_string())
            .spawn(move || {
                let mut relay = VoiceRelay::new(
                    poll,
                    udp,
                    udp_probe,
                    loop_commands,
                    loop_events,
                    loop_budget,
                    p2p_enabled,
                );
                // Preserve the failure detail in debug logs before falling
                // back to ordinary scheduling.
                #[allow(clippy::manual_unwrap_or_default)]
                let promoted = match request_realtime_priority() {
                    Ok(promoted) => promoted,
                    Err(_error) => {
                        kvlog::debug!(
                            "voice relay realtime priority unavailable",
                            error = %_error,
                            hint = "grant CAP_SYS_NICE to chatt-server to allow SCHED_FIFO"
                        );
                        false
                    }
                };
                let result = relay.run();
                let _ = restore_normal_priority(promoted);
                if let Err(error) = result {
                    let message = error.to_string();
                    kvlog::error!("voice relay stopped", error = message.as_str());
                    relay.events.submit_failure(message);
                }
            })
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to spawn voice relay worker: {error}"),
                )
            })?;
        Ok(Self {
            commands,
            events,
            handoff_budget,
            thread: Some(thread),
            udp_local_addrs,
        })
    }

    pub(super) fn submit(&self, command: VoiceCommand) {
        self.commands.submit(command);
    }

    /// Reserves the admission slot a classified lane needs before the control
    /// loop releases its own pre-auth slot. `None` means the unauthenticated
    /// lane budget is full and the connection must be closed instead.
    pub(super) fn reserve_tcp_handoff(&self) -> Option<TcpHandoffPermit> {
        self.handoff_budget.reserve()
    }

    pub(super) fn drain_events(&self, events: &mut VoiceEventBatch) {
        self.events.drain_into(events);
    }

    pub(super) fn local_addr(&self) -> SocketAddr {
        self.udp_local_addrs[0]
    }

    pub(super) fn local_addrs(&self) -> &[SocketAddr] {
        &self.udp_local_addrs
    }
}

impl Drop for VoiceRelayHandle {
    fn drop(&mut self) {
        self.commands.submit(VoiceCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                kvlog::error!("voice relay thread panicked");
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionRoute {
    room_id: RoomId,
    stream_id: StreamId,
}

impl From<VoiceRoute> for SessionRoute {
    fn from(route: VoiceRoute) -> Self {
        Self {
            room_id: route.room_id,
            stream_id: route.stream_id,
        }
    }
}

/// A token bucket over one ingress dimension, in tokens per second.
///
/// Tokens accrue with elapsed time and cap at `burst`, so a session that has
/// been quiet can spend a second's worth at once but cannot bank more.
struct TokenBucket {
    rate: u32,
    burst: u32,
    tokens: u32,
    /// Fractional tokens carried between refills, in token-microseconds, so a
    /// stream of sub-millisecond refills does not round down to nothing.
    remainder: u64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate: u32, now: Instant) -> Self {
        Self {
            rate,
            burst: rate,
            tokens: rate,
            remainder: 0,
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last);
        if elapsed.is_zero() {
            return;
        }
        self.last = now;
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let total = self
            .remainder
            .saturating_add(u64::from(self.rate).saturating_mul(micros));
        let gained = u32::try_from(total / 1_000_000).unwrap_or(u32::MAX);
        self.remainder = total % 1_000_000;
        self.tokens = self.tokens.saturating_add(gained).min(self.burst);
    }

    /// Spends `cost` tokens, reporting whether the budget covered it.
    fn take(&mut self, now: Instant, cost: u32) -> bool {
        self.refill(now);
        match self.tokens.checked_sub(cost) {
            Some(left) => {
                self.tokens = left;
                true
            }
            None => false,
        }
    }

    /// Accrues tokens and reports the balance, for a charge that has to clear
    /// more than one bucket before either is spent.
    fn available(&mut self, now: Instant) -> u32 {
        self.refill(now);
        self.tokens
    }
}

/// Per-session ingress ceilings, charged after a frame authenticates and before
/// any fan-out.
///
/// Deliberately transport independent: a lane and a datagram source cost the
/// relay the same sealing work per recipient, and only the UDP one is limited
/// by the network. Pings and feedback get their own small allowances so a
/// session that has spent its voice budget can still be measured and answered.
struct MediaBudget {
    voice_bytes: TokenBucket,
    voice_packets: TokenBucket,
    feedback_packets: TokenBucket,
    signal_packets: TokenBucket,
    /// Payloads dropped since the last report, flushed on the activity tick so
    /// a flood costs one log line per second rather than one per packet.
    dropped: u32,
}

impl MediaBudget {
    fn new(now: Instant) -> Self {
        Self {
            voice_bytes: TokenBucket::new(VOICE_INGRESS_BYTES_PER_SEC, now),
            voice_packets: TokenBucket::new(VOICE_INGRESS_PACKETS_PER_SEC, now),
            feedback_packets: TokenBucket::new(FEEDBACK_INGRESS_PACKETS_PER_SEC, now),
            signal_packets: TokenBucket::new(SIGNAL_INGRESS_PACKETS_PER_SEC, now),
            dropped: 0,
        }
    }

    fn admit_voice(&mut self, now: Instant, wire_bytes: usize) -> bool {
        let bytes = wire_bytes.min(u32::MAX as usize) as u32;
        // Both dimensions are charged only when both fit, so a rejected packet
        // never leaves one bucket drained by traffic that was not relayed.
        if self.voice_bytes.available(now) < bytes || self.voice_packets.available(now) < 1 {
            return self.reject();
        }
        self.voice_bytes.take(now, bytes);
        self.voice_packets.take(now, 1);
        true
    }

    fn admit_feedback(&mut self, now: Instant) -> bool {
        self.feedback_packets.take(now, 1) || self.reject()
    }

    fn admit_signal(&mut self, now: Instant) -> bool {
        self.signal_packets.take(now, 1) || self.reject()
    }

    fn reject(&mut self) -> bool {
        self.dropped = self.dropped.saturating_add(1);
        false
    }
}

struct TcpVoiceConn {
    socket: TcpStream,
    addr: SocketAddr,
    read_buf: RecvBuffer,
    readiness: Readiness,
    write_queue: WriteQueue,
    write_blocked: bool,
    session: Option<SessionId>,
    /// Held until the lane binds to a session; dropping the conn releases it.
    permit: Option<TcpHandoffPermit>,
    attached_at: Instant,
    /// When the write queue last went from empty to non-empty, `None` while it
    /// is empty. A healthy lane drains on every flush, so a queue that stays
    /// occupied bounds how stale its oldest unsent frame can be.
    backlog_since: Option<Instant>,
}

struct VoiceSession {
    slot: usize,
    user_id: UserId,
    protection: MediaProtection,
    recv_replay: AntiReplay,
    send_counter: u64,
    udp_addr: Option<SocketAddr>,
    udp_bind_index: usize,
    /// Bound TCP voice lane. While present it carries the session's downstream
    /// media instead of `udp_addr`; the client closes it to return to UDP.
    tcp_conn: Option<usize>,
    route: Option<SessionRoute>,
    budget: MediaBudget,
    last_activity: Instant,
    reported_rtt_ms: Option<u16>,
    rtt_reported_at: Option<Instant>,
    activity_dirty: bool,
}

#[derive(Clone, Copy)]
enum PacketOrigin {
    Udp {
        server_probe_id: u8,
        bind_index: usize,
        src: SocketAddr,
    },
    Tcp {
        slot: usize,
    },
}

#[derive(Default)]
struct VoiceRoom {
    members: [u64; SESSION_WORDS],
}

impl VoiceRoom {
    fn insert(&mut self, slot: usize) {
        self.members[slot / u64::BITS as usize] |= 1 << (slot % u64::BITS as usize);
    }

    fn remove(&mut self, slot: usize) {
        self.members[slot / u64::BITS as usize] &= !(1 << (slot % u64::BITS as usize));
    }

    fn is_empty(&self) -> bool {
        self.members.iter().all(|word| *word == 0)
    }
}

struct VoiceRelay {
    poll: Poll,
    /// Media sockets, indexed by bind. Index 0 is the primary bind, the one
    /// invite tickets advertise.
    udp: Vec<UdpSocket>,
    udp_probe: Option<UdpSocket>,
    commands: Arc<EventSubmission<VoiceCommand>>,
    events: Arc<VoiceEventSubmission>,
    p2p_enabled: bool,
    sessions: HashMap<SessionId, VoiceSession>,
    route_to_session: HashMap<u32, SessionId>,
    rooms: HashMap<RoomId, VoiceRoom>,
    stream_owners: HashMap<(RoomId, StreamId), SessionId>,
    session_slots: [Option<SessionId>; MAX_SESSIONS],
    next_activity_flush: Instant,
    poll_events: Events,
    command_buf: Vec<VoiceCommand>,
    event_buf: VoiceEventBatch,
    relay_recipients: Vec<SessionId>,
    /// Allocated once and never re-zeroed; taken and restored across a drain so
    /// receiving never memsets a buffer it is about to overwrite.
    recv_buf: Vec<u8>,
    udp_send_packet: Vec<u8>,
    udp_send_scratch: Vec<u8>,
    /// Sockets with readiness left to drain: bit `i` is media bind `i`, and
    /// [`PROBE_BIT`] is the probe socket.
    udp_work: u64,
    tcp_conns: Box<[Option<TcpVoiceConn>]>,
    /// Shared with the control loop: every lane awaiting authentication, queued
    /// or attached, holds one permit against [`MAX_PENDING_TCP_VOICE`].
    handoff_budget: Arc<TcpHandoffBudget>,
    /// Lane slots with readiness left to drain, mirroring `udp_work`.
    tcp_read_work: [u64; TCP_CONN_WORDS],
    tcp_write_work: [u64; TCP_CONN_WORDS],
    shutting_down: bool,
}

fn set_slot_bit(words: &mut [u64; TCP_CONN_WORDS], slot: usize) {
    words[slot / u64::BITS as usize] |= 1 << (slot % u64::BITS as usize);
}

fn clear_slot_bit(words: &mut [u64; TCP_CONN_WORDS], slot: usize) {
    words[slot / u64::BITS as usize] &= !(1 << (slot % u64::BITS as usize));
}

#[derive(Debug)]
enum PacketError {
    Media(media::MediaError),
    UnknownRoute,
    UnknownSession,
    NatProbeUnavailable,
    UnboundSource,
    UnknownNatProbe,
    UnauthenticatedConn,
    SessionMismatch,
}

impl std::fmt::Display for PacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketError::Media(error) => error.fmt(f),
            PacketError::UnknownRoute => f.write_str("unknown UDP route id"),
            PacketError::UnknownSession => f.write_str("unknown UDP session"),
            PacketError::NatProbeUnavailable => {
                f.write_str("NAT probe not available without transport encryption")
            }
            PacketError::UnboundSource => f.write_str("plaintext media from an unbound source"),
            PacketError::UnknownNatProbe => f.write_str("unknown NAT probe id"),
            PacketError::UnauthenticatedConn => {
                f.write_str("voice tcp conn must authenticate with a bind frame")
            }
            PacketError::SessionMismatch => {
                f.write_str("voice tcp frame routed to a different session")
            }
        }
    }
}

impl From<media::MediaError> for PacketError {
    fn from(error: media::MediaError) -> Self {
        PacketError::Media(error)
    }
}

impl VoiceRelay {
    fn new(
        poll: Poll,
        udp: Vec<UdpSocket>,
        udp_probe: Option<UdpSocket>,
        commands: Arc<EventSubmission<VoiceCommand>>,
        events: Arc<VoiceEventSubmission>,
        handoff_budget: Arc<TcpHandoffBudget>,
        p2p_enabled: bool,
    ) -> Self {
        debug_assert!(!udp.is_empty());
        debug_assert!(udp.len() <= MAX_BIND_ADDRS);
        Self {
            poll,
            udp,
            udp_probe,
            commands,
            events,
            p2p_enabled,
            sessions: HashMap::with_capacity(MAX_SESSIONS),
            route_to_session: HashMap::with_capacity(MAX_SESSIONS),
            rooms: HashMap::with_capacity(MAX_SESSIONS),
            stream_owners: HashMap::with_capacity(MAX_SESSIONS),
            session_slots: [None; MAX_SESSIONS],
            next_activity_flush: Instant::now() + ACTIVITY_FLUSH_INTERVAL,
            poll_events: Events::with_capacity(128),
            command_buf: Vec::with_capacity(MAX_SESSIONS),
            event_buf: VoiceEventBatch::with_capacity(),
            relay_recipients: Vec::with_capacity(MAX_SESSIONS),
            recv_buf: vec![0; MAX_UDP_DATAGRAM_BYTES],
            udp_send_packet: Vec::with_capacity(media::MAX_SEALED_MEDIA_BYTES),
            udp_send_scratch: Vec::with_capacity(media::SAFE_UDP_PAYLOAD_BYTES),
            udp_work: 0,
            tcp_conns: std::iter::repeat_with(|| None)
                .take(MAX_TCP_VOICE_CONNS)
                .collect(),
            handoff_budget,
            tcp_read_work: [0; TCP_CONN_WORDS],
            tcp_write_work: [0; TCP_CONN_WORDS],
            shutting_down: false,
        }
    }

    fn run(&mut self) -> io::Result<()> {
        while !self.shutting_down {
            self.drain_commands();
            if self.shutting_down {
                break;
            }
            // A drain that exhausts its budget re-arms its own bit on
            // `self.udp_work`, which this take has already cleared, so the
            // remainder is picked up on the next pass instead of starving the
            // other sockets.
            let mut ready = std::mem::take(&mut self.udp_work);
            while ready != 0 {
                let bit = ready.trailing_zeros();
                ready &= ready - 1;
                self.receive(bit);
                self.drain_commands();
                if self.shutting_down {
                    break;
                }
            }
            for word_index in 0..TCP_CONN_WORDS {
                if self.shutting_down {
                    break;
                }
                let mut word = std::mem::take(&mut self.tcp_read_work[word_index]);
                while word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    word &= word - 1;
                    self.receive_tcp(word_index * u64::BITS as usize + bit);
                    self.drain_commands();
                    if self.shutting_down {
                        break;
                    }
                }
            }
            for word_index in 0..TCP_CONN_WORDS {
                if self.shutting_down {
                    break;
                }
                let mut word = std::mem::take(&mut self.tcp_write_work[word_index]);
                while word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    word &= word - 1;
                    self.flush_tcp(word_index * u64::BITS as usize + bit);
                }
            }
            self.flush_activity(Instant::now());
            self.events.submit_all(&mut self.event_buf);
            if self.shutting_down {
                break;
            }

            let timeout = if self.udp_work != 0 || self.tcp_work_pending() {
                Duration::ZERO
            } else {
                POLL_TIMEOUT.min(
                    self.next_activity_flush
                        .saturating_duration_since(Instant::now()),
                )
            };
            match self.poll.poll(&mut self.poll_events, Some(timeout)) {
                Ok(()) => {}
                Err(error) if is_interrupted_io_error(&error) => continue,
                Err(error) => return Err(error),
            }
            for event in self.poll_events.iter() {
                let ready = MioReady::from_event(event);
                let token = event.token().0;
                if let Some(slot) = token.checked_sub(TCP_CONN_BASE)
                    && slot < MAX_TCP_VOICE_CONNS
                {
                    let Some(conn) = self.tcp_conns[slot].as_mut() else {
                        continue;
                    };
                    if ready.readable_like() {
                        conn.readiness.mark_ready();
                        set_slot_bit(&mut self.tcp_read_work, slot);
                    }
                    if ready.writable_like() {
                        conn.write_blocked = false;
                        if !conn.write_queue.is_empty() {
                            set_slot_bit(&mut self.tcp_write_work, slot);
                        }
                    }
                    continue;
                }
                if !ready.readable_like() {
                    continue;
                }
                // Media sockets own tokens below `udp.len()` and the probe owns
                // `PROBE_BIT`; the command waker sits above both and maps to no
                // bit. The arms cannot alias because the bind count is capped.
                if token < self.udp.len() || (token == UDP_PROBE.0 && self.udp_probe.is_some()) {
                    self.udp_work |= 1 << token;
                }
            }
        }
        Ok(())
    }

    fn drain_commands(&mut self) {
        debug_assert!(self.command_buf.is_empty());
        self.commands.drain_into(&mut self.command_buf);
        let mut commands = std::mem::take(&mut self.command_buf);
        for command in commands.drain(..) {
            match command {
                VoiceCommand::RegisterSession {
                    session_id,
                    user_id,
                    protection,
                } => self.register_session(session_id, user_id, protection),
                VoiceCommand::SetRoute { session_id, route } => {
                    self.set_route(session_id, route.map(Into::into));
                }
                VoiceCommand::RemoveSession { session_id } => self.remove_session(session_id),
                VoiceCommand::AttachTcp {
                    socket,
                    addr,
                    read_buf,
                    permit,
                } => self.attach_tcp_conn(socket, addr, read_buf, permit),
                VoiceCommand::Shutdown => self.shutting_down = true,
            }
        }
        self.command_buf = commands;
    }

    fn register_session(
        &mut self,
        session_id: SessionId,
        user_id: UserId,
        protection: MediaProtection,
    ) {
        let route_id = protection.route_id();
        if let Some(existing) = self.route_to_session.get(&route_id).copied()
            && existing != session_id
        {
            kvlog::error!(
                "voice relay route collision",
                route_id,
                existing_session_id = existing.0,
                rejected_session_id = session_id.0
            );
            return;
        }
        if self.sessions.contains_key(&session_id) {
            self.remove_session(session_id);
        }
        let Some(slot) = self.session_slots.iter().position(Option::is_none) else {
            kvlog::error!(
                "voice relay session capacity exhausted",
                session_id = session_id.0,
                max_sessions = MAX_SESSIONS
            );
            return;
        };
        let now = Instant::now();
        self.session_slots[slot] = Some(session_id);
        self.route_to_session.insert(route_id, session_id);
        self.sessions.insert(
            session_id,
            VoiceSession {
                slot,
                user_id,
                protection,
                recv_replay: AntiReplay::new(),
                send_counter: 0,
                udp_addr: None,
                udp_bind_index: 0,
                tcp_conn: None,
                route: None,
                budget: MediaBudget::new(now),
                last_activity: now,
                reported_rtt_ms: None,
                rtt_reported_at: None,
                activity_dirty: false,
            },
        );
    }

    fn set_route(&mut self, session_id: SessionId, route: Option<SessionRoute>) {
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let old = session.route;
        let slot = session.slot;
        if old == route {
            return;
        }
        if let Some(old) = old {
            let remove_room = if let Some(room) = self.rooms.get_mut(&old.room_id) {
                room.remove(slot);
                room.is_empty()
            } else {
                false
            };
            if self.stream_owners.get(&(old.room_id, old.stream_id)) == Some(&session_id) {
                self.stream_owners.remove(&(old.room_id, old.stream_id));
            }
            if remove_room {
                self.rooms.remove(&old.room_id);
            }
        }
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.route = route;
        } else {
            return;
        }
        if let Some(route) = route {
            let room = self.rooms.entry(route.room_id).or_default();
            room.insert(slot);
            self.stream_owners
                .insert((route.room_id, route.stream_id), session_id);
        }
    }

    fn remove_session(&mut self, session_id: SessionId) {
        self.set_route(session_id, None);
        if let Some(session) = self.sessions.remove(&session_id) {
            self.session_slots[session.slot] = None;
            self.route_to_session.remove(&session.protection.route_id());
            if let Some(slot) = session.tcp_conn {
                self.detach_tcp_conn(slot, "session removed");
            }
        }
    }

    fn tcp_work_pending(&self) -> bool {
        self.tcp_read_work.iter().any(|word| *word != 0)
            || self.tcp_write_work.iter().any(|word| *word != 0)
    }

    /// Takes ownership of a handed-over lane. `permit` was reserved by the
    /// control loop before it released its own pre-auth slot, so the cap is
    /// already enforced; dropping it here is what releases a rejected lane.
    fn attach_tcp_conn(
        &mut self,
        mut socket: TcpStream,
        addr: SocketAddr,
        read_buf: RecvBuffer,
        permit: TcpHandoffPermit,
    ) {
        let Some(slot) = self.tcp_conns.iter().position(Option::is_none) else {
            kvlog::warn!("voice tcp conn rejected, no free slot", addr = %addr);
            return;
        };
        if let Err(error) = self.poll.registry().register(
            &mut socket,
            Token(TCP_CONN_BASE + slot),
            Interest::READABLE | Interest::WRITABLE,
        ) {
            kvlog::warn!("voice tcp conn register failed", addr = %addr, error = %error);
            return;
        }
        if let Ok(local_addr) = socket.local_addr()
            && let Err(_error) = rpc::qos::apply_voice_qos(socket.as_raw_fd(), local_addr)
        {
            kvlog::debug!("voice tcp qos unavailable", addr = %addr, error = %_error);
        }
        self.tcp_conns[slot] = Some(TcpVoiceConn {
            socket,
            addr,
            read_buf,
            // Handed-over bytes may already hold whole frames and registration
            // only edges on new data, so the lane starts primed with read work.
            readiness: Readiness::primed(),
            write_queue: WriteQueue::new(),
            write_blocked: false,
            session: None,
            permit: Some(permit),
            attached_at: Instant::now(),
            backlog_since: None,
        });
        set_slot_bit(&mut self.tcp_read_work, slot);
    }

    fn detach_tcp_conn(&mut self, slot: usize, reason: &str) {
        let Some(mut conn) = self.tcp_conns[slot].take() else {
            return;
        };
        if let Err(_error) = self.poll.registry().deregister(&mut conn.socket) {
            kvlog::debug!("voice tcp deregister failed", slot, error = %_error);
        }
        clear_slot_bit(&mut self.tcp_read_work, slot);
        clear_slot_bit(&mut self.tcp_write_work, slot);
        match conn.session {
            Some(session_id) => {
                if let Some(session) = self.sessions.get_mut(&session_id)
                    && session.tcp_conn == Some(slot)
                {
                    session.tcp_conn = None;
                }
                kvlog::info!(
                    "voice tcp conn detached",
                    session_id = session_id.0,
                    addr = %conn.addr,
                    reason
                );
            }
            None => kvlog::info!("voice tcp conn detached", addr = %conn.addr, reason),
        }
    }

    fn bind_tcp_conn(&mut self, slot: usize, session_id: SessionId) {
        let Some(conn) = self.tcp_conns[slot].as_mut() else {
            return;
        };
        if conn.session.is_some() {
            return;
        }
        conn.session = Some(session_id);
        conn.permit = None;
        let previous = {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                return;
            };
            session.tcp_conn.replace(slot)
        };
        if let Some(previous) = previous
            && previous != slot
        {
            self.detach_tcp_conn(previous, "superseded by a newer voice tcp conn");
        }
        kvlog::info!("voice tcp conn bound", session_id = session_id.0, slot);
    }

    /// Drains one ready lane, mirroring [`VoiceRelay::receive`]: budgeted fill,
    /// then every whole frame handled from the buffer in place. Any protocol or
    /// stream error detaches the lane; unlike UDP there is no per-datagram
    /// tolerance because TCP already guarantees integrity.
    fn receive_tcp(&mut self, slot: usize) {
        let Some(conn) = self.tcp_conns[slot].as_mut() else {
            return;
        };
        let outcome = match read_into_buffer(
            &conn.socket,
            &mut conn.read_buf,
            &mut conn.readiness,
            MAX_UDP_DATAGRAM_BYTES,
            ReadLimit::ByteBudget(VOICE_TCP_READ_BUDGET_BYTES),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                kvlog::warn!("voice tcp read failed", slot, error = %error);
                self.detach_tcp_conn(slot, "read error");
                return;
            }
        };
        if outcome.hit_limit {
            set_slot_bit(&mut self.tcp_read_work, slot);
        }
        let mut read_buf = std::mem::take(&mut conn.read_buf);
        loop {
            let total = match frame::parse_frame_with_limit(
                read_buf.pending(),
                media::VOICE_TCP_MAX_FRAME_BYTES,
            ) {
                Ok(Some((_, total))) => total,
                Ok(None) => break,
                Err(error) => {
                    kvlog::warn!("voice tcp frame invalid", slot, error = %error);
                    self.detach_tcp_conn(slot, "invalid frame");
                    return;
                }
            };
            let packet = &mut read_buf.pending_mut()[frame::LENGTH_PREFIX_LEN..total];
            if let Err(error) = self.handle_media_packet(PacketOrigin::Tcp { slot }, packet) {
                kvlog::warn!("voice tcp packet rejected", slot, error = %error);
                self.detach_tcp_conn(slot, "rejected packet");
                return;
            }
            read_buf.consume(total);
        }
        let Some(conn) = self.tcp_conns[slot].as_mut() else {
            return;
        };
        conn.read_buf = read_buf;
        if outcome.disconnected {
            self.detach_tcp_conn(slot, "peer closed");
        }
    }

    fn flush_tcp(&mut self, slot: usize) {
        let Some(conn) = self.tcp_conns[slot].as_mut() else {
            return;
        };
        if conn.write_blocked || conn.write_queue.is_empty() {
            return;
        }
        match write_queue_to(
            &mut conn.socket,
            &mut conn.write_queue,
            VOICE_TCP_WRITE_ATTEMPTS,
        ) {
            Ok(outcome) => {
                let now = Instant::now();
                if conn.write_queue.is_empty() {
                    conn.backlog_since = None;
                }
                if outcome.blocked {
                    conn.write_blocked = true;
                } else if outcome.hit_limit {
                    set_slot_bit(&mut self.tcp_write_work, slot);
                }
                if outcome.wrote_zero {
                    self.detach_tcp_conn(slot, "write returned zero");
                } else {
                    self.detach_stale_tcp_backlog(slot, now);
                }
            }
            Err(error) => {
                kvlog::warn!("voice tcp write failed", slot, error = %error);
                self.detach_tcp_conn(slot, "write error");
            }
        }
    }

    fn sweep_unbound_tcp_conns(&mut self, now: Instant) {
        if self.handoff_budget.held() == 0 {
            return;
        }
        for slot in 0..self.tcp_conns.len() {
            let Some(conn) = &self.tcp_conns[slot] else {
                continue;
            };
            if conn.session.is_none()
                && now.saturating_duration_since(conn.attached_at) >= VOICE_TCP_AUTH_TIMEOUT
            {
                self.detach_tcp_conn(slot, "authentication timeout");
            }
        }
    }

    /// Drains one ready socket: media bind `bit`, or the probe at
    /// [`PROBE_BIT`]. Re-arms `bit` and returns once the drain budget is spent
    /// so no single socket can hold the loop.
    fn receive(&mut self, bit: u32) {
        let probe = bit == PROBE_BIT;
        if probe && self.udp_probe.is_none() {
            return;
        }
        // A probe datagram proves nothing about where a session can be reached,
        // so it never selects a reply socket and always reports bind 0.
        let server_probe_id = u8::from(probe);
        let bind_index = if probe { 0 } else { bit as usize };
        let mut buf = std::mem::take(&mut self.recv_buf);
        let mut datagrams = 0;
        loop {
            if datagrams >= UDP_DRAIN_BUDGET {
                self.udp_work |= 1 << bit;
                break;
            }
            let received = if probe {
                recv_udp_datagram(self.udp_probe.as_ref().expect("probe socket"), &mut buf)
            } else {
                recv_udp_datagram(&self.udp[bind_index], &mut buf)
            };
            let (len, src) = match received {
                Ok(Some(received)) => received,
                Ok(None) => break,
                Err(error) => {
                    kvlog::warn!(
                        "udp receive failed",
                        bind_index,
                        probe = server_probe_id,
                        error = %error
                    );
                    break;
                }
            };
            datagrams += 1;
            if let Err(error) =
                self.handle_packet(server_probe_id, bind_index, src, &mut buf[..len])
            {
                kvlog::warn!(
                    "udp packet rejected",
                    addr = %src,
                    packet_size = len,
                    error = %error
                );
            }
        }
        self.recv_buf = buf;
    }

    fn handle_packet(
        &mut self,
        server_probe_id: u8,
        main_bind_index: usize,
        src: SocketAddr,
        packet: &mut [u8],
    ) -> Result<(), PacketError> {
        self.handle_media_packet(
            PacketOrigin::Udp {
                server_probe_id,
                bind_index: main_bind_index,
                src,
            },
            packet,
        )
    }

    fn handle_media_packet(
        &mut self,
        origin: PacketOrigin,
        packet: &mut [u8],
    ) -> Result<(), PacketError> {
        let wire_bytes = packet.len();
        let (header, _) = media::parse_header(packet)?;
        if !self.p2p_enabled
            && header.kind == media::KIND_NAT_PROBE
            && matches!(origin, PacketOrigin::Udp { .. })
        {
            let session_id = *self
                .route_to_session
                .get(&header.route_id)
                .ok_or(PacketError::UnknownRoute)?;
            let session = self
                .sessions
                .get(&session_id)
                .ok_or(PacketError::UnknownSession)?;
            return if matches!(session.protection, MediaProtection::Clear { .. }) {
                Err(PacketError::NatProbeUnavailable)
            } else {
                Ok(())
            };
        }
        let session_id = *self
            .route_to_session
            .get(&header.route_id)
            .ok_or(PacketError::UnknownRoute)?;
        let (payload, udp_addr_changed, admitted) = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(PacketError::UnknownSession)?;
            match origin {
                PacketOrigin::Udp { src, .. } => {
                    if matches!(session.protection, MediaProtection::Clear { .. })
                        && header.kind != media::KIND_BIND
                        && session.udp_addr != Some(src)
                    {
                        return Err(PacketError::UnboundSource);
                    }
                }
                PacketOrigin::Tcp { slot } => {
                    let conn = self.tcp_conns[slot]
                        .as_ref()
                        .ok_or(PacketError::UnknownSession)?;
                    match conn.session {
                        Some(bound) if bound != session_id => {
                            return Err(PacketError::SessionMismatch);
                        }
                        // TCP stream continuity replaces the UDP source-address
                        // gate, but only once a proven Bind has tied the lane
                        // to its session.
                        None if header.kind != media::KIND_BIND => {
                            return Err(PacketError::UnauthenticatedConn);
                        }
                        _ => {}
                    }
                }
            }
            let opened =
                media::open_media_in_place(&session.protection, &mut session.recv_replay, packet)?;
            let udp_addr_changed = match origin {
                PacketOrigin::Udp {
                    server_probe_id,
                    bind_index,
                    src,
                } => match opened.address_proof {
                    media::AddressProof::AuthenticatedDatagram
                    | media::AddressProof::AuthenticatedAddressClaim => {
                        if server_probe_id == 0 {
                            let old = session.udp_addr.replace(src);
                            session.udp_bind_index = bind_index;
                            let address_changed = old.is_some_and(|old| old != src);
                            if address_changed {
                                session.reported_rtt_ms = None;
                                session.rtt_reported_at = None;
                            }
                            address_changed
                        } else {
                            false
                        }
                    }
                    media::AddressProof::None => {
                        debug_assert_eq!(session.udp_addr, Some(src));
                        false
                    }
                },
                // A lane frame proves nothing about where the session's UDP
                // datagrams can be delivered.
                PacketOrigin::Tcp { .. } => false,
            };
            let now = Instant::now();
            // Charged once per authenticated frame, before any fan-out, and the
            // same for both transports: relaying costs one seal per recipient
            // however the frame arrived, and only the UDP side of that is
            // limited by the network.
            let admitted = match header.kind {
                media::KIND_VOICE => session.budget.admit_voice(now, wire_bytes),
                media::KIND_VOICE_FEEDBACK => session.budget.admit_feedback(now),
                _ => session.budget.admit_signal(now),
            };
            session.last_activity = now;
            session.activity_dirty = true;
            (opened.payload, udp_addr_changed, admitted)
        };
        if let PacketOrigin::Tcp { slot } = origin {
            self.bind_tcp_conn(slot, session_id);
        }
        if !admitted {
            return Ok(());
        }

        match payload {
            MediaPayloadRef::Bind => {
                // Bind is the client's explicit acknowledgement handshake. An
                // earlier authenticated ping/probe may already have installed
                // the same address, so address change detection cannot gate it.
                // Probe-socket binds are never a usable relay address, and a
                // lane Bind proves nothing about the UDP path: `UdpBound` is
                // the client's UDP-recovery signal and must stay UDP-only.
                if let PacketOrigin::Udp {
                    server_probe_id: 0,
                    src,
                    ..
                } = origin
                {
                    self.event_buf.udp_bound.insert(session_id, src);
                }
                Ok(())
            }
            MediaPayloadRef::NatProbe { probe_id } => {
                let PacketOrigin::Udp {
                    server_probe_id,
                    src,
                    ..
                } = origin
                else {
                    return Ok(());
                };
                // A server without P2P has no probe socket or consumer for this
                // observation; its packets returned immediately after header
                // parsing and transport-mode lookup, before crypto, replay, or
                // activity tracking.
                if !self.p2p_enabled {
                    return Ok(());
                }
                let probe_id = probe_id.max(server_probe_id);
                if probe_id > 1 {
                    return Err(PacketError::UnknownNatProbe);
                }
                // Each client ICE restart needs fresh observations even if the
                // main public tuple stayed unchanged. The shared batch still
                // coalesces repeated packets while the control loop is busy.
                self.event_buf.nat_probe.insert((session_id, probe_id), src);
                Ok(())
            }
            MediaPayloadRef::Voice {
                stream_id,
                sequence,
                timestamp,
                flags,
                payload,
            } => self.relay_voice(session_id, stream_id, sequence, timestamp, flags, payload),
            MediaPayloadRef::VoiceFeedback {
                stream_id,
                feedback,
            } => self.relay_feedback(session_id, stream_id, feedback),
            MediaPayloadRef::Ping {
                nonce,
                observed_rtt_ms,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.reported_rtt_ms = if udp_addr_changed {
                        None
                    } else {
                        observed_rtt_ms
                    };
                    session.rtt_reported_at = Some(Instant::now());
                }
                self.send_pong(session_id, origin, nonce);
                Ok(())
            }
            MediaPayloadRef::PeerVoice { .. }
            | MediaPayloadRef::PeerVoiceFeedback { .. }
            | MediaPayloadRef::VoiceFeedbackFrom { .. }
            | MediaPayloadRef::Pong { .. } => Ok(()),
        }
    }

    fn relay_voice(
        &mut self,
        sender: SessionId,
        stream_id: StreamId,
        sequence: u32,
        timestamp: u32,
        flags: u8,
        voice: VoicePayloadRef<'_>,
    ) -> Result<(), PacketError> {
        let route = match self.sessions.get(&sender).and_then(|session| session.route) {
            Some(route) if route.stream_id == stream_id => route,
            _ => return Ok(()),
        };
        let mut recipients = std::mem::take(&mut self.relay_recipients);
        recipients.clear();
        if let Some(room) = self.rooms.get(&route.room_id) {
            for (word_index, word) in room.members.iter().copied().enumerate() {
                let mut remaining = word;
                while remaining != 0 {
                    let bit = remaining.trailing_zeros() as usize;
                    let slot = word_index * u64::BITS as usize + bit;
                    if let Some(recipient) = self.session_slots[slot]
                        && recipient != sender
                    {
                        recipients.push(recipient);
                    }
                    remaining &= remaining - 1;
                }
            }
        }
        let payload = MediaPayloadRef::Voice {
            stream_id,
            sequence,
            timestamp,
            flags,
            payload: voice,
        };
        if let MediaPayloadRef::Voice {
            stream_id,
            sequence,
            flags,
            payload,
            ..
        } = &payload
        {
            super::log_audio_pop_server_media_packet(
                "rx",
                sender,
                Some(route.room_id),
                *stream_id,
                *sequence,
                *flags,
                payload,
                Some(recipients.len()),
            );
        }
        for recipient in &recipients {
            self.send_payload(*recipient, &payload);
        }
        self.relay_recipients = recipients;
        Ok(())
    }

    fn relay_feedback(
        &mut self,
        reporter_session: SessionId,
        stream_id: StreamId,
        feedback: media::VoiceFeedback,
    ) -> Result<(), PacketError> {
        let Some(route) = self
            .sessions
            .get(&reporter_session)
            .and_then(|session| session.route)
        else {
            return Ok(());
        };
        let Some(owner) = self.stream_owners.get(&(route.room_id, stream_id)).copied() else {
            return Ok(());
        };
        if owner == reporter_session {
            return Ok(());
        }
        let Some(reporter) = self
            .sessions
            .get(&reporter_session)
            .map(|session| session.user_id)
        else {
            return Ok(());
        };
        self.send_payload(
            owner,
            &MediaPayloadRef::VoiceFeedbackFrom {
                reporter,
                stream_id,
                feedback,
            },
        );
        Ok(())
    }

    /// Sends over the session's current downstream path: its bound lane while
    /// one exists, its UDP address otherwise.
    fn send_payload(&mut self, session_id: SessionId, payload: &MediaPayloadRef<'_>) {
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let tcp_slot = session.tcp_conn;
        let udp_addr = session.udp_addr;
        let udp_bind_index = session.udp_bind_index;
        if let Some(slot) = tcp_slot {
            if !self.admit_tcp_send(slot) {
                return;
            }
            if !self.seal_for_session(session_id, payload) {
                return;
            }
            self.send_tcp_sealed(slot);
            return;
        }
        let Some(addr) = udp_addr else {
            return;
        };
        if !self.seal_for_session(session_id, payload) {
            return;
        }
        self.send_udp_sealed(session_id, udp_bind_index, addr);
    }

    /// Answers a probe on the transport it arrived on, so a client can prove
    /// the UDP path while a lane carries its media, and so an RTT sample never
    /// mixes the two directions across different transports.
    fn send_pong(&mut self, session_id: SessionId, origin: PacketOrigin, nonce: u64) {
        let payload = MediaPayloadRef::Pong { nonce };
        match origin {
            PacketOrigin::Udp {
                server_probe_id: 0,
                bind_index,
                src,
            } => {
                if !self.seal_for_session(session_id, &payload) {
                    return;
                }
                self.send_udp_sealed(session_id, bind_index, src);
            }
            PacketOrigin::Tcp { slot } => {
                if !self.admit_tcp_send(slot) {
                    return;
                }
                if !self.seal_for_session(session_id, &payload) {
                    return;
                }
                self.send_tcp_sealed(slot);
            }
            // A probe-socket ping proves nothing about the media path, so it
            // follows the session's ordinary downstream route.
            PacketOrigin::Udp { .. } => self.send_payload(session_id, &payload),
        }
    }

    /// Seals `payload` into `udp_send_packet` under the session's keys,
    /// consuming one send counter. `false` means there is nothing to send.
    fn seal_for_session(&mut self, session_id: SessionId, payload: &MediaPayloadRef<'_>) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        if let MediaPayloadRef::Voice {
            stream_id,
            sequence,
            flags,
            payload,
            ..
        } = payload
        {
            super::log_audio_pop_server_media_packet(
                "tx",
                session_id,
                session.route.map(|route| route.room_id),
                *stream_id,
                *sequence,
                *flags,
                payload,
                None,
            );
        }
        let counter = session.send_counter;
        session.send_counter = session.send_counter.wrapping_add(1);
        if let Err(error) = media::seal_media_ref_into(
            &session.protection,
            counter,
            payload,
            &mut self.udp_send_packet,
            &mut self.udp_send_scratch,
        ) {
            kvlog::warn!("media seal failed", session_id = session_id.0, error = %error);
            return false;
        }
        true
    }

    fn send_udp_sealed(&mut self, session_id: SessionId, bind_index: usize, addr: SocketAddr) {
        if let Err(error) = self.udp[bind_index].send_to(&self.udp_send_packet, addr)
            && error.kind() != io::ErrorKind::WouldBlock
        {
            kvlog::warn!(
                "udp send failed",
                session_id = session_id.0,
                addr = %addr,
                packet_size = self.udp_send_packet.len(),
                error = %error
            );
        }
    }

    /// Reserves room for a maximum media frame before spending a counter or
    /// doing crypto. A congested FIFO cannot discard its already-written
    /// prefix safely, so the only freshness-preserving recovery is to detach
    /// the stale lane.
    fn admit_tcp_send(&mut self, slot: usize) -> bool {
        if self.detach_stale_tcp_backlog(slot, Instant::now()) {
            return false;
        }
        let Some(conn) = self.tcp_conns[slot].as_ref() else {
            return false;
        };
        if conn.write_queue.len() <= VOICE_TCP_WRITE_CAP_BYTES - VOICE_TCP_MAX_WIRE_FRAME_BYTES {
            return true;
        }
        self.detach_tcp_conn(slot, "write backlog overflow");
        false
    }

    /// Detaches a lane whose queue has been occupied for
    /// [`VOICE_TCP_STALE_BACKLOG`], reporting whether it did.
    ///
    /// Checked when queueing and after each flush rather than on a timer: a
    /// backlog with nothing fresh behind it harms no one, and a lane carrying a
    /// room's speech is written to every 20 ms.
    fn detach_stale_tcp_backlog(&mut self, slot: usize, now: Instant) -> bool {
        let stale = self.tcp_conns[slot].as_ref().is_some_and(|conn| {
            conn.backlog_since.is_some_and(|since| {
                now.saturating_duration_since(since) >= VOICE_TCP_STALE_BACKLOG
            })
        });
        if stale {
            self.detach_tcp_conn(slot, "stale backlog");
        }
        stale
    }

    /// Queues the already-sealed packet in `udp_send_packet` onto an admitted
    /// lane.
    fn send_tcp_sealed(&mut self, slot: usize) {
        let Some(conn) = self.tcp_conns[slot].as_mut() else {
            return;
        };
        if frame::encode_frame(&self.udp_send_packet, conn.write_queue.tail_mut()).is_err() {
            debug_assert!(false, "sealed media exceeds the control frame cap");
            return;
        }
        if conn.backlog_since.is_none() {
            conn.backlog_since = Some(Instant::now());
        }
        self.flush_tcp(slot);
    }

    fn flush_activity(&mut self, now: Instant) {
        if now < self.next_activity_flush {
            return;
        }
        self.next_activity_flush = now + ACTIVITY_FLUSH_INTERVAL;
        self.sweep_unbound_tcp_conns(now);
        for (session_id, session) in &mut self.sessions {
            // Reported on the tick rather than per packet: a flood is the case
            // that produces these, and it must not also flood the log.
            if session.budget.dropped > 0 {
                kvlog::warn!(
                    "voice ingress rate limited",
                    session_id = session_id.0,
                    dropped = session.budget.dropped
                );
                session.budget.dropped = 0;
            }
            if !session.activity_dirty {
                continue;
            }
            session.activity_dirty = false;
            self.event_buf.activity.insert(
                *session_id,
                VoiceActivity {
                    last_activity: session.last_activity,
                    reported_rtt_ms: session.reported_rtt_ms,
                    rtt_reported_at: session.rtt_reported_at,
                },
            );
        }
    }
}

fn recv_udp_datagram(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<Option<(usize, SocketAddr)>> {
    rpc::evented::recv_datagram_with(buf, |buf| socket.recv_from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashbrown::HashSet;
    use rpc::{
        crypto::{KEY_LEN, KeyMaterial},
        media::{MediaPayload, VoiceFeedback, VoicePayload},
    };
    use std::io::{Read, Write};
    use std::net::{
        TcpListener as StdTcpListener, TcpStream as StdTcpStream, UdpSocket as StdUdpSocket,
    };

    fn key(byte: u8) -> KeyMaterial {
        KeyMaterial {
            id: u32::from(byte),
            bytes: [byte; KEY_LEN],
        }
    }

    fn protection(route_id: u32) -> MediaProtection {
        MediaProtection::Aead {
            route_id,
            send: key(4),
            recv: key(4),
        }
    }

    struct DirectRelay {
        _control_poll: Poll,
        relay: VoiceRelay,
    }

    impl DirectRelay {
        fn new(p2p_enabled: bool) -> Self {
            Self::with_udp_count(p2p_enabled, 1)
        }

        fn with_udp_count(p2p_enabled: bool, udp_count: usize) -> Self {
            let control_poll = Poll::new().unwrap();
            let control_waker = Arc::new(Waker::new(control_poll.registry(), Token(9)).unwrap());
            let control_notifier = Arc::new(EventNotifier::new(control_waker));
            let poll = Poll::new().unwrap();
            let command_waker = Arc::new(Waker::new(poll.registry(), COMMANDS).unwrap());
            let commands = Arc::new(EventSubmission::new(command_waker));
            let events = Arc::new(VoiceEventSubmission::new(control_notifier));
            let udp = (0..udp_count)
                .map(|_| UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap())
                .collect::<Vec<_>>();
            Self {
                _control_poll: control_poll,
                relay: VoiceRelay::new(
                    poll,
                    udp,
                    None,
                    commands,
                    events,
                    TcpHandoffBudget::new(),
                    p2p_enabled,
                ),
            }
        }

        fn reserve(&self) -> TcpHandoffPermit {
            self.relay
                .handoff_budget
                .reserve()
                .expect("handoff permit available")
        }
    }

    #[test]
    fn relay_preallocates_bounded_realtime_state() {
        let direct = DirectRelay::new(false);
        assert!(direct.relay.sessions.capacity() >= MAX_SESSIONS);
        assert!(direct.relay.route_to_session.capacity() >= MAX_SESSIONS);
        assert!(direct.relay.rooms.capacity() >= MAX_SESSIONS);
        assert!(direct.relay.stream_owners.capacity() >= MAX_SESSIONS);
        assert!(direct.relay.command_buf.capacity() >= MAX_SESSIONS);
        assert!(direct.relay.relay_recipients.capacity() >= MAX_SESSIONS);
        assert!(direct.relay.udp_send_packet.capacity() >= media::MAX_SEALED_MEDIA_BYTES);
        assert!(direct.relay.udp_send_scratch.capacity() >= media::SAFE_UDP_PAYLOAD_BYTES);
        assert!(direct.relay.event_buf.udp_bound.capacity() >= MAX_SESSIONS);
        assert!(direct.relay.event_buf.nat_probe.capacity() >= MAX_SESSIONS * 2);
        assert!(direct.relay.event_buf.activity.capacity() >= MAX_SESSIONS);
        // Lane slots are preallocated; per-lane buffers are created at attach,
        // on the control plane, never per packet.
        assert_eq!(direct.relay.tcp_conns.len(), MAX_TCP_VOICE_CONNS);
    }

    #[test]
    fn route_changes_update_stream_owner_index() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let first = SessionRoute {
            room_id: RoomId(2),
            stream_id: StreamId(10),
        };
        let second = SessionRoute {
            room_id: RoomId(3),
            stream_id: StreamId(11),
        };
        direct
            .relay
            .register_session(session_id, UserId(9), protection(77));

        direct.relay.set_route(session_id, Some(first));
        assert_eq!(
            direct
                .relay
                .stream_owners
                .get(&(first.room_id, first.stream_id)),
            Some(&session_id)
        );

        direct.relay.set_route(session_id, Some(second));
        assert!(
            !direct
                .relay
                .stream_owners
                .contains_key(&(first.room_id, first.stream_id))
        );
        assert_eq!(
            direct
                .relay
                .stream_owners
                .get(&(second.room_id, second.stream_id)),
            Some(&session_id)
        );

        direct.relay.set_route(session_id, None);
        assert!(
            !direct
                .relay
                .stream_owners
                .contains_key(&(second.room_id, second.stream_id))
        );
    }

    #[test]
    fn replies_use_the_udp_bind_that_received_the_session() {
        let mut direct = DirectRelay::with_udp_count(false, 2);
        let session_id = SessionId(1);
        let codec = protection(91);
        direct
            .relay
            .register_session(session_id, UserId(1), codec.clone());
        let client = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();

        direct
            .relay
            .handle_packet(0, 1, client.local_addr().unwrap(), &mut bind)
            .unwrap();
        direct
            .relay
            .send_payload(session_id, &MediaPayloadRef::Pong { nonce: 7 });

        let mut packet = [0; 2048];
        let (_, source) = client.recv_from(&mut packet).unwrap();
        assert_eq!(source, direct.relay.udp[1].local_addr().unwrap());
    }

    #[test]
    fn event_submission_wakes_and_swaps_reusable_vec() {
        let mut poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(7)).unwrap());
        let submission = EventSubmission::new(waker);

        // Seed the shared side with this allocation through an empty drain.
        let mut reusable = Vec::<u32>::with_capacity(8);
        let allocation = reusable.as_ptr();
        submission.drain_into(&mut reusable);
        assert_eq!(reusable.capacity(), 0);

        submission.submit(42);
        let mut events = Events::with_capacity(1);
        poll.poll(&mut events, Some(Duration::from_secs(1)))
            .unwrap();
        assert!(events.iter().any(|event| event.token() == Token(7)));

        submission.drain_into(&mut reusable);
        assert_eq!(reusable, vec![42]);
        assert_eq!(reusable.as_ptr(), allocation);
    }

    #[test]
    fn voice_events_keep_only_latest_observation_per_key() {
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(7)).unwrap());
        let notifier = Arc::new(EventNotifier::new(waker));
        let submission = VoiceEventSubmission::new(notifier);
        let session_id = SessionId(1);
        let first_addr: SocketAddr = "203.0.113.1:4000".parse().unwrap();
        let latest_addr: SocketAddr = "203.0.113.1:5000".parse().unwrap();

        let mut first = VoiceEventBatch::default();
        first.udp_bound.insert(session_id, first_addr);
        first.nat_probe.insert((session_id, 1), first_addr);
        first.activity.insert(
            session_id,
            VoiceActivity {
                last_activity: Instant::now(),
                reported_rtt_ms: Some(10),
                rtt_reported_at: Some(Instant::now()),
            },
        );
        submission.submit_all(&mut first);

        let mut latest = VoiceEventBatch::default();
        latest.udp_bound.insert(session_id, latest_addr);
        latest.nat_probe.insert((session_id, 1), latest_addr);
        latest.activity.insert(
            session_id,
            VoiceActivity {
                last_activity: Instant::now(),
                reported_rtt_ms: Some(20),
                rtt_reported_at: Some(Instant::now()),
            },
        );
        submission.submit_all(&mut latest);

        let mut drained = VoiceEventBatch::default();
        submission.drain_into(&mut drained);
        assert_eq!(
            drained.udp_bound,
            HashMap::from([(session_id, latest_addr)])
        );
        assert_eq!(
            drained.nat_probe,
            HashMap::from([((session_id, 1), latest_addr)])
        );
        assert_eq!(drained.activity.len(), 1);
        assert_eq!(
            drained.activity.get(&session_id).unwrap().reported_rtt_ms,
            Some(20)
        );
    }

    #[test]
    fn bind_is_acknowledged_after_another_packet_claims_the_address() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let client = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), protection(77));
        let receiver = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let src = receiver.local_addr().unwrap();
        let mut ping = media::seal_media(
            &client,
            1,
            &MediaPayload::Ping {
                nonce: 1,
                observed_rtt_ms: None,
            },
        )
        .unwrap();
        direct.relay.handle_packet(0, 0, src, &mut ping).unwrap();
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().udp_addr,
            Some(src)
        );

        let mut bind = media::seal_media(&client, 2, &MediaPayload::Bind).unwrap();
        direct.relay.handle_packet(0, 0, src, &mut bind).unwrap();
        assert_eq!(
            direct.relay.event_buf.udp_bound.get(&session_id),
            Some(&src)
        );
    }

    #[test]
    fn disabled_p2p_skips_probe_before_crypto_and_activity_tracking() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let client = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), protection(77));
        let src: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let mut probe =
            media::seal_media(&client, 1, &MediaPayload::NatProbe { probe_id: 0 }).unwrap();
        direct.relay.handle_packet(0, 0, src, &mut probe).unwrap();
        assert!(
            !direct
                .relay
                .sessions
                .get(&session_id)
                .unwrap()
                .activity_dirty
        );
        assert!(direct.relay.event_buf.nat_probe.is_empty());

        // Reusing the skipped probe's counter proves it never entered replay
        // protection or media decryption.
        let mut bind = media::seal_media(&client, 1, &MediaPayload::Bind).unwrap();
        direct.relay.handle_packet(0, 0, src, &mut bind).unwrap();
        assert_eq!(
            direct.relay.event_buf.udp_bound.get(&session_id),
            Some(&src)
        );
    }

    #[test]
    fn repeated_nat_probe_replaces_the_previous_observation() {
        let mut direct = DirectRelay::new(true);
        let session_id = SessionId(1);
        let client = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), protection(77));
        let first_addr: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let latest_addr: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        for (counter, addr) in [(1, first_addr), (2, latest_addr)] {
            let mut probe =
                media::seal_media(&client, counter, &MediaPayload::NatProbe { probe_id: 1 })
                    .unwrap();
            direct.relay.handle_packet(1, 0, addr, &mut probe).unwrap();
        }
        assert_eq!(direct.relay.event_buf.nat_probe.len(), 1);
        assert_eq!(
            direct.relay.event_buf.nat_probe.get(&(session_id, 1)),
            Some(&latest_addr)
        );
    }

    #[test]
    fn probe_socket_does_not_replace_main_media_address() {
        let mut direct = DirectRelay::new(true);
        let session_id = SessionId(1);
        let client = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), protection(77));
        let media_addr: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let mut bind = media::seal_media(&client, 1, &MediaPayload::Bind).unwrap();
        direct
            .relay
            .handle_packet(0, 0, media_addr, &mut bind)
            .unwrap();
        {
            let session = direct.relay.sessions.get_mut(&session_id).unwrap();
            session.reported_rtt_ms = Some(40);
            session.rtt_reported_at = Some(Instant::now());
        }

        let probe_addr: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        let mut probe =
            media::seal_media(&client, 2, &MediaPayload::NatProbe { probe_id: 1 }).unwrap();
        direct
            .relay
            .handle_packet(1, 0, probe_addr, &mut probe)
            .unwrap();
        let session = direct.relay.sessions.get(&session_id).unwrap();
        assert_eq!(session.udp_addr, Some(media_addr));
        assert_eq!(session.reported_rtt_ms, Some(40));

        let mut main_probe =
            media::seal_media(&client, 3, &MediaPayload::NatProbe { probe_id: 1 }).unwrap();
        direct
            .relay
            .handle_packet(0, 0, probe_addr, &mut main_probe)
            .unwrap();
        let session = direct.relay.sessions.get(&session_id).unwrap();
        assert_eq!(session.udp_addr, Some(probe_addr));
        assert_eq!(session.reported_rtt_ms, None);
    }

    #[test]
    fn plaintext_transport_requires_proven_bind_and_bound_source() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let bind_key = [9; KEY_LEN];
        direct.relay.register_session(
            session_id,
            UserId(9),
            MediaProtection::Clear {
                route_id: 88,
                bind_key,
            },
        );
        let client = MediaProtection::Clear {
            route_id: 88,
            bind_key,
        };
        let src: SocketAddr = "203.0.113.9:6000".parse().unwrap();
        let mut bind = media::seal_media(&client, 1, &MediaPayload::Bind).unwrap();
        direct.relay.handle_packet(0, 0, src, &mut bind).unwrap();
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().udp_addr,
            Some(src)
        );

        let spoof = MediaProtection::Clear {
            route_id: 88,
            bind_key: [1; KEY_LEN],
        };
        let evil: SocketAddr = "198.51.100.9:6000".parse().unwrap();
        let mut bind = media::seal_media(&spoof, 2, &MediaPayload::Bind).unwrap();
        assert!(direct.relay.handle_packet(0, 0, evil, &mut bind).is_err());

        let mut ping = media::seal_media(
            &client,
            3,
            &MediaPayload::Ping {
                nonce: 1,
                observed_rtt_ms: None,
            },
        )
        .unwrap();
        assert!(direct.relay.handle_packet(0, 0, evil, &mut ping).is_err());
        let mut probe =
            media::seal_media(&client, 4, &MediaPayload::NatProbe { probe_id: 0 }).unwrap();
        assert!(direct.relay.handle_packet(0, 0, src, &mut probe).is_err());
    }

    #[test]
    fn dedicated_thread_relays_while_control_side_does_not_drain() {
        let mut control_poll = Poll::new().unwrap();
        let control_waker = Arc::new(Waker::new(control_poll.registry(), Token(9)).unwrap());
        let control_notifier = Arc::new(EventNotifier::new(control_waker));
        let udp = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = udp.local_addr().unwrap();
        let relay = VoiceRelayHandle::spawn(vec![udp], None, control_notifier, false).unwrap();

        let alice_id = SessionId(1);
        let bob_id = SessionId(2);
        relay.submit(VoiceCommand::RegisterSession {
            session_id: alice_id,
            user_id: UserId(11),
            protection: protection(71),
        });
        relay.submit(VoiceCommand::RegisterSession {
            session_id: bob_id,
            user_id: UserId(22),
            protection: protection(72),
        });
        relay.submit(VoiceCommand::SetRoute {
            session_id: alice_id,
            route: Some(VoiceRoute {
                room_id: RoomId(3),
                stream_id: StreamId(101),
            }),
        });
        relay.submit(VoiceCommand::SetRoute {
            session_id: bob_id,
            route: Some(VoiceRoute {
                room_id: RoomId(3),
                stream_id: StreamId(102),
            }),
        });

        let alice = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let bob = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        bob.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let alice_codec = protection(71);
        let bob_codec = protection(72);
        let alice_bind = media::seal_media(&alice_codec, 1, &MediaPayload::Bind).unwrap();
        alice.send_to(&alice_bind, server_addr).unwrap();
        let bob_bind = media::seal_media(&bob_codec, 1, &MediaPayload::Bind).unwrap();
        bob.send_to(&bob_bind, server_addr).unwrap();

        // Wait until both binds reached the worker, then intentionally leave the
        // control-side event vector undrained while media continues.
        let mut poll_events = Events::with_capacity(4);
        let mut voice_events = VoiceEventBatch::default();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut bound = HashSet::new();
        while bound.len() != 2 && Instant::now() < deadline {
            control_poll
                .poll(&mut poll_events, Some(Duration::from_millis(20)))
                .unwrap();
            relay.drain_events(&mut voice_events);
            bound.extend(
                voice_events
                    .udp_bound
                    .drain()
                    .map(|(session_id, _)| session_id),
            );
        }
        assert_eq!(bound.len(), 2);

        let voice = MediaPayload::Voice {
            stream_id: StreamId(101),
            sequence: 7,
            timestamp: 960,
            flags: 0,
            payload: VoicePayload::Opus(vec![1, 2, 3, 4]),
        };
        let packet = media::seal_media(&alice_codec, 2, &voice).unwrap();
        alice.send_to(&packet, server_addr).unwrap();

        let mut datagram = [0; 2048];
        let (len, _) = bob.recv_from(&mut datagram).unwrap();
        let mut replay = AntiReplay::new();
        let opened = media::open_media(&bob_codec, &mut replay, &datagram[..len]).unwrap();
        assert_eq!(opened.payload, voice);
    }

    #[test]
    fn dedicated_thread_receives_and_replies_on_secondary_bind() {
        let mut control_poll = Poll::new().unwrap();
        let control_waker = Arc::new(Waker::new(control_poll.registry(), Token(9)).unwrap());
        let control_notifier = Arc::new(EventNotifier::new(control_waker));
        let primary = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let secondary = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let secondary_addr = secondary.local_addr().unwrap();
        let relay =
            VoiceRelayHandle::spawn(vec![primary, secondary], None, control_notifier, false)
                .unwrap();
        let session_id = SessionId(1);
        let codec = protection(81);
        relay.submit(VoiceCommand::RegisterSession {
            session_id,
            user_id: UserId(11),
            protection: codec.clone(),
        });

        let client = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        client.send_to(&bind, secondary_addr).unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut poll_events = Events::with_capacity(4);
        loop {
            control_poll
                .poll(&mut poll_events, Some(Duration::from_millis(20)))
                .unwrap();
            let mut voice_events = VoiceEventBatch::default();
            relay.drain_events(&mut voice_events);
            if voice_events.udp_bound.contains_key(&session_id) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "secondary UDP bind reached relay"
            );
        }

        let ping = media::seal_media(
            &codec,
            2,
            &MediaPayload::Ping {
                nonce: 7,
                observed_rtt_ms: None,
            },
        )
        .unwrap();
        client.send_to(&ping, secondary_addr).unwrap();
        let mut response = [0; 2048];
        let (_, source) = client.recv_from(&mut response).unwrap();

        assert_eq!(source, secondary_addr);
    }

    fn tcp_pair() -> (StdTcpStream, TcpStream) {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let client = StdTcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        (client, TcpStream::from_std(server))
    }

    fn attach_lane(direct: &mut DirectRelay) -> (StdTcpStream, usize) {
        let slot = direct
            .relay
            .tcp_conns
            .iter()
            .position(Option::is_none)
            .unwrap();
        let (client, server) = tcp_pair();
        let addr = server.peer_addr().unwrap();
        let permit = direct.reserve();
        direct
            .relay
            .attach_tcp_conn(server, addr, RecvBuffer::new(), permit);
        assert!(direct.relay.tcp_conns[slot].is_some());
        (client, slot)
    }

    fn send_frame(client: &mut StdTcpStream, packet: &[u8]) {
        let mut framed = Vec::new();
        frame::encode_frame(packet, &mut framed).unwrap();
        client.write_all(&framed).unwrap();
    }

    fn read_frame(client: &mut StdTcpStream) -> Vec<u8> {
        let mut prefix = [0u8; frame::LENGTH_PREFIX_LEN];
        client.read_exact(&mut prefix).unwrap();
        let mut payload = vec![0; u32::from_le_bytes(prefix) as usize];
        client.read_exact(&mut payload).unwrap();
        payload
    }

    /// Re-primes and drains the lane until `done` observes the expected state,
    /// absorbing loopback delivery latency without a running poll loop.
    fn pump_lane(
        direct: &mut DirectRelay,
        slot: usize,
        mut done: impl FnMut(&mut VoiceRelay) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(conn) = direct.relay.tcp_conns[slot].as_mut() {
                conn.readiness.mark_ready();
            }
            direct.relay.receive_tcp(slot);
            if done(&mut direct.relay) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "lane condition not reached in time"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn lane_bound(slot: usize) -> impl FnMut(&mut VoiceRelay) -> bool {
        move |relay| {
            relay.tcp_conns[slot]
                .as_ref()
                .is_some_and(|conn| conn.session.is_some())
        }
    }

    #[test]
    fn attach_binds_conn_after_first_sealed_bind_frame() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let (mut client, slot) = attach_lane(&mut direct);
        assert_eq!(direct.relay.handoff_budget.held(), 1);

        let bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));

        assert_eq!(
            direct.relay.tcp_conns[slot].as_ref().unwrap().session,
            Some(session_id)
        );
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().tcp_conn,
            Some(slot)
        );
        assert_eq!(direct.relay.handoff_budget.held(), 0);
        assert!(direct.relay.event_buf.udp_bound.is_empty());
    }

    #[test]
    fn first_tcp_frame_other_than_bind_detaches() {
        let mut direct = DirectRelay::new(false);
        let codec = protection(77);
        direct
            .relay
            .register_session(SessionId(1), UserId(9), codec.clone());
        let (mut client, slot) = attach_lane(&mut direct);

        let ping = media::seal_media(
            &codec,
            1,
            &MediaPayload::Ping {
                nonce: 1,
                observed_rtt_ms: None,
            },
        )
        .unwrap();
        send_frame(&mut client, &ping);
        pump_lane(&mut direct, slot, |relay| relay.tcp_conns[slot].is_none());
        assert_eq!(direct.relay.handoff_budget.held(), 0);
    }

    #[test]
    fn tcp_lane_voice_relays_to_udp_recipient() {
        let mut direct = DirectRelay::new(false);
        let sender = SessionId(1);
        let recipient = SessionId(2);
        let sender_codec = protection(71);
        let recipient_codec = protection(72);
        direct
            .relay
            .register_session(sender, UserId(11), sender_codec.clone());
        direct
            .relay
            .register_session(recipient, UserId(22), recipient_codec.clone());
        direct.relay.set_route(
            sender,
            Some(SessionRoute {
                room_id: RoomId(3),
                stream_id: StreamId(101),
            }),
        );
        direct.relay.set_route(
            recipient,
            Some(SessionRoute {
                room_id: RoomId(3),
                stream_id: StreamId(102),
            }),
        );

        let recipient_socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        recipient_socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut recipient_bind =
            media::seal_media(&recipient_codec, 1, &MediaPayload::Bind).unwrap();
        direct
            .relay
            .handle_packet(
                0,
                0,
                recipient_socket.local_addr().unwrap(),
                &mut recipient_bind,
            )
            .unwrap();

        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&sender_codec, 1, &MediaPayload::Bind).unwrap();
        let voice = MediaPayload::Voice {
            stream_id: StreamId(101),
            sequence: 7,
            timestamp: 960,
            flags: 0,
            payload: VoicePayload::Opus(vec![1, 2, 3, 4]),
        };
        let sealed_voice = media::seal_media(&sender_codec, 2, &voice).unwrap();
        send_frame(&mut client, &bind);
        send_frame(&mut client, &sealed_voice);
        pump_lane(&mut direct, slot, lane_bound(slot));

        let mut datagram = [0; 2048];
        let (len, _) = recipient_socket.recv_from(&mut datagram).unwrap();
        let mut replay = AntiReplay::new();
        let opened = media::open_media(&recipient_codec, &mut replay, &datagram[..len]).unwrap();
        assert_eq!(opened.payload, voice);
    }

    #[test]
    fn attached_conn_receives_downstream_instead_of_udp_until_close() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let udp_client = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        udp_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut udp_bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        direct
            .relay
            .handle_packet(0, 0, udp_client.local_addr().unwrap(), &mut udp_bind)
            .unwrap();

        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&codec, 2, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));

        direct
            .relay
            .send_payload(session_id, &MediaPayloadRef::Pong { nonce: 7 });
        let framed = read_frame(&mut client);
        let mut replay = AntiReplay::new();
        let opened = media::open_media(&codec, &mut replay, &framed).unwrap();
        assert_eq!(opened.payload, MediaPayload::Pong { nonce: 7 });

        drop(client);
        pump_lane(&mut direct, slot, |relay| relay.tcp_conns[slot].is_none());
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().tcp_conn,
            None
        );

        direct
            .relay
            .send_payload(session_id, &MediaPayloadRef::Pong { nonce: 8 });
        let mut datagram = [0; 2048];
        let (len, _) = udp_client.recv_from(&mut datagram).unwrap();
        let opened = media::open_media(&codec, &mut replay, &datagram[..len]).unwrap();
        assert_eq!(opened.payload, MediaPayload::Pong { nonce: 8 });
    }

    #[test]
    fn udp_bind_still_reports_bound_while_tcp_attached() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));
        assert!(direct.relay.event_buf.udp_bound.is_empty());

        let src: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let mut udp_bind = media::seal_media(&codec, 2, &MediaPayload::Bind).unwrap();
        direct
            .relay
            .handle_packet(0, 0, src, &mut udp_bind)
            .unwrap();
        assert_eq!(
            direct.relay.event_buf.udp_bound.get(&session_id),
            Some(&src)
        );
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().tcp_conn,
            Some(slot)
        );
    }

    /// A client on a lane keeps probing UDP to learn when it may switch back,
    /// so its UDP ping has to be answered over UDP even though the lane owns
    /// the session's downstream media.
    #[test]
    fn udp_ping_is_answered_over_udp_while_tcp_attached() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let udp_client = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        udp_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));

        let mut ping = media::seal_media(
            &codec,
            2,
            &MediaPayload::Ping {
                nonce: 21,
                observed_rtt_ms: None,
            },
        )
        .unwrap();
        direct
            .relay
            .handle_packet(0, 0, udp_client.local_addr().unwrap(), &mut ping)
            .unwrap();

        let mut datagram = [0; 2048];
        let (len, _) = udp_client.recv_from(&mut datagram).unwrap();
        let mut replay = AntiReplay::new();
        let opened = media::open_media(&codec, &mut replay, &datagram[..len]).unwrap();
        assert_eq!(opened.payload, MediaPayload::Pong { nonce: 21 });
        assert!(
            direct.relay.tcp_conns[slot]
                .as_ref()
                .unwrap()
                .write_queue
                .is_empty()
        );
    }

    #[test]
    fn unauthenticated_conn_times_out() {
        let mut direct = DirectRelay::new(false);
        let (_client, slot) = attach_lane(&mut direct);
        direct
            .relay
            .sweep_unbound_tcp_conns(Instant::now() + VOICE_TCP_AUTH_TIMEOUT);
        assert!(direct.relay.tcp_conns[slot].is_none());
        assert_eq!(direct.relay.handoff_budget.held(), 0);
    }

    #[test]
    fn pending_conn_cap_rejects_excess_attaches() {
        let mut direct = DirectRelay::new(false);
        let mut clients = Vec::new();
        for _ in 0..MAX_PENDING_TCP_VOICE {
            clients.push(attach_lane(&mut direct).0);
        }
        assert_eq!(direct.relay.handoff_budget.held(), MAX_PENDING_TCP_VOICE);

        assert!(direct.relay.handoff_budget.reserve().is_none());
        let attached = direct
            .relay
            .tcp_conns
            .iter()
            .filter(|conn| conn.is_some())
            .count();
        assert_eq!(attached, MAX_PENDING_TCP_VOICE);
        drop(clients);
    }

    /// The permit is what the control loop trades its own pre-auth slot for, so
    /// a handoff still queued for this thread has to hold the cap on its own.
    #[test]
    fn queued_handoff_permits_count_against_the_cap() {
        let direct = DirectRelay::new(false);
        let queued = (0..MAX_PENDING_TCP_VOICE)
            .map(|_| direct.reserve())
            .collect::<Vec<_>>();
        assert!(direct.relay.tcp_conns.iter().all(Option::is_none));
        assert!(direct.relay.handoff_budget.reserve().is_none());

        drop(queued);
        assert_eq!(direct.relay.handoff_budget.held(), 0);
        assert!(direct.relay.handoff_budget.reserve().is_some());
    }

    #[test]
    fn detached_lane_releases_its_permit() {
        let mut direct = DirectRelay::new(false);
        let (_client, slot) = attach_lane(&mut direct);
        assert_eq!(direct.relay.handoff_budget.held(), 1);
        direct.relay.detach_tcp_conn(slot, "test");
        assert_eq!(direct.relay.handoff_budget.held(), 0);
    }

    #[test]
    fn remove_session_closes_attached_conn() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));

        direct.relay.remove_session(session_id);
        assert!(direct.relay.tcp_conns[slot].is_none());
    }

    #[test]
    fn write_cap_detaches_instead_of_preserving_stale_voice() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));

        {
            let conn = direct.relay.tcp_conns[slot].as_mut().unwrap();
            conn.write_blocked = true;
            conn.write_queue
                .tail_mut()
                .extend_from_slice(&vec![0; VOICE_TCP_WRITE_CAP_BYTES + 1]);
        }
        direct.relay.send_payload(
            session_id,
            &MediaPayloadRef::Voice {
                stream_id: StreamId(101),
                sequence: 1,
                timestamp: 960,
                flags: 0,
                payload: VoicePayloadRef::Opus(&[1, 2, 3]),
            },
        );
        assert!(direct.relay.tcp_conns[slot].is_none());
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().tcp_conn,
            None
        );
    }

    #[test]
    fn write_cap_detaches_before_queueing_pong() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));

        {
            let conn = direct.relay.tcp_conns[slot].as_mut().unwrap();
            conn.write_blocked = true;
            conn.write_queue
                .tail_mut()
                .extend_from_slice(&vec![0; VOICE_TCP_WRITE_CAP_BYTES + 1]);
        }
        direct
            .relay
            .send_payload(session_id, &MediaPayloadRef::Pong { nonce: 7 });
        assert!(direct.relay.tcp_conns[slot].is_none());
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().tcp_conn,
            None
        );
    }

    /// A lane whose queue never drains is closed on age, not only on depth: at
    /// ordinary voice bitrates the byte cap is seconds of speech.
    #[test]
    fn stale_backlog_detaches_a_lane_far_under_the_write_cap() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));

        {
            let conn = direct.relay.tcp_conns[slot].as_mut().unwrap();
            conn.write_blocked = true;
            conn.write_queue.tail_mut().extend_from_slice(&[0; 64]);
            conn.backlog_since = Some(Instant::now() - VOICE_TCP_STALE_BACKLOG);
        }
        assert!(
            direct.relay.tcp_conns[slot]
                .as_ref()
                .unwrap()
                .write_queue
                .len()
                < VOICE_TCP_WRITE_CAP_BYTES
        );
        direct
            .relay
            .send_payload(session_id, &MediaPayloadRef::Pong { nonce: 7 });

        assert!(direct.relay.tcp_conns[slot].is_none());
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().tcp_conn,
            None
        );
    }

    #[test]
    fn queueing_a_frame_opens_the_backlog_window_and_draining_closes_it() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));

        direct.relay.tcp_conns[slot].as_mut().unwrap().write_blocked = true;
        direct
            .relay
            .send_payload(session_id, &MediaPayloadRef::Pong { nonce: 7 });
        assert!(
            direct.relay.tcp_conns[slot]
                .as_ref()
                .unwrap()
                .backlog_since
                .is_some()
        );

        direct.relay.tcp_conns[slot].as_mut().unwrap().write_blocked = false;
        direct.relay.flush_tcp(slot);
        assert_eq!(
            direct.relay.tcp_conns[slot].as_ref().unwrap().backlog_since,
            None
        );
        assert_eq!(
            read_frame(&mut client).len(),
            direct.relay.udp_send_packet.len()
        );
    }

    /// One session must not be able to spend the relay's fan-out budget just
    /// because TCP will carry frames as fast as it can write them.
    #[test]
    fn voice_ingress_over_budget_is_dropped_before_fan_out() {
        let mut direct = DirectRelay::new(false);
        let sender = SessionId(1);
        let recipient = SessionId(2);
        let sender_codec = protection(71);
        let recipient_codec = protection(72);
        direct
            .relay
            .register_session(sender, UserId(11), sender_codec.clone());
        direct
            .relay
            .register_session(recipient, UserId(22), recipient_codec.clone());
        direct.relay.set_route(
            sender,
            Some(SessionRoute {
                room_id: RoomId(3),
                stream_id: StreamId(101),
            }),
        );
        direct.relay.set_route(
            recipient,
            Some(SessionRoute {
                room_id: RoomId(3),
                stream_id: StreamId(102),
            }),
        );
        // Non-blocking: the drain below must not spend wall-clock time, or the
        // buckets would refill while the test waits on an empty socket.
        let recipient_socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        recipient_socket.set_nonblocking(true).unwrap();
        let mut recipient_bind =
            media::seal_media(&recipient_codec, 1, &MediaPayload::Bind).unwrap();
        direct
            .relay
            .handle_packet(
                0,
                0,
                recipient_socket.local_addr().unwrap(),
                &mut recipient_bind,
            )
            .unwrap();
        let sender_addr: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let mut sender_bind = media::seal_media(&sender_codec, 1, &MediaPayload::Bind).unwrap();
        direct
            .relay
            .handle_packet(0, 0, sender_addr, &mut sender_bind)
            .unwrap();

        let mut relayed = 0;
        let mut datagram = [0; 2048];
        for counter in 2..u64::from(VOICE_INGRESS_PACKETS_PER_SEC) * 4 {
            let mut voice = media::seal_media(
                &sender_codec,
                counter,
                &MediaPayload::Voice {
                    stream_id: StreamId(101),
                    sequence: counter as u32,
                    timestamp: counter as u32 * 960,
                    flags: 0,
                    payload: VoicePayload::Opus(vec![7; 200]),
                },
            )
            .unwrap();
            direct
                .relay
                .handle_packet(0, 0, sender_addr, &mut voice)
                .unwrap();
            while recipient_socket.recv_from(&mut datagram).is_ok() {
                relayed += 1;
            }
        }

        // A burst can spend a full second of budget, and no more: the rest is
        // dropped before the room is walked.
        assert!(
            relayed <= VOICE_INGRESS_PACKETS_PER_SEC as usize + 2,
            "relayed {relayed} of {} voice frames",
            VOICE_INGRESS_PACKETS_PER_SEC * 4
        );
        let budget = &direct.relay.sessions.get(&sender).unwrap().budget;
        assert!(budget.dropped > 0);
    }

    #[test]
    fn voice_bytes_budget_limits_oversized_frames_before_the_packet_budget() {
        let now = Instant::now();
        let mut budget = MediaBudget::new(now);
        let frame = media::MAX_SEALED_MEDIA_BYTES;
        let affordable = VOICE_INGRESS_BYTES_PER_SEC as usize / frame;
        for _ in 0..affordable {
            assert!(budget.admit_voice(now, frame));
        }
        assert!(!budget.admit_voice(now, frame));
        assert!(
            budget.voice_packets.tokens > 0,
            "packet budget was untouched"
        );

        // Tokens accrue with time rather than with calls.
        assert!(budget.admit_voice(now + Duration::from_secs(1), frame));
    }

    /// A session that has spent its voice budget must still be measurable and
    /// able to reclaim its address, or the limiter would take out the session
    /// along with the flood.
    #[test]
    fn pings_and_feedback_keep_their_own_allowance() {
        let now = Instant::now();
        let mut budget = MediaBudget::new(now);
        while budget.admit_voice(now, 200) {}

        assert!(budget.admit_signal(now));
        assert!(budget.admit_feedback(now));
    }

    #[test]
    fn replayed_frame_on_tcp_lane_is_rejected() {
        let mut direct = DirectRelay::new(false);
        let session_id = SessionId(1);
        let codec = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), codec.clone());
        let udp_src: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let mut udp_bind = media::seal_media(&codec, 1, &MediaPayload::Bind).unwrap();
        direct
            .relay
            .handle_packet(0, 0, udp_src, &mut udp_bind)
            .unwrap();
        let ping = media::seal_media(
            &codec,
            5,
            &MediaPayload::Ping {
                nonce: 1,
                observed_rtt_ms: None,
            },
        )
        .unwrap();
        let mut replayed = ping.clone();
        direct
            .relay
            .handle_packet(0, 0, udp_src, &mut replayed)
            .unwrap();

        let (mut client, slot) = attach_lane(&mut direct);
        let bind = media::seal_media(&codec, 2, &MediaPayload::Bind).unwrap();
        send_frame(&mut client, &bind);
        pump_lane(&mut direct, slot, lane_bound(slot));

        send_frame(&mut client, &ping);
        pump_lane(&mut direct, slot, |relay| relay.tcp_conns[slot].is_none());
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().tcp_conn,
            None
        );
    }

    #[test]
    fn dedicated_thread_relays_tcp_lane_voice_and_replies_over_lane() {
        let control_poll = Poll::new().unwrap();
        let control_waker = Arc::new(Waker::new(control_poll.registry(), Token(9)).unwrap());
        let control_notifier = Arc::new(EventNotifier::new(control_waker));
        let udp = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = udp.local_addr().unwrap();
        let relay = VoiceRelayHandle::spawn(vec![udp], None, control_notifier, false).unwrap();

        let alice_id = SessionId(1);
        let bob_id = SessionId(2);
        let alice_codec = protection(71);
        let bob_codec = protection(72);
        relay.submit(VoiceCommand::RegisterSession {
            session_id: alice_id,
            user_id: UserId(11),
            protection: alice_codec.clone(),
        });
        relay.submit(VoiceCommand::RegisterSession {
            session_id: bob_id,
            user_id: UserId(22),
            protection: bob_codec.clone(),
        });
        relay.submit(VoiceCommand::SetRoute {
            session_id: alice_id,
            route: Some(VoiceRoute {
                room_id: RoomId(3),
                stream_id: StreamId(101),
            }),
        });
        relay.submit(VoiceCommand::SetRoute {
            session_id: bob_id,
            route: Some(VoiceRoute {
                room_id: RoomId(3),
                stream_id: StreamId(102),
            }),
        });

        let bob = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        bob.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let bob_bind = media::seal_media(&bob_codec, 1, &MediaPayload::Bind).unwrap();
        bob.send_to(&bob_bind, server_addr).unwrap();

        let (mut alice, server_end) = tcp_pair();
        let addr = server_end.peer_addr().unwrap();
        relay.submit(VoiceCommand::AttachTcp {
            socket: server_end,
            addr,
            read_buf: RecvBuffer::new(),
            permit: relay.reserve_tcp_handoff().unwrap(),
        });
        send_frame(
            &mut alice,
            &media::seal_media(&alice_codec, 1, &MediaPayload::Bind).unwrap(),
        );
        send_frame(
            &mut alice,
            &media::seal_media(
                &alice_codec,
                2,
                &MediaPayload::Ping {
                    nonce: 9,
                    observed_rtt_ms: None,
                },
            )
            .unwrap(),
        );
        let pong = read_frame(&mut alice);
        let mut alice_replay = AntiReplay::new();
        let opened = media::open_media(&alice_codec, &mut alice_replay, &pong).unwrap();
        assert_eq!(opened.payload, MediaPayload::Pong { nonce: 9 });

        let voice = MediaPayload::Voice {
            stream_id: StreamId(101),
            sequence: 7,
            timestamp: 960,
            flags: 0,
            payload: VoicePayload::Opus(vec![1, 2, 3, 4]),
        };
        send_frame(
            &mut alice,
            &media::seal_media(&alice_codec, 3, &voice).unwrap(),
        );
        let mut datagram = [0; 2048];
        let (len, _) = bob.recv_from(&mut datagram).unwrap();
        let mut bob_replay = AntiReplay::new();
        let opened = media::open_media(&bob_codec, &mut bob_replay, &datagram[..len]).unwrap();
        assert_eq!(opened.payload, voice);
    }

    #[test]
    fn feedback_is_stamped_with_reporter_identity() {
        let mut direct = DirectRelay::new(false);
        let receiver = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let owner = SessionId(1);
        let reporter = SessionId(2);
        let stream = StreamId(100);
        direct
            .relay
            .register_session(owner, UserId(9), protection(77));
        direct
            .relay
            .register_session(reporter, UserId(5), protection(78));
        direct.relay.set_route(
            owner,
            Some(SessionRoute {
                room_id: RoomId(1),
                stream_id: stream,
            }),
        );
        direct.relay.set_route(
            reporter,
            Some(SessionRoute {
                room_id: RoomId(1),
                stream_id: StreamId(101),
            }),
        );
        let owner_client = protection(77);
        let mut bind = media::seal_media(&owner_client, 1, &MediaPayload::Bind).unwrap();
        direct
            .relay
            .handle_packet(0, 0, receiver.local_addr().unwrap(), &mut bind)
            .unwrap();

        let feedback = VoiceFeedback {
            lost_packets: 3,
            max_neteq_playout_delay_ms: 120,
            ..Default::default()
        };
        direct
            .relay
            .relay_feedback(reporter, stream, feedback)
            .unwrap();

        let mut buf = [0; 2048];
        let (len, _) = receiver.recv_from(&mut buf).unwrap();
        let mut replay = AntiReplay::new();
        let opened = media::open_media(&owner_client, &mut replay, &buf[..len]).unwrap();
        assert_eq!(
            opened.payload,
            MediaPayload::VoiceFeedbackFrom {
                reporter: UserId(5),
                stream_id: stream,
                feedback,
            }
        );
    }
}
