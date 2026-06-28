use std::collections::VecDeque;

use earshot::Detector as EarshotDetector;
use nnnoiseless::DenoiseState;
use sonora::config::{
    AdaptiveDigital, EchoCanceller as Aec3Config, GainController2, HighPassFilter, NoiseSuppression,
};
use sonora::{AudioProcessing, Config as ApmConfig, StreamConfig as ApmStreamConfig};

use crate::audio::capture::echo::EchoReference;
use crate::audio::shared::{
    AudioStats, DenoiseConfig, DenoiseSuppression, DenoiseTypingSuppression, FRAME_SAMPLES,
    LiveAudioTuning, SAMPLE_RATE, frames_for_duration, peak_i16_scale, rms_i16_scale,
    samples_for_duration,
};

const I16_SCALE: f32 = i16::MAX as f32;
const TYPING_GATE_RMS_MIN: f32 = 400.0;
const TYPING_GATE_DIFF_RATIO_MAX: f32 = 0.06;
const TYPING_GATE_GAIN: f32 = 0.05;
const TYPING_GATE_RAMP_SAMPLES: usize = 96;

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
    rnnoise: Option<Box<DenoiseState>>,
    denoised: Vec<f32>,
    render: Vec<f32>,
    render_out: Vec<f32>,
    near: Vec<f32>,
    cleaned: Vec<f32>,
}

impl CaptureProcessor {
    pub(crate) fn new(
        denoise: DenoiseConfig,
        max_gain_db: f32,
        echo_enabled: bool,
        suppression: DenoiseSuppression,
    ) -> Self {
        let stream = ApmStreamConfig::new(SAMPLE_RATE, 1);
        let apm = AudioProcessing::builder()
            .config(capture_apm_config(denoise, echo_enabled, max_gain_db))
            .capture_config(stream)
            .render_config(stream)
            .build();
        let rnnoise = matches!(denoise, DenoiseConfig::RnnNoise).then(|| {
            let mut state = DenoiseState::new();
            state.set_suppression_params(suppression.into());
            state
        });
        Self {
            apm,
            denoise,
            echo_enabled,
            gain_max_db: max_gain_db,
            rnnoise,
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

pub(crate) struct TypingNoiseGate {
    config: DenoiseTypingSuppression,
    release_confirm_frames: usize,
    release_ready_frames: usize,
    suppressing: bool,
    current_gain: f32,
}

impl TypingNoiseGate {
    pub(crate) fn new(config: DenoiseTypingSuppression, denoise: DenoiseConfig) -> Self {
        let mut config = config.normalized();
        config.enabled &= matches!(denoise, DenoiseConfig::RnnNoise);
        Self {
            config,
            release_confirm_frames: frames_for_duration(config.release_confirm).max(1),
            release_ready_frames: 0,
            suppressing: false,
            current_gain: 1.0,
        }
    }

    pub(crate) fn process(&mut self, rnnoise_vad: f32, earshot_vad: f32, frame: &mut [f32]) {
        if !self.config.enabled || frame.is_empty() {
            return;
        }
        let vad_probability = rnnoise_vad.max(earshot_vad);
        let rms = rms_i16_scale(frame) * I16_SCALE;
        let diff_ratio = derivative_rms_i16_scale(frame) / rms.max(1.0);
        let should_enter = vad_probability < self.config.vad_enter
            && rms >= TYPING_GATE_RMS_MIN
            && diff_ratio <= TYPING_GATE_DIFF_RATIO_MAX;
        if should_enter {
            self.suppressing = true;
            self.release_ready_frames = 0;
        } else if self.suppressing {
            if vad_probability >= self.config.vad_release {
                self.release_ready_frames = self.release_ready_frames.saturating_add(1);
                if self.release_ready_frames >= self.release_confirm_frames {
                    self.suppressing = false;
                    self.release_ready_frames = 0;
                }
            } else {
                self.release_ready_frames = 0;
            }
        }

        let target_gain = if self.suppressing {
            TYPING_GATE_GAIN
        } else {
            1.0
        };
        apply_ramped_frame_gain(frame, self.current_gain, target_gain);
        self.current_gain = target_gain;
    }
}

fn derivative_rms_i16_scale(samples: &[f32]) -> f32 {
    if samples.len() < 2 {
        return 0.0;
    }
    let square_sum = samples
        .windows(2)
        .map(|pair| {
            let diff = pair[1] - pair[0];
            diff * diff
        })
        .sum::<f32>();
    (square_sum / (samples.len() - 1) as f32).sqrt()
}

fn apply_ramped_frame_gain(samples: &mut [f32], start_gain: f32, target_gain: f32) {
    if samples.is_empty() {
        return;
    }
    if (start_gain - target_gain).abs() < f32::EPSILON {
        if target_gain != 1.0 {
            for sample in samples {
                *sample *= target_gain;
            }
        }
        return;
    }

    let ramp = TYPING_GATE_RAMP_SAMPLES.min(samples.len());
    for (index, sample) in samples.iter_mut().take(ramp).enumerate() {
        let t = (index + 1) as f32 / ramp as f32;
        let gain = start_gain + (target_gain - start_gain) * t;
        *sample *= gain;
    }
    for sample in samples.iter_mut().skip(ramp) {
        *sample *= target_gain;
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

    /// Clears all accumulated state. Used when the microphone is muted: the mute
    /// transition owns the fade and silence markers, and the gate's preroll holds
    /// pre-mute audio that must not replay when the user unmutes.
    pub(crate) fn reset(&mut self) {
        self.silence_frames = 0;
        self.suppressed = false;
        self.preroll.clear();
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

        // At capacity, reuse the evicted frame's buffer instead of dropping it
        // and allocating a fresh `Vec`. Sustained silence then cycles a fixed
        // set of buffers with no per-frame allocation.
        let mut frame = if self.preroll.len() >= self.preroll_frames {
            let mut reused = self.preroll.pop_front().expect("len checked above");
            reused.samples.clear();
            reused
        } else {
            CaptureBufferedFrame {
                samples: Vec::with_capacity(samples.len()),
            }
        };
        frame.samples.extend_from_slice(samples);
        self.preroll.push_back(frame);
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
    fn suppression_params_reach_rnnoise_and_attenuate_noise() {
        // A higher suppression strength routed through the processor must push a
        // broadband noise frame further down than stock RNNoise does. Drives both
        // processors with the same pseudo-random noise.
        fn run(suppression: DenoiseSuppression) -> f32 {
            let mut processor =
                CaptureProcessor::new(DenoiseConfig::RnnNoise, 0.0, false, suppression);
            let mut state: u32 = 0x9e37_79b9;
            let mut last = 0.0;
            for _ in 0..40 {
                let mut frame: Vec<f32> = (0..FRAME_SAMPLES)
                    .map(|_| {
                        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (state >> 8) as f32 / (1 << 24) as f32 * 0.2 * I16_SCALE - 0.1 * I16_SCALE
                    })
                    .collect();
                processor.process(&mut frame, None);
                last = rms_i16_scale(&frame);
            }
            last
        }
        let stock = run(DenoiseSuppression::IDENTITY);
        let strong = run(DenoiseSuppression {
            strength: 3.0,
            release: 1.0,
        });
        assert!(
            strong < stock,
            "strength=3 should attenuate noise below stock: stock={stock:.2} strong={strong:.2}"
        );
    }

    #[test]
    fn consolidated_processor_removes_dc_and_caps_full_scale() {
        // The always-on high-pass strips a DC bias, and with a positive gain
        // ceiling the AGC2 limiter keeps the signal inside full scale, all in one
        // consolidated pass.
        let mut processor = CaptureProcessor::new(
            DenoiseConfig::RnnNoise,
            8.0,
            false,
            DenoiseSuppression::IDENTITY,
        );
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
        let mut processor =
            CaptureProcessor::new(DenoiseConfig::None, 0.0, true, DenoiseSuppression::IDENTITY);
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
        let mut processor =
            CaptureProcessor::new(DenoiseConfig::None, 0.0, true, DenoiseSuppression::IDENTITY);
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
    fn typing_noise_gate_ducks_low_vad_table_thumps() {
        let mut gate = TypingNoiseGate::new(test_typing_gate_config(), DenoiseConfig::RnnNoise);
        let mut frame = sine_frame(220.0, 1_800.0);
        let before = rms_i16_scale(&frame) * I16_SCALE;

        gate.process(0.2, 0.2, &mut frame);
        let after_first = rms_i16_scale(&frame) * I16_SCALE;

        let mut next = sine_frame(220.0, 1_800.0);
        gate.process(0.2, 0.2, &mut next);
        let after_second = rms_i16_scale(&next) * I16_SCALE;

        assert!(
            after_first < before * 0.35,
            "first ducked frame should ramp down quickly: before={before:.1} after={after_first:.1}"
        );
        assert!(
            after_second < before * 0.08,
            "continued ducking should reach the floor: before={before:.1} after={after_second:.1}"
        );
    }

    #[test]
    fn typing_noise_gate_preserves_speech_like_frames() {
        let mut gate = TypingNoiseGate::new(test_typing_gate_config(), DenoiseConfig::RnnNoise);
        let mut voiced = sine_frame(1_600.0, 1_800.0);
        let before = rms_i16_scale(&voiced) * I16_SCALE;

        gate.process(0.2, 0.2, &mut voiced);
        let after = rms_i16_scale(&voiced) * I16_SCALE;

        assert!(
            after > before * 0.99,
            "high-derivative speech-like frame should not be ducked: before={before:.1} after={after:.1}"
        );
    }

    #[test]
    fn typing_noise_gate_preserves_frames_when_rnnoise_vad_is_speech() {
        let mut gate = TypingNoiseGate::new(test_typing_gate_config(), DenoiseConfig::RnnNoise);
        let mut frame = sine_frame(220.0, 1_800.0);
        let before = rms_i16_scale(&frame) * I16_SCALE;

        gate.process(0.9, 0.2, &mut frame);
        let after = rms_i16_scale(&frame) * I16_SCALE;

        assert!(
            after > before * 0.99,
            "high RNNoise VAD should prevent typing gate entry: before={before:.1} after={after:.1}"
        );
    }

    #[test]
    fn typing_noise_gate_disabled_is_transparent() {
        let mut gate =
            TypingNoiseGate::new(DenoiseTypingSuppression::DISABLED, DenoiseConfig::RnnNoise);
        let mut frame = sine_frame(220.0, 1_800.0);
        let before = frame.clone();

        gate.process(0.0, 0.0, &mut frame);

        assert_eq!(frame, before);
    }

    #[test]
    fn typing_noise_gate_requires_release_vad_before_opening() {
        let mut gate = TypingNoiseGate::new(test_typing_gate_config(), DenoiseConfig::RnnNoise);
        let mut thump = sine_frame(220.0, 1_800.0);
        gate.process(0.2, 0.2, &mut thump);

        let mut not_enough_vad = sine_frame(1_600.0, 1_800.0);
        let before = rms_i16_scale(&not_enough_vad) * I16_SCALE;
        gate.process(0.81, 0.81, &mut not_enough_vad);
        let held = rms_i16_scale(&not_enough_vad) * I16_SCALE;
        assert!(
            held < before * 0.10,
            "gate should stay closed below release VAD: before={before:.1} held={held:.1}"
        );

        for _ in 0..3 {
            let mut release = sine_frame(1_600.0, 1_800.0);
            gate.process(0.86, 0.86, &mut release);
        }
        let mut released = sine_frame(1_600.0, 1_800.0);
        gate.process(0.86, 0.86, &mut released);
        let after = rms_i16_scale(&released) * I16_SCALE;
        assert!(
            after > before * 0.95,
            "gate should open after sustained release VAD: before={before:.1} after={after:.1}"
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

    #[test]
    fn long_silence_gate_preroll_keeps_recent_frames_when_buffer_reused() {
        // ramp 0 keeps fade-in an identity so reused-buffer contents are exact.
        let mut gate = LongSilenceGate::with_limits(1, 2, 0);
        assert!(matches!(
            gate.observe(&mut vec![1.0; 4], true),
            CaptureGateDecision::TransmitCurrent { silence_hint: true }
        ));

        // Feed more silence frames than the 2-frame preroll holds, forcing the
        // capacity path that recycles evicted buffers.
        for value in [2.0_f32, 3.0, 4.0, 5.0] {
            assert!(matches!(
                gate.observe(&mut vec![value; 4], true),
                CaptureGateDecision::SuppressCurrent
            ));
        }

        let CaptureGateDecision::Resume(frames) = gate.observe(&mut vec![9.0; 4], false) else {
            panic!("speech should resume transmission");
        };

        // Only the two most recent prerolls survive, in order, plus the current
        // speech frame, with contents intact through buffer reuse.
        assert_eq!(frames.len(), 3);
        assert!(
            frames[0]
                .samples
                .iter()
                .all(|&s| (s - 4.0).abs() < f32::EPSILON)
        );
        assert!(
            frames[1]
                .samples
                .iter()
                .all(|&s| (s - 5.0).abs() < f32::EPSILON)
        );
        assert!(
            frames[2]
                .samples
                .iter()
                .all(|&s| (s - 9.0).abs() < f32::EPSILON)
        );
    }

    fn sine_frame(freq_hz: f32, amplitude: f32) -> Vec<f32> {
        (0..FRAME_SAMPLES)
            .map(|sample| {
                let phase = sample as f32 * freq_hz * std::f32::consts::TAU / SAMPLE_RATE as f32;
                phase.sin() * amplitude
            })
            .collect()
    }

    fn test_typing_gate_config() -> DenoiseTypingSuppression {
        DenoiseTypingSuppression {
            enabled: true,
            vad_enter: 0.80,
            vad_release: 0.82,
            release_confirm: std::time::Duration::from_millis(30),
        }
    }
}
