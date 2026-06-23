use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    fmt,
    time::{Duration, Instant},
};

use crate::audio::{VoicePayload, VoicePayloadRef};

pub(crate) const PROTOCOL_VERSION: u8 = 3;
pub(crate) const AUDIO_HEADER_LEN: usize = 7;
pub(crate) const FEEDBACK_PACKET_LEN: usize = 16;
pub(crate) const SAFE_UDP_PAYLOAD_BYTES: usize = 1_200;
pub(crate) const MAX_AUDIO_PAYLOAD_BYTES: usize = SAFE_UDP_PAYLOAD_BYTES - AUDIO_HEADER_LEN;

const VERSION_SHIFT: u8 = 4;
const KIND_MASK: u8 = 0x0f;
const KIND_AUDIO: u8 = 0;
const KIND_FEEDBACK: u8 = 1;
const AUDIO_PAYLOAD_OPUS: u8 = 0;
const AUDIO_PAYLOAD_SILENCE: u8 = 1;
const DEFAULT_MAX_BUFFERED_PACKETS: usize = 128;
const DEFAULT_MAX_REORDER_DELAY: Duration = Duration::from_millis(60);
const DEFAULT_MAX_LOSS_FILL_PACKETS: u32 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolError {
    DatagramTooShort,
    UnsupportedVersion(u8),
    UnknownPacketKind(u8),
    InvalidPayload,
    EmptyAudioPayload,
    AudioPayloadTooLarge,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::DatagramTooShort => f.write_str("datagram is too short"),
            ProtocolError::UnsupportedVersion(version) => {
                write!(f, "unsupported protocol version {version}")
            }
            ProtocolError::UnknownPacketKind(kind) => write!(f, "unknown packet kind {kind}"),
            ProtocolError::InvalidPayload => f.write_str("invalid audio datagram payload"),
            ProtocolError::EmptyAudioPayload => f.write_str("audio datagram has no Opus payload"),
            ProtocolError::AudioPayloadTooLarge => {
                f.write_str("audio payload exceeds safe UDP datagram budget")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AudioPacketRef<'a> {
    pub sequence: u32,
    pub flags: u8,
    pub payload: VoicePayloadRef<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TelemetryFeedback {
    pub highest_contiguous_sequence: u32,
    pub expected_packets: u16,
    pub lost_packets: u16,
    pub late_or_duplicate_packets: u16,
    pub rtt_ms: u16,
    pub window_ms: u16,
}

impl TelemetryFeedback {
    pub(crate) fn loss_fraction(self) -> f32 {
        if self.expected_packets == 0 {
            0.0
        } else {
            self.lost_packets as f32 / self.expected_packets as f32
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DatagramRef<'a> {
    Audio(AudioPacketRef<'a>),
    Feedback(TelemetryFeedback),
}

pub(crate) fn encode_audio_datagram(
    sequence: u32,
    flags: u8,
    payload: VoicePayloadRef<'_>,
    out: &mut Vec<u8>,
) -> Result<(), ProtocolError> {
    if let VoicePayloadRef::Opus(opus) = payload {
        if opus.is_empty() {
            return Err(ProtocolError::EmptyAudioPayload);
        }
        if opus.len() > MAX_AUDIO_PAYLOAD_BYTES {
            return Err(ProtocolError::AudioPayloadTooLarge);
        }
    }

    out.clear();
    out.reserve(AUDIO_HEADER_LEN + payload.len());
    out.push(packet_tag(KIND_AUDIO));
    out.push(flags);
    out.extend_from_slice(&sequence.to_le_bytes());
    match payload {
        VoicePayloadRef::Opus(opus) => {
            out.push(AUDIO_PAYLOAD_OPUS);
            out.extend_from_slice(opus);
        }
        VoicePayloadRef::Silence => {
            out.push(AUDIO_PAYLOAD_SILENCE);
        }
    }
    Ok(())
}

pub(crate) fn encode_feedback_datagram(feedback: TelemetryFeedback, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(FEEDBACK_PACKET_LEN);
    out.push(packet_tag(KIND_FEEDBACK));
    out.push(0);
    out.extend_from_slice(&feedback.highest_contiguous_sequence.to_le_bytes());
    out.extend_from_slice(&feedback.expected_packets.to_le_bytes());
    out.extend_from_slice(&feedback.lost_packets.to_le_bytes());
    out.extend_from_slice(&feedback.late_or_duplicate_packets.to_le_bytes());
    out.extend_from_slice(&feedback.rtt_ms.to_le_bytes());
    out.extend_from_slice(&feedback.window_ms.to_le_bytes());
}

pub(crate) fn parse_datagram(bytes: &[u8]) -> Result<DatagramRef<'_>, ProtocolError> {
    if bytes.len() < 2 {
        return Err(ProtocolError::DatagramTooShort);
    }

    let version = bytes[0] >> VERSION_SHIFT;
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }

    match bytes[0] & KIND_MASK {
        KIND_AUDIO => parse_audio_datagram(bytes).map(DatagramRef::Audio),
        KIND_FEEDBACK => parse_feedback_datagram(bytes).map(DatagramRef::Feedback),
        kind => Err(ProtocolError::UnknownPacketKind(kind)),
    }
}

fn parse_audio_datagram(bytes: &[u8]) -> Result<AudioPacketRef<'_>, ProtocolError> {
    if bytes.len() < AUDIO_HEADER_LEN {
        return Err(ProtocolError::DatagramTooShort);
    }
    let payload = match bytes[6] {
        AUDIO_PAYLOAD_OPUS => {
            let payload = &bytes[AUDIO_HEADER_LEN..];
            if payload.is_empty() {
                return Err(ProtocolError::EmptyAudioPayload);
            }
            VoicePayloadRef::Opus(payload)
        }
        AUDIO_PAYLOAD_SILENCE => {
            if bytes.len() != AUDIO_HEADER_LEN {
                return Err(ProtocolError::InvalidPayload);
            }
            VoicePayloadRef::Silence
        }
        _ => return Err(ProtocolError::InvalidPayload),
    };

    Ok(AudioPacketRef {
        sequence: u32::from_le_bytes(bytes[2..6].try_into().unwrap()),
        flags: bytes[1],
        payload,
    })
}

fn parse_feedback_datagram(bytes: &[u8]) -> Result<TelemetryFeedback, ProtocolError> {
    if bytes.len() < FEEDBACK_PACKET_LEN {
        return Err(ProtocolError::DatagramTooShort);
    }

    Ok(TelemetryFeedback {
        highest_contiguous_sequence: u32::from_le_bytes(bytes[2..6].try_into().unwrap()),
        expected_packets: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
        lost_packets: u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        late_or_duplicate_packets: u16::from_le_bytes(bytes[10..12].try_into().unwrap()),
        rtt_ms: u16::from_le_bytes(bytes[12..14].try_into().unwrap()),
        window_ms: u16::from_le_bytes(bytes[14..16].try_into().unwrap()),
    })
}

fn packet_tag(kind: u8) -> u8 {
    (PROTOCOL_VERSION << VERSION_SHIFT) | kind
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JitterBufferConfig {
    pub max_reorder_delay: Duration,
    pub max_buffered_packets: usize,
    pub max_loss_fill_packets: u32,
}

impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            max_reorder_delay: DEFAULT_MAX_REORDER_DELAY,
            max_buffered_packets: DEFAULT_MAX_BUFFERED_PACKETS,
            max_loss_fill_packets: DEFAULT_MAX_LOSS_FILL_PACKETS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InsertOutcome {
    Accepted,
    Duplicate,
    Late,
    BufferFull,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlayoutItem {
    Audio {
        sequence: u32,
        flags: u8,
        payload: VoicePayload,
    },
    Missing {
        sequence: u32,
    },
    FastForward {
        from_sequence: u32,
        to_sequence: u32,
        skipped_packets: u32,
    },
}

impl PlayoutItem {
    pub(crate) fn sequence(&self) -> u32 {
        match self {
            PlayoutItem::Audio { sequence, .. } | PlayoutItem::Missing { sequence } => *sequence,
            PlayoutItem::FastForward { from_sequence, .. } => *from_sequence,
        }
    }
}

#[derive(Clone, Debug)]
struct BufferedAudioPacket {
    sequence: u32,
    flags: u8,
    payload: VoicePayload,
}

#[derive(Clone, Debug)]
pub(crate) struct JitterBuffer {
    config: JitterBufferConfig,
    next_sequence: Option<u32>,
    missing_since: Option<Instant>,
    buffered: Vec<BufferedAudioPacket>,
}

impl JitterBuffer {
    pub(crate) fn new(config: JitterBufferConfig) -> Self {
        Self {
            config,
            next_sequence: None,
            missing_since: None,
            buffered: Vec::with_capacity(config.max_buffered_packets.min(256)),
        }
    }

    pub(crate) fn insert(&mut self, packet: AudioPacketRef<'_>) -> InsertOutcome {
        if let Some(next_sequence) = self.next_sequence {
            match sequence_distance(next_sequence, packet.sequence) {
                Some(0) => {}
                Some(_) => {}
                None => return InsertOutcome::Late,
            }
        }

        if self
            .buffered
            .iter()
            .any(|buffered| buffered.sequence == packet.sequence)
        {
            return InsertOutcome::Duplicate;
        }

        if self.buffered.len() >= self.config.max_buffered_packets {
            return InsertOutcome::BufferFull;
        }

        self.buffered.push(BufferedAudioPacket {
            sequence: packet.sequence,
            flags: packet.flags,
            payload: packet.payload.to_owned(),
        });
        InsertOutcome::Accepted
    }

    pub(crate) fn drain_ready(&mut self, now: Instant) -> Vec<PlayoutItem> {
        let mut ready = Vec::new();
        self.initialize_next_sequence();

        loop {
            let Some(next_sequence) = self.next_sequence else {
                break;
            };

            if let Some(position) = self
                .buffered
                .iter()
                .position(|packet| packet.sequence == next_sequence)
            {
                let packet = self.buffered.swap_remove(position);
                self.next_sequence = Some(next_sequence.wrapping_add(1));
                self.missing_since = None;
                ready.push(PlayoutItem::Audio {
                    sequence: packet.sequence,
                    flags: packet.flags,
                    payload: packet.payload,
                });
                continue;
            }

            let Some(distance_to_next_buffered) = self.closest_future_distance(next_sequence)
            else {
                self.missing_since = None;
                break;
            };

            if distance_to_next_buffered == 0 {
                continue;
            }

            let missing_since = *self.missing_since.get_or_insert(now);
            if now.duration_since(missing_since) < self.config.max_reorder_delay {
                break;
            }

            if distance_to_next_buffered > self.config.max_loss_fill_packets {
                let to_sequence = next_sequence.wrapping_add(distance_to_next_buffered);
                self.next_sequence = Some(to_sequence);
                self.missing_since = None;
                ready.push(PlayoutItem::FastForward {
                    from_sequence: next_sequence,
                    to_sequence,
                    skipped_packets: distance_to_next_buffered,
                });
                continue;
            }

            self.next_sequence = Some(next_sequence.wrapping_add(1));
            ready.push(PlayoutItem::Missing {
                sequence: next_sequence,
            });
        }

        ready
    }

    pub(crate) fn consume_silence_sequence(&mut self, sequence: u32) {
        let Some(next_sequence) = self.next_sequence else {
            return;
        };
        if sequence_distance(next_sequence, sequence).is_none() {
            return;
        }
        self.buffered
            .retain(|packet| sequence_distance(packet.sequence, sequence).is_none());
        self.next_sequence = Some(sequence.wrapping_add(1));
        self.missing_since = None;
    }

    pub(crate) fn skip_to_sequence(&mut self, sequence: u32) {
        let Some(next_sequence) = self.next_sequence else {
            return;
        };
        let Some(distance) = sequence_distance(next_sequence, sequence) else {
            return;
        };
        if distance == 0 {
            return;
        }
        let previous = sequence.wrapping_sub(1);
        self.buffered
            .retain(|packet| sequence_distance(packet.sequence, previous).is_none());
        self.next_sequence = Some(sequence);
        self.missing_since = None;
    }

    fn initialize_next_sequence(&mut self) {
        if self.next_sequence.is_some() || self.buffered.is_empty() {
            return;
        }

        let sequence = self
            .buffered
            .iter()
            .map(|packet| packet.sequence)
            .min()
            .unwrap();
        self.next_sequence = Some(sequence);
    }

    fn closest_future_distance(&self, sequence: u32) -> Option<u32> {
        self.buffered
            .iter()
            .filter_map(|packet| sequence_distance(sequence, packet.sequence))
            .filter(|distance| *distance > 0)
            .min()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TelemetryWindow {
    highest_contiguous_sequence: Option<u32>,
    expected_packets: u32,
    lost_packets: u32,
    late_or_duplicate_packets: u32,
}

impl TelemetryWindow {
    pub(crate) fn observe_insert(&mut self, outcome: &InsertOutcome) {
        if matches!(outcome, InsertOutcome::Duplicate | InsertOutcome::Late) {
            self.late_or_duplicate_packets = self.late_or_duplicate_packets.saturating_add(1);
        }
    }

    pub(crate) fn observe_playout(&mut self, item: &PlayoutItem) {
        match *item {
            PlayoutItem::Audio { sequence, .. } => {
                self.expected_packets = self.expected_packets.saturating_add(1);
                self.highest_contiguous_sequence = Some(sequence);
            }
            PlayoutItem::Missing { sequence } => {
                self.expected_packets = self.expected_packets.saturating_add(1);
                self.lost_packets = self.lost_packets.saturating_add(1);
                self.highest_contiguous_sequence = Some(sequence);
            }
            PlayoutItem::FastForward {
                to_sequence,
                skipped_packets,
                ..
            } => {
                self.expected_packets = self.expected_packets.saturating_add(skipped_packets);
                self.lost_packets = self.lost_packets.saturating_add(skipped_packets);
                self.highest_contiguous_sequence = Some(to_sequence.wrapping_sub(1));
            }
        }
    }

    pub(crate) fn build_feedback(&mut self, rtt_ms: u16, window_ms: u16) -> TelemetryFeedback {
        let feedback = TelemetryFeedback {
            highest_contiguous_sequence: self.highest_contiguous_sequence.unwrap_or(0),
            expected_packets: clamp_u16(self.expected_packets),
            lost_packets: clamp_u16(self.lost_packets),
            late_or_duplicate_packets: clamp_u16(self.late_or_duplicate_packets),
            rtt_ms,
            window_ms,
        };
        self.expected_packets = 0;
        self.lost_packets = 0;
        self.late_or_duplicate_packets = 0;
        feedback
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EncoderNetworkProfile {
    pub dred_duration_10ms: i32,
    pub bitrate_bps: i32,
    pub packet_loss_percent: i32,
}

impl EncoderNetworkProfile {
    pub(crate) const EXCELLENT: Self = Self {
        dred_duration_10ms: 0,
        bitrate_bps: 32_000,
        packet_loss_percent: 0,
    };
    // DRED needs spare bits beyond the VOIP core. Below ~40 kbps the core
    // consumes the whole budget and DRED reach collapses to under one frame,
    // making recovery impossible. The lossy profiles run at 48-64 kbps so each
    // packet carries multiple frames of usable redundancy.
    pub(crate) const DEGRADED: Self = Self {
        dred_duration_10ms: 100,
        bitrate_bps: 48_000,
        packet_loss_percent: 3,
    };
    pub(crate) const SEVERE: Self = Self {
        dred_duration_10ms: 100,
        bitrate_bps: 64_000,
        packet_loss_percent: 10,
    };
    pub(crate) const CRITICAL: Self = Self {
        dred_duration_10ms: 100,
        bitrate_bps: 64_000,
        packet_loss_percent: 20,
    };
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DynamicOverheadController {
    smoothed_loss: f32,
    alpha: f32,
    current_profile: EncoderNetworkProfile,
}

impl DynamicOverheadController {
    pub(crate) fn new(alpha: f32) -> Self {
        Self {
            smoothed_loss: 0.0,
            alpha: alpha.clamp(0.0, 1.0),
            current_profile: EncoderNetworkProfile::EXCELLENT,
        }
    }

    pub(crate) fn handle_feedback(&mut self, feedback: TelemetryFeedback) -> EncoderNetworkProfile {
        let loss_fraction = feedback.loss_fraction().clamp(0.0, 1.0);
        self.smoothed_loss = self.alpha * loss_fraction + (1.0 - self.alpha) * self.smoothed_loss;
        self.current_profile = profile_for_loss(self.smoothed_loss);
        self.current_profile
    }

    pub(crate) fn smoothed_loss(self) -> f32 {
        self.smoothed_loss
    }

    pub(crate) fn current_profile(self) -> EncoderNetworkProfile {
        self.current_profile
    }
}

pub(crate) trait EncoderNetworkTuning {
    type Error;

    fn apply_network_profile(&mut self, profile: EncoderNetworkProfile) -> Result<(), Self::Error>;
}

pub(crate) fn apply_feedback_to_encoder<T: EncoderNetworkTuning>(
    controller: &mut DynamicOverheadController,
    feedback: TelemetryFeedback,
    encoder: &mut T,
) -> Result<EncoderNetworkProfile, T::Error> {
    let profile = controller.handle_feedback(feedback);
    encoder.apply_network_profile(profile)?;
    Ok(profile)
}

fn profile_for_loss(loss: f32) -> EncoderNetworkProfile {
    if loss < 0.01 {
        EncoderNetworkProfile::EXCELLENT
    } else if loss < 0.05 {
        EncoderNetworkProfile::DEGRADED
    } else if loss < 0.15 {
        EncoderNetworkProfile::SEVERE
    } else {
        EncoderNetworkProfile::CRITICAL
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum GilbertState {
    Good,
    Bad,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct NetworkSimulatorConfig {
    pub good_to_bad_probability: f64,
    pub bad_to_good_probability: f64,
    pub loss_good: f64,
    pub loss_bad: f64,
    pub base_delay: Duration,
    pub jitter: Duration,
    pub duplicate_rate: f64,
    pub seed: u64,
}

impl NetworkSimulatorConfig {
    pub(crate) fn new(base_delay: Duration, jitter: Duration) -> Self {
        Self {
            good_to_bad_probability: 0.05,
            bad_to_good_probability: 0.30,
            loss_good: 0.005,
            loss_bad: 0.50,
            base_delay,
            jitter,
            duplicate_rate: 0.02,
            seed: 0x746f_6d63_6861_7401,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NetworkSimulatorStats {
    pub sent: u64,
    pub dropped: u64,
    pub duplicated: u64,
    pub delivered: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct NetworkSimulator {
    state: GilbertState,
    config: NetworkSimulatorConfig,
    rng: DeterministicRng,
    queue: BinaryHeap<DelayedDatagram>,
    next_serial: u64,
    stats: NetworkSimulatorStats,
}

impl NetworkSimulator {
    pub(crate) fn new(config: NetworkSimulatorConfig) -> Self {
        Self {
            state: GilbertState::Good,
            rng: DeterministicRng::new(config.seed),
            config,
            queue: BinaryHeap::new(),
            next_serial: 0,
            stats: NetworkSimulatorStats::default(),
        }
    }

    pub(crate) fn state(&self) -> GilbertState {
        self.state
    }

    pub(crate) fn stats(&self) -> NetworkSimulatorStats {
        self.stats
    }

    pub(crate) fn send_datagram(&mut self, bytes: &[u8], now: Instant) {
        self.stats.sent = self.stats.sent.saturating_add(1);
        self.transition_state();

        if self.rng.next_f64() < self.current_loss_probability() {
            self.stats.dropped = self.stats.dropped.saturating_add(1);
            return;
        }

        let deliver_at = now + self.delivery_delay();
        self.push_delayed(bytes.to_vec(), deliver_at);

        if self.rng.next_f64() < self.config.duplicate_rate {
            self.stats.duplicated = self.stats.duplicated.saturating_add(1);
            self.push_delayed(bytes.to_vec(), deliver_at + Duration::from_millis(5));
        }
    }

    pub(crate) fn receive_datagrams(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut ready = Vec::new();
        while self
            .queue
            .peek()
            .is_some_and(|packet| now >= packet.deliver_at)
        {
            let packet = self.queue.pop().unwrap();
            self.stats.delivered = self.stats.delivered.saturating_add(1);
            ready.push(packet.bytes);
        }
        ready
    }

    fn transition_state(&mut self) {
        let sample = self.rng.next_f64();
        self.state = match self.state {
            GilbertState::Good if sample < self.config.good_to_bad_probability => GilbertState::Bad,
            GilbertState::Bad if sample < self.config.bad_to_good_probability => GilbertState::Good,
            state => state,
        };
    }

    fn current_loss_probability(&self) -> f64 {
        match self.state {
            GilbertState::Good => self.config.loss_good,
            GilbertState::Bad => self.config.loss_bad,
        }
        .clamp(0.0, 1.0)
    }

    fn delivery_delay(&mut self) -> Duration {
        let jitter_span = self.config.jitter.as_secs_f64();
        let jitter_offset = (self.rng.next_f64() * 2.0 - 1.0) * jitter_span;
        let seconds = (self.config.base_delay.as_secs_f64() + jitter_offset).max(0.001);
        Duration::from_secs_f64(seconds)
    }

    fn push_delayed(&mut self, bytes: Vec<u8>, deliver_at: Instant) {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        self.queue.push(DelayedDatagram {
            deliver_at,
            serial,
            bytes,
        });
    }
}

#[derive(Clone, Debug)]
struct DelayedDatagram {
    deliver_at: Instant,
    serial: u64,
    bytes: Vec<u8>,
}

impl Eq for DelayedDatagram {}

impl PartialEq for DelayedDatagram {
    fn eq(&self, other: &Self) -> bool {
        self.deliver_at == other.deliver_at && self.serial == other.serial
    }
}

impl Ord for DelayedDatagram {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .deliver_at
            .cmp(&self.deliver_at)
            .then_with(|| other.serial.cmp(&self.serial))
    }
}

impl PartialOrd for DelayedDatagram {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug)]
struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn next_f64(&mut self) -> f64 {
        const DENOMINATOR: f64 = (1u64 << 53) as f64;
        ((self.next_u64() >> 11) as f64) / DENOMINATOR
    }
}

fn sequence_distance(from: u32, to: u32) -> Option<u32> {
    let distance = to.wrapping_sub(from);
    if distance < (1 << 31) {
        Some(distance)
    } else {
        None
    }
}

fn clamp_u16(value: u32) -> u16 {
    value.min(u16::MAX as u32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sequence: u32, payload: &[u8]) -> AudioPacketRef<'_> {
        packet_with_flags(sequence, 0, payload)
    }

    fn packet_with_flags(sequence: u32, flags: u8, payload: &[u8]) -> AudioPacketRef<'_> {
        AudioPacketRef {
            sequence,
            flags,
            payload: VoicePayloadRef::Opus(payload),
        }
    }

    fn feedback_with_loss(expected: u16, lost: u16) -> TelemetryFeedback {
        TelemetryFeedback {
            highest_contiguous_sequence: expected as u32,
            expected_packets: expected,
            lost_packets: lost,
            late_or_duplicate_packets: 0,
            rtt_ms: 50,
            window_ms: 1_000,
        }
    }

    #[test]
    fn audio_datagram_round_trips() {
        let mut bytes = Vec::new();
        encode_audio_datagram(
            0x1122_3344,
            0b1010_0001,
            VoicePayloadRef::Opus(&[1, 2, 3]),
            &mut bytes,
        )
        .unwrap();

        assert_eq!(bytes.len(), AUDIO_HEADER_LEN + 3);
        assert_eq!(bytes[0], packet_tag(KIND_AUDIO));
        assert_eq!(bytes[1], 0b1010_0001);

        let parsed = parse_datagram(&bytes).unwrap();
        assert_eq!(
            parsed,
            DatagramRef::Audio(AudioPacketRef {
                sequence: 0x1122_3344,
                flags: 0b1010_0001,
                payload: VoicePayloadRef::Opus(&[1, 2, 3]),
            })
        );
    }

    #[test]
    fn silence_audio_datagram_round_trips() {
        let mut bytes = Vec::new();
        encode_audio_datagram(7, 0b10, VoicePayloadRef::Silence, &mut bytes).unwrap();

        assert_eq!(bytes.len(), AUDIO_HEADER_LEN);
        assert_eq!(
            parse_datagram(&bytes),
            Ok(DatagramRef::Audio(AudioPacketRef {
                sequence: 7,
                flags: 0b10,
                payload: VoicePayloadRef::Silence,
            }))
        );
    }

    #[test]
    fn rejects_invalid_datagrams() {
        let mut bytes = Vec::new();
        assert_eq!(
            encode_audio_datagram(0, 0, VoicePayloadRef::Opus(&[]), &mut bytes),
            Err(ProtocolError::EmptyAudioPayload)
        );
        assert_eq!(
            encode_audio_datagram(
                0,
                0,
                VoicePayloadRef::Opus(&vec![0; MAX_AUDIO_PAYLOAD_BYTES + 1]),
                &mut bytes
            ),
            Err(ProtocolError::AudioPayloadTooLarge)
        );
        assert_eq!(parse_datagram(&[0]), Err(ProtocolError::DatagramTooShort));
        assert_eq!(
            parse_datagram(&[packet_tag(15), 0]),
            Err(ProtocolError::UnknownPacketKind(15))
        );
        assert_eq!(
            parse_datagram(&[1 << VERSION_SHIFT, 0]),
            Err(ProtocolError::UnsupportedVersion(1))
        );
    }

    #[test]
    fn feedback_datagram_round_trips_and_reports_loss_fraction() {
        let feedback = TelemetryFeedback {
            highest_contiguous_sequence: 42,
            expected_packets: 200,
            lost_packets: 8,
            late_or_duplicate_packets: 3,
            rtt_ms: 71,
            window_ms: 2_000,
        };
        let mut bytes = Vec::new();
        encode_feedback_datagram(feedback, &mut bytes);

        assert_eq!(bytes.len(), FEEDBACK_PACKET_LEN);
        assert_eq!(parse_datagram(&bytes), Ok(DatagramRef::Feedback(feedback)));
        assert!((feedback.loss_fraction() - 0.04).abs() < f32::EPSILON);
    }

    #[test]
    fn jitter_buffer_reorders_before_deadline() {
        let start = Instant::now();
        let mut jitter = JitterBuffer::new(JitterBufferConfig {
            max_reorder_delay: Duration::from_millis(40),
            ..Default::default()
        });

        assert_eq!(jitter.insert(packet(0, &[0])), InsertOutcome::Accepted);
        assert_eq!(
            jitter.drain_ready(start),
            vec![PlayoutItem::Audio {
                sequence: 0,
                flags: 0,
                payload: VoicePayload::Opus(vec![0]),
            }]
        );

        assert_eq!(jitter.insert(packet(2, &[2])), InsertOutcome::Accepted);
        assert!(
            jitter
                .drain_ready(start + Duration::from_millis(20))
                .is_empty()
        );

        assert_eq!(jitter.insert(packet(1, &[1])), InsertOutcome::Accepted);
        assert_eq!(
            jitter.drain_ready(start + Duration::from_millis(25)),
            vec![
                PlayoutItem::Audio {
                    sequence: 1,
                    flags: 0,
                    payload: VoicePayload::Opus(vec![1]),
                },
                PlayoutItem::Audio {
                    sequence: 2,
                    flags: 0,
                    payload: VoicePayload::Opus(vec![2]),
                },
            ]
        );
    }

    #[test]
    fn jitter_buffer_preserves_audio_flags() {
        let start = Instant::now();
        let mut jitter = JitterBuffer::new(JitterBufferConfig::default());

        assert_eq!(
            jitter.insert(packet_with_flags(7, 0b1010_0001, &[7])),
            InsertOutcome::Accepted
        );
        assert_eq!(
            jitter.drain_ready(start),
            vec![PlayoutItem::Audio {
                sequence: 7,
                flags: 0b1010_0001,
                payload: VoicePayload::Opus(vec![7]),
            }]
        );
    }

    #[test]
    fn jitter_buffer_fills_missing_frames_after_deadline() {
        let start = Instant::now();
        let mut jitter = JitterBuffer::new(JitterBufferConfig {
            max_reorder_delay: Duration::from_millis(40),
            ..Default::default()
        });

        assert_eq!(jitter.insert(packet(0, &[0])), InsertOutcome::Accepted);
        assert_eq!(jitter.drain_ready(start).len(), 1);
        assert_eq!(jitter.insert(packet(3, &[3])), InsertOutcome::Accepted);
        assert!(
            jitter
                .drain_ready(start + Duration::from_millis(20))
                .is_empty()
        );

        assert_eq!(
            jitter.drain_ready(start + Duration::from_millis(61)),
            vec![
                PlayoutItem::Missing { sequence: 1 },
                PlayoutItem::Missing { sequence: 2 },
                PlayoutItem::Audio {
                    sequence: 3,
                    flags: 0,
                    payload: VoicePayload::Opus(vec![3]),
                },
            ]
        );
    }

    #[test]
    fn jitter_buffer_consumes_silence_sequence_without_loss() {
        let start = Instant::now();
        let mut jitter = JitterBuffer::new(JitterBufferConfig::default());

        assert_eq!(jitter.insert(packet(0, &[0])), InsertOutcome::Accepted);
        assert_eq!(jitter.drain_ready(start).len(), 1);

        jitter.consume_silence_sequence(1);
        assert_eq!(jitter.insert(packet(2, &[2])), InsertOutcome::Accepted);

        assert_eq!(
            jitter.drain_ready(start),
            vec![PlayoutItem::Audio {
                sequence: 2,
                flags: 0,
                payload: VoicePayload::Opus(vec![2]),
            }]
        );
    }

    #[test]
    fn jitter_buffer_skips_lost_silence_sequences_before_resume() {
        let start = Instant::now();
        let mut jitter = JitterBuffer::new(JitterBufferConfig::default());

        assert_eq!(jitter.insert(packet(0, &[0])), InsertOutcome::Accepted);
        assert_eq!(jitter.drain_ready(start).len(), 1);

        jitter.skip_to_sequence(4);
        assert_eq!(jitter.insert(packet(4, &[4])), InsertOutcome::Accepted);

        assert_eq!(
            jitter.drain_ready(start),
            vec![PlayoutItem::Audio {
                sequence: 4,
                flags: 0,
                payload: VoicePayload::Opus(vec![4]),
            }]
        );
    }

    #[test]
    fn jitter_buffer_rejects_duplicate_and_late_packets() {
        let start = Instant::now();
        let mut jitter = JitterBuffer::new(JitterBufferConfig::default());

        assert_eq!(jitter.insert(packet(0, &[0])), InsertOutcome::Accepted);
        assert_eq!(jitter.insert(packet(0, &[0])), InsertOutcome::Duplicate);
        assert_eq!(jitter.drain_ready(start).len(), 1);
        assert_eq!(jitter.insert(packet(0, &[0])), InsertOutcome::Late);
    }

    #[test]
    fn jitter_buffer_fast_forwards_large_gaps() {
        let start = Instant::now();
        let mut jitter = JitterBuffer::new(JitterBufferConfig {
            max_reorder_delay: Duration::from_millis(10),
            max_loss_fill_packets: 4,
            ..Default::default()
        });

        assert_eq!(jitter.insert(packet(10, &[10])), InsertOutcome::Accepted);
        assert_eq!(jitter.drain_ready(start).len(), 1);
        assert_eq!(jitter.insert(packet(20, &[20])), InsertOutcome::Accepted);
        assert!(
            jitter
                .drain_ready(start + Duration::from_millis(5))
                .is_empty()
        );

        assert_eq!(
            jitter.drain_ready(start + Duration::from_millis(16)),
            vec![
                PlayoutItem::FastForward {
                    from_sequence: 11,
                    to_sequence: 20,
                    skipped_packets: 9,
                },
                PlayoutItem::Audio {
                    sequence: 20,
                    flags: 0,
                    payload: VoicePayload::Opus(vec![20]),
                },
            ]
        );
    }

    #[test]
    fn telemetry_window_counts_loss_and_duplicates() {
        let mut telemetry = TelemetryWindow::default();
        telemetry.observe_insert(&InsertOutcome::Duplicate);
        telemetry.observe_insert(&InsertOutcome::Late);
        telemetry.observe_playout(&PlayoutItem::Audio {
            sequence: 5,
            flags: 0,
            payload: VoicePayload::Opus(vec![0]),
        });
        telemetry.observe_playout(&PlayoutItem::Missing { sequence: 6 });
        telemetry.observe_playout(&PlayoutItem::FastForward {
            from_sequence: 7,
            to_sequence: 10,
            skipped_packets: 3,
        });

        let feedback = telemetry.build_feedback(25, 100);
        assert_eq!(
            feedback,
            TelemetryFeedback {
                highest_contiguous_sequence: 9,
                expected_packets: 5,
                lost_packets: 4,
                late_or_duplicate_packets: 2,
                rtt_ms: 25,
                window_ms: 100,
            }
        );

        let reset = telemetry.build_feedback(25, 100);
        assert_eq!(reset.expected_packets, 0);
        assert_eq!(reset.lost_packets, 0);
        assert_eq!(reset.late_or_duplicate_packets, 0);
        assert_eq!(reset.highest_contiguous_sequence, 9);
    }

    #[test]
    fn dynamic_controller_maps_loss_to_dred_and_core_bitrate() {
        let mut controller = DynamicOverheadController::new(1.0);

        assert_eq!(
            controller.handle_feedback(feedback_with_loss(100, 0)),
            EncoderNetworkProfile::EXCELLENT
        );
        assert_eq!(
            controller.handle_feedback(feedback_with_loss(100, 3)),
            EncoderNetworkProfile::DEGRADED
        );
        assert_eq!(
            controller.handle_feedback(feedback_with_loss(100, 10)),
            EncoderNetworkProfile::SEVERE
        );
        assert_eq!(
            controller.handle_feedback(feedback_with_loss(100, 20)),
            EncoderNetworkProfile::CRITICAL
        );
    }

    #[test]
    fn dynamic_controller_smooths_loss_before_changing_profile() {
        let mut controller = DynamicOverheadController::new(0.25);

        let first = controller.handle_feedback(feedback_with_loss(100, 20));
        assert_eq!(first, EncoderNetworkProfile::SEVERE);
        assert!((controller.smoothed_loss() - 0.05).abs() < 0.000_001);

        let second = controller.handle_feedback(feedback_with_loss(100, 20));
        assert_eq!(second, EncoderNetworkProfile::SEVERE);
        assert!((controller.smoothed_loss() - 0.0875).abs() < 0.000_001);
        assert_eq!(controller.current_profile(), second);
    }

    #[test]
    fn feedback_can_be_applied_to_encoder_adapter() {
        #[derive(Default)]
        struct FakeEncoder {
            applied: Vec<EncoderNetworkProfile>,
        }

        impl EncoderNetworkTuning for FakeEncoder {
            type Error = std::convert::Infallible;

            fn apply_network_profile(
                &mut self,
                profile: EncoderNetworkProfile,
            ) -> Result<(), Self::Error> {
                self.applied.push(profile);
                Ok(())
            }
        }

        let mut controller = DynamicOverheadController::new(1.0);
        let mut encoder = FakeEncoder::default();
        let profile =
            apply_feedback_to_encoder(&mut controller, feedback_with_loss(100, 6), &mut encoder)
                .unwrap();

        assert_eq!(profile, EncoderNetworkProfile::SEVERE);
        assert_eq!(encoder.applied, vec![EncoderNetworkProfile::SEVERE]);
    }

    #[test]
    fn network_simulator_produces_deterministic_loss_duplicates_and_reordering() {
        let start = Instant::now();
        let mut config =
            NetworkSimulatorConfig::new(Duration::from_millis(30), Duration::from_millis(45));
        config.good_to_bad_probability = 0.35;
        config.bad_to_good_probability = 0.15;
        config.loss_good = 0.0;
        config.loss_bad = 0.70;
        config.duplicate_rate = 0.10;
        config.seed = 0x1234_5678;
        let mut simulator = NetworkSimulator::new(config);
        let mut datagram = Vec::new();

        for sequence in 0..80 {
            encode_audio_datagram(
                sequence,
                0,
                VoicePayloadRef::Opus(&[sequence as u8]),
                &mut datagram,
            )
            .unwrap();
            simulator.send_datagram(
                &datagram,
                start + Duration::from_millis(u64::from(sequence) * 20),
            );
        }

        let delivered = simulator.receive_datagrams(start + Duration::from_secs(3));
        let sequences = delivered
            .iter()
            .map(|datagram| match parse_datagram(datagram).unwrap() {
                DatagramRef::Audio(audio) => audio.sequence,
                DatagramRef::Feedback(_) => unreachable!(),
            })
            .collect::<Vec<_>>();
        let stats = simulator.stats();

        assert!(stats.dropped > 0);
        assert!(stats.duplicated > 0);
        assert!(matches!(
            simulator.state(),
            GilbertState::Good | GilbertState::Bad
        ));
        assert_eq!(stats.delivered as usize, delivered.len());
        assert!(sequences.windows(2).any(|window| window[0] > window[1]));
    }

    #[test]
    fn simulated_pipeline_tracks_contiguous_playout_under_loss() {
        let start = Instant::now();
        let mut config =
            NetworkSimulatorConfig::new(Duration::from_millis(35), Duration::from_millis(25));
        config.good_to_bad_probability = 0.20;
        config.bad_to_good_probability = 0.30;
        config.loss_good = 0.0;
        config.loss_bad = 0.60;
        config.duplicate_rate = 0.05;
        config.seed = 0xfeed_babe;

        let mut simulator = NetworkSimulator::new(config);
        let mut jitter = JitterBuffer::new(JitterBufferConfig {
            max_reorder_delay: Duration::from_millis(50),
            ..Default::default()
        });
        let mut telemetry = TelemetryWindow::default();
        let mut datagram = Vec::new();
        let mut playout = Vec::new();

        for sequence in 0..100 {
            let now = start + Duration::from_millis(u64::from(sequence) * 20);
            encode_audio_datagram(
                sequence,
                0,
                VoicePayloadRef::Opus(&[sequence as u8]),
                &mut datagram,
            )
            .unwrap();
            simulator.send_datagram(&datagram, now);

            for delivered in simulator.receive_datagrams(now) {
                let DatagramRef::Audio(packet) = parse_datagram(&delivered).unwrap() else {
                    unreachable!();
                };
                let outcome = jitter.insert(packet);
                telemetry.observe_insert(&outcome);
            }

            for item in jitter.drain_ready(now) {
                telemetry.observe_playout(&item);
                playout.push(item);
            }
        }

        let final_now = start + Duration::from_secs(4);
        for delivered in simulator.receive_datagrams(final_now) {
            let DatagramRef::Audio(packet) = parse_datagram(&delivered).unwrap() else {
                unreachable!();
            };
            let outcome = jitter.insert(packet);
            telemetry.observe_insert(&outcome);
        }
        for item in jitter.drain_ready(final_now) {
            telemetry.observe_playout(&item);
            playout.push(item);
        }

        assert!(!playout.is_empty());
        assert!(
            playout
                .windows(2)
                .all(|window| window[1].sequence() > window[0].sequence())
        );

        let feedback = telemetry.build_feedback(40, 2_000);
        assert!(feedback.expected_packets > 0);
        assert!(feedback.lost_packets > 0);
        assert!(feedback.late_or_duplicate_packets > 0);
        assert!(feedback.loss_fraction() > 0.0);
    }
}
