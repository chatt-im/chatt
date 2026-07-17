use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{Receiver, SyncSender},
    },
    time::Instant,
};

use nnnoiseless::DenoiseState;

use crate::{
    audio::{
        capture::{
            dsp::{
                CaptureGain, CaptureGateDecision, CaptureHighPass, CaptureProcessor, EarshotVad,
                LongSilenceGate, TypingNoiseGate, is_capture_skip_safe_silence,
                store_processed_level_stats,
            },
            echo::{EchoReference, EchoReferenceSource},
            encoder::OpusVoiceEncoder,
        },
        resample::CaptureResampler,
        shared::{
            AUDIO_POP_DELTA_THRESHOLD, AudioStats, CapturedAudioChunk, DenoiseConfig,
            DenoiseSuppression, DenoiseTypingSuppression, FRAME_SAMPLES, LIVE_CAPTURE_MUTE_FADE,
            LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_MUTE, LIVE_PACKET_FLAG_OPUS_RESET,
            LIVE_PACKET_FLAG_SILENCE_HINT, LIVE_PACKET_FLAG_SILENCE_RESUME, LiveAudioTuning,
            LiveEncoderProfile, LocalVoiceFrame, MAX_OPUS_PACKET_BYTES, VoicePayload,
            apply_gain_ramp, audio_pop_logging_enabled, convert_i16_scale_to_pcm_i16,
            duration_to_us, max_adjacent_delta, mute_gain_step, optional_duration_to_us,
            peak_normalized, rms_normalized, samples_for_duration, vad_to_u8,
        },
    },
    network::{EncoderNetworkProfile, EncoderNetworkTuning},
    packet_log::PacketLogWriter,
};

const SILENCE_RESUME_HINT_PACKETS: u8 = 10;
const SILENCE_KEEPALIVE_CAPTURE_FRAMES: usize = 100;

fn device_samples_to_duration(samples: usize, device_rate: u32) -> std::time::Duration {
    let nanos = samples as u128 * 1_000_000_000u128 / u128::from(device_rate.max(1));
    std::time::Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

pub(crate) fn run_encoder_worker(
    receiver: Receiver<CapturedAudioChunk>,
    recycle: SyncSender<Vec<f32>>,
    mut writer: PacketLogWriter<std::io::BufWriter<std::fs::File>>,
    mut encoder: OpusVoiceEncoder,
    denoise: DenoiseConfig,
    max_amplification: f32,
    stats: AudioStats,
) {
    let result = run_encoder_worker_inner(
        receiver,
        recycle,
        &mut writer,
        &mut encoder,
        denoise,
        max_amplification,
        &stats,
    );
    if let Err(error) = result {
        stats.set_error(error);
    }
    if let Err(error) = writer.flush() {
        stats.set_error(format!("failed to flush packet log: {error}"));
    }
    stats.mark_worker_stopped();
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_live_encoder_worker<F>(
    receiver: Receiver<CapturedAudioChunk>,
    recycle: SyncSender<Vec<f32>>,
    encoder: OpusVoiceEncoder,
    denoise: DenoiseConfig,
    max_amplification_bits: Arc<AtomicU32>,
    encoder_loss_percent: Arc<AtomicU32>,
    mic_muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    tuning: LiveAudioTuning,
    suppression: DenoiseSuppression,
    typing_suppression: DenoiseTypingSuppression,
    echo_source: Option<EchoReferenceSource>,
    device_rate: u32,
    stats: AudioStats,
    mut on_packet: F,
) where
    F: FnMut(LocalVoiceFrame) + Send + 'static,
{
    let result = run_live_encoder_worker_inner(
        receiver,
        recycle,
        encoder,
        denoise,
        &max_amplification_bits,
        &encoder_loss_percent,
        &mic_muted,
        &deafened,
        tuning,
        suppression,
        typing_suppression,
        echo_source,
        device_rate,
        &stats,
        &mut on_packet,
    );
    if let Err(error) = result {
        stats.set_error(error);
    }
    stats.mark_worker_stopped();
}

pub(crate) fn run_encoder_worker_inner(
    receiver: Receiver<CapturedAudioChunk>,
    recycle: SyncSender<Vec<f32>>,
    writer: &mut PacketLogWriter<std::io::BufWriter<std::fs::File>>,
    encoder: &mut OpusVoiceEncoder,
    denoise: DenoiseConfig,
    max_amplification: f32,
    stats: &AudioStats,
) -> Result<(), String> {
    // The offline recorder has no APM, so it denoises with RNNoise whenever any
    // engine is selected and otherwise passes the frame through.
    let denoise_enabled = denoise.is_enabled();
    let mut denoise = DenoiseState::new();
    let mut high_pass = CaptureHighPass::new();
    let mut gain = CaptureGain::new(max_amplification);
    let mut accumulator = FrameAccumulator::new(FRAME_SAMPLES);
    let mut denoised_frame = vec![0.0; FRAME_SAMPLES];
    let mut opus_frame = vec![0i16; FRAME_SAMPLES];
    let mut encoded = vec![0u8; MAX_OPUS_PACKET_BYTES];

    for chunk in receiver {
        let CapturedAudioChunk { samples: chunk, .. } = chunk;
        let _ = stats.note_capture_chunk_dequeued();
        let result = accumulator.push_chunk(&chunk, |frame| {
            high_pass.process(frame);
            if let Some(gain) = gain.as_mut() {
                gain.process(frame);
            }
            let vad_probability = if denoise_enabled {
                let vad = denoise.process_frame(&mut denoised_frame, frame);
                frame.copy_from_slice(&denoised_frame);
                vad
            } else {
                0.0
            };
            store_processed_level_stats(stats, frame);
            stats.store_vad_probability(vad_probability);

            convert_i16_scale_to_pcm_i16(frame, &mut opus_frame);
            let packet_len = encoder.encode(&opus_frame, &mut encoded)?;
            writer
                .write_packet(&encoded[..packet_len])
                .map_err(|error| format!("failed to write packet log: {error}"))?;
            stats.record_encoded_packet(packet_len);
            Ok(())
        });
        let _ = recycle.try_send(chunk);
        result?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_live_encoder_worker_inner(
    receiver: Receiver<CapturedAudioChunk>,
    recycle: SyncSender<Vec<f32>>,
    encoder: OpusVoiceEncoder,
    denoise: DenoiseConfig,
    max_amplification_bits: &AtomicU32,
    encoder_loss_percent: &AtomicU32,
    mic_muted: &AtomicBool,
    deafened: &AtomicBool,
    tuning: LiveAudioTuning,
    suppression: DenoiseSuppression,
    typing_suppression: DenoiseTypingSuppression,
    echo_source: Option<EchoReferenceSource>,
    device_rate: u32,
    stats: &AudioStats,
    on_packet: &mut dyn FnMut(LocalVoiceFrame),
) -> Result<(), String> {
    let mut pipeline = LiveEncoderPipeline::new(
        encoder,
        denoise,
        tuning,
        f32::from_bits(max_amplification_bits.load(Ordering::Relaxed)),
        true,
        suppression,
        typing_suppression,
        echo_source,
        device_rate,
        mic_muted.load(Ordering::Relaxed) || deafened.load(Ordering::Relaxed),
    );
    let mut applied_loss_percent = LiveEncoderProfile::DRED_20.packet_loss_percent;

    for chunk in receiver {
        let CapturedAudioChunk {
            samples: chunk,
            enqueued_at,
            timing,
            queue_depth_after_enqueue,
        } = chunk;
        let dequeued_at = Instant::now();
        let queue_depth_after_dequeue = stats.note_capture_chunk_dequeued();
        let queue_age = dequeued_at.saturating_duration_since(enqueued_at);
        // Slave the media clock to cpal's capture timestamp before processing the
        // chunk. This subsumes both capture xruns and software drops: a dropped
        // callback simply leaves the counted clock behind the authoritative
        // capture instant, which surfaces here as a concealable gap.
        pipeline.note_capture_timestamp(timing.cpal_capture_ns)?;
        // The software-drop counter no longer drives the clock (the timestamp path
        // covers it); drain and log it purely as a backpressure signal.
        let dropped_samples = stats.take_dropped_capture_samples();
        if dropped_samples > 0 && audio_pop_logging_enabled() {
            kvlog::info!(
                "audio pop capture software drop observed",
                callback_sequence = timing.callback_sequence,
                dropped_device_samples = dropped_samples,
                device_rate
            );
        }
        let requested_loss_percent = encoder_loss_percent.load(Ordering::Relaxed).min(100) as i32;
        if requested_loss_percent != applied_loss_percent {
            pipeline.apply_encoder_profile(LiveEncoderProfile {
                packet_loss_percent: requested_loss_percent,
            })?;
            applied_loss_percent = requested_loss_percent;
        }
        let muted = mic_muted.load(Ordering::Relaxed) || deafened.load(Ordering::Relaxed);
        let process_start = Instant::now();
        let mut emitted_packets = 0u32;
        pipeline.push_chunk(
            &chunk,
            f32::from_bits(max_amplification_bits.load(Ordering::Relaxed)),
            muted,
            stats,
            &mut |packet| {
                emitted_packets = emitted_packets.wrapping_add(1);
                on_packet(packet);
            },
        )?;
        let process_time = process_start.elapsed();
        if audio_pop_logging_enabled() {
            let expected_callback_delta_us =
                duration_to_us(device_samples_to_duration(chunk.len(), device_rate));
            kvlog::info!(
                "audio pop capture chunk processed",
                callback_sequence = timing.callback_sequence,
                samples = chunk.len(),
                device_rate,
                expected_callback_delta_us,
                callback_delta_us = optional_duration_to_us(timing.callback_delta),
                cpal_callback_ns = timing.cpal_callback_ns,
                cpal_capture_ns = timing.cpal_capture_ns,
                cpal_callback_delta_us = optional_duration_to_us(timing.cpal_callback_delta),
                cpal_capture_to_callback_us = duration_to_us(timing.cpal_capture_to_callback),
                queue_age_us = duration_to_us(queue_age),
                queue_depth_after_enqueue,
                queue_depth_after_dequeue,
                process_us = duration_to_us(process_time),
                emitted_packets,
                muted,
            );
        }
        let _ = recycle.try_send(chunk);
    }

    Ok(())
}

/// Anchor tying the counted media clock to cpal's authoritative capture
/// timestamp. `last_effective_sample` is the media index (48 kHz) of the first
/// sample of the callback captured at `last_capture_ns` nanoseconds since stream
/// creation. Comparing the next callback's timestamp delta against how far the
/// counted clock actually advanced reveals any span the hardware captured but the
/// pipeline never saw.
#[derive(Clone, Copy)]
struct CaptureClock {
    last_capture_ns: u64,
    last_effective_sample: u32,
}

pub(crate) struct LiveEncoderPipeline {
    encoder: OpusVoiceEncoder,
    auto_gain_enabled: bool,
    tuning: LiveAudioTuning,
    /// Consolidated high-pass / AEC3 / noise-suppression / AGC2 front-end. When
    /// the RNNoise engine is active it also supplies the VAD, so `earshot` only
    /// runs as the fallback for the non-RNNoise engines.
    processor: CaptureProcessor,
    earshot: EarshotVad,
    typing_gate: TypingNoiseGate,
    long_silence: Option<LongSilenceGate>,
    gain_max_db: f32,
    /// Device-rate to 48 kHz resampler, present only for non-48 kHz capture
    /// devices. `None` keeps the native 48 kHz path allocation-free.
    resampler: Option<CaptureResampler>,
    resampled: Vec<f32>,
    accumulator: FrameAccumulator,
    opus_frame: Vec<i16>,
    encoded: Vec<u8>,
    pending_opus_samples: Vec<f32>,
    /// Media timestamp (48 kHz sample index) of the first sample currently in
    /// `pending_opus_samples`. Valid while `pending_opus_samples` is non-empty;
    /// re-anchored whenever queued frames transition pending from empty to
    /// non-empty. A partial frame must never straddle a suppressed span, so
    /// entering suppression drops it via
    /// [`Self::discard_pending_before_suppression`] to keep pending contiguous
    /// with `next_capture_sample`.
    pending_start_sample: u32,
    next_opus_packet_flags: u8,
    silence_resume_hint_packets: u8,
    sender_silence_active: bool,
    silence_keepalive_frames: usize,
    suppressed_frames: u64,
    /// Moving mute gain: 1.0 fully open, 0.0 fully muted. It ramps toward the
    /// current mute target over [`LIVE_CAPTURE_MUTE_FADE`], so muting fades out,
    /// unmuting fades back in, and toggling mid-fade reverses without a step.
    mute_gain: f32,
    /// Per-sample ramp rate for `mute_gain`.
    mute_gain_step: f32,
    /// True once `mute_gain` has reached zero while muted, so the sender has
    /// stopped encoding audio and is emitting only silence keepalive markers.
    mute_suppressed: bool,
    last_muted: bool,
    mute_transition_id: u64,
    echo_source: Option<EchoReferenceSource>,
    /// Whether the previous frame consumed the echo reference. On a
    /// disabled-to-enabled transition (AEC toggle or a fresh pipeline after a
    /// device change) the ring's backlog predates the transition and is stale,
    /// so it is cleared before the first pull.
    echo_reference_active: bool,
    /// Media sample clock in 48 kHz samples: the index the next captured sample
    /// will occupy. Advances by [`FRAME_SAMPLES`] for every accumulated 10 ms
    /// slot, including suppressed-silence slots, so an emitted packet's
    /// timestamp reflects true elapsed media time across pauses.
    next_capture_sample: u32,
    /// Slaving anchor for [`Self::note_capture_timestamp`]. `None` until the
    /// first callback establishes it.
    capture_clock: Option<CaptureClock>,
}

#[derive(Clone, Copy)]
struct ProcessedCaptureFrame<'a> {
    samples: &'a [f32],
}

impl<'a> ProcessedCaptureFrame<'a> {
    fn new(samples: &'a [f32]) -> Self {
        debug_assert_eq!(samples.len(), FRAME_SAMPLES);
        Self { samples }
    }
}

impl LiveEncoderPipeline {
    #[allow(clippy::too_many_arguments)]
    fn new(
        encoder: OpusVoiceEncoder,
        denoise: DenoiseConfig,
        tuning: LiveAudioTuning,
        max_amplification: f32,
        auto_gain_enabled: bool,
        suppression: DenoiseSuppression,
        typing_suppression: DenoiseTypingSuppression,
        echo_source: Option<EchoReferenceSource>,
        device_rate: u32,
        initial_muted: bool,
    ) -> Self {
        let echo_enabled = echo_source.as_ref().is_some_and(|source| source.enabled());
        let gain_max_db = if auto_gain_enabled {
            max_amplification
        } else {
            0.0
        };
        Self {
            encoder,
            auto_gain_enabled,
            tuning,
            processor: CaptureProcessor::new(denoise, gain_max_db, echo_enabled, suppression),
            earshot: EarshotVad::new(),
            typing_gate: TypingNoiseGate::new(typing_suppression, denoise),
            long_silence: tuning
                .capture_silence_gate
                .then(|| LongSilenceGate::new(tuning)),
            gain_max_db,
            resampler: CaptureResampler::new(device_rate),
            resampled: Vec::new(),
            accumulator: FrameAccumulator::new(FRAME_SAMPLES),
            opus_frame: vec![0i16; LIVE_OPUS_FRAME_SAMPLES],
            encoded: vec![0u8; MAX_OPUS_PACKET_BYTES],
            pending_opus_samples: Vec::with_capacity(LIVE_OPUS_FRAME_SAMPLES),
            pending_start_sample: 0,
            next_opus_packet_flags: LIVE_PACKET_FLAG_OPUS_RESET,
            silence_resume_hint_packets: 0,
            sender_silence_active: false,
            silence_keepalive_frames: 0,
            suppressed_frames: 0,
            mute_gain: if initial_muted { 0.0 } else { 1.0 },
            mute_gain_step: mute_gain_step(samples_for_duration(LIVE_CAPTURE_MUTE_FADE)),
            mute_suppressed: initial_muted,
            last_muted: initial_muted,
            mute_transition_id: 0,
            echo_source,
            echo_reference_active: false,
            next_capture_sample: 0,
            capture_clock: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn aec_enabled(&self) -> bool {
        self.processor.echo_enabled()
    }

    pub(crate) fn push_chunk(
        &mut self,
        chunk: &[f32],
        max_amplification: f32,
        muted: bool,
        stats: &AudioStats,
        on_packet: &mut dyn FnMut(LocalVoiceFrame),
    ) -> Result<(), String> {
        if self.auto_gain_enabled && max_amplification != self.gain_max_db {
            self.processor.set_max_gain_db(max_amplification);
            self.gain_max_db = max_amplification;
        }
        // Resample a non-48 kHz device to 48 kHz before any DSP, so the whole
        // pipeline downstream keeps assuming 48 kHz / 480-sample frames. The
        // resampled buffer is owned locally during the accumulator pass so it
        // does not alias `self`, then returned to the pipeline for reuse.
        let mut resampled = Vec::new();
        let chunk = match self.resampler.as_mut() {
            Some(resampler) => {
                resampled = std::mem::take(&mut self.resampled);
                resampled.clear();
                resampler.push(chunk, &mut resampled);
                resampled.as_slice()
            }
            None => chunk,
        };
        let mut accumulator = self.take_accumulator();
        let result = accumulator.push_chunk(chunk, |frame| {
            self.process_accumulated_frame(frame, muted, stats, on_packet)
        });
        self.accumulator = accumulator;
        if self.resampler.is_some() {
            self.resampled = resampled;
        }
        result
    }

    fn apply_encoder_profile(&mut self, profile: LiveEncoderProfile) -> Result<(), String> {
        self.encoder.apply_live_encoder_profile(profile)
    }

    /// Accounts `gap_samples` (48 kHz) of capture dropped under load. The media
    /// clock advances across the hole so the receiver sees the true elapsed time
    /// as a concealable timestamp gap instead of a hard splice, and the Opus
    /// stream re-anchors because the encoder state on either side of the hole is
    /// no longer continuous. Partial samples accumulated before the hole are
    /// discarded onto the clock rather than spliced against post-gap audio.
    pub(crate) fn note_capture_gap(&mut self, gap_samples: u32) -> Result<(), String> {
        if gap_samples == 0 {
            return Ok(());
        }
        let pending = self.accumulator.pending.len() as u32;
        self.accumulator.pending.clear();
        self.next_capture_sample = self
            .next_capture_sample
            .wrapping_add(gap_samples)
            .wrapping_add(pending);
        self.reset_opus_stream()
    }

    /// Slaves the media clock to cpal's authoritative capture timestamp
    /// (`capture_ns`, nanoseconds since stream creation). The counted clock
    /// advances one 10 ms slot per accumulated frame, so any span the hardware
    /// captured but the pipeline never processed — a capture xrun, or a callback
    /// dropped under load before it reached the encoder — surfaces here as the
    /// capture instant running ahead of what we counted. The shortfall is emitted
    /// through [`Self::note_capture_gap`] so the drop reaches the receiver as a
    /// concealable timestamp gap instead of a slow clock. Sub-frame jitter and a
    /// leading counted clock are ignored: the clock only ever jumps forward, in
    /// whole gaps, exactly as an under-load drop already does.
    pub(crate) fn note_capture_timestamp(&mut self, capture_ns: u64) -> Result<(), String> {
        // `effective` is the media index of the first sample of the incoming
        // chunk: accumulator `pending` holds the previous chunk's leftover, which
        // the new samples are appended after.
        let effective = self
            .next_capture_sample
            .wrapping_add(self.accumulator.pending.len() as u32);
        let Some(prev) = self.capture_clock else {
            self.capture_clock = Some(CaptureClock {
                last_capture_ns: capture_ns,
                last_effective_sample: effective,
            });
            return Ok(());
        };
        let dt_ns = capture_ns.saturating_sub(prev.last_capture_ns);
        let expected = ((dt_ns * u64::from(crate::audio::shared::SAMPLE_RATE) + 500_000_000)
            / 1_000_000_000) as u32;
        let actual = effective.wrapping_sub(prev.last_effective_sample);
        let shortfall = expected.saturating_sub(actual);
        if shortfall >= FRAME_SAMPLES as u32 {
            self.note_capture_gap(shortfall)?;
        }
        // Re-lock the anchor to the authoritative timestamp. After a gap the
        // recomputed `effective` has advanced by exactly `shortfall`, so the
        // stored clock tracks `expected` and rounding never accumulates. Clamp the
        // timestamp monotone so a transient backward blip cannot manufacture a
        // later gap.
        let last_effective_sample = self
            .next_capture_sample
            .wrapping_add(self.accumulator.pending.len() as u32);
        self.capture_clock = Some(CaptureClock {
            last_capture_ns: capture_ns.max(prev.last_capture_ns),
            last_effective_sample,
        });
        Ok(())
    }

    fn process_accumulated_frame(
        &mut self,
        frame: &mut [f32],
        muted: bool,
        stats: &AudioStats,
        on_packet: &mut dyn FnMut(LocalVoiceFrame),
    ) -> Result<(), String> {
        // One consolidated APM pass runs high-pass, AEC3, noise suppression, and
        // AGC2 in WebRTC order. The echo reference, present only while AEC is
        // enabled, supplies the 48 kHz render frame to cancel against. It runs
        // even while muted so AEC3 and AGC2 keep adapting and the stream resumes
        // cleanly on unmute.
        let reference = self
            .echo_source
            .as_ref()
            .filter(|source| source.enabled())
            .map(|source| source.reference());
        if let Some(reference) = reference
            && !self.echo_reference_active
        {
            reference.clear();
        }
        self.echo_reference_active = reference.is_some();
        self.processor.set_echo_enabled(reference.is_some());
        // The RNNoise engine supplies its own VAD and drives gating directly.
        // Earshot, which holds a non-zero floor on steady background noise, only
        // runs as the fallback for the non-RNNoise engines.
        let rnnoise_vad = self.processor.process(frame, reference);
        let earshot_vad = match rnnoise_vad {
            Some(_) => 0.0,
            None => self.earshot.process_48k_frame(frame),
        };
        let rnnoise_vad = rnnoise_vad.unwrap_or_default();
        let vad_probability = rnnoise_vad.max(earshot_vad);
        self.typing_gate.process(rnnoise_vad, earshot_vad, frame);
        store_processed_level_stats(stats, frame);
        stats.store_vad_probability(vad_probability);
        let vad = vad_to_u8(vad_probability);
        let silence = is_capture_skip_safe_silence(self.tuning, vad, frame);
        stats.store_voice_active(!muted && !silence);
        self.handle_mute_state_change(muted, frame);

        // Account this 10 ms slot on the media sample clock before any emit, so
        // packets stamp a timestamp that tracks real elapsed time even when the
        // slot is suppressed and emits nothing.
        self.next_capture_sample = self.next_capture_sample.wrapping_add(FRAME_SAMPLES as u32);

        // Mute takes precedence over the automatic silence gate: it transitions
        // immediately rather than after the long-silence timeout, and works even
        // when the gate is disabled. The fade-in/out and silence markers are
        // handled here; only a fully-open, unmuted stream falls through to the
        // normal gate path below.
        if muted || self.mute_gain < 1.0 || self.mute_suppressed {
            return self.process_mute_transition(frame, muted, stats, on_packet);
        }

        let decision = self
            .long_silence
            .as_mut()
            .map(|gate| gate.observe(frame, silence))
            .unwrap_or(CaptureGateDecision::TransmitCurrent {
                silence_hint: false,
            });
        if audio_pop_logging_enabled() {
            let slot_start = self.next_capture_sample.wrapping_sub(FRAME_SAMPLES as u32);
            let (decision_label, silence_hint, resume_frames) = match &decision {
                CaptureGateDecision::TransmitCurrent { silence_hint } => {
                    ("transmit", *silence_hint, 0usize)
                }
                CaptureGateDecision::SuppressCurrent => ("suppress", false, 0),
                CaptureGateDecision::Resume(frames) => ("resume", false, frames.len()),
            };
            kvlog::info!(
                "audio pop capture frame decision",
                slot_start_sample = slot_start,
                next_capture_sample = self.next_capture_sample,
                decision = decision_label,
                silence_hint,
                resume_frames,
                vad,
                silence,
                muted,
                pending_opus_samples = self.pending_opus_samples.len(),
                suppressed_frames = self.suppressed_frames,
                sender_silence_active = self.sender_silence_active
            );
        }
        match decision {
            CaptureGateDecision::TransmitCurrent { silence_hint } => {
                if silence_hint {
                    self.next_opus_packet_flags |= LIVE_PACKET_FLAG_SILENCE_HINT;
                }
                let slot_start = self.next_capture_sample.wrapping_sub(FRAME_SAMPLES as u32);
                let frame = ProcessedCaptureFrame::new(frame);
                self.queue_processed_capture_frame(frame, slot_start, stats, on_packet)?;
            }
            CaptureGateDecision::SuppressCurrent => {
                self.discard_pending_before_suppression();
                self.suppressed_frames = self.suppressed_frames.saturating_add(1);
                self.maybe_emit_silence_marker(0, on_packet);
            }
            CaptureGateDecision::Resume(frames) => {
                self.reset_opus_stream()?;
                let count = frames.len() as u32;
                for (index, frame) in frames.iter().enumerate() {
                    let slot_start = self
                        .next_capture_sample
                        .wrapping_sub(FRAME_SAMPLES as u32 * (count - index as u32));
                    let frame = ProcessedCaptureFrame::new(&frame.samples);
                    self.queue_processed_capture_frame(frame, slot_start, stats, on_packet)?;
                }
            }
        }
        Ok(())
    }

    fn handle_mute_state_change(&mut self, muted: bool, frame: &[f32]) {
        if muted == self.last_muted {
            return;
        }
        self.mute_transition_id = self.mute_transition_id.wrapping_add(1);
        self.last_muted = muted;
        if muted {
            // Discard the long-silence gate's state the moment the user mutes.
            // The mute fade owns silence from here and bypasses the gate, so its
            // `suppressed` flag and pre-mute preroll would otherwise survive the
            // mute and, on the next talkspurt, replay pre-mute audio as a
            // back-dated resume. The full-suppression unmute path also resets the
            // gate, but a mute/unmute shorter than the fade never reaches it.
            if let Some(gate) = self.long_silence.as_mut() {
                gate.reset();
            }
        }
        if !audio_pop_logging_enabled() {
            return;
        }
        kvlog::info!(
            "audio pop capture mute transition",
            transition_id = self.mute_transition_id,
            muted,
            mute_gain = self.mute_gain,
            mute_suppressed = self.mute_suppressed,
            pending_opus_samples = self.pending_opus_samples.len(),
            rms = rms_normalized(frame),
            peak = peak_normalized(frame),
            max_delta = max_adjacent_delta(frame),
            first_sample = frame.first().copied().unwrap_or_default(),
            last_sample = frame.last().copied().unwrap_or_default()
        );
    }

    /// Handles a captured frame during a mute fade-out, mute suppression, or
    /// unmute fade-in. The mute gain ramps toward 0 (muted) or 1 (unmuted):
    ///
    /// - While fading out, the faded frame is transmitted flagged
    ///   `MUTE | SILENCE_HINT` so the receiver fades the same tail and never treats
    ///   the pause as loss.
    /// - Once the gain reaches 0 the sender stops encoding and emits sparse
    ///   `Silence` keepalive markers (flagged `MUTE`) that occupy the sequence
    ///   numbers.
    /// - On unmute it resumes from suppression with a fresh Opus stream
    ///   (`OPUS_RESET | SILENCE_RESUME`) and fades the gain back in, so the tone
    ///   returns smoothly. Toggling mid-fade simply reverses the gain with no step.
    fn process_mute_transition(
        &mut self,
        frame: &mut [f32],
        muted: bool,
        stats: &AudioStats,
        on_packet: &mut dyn FnMut(LocalVoiceFrame),
    ) -> Result<(), String> {
        let target = if muted { 0.0 } else { 1.0 };
        let log_enabled = audio_pop_logging_enabled();
        let gain_before = self.mute_gain;
        let was_suppressed = self.mute_suppressed;
        let input_first = frame.first().copied().unwrap_or_default();
        let input_last = frame.last().copied().unwrap_or_default();
        let (input_rms, input_peak, input_max_delta) = if log_enabled {
            (
                rms_normalized(frame),
                peak_normalized(frame),
                max_adjacent_delta(frame),
            )
        } else {
            (0.0, 0.0, 0.0)
        };

        if muted && self.mute_gain <= 0.0 {
            // Fully faded out: stop encoding, emit only silence keepalives.
            self.mute_suppressed = true;
            self.discard_pending_before_suppression();
            self.suppressed_frames = self.suppressed_frames.saturating_add(1);
            self.maybe_emit_silence_marker(LIVE_PACKET_FLAG_MUTE, on_packet);
            return Ok(());
        }

        if self.mute_suppressed {
            // Resuming after a suppressed pause: re-anchor the Opus stream so the
            // first packet carries OPUS_RESET | SILENCE_RESUME. The silence gate
            // was already reset when the mute began and stays bypassed until the
            // fade fully reopens, so it needs no reset here.
            self.mute_suppressed = false;
            self.reset_opus_stream()?;
            if log_enabled {
                kvlog::info!(
                    "audio pop capture mute resume",
                    transition_id = self.mute_transition_id,
                    pending_opus_samples = self.pending_opus_samples.len(),
                    mute_gain = self.mute_gain,
                    input_rms,
                    input_peak,
                    input_max_delta,
                    input_first_sample = input_first,
                    input_last_sample = input_last
                );
            }
        }

        let fading_out = target <= 0.0;
        self.mute_gain = apply_gain_ramp(frame, self.mute_gain, target, self.mute_gain_step);
        if log_enabled
            && (muted
                || was_suppressed
                || gain_before < 1.0
                || self.mute_gain < 1.0
                || input_max_delta >= AUDIO_POP_DELTA_THRESHOLD)
        {
            kvlog::info!(
                "audio pop capture mute ramp",
                transition_id = self.mute_transition_id,
                muted,
                was_suppressed,
                target_gain = target,
                gain_before,
                gain_after = self.mute_gain,
                input_rms,
                input_peak,
                input_max_delta,
                input_first_sample = input_first,
                input_last_sample = input_last,
                output_rms = rms_normalized(frame),
                output_peak = peak_normalized(frame),
                output_max_delta = max_adjacent_delta(frame),
                output_first_sample = frame.first().copied().unwrap_or_default(),
                output_last_sample = frame.last().copied().unwrap_or_default()
            );
        }
        if fading_out {
            // The receiver fades this tail too and skips concealment over it.
            self.next_opus_packet_flags |= LIVE_PACKET_FLAG_MUTE | LIVE_PACKET_FLAG_SILENCE_HINT;
        }
        let slot_start = self.next_capture_sample.wrapping_sub(FRAME_SAMPLES as u32);
        let frame = ProcessedCaptureFrame::new(frame);
        self.queue_processed_capture_frame(frame, slot_start, stats, on_packet)
    }

    fn queue_processed_capture_frame(
        &mut self,
        frame: ProcessedCaptureFrame<'_>,
        slot_start_sample: u32,
        stats: &AudioStats,
        on_packet: &mut dyn FnMut(LocalVoiceFrame),
    ) -> Result<(), String> {
        if self.pending_opus_samples.is_empty() {
            self.pending_start_sample = slot_start_sample;
        } else {
            debug_assert_eq!(
                self.pending_start_sample
                    .wrapping_add(self.pending_opus_samples.len() as u32),
                slot_start_sample,
                "queued capture frame is not contiguous with pending samples"
            );
        }
        self.pending_opus_samples.extend_from_slice(frame.samples);

        while self.pending_opus_samples.len() >= LIVE_OPUS_FRAME_SAMPLES {
            let mut flags = self.next_opus_packet_flags;
            self.next_opus_packet_flags = 0;
            if self.silence_resume_hint_packets > 0 {
                flags |= LIVE_PACKET_FLAG_SILENCE_RESUME;
            }
            let encode_start = Instant::now();
            let payload = encode_live_frame(
                &self.pending_opus_samples[..LIVE_OPUS_FRAME_SAMPLES],
                &mut self.encoder,
                &mut self.opus_frame,
                &mut self.encoded,
            )?;
            let encode_us = encode_start.elapsed().as_micros() as u64;
            let timestamp = self.pending_start_sample;
            if audio_pop_logging_enabled() {
                kvlog::info!(
                    "audio pop capture encoded packet timing",
                    media_timestamp = timestamp,
                    flags,
                    payload_size = payload.len(),
                    pending_opus_samples = self.pending_opus_samples.len(),
                    encode_us
                );
            }
            if audio_pop_logging_enabled()
                && (flags != 0
                    || self.mute_gain < 1.0
                    || self.mute_suppressed
                    || max_adjacent_delta(&self.pending_opus_samples[..LIVE_OPUS_FRAME_SAMPLES])
                        >= AUDIO_POP_DELTA_THRESHOLD)
            {
                let samples = &self.pending_opus_samples[..LIVE_OPUS_FRAME_SAMPLES];
                kvlog::info!(
                    "audio pop capture encoded packet",
                    transition_id = self.mute_transition_id,
                    media_timestamp = timestamp,
                    flags,
                    flag_opus_reset = flags & LIVE_PACKET_FLAG_OPUS_RESET != 0,
                    flag_silence_hint = flags & LIVE_PACKET_FLAG_SILENCE_HINT != 0,
                    flag_silence_resume = flags & LIVE_PACKET_FLAG_SILENCE_RESUME != 0,
                    flag_mute = flags & LIVE_PACKET_FLAG_MUTE != 0,
                    encode_us,
                    mute_gain = self.mute_gain,
                    mute_suppressed = self.mute_suppressed,
                    packet_samples = LIVE_OPUS_FRAME_SAMPLES,
                    packet_rms = rms_normalized(samples),
                    packet_peak = peak_normalized(samples),
                    packet_max_delta = max_adjacent_delta(samples),
                    packet_first_sample = samples.first().copied().unwrap_or_default(),
                    packet_last_sample = samples.last().copied().unwrap_or_default(),
                    pending_opus_samples = self.pending_opus_samples.len(),
                    payload_size = payload.len()
                );
            }
            if self.silence_resume_hint_packets > 0 {
                self.silence_resume_hint_packets -= 1;
            }
            let packet_len = payload.len();
            self.sender_silence_active = false;
            self.silence_keepalive_frames = 0;
            on_packet(LocalVoiceFrame {
                flags,
                payload,
                timestamp,
            });
            stats.record_encoded_packet(packet_len);
            self.pending_opus_samples.drain(..LIVE_OPUS_FRAME_SAMPLES);
            self.pending_start_sample = self
                .pending_start_sample
                .wrapping_add(LIVE_OPUS_FRAME_SAMPLES as u32);
        }

        Ok(())
    }

    /// Drops any partial Opus frame left in `pending_opus_samples` at the start
    /// of a suppressed span (long-silence gate suppression or a fully faded
    /// mute).
    ///
    /// `next_capture_sample` advances for every 10 ms slot, including the
    /// suppressed ones that append nothing, so a partial frame kept across the
    /// gap would strand `pending_start_sample` behind the clock. The next queued
    /// frame — a gate `Resume`, or a mute fade-out that bypasses the gate — then
    /// fails the contiguity check in [`Self::queue_processed_capture_frame`].
    /// That partial is pre-silence tail which both resume paths discard through
    /// [`Self::reset_opus_stream`] anyway, so clearing it here transmits no less
    /// audio and keeps every queued frame contiguous with the clock.
    fn discard_pending_before_suppression(&mut self) {
        self.pending_opus_samples.clear();
    }

    fn maybe_emit_silence_marker(
        &mut self,
        extra_flags: u8,
        on_packet: &mut dyn FnMut(LocalVoiceFrame),
    ) {
        if self.sender_silence_active && self.silence_keepalive_frames > 0 {
            self.silence_keepalive_frames -= 1;
            return;
        }
        let entering_sender_silence = !self.sender_silence_active;
        self.sender_silence_active = true;
        self.silence_keepalive_frames = SILENCE_KEEPALIVE_CAPTURE_FRAMES;
        if entering_sender_silence {
            self.silence_resume_hint_packets = SILENCE_RESUME_HINT_PACKETS;
        }
        if entering_sender_silence && audio_pop_logging_enabled() {
            let flags = LIVE_PACKET_FLAG_SILENCE_HINT | extra_flags;
            let timestamp = self.next_capture_sample.wrapping_sub(FRAME_SAMPLES as u32);
            kvlog::info!(
                "audio pop capture silence marker",
                transition_id = self.mute_transition_id,
                media_timestamp = timestamp,
                flags,
                flag_silence_hint = flags & LIVE_PACKET_FLAG_SILENCE_HINT != 0,
                flag_mute = flags & LIVE_PACKET_FLAG_MUTE != 0,
                mute_gain = self.mute_gain,
                suppressed_frames = self.suppressed_frames,
                silence_resume_hint_packets = self.silence_resume_hint_packets
            );
        }
        // The marker stands in for the slot just accounted on the clock, so
        // stamp that slot's first sample index.
        let timestamp = self.next_capture_sample.wrapping_sub(FRAME_SAMPLES as u32);
        on_packet(LocalVoiceFrame {
            flags: LIVE_PACKET_FLAG_SILENCE_HINT | extra_flags,
            payload: VoicePayload::Silence,
            timestamp,
        });
    }

    fn take_accumulator(&mut self) -> FrameAccumulator {
        let frame_size = self.accumulator.frame_size;
        std::mem::replace(&mut self.accumulator, FrameAccumulator::empty(frame_size))
    }

    fn reset_opus_stream(&mut self) -> Result<(), String> {
        self.pending_opus_samples.clear();
        self.encoder.reset_state()?;
        self.next_opus_packet_flags = LIVE_PACKET_FLAG_OPUS_RESET;
        self.silence_resume_hint_packets = SILENCE_RESUME_HINT_PACKETS;
        self.sender_silence_active = true;
        self.silence_keepalive_frames = 0;
        if audio_pop_logging_enabled() {
            kvlog::info!(
                "audio pop capture opus reset",
                transition_id = self.mute_transition_id,
                next_capture_sample = self.next_capture_sample,
                mute_gain = self.mute_gain,
                mute_suppressed = self.mute_suppressed
            );
        }
        Ok(())
    }

    pub(crate) fn suppressed_frames(&self) -> u64 {
        self.suppressed_frames
    }

    /// The applied capture gain, for diagnostics only. AGC2 owns the gain
    /// internally and sonora exposes no per-frame value, so this returns `NaN`
    /// to mark the reading as unavailable rather than reporting a fabricated
    /// unity gain.
    pub(crate) fn current_gain(&self) -> f32 {
        f32::NAN
    }
}

#[cfg(test)]
pub(crate) fn build_live_encoder_pipeline(
    tuning: LiveAudioTuning,
    denoise_enabled: bool,
    max_amplification: f32,
    auto_gain_enabled: bool,
    network_profile: EncoderNetworkProfile,
    echo_reference: Option<Arc<EchoReference>>,
) -> Result<LiveEncoderPipeline, String> {
    build_live_encoder_pipeline_with_initial_mute(
        tuning,
        denoise_enabled,
        max_amplification,
        auto_gain_enabled,
        network_profile,
        echo_reference,
        false,
    )
}

pub(crate) fn build_live_encoder_pipeline_with_initial_mute(
    tuning: LiveAudioTuning,
    denoise_enabled: bool,
    max_amplification: f32,
    auto_gain_enabled: bool,
    network_profile: EncoderNetworkProfile,
    echo_reference: Option<Arc<EchoReference>>,
    initial_muted: bool,
) -> Result<LiveEncoderPipeline, String> {
    // The simulation and benchmark callers express denoise as on/off; on maps to
    // the default RNNoise engine.
    let denoise = if denoise_enabled {
        DenoiseConfig::RnnNoise
    } else {
        DenoiseConfig::None
    };
    let mut encoder = OpusVoiceEncoder::new(network_profile.bitrate_bps)?;
    encoder.apply_network_profile(network_profile)?;
    Ok(LiveEncoderPipeline::new(
        encoder,
        denoise,
        tuning,
        max_amplification,
        auto_gain_enabled,
        DenoiseSuppression::IDENTITY,
        DenoiseTypingSuppression::DISABLED,
        echo_reference.map(EchoReferenceSource::Always),
        crate::audio::shared::SAMPLE_RATE,
        initial_muted,
    ))
}

pub(crate) fn encode_live_frame(
    frame: &[f32],
    encoder: &mut OpusVoiceEncoder,
    opus_frame: &mut [i16],
    encoded: &mut [u8],
) -> Result<VoicePayload, String> {
    convert_i16_scale_to_pcm_i16(frame, opus_frame);
    let packet_len = encoder.encode(opus_frame, encoded)?;
    Ok(VoicePayload::Opus(encoded[..packet_len].to_vec()))
}

pub(crate) struct FrameAccumulator {
    frame_size: usize,
    pending: Vec<f32>,
}

impl FrameAccumulator {
    fn new(frame_size: usize) -> Self {
        Self {
            frame_size,
            pending: Vec::with_capacity(frame_size),
        }
    }

    fn empty(frame_size: usize) -> Self {
        Self {
            frame_size,
            pending: Vec::new(),
        }
    }

    fn push_chunk(
        &mut self,
        mut chunk: &[f32],
        mut on_frame: impl FnMut(&mut [f32]) -> Result<(), String>,
    ) -> Result<(), String> {
        // Copy in frame-sized runs rather than sample by sample: each
        // `extend_from_slice` is a single bulk memcpy the compiler can vectorize,
        // and a full frame that lands on an empty buffer skips straight to the
        // callback without per-sample bookkeeping.
        while !chunk.is_empty() {
            let needed = self.frame_size - self.pending.len();
            let take = needed.min(chunk.len());
            self.pending.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.pending.len() == self.frame_size {
                on_frame(&mut self.pending)?;
                self.pending.clear();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::EchoCancellationControl;
    use crate::audio::shared::{
        DEFAULT_LIVE_MAX_AMPLIFICATION, LIVE_PLAYBACK_DRED_MAX_SAMPLES, SAMPLE_RATE,
        frames_for_duration, normalized_to_i16_scale,
    };
    #[allow(unused_imports)]
    use crate::audio::test_support::*;
    use opus_codec::{Channels, Decoder, DredDecoder, DredState, SampleRate};
    use std::time::Duration;

    #[test]
    fn live_encoder_pipeline_emits_parseable_dred_for_sampled_speech() {
        let mut pipeline = build_live_encoder_pipeline(
            test_tuning(),
            true,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::CRITICAL,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let mut packets = Vec::new();

        for frame in sample_speech_frames().iter().take(240) {
            let chunk = normalized_to_i16_scale(frame);
            pipeline
                .push_chunk(
                    &chunk,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    false,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }

        let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
        let mut dred_decoder = DredDecoder::new().unwrap();
        let mut output = vec![0.0f32; LIVE_OPUS_FRAME_SAMPLES];
        let mut recoverable = 0usize;
        for packet in &packets {
            let VoicePayload::Opus(payload) = &packet.payload else {
                continue;
            };
            let mut dred_state = DredState::new().unwrap();
            let mut dred_end = 0;
            let parsed = dred_decoder
                .parse(
                    &mut dred_state,
                    payload.as_slice(),
                    LIVE_PLAYBACK_DRED_MAX_SAMPLES,
                    SampleRate::Hz48000,
                    &mut dred_end,
                    false,
                )
                .unwrap_or(0);
            let Ok(offset_samples) = i32::try_from(parsed) else {
                continue;
            };
            if offset_samples > 0
                && dred_decoder
                    .decode_into_f32(&mut decoder, &dred_state, offset_samples, &mut output)
                    .is_ok()
            {
                recoverable += 1;
            }
        }

        let direct_10ms = count_direct_encoder_recoverable_dred(FRAME_SAMPLES);
        let direct_20ms = count_direct_encoder_recoverable_dred(FRAME_SAMPLES * 2);

        assert!(
            recoverable > 0,
            "live encoder emitted no parseable DRED across {} packets; direct_10ms={direct_10ms}, direct_20ms={direct_20ms}",
            packets.len()
        );
    }

    #[test]
    fn live_encoder_marks_resume_after_silence_gate_as_opus_reset() {
        let mut tuning = test_tuning();
        tuning.capture_long_silence_stop = Duration::from_millis(20);
        tuning.capture_silence_preroll = Duration::from_millis(20);
        tuning.capture_silence_ramp = Duration::ZERO;
        tuning.silence_vad_max = u8::MAX;

        let mut pipeline = build_live_encoder_pipeline(
            tuning,
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let mut packets = Vec::new();
        let sampled_speech = sample_high_energy_speech_frame()
            .iter()
            .map(|sample| (sample * 6.0).clamp(-1.0, 1.0))
            .collect::<Vec<_>>();
        let speech = normalized_to_i16_scale(&sampled_speech);
        let silence = vec![0.0f32; FRAME_SAMPLES];

        for _ in 0..4 {
            pipeline
                .push_chunk(
                    &speech,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    false,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }
        for _ in 0..5 {
            pipeline
                .push_chunk(
                    &silence,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    false,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }
        for _ in 0..4 {
            pipeline
                .push_chunk(
                    &speech,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    false,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }

        let reset_index = packets
            .iter()
            .enumerate()
            .find_map(|(index, packet)| {
                (index > 0 && packet.flags & LIVE_PACKET_FLAG_OPUS_RESET != 0).then_some(index)
            })
            .expect("resume packet should carry opus reset flag");
        assert!(
            reset_index > 0,
            "resume reset should not be the first packet"
        );
        assert_ne!(
            packets[0].flags & LIVE_PACKET_FLAG_OPUS_RESET,
            0,
            "fresh encoder should reset Opus on its first packet"
        );
        assert!(
            packets[1..reset_index]
                .iter()
                .all(|packet| packet.flags & LIVE_PACKET_FLAG_OPUS_RESET == 0)
        );
        assert!(
            packets[..reset_index]
                .iter()
                .any(|packet| packet.flags & LIVE_PACKET_FLAG_SILENCE_HINT != 0),
            "transmitted silence before suppression should advertise the pending sender pause"
        );
        assert_ne!(
            packets[reset_index].flags & LIVE_PACKET_FLAG_SILENCE_RESUME,
            0,
            "resume packet should advertise the sender pause it follows"
        );
        assert!(
            packets[reset_index..]
                .iter()
                .all(|packet| packet.flags & LIVE_PACKET_FLAG_SILENCE_HINT == 0),
            "resume speech must not keep advertising a pending silence pause"
        );
        assert!(
            packets[reset_index + 1..]
                .iter()
                .all(|packet| packet.flags & LIVE_PACKET_FLAG_OPUS_RESET == 0)
        );
        assert!(
            packets[reset_index + 1..]
                .iter()
                .any(|packet| packet.flags & LIVE_PACKET_FLAG_SILENCE_RESUME != 0),
            "resume marker should survive loss of the first resume packet"
        );
    }

    #[test]
    fn silence_marker_marks_next_opus_packet_as_resume() {
        let mut pipeline = build_live_encoder_pipeline(
            test_tuning(),
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let mut silence_packets = Vec::new();
        pipeline.next_opus_packet_flags = 0;
        pipeline.silence_resume_hint_packets = 0;
        pipeline.sender_silence_active = false;

        pipeline.maybe_emit_silence_marker(0, &mut |packet| silence_packets.push(packet));

        assert_eq!(silence_packets.len(), 1);
        assert_ne!(silence_packets[0].flags & LIVE_PACKET_FLAG_SILENCE_HINT, 0);
        assert_eq!(
            pipeline.silence_resume_hint_packets,
            SILENCE_RESUME_HINT_PACKETS
        );

        let sampled_speech = sample_high_energy_speech_frame()
            .iter()
            .map(|sample| (sample * 6.0).clamp(-1.0, 1.0))
            .collect::<Vec<_>>();
        let mut speech_packets = Vec::new();
        for slot in 0..2 {
            let frame = ProcessedCaptureFrame::new(&sampled_speech);
            pipeline
                .queue_processed_capture_frame(
                    frame,
                    FRAME_SAMPLES as u32 * slot,
                    &stats,
                    &mut |packet| speech_packets.push(packet),
                )
                .unwrap();
        }

        let first_opus = speech_packets
            .iter()
            .find(|packet| matches!(packet.payload, VoicePayload::Opus(_)))
            .expect("speech should produce an Opus packet");
        assert_ne!(first_opus.flags & LIVE_PACKET_FLAG_SILENCE_RESUME, 0);
        assert_eq!(first_opus.flags & LIVE_PACKET_FLAG_OPUS_RESET, 0);
    }

    fn high_energy_speech_chunk() -> Vec<f32> {
        let sampled = sample_high_energy_speech_frame()
            .iter()
            .map(|sample| (sample * 6.0).clamp(-1.0, 1.0))
            .collect::<Vec<_>>();
        normalized_to_i16_scale(&sampled)
    }

    fn drive_silence_gate_cycles(
        preroll: Duration,
        cycles: usize,
    ) -> (Vec<LocalVoiceFrame>, Vec<u32>, usize) {
        let mut tuning = test_tuning();
        tuning.capture_silence_gate = true;
        tuning.capture_long_silence_stop = Duration::from_millis(20);
        tuning.capture_silence_preroll = preroll;
        tuning.capture_silence_ramp = Duration::ZERO;
        tuning.silence_vad_max = u8::MAX;

        let mut pipeline = build_live_encoder_pipeline(
            tuning,
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let speech = high_energy_speech_chunk();
        let silence = vec![0.0f32; FRAME_SAMPLES];
        let mut packets = Vec::new();
        let mut pushed_frames = 0u32;
        let mut resume_slot_starts = Vec::new();

        let push = |pipeline: &mut LiveEncoderPipeline,
                    frame: &[f32],
                    slot_start: u32,
                    packets: &mut Vec<LocalVoiceFrame>,
                    resume_slot_starts: &mut Vec<u32>| {
            let had_opus = packets
                .iter()
                .any(|packet| matches!(packet.payload, VoicePayload::Opus(_)));
            let before = packets.len();
            pipeline
                .push_chunk(
                    frame,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    false,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
            if had_opus
                && packets[before..].iter().any(|packet| {
                    matches!(packet.payload, VoicePayload::Opus(_))
                        && packet.flags & LIVE_PACKET_FLAG_OPUS_RESET != 0
                })
            {
                resume_slot_starts.push(slot_start);
            }
        };

        for _ in 0..4 {
            let slot_start = pushed_frames * FRAME_SAMPLES as u32;
            push(
                &mut pipeline,
                &speech,
                slot_start,
                &mut packets,
                &mut resume_slot_starts,
            );
            pushed_frames += 1;
        }

        for _ in 0..cycles {
            for _ in 0..10 {
                let slot_start = pushed_frames * FRAME_SAMPLES as u32;
                push(
                    &mut pipeline,
                    &silence,
                    slot_start,
                    &mut packets,
                    &mut resume_slot_starts,
                );
                pushed_frames += 1;
            }
            for _ in 0..4 {
                let slot_start = pushed_frames * FRAME_SAMPLES as u32;
                push(
                    &mut pipeline,
                    &speech,
                    slot_start,
                    &mut packets,
                    &mut resume_slot_starts,
                );
                pushed_frames += 1;
            }
        }

        let preroll_frames = frames_for_duration(preroll);
        (packets, resume_slot_starts, preroll_frames)
    }

    fn opus_packets(packets: &[LocalVoiceFrame]) -> Vec<&LocalVoiceFrame> {
        packets
            .iter()
            .filter(|packet| matches!(packet.payload, VoicePayload::Opus(_)))
            .collect()
    }

    fn assert_resume_timestamps(
        packets: &[LocalVoiceFrame],
        resume_slot_starts: &[u32],
        preroll_frames: usize,
    ) {
        let opus = opus_packets(packets);
        let timestamps = opus
            .iter()
            .map(|packet| packet.timestamp)
            .collect::<Vec<_>>();
        for (index, timestamp) in timestamps.iter().enumerate() {
            assert!(
                !timestamps[..index].contains(timestamp),
                "duplicate Opus timestamp {timestamp} in {timestamps:?}"
            );
        }

        let resume_indexes = opus
            .iter()
            .enumerate()
            .filter_map(|(index, packet)| {
                (index > 0 && packet.flags & LIVE_PACKET_FLAG_OPUS_RESET != 0).then_some(index)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            resume_indexes.len(),
            resume_slot_starts.len(),
            "unexpected resume packets in {timestamps:?}"
        );

        let replayed_frames = preroll_frames + 1;
        let immediate_resume_packets = replayed_frames / 2;
        assert!(
            immediate_resume_packets > 0,
            "test scenario must replay enough frames to emit a resume packet"
        );
        for (&resume_index, &resume_slot_start) in resume_indexes.iter().zip(resume_slot_starts) {
            let first_resume = opus[resume_index].timestamp;
            let expected_first =
                resume_slot_start.wrapping_sub(FRAME_SAMPLES as u32 * preroll_frames as u32);
            assert_eq!(
                first_resume, expected_first,
                "first resume packet should be back-dated by the preroll"
            );

            let burst_end = resume_index + immediate_resume_packets;
            assert!(
                burst_end < opus.len(),
                "resume should be followed by a live packet: {timestamps:?}"
            );
            for index in resume_index + 1..=burst_end {
                assert_eq!(
                    opus[index]
                        .timestamp
                        .wrapping_sub(opus[index - 1].timestamp),
                    LIVE_OPUS_FRAME_SAMPLES as u32,
                    "resume packets must be contiguous: {timestamps:?}"
                );
            }
        }
    }

    #[test]
    fn silence_gate_resume_emits_backdated_contiguous_timestamps() {
        let preroll = Duration::from_millis(30);
        let (packets, resume_slot_starts, preroll_frames) = drive_silence_gate_cycles(preroll, 1);

        assert_resume_timestamps(&packets, &resume_slot_starts, preroll_frames);
    }

    #[test]
    fn silence_gate_resume_with_leftover_pending_samples_stays_contiguous() {
        let preroll = Duration::from_millis(20);
        let (packets, resume_slot_starts, preroll_frames) = drive_silence_gate_cycles(preroll, 1);

        assert_resume_timestamps(&packets, &resume_slot_starts, preroll_frames);
    }

    #[test]
    fn silence_gate_resume_timestamps_reanchor_across_cycles() {
        let preroll = Duration::from_millis(30);
        let (packets, resume_slot_starts, preroll_frames) = drive_silence_gate_cycles(preroll, 3);

        assert_resume_timestamps(&packets, &resume_slot_starts, preroll_frames);
    }

    #[test]
    fn muting_during_gate_silence_suppression_stays_contiguous() {
        // Reproduces the capture-pipeline panic seen when a user clicks mute
        // while the long-silence gate is already suppressing: the gate leaves a
        // partial Opus frame in `pending_opus_samples`, the media clock keeps
        // advancing across the suppressed slots, and the mute fade-out bypasses
        // the gate (so it never resumes/resets) and queues a frame that used to
        // fail the contiguity assertion.
        let mut tuning = test_tuning();
        tuning.capture_silence_gate = true;
        tuning.capture_long_silence_stop = Duration::from_millis(20);
        tuning.capture_silence_preroll = Duration::from_millis(20);
        tuning.capture_silence_ramp = Duration::ZERO;
        tuning.silence_vad_max = u8::MAX;

        let mut pipeline = build_live_encoder_pipeline(
            tuning,
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let speech = high_energy_speech_chunk();
        let silence = vec![0.0f32; FRAME_SAMPLES];
        let mut packets = Vec::new();

        let mut push = |pipeline: &mut LiveEncoderPipeline, frame: &[f32], muted: bool| {
            pipeline
                .push_chunk(
                    frame,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    muted,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        };

        // Prime enough speech frames that the running transmitted-frame count is
        // odd when the gate suppresses, so a 480-sample partial is still pending
        // at that point — the stale buffer that used to desynchronise the mute
        // fade-out from the clock. The count is one 10 ms Opus half-frame, so the
        // exact priming depends on how many silence frames precede suppression.
        for _ in 0..5 {
            push(&mut pipeline, &speech, false);
        }
        // Feed silence until the gate suppresses, recording the pending fill the
        // suppressing frame inherited. That partial is the historical hazard, so
        // the test asserts it is genuinely non-empty and still reproduces the bug.
        let mut suppress_pending = None;
        for _ in 0..12 {
            let was_suppressing = pipeline.suppressed_frames > 0;
            let pending_before = pipeline.pending_opus_samples.len();
            push(&mut pipeline, &silence, false);
            if !was_suppressing && pipeline.suppressed_frames > 0 {
                suppress_pending = Some(pending_before);
            }
        }
        assert_eq!(
            suppress_pending,
            Some(FRAME_SAMPLES),
            "scenario must leave a partial frame pending when suppression begins"
        );

        // The user clicks mute mid-suppression. The fade-out and full-mute
        // frames must queue without panicking on a stale, non-contiguous pending
        // buffer.
        for _ in 0..40 {
            push(&mut pipeline, &silence, true);
        }

        // Unmuting resumes cleanly, and every Opus packet keeps a unique
        // timestamp anchored to the advancing media clock.
        for _ in 0..6 {
            push(&mut pipeline, &speech, false);
        }

        let timestamps = opus_packets(&packets)
            .iter()
            .map(|packet| packet.timestamp)
            .collect::<Vec<_>>();
        for (index, timestamp) in timestamps.iter().enumerate() {
            assert!(
                !timestamps[..index].contains(timestamp),
                "duplicate Opus timestamp {timestamp} in {timestamps:?}"
            );
        }
    }

    #[test]
    fn brief_mute_toggle_during_gate_suppression_resets_the_gate() {
        // A mute/unmute cycle shorter than the 60 ms fade never reaches full
        // mute suppression, so `mute_suppressed` never latches and the unmute
        // path that resets the long-silence gate is skipped. If the gate was
        // suppressing silence when the user muted, its stale `suppressed` state
        // and pre-mute preroll survive the toggle, and the first post-unmute
        // talkspurt makes the gate emit a back-dated `Resume` (OPUS_RESET) that
        // replays pre-mute audio. The mute transition must reset the gate.
        let mut tuning = test_tuning();
        tuning.capture_silence_gate = true;
        tuning.capture_long_silence_stop = Duration::from_millis(20);
        tuning.capture_silence_preroll = Duration::from_millis(30);
        tuning.capture_silence_ramp = Duration::ZERO;
        tuning.silence_vad_max = u8::MAX;

        let mut pipeline = build_live_encoder_pipeline(
            tuning,
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let speech = high_energy_speech_chunk();
        let silence = vec![0.0f32; FRAME_SAMPLES];
        let mut packets = Vec::new();

        let push = |pipeline: &mut LiveEncoderPipeline,
                    frame: &[f32],
                    muted: bool,
                    packets: &mut Vec<LocalVoiceFrame>| {
            pipeline
                .push_chunk(
                    frame,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    muted,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        };

        // Talk, then go silent long enough for the gate to suppress and buffer
        // pre-mute preroll frames.
        for _ in 0..4 {
            push(&mut pipeline, &speech, false, &mut packets);
        }
        for _ in 0..12 {
            push(&mut pipeline, &silence, false, &mut packets);
        }
        assert!(
            pipeline.suppressed_frames > 0,
            "gate must be suppressing before the mute click"
        );

        let before_mute = packets.len();

        // Briefly mute (three frames, well under the six-frame fade) then let
        // the fade back in complete. The fade never reaches zero, so
        // `mute_suppressed` stays false and the full-suppression unmute reset is
        // never taken.
        for _ in 0..3 {
            push(&mut pipeline, &silence, true, &mut packets);
        }
        assert!(
            !pipeline.mute_suppressed,
            "fade must not complete, or the scenario reduces to the reset path"
        );
        while pipeline.mute_gain < 1.0 {
            push(&mut pipeline, &silence, false, &mut packets);
        }

        // The first talkspurt after the fade fully reopens reaches the gate. A
        // reset gate transmits it directly; a stale, still-suppressed gate
        // resumes and replays a back-dated OPUS_RESET.
        push(&mut pipeline, &speech, false, &mut packets);

        let replayed_reset = packets[before_mute..].iter().any(|packet| {
            matches!(packet.payload, VoicePayload::Opus(_))
                && packet.flags & LIVE_PACKET_FLAG_OPUS_RESET != 0
        });
        assert!(
            !replayed_reset,
            "a brief mute toggle left the gate suppressed and replayed pre-mute audio"
        );
    }

    #[test]
    fn capture_gap_advances_media_clock_and_resets_stream() {
        let mut tuning = test_tuning();
        tuning.capture_silence_gate = false;
        let mut pipeline = build_live_encoder_pipeline(
            tuning,
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let speech = high_energy_speech_chunk();
        let mut packets = Vec::new();
        for _ in 0..4 {
            pipeline
                .push_chunk(
                    &speech,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    false,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }
        let last_pre_gap = opus_packets(&packets)
            .last()
            .expect("speech before the gap should emit Opus packets")
            .timestamp;
        let pre_gap_count = packets.len();

        // One second of capture was dropped under load.
        pipeline.note_capture_gap(SAMPLE_RATE).unwrap();

        for _ in 0..4 {
            pipeline
                .push_chunk(
                    &speech,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    false,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }
        let resumed = packets[pre_gap_count..]
            .iter()
            .find(|packet| matches!(packet.payload, VoicePayload::Opus(_)))
            .expect("speech after the gap should emit Opus packets");
        assert_ne!(
            resumed.flags & LIVE_PACKET_FLAG_OPUS_RESET,
            0,
            "the stream must re-anchor rather than continue the encoder over the hole"
        );
        let jump = resumed.timestamp.wrapping_sub(last_pre_gap);
        assert!(
            jump >= SAMPLE_RATE,
            "media clock spliced across the dropped capture: jump {jump}"
        );
    }

    /// Nanoseconds cpal advances the capture instant per 10 ms callback.
    const CALLBACK_PERIOD_NS: u64 = 10_000_000;

    /// Drives 24 tone callbacks through the worker at a 48 kHz device rate, with
    /// each callback's `cpal_capture_ns` supplied by `capture_ns(sequence)`, and
    /// returns the emitted packets. `dropped_before` seeds the software-drop
    /// counter to prove it no longer drives the clock.
    fn run_worker_with_capture_timestamps(
        capture_ns: impl Fn(u64) -> u64,
        dropped_before: u64,
    ) -> Vec<LocalVoiceFrame> {
        use std::sync::mpsc::sync_channel;

        let mut tuning = test_tuning();
        tuning.capture_silence_gate = false;
        let stats = AudioStats::new();
        let device_rate = 48_000u32;
        if dropped_before > 0 {
            stats.record_dropped_chunk(dropped_before);
        }

        let (sender, receiver) = sync_channel::<CapturedAudioChunk>(64);
        let (recycle_sender, _recycle_receiver) = sync_channel::<Vec<f32>>(64);
        let tone: Vec<f32> = (0..device_rate as usize / 100)
            .map(|n| (n as f32 * 0.07).sin() * 8_000.0)
            .collect();
        for sequence in 0..24 {
            sender
                .send(CapturedAudioChunk::new(
                    tone.clone(),
                    Instant::now(),
                    crate::audio::shared::CaptureCallbackTiming {
                        callback_sequence: sequence,
                        cpal_capture_ns: capture_ns(sequence),
                        ..crate::audio::shared::CaptureCallbackTiming::default()
                    },
                    0,
                ))
                .unwrap();
        }
        drop(sender);

        let max_amplification_bits = AtomicU32::new(DEFAULT_LIVE_MAX_AMPLIFICATION.to_bits());
        let encoder_loss_percent =
            AtomicU32::new(LiveEncoderProfile::DRED_20.packet_loss_percent as u32);
        let mic_muted = AtomicBool::new(false);
        let deafened = AtomicBool::new(false);
        let mut packets = Vec::new();
        run_live_encoder_worker_inner(
            receiver,
            recycle_sender,
            OpusVoiceEncoder::new(48_000).unwrap(),
            DenoiseConfig::None,
            &max_amplification_bits,
            &encoder_loss_percent,
            &mic_muted,
            &deafened,
            tuning,
            DenoiseSuppression::IDENTITY,
            DenoiseTypingSuppression::DISABLED,
            None,
            device_rate,
            &stats,
            &mut |packet| packets.push(packet),
        )
        .unwrap();
        packets
    }

    /// The first Opus packet whose timestamp jumps by at least `SAMPLE_RATE` from
    /// the previous Opus packet, i.e. the packet that resumes after a gap.
    fn resumed_after_gap(packets: &[LocalVoiceFrame]) -> &LocalVoiceFrame {
        let opus = opus_packets(packets);
        opus.windows(2)
            .find(|pair| pair[1].timestamp.wrapping_sub(pair[0].timestamp) >= SAMPLE_RATE)
            .map(|pair| pair[1])
            .expect("a post-gap resume packet should be emitted")
    }

    #[test]
    fn capture_timestamp_slaves_media_clock() {
        let mut tuning = test_tuning();
        tuning.capture_silence_gate = false;
        let mut pipeline = build_live_encoder_pipeline(
            tuning,
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        // One 10 ms frame at 48 kHz, so each callback advances the clock by exactly
        // FRAME_SAMPLES and stays locked to a 10 ms capture-timestamp cadence.
        let frame: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|n| (n as f32 * 0.07).sin() * 8_000.0)
            .collect();
        let push = |pipeline: &mut LiveEncoderPipeline| {
            pipeline
                .push_chunk(
                    &frame,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    false,
                    &stats,
                    &mut |_| {},
                )
                .unwrap();
        };

        // Steady cadence: the capture instant advances exactly one period per
        // processed frame, so no gap is ever emitted.
        pipeline.note_capture_timestamp(0).unwrap();
        push(&mut pipeline);
        for slot in 1..8u64 {
            pipeline
                .note_capture_timestamp(slot * CALLBACK_PERIOD_NS)
                .unwrap();
            push(&mut pipeline);
        }
        assert_eq!(
            pipeline.next_capture_sample,
            8 * FRAME_SAMPLES as u32,
            "steady capture timestamps must not perturb the media clock"
        );

        // Sub-frame jitter: one callback's capture instant lands 3 ms late but the
        // next returns to the true grid, so no audio was actually lost and no gap
        // is emitted.
        pipeline
            .note_capture_timestamp(8 * CALLBACK_PERIOD_NS + 3_000_000)
            .unwrap();
        push(&mut pipeline);
        pipeline
            .note_capture_timestamp(9 * CALLBACK_PERIOD_NS)
            .unwrap();
        push(&mut pipeline);
        assert_eq!(
            pipeline.next_capture_sample,
            10 * FRAME_SAMPLES as u32,
            "sub-frame jitter that returns to grid must not emit a gap"
        );

        // A full extra period the pipeline never saw: the capture instant runs one
        // frame ahead of the counted clock, so the missing frame is inserted and
        // the clock advances by the pushed frame plus the one-frame gap.
        let before = pipeline.next_capture_sample;
        pipeline
            .note_capture_timestamp(11 * CALLBACK_PERIOD_NS)
            .unwrap();
        push(&mut pipeline);
        let advanced = pipeline.next_capture_sample.wrapping_sub(before);
        assert_eq!(
            advanced,
            2 * FRAME_SAMPLES as u32,
            "a dropped period must advance the clock by the pushed frame plus the gap"
        );
    }

    #[test]
    fn live_worker_applies_capture_timestamp_gap_to_media_clock() {
        // A full second of capture is lost between callbacks 11 and 12: the
        // capture instant jumps a second beyond the steady cadence.
        let packets = run_worker_with_capture_timestamps(
            |sequence| {
                let extra = if sequence >= 12 { 1_000_000_000 } else { 0 };
                sequence * CALLBACK_PERIOD_NS + extra
            },
            0,
        );

        let resumed = resumed_after_gap(&packets);
        assert_ne!(
            resumed.flags & LIVE_PACKET_FLAG_OPUS_RESET,
            0,
            "the stream must re-anchor across the capture-timestamp gap"
        );
    }

    #[test]
    fn capture_timestamp_gap_subsumes_software_drop() {
        // Same one-second capture gap, but with the software-drop counter seeded to
        // a huge value. The clock must advance by the timestamp-implied second
        // only: the counter no longer drives the clock, so there is no double
        // count.
        let packets = run_worker_with_capture_timestamps(
            |sequence| {
                let extra = if sequence >= 12 { 1_000_000_000 } else { 0 };
                sequence * CALLBACK_PERIOD_NS + extra
            },
            10 * u64::from(SAMPLE_RATE),
        );

        let opus = opus_packets(&packets);
        let jump = opus
            .windows(2)
            .map(|pair| pair[1].timestamp.wrapping_sub(pair[0].timestamp))
            .find(|jump| *jump >= SAMPLE_RATE)
            .expect("a post-gap resume packet should be emitted");
        assert!(
            jump < 2 * SAMPLE_RATE,
            "the software-drop counter double-counted the gap: jump {jump}"
        );
    }

    /// Runs an unmuted -> muted -> unmuted episode, returning the emitted packets
    /// and the index at which the post-unmute (resume) packets begin.
    fn drive_mute_episode(silence_gate: bool) -> (Vec<LocalVoiceFrame>, usize) {
        let mut tuning = test_tuning();
        tuning.capture_silence_gate = silence_gate;
        let mut pipeline = build_live_encoder_pipeline(
            tuning,
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let speech = high_energy_speech_chunk();
        let mut packets = Vec::new();
        let push = |pipeline: &mut LiveEncoderPipeline, muted, packets: &mut Vec<_>| {
            pipeline
                .push_chunk(
                    &speech,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    muted,
                    &stats,
                    &mut |p| packets.push(p),
                )
                .unwrap();
        };

        // Warm up unmuted, then mute for long enough to exit the fade and emit a
        // silence marker, then unmute again.
        for _ in 0..6 {
            push(&mut pipeline, false, &mut packets);
        }
        for _ in 0..40 {
            push(&mut pipeline, true, &mut packets);
        }
        let resume_start = packets.len();
        for _ in 0..4 {
            push(&mut pipeline, false, &mut packets);
        }
        (packets, resume_start)
    }

    #[test]
    fn capture_timestamp_advances_across_silence() {
        // The whole point of the media timestamp is that it tracks real elapsed
        // time even while the sender suppresses audio. A receiver therefore sees
        // the resume packet jump far past the last pre-mute packet, instead of
        // the small +1-per-emitted-packet step the sequence number takes.
        let (packets, resume_start) = drive_mute_episode(true);
        let opus_timestamps = |slice: &[LocalVoiceFrame]| -> Vec<u32> {
            slice
                .iter()
                .filter(|p| matches!(p.payload, VoicePayload::Opus(_)))
                .map(|p| p.timestamp)
                .collect()
        };
        let pre = opus_timestamps(&packets[..resume_start]);
        let post = opus_timestamps(&packets[resume_start..]);
        assert!(!pre.is_empty() && !post.is_empty());

        // Every packet's timestamp is strictly increasing on the media clock.
        let all = opus_timestamps(&packets);
        for window in all.windows(2) {
            assert!(
                window[1] > window[0],
                "timestamps must be monotonic: {all:?}"
            );
        }

        // Across the ~40-slot muted gap the clock advanced through the silence,
        // so the resume timestamp jumps well past a single 20 ms opus frame.
        let last_pre = *pre.last().unwrap();
        let first_post = post[0];
        assert!(
            first_post - last_pre > LIVE_OPUS_FRAME_SAMPLES as u32 * 4,
            "resume timestamp {first_post} should jump well past pre-mute {last_pre}"
        );
    }

    #[test]
    fn mute_fades_then_emits_silence_markers_and_resumes() {
        let (packets, resume_start) = drive_mute_episode(true);

        // The fade tail is whole Opus packets bounded by the 60 ms fade window
        // (3 x 20 ms), each carrying the mute flag.
        let mute_opus = packets
            .iter()
            .filter(|p| p.flags & LIVE_PACKET_FLAG_MUTE != 0)
            .filter(|p| matches!(p.payload, VoicePayload::Opus(_)))
            .count();
        assert!(
            (1..=3).contains(&mute_opus),
            "expected 1..=3 faded mute Opus packets, got {mute_opus}"
        );
        assert!(
            packets.iter().any(|p| p.flags & LIVE_PACKET_FLAG_MUTE != 0
                && matches!(p.payload, VoicePayload::Silence)),
            "mute should drop to silence markers after the fade tail"
        );
        assert!(
            packets.iter().all(|p| {
                p.flags & LIVE_PACKET_FLAG_MUTE == 0 || p.flags & LIVE_PACKET_FLAG_SILENCE_HINT != 0
            }),
            "mute packets must also carry the silence hint so the jitter buffer skips them"
        );

        // The first Opus packet after unmute re-anchors the Opus stream.
        let resume = packets[resume_start..]
            .iter()
            .find(|p| matches!(p.payload, VoicePayload::Opus(_)))
            .expect("unmute should produce an Opus packet");
        assert_ne!(
            resume.flags & LIVE_PACKET_FLAG_OPUS_RESET,
            0,
            "resume packet should reset the Opus stream"
        );
        assert_ne!(
            resume.flags & LIVE_PACKET_FLAG_SILENCE_RESUME,
            0,
            "resume packet should advertise the mute pause it follows"
        );
        assert_eq!(
            resume.flags & LIVE_PACKET_FLAG_MUTE,
            0,
            "resumed speech must not still be flagged as muted"
        );
    }

    #[test]
    fn brief_mute_toggle_stays_continuous() {
        // Muting then unmuting before the fade completes must keep the Opus stream
        // continuous: no suppression, no reset, no silence marker. A reset on a
        // held tone is exactly what produces a click on a rapid toggle.
        let mut pipeline = build_live_encoder_pipeline(
            test_tuning(),
            false,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        let stats = AudioStats::new();
        let speech = high_energy_speech_chunk();
        let mut packets = Vec::new();
        let push = |pipeline: &mut LiveEncoderPipeline, muted, packets: &mut Vec<_>| {
            pipeline
                .push_chunk(
                    &speech,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    muted,
                    &stats,
                    &mut |p| packets.push(p),
                )
                .unwrap();
        };

        for _ in 0..6 {
            push(&mut pipeline, false, &mut packets);
        }
        let mark = packets.len();
        // Two muted frames begin the 60 ms fade but never reach silence.
        for _ in 0..2 {
            push(&mut pipeline, true, &mut packets);
        }
        for _ in 0..8 {
            push(&mut pipeline, false, &mut packets);
        }

        let after = &packets[mark..];
        assert!(
            after
                .iter()
                .all(|p| !matches!(p.payload, VoicePayload::Silence)),
            "a brief toggle must not drop to silence markers"
        );
        assert!(
            after
                .iter()
                .all(|p| p.flags & LIVE_PACKET_FLAG_OPUS_RESET == 0),
            "a brief toggle must not reset the Opus stream (would click)"
        );
        assert!(
            after.iter().any(|p| p.flags & LIVE_PACKET_FLAG_MUTE != 0),
            "the brief fade-out should still be flagged so the receiver fades it"
        );
    }

    #[test]
    fn mute_works_with_silence_gate_disabled() {
        // Mute must not depend on the optional long-silence gate.
        let (packets, _) = drive_mute_episode(false);
        assert!(
            packets.iter().any(|p| p.flags & LIVE_PACKET_FLAG_MUTE != 0
                && matches!(p.payload, VoicePayload::Silence)),
            "mute should still emit silence markers when the capture gate is off"
        );
        assert!(
            packets
                .iter()
                .any(|p| p.flags & LIVE_PACKET_FLAG_OPUS_RESET != 0
                    && p.flags & LIVE_PACKET_FLAG_SILENCE_RESUME != 0),
            "mute should still resume cleanly when the capture gate is off"
        );
    }

    #[test]
    fn mute_fade_tail_ramps_down_to_silence() {
        // The concatenated fade tail must end near silence: a soft ramp rather
        // than a hard cut, so the receiver hears no click into the mute.
        let (packets, _) = drive_mute_episode(false);
        let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
        let mut output = vec![0.0f32; LIVE_OPUS_FRAME_SAMPLES];
        let mut tail = Vec::new();
        for packet in &packets {
            let VoicePayload::Opus(payload) = &packet.payload else {
                continue;
            };
            let decoded = decoder.decode_float(payload, &mut output, false).unwrap();
            if packet.flags & LIVE_PACKET_FLAG_MUTE != 0 {
                tail.extend_from_slice(&output[..decoded]);
            }
        }
        assert!(!tail.is_empty(), "no faded tail packets were produced");
        // Opus decode is not sample-accurate, so assert a substantial decay rather
        // than exact silence; the precise envelope is unit-tested on the fade
        // helper itself in `shared`.
        let last = &tail[tail.len().saturating_sub(FRAME_SAMPLES / 2)..];
        let tail_peak = last.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        let start_peak = tail
            .iter()
            .take(FRAME_SAMPLES)
            .fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!(
            tail_peak < start_peak * 0.5,
            "fade tail must decay toward silence (start peak {start_peak}, end peak {tail_peak})"
        );
    }

    #[test]
    fn live_encoder_pipeline_resamples_non_48k_capture() {
        // A 44.1 kHz device is resampled to 48 kHz before the pipeline, so a
        // steady tone still encodes to Opus frames despite the rate mismatch.
        let encoder = OpusVoiceEncoder::new(48_000).unwrap();
        let mut pipeline = LiveEncoderPipeline::new(
            encoder,
            DenoiseConfig::None,
            test_tuning(),
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            DenoiseSuppression::IDENTITY,
            DenoiseTypingSuppression::DISABLED,
            None,
            44_100,
            false,
        );
        let stats = AudioStats::new();
        let mut packets = Vec::new();
        // 300 ms of a 44.1 kHz tone in i16 scale, fed as ragged chunks.
        let input: Vec<f32> = (0..13_230)
            .map(|n| (n as f32 * 0.05).sin() * 6_000.0)
            .collect();
        for chunk in input.chunks(101) {
            pipeline
                .push_chunk(
                    chunk,
                    DEFAULT_LIVE_MAX_AMPLIFICATION,
                    false,
                    &stats,
                    &mut |packet| packets.push(packet),
                )
                .unwrap();
        }
        assert!(
            packets
                .iter()
                .any(|packet| matches!(packet.payload, VoicePayload::Opus(_))),
            "resampled 44.1 kHz capture produced no Opus packets"
        );
    }

    #[test]
    fn accumulates_frames_across_arbitrary_chunk_boundaries() {
        let mut accumulator = FrameAccumulator::new(4);
        let mut frames = Vec::new();

        accumulator
            .push_chunk(&[1.0, 2.0, 3.0], |frame| {
                frames.push(frame.to_vec());
                Ok(())
            })
            .unwrap();
        accumulator
            .push_chunk(&[4.0, 5.0, 6.0, 7.0, 8.0], |frame| {
                frames.push(frame.to_vec());
                Ok(())
            })
            .unwrap();

        assert_eq!(
            frames,
            vec![vec![1.0, 2.0, 3.0, 4.0], vec![5.0, 6.0, 7.0, 8.0]]
        );
    }

    #[test]
    fn live_encoder_pipeline_enables_aec_only_with_reference() {
        let without = build_live_encoder_pipeline(
            test_tuning(),
            true,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            None,
        )
        .unwrap();
        assert!(!without.aec_enabled());

        let with = build_live_encoder_pipeline(
            test_tuning(),
            true,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            Some(Arc::new(EchoReference::new())),
        )
        .unwrap();
        assert!(with.aec_enabled());
    }

    #[test]
    fn enabling_aec_clears_the_stale_render_backlog() {
        let control = Arc::new(EchoCancellationControl::new(false));
        // Render audio accumulates in the reference while AEC is disabled (the
        // pipeline is not consuming it); once AEC turns on, this backlog is
        // stale by the whole disabled span and must be dropped, not cancelled
        // against.
        control.reference().push_frame(&vec![0.5; 4_800]);

        let encoder = OpusVoiceEncoder::new(48_000).unwrap();
        let mut pipeline = LiveEncoderPipeline::new(
            encoder,
            DenoiseConfig::None,
            test_tuning(),
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            DenoiseSuppression::IDENTITY,
            DenoiseTypingSuppression::DISABLED,
            Some(EchoReferenceSource::Controlled(Arc::clone(&control))),
            crate::audio::shared::SAMPLE_RATE,
            false,
        );
        let stats = AudioStats::new();
        let chunk = vec![0.0; FRAME_SAMPLES];

        pipeline
            .push_chunk(
                &chunk,
                DEFAULT_LIVE_MAX_AMPLIFICATION,
                false,
                &stats,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            control.reference().fill(),
            4_800,
            "disabled AEC must not consume the reference"
        );

        control.set_enabled(true);
        pipeline
            .push_chunk(
                &chunk,
                DEFAULT_LIVE_MAX_AMPLIFICATION,
                false,
                &stats,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            control.reference().fill(),
            0,
            "enabling AEC must clear the stale render backlog"
        );
    }

    #[test]
    fn live_encoder_pipeline_toggles_aec_from_control() {
        let control = Arc::new(EchoCancellationControl::new(false));
        let encoder = OpusVoiceEncoder::new(48_000).unwrap();
        let mut pipeline = LiveEncoderPipeline::new(
            encoder,
            DenoiseConfig::None,
            test_tuning(),
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            DenoiseSuppression::IDENTITY,
            DenoiseTypingSuppression::DISABLED,
            Some(EchoReferenceSource::Controlled(Arc::clone(&control))),
            crate::audio::shared::SAMPLE_RATE,
            false,
        );
        let stats = AudioStats::new();
        let chunk = vec![0.0; FRAME_SAMPLES];
        let mut packets = 0;

        pipeline
            .push_chunk(
                &chunk,
                DEFAULT_LIVE_MAX_AMPLIFICATION,
                false,
                &stats,
                &mut |_| packets += 1,
            )
            .unwrap();
        assert!(!pipeline.aec_enabled());

        control.set_enabled(true);
        pipeline
            .push_chunk(
                &chunk,
                DEFAULT_LIVE_MAX_AMPLIFICATION,
                false,
                &stats,
                &mut |_| packets += 1,
            )
            .unwrap();
        assert!(pipeline.aec_enabled());

        control.set_enabled(false);
        pipeline
            .push_chunk(
                &chunk,
                DEFAULT_LIVE_MAX_AMPLIFICATION,
                false,
                &stats,
                &mut |_| packets += 1,
            )
            .unwrap();
        assert!(!pipeline.aec_enabled());
        assert!(packets <= 1);
    }
}
