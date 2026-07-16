//! Dedicated UDP media event loop.
//!
//! The control loop submits infrequent session/topology changes through one
//! [`EventSubmission`]. The UDP loop owns every mutable packet-plane value and
//! returns only coalesced control-plane observations through another. Both
//! queues wake their consumer's `mio::Poll`, and consumers swap the queued
//! `Vec` with a reusable local buffer so draining does not allocate.

use hashbrown::{HashMap, HashSet};
use mio::{Events, Interest, Poll, Token, Waker, net::UdpSocket};
use rpc::{
    crypto::AntiReplay,
    evented::{MioReady, is_interrupted_io_error},
    ids::{RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayload, MediaProtection},
};
use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const UDP: Token = Token(0);
const UDP_PROBE: Token = Token(1);
const COMMANDS: Token = Token(2);
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const UDP_DRAIN_BUDGET: usize = 64;
const ACTIVITY_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

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

    fn submit_all(&self, events: &mut Vec<T>) {
        if events.is_empty() {
            return;
        }
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.is_empty() {
                std::mem::swap(&mut *pending, events);
            } else {
                pending.append(events);
            }
        }
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
    pub(super) session_id: SessionId,
    pub(super) last_activity: Instant,
    pub(super) reported_rtt_ms: Option<u16>,
    pub(super) rtt_reported_at: Option<Instant>,
}

pub(super) enum VoiceEvent {
    UdpBound {
        session_id: SessionId,
        addr: SocketAddr,
    },
    NatProbe {
        session_id: SessionId,
        probe_id: u8,
        addr: SocketAddr,
    },
    Activity(VoiceActivity),
    Failed(String),
}

pub(super) struct VoiceRelayHandle {
    commands: Arc<EventSubmission<VoiceCommand>>,
    events: Arc<EventSubmission<VoiceEvent>>,
    thread: Option<JoinHandle<()>>,
    udp_local_addr: SocketAddr,
}

impl VoiceRelayHandle {
    pub(super) fn spawn(
        mut udp: UdpSocket,
        mut udp_probe: Option<UdpSocket>,
        control_waker: Arc<Waker>,
        p2p_enabled: bool,
    ) -> io::Result<Self> {
        let udp_local_addr = udp.local_addr()?;
        let poll = Poll::new()?;
        poll.registry()
            .register(&mut udp, UDP, Interest::READABLE)?;
        if let Some(probe) = udp_probe.as_mut() {
            poll.registry()
                .register(probe, UDP_PROBE, Interest::READABLE)?;
        }
        let command_waker = Arc::new(Waker::new(poll.registry(), COMMANDS)?);
        let commands = Arc::new(EventSubmission::new(command_waker));
        let events = Arc::new(EventSubmission::new(control_waker));
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
                if let Err(error) = relay.run() {
                    let message = error.to_string();
                    kvlog::error!("voice relay stopped", error = message.as_str());
                    relay.events.submit(VoiceEvent::Failed(message));
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

    pub(super) fn drain_events(&self, events: &mut Vec<VoiceEvent>) {
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
    user_id: UserId,
    protection: MediaProtection,
    recv_replay: AntiReplay,
    send_counter: u64,
    udp_addr: Option<SocketAddr>,
    route: Option<SessionRoute>,
    last_activity: Instant,
    reported_rtt_ms: Option<u16>,
    rtt_reported_at: Option<Instant>,
    nat_probe_observed: [bool; 2],
}

#[derive(Default)]
struct VoiceRoom {
    members: Vec<SessionId>,
    stream_owners: HashMap<StreamId, SessionId>,
}

struct VoiceRelay {
    poll: Poll,
    udp: UdpSocket,
    udp_probe: Option<UdpSocket>,
    commands: Arc<EventSubmission<VoiceCommand>>,
    events: Arc<EventSubmission<VoiceEvent>>,
    p2p_enabled: bool,
    sessions: HashMap<SessionId, VoiceSession>,
    route_to_session: HashMap<u32, SessionId>,
    rooms: HashMap<RoomId, VoiceRoom>,
    dirty_activity: HashSet<SessionId>,
    next_activity_flush: Instant,
    command_buf: Vec<VoiceCommand>,
    event_buf: Vec<VoiceEvent>,
    relay_recipients: Vec<SessionId>,
    udp_send_packet: Vec<u8>,
    udp_send_scratch: Vec<u8>,
    udp_work: u8,
    shutting_down: bool,
}

impl VoiceRelay {
    fn new(
        poll: Poll,
        udp: UdpSocket,
        udp_probe: Option<UdpSocket>,
        commands: Arc<EventSubmission<VoiceCommand>>,
        events: Arc<EventSubmission<VoiceEvent>>,
        p2p_enabled: bool,
    ) -> Self {
        Self {
            poll,
            udp,
            udp_probe,
            commands,
            events,
            p2p_enabled,
            sessions: HashMap::new(),
            route_to_session: HashMap::new(),
            rooms: HashMap::new(),
            dirty_activity: HashSet::new(),
            next_activity_flush: Instant::now() + ACTIVITY_FLUSH_INTERVAL,
            command_buf: Vec::new(),
            event_buf: Vec::new(),
            relay_recipients: Vec::new(),
            udp_send_packet: Vec::new(),
            udp_send_scratch: Vec::new(),
            udp_work: 0,
            shutting_down: false,
        }
    }

    fn run(&mut self) -> io::Result<()> {
        let mut poll_events = Events::with_capacity(128);
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
            match self.poll.poll(&mut poll_events, Some(timeout)) {
                Ok(()) => {}
                Err(error) if is_interrupted_io_error(&error) => continue,
                Err(error) => return Err(error),
            }
            for event in poll_events.iter() {
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
        self.route_to_session.insert(route_id, session_id);
        self.sessions.insert(
            session_id,
            VoiceSession {
                user_id,
                protection,
                recv_replay: AntiReplay::new(),
                send_counter: 0,
                udp_addr: None,
                route: None,
                last_activity: Instant::now(),
                reported_rtt_ms: None,
                rtt_reported_at: None,
                nat_probe_observed: [false; 2],
            },
        );
    }

    fn set_route(&mut self, session_id: SessionId, route: Option<SessionRoute>) {
        let old = self.sessions.get(&session_id).and_then(|session| session.route);
        if old == route {
            return;
        }
        if let Some(old) = old {
            let remove_room = if let Some(room) = self.rooms.get_mut(&old.room_id) {
                room.members.retain(|member| *member != session_id);
                if room.stream_owners.get(&old.stream_id) == Some(&session_id) {
                    room.stream_owners.remove(&old.stream_id);
                }
                room.members.is_empty()
            } else {
                false
            };
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
            if !room.members.contains(&session_id) {
                room.members.push(session_id);
            }
            room.stream_owners.insert(route.stream_id, session_id);
        }
    }

    fn remove_session(&mut self, session_id: SessionId) {
        self.set_route(session_id, None);
        if let Some(session) = self.sessions.remove(&session_id) {
            self.route_to_session.remove(&session.protection.route_id());
        }
        self.dirty_activity.remove(&session_id);
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
            if let Err(error) = self.handle_packet(probe_id, src, &buf[..len]) {
                kvlog::warn!(
                    "udp packet rejected",
                    addr = %src,
                    packet_size = len,
                    error = error.as_str()
                );
            }
        }
    }

    fn handle_packet(
        &mut self,
        server_probe_id: u8,
        src: SocketAddr,
        packet: &[u8],
    ) -> Result<(), String> {
        let (header, _) = media::parse_header(packet).map_err(|error| error.to_string())?;
        let session_id = *self
            .route_to_session
            .get(&header.route_id)
            .ok_or_else(|| "unknown UDP route id".to_string())?;
        let (payload, udp_addr_changed, udp_binding_changed, external_link) = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or_else(|| "unknown UDP session".to_string())?;
            let external_link = matches!(session.protection, MediaProtection::Clear { .. });
            let opened = media::open_media(
                &session.protection,
                &mut session.recv_replay,
                packet,
            )
            .map_err(|error| error.to_string())?;
            let (udp_addr_changed, udp_binding_changed) = match opened.address_proof {
                media::AddressProof::AuthenticatedDatagram
                | media::AddressProof::AuthenticatedAddressClaim => {
                    if server_probe_id == 0 {
                        let old = session.udp_addr.replace(src);
                        let binding_changed = old != Some(src);
                        let address_changed = old.is_some_and(|old| old != src);
                        if address_changed {
                            session.reported_rtt_ms = None;
                            session.rtt_reported_at = None;
                            session.nat_probe_observed = [false; 2];
                        }
                        (address_changed, binding_changed)
                    } else {
                        (false, false)
                    }
                }
                media::AddressProof::None => {
                    if session.udp_addr != Some(src) {
                        return Err("external-link media from an unbound source".to_string());
                    }
                    (false, false)
                }
            };
            session.last_activity = Instant::now();
            (
                opened.payload,
                udp_addr_changed,
                udp_binding_changed,
                external_link,
            )
        };
        self.dirty_activity.insert(session_id);

        match payload {
            MediaPayload::Bind => {
                if udp_binding_changed {
                    self.event_buf.push(VoiceEvent::UdpBound {
                        session_id,
                        addr: src,
                    });
                }
                Ok(())
            }
            MediaPayload::NatProbe { probe_id } => {
                if external_link {
                    return Err("NAT probe not available in external-secure-link".into());
                }
                if !self.p2p_enabled {
                    return Ok(());
                }
                let probe_id = probe_id.max(server_probe_id);
                let Some(observed) = self
                    .sessions
                    .get_mut(&session_id)
                    .and_then(|session| session.nat_probe_observed.get_mut(usize::from(probe_id)))
                else {
                    return Err("unknown NAT probe id".into());
                };
                if !*observed {
                    *observed = true;
                    self.event_buf.push(VoiceEvent::NatProbe {
                        session_id,
                        probe_id,
                        addr: src,
                    });
                }
                Ok(())
            }
            MediaPayload::Voice {
                stream_id,
                sequence,
                timestamp,
                flags,
                payload,
            } => self.relay_voice(
                session_id,
                stream_id,
                sequence,
                timestamp,
                flags,
                payload,
            ),
            MediaPayload::VoiceFeedback {
                stream_id,
                feedback,
            } => self.relay_feedback(session_id, stream_id, feedback),
            MediaPayload::Ping {
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
                self.send_payload(session_id, &MediaPayload::Pong { nonce });
                Ok(())
            }
            MediaPayload::PeerVoice { .. }
            | MediaPayload::PeerVoiceFeedback { .. }
            | MediaPayload::VoiceFeedbackFrom { .. }
            | MediaPayload::Pong { .. } => Ok(()),
        }
    }

    fn relay_voice(
        &mut self,
        sender: SessionId,
        stream_id: StreamId,
        sequence: u32,
        timestamp: u32,
        flags: u8,
        voice: media::VoicePayload,
    ) -> Result<(), String> {
        let route = match self.sessions.get(&sender).and_then(|session| session.route) {
            Some(route) if route.stream_id == stream_id => route,
            _ => return Ok(()),
        };
        let mut recipients = std::mem::take(&mut self.relay_recipients);
        recipients.clear();
        if let Some(room) = self.rooms.get(&route.room_id) {
            recipients.extend(
                room.members
                    .iter()
                    .copied()
                    .filter(|recipient| *recipient != sender),
            );
        }
        let payload = MediaPayload::Voice {
            stream_id,
            sequence,
            timestamp,
            flags,
            payload: voice,
        };
        if let MediaPayload::Voice {
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
    ) -> Result<(), String> {
        let Some(route) = self
            .sessions
            .get(&reporter_session)
            .and_then(|session| session.route)
        else {
            return Ok(());
        };
        let Some(owner) = self
            .rooms
            .get(&route.room_id)
            .and_then(|room| room.stream_owners.get(&stream_id).copied())
        else {
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
            &MediaPayload::VoiceFeedbackFrom {
                reporter,
                stream_id,
                feedback,
            },
        );
        Ok(())
    }

    fn send_payload(&mut self, session_id: SessionId, payload: &MediaPayload) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(addr) = session.udp_addr else {
            return;
        };
        if let MediaPayload::Voice {
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
        if let Err(error) = media::seal_media_into(
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
        for session_id in self.dirty_activity.drain() {
            let Some(session) = self.sessions.get(&session_id) else {
                continue;
            };
            self.event_buf.push(VoiceEvent::Activity(VoiceActivity {
                session_id,
                last_activity: session.last_activity,
                reported_rtt_ms: session.reported_rtt_ms,
                rtt_reported_at: session.rtt_reported_at,
            }));
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
    use rpc::{
        crypto::{KEY_LEN, KeyMaterial},
        media::{VoiceFeedback, VoicePayload},
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
            let poll = Poll::new().unwrap();
            let command_waker = Arc::new(Waker::new(poll.registry(), COMMANDS).unwrap());
            let commands = Arc::new(EventSubmission::new(command_waker));
            let events = Arc::new(EventSubmission::new(control_waker));
            let udp = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            Self {
                _control_poll: control_poll,
                relay: VoiceRelay::new(poll, udp, None, commands, events, p2p_enabled),
            }
        }
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
    fn probe_socket_does_not_replace_main_media_address() {
        let mut direct = DirectRelay::new(true);
        let session_id = SessionId(1);
        let client = protection(77);
        direct
            .relay
            .register_session(session_id, UserId(9), protection(77));
        let media_addr: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let bind = media::seal_media(&client, 1, &MediaPayload::Bind).unwrap();
        direct.relay.handle_packet(0, media_addr, &bind).unwrap();
        {
            let session = direct.relay.sessions.get_mut(&session_id).unwrap();
            session.reported_rtt_ms = Some(40);
            session.rtt_reported_at = Some(Instant::now());
        }

        let probe_addr: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        let probe = media::seal_media(
            &client,
            2,
            &MediaPayload::NatProbe { probe_id: 1 },
        )
        .unwrap();
        direct
            .relay
            .handle_packet(1, probe_addr, &probe)
            .unwrap();
        let session = direct.relay.sessions.get(&session_id).unwrap();
        assert_eq!(session.udp_addr, Some(media_addr));
        assert_eq!(session.reported_rtt_ms, Some(40));

        let main_probe = media::seal_media(
            &client,
            3,
            &MediaPayload::NatProbe { probe_id: 1 },
        )
        .unwrap();
        direct
            .relay
            .handle_packet(0, probe_addr, &main_probe)
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
        let bind = media::seal_media(&client, 1, &MediaPayload::Bind).unwrap();
        direct.relay.handle_packet(0, src, &bind).unwrap();
        assert_eq!(
            direct.relay.sessions.get(&session_id).unwrap().udp_addr,
            Some(src)
        );

        let spoof = MediaProtection::Clear {
            route_id: 88,
            bind_key: [1; KEY_LEN],
        };
        let evil: SocketAddr = "198.51.100.9:6000".parse().unwrap();
        let bind = media::seal_media(&spoof, 2, &MediaPayload::Bind).unwrap();
        assert!(direct.relay.handle_packet(0, evil, &bind).is_err());

        let ping = media::seal_media(
            &client,
            3,
            &MediaPayload::Ping {
                nonce: 1,
                observed_rtt_ms: None,
            },
        )
        .unwrap();
        assert!(direct.relay.handle_packet(0, evil, &ping).is_err());
        let probe = media::seal_media(
            &client,
            4,
            &MediaPayload::NatProbe { probe_id: 0 },
        )
        .unwrap();
        assert!(direct.relay.handle_packet(0, src, &probe).is_err());
    }

    #[test]
    fn dedicated_thread_relays_while_control_side_does_not_drain() {
        let mut control_poll = Poll::new().unwrap();
        let control_waker = Arc::new(Waker::new(control_poll.registry(), Token(9)).unwrap());
        let udp = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = udp.local_addr().unwrap();
        let relay = VoiceRelayHandle::spawn(udp, None, control_waker, false).unwrap();

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
        let mut voice_events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut bound = HashSet::new();
        while bound.len() != 2 && Instant::now() < deadline {
            control_poll
                .poll(&mut poll_events, Some(Duration::from_millis(20)))
                .unwrap();
            relay.drain_events(&mut voice_events);
            for event in voice_events.drain(..) {
                if let VoiceEvent::UdpBound { session_id, .. } = event {
                    bound.insert(session_id);
                }
            }
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
        let bind = media::seal_media(&owner_client, 1, &MediaPayload::Bind).unwrap();
        direct
            .relay
            .handle_packet(0, receiver.local_addr().unwrap(), &bind)
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
