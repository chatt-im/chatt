use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
    mpsc::{Receiver, SyncSender},
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
            AudioStats, DenoiseConfig, DenoiseSuppression, DenoiseTypingSuppression, FRAME_SAMPLES,
            LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_OPUS_RESET, LIVE_PACKET_FLAG_SILENCE_HINT,
            LIVE_PACKET_FLAG_SILENCE_RESUME, LiveAudioTuning, LiveEncoderProfile, LocalVoiceFrame,
            MAX_OPUS_PACKET_BYTES, VoicePayload, convert_i16_scale_to_pcm_i16, vad_to_u8,
        },
    },
    network::{EncoderNetworkProfile, EncoderNetworkTuning},
    packet_log::PacketLogWriter,
};

const SILENCE_RESUME_HINT_PACKETS: u8 = 10;
const SILENCE_KEEPALIVE_CAPTURE_FRAMES: usize = 100;

pub(crate) fn run_encoder_worker(
    receiver: Receiver<Vec<f32>>,
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
    receiver: Receiver<Vec<f32>>,
    recycle: SyncSender<Vec<f32>>,
    encoder: OpusVoiceEncoder,
    denoise: DenoiseConfig,
    max_amplification_bits: Arc<AtomicU32>,
    encoder_loss_percent: Arc<AtomicU32>,
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
    receiver: Receiver<Vec<f32>>,
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
pub(crate) fn run_live_encoder_worker_inner<F>(
    receiver: Receiver<Vec<f32>>,
    recycle: SyncSender<Vec<f32>>,
    encoder: OpusVoiceEncoder,
    denoise: DenoiseConfig,
    max_amplification_bits: &AtomicU32,
    encoder_loss_percent: &AtomicU32,
    tuning: LiveAudioTuning,
    suppression: DenoiseSuppression,
    typing_suppression: DenoiseTypingSuppression,
    echo_source: Option<EchoReferenceSource>,
    device_rate: u32,
    stats: &AudioStats,
    on_packet: &mut F,
) -> Result<(), String>
where
    F: FnMut(LocalVoiceFrame),
{
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
    );
    let mut applied_loss_percent = LiveEncoderProfile::DRED_20.packet_loss_percent;

    for chunk in receiver {
        let requested_loss_percent = encoder_loss_percent.load(Ordering::Relaxed).min(100) as i32;
        if requested_loss_percent != applied_loss_percent {
            pipeline.apply_encoder_profile(LiveEncoderProfile {
                packet_loss_percent: requested_loss_percent,
            })?;
            applied_loss_percent = requested_loss_percent;
        }
        pipeline.push_chunk(
            &chunk,
            f32::from_bits(max_amplification_bits.load(Ordering::Relaxed)),
            stats,
            on_packet,
        )?;
        let _ = recycle.try_send(chunk);
    }

    Ok(())
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
    next_opus_packet_flags: u8,
    silence_resume_hint_packets: u8,
    sender_silence_active: bool,
    silence_keepalive_frames: usize,
    suppressed_frames: u64,
    echo_source: Option<EchoReferenceSource>,
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
            next_opus_packet_flags: LIVE_PACKET_FLAG_OPUS_RESET,
            silence_resume_hint_packets: 0,
            sender_silence_active: false,
            silence_keepalive_frames: 0,
            suppressed_frames: 0,
            echo_source,
        }
    }

    pub(crate) fn aec_enabled(&self) -> bool {
        self.processor.echo_enabled()
    }

    pub(crate) fn push_chunk<F>(
        &mut self,
        chunk: &[f32],
        max_amplification: f32,
        stats: &AudioStats,
        on_packet: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(LocalVoiceFrame),
    {
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
            self.process_accumulated_frame(frame, stats, on_packet)
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

    fn process_accumulated_frame<F>(
        &mut self,
        frame: &mut [f32],
        stats: &AudioStats,
        on_packet: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(LocalVoiceFrame),
    {
        // One consolidated APM pass runs high-pass, AEC3, noise suppression, and
        // AGC2 in WebRTC order. The echo reference, present only while AEC is
        // enabled, supplies the 48 kHz render frame to cancel against.
        let reference = self
            .echo_source
            .as_ref()
            .filter(|source| source.enabled())
            .map(|source| source.reference());
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

        let decision = self
            .long_silence
            .as_mut()
            .map(|gate| gate.observe(frame, silence))
            .unwrap_or(CaptureGateDecision::TransmitCurrent {
                silence_hint: false,
            });
        match decision {
            CaptureGateDecision::TransmitCurrent { silence_hint } => {
                if silence_hint {
                    self.next_opus_packet_flags |= LIVE_PACKET_FLAG_SILENCE_HINT;
                }
                let frame = ProcessedCaptureFrame::new(frame);
                self.queue_processed_capture_frame(frame, stats, on_packet)?;
            }
            CaptureGateDecision::SuppressCurrent => {
                self.suppressed_frames = self.suppressed_frames.saturating_add(1);
                self.maybe_emit_silence_marker(on_packet);
            }
            CaptureGateDecision::Resume(frames) => {
                self.reset_opus_stream()?;
                for frame in frames {
                    let frame = ProcessedCaptureFrame::new(&frame.samples);
                    self.queue_processed_capture_frame(frame, stats, on_packet)?;
                }
            }
        }
        Ok(())
    }

    fn queue_processed_capture_frame<F>(
        &mut self,
        frame: ProcessedCaptureFrame<'_>,
        stats: &AudioStats,
        on_packet: &mut F,
    ) -> Result<(), String>
    where
        F: FnMut(LocalVoiceFrame),
    {
        self.pending_opus_samples.extend_from_slice(frame.samples);

        while self.pending_opus_samples.len() >= LIVE_OPUS_FRAME_SAMPLES {
            let mut flags = self.next_opus_packet_flags;
            self.next_opus_packet_flags = 0;
            if self.silence_resume_hint_packets > 0 {
                flags |= LIVE_PACKET_FLAG_SILENCE_RESUME;
            }
            let payload = encode_live_frame(
                &self.pending_opus_samples[..LIVE_OPUS_FRAME_SAMPLES],
                &mut self.encoder,
                &mut self.opus_frame,
                &mut self.encoded,
            )?;
            if self.silence_resume_hint_packets > 0 {
                self.silence_resume_hint_packets -= 1;
            }
            let packet_len = payload.len();
            self.sender_silence_active = false;
            self.silence_keepalive_frames = 0;
            on_packet(LocalVoiceFrame { flags, payload });
            stats.record_encoded_packet(packet_len);
            self.pending_opus_samples.drain(..LIVE_OPUS_FRAME_SAMPLES);
        }

        Ok(())
    }

    fn maybe_emit_silence_marker<F>(&mut self, on_packet: &mut F)
    where
        F: FnMut(LocalVoiceFrame),
    {
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
        on_packet(LocalVoiceFrame {
            flags: LIVE_PACKET_FLAG_SILENCE_HINT,
            payload: VoicePayload::Silence,
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

pub(crate) fn build_live_encoder_pipeline(
    tuning: LiveAudioTuning,
    denoise_enabled: bool,
    max_amplification: f32,
    auto_gain_enabled: bool,
    network_profile: EncoderNetworkProfile,
    echo_reference: Option<Arc<EchoReference>>,
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
        DEFAULT_LIVE_MAX_AMPLIFICATION, LIVE_PLAYBACK_DRED_MAX_SAMPLES, normalized_to_i16_scale,
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

        pipeline.maybe_emit_silence_marker(&mut |packet| silence_packets.push(packet));

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
        for _ in 0..2 {
            let frame = ProcessedCaptureFrame::new(&sampled_speech);
            pipeline
                .queue_processed_capture_frame(frame, &stats, &mut |packet| {
                    speech_packets.push(packet)
                })
                .unwrap();
        }

        let first_opus = speech_packets
            .iter()
            .find(|packet| matches!(packet.payload, VoicePayload::Opus(_)))
            .expect("speech should produce an Opus packet");
        assert_ne!(first_opus.flags & LIVE_PACKET_FLAG_SILENCE_RESUME, 0);
        assert_eq!(first_opus.flags & LIVE_PACKET_FLAG_OPUS_RESET, 0);
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
        );
        let stats = AudioStats::new();
        let chunk = vec![0.0; FRAME_SAMPLES];
        let mut packets = 0;

        pipeline
            .push_chunk(&chunk, DEFAULT_LIVE_MAX_AMPLIFICATION, &stats, &mut |_| {
                packets += 1
            })
            .unwrap();
        assert!(!pipeline.aec_enabled());

        control.set_enabled(true);
        pipeline
            .push_chunk(&chunk, DEFAULT_LIVE_MAX_AMPLIFICATION, &stats, &mut |_| {
                packets += 1
            })
            .unwrap();
        assert!(pipeline.aec_enabled());

        control.set_enabled(false);
        pipeline
            .push_chunk(&chunk, DEFAULT_LIVE_MAX_AMPLIFICATION, &stats, &mut |_| {
                packets += 1
            })
            .unwrap();
        assert!(!pipeline.aec_enabled());
        assert!(packets <= 1);
    }
}
