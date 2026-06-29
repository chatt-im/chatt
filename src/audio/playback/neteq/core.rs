//! The NetEQ `GetAudio` loop — a port of `NetEqImpl::GetAudioInternal`,
//! `GetDecision`, `Decode`, `DecodeLoop`, `ExtractPackets`, and the `Do*`
//! operation handlers in `/tmp/webrtc/modules/audio_coding/neteq/neteq_impl.cc`,
//! plus the DRED-at-insertion path from `/tmp/webrtc-dred/.../neteq_impl.cc`
//! (`InsertPacketInternal`).
//!
//! [`NetEqCore`] owns the timestamp-keyed [`PacketBuffer`], the [`SyncBuffer`] of
//! decoded PCM, the [`DecisionLogic`] controller, the Opus/DRED decoders, and the
//! reused DSP ([`NetEqConcealment`] for normal/merge/expand and [`TimeScaler`]
//! for accelerate/preemptive-expand). [`NetEqCore::insert_packet`] expands a
//! packet into its redundancy units and inserts them; [`NetEqCore::get_audio`]
//! runs one decision, decodes what it needs, executes the operation against the
//! sync buffer, and emits exactly one 10 ms output block.
//!
//! Chatt is mono, single Opus stream, no DTMF and no RFC 3389 comfort noise, so
//! those branches of the reference never fire; the sync buffer is `f32` this
//! stage (the fixed-point re-port is the follow-up). The decoded buffer is the
//! reference `decoded_buffer_`; operations build a `Vec<f32>` algorithm buffer
//! and push it to the sync buffer, mirroring `algorithm_buffer_` + `PushBack`.

use std::rc::Rc;

use opus_codec::{Channels, Decoder, DredDecoder, DredState, SampleRate};

use super::decision_logic::DecisionLogic;
use super::neteq_status::{
    ControllerConfig, NetEqStatus, PacketArrivedInfo, PacketBufferInfo, PacketInfo,
};
use super::operation::{Mode, Operation};
use super::packet::{Packet, PacketPayload, is_newer_timestamp};
use super::packet_buffer::PacketBuffer;
use super::redundancy::{DredInfo, parse_payload_redundancy};
use super::sync_buffer::SyncBuffer;
use super::tick_timer::{Stopwatch, TickTimer};
use crate::audio::playback::MonoSampleQueue;
use crate::audio::playback::concealment::{EXPAND_OVERLAP_LENGTH, NetEqConcealment};
use crate::audio::playback::time_scale::{
    TimeScaler, accelerate_one_period, expand_one_period, threshold_for_mode,
};
use crate::audio::shared::{
    DecodedFrameSource, LIVE_CAPTURE_MUTE_FADE, LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_MUTE,
    LIVE_PACKET_FLAG_OPUS_RESET, LIVE_PLAYBACK_DRED_MAX_SAMPLES, MAX_OPUS_DECODE_SAMPLES,
    SAMPLE_RATE, TIME_SCALE_REF_OFFSET, TIME_SCALE_WINDOW, apply_gain_ramp, mute_gain_step,
    samples_for_duration,
};

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
/// One DRED chunk, 10 ms at 48 kHz.
const DRED_CHUNK_SAMPLES: usize = SAMPLE_RATE as usize / 100;

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

/// One cached DRED parse, reused across the 10 ms chunks recovered from the same
/// packet (mirrors the previous live path's `DredGapState`).
struct DredParse {
    sequence: u32,
    state: DredState,
}

/// The faithful NetEQ control + decode loop for the live playback path.
pub(crate) struct NetEqCore {
    tick_timer: TickTimer,
    packet_buffer: PacketBuffer,
    sync_buffer: SyncBuffer,
    decision_logic: DecisionLogic,
    decoder: Decoder,
    dred_decoder: Option<DredDecoder>,
    dred_parse: Option<DredParse>,
    concealment: NetEqConcealment,
    scaler: TimeScaler,
    /// `decoded_buffer_`: scratch for freshly decoded PCM.
    decoded_buffer: Vec<f32>,
    /// Scratch copy of the sync-buffer history handed to the concealment DSP.
    history_scratch: Vec<f32>,
    /// Scratch for the 30 ms time-scale analysis window.
    time_scale_window: Vec<f32>,
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
}

impl NetEqCore {
    pub(crate) fn new() -> Result<Self, String> {
        let tick_timer = TickTimer::new();
        let config = ControllerConfig {
            allow_time_stretching: true,
            max_packets_in_buffer: 200,
            base_min_delay_ms: 0,
        };
        let mut decision_logic = DecisionLogic::new(config, &tick_timer);
        decision_logic.set_sample_rate(FS_HZ, OUTPUT_SIZE_SAMPLES);
        Ok(Self {
            tick_timer,
            packet_buffer: PacketBuffer::new(200),
            sync_buffer: SyncBuffer::with_default_length(),
            decision_logic,
            decoder: Decoder::new(SampleRate::Hz48000, Channels::Mono)
                .map_err(|error| error.to_string())?,
            dred_decoder: DredDecoder::new().ok(),
            dred_parse: None,
            concealment: NetEqConcealment::new(),
            scaler: TimeScaler::new(),
            decoded_buffer: vec![0.0; MAX_OPUS_DECODE_SAMPLES],
            history_scratch: Vec::with_capacity(SyncBuffer::with_default_length().size()),
            time_scale_window: vec![0.0; TIME_SCALE_WINDOW],
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
        })
    }

    pub(crate) fn output_size_samples(&self) -> usize {
        OUTPUT_SIZE_SAMPLES
    }

    pub(crate) fn target_level_ms(&self) -> i32 {
        self.decision_logic.target_level_ms()
    }

    pub(crate) fn future_length(&self) -> usize {
        self.sync_buffer.future_length()
    }

    pub(crate) fn packets_buffered(&self) -> usize {
        self.packet_buffer.num_packets()
    }

    /// Flushes all state for a hard stream restart (decoder reset boundary).
    pub(crate) fn flush(&mut self) {
        self.packet_buffer.flush();
        self.sync_buffer.flush();
        self.concealment.reset();
        self.decision_logic.soft_reset(&self.tick_timer);
        self.last_mode = Mode::Normal;
        self.timestamp = 0;
        self.first_packet = true;
        self.new_codec = false;
        self.reset_decoder = false;
        self.generated_noise_stopwatch = None;
        self.dred_parse = None;
        self.mute_gain = 1.0;
        self.mute_target = 1.0;
        let _ = self.decoder.reset();
    }

    /// Inserts one Opus voice packet, expanding its DRED into priority-ranked
    /// recovery packets first. Port of `NetEqImpl::InsertPacketInternal`. `flags`
    /// carries the Chatt media flags; `LIVE_PACKET_FLAG_MUTE` fades the decoded
    /// tail on the receiver.
    /// Returns `true` if the packet arrived after its playout slot had already
    /// passed (older than the current playout point), i.e. too late to be played.
    pub(crate) fn insert_packet(
        &mut self,
        timestamp: u32,
        sequence: u32,
        flags: u8,
        opus: &[u8],
    ) -> bool {
        if opus.is_empty() {
            return false;
        }
        let playout_timestamp = self
            .sync_buffer
            .end_timestamp()
            .wrapping_sub(self.sync_buffer.future_length() as u32);
        let late = !self.first_packet && is_newer_timestamp(playout_timestamp, timestamp);
        let muted = flags & LIVE_PACKET_FLAG_MUTE != 0;
        if flags & LIVE_PACKET_FLAG_OPUS_RESET != 0 {
            // The sender restarted its encoder (e.g. resuming after a silence
            // gap); reset the decoder before the next decode to match.
            self.reset_decoder = true;
        }
        let datagram = Rc::new(opus.to_vec());

        // Compute the recovery gap behind this packet, exactly as the DRED fork's
        // InsertPacketInternal does: the hole between the most recent buffered (or
        // played) audio and this packet's timestamp.
        let current_gap = self.recovery_gap(timestamp);
        let dred = self.parse_dred(sequence, &datagram);

        let packets = parse_payload_redundancy(
            timestamp,
            sequence,
            Rc::clone(&datagram),
            current_gap,
            DEFAULT_FRAME_SAMPLES as u32,
            None,
            dred,
        );
        let main_timestamp = packets.first().map_or(timestamp, |packet| packet.timestamp);
        let mut packets = packets;
        if muted {
            // Tag the primary (codec_level 0) so the decode loop fades it.
            if let Some(primary) = packets
                .iter_mut()
                .find(|packet| packet.priority.codec_level == 0)
            {
                primary.muted = true;
            }
        }

        if self.first_packet {
            self.packet_buffer.flush();
            self.sync_buffer
                .increase_end_timestamp(main_timestamp.wrapping_sub(self.timestamp));
            self.timestamp = main_timestamp;
            self.first_packet = false;
            self.new_codec = true;
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
        for packet in packets {
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
                main_sequence_number: sequence as u16,
                is_cng_or_dtmf: false,
                is_dtx: false,
                buffer_flush,
            };
            let should_update_stats = !self.new_codec;
            self.decision_logic
                .packet_arrived(FS_HZ, should_update_stats, &info, &self.tick_timer);
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

    /// Parses the DRED region of `datagram`, caching the parsed state for the
    /// decode loop. Returns the DRED span so [`parse_payload_redundancy`] can
    /// place the recovery chunks.
    fn parse_dred(&mut self, sequence: u32, datagram: &Rc<Vec<u8>>) -> Option<DredInfo> {
        let decoder = self.dred_decoder.as_mut()?;
        let mut state = DredState::new().ok()?;
        let mut dred_end = 0;
        let reach = decoder
            .parse(
                &mut state,
                datagram,
                LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                SampleRate::Hz48000,
                &mut dred_end,
                false,
            )
            .unwrap_or(0);
        self.dred_parse = Some(DredParse { sequence, state });
        if reach == 0 {
            return None;
        }
        Some(DredInfo {
            samples: reach as i32,
            dred_end,
        })
    }

    /// Produces one 10 ms output block. Port of `NetEqImpl::GetAudioInternal`.
    pub(crate) fn get_audio(&mut self, output: &mut [f32]) -> AudioResult {
        debug_assert_eq!(output.len(), OUTPUT_SIZE_SAMPLES);
        self.tick_timer.increment();

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
            Operation::Normal | Operation::CodecInternalCng => self.do_normal(decoded_len),
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
        // overlap lookahead, mirroring GetAudioInternal lines 814-832.
        let got_audio = self.sync_buffer.get_next_audio(output);
        if !got_audio {
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
            Mode::Normal
                | Mode::AccelerateFail
                | Mode::PreemptiveExpandFail
                | Mode::CodecInternalCng
        ) {
            let data_len = self.sync_buffer.size();
            self.history_scratch.clear();
            self.history_scratch
                .extend_from_slice(self.sync_buffer.data());
            let _ = data_len;
            self.concealment
                .update_background_noise(&self.history_scratch);
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
            muted: self.last_mode.is_expand() && self.concealment.muted(),
            time_stretched,
        }
    }

    /// Port of `NetEqImpl::GetDecision`: build the status snapshot, ask the
    /// controller, post-process the decision, and extract the packets to decode.
    fn get_decision(&mut self) -> (Operation, Vec<Packet>) {
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

        let generated_noise_samples = self.generated_noise_samples();
        let next_packet = self
            .packet_buffer
            .peek_next_packet()
            .map(|packet| PacketInfo {
                timestamp: packet.timestamp,
                is_dtx: false,
                is_cng: false,
            });
        let packet_buffer_info = PacketBufferInfo {
            dtx_or_cng: false,
            num_samples: self.packet_buffer.num_samples(),
            span_samples: self.packet_buffer.span_samples(false, &self.tick_timer),
            span_samples_wait_time: self.packet_buffer.span_samples(true, &self.tick_timer),
            num_packets: self.packet_buffer.num_packets(),
        };
        let status = NetEqStatus {
            target_timestamp: self.sync_buffer.end_timestamp(),
            expand_mutefactor: self.concealment.expand_mute_factor_q14(),
            last_packet_samples: self.decoder_frame_length,
            next_packet,
            last_mode: self.last_mode,
            play_dtmf: false,
            generated_noise_samples: generated_noise_samples as usize,
            packet_buffer_info,
            sync_buffer_samples: self.sync_buffer.future_length(),
        };

        let mut operation = self.decision_logic.get_decision(&status, &self.tick_timer);

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
            return (Operation::Normal, Vec::new());
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
            self.sync_buffer
                .increase_end_timestamp(self.timestamp.wrapping_sub(end_timestamp));
            self.new_codec = false;
            self.decision_logic.soft_reset(&self.tick_timer);
            return self.finish_decision(operation, self.timestamp);
        }

        self.finish_decision(operation, end_timestamp)
    }

    /// The required-samples sizing and packet extraction tail of `GetDecision`
    /// (reference lines 1065-1186).
    fn finish_decision(
        &mut self,
        mut operation: Operation,
        end_timestamp: u32,
    ) -> (Operation, Vec<Packet>) {
        let samples_left = self
            .sync_buffer
            .future_length()
            .saturating_sub(EXPAND_OVERLAP_LENGTH) as i32;
        let mut required_samples = OUTPUT_SIZE_SAMPLES;

        match operation {
            Operation::Expand => {
                self.timestamp = end_timestamp;
                return (operation, Vec::new());
            }
            Operation::Rfc3389CngNoPacket | Operation::CodecInternalCng => {
                return (operation, Vec::new());
            }
            Operation::Accelerate | Operation::FastAccelerate => {
                if samples_left >= SAMPLES_30_MS as i32 {
                    self.decision_logic.set_sample_memory(samples_left);
                    self.decision_logic.set_prev_time_scale(true);
                    return (operation, Vec::new());
                } else if samples_left >= SAMPLES_10_MS as i32
                    && self.decoder_frame_length >= SAMPLES_30_MS
                {
                    return (Operation::Normal, Vec::new());
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
                    return (operation, Vec::new());
                }
                if samples_left < SAMPLES_20_MS as i32 && self.decoder_frame_length < SAMPLES_30_MS
                {
                    required_samples = 2 * OUTPUT_SIZE_SAMPLES;
                }
            }
            Operation::Merge => {
                // Merge needs enough future audio to correlate against.
                required_samples = required_samples.max(SAMPLES_30_MS);
            }
            _ => {}
        }

        let mut packet_list = Vec::new();
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
        packet_list: Vec<Packet>,
    ) -> (usize, DecodedFrameSource) {
        if self.reset_decoder {
            let _ = self.decoder.reset();
            self.reset_decoder = false;
        }
        if packet_list.is_empty() {
            return (0, DecodedFrameSource::Normal);
        }

        let mut decoded_length = 0usize;
        let mut source = DecodedFrameSource::Normal;
        let mut had_error = false;
        for packet in &packet_list {
            if packet.priority.codec_level == 2 {
                source = DecodedFrameSource::Dred;
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
                self.decoder
                    .decode_float(bytes, output, false)
                    .map_err(|_| ())
            }
            PacketPayload::OpusFec(bytes) => {
                let end = (out_start + LIVE_OPUS_FRAME_SAMPLES).min(self.decoded_buffer.len());
                let output = &mut self.decoded_buffer[out_start..end];
                self.decoder
                    .decode_float(bytes, output, true)
                    .map_err(|_| ())
            }
            PacketPayload::Dred { source, offset } => {
                self.decode_dred(packet.sequence_number, source, *offset, out_start)
            }
        }
    }

    fn decode_dred(
        &mut self,
        sequence: u32,
        source: &Rc<Vec<u8>>,
        offset: i32,
        out_start: usize,
    ) -> Result<usize, ()> {
        let Some(dred_decoder) = self.dred_decoder.as_mut() else {
            return Ok(0);
        };
        // Reuse the cached parse from insertion when it is for this packet.
        let cached = self
            .dred_parse
            .as_ref()
            .is_some_and(|parse| parse.sequence == sequence);
        if !cached {
            let mut state = DredState::new().map_err(|_| ())?;
            let mut dred_end = 0;
            let _ = dred_decoder.parse(
                &mut state,
                source,
                LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                SampleRate::Hz48000,
                &mut dred_end,
                false,
            );
            self.dred_parse = Some(DredParse { sequence, state });
        }
        let parse = self.dred_parse.as_ref().expect("dred parse present");
        let end = (out_start + DRED_CHUNK_SAMPLES).min(self.decoded_buffer.len());
        let output = &mut self.decoded_buffer[out_start..end];
        let decoder = &mut self.decoder;
        let dred_decoder = self.dred_decoder.as_mut().expect("dred decoder present");
        dred_decoder
            .decode_into_f32(decoder, &parse.state, offset, output)
            .map_err(|_| ())
    }

    /// Port of `NetEqImpl::DoNormal`: copy decoded audio (with an
    /// expand-to-normal transition when recovering from concealment) into the
    /// sync buffer.
    fn do_normal(&mut self, decoded_len: usize) {
        if decoded_len == 0 {
            return;
        }
        let mut decoded = self.decoded_buffer[..decoded_len].to_vec();
        if matches!(self.last_mode, Mode::Expand | Mode::CodecPlc) {
            self.copy_history();
            self.concealment
                .normal_after_expand(&self.history_scratch, &mut decoded);
        }
        self.sync_buffer.push_back(&decoded);
        self.last_mode = Mode::Normal;
    }

    /// Port of `NetEqImpl::DoMerge`: merge decoded audio onto the expand tail.
    fn do_merge(&mut self, decoded_len: usize) {
        let mut decoded = self.decoded_buffer[..decoded_len].to_vec();
        self.copy_history();
        self.concealment
            .merge_after_expand(&self.history_scratch, &mut decoded);
        self.sync_buffer.push_back(&decoded);
        self.last_mode = Mode::Merge;
    }

    /// Port of `NetEqImpl::DoExpand`: synthesize concealment until the sync
    /// buffer holds one output block past the overlap lookahead.
    fn do_expand(&mut self) {
        if self.generated_noise_stopwatch.is_none() {
            self.generated_noise_stopwatch = Some(self.tick_timer.new_stopwatch());
        }
        let mut guard = 0;
        while self
            .sync_buffer
            .future_length()
            .saturating_sub(EXPAND_OVERLAP_LENGTH)
            < OUTPUT_SIZE_SAMPLES
        {
            self.copy_history();
            let chunk = self.concealment.expand_chunk(&self.history_scratch);
            if chunk.samples.is_empty() {
                // No history to extrapolate from: emit silence to make progress.
                self.sync_buffer.push_back(&vec![0.0; OUTPUT_SIZE_SAMPLES]);
            } else {
                self.sync_buffer.blend_overlap_tail(&chunk.overlap);
                self.sync_buffer.push_back(&chunk.samples);
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
        let (input, borrowed) = self.build_timescale_input(decoded_len);
        if input.len() < TIME_SCALE_WINDOW {
            // Not enough to analyze: fall back to normal copy.
            self.apply_timescale_result(input, borrowed);
            self.last_mode = Mode::AccelerateFail;
            return 0;
        }
        let mut queue = MonoSampleQueue::new();
        queue.push_back(&input);
        self.time_scale_window.clear();
        self.time_scale_window
            .extend_from_slice(&input[..TIME_SCALE_WINDOW]);
        let analysis = self.scaler.analyze(&self.time_scale_window);
        let removed =
            if analysis.best_correlation <= threshold_for_mode(fast) && analysis.active_speech {
                self.last_mode = Mode::AccelerateFail;
                0
            } else {
                let peak_index = if fast {
                    let periods = TIME_SCALE_REF_OFFSET / analysis.peak_index.max(1);
                    (periods.max(1) * analysis.peak_index).min(TIME_SCALE_REF_OFFSET)
                } else {
                    analysis.peak_index
                };
                let removed = accelerate_one_period(&mut queue, 0, peak_index);
                self.last_mode = if analysis.active_speech {
                    Mode::AccelerateSuccess
                } else {
                    Mode::AccelerateLowEnergy
                };
                removed
            };
        let mut algorithm = Vec::with_capacity(queue.frames());
        queue.copy_window(0, queue.frames(), &mut algorithm);
        self.apply_timescale_result(algorithm, borrowed);
        removed as i32
    }

    /// Port of `NetEqImpl::DoPreemptiveExpand`: insert one pitch period.
    fn do_preemptive_expand(&mut self, decoded_len: usize) -> i32 {
        let (input, borrowed) = self.build_timescale_input(decoded_len);
        if input.len() < TIME_SCALE_WINDOW {
            self.apply_timescale_result(input, borrowed);
            self.last_mode = Mode::PreemptiveExpandFail;
            return 0;
        }
        let mut queue = MonoSampleQueue::new();
        queue.push_back(&input);
        self.time_scale_window.clear();
        self.time_scale_window
            .extend_from_slice(&input[..TIME_SCALE_WINDOW]);
        let analysis = self.scaler.analyze(&self.time_scale_window);
        let added =
            if analysis.best_correlation <= threshold_for_mode(false) && analysis.active_speech {
                self.last_mode = Mode::PreemptiveExpandFail;
                0
            } else {
                let added = expand_one_period(&mut queue, 0, analysis.peak_index);
                self.last_mode = if analysis.active_speech {
                    Mode::PreemptiveExpandSuccess
                } else {
                    Mode::PreemptiveExpandLowEnergy
                };
                added
            };
        let mut algorithm = Vec::with_capacity(queue.frames());
        queue.copy_window(0, queue.frames(), &mut algorithm);
        self.apply_timescale_result(algorithm, borrowed);
        added as i32
    }

    /// Builds the 30 ms time-scale input: the decoded buffer prefixed with
    /// samples borrowed from the end of the sync buffer when the decoder gave
    /// fewer than 30 ms. Mirrors the `ReadInterleavedFromEnd` borrow in
    /// `DoAccelerate`/`DoPreemptiveExpand`.
    fn build_timescale_input(&mut self, decoded_len: usize) -> (Vec<f32>, usize) {
        if decoded_len >= TIMESCALE_REQUIRED_SAMPLES {
            return (self.decoded_buffer[..decoded_len].to_vec(), 0);
        }
        let borrowed = TIMESCALE_REQUIRED_SAMPLES - decoded_len;
        let mut input = Vec::with_capacity(TIMESCALE_REQUIRED_SAMPLES);
        let mut tail = Vec::new();
        self.sync_buffer.read_from_end(borrowed, &mut tail);
        // The borrow may be shorter than requested if the buffer is small; pad to
        // keep the analysis window full.
        if tail.len() < borrowed {
            input.resize(borrowed - tail.len(), 0.0);
        }
        input.extend_from_slice(&tail);
        input.extend_from_slice(&self.decoded_buffer[..decoded_len]);
        (input, borrowed)
    }

    /// Writes a time-scaled algorithm buffer back, restoring the borrowed sync
    /// tail in place and pushing the remainder. Port of the borrow-restore tail
    /// of `DoAccelerate`/`DoPreemptiveExpand`.
    fn apply_timescale_result(&mut self, mut algorithm: Vec<f32>, borrowed: usize) {
        if borrowed > 0 {
            let size = self.sync_buffer.size();
            if algorithm.len() < borrowed {
                let len = algorithm.len();
                self.sync_buffer
                    .replace_at_index(&algorithm, len, size - borrowed);
                self.sync_buffer.push_front_zeros(borrowed - len);
                algorithm.clear();
            } else {
                self.sync_buffer.replace_at_index(
                    &algorithm[..borrowed],
                    borrowed,
                    size - borrowed,
                );
                algorithm.drain(..borrowed);
            }
        }
        if !algorithm.is_empty() {
            self.sync_buffer.push_back(&algorithm);
        }
    }

    fn copy_history(&mut self) {
        let next = self.sync_buffer.next_index();
        self.history_scratch.clear();
        self.history_scratch
            .extend_from_slice(&self.sync_buffer.data()[..next]);
    }

    fn generated_noise_samples(&self) -> u64 {
        self.generated_noise_stopwatch
            .as_ref()
            .map(|watch| watch.elapsed_ticks(&self.tick_timer) * OUTPUT_SIZE_SAMPLES as u64)
            .unwrap_or(0)
            + self.decision_logic.noise_fast_forward() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::capture::OpusVoiceEncoder;

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

    #[test]
    fn steady_stream_plays_back_continuously() {
        let mut core = NetEqCore::new().unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        let mut total_energy = 0.0f64;
        let mut blocks = 0;
        for seq in 0..60u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(seq * LIVE_OPUS_FRAME_SAMPLES as u32, seq, 0, &payload);
            // Two 10 ms output blocks per 20 ms packet.
            for _ in 0..2 {
                let result = core.get_audio(&mut output);
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

    #[test]
    fn missing_packets_expand_then_mute() {
        let mut core = NetEqCore::new().unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        // Prime with a few real frames.
        for seq in 0..6u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(seq * LIVE_OPUS_FRAME_SAMPLES as u32, seq, 0, &payload);
            core.get_audio(&mut output);
        }
        // Now starve the core: every block must conceal (Expand), and after a
        // long run it must reach the muted state rather than loop forever.
        let mut saw_expand = false;
        let mut saw_muted = false;
        for _ in 0..400 {
            let result = core.get_audio(&mut output);
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
    fn overfull_buffer_accelerates() {
        let mut core = NetEqCore::new().unwrap();
        let mut encoder = OpusVoiceEncoder::new(32_000).unwrap();
        let mut output = vec![0.0; OUTPUT_SIZE_SAMPLES];
        // Flood the buffer well past the target without draining much, so the
        // controller should choose to accelerate.
        let mut accelerated = false;
        for seq in 0..120u32 {
            let payload = encode_tone(&mut encoder, seq as usize * LIVE_OPUS_FRAME_SAMPLES);
            core.insert_packet(seq * LIVE_OPUS_FRAME_SAMPLES as u32, seq, 0, &payload);
            // Drain only one 10 ms block per 20 ms packet inserted minus a deficit
            // so the buffer keeps growing.
            let result = core.get_audio(&mut output);
            if result.time_stretched > 0 {
                accelerated = true;
            }
        }
        assert!(accelerated, "overfull buffer never accelerated");
    }
}
