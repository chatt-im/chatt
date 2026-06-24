use std::collections::VecDeque;

use earshot::Detector as EarshotDetector;
use nnnoiseless::DenoiseState;
use sonora::config::{
    AdaptiveDigital, EchoCanceller as Aec3Config, GainController2, HighPassFilter, NoiseSuppression,
};
use sonora::{AudioProcessing, Config as ApmConfig, StreamConfig as ApmStreamConfig};

use crate::audio::capture::echo::EchoReference;
use crate::audio::shared::DenoiseConfig;
use crate::audio::shared::{
    AudioStats, FRAME_SAMPLES, LiveAudioTuning, SAMPLE_RATE, frames_for_duration, peak_i16_scale,
    rms_i16_scale, samples_for_duration,
};

const I16_SCALE: f32 = i16::MAX as f32;

/// Capture high-pass / DC-block, the sonora WebRTC biquad cascade. Runs first on
/// every frame so a microphone DC bias or low-frequency rumble cannot defeat
/// silence detection or waste encoder bits. Scale-independent, so it operates on
/// the i16-scale frame directly. Unlike gain, it is always on: it is filtering,
/// not amplification.
pub(crate) struct CaptureHighPass {
    filter: sonora::high_pass_filter::HighPassFilter,
    scratch: Vec<f32>,
}

impl CaptureHighPass {
    pub(crate) fn new() -> Self {
        Self {
            filter: sonora::high_pass_filter::HighPassFilter::new(
                crate::audio::shared::SAMPLE_RATE as i32,
                1,
            ),
            scratch: Vec::with_capacity(FRAME_SAMPLES),
        }
    }

    pub(crate) fn process(&mut self, frame: &mut [f32]) {
        self.scratch.clear();
        self.scratch.extend_from_slice(frame);
        self.filter
            .process_channels(std::slice::from_mut(&mut self.scratch));
        frame.copy_from_slice(&self.scratch);
    }
}

/// Sane upper bound on the auto-gain ceiling, in dB. The user's
/// `max-amplification` setting is clamped to this so the default AGC2 curve,
/// which is not calibrated to any specific rig, cannot run away.
pub(crate) const MAX_CAPTURE_GAIN_DB: f32 = 30.0;

/// Optional capture gain stage: sonora AGC2 (adaptive digital gain plus a
/// limiter), replacing the hand-rolled `AutoGain` whose fixed fast-attack /
/// slow-release smoothing pumped and clipped transients. It is gated by the
/// user's `max-amplification` setting, now expressed as a maximum gain in dB:
/// `0` bypasses the pass entirely for a well-levelled rig, and any positive
/// value caps AGC2's gain at that many dB.
pub(crate) struct CaptureGain {
    apm: AudioProcessing,
    near: Vec<f32>,
    cleaned: Vec<f32>,
}

impl CaptureGain {
    /// Builds the gain stage for a positive `max_gain_db` ceiling, or `None`
    /// when it is zero (or non-finite), which bypasses auto gain entirely.
    pub(crate) fn new(max_gain_db: f32) -> Option<Self> {
        if !(max_gain_db.is_finite() && max_gain_db > 0.0) {
            return None;
        }
        let max_gain_db = max_gain_db.min(MAX_CAPTURE_GAIN_DB);
        let default_adaptive = AdaptiveDigital::default();
        let stream = ApmStreamConfig::new(crate::audio::shared::SAMPLE_RATE, 1);
        let config = ApmConfig {
            gain_controller2: Some(GainController2 {
                adaptive_digital: Some(AdaptiveDigital {
                    max_gain_db,
                    initial_gain_db: default_adaptive.initial_gain_db.min(max_gain_db),
                    ..default_adaptive
                }),
                ..GainController2::default()
            }),
            ..ApmConfig::default()
        };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(stream)
            .render_config(stream)
            .build();
        Some(Self {
            apm,
            near: vec![0.0; FRAME_SAMPLES],
            cleaned: vec![0.0; FRAME_SAMPLES],
        })
    }

    /// Gain-controls one `FRAME_SAMPLES` i16-scale frame in place. The module
    /// works in normalized `[-1.0, 1.0]`, so the frame is scaled in and back out.
    pub(crate) fn process(&mut self, frame: &mut [f32]) {
        if frame.len() != FRAME_SAMPLES {
            return;
        }
        for (near, sample) in self.near.iter_mut().zip(frame.iter()) {
            *near = sample / I16_SCALE;
        }
        let _ = self
            .apm
            .process_capture_f32(&[&self.near], &mut [&mut self.cleaned]);
        for (sample, cleaned) in frame.iter_mut().zip(self.cleaned.iter()) {
            *sample = cleaned * I16_SCALE;
        }
    }
}

/// Builds the live capture APM config bundling the always-on high-pass filter
/// with the optionally-enabled AEC3, spectral noise suppression, and AGC2
/// passes. A pass left out of the config is not run, so a well-levelled rig with
/// echo off and a non-spectral denoise engine pays only for the high-pass
/// filter and any enabled gain. RNNoise denoising runs outside the APM (see
/// [`CaptureProcessor::process`]).
fn capture_apm_config(denoise: DenoiseConfig, echo: bool, max_gain_db: f32) -> ApmConfig {
    let gain_controller2 = (max_gain_db.is_finite() && max_gain_db > 0.0).then(|| {
        let max_gain_db = max_gain_db.min(MAX_CAPTURE_GAIN_DB);
        let default_adaptive = AdaptiveDigital::default();
        GainController2 {
            adaptive_digital: Some(AdaptiveDigital {
                max_gain_db,
                initial_gain_db: default_adaptive.initial_gain_db.min(max_gain_db),
                ..default_adaptive
            }),
            ..GainController2::default()
        }
    });
    ApmConfig {
        high_pass_filter: Some(HighPassFilter::default()),
        echo_canceller: echo.then(Aec3Config::default),
        noise_suppression: matches!(denoise, DenoiseConfig::Spectral)
            .then(NoiseSuppression::default),
        gain_controller2,
        ..ApmConfig::default()
    }
}

/// Consolidated live capture front-end: one sonora WebRTC `AudioProcessing`
/// instance running the high-pass filter, AEC3, optional spectral noise
/// suppression, and AGC2 in the canonical WebRTC order in a single pass. With
/// the [`DenoiseConfig::RnnNoise`] engine, the higher-quality RNNoise denoiser
/// runs after that pass instead of the APM's spectral suppressor, matching the
/// historical "denoise last" ordering and supplying the VAD. Processes one 10 ms
/// mono frame at 48 kHz per call inside the capture worker, never a realtime
/// callback. AEC and gain toggle at runtime through `apply_config`.
pub(crate) struct CaptureProcessor {
    apm: AudioProcessing,
    denoise: DenoiseConfig,
    echo_enabled: bool,
    gain_max_db: f32,
    rnnoise: Option<Box<DenoiseState<'static>>>,
    denoised: Vec<f32>,
    render: Vec<f32>,
    render_out: Vec<f32>,
    near: Vec<f32>,
    cleaned: Vec<f32>,
}

impl CaptureProcessor {
    pub(crate) fn new(denoise: DenoiseConfig, max_gain_db: f32, echo_enabled: bool) -> Self {
        let stream = ApmStreamConfig::new(SAMPLE_RATE, 1);
        let apm = AudioProcessing::builder()
            .config(capture_apm_config(denoise, echo_enabled, max_gain_db))
            .capture_config(stream)
            .render_config(stream)
            .build();
        Self {
            apm,
            denoise,
            echo_enabled,
            gain_max_db: max_gain_db,
            rnnoise: matches!(denoise, DenoiseConfig::RnnNoise).then(DenoiseState::new),
            denoised: vec![0.0; FRAME_SAMPLES],
            render: vec![0.0; FRAME_SAMPLES],
            render_out: vec![0.0; FRAME_SAMPLES],
            near: vec![0.0; FRAME_SAMPLES],
            cleaned: vec![0.0; FRAME_SAMPLES],
        }
    }

    fn rebuild(&mut self) {
        self.apm.apply_config(capture_apm_config(
            self.denoise,
            self.echo_enabled,
            self.gain_max_db,
        ));
    }

    /// Retunes or bypasses the AGC2 gain ceiling when the user's
    /// `max-amplification` setting changes. A value of `0` drops the gain pass.
    pub(crate) fn set_max_gain_db(&mut self, max_gain_db: f32) {
        if max_gain_db != self.gain_max_db {
            self.gain_max_db = max_gain_db;
            self.rebuild();
        }
    }

    /// Enables or disables the echo canceller when the live AEC toggle changes.
    pub(crate) fn set_echo_enabled(&mut self, enabled: bool) {
        if enabled != self.echo_enabled {
            self.echo_enabled = enabled;
            self.rebuild();
        }
    }

    pub(crate) fn echo_enabled(&self) -> bool {
        self.echo_enabled
    }

    /// Processes one `FRAME_SAMPLES` i16-scale capture frame in place. When echo
    /// cancellation is enabled, `reference` supplies the latest 48 kHz render
    /// frame to align against. The module works in normalized `[-1.0, 1.0]`, so
    /// the frame is scaled in and back out. Returns the RNNoise voice-activity
    /// probability when the RNNoise engine is active, otherwise `None`.
    pub(crate) fn process(
        &mut self,
        frame: &mut [f32],
        reference: Option<&EchoReference>,
    ) -> Option<f32> {
        if frame.len() != FRAME_SAMPLES {
            return None;
        }
        if self.echo_enabled {
            if let Some(reference) = reference {
                reference.pull_frame(&mut self.render);
                let _ = self
                    .apm
                    .process_render_f32(&[&self.render], &mut [&mut self.render_out]);
            }
        }
        for (near, sample) in self.near.iter_mut().zip(frame.iter()) {
            *near = sample / I16_SCALE;
        }
        let _ = self
            .apm
            .process_capture_f32(&[&self.near], &mut [&mut self.cleaned]);
        for (sample, cleaned) in frame.iter_mut().zip(self.cleaned.iter()) {
            *sample = cleaned * I16_SCALE;
        }
        // RNNoise runs last on the cleaned i16-scale frame, the same scale it
        // expects, and yields the VAD probability for the silence gate.
        if let Some(rnnoise) = self.rnnoise.as_mut() {
            let vad = rnnoise.process_frame(&mut self.denoised, frame);
            frame.copy_from_slice(&self.denoised);
            return Some(vad);
        }
        None
    }
}

pub(crate) struct EarshotVad {
    detector: EarshotDetector,
    pending_16k: VecDeque<i16>,
    decimator: [f32; 3],
    decimator_len: usize,
    last_score: f32,
}

impl EarshotVad {
    pub(crate) fn new() -> Self {
        Self {
            detector: EarshotDetector::default(),
            pending_16k: VecDeque::with_capacity(512),
            decimator: [0.0; 3],
            decimator_len: 0,
            last_score: 0.0,
        }
    }

    pub(crate) fn process_48k_frame(&mut self, samples: &[f32]) -> f32 {
        let mut score = self.last_score;
        for sample in samples {
            self.decimator[self.decimator_len] = *sample;
            self.decimator_len += 1;
            if self.decimator_len == self.decimator.len() {
                let averaged = self.decimator.iter().sum::<f32>() / self.decimator.len() as f32;
                self.pending_16k
                    .push_back(averaged.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
                self.decimator_len = 0;
            }
        }

        while self.pending_16k.len() >= 256 {
            let mut frame = [0i16; 256];
            for sample in &mut frame {
                *sample = self.pending_16k.pop_front().unwrap_or_default();
            }
            score = self.detector.predict_i16(&frame).clamp(0.0, 1.0);
            self.last_score = score;
        }

        score
    }
}

pub(crate) fn is_capture_skip_safe_silence(
    tuning: LiveAudioTuning,
    vad: u8,
    samples: &[f32],
) -> bool {
    if samples.is_empty() {
        return true;
    }
    // Measure energy AC-coupled (frame mean removed) so a DC bias or constant
    // offset on the microphone cannot mask genuine silence. The capture-front
    // high-pass filter removes the bias from the transmitted signal too; this
    // keeps the classifier itself robust regardless of upstream filtering.
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let inv_scale = 1.0 / i16::MAX as f32;
    let mut peak = 0.0f32;
    let mut square_sum = 0.0f32;
    for &sample in samples {
        let ac = (sample - mean) * inv_scale;
        peak = peak.max(ac.abs());
        square_sum += ac * ac;
    }
    let rms = (square_sum / samples.len() as f32).sqrt();
    vad <= tuning.silence_vad_max && peak < 0.20 && rms < 0.05
}

pub(crate) struct CaptureBufferedFrame {
    pub(crate) samples: Vec<f32>,
}

pub(crate) enum CaptureGateDecision {
    TransmitCurrent { silence_hint: bool },
    SuppressCurrent,
    Resume(Vec<CaptureBufferedFrame>),
}

pub(crate) struct LongSilenceGate {
    silence_frames: usize,
    stop_frames: usize,
    preroll_frames: usize,
    ramp_samples: usize,
    suppressed: bool,
    preroll: VecDeque<CaptureBufferedFrame>,
}

impl LongSilenceGate {
    pub(crate) fn new(tuning: LiveAudioTuning) -> Self {
        Self::with_limits(
            frames_for_duration(tuning.capture_long_silence_stop),
            frames_for_duration(tuning.capture_silence_preroll),
            samples_for_duration(tuning.capture_silence_ramp),
        )
    }

    fn with_limits(stop_frames: usize, preroll_frames: usize, ramp_samples: usize) -> Self {
        Self {
            silence_frames: 0,
            stop_frames: stop_frames.max(1),
            preroll_frames,
            ramp_samples,
            suppressed: false,
            preroll: VecDeque::with_capacity(preroll_frames),
        }
    }

    pub(crate) fn observe(&mut self, samples: &mut [f32], silence: bool) -> CaptureGateDecision {
        if silence {
            self.silence_frames = self.silence_frames.saturating_add(1);
        } else {
            self.silence_frames = 0;
        }

        if self.suppressed {
            if silence {
                self.push_preroll(samples);
                return CaptureGateDecision::SuppressCurrent;
            }

            let mut frames = self.preroll.drain(..).collect::<Vec<_>>();
            frames.push(CaptureBufferedFrame {
                samples: samples.to_vec(),
            });
            apply_fade_in_to_frames(&mut frames, self.ramp_samples);
            self.suppressed = false;
            return CaptureGateDecision::Resume(frames);
        }

        if silence && self.silence_frames == self.stop_frames {
            apply_fade_out(samples, self.ramp_samples);
            return CaptureGateDecision::TransmitCurrent { silence_hint: true };
        }

        if silence && self.silence_frames > self.stop_frames {
            self.suppressed = true;
            self.push_preroll(samples);
            return CaptureGateDecision::SuppressCurrent;
        }

        CaptureGateDecision::TransmitCurrent {
            silence_hint: false,
        }
    }

    fn push_preroll(&mut self, samples: &[f32]) {
        if self.preroll_frames == 0 {
            return;
        }

        self.preroll.push_back(CaptureBufferedFrame {
            samples: samples.to_vec(),
        });
        while self.preroll.len() > self.preroll_frames {
            self.preroll.pop_front();
        }
    }
}

pub(crate) fn apply_fade_out(samples: &mut [f32], ramp_samples: usize) {
    let fade = ramp_samples.min(samples.len());
    if fade == 0 {
        return;
    }

    let start = samples.len() - fade;
    for index in 0..fade {
        let t = (index + 1) as f32 / fade as f32;
        let gain = ((1.0 - t) * std::f32::consts::FRAC_PI_2).sin();
        samples[start + index] *= gain;
    }
}

pub(crate) fn apply_fade_in_to_frames(frames: &mut [CaptureBufferedFrame], ramp_samples: usize) {
    let total = frames
        .iter()
        .map(|frame| frame.samples.len())
        .sum::<usize>();
    let fade = ramp_samples.min(total);
    if fade == 0 {
        return;
    }

    let mut cursor = 0usize;
    for frame in frames {
        for sample in &mut frame.samples {
            if cursor >= fade {
                return;
            }
            let t = (cursor + 1) as f32 / fade as f32;
            let gain = (t * std::f32::consts::FRAC_PI_2).sin();
            *sample *= gain;
            cursor += 1;
        }
    }
}

pub(crate) fn store_processed_level_stats(stats: &AudioStats, samples: &[f32]) {
    let rms = rms_i16_scale(samples);
    let peak = peak_i16_scale(samples);
    stats.store_levels(rms, peak);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::rms_i16_scale;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

    #[test]
    fn consolidated_processor_removes_dc_and_caps_full_scale() {
        // The always-on high-pass strips a DC bias, and with a positive gain
        // ceiling the AGC2 limiter keeps the signal inside full scale, all in one
        // consolidated pass.
        let mut processor = CaptureProcessor::new(DenoiseConfig::RnnNoise, 8.0, false);
        let mut frame = vec![0.3 * I16_SCALE; FRAME_SAMPLES];
        for _ in 0..8 {
            frame
                .iter_mut()
                .for_each(|sample| *sample = 0.3 * I16_SCALE);
            processor.process(&mut frame, None);
        }
        let mean = frame.iter().sum::<f32>() / frame.len() as f32;
        assert!(mean.abs() < 0.05 * I16_SCALE, "DC not removed: mean={mean}");
        assert!(
            frame.iter().all(|sample| sample.abs() <= I16_SCALE + 1.0),
            "limiter let a sample exceed full scale"
        );
    }

    #[test]
    fn consolidated_processor_attenuates_aligned_echo() {
        // With echo cancellation enabled the consolidated APM attenuates a
        // far-end-only echo aligned against the render reference.
        let reference = EchoReference::new();
        let mut processor = CaptureProcessor::new(DenoiseConfig::None, 0.0, true);
        let frames = sample_speech_frames();
        let gain = 0.5f32;
        let warmup = 600usize;
        let measure = 200usize;
        let mut echo_path = EchoPath::new(4, gain);
        let mut echo_in = 0.0f32;
        let mut residual = 0.0f32;
        for index in 0..warmup + measure {
            let render = &frames[index % frames.len()];
            reference.push_frame(render);
            let mut mic = echo_path.capture(render, &[]);
            let before = rms_i16_scale(&mic);
            processor.process(&mut mic, Some(&reference));
            let after = rms_i16_scale(&mic);
            if index >= warmup {
                echo_in += before;
                residual += after;
            }
        }
        assert!(
            residual < echo_in * 0.3,
            "far-end-only echo should be attenuated: echo_in={echo_in:.1}, residual={residual:.1}"
        );
    }

    #[test]
    fn consolidated_processor_preserves_double_talk() {
        let reference = EchoReference::new();
        let mut processor = CaptureProcessor::new(DenoiseConfig::None, 0.0, true);
        let frames = sample_speech_frames();
        let gain = 0.5f32;
        let warmup = 600usize;
        let measure = 200usize;
        let mut echo_path = EchoPath::new(4, gain);
        let mut near_in = 0.0f32;
        let mut near_out = 0.0f32;
        for index in 0..warmup + measure {
            let render = &frames[index % frames.len()];
            let near = &frames[(index + frames.len() / 2) % frames.len()];
            reference.push_frame(render);
            let mut mic = echo_path.capture(render, near);
            let near_only = rms_i16_scale(
                &near
                    .iter()
                    .map(|sample| sample * i16::MAX as f32)
                    .collect::<Vec<_>>(),
            );
            processor.process(&mut mic, Some(&reference));
            if index >= warmup {
                near_in += near_only;
                near_out += rms_i16_scale(&mic);
            }
        }
        assert!(
            near_out > near_in * 0.4,
            "near-end speech must survive double talk: near_in={near_in:.1}, near_out={near_out:.1}"
        );
    }

    #[test]
    fn capture_high_pass_removes_dc_bias() {
        // A constant DC offset is filtered out to near zero once the biquad
        // cascade settles past its transient.
        let mut hpf = CaptureHighPass::new();
        let mut frame = vec![0.3 * I16_SCALE; FRAME_SAMPLES];
        for _ in 0..8 {
            frame
                .iter_mut()
                .for_each(|sample| *sample = 0.3 * I16_SCALE);
            hpf.process(&mut frame);
        }

        let mean = frame.iter().sum::<f32>() / frame.len() as f32;
        assert!(mean.abs() < 0.02 * I16_SCALE, "DC not removed: mean={mean}");
    }

    #[test]
    fn capture_gain_is_bypassed_at_zero_max_amplification() {
        // Zero max-amplification bypasses the auto-gain pass entirely so a
        // well-levelled rig passes through untouched; any positive value enables
        // AGC2 and keeps the signal within full scale via its limiter.
        assert!(CaptureGain::new(0.0).is_none());
        assert!(CaptureGain::new(-1.0).is_none());

        let mut gain = CaptureGain::new(8.0).expect("positive max-amplification enables gain");
        let mut frame = vec![0.4 * I16_SCALE; FRAME_SAMPLES];
        for _ in 0..8 {
            frame
                .iter_mut()
                .for_each(|sample| *sample = 0.4 * I16_SCALE);
            gain.process(&mut frame);
        }
        assert!(
            frame.iter().all(|sample| sample.abs() <= I16_SCALE + 1.0),
            "limiter let a sample exceed full scale"
        );
    }

    #[test]
    fn dc_offset_silence_remains_skip_safe() {
        let frame = vec![0.06 * f32::from(i16::MAX); FRAME_SAMPLES];

        assert!(
            is_capture_skip_safe_silence(test_tuning(), 0, &frame),
            "DC offset alone must not disable silence trimming"
        );
    }

    #[test]
    fn long_silence_gate_suppresses_after_threshold_with_fade_out() {
        let mut gate = LongSilenceGate::with_limits(3, 2, 4);

        for _ in 0..2 {
            let mut frame = vec![1.0; 8];
            assert!(matches!(
                gate.observe(&mut frame, true),
                CaptureGateDecision::TransmitCurrent {
                    silence_hint: false
                }
            ));
            assert!(
                frame
                    .iter()
                    .all(|sample| (*sample - 1.0).abs() < f32::EPSILON)
            );
        }

        let mut final_frame = vec![1.0; 8];
        assert!(matches!(
            gate.observe(&mut final_frame, true),
            CaptureGateDecision::TransmitCurrent { silence_hint: true }
        ));
        assert!(final_frame[4] < 1.0);
        assert!(final_frame[7].abs() < f32::EPSILON);

        let mut suppressed_frame = vec![1.0; 8];
        assert!(matches!(
            gate.observe(&mut suppressed_frame, true),
            CaptureGateDecision::SuppressCurrent
        ));
    }

    #[test]
    fn long_silence_gate_resumes_with_recent_preroll_and_fade_in() {
        let mut gate = LongSilenceGate::with_limits(1, 2, 4);
        let mut threshold_frame = vec![0.1; 4];
        assert!(matches!(
            gate.observe(&mut threshold_frame, true),
            CaptureGateDecision::TransmitCurrent { silence_hint: true }
        ));

        let mut old_preroll = vec![0.2; 4];
        assert!(matches!(
            gate.observe(&mut old_preroll, true),
            CaptureGateDecision::SuppressCurrent
        ));
        let mut recent_preroll = vec![0.4; 4];
        assert!(matches!(
            gate.observe(&mut recent_preroll, true),
            CaptureGateDecision::SuppressCurrent
        ));

        let mut speech = vec![1.0; 4];
        let CaptureGateDecision::Resume(frames) = gate.observe(&mut speech, false) else {
            panic!("speech should resume transmission");
        };

        assert_eq!(frames.len(), 3);
        assert!(frames[0].samples[0] > 0.0);
        assert!(frames[0].samples[0] < frames[0].samples[3]);
        assert!((frames[1].samples[0] - 0.4).abs() < f32::EPSILON);
        assert!((frames[2].samples[0] - 1.0).abs() < f32::EPSILON);
    }
}
