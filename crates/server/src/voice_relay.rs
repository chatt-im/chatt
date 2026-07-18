//! Dedicated UDP media event loop.
//!
//! The control loop submits infrequent session/topology changes through one
//! [`EventSubmission`]. The UDP loop owns every mutable packet-plane value and
//! returns only coalesced control-plane observations through
//! [`VoiceEventSubmission`]. Both queues wake their consumer's `mio::Poll`, and
//! consumers swap reusable buffers so draining does not allocate.

use hashbrown::HashMap;
use mio::{Events, Interest, Poll, Token, Waker, net::UdpSocket};
use rpc::{
    crypto::{AntiReplay, TAG_LEN},
    evented::{MioReady, is_interrupted_io_error},
    ids::{RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayloadRef, MediaProtection, VoicePayloadRef},
};
use std::{
    io,
    net::SocketAddr,
    os::fd::AsRawFd,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::event_queue::{EventNotifier, VOICE_EVENTS};

const UDP: Token = Token(0);
const UDP_PROBE: Token = Token(1);
const COMMANDS: Token = Token(2);
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const UDP_DRAIN_BUDGET: usize = 64;
const ACTIVITY_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_SESSIONS: usize = super::MAX_CLIENTS;
const SESSION_WORDS: usize = MAX_SESSIONS.div_ceil(u64::BITS as usize);
const MAX_UDP_PACKET_BYTES: usize = media::UDP_HEADER_LEN + media::SAFE_UDP_PAYLOAD_BYTES + TAG_LEN;

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
    thread: Option<JoinHandle<()>>,
    udp_local_addr: SocketAddr,
}

impl VoiceRelayHandle {
    pub(super) fn spawn(
        mut udp: UdpSocket,
        mut udp_probe: Option<UdpSocket>,
        control_notifier: Arc<EventNotifier>,
        p2p_enabled: bool,
    ) -> io::Result<Self> {
        let udp_local_addr = udp.local_addr()?;
        if let Err(error) = rpc::qos::apply_voice_qos(udp.as_raw_fd(), udp_local_addr) {
            kvlog::warn!(
                "voice udp qos unavailable",
                addr = %udp_local_addr,
                dscp = rpc::qos::VOICE_DSCP,
                error = %error
            );
        }
        let poll = Poll::new()?;
        poll.registry()
            .register(&mut udp, UDP, Interest::READABLE)?;
        if let Some(probe) = udp_probe.as_mut() {
            poll.registry()
                .register(probe, UDP_PROBE, Interest::READABLE)?;
        }
        let command_waker = Arc::new(Waker::new(poll.registry(), COMMANDS)?);
        let commands = Arc::new(EventSubmission::new(command_waker));
        let events = Arc::new(VoiceEventSubmission::new(control_notifier));
        let loop_commands = Arc::clone(&commands);
        let loop_events = Arc::clone(&events);
        let thread = thread::Builder::new()
            .name("chatt-voice-relay".to_string())
            .spawn(move || {
                let mut relay = VoiceRelay::new(
                    poll,
                    udp,
                    udp_probe,
                    loop_commands,
                    loop_events,
                    p2p_enabled,
                );
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
            })?;
        Ok(Self {
            commands,
            events,
            thread: Some(thread),
            udp_local_addr,
        })
    }

    pub(super) fn submit(&self, command: VoiceCommand) {
        self.commands.submit(command);
    }

    pub(super) fn drain_events(&self, events: &mut VoiceEventBatch) {
        self.events.drain_into(events);
    }

    pub(super) fn local_addr(&self) -> SocketAddr {
        self.udp_local_addr
    }
}

impl Drop for VoiceRelayHandle {
    fn drop(&mut self) {
        self.commands.submit(VoiceCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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

struct VoiceSession {
    slot: usize,
    user_id: UserId,
    protection: MediaProtection,
    recv_replay: AntiReplay,
    send_counter: u64,
    udp_addr: Option<SocketAddr>,
    route: Option<SessionRoute>,
    last_activity: Instant,
    reported_rtt_ms: Option<u16>,
    rtt_reported_at: Option<Instant>,
    activity_dirty: bool,
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
    udp: UdpSocket,
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
    udp_send_packet: Vec<u8>,
    udp_send_scratch: Vec<u8>,
    udp_work: u8,
    shutting_down: bool,
}

#[derive(Debug)]
enum PacketError {
    Media(media::MediaError),
    UnknownRoute,
    UnknownSession,
    NatProbeUnavailable,
    UnboundSource,
    UnknownNatProbe,
}

impl std::fmt::Display for PacketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketError::Media(error) => error.fmt(f),
            PacketError::UnknownRoute => f.write_str("unknown UDP route id"),
            PacketError::UnknownSession => f.write_str("unknown UDP session"),
            PacketError::NatProbeUnavailable => {
                f.write_str("NAT probe not available in external-secure-link")
            }
            PacketError::UnboundSource => f.write_str("external-link media from an unbound source"),
            PacketError::UnknownNatProbe => f.write_str("unknown NAT probe id"),
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
        udp: UdpSocket,
        udp_probe: Option<UdpSocket>,
        commands: Arc<EventSubmission<VoiceCommand>>,
        events: Arc<VoiceEventSubmission>,
        p2p_enabled: bool,
    ) -> Self {
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
            udp_send_packet: Vec::with_capacity(MAX_UDP_PACKET_BYTES),
            udp_send_scratch: Vec::with_capacity(media::SAFE_UDP_PAYLOAD_BYTES),
            udp_work: 0,
            shutting_down: false,
        }
    }

    fn run(&mut self) -> io::Result<()> {
        while !self.shutting_down {
            self.drain_commands();
            if self.shutting_down {
                break;
            }
            for probe_id in UdpWork::take(&mut self.udp_work) {
                self.receive_udp(probe_id);
                self.drain_commands();
                if self.shutting_down {
                    break;
                }
            }
            self.flush_activity(Instant::now());
            self.events.submit_all(&mut self.event_buf);
            if self.shutting_down {
                break;
            }

            let timeout = if self.udp_work != 0 {
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
                if !ready.readable_like() {
                    continue;
                }
                match event.token() {
                    UDP => self.udp_work |= 1,
                    UDP_PROBE => self.udp_work |= 2,
                    COMMANDS => {}
                    _ => {}
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
                route: None,
                last_activity: Instant::now(),
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
        }
    }

    fn receive_udp(&mut self, probe_id: u8) {
        let mut buf = [0u8; 2048];
        let mut datagrams = 0;
        loop {
            if datagrams >= UDP_DRAIN_BUDGET {
                self.udp_work |= 1 << probe_id;
                return;
            }
            let received = if probe_id == 0 {
                recv_udp_datagram(&self.udp, &mut buf)
            } else {
                let Some(probe) = self.udp_probe.as_ref() else {
                    return;
                };
                recv_udp_datagram(probe, &mut buf)
            };
            let (len, src) = match received {
                Ok(Some(received)) => received,
                Ok(None) => return,
                Err(error) => {
                    kvlog::warn!("udp receive failed", probe_id, error = %error);
                    return;
                }
            };
            datagrams += 1;
            if let Err(error) = self.handle_packet(probe_id, src, &mut buf[..len]) {
                kvlog::warn!(
                    "udp packet rejected",
                    addr = %src,
                    packet_size = len,
                    error = %error
                );
            }
        }
    }

    fn handle_packet(
        &mut self,
        server_probe_id: u8,
        src: SocketAddr,
        packet: &mut [u8],
    ) -> Result<(), PacketError> {
        let (header, _) = media::parse_header(packet)?;
        if !self.p2p_enabled && header.kind == media::KIND_NAT_PROBE {
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
        let (payload, udp_addr_changed) = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or(PacketError::UnknownSession)?;
            if matches!(session.protection, MediaProtection::Clear { .. })
                && header.kind != media::KIND_BIND
                && session.udp_addr != Some(src)
            {
                return Err(PacketError::UnboundSource);
            }
            let opened =
                media::open_media_in_place(&session.protection, &mut session.recv_replay, packet)?;
            let udp_addr_changed = match opened.address_proof {
                media::AddressProof::AuthenticatedDatagram
                | media::AddressProof::AuthenticatedAddressClaim => {
                    if server_probe_id == 0 {
                        let old = session.udp_addr.replace(src);
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
            };
            session.last_activity = Instant::now();
            session.activity_dirty = true;
            (opened.payload, udp_addr_changed)
        };

        match payload {
            MediaPayloadRef::Bind => {
                // Bind is the client's explicit acknowledgement handshake. An
                // earlier authenticated ping/probe may already have installed
                // the same address, so address change detection cannot gate it.
                // Probe-socket binds are never a usable relay address.
                if server_probe_id == 0 {
                    self.event_buf.udp_bound.insert(session_id, src);
                }
                Ok(())
            }
            MediaPayloadRef::NatProbe { probe_id } => {
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
                self.send_payload(session_id, &MediaPayloadRef::Pong { nonce });
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

    fn send_payload(&mut self, session_id: SessionId, payload: &MediaPayloadRef<'_>) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(addr) = session.udp_addr else {
            return;
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
            kvlog::warn!("udp seal failed", session_id = session_id.0, error = %error);
            return;
        }
        if let Err(error) = self.udp.send_to(&self.udp_send_packet, addr)
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

    fn flush_activity(&mut self, now: Instant) {
        if now < self.next_activity_flush {
            return;
        }
        self.next_activity_flush = now + ACTIVITY_FLUSH_INTERVAL;
        for (session_id, session) in &mut self.sessions {
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

struct UdpWork {
    mask: u8,
}

impl UdpWork {
    fn take(mask: &mut u8) -> Self {
        Self {
            mask: std::mem::take(mask),
        }
    }
}

impl Iterator for UdpWork {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        if self.mask & 1 != 0 {
            self.mask &= !1;
            return Some(0);
        }
        if self.mask & 2 != 0 {
            self.mask &= !2;
            return Some(1);
        }
        None
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
    use std::net::UdpSocket as StdUdpSocket;

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
            let control_poll = Poll::new().unwrap();
            let control_waker = Arc::new(Waker::new(control_poll.registry(), Token(9)).unwrap());
            let control_notifier = Arc::new(EventNotifier::new(control_waker));
            let poll = Poll::new().unwrap();
            let command_waker = Arc::new(Waker::new(poll.registry(), COMMANDS).unwrap());
            let commands = Arc::new(EventSubmission::new(command_waker));
            let events = Arc::new(VoiceEventSubmission::new(control_notifier));
            let udp = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            Self {
                _control_poll: control_poll,
                relay: VoiceRelay::new(poll, udp, None, commands, events, p2p_enabled),
            }
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
        assert!(direct.relay.udp_send_packet.capacity() >= MAX_UDP_PACKET_BYTES);
        assert!(direct.relay.udp_send_scratch.capacity() >= media::SAFE_UDP_PAYLOAD_BYTES);
        assert!(direct.relay.event_buf.udp_bound.capacity() >= MAX_SESSIONS);
        assert!(direct.relay.event_buf.nat_probe.capacity() >= MAX_SESSIONS * 2);
        assert!(direct.relay.event_buf.activity.capacity() >= MAX_SESSIONS);
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
        direct.relay.handle_packet(0, src, &mut ping).unwrap();
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().udp_addr,
            Some(src)
        );

        let mut bind = media::seal_media(&client, 2, &MediaPayload::Bind).unwrap();
        direct.relay.handle_packet(0, src, &mut bind).unwrap();
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
        direct.relay.handle_packet(0, src, &mut probe).unwrap();
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
        direct.relay.handle_packet(0, src, &mut bind).unwrap();
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
            direct.relay.handle_packet(1, addr, &mut probe).unwrap();
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
            .handle_packet(0, media_addr, &mut bind)
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
            .handle_packet(1, probe_addr, &mut probe)
            .unwrap();
        let session = direct.relay.sessions.get(&session_id).unwrap();
        assert_eq!(session.udp_addr, Some(media_addr));
        assert_eq!(session.reported_rtt_ms, Some(40));

        let mut main_probe =
            media::seal_media(&client, 3, &MediaPayload::NatProbe { probe_id: 1 }).unwrap();
        direct
            .relay
            .handle_packet(0, probe_addr, &mut main_probe)
            .unwrap();
        let session = direct.relay.sessions.get(&session_id).unwrap();
        assert_eq!(session.udp_addr, Some(probe_addr));
        assert_eq!(session.reported_rtt_ms, None);
    }

    #[test]
    fn external_link_requires_proven_bind_and_bound_source() {
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
        direct.relay.handle_packet(0, src, &mut bind).unwrap();
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
        assert!(direct.relay.handle_packet(0, evil, &mut bind).is_err());

        let mut ping = media::seal_media(
            &client,
            3,
            &MediaPayload::Ping {
                nonce: 1,
                observed_rtt_ms: None,
            },
        )
        .unwrap();
        assert!(direct.relay.handle_packet(0, evil, &mut ping).is_err());
        let mut probe =
            media::seal_media(&client, 4, &MediaPayload::NatProbe { probe_id: 0 }).unwrap();
        assert!(direct.relay.handle_packet(0, src, &mut probe).is_err());
    }

    #[test]
    fn dedicated_thread_relays_while_control_side_does_not_drain() {
        let mut control_poll = Poll::new().unwrap();
        let control_waker = Arc::new(Waker::new(control_poll.registry(), Token(9)).unwrap());
        let control_notifier = Arc::new(EventNotifier::new(control_waker));
        let udp = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = udp.local_addr().unwrap();
        let relay = VoiceRelayHandle::spawn(udp, None, control_notifier, false).unwrap();

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
        thread::sleep(Duration::from_millis(25));

        let mut datagram = [0; 2048];
        let (len, _) = bob.recv_from(&mut datagram).unwrap();
        let mut replay = AntiReplay::new();
        let opened = media::open_media(&bob_codec, &mut replay, &datagram[..len]).unwrap();
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
            .handle_packet(0, receiver.local_addr().unwrap(), &mut bind)
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
