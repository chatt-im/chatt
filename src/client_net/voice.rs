//! Dedicated client UDP media event loop.
//!
//! The control worker submits ordered session and topology changes here. Audio
//! producers use the same queue directly for the four packet-plane commands,
//! so neither outbound voice nor playback feedback waits behind TCP, MLS, or
//! file work. The queue lock is held only while updating the pending deque;
//! the voice thread swaps it into a reusable buffer before doing any socket or
//! cryptographic work.

use aws_lc_rs::rand::SecureRandom;
use chatt_p2p::{
    Action as P2pAction, AgentConfig as P2pAgentConfig, Candidate, CandidateKind,
    ReflexiveObservation, StunAuth, TraversalAgent, interfaces::InterfaceSnapshot,
    stun::StunMessage,
};
use hashbrown::{HashMap, HashSet};
use mio::{
    Events, Interest, Poll, Registry, Token, Waker,
    net::{TcpStream, UdpSocket},
};
use rpc::{
    control::{P2pCandidate, P2pNatKind, P2pPeerInfo, ParticipantServerRtt},
    crypto::{AntiReplay, KeyMaterial, TransportMode},
    evented::{ReadLimit, Readiness, WriteQueue, read_into_buffer, write_queue_to},
    frame,
    ids::{RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayload, MediaProtection},
    recv::RecvBuffer,
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
    config::{CandidatePrivacy, MediaTransportSetting},
    mdns::MdnsSystem,
};

use super::{
    DIRECT_CONFIRM_WINDOW, DIRECT_FAILOVER_IDLE, ENCODER_FEEDBACK_ALPHA, ENCODER_PROFILE_HOLD,
    INTERFACE_POLL_INTERVAL, MAX_RECENT_VOICE_SEQUENCES, MediaTransportState, NetworkCommand,
    NetworkEvent, P2P_CONSENT_TIMEOUT, P2P_KEEPALIVE_INTERVAL, RECENT_VOICE_SEQUENCE_WORD_BITS,
    RECENT_VOICE_SEQUENCE_WORDS, RELAY_KEEPALIVE_INTERVAL, RTT_PROBE_INTERVAL, RestartPortPolicy,
    UDP_BIND_FAILURE_ATTEMPTS, UDP_BIND_RETRY_INTERVAL, advance_local_voice_sequence_past,
    allocate_local_voice_sequence, apply_candidate_privacy, audio_payload_from_media,
    candidate_from_control, candidate_from_control_with_addr, clamp_rtt_ms, combined_relay_rtt,
    configured_nat_kind, connection_id_from_p2p_username, control_nat_kind,
    dispatch_voice_packet_to, fold_rtt_ewma, ice_role_from_control, key_from_control,
    live_feedback_from_media, log_audio_pop_media_packet, media_feedback_from_live,
    media_payload_from_audio, media_payload_kind, media_voice_payload_kind, nat_from_control,
    p2p_peer_is_republish, p2p_username, push_rtt_in_flight, random_u64, rtt_sample_is_stale,
    split_mdns_addr, take_rtt_sample, voice_payload_kind,
};

const COMMANDS: Token = Token(0);
const UDP: Token = Token(1);
const MDNS_V4: Token = Token(2);
const MDNS_V6: Token = Token(3);
const VOICE_TCP: Token = Token(4);
const IDLE_POLL_TIMEOUT: Duration = Duration::from_secs(60);
const TCP_LANE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TCP_LANE_BACKOFF_MIN: Duration = Duration::from_millis(500);
const TCP_LANE_BACKOFF_MAX: Duration = Duration::from_secs(8);
/// UDP recovery attempts made while a confirmed TCP lane safely carries
/// voice. A path that stays unavailable through the final attempt is parked
/// until the session or network interface restarts it.
const UDP_OVER_TCP_RECOVERY_DELAYS: [Duration; 7] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
    Duration::from_secs(30),
];
const UDP_RECOVERY_JITTER_PERMILLE: i64 = 100;
/// After UDP recovers the lane half-closes and keeps reading briefly so
/// downstream frames already in flight still play before the socket drops.
const TCP_LANE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const TCP_LANE_READ_BUDGET_BYTES: usize = 16 * 1024;
const TCP_LANE_WRITE_ATTEMPTS: usize = 32;
/// A lane is closed instead of queued beyond this backlog; preserving bytes at
/// this depth would replay stale speech after the path becomes writable.
const TCP_LANE_WRITE_CAP_BYTES: usize = 32 * 1024;
/// A lane is closed once its oldest unsent byte is this old. The byte cap alone
/// is not a latency bound — at ordinary voice bitrates it is seconds of speech —
/// and audio this stale is past any jitter buffer's reach.
const TCP_LANE_STALE_BACKLOG: Duration = Duration::from_millis(250);
const TCP_LANE_MAX_WIRE_FRAME_BYTES: usize =
    frame::LENGTH_PREFIX_LEN + media::VOICE_TCP_MAX_FRAME_BYTES;
const _: () = assert!(TCP_LANE_WRITE_CAP_BYTES > TCP_LANE_MAX_WIRE_FRAME_BYTES);
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
    let socket =
        chatt_p2p::socket::bind_udp_socket(addr, chatt_p2p::socket::UdpSocketOptions::default())?;
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

/// Which transport a server media packet arrived on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MediaPath {
    Udp,
    Tcp,
}

impl MediaPath {
    fn label(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }
}

/// What is known about the relay's UDP path.
///
/// Only a `Pong` answering a nonce this client sent over UDP proves it: inbound
/// speech travels the opposite direction and says nothing about whether the
/// microphone is reaching the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UdpPath {
    /// Nothing proven yet. Media still rides it until a lane is up — nothing
    /// better exists — but the lane is wanted once the cold-start grace passes.
    Unproven,
    /// Recent matched round trips: UDP carries media and no lane is wanted.
    Verified,
    /// Probes or binds went unanswered. The UI reports this as unavailable
    /// only when no confirmed TCP lane carries media instead.
    Failed,
}

/// Ping/Pong liveness for one server transport.
///
/// Nonces are tracked per path so a `Pong` only proves the path it came back
/// on, and so a UDP recovery probe answered while the lane carries media never
/// folds into the lane's RTT.
#[derive(Default)]
struct PathProbes {
    /// Outstanding nonces with their send time, bounded by [`RTT_IN_FLIGHT_CAP`].
    in_flight: VecDeque<(u64, Instant)>,
    /// When the oldest probe with nothing matched since was sent. `None` while
    /// the path is proven live; reaching [`RTT_STALE_AFTER`] declares it dead.
    /// Unlike an RTT sample this starts at authentication, so a path that never
    /// answers at all is still detected.
    pending_since: Option<Instant>,
    /// Consecutive matched round trips since the last miss, the recovery score.
    streak: u8,
}

impl PathProbes {
    /// Records an outbound probe, opening the liveness window if the path has
    /// nothing outstanding.
    fn sent(&mut self, nonce: u64, now: Instant) {
        push_rtt_in_flight(&mut self.in_flight, nonce, now);
        if self.pending_since.is_none() {
            self.pending_since = Some(now);
        }
    }

    /// Matches an inbound `Pong`, returning its round-trip time in
    /// milliseconds. An unknown nonce is not proof of anything and leaves the
    /// window running.
    fn matched(&mut self, nonce: u64, now: Instant) -> Option<f32> {
        let sample = take_rtt_sample(&mut self.in_flight, nonce, now)?;
        self.pending_since = None;
        self.streak = self.streak.saturating_add(1);
        Some(sample)
    }

    /// Drops probes that have gone unanswered for `timeout` and resets the
    /// streak when any is found, so one missed round trip restarts the dwell.
    fn expire(&mut self, now: Instant, timeout: Duration) {
        let mut missed = false;
        while let Some((_, sent)) = self.in_flight.front() {
            if now.saturating_duration_since(*sent) < timeout {
                break;
            }
            self.in_flight.pop_front();
            missed = true;
        }
        if missed {
            self.streak = 0;
        }
    }

    /// Restarts the window for a path that has been neither proven nor
    /// disproven, used when a transport takes over carrying media.
    fn restart(&mut self, now: Instant) {
        self.in_flight.clear();
        self.pending_since = Some(now);
        self.streak = 0;
    }
}

/// Bounded UDP recovery while confirmed TCP keeps media available.
///
/// Jitter is deliberately local and non-cryptographic: its only job is to keep
/// clients that observed the same server-side UDP outage from probing in
/// lockstep.
struct UdpOverTcpRecovery {
    next_attempt: Option<Instant>,
    next_delay: usize,
    jitter_state: u64,
}

impl UdpOverTcpRecovery {
    fn new(jitter_seed: u64) -> Self {
        Self {
            next_attempt: None,
            next_delay: 0,
            jitter_state: jitter_seed.max(1),
        }
    }

    fn arm(&mut self, now: Instant) {
        self.next_delay = 0;
        self.schedule_next(now);
    }

    fn disarm(&mut self) {
        self.next_attempt = None;
        self.next_delay = 0;
    }

    fn next_attempt(&self) -> Option<Instant> {
        self.next_attempt
    }

    /// Consumes a due attempt and schedules its successor. `false` after the
    /// bounded schedule has been exhausted.
    fn take_due(&mut self, now: Instant) -> bool {
        if self.next_attempt.is_none_or(|deadline| now < deadline) {
            return false;
        }
        self.schedule_next(now);
        true
    }

    fn schedule_next(&mut self, now: Instant) {
        let Some(delay) = UDP_OVER_TCP_RECOVERY_DELAYS.get(self.next_delay).copied() else {
            self.next_attempt = None;
            return;
        };
        self.next_delay += 1;
        self.next_attempt = Some(now + self.jitter(delay));
    }

    fn jitter(&mut self, delay: Duration) -> Duration {
        let mut state = self.jitter_state;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.jitter_state = state;
        let span = UDP_RECOVERY_JITTER_PERMILLE * 2 + 1;
        let offset = (state % span as u64) as i64 - UDP_RECOVERY_JITTER_PERMILLE;
        let millis = delay.as_millis() as i64;
        Duration::from_millis((millis + millis * offset / 1_000).max(1) as u64)
    }
}

enum TcpLaneState {
    Connecting {
        deadline: Instant,
    },
    Active,
    /// Half-closed after UDP recovered: writes are shut down, reads continue
    /// until the server's EOF or the deadline.
    Draining {
        deadline: Instant,
    },
}

struct TcpLane {
    stream: TcpStream,
    state: TcpLaneState,
    /// The server has opened and answered on this lane. A completed TCP
    /// handshake alone does not prove that the relay accepted the magic and
    /// authenticated Bind.
    confirmed: bool,
    read_buf: RecvBuffer,
    readiness: Readiness,
    write_queue: WriteQueue,
    write_blocked: bool,
    /// When the write queue last went from empty to non-empty, `None` while it
    /// is empty. A healthy lane drains on every flush, so a queue that stays
    /// occupied bounds how stale its oldest unsent byte can be — the latency
    /// limit [`TCP_LANE_WRITE_CAP_BYTES`] cannot express.
    backlog_since: Option<Instant>,
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

/// The UDP media leg handed to a starting session: the socket plus the server
/// addresses it talks to. A forced-TCP session has none, and then opens no UDP
/// socket at all.
pub(super) struct UdpMediaSetup {
    pub(super) socket: StdUdpSocket,
    pub(super) server_addr: SocketAddr,
    pub(super) server_probe_addr: Option<SocketAddr>,
}

pub(super) enum VoiceCommand {
    StartSession {
        generation: u64,
        udp: Option<UdpMediaSetup>,
        media: MediaProtection,
        initial_bind_attempted: bool,
        transport_mode: TransportMode,
        /// The control connection's resolved peer address; the TCP voice lane
        /// dials exactly the address that already worked for control.
        server_tcp_addr: SocketAddr,
        media_transport: MediaTransportSetting,
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

    // Rejection returns the unsent command so the caller retains ownership.
    #[allow(clippy::result_large_err)]
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
                    matches!(
                        command,
                        VoiceCommand::EndSession { .. } | VoiceCommand::Shutdown
                    )
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

    fn submit_playback_sink(
        &self,
        sink: Option<LivePlaybackSink>,
    ) -> Result<(), Option<LivePlaybackSink>> {
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
        if self.closed.load(Ordering::Relaxed) || mailbox.ingress_generation != Some(generation) {
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
            NetworkCommand::SequencedLocalVoicePacket { sequence, frame } => self
                .submission
                .submit_microphone(Some(sequence), frame)
                .map_err(|frame| {
                    SendError(NetworkCommand::SequencedLocalVoicePacket { sequence, frame })
                }),
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
        self.submission
            .drain_into(&mut controls, &mut microphone, &mut feedback, &mut sink);
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
        self.publish_p2p.is_none() && self.session_failure.is_none() && self.fatal_failure.is_none()
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

    // Preserve the mailbox API's recoverable unsent command.
    #[allow(clippy::result_large_err)]
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
    tcp_read_work: bool,
    tcp_writable: bool,
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
            tcp_read_work: false,
            tcp_writable: false,
            command_work: false,
            shutting_down: false,
        }
    }

    fn run(&mut self) -> io::Result<()> {
        while !self.shutting_down {
            self.drain_commands();
            if self.udp_work {
                self.udp_work = self.session.as_mut().is_some_and(VoiceSession::read_udp);
            }
            if self.tcp_writable {
                self.tcp_writable = false;
                if let Some(session) = self.session.as_mut() {
                    session.tcp_lane_writable();
                }
            }
            if self.tcp_read_work {
                self.tcp_read_work = self
                    .session
                    .as_mut()
                    .is_some_and(VoiceSession::read_tcp_lane);
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
            let timeout = if self.command_work
                || self.udp_work
                || self.mdns_work != 0
                || self.tcp_read_work
                || self.tcp_writable
            {
                Duration::ZERO
            } else {
                self.session.as_ref().map_or(IDLE_POLL_TIMEOUT, |session| {
                    session.next_poll_timeout(Instant::now())
                })
            };
            match self.poll.poll(&mut self.poll_events, Some(timeout)) {
                Ok(()) => {}
                Err(error) if rpc::evented::is_interrupted_io_error(&error) => continue,
                Err(error) => return Err(error),
            }
            for event in self.poll_events.iter() {
                let ready = rpc::evented::MioReady::from_event(event);
                match event.token() {
                    COMMANDS => {}
                    UDP if ready.readable_like() => self.udp_work = true,
                    MDNS_V4 if ready.readable_like() => self.mdns_work |= 1,
                    MDNS_V6 if ready.readable_like() => self.mdns_work |= 2,
                    VOICE_TCP => {
                        if ready.readable_like() {
                            self.tcp_read_work = true;
                            if let Some(lane) = self
                                .session
                                .as_mut()
                                .and_then(|session| session.tcp_lane.as_mut())
                            {
                                lane.readiness.mark_ready();
                            }
                        }
                        if ready.writable_like() {
                            self.tcp_writable = true;
                        }
                    }
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
                server_tcp_addr,
                media_transport,
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
                    server_tcp_addr,
                    media_transport,
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
            VoiceCommand::RoomRttSnapshot {
                room_id, members, ..
            } => {
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
            VoiceCommand::RemovePeer {
                session_id,
                user_id,
                ..
            } => {
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
        self.tcp_read_work = false;
        self.tcp_writable = false;
    }
}

/// The session's UDP media leg. Absent in forced-TCP mode, where the client
/// opens no UDP socket at all: every relay-UDP and P2P path is gated on it.
struct UdpMedia {
    socket: UdpSocket,
    local_addr: SocketAddr,
    server_addr: SocketAddr,
    server_probe_addr: Option<SocketAddr>,
}

struct VoiceSession {
    generation: u64,
    events: NetworkEventSender,
    registry: Registry,
    udp: Option<UdpMedia>,
    server_tcp_addr: SocketAddr,
    media_transport: MediaTransportSetting,
    tcp_lane: Option<TcpLane>,
    /// Whether UDP has proven itself with matched round trips. Anything but
    /// [`UdpPath::Verified`] keeps the TCP voice lane up in `Auto` mode, so a
    /// session that has never proven UDP is treated like one that lost it.
    udp_path: UdpPath,
    next_tcp_connect: Instant,
    tcp_backoff: Duration,
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
    /// Next fast UDP verification probe while no confirmed fallback carries
    /// media. Confirmed TCP uses [`UdpOverTcpRecovery`] instead.
    next_udp_verify: Instant,
    udp_over_tcp_recovery: UdpOverTcpRecovery,
    rtt_probe_seq: u64,
    /// Per-transport probe liveness. A `Pong` proves only the path it returned
    /// on, so a session whose upstream UDP is blackholed is detected even while
    /// a room's inbound speech keeps arriving over UDP.
    udp_probes: PathProbes,
    tcp_probes: PathProbes,
    server_rtt_ms: Option<f32>,
    server_rtt_last_sample_at: Option<Instant>,
    playback_ingress: PlaybackIngressState,
    pending_playback_packets: VecDeque<crate::audio::RemoteVoicePacket>,
    encoder_feedback: EncoderFeedbackController,
    restart_port_policy: RestartPortPolicy,
    udp_rebind_requested: bool,
    awaiting_udp_bound: bool,
    udp_bind_attempts: u32,
    reported_media_transport: MediaTransportState,
    interface_monitor: InterfaceMonitor,
    pending_publish: Option<PublishP2pRequest>,
}

impl VoiceSession {
    #[allow(clippy::too_many_arguments)]
    fn new(
        poll: &Poll,
        generation: u64,
        udp: Option<UdpMediaSetup>,
        media: MediaProtection,
        initial_bind_attempted: bool,
        transport_mode: TransportMode,
        server_tcp_addr: SocketAddr,
        media_transport: MediaTransportSetting,
        p2p_enabled: bool,
        candidate_privacy: CandidatePrivacy,
        prefer_ipv6: bool,
        events: NetworkEventSender,
    ) -> Result<Self, String> {
        let udp = match udp {
            Some(setup) => {
                setup.socket.set_nonblocking(true).map_err(|error| {
                    format!("failed to make voice UDP socket nonblocking: {error}")
                })?;
                let local_addr = setup
                    .socket
                    .local_addr()
                    .map_err(|error| format!("failed to read UDP socket address: {error}"))?;
                let mut socket = UdpSocket::from_std(setup.socket);
                poll.registry()
                    .register(&mut socket, UDP, Interest::READABLE)
                    .map_err(|error| format!("failed to register UDP socket: {error}"))?;
                Some(UdpMedia {
                    socket,
                    local_addr,
                    server_addr: setup.server_addr,
                    server_probe_addr: setup.server_probe_addr,
                })
            }
            None => None,
        };
        // Direct P2P is UDP-only, so a session without the leg relays through
        // the server lane no matter what the config asks for.
        if p2p_enabled && udp.is_none() {
            let _ = events.send(NetworkEvent::Status(
                "P2P unavailable while media transport is forced to TCP".to_string(),
            ));
        }
        let p2p_enabled = p2p_enabled && udp.is_some();
        let registry = poll
            .registry()
            .try_clone()
            .map_err(|error| format!("failed to clone voice poll registry: {error}"))?;
        let mut mdns = if p2p_enabled {
            MdnsSystem::bind()
        } else {
            MdnsSystem::unbound()
        };
        if let Err(error) = mdns.register(poll.registry(), MDNS_V4, MDNS_V6) {
            kvlog::warn!("failed to register voice mdns sockets", error = %error);
        }
        let now = Instant::now();
        let reported_media_transport = if media_transport.is_auto() {
            MediaTransportState::Udp
        } else {
            let _ = events.send(NetworkEvent::MediaTransport {
                state: MediaTransportState::Unavailable,
            });
            MediaTransportState::Unavailable
        };
        Ok(Self {
            generation,
            events,
            registry,
            udp,
            server_tcp_addr,
            media_transport,
            tcp_lane: None,
            udp_path: UdpPath::Unproven,
            next_tcp_connect: now,
            tcp_backoff: TCP_LANE_BACKOFF_MIN,
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
            next_udp_verify: now,
            udp_over_tcp_recovery: UdpOverTcpRecovery::new(random_u64().unwrap_or(1)),
            rtt_probe_seq: 0,
            udp_probes: PathProbes::default(),
            tcp_probes: PathProbes::default(),
            server_rtt_ms: None,
            server_rtt_last_sample_at: None,
            playback_ingress: PlaybackIngressState::default(),
            pending_playback_packets: VecDeque::new(),
            encoder_feedback: EncoderFeedbackController::new(),
            restart_port_policy: RestartPortPolicy::default(),
            udp_rebind_requested: false,
            awaiting_udp_bound: false,
            udp_bind_attempts: 0,
            reported_media_transport,
            interface_monitor: InterfaceMonitor::new(now),
            pending_publish: None,
        })
    }

    fn shutdown(&mut self, poll: &Poll) {
        if let Some(udp) = self.udp.as_mut() {
            let _ = poll.registry().deregister(&mut udp.socket);
        }
        if let Some(mut lane) = self.tcp_lane.take() {
            let _ = poll.registry().deregister(&mut lane.stream);
        }
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
        if self.tcp_lane_confirmed() {
            if let Some(next_attempt) = self.udp_over_tcp_recovery.next_attempt() {
                timeout = timeout.min(next_attempt.saturating_duration_since(now));
            }
        } else {
            if self.awaiting_udp_bound {
                timeout = timeout.min(self.next_udp_bind_retry.saturating_duration_since(now));
            }
            if self.udp_verify_wanted() {
                timeout = timeout.min(self.next_udp_verify.saturating_duration_since(now));
            }
        }
        match &self.tcp_lane {
            Some(lane) => {
                match lane.state {
                    TcpLaneState::Connecting { deadline } | TcpLaneState::Draining { deadline } => {
                        timeout = timeout.min(deadline.saturating_duration_since(now));
                    }
                    TcpLaneState::Active => {}
                }
                if !lane.write_blocked && !lane.write_queue.is_empty() {
                    return Duration::ZERO;
                }
            }
            None => {
                if self.lane_wanted() && self.session_id.is_some() {
                    timeout = timeout.min(self.next_tcp_connect.saturating_duration_since(now));
                }
            }
        }
        timeout = timeout.min(self.next_relay_keepalive.saturating_duration_since(now));
        timeout = timeout.min(self.next_rtt_probe.saturating_duration_since(now));
        if let Some(sample_at) = self.server_rtt_last_sample_at {
            timeout =
                timeout.min((sample_at + super::RTT_STALE_AFTER).saturating_duration_since(now));
        }
        if let Some(since) = self.carrying_probes().pending_since {
            timeout = timeout.min((since + super::RTT_STALE_AFTER).saturating_duration_since(now));
        }
        if self.interface_monitor_wanted() {
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
        if self.interface_monitor_wanted() {
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
        if self.tcp_lane_confirmed() {
            self.poll_udp_over_tcp_recovery(now);
        } else {
            self.poll_udp_bind_retry(now);
            self.poll_udp_verify(now);
        }
        self.poll_relay_keepalive(now);
        self.poll_rtt_probe(now);
        self.poll_server_path_liveness(now);
        self.poll_tcp_lane(now);
        if self.pending_publish.is_some() {
            output.publish_p2p = self.pending_publish.take();
        }
    }

    fn authenticated(&mut self, session_id: SessionId) {
        self.session_id = Some(session_id);
        let now = Instant::now();
        self.carrying_probes_mut().pending_since = Some(now);
        if !self.media_transport.is_auto() {
            self.next_tcp_connect = now;
            return;
        }
        if self.initial_bind_attempted {
            self.awaiting_udp_bound = true;
            self.udp_bind_attempts = 0;
            self.next_udp_bind_retry = now + UDP_BIND_RETRY_INTERVAL;
            if self.p2p_enabled {
                self.send_server_nat_probes();
            }
        } else {
            self.bind_udp();
        }
        self.begin_udp_verification(now);
    }

    /// Restarts UDP verification for a path that has proven nothing: at
    /// authentication, and after a rebind moves the session to a fresh port.
    ///
    /// A lane is wanted from here — UDP is unproven, and putting speech on an
    /// unproven path is what loses the opening seconds of a call — but a single
    /// round trip settles it, so the probe gets a grace period before a
    /// connection is spent on a path that is probably fine.
    fn begin_udp_verification(&mut self, now: Instant) {
        self.udp_path = UdpPath::Unproven;
        self.udp_probes.restart(now);
        self.next_udp_verify = now;
        self.next_tcp_connect = self.next_tcp_connect.max(now + super::UDP_COLD_START_GRACE);
        self.send_udp_verify_probe(now);
        if self.tcp_lane_confirmed() {
            self.udp_over_tcp_recovery.arm(now);
        } else {
            self.udp_over_tcp_recovery.disarm();
        }
    }

    fn send_server_nat_probes(&mut self) {
        let Some(udp) = self.udp.as_ref() else {
            return;
        };
        let server_addr = udp.server_addr;
        let probe_addr = udp.server_probe_addr;
        self.send_nat_probe(0, server_addr);
        if let Some(addr) = probe_addr {
            self.send_nat_probe(1, addr);
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

    /// The server confirming the address claim proves only that datagrams reach
    /// it, so this stops the bind retries but leaves the path unverified until
    /// [`VoiceSession::maybe_recover_udp`] also sees answered probes come back.
    fn udp_bound(&mut self) {
        if !self.awaiting_udp_bound {
            return;
        }
        self.awaiting_udp_bound = false;
        self.udp_bind_attempts = 0;
        kvlog::info!("client udp bound");
        self.maybe_recover_udp();
    }

    /// Promotes UDP to carrying media once its probes come back.
    ///
    /// A path that has merely never been tested needs one round trip; one that
    /// already failed needs [`UDP_VERIFY_SUCCESSES`] in a row. The asymmetry is
    /// the point: hysteresis exists to stop a lane flapping back onto a path
    /// that keeps dropping, and at cold start there is no lane to flap.
    fn maybe_recover_udp(&mut self) {
        let required = match self.udp_path {
            UdpPath::Verified => return,
            UdpPath::Unproven => 1,
            UdpPath::Failed => super::UDP_VERIFY_SUCCESSES,
        };
        if self.awaiting_udp_bound || self.udp_probes.streak < required {
            return;
        }
        kvlog::info!(
            "udp path verified",
            round_trips = u32::from(self.udp_probes.streak)
        );
        self.udp_path = UdpPath::Verified;
        self.udp_over_tcp_recovery.disarm();
        self.report_media_transport(MediaTransportState::Udp);
        if self.media_transport.is_auto() {
            self.drain_tcp_lane();
        }
    }

    fn nat_probe_observed(&mut self, probe_id: u8, addr: SocketAddr) {
        let Some(server_addr) = self
            .probe_addr_for_id(probe_id)
            .or_else(|| self.udp.as_ref().map(|udp| udp.server_addr))
        else {
            return;
        };
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
        if enabled && self.transport_mode != TransportMode::Encrypted {
            let _ = self.events.send(NetworkEvent::Status(
                "P2P unavailable without transport encryption".to_string(),
            ));
            return;
        }
        if enabled && self.udp.is_none() {
            let _ = self.events.send(NetworkEvent::Status(
                "P2P unavailable while media transport is forced to TCP".to_string(),
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
        let Some(session_id) = self.session_id else {
            return;
        };
        if self.udp.is_none() {
            return;
        }
        kvlog::info!("udp bind sending", session_id = session_id.0);
        self.awaiting_udp_bound = true;
        self.udp_bind_attempts = 0;
        self.next_udp_bind_retry = Instant::now() + UDP_BIND_RETRY_INTERVAL;
        self.send_media_udp(&MediaPayload::Bind);
        if self.p2p_enabled {
            self.send_server_nat_probes();
        }
    }

    /// Fast Bind retries used until TCP is confirmed. Once the lane carries
    /// media, [`VoiceSession::poll_udp_over_tcp_recovery`] sends Bind and Ping
    /// together on its bounded backoff.
    fn poll_udp_bind_retry(&mut self, now: Instant) {
        if !self.awaiting_udp_bound || now < self.next_udp_bind_retry {
            return;
        }
        self.next_udp_bind_retry = now + UDP_BIND_RETRY_INTERVAL;
        if self.session_id.is_some() {
            self.send_media_udp(&MediaPayload::Bind);
        }
        self.udp_bind_attempts = self.udp_bind_attempts.saturating_add(1);
        if self.udp_bind_attempts >= UDP_BIND_FAILURE_ATTEMPTS {
            self.latch_udp_suspect();
        }
    }

    /// Sends one bounded UDP recovery attempt while confirmed TCP carries
    /// media. Bind and Ping share this cadence so a permanently UDP-blocked
    /// network cannot leave either retry loop waking once a second forever.
    fn poll_udp_over_tcp_recovery(&mut self, now: Instant) {
        self.udp_probes.expire(now, super::UDP_VERIFY_TIMEOUT);
        if !self.udp_over_tcp_recovery.take_due(now) {
            return;
        }
        if self.awaiting_udp_bound {
            self.send_media_udp(&MediaPayload::Bind);
            self.udp_bind_attempts = self.udp_bind_attempts.saturating_add(1);
            if self.udp_bind_attempts >= UDP_BIND_FAILURE_ATTEMPTS {
                self.latch_udp_suspect();
            }
        }
        self.send_udp_verify_probe(now);
        if self.udp_over_tcp_recovery.next_attempt().is_none() {
            kvlog::info!("udp recovery parked while tcp voice lane is healthy");
        }
    }

    /// Demotes UDP after a real failure. Deliberately coarse: only exhausted
    /// bind retries or an elapsed liveness window land here, so one lost `Pong`
    /// on a working path cannot cost a lane.
    fn latch_udp_suspect(&mut self) {
        if self.udp_path != UdpPath::Failed {
            kvlog::info!("udp path failed, wanting tcp voice lane");
        }
        self.udp_path = UdpPath::Failed;
        self.udp_probes.streak = 0;
        if !self.tcp_lane_confirmed() {
            self.report_media_transport(MediaTransportState::Unavailable);
        }
    }

    /// The probe tracker for the transport currently carrying media.
    fn carrying_path(&self) -> MediaPath {
        if self.tcp_lane_active() {
            MediaPath::Tcp
        } else {
            MediaPath::Udp
        }
    }

    fn probes_mut(&mut self, path: MediaPath) -> &mut PathProbes {
        match path {
            MediaPath::Udp => &mut self.udp_probes,
            MediaPath::Tcp => &mut self.tcp_probes,
        }
    }

    fn carrying_probes(&self) -> &PathProbes {
        match self.carrying_path() {
            MediaPath::Udp => &self.udp_probes,
            MediaPath::Tcp => &self.tcp_probes,
        }
    }

    fn carrying_probes_mut(&mut self) -> &mut PathProbes {
        self.probes_mut(self.carrying_path())
    }

    fn udp_verify_wanted(&self) -> bool {
        if self.udp.is_none() || self.session_id.is_none() {
            return false;
        }
        // A path that has proven nothing, or has failed, needs probing to say
        // anything at all. So does one that is carrying media with a probe
        // still outstanding: escalating to this cadence is what separates a
        // single unlucky lost `Pong` from a path that has actually gone, before
        // the sparse [`RTT_PROBE_INTERVAL`] would have asked twice more.
        self.udp_path != UdpPath::Verified || self.udp_probes.pending_since.is_some()
    }

    /// Probes UDP independently of whichever transport carries media, so a
    /// blackholed path is proven — and a recovered one re-proven — without
    /// waiting for the sparse [`RTT_PROBE_INTERVAL`] cadence.
    fn poll_udp_verify(&mut self, now: Instant) {
        if !self.udp_verify_wanted() {
            return;
        }
        self.udp_probes.expire(now, super::UDP_VERIFY_TIMEOUT);
        if now < self.next_udp_verify {
            return;
        }
        self.send_udp_verify_probe(now);
    }

    fn send_udp_verify_probe(&mut self, now: Instant) {
        if !self.udp_verify_wanted() {
            return;
        }
        self.next_udp_verify = now + super::UDP_VERIFY_INTERVAL;
        let nonce = self.next_rtt_nonce();
        self.udp_probes.sent(nonce, now);
        self.send_media_udp(&MediaPayload::Ping {
            nonce,
            observed_rtt_ms: None,
        });
    }

    fn send_nat_probe(&mut self, probe_id: u8, addr: SocketAddr) {
        let counter = self.media_send_counter;
        self.media_send_counter = self.media_send_counter.wrapping_add(1);
        match media::seal_media(&self.media, counter, &MediaPayload::NatProbe { probe_id }) {
            Ok(packet) => self.send_udp_raw("nat_probe", None, addr, &packet),
            Err(error) => kvlog::warn!("nat probe seal failed", probe_id, error = %error),
        }
    }

    fn probe_addr_for_id(&self, probe_id: u8) -> Option<SocketAddr> {
        let udp = self.udp.as_ref()?;
        match probe_id {
            0 => Some(udp.server_addr),
            1 => udp.server_probe_addr,
            _ => None,
        }
    }

    fn interface_monitor_wanted(&self) -> bool {
        (self.p2p_enabled && self.voice_room.is_some())
            || (self.media_transport.is_auto() && self.udp.is_some() && self.tcp_lane_confirmed())
    }

    fn poll_interfaces(&mut self, now: Instant) {
        match self.interface_monitor.poll_with(
            self.interface_monitor_wanted(),
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
        let Some(udp) = self.udp.as_mut() else {
            return Ok(());
        };
        let _ = poll.registry().deregister(&mut udp.socket);
        self.restart_port_policy.record(udp.local_addr.port());
        let bind_addr = RestartPortPolicy::bind_addr_for_restart(if udp.server_addr.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        });
        let mut last_error = None;
        for _ in 0..8 {
            match bind_voice_udp_socket(bind_addr) {
                Ok(socket) => {
                    let local_addr = socket
                        .local_addr()
                        .map_err(|error| format!("failed to read rebound UDP address: {error}"))?;
                    if !self.restart_port_policy.accepts(local_addr.port()) {
                        self.restart_port_policy.record(local_addr.port());
                        continue;
                    }
                    let Some(udp) = self.udp.as_mut() else {
                        return Ok(());
                    };
                    udp.local_addr = local_addr;
                    udp.socket = UdpSocket::from_std(socket);
                    poll.registry()
                        .register(&mut udp.socket, UDP, Interest::READABLE)
                        .map_err(|error| {
                            format!("failed to register rebound UDP socket: {error}")
                        })?;
                    self.reset_server_rtt();
                    self.bind_udp();
                    // The fresh port has proven nothing, whatever the old one
                    // managed: re-verify before trusting it with media.
                    self.begin_udp_verification(Instant::now());
                    self.publish_p2p_candidates();
                    return Ok(());
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(format!(
            "failed to rebind UDP socket to fresh port{}",
            last_error
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
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
        let Some(udp) = self.udp.as_ref() else {
            return GatheredP2p {
                local: Vec::new(),
                published: Vec::new(),
                mdns_names: HashMap::new(),
            };
        };
        let mut next_id = 1;
        let mut candidates = self
            .interface_monitor
            .snapshot()
            .map(|snapshot| {
                snapshot.host_candidates_with_metadata(
                    1,
                    self.p2p_generation,
                    udp.local_addr.port(),
                    true,
                    &mut next_id,
                    self.prefer_ipv6,
                )
            })
            .unwrap_or_default();
        if candidates.is_empty() {
            let fallback_ip = if udp.server_addr.is_ipv4() {
                "127.0.0.1".parse().unwrap()
            } else {
                "::1".parse().unwrap()
            };
            candidates.push(Candidate::with_metadata(
                next_id,
                1,
                self.p2p_generation,
                CandidateKind::Host,
                SocketAddr::new(fallback_ip, udp.local_addr.port()),
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
                Some(udp.local_addr),
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
            udp.server_addr,
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
            // Recorded against the path the probe actually left on: the server
            // answers each `Ping` on its arrival transport, so only a `Pong`
            // returning there proves anything.
            if let Some(path) = self.send_media(&MediaPayload::Ping {
                nonce,
                observed_rtt_ms: self.server_rtt_ms.map(clamp_rtt_ms),
            }) {
                self.probes_mut(path).sent(nonce, now);
                if path == MediaPath::Udp {
                    // This probe is the escalation timer's first ask; a healthy
                    // answer arrives long before it would follow up.
                    self.next_udp_verify = now + super::UDP_VERIFY_INTERVAL;
                }
            }
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
        let Some(udp) = self.udp.as_ref() else {
            return false;
        };
        let server_addr = udp.server_addr;
        let mut buf = [0u8; 2048];
        let mut datagrams_this_wake = 0usize;
        loop {
            let Some(udp) = self.udp.as_ref() else {
                return false;
            };
            let (len, src) = match recv_udp_datagram(&udp.socket, &mut buf) {
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
            } else if src != server_addr {
                kvlog::warn!(
                    "udp packet ignored",
                    addr = %src,
                    expected_addr = %server_addr,
                    packet_size = len
                );
            } else {
                let packet = &mut buf[..len];
                match media::open_media_in_place(&self.media, &mut self.media_recv_replay, packet) {
                    Ok(opened) => {
                        let payload = opened.payload.into_owned();
                        self.note_server_media(MediaPath::Udp);
                        self.handle_server_media(now, MediaPath::Udp, payload);
                    }
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

    /// Dispatches an authenticated server media payload, shared by the UDP
    /// path and the TCP voice lane. `path` is where it arrived, which is what
    /// a `Pong` proves and where an inbound `Ping` must be answered.
    fn handle_server_media(&mut self, now: Instant, path: MediaPath, payload: MediaPayload) {
        match payload {
            MediaPayload::Voice {
                stream_id,
                sequence,
                timestamp,
                flags,
                payload,
            } => {
                let payload_size = payload.len();
                let payload_kind = media_voice_payload_kind(&payload);
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
            MediaPayload::Pong { nonce } => self.observe_pong(now, path, nonce),
            MediaPayload::VoiceFeedbackFrom {
                reporter,
                stream_id,
                feedback,
            } => {
                let feedback = live_feedback_from_media(stream_id, feedback);
                self.handle_encoder_feedback(reporter, feedback, now);
            }
            MediaPayload::Ping { nonce, .. } => {
                // Answered where it arrived, so the peer's probe measures the
                // path it was testing rather than whichever one carries media.
                match path {
                    MediaPath::Udp => self.send_media_udp(&MediaPayload::Pong { nonce }),
                    MediaPath::Tcp => {
                        self.send_media_tcp(&MediaPayload::Pong { nonce });
                    }
                }
            }
            MediaPayload::Bind | MediaPayload::NatProbe { .. } => {}
            MediaPayload::PeerVoice { .. }
            | MediaPayload::PeerVoiceFeedback { .. }
            | MediaPayload::VoiceFeedback { .. } => {}
        }
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
                kvlog::debug!(
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

    /// Sends a server media payload over the active transport: the TCP lane
    /// while it is up, UDP otherwise. Force-TCP sessions drop payloads while
    /// the lane is down instead of leaking them onto UDP.
    ///
    /// Returns the transport it left on, so probes can be recorded against the
    /// path that will answer them; `None` means the payload was dropped.
    fn send_media(&mut self, payload: &MediaPayload) -> Option<MediaPath> {
        if self.tcp_lane_active() {
            return self.send_media_tcp(payload).then_some(MediaPath::Tcp);
        }
        if !self.media_transport.is_auto() || self.udp.is_none() {
            return None;
        }
        self.send_media_udp(payload);
        Some(MediaPath::Udp)
    }

    fn send_media_udp(&mut self, payload: &MediaPayload) {
        let Some(udp) = self.udp.as_ref() else {
            return;
        };
        let server_addr = udp.server_addr;
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
                if let Err(error) = udp.socket.send_to(&packet, server_addr) {
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

    /// Queues a payload on the lane, reporting whether it was accepted.
    fn send_media_tcp(&mut self, payload: &MediaPayload) -> bool {
        let now = Instant::now();
        if self.fail_stale_lane_backlog(now) {
            return false;
        }
        let Some(lane) = self.tcp_lane.as_ref() else {
            return false;
        };
        // Reserve a maximum frame before spending a counter or doing crypto.
        // Once the queue reaches real-time latency territory, abandon the
        // entire stale stream rather than later replaying it in FIFO order.
        if lane.write_queue.len() > TCP_LANE_WRITE_CAP_BYTES - TCP_LANE_MAX_WIRE_FRAME_BYTES {
            self.fail_tcp_lane("write backlog overflow");
            return false;
        }
        let droppable = matches!(
            payload,
            MediaPayload::Voice { .. } | MediaPayload::VoiceFeedback { .. }
        );
        let kind = media_payload_kind(payload);
        let counter = self.media_send_counter;
        self.media_send_counter = self.media_send_counter.wrapping_add(1);
        if let Err(error) = media::seal_media_into(
            &self.media,
            counter,
            payload,
            &mut self.media_packet,
            &mut self.media_scratch,
        ) {
            kvlog::warn!("voice tcp seal failed", kind, error = %error);
            return false;
        }
        let Some(lane) = self.tcp_lane.as_mut() else {
            return false;
        };
        let packet = std::mem::take(&mut self.media_packet);
        if frame::encode_frame(&packet, lane.write_queue.tail_mut()).is_err() {
            debug_assert!(false, "sealed media exceeds the control frame cap");
            self.media_packet = packet;
            return false;
        }
        if lane.backlog_since.is_none() {
            lane.backlog_since = Some(now);
        }
        if !droppable {
            kvlog::info!(
                "voice tcp packet sent",
                kind,
                packet_size = packet.len(),
                counter
            );
        }
        self.media_packet = packet;
        self.flush_tcp_lane();
        true
    }

    /// Closes a lane whose oldest unsent byte is older than
    /// [`TCP_LANE_STALE_BACKLOG`], reporting whether it did.
    ///
    /// The byte cap is a memory bound, not a latency one: 32 KiB is seconds of
    /// speech at ordinary bitrates, so freshness needs its own limit. Checked
    /// at enqueue and after each flush rather than on a timer — a backlog with
    /// nothing fresh behind it is harmless, and a speaking client enqueues
    /// every 20 ms.
    fn fail_stale_lane_backlog(&mut self, now: Instant) -> bool {
        let stale = self.tcp_lane.as_ref().is_some_and(|lane| {
            matches!(lane.state, TcpLaneState::Active)
                && lane.backlog_since.is_some_and(|since| {
                    now.saturating_duration_since(since) >= TCP_LANE_STALE_BACKLOG
                })
        });
        if stale {
            self.fail_tcp_lane("stale backlog");
        }
        stale
    }

    fn lane_wanted(&self) -> bool {
        !self.media_transport.is_auto() || self.udp_path != UdpPath::Verified
    }

    fn tcp_lane_active(&self) -> bool {
        self.tcp_lane
            .as_ref()
            .is_some_and(|lane| matches!(lane.state, TcpLaneState::Active))
    }

    fn tcp_lane_confirmed(&self) -> bool {
        self.tcp_lane
            .as_ref()
            .is_some_and(|lane| lane.confirmed && matches!(lane.state, TcpLaneState::Active))
    }

    fn suspect_udp_path(&mut self, now: Instant) {
        if !self.media_transport.is_auto() || self.tcp_lane_active() {
            return;
        }
        self.latch_udp_suspect();
        if !self.awaiting_udp_bound {
            self.awaiting_udp_bound = true;
            self.udp_bind_attempts = 0;
            self.next_udp_bind_retry = now + UDP_BIND_RETRY_INTERVAL;
        }
    }

    /// Records that the relay accepted and answered on a freshly opened lane.
    ///
    /// Deliberately not liveness: an authenticated packet only proves the
    /// server-to-client direction, and in a busy room inbound speech would
    /// otherwise mask a blackholed upstream forever. Path liveness comes from
    /// [`VoiceSession::observe_pong`] alone.
    fn note_server_media(&mut self, path: MediaPath) {
        if path != MediaPath::Tcp {
            return;
        }
        let newly_confirmed = self.tcp_lane.as_mut().is_some_and(|lane| {
            if matches!(lane.state, TcpLaneState::Active) && !lane.confirmed {
                lane.confirmed = true;
                true
            } else {
                false
            }
        });
        if newly_confirmed {
            kvlog::info!("voice tcp lane confirmed by server");
            self.tcp_backoff = TCP_LANE_BACKOFF_MIN;
            self.report_media_transport(MediaTransportState::Tcp);
            if self.udp.is_some() && self.udp_path != UdpPath::Verified {
                self.udp_over_tcp_recovery.arm(Instant::now());
            }
        }
    }

    /// Matches a `Pong` against the probes `path` sent, the only proof that a
    /// transport works in both directions. An unmatched nonce, or one this path
    /// never sent, leaves the liveness window running.
    fn observe_pong(&mut self, now: Instant, path: MediaPath, nonce: u64) {
        let Some(sample) = self.probes_mut(path).matched(nonce, now) else {
            kvlog::debug!("unmatched pong ignored", path = path.label(), nonce);
            return;
        };
        // Only the carrying transport's samples describe the media path; a UDP
        // verification probe answered while the lane carries media must not
        // move the reported RTT.
        if path == self.carrying_path() {
            let rtt = fold_rtt_ewma(self.server_rtt_ms, sample);
            self.server_rtt_ms = Some(rtt);
            self.server_rtt_last_sample_at = Some(now);
            let _ = self.events.send(NetworkEvent::ServerRtt {
                rtt_ms: Some(clamp_rtt_ms(rtt)),
            });
            self.publish_all_relay_rtts();
        }
        if path == MediaPath::Udp {
            if self.tcp_lane_confirmed() && self.udp_path != UdpPath::Verified {
                self.udp_over_tcp_recovery.arm(now);
            }
            self.maybe_recover_udp();
        }
    }

    /// Fails the media path once probes have gone unanswered for
    /// [`RTT_STALE_AFTER`]. A live lane is reconnected under its own backoff; a
    /// blackholed UDP path asks for a lane instead.
    fn poll_server_path_liveness(&mut self, now: Instant) {
        let carrying = self.carrying_path();
        let Some(since) = self.probes_mut(carrying).pending_since else {
            return;
        };
        if now.saturating_duration_since(since) < super::RTT_STALE_AFTER {
            return;
        }
        kvlog::warn!("server media path silent", path = carrying.label());
        if self.tcp_lane_active() {
            self.drop_tcp_lane("server silent");
            self.schedule_tcp_reconnect(now);
            return;
        }
        // Restarted rather than cleared: an unanswered UDP path keeps the
        // window running so a lane that later drops is detected again.
        self.udp_probes.pending_since = Some(now);
        self.suspect_udp_path(now);
    }

    fn poll_tcp_lane(&mut self, now: Instant) {
        match &self.tcp_lane {
            None => {
                if self.lane_wanted() && self.session_id.is_some() && now >= self.next_tcp_connect {
                    self.connect_tcp_lane(now);
                }
            }
            Some(lane) => {
                match lane.state {
                    TcpLaneState::Connecting { deadline } if now >= deadline => {
                        self.fail_tcp_lane("connect timeout");
                        return;
                    }
                    TcpLaneState::Draining { deadline } if now >= deadline => {
                        self.drop_tcp_lane("drain timeout");
                        return;
                    }
                    _ => {}
                }
                self.flush_tcp_lane();
            }
        }
    }

    fn connect_tcp_lane(&mut self, now: Instant) {
        debug_assert!(self.tcp_lane.is_none());
        let mut stream = match TcpStream::connect(self.server_tcp_addr) {
            Ok(stream) => stream,
            Err(error) => {
                kvlog::warn!("voice tcp lane connect failed", addr = %self.server_tcp_addr, error = %error);
                self.schedule_tcp_reconnect(now);
                return;
            }
        };
        if let Err(error) = self.registry.register(
            &mut stream,
            VOICE_TCP,
            Interest::READABLE | Interest::WRITABLE,
        ) {
            kvlog::warn!("voice tcp lane register failed", error = %error);
            self.schedule_tcp_reconnect(now);
            return;
        }
        kvlog::info!("voice tcp lane connecting", addr = %self.server_tcp_addr);
        self.tcp_lane = Some(TcpLane {
            stream,
            state: TcpLaneState::Connecting {
                deadline: now + TCP_LANE_CONNECT_TIMEOUT,
            },
            confirmed: false,
            read_buf: RecvBuffer::new(),
            readiness: Readiness::new(),
            write_queue: WriteQueue::new(),
            write_blocked: false,
            backlog_since: None,
        });
    }

    fn tcp_lane_writable(&mut self) {
        let Some(lane) = self.tcp_lane.as_mut() else {
            return;
        };
        lane.write_blocked = false;
        if let TcpLaneState::Connecting { .. } = lane.state {
            let error = match lane.stream.take_error() {
                Ok(None) => match lane.stream.peer_addr() {
                    Ok(_) => None,
                    Err(error) if error.kind() == io::ErrorKind::NotConnected => return,
                    Err(error) => Some(error),
                },
                Ok(Some(error)) | Err(error) => Some(error),
            };
            if let Some(error) = error {
                kvlog::warn!("voice tcp lane connect failed", addr = %self.server_tcp_addr, error = %error);
                self.fail_tcp_lane("connect error");
                return;
            }
            self.activate_tcp_lane();
        }
        self.flush_tcp_lane();
    }

    fn activate_tcp_lane(&mut self) {
        let Some(lane) = self.tcp_lane.as_mut() else {
            return;
        };
        if let Err(error) = lane.stream.set_nodelay(true) {
            kvlog::warn!("voice tcp lane nodelay failed", error = %error);
        }
        if let Ok(local_addr) = lane.stream.local_addr()
            && let Err(_error) = rpc::qos::apply_voice_qos(lane.stream.as_raw_fd(), local_addr)
        {
            kvlog::debug!("voice tcp lane qos unavailable", error = %_error);
        }
        let now = Instant::now();
        lane.state = TcpLaneState::Active;
        lane.write_queue
            .tail_mut()
            .extend_from_slice(&media::VOICE_TCP_MAGIC);
        lane.backlog_since = Some(now);
        kvlog::info!("voice tcp lane connected", addr = %self.server_tcp_addr);
        // The sealed Bind authenticates the lane to the session; pulling the
        // RTT probe forward gets a Ping/Pong RTT sample over it immediately.
        // The liveness window restarts with the transport: the new path has
        // been neither proven nor disproven.
        self.send_media(&MediaPayload::Bind);
        self.next_rtt_probe = now;
        self.tcp_probes.restart(now);
    }

    fn flush_tcp_lane(&mut self) {
        let Some(lane) = self.tcp_lane.as_mut() else {
            return;
        };
        if matches!(lane.state, TcpLaneState::Draining { .. })
            || lane.write_blocked
            || lane.write_queue.is_empty()
        {
            return;
        }
        match write_queue_to(
            &mut lane.stream,
            &mut lane.write_queue,
            TCP_LANE_WRITE_ATTEMPTS,
        ) {
            Ok(outcome) => {
                let now = Instant::now();
                if lane.write_queue.is_empty() {
                    lane.backlog_since = None;
                }
                if outcome.blocked {
                    lane.write_blocked = true;
                }
                if outcome.wrote_zero {
                    self.fail_tcp_lane("write returned zero");
                } else {
                    self.fail_stale_lane_backlog(now);
                }
            }
            Err(error) => {
                kvlog::warn!("voice tcp lane write failed", error = %error);
                self.fail_tcp_lane("write error");
            }
        }
    }

    /// Drains a bounded lane read burst, mirroring [`VoiceSession::read_udp`]:
    /// `true` retains work after the budget so buffered frames are not
    /// stranded.
    fn read_tcp_lane(&mut self) -> bool {
        let Some(lane) = self.tcp_lane.as_mut() else {
            return false;
        };
        let outcome = match read_into_buffer(
            &lane.stream,
            &mut lane.read_buf,
            &mut lane.readiness,
            2048,
            ReadLimit::ByteBudget(TCP_LANE_READ_BUDGET_BYTES),
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                kvlog::warn!("voice tcp lane read failed", error = %error);
                self.fail_tcp_lane("read error");
                return false;
            }
        };
        let mut read_buf = std::mem::take(&mut lane.read_buf);
        loop {
            let total = match frame::parse_frame_with_limit(
                read_buf.pending(),
                media::VOICE_TCP_MAX_FRAME_BYTES,
            ) {
                Ok(Some((_, total))) => total,
                Ok(None) => break,
                Err(error) => {
                    kvlog::warn!("voice tcp lane frame invalid", error = %error);
                    self.fail_tcp_lane("invalid frame");
                    return false;
                }
            };
            let now = Instant::now();
            let packet = &mut read_buf.pending_mut()[frame::LENGTH_PREFIX_LEN..total];
            match media::open_media_in_place(&self.media, &mut self.media_recv_replay, packet) {
                Ok(opened) => {
                    let payload = opened.payload.into_owned();
                    self.note_server_media(MediaPath::Tcp);
                    self.handle_server_media(now, MediaPath::Tcp, payload);
                }
                Err(error) => {
                    kvlog::warn!("voice tcp lane packet rejected", error = %error);
                }
            }
            read_buf.consume(total);
        }
        let Some(lane) = self.tcp_lane.as_mut() else {
            return false;
        };
        lane.read_buf = read_buf;
        if outcome.disconnected {
            if matches!(lane.state, TcpLaneState::Draining { .. }) {
                self.drop_tcp_lane("drained");
            } else {
                self.fail_tcp_lane("server closed");
            }
            return false;
        }
        outcome.hit_limit
    }

    /// Half-closes an active lane after UDP recovery: no more writes, but
    /// in-flight downstream frames still play until the server's EOF.
    fn drain_tcp_lane(&mut self) {
        self.flush_tcp_lane();
        let Some(lane) = self.tcp_lane.as_mut() else {
            return;
        };
        if !matches!(lane.state, TcpLaneState::Active) {
            self.drop_tcp_lane("udp recovered");
            return;
        }
        kvlog::info!("voice tcp lane draining, udp recovered");
        let _ = lane.stream.shutdown(std::net::Shutdown::Write);
        let now = Instant::now();
        lane.state = TcpLaneState::Draining {
            deadline: now + TCP_LANE_DRAIN_TIMEOUT,
        };
        // UDP takes over carrying media: its window runs from here.
        self.udp_probes.pending_since.get_or_insert(now);
    }

    fn drop_tcp_lane(&mut self, reason: &str) {
        let Some(mut lane) = self.tcp_lane.take() else {
            return;
        };
        self.udp_over_tcp_recovery.disarm();
        let _ = self.registry.deregister(&mut lane.stream);
        kvlog::info!("voice tcp lane closed", reason);
        self.udp_probes.pending_since.get_or_insert(Instant::now());
        if matches!(lane.state, TcpLaneState::Active) && self.lane_wanted() {
            self.report_media_transport(MediaTransportState::Unavailable);
        }
    }

    fn report_media_transport(&mut self, state: MediaTransportState) {
        if self.reported_media_transport == state {
            return;
        }
        self.reported_media_transport = state;
        let _ = self.events.send(NetworkEvent::MediaTransport { state });
    }

    fn fail_tcp_lane(&mut self, reason: &str) {
        self.drop_tcp_lane(reason);
        self.schedule_tcp_reconnect(Instant::now());
    }

    fn schedule_tcp_reconnect(&mut self, now: Instant) {
        self.next_tcp_connect = now + self.tcp_backoff;
        self.tcp_backoff = (self.tcp_backoff * 2).min(TCP_LANE_BACKOFF_MAX);
    }

    fn send_udp_raw(
        &mut self,
        kind: &'static str,
        session_id: Option<SessionId>,
        addr: SocketAddr,
        packet: &[u8],
    ) {
        let Some(udp) = self.udp.as_ref() else {
            return;
        };
        match udp.socket.send_to(packet, addr) {
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
                Ok((
                    _,
                    MediaPayload::PeerVoice {
                        connection_id,
                        stream_id,
                        sequence,
                        timestamp,
                        flags,
                        payload,
                    },
                )) if connection_id == peer.connection_id
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
                Ok((
                    _,
                    MediaPayload::PeerVoiceFeedback {
                        connection_id,
                        stream_id,
                        feedback,
                    },
                )) if connection_id == peer.connection_id && active_stream == Some(stream_id) => {
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
                    let rtt_ms =
                        take_rtt_sample(&mut peer.rtt_in_flight, nonce, now).map(|sample| {
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
            Ok(P2pMediaPacket::Feedback {
                stream_id,
                feedback,
                action,
            }) => {
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
            Some((
                addr,
                media::seal_peer_media(&peer.send_key, counter, &payload),
            ))
        }) else {
            return;
        };
        match packet {
            Ok(packet) => self.send_udp_raw("p2p_voice_feedback", Some(session_id), addr, &packet),
            Err(error) => kvlog::warn!("p2p feedback seal failed", error = %error),
        }
    }

    fn apply_p2p_actions(&mut self, session_id: SessionId, actions: Vec<P2pAction>) {
        for action in actions {
            match action {
                P2pAction::UseRelay { reason, .. } => {
                    if let Some(user_id) = self.p2p_peers.get(&session_id).map(|peer| peer.user_id)
                    {
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
    use std::io::{Read, Write};
    use std::net::TcpListener as StdTcpListener;
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
        let socket = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        socket.set_nonblocking(true).unwrap();
        VoiceCommand::StartSession {
            generation,
            udp: Some(UdpMediaSetup {
                socket,
                server_addr: "127.0.0.1:9".parse().unwrap(),
                server_probe_addr: None,
            }),
            media: protection(100 + generation as u32),
            initial_bind_attempted: false,
            transport_mode: TransportMode::Encrypted,
            server_tcp_addr: "127.0.0.1:9".parse().unwrap(),
            media_transport: MediaTransportSetting::Auto,
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
            assert!(
                submission
                    .submit_microphone(Some(sequence), frame(sequence))
                    .is_ok()
            );
        }
        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        assert!(!submission.drain_into(&mut controls, &mut microphone, &mut feedback, &mut sink,));
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
        assert!(!submission.drain_into(&mut controls, &mut microphone, &mut feedback, &mut sink,));
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
        poll.poll(&mut events, Some(Duration::from_secs(1)))
            .unwrap();
        assert!(events.iter().any(|event| event.token() == COMMANDS));
    }

    #[test]
    fn session_controls_wait_for_voice_activation() {
        let (mut poll, submission) = submission();
        assert!(submission.submit(start_command(7)).is_ok());
        assert!(
            submission
                .submit(VoiceCommand::Authenticated {
                    generation: 7,
                    session_id: SessionId(4),
                })
                .is_ok()
        );

        let mut events = Events::with_capacity(2);
        poll.poll(&mut events, Some(Duration::ZERO)).unwrap();
        assert!(events.is_empty());
        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        assert!(!submission.drain_into(&mut controls, &mut microphone, &mut feedback, &mut sink,));
        assert!(controls.is_empty());

        assert!(submission.activate(7).is_ok());
        poll.poll(&mut events, Some(Duration::from_secs(1)))
            .unwrap();
        assert!(events.iter().any(|event| event.token() == COMMANDS));
        assert!(!submission.drain_into(&mut controls, &mut microphone, &mut feedback, &mut sink,));
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
        assert!(submission.submit(start_command_with_p2p(7, true)).is_ok());
        assert!(
            submission
                .submit(VoiceCommand::SetP2pEnabled {
                    generation: 7,
                    enabled: false,
                })
                .is_ok()
        );

        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        assert!(!submission.drain_into(&mut controls, &mut microphone, &mut feedback, &mut sink,));
        assert!(controls.is_empty());
        assert!(submission.activate(7).is_ok());
        assert!(!submission.drain_into(&mut controls, &mut microphone, &mut feedback, &mut sink,));
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
            assert!(
                submission
                    .submit(VoiceCommand::Authenticated {
                        generation: 1,
                        session_id: SessionId(2),
                    })
                    .is_ok()
            );
        }
        assert!(
            submission
                .submit(VoiceCommand::Authenticated {
                    generation: 1,
                    session_id: SessionId(2),
                })
                .is_err()
        );
        assert!(submission.submit(VoiceCommand::Shutdown).is_ok());
        let mailbox = submission.mailbox.lock().unwrap();
        assert_eq!(mailbox.controls.len(), 1);
        assert!(matches!(
            mailbox.controls.front(),
            Some(VoiceCommand::Shutdown)
        ));
    }

    #[test]
    fn stopped_output_wakes_and_is_observable_without_a_fatal_payload() {
        let mut poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(9)).unwrap());
        let output = VoiceOutputSubmission::new(waker);
        output.stop();
        let mut events = Events::with_capacity(2);
        poll.poll(&mut events, Some(Duration::from_secs(1)))
            .unwrap();
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
        assert!(
            submission
                .submit_playback_sink(Some(LivePlaybackSink::for_test()))
                .is_ok()
        );
        assert!(submission.submit_playback_sink(None).is_ok());

        let mut controls = VecDeque::new();
        let mut microphone = VecDeque::new();
        let mut feedback = Vec::new();
        let mut sink = None;
        assert!(!submission.drain_into(&mut controls, &mut microphone, &mut feedback, &mut sink,));
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback[0].feedback.highest_contiguous_sequence, 9);
        assert!(matches!(sink, Some(None)));
        sink = None;

        assert!(
            submission
                .submit(VoiceCommand::EndSession { generation: 1 })
                .is_ok()
        );
        assert!(submission.submit_feedback(old).is_ok());
        feedback.clear();
        assert!(!submission.drain_into(&mut controls, &mut microphone, &mut feedback, &mut sink,));
        assert!(matches!(
            controls.pop_front(),
            Some(VoiceCommand::EndSession { generation: 1 })
        ));
        assert!(feedback.is_empty());
    }

    #[test]
    fn wrong_server_source_does_not_consume_replay_or_dispatch_packet() {
        let mut actor_poll = Poll::new().unwrap();
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
            Some(UdpMediaSetup {
                socket: udp,
                server_addr: server.local_addr().unwrap(),
                server_probe_addr: None,
            }),
            protection(55),
            false,
            TransportMode::Encrypted,
            server.local_addr().unwrap(),
            MediaTransportSetting::Auto,
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
        let mut events = Events::with_capacity(1);
        wrong.send_to(&packet, actor_addr).unwrap();
        actor_poll
            .poll(&mut events, Some(Duration::from_secs(1)))
            .unwrap();
        assert!(events.iter().any(|event| event.token() == UDP));
        assert!(!session.read_udp());
        assert!(session.pending_playback_packets.is_empty());

        let before = Instant::now();
        server.send_to(&packet, actor_addr).unwrap();
        actor_poll
            .poll(&mut events, Some(Duration::from_secs(1)))
            .unwrap();
        assert!(events.iter().any(|event| event.token() == UDP));
        assert!(!session.read_udp());
        let after = Instant::now();
        let received = session.pending_playback_packets.front().unwrap();
        assert_eq!(received.stream_id, 8);
        assert!(received.received_at >= before && received.received_at <= after);

        session.voice_room = Some(RoomId(2));
        session.voice_started(RoomId(2), SessionId(6), UserId(7), StreamId(8), false);
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
            Some(UdpMediaSetup {
                socket: udp,
                server_addr: server.local_addr().unwrap(),
                server_probe_addr: None,
            }),
            protection(55),
            false,
            TransportMode::Encrypted,
            server.local_addr().unwrap(),
            MediaTransportSetting::Auto,
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
        session.voice_started(RoomId(2), SessionId(6), UserId(7), StreamId(8), false);
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
            Some(UdpMediaSetup {
                socket: udp,
                server_addr: server.local_addr().unwrap(),
                server_probe_addr: None,
            }),
            protection(55),
            false,
            TransportMode::Encrypted,
            server.local_addr().unwrap(),
            MediaTransportSetting::Auto,
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
            crate::app::AppEvent::NetworkFor { event: NetworkEvent::PlaybackFeedback(received), .. }
                if received.stream_id == 8
        ));
        let mut datagram = [0u8; 2048];
        let (len, _) = server.recv_from(&mut datagram).unwrap();
        let opened =
            media::open_media(&protection(55), &mut AntiReplay::new(), &datagram[..len]).unwrap();
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
        actor.playback_ingress = PlaybackIngressState::Attached(LivePlaybackSink::for_test());
        actor.apply_command(start_command(4));
        let session = actor.session.as_mut().unwrap();
        assert!(session.playback_ingress.sink().is_some());
        session.media_send_counter = 99;
        session.local_sequence = 77;
        session.voice_room = Some(RoomId(8));
        session.active_stream = Some(StreamId(12));
        session
            .pending_playback_packets
            .push_back(crate::audio::RemoteVoicePacket {
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
        assert!(session.udp_probes.in_flight.is_empty());
        assert!(session.tcp_probes.in_flight.is_empty());
        assert!(session.playback_ingress.sink().is_some());
    }

    #[test]
    fn dropping_handle_joins_blocked_loop_and_closes_direct_sender() {
        let main_poll = Poll::new().unwrap();
        let main_waker = Arc::new(Waker::new(main_poll.registry(), Token(9)).unwrap());
        let (event_tx, _event_rx) = mpsc::channel();
        let handle =
            VoiceLoopHandle::spawn(NetworkEventSender::for_test(event_tx), main_waker).unwrap();
        let control = handle.control();
        assert!(control.submit(start_command(1)).is_ok());
        assert!(control.activate(1).is_ok());
        let input = handle.input_sender();
        drop(handle);
        let command = NetworkCommand::LocalVoicePacket(frame(1));
        assert!(matches!(
            input.send(command),
            Err(SendError(NetworkCommand::LocalVoicePacket(_)))
        ));
    }

    #[test]
    fn initial_bind_dispatch_does_not_start_voice_loop() {
        let main_poll = Poll::new().unwrap();
        let main_waker = Arc::new(Waker::new(main_poll.registry(), Token(9)).unwrap());
        let (event_tx, _event_rx) = mpsc::channel();
        let handle =
            VoiceLoopHandle::spawn(NetworkEventSender::for_test(event_tx), main_waker).unwrap();
        let server = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let udp = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let bind =
            InitialUdpBind::prepare(&udp, &protection(78), server.local_addr().unwrap()).unwrap();

        bind.dispatch().unwrap();
        let mut empty = [0u8; 1];
        assert_eq!(
            udp.recv_from(&mut empty).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        let mut datagram = [0u8; 2048];
        let (len, _) = server.recv_from(&mut datagram).unwrap();
        let opened =
            media::open_media(&protection(78), &mut AntiReplay::new(), &datagram[..len]).unwrap();
        assert_eq!(opened.header.counter, 0);
        assert_eq!(opened.payload, MediaPayload::Bind);
        assert!(handle.runtime.thread.lock().unwrap().is_none());
    }

    #[test]
    fn dedicated_thread_binds_and_sends_voice_while_main_side_does_not_drain() {
        let main_poll = Poll::new().unwrap();
        let main_waker = Arc::new(Waker::new(main_poll.registry(), Token(9)).unwrap());
        let (event_tx, _event_rx) = mpsc::channel();
        let handle =
            VoiceLoopHandle::spawn(NetworkEventSender::for_test(event_tx), main_waker).unwrap();
        assert!(handle.runtime.thread.lock().unwrap().is_none());
        let control = handle.control();
        let input = handle.input_sender();
        let server = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let udp = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        udp.set_nonblocking(true).unwrap();
        assert!(
            control
                .submit(VoiceCommand::StartSession {
                    generation: 7,
                    udp: Some(UdpMediaSetup {
                        socket: udp,
                        server_addr: server.local_addr().unwrap(),
                        server_probe_addr: None,
                    }),
                    media: protection(77),
                    initial_bind_attempted: false,
                    transport_mode: TransportMode::Encrypted,
                    server_tcp_addr: server.local_addr().unwrap(),
                    media_transport: MediaTransportSetting::Auto,
                    p2p_enabled: true,
                    candidate_privacy: CandidatePrivacy::Disabled,
                    prefer_ipv6: false,
                })
                .is_ok()
        );
        assert!(handle.runtime.thread.lock().unwrap().is_none());
        assert!(
            control
                .submit(VoiceCommand::Authenticated {
                    generation: 7,
                    session_id: SessionId(4),
                })
                .is_ok()
        );
        assert!(control.activate(7).is_ok());
        assert!(handle.runtime.thread.lock().unwrap().is_some());

        let mut replay = AntiReplay::new();
        let mut datagram = [0u8; 2048];
        let (len, _) = server.recv_from(&mut datagram).unwrap();
        let opened = media::open_media(&protection(77), &mut replay, &datagram[..len]).unwrap();
        assert_eq!(opened.header.counter, 0);
        assert_eq!(opened.payload, MediaPayload::Bind);

        assert!(
            control
                .submit(VoiceCommand::VoiceStarted {
                    generation: 7,
                    room_id: RoomId(2),
                    session_id: SessionId(4),
                    user_id: UserId(3),
                    stream_id: StreamId(9),
                    local: true,
                })
                .is_ok()
        );
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
            if let MediaPayload::Voice {
                stream_id,
                sequence,
                ..
            } = opened.payload
            {
                assert_eq!(stream_id, StreamId(9));
                assert_eq!(sequence, 12);
                received_voice = true;
                break;
            }
        }
        assert!(received_voice);
        drop(handle);
    }

    fn lane_test_session(
        media_transport: MediaTransportSetting,
        server_tcp_addr: SocketAddr,
    ) -> (VoiceSession, Poll, mpsc::Receiver<crate::app::AppEvent>) {
        let poll = Poll::new().unwrap();
        // Mirrors the worker: a forced-TCP session is handed no UDP leg at all.
        let udp = media_transport.is_auto().then(|| {
            let socket = bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
            socket.set_nonblocking(true).unwrap();
            UdpMediaSetup {
                socket,
                server_addr: "127.0.0.1:9".parse().unwrap(),
                server_probe_addr: None,
            }
        });
        let (event_tx, event_rx) = mpsc::channel();
        let session = VoiceSession::new(
            &poll,
            1,
            udp,
            protection(55),
            false,
            TransportMode::Encrypted,
            server_tcp_addr,
            media_transport,
            false,
            CandidatePrivacy::Disabled,
            false,
            NetworkEventSender::for_test(event_tx),
        )
        .unwrap();
        (session, poll, event_rx)
    }

    fn drive_lane_active(session: &mut VoiceSession) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !session.tcp_lane_active() {
            assert!(Instant::now() < deadline, "lane failed to activate");
            session.tcp_lane_writable();
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn drive_lane_confirmed(session: &mut VoiceSession) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let lane = session
                .tcp_lane
                .as_ref()
                .expect("lane closed before confirmation");
            if lane.confirmed {
                return;
            }
            assert!(Instant::now() < deadline, "lane failed to confirm");
            session.tcp_lane.as_mut().unwrap().readiness.mark_ready();
            session.read_tcp_lane();
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn force_tcp_lane_skips_udp_and_authenticates_with_magic_then_bind() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Tcp, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        assert!(!session.awaiting_udp_bound);

        session.poll_tcp_lane(Instant::now());
        assert!(session.tcp_lane.is_some());
        let (mut server_end, _) = listener.accept().unwrap();
        server_end
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        drive_lane_active(&mut session);

        let mut magic = [0u8; 8];
        server_end.read_exact(&mut magic).unwrap();
        assert_eq!(magic, media::VOICE_TCP_MAGIC);
        let mut prefix = [0u8; frame::LENGTH_PREFIX_LEN];
        server_end.read_exact(&mut prefix).unwrap();
        let mut sealed = vec![0u8; u32::from_le_bytes(prefix) as usize];
        server_end.read_exact(&mut sealed).unwrap();
        let mut replay = AntiReplay::new();
        let opened = media::open_media(&protection(55), &mut replay, &sealed).unwrap();
        assert_eq!(opened.payload, MediaPayload::Bind);
        assert_eq!(opened.header.counter, 0);
    }

    #[test]
    fn tcp_lane_is_confirmed_only_by_valid_server_media() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, event_rx) =
            lane_test_session(MediaTransportSetting::Tcp, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        session.tcp_backoff = TCP_LANE_BACKOFF_MAX;
        session.poll_tcp_lane(Instant::now());
        let (mut server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);

        assert!(!session.tcp_lane.as_ref().unwrap().confirmed);
        assert_eq!(session.tcp_backoff, TCP_LANE_BACKOFF_MAX);
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Unavailable,
                },
                ..
            }
        )));

        let packet =
            media::seal_media(&protection(55), 0, &MediaPayload::Pong { nonce: 17 }).unwrap();
        let mut framed = Vec::new();
        frame::encode_frame(&packet, &mut framed).unwrap();
        let packet =
            media::seal_media(&protection(55), 1, &MediaPayload::Pong { nonce: 18 }).unwrap();
        frame::encode_frame(&packet, &mut framed).unwrap();
        server_end.write_all(&framed).unwrap();
        drive_lane_confirmed(&mut session);

        assert!(session.tcp_lane.as_ref().unwrap().confirmed);
        assert_eq!(session.tcp_backoff, TCP_LANE_BACKOFF_MIN);
        assert_eq!(
            event_rx
                .try_iter()
                .filter(|event| matches!(
                    event,
                    crate::app::AppEvent::NetworkFor {
                        event: NetworkEvent::MediaTransport {
                            state: MediaTransportState::Tcp,
                        },
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn unconfirmed_tcp_close_preserves_escalating_backoff() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, event_rx) =
            lane_test_session(MediaTransportSetting::Tcp, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Unavailable,
                },
                ..
            }
        )));
        session.tcp_backoff = TCP_LANE_BACKOFF_MIN * 4;
        session.poll_tcp_lane(Instant::now());
        let (server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);
        drop(server_end);

        session.tcp_lane.as_mut().unwrap().readiness.mark_ready();
        session.read_tcp_lane();

        assert!(session.tcp_lane.is_none());
        assert_eq!(session.tcp_backoff, TCP_LANE_BACKOFF_MIN * 8);
        assert!(!event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport { .. },
                ..
            }
        )));
    }

    #[test]
    fn tcp_write_cap_closes_confirmed_lane_and_discards_backlog() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, event_rx) =
            lane_test_session(MediaTransportSetting::Tcp, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        session.poll_tcp_lane(Instant::now());
        let (_server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);
        session.note_server_media(MediaPath::Tcp);
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Tcp,
                },
                ..
            }
        )));

        let counter = session.media_send_counter;
        let lane = session.tcp_lane.as_mut().unwrap();
        lane.write_blocked = true;
        lane.write_queue
            .tail_mut()
            .extend_from_slice(&vec![0; TCP_LANE_WRITE_CAP_BYTES + 1]);
        session.send_media(&MediaPayload::Pong { nonce: 19 });

        assert!(session.tcp_lane.is_none());
        assert_eq!(session.media_send_counter, counter);
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Unavailable,
                },
                ..
            }
        )));
    }

    /// Answers the oldest probe outstanding on `path`, the way the relay does:
    /// on the transport the `Ping` arrived over.
    fn answer_probe(session: &mut VoiceSession, path: MediaPath) {
        answer_probe_at(session, path, Instant::now());
    }

    fn answer_probe_at(session: &mut VoiceSession, path: MediaPath, now: Instant) {
        let nonce = session
            .probes_mut(path)
            .in_flight
            .front()
            .expect("a probe was outstanding")
            .0;
        session.handle_server_media(now, path, MediaPayload::Pong { nonce });
    }

    #[test]
    fn repeated_bind_failures_latch_udp_suspect() {
        let (mut session, _poll, event_rx) =
            lane_test_session(MediaTransportSetting::Auto, "127.0.0.1:9".parse().unwrap());
        session.authenticated(SessionId(6));
        assert!(session.awaiting_udp_bound);
        assert_eq!(session.udp_path, UdpPath::Unproven);

        let mut now = Instant::now();
        for _ in 0..UDP_BIND_FAILURE_ATTEMPTS {
            now += UDP_BIND_RETRY_INTERVAL;
            session.poll_udp_bind_retry(now);
        }
        assert_eq!(session.udp_path, UdpPath::Failed);
        assert!(session.lane_wanted());
        let unreachable_reports = event_rx
            .try_iter()
            .filter(|event| {
                matches!(
                    event,
                    crate::app::AppEvent::NetworkFor {
                        event: NetworkEvent::MediaTransport {
                            state: MediaTransportState::Unavailable,
                        },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(unreachable_reports, 1);
    }

    #[test]
    fn udp_bind_failures_preserve_confirmed_tcp_status() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, event_rx) =
            lane_test_session(MediaTransportSetting::Auto, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        session.suspect_udp_path(Instant::now());
        session.poll_tcp_lane(session.next_tcp_connect);
        let (_server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);
        session.note_server_media(MediaPath::Tcp);
        assert_eq!(session.reported_media_transport, MediaTransportState::Tcp);
        event_rx.try_iter().for_each(drop);

        let counter_before = session.media_send_counter;
        let mut now = Instant::now();
        for _ in UDP_OVER_TCP_RECOVERY_DELAYS {
            now = session
                .udp_over_tcp_recovery
                .next_attempt()
                .expect("recovery attempt was scheduled");
            session.poll_udp_over_tcp_recovery(now);
        }

        assert_eq!(session.udp_path, UdpPath::Failed);
        assert_eq!(session.reported_media_transport, MediaTransportState::Tcp);
        assert!(session.udp_over_tcp_recovery.next_attempt().is_none());
        // Each combined attempt sends one Bind and one Ping.
        assert_eq!(
            session.media_send_counter - counter_before,
            (UDP_OVER_TCP_RECOVERY_DELAYS.len() * 2) as u64
        );
        session.poll_udp_over_tcp_recovery(now + Duration::from_secs(300));
        assert_eq!(
            session.media_send_counter - counter_before,
            (UDP_OVER_TCP_RECOVERY_DELAYS.len() * 2) as u64
        );
        assert!(!event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Unavailable,
                },
                ..
            }
        )));

        // A network-interface rebind calls this same reset path and gets a
        // fresh bounded schedule after the old one parked.
        let restart_at = now + Duration::from_secs(301);
        session.begin_udp_verification(restart_at);
        let restarted_delay = session
            .udp_over_tcp_recovery
            .next_attempt()
            .unwrap()
            .duration_since(restart_at);
        assert!(restarted_delay >= Duration::from_millis(900));
        assert!(restarted_delay <= Duration::from_millis(1_100));
    }

    #[test]
    fn udp_over_tcp_recovery_uses_jittered_bounded_backoff() {
        let start = Instant::now();
        let mut recovery = UdpOverTcpRecovery::new(0x1234_5678_9abc_def0);
        recovery.arm(start);
        let mut previous = start;

        for base in UDP_OVER_TCP_RECOVERY_DELAYS {
            let deadline = recovery.next_attempt().expect("attempt was scheduled");
            let delay = deadline.duration_since(previous);
            let millis = base.as_millis() as u64;
            assert!(delay >= Duration::from_millis(millis * 9 / 10));
            assert!(delay <= Duration::from_millis(millis * 11 / 10));
            assert!(recovery.take_due(deadline));
            previous = deadline;
        }

        assert!(recovery.next_attempt().is_none());
        assert!(!recovery.take_due(previous + Duration::from_secs(300)));

        recovery.arm(previous);
        let fast_delay = recovery.next_attempt().unwrap().duration_since(previous);
        assert!(fast_delay >= Duration::from_millis(900));
        assert!(fast_delay <= Duration::from_millis(1_100));
    }

    #[test]
    fn matched_udp_pong_restarts_fast_recovery_over_tcp() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        session.suspect_udp_path(Instant::now());
        session.poll_tcp_lane(session.next_tcp_connect);
        let (_server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);
        session.note_server_media(MediaPath::Tcp);

        let first = session.udp_over_tcp_recovery.next_attempt().unwrap();
        assert!(session.udp_over_tcp_recovery.take_due(first));
        let backed_off = session.udp_over_tcp_recovery.next_attempt().unwrap();
        assert!(backed_off.duration_since(first) >= Duration::from_millis(1_800));

        let response_at = first + Duration::from_millis(10);
        let nonce = 0xf00d;
        session
            .udp_probes
            .sent(nonce, response_at - Duration::from_millis(5));
        session.observe_pong(response_at, MediaPath::Udp, nonce);

        let fast_delay = session
            .udp_over_tcp_recovery
            .next_attempt()
            .unwrap()
            .duration_since(response_at);
        assert!(fast_delay >= Duration::from_millis(900));
        assert!(fast_delay <= Duration::from_millis(1_100));
        assert_eq!(session.udp_path, UdpPath::Failed);
    }

    /// `UdpBound` travels over the control connection, so on its own it proves
    /// only that the client's datagrams arrive, and inbound speech proves only
    /// the reverse direction. Recovery has to wait for round trips that close,
    /// and for enough of them that one transient success cannot flap the lane.
    #[test]
    fn udp_recovery_needs_repeated_matched_pongs() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, event_rx) =
            lane_test_session(MediaTransportSetting::Auto, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        session.suspect_udp_path(Instant::now());
        session.poll_tcp_lane(session.next_tcp_connect);
        let (server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);
        session.note_server_media(MediaPath::Tcp);
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Tcp,
                },
                ..
            }
        )));

        session.udp_bound();
        assert_eq!(session.udp_path, UdpPath::Failed);
        assert!(!event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Udp,
                },
                ..
            }
        )));
        assert!(matches!(
            session.tcp_lane.as_ref().unwrap().state,
            TcpLaneState::Active
        ));

        // A room's inbound speech arriving over UDP says nothing about the
        // direction the microphone travels.
        session.handle_server_media(
            Instant::now(),
            MediaPath::Udp,
            MediaPayload::Voice {
                stream_id: StreamId(4),
                sequence: 1,
                timestamp: 960,
                flags: 0,
                payload: media::VoicePayload::Opus(vec![1, 2, 3]),
            },
        );
        assert_eq!(session.udp_path, UdpPath::Failed);

        for round in 1..crate::client_net::UDP_VERIFY_SUCCESSES {
            let probe_at = session.udp_over_tcp_recovery.next_attempt().unwrap();
            session.poll_udp_over_tcp_recovery(probe_at);
            answer_probe_at(
                &mut session,
                MediaPath::Udp,
                probe_at + Duration::from_millis(10),
            );
            assert_eq!(session.udp_probes.streak, round);
            assert_eq!(session.udp_path, UdpPath::Failed, "round {round}");
            assert!(matches!(
                session.tcp_lane.as_ref().unwrap().state,
                TcpLaneState::Active
            ));
        }

        let probe_at = session.udp_over_tcp_recovery.next_attempt().unwrap();
        session.poll_udp_over_tcp_recovery(probe_at);
        answer_probe_at(
            &mut session,
            MediaPath::Udp,
            probe_at + Duration::from_millis(10),
        );
        assert_eq!(session.udp_path, UdpPath::Verified);
        assert!(matches!(
            session.tcp_lane.as_ref().unwrap().state,
            TcpLaneState::Draining { .. }
        ));
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Udp,
                },
                ..
            }
        )));

        drop(server_end);
        let deadline = Instant::now() + Duration::from_secs(1);
        while session.tcp_lane.is_some() {
            assert!(Instant::now() < deadline, "drained lane failed to close");
            if let Some(lane) = session.tcp_lane.as_mut() {
                lane.readiness.mark_ready();
            }
            session.read_tcp_lane();
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn force_tcp_lane_loss_reports_unavailable_until_reconfirmed() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, event_rx) =
            lane_test_session(MediaTransportSetting::Tcp, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Unavailable,
                },
                ..
            }
        )));

        session.poll_tcp_lane(Instant::now());
        let (server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);
        session.note_server_media(MediaPath::Tcp);
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Tcp,
                },
                ..
            }
        )));

        drop(server_end);
        session.tcp_lane.as_mut().unwrap().readiness.mark_ready();
        session.read_tcp_lane();
        assert!(session.tcp_lane.is_none());
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Unavailable,
                },
                ..
            }
        )));

        let counter = session.media_send_counter;
        session.send_media(&MediaPayload::Voice {
            stream_id: StreamId(9),
            sequence: 12,
            timestamp: 960,
            flags: 0,
            payload: media::VoicePayload::Opus(vec![1, 2, 3]),
        });
        assert_eq!(session.media_send_counter, counter);

        session.poll_tcp_lane(session.next_tcp_connect);
        let (_server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);
        assert!(!event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Tcp,
                },
                ..
            }
        )));
        session.note_server_media(MediaPath::Tcp);
        assert!(event_rx.try_iter().any(|event| matches!(
            event,
            crate::app::AppEvent::NetworkFor {
                event: NetworkEvent::MediaTransport {
                    state: MediaTransportState::Tcp,
                },
                ..
            }
        )));
    }

    /// The server confirms the bind over the control connection, so a session
    /// whose inbound UDP is dropped never gets a first RTT sample at all. The
    /// liveness window has to run from authentication for that to be caught.
    #[test]
    fn unanswered_probes_suspect_udp_without_any_rtt_sample() {
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, "127.0.0.1:9".parse().unwrap());
        session.authenticated(SessionId(6));
        session.udp_bound();
        assert_eq!(session.udp_path, UdpPath::Unproven);
        assert!(session.server_rtt_last_sample_at.is_none());

        let now = session.udp_probes.pending_since.unwrap() + crate::client_net::RTT_STALE_AFTER;
        session.poll_rtt_probe(now);
        session.poll_server_path_liveness(now);
        assert_eq!(session.udp_path, UdpPath::Failed);
        assert!(session.lane_wanted());
    }

    /// The failure this whole scheme exists for: in a busy room a client whose
    /// upstream UDP is blackholed keeps hearing everyone, so any rule that
    /// treats inbound traffic as liveness never fires and the microphone stays
    /// silently dead.
    #[test]
    fn inbound_voice_does_not_stand_in_for_a_matched_pong() {
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, "127.0.0.1:9".parse().unwrap());
        session.authenticated(SessionId(6));
        session.udp_bound();
        let opened_at = session.udp_probes.pending_since.unwrap();

        let mut now = Instant::now();
        for _ in 0..8 {
            now += Duration::from_secs(2);
            session.handle_server_media(
                now,
                MediaPath::Udp,
                MediaPayload::Voice {
                    stream_id: StreamId(4),
                    sequence: 1,
                    timestamp: 960,
                    flags: 0,
                    payload: media::VoicePayload::Opus(vec![1, 2, 3]),
                },
            );
            session.note_server_media(MediaPath::Udp);
        }
        assert_eq!(session.udp_probes.pending_since, Some(opened_at));

        let now = opened_at + crate::client_net::RTT_STALE_AFTER;
        session.poll_server_path_liveness(now);
        assert_eq!(session.udp_path, UdpPath::Failed);
        assert!(session.lane_wanted());
    }

    #[test]
    fn a_matched_pong_clears_the_liveness_window() {
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, "127.0.0.1:9".parse().unwrap());
        session.authenticated(SessionId(6));
        session.udp_bound();
        assert!(session.udp_probes.pending_since.is_some());
        answer_probe(&mut session, MediaPath::Udp);
        assert!(session.udp_probes.pending_since.is_none());

        let now = Instant::now() + crate::client_net::RTT_STALE_AFTER;
        session.poll_server_path_liveness(now);
        assert_eq!(session.udp_path, UdpPath::Verified);
    }

    /// A `Pong` proves the path it came back on. One arriving over the lane for
    /// a probe UDP sent would otherwise let a dead UDP path claim to be alive.
    #[test]
    fn a_pong_on_the_wrong_path_or_nonce_proves_nothing() {
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, "127.0.0.1:9".parse().unwrap());
        session.authenticated(SessionId(6));
        let nonce = session.udp_probes.in_flight.front().unwrap().0;
        let opened_at = session.udp_probes.pending_since.unwrap();

        let now = Instant::now();
        session.handle_server_media(now, MediaPath::Tcp, MediaPayload::Pong { nonce });
        session.handle_server_media(
            now,
            MediaPath::Udp,
            MediaPayload::Pong {
                nonce: nonce.wrapping_add(1000),
            },
        );

        assert_eq!(session.udp_probes.pending_since, Some(opened_at));
        assert_eq!(session.udp_probes.streak, 0);
        assert_eq!(session.udp_path, UdpPath::Unproven);
    }

    /// Cold start is the same problem as a mid-session failure: nothing has
    /// proven UDP yet, so speech must not sit on it for the whole liveness
    /// window. One round trip settles it, so the lane is only worth opening if
    /// that round trip does not come back.
    #[test]
    fn cold_start_opens_a_lane_only_when_udp_stays_unproven() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, listener.local_addr().unwrap());
        let start = Instant::now();
        session.authenticated(SessionId(6));
        assert!(session.lane_wanted());

        assert!(session.next_tcp_connect >= start + crate::client_net::UDP_COLD_START_GRACE);
        session.poll_tcp_lane(start);
        assert!(session.tcp_lane.is_none(), "lane opened inside the grace");

        session.poll_tcp_lane(session.next_tcp_connect);
        assert!(session.tcp_lane.is_some());
    }

    #[test]
    fn cold_start_verifies_udp_within_the_grace_without_a_lane() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, listener.local_addr().unwrap());
        let start = Instant::now();
        session.authenticated(SessionId(6));
        session.udp_bound();
        answer_probe(&mut session, MediaPath::Udp);

        assert_eq!(session.udp_path, UdpPath::Verified);
        assert!(!session.lane_wanted());
        session.poll_tcp_lane(start + crate::client_net::UDP_COLD_START_GRACE);
        assert!(session.tcp_lane.is_none());
    }

    /// One lost `Pong` on a working path must not cost a lane. An outstanding
    /// probe escalates the cadence to a second, so the fifteen-second window
    /// closes on the follow-up instead of expiring after three sparse asks.
    #[test]
    fn a_single_lost_pong_escalates_probing_instead_of_failing_over() {
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, "127.0.0.1:9".parse().unwrap());
        session.authenticated(SessionId(6));
        session.udp_bound();
        answer_probe(&mut session, MediaPath::Udp);
        assert_eq!(session.udp_path, UdpPath::Verified);
        assert!(!session.udp_verify_wanted());

        // The periodic probe goes out over UDP and its answer is lost.
        let mut now = session.next_rtt_probe;
        session.poll_rtt_probe(now);
        assert!(session.udp_verify_wanted());
        assert_eq!(
            session.next_udp_verify,
            now + crate::client_net::UDP_VERIFY_INTERVAL
        );

        now += crate::client_net::UDP_VERIFY_INTERVAL;
        session.poll_udp_verify(now);
        now += crate::client_net::UDP_VERIFY_INTERVAL;
        session.poll_udp_verify(now);
        answer_probe(&mut session, MediaPath::Udp);

        assert!(session.udp_probes.pending_since.is_none());
        session.poll_server_path_liveness(now + crate::client_net::RTT_STALE_AFTER);
        assert_eq!(session.udp_path, UdpPath::Verified);
        assert!(!session.lane_wanted());
    }

    /// A probe that goes unanswered restarts the dwell, so recovery cannot be
    /// assembled out of successes spread across an unhealthy path.
    #[test]
    fn a_missed_probe_resets_the_recovery_streak() {
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, "127.0.0.1:9".parse().unwrap());
        session.authenticated(SessionId(6));
        session.udp_bound();
        session.latch_udp_suspect();

        let mut now = Instant::now();
        now += crate::client_net::UDP_VERIFY_INTERVAL;
        session.poll_udp_verify(now);
        answer_probe(&mut session, MediaPath::Udp);
        assert_eq!(session.udp_probes.streak, 1);

        now += crate::client_net::UDP_VERIFY_INTERVAL;
        session.poll_udp_verify(now);
        now += crate::client_net::UDP_VERIFY_TIMEOUT;
        session.poll_udp_verify(now);
        assert_eq!(session.udp_probes.streak, 0);
        assert_eq!(session.udp_path, UdpPath::Failed);
    }

    /// The byte cap bounds memory, not latency: 32 KiB is seconds of speech, so
    /// a lane whose queue stops draining has to be closed on age too.
    #[test]
    fn stale_backlog_fails_a_lane_far_under_the_write_cap() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Tcp, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        session.poll_tcp_lane(Instant::now());
        let (_server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);

        let lane = session.tcp_lane.as_mut().unwrap();
        lane.write_blocked = true;
        lane.write_queue.tail_mut().extend_from_slice(&[0; 64]);
        lane.backlog_since = Some(Instant::now() - TCP_LANE_STALE_BACKLOG);
        assert!(lane.write_queue.len() < TCP_LANE_WRITE_CAP_BYTES);

        session.send_media(&MediaPayload::Pong { nonce: 3 });
        assert!(session.tcp_lane.is_none());
    }

    #[test]
    fn a_drained_lane_queue_closes_the_backlog_window() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Tcp, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        session.poll_tcp_lane(Instant::now());
        let (_server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);

        // Activation queues the magic and a Bind, both flushed to the socket.
        assert_eq!(session.tcp_lane.as_ref().unwrap().backlog_since, None);

        session.tcp_lane.as_mut().unwrap().write_blocked = true;
        session.send_media(&MediaPayload::Pong { nonce: 3 });
        assert!(session.tcp_lane.as_ref().unwrap().backlog_since.is_some());

        session.tcp_lane.as_mut().unwrap().write_blocked = false;
        session.flush_tcp_lane();
        assert_eq!(session.tcp_lane.as_ref().unwrap().backlog_since, None);
    }

    /// A silently blackholed lane holds a force-TCP session hostage until the
    /// OS TCP timeout, which an idle session never reaches on its own.
    #[test]
    fn silent_tcp_lane_is_dropped_and_reconnected() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Tcp, listener.local_addr().unwrap());
        session.authenticated(SessionId(6));
        session.poll_tcp_lane(Instant::now());
        let (_server_end, _) = listener.accept().unwrap();
        drive_lane_active(&mut session);
        assert!(session.tcp_probes.pending_since.is_some());

        let now = session.tcp_probes.pending_since.unwrap() + crate::client_net::RTT_STALE_AFTER;
        session.poll_server_path_liveness(now);
        assert!(session.tcp_lane.is_none());
        assert_eq!(session.next_tcp_connect, now + TCP_LANE_BACKOFF_MIN);
        assert!(session.lane_wanted());
    }

    #[test]
    fn tcp_reconnect_backoff_doubles_to_cap() {
        let (mut session, _poll, _event_rx) =
            lane_test_session(MediaTransportSetting::Auto, "127.0.0.1:9".parse().unwrap());
        let now = Instant::now();
        session.schedule_tcp_reconnect(now);
        assert_eq!(session.next_tcp_connect, now + TCP_LANE_BACKOFF_MIN);
        session.schedule_tcp_reconnect(now);
        assert_eq!(session.next_tcp_connect, now + TCP_LANE_BACKOFF_MIN * 2);
        for _ in 0..10 {
            session.schedule_tcp_reconnect(now);
        }
        assert_eq!(session.next_tcp_connect, now + TCP_LANE_BACKOFF_MAX);
    }
}
