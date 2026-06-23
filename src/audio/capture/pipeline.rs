use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
    mpsc::Receiver,
};

use nnnoiseless::DenoiseState;

use crate::{
    audio::{
        capture::{
            dsp::{
                CaptureGain, CaptureGateDecision, CaptureHighPass, EarshotVad, LongSilenceGate,
                is_capture_skip_safe_silence, store_processed_level_stats,
            },
            echo::{EchoCanceller, EchoReference, EchoReferenceSource},
            encoder::OpusVoiceEncoder,
        },
        shared::{
            AudioStats, FRAME_SAMPLES, LIVE_OPUS_FRAME_SAMPLES, LIVE_PACKET_FLAG_OPUS_RESET,
            LIVE_PACKET_FLAG_SILENCE_HINT, LIVE_PACKET_FLAG_SILENCE_RESUME, LiveAudioTuning,
            LiveEncoderProfile, LocalVoiceFrame, MAX_OPUS_PACKET_BYTES, VoicePayload,
            convert_i16_scale_to_pcm_i16, vad_to_u8,
        },
    },
    network::{EncoderNetworkProfile, EncoderNetworkTuning},
    packet_log::PacketLogWriter,
};

const SILENCE_RESUME_HINT_PACKETS: u8 = 10;
const SILENCE_KEEPALIVE_CAPTURE_FRAMES: usize = 100;

pub(crate) fn run_encoder_worker(
    receiver: Receiver<Vec<f32>>,
    mut writer: PacketLogWriter<std::io::BufWriter<std::fs::File>>,
    mut encoder: OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification: f32,
    stats: AudioStats,
) {
    let result = run_encoder_worker_inner(
        receiver,
        &mut writer,
        &mut encoder,
        denoise_enabled,
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

pub(crate) fn run_live_encoder_worker<F>(
    receiver: Receiver<Vec<f32>>,
    encoder: OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification_bits: Arc<AtomicU32>,
    encoder_loss_percent: Arc<AtomicU32>,
    tuning: LiveAudioTuning,
    echo_source: Option<EchoReferenceSource>,
    stats: AudioStats,
    mut on_packet: F,
) where
    F: FnMut(LocalVoiceFrame) + Send + 'static,
{
    let result = run_live_encoder_worker_inner(
        receiver,
        encoder,
        denoise_enabled,
        &max_amplification_bits,
        &encoder_loss_percent,
        tuning,
        echo_source,
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
    writer: &mut PacketLogWriter<std::io::BufWriter<std::fs::File>>,
    encoder: &mut OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification: f32,
    stats: &AudioStats,
) -> Result<(), String> {
    let mut denoise = DenoiseState::new();
    let mut high_pass = CaptureHighPass::new();
    let mut gain = CaptureGain::new(max_amplification);
    let mut accumulator = FrameAccumulator::new(FRAME_SAMPLES);
    let mut denoised_frame = vec![0.0; FRAME_SAMPLES];
    let mut opus_frame = vec![0i16; FRAME_SAMPLES];
    let mut encoded = vec![0u8; MAX_OPUS_PACKET_BYTES];

    for chunk in receiver {
        accumulator.push_chunk(&chunk, |frame| {
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
        })?;
    }

    Ok(())
}

pub(crate) fn run_live_encoder_worker_inner<F>(
    receiver: Receiver<Vec<f32>>,
    encoder: OpusVoiceEncoder,
    denoise_enabled: bool,
    max_amplification_bits: &AtomicU32,
    encoder_loss_percent: &AtomicU32,
    tuning: LiveAudioTuning,
    echo_source: Option<EchoReferenceSource>,
    stats: &AudioStats,
    on_packet: &mut F,
) -> Result<(), String>
where
    F: FnMut(LocalVoiceFrame),
{
    let mut pipeline = LiveEncoderPipeline::new(
        encoder,
        denoise_enabled,
        tuning,
        f32::from_bits(max_amplification_bits.load(Ordering::Relaxed)),
        true,
        echo_source,
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
    }

    Ok(())
}

pub(crate) struct LiveEncoderPipeline {
    encoder: OpusVoiceEncoder,
    denoise_enabled: bool,
    auto_gain_enabled: bool,
    tuning: LiveAudioTuning,
    denoise: Box<DenoiseState<'static>>,
    earshot: EarshotVad,
    long_silence: Option<LongSilenceGate>,
    high_pass: CaptureHighPass,
    /// AGC2 gain stage, present only while the user's `max-amplification` dB
    /// ceiling is positive and auto gain is enabled. Rebuilt when the ceiling
    /// changes so it tracks the live setting.
    gain: Option<CaptureGain>,
    gain_max_db: f32,
    accumulator: FrameAccumulator,
    denoised_frame: Vec<f32>,
    opus_frame: Vec<i16>,
    encoded: Vec<u8>,
    pending_opus_samples: Vec<f32>,
    next_opus_packet_flags: u8,
    silence_resume_hint_packets: u8,
    sender_silence_active: bool,
    silence_keepalive_frames: usize,
    suppressed_frames: u64,
    echo: Option<EchoCanceller>,
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
    fn new(
        encoder: OpusVoiceEncoder,
        denoise_enabled: bool,
        tuning: LiveAudioTuning,
        max_amplification: f32,
        auto_gain_enabled: bool,
        echo_source: Option<EchoReferenceSource>,
    ) -> Self {
        let echo = echo_source
            .as_ref()
            .and_then(|source| source.enabled().then(EchoCanceller::new));
        Self {
            encoder,
            denoise_enabled,
            auto_gain_enabled,
            tuning,
            denoise: DenoiseState::new(),
            earshot: EarshotVad::new(),
            long_silence: tuning
                .capture_silence_gate
                .then(|| LongSilenceGate::new(tuning)),
            high_pass: CaptureHighPass::new(),
            gain: auto_gain_enabled
                .then(|| CaptureGain::new(max_amplification))
                .flatten(),
            gain_max_db: max_amplification,
            accumulator: FrameAccumulator::new(FRAME_SAMPLES),
            denoised_frame: vec![0.0; FRAME_SAMPLES],
            opus_frame: vec![0i16; LIVE_OPUS_FRAME_SAMPLES],
            encoded: vec![0u8; MAX_OPUS_PACKET_BYTES],
            pending_opus_samples: Vec::with_capacity(LIVE_OPUS_FRAME_SAMPLES),
            next_opus_packet_flags: LIVE_PACKET_FLAG_OPUS_RESET,
            silence_resume_hint_packets: 0,
            sender_silence_active: false,
            silence_keepalive_frames: 0,
            suppressed_frames: 0,
            echo,
            echo_source,
        }
    }

    fn process_echo(&mut self, frame: &mut [f32]) {
        let Some(source) = self.echo_source.as_ref() else {
            return;
        };
        if !source.enabled() {
            self.echo = None;
            return;
        }
        self.echo
            .get_or_insert_with(EchoCanceller::new)
            .process(frame, source.reference());
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
            self.gain = CaptureGain::new(max_amplification);
            self.gain_max_db = max_amplification;
        }
        let mut accumulator = self.take_accumulator();
        let result = accumulator.push_chunk(chunk, |frame| {
            self.process_accumulated_frame(frame, stats, on_packet)
        });
        self.accumulator = accumulator;
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
        // High-pass first to strip DC and rumble, then cancel echo, then apply
        // gain so AGC2 never amplifies un-cancelled echo.
        self.high_pass.process(frame);
        self.process_echo(frame);
        if let Some(gain) = self.gain.as_mut() {
            gain.process(frame);
        }
        let vad_probability = if self.denoise_enabled {
            let vad = self.denoise.process_frame(&mut self.denoised_frame, frame);
            frame.copy_from_slice(&self.denoised_frame);
            vad
        } else {
            self.earshot.process_48k_frame(frame)
        };
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
            match encode_live_frame(
                &self.pending_opus_samples[..LIVE_OPUS_FRAME_SAMPLES],
                &mut self.encoder,
                &mut self.opus_frame,
                &mut self.encoded,
            )? {
                VoiceEncodeResult::Opus(payload) => {
                    if self.silence_resume_hint_packets > 0 {
                        self.silence_resume_hint_packets -= 1;
                    }
                    let packet_len = payload.len();
                    self.sender_silence_active = false;
                    self.silence_keepalive_frames = 0;
                    on_packet(LocalVoiceFrame { flags, payload });
                    stats.record_encoded_packet(packet_len);
                }
                VoiceEncodeResult::Dtx => {
                    // DTX produces no Opus frame. Restore the flags so the
                    // first real Opus frame after this silence still resets the
                    // decoder and advertises resume.
                    self.next_opus_packet_flags = flags;
                    self.maybe_emit_silence_marker(on_packet);
                }
            }
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
    let mut encoder = OpusVoiceEncoder::new(network_profile.bitrate_bps)?;
    encoder.apply_network_profile(network_profile)?;
    Ok(LiveEncoderPipeline::new(
        encoder,
        denoise_enabled,
        tuning,
        max_amplification,
        auto_gain_enabled,
        echo_reference.map(EchoReferenceSource::Always),
    ))
}

pub(crate) fn encode_live_frame(
    frame: &[f32],
    encoder: &mut OpusVoiceEncoder,
    opus_frame: &mut [i16],
    encoded: &mut [u8],
) -> Result<VoiceEncodeResult, String> {
    convert_i16_scale_to_pcm_i16(frame, opus_frame);
    let packet_len = encoder.encode(opus_frame, encoded)?;
    if encoder.in_dtx()? {
        return Ok(VoiceEncodeResult::Dtx);
    }
    Ok(VoiceEncodeResult::Opus(VoicePayload::Opus(
        encoded[..packet_len].to_vec(),
    )))
}

pub(crate) enum VoiceEncodeResult {
    Opus(VoicePayload),
    Dtx,
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
        chunk: &[f32],
        mut on_frame: impl FnMut(&mut [f32]) -> Result<(), String>,
    ) -> Result<(), String> {
        for sample in chunk {
            self.pending.push(*sample);
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
    fn dtx_silence_marker_marks_next_opus_packet_as_resume() {
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
        assert!(without.echo.is_none());

        let with = build_live_encoder_pipeline(
            test_tuning(),
            true,
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            EncoderNetworkProfile::EXCELLENT,
            Some(Arc::new(EchoReference::new())),
        )
        .unwrap();
        assert!(with.echo.is_some());
    }

    #[test]
    fn live_encoder_pipeline_toggles_aec_from_control() {
        let control = Arc::new(EchoCancellationControl::new(false));
        let encoder = OpusVoiceEncoder::new(48_000).unwrap();
        let mut pipeline = LiveEncoderPipeline::new(
            encoder,
            false,
            test_tuning(),
            DEFAULT_LIVE_MAX_AMPLIFICATION,
            true,
            Some(EchoReferenceSource::Controlled(Arc::clone(&control))),
        );
        let stats = AudioStats::new();
        let chunk = vec![0.0; FRAME_SAMPLES];
        let mut packets = 0;

        pipeline
            .push_chunk(&chunk, DEFAULT_LIVE_MAX_AMPLIFICATION, &stats, &mut |_| {
                packets += 1
            })
            .unwrap();
        assert!(pipeline.echo.is_none());

        control.set_enabled(true);
        pipeline
            .push_chunk(&chunk, DEFAULT_LIVE_MAX_AMPLIFICATION, &stats, &mut |_| {
                packets += 1
            })
            .unwrap();
        assert!(pipeline.echo.is_some());

        control.set_enabled(false);
        pipeline
            .push_chunk(&chunk, DEFAULT_LIVE_MAX_AMPLIFICATION, &stats, &mut |_| {
                packets += 1
            })
            .unwrap();
        assert!(pipeline.echo.is_none());
        assert!(packets <= 1);
    }
}
