//! Dedicated client UDP media event loop.
//!
//! The control worker submits ordered session and topology changes here. Audio
//! producers use the same queue directly for the four packet-plane commands,
//! so neither outbound voice nor playback feedback waits behind TCP, MLS, or
//! file work. The queue lock is held only while updating the pending deque;
//! the voice thread swaps it into a reusable buffer before doing any socket or
//! cryptographic work.

use hashbrown::{HashMap, HashSet};
use aws_lc_rs::rand::SecureRandom;
use chatt_p2p::{
    Action as P2pAction, AgentConfig as P2pAgentConfig, Candidate, CandidateKind,
    ReflexiveObservation, StunAuth, TraversalAgent,
    interfaces::InterfaceSnapshot,
    stun::StunMessage,
};
use mio::{Events, Interest, Poll, Token, Waker, net::UdpSocket};
use rpc::{
    control::{P2pCandidate, P2pNatKind, P2pPeerInfo, ParticipantServerRtt},
    crypto::{AntiReplay, KeyMaterial, TransportMode},
    ids::{RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayload, MediaProtection},
};
use std::{
    collections::VecDeque,
    io,
    net::{IpAddr, SocketAddr, UdpSocket as StdUdpSocket},
    os::fd::AsRawFd,
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::SendError,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    app::NetworkEventSender,
    audio::{LiveEncoderProfile, LivePlaybackFeedback, LivePlaybackSink, LocalVoiceFrame},
    config::CandidatePrivacy,
    mdns::MdnsSystem,
};

use super::{
    DIRECT_CONFIRM_WINDOW,
    RELAY_KEEPALIVE_INTERVAL, RTT_PROBE_INTERVAL, RestartPortPolicy, UDP_BIND_RETRY_INTERVAL,
    NetworkCommand,
    NetworkEvent, allocate_local_voice_sequence, advance_local_voice_sequence_past,
    audio_payload_from_media, configured_nat_kind, dispatch_voice_packet_to,
    live_feedback_from_media, log_audio_pop_media_packet, media_feedback_from_live,
    media_payload_from_audio, media_payload_kind, media_voice_payload_kind, random_u64,
    take_rtt_sample, fold_rtt_ewma, clamp_rtt_ms, voice_payload_kind,
    apply_candidate_privacy, candidate_from_control, candidate_from_control_with_addr,
    combined_relay_rtt, connection_id_from_p2p_username, control_nat_kind,
    ice_role_from_control, key_from_control, nat_from_control,
    p2p_peer_is_republish, p2p_username, push_rtt_in_flight,
    rtt_sample_is_stale, split_mdns_addr,
    DIRECT_FAILOVER_IDLE, P2P_CONSENT_TIMEOUT, P2P_KEEPALIVE_INTERVAL,
    UDP_BIND_FAILURE_ATTEMPTS, ENCODER_FEEDBACK_ALPHA, ENCODER_PROFILE_HOLD,
    INTERFACE_POLL_INTERVAL, MAX_RECENT_VOICE_SEQUENCES, RECENT_VOICE_SEQUENCE_WORD_BITS,
    RECENT_VOICE_SEQUENCE_WORDS,
};

const COMMANDS: Token = Token(0);
const UDP: Token = Token(1);
const MDNS_V4: Token = Token(2);
const MDNS_V6: Token = Token(3);
const IDLE_POLL_TIMEOUT: Duration = Duration::from_secs(60);
const P2P_POLL_TIMEOUT: Duration = Duration::from_millis(20);
const UDP_DRAIN_BUDGET: usize = 64;
const MDNS_DRAIN_BUDGET: usize = 32;
const MAX_COMMANDS_PER_TICK: usize = 64;
const MAX_QUEUED_CONTROL_COMMANDS: usize = 256;
const MAX_QUEUED_FEEDBACK_STREAMS: usize = 256;
const MAX_RECENT_VOICE_STREAMS: usize = 256;
/// Capture emits one packet every 20 ms. Retaining 10 packets bounds a stalled
/// producer queue to 200 ms; when full, the oldest microphone packet is
/// discarded so recovery sends current audio instead of a stale burst.
const MAX_QUEUED_MICROPHONE_PACKETS: usize = 10;

/// How inbound audio should behave when no playback sink is currently usable.
///
/// WebRTC keeps pulling receive streams through a null audio device while
/// device playout is disabled, which prevents NetEQ from accumulating old
/// audio. Chatt tears its playback graph down on deafen instead, so suspended
/// ingress discards packets until a fresh sink attaches. The distinct initial
/// state retains the short startup queue used while the first sink is opening.
#[derive(Clone, Default)]
enum PlaybackIngressState {
    #[default]
    Buffering,
    Attached(LivePlaybackSink),
    Suspended,
}

impl PlaybackIngressState {
    fn sink(&self) -> Option<&LivePlaybackSink> {
        match self {
            Self::Attached(sink) => Some(sink),
            Self::Buffering | Self::Suspended => None,
        }
    }

    fn buffer_without_sink(&self) -> bool {
        matches!(self, Self::Buffering)
    }

    fn from_sink_update(sink: Option<LivePlaybackSink>) -> Self {
        match sink {
            Some(sink) => Self::Attached(sink),
            None => Self::Suspended,
        }
    }
}

/// Candidate privacy output shared with focused candidate-policy tests.
pub(super) struct GatheredP2p {
    pub(super) local: Vec<Candidate>,
    pub(super) published: Vec<P2pCandidate>,
    pub(super) mdns_names: HashMap<String, IpAddr>,
}

#[derive(Debug)]
pub(super) struct InterfaceMonitor {
    snapshot: Option<InterfaceSnapshot>,
    next_poll: Instant,
}

impl InterfaceMonitor {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            snapshot: None,
            next_poll: now,
        }
    }

    pub(super) fn snapshot(&self) -> Option<&InterfaceSnapshot> {
        self.snapshot.as_ref()
    }

    pub(super) fn deactivate(&mut self, now: Instant) {
        self.snapshot = None;
        self.next_poll = now;
    }

    pub(super) fn next_wake(&self, now: Instant) -> Duration {
        self.next_poll.saturating_duration_since(now)
    }

    pub(super) fn ensure_with<F>(&mut self, now: Instant, capture: F) -> io::Result<()>
    where
        F: FnOnce() -> io::Result<InterfaceSnapshot>,
    {
        if self.snapshot.is_none() {
            let _ = self.refresh_with(now, capture)?;
        }
        Ok(())
    }

    pub(super) fn poll_with<F>(
        &mut self,
        active: bool,
        now: Instant,
        capture: F,
    ) -> io::Result<Option<bool>>
    where
        F: FnOnce() -> io::Result<InterfaceSnapshot>,
    {
        if !active {
            self.deactivate(now);
            return Ok(None);
        }
        self.refresh_with(now, capture)
    }

    /// Refreshes a due snapshot and reports whether it differs from the
    /// previous successful capture. A failed capture retains the previous
    /// baseline and is retried at the normal interval.
    fn refresh_with<F>(&mut self, now: Instant, capture: F) -> io::Result<Option<bool>>
    where
        F: FnOnce() -> io::Result<InterfaceSnapshot>,
    {
        if now < self.next_poll {
            return Ok(None);
        }
        self.next_poll = now + INTERFACE_POLL_INTERVAL;
        let snapshot = capture()?;
        let changed = self
            .snapshot
            .as_ref()
            .is_some_and(|previous| snapshot.changed_from(previous));
        self.snapshot = Some(snapshot);
        Ok(Some(changed))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecentVoiceSequenceResult {
    New,
    Duplicate,
    Stale,
}

#[derive(Debug)]
pub(super) struct RecentVoiceSequences {
    highest: Option<u32>,
    seen: [u64; RECENT_VOICE_SEQUENCE_WORDS],
    last_touched: u64,
}

impl Default for RecentVoiceSequences {
    fn default() -> Self {
        Self {
            highest: None,
            seen: [0; RECENT_VOICE_SEQUENCE_WORDS],
            last_touched: 0,
        }
    }
}

impl RecentVoiceSequences {
    pub(super) fn observe(&mut self, sequence: u32) -> RecentVoiceSequenceResult {
        let Some(highest) = self.highest else {
            self.highest = Some(sequence);
            self.set_seen(0);
            return RecentVoiceSequenceResult::New;
        };

        if let Some(forward) = voice_sequence_distance_forward(highest, sequence) {
            if forward == 0 {
                return if self.is_seen(0) {
                    RecentVoiceSequenceResult::Duplicate
                } else {
                    self.set_seen(0);
                    RecentVoiceSequenceResult::New
                };
            }

            self.shift_seen(forward as usize);
            self.highest = Some(sequence);
            self.set_seen(0);
            return RecentVoiceSequenceResult::New;
        }

        let Some(backward) = voice_sequence_distance_forward(sequence, highest) else {
            return RecentVoiceSequenceResult::Stale;
        };
        let backward = backward as usize;
        if backward >= MAX_RECENT_VOICE_SEQUENCES {
            return RecentVoiceSequenceResult::Stale;
        }
        if self.is_seen(backward) {
            RecentVoiceSequenceResult::Duplicate
        } else {
            self.set_seen(backward);
            RecentVoiceSequenceResult::New
        }
    }

    fn shift_seen(&mut self, shift: usize) {
        if shift >= MAX_RECENT_VOICE_SEQUENCES {
            self.seen.fill(0);
            return;
        }

        let word_shift = shift / RECENT_VOICE_SEQUENCE_WORD_BITS;
        let bit_shift = shift % RECENT_VOICE_SEQUENCE_WORD_BITS;

        if word_shift > 0 {
            for index in (0..RECENT_VOICE_SEQUENCE_WORDS).rev() {
                self.seen[index] = if index >= word_shift {
                    self.seen[index - word_shift]
                } else {
                    0
                };
            }
        }

        if bit_shift > 0 {
            for index in (0..RECENT_VOICE_SEQUENCE_WORDS).rev() {
                let carry = if index > 0 {
                    self.seen[index - 1] >> (RECENT_VOICE_SEQUENCE_WORD_BITS - bit_shift)
                } else {
                    0
                };
                self.seen[index] = (self.seen[index] << bit_shift) | carry;
            }
        }
    }

    fn is_seen(&self, distance: usize) -> bool {
        debug_assert!(distance < MAX_RECENT_VOICE_SEQUENCES);
        let word = distance / RECENT_VOICE_SEQUENCE_WORD_BITS;
        let bit = distance % RECENT_VOICE_SEQUENCE_WORD_BITS;
        self.seen[word] & (1u64 << bit) != 0
    }

    fn set_seen(&mut self, distance: usize) {
        debug_assert!(distance < MAX_RECENT_VOICE_SEQUENCES);
        let word = distance / RECENT_VOICE_SEQUENCE_WORD_BITS;
        let bit = distance % RECENT_VOICE_SEQUENCE_WORD_BITS;
        self.seen[word] |= 1u64 << bit;
    }
}

#[derive(Debug)]
struct VoicePacketDeduplicator {
    streams: HashMap<u32, RecentVoiceSequences>,
    clock: u64,
}

impl VoicePacketDeduplicator {
    fn new() -> Self {
        Self {
            streams: HashMap::with_capacity(MAX_RECENT_VOICE_STREAMS),
            clock: 0,
        }
    }

    fn observe(&mut self, stream_id: u32, sequence: u32) -> RecentVoiceSequenceResult {
        if !self.streams.contains_key(&stream_id) && self.streams.len() >= MAX_RECENT_VOICE_STREAMS
        {
            self.evict_oldest_stream();
        }
        self.clock = self.clock.wrapping_add(1);
        let stream = self.streams.entry(stream_id).or_default();
        stream.last_touched = self.clock;
        stream.observe(sequence)
    }

    fn remove_stream(&mut self, stream_id: StreamId) {
        self.streams.remove(&stream_id.0);
    }

    fn clear(&mut self) {
        self.streams.clear();
    }

    fn evict_oldest_stream(&mut self) {
        let oldest = self
            .streams
            .iter()
            .min_by_key(|(_, stream)| stream.last_touched)
            .map(|(stream_id, _)| *stream_id);
        if let Some(stream_id) = oldest {
            self.streams.remove(&stream_id);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.streams.len()
    }
}

impl Default for VoicePacketDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a direct path counts as healthy right now: a candidate pair is
/// selected and an inbound packet arrived within `failover_idle`.
pub(super) fn direct_path_healthy(
    selected: bool,
    last_inbound: Option<Instant>,
    now: Instant,
    failover_idle: Duration,
) -> bool {
    selected && last_inbound.is_some_and(|at| now.saturating_duration_since(at) <= failover_idle)
}

/// Whether the server relay can be dropped: there is at least one other online
/// participant and every one of them has a peer whose direct path has been
/// stable for at least `confirm_window`.
pub(super) fn relay_suppressed(
    now: Instant,
    confirm_window: Duration,
    voice_others: &HashSet<UserId>,
    peers: impl Iterator<Item = (UserId, Option<Instant>)>,
) -> bool {
    if voice_others.is_empty() {
        return false;
    }
    let mut covered = HashSet::new();
    for (user_id, stable_since) in peers {
        if let Some(since) = stable_since {
            if now.saturating_duration_since(since) >= confirm_window {
                covered.insert(user_id);
            }
        }
    }
    voice_others.iter().all(|user_id| covered.contains(user_id))
}

fn voice_sequence_distance_forward(from: u32, to: u32) -> Option<u32> {
    let distance = to.wrapping_sub(from);
    if distance < (1 << 31) {
        Some(distance)
    } else {
        None
    }
}

/// A token bucket that paces upload chunk emission to a byte-per-second ceiling.
///
/// A `rate` of `0` disables pacing: [`budget`](Self::budget) is unbounded and the
/// other operations are no-ops. Otherwise tokens accrue at `rate` bytes per
/// second, capped at one second's worth so a poll loop that parked between


pub(super) struct PeerConnection {
    pub(super) user_id: UserId,
    pub(super) agent: TraversalAgent,
    pub(super) send_key: KeyMaterial,
    pub(super) recv_key: KeyMaterial,
    pub(super) send_counter: u64,
    pub(super) recv_replay: AntiReplay,
    pub(super) connection_id: u64,
    /// The peer's candidate generation this agent was built from.
    pub(super) remote_generation: u64,
    /// Our own candidate generation when this agent was built. A local restart
    /// bumps it, so a matching `P2pPeer` must still rebuild the agent to pick
    /// up the fresh local candidates.
    pub(super) local_generation: u64,
    /// When the current healthy direct path was first observed, the clock for
    /// the [`DIRECT_CONFIRM_WINDOW`] confirmation. `None` while no healthy direct
    /// path exists.
    pub(super) direct_stable_since: Option<Instant>,
    /// Last inbound direct packet (media or STUN) from this peer.
    pub(super) last_direct_inbound: Option<Instant>,
    /// Outstanding RTT probe nonces sent over the direct path, paired with their
    /// send time. Bounded by [`RTT_IN_FLIGHT_CAP`].
    pub(super) rtt_in_flight: VecDeque<(u64, Instant)>,
    /// Smoothed round-trip time to this peer over the direct path, in
    /// milliseconds. `None` until the first `Pong` arrives.
    pub(super) rtt_ms: Option<f32>,
}

enum P2pMediaPacket {
    Voice {
        stream_id: StreamId,
        sequence: u32,
        timestamp: u32,
        flags: u8,
        payload: media::VoicePayload,
        action: Option<P2pAction>,
    },
    Feedback {
        stream_id: StreamId,
        feedback: media::VoiceFeedback,
        action: Option<P2pAction>,
    },
    Ping {
        nonce: u64,
        action: Option<P2pAction>,
    },
    Pong {
        rtt_ms: Option<u16>,
        action: Option<P2pAction>,
    },
}

pub(super) struct EncoderFeedbackController {
    current: LiveEncoderProfile,
    smoothed_loss: f32,
    high_loss_windows: u8,
    hold_until: Instant,
}

impl EncoderFeedbackController {
    pub(super) fn new() -> Self {
        Self {
            current: LiveEncoderProfile::DRED_20,
            smoothed_loss: 0.0,
            high_loss_windows: 0,
            hold_until: Instant::now(),
        }
    }

    pub(super) fn observe(
        &mut self,
        feedback: LivePlaybackFeedback,
        now: Instant,
    ) -> Option<LiveEncoderProfile> {
        if feedback.expected_packets == 0 {
            return None;
        }
        let effective_loss = f32::from(feedback.lost_packets.saturating_add(feedback.late_packets))
            / f32::from(feedback.expected_packets);
        self.smoothed_loss = ENCODER_FEEDBACK_ALPHA * effective_loss
            + (1.0 - ENCODER_FEEDBACK_ALPHA) * self.smoothed_loss;
        if effective_loss >= 0.45 {
            self.high_loss_windows = self.high_loss_windows.saturating_add(1).min(2);
        } else {
            self.high_loss_windows = 0;
        }

        let target = if effective_loss >= 0.55 || self.high_loss_windows >= 2 {
            LiveEncoderProfile::DRED_60
        } else if effective_loss >= 0.40 {
            LiveEncoderProfile::DRED_50
        } else if effective_loss >= 0.25 {
            LiveEncoderProfile::DRED_35
        } else {
            LiveEncoderProfile::DRED_20
        };

        if target.packet_loss_percent > self.current.packet_loss_percent {
            return self.set_current(target, now);
        }
        if target.packet_loss_percent == self.current.packet_loss_percent
            && self.current.packet_loss_percent > LiveEncoderProfile::DRED_20.packet_loss_percent
        {
            self.hold_until = now + ENCODER_PROFILE_HOLD;
            return None;
        }
        if now < self.hold_until {
            return None;
        }

        let next = match self.current.packet_loss_percent {
            60 if self.smoothed_loss < 0.45 => Some(LiveEncoderProfile::DRED_50),
            50 if self.smoothed_loss < 0.30 => Some(LiveEncoderProfile::DRED_35),
            35 if self.smoothed_loss < 0.15
                && feedback.max_neteq_target_ms < 200
                && feedback.max_neteq_playout_delay_ms < 200
                && feedback.max_interarrival_jitter_ms < 50 =>
            {
                Some(LiveEncoderProfile::DRED_20)
            }
            _ => None,
        };
        next.and_then(|profile| self.set_current(profile, now))
    }

    fn set_current(
        &mut self,
        profile: LiveEncoderProfile,
        now: Instant,
    ) -> Option<LiveEncoderProfile> {
        if profile == self.current {
            return None;
        }
        self.current = profile;
        if profile.packet_loss_percent > LiveEncoderProfile::DRED_20.packet_loss_percent {
            self.hold_until = now + ENCODER_PROFILE_HOLD;
        }
        Some(profile)
    }
}


struct MdnsPending {
    session_id: SessionId,
    control: P2pCandidate,
    port: u16,
}

struct P2pVoiceRoute {
    session_id: SessionId,
    addr: SocketAddr,
    connection_id: u64,
    counter: u64,
    key: KeyMaterial,
}

struct InboundVoiceStream {
    session_id: SessionId,
    user_id: UserId,
}

fn stream_owner_matches(
    streams: &HashMap<StreamId, InboundVoiceStream>,
    stream_id: StreamId,
    session_id: SessionId,
) -> bool {
    streams
        .get(&stream_id)
        .is_some_and(|stream| stream.session_id == session_id)
}

pub(super) fn bind_voice_udp_socket(addr: SocketAddr) -> io::Result<StdUdpSocket> {
    let socket = chatt_p2p::socket::bind_udp_socket(
        addr,
        chatt_p2p::socket::UdpSocketOptions::default(),
    )?;
    if let Err(error) = rpc::qos::apply_voice_qos(socket.as_raw_fd(), addr) {
        kvlog::warn!(
            "voice udp qos unavailable",
            addr = %addr,
            dscp = rpc::qos::VOICE_DSCP,
            error = %error
        );
    }
    Ok(socket)
}

pub(super) struct InitialUdpBind {
    udp: StdUdpSocket,
    packet: Vec<u8>,
    server_addr: SocketAddr,
}

impl InitialUdpBind {
    pub(super) fn prepare(
        udp: &StdUdpSocket,
        media: &MediaProtection,
        server_addr: SocketAddr,
    ) -> Result<Self, String> {
        Ok(Self {
            udp: udp
                .try_clone()
                .map_err(|error| format!("failed to clone initial UDP bind socket: {error}"))?,
            packet: media::seal_media(media, 0, &MediaPayload::Bind)
                .map_err(|error| format!("failed to seal initial UDP bind: {error}"))?,
            server_addr,
        })
    }

    /// Sends counter zero while the fresh socket is still blocking. This runs
    /// after authentication but before voice activation, so it cannot race the
    /// dedicated voice loop's later counters.
    pub(super) fn dispatch(self) -> Result<Option<io::Error>, String> {
        self.udp
            .set_nonblocking(false)
            .map_err(|error| format!("failed to make initial UDP bind blocking: {error}"))?;
        let send_result = self
            .udp
            .send_to(&self.packet, self.server_addr)
            .and_then(|sent| {
                if sent == self.packet.len() {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "initial UDP bind was only partially sent",
                    ))
                }
            });
        self.udp
            .set_nonblocking(true)
            .map_err(|error| format!("failed to restore nonblocking voice UDP socket: {error}"))?;
        Ok(send_result.err())
    }
}

#[inline]
fn recv_udp_datagram(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<Option<(usize, SocketAddr)>> {
    rpc::evented::recv_datagram_with(buf, |buf| socket.recv_from(buf))
}

pub(super) struct PublishP2pRequest {
    pub(super) generation: u64,
    pub(super) room_id: RoomId,
    pub(super) candidate_generation: u64,
    pub(super) nat: P2pNatKind,
    pub(super) tie_breaker: u64,
    pub(super) candidates: Vec<P2pCandidate>,
}

pub(super) enum VoiceCommand {
    StartSession {
        generation: u64,
        udp: StdUdpSocket,
        media: MediaProtection,
        initial_bind_attempted: bool,
        transport_mode: TransportMode,
        server_udp_addr: SocketAddr,
        server_udp_probe_addr: Option<SocketAddr>,
        p2p_enabled: bool,
        candidate_privacy: CandidatePrivacy,
        prefer_ipv6: bool,
    },
    Authenticated {
        generation: u64,
        session_id: SessionId,
    },
    VoiceStarted {
        generation: u64,
        room_id: RoomId,
        session_id: SessionId,
        user_id: UserId,
        stream_id: StreamId,
        local: bool,
    },
    VoiceStopped {
        generation: u64,
        room_id: RoomId,
        session_id: SessionId,
        user_id: UserId,
        stream_id: StreamId,
        local: bool,
    },
    RoomRttSnapshot {
        generation: u64,
        room_id: RoomId,
        members: Vec<ParticipantServerRtt>,
    },
    UserOffline {
        generation: u64,
        user_id: UserId,
    },
    UdpBound {
        generation: u64,
    },
    UdpReflexive {
        generation: u64,
        addr: SocketAddr,
    },
    NatProbeObserved {
        generation: u64,
        probe_id: u8,
        addr: SocketAddr,
    },
    InstallPeer {
        generation: u64,
        peer: P2pPeerInfo,
    },
    RemovePeer {
        generation: u64,
        session_id: SessionId,
        user_id: UserId,
    },
    SetP2pEnabled {
        generation: u64,
        enabled: bool,
    },
    EndSession {
        generation: u64,
    },
    Shutdown,
}

impl VoiceCommand {
    fn generation(&self) -> Option<u64> {
        match self {
            Self::StartSession { generation, .. }
            | Self::Authenticated { generation, .. }
            | Self::VoiceStarted { generation, .. }
            | Self::VoiceStopped { generation, .. }
            | Self::RoomRttSnapshot { generation, .. }
            | Self::UserOffline { generation, .. }
            | Self::UdpBound { generation }
            | Self::UdpReflexive { generation, .. }
            | Self::NatProbeObserved { generation, .. }
            | Self::InstallPeer { generation, .. }
            | Self::RemovePeer { generation, .. }
            | Self::SetP2pEnabled { generation, .. }
            | Self::EndSession { generation } => Some(*generation),
            Self::Shutdown => None,
        }
    }
}

struct QueuedMicrophonePacket {
    generation: u64,
    sequence: Option<u32>,
    frame: LocalVoiceFrame,
}

struct QueuedPlaybackFeedback {
    generation: u64,
    feedback: LivePlaybackFeedback,
}

#[derive(Default)]
struct VoiceMailbox {
    controls: VecDeque<VoiceCommand>,
    microphone: VecDeque<QueuedMicrophonePacket>,
    feedback: HashMap<u32, QueuedPlaybackFeedback>,
    playback_sink: Option<Option<LivePlaybackSink>>,
    ingress_generation: Option<u64>,
    activated_generation: Option<u64>,
    notified: bool,
}

impl VoiceMailbox {
    fn front_control_is_runnable(&self) -> bool {
        match self.controls.front() {
            Some(VoiceCommand::StartSession { generation, .. }) => {
                self.activated_generation == Some(*generation)
            }
            Some(_) => true,
            None => false,
        }
    }

    fn has_runnable_work(&self) -> bool {
        self.front_control_is_runnable()
            || self.activated_generation.is_some()
                && (!self.microphone.is_empty()
                    || !self.feedback.is_empty()
                    || self.playback_sink.is_some())
    }

    fn arm_wake(&mut self) -> bool {
        if self.notified {
            false
        } else {
            self.notified = true;
            true
        }
    }
}

/// Split, waker-backed mailbox for ordered control and bounded packet-plane state.
pub(super) struct VoiceCommandSubmission {
    mailbox: Mutex<VoiceMailbox>,
    waker: Mutex<Option<Arc<Waker>>>,
    closed: AtomicBool,
    microphone_drops: AtomicU64,
}

impl VoiceCommandSubmission {
    fn new() -> Self {
        Self {
            mailbox: Mutex::new(VoiceMailbox {
                controls: VecDeque::with_capacity(32),
                microphone: VecDeque::with_capacity(MAX_QUEUED_MICROPHONE_PACKETS),
                feedback: HashMap::new(),
                ..VoiceMailbox::default()
            }),
            waker: Mutex::new(None),
            closed: AtomicBool::new(false),
            microphone_drops: AtomicU64::new(0),
        }
    }

    fn submit(&self, command: VoiceCommand) -> Result<(), VoiceCommand> {
        if self.closed.load(Ordering::Acquire) {
            return Err(command);
        }
        let mut mailbox = self.mailbox.lock().unwrap();
        if self.closed.load(Ordering::Relaxed) {
            return Err(command);
        }
        let mut enqueue = true;
        let wake_required = match &command {
            VoiceCommand::StartSession { generation, .. } => {
                mailbox.ingress_generation = Some(*generation);
                mailbox.activated_generation = None;
                mailbox.controls.retain(|command| {
                    matches!(command, VoiceCommand::EndSession { .. } | VoiceCommand::Shutdown)
                });
                mailbox.microphone.clear();
                mailbox.feedback.clear();
                false
            }
            VoiceCommand::EndSession { generation }
                if mailbox.ingress_generation == Some(*generation) =>
            {
                let activated = mailbox.activated_generation == Some(*generation);
                mailbox.ingress_generation = None;
                mailbox.activated_generation = None;
                mailbox
                    .controls
                    .retain(|command| matches!(command, VoiceCommand::Shutdown));
                mailbox.microphone.clear();
                mailbox.feedback.clear();
                enqueue = activated;
                activated
            }
            VoiceCommand::EndSession { .. } => {
                enqueue = false;
                false
            }
            VoiceCommand::Shutdown => {
                mailbox.ingress_generation = None;
                mailbox.activated_generation = None;
                mailbox.controls.clear();
                mailbox.microphone.clear();
                mailbox.feedback.clear();
                true
            }
            _ if mailbox.controls.len() >= MAX_QUEUED_CONTROL_COMMANDS => return Err(command),
            _ if command.generation().is_some()
                && command.generation() != mailbox.ingress_generation =>
            {
                enqueue = false;
                false
            }
            _ => mailbox.activated_generation.is_some(),
        };
        if enqueue {
            mailbox.controls.push_back(command);
        }
        let wake = wake_required && mailbox.arm_wake();
        drop(mailbox);
        if wake {
            self.wake();
        }
        Ok(())
    }

    fn submit_microphone(
        &self,
        sequence: Option<u32>,
        frame: LocalVoiceFrame,
    ) -> Result<(), LocalVoiceFrame> {
        if self.closed.load(Ordering::Acquire) {
            return Err(frame);
        }
        let mut mailbox = self.mailbox.lock().unwrap();
        if self.closed.load(Ordering::Relaxed) {
            return Err(frame);
        }
        let Some(generation) = mailbox.activated_generation else {
            return Ok(());
        };
        let dropped = if mailbox.microphone.len() == MAX_QUEUED_MICROPHONE_PACKETS {
            mailbox.microphone.pop_front();
            Some(self.microphone_drops.fetch_add(1, Ordering::Relaxed) + 1)
        } else {
            None
        };
        mailbox.microphone.push_back(QueuedMicrophonePacket {
            generation,
            sequence,
            frame,
        });
        let wake = mailbox.arm_wake();
        drop(mailbox);
        if let Some(dropped) = dropped
            && (dropped.is_power_of_two() || dropped % 1024 == 0)
        {
            kvlog::warn!(
                "client voice mailbox dropped stale microphone packets",
                max_queued_microphone_packets = MAX_QUEUED_MICROPHONE_PACKETS,
                microphone_packets_dropped = dropped
            );
        }
        if wake {
            self.wake();
        }
        Ok(())
    }

    fn submit_feedback(&self, feedback: LivePlaybackFeedback) -> Result<(), LivePlaybackFeedback> {
        if self.closed.load(Ordering::Acquire) {
            return Err(feedback);
        }
        let mut mailbox = self.mailbox.lock().unwrap();
        if self.closed.load(Ordering::Relaxed) {
            return Err(feedback);
        }
        let Some(generation) = mailbox.activated_generation else {
            return Ok(());
        };
        if mailbox.feedback.len() >= MAX_QUEUED_FEEDBACK_STREAMS
            && !mailbox.feedback.contains_key(&feedback.stream_id)
            && let Some(stream_id) = mailbox.feedback.keys().next().copied()
        {
            mailbox.feedback.remove(&stream_id);
        }
        mailbox.feedback.insert(
            feedback.stream_id,
            QueuedPlaybackFeedback {
                generation,
                feedback,
            },
        );
        let wake = mailbox.arm_wake();
        drop(mailbox);
        if wake {
            self.wake();
        }
        Ok(())
    }

    fn submit_playback_sink(&self, sink: Option<LivePlaybackSink>) -> Result<(), Option<LivePlaybackSink>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(sink);
        }
        let mut mailbox = self.mailbox.lock().unwrap();
        if self.closed.load(Ordering::Relaxed) {
            return Err(sink);
        }
        mailbox.playback_sink = Some(sink);
        let wake = mailbox.activated_generation.is_some() && mailbox.arm_wake();
        drop(mailbox);
        if wake {
            self.wake();
        }
        Ok(())
    }

    fn drain_into(
        &self,
        controls: &mut VecDeque<VoiceCommand>,
        microphone: &mut VecDeque<QueuedMicrophonePacket>,
        feedback: &mut Vec<QueuedPlaybackFeedback>,
        playback_sink: &mut Option<Option<LivePlaybackSink>>,
    ) -> bool {
        debug_assert!(controls.is_empty());
        debug_assert!(microphone.is_empty());
        debug_assert!(feedback.is_empty());
        debug_assert!(playback_sink.is_none());
        let mut mailbox = self.mailbox.lock().unwrap();
        for _ in 0..MAX_COMMANDS_PER_TICK {
            if !mailbox.front_control_is_runnable() {
                break;
            }
            let Some(command) = mailbox.controls.pop_front() else {
                break;
            };
            controls.push_back(command);
        }
        if mailbox.controls.is_empty() && mailbox.activated_generation.is_some() {
            std::mem::swap(&mut mailbox.microphone, microphone);
            feedback.extend(mailbox.feedback.drain().map(|(_, feedback)| feedback));
            *playback_sink = mailbox.playback_sink.take();
        }
        let work_remains = mailbox.has_runnable_work();
        mailbox.notified = work_remains;
        work_remains
    }

    fn wake(&self) {
        let waker = self.waker.lock().unwrap().clone();
        if let Some(waker) = waker
            && let Err(error) = waker.wake()
        {
            kvlog::warn!("client voice command wake failed", error = %error);
        }
    }

    fn install_waker(&self, waker: Arc<Waker>) {
        *self.waker.lock().unwrap() = Some(waker);
    }

    fn activate(&self, generation: u64) -> Result<(), ()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(());
        }
        let mut mailbox = self.mailbox.lock().unwrap();
        if self.closed.load(Ordering::Relaxed)
            || mailbox.ingress_generation != Some(generation)
        {
            return Err(());
        }
        mailbox.activated_generation = Some(generation);
        let wake = mailbox.arm_wake();
        drop(mailbox);
        if wake {
            self.wake();
        }
        Ok(())
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let mut mailbox = self.mailbox.lock().unwrap();
        mailbox.controls.clear();
        mailbox.microphone.clear();
        mailbox.feedback.clear();
        mailbox.playback_sink = None;
        mailbox.ingress_generation = None;
        mailbox.activated_generation = None;
        mailbox.notified = false;
    }
}

#[derive(Clone)]
pub(super) struct VoiceInputSender {
    submission: Arc<VoiceCommandSubmission>,
}

impl VoiceInputSender {
    pub(super) fn send(&self, command: NetworkCommand) -> Result<(), SendError<NetworkCommand>> {
        match command {
            NetworkCommand::LocalVoicePacket(frame) => self
                .submission
                .submit_microphone(None, frame)
                .map_err(|frame| SendError(NetworkCommand::LocalVoicePacket(frame))),
            NetworkCommand::SequencedLocalVoicePacket { sequence, frame } => {
                self.submission
                    .submit_microphone(Some(sequence), frame)
                    .map_err(|frame| {
                        SendError(NetworkCommand::SequencedLocalVoicePacket { sequence, frame })
                    })
            }
            NetworkCommand::SetPlaybackSink(sink) => self
                .submission
                .submit_playback_sink(sink)
                .map_err(|sink| SendError(NetworkCommand::SetPlaybackSink(sink))),
            NetworkCommand::PlaybackFeedback(feedback) => self
                .submission
                .submit_feedback(feedback)
                .map_err(|feedback| SendError(NetworkCommand::PlaybackFeedback(feedback))),
            command => Err(SendError(command)),
        }
    }
}

#[cfg(test)]
pub(super) struct TestVoiceReceiver {
    _poll: Poll,
    submission: Arc<VoiceCommandSubmission>,
}

#[cfg(test)]
impl TestVoiceReceiver {
    pub(super) fn drain_microphone_sequences(&self) -> Vec<Option<u32>> {
        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        self.submission.drain_into(
            &mut controls,
            &mut microphone,
            &mut feedback,
            &mut sink,
        );
        assert!(controls.is_empty());
        assert!(feedback.is_empty());
        assert!(sink.is_none());
        microphone
            .into_iter()
            .map(|packet| packet.sequence)
            .collect()
    }
}

#[cfg(test)]
pub(super) fn input_for_test() -> (VoiceInputSender, TestVoiceReceiver) {
    let poll = Poll::new().unwrap();
    let waker = Arc::new(Waker::new(poll.registry(), COMMANDS).unwrap());
    let submission = Arc::new(VoiceCommandSubmission::new());
    submission.install_waker(waker);
    {
        let mut mailbox = submission.mailbox.lock().unwrap();
        mailbox.ingress_generation = Some(1);
        mailbox.activated_generation = Some(1);
    }
    (
        VoiceInputSender {
            submission: Arc::clone(&submission),
        },
        TestVoiceReceiver {
            _poll: poll,
            submission,
        },
    )
}

#[derive(Default)]
pub(super) struct VoiceOutputBatch {
    pub(super) publish_p2p: Option<PublishP2pRequest>,
    pub(super) session_failure: Option<(u64, String)>,
    pub(super) fatal_failure: Option<String>,
}

impl VoiceOutputBatch {
    pub(super) fn is_empty(&self) -> bool {
        self.publish_p2p.is_none()
            && self.session_failure.is_none()
            && self.fatal_failure.is_none()
    }
}

pub(super) struct VoiceOutputSubmission {
    pending: Mutex<VoiceOutputBatch>,
    main_waker: Arc<Waker>,
    ready: AtomicBool,
    stopped: AtomicBool,
}

impl VoiceOutputSubmission {
    fn new(main_waker: Arc<Waker>) -> Self {
        Self {
            pending: Mutex::new(VoiceOutputBatch::default()),
            main_waker,
            ready: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }
    }

    fn submit_all(&self, output: &mut VoiceOutputBatch) {
        if output.is_empty() {
            return;
        }
        let notify = {
            let mut pending = self.pending.lock().unwrap();
            let notify = pending.is_empty();
            if output.publish_p2p.is_some() {
                pending.publish_p2p = output.publish_p2p.take();
            }
            if output.session_failure.is_some() {
                pending.session_failure = output.session_failure.take();
            }
            if output.fatal_failure.is_some() {
                pending.fatal_failure = output.fatal_failure.take();
            }
            self.ready.store(true, Ordering::Release);
            notify
        };
        if notify {
            self.notify();
        }
    }

    pub(super) fn drain_into(&self, output: &mut VoiceOutputBatch) -> bool {
        debug_assert!(output.is_empty());
        if !self.ready.swap(false, Ordering::AcqRel) {
            return self.stopped.load(Ordering::Acquire);
        }
        let mut pending = self.pending.lock().unwrap();
        std::mem::swap(&mut *pending, output);
        self.stopped.load(Ordering::Acquire)
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.notify();
    }

    fn notify(&self) {
        if let Err(error) = self.main_waker.wake() {
            kvlog::warn!("client voice output wake failed", error = %error);
        }
    }
}

pub(super) struct VoiceLoopHandle {
    commands: Arc<VoiceCommandSubmission>,
    pub(super) outputs: Arc<VoiceOutputSubmission>,
    runtime: Arc<VoiceRuntime>,
    input: VoiceInputSender,
}

#[derive(Clone)]
pub(super) struct VoiceControl {
    commands: Arc<VoiceCommandSubmission>,
    outputs: Arc<VoiceOutputSubmission>,
    runtime: Arc<VoiceRuntime>,
}

impl VoiceControl {
    pub(super) fn activate(&self, generation: u64) -> Result<(), String> {
        self.runtime.ensure_started()?;
        self.commands
            .activate(generation)
            .map_err(|_| "client voice session is unavailable".to_string())
    }

    pub(super) fn submit(&self, command: VoiceCommand) -> Result<(), VoiceCommand> {
        self.commands.submit(command)
    }

    pub(super) fn drain_outputs(&self, output: &mut VoiceOutputBatch) -> bool {
        self.outputs.drain_into(output)
    }

}

struct VoiceRuntime {
    events: NetworkEventSender,
    commands: Arc<VoiceCommandSubmission>,
    outputs: Arc<VoiceOutputSubmission>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl VoiceRuntime {
    fn ensure_started(&self) -> Result<(), String> {
        let mut thread = self.thread.lock().unwrap();
        if thread.is_some() {
            return Ok(());
        }
        let poll = Poll::new().map_err(|error| format!("failed to create voice poll: {error}"))?;
        let command_waker = Arc::new(
            Waker::new(poll.registry(), COMMANDS)
                .map_err(|error| format!("failed to create voice waker: {error}"))?,
        );
        self.commands.install_waker(command_waker);
        let events = self.events.clone();
        let loop_commands = Arc::clone(&self.commands);
        let loop_outputs = Arc::clone(&self.outputs);
        let fatal_outputs = Arc::clone(&self.outputs);
        let commands_for_close = Arc::clone(&self.commands);
        *thread = Some(
            thread::Builder::new()
                .name("chatt-voice".to_string())
                .stack_size(512 * 1024)
                .spawn(move || {
                    let result = panic::catch_unwind(AssertUnwindSafe(|| {
                        VoiceLoop::new(poll, events, loop_commands, loop_outputs).run()
                    }));
                    commands_for_close.close();
                    let failure = match result {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(format!("voice poll failed: {error}")),
                        Err(_) => Some("voice worker panicked".to_string()),
                    };
                    if let Some(reason) = failure {
                        kvlog::error!("client voice worker stopped", error = reason.as_str());
                        let mut batch = VoiceOutputBatch {
                            fatal_failure: Some(reason.clone()),
                            ..VoiceOutputBatch::default()
                        };
                        fatal_outputs.submit_all(&mut batch);
                    }
                    fatal_outputs.stop();
                })
                .map_err(|error| format!("failed to spawn voice worker: {error}"))?,
        );
        Ok(())
    }

    fn stop(&self) {
        let mut thread = self.thread.lock().unwrap();
        if thread.is_some() {
            let _ = self.commands.submit(VoiceCommand::Shutdown);
        }
        if let Some(thread) = thread.take() {
            let _ = thread.join();
        }
        self.commands.close();
    }
}

impl VoiceLoopHandle {
    pub(super) fn spawn(
        events: NetworkEventSender,
        main_waker: Arc<Waker>,
    ) -> Result<Self, String> {
        let commands = Arc::new(VoiceCommandSubmission::new());
        let outputs = Arc::new(VoiceOutputSubmission::new(main_waker));
        let runtime = Arc::new(VoiceRuntime {
            events,
            commands: Arc::clone(&commands),
            outputs: Arc::clone(&outputs),
            thread: Mutex::new(None),
        });
        let input = VoiceInputSender {
            submission: Arc::clone(&commands),
        };
        Ok(Self {
            commands,
            outputs,
            runtime,
            input,
        })
    }

    pub(super) fn input_sender(&self) -> VoiceInputSender {
        self.input.clone()
    }

    pub(super) fn control(&self) -> VoiceControl {
        VoiceControl {
            commands: Arc::clone(&self.commands),
            outputs: Arc::clone(&self.outputs),
            runtime: Arc::clone(&self.runtime),
        }
    }

    pub(super) fn stop(&mut self) {
        self.runtime.stop();
    }
}

impl Drop for VoiceLoopHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct VoiceLoop {
    poll: Poll,
    events: NetworkEventSender,
    commands: Arc<VoiceCommandSubmission>,
    outputs: Arc<VoiceOutputSubmission>,
    poll_events: Events,
    command_buf: VecDeque<VoiceCommand>,
    microphone_buf: VecDeque<QueuedMicrophonePacket>,
    feedback_buf: Vec<QueuedPlaybackFeedback>,
    playback_sink_update: Option<Option<LivePlaybackSink>>,
    output_buf: VoiceOutputBatch,
    generation: Option<u64>,
    session: Option<VoiceSession>,
    playback_ingress: PlaybackIngressState,
    udp_work: bool,
    mdns_work: u8,
    command_work: bool,
    shutting_down: bool,
}

impl VoiceLoop {
    fn new(
        poll: Poll,
        events: NetworkEventSender,
        commands: Arc<VoiceCommandSubmission>,
        outputs: Arc<VoiceOutputSubmission>,
    ) -> Self {
        Self {
            poll,
            events,
            commands,
            outputs,
            poll_events: Events::with_capacity(128),
            command_buf: VecDeque::with_capacity(32),
            microphone_buf: VecDeque::with_capacity(MAX_QUEUED_MICROPHONE_PACKETS),
            feedback_buf: Vec::new(),
            playback_sink_update: None,
            output_buf: VoiceOutputBatch::default(),
            generation: None,
            session: None,
            playback_ingress: PlaybackIngressState::default(),
            udp_work: false,
            mdns_work: 0,
            command_work: false,
            shutting_down: false,
        }
    }

    fn run(&mut self) -> io::Result<()> {
        while !self.shutting_down {
            self.drain_commands();
            if self.udp_work {
                self.udp_work = self
                    .session
                    .as_mut()
                    .is_some_and(VoiceSession::read_udp);
            }
            if self.mdns_work != 0 {
                let work = std::mem::take(&mut self.mdns_work);
                if let Some(session) = self.session.as_mut() {
                    if work & 1 != 0 {
                        if session.handle_mdns_readable(MDNS_V4, Instant::now()) {
                            self.mdns_work |= 1;
                        }
                    }
                    if work & 2 != 0 {
                        if session.handle_mdns_readable(MDNS_V6, Instant::now()) {
                            self.mdns_work |= 2;
                        }
                    }
                }
            }
            if let Some(session) = self.session.as_mut() {
                let now = Instant::now();
                session.run_timers(&mut self.poll, now, &mut self.output_buf);
            }
            if self.output_buf.session_failure.is_some() {
                self.end_session();
            }
            self.outputs.submit_all(&mut self.output_buf);
            if self.shutting_down {
                break;
            }
            let timeout = if self.command_work || self.udp_work || self.mdns_work != 0 {
                Duration::ZERO
            } else {
                self.session
                    .as_ref()
                    .map_or(IDLE_POLL_TIMEOUT, |session| session.next_poll_timeout(Instant::now()))
            };
            match self.poll.poll(&mut self.poll_events, Some(timeout)) {
                Ok(()) => {}
                Err(error) if rpc::evented::is_interrupted_io_error(&error) => continue,
                Err(error) => return Err(error),
            }
            for event in self.poll_events.iter() {
                if !rpc::evented::MioReady::from_event(event).readable_like() {
                    continue;
                }
                match event.token() {
                    COMMANDS => {}
                    UDP => self.udp_work = true,
                    MDNS_V4 => self.mdns_work |= 1,
                    MDNS_V6 => self.mdns_work |= 2,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn drain_commands(&mut self) {
        debug_assert!(self.command_buf.is_empty());
        self.command_work = self.commands.drain_into(
            &mut self.command_buf,
            &mut self.microphone_buf,
            &mut self.feedback_buf,
            &mut self.playback_sink_update,
        );
        while let Some(command) = self.command_buf.pop_front() {
            self.apply_command(command);
            if self.shutting_down {
                break;
            }
        }
        if self.shutting_down {
            self.microphone_buf.clear();
            self.feedback_buf.clear();
            self.playback_sink_update = None;
            return;
        }
        if let Some(sink) = self.playback_sink_update.take() {
            self.playback_ingress = PlaybackIngressState::from_sink_update(sink.clone());
            if let Some(session) = self.session.as_mut() {
                session.set_playback_sink(sink);
            }
        }
        while let Some(packet) = self.microphone_buf.pop_front() {
            if self.generation == Some(packet.generation)
                && let Some(session) = self.session.as_mut()
            {
                session.send_local_voice(packet.frame, packet.sequence);
            }
        }
        for queued in self.feedback_buf.drain(..) {
            if self.generation == Some(queued.generation)
                && let Some(session) = self.session.as_mut()
            {
                session.send_playback_feedback(queued.feedback);
            }
        }
    }

    fn apply_command(&mut self, command: VoiceCommand) {
        match command {
            VoiceCommand::StartSession {
                generation,
                udp,
                media,
                initial_bind_attempted,
                transport_mode,
                server_udp_addr,
                server_udp_probe_addr,
                p2p_enabled,
                candidate_privacy,
                prefer_ipv6,
            } => {
                self.end_session();
                let result = VoiceSession::new(
                    &self.poll,
                    generation,
                    udp,
                    media,
                    initial_bind_attempted,
                    transport_mode,
                    server_udp_addr,
                    server_udp_probe_addr,
                    p2p_enabled,
                    candidate_privacy,
                    prefer_ipv6,
                    self.events.clone(),
                );
                match result {
                    Ok(mut session) => {
                        session.set_playback_ingress(self.playback_ingress.clone());
                        self.generation = Some(generation);
                        self.session = Some(session);
                        self.udp_work = true;
                    }
                    Err(error) => {
                        self.output_buf.session_failure = Some((generation, error));
                    }
                }
            }
            VoiceCommand::EndSession { generation } if self.generation == Some(generation) => {
                self.end_session();
            }
            VoiceCommand::Shutdown => {
                self.end_session();
                self.shutting_down = true;
            }
            VoiceCommand::Authenticated { generation, .. }
            | VoiceCommand::VoiceStarted { generation, .. }
            | VoiceCommand::VoiceStopped { generation, .. }
            | VoiceCommand::RoomRttSnapshot { generation, .. }
            | VoiceCommand::UserOffline { generation, .. }
            | VoiceCommand::UdpBound { generation }
            | VoiceCommand::UdpReflexive { generation, .. }
            | VoiceCommand::NatProbeObserved { generation, .. }
            | VoiceCommand::InstallPeer { generation, .. }
            | VoiceCommand::RemovePeer { generation, .. }
            | VoiceCommand::SetP2pEnabled { generation, .. }
            | VoiceCommand::EndSession { generation }
                if self.generation != Some(generation) =>
            {
                kvlog::debug!("stale client voice command ignored", generation);
            }
            VoiceCommand::Authenticated { session_id, .. } => {
                if let Some(session) = self.session.as_mut() {
                    session.authenticated(session_id);
                }
            }
            VoiceCommand::VoiceStarted {
                room_id,
                session_id,
                user_id,
                stream_id,
                local,
                ..
            } => {
                if let Some(session) = self.session.as_mut() {
                    session.voice_started(room_id, session_id, user_id, stream_id, local);
                }
            }
            VoiceCommand::VoiceStopped {
                room_id,
                session_id,
                user_id,
                stream_id,
                local,
                ..
            } => {
                if let Some(session) = self.session.as_mut() {
                    session.voice_stopped(room_id, session_id, user_id, stream_id, local);
                }
            }
            VoiceCommand::RoomRttSnapshot { room_id, members, .. } => {
                if let Some(session) = self.session.as_mut()
                    && session.voice_room == Some(room_id)
                {
                    session.room_server_rtts = members
                        .into_iter()
                        .filter_map(|member| member.server_rtt_ms.map(|rtt| (member.user_id, rtt)))
                        .collect();
                    session.publish_all_relay_rtts();
                }
            }
            VoiceCommand::UserOffline { user_id, .. } => {
                if let Some(session) = self.session.as_mut() {
                    session.room_server_rtts.remove(&user_id);
                    session.voice_others.remove(&user_id);
                    let peer_sessions = session
                        .p2p_peers
                        .iter()
                        .filter_map(|(id, peer)| (peer.user_id == user_id).then_some(*id))
                        .collect::<Vec<_>>();
                    let offline_streams = session
                        .inbound_streams
                        .iter()
                        .filter_map(|(stream_id, stream)| {
                            (stream.user_id == user_id).then_some(*stream_id)
                        })
                        .collect::<Vec<_>>();
                    for stream_id in offline_streams {
                        session.inbound_streams.remove(&stream_id);
                        session.voice_dedup.remove_stream(stream_id);
                        session.clear_pending_playback_stream(stream_id);
                    }
                    for session_id in peer_sessions {
                        session.p2p_peers.remove(&session_id);
                    }
                }
            }
            VoiceCommand::UdpBound { .. } => {
                if let Some(session) = self.session.as_mut() {
                    session.udp_bound();
                }
            }
            VoiceCommand::UdpReflexive { addr, .. } => {
                if let Some(session) = self.session.as_mut()
                    && session.p2p_reflexive_addr != Some(addr)
                {
                    session.p2p_reflexive_addr = Some(addr);
                    session.publish_p2p_candidates();
                }
            }
            VoiceCommand::NatProbeObserved { probe_id, addr, .. } => {
                if let Some(session) = self.session.as_mut() {
                    session.nat_probe_observed(probe_id, addr);
                }
            }
            VoiceCommand::InstallPeer { peer, .. } => {
                if let Some(session) = self.session.as_mut()
                    && let Err(error) = session.install_p2p_peer(peer)
                {
                    kvlog::warn!("p2p peer rejected", error = %error);
                    let _ = session.events.send(NetworkEvent::Error(error));
                }
            }
            VoiceCommand::RemovePeer { session_id, user_id, .. } => {
                if let Some(session) = self.session.as_mut() {
                    session.remove_peer(session_id, user_id);
                }
            }
            VoiceCommand::SetP2pEnabled { enabled, .. } => {
                if let Some(session) = self.session.as_mut() {
                    session.set_p2p_enabled(enabled);
                }
            }
            VoiceCommand::EndSession { .. } => unreachable!("matching generation handled above"),
        }
    }

    fn end_session(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.shutdown(&self.poll);
        }
        self.generation = None;
        self.udp_work = false;
        self.mdns_work = 0;
    }
}

struct VoiceSession {
    generation: u64,
    events: NetworkEventSender,
    udp: UdpSocket,
    udp_local_addr: SocketAddr,
    server_udp_addr: SocketAddr,
    server_udp_probe_addr: Option<SocketAddr>,
    media: MediaProtection,
    transport_mode: TransportMode,
    media_send_counter: u64,
    initial_bind_attempted: bool,
    media_recv_replay: AntiReplay,
    media_packet: Vec<u8>,
    media_scratch: Vec<u8>,
    p2p_routes: Vec<P2pVoiceRoute>,
    session_id: Option<SessionId>,
    voice_room: Option<RoomId>,
    active_stream: Option<StreamId>,
    local_sequence: u32,
    p2p_generation: u64,
    p2p_tie_breaker: u64,
    p2p_nat: P2pNatKind,
    p2p_nat_classifier: chatt_p2p::NatClassifier,
    p2p_reflexive_addr: Option<SocketAddr>,
    p2p_candidates: Vec<P2pCandidate>,
    p2p_local_candidates: Vec<chatt_p2p::Candidate>,
    p2p_enabled: bool,
    candidate_privacy: CandidatePrivacy,
    prefer_ipv6: bool,
    mdns: MdnsSystem,
    mdns_pending: HashMap<String, MdnsPending>,
    p2p_peers: HashMap<SessionId, PeerConnection>,
    inbound_streams: HashMap<StreamId, InboundVoiceStream>,
    voice_dedup: VoicePacketDeduplicator,
    voice_others: HashSet<UserId>,
    room_server_rtts: HashMap<UserId, u16>,
    next_relay_keepalive: Instant,
    next_rtt_probe: Instant,
    next_udp_bind_retry: Instant,
    rtt_probe_seq: u64,
    server_rtt_in_flight: VecDeque<(u64, Instant)>,
    server_rtt_ms: Option<f32>,
    server_rtt_last_sample_at: Option<Instant>,
    playback_ingress: PlaybackIngressState,
    pending_playback_packets: VecDeque<crate::audio::RemoteVoicePacket>,
    encoder_feedback: EncoderFeedbackController,
    restart_port_policy: RestartPortPolicy,
    udp_rebind_requested: bool,
    awaiting_udp_bound: bool,
    udp_bind_attempts: u32,
    udp_reported_unreachable: bool,
    interface_monitor: InterfaceMonitor,
    pending_publish: Option<PublishP2pRequest>,
}

impl VoiceSession {
    #[allow(clippy::too_many_arguments)]
    fn new(
        poll: &Poll,
        generation: u64,
        udp: StdUdpSocket,
        media: MediaProtection,
        initial_bind_attempted: bool,
        transport_mode: TransportMode,
        server_udp_addr: SocketAddr,
        server_udp_probe_addr: Option<SocketAddr>,
        p2p_enabled: bool,
        candidate_privacy: CandidatePrivacy,
        prefer_ipv6: bool,
        events: NetworkEventSender,
    ) -> Result<Self, String> {
        udp.set_nonblocking(true)
            .map_err(|error| format!("failed to make voice UDP socket nonblocking: {error}"))?;
        let udp_local_addr = udp
            .local_addr()
            .map_err(|error| format!("failed to read UDP socket address: {error}"))?;
        let mut udp = UdpSocket::from_std(udp);
        poll.registry()
            .register(&mut udp, UDP, Interest::READABLE)
            .map_err(|error| format!("failed to register UDP socket: {error}"))?;
        let mut mdns = if p2p_enabled {
            MdnsSystem::bind()
        } else {
            MdnsSystem::unbound()
        };
        if let Err(error) = mdns.register(poll.registry(), MDNS_V4, MDNS_V6) {
            kvlog::warn!("failed to register voice mdns sockets", error = %error);
        }
        let now = Instant::now();
        Ok(Self {
            generation,
            events,
            udp,
            udp_local_addr,
            server_udp_addr,
            server_udp_probe_addr,
            media,
            transport_mode,
            media_send_counter: u64::from(initial_bind_attempted),
            initial_bind_attempted,
            media_recv_replay: AntiReplay::new(),
            media_packet: Vec::new(),
            media_scratch: Vec::new(),
            p2p_routes: Vec::new(),
            session_id: None,
            voice_room: None,
            active_stream: None,
            local_sequence: 0,
            p2p_generation: 1,
            p2p_tie_breaker: random_u64().unwrap_or(1),
            p2p_nat: configured_nat_kind(),
            p2p_nat_classifier: chatt_p2p::NatClassifier::new(),
            p2p_reflexive_addr: None,
            p2p_candidates: Vec::new(),
            p2p_local_candidates: Vec::new(),
            p2p_enabled,
            candidate_privacy,
            prefer_ipv6,
            mdns,
            mdns_pending: HashMap::new(),
            p2p_peers: HashMap::new(),
            inbound_streams: HashMap::new(),
            voice_dedup: VoicePacketDeduplicator::new(),
            voice_others: HashSet::new(),
            room_server_rtts: HashMap::new(),
            next_relay_keepalive: now + RELAY_KEEPALIVE_INTERVAL,
            next_rtt_probe: now + RTT_PROBE_INTERVAL,
            next_udp_bind_retry: now + UDP_BIND_RETRY_INTERVAL,
            rtt_probe_seq: 0,
            server_rtt_in_flight: VecDeque::new(),
            server_rtt_ms: None,
            server_rtt_last_sample_at: None,
            playback_ingress: PlaybackIngressState::default(),
            pending_playback_packets: VecDeque::new(),
            encoder_feedback: EncoderFeedbackController::new(),
            restart_port_policy: RestartPortPolicy::default(),
            udp_rebind_requested: false,
            awaiting_udp_bound: false,
            udp_bind_attempts: 0,
            udp_reported_unreachable: false,
            interface_monitor: InterfaceMonitor::new(now),
            pending_publish: None,
        })
    }

    fn shutdown(&mut self, poll: &Poll) {
        let _ = poll.registry().deregister(&mut self.udp);
        self.mdns.shutdown(poll.registry());
        self.playback_ingress = PlaybackIngressState::Suspended;
        self.pending_playback_packets.clear();
        self.p2p_peers.clear();
        self.inbound_streams.clear();
        self.voice_dedup.clear();
        self.media_send_counter = 0;
        self.media_recv_replay = AntiReplay::new();
    }

    fn next_poll_timeout(&self, now: Instant) -> Duration {
        if self.udp_rebind_requested || self.pending_publish.is_some() {
            return Duration::ZERO;
        }
        let mut timeout = IDLE_POLL_TIMEOUT;
        if self.awaiting_udp_bound {
            timeout = timeout.min(self.next_udp_bind_retry.saturating_duration_since(now));
        }
        timeout = timeout.min(self.next_relay_keepalive.saturating_duration_since(now));
        timeout = timeout.min(self.next_rtt_probe.saturating_duration_since(now));
        if let Some(sample_at) = self.server_rtt_last_sample_at {
            timeout = timeout.min(
                (sample_at + super::RTT_STALE_AFTER).saturating_duration_since(now),
            );
        }
        if self.p2p_enabled && self.voice_room.is_some() {
            timeout = timeout.min(self.interface_monitor.next_wake(now));
        }
        if let Some(delay) = self.mdns.next_timeout(now) {
            timeout = timeout.min(delay);
        }
        if self.p2p_enabled && !self.p2p_peers.is_empty() {
            timeout = timeout.min(P2P_POLL_TIMEOUT);
        }
        timeout
    }

    fn run_timers(&mut self, poll: &mut Poll, now: Instant, output: &mut VoiceOutputBatch) {
        if self.p2p_enabled {
            self.poll_interfaces(now);
        }
        if self.udp_rebind_requested {
            self.reconcile_mdns(poll);
            if let Err(error) = self.rebind_udp_socket(poll) {
                output.session_failure = Some((self.generation, error));
                return;
            }
        }
        if self.p2p_enabled {
            self.poll_p2p(now);
            self.poll_mdns(now);
        }
        self.poll_udp_bind_retry(now);
        self.poll_relay_keepalive(now);
        self.poll_rtt_probe(now);
        if self.pending_publish.is_some() {
            output.publish_p2p = self.pending_publish.take();
        }
    }

    fn authenticated(&mut self, session_id: SessionId) {
        self.session_id = Some(session_id);
        if self.initial_bind_attempted {
            self.awaiting_udp_bound = true;
            self.udp_bind_attempts = 0;
            self.next_udp_bind_retry = Instant::now() + UDP_BIND_RETRY_INTERVAL;
            if self.p2p_enabled {
                self.send_nat_probe(0, self.server_udp_addr);
                if let Some(addr) = self.server_udp_probe_addr {
                    self.send_nat_probe(1, addr);
                }
            }
        } else {
            self.bind_udp();
        }
    }

    fn voice_started(
        &mut self,
        room_id: RoomId,
        session_id: SessionId,
        user_id: UserId,
        stream_id: StreamId,
        local: bool,
    ) {
        if local {
            self.reset_voice_peer_state();
            self.voice_room = Some(room_id);
            self.active_stream = Some(stream_id);
            self.voice_others.clear();
            self.inbound_streams.clear();
            self.voice_dedup.clear();
            self.local_sequence = 0;
            self.encoder_feedback = EncoderFeedbackController::new();
            let _ = self.events.send(NetworkEvent::EncoderProfileChanged(
                crate::audio::LiveEncoderProfile::DRED_20,
            ));
            self.publish_p2p_candidates();
        } else if self.voice_room == Some(room_id) {
            self.voice_others.insert(user_id);
            let previous = self.inbound_streams.insert(
                stream_id,
                InboundVoiceStream {
                    session_id,
                    user_id,
                },
            );
            if previous.is_some_and(|previous| previous.session_id != session_id) {
                self.voice_dedup.remove_stream(stream_id);
            }
        }
    }

    fn voice_stopped(
        &mut self,
        room_id: RoomId,
        _session_id: SessionId,
        user_id: UserId,
        stream_id: StreamId,
        local: bool,
    ) {
        if local || self.active_stream == Some(stream_id) {
            self.active_stream = None;
            if self.voice_room == Some(room_id) {
                self.voice_room = None;
            }
            self.reset_voice_peer_state();
            self.inbound_streams.clear();
            self.voice_dedup.clear();
            self.pending_playback_packets.clear();
        } else if self.voice_room == Some(room_id) {
            self.voice_others.remove(&user_id);
        }
        self.inbound_streams.remove(&stream_id);
        self.voice_dedup.remove_stream(stream_id);
        self.clear_pending_playback_stream(stream_id);
    }

    fn udp_bound(&mut self) {
        if !self.awaiting_udp_bound {
            return;
        }
        self.awaiting_udp_bound = false;
        kvlog::info!("client udp bound");
        if self.udp_reported_unreachable {
            let _ = self
                .events
                .send(NetworkEvent::MediaConnectivity { udp_ok: true });
        }
        self.udp_reported_unreachable = false;
        self.udp_bind_attempts = 0;
    }

    fn nat_probe_observed(&mut self, probe_id: u8, addr: SocketAddr) {
        let server_addr = self
            .probe_addr_for_id(probe_id)
            .unwrap_or(self.server_udp_addr);
        self.p2p_nat_classifier.observe(ReflexiveObservation {
            server_addr,
            mapped_addr: addr,
        });
        let previous = (self.p2p_nat, self.p2p_reflexive_addr);
        self.p2p_nat = control_nat_kind(self.p2p_nat_classifier.classify());
        self.p2p_reflexive_addr = self.p2p_nat_classifier.primary_reflexive_addr();
        if (self.p2p_nat, self.p2p_reflexive_addr) != previous {
            self.publish_p2p_candidates();
        }
    }

    fn remove_peer(&mut self, session_id: SessionId, user_id: UserId) {
        self.p2p_peers.remove(&session_id);
        let _ = self.events.send(NetworkEvent::PeerTransport {
            user_id,
            direct: false,
        });
        self.publish_relay_rtt(user_id);
    }

    fn set_p2p_enabled(&mut self, enabled: bool) {
        if enabled && self.transport_mode != TransportMode::NativeEncrypted {
            let _ = self.events.send(NetworkEvent::Status(
                "P2P unavailable in external-secure-link mode".to_string(),
            ));
            return;
        }
        if self.p2p_enabled == enabled {
            return;
        }
        self.p2p_enabled = enabled;
        self.request_p2p_restart();
        if enabled {
            self.publish_p2p_candidates();
            let _ = self
                .events
                .send(NetworkEvent::Status("P2P enabled".to_string()));
        } else {
            self.publish_p2p_disabled();
            self.reset_voice_peer_state();
            self.interface_monitor.deactivate(Instant::now());
            let _ = self.events.send(NetworkEvent::Status(
                "P2P disabled; using relay".to_string(),
            ));
        }
    }

    fn bind_udp(&mut self) {
        if let Some(session_id) = self.session_id {
            kvlog::info!("udp bind sending", session_id = session_id.0);
            self.awaiting_udp_bound = true;
            self.udp_bind_attempts = 0;
            self.next_udp_bind_retry = Instant::now() + UDP_BIND_RETRY_INTERVAL;
            self.send_media(&MediaPayload::Bind);
            if self.p2p_enabled {
                self.send_nat_probe(0, self.server_udp_addr);
                if let Some(addr) = self.server_udp_probe_addr {
                    self.send_nat_probe(1, addr);
                }
            }
        }
    }

    fn poll_udp_bind_retry(&mut self, now: Instant) {
        if !self.awaiting_udp_bound || now < self.next_udp_bind_retry {
            return;
        }
        self.next_udp_bind_retry = now + UDP_BIND_RETRY_INTERVAL;
        if self.session_id.is_some() {
            self.send_media(&MediaPayload::Bind);
        }
        self.udp_bind_attempts = self.udp_bind_attempts.saturating_add(1);
        if self.udp_bind_attempts >= UDP_BIND_FAILURE_ATTEMPTS && !self.udp_reported_unreachable {
            self.udp_reported_unreachable = true;
            let _ = self
                .events
                .send(NetworkEvent::MediaConnectivity { udp_ok: false });
        }
    }

    fn send_nat_probe(&mut self, probe_id: u8, addr: SocketAddr) {
        let counter = self.media_send_counter;
        self.media_send_counter = self.media_send_counter.wrapping_add(1);
        match media::seal_media(
            &self.media,
            counter,
            &MediaPayload::NatProbe { probe_id },
        ) {
            Ok(packet) => self.send_udp_raw("nat_probe", None, addr, &packet),
            Err(error) => kvlog::warn!("nat probe seal failed", probe_id, error = %error),
        }
    }

    fn probe_addr_for_id(&self, probe_id: u8) -> Option<SocketAddr> {
        match probe_id {
            0 => Some(self.server_udp_addr),
            1 => self.server_udp_probe_addr,
            _ => None,
        }
    }

    fn poll_interfaces(&mut self, now: Instant) {
        match self.interface_monitor.poll_with(
            self.p2p_enabled && self.voice_room.is_some(),
            now,
            InterfaceSnapshot::capture,
        ) {
            Ok(Some(true)) => self.request_p2p_restart(),
            Ok(_) => {}
            Err(error) => kvlog::warn!("network interface discovery failed", error = %error),
        }
    }

    fn request_p2p_restart(&mut self) {
        self.p2p_generation = self.p2p_generation.wrapping_add(1).max(1);
        self.p2p_reflexive_addr = None;
        self.p2p_candidates.clear();
        self.p2p_local_candidates.clear();
        self.mdns_pending.clear();
        self.p2p_nat_classifier = chatt_p2p::NatClassifier::new();
        self.p2p_nat = configured_nat_kind();
        self.udp_rebind_requested = true;
    }

    fn reconcile_mdns(&mut self, poll: &Poll) {
        if self.p2p_enabled && !self.mdns.is_bound() {
            if let Err(error) = self.mdns.rebind(poll.registry()) {
                kvlog::warn!("failed to register mdns sockets", error = %error);
            }
        } else if !self.p2p_enabled && self.mdns.is_bound() {
            self.mdns.shutdown(poll.registry());
        }
    }

    fn rebind_udp_socket(&mut self, poll: &mut Poll) -> Result<(), String> {
        self.udp_rebind_requested = false;
        let _ = poll.registry().deregister(&mut self.udp);
        self.restart_port_policy.record(self.udp_local_addr.port());
        let bind_addr = RestartPortPolicy::bind_addr_for_restart(
            if self.server_udp_addr.is_ipv4() {
                "0.0.0.0:0".parse().unwrap()
            } else {
                "[::]:0".parse().unwrap()
            },
        );
        let mut last_error = None;
        for _ in 0..8 {
            match bind_voice_udp_socket(bind_addr) {
                Ok(socket) => {
                    let local_addr = socket.local_addr().map_err(|error| {
                        format!("failed to read rebound UDP address: {error}")
                    })?;
                    if !self.restart_port_policy.accepts(local_addr.port()) {
                        self.restart_port_policy.record(local_addr.port());
                        continue;
                    }
                    self.udp_local_addr = local_addr;
                    self.udp = UdpSocket::from_std(socket);
                    poll.registry()
                        .register(&mut self.udp, UDP, Interest::READABLE)
                        .map_err(|error| format!("failed to register rebound UDP socket: {error}"))?;
                    self.reset_server_rtt();
                    self.bind_udp();
                    self.publish_p2p_candidates();
                    return Ok(());
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(format!(
            "failed to rebind UDP socket to fresh port{}",
            last_error.map(|error| format!(": {error}")).unwrap_or_default()
        ))
    }

    fn publish_p2p_candidates(&mut self) {
        if !self.p2p_enabled {
            return;
        }
        let Some(room_id) = self.voice_room else {
            return;
        };
        if self.session_id.is_none() {
            return;
        }
        if let Err(error) = self
            .interface_monitor
            .ensure_with(Instant::now(), InterfaceSnapshot::capture)
        {
            kvlog::warn!("host candidate discovery failed", error = %error);
        }
        let gathered = self.gather_p2p_candidates();
        self.p2p_local_candidates = gathered.local;
        self.p2p_candidates = gathered.published.clone();
        self.mdns.publish_names(gathered.mdns_names);
        self.pending_publish = Some(PublishP2pRequest {
            generation: self.generation,
            room_id,
            candidate_generation: self.p2p_generation,
            nat: self.p2p_nat,
            tie_breaker: self.p2p_tie_breaker,
            candidates: gathered.published,
        });
    }

    fn publish_p2p_disabled(&mut self) {
        let Some(room_id) = self.voice_room else {
            return;
        };
        if self.session_id.is_none() {
            return;
        }
        self.p2p_local_candidates.clear();
        self.p2p_candidates.clear();
        self.mdns.publish_names(std::iter::empty());
        self.pending_publish = Some(PublishP2pRequest {
            generation: self.generation,
            room_id,
            candidate_generation: self.p2p_generation,
            nat: self.p2p_nat,
            tie_breaker: self.p2p_tie_breaker,
            candidates: Vec::new(),
        });
    }

    fn reset_voice_peer_state(&mut self) {
        let users = self
            .p2p_peers
            .values()
            .map(|peer| peer.user_id)
            .collect::<HashSet<_>>();
        self.p2p_peers.clear();
        self.mdns_pending.clear();
        self.room_server_rtts.clear();
        self.voice_others.clear();
        for user_id in users {
            let _ = self.events.send(NetworkEvent::PeerTransport {
                user_id,
                direct: false,
            });
        }
    }

    fn gather_p2p_candidates(&self) -> GatheredP2p {
        let mut next_id = 1;
        let mut candidates = self
            .interface_monitor
            .snapshot()
            .map(|snapshot| {
                snapshot.host_candidates_with_metadata(
                    1,
                    self.p2p_generation,
                    self.udp_local_addr.port(),
                    true,
                    &mut next_id,
                    self.prefer_ipv6,
                )
            })
            .unwrap_or_default();
        if candidates.is_empty() {
            let fallback_ip = if self.server_udp_addr.is_ipv4() {
                "127.0.0.1".parse().unwrap()
            } else {
                "::1".parse().unwrap()
            };
            candidates.push(Candidate::with_metadata(
                next_id,
                1,
                self.p2p_generation,
                CandidateKind::Host,
                SocketAddr::new(fallback_ip, self.udp_local_addr.port()),
                None,
                true,
                self.prefer_ipv6,
            ));
            next_id = next_id.wrapping_add(1).max(1);
        }
        if let Some(reflexive) = self.p2p_reflexive_addr {
            candidates.push(Candidate::with_metadata(
                next_id,
                1,
                self.p2p_generation,
                CandidateKind::ServerReflexive,
                reflexive,
                Some(self.udp_local_addr),
                true,
                self.prefer_ipv6,
            ));
            next_id = next_id.wrapping_add(1).max(1);
        }
        candidates.push(Candidate::with_metadata(
            next_id,
            1,
            self.p2p_generation,
            CandidateKind::Relay,
            self.server_udp_addr,
            None,
            true,
            self.prefer_ipv6,
        ));
        apply_candidate_privacy(
            candidates,
            self.candidate_privacy,
            &aws_lc_rs::rand::SystemRandom::new(),
        )
    }

    fn install_p2p_peer(&mut self, peer: P2pPeerInfo) -> Result<(), String> {
        if !self.p2p_enabled {
            return Ok(());
        }
        if self.voice_room != Some(peer.room_id) {
            return Ok(());
        }
        if let Some(existing) = self.p2p_peers.get(&peer.session_id)
            && p2p_peer_is_republish(existing, &peer, self.p2p_generation)
        {
            return Ok(());
        }
        let send_key = key_from_control(&peer.send_key)?;
        let recv_key = key_from_control(&peer.recv_key)?;
        let stun_key = key_from_control(&peer.stun_key)?.bytes;
        let mut transaction_salt = [0u8; 32];
        aws_lc_rs::rand::SystemRandom::new()
            .fill(&mut transaction_salt)
            .map_err(|_| "failed to generate STUN transaction salt".to_string())?;
        let auth = StunAuth::new(stun_key, transaction_salt);
        let local_candidates = self.p2p_local_candidates.clone();
        let mut remote_candidates = Vec::new();
        let mut pending = Vec::new();
        for control in &peer.candidates {
            if let Some(candidate) = candidate_from_control(control) {
                remote_candidates.push(candidate);
            } else if let Some((name, port)) = split_mdns_addr(&control.addr) {
                pending.push((name, control.clone(), port));
            }
        }
        if local_candidates.is_empty() {
            return Err("missing local P2P candidates".to_string());
        }
        if remote_candidates.is_empty() && pending.is_empty() {
            return Err("missing remote P2P candidates".to_string());
        }
        let config = P2pAgentConfig {
            username: Some(p2p_username(peer.connection_id)),
            keepalive_interval: P2P_KEEPALIVE_INTERVAL,
            consent_timeout: P2P_CONSENT_TIMEOUT,
            ..P2pAgentConfig::with_auth(auth)
        };
        let agent = TraversalAgent::new(
            Instant::now(),
            config,
            ice_role_from_control(peer.role),
            self.p2p_tie_breaker,
            nat_from_control(self.p2p_nat),
            nat_from_control(peer.nat),
            local_candidates,
            remote_candidates,
        );
        let session_id = peer.session_id;
        self.p2p_peers.insert(
            session_id,
            PeerConnection {
                user_id: peer.user_id,
                agent,
                send_key,
                recv_key,
                send_counter: 0,
                recv_replay: AntiReplay::new(),
                connection_id: peer.connection_id,
                remote_generation: peer.generation,
                local_generation: self.p2p_generation,
                direct_stable_since: None,
                last_direct_inbound: None,
                rtt_in_flight: VecDeque::new(),
                rtt_ms: None,
            },
        );
        let now = Instant::now();
        for (name, control, port) in pending {
            self.mdns.start_resolve(&name, now);
            self.mdns_pending.insert(
                name,
                MdnsPending {
                    session_id,
                    control,
                    port,
                },
            );
        }
        Ok(())
    }

    fn handle_mdns_readable(&mut self, token: Token, now: Instant) -> bool {
        let outcome = self.mdns.handle_readable(token, now, MDNS_DRAIN_BUDGET);
        for (name, ip) in outcome.resolved {
            let Some(pending) = self.mdns_pending.remove(&name) else {
                continue;
            };
            let addr = SocketAddr::new(ip, pending.port);
            let candidate = candidate_from_control_with_addr(&pending.control, addr);
            if let Some(peer) = self.p2p_peers.get_mut(&pending.session_id) {
                peer.agent.add_remote_candidate(now, candidate);
            }
        }
        outcome.hit_limit
    }

    fn poll_mdns(&mut self, now: Instant) {
        for name in self.mdns.handle_timeout(now) {
            self.mdns_pending.remove(&name);
        }
    }

    fn poll_p2p(&mut self, now: Instant) {
        let actions = self
            .p2p_peers
            .iter_mut()
            .map(|(session_id, peer)| (*session_id, peer.agent.poll(now)))
            .filter(|(_, actions)| !actions.is_empty())
            .collect::<Vec<_>>();
        for (session_id, actions) in actions {
            self.apply_p2p_actions(session_id, actions);
        }
        self.reconcile_direct_stability(now);
    }

    fn reconcile_direct_stability(&mut self, now: Instant) {
        for peer in self.p2p_peers.values_mut() {
            let healthy = direct_path_healthy(
                peer.agent.selected().is_some(),
                peer.last_direct_inbound,
                now,
                DIRECT_FAILOVER_IDLE,
            );
            if healthy {
                if peer.direct_stable_since.is_none() {
                    peer.direct_stable_since = Some(now);
                }
            } else {
                peer.direct_stable_since = None;
            }
        }
    }

    fn relay_suppressed(&self, now: Instant) -> bool {
        relay_suppressed(
            now,
            DIRECT_CONFIRM_WINDOW,
            &self.voice_others,
            self.p2p_peers
                .values()
                .map(|peer| (peer.user_id, peer.direct_stable_since)),
        )
    }

    fn poll_relay_keepalive(&mut self, now: Instant) {
        if !self.relay_suppressed(now) {
            self.next_relay_keepalive = now + RELAY_KEEPALIVE_INTERVAL;
            return;
        }
        if now >= self.next_relay_keepalive {
            self.next_relay_keepalive = now + RELAY_KEEPALIVE_INTERVAL;
            if self.session_id.is_some() {
                self.send_media(&MediaPayload::Bind);
            }
        }
    }

    fn publish_relay_rtt(&self, user_id: UserId) {
        if self
            .p2p_peers
            .values()
            .any(|peer| peer.user_id == user_id && peer.agent.selected().is_some())
        {
            return;
        }
        let rtt_ms = combined_relay_rtt(
            self.server_rtt_ms,
            self.room_server_rtts.get(&user_id).copied(),
        );
        let _ = self.events.send(NetworkEvent::PeerRtt { user_id, rtt_ms });
    }

    fn publish_all_relay_rtts(&self) {
        for user_id in &self.voice_others {
            self.publish_relay_rtt(*user_id);
        }
    }

    fn reset_server_rtt(&mut self) {
        self.server_rtt_ms = None;
        self.server_rtt_last_sample_at = None;
        self.server_rtt_in_flight.clear();
        let _ = self.events.send(NetworkEvent::ServerRtt { rtt_ms: None });
        self.publish_all_relay_rtts();
    }

    fn next_rtt_nonce(&mut self) -> u64 {
        self.rtt_probe_seq = self.rtt_probe_seq.wrapping_add(1);
        self.rtt_probe_seq
    }

    fn poll_rtt_probe(&mut self, now: Instant) {
        if rtt_sample_is_stale(self.server_rtt_last_sample_at, now) {
            self.reset_server_rtt();
        }
        if now < self.next_rtt_probe {
            return;
        }
        self.next_rtt_probe = now + RTT_PROBE_INTERVAL;
        if self.session_id.is_some() {
            let nonce = self.next_rtt_nonce();
            push_rtt_in_flight(&mut self.server_rtt_in_flight, nonce, now);
            self.send_media(&MediaPayload::Ping {
                nonce,
                observed_rtt_ms: self.server_rtt_ms.map(clamp_rtt_ms),
            });
        }
        let peer_sessions = self
            .p2p_peers
            .iter()
            .filter_map(|(id, peer)| peer.agent.selected().is_some().then_some(*id))
            .collect::<Vec<_>>();
        for session_id in peer_sessions {
            let nonce = self.next_rtt_nonce();
            self.send_p2p_ping(session_id, nonce, now);
        }
    }

    fn send_p2p_ping(&mut self, session_id: SessionId, nonce: u64, now: Instant) {
        let Some((addr, packet)) = self.p2p_peers.get_mut(&session_id).and_then(|peer| {
            let addr = peer.agent.selected()?.remote_addr;
            let counter = peer.send_counter;
            peer.send_counter = peer.send_counter.wrapping_add(1);
            push_rtt_in_flight(&mut peer.rtt_in_flight, nonce, now);
            Some((
                addr,
                media::seal_peer_media(
                    &peer.send_key,
                    counter,
                    &MediaPayload::Ping {
                        nonce,
                        observed_rtt_ms: None,
                    },
                ),
            ))
        }) else {
            return;
        };
        match packet {
            Ok(packet) => self.send_udp_raw("p2p_ping", Some(session_id), addr, &packet),
            Err(error) => kvlog::warn!("p2p ping seal failed", error = %error),
        }
    }

    fn send_p2p_pong(&mut self, session_id: SessionId, nonce: u64) {
        let Some((addr, packet)) = self.p2p_peers.get_mut(&session_id).and_then(|peer| {
            let addr = peer.agent.selected()?.remote_addr;
            let counter = peer.send_counter;
            peer.send_counter = peer.send_counter.wrapping_add(1);
            Some((
                addr,
                media::seal_peer_media(&peer.send_key, counter, &MediaPayload::Pong { nonce }),
            ))
        }) else {
            return;
        };
        match packet {
            Ok(packet) => self.send_udp_raw("p2p_pong", Some(session_id), addr, &packet),
            Err(error) => kvlog::warn!("p2p pong seal failed", error = %error),
        }
    }

    /// Drains a bounded receive burst. `true` retains local work so an
    /// edge-triggered readable socket cannot strand datagrams after the budget.
    fn read_udp(&mut self) -> bool {
        let mut buf = [0u8; 2048];
        let mut datagrams_this_wake = 0usize;
        loop {
            let (len, src) = match recv_udp_datagram(&self.udp, &mut buf) {
                Ok(Some(value)) => value,
                Ok(None) => {
                    if datagrams_this_wake > 1 {
                        kvlog::info!("udp read coalesced", datagrams = datagrams_this_wake);
                    }
                    return false;
                }
                Err(error) => {
                    kvlog::warn!("udp receive failed", error = %error);
                    let _ = self
                        .events
                        .send(NetworkEvent::Error(format!("UDP receive failed: {error}")));
                    return false;
                }
            };
            datagrams_this_wake += 1;
            // Capture arrival immediately after recv_from, before parsing,
            // decryption, logging, or any cross-thread notification.
            let now = Instant::now();
            let packet = &buf[..len];
            if chatt_p2p::stun::is_stun_message(packet) {
                self.handle_p2p_stun(now, src, packet);
            } else if self.handle_p2p_media(now, src, packet) {
            } else if src != self.server_udp_addr {
                kvlog::warn!(
                    "udp packet ignored",
                    addr = %src,
                    expected_addr = %self.server_udp_addr,
                    packet_size = len
                );
            } else {
                match self.open_server_media(packet) {
                    Ok((_, MediaPayload::Voice {
                        stream_id,
                        sequence,
                        timestamp,
                        flags,
                        payload,
                    })) => {
                        let payload_size = payload.len();
                        let payload_kind = media_voice_payload_kind(&payload);
                        kvlog::info!(
                            "voice packet received",
                            route = "server",
                            stream_id = stream_id.0,
                            sequence,
                            media_timestamp = timestamp,
                            flags,
                            payload_size,
                            payload_kind
                        );
                        log_audio_pop_media_packet(
                            "rx",
                            "server",
                            stream_id.0,
                            sequence,
                            timestamp,
                            flags,
                            payload_size,
                            payload_kind,
                        );
                        self.dispatch_voice_packet(
                            crate::audio::RemoteVoicePacket {
                                stream_id: stream_id.0,
                                sequence,
                                timestamp,
                                flags,
                                payload: audio_payload_from_media(payload),
                                received_at: now,
                            },
                            "server",
                        );
                    }
                    Ok((_, MediaPayload::Pong { nonce })) => {
                        if let Some(sample) =
                            take_rtt_sample(&mut self.server_rtt_in_flight, nonce, now)
                        {
                            let rtt = fold_rtt_ewma(self.server_rtt_ms, sample);
                            self.server_rtt_ms = Some(rtt);
                            self.server_rtt_last_sample_at = Some(now);
                            let _ = self.events.send(NetworkEvent::ServerRtt {
                                rtt_ms: Some(clamp_rtt_ms(rtt)),
                            });
                            self.publish_all_relay_rtts();
                        }
                    }
                    Ok((_, MediaPayload::VoiceFeedbackFrom {
                        reporter,
                        stream_id,
                        feedback,
                    })) => {
                        let feedback = live_feedback_from_media(stream_id, feedback);
                        self.handle_encoder_feedback(reporter, feedback, now);
                    }
                    Ok((_, MediaPayload::Ping { nonce, .. })) => {
                        self.send_media(&MediaPayload::Pong { nonce });
                    }
                    Ok((_, MediaPayload::Bind | MediaPayload::NatProbe { .. })) => {}
                    Ok((_, MediaPayload::PeerVoice { .. }
                        | MediaPayload::PeerVoiceFeedback { .. }
                        | MediaPayload::VoiceFeedback { .. })) => {}
                    Err(error) => {
                        kvlog::warn!("udp packet rejected", packet_size = len, error = %error);
                        let _ = self
                            .events
                            .send(NetworkEvent::Error(format!("UDP packet rejected: {error}")));
                    }
                }
            }
            if datagrams_this_wake >= UDP_DRAIN_BUDGET {
                return true;
            }
        }
    }

    fn open_server_media(
        &mut self,
        packet: &[u8],
    ) -> Result<(media::UdpHeader, MediaPayload), media::MediaError> {
        let opened = media::open_media(&self.media, &mut self.media_recv_replay, packet)?;
        Ok((opened.header, opened.payload))
    }

    fn dispatch_voice_packet(
        &mut self,
        packet: crate::audio::RemoteVoicePacket,
        route: &'static str,
    ) {
        let stream_id = packet.stream_id;
        let sequence = packet.sequence;
        let timestamp = packet.timestamp;
        let flags = packet.flags;
        let payload_size = packet.payload.len();
        let payload_kind = voice_payload_kind(&packet.payload);
        match self.voice_dedup.observe(stream_id, sequence) {
            RecentVoiceSequenceResult::New => {
                kvlog::info!(
                    "voice packet accepted",
                    route,
                    stream_id,
                    sequence,
                    media_timestamp = timestamp,
                    flags,
                    payload_size,
                    payload_kind
                );
            }
            RecentVoiceSequenceResult::Duplicate => {
                kvlog::info!(
                    "duplicate voice packet dropped",
                    route,
                    stream_id,
                    sequence,
                    media_timestamp = timestamp,
                    flags,
                    payload_size,
                    payload_kind
                );
                return;
            }
            RecentVoiceSequenceResult::Stale => {
                kvlog::info!(
                    "stale voice packet dropped",
                    route,
                    stream_id,
                    sequence,
                    media_timestamp = timestamp,
                    flags,
                    payload_size,
                    payload_kind
                );
                return;
            }
        }
        dispatch_voice_packet_to(
            &self.events,
            self.playback_ingress.sink(),
            self.playback_ingress.buffer_without_sink(),
            &mut self.pending_playback_packets,
            packet,
        );
    }

    fn send_local_voice(&mut self, frame: LocalVoiceFrame, sequence: Option<u32>) {
        let Some(stream_id) = self.active_stream else {
            return;
        };
        let sequence = match sequence {
            Some(sequence) => {
                advance_local_voice_sequence_past(&mut self.local_sequence, sequence);
                sequence
            }
            None => allocate_local_voice_sequence(&mut self.local_sequence),
        };
        let timestamp = frame.timestamp;
        kvlog::info!(
            "voice packet sent",
            stream_id = stream_id.0,
            sequence,
            media_timestamp = timestamp,
            flags = frame.flags,
            payload_size = frame.payload.len(),
            payload_kind = voice_payload_kind(&frame.payload)
        );
        log_audio_pop_media_packet(
            "tx",
            "local",
            stream_id.0,
            sequence,
            timestamp,
            frame.flags,
            frame.payload.len(),
            voice_payload_kind(&frame.payload),
        );
        if !self.relay_suppressed(Instant::now()) {
            self.send_media(&MediaPayload::Voice {
                stream_id,
                sequence,
                timestamp,
                flags: frame.flags,
                payload: media_payload_from_audio(&frame.payload),
            });
        }
        self.send_p2p_voice(stream_id, sequence, timestamp, frame.flags, &frame.payload);
    }

    fn set_playback_sink(&mut self, sink: Option<LivePlaybackSink>) {
        self.set_playback_ingress(PlaybackIngressState::from_sink_update(sink));
    }

    fn set_playback_ingress(&mut self, state: PlaybackIngressState) {
        let PlaybackIngressState::Attached(sink) = state else {
            if matches!(state, PlaybackIngressState::Suspended) {
                self.pending_playback_packets.clear();
            }
            self.playback_ingress = state;
            return;
        };
        while let Some(packet) = self.pending_playback_packets.pop_front() {
            sink.push(packet);
        }
        self.playback_ingress = PlaybackIngressState::Attached(sink);
    }

    fn clear_pending_playback_stream(&mut self, stream_id: StreamId) {
        self.pending_playback_packets
            .retain(|packet| packet.stream_id != stream_id.0);
    }

    fn send_playback_feedback(&mut self, feedback: LivePlaybackFeedback) {
        let stream_id = StreamId(feedback.stream_id);
        let owner = self
            .inbound_streams
            .get(&stream_id)
            .map(|stream| stream.session_id);
        let _ = self.events.send(NetworkEvent::PlaybackFeedback(feedback));
        let owner_direct_stable = owner
            .and_then(|owner| self.p2p_peers.get(&owner))
            .and_then(|peer| peer.direct_stable_since)
            .is_some_and(|since| {
                Instant::now().saturating_duration_since(since) >= DIRECT_CONFIRM_WINDOW
            });
        if !owner_direct_stable {
            self.send_media(&MediaPayload::VoiceFeedback {
                stream_id,
                feedback: media_feedback_from_live(feedback),
            });
        }
        if let Some(owner) = owner {
            self.send_p2p_voice_feedback(owner, stream_id, feedback);
        }
    }

    fn send_media(&mut self, payload: &MediaPayload) {
        let kind = media_payload_kind(payload);
        let counter = self.media_send_counter;
        self.media_send_counter = self.media_send_counter.wrapping_add(1);
        match media::seal_media_into(
            &self.media,
            counter,
            payload,
            &mut self.media_packet,
            &mut self.media_scratch,
        ) {
            Ok(()) => {
                let packet = std::mem::take(&mut self.media_packet);
                if let Err(error) = self.udp.send_to(&packet, self.server_udp_addr) {
                    kvlog::warn!("udp send failed", kind, packet_size = packet.len(), error = %error);
                    let _ = self
                        .events
                        .send(NetworkEvent::Error(format!("UDP send failed: {error}")));
                } else if !matches!(
                    payload,
                    MediaPayload::Voice { .. } | MediaPayload::VoiceFeedback { .. }
                ) {
                    kvlog::info!("udp packet sent", kind, packet_size = packet.len(), counter);
                }
                self.media_packet = packet;
            }
            Err(error) => {
                kvlog::warn!("udp seal failed", kind, error = %error);
                let _ = self
                    .events
                    .send(NetworkEvent::Error(format!("UDP seal failed: {error}")));
            }
        }
    }

    fn send_udp_raw(
        &mut self,
        kind: &'static str,
        session_id: Option<SessionId>,
        addr: SocketAddr,
        packet: &[u8],
    ) {
        match self.udp.send_to(packet, addr) {
            Ok(_) => {}
            Err(error) if chatt_p2p::socket::is_ignorable_udp_error(&error) => {
                kvlog::warn!(
                    "udp send got ignorable socket error",
                    kind,
                    session_id = session_id.map(|id| id.0),
                    addr = %addr,
                    error = %error
                );
            }
            Err(error) => {
                kvlog::warn!(
                    "udp send failed",
                    kind,
                    session_id = session_id.map(|id| id.0),
                    addr = %addr,
                    error = %error
                );
                let _ = self
                    .events
                    .send(NetworkEvent::Error(format!("UDP send failed: {error}")));
            }
        }
    }

    fn handle_p2p_stun(&mut self, now: Instant, src: SocketAddr, packet: &[u8]) {
        let username = StunMessage::decode(packet)
            .ok()
            .and_then(|message| message.username);
        let targets = if let Some(connection_id) = username
            .as_deref()
            .and_then(connection_id_from_p2p_username)
        {
            self.p2p_peers
                .iter()
                .filter_map(|(session_id, peer)| {
                    (peer.connection_id == connection_id).then_some(*session_id)
                })
                .collect::<Vec<_>>()
        } else {
            self.p2p_peers.keys().copied().collect::<Vec<_>>()
        };
        let mut pending = Vec::new();
        for session_id in targets {
            let Some(peer) = self.p2p_peers.get_mut(&session_id) else {
                continue;
            };
            match peer.agent.handle_inbound(now, src, packet) {
                Ok(actions) => {
                    peer.last_direct_inbound = Some(now);
                    if !actions.is_empty() {
                        pending.push((session_id, actions));
                    }
                }
                Err(error) => kvlog::warn!(
                    "p2p stun packet rejected",
                    session_id = session_id.0,
                    addr = %src,
                    error = %error
                ),
            }
        }
        for (session_id, actions) in pending {
            self.apply_p2p_actions(session_id, actions);
        }
    }

    fn handle_p2p_media(&mut self, now: Instant, src: SocketAddr, packet: &[u8]) -> bool {
        let Ok((header, _)) = media::parse_header(packet) else {
            return false;
        };
        let Some(session_id) = self.p2p_peers.iter().find_map(|(session_id, peer)| {
            (peer.recv_key.id == header.route_id).then_some(*session_id)
        }) else {
            return false;
        };
        let inbound_streams = &self.inbound_streams;
        let active_stream = self.active_stream;
        let outcome = {
            let peer = self.p2p_peers.get_mut(&session_id).unwrap();
            match media::open_peer_media(&peer.recv_key, &mut peer.recv_replay, packet) {
                Ok((_, MediaPayload::PeerVoice {
                    connection_id,
                    stream_id,
                    sequence,
                    timestamp,
                    flags,
                    payload,
                })) if connection_id == peer.connection_id
                    && stream_owner_matches(inbound_streams, stream_id, session_id) =>
                {
                    let action = peer.agent.observe_authenticated_packet(now, src);
                    peer.last_direct_inbound = Some(now);
                    Ok(P2pMediaPacket::Voice {
                        stream_id,
                        sequence,
                        timestamp,
                        flags,
                        payload,
                        action,
                    })
                }
                Ok((_, MediaPayload::PeerVoiceFeedback {
                    connection_id,
                    stream_id,
                    feedback,
                })) if connection_id == peer.connection_id
                    && active_stream == Some(stream_id) =>
                {
                    let action = peer.agent.observe_authenticated_packet(now, src);
                    peer.last_direct_inbound = Some(now);
                    Ok(P2pMediaPacket::Feedback {
                        stream_id,
                        feedback,
                        action,
                    })
                }
                Ok((_, MediaPayload::Ping { nonce, .. })) => {
                    let action = peer.agent.observe_authenticated_packet(now, src);
                    peer.last_direct_inbound = Some(now);
                    Ok(P2pMediaPacket::Ping { nonce, action })
                }
                Ok((_, MediaPayload::Pong { nonce })) => {
                    let action = peer.agent.observe_authenticated_packet(now, src);
                    peer.last_direct_inbound = Some(now);
                    let rtt_ms = take_rtt_sample(&mut peer.rtt_in_flight, nonce, now).map(|sample| {
                        let rtt = fold_rtt_ewma(peer.rtt_ms, sample);
                        peer.rtt_ms = Some(rtt);
                        clamp_rtt_ms(rtt)
                    });
                    Ok(P2pMediaPacket::Pong { rtt_ms, action })
                }
                Ok(_) => Err("unexpected P2P media payload".to_string()),
                Err(error) => Err(error.to_string()),
            }
        };
        match outcome {
            Ok(P2pMediaPacket::Voice {
                stream_id,
                sequence,
                timestamp,
                flags,
                payload,
                action,
            }) => {
                if let Some(action) = action {
                    self.apply_p2p_actions(session_id, vec![action]);
                }
                let payload_size = payload.len();
                let payload_kind = media_voice_payload_kind(&payload);
                kvlog::info!(
                    "voice packet received",
                    route = "p2p",
                    stream_id = stream_id.0,
                    sequence,
                    media_timestamp = timestamp,
                    flags,
                    payload_size,
                    payload_kind
                );
                log_audio_pop_media_packet(
                    "rx",
                    "p2p",
                    stream_id.0,
                    sequence,
                    timestamp,
                    flags,
                    payload_size,
                    payload_kind,
                );
                self.dispatch_voice_packet(
                    crate::audio::RemoteVoicePacket {
                        stream_id: stream_id.0,
                        sequence,
                        timestamp,
                        flags,
                        payload: audio_payload_from_media(payload),
                        received_at: now,
                    },
                    "p2p",
                );
            }
            Ok(P2pMediaPacket::Feedback { stream_id, feedback, action }) => {
                if let Some(action) = action {
                    self.apply_p2p_actions(session_id, vec![action]);
                }
                if let Some(reporter) = self.p2p_peers.get(&session_id).map(|peer| peer.user_id) {
                    self.handle_encoder_feedback(
                        reporter,
                        live_feedback_from_media(stream_id, feedback),
                        now,
                    );
                }
            }
            Ok(P2pMediaPacket::Ping { nonce, action }) => {
                if let Some(action) = action {
                    self.apply_p2p_actions(session_id, vec![action]);
                }
                self.send_p2p_pong(session_id, nonce);
            }
            Ok(P2pMediaPacket::Pong { rtt_ms, action }) => {
                if let Some(action) = action {
                    self.apply_p2p_actions(session_id, vec![action]);
                }
                if let (Some(rtt_ms), Some(user_id)) = (
                    rtt_ms,
                    self.p2p_peers.get(&session_id).map(|peer| peer.user_id),
                ) {
                    let _ = self.events.send(NetworkEvent::PeerRtt {
                        user_id,
                        rtt_ms: Some(rtt_ms),
                    });
                }
            }
            Err(error) => kvlog::warn!(
                "p2p media packet rejected",
                session_id = session_id.0,
                addr = %src,
                error = error.as_str()
            ),
        }
        true
    }

    fn send_p2p_voice(
        &mut self,
        stream_id: StreamId,
        sequence: u32,
        timestamp: u32,
        flags: u8,
        audio_payload: &crate::audio::VoicePayload,
    ) {
        let mut routes = std::mem::take(&mut self.p2p_routes);
        routes.clear();
        for (session_id, peer) in &mut self.p2p_peers {
            let Some(selected) = peer.agent.selected() else {
                continue;
            };
            let counter = peer.send_counter;
            peer.send_counter = peer.send_counter.wrapping_add(1);
            routes.push(P2pVoiceRoute {
                session_id: *session_id,
                addr: selected.remote_addr,
                connection_id: peer.connection_id,
                counter,
                key: peer.send_key.clone(),
            });
        }
        for route in &routes {
            let payload = MediaPayload::PeerVoice {
                connection_id: route.connection_id,
                stream_id,
                sequence,
                timestamp,
                flags,
                payload: media_payload_from_audio(audio_payload),
            };
            match media::seal_peer_media_into(
                &route.key,
                route.counter,
                &payload,
                &mut self.media_packet,
                &mut self.media_scratch,
            ) {
                Ok(()) => {
                    let packet = std::mem::take(&mut self.media_packet);
                    self.send_udp_raw("p2p_voice", Some(route.session_id), route.addr, &packet);
                    self.media_packet = packet;
                }
                Err(error) => kvlog::warn!("p2p media seal failed", error = %error),
            }
        }
        self.p2p_routes = routes;
    }

    fn send_p2p_voice_feedback(
        &mut self,
        session_id: SessionId,
        stream_id: StreamId,
        feedback: LivePlaybackFeedback,
    ) {
        let Some((addr, packet)) = self.p2p_peers.get_mut(&session_id).and_then(|peer| {
            let addr = peer.agent.selected()?.remote_addr;
            let payload = MediaPayload::PeerVoiceFeedback {
                connection_id: peer.connection_id,
                stream_id,
                feedback: media_feedback_from_live(feedback),
            };
            let counter = peer.send_counter;
            peer.send_counter = peer.send_counter.wrapping_add(1);
            Some((addr, media::seal_peer_media(&peer.send_key, counter, &payload)))
        }) else {
            return;
        };
        match packet {
            Ok(packet) => self.send_udp_raw(
                "p2p_voice_feedback",
                Some(session_id),
                addr,
                &packet,
            ),
            Err(error) => kvlog::warn!("p2p feedback seal failed", error = %error),
        }
    }

    fn apply_p2p_actions(&mut self, session_id: SessionId, actions: Vec<P2pAction>) {
        for action in actions {
            match action {
                P2pAction::UseRelay { reason, .. } => {
                    if let Some(user_id) = self.p2p_peers.get(&session_id).map(|peer| peer.user_id) {
                        let _ = self.events.send(NetworkEvent::PeerTransport {
                            user_id,
                            direct: false,
                        });
                        self.publish_relay_rtt(user_id);
                    }
                    kvlog::info!("p2p using relay", session_id = session_id.0, reason = ?reason);
                }
                P2pAction::SendStun { to, bytes, .. }
                | P2pAction::SendStunResponse { to, bytes, .. }
                | P2pAction::SendKeepalive { to, bytes, .. } => {
                    self.send_udp_raw("p2p_stun", Some(session_id), to, &bytes);
                }
                P2pAction::DirectReady { selected } | P2pAction::Migrated { selected } => {
                    let user_id = self.p2p_peers.get(&session_id).map(|peer| peer.user_id);
                    if let Some(user_id) = user_id {
                        let _ = self.events.send(NetworkEvent::PeerTransport {
                            user_id,
                            direct: true,
                        });
                        let _ = self.events.send(NetworkEvent::Status(format!(
                            "p2p direct path to user {}",
                            user_id.0
                        )));
                    }
                    kvlog::info!(
                        "p2p direct path selected",
                        session_id = session_id.0,
                        addr = %selected.remote_addr,
                        peer_reflexive = selected.peer_reflexive
                    );
                }
                P2pAction::IceRestart { .. } => self.request_p2p_restart(),
                P2pAction::Disconnected => {
                    if let Some(peer) = self.p2p_peers.remove(&session_id) {
                        let _ = self.events.send(NetworkEvent::PeerTransport {
                            user_id: peer.user_id,
                            direct: false,
                        });
                        self.publish_relay_rtt(peer.user_id);
                    }
                    let _ = self.events.send(NetworkEvent::Status(
                        "p2p direct path timed out; using relay".to_string(),
                    ));
                }
                P2pAction::ConsentExpired => {
                    if let Some(peer) = self.p2p_peers.get_mut(&session_id) {
                        peer.direct_stable_since = None;
                        let user_id = peer.user_id;
                        let _ = self.events.send(NetworkEvent::PeerTransport {
                            user_id,
                            direct: false,
                        });
                        self.publish_relay_rtt(user_id);
                    }
                    let _ = self.events.send(NetworkEvent::Status(
                        "p2p consent expired; using relay".to_string(),
                    ));
                }
            }
        }
    }

    fn handle_encoder_feedback(
        &mut self,
        reporter: UserId,
        feedback: LivePlaybackFeedback,
        now: Instant,
    ) {
        let _ = self
            .events
            .send(NetworkEvent::OutboundFeedback { reporter, feedback });
        if self.active_stream != Some(StreamId(feedback.stream_id)) {
            return;
        }
        if let Some(profile) = self.encoder_feedback.observe(feedback, now) {
            let _ = self
                .events
                .send(NetworkEvent::EncoderProfileChanged(profile));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::VoicePayload;
    use rpc::crypto::{KEY_LEN, KeyMaterial};
    use std::sync::mpsc;

    fn protection(route_id: u32) -> MediaProtection {
        let key = KeyMaterial {
            id: route_id,
            bytes: [7; KEY_LEN],
        };
        MediaProtection::Aead {
            route_id,
            send: key.clone(),
            recv: key,
        }
    }

    fn submission() -> (Poll, VoiceCommandSubmission) {
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), COMMANDS).unwrap());
        let submission = VoiceCommandSubmission::new();
        submission.install_waker(waker);
        (poll, submission)
    }

    fn direct_loop() -> (VoiceLoop, Poll) {
        let poll = Poll::new().unwrap();
        let command_waker = Arc::new(Waker::new(poll.registry(), COMMANDS).unwrap());
        let commands = Arc::new(VoiceCommandSubmission::new());
        commands.install_waker(command_waker);
        let main_poll = Poll::new().unwrap();
        let main_waker = Arc::new(Waker::new(main_poll.registry(), Token(9)).unwrap());
        let outputs = Arc::new(VoiceOutputSubmission::new(main_waker));
        let (event_tx, _event_rx) = mpsc::channel();
        (
            VoiceLoop::new(
                poll,
                NetworkEventSender::for_test(event_tx),
                commands,
                outputs,
            ),
            main_poll,
        )
    }

    fn start_command(generation: u64) -> VoiceCommand {
        start_command_with_p2p(generation, false)
    }

    fn start_command_with_p2p(generation: u64, p2p_enabled: bool) -> VoiceCommand {
        let udp = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        udp.set_nonblocking(true).unwrap();
        VoiceCommand::StartSession {
            generation,
            udp,
            media: protection(100 + generation as u32),
            initial_bind_attempted: false,
            transport_mode: TransportMode::NativeEncrypted,
            server_udp_addr: "127.0.0.1:9".parse().unwrap(),
            server_udp_probe_addr: None,
            p2p_enabled,
            candidate_privacy: CandidatePrivacy::Disabled,
            prefer_ipv6: false,
        }
    }

    fn frame(sequence: u32) -> LocalVoiceFrame {
        LocalVoiceFrame {
            timestamp: sequence * 960,
            flags: 0,
            payload: VoicePayload::Opus(vec![sequence as u8]),
        }
    }

    fn local_voice_started(generation: u64) -> VoiceCommand {
        VoiceCommand::VoiceStarted {
            generation,
            room_id: RoomId(2),
            session_id: SessionId(4),
            user_id: UserId(3),
            stream_id: StreamId(9),
            local: true,
        }
    }

    #[test]
    fn microphone_queue_is_bounded_and_retains_newest_sequences() {
        let (_poll, submission) = submission();
        {
            let mut mailbox = submission.mailbox.lock().unwrap();
            mailbox.ingress_generation = Some(4);
            mailbox.activated_generation = Some(4);
        }
        for sequence in 0..(MAX_QUEUED_MICROPHONE_PACKETS as u32 + 5) {
            assert!(submission
                .submit_microphone(Some(sequence), frame(sequence))
                .is_ok());
        }
        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        assert!(!submission.drain_into(
            &mut controls,
            &mut microphone,
            &mut feedback,
            &mut sink,
        ));
        let sequences = microphone
            .into_iter()
            .map(|packet| packet.sequence.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(sequences.len(), MAX_QUEUED_MICROPHONE_PACKETS);
        assert_eq!(sequences, (5..15).collect::<Vec<_>>());
    }

    #[test]
    fn new_session_discards_old_fast_path_state_before_accepting_new_packets() {
        let (_poll, submission) = submission();
        {
            let mut mailbox = submission.mailbox.lock().unwrap();
            mailbox.ingress_generation = Some(1);
            mailbox.activated_generation = Some(1);
        }
        submission.submit_microphone(Some(1), frame(1)).unwrap();
        submission
            .submit_feedback(LivePlaybackFeedback {
                stream_id: 8,
                highest_contiguous_sequence: 1,
                ..LivePlaybackFeedback::default()
            })
            .unwrap();

        assert!(submission.submit(start_command(2)).is_ok());
        assert!(submission.activate(2).is_ok());
        assert!(submission.submit(local_voice_started(2)).is_ok());
        submission.submit_microphone(Some(2), frame(2)).unwrap();

        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        assert!(!submission.drain_into(
            &mut controls,
            &mut microphone,
            &mut feedback,
            &mut sink,
        ));
        assert!(matches!(
            controls.pop_front(),
            Some(VoiceCommand::StartSession { generation: 2, .. })
        ));
        assert_eq!(microphone.len(), 1);
        assert_eq!(microphone[0].generation, 2);
        assert_eq!(microphone[0].sequence, Some(2));
        assert!(feedback.is_empty());
    }

    #[test]
    fn submission_wakes_blocked_command_poll() {
        let (mut poll, submission) = submission();
        {
            let mut mailbox = submission.mailbox.lock().unwrap();
            mailbox.ingress_generation = Some(1);
            mailbox.activated_generation = Some(1);
        }
        assert!(submission.submit_microphone(None, frame(1)).is_ok());
        let mut events = Events::with_capacity(2);
        poll.poll(&mut events, Some(Duration::from_secs(1))).unwrap();
        assert!(events.iter().any(|event| event.token() == COMMANDS));
    }

    #[test]
    fn session_controls_wait_for_voice_activation() {
        let (mut poll, submission) = submission();
        assert!(submission.submit(start_command(7)).is_ok());
        assert!(submission
            .submit(VoiceCommand::Authenticated {
                generation: 7,
                session_id: SessionId(4),
            })
            .is_ok());

        let mut events = Events::with_capacity(2);
        poll.poll(&mut events, Some(Duration::ZERO)).unwrap();
        assert!(events.is_empty());
        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        assert!(!submission.drain_into(
            &mut controls,
            &mut microphone,
            &mut feedback,
            &mut sink,
        ));
        assert!(controls.is_empty());

        assert!(submission.activate(7).is_ok());
        poll.poll(&mut events, Some(Duration::from_secs(1))).unwrap();
        assert!(events.iter().any(|event| event.token() == COMMANDS));
        assert!(!submission.drain_into(
            &mut controls,
            &mut microphone,
            &mut feedback,
            &mut sink,
        ));
        assert!(matches!(
            controls.pop_front(),
            Some(VoiceCommand::StartSession { generation: 7, .. })
        ));
        assert!(matches!(
            controls.pop_front(),
            Some(VoiceCommand::Authenticated { generation: 7, .. })
        ));
        assert!(controls.is_empty());
    }

    #[test]
    fn p2p_disable_is_retained_before_local_voice_starts() {
        let (_poll, submission) = submission();
        assert!(submission
            .submit(start_command_with_p2p(7, true))
            .is_ok());
        assert!(submission
            .submit(VoiceCommand::SetP2pEnabled {
                generation: 7,
                enabled: false,
            })
            .is_ok());

        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        assert!(!submission.drain_into(
            &mut controls,
            &mut microphone,
            &mut feedback,
            &mut sink,
        ));
        assert!(controls.is_empty());
        assert!(submission.activate(7).is_ok());
        assert!(!submission.drain_into(
            &mut controls,
            &mut microphone,
            &mut feedback,
            &mut sink,
        ));
        assert!(matches!(
            controls.pop_front(),
            Some(VoiceCommand::StartSession {
                generation: 7,
                p2p_enabled: true,
                ..
            })
        ));
        assert!(matches!(
            controls.pop_front(),
            Some(VoiceCommand::SetP2pEnabled {
                generation: 7,
                enabled: false,
            })
        ));
    }

    #[test]
    fn control_mailbox_is_bounded_but_shutdown_is_always_admitted() {
        let (_poll, submission) = submission();
        {
            let mut mailbox = submission.mailbox.lock().unwrap();
            mailbox.ingress_generation = Some(1);
            mailbox.activated_generation = Some(1);
        }
        for _ in 0..MAX_QUEUED_CONTROL_COMMANDS {
            assert!(submission
                .submit(VoiceCommand::Authenticated {
                    generation: 1,
                    session_id: SessionId(2),
                })
                .is_ok());
        }
        assert!(submission
            .submit(VoiceCommand::Authenticated {
                generation: 1,
                session_id: SessionId(2),
            })
            .is_err());
        assert!(submission.submit(VoiceCommand::Shutdown).is_ok());
        let mailbox = submission.mailbox.lock().unwrap();
        assert_eq!(mailbox.controls.len(), 1);
        assert!(matches!(mailbox.controls.front(), Some(VoiceCommand::Shutdown)));
    }

    #[test]
    fn stopped_output_wakes_and_is_observable_without_a_fatal_payload() {
        let mut poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(9)).unwrap());
        let output = VoiceOutputSubmission::new(waker);
        output.stop();
        let mut events = Events::with_capacity(2);
        poll.poll(&mut events, Some(Duration::from_secs(1))).unwrap();
        assert!(events.iter().any(|event| event.token() == Token(9)));
        assert!(output.drain_into(&mut VoiceOutputBatch::default()));
    }

    #[test]
    fn feedback_and_sink_use_latest_state_and_clear_at_lifecycle_boundary() {
        let (_poll, submission) = submission();
        {
            let mut mailbox = submission.mailbox.lock().unwrap();
            mailbox.ingress_generation = Some(1);
            mailbox.activated_generation = Some(1);
        }
        let old = LivePlaybackFeedback {
            stream_id: 4,
            highest_contiguous_sequence: 1,
            ..LivePlaybackFeedback::default()
        };
        let newest = LivePlaybackFeedback {
            highest_contiguous_sequence: 9,
            ..old
        };
        assert!(submission.submit_feedback(old).is_ok());
        assert!(submission.submit_feedback(newest).is_ok());
        assert!(submission
            .submit_playback_sink(Some(LivePlaybackSink::for_test()))
            .is_ok());
        assert!(submission.submit_playback_sink(None).is_ok());

        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        assert!(!submission.drain_into(
            &mut controls,
            &mut microphone,
            &mut feedback,
            &mut sink,
        ));
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback[0].feedback.highest_contiguous_sequence, 9);
        assert!(matches!(sink, Some(None)));
        sink = None;

        assert!(submission
            .submit(VoiceCommand::EndSession { generation: 1 })
            .is_ok());
        assert!(submission.submit_feedback(old).is_ok());
        feedback.clear();
        assert!(!submission.drain_into(
            &mut controls,
            &mut microphone,
            &mut feedback,
            &mut sink,
        ));
        assert!(matches!(controls.pop_front(), Some(VoiceCommand::EndSession { generation: 1 })));
        assert!(feedback.is_empty());
    }

    #[test]
    fn wrong_server_source_does_not_consume_replay_or_dispatch_packet() {
        let actor_poll = Poll::new().unwrap();
        let server = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let wrong = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let udp = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        udp.set_nonblocking(true).unwrap();
        let actor_addr = udp.local_addr().unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let events = NetworkEventSender::for_test(event_tx);
        let mut session = VoiceSession::new(
            &actor_poll,
            1,
            udp,
            protection(55),
            false,
            TransportMode::NativeEncrypted,
            server.local_addr().unwrap(),
            None,
            false,
            CandidatePrivacy::Disabled,
            false,
            events,
        )
        .unwrap();
        assert!(!stream_owner_matches(
            &session.inbound_streams,
            StreamId(8),
            SessionId(6)
        ));
        let voice = MediaPayload::Voice {
            stream_id: StreamId(8),
            sequence: 3,
            timestamp: 2880,
            flags: 0,
            payload: media::VoicePayload::Opus(vec![1, 2, 3]),
        };
        let packet = media::seal_media(&protection(55), 1, &voice).unwrap();
        wrong.send_to(&packet, actor_addr).unwrap();
        assert!(!session.read_udp());
        assert!(session.pending_playback_packets.is_empty());

        let before = Instant::now();
        server.send_to(&packet, actor_addr).unwrap();
        assert!(!session.read_udp());
        let after = Instant::now();
        let received = session.pending_playback_packets.front().unwrap();
        assert_eq!(received.stream_id, 8);
        assert!(received.received_at >= before && received.received_at <= after);

        session.voice_room = Some(RoomId(2));
        session.voice_started(
            RoomId(2),
            SessionId(6),
            UserId(7),
            StreamId(8),
            false,
        );
        assert!(stream_owner_matches(
            &session.inbound_streams,
            StreamId(8),
            SessionId(6)
        ));
    }

    #[test]
    fn relay_packet_preceding_voice_started_keeps_dedup_state() {
        let actor_poll = Poll::new().unwrap();
        let server = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        let udp = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut session = VoiceSession::new(
            &actor_poll,
            1,
            udp,
            protection(55),
            false,
            TransportMode::NativeEncrypted,
            server.local_addr().unwrap(),
            None,
            false,
            CandidatePrivacy::Disabled,
            false,
            NetworkEventSender::for_test(event_tx),
        )
        .unwrap();
        let packet = crate::audio::RemoteVoicePacket {
            stream_id: 8,
            sequence: 3,
            timestamp: 2880,
            flags: 0,
            payload: VoicePayload::Opus(vec![1, 2, 3]),
            received_at: Instant::now(),
        };

        session.dispatch_voice_packet(packet.clone(), "server");
        assert_eq!(session.pending_playback_packets.len(), 1);
        assert!(!stream_owner_matches(
            &session.inbound_streams,
            StreamId(8),
            SessionId(6)
        ));

        session.voice_room = Some(RoomId(2));
        session.voice_started(
            RoomId(2),
            SessionId(6),
            UserId(7),
            StreamId(8),
            false,
        );
        session.dispatch_voice_packet(packet, "p2p");
        assert_eq!(session.pending_playback_packets.len(), 1);
    }

    #[test]
    fn deafened_playback_does_not_queue_stale_audio_for_resume() {
        let (mut actor, _main_poll) = direct_loop();
        assert!(actor.commands.submit(start_command(1)).is_ok());
        actor.commands.activate(1).unwrap();
        actor.drain_commands();

        // Deafen detaches the playback sink. A continuous 20 ms stream must not
        // accumulate behind it: feeding those packets to the replacement NetEQ
        // on undeafen turns the intentional pause into playout delay.
        actor.commands.submit_playback_sink(None).unwrap();
        actor.drain_commands();
        {
            let session = actor.session.as_mut().unwrap();
            for sequence in 0..75 {
                session.dispatch_voice_packet(
                    crate::audio::RemoteVoicePacket {
                        stream_id: 8,
                        sequence,
                        timestamp: sequence * 960,
                        flags: 0,
                        payload: VoicePayload::Opus(vec![1, 2, 3]),
                        received_at: Instant::now(),
                    },
                    "server",
                );
            }

            assert!(
                session.pending_playback_packets.is_empty(),
                "deafened playback retained {} ms of stale audio",
                session.pending_playback_packets.len() * 20
            );
        }

        actor
            .commands
            .submit_playback_sink(Some(LivePlaybackSink::for_test()))
            .unwrap();
        actor.drain_commands();
        let session = actor.session.as_ref().unwrap();
        assert!(session.playback_ingress.sink().is_some());
        assert!(session.pending_playback_packets.is_empty());
    }

    #[test]
    fn playback_feedback_without_stream_owner_uses_server_relay() {
        let actor_poll = Poll::new().unwrap();
        let server = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let udp = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let (event_tx, event_rx) = mpsc::channel();
        let mut session = VoiceSession::new(
            &actor_poll,
            1,
            udp,
            protection(55),
            false,
            TransportMode::NativeEncrypted,
            server.local_addr().unwrap(),
            None,
            false,
            CandidatePrivacy::Disabled,
            false,
            NetworkEventSender::for_test(event_tx),
        )
        .unwrap();
        let feedback = LivePlaybackFeedback {
            stream_id: 8,
            highest_contiguous_sequence: 3,
            ..LivePlaybackFeedback::default()
        };

        session.send_playback_feedback(feedback);

        assert!(matches!(
            event_rx.recv().unwrap(),
            crate::app::AppEvent::Network(NetworkEvent::PlaybackFeedback(received))
                if received.stream_id == 8
        ));
        let mut datagram = [0u8; 2048];
        let (len, _) = server.recv_from(&mut datagram).unwrap();
        let opened = media::open_media(
            &protection(55),
            &mut AntiReplay::new(),
            &datagram[..len],
        )
        .unwrap();
        assert!(matches!(
            opened.payload,
            MediaPayload::VoiceFeedback { stream_id, .. } if stream_id == StreamId(8)
        ));
    }

    #[test]
    fn voice_packet_deduplicator_bounds_stream_table() {
        let mut dedup = VoicePacketDeduplicator::new();

        for stream_id in 0..(MAX_RECENT_VOICE_STREAMS as u32 + 8) {
            assert_eq!(dedup.observe(stream_id, 0), RecentVoiceSequenceResult::New);
        }

        assert_eq!(dedup.len(), MAX_RECENT_VOICE_STREAMS);
    }

    #[test]
    fn generation_lifecycle_resets_packet_state_and_ignores_stale_commands() {
        let (mut actor, _main_poll) = direct_loop();
        actor.playback_ingress =
            PlaybackIngressState::Attached(LivePlaybackSink::for_test());
        actor.apply_command(start_command(4));
        let session = actor.session.as_mut().unwrap();
        assert!(session.playback_ingress.sink().is_some());
        session.media_send_counter = 99;
        session.local_sequence = 77;
        session.voice_room = Some(RoomId(8));
        session.active_stream = Some(StreamId(12));
        session.pending_playback_packets.push_back(crate::audio::RemoteVoicePacket {
            stream_id: 12,
            sequence: 1,
            timestamp: 0,
            flags: 0,
            payload: VoicePayload::Opus(vec![1]),
            received_at: Instant::now(),
        });

        actor.apply_command(VoiceCommand::EndSession { generation: 3 });
        assert!(actor.session.is_some());
        actor.apply_command(VoiceCommand::EndSession { generation: 4 });
        assert!(actor.session.is_none());

        actor.apply_command(start_command(5));
        let session = actor.session.as_ref().unwrap();
        assert_eq!(session.media_send_counter, 0);
        assert_eq!(session.local_sequence, 0);
        assert!(session.voice_room.is_none());
        assert!(session.active_stream.is_none());
        assert!(session.pending_playback_packets.is_empty());
        assert!(session.p2p_peers.is_empty());
        assert!(session.server_rtt_in_flight.is_empty());
        assert!(session.playback_ingress.sink().is_some());
    }

    #[test]
    fn dropping_handle_joins_blocked_loop_and_closes_direct_sender() {
        let main_poll = Poll::new().unwrap();
        let main_waker = Arc::new(Waker::new(main_poll.registry(), Token(9)).unwrap());
        let (event_tx, _event_rx) = mpsc::channel();
        let handle = VoiceLoopHandle::spawn(
            NetworkEventSender::for_test(event_tx),
            main_waker,
        )
        .unwrap();
        let control = handle.control();
        assert!(control.submit(start_command(1)).is_ok());
        assert!(control.activate(1).is_ok());
        let input = handle.input_sender();
        drop(handle);
        let command = NetworkCommand::LocalVoicePacket(frame(1));
        assert!(matches!(input.send(command), Err(SendError(NetworkCommand::LocalVoicePacket(_)))));
    }

    #[test]
    fn initial_bind_dispatch_does_not_start_voice_loop() {
        let main_poll = Poll::new().unwrap();
        let main_waker = Arc::new(Waker::new(main_poll.registry(), Token(9)).unwrap());
        let (event_tx, _event_rx) = mpsc::channel();
        let handle = VoiceLoopHandle::spawn(
            NetworkEventSender::for_test(event_tx),
            main_waker,
        )
        .unwrap();
        let server = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let udp = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let bind = InitialUdpBind::prepare(
            &udp,
            &protection(78),
            server.local_addr().unwrap(),
        )
        .unwrap();

        bind.dispatch().unwrap();
        let mut empty = [0u8; 1];
        assert_eq!(
            udp.recv_from(&mut empty).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        let mut datagram = [0u8; 2048];
        let (len, _) = server.recv_from(&mut datagram).unwrap();
        let opened = media::open_media(
            &protection(78),
            &mut AntiReplay::new(),
            &datagram[..len],
        )
        .unwrap();
        assert_eq!(opened.header.counter, 0);
        assert_eq!(opened.payload, MediaPayload::Bind);
        assert!(handle.runtime.thread.lock().unwrap().is_none());
    }

    #[test]
    fn dedicated_thread_binds_and_sends_voice_while_main_side_does_not_drain() {
        let main_poll = Poll::new().unwrap();
        let main_waker = Arc::new(Waker::new(main_poll.registry(), Token(9)).unwrap());
        let (event_tx, _event_rx) = mpsc::channel();
        let handle = VoiceLoopHandle::spawn(
            NetworkEventSender::for_test(event_tx),
            main_waker,
        )
        .unwrap();
        assert!(handle.runtime.thread.lock().unwrap().is_none());
        let control = handle.control();
        let input = handle.input_sender();
        let server = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let udp = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        udp.set_nonblocking(true).unwrap();
        assert!(control
            .submit(VoiceCommand::StartSession {
                generation: 7,
                udp,
                media: protection(77),
                initial_bind_attempted: false,
                transport_mode: TransportMode::NativeEncrypted,
                server_udp_addr: server.local_addr().unwrap(),
                server_udp_probe_addr: None,
                p2p_enabled: true,
                candidate_privacy: CandidatePrivacy::Disabled,
                prefer_ipv6: false,
            })
            .is_ok());
        assert!(handle.runtime.thread.lock().unwrap().is_none());
        assert!(control
            .submit(VoiceCommand::Authenticated {
                generation: 7,
                session_id: SessionId(4),
            })
            .is_ok());
        assert!(control.activate(7).is_ok());
        assert!(handle.runtime.thread.lock().unwrap().is_some());

        let mut replay = AntiReplay::new();
        let mut datagram = [0u8; 2048];
        let (len, _) = server.recv_from(&mut datagram).unwrap();
        let opened = media::open_media(&protection(77), &mut replay, &datagram[..len]).unwrap();
        assert_eq!(opened.header.counter, 0);
        assert_eq!(opened.payload, MediaPayload::Bind);

        assert!(control
            .submit(VoiceCommand::VoiceStarted {
                generation: 7,
                room_id: RoomId(2),
                session_id: SessionId(4),
                user_id: UserId(3),
                stream_id: StreamId(9),
                local: true,
            })
            .is_ok());
        assert!(handle.runtime.thread.lock().unwrap().is_some());
        input
            .send(NetworkCommand::SequencedLocalVoicePacket {
                sequence: 12,
                frame: frame(12),
            })
            .unwrap();

        let mut received_voice = false;
        for _ in 0..3 {
            let (len, _) = server.recv_from(&mut datagram).unwrap();
            let opened = media::open_media(&protection(77), &mut replay, &datagram[..len]).unwrap();
            if let MediaPayload::Voice { stream_id, sequence, .. } = opened.payload {
                assert_eq!(stream_id, StreamId(9));
                assert_eq!(sequence, 12);
                received_voice = true;
                break;
            }
        }
        assert!(received_voice);
        drop(handle);
    }
}
