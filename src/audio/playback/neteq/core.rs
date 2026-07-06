//! The NetEQ `GetAudio` loop — a port of `NetEqImpl::GetAudioInternal`,
//! `GetDecision`, `Decode`, `DecodeLoop`, `ExtractPackets`, and the `Do*`
//! operation handlers in `/tmp/webrtc/modules/audio_coding/neteq/neteq_impl.cc`,
//! plus the DRED-at-insertion path from `/tmp/webrtc-dred/.../neteq_impl.cc`
//! (`InsertPacketInternal`).
//!
//! [`NetEqCore`] owns the timestamp-keyed [`PacketBuffer`], the [`SyncBuffer`] of
//! decoded PCM, the [`DecisionLogic`] controller, the Opus/DRED decoders, and the
//! fixed-point DSP ([`Expand`]/[`merge`]/[`normal`] for normal/merge/expand and
//! [`time_stretch`] for accelerate/preemptive-expand), sharing one
//! [`BackgroundNoise`] and one [`RandomVector`] across them as WebRTC does.
//! [`NetEqCore::insert_packet`] expands a packet into its redundancy units and
//! inserts them; [`NetEqCore::get_audio`] runs one decision, decodes what it
//! needs, executes the operation against the sync buffer, and emits exactly one
//! 10 ms output block.
//!
//! Chatt is mono, single Opus stream, no DTMF and no RFC 3389 comfort noise, so
//! those branches of the reference never fire. The sync buffer is `i16` PCM; the
//! decoded buffer is the reference `decoded_buffer_`. Operations write into the
//! sync buffer directly (overlap-add tails, merged prefixes) and push their
//! emitted samples, mirroring `algorithm_buffer_` + `PushBack`. The only
//! `i16`<->`f32` conversions are at decode (Opus i16 out) and the final output
//! block (i16 -> f32 for the mixer/ring).

use std::sync::Arc;
use std::time::Instant;

use opus_codec::{
    Channels, Decoder, DredDecoder, DredState, SampleRate, packet_has_lbrr,
    packet_samples_per_frame,
};

use super::decision_logic::DecisionLogic;
use super::dsp::background_noise::BackgroundNoise;
use super::dsp::expand::Expand;
use super::dsp::random_vector::RandomVector;
use super::dsp::scratch::{DspScratch, TimescaleScratch};
use super::dsp::time_stretch::{self, ReturnCode};
use super::dsp::{merge, normal};
use super::neteq_status::{
    ControllerConfig, NetEqStatus, PacketArrivedInfo, PacketBufferInfo, PacketInfo,
};
use super::operation::{Mode, Operation};
use super::packet::{Packet, PacketPayload, Priority, is_newer_timestamp};
use super::packet_buffer::PacketBuffer;
use super::redundancy::{DredInfo, FecInfo, parse_payload_redundancy};
use super::sync_buffer::SyncBuffer;
use super::tick_timer::{Stopwatch, TickTimer};
use crate::audio::shared::{
    DecodedFrameSource, LIVE_CAPTURE_MUTE_FADE, LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_MUTE,
    LIVE_PACKET_FLAG_OPUS_RESET, LIVE_PACKET_FLAG_SILENCE_RESUME, LIVE_PLAYBACK_DRED_MAX_SAMPLES,
    LiveAudioTuning, MAX_OPUS_DECODE_SAMPLES, SAMPLE_RATE, apply_gain_ramp, duration_to_ms,
    mute_gain_step, samples_for_duration, samples_to_ms,
};

/// Expand's overlap lookahead the sync buffer always retains (`5 * fs_mult` at
/// 48 kHz). Matches `Expand::overlap_length`.
const EXPAND_OVERLAP_LENGTH: usize = 30;

/// 48 kHz. The live path is always 48 kHz mono.
const FS_HZ: i32 = SAMPLE_RATE as i32;
const FS_MULT: usize = SAMPLE_RATE as usize / 8_000;
/// 10 ms output block, matching `output_size_samples_` and the consumer cadence.
const OUTPUT_SIZE_SAMPLES: usize = SAMPLE_RATE as usize / 100;
/// 30 ms — the minimum input the time-scale operations need (`240 * fs_mult`).
const TIMESCALE_REQUIRED_SAMPLES: usize = 240 * FS_MULT;
const SAMPLES_10_MS: usize = 80 * FS_MULT;
const SAMPLES_20_MS: usize = 2 * SAMPLES_10_MS;
const SAMPLES_30_MS: usize = 3 * SAMPLES_10_MS;
/// The default packet duration before any packet has been decoded: Chatt's 20 ms
/// Opus frame.
const DEFAULT_FRAME_SAMPLES: usize = LIVE_OPUS_FRAME_SAMPLES;
/// Packet-buffer overflow threshold, also sizing the extraction scratch list.
const MAX_PACKETS_IN_BUFFER: usize = 200;
/// One DRED chunk, 10 ms at 48 kHz.
const DRED_CHUNK_SAMPLES: usize = SAMPLE_RATE as usize / 100;
/// How far a sequence-advancing packet's timestamp may trail the playout point
/// before it is treated as a sender media-clock rebase (capture restart) rather
/// than late data. Must stay well above the silence-gate preroll back-dating
/// and any plausible reorder delay, both of which are a few hundred ms at most.
const TIMELINE_REBASE_TOLERANCE_SAMPLES: u32 = SAMPLE_RATE;

/// What [`NetEqCore::get_audio`] produced for one output block. The mode feeds
/// back as `last_mode` and drives stats/provenance.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AudioResult {
    pub mode: Mode,
    pub source: DecodedFrameSource,
    /// True when the output is a muted (all-zero) expand frame.
    pub muted: bool,
    /// Samples removed by accelerate / added by preemptive-expand this block.
    pub time_stretched: i32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct NetEqDiagnostics {
    pub target_ms: u64,
    pub playout_delay_ms: u64,
    pub sync_buffer_ms: u64,
    pub packet_buffer_ms: u64,
    pub packet_buffer_wait_ms: u64,
    pub packets_buffered: usize,
    pub packets_discarded: u64,
    pub secondary_packets_discarded: u64,
    #[cfg(test)]
    pub trash_overflow: u64,
    pub next_packet_gap_ms: Option<i64>,
    pub operation: &'static str,
    pub reason: &'static str,
    pub dred_last_horizon_ms: u64,
    pub dred_missed_horizon_count: u64,
    pub dred_missed_horizon_ms: u64,
}

/// Cheap state the worker snapshots before preparing a packet outside the
/// callback-shared NetEQ mutex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NetEqInsertContext {
    current_gap: u32,
    dred_enabled: bool,
}

/// One cached DRED parse, reused across the 10 ms chunks recovered from the same
/// packet (mirrors the previous live path's `DredGapState`). The core keeps one
/// slot alive for the stream's lifetime so a callback-side re-parse reuses the
/// existing `DredState` allocation via [`DredState::reset`]. `sequence` is
/// `None` while the slot holds no valid parse.
struct DredParse {
    sequence: Option<u32>,
    state: DredState,
    processed: bool,
    /// Final-pair PCM: the last two 10 ms chunks decoded as one 20 ms deep-PLC
    /// call, keyed by `span` (the anchor offset of the pair's start). Capacity
    /// is reserved off the audio callback so the fill does not allocate.
    pcm: Vec<i16>,
    span: usize,
}

/// Extra parse span beyond the recovery gap, in samples. The deep-PLC synthesis
/// feeds feature frames past the requested reconstruction span (init frames plus
/// `dred_ec_decode`'s pairwise latent rounding); a parse bounded exactly to the
/// gap starves those and the first recovered chunks render with hallucinated
/// features, which audibly corrupts pulse trains (vocal fry) at recovery onsets.
const DRED_PARSE_MARGIN_SAMPLES: usize = 4 * DRED_CHUNK_SAMPLES;

/// Packet bytes and redundancy expansion prepared before the final NetEQ insert
/// lock hold.
pub(crate) struct NetEqPreparedPacket {
    context: NetEqInsertContext,
    timestamp: u32,
    sequence: u32,
    flags: u8,
    packets: Vec<Packet>,
    dred_parse: Option<DredParse>,
    available_horizon_samples: u32,
}

impl NetEqPreparedPacket {
    pub(crate) fn prepare(
        context: NetEqInsertContext,
        timestamp: u32,
        sequence: u32,
        flags: u8,
        datagram: Arc<Vec<u8>>,
        dred_decoder: Option<&mut DredDecoder>,
    ) -> Option<Self> {
        if datagram.is_empty() {
            return None;
        }
        let fec = NetEqCore::parse_fec(datagram.as_slice());
        let (dred, dred_parse, available_horizon_samples) = if context.dred_enabled {
            NetEqCore::parse_dred_for_insert(dred_decoder, sequence, &datagram, context.current_gap)
        } else {
            (None, None, 0)
        };
        let mut packets = parse_payload_redundancy(
            timestamp,
            sequence,
            Arc::clone(&datagram),
            context.current_gap,
            DEFAULT_FRAME_SAMPLES as u32,
            fec,
            dred,
        );
        if flags & LIVE_PACKET_FLAG_MUTE != 0
            && let Some(primary) = packets
                .iter_mut()
                .find(|packet| packet.priority == Priority::PRIMARY)
        {
            primary.muted = true;
        }
        Some(Self {
            context,
            timestamp,
            sequence,
            flags,
            packets,
            dred_parse,
            available_horizon_samples,
        })
    }

    pub(crate) fn context(&self) -> NetEqInsertContext {
        self.context
    }
}

/// The faithful NetEQ control + decode loop for the live playback path.
pub(crate) struct NetEqCore {
    tick_timer: TickTimer,
    /// Origin of the wall-clock arrival timeline, set on first observation.
    /// WebRTC uses tick counts as its arrival clock because its `GetAudio` is
    /// device-locked to 10 ms; Chatt's pulls are ring-scheduled and bunch
    /// around muted-suppression drains, so arrival statistics use wall time.
    wall_origin: Option<Instant>,
    /// The latest wall-clock observation in 48 kHz sample units.
    wall_now_samples: i64,
    packet_buffer: PacketBuffer,
    sync_buffer: SyncBuffer,
    decision_logic: DecisionLogic,
    decoder: Decoder,
    dred_decoder: Option<DredDecoder>,
    dred_parse: Option<DredParse>,
    /// The packet-loss concealment generator. Shares `background_noise` and
    /// `random_vector` with the time-stretch ops, exactly as WebRTC does.
    expand: Expand,
    /// Comfort-noise model, updated from decoded audio and read by Expand and
    /// the time-stretch VAD.
    background_noise: BackgroundNoise,
    /// Concealment RNG; its seed-increment ordering is part of bit-exactness.
    random_vector: RandomVector,
    /// Preallocated DSP temporaries so no operation allocates on the callback.
    scratch: DspScratch,
    /// `decoded_buffer_`: scratch for freshly decoded PCM.
    decoded_buffer: Vec<i16>,
    /// Reusable extraction list so `GetAudio` never allocates one. Taken by
    /// `get_decision`, threaded to `decode`, and restored there emptied, with
    /// consumed packets parked in the packet-buffer trash.
    packet_scratch: Vec<Packet>,
    last_mode: Mode,
    /// `decoder_frame_length_`: samples per channel of the last decoded frame.
    decoder_frame_length: usize,
    /// `timestamp_`: the media timestamp the next decode aligns to.
    timestamp: u32,
    first_packet: bool,
    new_codec: bool,
    reset_decoder: bool,
    /// `generated_noise_stopwatch_`: counts ticks of concealment/CNG output.
    generated_noise_stopwatch: Option<Stopwatch>,
    /// Moving receiver mute gain, ramped toward 0 across a muted fade-out tail and
    /// back to 1 on normal audio. Mirrors the capture-side `mute_gain`, applied to
    /// decoded normal/merge audio so a muted tail ends softly even if the sender's
    /// own fade was clipped by loss.
    mute_gain: f32,
    mute_gain_step: f32,
    /// The standing mute target (0.0 muted, 1.0 open), set when a primary packet
    /// is decoded and held across the following concealment so the fade completes
    /// smoothly even when decode stops mid-fade. Applied to every output block.
    mute_target: f32,
    last_operation: Operation,
    last_decision_reason: &'static str,
    /// The newest wire sequence inserted, distinguishing a sender media-clock
    /// rebase (sequence advances, timestamp leaps backward) from a straggler
    /// (both are old). WebRTC gets this signal from an SSRC change; Chatt's wire
    /// sequence is monotonic across sender capture restarts, so the timestamp
    /// discontinuity itself is the restart marker.
    last_sequence: Option<u32>,
    /// Sequence of the packet that triggered the last timeline rebase. Packets
    /// with older sequences belong to the abandoned timeline: their timestamps
    /// read as far-future on the new one and would jam the buffer, so they are
    /// dropped as late.
    rebase_floor_sequence: Option<u32>,
    dred_last_horizon_samples: usize,
    dred_missed_horizon_count: u64,
    dred_missed_horizon_samples: u64,
}

/// The audio callback pulls a `NetEqCore` owned behind an `Arc<Mutex<_>>` shared
/// with the decode worker, so the core must stay `Send`.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<NetEqCore>()
};

impl NetEqCore {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Result<Self, String> {
        tuning.validate()?;
        let tick_timer = TickTimer::new();
        let start_delay_ms = duration_to_i32_ms(tuning.neteq_start_delay);
        let min_delay_ms = duration_to_i32_ms(tuning.neteq_min_delay);
        let base_min_delay_ms = duration_to_i32_ms(tuning.neteq_base_minimum_delay);
        let max_delay_ms = duration_to_i32_ms(tuning.neteq_max_delay);
        let config = ControllerConfig {
            allow_time_stretching: true,
            max_packets_in_buffer: MAX_PACKETS_IN_BUFFER as i32,
            start_delay_ms,
            min_delay_ms,
            base_min_delay_ms,
            max_delay_ms,
        };
        let mut decision_logic = DecisionLogic::new(config, &tick_timer);
        decision_logic.set_sample_rate(FS_HZ, OUTPUT_SIZE_SAMPLES);
        if !decision_logic.set_minimum_delay(min_delay_ms)
            || !decision_logic.set_base_minimum_delay(base_min_delay_ms)
            || !decision_logic.set_maximum_delay(max_delay_ms)
        {
            return Err("invalid NetEQ delay constraints".to_string());
        }
        // The parse slot is created eagerly so the callback-side DRED fallback
        // reuses its allocation via reset() instead of opus_dred_alloc. DRED is
        // disabled outright if the state cannot be allocated, keeping the
        // "decoder present implies slot present" invariant.
        let mut dred_decoder = DredDecoder::new().ok();
        let dred_parse = match &dred_decoder {
            Some(_) => match DredState::new() {
                Ok(state) => Some(DredParse {
                    sequence: None,
                    state,
                    processed: false,
                    pcm: Vec::with_capacity(2 * DRED_CHUNK_SAMPLES),
                    span: 0,
                }),
                Err(_) => {
                    dred_decoder = None;
                    None
                }
            },
            None => None,
        };
        Ok(Self {
            tick_timer,
            wall_origin: None,
            wall_now_samples: 0,
            packet_buffer: PacketBuffer::new(MAX_PACKETS_IN_BUFFER),
            sync_buffer: SyncBuffer::with_default_length(),
            decision_logic,
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Mono)
                .map_err(|error| error.to_string())?,
            dred_decoder,
            dred_parse,
            expand: Expand::new(),
            background_noise: BackgroundNoise::new(),
            random_vector: RandomVector::new(),
            scratch: DspScratch::new(),
            decoded_buffer: vec![0; MAX_OPUS_DECODE_SAMPLES],
            packet_scratch: Vec::with_capacity(MAX_PACKETS_IN_BUFFER),
            last_mode: Mode::Normal,
            decoder_frame_length: DEFAULT_FRAME_SAMPLES,
            timestamp: 0,
            first_packet: true,
            new_codec: false,
            reset_decoder: false,
            generated_noise_stopwatch: None,
            mute_gain: 1.0,
            mute_gain_step: mute_gain_step(samples_for_duration(LIVE_CAPTURE_MUTE_FADE)),
            mute_target: 1.0,
            last_operation: Operation::Normal,
            last_decision_reason: "startup",
            last_sequence: None,
            rebase_floor_sequence: None,
            dred_last_horizon_samples: 0,
            dred_missed_horizon_count: 0,
            dred_missed_horizon_samples: 0,
        })
    }

    /// Advances the wall-clock arrival timeline to `now`, returning it in
    /// 48 kHz sample units since the first observation.
    fn observe_wall_clock(&mut self, now: Instant) -> i64 {
        let origin = *self.wall_origin.get_or_insert(now);
        let micros = now.saturating_duration_since(origin).as_micros() as i64;
        self.wall_now_samples = micros * (FS_HZ as i64) / 1_000_000;
        self.wall_now_samples
    }

    #[cfg(test)]
    pub(crate) fn packets_buffered(&self) -> usize {
        self.packet_buffer.num_packets()
    }

    /// Worker side. See [`PacketBuffer::swap_trash`].
    pub(crate) fn swap_packet_trash(&mut self, into: &mut Vec<Packet>) {
        self.packet_buffer.swap_trash(into);
    }

    fn playout_timestamp(&self) -> u32 {
        self.sync_buffer
            .end_timestamp()
            .wrapping_sub(self.sync_buffer.future_length() as u32)
    }

    pub(crate) fn diagnostics(&self) -> NetEqDiagnostics {
        let status = self.status();
        // Match `DecisionLogic::playout_delay_ms`: the end timestamp freezes during
        // Expand, so add the synthetic `generated_noise_samples` to recover the true
        // playout position.
        let playout_timestamp = status
            .target_timestamp
            .wrapping_add(status.generated_noise_samples as u32)
            .wrapping_sub(status.sync_buffer_samples as u32);
        let next_packet_gap_ms = status.next_packet.map(|packet| {
            let gap_samples = packet.timestamp.wrapping_sub(playout_timestamp) as i32;
            i64::from(gap_samples) / i64::from(FS_HZ / 1000)
        });

        NetEqDiagnostics {
            target_ms: self.decision_logic.target_level_ms().max(0) as u64,
            playout_delay_ms: self
                .decision_logic
                .playout_delay_ms(&status, self.wall_now_samples)
                .max(0) as u64,
            sync_buffer_ms: samples_to_ms(status.sync_buffer_samples),
            packet_buffer_ms: samples_to_ms(status.packet_buffer_info.span_samples),
            packet_buffer_wait_ms: samples_to_ms(status.packet_buffer_info.span_samples_wait_time),
            packets_buffered: status.packet_buffer_info.num_packets,
            packets_discarded: self.packet_buffer.packets_discarded(),
            secondary_packets_discarded: self.packet_buffer.secondary_packets_discarded(),
            #[cfg(test)]
            trash_overflow: self.packet_buffer.trash_overflow(),
            next_packet_gap_ms,
            operation: self.last_operation.label(),
            reason: self.last_decision_reason,
            dred_last_horizon_ms: samples_to_ms(self.dred_last_horizon_samples),
            dred_missed_horizon_count: self.dred_missed_horizon_count,
            dred_missed_horizon_ms: samples_to_ms(self.dred_missed_horizon_samples as usize),
        }
    }

    /// Flushes all state for a hard stream restart (decoder reset boundary).
    pub(crate) fn flush(&mut self) {
        self.packet_buffer.flush();
        self.sync_buffer.flush();
        self.expand.reset();
        self.background_noise.reset();
        self.decision_logic.soft_reset(&self.tick_timer);
        self.last_mode = Mode::Normal;
        self.timestamp = 0;
        self.first_packet = true;
        self.new_codec = false;
        self.reset_decoder = false;
        self.generated_noise_stopwatch = None;
        if let Some(parse) = &mut self.dred_parse {
            // Invalidate rather than drop: the state allocation is reused and
            // flush must stay allocation/free-free for callers under the lock.
            parse.sequence = None;
        }
        self.mute_gain = 1.0;
        self.mute_target = 1.0;
        self.last_operation = Operation::Normal;
        self.last_decision_reason = "flush";
        self.dred_last_horizon_samples = 0;
        self.dred_missed_horizon_count = 0;
        self.dred_missed_horizon_samples = 0;
        let _ = self.decoder.reset();
    }

    /// Inserts one Opus voice packet, expanding its DRED into priority-ranked
    /// recovery packets first. Port of `NetEqImpl::InsertPacketInternal`. `flags`
    /// carries the Chatt media flags; `LIVE_PACKET_FLAG_MUTE` fades the decoded
    /// tail on the receiver.
    /// Returns `true` if the packet arrived after its playout slot had already
    /// passed (older than the current playout point), i.e. too late to be played.
    /// `now` is the packet's wall-clock arrival time.
    #[cfg(test)]
    pub(crate) fn insert_packet(
        &mut self,
        now: Instant,
        timestamp: u32,
        sequence: u32,
        flags: u8,
        opus: &[u8],
    ) -> bool {
        self.insert_datagram(now, timestamp, sequence, flags, Arc::new(opus.to_vec()))
    }

    pub(crate) fn insert_datagram(
        &mut self,
        now: Instant,
        timestamp: u32,
        sequence: u32,
        flags: u8,
        datagram: Arc<Vec<u8>>,
    ) -> bool {
        let Some(prepared) = NetEqPreparedPacket::prepare(
            self.capture_insert_context(timestamp, sequence),
            timestamp,
            sequence,
            flags,
            datagram,
            self.dred_decoder.as_mut(),
        ) else {
            return false;
        };
        self.insert_prepared_packet(now, prepared)
    }

    pub(crate) fn capture_insert_context(
        &self,
        timestamp: u32,
        sequence: u32,
    ) -> NetEqInsertContext {
        let playout_timestamp = self.playout_timestamp();
        let sequence_advanced = self
            .last_sequence
            .is_none_or(|last| is_newer_timestamp(sequence, last));
        let rebases_timeline = !self.first_packet
            && sequence_advanced
            && is_newer_timestamp(playout_timestamp, timestamp)
            && playout_timestamp.wrapping_sub(timestamp) > TIMELINE_REBASE_TOLERANCE_SAMPLES;
        NetEqInsertContext {
            current_gap: if self.first_packet || rebases_timeline {
                0
            } else {
                self.recovery_gap(timestamp)
            },
            dred_enabled: self.dred_decoder.is_some(),
        }
    }

    pub(crate) fn insert_prepared_packet(
        &mut self,
        now: Instant,
        mut prepared: NetEqPreparedPacket,
    ) -> bool {
        let arrival_samples = self.observe_wall_clock(now);
        let timestamp = prepared.timestamp;
        let sequence = prepared.sequence;
        let flags = prepared.flags;
        if let Some(floor) = self.rebase_floor_sequence
            && is_newer_timestamp(floor, sequence)
        {
            return true;
        }
        let playout_timestamp = self.playout_timestamp();
        // A sender capture restart (e.g. a mic device switch mid-call) rebases
        // its media clock to zero while the stream and its wire sequence persist.
        // Relative to the old timeline the new timestamps read as a huge
        // backward leap, which the buffer decision would treat as far-future
        // (wrapped) data and expand over until the packet buffer overflowed.
        // Mirror WebRTC's SSRC-change reinitialization instead: flush everything
        // and let the first-packet path below re-anchor to the new timeline. A
        // straggler cannot trigger this because its sequence is old too.
        let sequence_advanced = self
            .last_sequence
            .is_none_or(|last| is_newer_timestamp(sequence, last));
        if !self.first_packet
            && sequence_advanced
            && is_newer_timestamp(playout_timestamp, timestamp)
            && playout_timestamp.wrapping_sub(timestamp) > TIMELINE_REBASE_TOLERANCE_SAMPLES
        {
            self.flush();
            self.rebase_floor_sequence = Some(sequence);
            prepared
                .packets
                .retain(|packet| packet.priority.codec_level != 2);
            prepared.dred_parse = None;
        }
        if sequence_advanced {
            self.last_sequence = Some(sequence);
        }
        let late = !self.first_packet && is_newer_timestamp(playout_timestamp, timestamp);
        // The sender drains its buffered preroll tail as a burst of back-dated
        // packets when resuming from a silence-gated pause. Those carry RTP
        // timestamps trailing wall-clock by the preroll length, so feeding them
        // to the arrival statistics reads as a ~preroll-sized late arrival,
        // spiking `PacketArrivalHistory::max_delay_ms()` (and the optimizers) for
        // up to the 2 s history window. That inflates the Accelerate `high_limit`
        // and pins the playout buffer deep. Honor the sender's resume flag and
        // withhold these transient packets from the controller's statistics; they
        // are still buffered and decoded normally.
        let silence_resume = flags & LIVE_PACKET_FLAG_SILENCE_RESUME != 0;
        if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 {
            // The sender restarted its encoder (e.g. resuming after a silence
            // gap); reset the decoder before the next decode to match.
            self.reset_decoder = true;
        }

        // Compute the recovery gap behind this packet, exactly as the DRED fork's
        // InsertPacketInternal does: the hole between the most recent buffered (or
        // played) audio and this packet's timestamp.
        let current_gap = self.recovery_gap(timestamp);
        self.dred_last_horizon_samples = prepared.available_horizon_samples as usize;
        if current_gap > prepared.available_horizon_samples {
            self.dred_missed_horizon_count = self.dred_missed_horizon_count.saturating_add(1);
            self.dred_missed_horizon_samples = self
                .dred_missed_horizon_samples
                .saturating_add(u64::from(current_gap - prepared.available_horizon_samples));
        }
        if let Some(dred_parse) = prepared.dred_parse.take() {
            self.dred_parse = Some(dred_parse);
        }

        let main_timestamp = prepared
            .packets
            .first()
            .map_or(timestamp, |packet| packet.timestamp);
        let was_first_packet = self.first_packet;
        if self.first_packet {
            self.packet_buffer.flush();
            self.sync_buffer
                .increase_end_timestamp(main_timestamp.wrapping_sub(self.timestamp));
            self.timestamp = main_timestamp;
            self.first_packet = false;
        }

        // Insert each redundancy unit and notify the controller once per unit,
        // exactly as `NetEqImpl::InsertPacketInternal` loops over the parsed
        // packet list calling `ToPacketArrivedInfo(packet)` + `PacketArrived` for
        // every entry. Each unit reports its OWN timestamp and decoded duration
        // (a 10 ms DRED chunk, a 20 ms FEC/primary frame), not a single rewritten
        // `main_timestamp`. This is what keeps a genuinely reordered primary from
        // colliding in `PacketArrivalHistory` with a DRED chunk that pre-recovered
        // its timestamp: the chunk's 10 ms span does not contain the 20 ms primary,
        // so the reorder still reaches the reorder optimizer.
        for packet in prepared.packets {
            let packet_timestamp = packet.timestamp;
            let packet_length_samples = packet.duration_samples;
            let buffer_flush = matches!(
                self.packet_buffer.insert_packet(packet, &self.tick_timer),
                super::packet_buffer::InsertOutcome::Flushed
            );
            if buffer_flush {
                self.new_codec = true;
            }
            let info = PacketArrivedInfo {
                packet_length_samples,
                main_timestamp: packet_timestamp,
                is_cng_or_dtmf: false,
                buffer_flush,
            };
            let should_update_stats = !self.new_codec && !silence_resume;
            self.decision_logic.packet_arrived(
                FS_HZ,
                should_update_stats,
                &info,
                &self.tick_timer,
                arrival_samples,
            );
        }

        // Reference sets `new_codec_` only after the arrival loop, inside the
        // `first_packet_` block, so the first redundancy unit still records its
        // arrival statistics (`MaybeChangePayloadType` returns false on the very
        // first packet). Setting it before the loop dropped the startup arrival
        // from `PacketArrivalHistory` and skewed the initial delay estimate.
        if was_first_packet {
            self.new_codec = true;
        }
        late
    }

    /// The DRED recovery gap behind a packet at `timestamp`. Port of the
    /// `current_gap` computation in `InsertPacketInternal`.
    fn recovery_gap(&self, timestamp: u32) -> u32 {
        if self.first_packet {
            return 0;
        }
        let previous_timestamp = match self.packet_buffer.next_lower_timestamp(timestamp) {
            Some(prev) => prev.wrapping_add(self.decoder_frame_length as u32),
            None => {
                // Earliest packet in the buffer. The hole is measured from the
                // current playout end; if this packet is contiguous there is none.
                let end = self.sync_buffer.end_timestamp();
                if is_newer_timestamp(timestamp, end)
                    && timestamp.wrapping_sub(end) > self.decoder_frame_length as u32
                {
                    end
                } else {
                    timestamp
                }
            }
        };
        let gap = timestamp.wrapping_sub(previous_timestamp);
        if (gap as i32) > 0 { gap } else { 0 }
    }

    /// Detects LBRR in-band FEC and returns the recovered frame duration, the
    /// port of `PacketHasFec` + `WebRtcOpus_FecDurationEst`.
    fn parse_fec(opus: &[u8]) -> Option<FecInfo> {
        if !packet_has_lbrr(opus).unwrap_or(false) {
            return None;
        }
        let duration = packet_samples_per_frame(opus, SampleRate::Hz48000).ok()?;
        // The reference clamps to [10 ms, 120 ms] at 48 kHz = [480, 5760] samples.
        if !(480..=5760).contains(&duration) {
            return None;
        }
        Some(FecInfo {
            duration: duration as u32,
        })
    }

    /// Parses the DRED region of `datagram`, caching a gap-bounded state for the
    /// decode loop. Returns the gap-bounded DRED span for recovery placement plus
    /// the packet's full available DRED horizon for diagnostics.
    #[cfg(test)]
    fn parse_dred(
        &mut self,
        sequence: u32,
        datagram: &Arc<Vec<u8>>,
        current_gap: u32,
    ) -> (Option<DredInfo>, u32) {
        let (dred, dred_parse, available_horizon_samples) = Self::parse_dred_for_insert(
            self.dred_decoder.as_mut(),
            sequence,
            datagram,
            current_gap,
        );
        if let Some(dred_parse) = dred_parse {
            self.dred_parse = Some(dred_parse);
        }
        (dred, available_horizon_samples)
    }

    fn parse_dred_for_insert(
        dred_decoder: Option<&mut DredDecoder>,
        sequence: u32,
        datagram: &Arc<Vec<u8>>,
        current_gap: u32,
    ) -> (Option<DredInfo>, Option<DredParse>, u32) {
        if current_gap == 0 {
            return (None, None, 0);
        }
        let Some(decoder) = dred_decoder else {
            return (None, None, 0);
        };
        let available_horizon_samples =
            Self::parse_dred_state(decoder, datagram, LIVE_PLAYBACK_DRED_MAX_SAMPLES)
                .map(|(_, reach, _)| reach as u32)
                .unwrap_or_default();
        let max_dred_samples = usize::try_from(current_gap)
            .unwrap_or(usize::MAX)
            .saturating_add(DRED_PARSE_MARGIN_SAMPLES)
            .min(LIVE_PLAYBACK_DRED_MAX_SAMPLES);
        let Some((state, reach, dred_end)) =
            Self::parse_dred_state(decoder, datagram, max_dred_samples)
        else {
            return (None, None, available_horizon_samples);
        };
        if reach == 0 {
            return (None, None, available_horizon_samples);
        }
        let dred_parse = DredParse {
            sequence: Some(sequence),
            state,
            processed: false,
            pcm: Vec::with_capacity(2 * DRED_CHUNK_SAMPLES),
            span: 0,
        };
        (
            Some(DredInfo {
                samples: reach as i32,
                dred_end,
            }),
            Some(dred_parse),
            available_horizon_samples,
        )
    }

    fn parse_dred_state(
        decoder: &mut DredDecoder,
        datagram: &[u8],
        max_dred_samples: usize,
    ) -> Option<(DredState, usize, i32)> {
        let mut state = DredState::new().ok()?;
        let mut dred_end = 0;
        let reach = decoder
            .parse(
                &mut state,
                datagram,
                max_dred_samples,
                SampleRate::Hz48000,
                &mut dred_end,
                true,
            )
            .unwrap_or(0);
        Some((state, reach, dred_end))
    }

    /// Produces one 10 ms output block. Port of `NetEqImpl::GetAudioInternal`.
    /// `now` is the wall-clock time of this pull.
    pub(crate) fn get_audio(&mut self, now: Instant, output: &mut [f32]) -> AudioResult {
        debug_assert_eq!(output.len(), OUTPUT_SIZE_SAMPLES);
        self.tick_timer.increment();
        self.observe_wall_clock(now);

        // Note: WebRTC's `enable_muted_state_` short-circuit (zero-fill once expand
        // has muted) defaults to off, and using it here breaks the resume catch-up
        // because it freezes the sync buffer and bypasses GetDecision. We follow
        // the default: keep running full GetDecision/Expand, which mutes itself to
        // ~0 via the expand mute-slope, so an idle stream decays to silence while a
        // resuming talkspurt still has continuous concealment to merge against.
        let (mut operation, packet_list) = self.get_decision();
        let (decoded_len, mut source) = self.decode(&mut operation, packet_list);

        let mut time_stretched = 0;
        match operation {
            Operation::Normal => self.do_normal(decoded_len),
            Operation::Merge => {
                self.do_merge(decoded_len);
                source = DecodedFrameSource::Expand;
            }
            Operation::Expand => {
                self.do_expand();
                source = DecodedFrameSource::Expand;
            }
            Operation::Accelerate | Operation::FastAccelerate => {
                let fast = operation == Operation::FastAccelerate;
                time_stretched = self.do_accelerate(decoded_len, fast);
            }
            Operation::PreemptiveExpand => {
                time_stretched = -self.do_preemptive_expand(decoded_len);
            }
            // No CNG/DTMF path in Chatt; treat as expand to stay safe.
            Operation::Rfc3389Cng
            | Operation::Rfc3389CngNoPacket
            | Operation::Dtmf
            | Operation::Undefined => {
                self.do_expand();
                source = DecodedFrameSource::Expand;
            }
        }

        // Extract the output block from the sync buffer and reinstall the
        // overlap lookahead, mirroring GetAudioInternal lines 814-832. NetEQ
        // works in i16; convert to the f32 the mixer/ring consume after.
        let mut block = [0i16; OUTPUT_SIZE_SAMPLES];
        let got_audio = self.sync_buffer.get_next_audio(&mut block);
        if got_audio {
            for (dst, &src) in output.iter_mut().zip(block.iter()) {
                *dst = src as f32 / 32768.0;
            }
        } else {
            output.fill(0.0);
        }
        let future = self.sync_buffer.future_length();
        if future < EXPAND_OVERLAP_LENGTH {
            let missing = EXPAND_OVERLAP_LENGTH - future;
            let next = self.sync_buffer.next_index();
            self.sync_buffer
                .set_next_index(next.saturating_sub(missing));
        }

        // Receiver mute fade, applied to the final output of every operation so it
        // ramps smoothly to silence on mute (continuing across the normal→expand
        // boundary) and back up on unmute, regardless of which NetEQ operation
        // produced the block. `mute_target` is held across concealment.
        self.mute_gain = apply_gain_ramp(
            output,
            self.mute_gain,
            self.mute_target,
            self.mute_gain_step,
        );

        // Background noise tracks audio written straight from the decoder.
        if matches!(
            self.last_mode,
            Mode::Normal | Mode::AccelerateFail | Mode::PreemptiveExpandFail
        ) {
            self.background_noise.update(self.sync_buffer.data());
        }

        // Reset the generated-noise stopwatch unless we are still concealing.
        if !self.last_mode.is_expand() && !self.last_mode.is_cng() {
            self.generated_noise_stopwatch = None;
        }

        AudioResult {
            mode: self.last_mode,
            source,
            // "Muted" once expand has decayed to silence: the stream is idle and
            // the output is ~0, which the worker uses only to mark voice inactive.
            muted: self.last_mode.is_expand() && self.expand.muted(),
            time_stretched,
        }
    }

    /// Port of `NetEqImpl::GetDecision`: build the status snapshot, ask the
    /// controller, post-process the decision, and extract the packets to decode.
    fn get_decision(&mut self) -> (Operation, Vec<Packet>) {
        let packet_list = std::mem::take(&mut self.packet_scratch);
        debug_assert!(packet_list.is_empty());
        let end_timestamp = self.sync_buffer.end_timestamp();
        if !self.new_codec {
            let five_seconds = 5 * FS_HZ as u32;
            self.packet_buffer
                .discard_old_packets(end_timestamp, five_seconds);
        }

        let samples_left = self
            .sync_buffer
            .future_length()
            .saturating_sub(EXPAND_OVERLAP_LENGTH) as i32;
        if self.last_mode.is_timestretch() {
            self.decision_logic
                .add_sample_memory(-(samples_left + OUTPUT_SIZE_SAMPLES as i32));
        }

        let status = self.status();

        let mut operation =
            self.decision_logic
                .get_decision(&status, &self.tick_timer, self.wall_now_samples);
        self.last_operation = operation;
        self.last_decision_reason = self.decision_reason(operation, &status);

        // Enough decoded samples already buffered → just play them out.
        if samples_left >= OUTPUT_SIZE_SAMPLES as i32
            && !matches!(
                operation,
                Operation::Merge
                    | Operation::Accelerate
                    | Operation::FastAccelerate
                    | Operation::PreemptiveExpand
            )
        {
            self.last_operation = Operation::Normal;
            self.last_decision_reason = "decoded_buffer_ready";
            return (Operation::Normal, packet_list);
        }

        if self.new_codec || operation == Operation::Undefined {
            if let Some(packet) = self.packet_buffer.peek_next_packet() {
                self.timestamp = packet.timestamp;
                if operation != Operation::Rfc3389Cng {
                    operation = Operation::Normal;
                }
            } else {
                operation = Operation::Expand;
            }
            self.last_operation = operation;
            self.last_decision_reason = self.decision_reason(operation, &status);
            self.sync_buffer
                .increase_end_timestamp(self.timestamp.wrapping_sub(end_timestamp));
            self.new_codec = false;
            self.decision_logic.soft_reset(&self.tick_timer);
            let result = self.finish_decision(operation, self.timestamp, packet_list);
            self.last_operation = result.0;
            self.last_decision_reason = self.decision_reason(result.0, &status);
            return result;
        }

        let result = self.finish_decision(operation, end_timestamp, packet_list);
        self.last_operation = result.0;
        self.last_decision_reason = self.decision_reason(result.0, &status);
        result
    }

    fn status(&self) -> NetEqStatus {
        let next_packet = self
            .packet_buffer
            .peek_next_packet()
            .map(|packet| PacketInfo {
                timestamp: packet.timestamp,
                is_cng: false,
            });
        NetEqStatus {
            target_timestamp: self.sync_buffer.end_timestamp(),
            expand_mutefactor: self.expand.mute_factor(),
            next_packet,
            last_mode: self.last_mode,
            play_dtmf: false,
            generated_noise_samples: self.generated_noise_samples() as usize,
            packet_buffer_info: PacketBufferInfo {
                dtx_or_cng: false,
                span_samples: self.packet_buffer.span_samples(false, &self.tick_timer),
                span_samples_wait_time: self.packet_buffer.span_samples(true, &self.tick_timer),
                num_packets: self.packet_buffer.num_packets(),
            },
            sync_buffer_samples: self.sync_buffer.future_length(),
        }
    }

    fn decision_reason(&self, operation: Operation, status: &NetEqStatus) -> &'static str {
        match operation {
            Operation::Normal => {
                if status.next_packet.is_some() {
                    "expected_packet"
                } else {
                    "decoded_buffer_ready"
                }
            }
            Operation::Expand => {
                if status.next_packet.is_none() {
                    "no_packet"
                } else if self.packet_too_early_for_status(status) {
                    "packet_too_early_below_target"
                } else {
                    "concealment"
                }
            }
            Operation::Merge => "merge_after_expand",
            Operation::Accelerate | Operation::FastAccelerate => "above_target",
            Operation::PreemptiveExpand => "below_target",
            Operation::Rfc3389CngNoPacket => "cng_no_packet",
            Operation::Rfc3389Cng => "cng",
            Operation::Dtmf => "dtmf",
            Operation::Undefined => "undefined",
        }
    }

    fn packet_too_early_for_status(&self, status: &NetEqStatus) -> bool {
        let Some(next) = status.next_packet else {
            return false;
        };
        let timestamp_leap = next.timestamp.wrapping_sub(status.target_timestamp) as usize;
        timestamp_leap > status.generated_noise_samples
    }

    /// The required-samples sizing and packet extraction tail of `GetDecision`
    /// (reference lines 1065-1186).
    fn finish_decision(
        &mut self,
        mut operation: Operation,
        end_timestamp: u32,
        mut packet_list: Vec<Packet>,
    ) -> (Operation, Vec<Packet>) {
        let samples_left = self
            .sync_buffer
            .future_length()
            .saturating_sub(EXPAND_OVERLAP_LENGTH) as i32;
        let mut required_samples = OUTPUT_SIZE_SAMPLES;

        match operation {
            Operation::Expand => {
                self.timestamp = end_timestamp;
                return (operation, packet_list);
            }
            Operation::Rfc3389CngNoPacket => {
                return (operation, packet_list);
            }
            Operation::Accelerate | Operation::FastAccelerate => {
                if samples_left >= SAMPLES_30_MS as i32 {
                    self.decision_logic.set_sample_memory(samples_left);
                    self.decision_logic.set_prev_time_scale(true);
                    return (operation, packet_list);
                } else if samples_left >= SAMPLES_10_MS as i32
                    && self.decoder_frame_length >= SAMPLES_30_MS
                {
                    return (Operation::Normal, packet_list);
                } else if samples_left < SAMPLES_20_MS as i32
                    && self.decoder_frame_length < SAMPLES_30_MS
                {
                    required_samples = 2 * OUTPUT_SIZE_SAMPLES;
                    operation = Operation::Normal;
                }
            }
            Operation::PreemptiveExpand => {
                if samples_left >= SAMPLES_30_MS as i32
                    || (samples_left >= SAMPLES_10_MS as i32
                        && self.decoder_frame_length >= SAMPLES_30_MS)
                {
                    self.decision_logic.set_sample_memory(samples_left);
                    self.decision_logic.set_prev_time_scale(true);
                    return (operation, packet_list);
                }
                if samples_left < SAMPLES_20_MS as i32 && self.decoder_frame_length < SAMPLES_30_MS
                {
                    required_samples = 2 * OUTPUT_SIZE_SAMPLES;
                }
            }
            Operation::Merge => {
                // `Merge::RequiredFutureSamples` is `fs_hz / 100` (10 ms), so the
                // reference computes `max(output_size, output_size)`, leaving
                // `required_samples` at one output block. Pulling 30 ms here
                // over-extracted an extra frame on every loss recovery, inflating
                // the buffer by ~20 ms per merge.
                required_samples = required_samples.max(OUTPUT_SIZE_SAMPLES);
            }
            _ => {}
        }

        let mut extracted_samples = 0i32;
        if let Some(first_timestamp) = self.packet_buffer.next_timestamp() {
            self.sync_buffer
                .increase_end_timestamp(first_timestamp.wrapping_sub(end_timestamp));
            extracted_samples = self.extract_packets(required_samples, &mut packet_list);
        }

        if matches!(
            operation,
            Operation::Accelerate | Operation::FastAccelerate | Operation::PreemptiveExpand
        ) {
            self.decision_logic
                .set_sample_memory(samples_left + extracted_samples);
            self.decision_logic.set_prev_time_scale(true);
        }

        if matches!(operation, Operation::Accelerate | Operation::FastAccelerate)
            && extracted_samples + samples_left < SAMPLES_30_MS as i32
        {
            operation = Operation::Normal;
        }

        self.timestamp = self.sync_buffer.end_timestamp();
        (operation, packet_list)
    }

    /// Port of `NetEqImpl::ExtractPackets`: pop contiguous packets covering
    /// `required_samples`, returning the total media samples spanned.
    fn extract_packets(&mut self, required_samples: usize, packet_list: &mut Vec<Packet>) -> i32 {
        let Some(first_timestamp) = self.packet_buffer.next_timestamp() else {
            return 0;
        };
        let mut extracted_samples = 0usize;
        loop {
            let Some(packet) = self.packet_buffer.get_next_packet() else {
                break;
            };
            self.timestamp = packet.timestamp;
            let mut packet_duration = packet.duration_samples;
            if packet_duration == 0 {
                packet_duration = self.decoder_frame_length;
            }
            extracted_samples =
                (packet.timestamp.wrapping_sub(first_timestamp) as usize) + packet_duration;
            let payload_timestamp = packet.timestamp;
            packet_list.push(packet);

            let next_available = self.packet_buffer.peek_next_packet().is_some_and(|next| {
                next.timestamp == payload_timestamp.wrapping_add(packet_duration as u32)
            });
            if extracted_samples >= required_samples || !next_available {
                break;
            }
        }
        if extracted_samples > 0 {
            self.packet_buffer.discard_old_packets(self.timestamp, 0);
        }
        extracted_samples as i32
    }

    /// Port of `NetEqImpl::Decode` + `DecodeLoop`: decode the extracted packets
    /// into `decoded_buffer_`. Returns `(decoded_length, source)`.
    fn decode(
        &mut self,
        operation: &mut Operation,
        mut packet_list: Vec<Packet>,
    ) -> (usize, DecodedFrameSource) {
        if self.reset_decoder {
            let _ = self.decoder.reset();
            self.reset_decoder = false;
        }
        if packet_list.is_empty() {
            self.packet_scratch = packet_list;
            return (0, DecodedFrameSource::Normal);
        }

        let mut decoded_length = 0usize;
        let mut source = DecodedFrameSource::Normal;
        let mut had_error = false;
        for packet in &packet_list {
            if packet.priority.codec_level == 2 {
                source = DecodedFrameSource::Dred;
            }
            if packet.priority.codec_level == 1 {
                source = DecodedFrameSource::Fec;
            }
            if packet.priority.codec_level == 0 {
                // Hold this target across the following concealment too.
                self.mute_target = if packet.muted { 0.0 } else { 1.0 };
            }
            let out_start = decoded_length;
            let decoded = self.decode_one(packet, out_start);
            match decoded {
                Ok(samples) if samples > 0 => {
                    decoded_length += samples;
                    self.decoder_frame_length = samples;
                }
                Ok(_) => {}
                Err(()) => {
                    had_error = true;
                    break;
                }
            }
        }

        for packet in packet_list.drain(..) {
            self.packet_buffer.retire(packet);
        }
        self.packet_scratch = packet_list;

        if had_error {
            // Reference: a decode error rewinds to expand and advances the clock.
            self.sync_buffer
                .increase_end_timestamp(self.decoder_frame_length as u32);
            *operation = Operation::Expand;
            return (0, DecodedFrameSource::DecodeError);
        }

        self.sync_buffer
            .increase_end_timestamp(decoded_length as u32);
        (decoded_length, source)
    }

    /// Decodes a single extracted packet into `decoded_buffer[out_start..]`.
    fn decode_one(&mut self, packet: &Packet, out_start: usize) -> Result<usize, ()> {
        match &packet.payload {
            PacketPayload::Opus(bytes) => {
                let end = (out_start + MAX_OPUS_DECODE_SAMPLES).min(self.decoded_buffer.len());
                let output = &mut self.decoded_buffer[out_start..end];
                self.decoder.decode(bytes, output, false).map_err(|_| ())
            }
            PacketPayload::OpusFec(bytes) => {
                let end = (out_start + LIVE_OPUS_FRAME_SAMPLES).min(self.decoded_buffer.len());
                let output = &mut self.decoded_buffer[out_start..end];
                self.decoder.decode(bytes, output, true).map_err(|_| ())
            }
            PacketPayload::Dred { source, offset } => {
                self.decode_dred(packet.sequence_number, source, *offset, out_start)
            }
        }
    }

    fn decode_dred(
        &mut self,
        sequence: u32,
        source: &Arc<Vec<u8>>,
        offset: i32,
        out_start: usize,
    ) -> Result<usize, ()> {
        let Some(dred_decoder) = self.dred_decoder.as_mut() else {
            return Ok(0);
        };
        let parse = self.dred_parse.as_mut().ok_or(())?;
        // Reuse the cached parse from insertion when it is for this packet.
        // Otherwise re-parse into the same slot: reset() re-zeroes the existing
        // DredState in place, so this fallback never touches the allocator.
        if parse.sequence != Some(sequence) {
            parse.state.reset();
            let mut dred_end = 0;
            let max_dred_samples = usize::try_from(offset.max(DRED_CHUNK_SAMPLES as i32))
                .unwrap_or(usize::MAX)
                .saturating_add(DRED_PARSE_MARGIN_SAMPLES)
                .min(LIVE_PLAYBACK_DRED_MAX_SAMPLES);
            let _ = dred_decoder.parse(
                &mut parse.state,
                source,
                max_dred_samples,
                SampleRate::Hz48000,
                &mut dred_end,
                true,
            );
            parse.sequence = Some(sequence);
            parse.processed = false;
            parse.pcm.clear();
            parse.span = 0;
        }
        if !parse.processed {
            dred_decoder
                .process_in_place(&mut parse.state)
                .map_err(|_| ())?;
            parse.processed = true;
        }
        let decoder = &mut self.decoder;
        let end = (out_start + DRED_CHUNK_SAMPLES).min(self.decoded_buffer.len());
        let output = &mut self.decoded_buffer[out_start..end];
        let Ok(offset) = usize::try_from(offset) else {
            return Err(());
        };
        // Serve the tail chunk from the pair cache when the previous pull
        // decoded it (see below).
        if parse.span == offset + DRED_CHUNK_SAMPLES && parse.pcm.len() == 2 * DRED_CHUNK_SAMPLES {
            let served = output.len().min(DRED_CHUNK_SAMPLES);
            output[..served].copy_from_slice(&parse.pcm[DRED_CHUNK_SAMPLES..][..served]);
            return Ok(served);
        }
        // The size of the deep-PLC call that ends at the anchor decides how
        // cleanly the decoder blends into the following real decode: a final
        // 10 ms call triples the transition click vs a final 20 ms call, while
        // the sizes of earlier calls are irrelevant. So the last two chunks are
        // decoded as one 20 ms pass and the tail chunk is served from `pcm`
        // above. Chunks are otherwise decoded one 10 ms pull at a time, so a
        // late real packet replacing a buffered chunk never leaves the decoder
        // state advanced past audio that actually played.
        if offset == 2 * DRED_CHUNK_SAMPLES {
            parse.pcm.clear();
            parse.pcm.resize(2 * DRED_CHUNK_SAMPLES, 0);
            parse.span = offset;
            dred_decoder
                .decode_into_i16(decoder, &parse.state, offset as i32, &mut parse.pcm)
                .map_err(|_| ())?;
            let served = output.len().min(DRED_CHUNK_SAMPLES);
            output[..served].copy_from_slice(&parse.pcm[..served]);
            return Ok(served);
        }
        dred_decoder
            .decode_into_i16(decoder, &parse.state, offset as i32, output)
            .map_err(|_| ())
    }

    /// Port of `NetEqImpl::DoNormal`: copy decoded audio (with an
    /// expand-to-normal transition when recovering from concealment) into the
    /// sync buffer.
    fn do_normal(&mut self, decoded_len: usize) {
        if decoded_len == 0 {
            return;
        }
        let decoded = &self.decoded_buffer[..decoded_len];
        if matches!(self.last_mode, Mode::Expand) {
            // Resuming after concealment: unmute and cross-fade the seam.
            // `process_after_expand` reads/writes the sync buffer and resets
            // Expand internally (matching `Normal::Process`).
            normal::process_after_expand(
                decoded,
                self.sync_buffer.data_mut(),
                &mut self.expand,
                &mut self.background_noise,
                &mut self.random_vector,
                &mut self.scratch.expand,
                &mut self.scratch.expand_out,
                &mut self.scratch.op_out,
            );
            self.sync_buffer.push_back(&self.scratch.op_out);
        } else {
            self.sync_buffer.push_back(decoded);
        }
        self.last_mode = Mode::Normal;
    }

    /// Port of `NetEqImpl::DoMerge`: merge decoded audio onto the expand tail.
    fn do_merge(&mut self, decoded_len: usize) {
        let decoded = &self.decoded_buffer[..decoded_len];
        let next_index = self.sync_buffer.next_index();
        merge::process(
            decoded,
            self.sync_buffer.data_mut(),
            next_index,
            &mut self.expand,
            &mut self.background_noise,
            &mut self.random_vector,
            &mut self.scratch,
        );
        self.sync_buffer.push_back(&self.scratch.op_out);
        self.expand.reset();
        self.last_mode = Mode::Merge;
    }

    /// Port of `NetEqImpl::DoExpand`: synthesize concealment until the sync
    /// buffer holds one output block past the overlap lookahead.
    fn do_expand(&mut self) {
        if self.generated_noise_stopwatch.is_none() {
            self.generated_noise_stopwatch = Some(self.tick_timer.new_stopwatch());
        }
        // Fast path for an idle stream. Once Expand has muted to silence and no
        // comfort-noise floor is active, every synthesized concealment sample is
        // exactly zero, so the LPC synthesis, the full-history copy, and the
        // per-call allocations are wasted work. Shift in reused silence instead.
        // The output is bit-identical to the synthesized path and resume stays
        // faithful: a returning packet still merges against the zero tail exactly
        // as it would have against the muted (×0) synthesis.
        if self.expand.muted() && !self.background_noise.initialized() {
            let target = OUTPUT_SIZE_SAMPLES + EXPAND_OVERLAP_LENGTH;
            let future = self.sync_buffer.future_length();
            if future < target {
                self.sync_buffer.push_back_zeros(target - future);
            }
            self.last_mode = Mode::Expand;
            return;
        }
        let mut guard = 0;
        while self
            .sync_buffer
            .future_length()
            .saturating_sub(EXPAND_OVERLAP_LENGTH)
            < OUTPUT_SIZE_SAMPLES
        {
            // `process` analyzes the history on the first chunk of a run and
            // overlap-adds into the sync-buffer tail, then fills `out` with the
            // new concealment samples to append.
            let out = &mut self.scratch.expand_out;
            self.expand.process(
                self.sync_buffer.data_mut(),
                &mut self.background_noise,
                &mut self.random_vector,
                &mut self.scratch.expand,
                out,
            );
            if out.is_empty() {
                // No history to extrapolate from: emit silence to make progress.
                self.sync_buffer.push_back_zeros(OUTPUT_SIZE_SAMPLES);
            } else {
                self.sync_buffer.push_back(&out);
            }
            self.last_mode = Mode::Expand;
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        self.last_mode = Mode::Expand;
    }

    /// Port of `NetEqImpl::DoAccelerate`: borrow 30 ms across the decoded buffer
    /// and the sync tail, time-compress one pitch period, and write back.
    fn do_accelerate(&mut self, decoded_len: usize, fast: bool) -> i32 {
        let borrowed = self.build_timescale_input(decoded_len);
        let TimescaleScratch { input, output, .. } = &mut self.scratch.timescale;
        let result = time_stretch::accelerate_process(input, fast, &self.background_noise, output);
        self.last_mode = match result.return_code {
            ReturnCode::Success => Mode::AccelerateSuccess,
            ReturnCode::SuccessLowEnergy => Mode::AccelerateLowEnergy,
            ReturnCode::NoStretch | ReturnCode::Error => Mode::AccelerateFail,
        };
        let removed = result.length_change_samples;
        self.apply_timescale_result(borrowed);
        self.expand.reset();
        removed as i32
    }

    /// Port of `NetEqImpl::DoPreemptiveExpand`: insert one pitch period.
    fn do_preemptive_expand(&mut self, decoded_len: usize) -> i32 {
        // `old_data_length` is how many of the borrowed samples were already
        // played out (came from before `next_index`), exactly as
        // `NetEqImpl::DoPreemptiveExpand` computes `old_borrowed_samples`.
        let future = self.sync_buffer.future_length();
        let borrowed = TIMESCALE_REQUIRED_SAMPLES.saturating_sub(decoded_len);
        let old_data_length = borrowed.saturating_sub(future);
        let overlap_samples = self.expand.overlap_length();
        let borrowed = self.build_timescale_input(decoded_len);
        let TimescaleScratch { input, output, .. } = &mut self.scratch.timescale;
        let result = time_stretch::preemptive_expand_process(
            input,
            old_data_length,
            overlap_samples,
            &self.background_noise,
            output,
        );
        self.last_mode = match result.return_code {
            ReturnCode::Success => Mode::PreemptiveExpandSuccess,
            ReturnCode::SuccessLowEnergy => Mode::PreemptiveExpandLowEnergy,
            ReturnCode::NoStretch | ReturnCode::Error => Mode::PreemptiveExpandFail,
        };
        let added = result.length_change_samples;
        self.apply_timescale_result(borrowed);
        self.expand.reset();
        added as i32
    }

    /// Builds the 30 ms time-scale input into `scratch.timescale.input`: the
    /// decoded buffer prefixed with samples borrowed from the end of the sync
    /// buffer when the decoder gave fewer than 30 ms. Mirrors the
    /// `ReadInterleavedFromEnd` borrow in `DoAccelerate`/`DoPreemptiveExpand`.
    /// Returns the borrowed sample count.
    fn build_timescale_input(&mut self, decoded_len: usize) -> usize {
        let TimescaleScratch { input, tail, .. } = &mut self.scratch.timescale;
        input.clear();
        if decoded_len >= TIMESCALE_REQUIRED_SAMPLES {
            input.extend_from_slice(&self.decoded_buffer[..decoded_len]);
            return 0;
        }
        let borrowed = TIMESCALE_REQUIRED_SAMPLES - decoded_len;
        self.sync_buffer.read_from_end(borrowed, tail);
        // The borrow may be shorter than requested if the buffer is small; pad to
        // keep the analysis window full.
        if tail.len() < borrowed {
            input.resize(borrowed - tail.len(), 0);
        }
        input.extend_from_slice(tail);
        input.extend_from_slice(&self.decoded_buffer[..decoded_len]);
        borrowed
    }

    /// Writes the time-scaled `scratch.timescale.output` back, restoring the
    /// borrowed sync tail in place and pushing the remainder. Port of the
    /// borrow-restore tail of `DoAccelerate`/`DoPreemptiveExpand`.
    fn apply_timescale_result(&mut self, borrowed: usize) {
        let algorithm = &mut self.scratch.timescale.output;
        let mut start = 0;
        if borrowed > 0 {
            let size = self.sync_buffer.size();
            if algorithm.len() < borrowed {
                let len = algorithm.len();
                self.sync_buffer
                    .replace_at_index(algorithm, len, size - borrowed);
                self.sync_buffer.push_front_zeros(borrowed - len);
                start = algorithm.len();
            } else {
                self.sync_buffer.replace_at_index(
                    &algorithm[..borrowed],
                    borrowed,
                    size - borrowed,
                );
                start = borrowed;
            }
        }
        if start < algorithm.len() {
            self.sync_buffer.push_back(&algorithm[start..]);
        }
    }

    fn generated_noise_samples(&self) -> u64 {
        self.generated_noise_stopwatch
            .as_ref()
            .map(|watch| watch.elapsed_ticks(&self.tick_timer) * OUTPUT_SIZE_SAMPLES as u64)
            .unwrap_or(0)
            + self.decision_logic.noise_fast_forward() as u64
    }
}

fn duration_to_i32_ms(duration: std::time::Duration) -> i32 {
    duration_to_ms(duration).min(i32::MAX as u64) as i32
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::audio::capture::OpusVoiceEncoder;
    use crate::network::{EncoderNetworkProfile, EncoderNetworkTuning};

    /// Wall clock advancing like a real device: 10 ms per `get_audio` pull.
    struct WallClock(Instant);

    impl WallClock {
        fn new() -> Self {
            Self(Instant::now())
        }

        fn now(&self) -> Instant {
            self.0
        }

        fn advance_block(&mut self) {
            self.0 += Duration::from_millis(10);
        }
    }

    fn encode_tone(encoder: &mut OpusVoiceEncoder, base: usize) -> Vec<u8> {
        let frame: Vec<i16> = (0..LIVE_OPUS_FRAME_SAMPLES)
            .map(|n| {
                let value = (2.0 * std::f32::consts::PI * 220.0 * (base + n) as f32
                    / SAMPLE_RATE as f32)
                    .sin()
                    * 0.3
                    * i16::MAX as f32;
                value.round() as i16
            })
            .collect();
        let mut output = vec![0u8; 4_000];
        let len = encoder.encode(&frame, &mut output).expect("encode");
        output.truncate(len);
        output
    }

    /// One output block's observable state from [`run_tone_stream`].
    struct ToneBlock {
        mode: Mode,
        source: DecodedFrameSource,
        reason: &'static str,
        target_ms: u64,
        packet_buffer_ms: u64,
    }

    impl ToneBlock {
        #[allow(dead_code)]
        fn describe(&self) -> String {
            format!(
                "{}/{:?}/{} t={} b={}",
                self.mode.label(),
                self.source,
                self.reason,
                self.target_ms,
                self.packet_buffer_ms
            )
        }
    }

    /// Runs a continuous-phase tone stream through NetEQ under an arbitrary
    /// delivery schedule. `schedule` maps a packet's sequence to its arrival
    /// tick (one tick per 10 ms output block, nominal send tick is `seq * 2`)
    /// or `None` to drop it. The encoder still sees every frame so phase stays
    /// continuous. `fec` applies the live encoder loss profile so packets carry
    /// LBRR. `dred` controls the receiver's DRED decoder. Returns all output
    /// samples plus per-block state.
    fn run_tone_stream(
        total: u32,
        fec: bool,
        dred: bool,
        schedule: impl Fn(u32) -> Option<u32>,
    ) -> (Vec<f32>, Vec<ToneBlock>) {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        if !dred {
            core.dred_decoder = None;
        }
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        if fec {
            encoder
                .apply_network_profile(EncoderNetworkProfile::CRITICAL)
                .unwrap();
        }
        let mut arrivals: Vec<Vec<(u32, Vec<u8>)>> = vec![Vec::new(); total as usize * 2 + 1];
        for seq in 0..total {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            let Some(tick) = schedule(seq) else { continue };
            let tick = (tick as usize).min(arrivals.len() - 1);
            arrivals[tick].push((seq, payload));
        }

        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        let mut clock = WallClock::new();
        let mut samples = Vec::new();
        let mut blocks = Vec::new();
        for tick_arrivals in &arrivals {
            for (seq, payload) in tick_arrivals {
                core.insert_packet(
                    clock.now(),
                    seq * LIVE_OPUS_FRAME_SAMPLES as u32,
                    *seq,
                    0,
                    payload,
                );
            }
            let result = core.get_audio(clock.now(), &mut output);
            clock.advance_block();
            samples.extend_from_slice(&output);
            let diagnostics = core.diagnostics();
            blocks.push(ToneBlock {
                mode: result.mode,
                source: result.source,
                reason: diagnostics.reason,
                target_ms: diagnostics.target_ms,
                packet_buffer_ms: diagnostics.packet_buffer_ms,
            });
        }
        (samples, blocks)
    }

    /// Peak 3-tap linear-prediction residual of a `freq` Hz tone over
    /// `samples[skip..]`. A pure sinusoid is predicted exactly, so the residual
    /// sits at the codec noise floor and any phase or level splice spikes above
    /// it. See the identical helper in the sim harness tests.
    fn peak_tone_prediction_residual(samples: &[f32], freq: f64, skip: usize) -> (f32, usize) {
        let w = 2.0 * std::f64::consts::PI * freq / SAMPLE_RATE as f64;
        let two_cos_w = 2.0 * w.cos() as f32;
        let mut peak = 0.0f32;
        let mut peak_index = 0;
        for (index, window) in samples.windows(3).enumerate().skip(skip) {
            let residual = (window[2] - two_cos_w * window[1] + window[0]).abs();
            if residual > peak {
                peak = residual;
                peak_index = index;
            }
        }
        (peak, peak_index)
    }

    /// A short timeline hole reaches the playout point while later packets sit
    /// buffered, so NetEQ expands into the gap and then merges back into
    /// decoded audio. The expand entry and merge return splices must not click.
    #[test]
    fn short_gap_expand_merge_splice_stays_smooth() {
        let freq = 220.0;
        let skip = (SAMPLE_RATE as usize * 3) / 20;

        let (control, control_blocks) = run_tone_stream(200, false, false, |seq| Some(seq * 2));
        let (control_residual, _) = peak_tone_prediction_residual(&control, freq, skip);
        assert!(
            !control_blocks[20..]
                .iter()
                .any(|block| block.mode.is_expand()),
            "control run concealed unexpectedly"
        );

        let (output, blocks) = run_tone_stream(200, false, false, |seq| {
            if seq == 100 || seq == 101 {
                None
            } else {
                Some(seq * 2)
            }
        });
        let (residual, _) = peak_tone_prediction_residual(&output, freq, skip);
        let gap_modes: Vec<&str> = blocks[190..215]
            .iter()
            .map(|block| block.mode.label())
            .collect();
        assert!(
            blocks[190..215].iter().any(|block| block.mode.is_expand()),
            "the hole never reached concealment: {gap_modes:?}"
        );
        assert!(
            residual < 3.0 * control_residual.max(0.002),
            "expand/merge splice injected a discontinuity: residual {residual:.5} \
             vs control {control_residual:.5}, modes {gap_modes:?}"
        );
    }

    /// The same hole under E2E-like conditions: FEC/DRED-bearing packets and
    /// jittery bursty arrivals that inflate the delay target so the buffer runs
    /// deep with time-stretch churn before the hole reaches playout. Catches
    /// both the 10 ms-final-DRED-call blend regression and DRED interruption
    /// over-advance.
    #[test]
    fn short_gap_splice_stays_smooth_under_jitter_and_fec() {
        let freq = 220.0;
        let skip = (SAMPLE_RATE as usize * 3) / 20;
        let jitter = |seq: u32| {
            let hold = (seq % 4) * 2 + u32::from(seq % 7 == 0) * 3;
            Some(seq * 2 + hold)
        };

        let (control, _) = run_tone_stream(500, true, true, jitter);
        let (control_residual, _) = peak_tone_prediction_residual(&control, freq, skip);

        let (output, blocks) = run_tone_stream(500, true, true, |seq| {
            if seq == 400 || seq == 401 {
                None
            } else {
                jitter(seq)
            }
        });
        let (residual, residual_index) = peak_tone_prediction_residual(&output, freq, skip);
        let peak_block = residual_index / OUTPUT_SIZE_SAMPLES;
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block.source, DecodedFrameSource::Dred)),
            "the hole never exercised DRED recovery"
        );
        // The DRED-to-real-decode boundary carries an inherent deep-PLC blend
        // residual of ~0.0066 even under libopus's own `opus_demo` consumption
        // pattern; the regressions this guards against (a 10 ms final DRED
        // call, or a gap-starved feature parse) measure 0.019+. 0.009 separates
        // the two with margin on both sides.
        assert!(
            residual < 0.009,
            "recovery splice injected a discontinuity: residual {residual:.5} \
             vs control {control_residual:.5} at block {peak_block} ({})",
            blocks[peak_block.saturating_sub(2)..(peak_block + 3).min(blocks.len())]
                .iter()
                .map(|block| block.describe())
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }

    #[test]
    fn steady_stream_plays_back_continuously() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        let mut total_energy = 0.0f64;
        let mut blocks = 0;
        let mut clock = WallClock::new();
        for seq in 0..60u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(
                clock.now(),
                seq * LIVE_OPUS_FRAME_SAMPLES as u32,
                seq,
                0,
                &payload,
            );
            // Two 10 ms output blocks per 20 ms packet.
            for _ in 0..2 {
                let result = core.get_audio(clock.now(), &mut output);
                clock.advance_block();
                if seq > 4 {
                    // After priming, audio should be flowing (normal or stretch).
                    assert!(!result.muted, "unexpected mute at seq {seq}");
                    total_energy += output.iter().map(|s| (s * s) as f64).sum::<f64>();
                    blocks += 1;
                }
            }
        }
        assert!(blocks > 0);
        assert!(total_energy > 0.0, "no audio produced");
    }

    /// A clip plays, the sender goes idle for a long stretch (NetEQ keeps being
    /// pulled and sustains Expand), then the clip resumes with a timestamp that
    /// leaped forward across the silence. The reported playout delay must stay
    /// bounded throughout: the sync-buffer end timestamp freezes during Expand,
    /// and folding `generated_noise_samples` back in keeps the playout position
    /// tracking wall-clock instead of ageing until it wraps (which previously
    /// drove constant Accelerate and a wildly inflated jitter-buffer readout).
    #[test]
    fn silence_then_resume_keeps_playout_delay_bounded() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        // Play a clip: 100 packets (2 s) of tone.
        let mut clock = WallClock::new();
        let mut seq = 0u32;
        let mut ts = 0u32;
        for _ in 0..100u32 {
            let payload = encode_tone(&mut encoder, (seq as usize) * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(clock.now(), ts, seq, 0, &payload);
            for _ in 0..2 {
                core.get_audio(clock.now(), &mut output);
                clock.advance_block();
            }
            seq += 1;
            ts += LIVE_OPUS_FRAME_SAMPLES as u32;
        }
        // Wait a while: 30 s of silence, NetEQ keeps being pulled (no packets).
        let mut max_silence = 0u64;
        for _ in 0..3000 {
            core.get_audio(clock.now(), &mut output);
            clock.advance_block();
            max_silence = max_silence.max(core.diagnostics().playout_delay_ms);
        }
        // Resume the clip. The capture media timestamp advanced across silence,
        // so the new packet's timestamp leaps forward by the silence duration.
        ts = ts.wrapping_add(30 * SAMPLE_RATE as u32);
        let mut max_resume = 0u64;
        for _ in 0..100u32 {
            let payload = encode_tone(&mut encoder, (seq as usize) * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(clock.now(), ts, seq, 0, &payload);
            for _ in 0..2 {
                core.get_audio(clock.now(), &mut output);
                clock.advance_block();
                max_resume = max_resume.max(core.diagnostics().playout_delay_ms);
            }
            seq += 1;
            ts += LIVE_OPUS_FRAME_SAMPLES as u32;
        }
        assert!(
            max_silence < 1_000 && max_resume < 1_000,
            "playout delay aged during sustained expand: silence={max_silence}ms resume={max_resume}ms"
        );
    }

    #[test]
    fn stale_resume_timestamp_after_idle_keeps_playout_delay_bounded() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];

        let mut clock = WallClock::new();
        let mut seq = 0u32;
        let mut ts = 0u32;
        let mut last_ts = 0u32;
        for _ in 0..100u32 {
            let payload = encode_tone(&mut encoder, (seq as usize) * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(clock.now(), ts, seq, 0, &payload);
            for _ in 0..2 {
                core.get_audio(clock.now(), &mut output);
                clock.advance_block();
            }
            last_ts = ts;
            seq += 1;
            ts += LIVE_OPUS_FRAME_SAMPLES as u32;
        }

        for _ in 0..3000 {
            core.get_audio(clock.now(), &mut output);
            clock.advance_block();
        }

        // A replayed source may restart its local media clock; if the sender only
        // rebases it just past the previous packet instead of across the whole
        // idle wall-clock gap, the receiver must discard the old arrival-history
        // baseline so diagnostics do not report the idle as queued latency.
        ts = last_ts.wrapping_add(OUTPUT_SIZE_SAMPLES as u32);
        let mut max_resume = 0u64;
        for offset in 0..100u32 {
            let flags = if offset == 0 {
                LIVE_PACKET_FLAG_OPUS_RESET
            } else {
                0
            };
            let payload = encode_tone(&mut encoder, (seq as usize) * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(clock.now(), ts, seq, flags, &payload);
            for _ in 0..2 {
                core.get_audio(clock.now(), &mut output);
                clock.advance_block();
                max_resume = max_resume.max(core.diagnostics().playout_delay_ms);
            }
            seq += 1;
            ts = ts.wrapping_add(LIVE_OPUS_FRAME_SAMPLES as u32);
        }

        assert!(
            max_resume < 1_000,
            "stale resume timestamp inflated playout delay to {max_resume}ms"
        );
    }

    /// A sender capture restart rebases its media clock to zero mid-stream while
    /// the wire sequence keeps counting. The receiver must flush and re-anchor
    /// immediately instead of expanding until the 200-packet buffer overflows
    /// (a multi-second outage).
    #[test]
    fn backward_timestamp_rebase_recovers_immediately() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        let mut clock = WallClock::new();
        let mut seq = 0u32;
        // Five minutes into the old timeline.
        let base_ts = 5 * 60 * SAMPLE_RATE;
        for offset in 0..100u32 {
            let payload = encode_tone(&mut encoder, (seq as usize) * LIVE_OPUS_FRAME_SAMPLES);
            let ts = base_ts + offset * LIVE_OPUS_FRAME_SAMPLES as u32;
            core.insert_packet(clock.now(), ts, seq, 0, &payload);
            for _ in 0..2 {
                core.get_audio(clock.now(), &mut output);
                clock.advance_block();
            }
            seq += 1;
        }

        // The capture pipeline restarts: timestamps rebase to zero, the first
        // packet carries OPUS_RESET, and the sequence keeps advancing.
        let mut blocks_until_audio = None;
        let mut blocks = 0usize;
        let mut max_playout = 0u64;
        for offset in 0..50u32 {
            let flags = if offset == 0 {
                LIVE_PACKET_FLAG_OPUS_RESET
            } else {
                0
            };
            let payload = encode_tone(&mut encoder, (seq as usize) * LIVE_OPUS_FRAME_SAMPLES);
            let ts = offset * LIVE_OPUS_FRAME_SAMPLES as u32;
            core.insert_packet(clock.now(), ts, seq, flags, &payload);
            for _ in 0..2 {
                let result = core.get_audio(clock.now(), &mut output);
                clock.advance_block();
                blocks += 1;
                max_playout = max_playout.max(core.diagnostics().playout_delay_ms);
                if blocks_until_audio.is_none()
                    && !result.muted
                    && output.iter().any(|sample| sample.abs() > 1e-3)
                {
                    blocks_until_audio = Some(blocks);
                }
            }
            seq += 1;
        }
        let blocks_until_audio =
            blocks_until_audio.expect("stream never recovered from the backward rebase");
        assert!(
            blocks_until_audio <= 20,
            "recovery took {blocks_until_audio} blocks ({}ms)",
            blocks_until_audio * 10
        );
        assert!(
            max_playout < 1_000,
            "rebased stream reported {max_playout}ms of playout delay"
        );
    }

    /// A network straggler carries an old sequence along with its old timestamp,
    /// so it must be reported late without flushing the healthy stream.
    #[test]
    fn late_straggler_does_not_rebase_timeline() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        let mut clock = WallClock::new();
        let base_ts = 5 * 60 * SAMPLE_RATE;
        for seq in 0..300u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            let ts = base_ts + seq * LIVE_OPUS_FRAME_SAMPLES as u32;
            core.insert_packet(clock.now(), ts, seq, 0, &payload);
            for _ in 0..2 {
                core.get_audio(clock.now(), &mut output);
                clock.advance_block();
            }
        }

        // A packet from four seconds ago finally arrives: old sequence, old
        // timestamp, both far outside the rebase tolerance.
        let straggler = encode_tone(&mut encoder, 0);
        let late = core.insert_packet(
            clock.now(),
            base_ts + 100 * LIVE_OPUS_FRAME_SAMPLES as u32,
            100,
            0,
            &straggler,
        );
        assert!(late, "straggler should be reported late");

        let payload = encode_tone(&mut encoder, 300 * LIVE_OPUS_FRAME_SAMPLES);
        core.insert_packet(
            clock.now(),
            base_ts + 300 * LIVE_OPUS_FRAME_SAMPLES as u32,
            300,
            0,
            &payload,
        );
        let result = core.get_audio(clock.now(), &mut output);
        assert!(
            !result.muted,
            "straggler flushed the stream: {:?}",
            core.diagnostics()
        );
    }

    /// Old-timeline packets still in flight when the rebase lands read as
    /// far-future on the new timeline; they must be dropped rather than jamming
    /// the packet buffer as never-playable data.
    #[test]
    fn old_timeline_stragglers_after_rebase_are_dropped() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        let mut clock = WallClock::new();
        let base_ts = 5 * 60 * SAMPLE_RATE;
        for seq in 0..100u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            let ts = base_ts + seq * LIVE_OPUS_FRAME_SAMPLES as u32;
            core.insert_packet(clock.now(), ts, seq, 0, &payload);
            for _ in 0..2 {
                core.get_audio(clock.now(), &mut output);
                clock.advance_block();
            }
        }

        let payload = encode_tone(&mut encoder, 100 * LIVE_OPUS_FRAME_SAMPLES);
        core.insert_packet(clock.now(), 0, 100, LIVE_PACKET_FLAG_OPUS_RESET, &payload);
        let buffered_after_rebase = core.packets_buffered();

        // A reordered pre-restart packet (sequence 99, old timeline) arrives.
        let straggler = encode_tone(&mut encoder, 99 * LIVE_OPUS_FRAME_SAMPLES);
        let late = core.insert_packet(
            clock.now(),
            base_ts + 99 * LIVE_OPUS_FRAME_SAMPLES as u32,
            99,
            0,
            &straggler,
        );
        assert!(late, "old-timeline straggler should be reported late");
        assert_eq!(
            core.packets_buffered(),
            buffered_after_rebase,
            "old-timeline straggler was buffered as far-future data"
        );
    }

    #[test]
    fn missing_packets_expand_then_mute() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        // Prime with a few real frames.
        let mut clock = WallClock::new();
        for seq in 0..6u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(
                clock.now(),
                seq * LIVE_OPUS_FRAME_SAMPLES as u32,
                seq,
                0,
                &payload,
            );
            core.get_audio(clock.now(), &mut output);
            clock.advance_block();
        }
        // Now starve the core: every block must conceal (Expand), and after a
        // long run it must reach the muted state rather than loop forever.
        let mut saw_expand = false;
        let mut saw_muted = false;
        for _ in 0..400 {
            let result = core.get_audio(clock.now(), &mut output);
            clock.advance_block();
            if matches!(result.mode, Mode::Expand) {
                saw_expand = true;
            }
            if result.muted {
                saw_muted = true;
                break;
            }
        }
        assert!(saw_expand, "starvation did not produce expand");
        assert!(saw_muted, "expand never reached the muted state");
    }

    #[test]
    fn muted_silence_emits_zero_then_resumes() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        // Prime with real frames, then starve the core until it mutes.
        let mut clock = WallClock::new();
        for seq in 0..6u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(
                clock.now(),
                seq * LIVE_OPUS_FRAME_SAMPLES as u32,
                seq,
                0,
                &payload,
            );
            core.get_audio(clock.now(), &mut output);
            clock.advance_block();
        }
        // The block on which expand reaches the muted state can still carry the
        // final fade-out samples, so wait for the first fully silent one.
        let mut muted = false;
        for _ in 0..400 {
            let result = core.get_audio(clock.now(), &mut output);
            clock.advance_block();
            if result.muted && output.iter().all(|sample| *sample == 0.0) {
                muted = true;
                break;
            }
        }
        assert!(muted, "expand never reached the muted state");

        // Once muted with no comfort-noise floor, the fast path is taken and
        // every concealment block is exactly silent.
        for _ in 0..50 {
            let result = core.get_audio(clock.now(), &mut output);
            clock.advance_block();
            assert!(result.muted, "stream left the muted state while starved");
            assert!(
                output.iter().all(|sample| *sample == 0.0),
                "muted concealment emitted non-zero audio"
            );
        }

        // A resuming talkspurt still merges back to audible output.
        let mut resumed = false;
        for seq in 6..40u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(
                clock.now(),
                seq * LIVE_OPUS_FRAME_SAMPLES as u32,
                seq,
                0,
                &payload,
            );
            core.get_audio(clock.now(), &mut output);
            clock.advance_block();
            if output.iter().any(|sample| sample.abs() > 1e-3) {
                resumed = true;
                break;
            }
        }
        assert!(resumed, "stream never resumed audible output after muting");
    }

    #[test]
    fn overfull_buffer_accelerates() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        // Flood the buffer well past the target without draining much, so the
        // controller should choose to accelerate.
        let mut accelerated = false;
        let mut clock = WallClock::new();
        for seq in 0..120u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(
                clock.now(),
                seq * LIVE_OPUS_FRAME_SAMPLES as u32,
                seq,
                0,
                &payload,
            );
            // Drain only one 10 ms block per 20 ms packet inserted minus a deficit
            // so the buffer keeps growing.
            let result = core.get_audio(clock.now(), &mut output);
            clock.advance_block();
            if result.time_stretched > 0 {
                accelerated = true;
            }
        }
        assert!(accelerated, "overfull buffer never accelerated");
    }

    #[test]
    fn live_fec_packet_inserts_recovery_unit() {
        // The live encoder profile encodes in-band FEC (LBRR) on every packet.
        let packets = crate::audio::test_support::encode_live_dred_packets(
            EncoderNetworkProfile::CRITICAL,
            8,
        );
        let fec = NetEqCore::parse_fec(&packets[2]).expect("live profile encodes LBRR");
        assert_eq!(fec.duration, LIVE_OPUS_FRAME_SAMPLES as u32);

        // The detected FEC must lay an OpusFec recovery frame one frame before the
        // primary, exactly where the dropped predecessor would have played.
        let timestamp = 2 * LIVE_OPUS_FRAME_SAMPLES as u32;
        let units = parse_payload_redundancy(
            timestamp,
            2,
            Arc::new(packets[2].clone()),
            0,
            LIVE_OPUS_FRAME_SAMPLES as u32,
            Some(fec),
            None,
        );
        let recovered = timestamp - LIVE_OPUS_FRAME_SAMPLES as u32;
        assert!(
            units.iter().any(|packet| {
                matches!(packet.payload, PacketPayload::OpusFec(_)) && packet.timestamp == recovered
            }),
            "FEC unit missing from parsed redundancy: {units:?}",
        );
    }

    #[test]
    fn stale_prepared_insert_falls_back_to_serialized_insert() {
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let payload = Arc::new(encode_tone(&mut encoder, 0));
        let timestamp = 3 * LIVE_OPUS_FRAME_SAMPLES as u32;
        let sequence = 7;

        core.first_packet = false;
        let context_before = core.capture_insert_context(timestamp, sequence);
        assert!(context_before.current_gap > 0);
        let prepared = NetEqPreparedPacket::prepare(
            context_before,
            timestamp,
            sequence,
            0,
            Arc::clone(&payload),
            core.dred_decoder.as_mut(),
        )
        .expect("valid payload prepares");

        core.sync_buffer
            .increase_end_timestamp(LIVE_OPUS_FRAME_SAMPLES as u32);
        let context_after = core.capture_insert_context(timestamp, sequence);
        assert_ne!(context_after, prepared.context());

        let late = if context_after == prepared.context() {
            core.insert_prepared_packet(Instant::now(), prepared)
        } else {
            core.insert_datagram(Instant::now(), timestamp, sequence, 0, payload)
        };

        assert!(!late);
        assert_eq!(core.packet_buffer.num_packets(), 1);
        assert_eq!(
            core.packet_buffer
                .peek_next_packet()
                .expect("inserted packet")
                .timestamp,
            timestamp,
        );
    }

    #[test]
    fn contiguous_packets_do_not_parse_dred_on_insert() {
        let packets = crate::audio::test_support::encode_live_dred_packets(
            EncoderNetworkProfile::CRITICAL,
            24,
        );
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();

        let mut clock = WallClock::new();
        for (seq, payload) in packets.iter().enumerate() {
            core.insert_packet(
                clock.now(),
                seq as u32 * LIVE_OPUS_FRAME_SAMPLES as u32,
                seq as u32,
                0,
                payload,
            );
            clock.advance_block();
            clock.advance_block();
            assert!(
                core.dred_parse
                    .as_ref()
                    .is_none_or(|parse| parse.sequence.is_none()),
                "zero-gap insert parsed DRED at seq {seq}",
            );
        }
    }

    #[test]
    fn dred_horizon_diagnostic_reports_available_horizon_not_gap_capped_reach() {
        let packets = crate::audio::test_support::encode_live_dred_packets(
            EncoderNetworkProfile::CRITICAL,
            80,
        );
        let gap_samples = (3 * DRED_CHUNK_SAMPLES) as u32;
        let mut probe = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        let (sequence, payload, available_horizon) = packets
            .iter()
            .enumerate()
            .find_map(|(sequence, payload)| {
                let datagram = Arc::new(payload.clone());
                let (dred, available_horizon) =
                    probe.parse_dred(sequence as u32, &datagram, gap_samples);
                let usable_reach = dred
                    .map(|info| {
                        let mut samples = info.samples;
                        if info.dred_end < samples {
                            samples -= info.dred_end;
                        }
                        samples.max(0) as u32
                    })
                    .unwrap_or_default();
                (available_horizon > gap_samples && usable_reach >= gap_samples)
                    .then(|| (sequence as u32, payload.clone(), available_horizon))
            })
            .expect("test profile should emit DRED beyond a 30 ms gap");

        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        core.first_packet = false;
        core.packet_buffer.insert_packet(
            Packet::new(
                0,
                0,
                super::super::packet::Priority::PRIMARY,
                LIVE_OPUS_FRAME_SAMPLES,
                PacketPayload::Opus(Arc::new(vec![0xF8])),
            ),
            &core.tick_timer,
        );

        let timestamp = LIVE_OPUS_FRAME_SAMPLES as u32 + gap_samples;
        core.insert_packet(Instant::now(), timestamp, sequence, 0, &payload);

        let diagnostics = core.diagnostics();
        assert_eq!(
            diagnostics.dred_last_horizon_ms,
            samples_to_ms(available_horizon as usize),
        );
        assert_eq!(diagnostics.dred_missed_horizon_count, 0);
        assert_eq!(diagnostics.dred_missed_horizon_ms, 0);

        let mut dred_units = 0;
        while let Some(packet) = core.packet_buffer.get_next_packet() {
            if matches!(packet.payload, PacketPayload::Dred { .. }) {
                dred_units += 1;
                assert_eq!(packet.timestamp, timestamp - gap_samples);
                assert_eq!(packet.duration_samples, DRED_CHUNK_SAMPLES);
            }
        }
        assert_eq!(dred_units, 1);
    }

    #[test]
    fn dropped_primary_recovered_by_fec_not_expand() {
        let packets = crate::audio::test_support::encode_live_dred_packets(
            EncoderNetworkProfile::CRITICAL,
            60,
        );
        let mut core = NetEqCore::new(LiveAudioTuning::default()).unwrap();
        // Disable DRED so in-band FEC is the only redundancy that can conceal the
        // drop. Without the FEC wiring the hole would surface as Expand.
        core.dred_decoder = None;
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        let drop = 30u32;
        let mut expand_after_drop = 0;
        let mut clock = WallClock::new();
        for seq in 0..packets.len() as u32 {
            if seq != drop {
                core.insert_packet(
                    clock.now(),
                    seq * LIVE_OPUS_FRAME_SAMPLES as u32,
                    seq,
                    0,
                    &packets[seq as usize],
                );
            }
            for _ in 0..2 {
                let result = core.get_audio(clock.now(), &mut output);
                clock.advance_block();
                if seq >= drop && matches!(result.mode, Mode::Expand) {
                    expand_after_drop += 1;
                }
            }
        }
        assert_eq!(
            expand_after_drop, 0,
            "FEC should conceal the single drop without any expand",
        );
    }
}
